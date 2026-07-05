//! The `spec_runtime` table — mutable per-spec execution state.
//!
//! One row per spec, created at dispatch by [`initialize`] and mutated through
//! its lifecycle by [`update_status`]. In production every mutation routes
//! through the event bus (Phase 4); this layer enforces no state-machine
//! *legality* — only the storage-level invariants the schema's CHECK encodes.
//!
//! ## The status / reason mutex
//!
//! The `spec_runtime` CHECK (design §3.0, B8) demands: a `failed` row has a
//! `failure_reason` and no `cancellation_reason`; a `canceled` row the
//! mirror; and `queued` / `running` / `completed` rows have neither.
//! [`update_status`] threads a [`TerminalReason`] so the caller cannot supply
//! the wrong reason kind — and the DB CHECK backstops a programming error.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::SqlitePool;

use crate::repo::db::RepoError;
use crate::types::ids::SpecId;
use crate::types::reasons::{CancellationReason, FailureReason};
use crate::types::state::SpecStatus;

/// The typed reason for a spec entering a terminal status.
///
/// A `failed` spec carries a [`FailureReason`]; a `canceled` spec a
/// [`CancellationReason`]. Non-terminal transitions pass `None` to
/// [`update_status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalReason {
    /// The spec failed — pairs with [`SpecStatus::Failed`].
    Failure(FailureReason),
    /// The spec was canceled — pairs with [`SpecStatus::Canceled`].
    Cancellation(CancellationReason),
}

/// A row of the `spec_runtime` table.
///
/// The two reason columns are kept as raw [`Value`] JSON; use
/// [`SpecRuntimeRow::terminal_reason`] to decode whichever one is populated
/// into a typed [`TerminalReason`].
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SpecRuntimeRow {
    /// The spec this row belongs to.
    pub spec_id: String,
    /// Which `spec_versions` row is the live authored intent.
    pub current_version: i64,
    /// Lifecycle status — `queued` / `running` / `completed` / `failed` /
    /// `canceled`.
    pub status: String,
    /// `FailureReason` JSON when `status = failed`, else `NULL`.
    pub failure_reason: Option<Value>,
    /// `CancellationReason` JSON when `status = canceled`, else `NULL`.
    pub cancellation_reason: Option<Value>,
    /// When the spec started running.
    pub started_at: Option<DateTime<Utc>>,
    /// When the spec reached a terminal status.
    pub completed_at: Option<DateTime<Utc>>,
    /// `plan ↔ critique_plan` spec-level loop iteration count (G21.1).
    pub iterations_plan_critique: i64,
    /// Spec-level `review` loop iteration count (G21.1).
    pub iterations_spec_review: i64,
}

impl SpecRuntimeRow {
    /// Decode the populated reason column (if any) into a typed
    /// [`TerminalReason`].
    ///
    /// Returns `Ok(None)` for a non-terminal row. The schema CHECK guarantees
    /// at most one reason column is set, so this never has to disambiguate.
    pub fn terminal_reason(&self) -> Result<Option<TerminalReason>, RepoError> {
        if let Some(j) = &self.failure_reason {
            let r: FailureReason = serde_json::from_value(j.clone())?;
            return Ok(Some(TerminalReason::Failure(r)));
        }
        if let Some(j) = &self.cancellation_reason {
            let r: CancellationReason = serde_json::from_value(j.clone())?;
            return Ok(Some(TerminalReason::Cancellation(r)));
        }
        Ok(None)
    }
}

/// Create the `spec_runtime` row for a freshly-dispatched spec.
///
/// The row starts `status = 'queued'` with both reason columns NULL. Requires
/// the matching `spec_versions` row to exist already (the
/// `(spec_id, current_version)` FK).
pub async fn initialize(
    pool: &SqlitePool,
    spec_id: &SpecId,
    current_version: i64,
) -> Result<(), RepoError> {
    let id = spec_id.as_str();
    let status = SpecStatus::Queued.as_str();
    let res = sqlx::query!(
        "INSERT INTO spec_runtime (spec_id, current_version, status) VALUES (?1, ?2, ?3)",
        id,
        current_version,
        status,
    )
    .execute(pool)
    .await;
    match res {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(RepoError::Duplicate(
            format!("spec_runtime for {spec_id} already initialized"),
        )),
        Err(e) => Err(RepoError::Sqlx(e)),
    }
}

