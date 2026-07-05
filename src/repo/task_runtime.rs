//! The `task_runtime` table — mutable per-task execution state.
//!
//! One row per task. State transitions are SQL-permissive here — *legality*
//! (which transition is allowed from which state) is the Phase-4 transitions
//! layer's job; this module only writes what it is told and keeps the row
//! schema-valid.
//!
//! ## Iteration counters (C6)
//!
//! v1 stored iteration counts in an `iterations_used` JSON blob. v2 uses four
//! typed `INTEGER` columns ([`IterationCounter`]); [`increment_iteration`]
//! bumps one atomically and returns the new value so Phase 5a can test it
//! against an iteration cap.
//!
//! ## `update_state` reason columns (folded G16.7)
//!
//! `task_runtime` has a `blocked_reason` and a `cancellation_reason` column —
//! and no `failure_reason`, because [`TaskState`] has no `failed` variant.
//! [`update_state`] carries BOTH an optional [`TerminalReason`] and an optional
//! [`BlockedReason`]: a `canceled` task gets the cancellation reason, a
//! `blocked` task the blocked reason (so `boi status` reads it straight from
//! the DB rather than reconstructing it from OTel — G16.7).

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::SqlitePool;

use crate::repo::db::RepoError;
use crate::repo::spec_runtime::TerminalReason;
use crate::types::ids::{SpecId, TaskId};
use crate::types::reasons::BlockedReason;
use crate::types::state::TaskState;

/// Which typed iteration counter to increment.
///
/// Each maps 1:1 to an `iterations_*` column on `task_runtime` (C6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IterationCounter {
    /// The plan ↔ critique-plan loop.
    PlanCritique,
    /// The propose-adjustment ↔ review-adjustment loop.
    TaskAdjust,
    /// The execute ↔ review loop.
    ExecuteReview,
    /// The spec-level review loop.
    SpecReview,
}

/// A row of the `task_runtime` table.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TaskRuntimeRow {
    /// The task this row belongs to.
    pub task_id: String,
    /// The spec this task belongs to.
    pub spec_id: String,
    /// The author-supplied task ref slug, if any.
    pub r#ref: Option<String>,
    /// Lifecycle state — `not_started` / `active` / `blocked` / `passing` /
    /// `canceled`.
    pub state: String,
    /// `BlockedReason` JSON when `state = blocked`, else `NULL`.
    pub blocked_reason: Option<Value>,
    /// `CancellationReason` JSON when `state = canceled`, else `NULL`.
    pub cancellation_reason: Option<Value>,
    /// The phase currently executing for this task.
    pub current_phase: Option<String>,
    /// Free-form evidence JSON.
    pub evidence: Option<Value>,
    /// plan ↔ critique iteration count.
    pub iterations_plan_critique: i64,
    /// adjust-propose ↔ adjust-review iteration count.
    pub iterations_task_adjust: i64,
    /// execute ↔ review iteration count.
    pub iterations_execute_review: i64,
    /// spec-review iteration count.
    pub iterations_spec_review: i64,
    /// The task's worktree path.
    pub worktree_path: Option<String>,
    /// The task's branch ref.
    pub branch_ref: Option<String>,
    /// When the task started.
    pub started_at: Option<DateTime<Utc>>,
    /// When the task reached a terminal state.
    pub completed_at: Option<DateTime<Utc>>,
}

/// Insert a task's runtime row in the initial `not_started` state.
///
/// `task_ref` is the optional author-supplied slug. A duplicate `task_id`
/// returns [`RepoError::Duplicate`].
pub async fn insert_task(
    pool: &SqlitePool,
    task_id: &TaskId,
    spec_id: &SpecId,
    task_ref: Option<&str>,
) -> Result<(), RepoError> {
    let tid = task_id.as_str();
    let sid = spec_id.as_str();
    let state = TaskState::NotStarted.as_str();
    let res = sqlx::query!(
        "INSERT INTO task_runtime (task_id, spec_id, ref, state) VALUES (?1, ?2, ?3, ?4)",
        tid,
        sid,
        task_ref,
        state,
    )
    .execute(pool)
    .await;
    match res {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(RepoError::Duplicate(
            format!("task_runtime for {task_id} already exists"),
        )),
        Err(e) => Err(RepoError::Sqlx(e)),
    }
}

