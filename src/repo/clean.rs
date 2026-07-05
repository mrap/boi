//! `boi clean` — the per-spec cascade delete.
//!
//! Every FK in the schema is `ON DELETE RESTRICT`, so a clean must delete
//! child rows before their parents. [`clean_spec`] runs all seven deletions in
//! the design §11 cascade order inside ONE transaction — the atomic boundary
//! is inherently a repo concern, so it lives here.
//!
//! ## The terminal-status guard (A-SF-3)
//!
//! [`clean_spec`] refuses to delete a spec that is not in a terminal status —
//! cleaning a live `running` (or even `queued`) spec out from under the
//! running engine corrupts state. The check is a storage-integrity invariant,
//! not a leaked *workflow* policy, so it lives here. [`clean_spec_forced`] is
//! the explicit override Phase 9's `boi clean --force` calls.
//!
//! `specs` rows are pruned by [`clean_spec`] — note this differs from the
//! design's "never pruned" wording for the *audit-identity* use case; the
//! plan's Task 3.10 explicitly lists `specs` as the final cascade step, so a
//! full `clean_spec` removes the identity row too. (A retention-only clean
//! that keeps identity is [`clean_phase_runs_older_than`].)

use chrono::{DateTime, Duration, Utc};
use sqlx::SqlitePool;

use crate::repo::db::RepoError;
use crate::types::ids::SpecId;
use crate::types::state::SpecStatus;

/// Per-table delete counts from a clean operation.
///
/// One `u64` per table the clean can touch. The cascade order is observable in
/// these counts — a caller can confirm children were removed before parents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanReport {
    /// `decisions` rows deleted.
    pub decisions_deleted: u64,
    /// `phase_runs` rows deleted.
    pub phase_runs_deleted: u64,
    /// `task_deps` edges deleted.
    ///
    /// Not in the plan's `CleanReport` sketch — that struct predates the
    /// `task_deps` table (Batch A review L2). Added so the report genuinely
    /// covers "per-table counts across all 7 tables" (plan Task 3.10 exit).
    pub task_deps_deleted: u64,
    /// `task_runtime` rows deleted.
    pub task_runtime_deleted: u64,
    /// `spec_runtime` rows deleted.
    pub spec_runtime_deleted: u64,
    /// `spec_versions` rows deleted.
    pub spec_versions_deleted: u64,
    /// `specs` rows deleted.
    pub specs_deleted: u64,
}

/// Delete a spec and every row that depends on it, in FK-cascade order —
/// **only if the spec is in a terminal status**.
///
/// Reads `spec_runtime.status` first; if it is not terminal
/// (`completed`/`failed`/`canceled`) the function returns
/// [`RepoError::SpecNotTerminal`] and deletes nothing — cleaning a live spec
/// out from under the running engine corrupts state (A-SF-3). A spec with no
/// `spec_runtime` row at all is [`RepoError::NotFound`].
///
/// When the guard passes, the seven deletions run in one transaction in the
/// design §11 order. Either the whole cascade commits or none of it does. To
/// clean a non-terminal spec deliberately (Phase 9's `boi clean --force`), use
/// [`clean_spec_forced`].
pub async fn clean_spec(pool: &SqlitePool, spec_id: &SpecId) -> Result<CleanReport, RepoError> {
    let status = crate::repo::spec_runtime::fetch(pool, spec_id)
        .await?
        .status;
    let parsed: SpecStatus = status
        .parse()
        .map_err(|e| RepoError::NotFound(format!("spec {spec_id} has a corrupt status: {e}")))?;
    match parsed {
        SpecStatus::Completed | SpecStatus::Failed | SpecStatus::Canceled => {
            clean_spec_cascade(pool, spec_id).await
        }
        SpecStatus::Queued | SpecStatus::Running => Err(RepoError::SpecNotTerminal {
            spec_id: spec_id.to_string(),
            status,
        }),
    }
}

