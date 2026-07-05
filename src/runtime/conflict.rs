//! Interactive merge-conflict resolution — the runtime side of
//! `boi resolve-conflict` (Phase 9 Task 9.6).
//!
//! `boi resolve-conflict <task>` drops the operator into a shell to fix a
//! `MergeConflict`-blocked task by hand. The subprocess spawning — the `git
//! rebase` that re-creates the conflict and the interactive shell itself —
//! MUST live in `runtime/` (`no-subprocess-outside-runtime.sh`); `cli/` only
//! decides *when* to call this.
//!
//! ## Why a `git` CLI subprocess, not `git2`
//!
//! Phase 6's `git_ops::rebase_onto` deliberately **aborts** a conflicted
//! rebase (review S7 — a half-finished rebase corrupts the worktree for later
//! steps). To let an operator *resolve* the conflict, it must be re-created and
//! **left in place** — which `git2`'s rebase API does not cleanly support
//! mid-operation. Shelling out to `git rebase` re-creates the conflict with
//! the rebase genuinely in progress, exactly the state `git rebase --continue`
//! expects after the operator edits the files. `runtime/` is the only layer
//! allowed to spawn subprocesses, so this is the correct home.

use std::path::Path;

use tokio::process::Command;

/// An interactive conflict-resolution session failed.
#[derive(Debug, thiserror::Error)]
pub enum ConflictError {
    /// A `git` / shell subprocess could not be spawned or waited on.
    #[error("conflict-resolution subprocess failed: {0}")]
    Spawn(String),
    /// The task worktree does not exist on disk.
    #[error("task worktree {0} does not exist")]
    NoWorktree(String),
}

/// How an interactive resolution session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// The operator resolved every conflict — the worktree is clean and no
    /// rebase is in progress. The caller emits `TaskUnblocked`.
    Resolved,
    /// The worktree is still dirty or a rebase is still in progress — a
    /// half-resolved conflict. The caller leaves the task blocked.
    StillConflicted {
        /// What was still wrong (for the operator-facing message).
        detail: String,
    },
}

/// Re-create a task's merge conflict, drop the operator into an interactive
/// shell, and verify the result.
///
/// Steps:
///
/// 1. `git -C <worktree> rebase <integration>` — re-creates the conflict
///    (Phase 6's `rebase_onto` aborted the original). A non-zero exit here is
///    *expected* — it means the conflict is back, in progress.
/// 2. Print the conflicted files.
/// 3. Spawn the operator's `$SHELL` (or `/bin/sh`) in the worktree, inheriting
///    stdio — this is the interactive session.
/// 4. On shell exit, verify: `git status` reports a clean tree AND no rebase
///    is in progress. Clean → [`ResolveOutcome::Resolved`]; otherwise
///    [`ResolveOutcome::StillConflicted`].
///
/// `worktree` is the conflicted task's worktree path; `integration` is the
/// integration branch the task failed to merge into.
pub async fn resolve_interactively(
    worktree: &Path,
    integration: &str,
) -> Result<ResolveOutcome, ConflictError> {
    if !worktree.exists() {
        return Err(ConflictError::NoWorktree(worktree.display().to_string()));
    }

    // (1) — re-create the conflict. A non-zero exit is the expected outcome
    // (the rebase stopped on the conflict); only a *spawn* failure is an error.
    let rebase = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .arg("rebase")
        .arg(integration)
        .status()
        .await
        .map_err(|e| ConflictError::Spawn(format!("`git rebase {integration}` failed: {e}")))?;
    if rebase.success() {
        // No conflict after all — the integration branch already merges
        // cleanly. Nothing to resolve; the task can be unblocked.
        return Ok(ResolveOutcome::Resolved);
    }

    // (2) — show the conflicted files.
    let conflicts = conflicted_files(worktree).await?;
    println!("Merge conflict in {}:", worktree.display());
    for file in &conflicts {
        println!("  {file}");
    }
    println!(
        "\nResolve the conflicts, `git add` the files, then `git rebase --continue`.\n\
         Exit the shell when done.\n"
    );

    // (3) — the interactive shell, stdio inherited.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
    let shell_status = Command::new(&shell)
        .current_dir(worktree)
        .status()
        .await
        .map_err(|e| ConflictError::Spawn(format!("interactive shell `{shell}` failed: {e}")))?;
    tracing::debug!(shell = %shell, code = ?shell_status.code(), "resolve-conflict shell exited");

    // (4) — verify. A half-resolved rebase re-blocks (the plan's postcondition).
    verify_resolved(worktree).await
}

/// The list of conflict-marked files in `worktree`, via
/// `git diff --name-only --diff-filter=U`.
async fn conflicted_files(worktree: &Path) -> Result<Vec<String>, ConflictError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["diff", "--name-only", "--diff-filter=U"])
        .output()
        .await
        .map_err(|e| ConflictError::Spawn(format!("`git diff` failed: {e}")))?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

/// Verify the worktree is fully resolved: a clean working tree AND no rebase
/// in progress.
async fn verify_resolved(worktree: &Path) -> Result<ResolveOutcome, ConflictError> {
    // A rebase still in progress leaves `.git/rebase-merge` / `rebase-apply`.
    let rebase_dir = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["rev-parse", "--git-path", "rebase-merge"])
        .output()
        .await
        .map_err(|e| ConflictError::Spawn(format!("`git rev-parse` failed: {e}")))?;
    let rebase_path = String::from_utf8_lossy(&rebase_dir.stdout)
        .trim()
        .to_owned();
    if !rebase_path.is_empty() {
        let abs = worktree.join(&rebase_path);
        if abs.exists() {
            return Ok(ResolveOutcome::StillConflicted {
                detail: "a rebase is still in progress — run `git rebase --continue`".to_owned(),
            });
        }
    }

    // `git status --porcelain` empty ⇒ a clean working tree.
    let status = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["status", "--porcelain"])
        .output()
        .await
        .map_err(|e| ConflictError::Spawn(format!("`git status` failed: {e}")))?;
    let dirty = !String::from_utf8_lossy(&status.stdout).trim().is_empty();
    if dirty {
        return Ok(ResolveOutcome::StillConflicted {
            detail: "the worktree still has uncommitted changes".to_owned(),
        });
    }

    Ok(ResolveOutcome::Resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `resolve_interactively` against a non-existent worktree is a loud
    /// [`ConflictError::NoWorktree`] — never a panic.
    #[tokio::test]
    async fn test_l2_resolve_missing_worktree_is_loud() {
        let err = resolve_interactively(Path::new("/nonexistent/worktree"), "spec/S/integration")
            .await
            .unwrap_err();
        assert!(matches!(err, ConflictError::NoWorktree(_)), "got {err:?}");
    }

    /// `verify_resolved` reports `Resolved` for a clean worktree with no rebase
    /// in progress. Built against a real throwaway git repo.
    #[tokio::test]
    async fn test_l2_verify_resolved_clean_worktree() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("boi-conflict-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // A minimal git repo with one committed file.
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap()
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("f.txt"), "hello").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "init"]);

        let outcome = verify_resolved(&dir).await.unwrap();
        assert_eq!(
            outcome,
            ResolveOutcome::Resolved,
            "a clean repo is resolved"
        );

        // A dirty worktree → StillConflicted.
        std::fs::write(dir.join("f.txt"), "changed").unwrap();
        let dirty = verify_resolved(&dir).await.unwrap();
        assert!(
            matches!(dirty, ResolveOutcome::StillConflicted { .. }),
            "a dirty worktree is still-conflicted, got {dirty:?}",
        );

        drop(std::fs::remove_dir_all(&dir));
    }
}