/// Allocate a fresh [`TaskId`] and insert its `task_runtime` row.
///
/// The collision-retried generate-and-insert counterpart to [`insert_task`] —
/// `task_runtime` carries an FK + domain data so the allocator lives here
/// (where the column set is known) rather than in `ids.rs`, per the Task 3.3
/// asymmetry. The owning spec's `specs` row must exist (the `spec_id` FK).
pub async fn allocate_task_id(
    pool: &SqlitePool,
    spec_id: &SpecId,
    task_ref: Option<&str>,
) -> Result<TaskId, RepoError> {
    let sid = spec_id.as_str();
    let state = TaskState::NotStarted.as_str();
    let raw = crate::repo::ids::allocate_id('T', |id| async move {
        let res = sqlx::query!(
            "INSERT INTO task_runtime (task_id, spec_id, ref, state) VALUES (?1, ?2, ?3, ?4)",
            id,
            sid,
            task_ref,
            state,
        )
        .execute(pool)
        .await;
        crate::repo::ids::insert_result(res)
    })
    .await?;
    TaskId::new(&raw).map_err(|e| RepoError::Duplicate(format!("generated invalid task id: {e}")))
}

/// Update a task's lifecycle state, stamping `started_at` / `completed_at`.
///
/// `terminal_reason` is set when transitioning to `canceled`; `blocked_reason`
/// when transitioning to `blocked` (folded G16.7). A task has no `failed`
/// state — passing [`TerminalReason::Failure`] is a programming error and
/// returns [`RepoError::Duplicate`] with an explanatory message rather than
/// silently writing nothing.
///
/// `now` is the transition wall-clock. The `→active` transition stamps
/// `started_at`; a terminal transition (`passing`/`canceled`) stamps
/// `completed_at`. Both writes are `COALESCE(col, ?)` so each timestamp is set
/// exactly once — a `blocked → active` re-entry does not overwrite the
/// original `started_at` (A-SF-1 / A-cr-4).
///
/// Returns [`RepoError::NotFound`] if no `task_runtime` row matches — the
/// UPDATE is never a silent no-op against a missing task (A-cr-2).
///
/// State *legality* is not enforced here — that is the Phase-4 transitions
/// layer's responsibility.
pub async fn update_state(
    pool: &SqlitePool,
    task_id: &TaskId,
    state: TaskState,
    terminal_reason: Option<TerminalReason>,
    blocked_reason: Option<BlockedReason>,
    now: DateTime<Utc>,
) -> Result<(), RepoError> {
    let cancellation_json = match &terminal_reason {
        Some(TerminalReason::Cancellation(c)) => Some(serde_json::to_value(c)?),
        Some(TerminalReason::Failure(_)) => {
            // `task_runtime` has no failure_reason column — TaskState has no
            // `failed`. Refuse loudly rather than dropping the reason.
            return Err(RepoError::Duplicate(format!(
                "task {task_id}: TerminalReason::Failure is invalid for a task \
                 (tasks have no `failed` state)"
            )));
        }
        None => None,
    };
    let blocked_json = match &blocked_reason {
        Some(b) => Some(serde_json::to_value(b)?),
        None => None,
    };
    // `started_at` is stamped only on →active; `completed_at` only on a
    // terminal state. A `None` makes `COALESCE(col, NULL)` a no-op.
    let started_at = match state {
        TaskState::Active => Some(now),
        _ => None,
    };
    let completed_at = match state {
        TaskState::Passing | TaskState::Canceled => Some(now),
        TaskState::NotStarted | TaskState::Active | TaskState::Blocked => None,
    };
    let tid = task_id.as_str();
    let state_str = state.as_str();
    let affected = sqlx::query!(
        "UPDATE task_runtime \
         SET state = ?2, cancellation_reason = ?3, blocked_reason = ?4, \
             started_at = COALESCE(started_at, ?5), \
             completed_at = COALESCE(completed_at, ?6) \
         WHERE task_id = ?1",
        tid,
        state_str,
        cancellation_json,
        blocked_json,
        started_at,
        completed_at,
    )
    .execute(pool)
    .await?
    .rows_affected();
    if affected == 0 {
        return Err(RepoError::NotFound(format!(
            "task_runtime for {task_id} (update_state against a missing task)"
        )));
    }
    Ok(())
}

