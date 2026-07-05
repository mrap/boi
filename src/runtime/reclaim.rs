//! Worktree reclamation for terminal specs (audit C1; design §5).
//!
//! ## Why this exists
//!
//! `teardown` (worktree.rs) is reachable ONLY as the terminal phase of the
//! *success* pipeline — `SpecFailed` / `SpecCanceled` / crash-recovery never
//! touch disk, so failed/canceled specs leaked their worktrees forever
//! (audit C1: 36 GB in 4 days), and `boi clean` deleted the DB rows that
//! mapped directories to specs while leaving the gigabytes. This module is
//! the missing reclaim: given a spec id, the operator workspace, and the
//! worktree root, it removes the spec's worktree directories from disk and
//! prunes their `.git/worktrees/<name>` registrations in the workspace
//! (design §5: "worktrees stay until `boi clean`" — `boi clean` must
//! therefore actually take them).
//!
//! Two callers:
//! - `cli::clean` — `boi clean <spec-id>` reclaims before the row cascade.
//! - `service::sweeper`'s auto-clean pass — the design-§5
//!   `auto_clean_canceled_after` retention (via the [`SpecReclaimer`] port;
//!   this module provides the `runtime/` implementation, mirroring the
//!   `PhaseExecutor` port pattern).
//!
//! ## The dirty-check safety rule (audit A1's lesson)
//!
//! Before deleting a worktree directory it is checked with
//! [`git_ops::is_clean`] — uncommitted changes (INCLUDING untracked files:
//! `commit_all` stages untracked files, so an uncommitted untracked file can
//! be un-landed work) mean the directory is SKIPPED and reported, never
//! silently destroyed. A directory whose cleanliness cannot be determined
//! (corrupt checkout, missing admin entry) is also skipped-and-reported:
//! when in doubt, keep the bytes and tell the operator.
//!
//! ## `git2` blocks
//!
//! Everything here is blocking libgit2 + `std::fs` work; the public async
//! entry point wraps the whole pass in [`tokio::task::spawn_blocking`].

use std::path::{Path, PathBuf};

use crate::runtime::{git_ops, worktree};
use crate::service::sweeper::{ReclaimError, ReclaimOutcome, SpecReclaimer};
use crate::types::ids::SpecId;

/// The sweeper-facing [`SpecReclaimer`] implementation — holds the worktree
/// root (`~/.boi/v2/worktrees` in production; a tempdir in tests).
#[derive(Debug, Clone)]
pub struct SpecWorktreeReclaimer {
    /// The §5 worktree root the spec directories live under.
    pub worktree_root: PathBuf,
}

#[async_trait::async_trait]
impl SpecReclaimer for SpecWorktreeReclaimer {
    async fn reclaim(
        &self,
        workspace: &Path,
        spec_id: &SpecId,
    ) -> Result<ReclaimOutcome, ReclaimError> {
        reclaim_spec_worktrees(
            workspace.to_path_buf(),
            spec_id.clone(),
            self.worktree_root.clone(),
        )
        .await
    }
}

/// Remove every worktree directory of `spec_id` under
/// `<worktree_root>/<SpecId>/` and prune the matching `.git/worktrees/`
/// registrations in `workspace`, skipping (and reporting) any directory with
/// uncommitted changes. Also prunes STALE registrations — `spec-<SpecId>-*`
/// entries whose directory is already gone (the audit-C1 pollution class).
///
/// Idempotent: a spec with nothing on disk returns an empty
/// [`ReclaimOutcome`] without opening the workspace repo (the cheap path the
/// sweeper hits every pass).
pub async fn reclaim_spec_worktrees(
    workspace: PathBuf,
    spec_id: SpecId,
    worktree_root: PathBuf,
) -> Result<ReclaimOutcome, ReclaimError> {
    tokio::task::spawn_blocking(move || reclaim_blocking(&workspace, &spec_id, &worktree_root))
        .await
        .map_err(|e| ReclaimError(format!("reclaim task panicked: {e}")))?
}

