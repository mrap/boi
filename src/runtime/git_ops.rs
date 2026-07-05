//! The lowest-level `git2` layer — branch / worktree / merge / rebase / diff
//! primitives. Every other Phase-6 module calls it; nothing here knows about
//! phases or specs.
//!
//! ## Blocking calls
//!
//! `git2` is a *linked library*, not a subprocess — but its calls block the
//! thread. Every `git_ops` function is therefore synchronous; the callers
//! (`worktree.rs`, `tool_host.rs`) wrap each call in
//! [`tokio::task::spawn_blocking`] (the Phase 6 preamble rule). This module
//! keeps the `git2` surface in one place and never reaches `git` over the CLI.
//!
//! ## `rebase_onto` postconditions (review S7)
//!
//! The clean path loops every rebase operation to completion then calls
//! `Rebase::finish`. The conflict path collects the conflicted paths AND calls
//! `Rebase::abort` — a conflicted rebase left in-progress corrupts the worktree
//! for every later step. After [`rebase_onto`] returns
//! [`RebaseOutcome::Conflicts`] the worktree is back on its original branch in
//! [`git2::RepositoryState::Clean`].

use std::path::{Path, PathBuf};

use git2::{
    AnnotatedCommit, BranchType, ErrorCode, Repository, RepositoryState, Signature, StatusOptions,
};

/// A `git_ops` operation failed.
///
/// A thin typed wrapper over [`git2::Error`] plus the operations that have no
/// `git2` error of their own (a missing worktree directory). Phase 6's
/// `worktree.rs` converts this to `StepError::Git(e.to_string())` at its own
/// boundary — `StepError` (a `types/` layer-0 type) cannot `#[from]` a
/// `runtime/` error (G14.1).
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// The underlying `git2` (libgit2) call failed.
    #[error("libgit2: {0}")]
    Libgit2(#[from] git2::Error),
    /// A path argument did not point at a usable git repository / worktree.
    #[error("not a git path: {0}")]
    BadPath(String),
    /// A fast-forward was REFUSED because the checkout holding the branch
    /// being advanced — MAIN or LINKED, either can be the operator's own
    /// (review M1 finding 1: BOI never checks out `[contract].base_branch`
    /// in a worktree it creates, so ANY checkout of the merge target is
    /// foreign) — has changes the post-merge forced sync would destroy
    /// (audit A1 / OBS-030): tracked modifications, or untracked files at
    /// paths the merge introduces (review M1 finding 4 — untracked files
    /// NOT on an incoming path survive a forced checkout and do not refuse).
    /// Nothing was mutated when this is returned: the branch ref did not
    /// move, the dirty files are intact, and the merged work remains on the
    /// `from` branch.
    #[error(
        "refusing to fast-forward '{branch}': the checkout at {} has \
         uncommitted changes that the post-merge forced sync would destroy \
         — commit or stash them; nothing was mutated, the merged work \
         remains on the source branch",
        path.display()
    )]
    TargetCheckoutDirty {
        /// The branch the fast-forward would have advanced.
        branch: String,
        /// The dirty checkout holding that branch.
        path: PathBuf,
    },
}

/// The result of a fast-forward merge attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The `into` branch was fast-forwarded to `from`. Carries the merged
    /// commit's `Oid` — the SHA `into` now points at (G25.2; a bare unit
    /// variant would have discarded it).
    FastForwarded(git2::Oid),
    /// `from` is not strictly ahead of `into` — a fast-forward is impossible.
    NotFastForwardable,
}

/// The result of a rebase attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseOutcome {
    /// Every operation applied cleanly; the rebase was finished.
    Clean,
    /// The rebase hit conflicts — it was ABORTED and the worktree is back on
    /// its original branch. Carries the conflicted paths.
    Conflicts(Vec<PathBuf>),
}

/// The committer/author identity used for rebase-replayed commits.
///
/// A rebase replays existing commits; `git_rebase_commit` still needs a
/// *committer* signature. `git2`'s `Repository::signature()` reads `user.name`
/// / `user.email` from git config, which a throwaway CI repo may not set — so
/// `rebase_onto` falls back to this fixed identity rather than failing.
fn rebase_signature(repo: &Repository) -> Result<Signature<'static>, GitError> {
    match repo.signature() {
        Ok(sig) => Ok(sig),
        // No `user.name`/`user.email` configured — use a fixed BOI identity so
        // a rebase is never blocked by missing git config.
        Err(_) => Ok(Signature::now("boi", "boi@localhost")?),
    }
}

/// Open the repository that owns `path` (a repo root or a worktree).
fn open(path: &Path) -> Result<Repository, GitError> {
    if !path.exists() {
        return Err(GitError::BadPath(format!(
            "{} does not exist",
            path.display()
        )));
    }
    Repository::open(path).map_err(GitError::from)
}

/// Create a branch `name` pointing at the commit `from_ref` resolves to.
///
/// `from_ref` is any revision spec git understands — a branch name, a tag, a
/// SHA. The branch is created non-force; a duplicate name is a `GitError`.
pub fn create_branch(repo: &Path, name: &str, from_ref: &str) -> Result<(), GitError> {
    let repo = open(repo)?;
    let object = repo.revparse_single(from_ref)?;
    let commit = object.peel_to_commit()?;
    repo.branch(name, &commit, false)?;
    Ok(())
}

/// Whether the local branch `name` exists in the repository owning `repo`.
///
/// The re-entry probe for `worktree::verify_in` (audit A2 / design §6
/// recovery): a surviving task branch means `boi unblock` restarted the task
/// and the §5 state must be ADOPTED, not re-created. Only `NotFound` maps to
/// `Ok(false)` — any other libgit2 failure stays a loud `Err`.
pub fn branch_exists(repo: &Path, name: &str) -> Result<bool, GitError> {
    let repo = open(repo)?;
    match repo.find_branch(name, BranchType::Local) {
        Ok(_) => Ok(true),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(false),
        Err(e) => Err(GitError::from(e)),
    }
}

/// The branch name checked out at the worktree `worktree` (HEAD's shorthand,
/// e.g. `spec/<SpecId>/<TaskId>`).
///
/// Used by `worktree::verify_in`'s re-entry adoption (audit A2) to confirm a
/// surviving worktree really is the task branch's checkout — a detached or
/// foreign HEAD is corrupt re-entry state and must fail truthfully.
pub fn head_branch(worktree: &Path) -> Result<String, GitError> {
    let repo = open(worktree)?;
    let head = repo.head()?;
    head.shorthand().map(str::to_owned).ok_or_else(|| {
        GitError::BadPath(format!(
            "HEAD at {} is not a named branch",
            worktree.display()
        ))
    })
}

