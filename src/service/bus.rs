//! The `EventBus` — BOI's single chokepoint for state-machine transitions and
//! the observational event log, with the four-phase emit (design §2).
//!
//! This file is built in two tasks. **Part 1 (Task 4.2 — this section to the
//! adapters)** defines the emit-pipeline *port trait* ([`EmitObserver`]) and its
//! null-object production adapter ([`NoopObserver`]). **Part 2 (Task 4.3)**
//! defines the `EventBus` struct itself and the `persist` pipeline.
//!
//! ## Ports and adapters — why `service/` defines traits, not concrete types
//!
//! The capability the emit pipeline needs — OTel emission — physically lives in
//! `runtime/`, and the Layered Domain Architecture (§13) forbids `service/` from
//! naming a `runtime/` type. So `service/` defines the *port* (the trait below);
//! `runtime/` provides the adapter (`runtime::otel` is the wired observer). The
//! `service/` layer compiles and its tests pass independently against the
//! [`NoopObserver`] null object. (This is design patch G11 — §2's struct
//! originally named concrete `runtime/` types.)

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use sqlx::SqlitePool;

use crate::repo;
use crate::repo::db::RepoError;
use crate::repo::spec_runtime::TerminalReason;
use crate::service::transitions::{self, TransitionError};
use crate::types::event::BoiEvent;
use crate::types::ids::{SpecId, TaskId};
use crate::types::reasons::{BlockedReason, CancellationReason};
use crate::types::state::{SpecStatus, TaskState};
use crate::types::verdict::VerdictOutcome;

/// An [`EmitObserver`] failed while observing an event.
///
/// Observation is best-effort telemetry — the bus logs this `warn!` and
/// continues (emit-Phase 2). The single message-carrying variant is
/// deliberate: the real failure taxonomy belongs to the Phase 8a OTel adapter,
/// not to this port definition.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("observer failed: {0}")]
pub struct ObserverError(pub String);

/// Emit-Phase 2 port — best-effort observation of every emitted event.
///
/// The wired implementation is OTel span emission (Phase 8a). An `observe`
/// error never aborts the emit; the bus logs it `warn!` and continues.
///
/// `async-trait` boxes one future per call. That cost is acceptable at observe
/// frequency — once per emitted event — and is the price of `dyn` dispatch
/// (native async-fn-in-trait is not `dyn`-compatible at the pinned toolchain).
#[async_trait]
pub trait EmitObserver: Send + Sync {
    /// Observe an event. Best-effort: an `Err` is logged `warn!`, never fatal.
    async fn observe(&self, event: &BoiEvent) -> Result<(), ObserverError>;
}

/// The null-object [`EmitObserver`] — the wired default until the Phase 8a OTel
/// adapter lands.
///
/// This is a *production* type, not a test double: a bus with no real observer
/// holds one of these so `emit`'s Phase 2 loop is uniform (no `Option`, no
/// special-casing an empty `Vec`).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopObserver;

