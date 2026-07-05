//! [`RuntimeToolHost`] — the `runtime/` adapter for the [`WorkerToolHost`]
//! port (Task 4.4).
//!
//! ## Ports and adapters — the adapter side
//!
//! `service/mcp.rs` defined the [`WorkerToolHost`] port — the runtime
//! capability behind the `verify_run` and `worktree_diff` MCP tools (`service/`
//! may not spawn a subprocess or call `git2`; LDA §13). `RuntimeToolHost` is
//! that adapter: `verify_run` → [`validate::run_command`], `worktree_diff` →
//! [`git_ops::diff_against`].
//!
//! ## Resolving `task_id` → worktree
//!
//! A `task_id` resolves to a worktree path via the §5 layout: `task_runtime`
//! (read through `repo`) gives the task's `spec_id`; the path is then
//! `worktree::task_worktree(root, spec_id, task_id)`. A `task_id` with no
//! `task_runtime` row → a loud [`ToolHostError`] ("no worktree"); a resolved
//! path whose directory is absent → the same. `worktree_diff` diffs the task
//! worktree against the integration branch — the task branch's base (§5).

use std::path::PathBuf;

use async_trait::async_trait;
use sqlx::SqlitePool;

use crate::repo;
use crate::runtime::{git_ops, validate, worktree};
use crate::service::mcp::{ToolHostError, VerificationOutput, WorkerToolHost};
use crate::types::ids::{SpecId, TaskId};
use tokio_util::sync::CancellationToken;

/// The `runtime/` adapter behind the `verify_run` / `worktree_diff` MCP tools.
pub struct RuntimeToolHost {
    /// The SQLite pool — used to resolve a `task_id` to its `spec_id`.
    pool: SqlitePool,
    /// The §5 worktree root (`~/.boi/v2/worktrees` in production).
    worktree_root: PathBuf,
}

impl RuntimeToolHost {
    /// Construct the tool host with the production worktree root
    /// (`~/.boi/v2/worktrees`).
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            worktree_root: worktree::default_worktree_root(),
        }
    }

    /// Construct the tool host with an explicit worktree root — a test seam
    /// (the same pattern as `DeterministicExecutor::with_worktree_root`).
    pub fn with_worktree_root(pool: SqlitePool, worktree_root: PathBuf) -> Self {
        Self {
            pool,
            worktree_root,
        }
    }

    /// Resolve a `task_id` to `(spec_id, task_worktree_path)`.
    ///
    /// A `task_id` with no `task_runtime` row, or whose worktree directory does
    /// not exist on disk, yields a loud [`ToolHostError`] — never a silent
    /// empty result (no-quiet-failures).
    async fn resolve_worktree(&self, task_id: &TaskId) -> Result<(SpecId, PathBuf), ToolHostError> {
        let row = repo::task_runtime::fetch(&self.pool, task_id)
            .await
            .map_err(|e| {
                // A missing row IS the "no worktree" case — the worktree-scoped
                // tools fail loud rather than acting on a nonexistent task.
                ToolHostError(format!("no worktree for task {task_id}: {e}"))
            })?;
        let spec_id = SpecId::new(&row.spec_id)
            .map_err(|e| ToolHostError(format!("task {task_id} has an invalid spec id: {e}")))?;
        let path = worktree::task_worktree(&self.worktree_root, &spec_id, task_id);
        if !path.is_dir() {
            return Err(ToolHostError(format!(
                "no worktree for task {task_id}: {} does not exist",
                path.display()
            )));
        }
        Ok((spec_id, path))
    }
}