/// Add a worktree for `branch` at `worktree_path`.
///
/// The branch must already exist (call [`create_branch`] first). The worktree's
/// name is derived from the last path component. The new worktree is checked
/// out onto the existing `branch` reference.
///
/// libgit2's `git_worktree_add` does NOT create intermediate parent
/// directories — the `<worktree_root>/<SpecId>/` parent is created here first
/// so `~/.boi/v2/worktrees/<SpecId>/integration` works on a fresh root.
pub fn add_worktree(
    repo: &Path,
    branch: &str,
    name: &str,
    worktree_path: &Path,
) -> Result<(), GitError> {
    // BUG-FIX 2026-05-24 (OBS-023): previously this derived `name` from
    // `worktree_path.file_name()`, which collided for every spec because BOI's
    // integration worktree layout is `<root>/<SpecId>/integration` — the
    // basename is always `"integration"`, so the second-and-onward specs hit
    // libgit2 `directory exists` on `.git/worktrees/integration/`. Callers now
    // pass an explicit, spec-scoped `name` (see
    // `worktree::integration_worktree_name` /
    // `worktree::task_worktree_name`).
    let repo = open(repo)?;
    // libgit2 will not `mkdir -p` — create the parent chain ourselves.
    if let Some(parent) = worktree_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            GitError::BadPath(format!(
                "creating worktree parent {}: {e}",
                parent.display()
            ))
        })?;
    }
    // Point the worktree at the (already-created) branch reference.
    let reference = repo
        .find_branch(branch, BranchType::Local)?
        .into_reference();
    let mut opts = git2::WorktreeAddOptions::new();
    opts.reference(Some(&reference));
    repo.worktree(name, worktree_path, Some(&opts))?;
    Ok(())
}

/// Remove a worktree: delete its working directory and prune the admin entry.
///
/// Idempotent on a missing directory — a teardown that runs twice (or after a
/// crash) does not fail. The libgit2 admin entry under `.git/worktrees/` is
/// pruned with `valid(true)` so a still-present-on-disk check does not block
/// the prune.
pub fn remove_worktree(repo: &Path, name: &str, worktree_path: &Path) -> Result<(), GitError> {
    // BUG-FIX 2026-05-24 (OBS-023): see `add_worktree`'s note. The `name` is
    // now an explicit caller-supplied registration name (matching what
    // `add_worktree` was given), not a path-basename derivation.
    //
    // Remove the working directory first (best-effort: a missing dir is fine).
    if worktree_path.exists() {
        std::fs::remove_dir_all(worktree_path)
            .map_err(|e| GitError::BadPath(format!("removing {}: {e}", worktree_path.display())))?;
    }
    let repo = open(repo)?;
    // The worktree may already be gone from libgit2's admin area; only
    // prune one we can still open.
    if let Ok(worktree) = repo.find_worktree(name) {
        let mut prune = git2::WorktreePruneOptions::new();
        prune.valid(true).working_tree(true);
        worktree.prune(Some(&mut prune))?;
    }
    Ok(())
}