/// The blocking reclaim pass — called only inside `spawn_blocking`.
fn reclaim_blocking(
    workspace: &Path,
    spec_id: &SpecId,
    worktree_root: &Path,
) -> Result<ReclaimOutcome, ReclaimError> {
    let mut out = ReclaimOutcome::default();
    let spec_dir = worktree_root.join(spec_id.as_str());

    // The cheap idempotent path: nothing on disk for this spec → nothing to
    // do, and the workspace repo is never even opened (the sweeper hits this
    // for every already-reclaimed spec on every pass).
    if !spec_dir.is_dir() {
        return Ok(out);
    }

    // (1) — every worktree directory under `<root>/<SpecId>/` (the §5
    // layout: `integration` + one dir per TaskId). Dirty-check FIRST; a
    // clean dir is removed and its registration pruned in one call
    // (`git_ops::remove_worktree`); a dirty or undeterminable dir is
    // skipped-and-reported — never silently destroyed (audit A1's lesson).
    let entries = std::fs::read_dir(&spec_dir)
        .map_err(|e| ReclaimError(format!("reading {}: {e}", spec_dir.display())))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| ReclaimError(format!("dir entry in {}: {e}", spec_dir.display())))?;
        let path = entry.path();
        if !path.is_dir() {
            continue; // a stray file blocks the final rmdir, which is correct
        }
        // The registration NAME is spec-scoped, never path-basename-scoped
        // (OBS-023) — reconstruct it exactly as `verify_in`/`prepare_spec`
        // installed it.
        let Some(leaf) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let name = if leaf == "integration" {
            worktree::integration_worktree_name(spec_id)
        } else {
            format!("spec-{}-task-{leaf}", spec_id.as_str())
        };
        match git_ops::is_clean(&path) {
            Ok(true) => match git_ops::remove_worktree(workspace, &name, &path) {
                Ok(()) => out.removed.push(path),
                Err(e) => out.failed.push((path, format!("removing worktree: {e}"))),
            },
            Ok(false) => out.skipped_dirty.push(path),
            // Cleanliness undeterminable (corrupt checkout, lost admin
            // entry): when in doubt, keep the bytes and report.
            Err(e) => out
                .failed
                .push((path, format!("dirty-check failed — NOT deleting: {e}"))),
        }
    }

    // (2) — STALE registrations: `spec-<SpecId>-*` admin entries in the
    // workspace whose directory is already gone (the audit-C1 pollution
    // class — dead entries accumulate in operator repos). A registration
    // whose path still exists (e.g. a dirty skip above) is left alone.
    match prune_stale_registrations(workspace, spec_id) {
        Ok(mut names) => out.pruned_registrations.append(&mut names),
        Err(e) => out.failed.push((
            workspace.to_path_buf(),
            format!("stale-registration prune failed: {e}"),
        )),
    }

    // (3) — remove the emptied `<root>/<SpecId>/` directory. Anything left
    // inside (a dirty skip, a fault, a stray file) keeps it — `remove_dir`
    // only takes an empty directory, so this can never destroy a skip.
    if spec_dir.is_dir() {
        let is_empty = std::fs::read_dir(&spec_dir)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if is_empty {
            if let Err(e) = std::fs::remove_dir(&spec_dir) {
                out.failed
                    .push((spec_dir, format!("removing emptied spec dir: {e}")));
            }
        }
    }

    Ok(out)
}