/// Delete a spec and its cascade **without** the terminal-status guard.
///
/// The escape hatch [`clean_spec`]'s guard does not allow — Phase 9's
/// `boi clean --force` calls this to clean a stuck non-terminal spec. Skips
/// only the status check; the cascade itself (FK order, single transaction)
/// is identical.
pub async fn clean_spec_forced(
    pool: &SqlitePool,
    spec_id: &SpecId,
) -> Result<CleanReport, RepoError> {
    clean_spec_cascade(pool, spec_id).await
}

/// The FK-ordered seven-table cascade delete, in one transaction.
///
/// Shared by [`clean_spec`] (after its terminal-status guard) and
/// [`clean_spec_forced`]. Deletions run in the design §11 order:
/// `decisions → phase_runs → task_deps → task_runtime → spec_runtime →
/// spec_versions → specs`. A failure mid-cascade is logged `warn!` with the
/// spec id and the error before it propagates (A-SF-4) — the transaction then
/// rolls back, so no partial delete survives, but the operator sees which
/// spec's clean failed and why.
async fn clean_spec_cascade(pool: &SqlitePool, spec_id: &SpecId) -> Result<CleanReport, RepoError> {
    match clean_spec_cascade_inner(pool, spec_id).await {
        Ok(report) => Ok(report),
        Err(e) => {
            tracing::warn!(
                spec_id = %spec_id,
                error = ?e,
                "clean_spec cascade failed — transaction rolled back, no rows deleted",
            );
            Err(e)
        }
    }
}