/// Fast-forward `into` to `from`, if `from` is strictly ahead.
///
/// Uses `merge_analysis` against `from`'s commit. On `ANALYSIS_FASTFORWARD` (or
/// `ANALYSIS_UP_TO_DATE`) the `into` branch reference is moved to `from`'s
/// commit, the worktree that has `into` checked out (if any) is advanced to
/// that commit — a fast-forward must update the working tree, not just the ref
/// — and [`MergeOutcome::FastForwarded`] is returned **carrying the merged
/// commit's `Oid`** — the SHA `into` now points at (G25.2). Anything else —
/// a genuine 3-way merge would be needed — returns
/// [`MergeOutcome::NotFastForwardable`] WITHOUT mutating any ref.
pub fn ff_merge(repo: &Path, into: &str, from: &str) -> Result<MergeOutcome, GitError> {
    let repo = open(repo)?;
    let from_commit = repo
        .find_branch(from, BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let merged_oid = from_commit.id();
    let from_annotated: AnnotatedCommit<'_> = repo.find_annotated_commit(merged_oid)?;
    let (analysis, _) = repo.merge_analysis_for_ref(
        &repo.find_branch(into, BranchType::Local)?.into_reference(),
        &[&from_annotated],
    )?;

    if analysis.is_up_to_date() {
        // `into` already contains `from` — treat as a (no-op) fast-forward.
        // The merged commit is `from`'s tip (== `into`'s tip).
        return Ok(MergeOutcome::FastForwarded(merged_oid));
    }
    if analysis.is_fast_forward() {
        // Locate the checkout holding `into` BEFORE any mutation: the A1
        // guard below must refuse with NOTHING moved — moving the ref first
        // and then refusing would strand that checkout in exactly the
        // bug-#3 phantom-staged-revert state.
        let checked_out = checked_out_worktree_for(&repo, into)?;
        // A1 guard (OBS-030 — fired twice in production, wiping uncommitted
        // operator edits), covering EVERY checkout of `into` — main or
        // linked. The original guard matched the MAIN worktree only, on the
        // premise that a linked worktree on the merge path is BOI-created
        // and clean by §5. Review M1 finding 1 disproved it: BOI only ever
        // creates worktrees on `spec/<id>/integration` / `spec/<id>/<task>`
        // branches, never on `[contract].base_branch`, so a LINKED worktree
        // holding the merge target is operator-created — and the unguarded
        // force-sync destroyed its uncommitted edits (empirically
        // reproduced). BOI-owned checkouts (`merge_to_integration`'s
        // integration worktree) still pass: they are clean of tracked
        // modifications at every merge point (§5 — `workspace_verify_out`
        // precedes the merge, and a lazy rebase leaves the worktree clean),
        // and `sync_would_destroy` ignores untracked files unless the merge
        // would overwrite them (review M1 finding 4 — a forced checkout
        // does not touch untracked files off the incoming paths, and the
        // operator workspace carries untracked files in steady state, so
        // counting them made `delivery = "merge"` permanently inoperable).
        // Refuse LOUDLY instead of syncing: `into` does not move and the
        // merged work stays intact on `from`.
        if let Some(path) = &checked_out {
            if sync_would_destroy(path, merged_oid)? {
                return Err(GitError::TargetCheckoutDirty {
                    branch: into.to_owned(),
                    path: path.clone(),
                });
            }
        }
        // Move the `into` branch reference forward to `from`'s commit.
        {
            let mut into_ref = repo.find_branch(into, BranchType::Local)?.into_reference();
            into_ref.set_target(merged_oid, "boi: ff-merge")?;
        }
        // Moving the ref is only half a fast-forward: the worktree that has
        // `into` checked out must advance to the new commit too. Leaving its
        // index + working tree frozen at the pre-merge commit makes git report
        // a phantom staged revert there — the bug the spec-level `review`
        // phase caught (bug #3).
        if let Some(worktree) = checked_out {
            sync_worktree(&worktree, merged_oid)?;
        }
        return Ok(MergeOutcome::FastForwarded(merged_oid));
    }
    Ok(MergeOutcome::NotFastForwardable)
}

/// The working-directory path of the checkout — main OR linked worktree —
/// that currently has `branch` checked out, or `Ok(None)` if it is checked
/// out nowhere.
///
/// Git allows a branch to be checked out in at most one worktree, so at most
/// one path matches. [`ff_merge`] uses this to guard (A1) and advance that
/// worktree when a fast-forward moves the branch ref. The main/linked KIND
/// is deliberately NOT distinguished any more: the A1 guard once keyed off
/// it ("linked ⇒ BOI-created ⇒ clean by §5"), a premise review M1 finding 1
/// disproved — an operator's linked checkout of the base branch is exactly
/// as foreign as the main one, so every match is guarded identically. A
/// failure to enumerate the linked worktrees propagates — never a silent
/// "found nothing".
fn checked_out_worktree_for(repo: &Repository, branch: &str) -> Result<Option<PathBuf>, GitError> {
    let want = format!("refs/heads/{branch}");
    let head_ref = |r: &Repository| -> Option<String> {
        r.head().ok().and_then(|h| h.name().map(str::to_owned))
    };
    // The main worktree.
    if head_ref(repo).as_deref() == Some(want.as_str()) {
        return Ok(repo.workdir().map(Path::to_path_buf));
    }
    // Linked worktrees — skip any whose directory no longer opens (a removed
    // but unpruned worktree cannot, and need not, be synced).
    for name in repo.worktrees()?.iter().flatten() {
        let Ok(worktree) = repo.find_worktree(name) else {
            continue;
        };
        if let Ok(wt_repo) = Repository::open(worktree.path()) {
            if head_ref(&wt_repo).as_deref() == Some(want.as_str()) {
                return Ok(Some(worktree.path().to_path_buf()));
            }
        }
    }
    Ok(None)
}

/// Whether the forced checkout that syncs `checkout` to `target` would
/// destroy work in it — the A1 guard's predicate (review M1 findings 1+4).
///
/// Exactly two things are destroyed by `CheckoutBuilder::force()`:
///
/// 1. ANY tracked change — staged or unstaged modification, deletion,
///    rename, typechange, conflict — is reset to `target`'s content
///    (OBS-030, the class that wiped uncommitted operator edits twice in
///    production).
/// 2. An UNTRACKED file whose path exists in `target`'s tree — the checkout
///    writes the incoming blob over it.
///
/// Untracked files OFF the incoming paths survive a forced checkout, so they
/// do NOT count (review M1 finding 4): the operator workspace carries
/// untracked files in steady state, and counting them — as the original
/// guard's [`is_clean`] did — refused every default-delivery merge into it,
/// terminally. Deliberately NOT a reuse of [`is_clean`], whose
/// include-untracked semantics serve the §5 "no orphans" worktree invariant,
/// a different question.
fn sync_would_destroy(checkout: &Path, target: git2::Oid) -> Result<bool, GitError> {
    let repo = open(checkout)?;
    let mut opts = StatusOptions::new();
    // Recurse untracked directories: collisions INSIDE a new directory must
    // be seen individually (the default lists `dir/` as one opaque entry).
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);
    let statuses = repo.statuses(Some(&mut opts))?;
    if statuses.is_empty() {
        return Ok(false);
    }
    let target_tree = repo.find_commit(target)?.tree()?;
    for entry in statuses.iter() {
        if entry.status() == git2::Status::WT_NEW {
            // Purely untracked — destroyed only if the merge introduces a
            // file at this exact path. A non-UTF-8 path cannot be checked
            // against the tree: refuse conservatively rather than risk the
            // silent overwrite (SO S6 — loud beats lossy).
            let Some(p) = entry.path() else {
                return Ok(true);
            };
            if target_tree.get_path(Path::new(p)).is_ok() {
                return Ok(true);
            }
        } else {
            // Any index or working-tree change to a TRACKED file — the
            // forced checkout resets it unconditionally.
            return Ok(true);
        }
    }
    Ok(false)
}

/// Advance `worktree`'s working tree + index to `commit` (a forced checkout).
///
/// Called by [`ff_merge`] after a fast-forward: the worktree that had the
/// branch checked out is otherwise left frozen at the pre-merge commit, which
/// git reports as a phantom staged revert. The forced checkout discards
/// nothing here because [`ff_merge`] has already established the target is
/// safe: EVERY checkout of the merged branch — main or linked, no §5
/// ownership assumption (review M1 finding 1) — was verified by the A1
/// guard's [`sync_would_destroy`] (a destructive sync refuses the merge
/// before any ref moves; OBS-030).
fn sync_worktree(worktree: &Path, commit: git2::Oid) -> Result<(), GitError> {
    let repo = open(worktree)?;
    let object = repo.find_object(commit, None)?;
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.force();
    repo.checkout_tree(&object, Some(&mut checkout))?;
    Ok(())
}

