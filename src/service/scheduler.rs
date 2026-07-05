//! The scheduler — dep-respecting task readiness (design §5).
//!
//! BOI fans the per-task lifecycle out at the `<tasks>` boundary
//! ([`PipelinePhase::Tasks`]) and re-runs the task lifecycle in parallel. The
//! scheduler answers the two questions that fan-out needs:
//!
//! [`PipelinePhase::Tasks`]: crate::config::pipeline::PipelinePhase::Tasks
//!
//! - [`ready_tasks`] — which `not_started` tasks have *all* their dependencies
//!   `passing`, so they can be spawned now.
//! - [`all_tasks_settled`] — has *every* task reached a terminal state, so the
//!   spec pipeline can resume past the `<tasks>` boundary.
//!
//! ## Idempotence — why re-running `ready_tasks` cannot double-spawn (item 22)
//!
//! [`ready_tasks`] returns only `not_started` tasks. The orchestrator calls it
//! at the `<tasks>` boundary, on every `TaskPassed`, and on `PlanRevised` — and
//! because spawning a task flips it `not_started → active` (the bus's
//! `TaskStarted` persist), an already-spawned task is `active` and never
//! re-returned. So re-running `ready_tasks` after each `TaskPassed` is
//! idempotent — it cannot double-spawn. (The orchestrator additionally treats
//! an `IllegalTransition` on a spawn `TaskStarted` as benign — already-spawned,
//! log and continue — a belt-and-braces guard.)
//!
//! ## No JSON parsing — `task_deps` is a real table (review L2)
//!
//! v1 stored a task's blockers in an un-validated `blocked_by_task_ids` JSON
//! array. v2's `task_deps` table has an FK on both endpoints, so the scheduler
//! reads [`repo::task_deps::deps_of`] — an indexed lookup, no JSON parse, no
//! dangling edge possible.

use sqlx::SqlitePool;

use crate::repo;
use crate::repo::db::RepoError;
use crate::types::ids::{SpecId, TaskId};
use crate::types::state::TaskState;

/// A scheduler query failed.
///
/// The only failure mode is the underlying repo query — the scheduler does no
/// fallible work beyond it. Wrapping (rather than re-exporting `RepoError`)
/// keeps a `repo`-layer type off the `service`-layer surface.
#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    /// A repo-layer query failed while computing task readiness.
    #[error("scheduler query failed: {0}")]
    Repo(#[from] RepoError),
}

/// The tasks of `spec_id` that are ready to spawn.
///
/// A task is **ready** iff its state is `not_started` AND every dependency in
/// `task_deps` is `passing`. The result is sorted by `task_id` (the
/// [`repo::task_runtime::tasks_for_spec`] order) for a deterministic spawn
/// order.
///
/// Re-running this after a `TaskPassed` is idempotent — an already-spawned
/// (`active`) task is filtered out by the `not_started` predicate, so it is
/// never returned twice and cannot be double-spawned (see the module doc).
pub async fn ready_tasks(
    pool: &SqlitePool,
    spec_id: &SpecId,
) -> Result<Vec<TaskId>, SchedulerError> {
    let tasks = repo::task_runtime::tasks_for_spec(pool, spec_id).await?;

    // Index every task's state once, so the per-dep readiness check is an
    // in-memory map lookup rather than another query per edge.
    let mut state_by_id: std::collections::HashMap<String, TaskState> =
        std::collections::HashMap::with_capacity(tasks.len());
    for row in &tasks {
        state_by_id.insert(row.task_id.clone(), parse_state(&row.task_id, &row.state)?);
    }

    let mut ready = Vec::new();
    for row in &tasks {
        // Only `not_started` tasks are spawn candidates.
        if state_by_id.get(&row.task_id) != Some(&TaskState::NotStarted) {
            continue;
        }
        let task_id = parse_task_id(&row.task_id)?;
        let deps = repo::task_deps::deps_of(pool, &task_id).await?;
        // Every dependency must be `passing`. A dep with an unknown state
        // (not in this spec's task set — should be impossible via the FK) is
        // treated as not-passing, so a task is never spawned on a phantom dep.
        let all_deps_passing = deps
            .iter()
            .all(|dep| state_by_id.get(dep.as_str()) == Some(&TaskState::Passing));
        if all_deps_passing {
            ready.push(task_id);
        }
    }
    Ok(ready)
}

