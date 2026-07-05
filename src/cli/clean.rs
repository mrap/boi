//! `boi clean` — the per-spec retention / teardown command (design §11 + §5).
//!
//! Two shapes:
//!
//! - `boi clean <spec_id> [--force]` — worktree reclamation (audit C1) THEN
//!   the full cascade delete. Design §5 promises "worktrees stay until
//!   `boi clean`" — so `boi clean` must actually take them: the spec's
//!   worktree directories under `~/.boi/v2/worktrees/<SpecId>/` are removed
//!   and their `.git/worktrees/` registrations pruned from the operator
//!   workspace (`runtime::reclaim`), with a dirty-check skip (audit A1's
//!   lesson: never silently destroy uncommitted work). Disk runs FIRST, rows
//!   second — the failure mode of the old order (rows deleted, gigabytes
//!   orphaned with no spec↔directory mapping) was audit C1's headline. The
//!   **terminal-state safety check** lives in `repo::clean::clean_spec` (the
//!   guard is a storage invariant); this CLI ALSO pre-checks it before
//!   touching disk — a non-terminal spec must not lose its worktrees either.
//!   `--force` routes to `repo::clean::clean_spec_forced` (and skips the
//!   pre-check, matching the cascade's force semantics).
//! - `boi clean <spec_id> --phase-runs-older-than <dur>` — the retention
//!   prune: delete only this spec's completed `phase_runs` rows older than
//!   `<dur>` (a `humantime`-style duration; a malformed value is loud). No
//!   worktree reclamation — the spec stays.
//!
//! `boi clean` mutates the DB directly — but only DELETEs, and only of a spec
//! the daemon is no longer running (the terminal-state guard, or the operator's
//! explicit `--force`). It is not a control-socket command.

use std::path::{Path, PathBuf};

use chrono::{Duration, Utc};

use crate::cli::paths::{self, PathError};
use crate::repo;
use crate::repo::clean::CleanReport;
use crate::repo::db::RepoError;
use crate::service::sweeper::ReclaimOutcome;
use crate::types::ids::SpecId;
use crate::types::state::SpecStatus;