#[async_trait]
impl EmitObserver for NoopObserver {
    async fn observe(&self, _event: &BoiEvent) -> Result<(), ObserverError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Part 2 (Task 4.3) — the EventBus + emit pipeline.
// ---------------------------------------------------------------------------

/// An [`EventBus::emit`] failed.
///
/// Only emit-Phase 1 (persist) can abort an emit. A [`BusError::Persist`] wraps
/// a repo-layer failure (a row not found, a constraint hit); a
/// [`BusError::IllegalTransition`] is the transition guard rejecting a worker's
/// proposed state change (the bus DISPOSES — design §6). Emit-Phase 2
/// (observe) is best-effort and never produces a `BusError`.
#[derive(Debug, thiserror::Error)]
pub enum BusError {
    /// The persist phase failed at the repo layer.
    #[error("event persist failed: {0}")]
    Persist(#[from] RepoError),
    /// The transition guard rejected the proposed state change.
    #[error("event rejected: {0}")]
    IllegalTransition(#[from] TransitionError),
}

/// The event bus — BOI's single chokepoint for state-machine transitions and
/// the observational event log (design §2).
///
/// Every notable thing flows through [`EventBus::emit`]. `emit` runs emit-Phases
/// 1–2 (persist → observe); it owns **no channel** — emit-Phase 4
/// (notifying the orchestrator) is the caller's responsibility, split out by
/// the C1 producer-split (see [`EventBus::emit`]).
///
/// Construct via [`EventBus::new`]; the fields are private so the only way to
/// build one is the constructor (`boot`, Phase 9, uses it — never a
/// cross-module struct literal).
pub struct EventBus {
    pool: SqlitePool,
    observers: Vec<Arc<dyn EmitObserver>>,
}

// A future non-`Send`/non-`Sync` field fails the build here — at the type —
// rather than at a distant `tokio::spawn` call site (review S1).
const _: () = {
    fn _assert_send_sync<T: Send + Sync>() {}
    fn _check() {
        _assert_send_sync::<EventBus>();
    }
};

impl EventBus {
    /// Construct an event bus over `pool` with the given observer adapters.
    ///
    /// Until the Phase 8a runtime adapter lands, `boot` passes
    /// `vec![Arc::new(NoopObserver)]`.
    pub fn new(pool: SqlitePool, observers: Vec<Arc<dyn EmitObserver>>) -> Self {
        Self { pool, observers }
    }

    /// Borrow the bus's connection pool.
    ///
    /// The bus is the `phase_runs` write chokepoint, but Phase 5a's drain task
    /// also needs the pool for its scoped liveness `record_heartbeat` write
    /// (`registry::drain_phase`) — and the drain holds an `Arc<EventBus>`, not
    /// a separate pool handle. Exposing the pool read-only keeps the drain from
    /// having to be threaded a second `SqlitePool` clone.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Emit an event through emit-Phases 1–3.
    ///
    /// - **Phase 1 — persist** (critical): the state-machine write. An `Err`
    ///   here — a rejected transition or a repo failure — **aborts** the emit
    ///   before any observer or bridge runs (the worker's proposed transition
    ///   is rejected; the bus DISPOSES).
    /// - **Phase 2 — observe** (best-effort): every [`EmitObserver`]. An `Err`
    ///   is logged `warn!` and the emit continues.
    ///
    /// Emit-Phase 4 (notify the orchestrator) is **not** here. Per the C1
    /// producer-split, putting the orchestrator-channel send inside `emit`
    /// self-deadlocks — the orchestrator's `handle` loop is the channel's sole
    /// consumer and `handle` itself emits events. So `EventBus` owns no
    /// channel: the *caller* runs Phase 4 — Phase 5a drain tasks `send` on a
    /// `daemon_tx` clone after `emit` returns `Ok`, and the orchestrator's own
    /// `handle` pushes onto a loop-local `VecDeque`. Phase-1-before-Phase-4
    /// ordering still holds (emit, then send); only the *transport* of Phase 4
    /// is the caller's.
    pub async fn emit(&self, event: &BoiEvent) -> Result<(), BusError> {
        self.persist(event).await?; // Phase 1 — abort on Err
        for observer in &self.observers {
            // Phase 2 — warn on Err
            if let Err(e) = observer.observe(event).await {
                tracing::warn!(error = %e, "emit observer failed (telemetry — best-effort)");
            }
        }
        Ok(())
    }

    /// Emit-Phase 1 — persist an event's state-machine effect.
    ///
    /// The `match` has **no `_` arm**: every [`BoiEvent`] variant is named, so a
    /// future variant breaks the build until it is handled here — a state
    /// mutation can never silently fall through (review S10). Lifecycle
    /// variants run their transition guard before the repo write; the
    /// OTel-only group is an explicit `=> Ok(())` arm list, never a wildcard.
    async fn persist(&self, event: &BoiEvent) -> Result<(), BusError> {
        match event {
            // ---- Task lifecycle: fetch current state, guard, then write ----
            BoiEvent::TaskStarted { task_id, .. } => {
                self.transition_task(task_id, TaskState::Active, None, None)
                    .await
            }
            BoiEvent::TaskBlocked {
                task_id, reason, ..
            } => {
                self.transition_task(task_id, TaskState::Blocked, None, Some(reason.clone()))
                    .await
            }
            BoiEvent::TaskUnblocked { task_id, .. } => {
                self.transition_task(task_id, TaskState::Active, None, None)
                    .await
            }
            BoiEvent::TaskPassed { task_id, .. } => {
                self.transition_task(task_id, TaskState::Passing, None, None)
                    .await
            }
            BoiEvent::TaskCanceled {
                task_id, reason, ..
            } => {
                self.transition_task(
                    task_id,
                    TaskState::Canceled,
                    Some(TerminalReason::Cancellation(reason.clone())),
                    None,
                )
                .await
            }

            // ---- Spec lifecycle: fetch current status, guard, then write ----
            BoiEvent::SpecStarted { spec_id } => {
                self.transition_spec(spec_id, SpecStatus::Running, None)
                    .await
            }
            BoiEvent::SpecCompleted { spec_id } => {
                self.transition_spec(spec_id, SpecStatus::Completed, None)
                    .await
            }
            BoiEvent::SpecFailed { spec_id, reason } => {
                self.transition_spec(
                    spec_id,
                    SpecStatus::Failed,
                    Some(TerminalReason::Failure(reason.clone())),
                )
                .await
            }
            BoiEvent::SpecCanceled { spec_id, reason } => {
                self.transition_spec(
                    spec_id,
                    SpecStatus::Canceled,
                    Some(TerminalReason::Cancellation(reason.clone())),
                )
                .await
            }

            // ---- Observational rows the bus owns: phase_runs, decisions ----
            BoiEvent::PhaseStarted {
                phase_run_id,
                spec_id,
                task_id,
                phase,
                provider,
                iteration,
                ..
            } => {
                // `insert_start` needs the authored `spec_version` this run
                // executes against — the spec's live version (design §3.0).
                let spec_version = repo::spec_runtime::fetch(&self.pool, spec_id)
                    .await?
                    .current_version;
                repo::phase_runs::insert_start(
                    &self.pool,
                    phase_run_id,
                    spec_id,
                    task_id.as_ref(),
                    phase,
                    *iteration,
                    spec_version,
                    provider,
                    None, // worker_id — not carried on the event; set by Phase 7 later if needed
                    Utc::now(),
                )
                .await?;
                Ok(())
            }
            BoiEvent::PhaseCompleted {
                phase_run_id,
                verdict,
                tokens_in,
                tokens_out,
                ..
            } => {
                // A `Passing` verdict carries the files it touched; the other
                // outcomes touch nothing recorded here.
                let files_touched: &[std::path::PathBuf] = match &verdict.outcome {
                    VerdictOutcome::Passing { evidence } => &evidence.files_touched,
                    VerdictOutcome::Redo { .. }
                    | VerdictOutcome::Blocked { .. }
                    | VerdictOutcome::Fail { .. }
                    | VerdictOutcome::Canceled => &[],
                };
                // G25.1: the token figures the event carries are now
                // persisted on the `phase_runs` row (the `0002` migration's
                // columns) — Phase 8b's `metrics` aggregate reads them.
                // (Per the 2026-06-01 directive the per-run dollar column
                // is gone — migration 0003 — and the event no longer carries
                // a cost figure.)
                repo::phase_runs::update_end(
                    &self.pool,
                    phase_run_id,
                    &verdict.synopsis,
                    verdict,
                    files_touched,
                    *tokens_in,
                    *tokens_out,
                    Utc::now(),
                )
                .await?;
                Ok(())
            }
            BoiEvent::DecisionMade { decision } => {
                repo::decisions::insert(&self.pool, decision).await?;
                Ok(())
            }

            // ---- Observed-only: no SQLite row of their own.
            //      Explicit arm list — NO wildcard. ----
            BoiEvent::PlanRevised { .. }
            | BoiEvent::ReportReceived { .. }
            | BoiEvent::VerifyChecked { .. }
            | BoiEvent::ToolInvoked { .. }
            | BoiEvent::ErrorEncountered { .. } => Ok(()),
        }
    }

    /// Fetch a task's current state, guard the transition, then write it.
    async fn transition_task(
        &self,
        task_id: &TaskId,
        to: TaskState,
        terminal_reason: Option<TerminalReason>,
        blocked_reason: Option<BlockedReason>,
    ) -> Result<(), BusError> {
        let current = repo::task_runtime::fetch(&self.pool, task_id).await?;
        let from: TaskState = current.state.parse().map_err(|e| {
            BusError::Persist(RepoError::NotFound(format!("corrupt task state: {e}")))
        })?;
        transitions::check_task(task_id, from, to)?;
        repo::task_runtime::update_state(
            &self.pool,
            task_id,
            to,
            terminal_reason,
            blocked_reason,
            Utc::now(),
        )
        .await?;
        Ok(())
    }

    /// Fetch a spec's current status, guard the transition, then write it.
    async fn transition_spec(
        &self,
        spec_id: &SpecId,
        to: SpecStatus,
        reason: Option<TerminalReason>,
    ) -> Result<(), BusError> {
        let current = repo::spec_runtime::fetch(&self.pool, spec_id).await?;
        let from: SpecStatus = current.status.parse().map_err(|e| {
            BusError::Persist(RepoError::NotFound(format!("corrupt spec status: {e}")))
        })?;
        transitions::check_spec(spec_id, from, to)?;
        repo::spec_runtime::update_status(&self.pool, spec_id, to, reason, Utc::now()).await?;
        // Cascade: once a spec reaches a terminal status, NO task of it may
        // remain `active`. `active` uniquely means "a worker is executing this
        // right now" — impossible under a terminal spec — so a stranded
        // `active` row is a lie that corrupts liveness queries (the ghost-task
        // bug: a `failed` spec still showing an `active` task with no worker).
        // We sweep ONLY `active`: `blocked` / `not_started` are passive resting
        // states the design deliberately retains under a `failed` spec as a
        // forensic record (see the orchestrator's
        // `failed_plan_revision_*_not_strands_the_task` /
        // `adjustment_side_chain_caps_*` tests). Full non-passing cascade on
        // `canceled` is the orchestrator's job (the `spec_canceled_cascades_*`
        // test); this chokepoint only closes the `active` liveness gap, and it
        // is idempotent with that path (already-terminal tasks are skipped).
        if matches!(to, SpecStatus::Failed | SpecStatus::Canceled) {
            self.cancel_stranded_active_tasks(spec_id).await?;
        }
        Ok(())
    }

    /// Sweep every still-`active` task of `spec_id` to terminal `canceled`
    /// (reason [`CancellationReason::SpecCanceled`] — the parent spec reached a
    /// terminal state). Non-`active` tasks are left untouched. Each sweep
    /// re-checks the task guard via [`Self::transition_task`], so an illegal
    /// edge stays loud.
    async fn cancel_stranded_active_tasks(&self, spec_id: &SpecId) -> Result<(), BusError> {
        let tasks = repo::task_runtime::tasks_for_spec(&self.pool, spec_id).await?;
        for row in tasks {
            let state: TaskState = row.state.parse().map_err(|e| {
                BusError::Persist(RepoError::NotFound(format!("corrupt task state: {e}")))
            })?;
            if state != TaskState::Active {
                continue; // only `active` is an illegal resting state under a terminal spec
            }
            let task_id = TaskId::new(&row.task_id).map_err(|e| {
                BusError::Persist(RepoError::NotFound(format!("corrupt task id: {e}")))
            })?;
            self.transition_task(
                &task_id,
                TaskState::Canceled,
                Some(TerminalReason::Cancellation(
                    CancellationReason::SpecCanceled,
                )),
                None,
            )
            .await?;
        }
        Ok(())
    }
}

/// Test doubles for the emit-pipeline ports.
///
/// Behind `#[cfg(test)]` and `pub(crate)` so Phase 5's `service/` test modules
/// can consume them — but the crate's *public* surface (`service/mod.rs`
/// re-exports) never exposes a `Recording*` / `Mock*` type.
#[cfg(test)]
pub(crate) mod testkit {
    use std::sync::{Arc, Mutex};

    use super::{EmitObserver, ObserverError};
    use crate::types::event::BoiEvent;
    use async_trait::async_trait;

    /// An [`EmitObserver`] that records every event it sees, in order.
    ///
    /// `std::sync::Mutex` (NOT `tokio::sync::Mutex`) is deliberate: `observe`
    /// only pushes and returns — it never `.await`s while holding the lock —
    /// and non-async test bodies must read [`RecordingObserver::seen`] without
    /// an `.await`.
    #[derive(Clone, Default)]
    pub(crate) struct RecordingObserver {
        events: Arc<Mutex<Vec<BoiEvent>>>,
    }

    impl RecordingObserver {
        /// A fresh recorder with an empty log.
        pub(crate) fn new() -> Self {
            Self::default()
        }

        /// A snapshot of every event observed so far, in observation order.
        pub(crate) fn seen(&self) -> Vec<BoiEvent> {
            self.events
                .lock()
                .expect("RecordingObserver mutex poisoned")
                .clone()
        }

        /// How many events have been observed.
        pub(crate) fn count(&self) -> usize {
            self.events
                .lock()
                .expect("RecordingObserver mutex poisoned")
                .len()
        }
    }

    #[async_trait]
    impl EmitObserver for RecordingObserver {
        async fn observe(&self, event: &BoiEvent) -> Result<(), ObserverError> {
            self.events
                .lock()
                .expect("RecordingObserver mutex poisoned")
                .push(event.clone());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ids::SpecId;

    fn sample_event() -> BoiEvent {
        BoiEvent::SpecStarted {
            spec_id: SpecId::new("S0000001a").unwrap(),
        }
    }

    /// `NoopObserver` returns `Ok` for any event.
    #[tokio::test]
    async fn test_l1_noop_adapters_return_ok() {
        let ev = sample_event();
        assert!(NoopObserver.observe(&ev).await.is_ok());
    }

    /// `RecordingObserver` captures every event it observes, in order.
    #[tokio::test]
    async fn test_l1_recording_observer_captures_in_order() {
        use testkit::RecordingObserver;
        let rec = RecordingObserver::new();
        assert_eq!(rec.count(), 0, "starts empty");

        let first = BoiEvent::SpecStarted {
            spec_id: SpecId::new("S0000001a").unwrap(),
        };
        let second = BoiEvent::SpecCompleted {
            spec_id: SpecId::new("S0000002b").unwrap(),
        };
        rec.observe(&first).await.unwrap();
        rec.observe(&second).await.unwrap();

        let seen = rec.seen();
        assert_eq!(seen.len(), 2);
        // Observation order is preserved.
        assert!(matches!(seen[0], BoiEvent::SpecStarted { .. }));
        assert!(matches!(seen[1], BoiEvent::SpecCompleted { .. }));
    }

    // -----------------------------------------------------------------------
    // Task 4.3 L2 tests — real `sqlite::memory:` pool + RecordingObserver.
    // -----------------------------------------------------------------------

    use crate::repo;
    use crate::repo::db::connect;
    use crate::types::ids::{PhaseRunId, TaskId};
    use crate::types::reasons::{BlockedReason, CancellationReason, FailureReason};
    use crate::types::verdict::{Evidence, VerdictOutcome, WorkerVerdict};
    use chrono::Utc;
    use testkit::RecordingObserver;

    fn spec_id() -> SpecId {
        SpecId::new("S0000001a").unwrap()
    }
    fn task_id() -> TaskId {
        TaskId::new("T0000001a").unwrap()
    }

    /// An in-memory pool seeded with a spec (specs row + v1 snapshot +
    /// initialized `spec_runtime`) and one `not_started` task.
    async fn seeded_pool() -> sqlx::SqlitePool {
        let pool = connect("sqlite::memory:").await.unwrap();
        repo::insert_spec(&pool, &spec_id(), Utc::now())
            .await
            .unwrap();
        repo::append_version(
            &pool,
            &spec_id(),
            1,
            &serde_json::json!({ "title": "demo" }),
            repo::VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        repo::spec_runtime::initialize(&pool, &spec_id(), 1)
            .await
            .unwrap();
        repo::task_runtime::insert_task(&pool, &task_id(), &spec_id(), Some("setup"))
            .await
            .unwrap();
        pool
    }

    /// A bus with one `RecordingObserver` (returned for assertions).
    fn bus_with_recorder(pool: sqlx::SqlitePool) -> (EventBus, RecordingObserver) {
        let rec = RecordingObserver::new();
        let bus = EventBus::new(pool, vec![Arc::new(rec.clone())]);
        (bus, rec)
    }

    fn passing_verdict() -> WorkerVerdict {
        WorkerVerdict {
            synopsis: "did the work".into(),
            outcome: VerdictOutcome::Passing {
                evidence: Evidence {
                    files_touched: vec![std::path::PathBuf::from("src/a.rs")],
                    verifications: vec![],
                    summary: "ok".into(),
                    merge_commit_sha: None,
                },
            },
        }
    }

    /// Emitting `TaskStarted` against a seeded `not_started` task flips the
    /// `task_runtime` row to `active` (emit-Phase 1) and the observer sees the
    /// event (emit-Phase 2).
    #[tokio::test]
    async fn test_l2_emit_task_started_flips_state_and_observes() {
        let pool = seeded_pool().await;
        let (bus, rec) = bus_with_recorder(pool.clone());

        bus.emit(&BoiEvent::TaskStarted {
            spec_id: spec_id(),
            task_id: task_id(),
        })
        .await
        .unwrap();

        // Phase 1 — state flipped.
        let row = repo::task_runtime::fetch(&pool, &task_id()).await.unwrap();
        assert_eq!(row.state, "active");
        // Phase 2 — observer saw exactly that event.
        let seen = rec.seen();
        assert_eq!(seen.len(), 1);
        assert!(matches!(seen[0], BoiEvent::TaskStarted { .. }));
    }

    /// Emitting `TaskBlocked` against an already-`passing` task is rejected by
    /// the transition guard: `emit` returns `Err(IllegalTransition)`, the
    /// `task_runtime` state is unchanged, and the observer is NOT called —
    /// emit-Phase 1 aborted before emit-Phase 2.
    #[tokio::test]
    async fn test_l2_emit_illegal_transition_aborts_before_observe() {
        let pool = seeded_pool().await;
        // Drive the task to the terminal `passing` state first.
        repo::task_runtime::update_state(
            &pool,
            &task_id(),
            TaskState::Passing,
            None,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        let (bus, rec) = bus_with_recorder(pool.clone());

        let err = bus
            .emit(&BoiEvent::TaskBlocked {
                spec_id: spec_id(),
                task_id: task_id(),
                reason: BlockedReason::Manual {
                    operator_note: None,
                },
            })
            .await
            .unwrap_err();
        assert!(
            matches!(err, BusError::IllegalTransition(_)),
            "passing -> blocked must be rejected, got {err:?}",
        );

        // No state change — still `passing`.
        let row = repo::task_runtime::fetch(&pool, &task_id()).await.unwrap();
        assert_eq!(row.state, "passing");
        // The observer was never reached — Phase 1 aborted the emit.
        assert_eq!(
            rec.count(),
            0,
            "observer must not run after a Phase 1 abort"
        );
    }

    /// A spec reaching a terminal *failure* status sweeps any still-`active`
    /// task to terminal `canceled` (reason `SpecCanceled`) — `active` is a lie
    /// under a terminal spec (no worker is running). But `blocked` and
    /// `not_started` tasks are DELIBERATELY retained as a forensic record (the
    /// design the orchestrator's `..._not_strands_the_task` tests pin), and
    /// `passing` is already terminal. This closes the ghost-`active` bug
    /// found (a `failed` spec showing an `active` task months later) without
    /// erasing diagnostic state.
    #[tokio::test]
    async fn test_l2_spec_failed_cancels_only_stranded_active_tasks() {
        let pool = seeded_pool().await; // seeds T0000001a as not_started
        // Drive the spec to `running` so `running -> failed` is legal.
        repo::spec_runtime::update_status(&pool, &spec_id(), SpecStatus::Running, None, Utc::now())
            .await
            .unwrap();

        // Three more tasks: one active, one blocked, one already passing.
        let active = TaskId::new("T0000002a").unwrap();
        let blocked = TaskId::new("T0000003a").unwrap();
        let passing = TaskId::new("T0000004a").unwrap();
        for (t, st) in [
            (&active, TaskState::Active),
            (&blocked, TaskState::Blocked),
            (&passing, TaskState::Passing),
        ] {
            repo::task_runtime::insert_task(&pool, t, &spec_id(), None)
                .await
                .unwrap();
            repo::task_runtime::update_state(&pool, t, st, None, None, Utc::now())
                .await
                .unwrap();
        }

        let (bus, _rec) = bus_with_recorder(pool.clone());
        bus.emit(&BoiEvent::SpecFailed {
            spec_id: spec_id(),
            reason: FailureReason::DaemonCrash,
        })
        .await
        .unwrap();

        // Spec is failed.
        let spec_row = repo::spec_runtime::fetch(&pool, &spec_id()).await.unwrap();
        assert_eq!(spec_row.status, "failed");

        // The stranded `active` task is swept to `canceled{SpecCanceled}`.
        let a = repo::task_runtime::fetch(&pool, &active).await.unwrap();
        assert_eq!(
            a.state, "canceled",
            "an active task must not survive a terminal spec"
        );
        let reason: CancellationReason =
            serde_json::from_value(a.cancellation_reason.expect("cancel reason set"))
                .expect("reason parses");
        assert_eq!(reason, CancellationReason::SpecCanceled);

        // `blocked` and `not_started` are retained as forensic record; `passing`
        // is untouched.
        assert_eq!(
            repo::task_runtime::fetch(&pool, &blocked)
                .await
                .unwrap()
                .state,
            "blocked",
            "a blocked task's diagnostic state is retained under a failed spec",
        );
        assert_eq!(
            repo::task_runtime::fetch(&pool, &task_id())
                .await
                .unwrap()
                .state,
            "not_started",
            "a not_started task is retained under a failed spec",
        );
        assert_eq!(
            repo::task_runtime::fetch(&pool, &passing)
                .await
                .unwrap()
                .state,
            "passing",
            "a terminal task must not be re-swept",
        );
    }

    /// `PhaseStarted` then `PhaseCompleted` for the same `phase_run_id`: the
    /// bus INSERTs an open `phase_runs` row, then UPDATEs it closed.
    #[tokio::test]
    async fn test_l2_emit_phase_started_then_completed_writes_phase_run() {
        let pool = seeded_pool().await;
        let (bus, _rec) = bus_with_recorder(pool.clone());
        let pr = PhaseRunId::new("P0000001a").unwrap();

        bus.emit(&BoiEvent::PhaseStarted {
            phase_run_id: pr.clone(),
            spec_id: spec_id(),
            task_id: Some(task_id()),
            phase: "execute".into(),
            provider: "claude_code".into(),
            model: "claude-opus-4-7".into(),
            iteration: 0,
        })
        .await
        .unwrap();

        let open = repo::phase_runs::fetch(&pool, &pr).await.unwrap();
        assert!(open.is_open(), "row open after PhaseStarted");
        assert_eq!(open.phase, "execute");
        assert_eq!(
            open.spec_version, 1,
            "spec_version sourced from spec_runtime"
        );

        bus.emit(&BoiEvent::PhaseCompleted {
            phase_run_id: pr.clone(),
            spec_id: spec_id(),
            task_id: Some(task_id()),
            phase: "execute".into(),
            verdict: passing_verdict(),
            tokens_in: 100,
            tokens_out: 20,
            duration_ms: 1000,
        })
        .await
        .unwrap();

        let closed = repo::phase_runs::fetch(&pool, &pr).await.unwrap();
        assert!(!closed.is_open(), "row closed after PhaseCompleted");
        assert_eq!(closed.synopsis, "did the work");
        // The `Passing` verdict's `files_touched` was persisted.
        assert_eq!(closed.files_touched.as_array().unwrap().len(), 1);
    }

    /// `DecisionMade` inserts the decision row (emit-Phase 1 for the
    /// `decisions` table).
    #[tokio::test]
    async fn test_l2_emit_decision_made_inserts_row() {
        let pool = seeded_pool().await;
        let (bus, _rec) = bus_with_recorder(pool.clone());
        // A runtime decision needs a parent phase_run — create one.
        let pr = PhaseRunId::new("P0000001a").unwrap();
        repo::phase_runs::insert_start(
            &pool,
            &pr,
            &spec_id(),
            Some(&task_id()),
            "execute",
            0,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        let decision_id = repo::allocate_decision_id(&pool).await.unwrap();
        let decision = crate::types::decision::DecisionRecord::new_runtime(
            decision_id.clone(),
            spec_id(),
            Some(pr),
            "Use sqlx".into(),
            "Compile-checked queries.".into(),
            "Type safety.".into(),
            vec![],
            None,
            Utc::now(),
        )
        .unwrap();

        bus.emit(&BoiEvent::DecisionMade { decision })
            .await
            .unwrap();

        let back = repo::fetch_by_id(&pool, &decision_id).await.unwrap();
        assert_eq!(back.title, "Use sqlx");
    }

    /// A `SpecCanceled` against a `queued` spec is legal (the `queued →
    /// canceled` asymmetry) and writes the `canceled` status + reason.
    #[tokio::test]
    async fn test_l2_emit_spec_canceled_from_queued_is_legal() {
        let pool = seeded_pool().await;
        let (bus, _rec) = bus_with_recorder(pool.clone());

        bus.emit(&BoiEvent::SpecCanceled {
            spec_id: spec_id(),
            reason: CancellationReason::Operator {
                note: Some("scope cut".into()),
            },
        })
        .await
        .unwrap();

        let row = repo::spec_runtime::fetch(&pool, &spec_id()).await.unwrap();
        assert_eq!(row.status, "canceled");
        assert!(row.cancellation_reason.is_some());
    }

    /// An OTel-only event (`VerifyChecked`) persists no row but is still
    /// observed — emit-Phase 1 is a no-op, emit-Phase 2 carries it.
    #[tokio::test]
    async fn test_l2_emit_otel_only_event_persists_nothing_but_observes() {
        let pool = seeded_pool().await;
        let (bus, rec) = bus_with_recorder(pool.clone());

        bus.emit(&BoiEvent::VerifyChecked {
            spec_id: spec_id(),
            task_id: task_id(),
            level: "l1".into(),
            command: "cargo test".into(),
            exit_code: 0,
            stdout_excerpt: "ok".into(),
        })
        .await
        .unwrap();

        // Task state untouched (still `not_started`); observer saw the event.
        assert_eq!(
            repo::task_runtime::fetch(&pool, &task_id())
                .await
                .unwrap()
                .state,
            "not_started",
        );
        assert_eq!(rec.count(), 1);
    }

    /// The `_assert_send_sync::<EventBus>()` compile-time assertion is present;
    /// this test simply pins that `EventBus` is `Send + Sync` at runtime too,
    /// so a regression is caught even if the `const _` block were removed.
    #[tokio::test]
    async fn test_l2_event_bus_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<EventBus>();
    }
}