/// Update a spec's lifecycle status, stamping `started_at` / `completed_at`.
///
/// `reason` MUST be `Some` exactly when `status` is terminal-with-reason, and
/// the variant must match the status: [`SpecStatus::Failed`] pairs with
/// [`TerminalReason::Failure`], [`SpecStatus::Canceled`] with
/// [`TerminalReason::Cancellation`]. A mismatch (or a non-NULL reason on a
/// non-terminal status) is rejected by the DB CHECK and surfaces as
/// [`RepoError::Sqlx`] — the mutex is enforced, never silently dropped.
///
/// `now` is the transition wall-clock. The `→running` transition stamps
/// `started_at`; any terminal transition (`completed`/`failed`/`canceled`)
/// stamps `completed_at`. Both writes are `COALESCE(col, ?)` so the timestamp
/// is set exactly once — a redundant later transition to the same kind of
/// status leaves the original stamp intact (A-SF-1 / A-cr-4).
///
/// Returns [`RepoError::NotFound`] if no `spec_runtime` row matches — the
/// UPDATE is never a silent no-op against a missing spec (A-cr-2).
pub async fn update_status(
    pool: &SqlitePool,
    spec_id: &SpecId,
    status: SpecStatus,
    reason: Option<TerminalReason>,
    now: DateTime<Utc>,
) -> Result<(), RepoError> {
    let (failure_json, cancellation_json) = match &reason {
        Some(TerminalReason::Failure(f)) => (Some(serde_json::to_value(f)?), None),
        Some(TerminalReason::Cancellation(c)) => (None, Some(serde_json::to_value(c)?)),
        None => (None, None),
    };
    // `started_at` is stamped only on →running; `completed_at` only on a
    // terminal status. A `None` here makes `COALESCE(col, NULL)` a no-op, so
    // the column keeps whatever it already held.
    let started_at = match status {
        SpecStatus::Running => Some(now),
        _ => None,
    };
    let completed_at = match status {
        SpecStatus::Completed | SpecStatus::Failed | SpecStatus::Canceled => Some(now),
        SpecStatus::Queued | SpecStatus::Running => None,
    };
    let id = spec_id.as_str();
    let status_str = status.as_str();
    let affected = sqlx::query!(
        "UPDATE spec_runtime \
         SET status = ?2, failure_reason = ?3, cancellation_reason = ?4, \
             started_at = COALESCE(started_at, ?5), \
             completed_at = COALESCE(completed_at, ?6) \
         WHERE spec_id = ?1",
        id,
        status_str,
        failure_json,
        cancellation_json,
        started_at,
        completed_at,
    )
    .execute(pool)
    .await?
    .rows_affected();
    if affected == 0 {
        return Err(RepoError::NotFound(format!(
            "spec_runtime for {spec_id} (update_status against a missing spec)"
        )));
    }
    Ok(())
}

/// Advance a spec's `current_version` pointer.
///
/// A plan revision appends a new `spec_versions` row, then moves the spec's
/// live pointer to it (Phase 10 erratum — see `service::plan_layer::apply_revision`).
/// The orchestrator's `run_phase` re-hydrates phase contracts from
/// `spec_versions` AT `current_version`, so a revision that adds a task MUST
/// advance this pointer to the new version whose snapshot carries that task's
/// contract — otherwise `run_phase` faults re-hydrating a task absent from the
/// stale snapshot.
///
/// Returns [`RepoError::NotFound`] if no `spec_runtime` row matches — the
/// UPDATE is never a silent no-op against a missing spec.
pub async fn update_current_version(
    pool: &SqlitePool,
    spec_id: &SpecId,
    new_version: i64,
) -> Result<(), RepoError> {
    let id = spec_id.as_str();
    let affected = sqlx::query!(
        "UPDATE spec_runtime SET current_version = ?2 WHERE spec_id = ?1",
        id,
        new_version,
    )
    .execute(pool)
    .await?
    .rows_affected();
    if affected == 0 {
        return Err(RepoError::NotFound(format!(
            "spec_runtime for {spec_id} (update_current_version against a missing spec)"
        )));
    }
    Ok(())
}