/// Prune every `spec-<SpecId>-*` worktree registration in `workspace` whose
/// directory no longer exists. Returns the pruned names.
fn prune_stale_registrations(
    workspace: &Path,
    spec_id: &SpecId,
) -> Result<Vec<String>, git_ops::GitError> {
    let repo = git2::Repository::open(workspace)?;
    let prefix = format!("spec-{}-", spec_id.as_str());
    let mut pruned = Vec::new();
    for name in repo.worktrees()?.iter().flatten() {
        if !name.starts_with(&prefix) {
            continue;
        }
        let worktree = repo.find_worktree(name)?;
        if worktree.path().exists() {
            // A surviving directory (a dirty skip, or a live worktree) keeps
            // its registration — git must keep mapping it.
            continue;
        }
        // `valid(true)` forces the prune past libgit2's own validity check;
        // `working_tree(true)` is a no-op here (the dir is already gone).
        let mut opts = git2::WorktreePruneOptions::new();
        opts.valid(true).working_tree(true);
        worktree.prune(Some(&mut opts))?;
        pruned.push(name.to_owned());
    }
    Ok(pruned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::git_ops;
    use crate::runtime::worktree::{
        integration_worktree, integration_worktree_name, task_worktree, task_worktree_name,
    };
    use crate::types::ids::TaskId;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A throwaway directory removed on drop — `std`-only.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("boi-reclaim-{}-{tag}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.path));
        }
    }

    fn spec_id() -> SpecId {
        SpecId::new("S0000001a").unwrap()
    }
    fn task_id() -> TaskId {
        TaskId::new("T0000001a").unwrap()
    }

    /// Init a source repo with one commit on `main`.
    fn init_source_repo(path: &Path) {
        use git2::{Repository, Signature};
        let repo = Repository::init(path).expect("init");
        let sig = Signature::now("test", "test@localhost").unwrap();
        std::fs::write(path.join("README.md"), "hello\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("README.md")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
            .unwrap();
        if repo.find_branch("main", git2::BranchType::Local).is_err() {
            let head = repo.head().unwrap().peel_to_commit().unwrap();
            repo.branch("main", &head, true).unwrap();
            repo.set_head("refs/heads/main").unwrap();
        }
    }

    /// Stand up the §5 layout for `spec_id()`: an integration worktree and
    /// one task worktree, both registered in `repo`.
    fn build_spec_worktrees(repo: &Path, root: &Path) -> (PathBuf, PathBuf) {
        let integration_branch = crate::runtime::worktree::integration_branch(&spec_id());
        let task_branch = crate::runtime::worktree::task_branch(&spec_id(), &task_id());
        git_ops::create_branch(repo, &integration_branch, "main").unwrap();
        git_ops::create_branch(repo, &task_branch, "main").unwrap();

        let integration = integration_worktree(root, &spec_id());
        let task = task_worktree(root, &spec_id(), &task_id());
        git_ops::add_worktree(
            repo,
            &integration_branch,
            &integration_worktree_name(&spec_id()),
            &integration,
        )
        .unwrap();
        git_ops::add_worktree(
            repo,
            &task_branch,
            &task_worktree_name(&spec_id(), &task_id()),
            &task,
        )
        .unwrap();
        (integration, task)
    }

    /// The registered worktree names in `repo`.
    fn registration_names(repo: &Path) -> Vec<String> {
        let repo = git2::Repository::open(repo).unwrap();
        repo.worktrees()
            .unwrap()
            .iter()
            .flatten()
            .map(str::to_owned)
            .collect()
    }

    /// Clean worktrees are removed from disk, their registrations pruned,
    /// and the emptied `<root>/<SpecId>/` directory is removed too.
    #[tokio::test]
    async fn test_l2_reclaim_removes_clean_worktrees_and_prunes_registrations() {
        let dir = TempDir::new("clean");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        let root = dir.path.join("worktrees");
        let (integration, task) = build_spec_worktrees(&repo, &root);

        let outcome = reclaim_spec_worktrees(repo.clone(), spec_id(), root.clone())
            .await
            .expect("reclaim runs");

        assert_eq!(outcome.removed.len(), 2, "both worktrees removed");
        assert!(outcome.skipped_dirty.is_empty(), "nothing was dirty");
        assert!(outcome.failed.is_empty(), "no faults: {:?}", outcome.failed);
        assert!(!integration.exists(), "integration dir gone");
        assert!(!task.exists(), "task dir gone");
        assert!(
            !root.join(spec_id().as_str()).exists(),
            "emptied spec dir gone"
        );
        assert!(
            registration_names(&repo).is_empty(),
            "registrations pruned, got {:?}",
            registration_names(&repo),
        );
    }

    /// A DIRTY worktree (uncommitted change to a tracked file) is SKIPPED —
    /// reported in the outcome, directory intact, registration intact — while
    /// its clean siblings are still reclaimed (audit A1's lesson: never
    /// silently destroy work).
    #[tokio::test]
    async fn test_l2_reclaim_skips_dirty_worktree_and_reports_it() {
        let dir = TempDir::new("dirty");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        let root = dir.path.join("worktrees");
        let (integration, task) = build_spec_worktrees(&repo, &root);

        // Dirty the TASK worktree: an uncommitted edit to a tracked file.
        std::fs::write(task.join("README.md"), "uncommitted work\n").unwrap();

        let outcome = reclaim_spec_worktrees(repo.clone(), spec_id(), root.clone())
            .await
            .expect("reclaim runs");

        assert_eq!(
            outcome.skipped_dirty,
            vec![task.clone()],
            "the dirty task worktree is reported as skipped",
        );
        assert!(task.join("README.md").is_file(), "dirty dir survives");
        assert_eq!(
            outcome.removed,
            vec![integration.clone()],
            "the clean integration worktree is still reclaimed",
        );
        assert!(!integration.exists(), "integration dir gone");
        assert_eq!(
            registration_names(&repo),
            vec![task_worktree_name(&spec_id(), &task_id())],
            "the dirty worktree keeps its registration; the clean one is pruned",
        );
        assert!(
            root.join(spec_id().as_str()).exists(),
            "spec dir kept — it still holds the skipped worktree",
        );
    }

    /// A STALE registration — a `spec-<SpecId>-*` admin entry whose directory
    /// is already gone (the audit-C1 pollution class: 15 dead entries in a
    /// live operator repo) — is pruned even though there is nothing on disk
    /// to remove for it.
    #[tokio::test]
    async fn test_l2_reclaim_prunes_stale_registrations_for_missing_dirs() {
        let dir = TempDir::new("stale");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        let root = dir.path.join("worktrees");
        let (integration, task) = build_spec_worktrees(&repo, &root);

        // Simulate the leak: the task DIRECTORY vanished (a manual `rm -rf`)
        // but its registration survived.
        std::fs::remove_dir_all(&task).unwrap();
        assert_eq!(registration_names(&repo).len(), 2, "stale entry present");

        let outcome = reclaim_spec_worktrees(repo.clone(), spec_id(), root.clone())
            .await
            .expect("reclaim runs");

        assert_eq!(
            outcome.pruned_registrations,
            vec![task_worktree_name(&spec_id(), &task_id())],
            "the stale registration is pruned and reported",
        );
        assert!(!integration.exists(), "integration reclaimed too");
        assert!(
            registration_names(&repo).is_empty(),
            "no spec registrations survive",
        );
    }

    /// A spec with nothing on disk is a cheap no-op — the sweeper hits this
    /// path on every pass for already-reclaimed specs.
    #[tokio::test]
    async fn test_l2_reclaim_is_a_noop_when_nothing_is_on_disk() {
        let dir = TempDir::new("noop");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        let root = dir.path.join("worktrees");

        let outcome = reclaim_spec_worktrees(repo, spec_id(), root)
            .await
            .expect("reclaim runs");
        assert!(outcome.is_noop(), "nothing to do: {outcome:?}");
    }
}