/// Whether every task of `spec_id` has reached a terminal state.
///
/// True iff every task is `passing` or `canceled` — the spec-pipeline resume
/// gate. A single `active` / `blocked` / `not_started` task ⇒ false. A spec
/// with no tasks at all is vacuously settled (`true`).
pub async fn all_tasks_settled(
    pool: &SqlitePool,
    spec_id: &SpecId,
) -> Result<bool, SchedulerError> {
    let tasks = repo::task_runtime::tasks_for_spec(pool, spec_id).await?;
    for row in &tasks {
        let state = parse_state(&row.task_id, &row.state)?;
        match state {
            TaskState::Passing | TaskState::Canceled => {}
            // Any non-terminal task means the spec is not yet settled.
            TaskState::NotStarted | TaskState::Active | TaskState::Blocked => {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Whether ANY task of `spec_id` is `passing`.
///
/// The settlement tie-breaker (audit A3): once [`all_tasks_settled`] is true,
/// the orchestrator must decide between resuming the spec pipeline past the
/// `<tasks>` boundary (≥ 1 task passed — there is work to merge) and
/// terminating the spec (every task ended `canceled` — nothing to merge).
/// A spec with no tasks returns `false`.
pub async fn any_task_passing(pool: &SqlitePool, spec_id: &SpecId) -> Result<bool, SchedulerError> {
    let tasks = repo::task_runtime::tasks_for_spec(pool, spec_id).await?;
    for row in &tasks {
        if parse_state(&row.task_id, &row.state)? == TaskState::Passing {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Parse a `task_runtime.state` string into a [`TaskState`].
///
/// A value that does not parse means DB corruption — surfaced loudly as
/// [`RepoError::NotFound`], never silently treated as some default state.
fn parse_state(task_id: &str, state: &str) -> Result<TaskState, SchedulerError> {
    state.parse::<TaskState>().map_err(|e| {
        SchedulerError::Repo(RepoError::NotFound(format!(
            "corrupt task_runtime.state for {task_id}: {e}"
        )))
    })
}

/// Parse a stored `task_id` string into a [`TaskId`].
fn parse_task_id(s: &str) -> Result<TaskId, SchedulerError> {
    TaskId::new(s).map_err(|e| {
        SchedulerError::Repo(RepoError::NotFound(format!(
            "corrupt task id in task_runtime: {e}"
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::db::connect;
    use crate::repo::specs::insert_spec;
    use crate::repo::task_runtime::{insert_task, update_state};
    use chrono::Utc;

    fn spec() -> SpecId {
        SpecId::new("S0000001a").unwrap()
    }

    /// A pool with a spec; tasks are added per-test.
    async fn seeded() -> SqlitePool {
        let pool = connect("sqlite::memory:").await.unwrap();
        insert_spec(&pool, &spec(), Utc::now()).await.unwrap();
        pool
    }

    /// Insert a task in `not_started`, then drive it to `state` if different.
    async fn task(pool: &SqlitePool, id: &str, state: TaskState) -> TaskId {
        let t = TaskId::new(id).unwrap();
        insert_task(pool, &t, &spec(), None).await.unwrap();
        if state != TaskState::NotStarted {
            update_state(pool, &t, state, None, None, Utc::now())
                .await
                .unwrap();
        }
        t
    }

    /// Linear chain A → B → C: only A is ready initially; once A is `passing`,
    /// B becomes ready (C still blocked behind B).
    #[tokio::test]
    async fn test_l2_linear_chain_unblocks_one_at_a_time() {
        let pool = seeded().await;
        let a = task(&pool, "T000000aa", TaskState::NotStarted).await;
        let b = task(&pool, "T000000bb", TaskState::NotStarted).await;
        let c = task(&pool, "T000000cc", TaskState::NotStarted).await;
        repo::task_deps::add_dep(&pool, &b, &a).await.unwrap(); // B depends on A
        repo::task_deps::add_dep(&pool, &c, &b).await.unwrap(); // C depends on B

        // Initially only A (no deps) is ready.
        assert_eq!(ready_tasks(&pool, &spec()).await.unwrap(), vec![a.clone()]);

        // A passes → B becomes ready, C still blocked.
        update_state(&pool, &a, TaskState::Passing, None, None, Utc::now())
            .await
            .unwrap();
        assert_eq!(ready_tasks(&pool, &spec()).await.unwrap(), vec![b.clone()]);

        // B passes → C becomes ready.
        update_state(&pool, &b, TaskState::Passing, None, None, Utc::now())
            .await
            .unwrap();
        assert_eq!(ready_tasks(&pool, &spec()).await.unwrap(), vec![c]);
    }

    /// Diamond A → {B, C} → D: B and C are ready together; D only after BOTH
    /// B and C pass.
    #[tokio::test]
    async fn test_l2_diamond_d_waits_for_both_branches() {
        let pool = seeded().await;
        let a = task(&pool, "T000000aa", TaskState::Passing).await;
        let b = task(&pool, "T000000bb", TaskState::NotStarted).await;
        let c = task(&pool, "T000000cc", TaskState::NotStarted).await;
        let d = task(&pool, "T000000dd", TaskState::NotStarted).await;
        repo::task_deps::add_dep(&pool, &b, &a).await.unwrap();
        repo::task_deps::add_dep(&pool, &c, &a).await.unwrap();
        repo::task_deps::add_dep(&pool, &d, &b).await.unwrap();
        repo::task_deps::add_dep(&pool, &d, &c).await.unwrap();

        // A is `passing` → B and C are both ready; D is not.
        assert_eq!(
            ready_tasks(&pool, &spec()).await.unwrap(),
            vec![b.clone(), c.clone()],
        );

        // Only B passes — D still waits on C.
        update_state(&pool, &b, TaskState::Passing, None, None, Utc::now())
            .await
            .unwrap();
        assert_eq!(ready_tasks(&pool, &spec()).await.unwrap(), vec![c.clone()]);

        // C passes too — now D is ready.
        update_state(&pool, &c, TaskState::Passing, None, None, Utc::now())
            .await
            .unwrap();
        assert_eq!(ready_tasks(&pool, &spec()).await.unwrap(), vec![d]);
    }

    /// Idempotence: `ready_tasks` called twice with no state change returns the
    /// same set, and an already-`active` task is never re-returned.
    #[tokio::test]
    async fn test_l2_ready_tasks_is_idempotent_and_skips_active() {
        let pool = seeded().await;
        let a = task(&pool, "T000000aa", TaskState::NotStarted).await;
        let _b = task(&pool, "T000000bb", TaskState::NotStarted).await;

        let first = ready_tasks(&pool, &spec()).await.unwrap();
        let second = ready_tasks(&pool, &spec()).await.unwrap();
        assert_eq!(first, second, "no state change ⇒ identical ready set");
        assert_eq!(first.len(), 2, "both no-dep tasks ready");

        // Spawn A — it flips to `active`. `ready_tasks` no longer returns it.
        update_state(&pool, &a, TaskState::Active, None, None, Utc::now())
            .await
            .unwrap();
        let after = ready_tasks(&pool, &spec()).await.unwrap();
        assert!(
            !after.contains(&a),
            "an active task must not be re-returned (no double-spawn)",
        );
        assert_eq!(after.len(), 1, "only the still-not_started task remains");
    }

    /// `all_tasks_settled` is false while one task is `active`, true once every
    /// task is `passing`/`canceled`.
    #[tokio::test]
    async fn test_l2_all_tasks_settled_tracks_terminal_states() {
        let pool = seeded().await;
        let a = task(&pool, "T000000aa", TaskState::Active).await;
        let b = task(&pool, "T000000bb", TaskState::Passing).await;

        // One `active` task ⇒ not settled.
        assert!(!all_tasks_settled(&pool, &spec()).await.unwrap());

        // Drive A to `passing` — both terminal now.
        update_state(&pool, &a, TaskState::Passing, None, None, Utc::now())
            .await
            .unwrap();
        assert!(all_tasks_settled(&pool, &spec()).await.unwrap());

        // A `canceled` task also counts as settled.
        update_state(&pool, &b, TaskState::Canceled, None, None, Utc::now())
            .await
            .unwrap();
        assert!(all_tasks_settled(&pool, &spec()).await.unwrap());
    }

    /// A `blocked` task means the spec is not settled — `blocked` is not
    /// terminal (it is recoverable via `TaskUnblocked`).
    #[tokio::test]
    async fn test_l2_all_tasks_settled_false_with_blocked_task() {
        let pool = seeded().await;
        let _a = task(&pool, "T000000aa", TaskState::Blocked).await;
        let _b = task(&pool, "T000000bb", TaskState::Passing).await;
        assert!(
            !all_tasks_settled(&pool, &spec()).await.unwrap(),
            "a blocked task is non-terminal ⇒ spec not settled",
        );
    }

    /// A spec with no tasks is vacuously settled, and has no ready tasks.
    #[tokio::test]
    async fn test_l2_empty_spec_is_settled_with_no_ready_tasks() {
        let pool = seeded().await;
        assert!(all_tasks_settled(&pool, &spec()).await.unwrap());
        assert!(ready_tasks(&pool, &spec()).await.unwrap().is_empty());
    }

    /// `any_task_passing` — the A3 settlement tie-breaker: false for an empty
    /// spec and an all-`canceled` task set; true once one task is `passing`.
    #[tokio::test]
    async fn test_l2_any_task_passing_distinguishes_merge_work_from_all_canceled() {
        let pool = seeded().await;
        // Empty spec → nothing passing.
        assert!(!any_task_passing(&pool, &spec()).await.unwrap());

        // Two canceled tasks → settled, but still nothing to merge.
        let a = task(&pool, "T000000aa", TaskState::Canceled).await;
        let _b = task(&pool, "T000000bb", TaskState::Canceled).await;
        assert!(all_tasks_settled(&pool, &spec()).await.unwrap());
        assert!(
            !any_task_passing(&pool, &spec()).await.unwrap(),
            "an all-canceled task set has no merge work",
        );

        // One task passing → there IS merge work. (Drive via a fresh pool —
        // `canceled` is terminal, so task A cannot legally leave it.)
        drop(a);
        let pool = seeded().await;
        let _p = task(&pool, "T000000aa", TaskState::Passing).await;
        let _c = task(&pool, "T000000bb", TaskState::Canceled).await;
        assert!(
            any_task_passing(&pool, &spec()).await.unwrap(),
            "a passing task means the spec has merge work",
        );
    }
}