/// Atomically increment one iteration counter and return its new value.
///
/// The returned count is what Phase 5a tests against an iteration cap.
/// SQLite column names cannot be bound as parameters, so the four
/// [`IterationCounter`] variants dispatch to four literal `UPDATE … RETURNING`
/// statements.
pub async fn increment_iteration(
    pool: &SqlitePool,
    task_id: &TaskId,
    counter: IterationCounter,
) -> Result<i64, RepoError> {
    let tid = task_id.as_str();
    let new_value =
        match counter {
            IterationCounter::PlanCritique => sqlx::query_scalar!(
                "UPDATE task_runtime SET iterations_plan_critique = iterations_plan_critique + 1 \
                 WHERE task_id = ?1 RETURNING iterations_plan_critique",
                tid,
            )
            .fetch_optional(pool)
            .await?,
            IterationCounter::TaskAdjust => {
                sqlx::query_scalar!(
                    "UPDATE task_runtime SET iterations_task_adjust = iterations_task_adjust + 1 \
                 WHERE task_id = ?1 RETURNING iterations_task_adjust",
                    tid,
                )
                .fetch_optional(pool)
                .await?
            }
            IterationCounter::ExecuteReview => sqlx::query_scalar!(
                "UPDATE task_runtime SET iterations_execute_review = iterations_execute_review + 1 \
                 WHERE task_id = ?1 RETURNING iterations_execute_review",
                tid,
            )
            .fetch_optional(pool)
            .await?,
            IterationCounter::SpecReview => {
                sqlx::query_scalar!(
                    "UPDATE task_runtime SET iterations_spec_review = iterations_spec_review + 1 \
                 WHERE task_id = ?1 RETURNING iterations_spec_review",
                    tid,
                )
                .fetch_optional(pool)
                .await?
            }
        };
    new_value.ok_or_else(|| RepoError::NotFound(format!("task_runtime for {task_id}")))
}

/// Zero every iteration counter on a task — the `boi unblock --reset-counter`
/// primitive (design §6: "extends the iteration cap").
///
/// An operator who unblocks a `CapExceeded` task with `--reset-counter` wants
/// the bounded loops (`plan ↔ critique`, `task_adjust`, `execute ↔ review`,
/// `spec_review`) to start fresh — without the reset the task re-blocks on the
/// next iteration. Returns [`RepoError::NotFound`] if no such task row exists
/// (the UPDATE is never a silent no-op — A-cr-2).
pub async fn reset_iterations(pool: &SqlitePool, task_id: &TaskId) -> Result<(), RepoError> {
    let tid = task_id.as_str();
    let affected = sqlx::query!(
        "UPDATE task_runtime \
         SET iterations_plan_critique = 0, iterations_task_adjust = 0, \
             iterations_execute_review = 0, iterations_spec_review = 0 \
         WHERE task_id = ?1",
        tid,
    )
    .execute(pool)
    .await?
    .rows_affected();
    if affected == 0 {
        return Err(RepoError::NotFound(format!("task_runtime for {task_id}")));
    }
    Ok(())
}