/// Fetch a spec's runtime row.
///
/// Returns [`RepoError::NotFound`] if the spec was never [`initialize`]d.
pub async fn fetch(pool: &SqlitePool, spec_id: &SpecId) -> Result<SpecRuntimeRow, RepoError> {
    let id = spec_id.as_str();
    let row = sqlx::query_as::<_, SpecRuntimeRow>(
        "SELECT spec_id, current_version, status, failure_reason, cancellation_reason, \
                started_at, completed_at, iterations_plan_critique, iterations_spec_review \
         FROM spec_runtime WHERE spec_id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.ok_or_else(|| RepoError::NotFound(format!("spec_runtime for {spec_id}")))
}

/// Fetch every `spec_runtime` row, newest-spec last.
///
/// Backs `boi status` with no spec argument — the fleet-wide view (Phase 9
/// Task 9.5). Ordered by `started_at` (NULLs — never-started `queued` specs —
/// sort first) so the display lists the oldest activity first; `boi status`
/// re-sorts as it sees fit.
pub async fn all(pool: &SqlitePool) -> Result<Vec<SpecRuntimeRow>, RepoError> {
    let rows = sqlx::query_as::<_, SpecRuntimeRow>(
        "SELECT spec_id, current_version, status, failure_reason, cancellation_reason, \
                started_at, completed_at, iterations_plan_critique, iterations_spec_review \
         FROM spec_runtime ORDER BY started_at",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Which spec-level iteration counter to increment (G21.1).
///
/// Each maps 1:1 to an `iterations_*` column on `spec_runtime` — the
/// spec-level analogue of [`task_runtime::IterationCounter`]. `plan`,
/// `critique_plan`, and spec-level `review` are spec-level phases with no task
/// row, so their loop caps (`CAP_PLAN_CRITIQUE`, `CAP_SPEC_REVIEW`) count here
/// rather than on `task_runtime`.
///
/// [`task_runtime::IterationCounter`]: crate::repo::task_runtime::IterationCounter
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecIterationCounter {
    /// The spec-level `plan ↔ critique_plan` loop.
    PlanCritique,
    /// The spec-level `review` loop.
    SpecReview,
}

/// Atomically increment one spec-level iteration counter and return its new
/// value (G21.1).
///
/// The returned count is what the (R2) `route_spec` cap-check tests against
/// `CAP_PLAN_CRITIQUE` / `CAP_SPEC_REVIEW`. SQLite column names cannot be
/// bound as parameters, so the two [`SpecIterationCounter`] variants dispatch
/// to two literal `UPDATE … RETURNING` statements — the
/// [`task_runtime::increment_iteration`] precedent.
///
/// Returns [`RepoError::NotFound`] if no `spec_runtime` row matches.
///
/// [`task_runtime::increment_iteration`]: crate::repo::task_runtime::increment_iteration
pub async fn increment_iteration(
    pool: &SqlitePool,
    spec_id: &SpecId,
    counter: SpecIterationCounter,
) -> Result<i64, RepoError> {
    let id = spec_id.as_str();
    let new_value =
        match counter {
            SpecIterationCounter::PlanCritique => sqlx::query_scalar!(
                "UPDATE spec_runtime SET iterations_plan_critique = iterations_plan_critique + 1 \
             WHERE spec_id = ?1 RETURNING iterations_plan_critique",
                id,
            )
            .fetch_optional(pool)
            .await?,
            SpecIterationCounter::SpecReview => {
                sqlx::query_scalar!(
                    "UPDATE spec_runtime SET iterations_spec_review = iterations_spec_review + 1 \
             WHERE spec_id = ?1 RETURNING iterations_spec_review",
                    id,
                )
                .fetch_optional(pool)
                .await?
            }
        };
    new_value.ok_or_else(|| RepoError::NotFound(format!("spec_runtime for {spec_id}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::db::connect;
    use crate::repo::spec_versions::{VersionTrigger, append_version};
    use crate::repo::specs::insert_spec;
    use serde_json::json;

    /// A pool with a spec + its version 1 row, ready for `initialize`.
    async fn seeded_pool() -> (SqlitePool, SpecId) {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        insert_spec(&pool, &spec, Utc::now()).await.unwrap();
        append_version(
            &pool,
            &spec,
            1,
            &json!({ "title": "demo" }),
            VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        (pool, spec)
    }

    /// The happy path `queued -> running -> completed` is accepted; each
    /// non-terminal status leaves both reason columns NULL.
    #[tokio::test]
    async fn legal_transitions_accepted() {
        let (pool, spec) = seeded_pool().await;
        initialize(&pool, &spec, 1).await.unwrap();
        assert_eq!(fetch(&pool, &spec).await.unwrap().status, "queued");

        update_status(&pool, &spec, SpecStatus::Running, None, Utc::now())
            .await
            .unwrap();
        assert_eq!(fetch(&pool, &spec).await.unwrap().status, "running");

        update_status(&pool, &spec, SpecStatus::Completed, None, Utc::now())
            .await
            .unwrap();
        let row = fetch(&pool, &spec).await.unwrap();
        assert_eq!(row.status, "completed");
        assert!(row.failure_reason.is_none() && row.cancellation_reason.is_none());
        assert!(row.terminal_reason().unwrap().is_none());
    }

    /// A-SF-1 regression: `update_status` stamps `started_at` on →running and
    /// `completed_at` on a terminal status. Before the fix both columns
    /// existed but no code path ever wrote them, so `boi status` / Phase 8b's
    /// `duration_ms` had no source. A `queued` row has neither stamp.
    #[tokio::test]
    async fn update_status_stamps_started_and_completed_at() {
        let (pool, spec) = seeded_pool().await;
        initialize(&pool, &spec, 1).await.unwrap();
        // Fresh `queued` row: no timestamps yet.
        let queued = fetch(&pool, &spec).await.unwrap();
        assert!(queued.started_at.is_none(), "queued has no started_at");
        assert!(queued.completed_at.is_none(), "queued has no completed_at");

        let t_start = Utc::now();
        update_status(&pool, &spec, SpecStatus::Running, None, t_start)
            .await
            .unwrap();
        let running = fetch(&pool, &spec).await.unwrap();
        assert_eq!(
            running.started_at,
            Some(t_start),
            "→running must stamp started_at",
        );
        assert!(
            running.completed_at.is_none(),
            "→running must NOT stamp completed_at",
        );

        let t_end = t_start + chrono::Duration::minutes(5);
        update_status(&pool, &spec, SpecStatus::Completed, None, t_end)
            .await
            .unwrap();
        let done = fetch(&pool, &spec).await.unwrap();
        assert_eq!(
            done.completed_at,
            Some(t_end),
            "→terminal must stamp completed_at",
        );
    }

    /// A-SF-1 regression: both timestamp writes are `COALESCE(col, ?)`, so a
    /// timestamp is stamped exactly once. A second →running leaves the first
    /// `started_at` intact rather than overwriting it.
    #[tokio::test]
    async fn update_status_timestamps_are_stamped_once() {
        let (pool, spec) = seeded_pool().await;
        initialize(&pool, &spec, 1).await.unwrap();

        let first = Utc::now();
        update_status(&pool, &spec, SpecStatus::Running, None, first)
            .await
            .unwrap();
        // A redundant later →running with a different `now`.
        let later = first + chrono::Duration::hours(1);
        update_status(&pool, &spec, SpecStatus::Running, None, later)
            .await
            .unwrap();
        assert_eq!(
            fetch(&pool, &spec).await.unwrap().started_at,
            Some(first),
            "started_at must keep the FIRST →running timestamp",
        );
    }

    /// A-cr-2 regression: `update_status` against a spec with no `spec_runtime`
    /// row is `RepoError::NotFound` — it does not silently succeed against
    /// zero affected rows.
    #[tokio::test]
    async fn update_status_on_missing_spec_is_not_found() {
        let (pool, spec) = seeded_pool().await;
        // Note: `initialize` was NOT called — there is no spec_runtime row.
        let err = update_status(&pool, &spec, SpecStatus::Running, None, Utc::now())
            .await
            .unwrap_err();
        assert!(matches!(err, RepoError::NotFound(_)), "got {err:?}");
    }

    /// G21.1: the two spec-level iteration counters increment independently,
    /// start at 0, and `increment_iteration` returns the new value.
    #[tokio::test]
    async fn spec_iteration_counters_increment_independently() {
        let (pool, spec) = seeded_pool().await;
        initialize(&pool, &spec, 1).await.unwrap();
        // Fresh row: both counters at 0.
        let row = fetch(&pool, &spec).await.unwrap();
        assert_eq!(row.iterations_plan_critique, 0);
        assert_eq!(row.iterations_spec_review, 0);

        assert_eq!(
            increment_iteration(&pool, &spec, SpecIterationCounter::PlanCritique)
                .await
                .unwrap(),
            1,
        );
        assert_eq!(
            increment_iteration(&pool, &spec, SpecIterationCounter::PlanCritique)
                .await
                .unwrap(),
            2,
        );
        // Bumping PlanCritique twice left SpecReview at 0.
        assert_eq!(
            increment_iteration(&pool, &spec, SpecIterationCounter::SpecReview)
                .await
                .unwrap(),
            1,
        );
        let row = fetch(&pool, &spec).await.unwrap();
        assert_eq!(row.iterations_plan_critique, 2);
        assert_eq!(row.iterations_spec_review, 1);
    }

    /// G21.1: `increment_iteration` on a spec with no `spec_runtime` row is
    /// `RepoError::NotFound` — never a silent no-op.
    #[tokio::test]
    async fn spec_increment_iteration_on_missing_spec_is_not_found() {
        let (pool, spec) = seeded_pool().await;
        let err = increment_iteration(&pool, &spec, SpecIterationCounter::SpecReview)
            .await
            .unwrap_err();
        assert!(matches!(err, RepoError::NotFound(_)), "got {err:?}");
    }

    /// `failed` with a `FailureReason` and `canceled` with a
    /// `CancellationReason` are accepted; `terminal_reason` decodes them back.
    #[tokio::test]
    async fn terminal_status_with_matching_reason_accepted() {
        let (pool, spec) = seeded_pool().await;
        initialize(&pool, &spec, 1).await.unwrap();

        update_status(
            &pool,
            &spec,
            SpecStatus::Failed,
            Some(TerminalReason::Failure(FailureReason::DaemonCrash)),
            Utc::now(),
        )
        .await
        .unwrap();
        let row = fetch(&pool, &spec).await.unwrap();
        assert_eq!(row.status, "failed");
        assert_eq!(
            row.terminal_reason().unwrap(),
            Some(TerminalReason::Failure(FailureReason::DaemonCrash)),
        );
        assert!(row.cancellation_reason.is_none());
    }

    /// The DB CHECK rejects an illegal status/reason combination — here a
    /// `failed` status carrying a `cancellation_reason`. The mutex violation
    /// is loud (an error), not a silent accept.
    #[tokio::test]
    async fn illegal_status_reason_combo_rejected_by_check() {
        let (pool, spec) = seeded_pool().await;
        initialize(&pool, &spec, 1).await.unwrap();

        // Hand-craft the illegal write: status `failed` but a
        // `cancellation_reason` set — exactly what the CHECK forbids.
        let bad = sqlx::query(
            "UPDATE spec_runtime SET status = 'failed', cancellation_reason = '{\"type\":\"spec_canceled\"}' \
             WHERE spec_id = ?1",
        )
        .bind(spec.as_str())
        .execute(&pool)
        .await;
        assert!(
            bad.is_err(),
            "CHECK must reject status=failed with a cancellation_reason",
        );
    }

    /// `update_status` correctly clears a stale reason when moving back to a
    /// non-terminal status would violate the mutex — i.e. a `canceled` reason
    /// is not left dangling. (Defensive: the bus never does this, but the
    /// repo write must keep the row CHECK-valid.)
    #[tokio::test]
    async fn canceled_with_cancellation_reason_accepted() {
        let (pool, spec) = seeded_pool().await;
        initialize(&pool, &spec, 1).await.unwrap();
        update_status(
            &pool,
            &spec,
            SpecStatus::Canceled,
            Some(TerminalReason::Cancellation(
                CancellationReason::SpecCanceled,
            )),
            Utc::now(),
        )
        .await
        .unwrap();
        let row = fetch(&pool, &spec).await.unwrap();
        assert_eq!(row.status, "canceled");
        assert_eq!(
            row.terminal_reason().unwrap(),
            Some(TerminalReason::Cancellation(
                CancellationReason::SpecCanceled
            )),
        );
    }

    /// `fetch` on an un-initialized spec is `RepoError::NotFound`.
    #[tokio::test]
    async fn fetch_uninitialized_is_not_found() {
        let (pool, spec) = seeded_pool().await;
        let err = fetch(&pool, &spec).await.unwrap_err();
        assert!(matches!(err, RepoError::NotFound(_)), "got {err:?}");
    }
}