/// The cascade body — separated so [`clean_spec_cascade`] can wrap every exit
/// in one `warn!` site (A-SF-4) without an early-return escaping the log.
async fn clean_spec_cascade_inner(
    pool: &SqlitePool,
    spec_id: &SpecId,
) -> Result<CleanReport, RepoError> {
    let sid = spec_id.as_str();
    let mut tx = pool.begin().await?;

    // 1. decisions — leaf table (FK to specs + phase_runs).
    let decisions_deleted = sqlx::query!("DELETE FROM decisions WHERE spec_id = ?1", sid)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    // 2. phase_runs — FK to specs, task_runtime, spec_versions.
    let phase_runs_deleted = sqlx::query!("DELETE FROM phase_runs WHERE spec_id = ?1", sid)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    // 3. task_deps — both columns FK to task_runtime; delete edges before the
    //    task_runtime rows they reference (else ON DELETE RESTRICT fires).
    let task_deps_deleted = sqlx::query!(
        "DELETE FROM task_deps WHERE task_id IN (SELECT task_id FROM task_runtime WHERE spec_id = ?1) \
            OR depends_on IN (SELECT task_id FROM task_runtime WHERE spec_id = ?1)",
        sid,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // 4. task_runtime — FK to specs.
    let task_runtime_deleted = sqlx::query!("DELETE FROM task_runtime WHERE spec_id = ?1", sid)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    // 5. spec_runtime — FK to specs + spec_versions.
    let spec_runtime_deleted = sqlx::query!("DELETE FROM spec_runtime WHERE spec_id = ?1", sid)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    // 6. spec_versions — FK to specs.
    let spec_versions_deleted = sqlx::query!("DELETE FROM spec_versions WHERE spec_id = ?1", sid)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    // 7. specs — the identity row, last.
    let specs_deleted = sqlx::query!("DELETE FROM specs WHERE spec_id = ?1", sid)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    tx.commit().await?;
    Ok(CleanReport {
        decisions_deleted,
        phase_runs_deleted,
        task_deps_deleted,
        task_runtime_deleted,
        spec_runtime_deleted,
        spec_versions_deleted,
        specs_deleted,
    })
}

/// Retention prune — delete a spec's *completed* phase runs older than
/// `threshold`, keeping the spec and everything else.
///
/// A `phase_runs` row is FK-referenced by `decisions.phase_run_id`
/// (`ON DELETE RESTRICT`), so the decisions attached to the pruned runs are
/// deleted first, in the same transaction. Only completed runs
/// (`completed_at IS NOT NULL`) older than the cutoff are pruned — an in-flight
/// run is never touched. Only the `decisions_deleted` / `phase_runs_deleted`
/// fields of the returned [`CleanReport`] are non-zero.
///
/// ## The surviving-superseder FK fix (A-cr-1)
///
/// `decisions.supersedes` is a self-FK, `ON DELETE RESTRICT`. If a pruned old
/// decision is *superseded by* a newer decision that survives the prune,
/// deleting the old row violates the RESTRICT — and rolls the whole
/// transaction back, so `boi clean --phase-runs-older-than` is broken for any
/// spec with a supersede chain across the retention boundary. The fix: inside
/// the same transaction, NULL the `supersedes` column of every surviving
/// decision that points at a decision about to be deleted, *before* the
/// delete. The supersede edge is dropped (the superseded decision is gone, so
/// the edge has no meaning) but the surviving decision itself is kept.
///
/// A mid-prune failure is logged `warn!` before it propagates (A-SF-4).
pub async fn clean_phase_runs_older_than(
    pool: &SqlitePool,
    spec_id: &SpecId,
    threshold: Duration,
    now: DateTime<Utc>,
) -> Result<CleanReport, RepoError> {
    match clean_phase_runs_older_than_inner(pool, spec_id, threshold, now).await {
        Ok(report) => Ok(report),
        Err(e) => {
            tracing::warn!(
                spec_id = %spec_id,
                error = ?e,
                "clean_phase_runs_older_than failed — transaction rolled back, no rows pruned",
            );
            Err(e)
        }
    }
}

/// The retention-prune body — separated so [`clean_phase_runs_older_than`] can
/// wrap every exit in one `warn!` site (A-SF-4).
async fn clean_phase_runs_older_than_inner(
    pool: &SqlitePool,
    spec_id: &SpecId,
    threshold: Duration,
    now: DateTime<Utc>,
) -> Result<CleanReport, RepoError> {
    let sid = spec_id.as_str();
    let cutoff = now - threshold;
    let mut tx = pool.begin().await?;

    // A-cr-1: a surviving decision may `supersede` one of the decisions about
    // to be pruned. `decisions.supersedes` is a RESTRICT self-FK, so that
    // surviving referrer would block the DELETE and roll the whole prune back.
    // NULL the dangling supersede edges first — the superseded decision is
    // being removed, so the edge has no target to point at.
    sqlx::query!(
        "UPDATE decisions SET supersedes = NULL \
         WHERE supersedes IN \
           (SELECT id FROM decisions WHERE phase_run_id IN \
              (SELECT id FROM phase_runs \
               WHERE spec_id = ?1 AND completed_at IS NOT NULL AND completed_at < ?2))",
        sid,
        cutoff,
    )
    .execute(&mut *tx)
    .await?;

    // Decisions attached to the soon-to-be-pruned phase runs — delete first
    // to satisfy the decisions->phase_runs FK.
    let decisions_deleted = sqlx::query!(
        "DELETE FROM decisions WHERE phase_run_id IN \
           (SELECT id FROM phase_runs \
            WHERE spec_id = ?1 AND completed_at IS NOT NULL AND completed_at < ?2)",
        sid,
        cutoff,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();

    let phase_runs_deleted = sqlx::query!(
        "DELETE FROM phase_runs \
         WHERE spec_id = ?1 AND completed_at IS NOT NULL AND completed_at < ?2",
        sid,
        cutoff,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();

    tx.commit().await?;
    Ok(CleanReport {
        decisions_deleted,
        phase_runs_deleted,
        ..CleanReport::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::db::connect;
    use crate::repo::spec_versions::{VersionTrigger, append_version};
    use crate::repo::specs::insert_spec;
    use crate::repo::task_runtime::insert_task;
    use crate::repo::{phase_runs, task_deps};
    use crate::types::decision::{DecisionRecord, RejectedAlternative};
    use crate::types::ids::{DecisionId, PhaseRunId, TaskId};
    use crate::types::verdict::{Evidence, VerdictOutcome, WorkerVerdict};
    use serde_json::json;

    fn verdict() -> WorkerVerdict {
        WorkerVerdict {
            synopsis: "done".into(),
            outcome: VerdictOutcome::Passing {
                evidence: Evidence::default(),
            },
        }
    }

    /// Drive a spec's `spec_runtime` row to a terminal status so `clean_spec`'s
    /// A-SF-3 guard accepts it. (`completed` is the default terminal status
    /// tests use; the guard treats `failed`/`canceled` identically.)
    async fn mark_completed(pool: &SqlitePool, spec: &SpecId) {
        crate::repo::spec_runtime::update_status(pool, spec, SpecStatus::Running, None, Utc::now())
            .await
            .unwrap();
        crate::repo::spec_runtime::update_status(
            pool,
            spec,
            SpecStatus::Completed,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
    }

    /// Seed a spec with rows in ALL seven tables: 1 spec, 1 version, 1
    /// spec_runtime, 2 tasks, 1 task_deps edge, 2 phase_runs, 2 decisions.
    /// The spec is left in a terminal (`completed`) status so `clean_spec`'s
    /// A-SF-3 guard accepts it.
    async fn fully_seeded() -> (SqlitePool, SpecId) {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        let task_a = TaskId::new("T000000aa").unwrap();
        let task_b = TaskId::new("T000000bb").unwrap();
        let pr1 = PhaseRunId::new("P0000001a").unwrap();
        let pr2 = PhaseRunId::new("P0000002b").unwrap();
        let now = Utc::now();

        insert_spec(&pool, &spec, now).await.unwrap();
        append_version(
            &pool,
            &spec,
            1,
            &json!({ "title": "demo" }),
            VersionTrigger::Dispatch,
            None,
            now,
        )
        .await
        .unwrap();
        crate::repo::spec_runtime::initialize(&pool, &spec, 1)
            .await
            .unwrap();
        insert_task(&pool, &task_a, &spec, None).await.unwrap();
        insert_task(&pool, &task_b, &spec, None).await.unwrap();
        // task_b depends on task_a.
        task_deps::add_dep(&pool, &task_b, &task_a).await.unwrap();
        // Two phase runs.
        phase_runs::insert_start(
            &pool,
            &pr1,
            &spec,
            Some(&task_a),
            "execute",
            0,
            1,
            "claude_code",
            None,
            now,
        )
        .await
        .unwrap();
        phase_runs::insert_start(
            &pool,
            &pr2,
            &spec,
            Some(&task_b),
            "execute",
            0,
            1,
            "claude_code",
            None,
            now,
        )
        .await
        .unwrap();
        phase_runs::update_end(&pool, &pr1, "ok", &verdict(), &[], 0, 0, now)
            .await
            .unwrap();
        // An authored decision (no phase_run) and a runtime one (phase_run pr1).
        crate::repo::decisions::insert(
            &pool,
            &DecisionRecord::new_authored(
                DecisionId::new("D0000001a").unwrap(),
                spec.clone(),
                None,
                "authored".into(),
                "s".into(),
                "r".into(),
                vec![RejectedAlternative {
                    name: "x".into(),
                    reason: "y".into(),
                }],
                None,
                now,
            )
            .unwrap(),
        )
        .await
        .unwrap();
        crate::repo::decisions::insert(
            &pool,
            &DecisionRecord::new_runtime(
                DecisionId::new("D0000002b").unwrap(),
                spec.clone(),
                Some(pr1.clone()),
                "runtime".into(),
                "s".into(),
                "r".into(),
                vec![],
                None,
                now,
            )
            .unwrap(),
        )
        .await
        .unwrap();
        // Terminal status so `clean_spec`'s A-SF-3 guard accepts the spec.
        mark_completed(&pool, &spec).await;
        (pool, spec)
    }

    /// Count rows across all 7 tables for a spec.
    async fn row_counts(pool: &SqlitePool, spec: &SpecId) -> [i64; 7] {
        let sid = spec.as_str();
        let q = |sql: String| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(&sql)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        [
            q(format!("SELECT COUNT(*) FROM specs WHERE spec_id='{sid}'")).await,
            q(format!(
                "SELECT COUNT(*) FROM spec_versions WHERE spec_id='{sid}'"
            ))
            .await,
            q(format!(
                "SELECT COUNT(*) FROM spec_runtime WHERE spec_id='{sid}'"
            ))
            .await,
            q(format!(
                "SELECT COUNT(*) FROM task_runtime WHERE spec_id='{sid}'"
            ))
            .await,
            q(format!(
                "SELECT COUNT(*) FROM task_deps WHERE task_id IN \
                 (SELECT task_id FROM task_runtime WHERE spec_id='{sid}')"
            ))
            .await,
            q(format!(
                "SELECT COUNT(*) FROM phase_runs WHERE spec_id='{sid}'"
            ))
            .await,
            q(format!(
                "SELECT COUNT(*) FROM decisions WHERE spec_id='{sid}'"
            ))
            .await,
        ]
    }

    /// `clean_spec` leaves zero rows across all 7 tables, and the returned
    /// `CleanReport` reflects the per-table counts that were deleted.
    #[tokio::test]
    async fn clean_spec_leaves_no_orphans() {
        let (pool, spec) = fully_seeded().await;
        // Pre-clean: every table has rows.
        let before = row_counts(&pool, &spec).await;
        assert_eq!(before, [1, 1, 1, 2, 1, 2, 2], "all 7 tables seeded");

        let report = clean_spec(&pool, &spec).await.unwrap();
        assert_eq!(
            report,
            CleanReport {
                decisions_deleted: 2,
                phase_runs_deleted: 2,
                task_deps_deleted: 1,
                task_runtime_deleted: 2,
                spec_runtime_deleted: 1,
                spec_versions_deleted: 1,
                specs_deleted: 1,
            },
        );

        // Post-clean: every table is empty for this spec — no FK-RESTRICT
        // violation occurred, which proves the cascade order was correct.
        let after = row_counts(&pool, &spec).await;
        assert_eq!(after, [0; 7], "clean left no orphans in any table");
    }

    /// Cleaning a spec with `task_deps` edges removes the edges before the
    /// `task_runtime` rows — if the order were wrong, the `task_runtime`
    /// delete would hit an `ON DELETE RESTRICT` violation and the whole
    /// transaction would roll back. A successful clean is the proof.
    #[tokio::test]
    async fn clean_respects_task_deps_fk_order() {
        let (pool, spec) = fully_seeded().await;
        let report = clean_spec(&pool, &spec).await.unwrap();
        assert_eq!(report.task_deps_deleted, 1);
        assert_eq!(report.task_runtime_deleted, 2);
    }

    /// `clean_phase_runs_older_than` prunes completed phase runs older than
    /// the threshold (and their decisions) while keeping the spec, recent
    /// runs, and in-flight runs.
    #[tokio::test]
    async fn clean_phase_runs_older_than_prunes_old_completed_runs() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        let task = TaskId::new("T000000aa").unwrap();
        let now = Utc::now();
        let old = now - Duration::days(120);

        insert_spec(&pool, &spec, old).await.unwrap();
        append_version(
            &pool,
            &spec,
            1,
            &json!({ "title": "demo" }),
            VersionTrigger::Dispatch,
            None,
            old,
        )
        .await
        .unwrap();
        insert_task(&pool, &task, &spec, None).await.unwrap();

        // An old completed run + a decision attached to it.
        let old_run = PhaseRunId::new("P0000009d").unwrap();
        phase_runs::insert_start(
            &pool,
            &old_run,
            &spec,
            Some(&task),
            "execute",
            0,
            1,
            "claude_code",
            None,
            old,
        )
        .await
        .unwrap();
        phase_runs::update_end(&pool, &old_run, "ok", &verdict(), &[], 0, 0, old)
            .await
            .unwrap();
        crate::repo::decisions::insert(
            &pool,
            &DecisionRecord::new_runtime(
                DecisionId::new("D0000009d").unwrap(),
                spec.clone(),
                Some(old_run.clone()),
                "old".into(),
                "s".into(),
                "r".into(),
                vec![],
                None,
                old,
            )
            .unwrap(),
        )
        .await
        .unwrap();

        // A recent completed run.
        let recent_run = PhaseRunId::new("P00000new").unwrap();
        phase_runs::insert_start(
            &pool,
            &recent_run,
            &spec,
            Some(&task),
            "review",
            0,
            1,
            "claude_code",
            None,
            now,
        )
        .await
        .unwrap();
        phase_runs::update_end(&pool, &recent_run, "ok", &verdict(), &[], 0, 0, now)
            .await
            .unwrap();

        // An in-flight (un-completed) run that started long ago.
        let in_flight = PhaseRunId::new("P0000f1y2").unwrap();
        phase_runs::insert_start(
            &pool,
            &in_flight,
            &spec,
            Some(&task),
            "plan",
            0,
            1,
            "claude_code",
            None,
            old,
        )
        .await
        .unwrap();

        let report = clean_phase_runs_older_than(&pool, &spec, Duration::days(90), now)
            .await
            .unwrap();
        assert_eq!(report.phase_runs_deleted, 1, "only the old completed run");
        assert_eq!(report.decisions_deleted, 1, "its attached decision too");

        // The spec, recent run, and in-flight run all survive.
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM phase_runs WHERE spec_id=?1")
            .bind(spec.as_str())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 2, "recent + in-flight runs kept");
        assert!(crate::repo::specs::exists(&pool, &spec).await.unwrap());
    }

    /// A-cr-1 regression: an OLD decision (on a soon-to-be-pruned run) is
    /// superseded by a NEWER decision on a *surviving* run. `decisions.supersedes`
    /// is a RESTRICT self-FK — without the in-txn `supersedes = NULL` patch the
    /// `DELETE FROM decisions` of the old row hits the surviving referrer's
    /// RESTRICT and rolls the WHOLE prune back. With the fix the prune succeeds:
    /// the old run + old decision are gone, the surviving decision is kept with
    /// its `supersedes` nulled.
    #[tokio::test]
    async fn clean_phase_runs_older_than_handles_surviving_superseder() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        let task = TaskId::new("T000000aa").unwrap();
        let now = Utc::now();
        let old = now - Duration::days(120);

        insert_spec(&pool, &spec, old).await.unwrap();
        append_version(
            &pool,
            &spec,
            1,
            &json!({ "title": "demo" }),
            VersionTrigger::Dispatch,
            None,
            old,
        )
        .await
        .unwrap();
        insert_task(&pool, &task, &spec, None).await.unwrap();

        // The OLD run (past the retention cutoff) + its decision.
        let old_run = PhaseRunId::new("P0000aged").unwrap();
        phase_runs::insert_start(
            &pool,
            &old_run,
            &spec,
            Some(&task),
            "execute",
            0,
            1,
            "claude_code",
            None,
            old,
        )
        .await
        .unwrap();
        phase_runs::update_end(&pool, &old_run, "ok", &verdict(), &[], 0, 0, old)
            .await
            .unwrap();
        let old_decision = DecisionId::new("D0000aged").unwrap();
        crate::repo::decisions::insert(
            &pool,
            &DecisionRecord::new_runtime(
                old_decision.clone(),
                spec.clone(),
                Some(old_run.clone()),
                "old decision".into(),
                "s".into(),
                "r".into(),
                vec![],
                None,
                old,
            )
            .unwrap(),
        )
        .await
        .unwrap();

        // The SURVIVING recent run + a decision that SUPERSEDES the old one.
        let recent_run = PhaseRunId::new("P000000n1").unwrap();
        phase_runs::insert_start(
            &pool,
            &recent_run,
            &spec,
            Some(&task),
            "review",
            0,
            1,
            "claude_code",
            None,
            now,
        )
        .await
        .unwrap();
        phase_runs::update_end(&pool, &recent_run, "ok", &verdict(), &[], 0, 0, now)
            .await
            .unwrap();
        let surviving_decision = DecisionId::new("D00000n3w").unwrap();
        crate::repo::decisions::insert(
            &pool,
            &DecisionRecord::new_runtime(
                surviving_decision.clone(),
                spec.clone(),
                Some(recent_run.clone()),
                "newer decision".into(),
                "s".into(),
                "r".into(),
                vec![],
                // This is the FK that broke the prune before A-cr-1's fix.
                Some(old_decision.clone()),
                now,
            )
            .unwrap(),
        )
        .await
        .unwrap();

        // The prune must SUCCEED — before the fix this returned a RESTRICT
        // FK error and deleted nothing.
        let report = clean_phase_runs_older_than(&pool, &spec, Duration::days(90), now)
            .await
            .expect("prune must succeed despite the surviving supersede edge");
        assert_eq!(report.phase_runs_deleted, 1, "the old run was pruned");
        assert_eq!(report.decisions_deleted, 1, "the old decision was pruned");

        // The old decision is gone; the surviving one is kept with a NULL
        // `supersedes` (the dangling edge was cleared).
        let old_gone: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM decisions WHERE id = ?1")
            .bind(old_decision.as_str())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(old_gone, 0, "old decision pruned");
        let surviving_supersedes: Option<String> =
            sqlx::query_scalar("SELECT supersedes FROM decisions WHERE id = ?1")
                .bind(surviving_decision.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            surviving_supersedes.is_none(),
            "the surviving decision is kept, its dangling supersedes nulled",
        );
    }

    /// A-SF-3 regression: `clean_spec` refuses a non-terminal spec. A `running`
    /// spec returns `RepoError::SpecNotTerminal` and deletes nothing — cleaning
    /// a live spec out from under the engine would corrupt state.
    #[tokio::test]
    async fn clean_spec_refuses_a_non_terminal_spec() {
        let (pool, spec) = fully_seeded().await;
        // Drive the spec BACK to a non-terminal status.
        crate::repo::spec_runtime::update_status(
            &pool,
            &spec,
            SpecStatus::Running,
            None,
            Utc::now(),
        )
        .await
        .unwrap();

        let err = clean_spec(&pool, &spec).await.unwrap_err();
        match err {
            RepoError::SpecNotTerminal { status, .. } => assert_eq!(status, "running"),
            other => panic!("expected SpecNotTerminal, got {other:?}"),
        }
        // Nothing was deleted — every table still has its rows.
        let after = row_counts(&pool, &spec).await;
        assert_eq!(
            after,
            [1, 1, 1, 2, 1, 2, 2],
            "a refused clean must delete nothing",
        );
    }

    /// A-SF-3: `clean_spec_forced` skips the terminal-status guard — Phase 9's
    /// `--force`. A `running` spec that `clean_spec` refuses, `clean_spec_forced`
    /// cleans.
    #[tokio::test]
    async fn clean_spec_forced_cleans_a_non_terminal_spec() {
        let (pool, spec) = fully_seeded().await;
        crate::repo::spec_runtime::update_status(
            &pool,
            &spec,
            SpecStatus::Running,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        // `clean_spec` refuses it ...
        assert!(matches!(
            clean_spec(&pool, &spec).await.unwrap_err(),
            RepoError::SpecNotTerminal { .. },
        ));
        // ... but the forced variant cleans it anyway.
        let report = clean_spec_forced(&pool, &spec).await.unwrap();
        assert_eq!(report.specs_deleted, 1);
        let after = row_counts(&pool, &spec).await;
        assert_eq!(after, [0; 7], "forced clean removes every row");
    }

    /// A-SF-3: `clean_spec` on a spec with no `spec_runtime` row at all is
    /// `RepoError::NotFound` — distinct from the non-terminal refusal.
    #[tokio::test]
    async fn clean_spec_on_uninitialized_spec_is_not_found() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        insert_spec(&pool, &spec, Utc::now()).await.unwrap();
        // No `spec_runtime` row was ever created.
        let err = clean_spec(&pool, &spec).await.unwrap_err();
        assert!(matches!(err, RepoError::NotFound(_)), "got {err:?}");
    }
}