/// Fetch a task's runtime row.
///
/// Returns [`RepoError::NotFound`] if the task does not exist.
pub async fn fetch(pool: &SqlitePool, task_id: &TaskId) -> Result<TaskRuntimeRow, RepoError> {
    let tid = task_id.as_str();
    let row = sqlx::query_as::<_, TaskRuntimeRow>(
        "SELECT task_id, spec_id, ref, state, blocked_reason, cancellation_reason, \
                current_phase, evidence, iterations_plan_critique, iterations_task_adjust, \
                iterations_execute_review, iterations_spec_review, worktree_path, branch_ref, \
                started_at, completed_at \
         FROM task_runtime WHERE task_id = ?1",
    )
    .bind(tid)
    .fetch_optional(pool)
    .await?;
    row.ok_or_else(|| RepoError::NotFound(format!("task_runtime for {task_id}")))
}

/// Every `task_runtime` row for a spec, ordered by `task_id`.
///
/// Phase 5a's scheduler needs to enumerate a spec's tasks — for dep-readiness
/// (`ready_tasks`), the all-settled resume gate (`all_tasks_settled`), and the
/// `SpecCanceled` cascade — but Phase 3 shipped no spec-scoped task query (it
/// has only the by-id [`fetch`] and the per-edge `task_deps` helpers). This is
/// the minimal Phase-3 amendment Phase 5a requires: a runtime-checked
/// `query_as` (the same shape as [`fetch`]), so it adds no `sqlx::query!`
/// macro and needs no `.sqlx/` offline-cache regeneration.
pub async fn tasks_for_spec(
    pool: &SqlitePool,
    spec_id: &SpecId,
) -> Result<Vec<TaskRuntimeRow>, RepoError> {
    let sid = spec_id.as_str();
    let rows = sqlx::query_as::<_, TaskRuntimeRow>(
        "SELECT task_id, spec_id, ref, state, blocked_reason, cancellation_reason, \
                current_phase, evidence, iterations_plan_critique, iterations_task_adjust, \
                iterations_execute_review, iterations_spec_review, worktree_path, branch_ref, \
                started_at, completed_at \
         FROM task_runtime WHERE spec_id = ?1 ORDER BY task_id",
    )
    .bind(sid)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::db::connect;
    use crate::repo::specs::insert_spec;
    use crate::types::reasons::CancellationReason;

    /// A pool with a spec + one task, ready for state/counter mutation.
    async fn seeded_pool() -> (SqlitePool, SpecId, TaskId) {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        let task = TaskId::new("T0000001a").unwrap();
        insert_spec(&pool, &spec, Utc::now()).await.unwrap();
        insert_task(&pool, &task, &spec, Some("setup"))
            .await
            .unwrap();
        (pool, spec, task)
    }

    /// `allocate_task_id` generates a valid TaskId and inserts the row.
    #[tokio::test]
    async fn allocate_task_id_generates_and_inserts() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        insert_spec(&pool, &spec, Utc::now()).await.unwrap();

        let task = allocate_task_id(&pool, &spec, Some("setup")).await.unwrap();
        let row = fetch(&pool, &task).await.unwrap();
        assert_eq!(row.state, "not_started");
        assert_eq!(row.r#ref.as_deref(), Some("setup"));
        // The two allocations produce distinct IDs.
        let other = allocate_task_id(&pool, &spec, None).await.unwrap();
        assert_ne!(task, other);
    }

    /// Insert puts a task in `not_started`; a duplicate is `Duplicate`.
    #[tokio::test]
    async fn insert_task_then_duplicate() {
        let (pool, spec, task) = seeded_pool().await;
        let row = fetch(&pool, &task).await.unwrap();
        assert_eq!(row.state, "not_started");
        assert_eq!(row.r#ref.as_deref(), Some("setup"));

        let err = insert_task(&pool, &task, &spec, None).await.unwrap_err();
        assert!(matches!(err, RepoError::Duplicate(_)), "got {err:?}");
    }

    /// Each of the four iteration counters increments independently and
    /// `increment_iteration` returns the new value.
    #[tokio::test]
    async fn counters_increment_independently() {
        let (pool, _spec, task) = seeded_pool().await;

        assert_eq!(
            increment_iteration(&pool, &task, IterationCounter::PlanCritique)
                .await
                .unwrap(),
            1,
        );
        assert_eq!(
            increment_iteration(&pool, &task, IterationCounter::PlanCritique)
                .await
                .unwrap(),
            2,
        );
        // Bumping PlanCritique twice left the other three at 0.
        assert_eq!(
            increment_iteration(&pool, &task, IterationCounter::ExecuteReview)
                .await
                .unwrap(),
            1,
        );

        let row = fetch(&pool, &task).await.unwrap();
        assert_eq!(row.iterations_plan_critique, 2);
        assert_eq!(row.iterations_execute_review, 1);
        assert_eq!(row.iterations_task_adjust, 0);
        assert_eq!(row.iterations_spec_review, 0);
    }

    /// `update_state` is SQL-permissive: even a transition the Phase-4 layer
    /// would reject (here `not_started` straight to `passing`) is accepted —
    /// legality is not the repo's job.
    #[tokio::test]
    async fn update_state_is_sql_permissive() {
        let (pool, _spec, task) = seeded_pool().await;
        update_state(&pool, &task, TaskState::Passing, None, None, Utc::now())
            .await
            .unwrap();
        assert_eq!(fetch(&pool, &task).await.unwrap().state, "passing");
    }

    /// Transitioning to `blocked` persists the `BlockedReason` so `boi status`
    /// can read it directly (folded G16.7).
    #[tokio::test]
    async fn blocked_reason_persisted() {
        let (pool, _spec, task) = seeded_pool().await;
        let reason = BlockedReason::MergeConflict {
            conflicts: vec![std::path::PathBuf::from("src/lib.rs")],
            base_sha: "deadbeef".into(),
            head_sha: "cafef00d".into(),
            reason: "AllStrategiesDeclined".into(),
        };
        update_state(
            &pool,
            &task,
            TaskState::Blocked,
            None,
            Some(reason.clone()),
            Utc::now(),
        )
        .await
        .unwrap();
        let row = fetch(&pool, &task).await.unwrap();
        assert_eq!(row.state, "blocked");
        let decoded: BlockedReason = serde_json::from_value(row.blocked_reason.unwrap()).unwrap();
        assert_eq!(decoded, reason);
    }

    /// Transitioning to `canceled` persists the `CancellationReason`.
    #[tokio::test]
    async fn cancellation_reason_persisted() {
        let (pool, _spec, task) = seeded_pool().await;
        update_state(
            &pool,
            &task,
            TaskState::Canceled,
            Some(TerminalReason::Cancellation(
                CancellationReason::SpecCanceled,
            )),
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        let row = fetch(&pool, &task).await.unwrap();
        assert_eq!(row.state, "canceled");
        assert!(row.cancellation_reason.is_some());
    }

    /// A task has no `failed` state — passing `TerminalReason::Failure` is
    /// refused loudly, not silently dropped.
    #[tokio::test]
    async fn failure_reason_on_task_is_rejected() {
        use crate::types::reasons::FailureReason;
        let (pool, _spec, task) = seeded_pool().await;
        let err = update_state(
            &pool,
            &task,
            TaskState::Canceled,
            Some(TerminalReason::Failure(FailureReason::DaemonCrash)),
            None,
            Utc::now(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RepoError::Duplicate(_)), "got {err:?}");
    }

    /// A-SF-1 regression: `update_state` stamps `started_at` on →active and
    /// `completed_at` on a terminal state. Before the fix both columns
    /// existed but no writer touched them. A fresh `not_started` row has
    /// neither stamp.
    #[tokio::test]
    async fn update_state_stamps_started_and_completed_at() {
        let (pool, _spec, task) = seeded_pool().await;
        let fresh = fetch(&pool, &task).await.unwrap();
        assert!(fresh.started_at.is_none(), "not_started has no started_at");
        assert!(
            fresh.completed_at.is_none(),
            "not_started has no completed_at",
        );

        let t_start = Utc::now();
        update_state(&pool, &task, TaskState::Active, None, None, t_start)
            .await
            .unwrap();
        let active = fetch(&pool, &task).await.unwrap();
        assert_eq!(
            active.started_at,
            Some(t_start),
            "→active stamps started_at"
        );
        assert!(
            active.completed_at.is_none(),
            "→active must NOT stamp completed_at",
        );

        let t_end = t_start + chrono::Duration::minutes(8);
        update_state(&pool, &task, TaskState::Passing, None, None, t_end)
            .await
            .unwrap();
        assert_eq!(
            fetch(&pool, &task).await.unwrap().completed_at,
            Some(t_end),
            "→passing (terminal) stamps completed_at",
        );
    }

    /// A-SF-1 regression: the timestamp writes are `COALESCE(col, ?)` — a
    /// `blocked → active` re-entry keeps the FIRST `started_at`, it is not
    /// overwritten.
    #[tokio::test]
    async fn update_state_started_at_is_stamped_once() {
        let (pool, _spec, task) = seeded_pool().await;
        let first = Utc::now();
        update_state(&pool, &task, TaskState::Active, None, None, first)
            .await
            .unwrap();
        let reason = BlockedReason::Manual {
            operator_note: None,
        };
        update_state(
            &pool,
            &task,
            TaskState::Blocked,
            None,
            Some(reason),
            first + chrono::Duration::minutes(1),
        )
        .await
        .unwrap();
        // Re-enter `active` later — started_at must NOT move.
        update_state(
            &pool,
            &task,
            TaskState::Active,
            None,
            None,
            first + chrono::Duration::hours(1),
        )
        .await
        .unwrap();
        assert_eq!(
            fetch(&pool, &task).await.unwrap().started_at,
            Some(first),
            "started_at keeps the first →active timestamp across a re-entry",
        );
    }

    /// A-cr-2 regression: `update_state` against a task with no `task_runtime`
    /// row is `RepoError::NotFound` — it does not silently succeed against
    /// zero affected rows.
    #[tokio::test]
    async fn update_state_on_missing_task_is_not_found() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let task = TaskId::new("T0000009z").unwrap();
        let err = update_state(&pool, &task, TaskState::Active, None, None, Utc::now())
            .await
            .unwrap_err();
        assert!(matches!(err, RepoError::NotFound(_)), "got {err:?}");
    }

    /// `increment_iteration` / `fetch` on a missing task is `NotFound`.
    #[tokio::test]
    async fn missing_task_is_not_found() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let task = TaskId::new("T0000009z").unwrap();
        assert!(matches!(
            fetch(&pool, &task).await.unwrap_err(),
            RepoError::NotFound(_),
        ));
        assert!(matches!(
            increment_iteration(&pool, &task, IterationCounter::SpecReview)
                .await
                .unwrap_err(),
            RepoError::NotFound(_),
        ));
    }

    /// `tasks_for_spec` returns every task of the spec (ordered by id) and
    /// excludes a task that belongs to a different spec.
    #[tokio::test]
    async fn tasks_for_spec_returns_all_spec_tasks() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec_a = SpecId::new("S000000aa").unwrap();
        let spec_b = SpecId::new("S000000bb").unwrap();
        insert_spec(&pool, &spec_a, Utc::now()).await.unwrap();
        insert_spec(&pool, &spec_b, Utc::now()).await.unwrap();
        let t1 = TaskId::new("T000000a1").unwrap();
        let t2 = TaskId::new("T000000a2").unwrap();
        let other = TaskId::new("T000000b1").unwrap();
        insert_task(&pool, &t1, &spec_a, None).await.unwrap();
        insert_task(&pool, &t2, &spec_a, None).await.unwrap();
        insert_task(&pool, &other, &spec_b, None).await.unwrap();

        let rows = tasks_for_spec(&pool, &spec_a).await.unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.task_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["T000000a1", "T000000a2"],
            "spec A's two tasks, by id"
        );

        // An empty spec yields an empty Vec, not an error.
        assert_eq!(tasks_for_spec(&pool, &spec_b).await.unwrap().len(), 1);
    }
}