#[async_trait]
impl WorkerToolHost for RuntimeToolHost {
    /// Run a verification command in the task's worktree — delegates to
    /// [`validate::run_command`].
    async fn run_verification(
        &self,
        task_id: &TaskId,
        command: &str,
    ) -> Result<VerificationOutput, ToolHostError> {
        let (_spec_id, worktree_path) = self.resolve_worktree(task_id).await?;
        // The MCP `verify_run` tool has no cancellation channel of its own —
        // run the command under a fresh, never-fired token.
        let output = validate::run_command(&worktree_path, command, &CancellationToken::new())
            .await
            .map_err(|e| ToolHostError(format!("verification command failed to run: {e}")))?;
        Ok(VerificationOutput {
            exit_code: output.exit_code,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    /// Return the `git diff` of the task's worktree against the task branch
    /// base (the integration branch, §5) — delegates to
    /// [`git_ops::diff_against`].
    async fn worktree_diff(&self, task_id: &TaskId) -> Result<String, ToolHostError> {
        let (spec_id, worktree_path) = self.resolve_worktree(task_id).await?;
        let base = worktree::integration_branch(&spec_id);
        // `git2` blocks — run the diff on the blocking pool.
        let diff = tokio::task::spawn_blocking(move || git_ops::diff_against(&worktree_path, &base))
                .await
                // Recover the panic payload, not the bare "task panicked" (NIT).
                .map_err(|e| {
                    ToolHostError(format!(
                        "worktree diff task failed: {}",
                        worktree::join_error_detail(e),
                    ))
                })?
                .map_err(|e| ToolHostError(format!("worktree diff failed: {e}")))?;
        Ok(diff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::db::connect;
    use crate::repo::specs::insert_spec;
    use crate::repo::task_runtime::insert_task;
    use crate::types::ids::{SpecId, TaskId};
    use chrono::Utc;
    use git2::{Repository, Signature};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A throwaway directory removed on drop — `std`-only.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("boi-tool-host-{}-{tag}-{n}", std::process::id()));
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

    /// Init a source repo with one commit; create the integration branch and a
    /// task worktree under `worktree_root`. Returns nothing — the side effect
    /// is the on-disk worktree the host will resolve.
    fn seed_worktree(repo_path: &Path, worktree_root: &Path) {
        let repo = Repository::init(repo_path).expect("init");
        let sig = Signature::now("test", "test@localhost").unwrap();
        std::fs::write(repo_path.join("README.md"), "hello\n").unwrap();
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
        // Integration branch + worktree, then a task branch + worktree — the §5
        // chain the host's resolve_worktree expects.
        let integration = worktree::integration_branch(&spec_id());
        git_ops::create_branch(repo_path, &integration, "main").unwrap();
        git_ops::add_worktree(
            repo_path,
            &integration,
            &worktree::integration_worktree_name(&spec_id()),
            &worktree::integration_worktree(worktree_root, &spec_id()),
        )
        .unwrap();
        let task_branch = worktree::task_branch(&spec_id(), &task_id());
        git_ops::create_branch(repo_path, &task_branch, &integration).unwrap();
        git_ops::add_worktree(
            repo_path,
            &task_branch,
            &worktree::task_worktree_name(&spec_id(), &task_id()),
            &worktree::task_worktree(worktree_root, &spec_id(), &task_id()),
        )
        .unwrap();
    }

    /// A pool seeded with the spec + task rows so `resolve_worktree`'s
    /// `task_runtime::fetch` succeeds.
    async fn seeded_pool() -> SqlitePool {
        let pool = connect("sqlite::memory:").await.unwrap();
        insert_spec(&pool, &spec_id(), Utc::now()).await.unwrap();
        insert_task(&pool, &task_id(), &spec_id(), Some("setup"))
            .await
            .unwrap();
        pool
    }

    /// `run_verification` against a seeded task worktree returns the command
    /// output.
    #[tokio::test]
    async fn test_l2_run_verification_against_a_seeded_worktree() {
        let dir = TempDir::new("run-verify");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let worktree_root = dir.path.join("worktrees");
        seed_worktree(&repo, &worktree_root);

        let host = RuntimeToolHost::with_worktree_root(seeded_pool().await, worktree_root);
        let out = host
            .run_verification(&task_id(), "echo verified")
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.contains("verified"), "stdout: {:?}", out.stdout);
    }

    /// `worktree_diff` returns a non-empty diff after an edit in the worktree.
    #[tokio::test]
    async fn test_l2_worktree_diff_returns_a_nonempty_diff_after_an_edit() {
        let dir = TempDir::new("diff");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let worktree_root = dir.path.join("worktrees");
        seed_worktree(&repo, &worktree_root);

        // Edit a tracked file in the task worktree.
        let task_wt = worktree::task_worktree(&worktree_root, &spec_id(), &task_id());
        std::fs::write(task_wt.join("README.md"), "hello\nan edit\n").unwrap();

        let host = RuntimeToolHost::with_worktree_root(seeded_pool().await, worktree_root);
        let diff = host.worktree_diff(&task_id()).await.unwrap();
        assert!(
            diff.contains("an edit"),
            "the diff must show the edit, got: {diff}",
        );
    }

    /// An unknown `task_id` (no `task_runtime` row) → a loud `ToolHostError`.
    #[tokio::test]
    async fn test_l2_unknown_task_id_is_a_loud_no_worktree_error() {
        let dir = TempDir::new("unknown-task");
        let host = RuntimeToolHost::with_worktree_root(
            connect("sqlite::memory:").await.unwrap(),
            dir.path.join("worktrees"),
        );
        // The task has no `task_runtime` row at all.
        let unknown = TaskId::new("T000000zz").unwrap();
        let err = host.run_verification(&unknown, "echo x").await.unwrap_err();
        assert!(
            err.0.contains("no worktree"),
            "an unknown task must fail loudly with a no-worktree error, got: {}",
            err.0,
        );
        // worktree_diff fails the same way.
        let err = host.worktree_diff(&unknown).await.unwrap_err();
        assert!(err.0.contains("no worktree"), "got: {}", err.0);
    }

    /// A `task_id` with a row but no on-disk worktree → a loud error.
    #[tokio::test]
    async fn test_l2_missing_worktree_directory_is_a_loud_error() {
        let dir = TempDir::new("missing-wt");
        // The pool has the task row, but no worktree was ever created on disk.
        let host =
            RuntimeToolHost::with_worktree_root(seeded_pool().await, dir.path.join("worktrees"));
        let err = host
            .run_verification(&task_id(), "echo x")
            .await
            .unwrap_err();
        assert!(
            err.0.contains("no worktree") && err.0.contains("does not exist"),
            "a missing worktree dir must fail loudly, got: {}",
            err.0,
        );
    }
}