/// Rebase the branch currently checked out in `worktree` onto `onto`.
///
/// On a clean rebase every operation is committed and `Rebase::finish` is
/// called → [`RebaseOutcome::Clean`]. On a conflict the conflicted paths are
/// collected from the rebase's in-memory index, the rebase is **aborted**
/// (review S7 — never left in-progress), and [`RebaseOutcome::Conflicts`] is
/// returned with the worktree back on its original branch.
pub fn rebase_onto(worktree: &Path, onto: &str) -> Result<RebaseOutcome, GitError> {
    let repo = open(worktree)?;
    let signature = rebase_signature(&repo)?;

    // `branch = None` → rebase HEAD; `upstream`/`onto` = the target.
    let onto_commit = repo
        .find_branch(onto, BranchType::Local)?
        .into_reference()
        .peel_to_commit()?;
    let onto_annotated = repo.find_annotated_commit(onto_commit.id())?;
    let mut rebase = repo.rebase(None, Some(&onto_annotated), None, None)?;

    // `rebase.next()` yields one operation at a time; the borrow it returns is
    // dropped at the `?` so `rebase` is free to mutate immediately after.
    while let Some(operation) = rebase.next() {
        operation?;
        // A conflict shows up as conflicts in the rebase's in-memory index.
        let index = repo.index()?;
        if index.has_conflicts() {
            let conflicts = collect_conflicts(&index)?;
            // Abort — a conflicted rebase left in-progress corrupts the
            // worktree for every later step (review S7).
            rebase.abort()?;
            return Ok(RebaseOutcome::Conflicts(conflicts));
        }
        rebase.commit(None, &signature, None)?;
    }
    rebase.finish(Some(&signature))?;
    Ok(RebaseOutcome::Clean)
}

/// Collect the conflicted paths from an index that `has_conflicts()`.
///
/// Each conflict carries up to three sides (ancestor / our / their); the first
/// non-`None` side's path identifies the file.
fn collect_conflicts(index: &git2::Index) -> Result<Vec<PathBuf>, GitError> {
    let mut paths = Vec::new();
    for conflict in index.conflicts()? {
        let conflict = conflict?;
        let entry = conflict.our.or(conflict.their).or(conflict.ancestor);
        if let Some(entry) = entry {
            // Index paths are stored as raw bytes, slash-separated.
            let path = String::from_utf8_lossy(&entry.path).into_owned();
            paths.push(PathBuf::from(path));
        }
    }
    Ok(paths)
}

/// Return the `git diff` of `worktree` against `base_ref`, as a unified-diff
/// string.
///
/// Diffs `base_ref`'s tree against the worktree's working directory (tracked
/// changes only), in the standard `git diff` patch format.
pub fn diff_against(worktree: &Path, base_ref: &str) -> Result<String, GitError> {
    let repo = open(worktree)?;
    let base_tree = repo.revparse_single(base_ref)?.peel_to_tree()?;
    let diff = repo.diff_tree_to_workdir_with_index(Some(&base_tree), None)?;

    let mut out = String::new();
    diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
        // `origin` is the diff-line prefix (' ', '+', '-', or a header marker).
        match line.origin() {
            '+' | '-' | ' ' => out.push(line.origin()),
            _ => {}
        }
        out.push_str(&String::from_utf8_lossy(line.content()));
        true
    })?;
    Ok(out)
}

/// Whether `worktree` is clean — no uncommitted, staged, or untracked changes.
///
/// Untracked files DO count as unclean: a clean-state invariant (§5) means
/// "build passes, tests pass, no orphans", and an orphan file is unclean.
pub fn is_clean(worktree: &Path) -> Result<bool, GitError> {
    let repo = open(worktree)?;
    let mut opts = StatusOptions::new();
    opts.include_untracked(true).include_ignored(false);
    let statuses = repo.statuses(Some(&mut opts))?;
    Ok(statuses.is_empty())
}

/// Whether `worktree`'s repository is in [`RepositoryState::Clean`] — no
/// half-finished merge/rebase/cherry-pick in progress.
///
/// Distinct from [`is_clean`] (which is about the *working tree*); this is
/// about the *repository state machine*. Used by the `rebase_onto` regression
/// test to assert the abort postcondition.
pub fn repository_state_is_clean(worktree: &Path) -> Result<bool, GitError> {
    let repo = open(worktree)?;
    Ok(repo.state() == RepositoryState::Clean)
}

/// Whether `error` is a "reference / object not found" error — useful for
/// callers distinguishing "missing" from "genuinely broken".
pub fn is_not_found(error: &GitError) -> bool {
    matches!(
        error,
        GitError::Libgit2(e) if e.code() == ErrorCode::NotFound
    )
}