/// A management command (`clean` / `spec show`) failed.
#[derive(Debug, thiserror::Error)]
pub enum ManageError {
    /// The `~/.boi/v2/` path layout could not be resolved.
    #[error(transparent)]
    Path(#[from] PathError),
    /// A repo-layer query failed.
    #[error("{0}")]
    Repo(#[from] RepoError),
    /// The id argument was not a well-formed spec id.
    #[error("invalid spec id: {0}")]
    BadId(String),
    /// `--phase-runs-older-than` could not be parsed as a duration.
    #[error("invalid --phase-runs-older-than duration `{got}`: {detail}")]
    BadDuration {
        /// The unparseable duration string.
        got: String,
        /// The parser's message.
        detail: String,
    },
    /// The worktree reclamation could not run at all (audit C1). The row
    /// cascade was NOT executed — the spec↔directory mapping survives so the
    /// operator can retry after fixing the fault.
    #[error("worktree reclamation failed (no DB rows were deleted): {0}")]
    Reclaim(String),
}

/// Everything one `boi clean <spec_id>` did — the row cascade plus the
/// audit-C1 worktree reclamation evidence, for `run` to print.
#[derive(Debug)]
pub struct CleanSummary {
    /// Per-table delete counts from the cascade.
    pub report: CleanReport,
    /// What the worktree reclamation did. `None` only when
    /// `workspace_unresolved` explains why it could not run.
    pub reclaim: Option<ReclaimOutcome>,
    /// Why the workspace could not be resolved from the spec snapshot (so no
    /// registration pruning / reclamation ran). Printed LOUDLY — the
    /// worktree directories then survive under the worktree root.
    pub workspace_unresolved: Option<String>,
}

/// `boi clean <spec_id>`'s full-cascade arm, parameterized for tests: the
/// worktree reclamation (audit C1) followed by the design-§11 row cascade.
///
/// Order is deliberate:
/// 1. **Terminal-state pre-check** (skipped by `force`, matching the
///    cascade's force semantics) — never touch a live spec's worktrees.
/// 2. **Workspace resolution** from the spec's current snapshot — must
///    happen BEFORE the cascade deletes `spec_versions` (the audit-C1 bug:
///    rows deleted first leave gigabytes with no mapping).
/// 3. **Disk + registrations** (`runtime::reclaim`) — dirty worktrees are
///    skipped and reported, never destroyed (audit A1's lesson). A wholesale
///    reclaim fault ABORTS before any row is deleted.
/// 4. **Row cascade** — semantics unchanged from design §11 / plan Task 3.10.
pub async fn clean_spec_with_reclaim(
    pool: &sqlx::SqlitePool,
    spec_id: &SpecId,
    force: bool,
    worktree_root: &Path,
) -> Result<CleanSummary, ManageError> {
    // (1) — the terminal-state pre-check (the cascade re-checks atomically;
    // this copy exists so DISK is never touched for a live spec).
    let runtime_row = repo::spec_runtime::fetch(pool, spec_id).await?;
    if !force {
        let parsed: SpecStatus = runtime_row.status.parse().map_err(|e| {
            ManageError::Repo(RepoError::NotFound(format!(
                "spec {spec_id} has a corrupt status: {e}"
            )))
        })?;
        if matches!(parsed, SpecStatus::Queued | SpecStatus::Running) {
            return Err(ManageError::Repo(RepoError::SpecNotTerminal {
                spec_id: spec_id.to_string(),
                status: runtime_row.status,
            }));
        }
    }

    // (2) — workspace resolution, BEFORE any row is deleted. An
    // unresolvable workspace (missing/corrupt snapshot) downgrades to a
    // LOUD "not reclaimed" warning rather than blocking the row clean —
    // matching the command's pre-C1 reach while never failing silently.
    // (3) — disk + registrations. A wholesale reclaim fault ABORTS before
    // the cascade: deleting the rows that map directories to specs while
    // leaving the gigabytes was exactly audit C1's complaint.
    let (reclaim, workspace_unresolved): (Option<ReclaimOutcome>, Option<String>) =
        match resolve_workspace(pool, spec_id, runtime_row.current_version).await {
            Ok(workspace) => {
                let outcome = crate::runtime::reclaim::reclaim_spec_worktrees(
                    workspace,
                    spec_id.clone(),
                    worktree_root.to_path_buf(),
                )
                .await
                .map_err(|e| ManageError::Reclaim(e.to_string()))?;
                (Some(outcome), None)
            }
            Err(why) => (None, Some(why)),
        };

    // (4) — the row cascade, semantics unchanged.
    let report = if force {
        repo::clean::clean_spec_forced(pool, spec_id).await?
    } else {
        // `clean_spec`'s terminal-state guard re-checks in-transaction — a
        // `RepoError::SpecNotTerminal` surfaces here as a loud error.
        repo::clean::clean_spec(pool, spec_id).await?
    };
    Ok(CleanSummary {
        report,
        reclaim,
        workspace_unresolved,
    })
}

/// Resolve the spec's workspace path from its CURRENT snapshot
/// (`spec_versions` → `spec_contract.workspace`) — read BEFORE the cascade
/// deletes the snapshot rows. Any failure is returned as the human-readable
/// `why` for [`CleanSummary::workspace_unresolved`].
async fn resolve_workspace(
    pool: &sqlx::SqlitePool,
    spec_id: &SpecId,
    current_version: i64,
) -> Result<PathBuf, String> {
    let snapshot = repo::spec_versions::fetch_snapshot(pool, spec_id, current_version)
        .await
        .map_err(|e| format!("cannot load spec snapshot v{current_version}: {e}"))?;
    let contract = snapshot
        .get("spec_contract")
        .ok_or_else(|| "snapshot has no `spec_contract` key".to_owned())?;
    let contract: crate::types::context::SpecContract = serde_json::from_value(contract.clone())
        .map_err(|e| format!("`spec_contract` is malformed: {e}"))?;
    Ok(contract.workspace)
}

/// Run `boi clean`.
///
/// Dispatches on `--phase-runs-older-than`: present → the retention prune;
/// absent → the full cascade (`--force` skips the terminal-state guard).
pub async fn run(
    spec_id: &str,
    force: bool,
    phase_runs_older_than: Option<&str>,
) -> Result<(), ManageError> {
    let sid = SpecId::new(spec_id).map_err(|e| ManageError::BadId(e.to_string()))?;
    let db_url = paths::boi_db_url()?;
    let pool = repo::connect(&db_url).await?;

    match phase_runs_older_than {
        Some(dur_str) => {
            let threshold = parse_duration(dur_str)?;
            let report =
                repo::clean::clean_phase_runs_older_than(&pool, &sid, threshold, Utc::now())
                    .await?;
            println!(
                "Pruned {} phase_runs row(s) for {spec_id} older than {dur_str}.",
                report.phase_runs_deleted,
            );
        }
        None => {
            let worktree_root = crate::runtime::worktree::default_worktree_root();
            let summary = clean_spec_with_reclaim(&pool, &sid, force, &worktree_root).await?;
            print_summary(spec_id, &worktree_root, &summary);
        }
    }
    Ok(())
}

/// Print one full-cascade clean's evidence: the reclamation results (with
/// every dirty skip and per-directory fault LOUD on stderr — SO S6) and the
/// row-cascade counts.
fn print_summary(spec_id: &str, worktree_root: &Path, summary: &CleanSummary) {
    if let Some(why) = &summary.workspace_unresolved {
        eprintln!(
            "WARNING: could not resolve the spec's workspace — worktrees were NOT \
             reclaimed ({why}). Any leftover directories remain under {}/{spec_id}/.",
            worktree_root.display(),
        );
    }
    if let Some(reclaim) = &summary.reclaim {
        if !reclaim.removed.is_empty() {
            println!("Reclaimed {} worktree dir(s):", reclaim.removed.len());
            for path in &reclaim.removed {
                println!("  removed {}", path.display());
            }
        }
        if !reclaim.pruned_registrations.is_empty() {
            println!(
                "Pruned {} stale git worktree registration(s): {}",
                reclaim.pruned_registrations.len(),
                reclaim.pruned_registrations.join(", "),
            );
        }
        for path in &reclaim.skipped_dirty {
            eprintln!(
                "WARNING: SKIPPED dirty worktree {} — it has uncommitted changes. \
                 Commit/stash what you need, then remove it manually.",
                path.display(),
            );
        }
        for (path, why) in &reclaim.failed {
            eprintln!(
                "ERROR: could not reclaim {} — {why}. The directory (if any) was \
                 left in place.",
                path.display(),
            );
        }
    }
    println!(
        "Cleaned {spec_id}: {} phase_runs, {} task_runtime, {} spec_versions, \
         {} decisions deleted.",
        summary.report.phase_runs_deleted,
        summary.report.task_runtime_deleted,
        summary.report.spec_versions_deleted,
        summary.report.decisions_deleted,
    );
}

/// Parse a `humantime`-style duration (`90d`, `2w`, …) into a
/// [`chrono::Duration`].
///
/// `humantime` yields a `std::time::Duration`; the repo retention API takes a
/// `chrono::Duration`. A malformed input is a loud [`ManageError::BadDuration`]
/// — never a silent default window.
fn parse_duration(s: &str) -> Result<Duration, ManageError> {
    let std_dur = humantime::parse_duration(s).map_err(|e| ManageError::BadDuration {
        got: s.to_owned(),
        detail: e.to_string(),
    })?;
    Duration::from_std(std_dur).map_err(|e| ManageError::BadDuration {
        got: s.to_owned(),
        detail: format!("duration out of range: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `parse_duration` accepts the plan's short forms and rejects garbage
    /// loudly.
    #[test]
    fn test_l1_parse_duration_accepts_short_forms_rejects_garbage() {
        assert_eq!(
            parse_duration("90d").unwrap(),
            Duration::days(90),
            "`90d` is 90 days",
        );
        assert_eq!(parse_duration("2w").unwrap(), Duration::weeks(2));
        let err = parse_duration("not-a-duration").unwrap_err();
        assert!(
            matches!(err, ManageError::BadDuration { .. }),
            "garbage is a loud BadDuration, got {err:?}",
        );
    }
}