/// Whether `error` is an "HEAD has no commit yet" error — the legitimate
/// first-commit case, distinct from a genuinely-corrupt HEAD.
///
/// `Repository::head()` on a fresh repo returns `ErrorCode::UnbornBranch`;
/// peeling a HEAD whose ref/target is missing returns `ErrorCode::NotFound`.
/// BOTH mean "there is no parent commit" — a caller about to write a commit
/// treats either as a `None` parent (a root commit). Anything else (a corrupt
/// object DB, a permissions error) is a genuine failure that must NOT be
/// collapsed to "no parent" — see `worktree::commit_all` (review C-rt-S2).
pub fn is_unborn_head(error: &GitError) -> bool {
    matches!(
        error,
        GitError::Libgit2(e)
            if e.code() == ErrorCode::UnbornBranch || e.code() == ErrorCode::NotFound
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A throwaway directory under the system temp dir, removed on drop.
    /// `std`-only — avoids pulling in `tempfile`.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("boi-git-ops-{}-{tag}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.path));
        }
    }

    /// A fixed signature for test commits.
    fn sig() -> Signature<'static> {
        Signature::now("test", "test@localhost").unwrap()
    }

    /// Write `content` to `path/name`, stage it, and commit on the current
    /// branch. Returns the new commit's `Oid`.
    fn commit_file(repo: &Repository, name: &str, content: &str, message: &str) -> git2::Oid {
        let workdir = repo.workdir().expect("non-bare repo");
        std::fs::write(workdir.join(name), content).expect("write file");
        let mut index = repo.index().expect("index");
        index.add_path(Path::new(name)).expect("add path");
        index.write().expect("write index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        let parents: Vec<git2::Commit<'_>> = match repo.head() {
            Ok(head) => vec![head.peel_to_commit().expect("head commit")],
            Err(_) => vec![], // first commit — unborn HEAD
        };
        let parent_refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig(), &sig(), message, &tree, &parent_refs)
            .expect("commit")
    }

    /// Initialise a repo with one commit on `main`. Returns the repo.
    fn init_repo(path: &Path) -> Repository {
        let repo = Repository::init(path).expect("init repo");
        commit_file(&repo, "README.md", "hello\n", "initial");
        // Ensure the default branch is named `main`.
        if repo.find_branch("main", BranchType::Local).is_err() {
            let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
            repo.branch("main", &head_commit, true).unwrap();
            repo.set_head("refs/heads/main").unwrap();
        }
        repo
    }

    #[test]
    fn test_l2_create_add_remove_worktree_roundtrip() {
        let dir = TempDir::new("wt-roundtrip");
        let repo_path = dir.path.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        init_repo(&repo_path);

        // create_branch off main, then add a worktree for it.
        create_branch(&repo_path, "feature", "main").unwrap();
        let wt_path = dir.path.join("wt-feature");
        add_worktree(&repo_path, "feature", "feature-wt", &wt_path).unwrap();
        assert!(wt_path.join("README.md").is_file(), "worktree checked out");

        // remove_worktree deletes the directory.
        remove_worktree(&repo_path, "feature-wt", &wt_path).unwrap();
        assert!(!wt_path.exists(), "worktree directory removed");
        // Idempotent — a second remove does not fail.
        remove_worktree(&repo_path, "feature-wt", &wt_path).unwrap();
    }

    /// `branch_exists` distinguishes a present branch from an absent one and
    /// only maps `NotFound` to `Ok(false)` (audit A2 — the verify_in re-entry
    /// probe).
    #[test]
    fn test_l2_branch_exists_true_for_present_false_for_absent() {
        let dir = TempDir::new("branch-exists");
        let repo_path = dir.path.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        init_repo(&repo_path);

        assert!(branch_exists(&repo_path, "main").unwrap(), "main exists");
        assert!(
            !branch_exists(&repo_path, "no-such-branch").unwrap(),
            "an absent branch is Ok(false), not an error",
        );
        // A bad repo path stays a loud error.
        assert!(branch_exists(Path::new("/no/such/repo"), "main").is_err());
    }

    /// `head_branch` names the branch a worktree has checked out (audit A2 —
    /// the verify_in adoption sanity check).
    #[test]
    fn test_l2_head_branch_names_the_checked_out_branch() {
        let dir = TempDir::new("head-branch");
        let repo_path = dir.path.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        init_repo(&repo_path);

        create_branch(&repo_path, "spec/S1/T1", "main").unwrap();
        let wt_path = dir.path.join("wt");
        add_worktree(&repo_path, "spec/S1/T1", "spec-S1-task-T1", &wt_path).unwrap();

        assert_eq!(head_branch(&repo_path).unwrap(), "main");
        assert_eq!(
            head_branch(&wt_path).unwrap(),
            "spec/S1/T1",
            "the worktree's HEAD shorthand is the full branch name",
        );
    }

    /// OBS-023 regression test (2026-05-24). Two worktrees at paths whose
    /// final component is identical (the BOI integration layout —
    /// `<root>/<SpecId>/integration`) must both succeed when given distinct
    /// explicit names. Before the fix, the second `add_worktree` failed with
    /// `directory exists; class=Filesystem (30); code=Exists (-4)`, blocking
    /// every BOI spec after the first.
    #[test]
    fn test_l2_add_worktree_uses_explicit_name_not_path_basename() {
        let dir = TempDir::new("wt-explicit-name");
        let repo_path = dir.path.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        init_repo(&repo_path);

        create_branch(&repo_path, "feature-a", "main").unwrap();
        create_branch(&repo_path, "feature-b", "main").unwrap();

        // Same `file_name()` ("integration"), different explicit names.
        let wt_a = dir.path.join("spec-a").join("integration");
        let wt_b = dir.path.join("spec-b").join("integration");
        add_worktree(&repo_path, "feature-a", "spec-a-integration", &wt_a)
            .expect("first add succeeds");
        add_worktree(&repo_path, "feature-b", "spec-b-integration", &wt_b).expect(
            "second add with distinct name succeeds — pre-fix would error with `directory exists`",
        );

        assert!(wt_a.join("README.md").is_file(), "wt_a checked out");
        assert!(wt_b.join("README.md").is_file(), "wt_b checked out");

        // Registrations live in `.git/worktrees/<name>/`.
        assert!(
            repo_path.join(".git/worktrees/spec-a-integration").is_dir(),
            "spec-a-integration registration exists"
        );
        assert!(
            repo_path.join(".git/worktrees/spec-b-integration").is_dir(),
            "spec-b-integration registration exists"
        );

        // Both must be removable by their explicit names.
        remove_worktree(&repo_path, "spec-a-integration", &wt_a).unwrap();
        remove_worktree(&repo_path, "spec-b-integration", &wt_b).unwrap();
        assert!(!wt_a.exists());
        assert!(!wt_b.exists());
    }

    #[test]
    fn test_l2_ff_merge_fast_forwards_a_strictly_ahead_branch() {
        let dir = TempDir::new("ff-ahead");
        let repo_path = dir.path.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        let repo = init_repo(&repo_path);

        // `feature` is `main` + one commit — strictly ahead.
        create_branch(&repo_path, "feature", "main").unwrap();
        repo.set_head("refs/heads/feature").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        commit_file(&repo, "feature.txt", "work\n", "feature work");

        let outcome = ff_merge(&repo_path, "main", "feature").unwrap();
        // `main` now points at `feature`'s commit.
        let main_id = repo
            .find_branch("main", BranchType::Local)
            .unwrap()
            .into_reference()
            .peel_to_commit()
            .unwrap()
            .id();
        let feature_id = repo
            .find_branch("feature", BranchType::Local)
            .unwrap()
            .into_reference()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(main_id, feature_id);
        // G25.2 — `FastForwarded` carries the merged commit `Oid`, and it is
        // exactly the SHA `main` (the `into` branch) now points at.
        let MergeOutcome::FastForwarded(merged_oid) = outcome else {
            unreachable!("a strictly-ahead branch fast-forwards, got {outcome:?}");
        };
        assert_eq!(
            merged_oid, main_id,
            "the FastForwarded Oid must be the merged commit SHA (G25.2)",
        );
    }

    /// Regression for bug #3 — a fast-forward must sync the worktree that has
    /// `into` checked out, not merely move the branch ref.
    ///
    /// `merge_to_integration` fast-forwarded `spec/<id>/integration` but the
    /// integration *worktree* stayed frozen at the pre-merge commit: its index
    /// and working tree still held the old blob while HEAD (the moved ref) held
    /// the new one, so `git status` reported a phantom staged revert — which
    /// the spec-level `review` worker correctly refused to pass. The OLD
    /// `ff_merge` moved the ref and stopped; this test inspects the *linked
    /// worktree*'s file content and cleanliness, which the OLD code fails.
    #[test]
    fn test_l2_ff_merge_syncs_the_checked_out_worktree() {
        let dir = TempDir::new("ff-sync-wt");
        let repo_path = dir.path.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        let repo = init_repo(&repo_path);

        // `integration` branches off main and is checked out in its OWN linked
        // worktree — exactly `workspace_prepare`'s setup.
        create_branch(&repo_path, "integration", "main").unwrap();
        let integration_wt = dir.path.join("integration-wt");
        add_worktree(&repo_path, "integration", "integration-wt", &integration_wt).unwrap();

        // `feature` = main + one commit that edits the tracked README.
        create_branch(&repo_path, "feature", "main").unwrap();
        repo.set_head("refs/heads/feature").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        commit_file(&repo, "README.md", "fixed content\n", "fix the readme");

        // Fast-forward integration → feature.
        let outcome = ff_merge(&repo_path, "integration", "feature").unwrap();
        assert!(
            matches!(outcome, MergeOutcome::FastForwarded(_)),
            "a strictly-ahead branch fast-forwards, got {outcome:?}",
        );

        // The integration LINKED WORKTREE must reflect the merge: file content
        // updated AND no phantom staged revert.
        assert_eq!(
            std::fs::read_to_string(integration_wt.join("README.md")).unwrap(),
            "fixed content\n",
            "ff_merge must advance the integration worktree's working tree, \
             not just move the branch ref (bug #3)",
        );
        assert!(
            is_clean(&integration_wt).unwrap(),
            "the integration worktree must be clean after a fast-forward — a \
             stale index/working tree is the phantom staged revert of bug #3",
        );
    }

    /// Audit A1 regression (OBS-030 — fired twice in production, 2026-06-07
    /// and 2026-06-10, wiping uncommitted operator edits). A fast-forward
    /// whose `into` branch is checked out in the MAIN worktree — the
    /// OPERATOR's own checkout, which §5's clean-state invariants do NOT
    /// cover — must REFUSE to proceed while that checkout is dirty, mutating
    /// NOTHING: the dirty edit survives, `into` does not move, and the
    /// merged work stays intact on `from`.
    #[test]
    fn test_l2_ff_merge_refuses_a_dirty_main_checkout_without_mutating_anything() {
        let dir = TempDir::new("ff-dirty-main");
        let repo_path = dir.path.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        let repo = init_repo(&repo_path);

        // `feature` = main + one commit, built in a LINKED worktree so the
        // MAIN checkout stays on `main` throughout (`merge_spec`'s exact
        // shape: workspace on base_branch, work on a BOI branch).
        create_branch(&repo_path, "feature", "main").unwrap();
        let feature_wt = dir.path.join("feature-wt");
        add_worktree(&repo_path, "feature", "feature-wt", &feature_wt).unwrap();
        let feature_repo = Repository::open(&feature_wt).unwrap();
        commit_file(
            &feature_repo,
            "feature.txt",
            "merged work\n",
            "feature work",
        );

        // The operator edits a tracked file in the MAIN checkout — uncommitted.
        std::fs::write(repo_path.join("README.md"), "uncommitted operator edit\n").unwrap();
        let main_before = repo
            .find_branch("main", BranchType::Local)
            .unwrap()
            .into_reference()
            .peel_to_commit()
            .unwrap()
            .id();

        let err = ff_merge(&repo_path, "main", "feature")
            .expect_err("a dirty main checkout must refuse the fast-forward (A1)");
        assert!(
            matches!(err, GitError::TargetCheckoutDirty { .. }),
            "expected TargetCheckoutDirty, got {err:?}",
        );
        // The refusal is actionable: it names the dirty checkout's path and
        // the recovery ("commit or stash").
        let msg = err.to_string();
        assert!(
            msg.contains(&repo_path.display().to_string()),
            "the refusal must name the dirty checkout: {msg}",
        );
        assert!(
            msg.contains("commit or stash"),
            "the refusal must name the recovery: {msg}",
        );

        // NOTHING was mutated: the operator's edit is byte-identical, and
        // `main` did not move.
        assert_eq!(
            std::fs::read_to_string(repo_path.join("README.md")).unwrap(),
            "uncommitted operator edit\n",
            "the operator's uncommitted edit must be untouched",
        );
        let main_after = repo
            .find_branch("main", BranchType::Local)
            .unwrap()
            .into_reference()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(main_before, main_after, "`main` must not move on a refusal");
    }

    /// The clean-side companion of the A1 guard: a CLEAN main checkout is
    /// still fast-forwarded AND synced (the bug-#3 behaviour must survive the
    /// guard) — the merged file appears in the operator's working tree and no
    /// phantom staged revert is left behind.
    #[test]
    fn test_l2_ff_merge_syncs_a_clean_main_checkout() {
        let dir = TempDir::new("ff-clean-main");
        let repo_path = dir.path.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        init_repo(&repo_path);

        create_branch(&repo_path, "feature", "main").unwrap();
        let feature_wt = dir.path.join("feature-wt");
        add_worktree(&repo_path, "feature", "feature-wt", &feature_wt).unwrap();
        let feature_repo = Repository::open(&feature_wt).unwrap();
        commit_file(
            &feature_repo,
            "feature.txt",
            "merged work\n",
            "feature work",
        );

        let outcome = ff_merge(&repo_path, "main", "feature").unwrap();
        assert!(
            matches!(outcome, MergeOutcome::FastForwarded(_)),
            "a clean main checkout fast-forwards, got {outcome:?}",
        );
        assert_eq!(
            std::fs::read_to_string(repo_path.join("feature.txt")).unwrap(),
            "merged work\n",
            "the main checkout must be synced to the merged tree (bug #3)",
        );
        assert!(
            is_clean(&repo_path).unwrap(),
            "the main checkout must be clean after the synced fast-forward",
        );
    }

    /// Review M1 finding 1 (critical — empirically reproduced, the exact
    /// OBS-030 class): a dirty LINKED checkout of the target branch must
    /// refuse the fast-forward exactly like a dirty MAIN checkout.
    ///
    /// The original A1 guard matched `CheckedOutWorktree::Main` only, on the
    /// premise that "a Linked worktree on the merge path is always
    /// BOI-created (§5)". That premise is FALSE for `merge_spec`'s path: BOI
    /// only ever creates worktrees on `spec/<id>/integration` /
    /// `spec/<id>/<task>` branches, never on `[contract].base_branch` — so a
    /// linked worktree holding the base branch is FOREIGN (operator-created,
    /// e.g. main checkout on `dev` + `git worktree add ../repo-main main`,
    /// the all-work-in-worktrees layout). The unguarded force-sync silently
    /// destroyed its uncommitted edits.
    #[test]
    fn test_l2_ff_merge_refuses_a_dirty_linked_checkout_of_the_target_branch() {
        let dir = TempDir::new("ff-dirty-linked");
        let repo_path = dir.path.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        let repo = init_repo(&repo_path);

        // The operator's layout: MAIN checkout parked on `dev`, `main` held
        // in an operator-created LINKED worktree.
        create_branch(&repo_path, "dev", "main").unwrap();
        repo.set_head("refs/heads/dev").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let main_wt = dir.path.join("main-wt");
        add_worktree(&repo_path, "main", "main-wt", &main_wt).unwrap();

        // `feature` = main + one commit, built in its own worktree.
        create_branch(&repo_path, "feature", "main").unwrap();
        let feature_wt = dir.path.join("feature-wt");
        add_worktree(&repo_path, "feature", "feature-wt", &feature_wt).unwrap();
        let feature_repo = Repository::open(&feature_wt).unwrap();
        commit_file(
            &feature_repo,
            "feature.txt",
            "merged work\n",
            "feature work",
        );

        // The operator edits a tracked file in the linked `main` checkout —
        // uncommitted (the reproduced data-loss state).
        std::fs::write(main_wt.join("README.md"), "uncommitted operator edit\n").unwrap();
        let main_before = repo
            .find_branch("main", BranchType::Local)
            .unwrap()
            .into_reference()
            .peel_to_commit()
            .unwrap()
            .id();

        let err = ff_merge(&repo_path, "main", "feature")
            .expect_err("a dirty linked checkout of the target branch must refuse (M1 finding 1)");
        assert!(
            matches!(err, GitError::TargetCheckoutDirty { .. }),
            "expected TargetCheckoutDirty, got {err:?}",
        );
        // The refusal names the dirty checkout — the LINKED worktree's path.
        assert!(
            err.to_string().contains(&main_wt.display().to_string()),
            "the refusal must name the dirty linked checkout: {err}",
        );

        // NOTHING was mutated: the edit is byte-identical, `main` unmoved.
        assert_eq!(
            std::fs::read_to_string(main_wt.join("README.md")).unwrap(),
            "uncommitted operator edit\n",
            "the operator's uncommitted edit in the linked checkout must survive",
        );
        let main_after = repo
            .find_branch("main", BranchType::Local)
            .unwrap()
            .into_reference()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(main_before, main_after, "`main` must not move on a refusal");
    }

    /// Review M1 finding 4 (high): untracked files alone must NOT refuse the
    /// merge. The destruction the A1 guard prevents is the forced checkout's
    /// reset of TRACKED modifications — an untracked file survives the sync
    /// absent a path collision. The production workspace carries untracked
    /// files in steady state, so an include-untracked predicate made the
    /// default `delivery = "merge"` permanently inoperable there (a one-shot
    /// terminal spec failure even after the operator committed all tracked
    /// work).
    #[test]
    fn test_l2_ff_merge_proceeds_when_target_checkout_has_only_untracked_files() {
        let dir = TempDir::new("ff-untracked-only");
        let repo_path = dir.path.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        init_repo(&repo_path);

        create_branch(&repo_path, "feature", "main").unwrap();
        let feature_wt = dir.path.join("feature-wt");
        add_worktree(&repo_path, "feature", "feature-wt", &feature_wt).unwrap();
        let feature_repo = Repository::open(&feature_wt).unwrap();
        commit_file(
            &feature_repo,
            "feature.txt",
            "merged work\n",
            "feature work",
        );

        // Untracked file in the MAIN checkout at a path the merge does NOT
        // introduce — the steady-state operator workspace.
        std::fs::write(repo_path.join("notes.txt"), "operator scratch notes\n").unwrap();

        let outcome = ff_merge(&repo_path, "main", "feature")
            .expect("an untracked-only checkout must not refuse the merge (M1 finding 4)");
        assert!(
            matches!(outcome, MergeOutcome::FastForwarded(_)),
            "untracked-only checkout fast-forwards, got {outcome:?}",
        );
        // The merge landed AND the untracked file survived the forced sync.
        assert_eq!(
            std::fs::read_to_string(repo_path.join("feature.txt")).unwrap(),
            "merged work\n",
            "the checkout must be synced to the merged tree (bug #3 preserved)",
        );
        assert_eq!(
            std::fs::read_to_string(repo_path.join("notes.txt")).unwrap(),
            "operator scratch notes\n",
            "the untracked file must survive the forced sync untouched",
        );
    }

    /// Review M1 finding 4, the collision edge: an untracked file at a path
    /// the merge INTRODUCES would be overwritten by the forced sync — that is
    /// real destruction, so it must refuse. Staged on a LINKED checkout of
    /// the target branch so the refusal also exercises the finding-1 widening
    /// (the old code force-synced linked checkouts with no guard at all).
    #[test]
    fn test_l2_ff_merge_refuses_when_an_untracked_file_collides_with_an_incoming_path() {
        let dir = TempDir::new("ff-untracked-collision");
        let repo_path = dir.path.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        let repo = init_repo(&repo_path);

        // MAIN checkout on `dev`; `main` in an operator-created linked worktree.
        create_branch(&repo_path, "dev", "main").unwrap();
        repo.set_head("refs/heads/dev").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let main_wt = dir.path.join("main-wt");
        add_worktree(&repo_path, "main", "main-wt", &main_wt).unwrap();

        // `feature` introduces `feature.txt`.
        create_branch(&repo_path, "feature", "main").unwrap();
        let feature_wt = dir.path.join("feature-wt");
        add_worktree(&repo_path, "feature", "feature-wt", &feature_wt).unwrap();
        let feature_repo = Repository::open(&feature_wt).unwrap();
        commit_file(
            &feature_repo,
            "feature.txt",
            "merged work\n",
            "feature work",
        );

        // The operator has an UNTRACKED file at the very path the merge
        // introduces — the forced sync would overwrite it.
        std::fs::write(main_wt.join("feature.txt"), "operator draft — untracked\n").unwrap();
        let main_before = repo
            .find_branch("main", BranchType::Local)
            .unwrap()
            .into_reference()
            .peel_to_commit()
            .unwrap()
            .id();

        let err = ff_merge(&repo_path, "main", "feature")
            .expect_err("an untracked file colliding with an incoming path must refuse");
        assert!(
            matches!(err, GitError::TargetCheckoutDirty { .. }),
            "expected TargetCheckoutDirty, got {err:?}",
        );
        // The colliding untracked file is untouched and `main` did not move.
        assert_eq!(
            std::fs::read_to_string(main_wt.join("feature.txt")).unwrap(),
            "operator draft — untracked\n",
            "the colliding untracked file must survive the refusal",
        );
        let main_after = repo
            .find_branch("main", BranchType::Local)
            .unwrap()
            .into_reference()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(main_before, main_after, "`main` must not move on a refusal");
    }

    #[test]
    fn test_l2_ff_merge_reports_not_fast_forwardable_for_a_diverged_branch() {
        let dir = TempDir::new("ff-diverged");
        let repo_path = dir.path.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        let repo = init_repo(&repo_path);

        // `feature` diverges: commit on feature AND a different commit on main.
        create_branch(&repo_path, "feature", "main").unwrap();
        repo.set_head("refs/heads/feature").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        commit_file(&repo, "feature.txt", "feature side\n", "feature commit");

        repo.set_head("refs/heads/main").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        commit_file(&repo, "main.txt", "main side\n", "main commit");

        let outcome = ff_merge(&repo_path, "main", "feature").unwrap();
        assert_eq!(outcome, MergeOutcome::NotFastForwardable);
    }

    #[test]
    fn test_l2_rebase_onto_clean_replays_commits() {
        let dir = TempDir::new("rebase-clean");
        let repo_path = dir.path.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        let repo = init_repo(&repo_path);

        // feature: edits feature.txt; main: edits a DIFFERENT file → no conflict.
        create_branch(&repo_path, "feature", "main").unwrap();
        repo.set_head("refs/heads/feature").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        commit_file(&repo, "feature.txt", "feature work\n", "feature commit");

        repo.set_head("refs/heads/main").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        commit_file(&repo, "main.txt", "main work\n", "main commit");

        // Rebase feature onto main — no overlap, so it is clean.
        repo.set_head("refs/heads/feature").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let outcome = rebase_onto(&repo_path, "main").unwrap();
        assert_eq!(outcome, RebaseOutcome::Clean);
        assert!(
            repository_state_is_clean(&repo_path).unwrap(),
            "repo state Clean after a clean rebase",
        );
    }

    #[test]
    fn test_l2_rebase_onto_conflicts_aborts_and_leaves_clean_state() {
        let dir = TempDir::new("rebase-conflict");
        let repo_path = dir.path.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        let repo = init_repo(&repo_path);

        // BOTH branches edit the SAME file (README.md) → guaranteed conflict.
        create_branch(&repo_path, "feature", "main").unwrap();
        repo.set_head("refs/heads/feature").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        commit_file(&repo, "README.md", "feature version\n", "feature edit");

        repo.set_head("refs/heads/main").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        commit_file(&repo, "README.md", "main version\n", "main edit");

        repo.set_head("refs/heads/feature").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let outcome = rebase_onto(&repo_path, "main").unwrap();
        let RebaseOutcome::Conflicts(files) = outcome else {
            unreachable!("a same-file edit on both branches must conflict");
        };
        assert!(
            files.iter().any(|p| p.ends_with("README.md")),
            "README.md must be among the conflicted paths, got {files:?}",
        );
        // The abort postcondition (review S7): repo state back to Clean.
        assert!(
            repository_state_is_clean(&repo_path).unwrap(),
            "rebase_onto must abort a conflicted rebase — repo state must be Clean",
        );
    }

    #[test]
    fn test_l2_is_clean_true_on_clean_false_on_dirty() {
        let dir = TempDir::new("is-clean");
        let repo_path = dir.path.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        let _repo = init_repo(&repo_path);

        // Freshly-committed repo — clean.
        assert!(is_clean(&repo_path).unwrap(), "fresh repo is clean");

        // An untracked file makes it unclean (orphan = unclean).
        std::fs::write(repo_path.join("orphan.txt"), "stray\n").unwrap();
        assert!(
            !is_clean(&repo_path).unwrap(),
            "an untracked file makes the worktree unclean",
        );
    }

    #[test]
    fn test_l2_diff_against_shows_a_tracked_edit() {
        let dir = TempDir::new("diff");
        let repo_path = dir.path.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        let repo = init_repo(&repo_path);

        // Edit README.md (a tracked file) without committing.
        std::fs::write(repo_path.join("README.md"), "hello\nnew line\n").unwrap();
        let diff = diff_against(&repo_path, "main").unwrap();
        assert!(diff.contains("new line"), "diff shows the new line: {diff}");
        // The unmodified base ref produces an empty diff once committed.
        commit_file(&repo, "README.md", "hello\nnew line\n", "commit the edit");
        let after = diff_against(&repo_path, "HEAD").unwrap();
        assert!(after.is_empty(), "no diff against HEAD after a commit");
    }

    #[test]
    fn test_l1_bad_path_is_a_typed_error() {
        let err = create_branch(Path::new("/no/such/repo/path"), "x", "main").unwrap_err();
        assert!(matches!(err, GitError::BadPath(_)), "got {err:?}");
    }

    #[test]
    fn test_l1_is_not_found_classifies_a_missing_ref() {
        let dir = TempDir::new("not-found");
        let repo_path = dir.path.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        init_repo(&repo_path);
        // Branching off a ref that does not exist → a NotFound libgit2 error.
        let err = create_branch(&repo_path, "x", "no-such-ref").unwrap_err();
        assert!(is_not_found(&err), "missing ref must classify as not-found");
    }

    /// `is_unborn_head` classifies a fresh repo's `head()` error (the
    /// `UnbornBranch` code) as the legitimate no-parent case — distinct from
    /// `is_not_found`, which would NOT match `UnbornBranch` (review C-rt-S2).
    #[test]
    fn test_l1_is_unborn_head_classifies_a_fresh_repo_head() {
        let dir = TempDir::new("unborn-head");
        let repo_path = dir.path.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        // A freshly `init`-ed repo with NO commits — `head()` errors UnbornBranch.
        let repo = Repository::init(&repo_path).expect("init repo");
        let err = match repo.head() {
            Ok(_) => unreachable!("an unborn repo has no HEAD"),
            Err(e) => GitError::from(e),
        };
        assert!(
            is_unborn_head(&err),
            "a fresh repo's unborn HEAD must classify as unborn, got {err:?}",
        );
        // The narrower `is_not_found` does NOT cover UnbornBranch — which is
        // exactly why `commit_all` must use `is_unborn_head` (C-rt-S2): keying
        // only on `is_not_found` would misclassify a first commit as broken.
        assert!(
            !is_not_found(&err),
            "UnbornBranch is a distinct code from NotFound",
        );
    }
}
