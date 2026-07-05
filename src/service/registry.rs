//! The [`PhaseExecutor`] port and the per-phase-run *drain task* — Phase 5a's
//! ports-and-adapters seam plus the concurrent producer that feeds the
//! orchestrator's channel.
//!
//! ## The `PhaseExecutor` port (ports-and-adapters, continued)
//!
//! Phase execution physically lives in `runtime/` — `GooseRuntime` for worker
//! phases (Phase 7), `DETERMINISTIC_STEPS` for deterministic phases (Phase 6) —
//! and the Layered Domain Architecture forbids `service/` from naming a
//! `runtime/` type. So `service/` defines the *port* ([`PhaseExecutor`]); the
//! real adapters land in Phases 6/7. Phase 5a's own tests drive the
//! orchestrator through the `testkit::MockExecutor` double (gated on
//! `#[cfg(any(test, feature = "testkit"))]`), with zero `runtime/` dependency.
//!
//! ## The drain task and the C1 producer-split
//!
//! [`EventBus::emit`] runs emit-Phases 1–3 only and owns no channel — Phase 4
//! (notify the orchestrator) is split out by producer. A **drain task** is one
//! of the two concurrent producers (the other is the sweeper): a separate
//! `tokio` task — *not* the channel consumer — so it may safely
//! `daemon_tx.send().await` on the bounded channel without self-deadlocking.
//! The orchestrator's own `handle_*` code is NEVER a channel producer (it
//! pushes onto a loop-local `VecDeque`); that invariant is what makes the
//! bounded `mpsc::channel(1024)` deadlock-free.
//!
//! [`drain_phase`] is `run_phase`'s (Phase 5a `orchestrator.rs`) clock-in: it
//! emits `PhaseStarted` (so `run_phase` is never itself a channel producer —
//! review C1), relays every executor stream event through the bus, and — no
//! matter the outcome (clean completion, cancellation, stream error, or a
//! caught panic) — sends exactly one [`DaemonNotification::DrainTerminated`] as
//! its last act. A panicking drain becomes a *visible* signal, not a silently
//! dropped `JoinHandle` (review C3).

use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::FutureExt;
use futures::stream::{BoxStream, StreamExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::PhaseDef;
use crate::repo;
use crate::service::bus::EventBus;
use crate::types::context::PhaseContext;
use crate::types::event::BoiEvent;
use crate::types::ids::PhaseRunId;

/// Port: execute one phase.
///
/// The returned stream yields [`BoiEvent`]s and, in the normal case, terminates
/// in exactly one `PhaseCompleted`; on cancellation it may end early. Worker
/// phases route to `GooseRuntime` (Phase 7), deterministic phases to
/// `DETERMINISTIC_STEPS` (Phase 6). A plain `fn` returning a boxed stream is
/// `dyn`-compatible without `async-trait` — unlike Phase 4's `async-trait`
/// ports, this one is stream-shaped, which `async-trait` cannot express.
///
/// # Contract — Phase 6/7 implementations MUST honour
///
/// - All captured state is moved/cloned into the `'static` stream — no `&self`
///   field is borrowed.
/// - Deterministic-phase adapters LIFT `StepOutcome` into `WorkerVerdict`
///   before the terminal `PhaseCompleted`: a `Pass{evidence}` becomes a
///   `WorkerVerdict` with `VerdictOutcome::Passing{evidence}`, a
///   `Fail{error_why_fix}` a `WorkerVerdict` with `VerdictOutcome::Fail{..}`.
///   A deterministic phase therefore only ever yields `Passing` or `Fail` —
///   never `Redo`/`Blocked`. This is the C6 verdict seam that lets `routing.rs`
///   keep one 4-arm verdict router for worker AND deterministic phases.
/// - When `cancel` fires, the executor MUST terminate any underlying
///   subprocess and end the returned stream within a bounded grace period.
pub trait PhaseExecutor: Send + Sync {
    /// Execute `phase` against `ctx`, returning a stream of lifecycle events.
    fn execute(
        &self,
        phase: PhaseDef,
        ctx: PhaseContext,
        cancel: CancellationToken,
    ) -> BoxStream<'static, BoiEvent>;
}

/// A message into the orchestrator's bounded channel.
///
/// The only two producers are the [`drain_phase`] tasks and the heartbeat
/// sweeper (Phase 5a `sweeper.rs`) — never the orchestrator itself (C1).
#[derive(Debug, Clone)]
pub enum DaemonNotification {
    /// A `BoiEvent` the bus has already emitted (persist → observe → bridge);
    /// the orchestrator routes it.
    Event(BoiEvent),
    /// A drain task ended — exactly one of these per phase run. The
    /// orchestrator's `handle_drain_terminated` removes the [`InFlight`] entry
    /// and, on any status that did NOT relay a terminal `PhaseCompleted`
    /// (`Panicked`, `StreamError`, `CompletedWithoutVerdict`), surfaces a
    /// visible `TaskBlocked` / `SpecFailed`.
    DrainTerminated {
        /// The phase run that ended.
        phase_run_id: PhaseRunId,
        /// How it ended.
        status: DrainStatus,
    },
}

/// How a [`drain_phase`] task ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainStatus {
    /// The executor stream ran to completion AND a terminal `PhaseCompleted`
    /// for this phase run was relayed through the bus — the normal path. The
    /// orchestrator has a verdict to route; `handle_drain_terminated` is
    /// cleanup-only.
    Completed,
    /// The drain's `cancel` token fired and the drain stopped early.
    Canceled,
    /// The executor stream ended *cleanly* but never yielded a terminal
    /// `PhaseCompleted` for this `phase_run_id` — so no verdict ever reached
    /// routing (review B-svc-1). Treated exactly like a `StreamError`: the
    /// orchestrator surfaces a visible `TaskBlocked` / `SpecFailed` rather than
    /// leaving the task stranded `active` forever. (A Phase 6/7 `PhaseExecutor`
    /// adapter that honours G21.5 never produces this; a buggy or empty
    /// executor stream does.)
    CompletedWithoutVerdict,
    /// A stream event was rejected by the bus (an illegal transition or a
    /// persist fault) — the drain terminated loudly. Carries the error string.
    StreamError(String),
    /// The drain body panicked — caught by `catch_unwind` so it surfaces as a
    /// signal, never a silently swallowed `JoinHandle` panic (review C3).
    Panicked,
}

/// The orchestrator's registry entry for one in-flight phase run.
///
/// It holds NO `JoinHandle` — deliberately. A stored `JoinHandle<()>` that
/// nobody awaits silently swallows a panic; instead the drain self-reports via
/// `DrainTerminated`, and `handle_drain_terminated` removes the entry on that
/// signal (review C3).
///
/// Beyond the `cancel` token it carries the run's `spec_id` / `task_id` —
/// a small extension of the plan's `{ cancel }`-only sketch. The orchestrator
/// keys `in_flight` by [`PhaseRunId`], but `TaskBlocked` must cancel *the
/// drain of one task* and `SpecCanceled` *every drain of one spec*; without the
/// identity here the orchestrator would need a second index. (The "no
/// `JoinHandle`" invariant — the load-bearing part of the plan's note — holds.)
#[derive(Debug)]
pub struct InFlight {
    /// Fires to cancel the phase run's drain + underlying executor.
    pub cancel: CancellationToken,
    /// The spec this phase run belongs to.
    pub spec_id: crate::types::ids::SpecId,
    /// The task this phase run belongs to — `None` for a spec-level phase.
    pub task_id: Option<crate::types::ids::TaskId>,
    /// The phase this run executes. Load-bearing for the post-`<tasks>`
    /// resume's idempotence guard (review M1 finding 3): a second settling
    /// event must SEE the already-spawned resume drain *synchronously* —
    /// the drain's `phase_runs` row is only INSERTed once the spawned task
    /// emits `PhaseStarted`, which may not have run yet when the next event
    /// is handled on the orchestrator loop.
    pub phase: String,
}

/// Drain one phase run: emit `PhaseStarted`, relay the executor stream through
/// the bus onto `daemon_tx`, and send exactly one `DrainTerminated` at the end.
///
/// `run_phase` (Phase 5a `orchestrator.rs`) `tokio::spawn`s this once per phase
/// run, after `Arc::clone`-ing everything it captures (so the spawned future is
/// `'static` and borrows nothing from the orchestrator — review S1).
///
/// Steps:
///
/// 1. **First action** — build `PhaseStarted`, `bus.emit(&ev)`,
///    `daemon_tx.send(Event(ev))`. `run_phase` does NOT emit `PhaseStarted` —
///    the drain does, so the orchestrator's `handle` is never a channel
///    producer (review C1). The bus's `persist(PhaseStarted)` INSERTs the
///    `phase_runs` row.
/// 2. **Loop** — `select!` over `stream.next()` vs `cancel.cancelled()`. For
///    each stream `BoiEvent`: `bus.emit`; on `Ok` → `daemon_tx.send(Event)`;
///    on `Err` (an illegal transition the bus DISPOSED, or a persist fault) →
///    `error!` LOUD and terminate the drain with `StreamError` — a worker
///    proposing an illegal transition is never a silent `let _ =`. After a
///    relayed event, record a heartbeat (scoped — see [`record_heartbeat`]).
///    The loop also tracks whether a terminal `PhaseCompleted` for *this*
///    `phase_run_id` was relayed — when the stream ends without one the drain
///    returns [`DrainStatus::CompletedWithoutVerdict`] so the orchestrator
///    surfaces a visible failure, never a stranded `active` task (B-svc-1).
/// 3. **Terminal** — the loop body runs inside `catch_unwind`; whatever the
///    outcome the drain's last act is `daemon_tx.send(DrainTerminated{..})`.
///
/// [`record_heartbeat`]: crate::repo::phase_runs::record_heartbeat
pub async fn drain_phase(
    bus: Arc<EventBus>,
    daemon_tx: mpsc::Sender<DaemonNotification>,
    executor: Arc<dyn PhaseExecutor>,
    phase_def: PhaseDef,
    ctx: PhaseContext,
    cancel: CancellationToken,
    phase_run_id: PhaseRunId,
) {
    // The whole drain body is wrapped in `catch_unwind` so a panic becomes a
    // `DrainTerminated{Panicked}` signal (review C3). `AssertUnwindSafe` is
    // sound here: a panic is *explicitly* handled — it does not silently leave
    // a half-mutated value in use; the orchestrator treats `Panicked` as a
    // hard, visible failure of this phase run.
    let body = drain_body(
        bus,
        daemon_tx.clone(),
        executor,
        phase_def,
        ctx,
        cancel,
        phase_run_id.clone(),
    );
    let status = match AssertUnwindSafe(body).catch_unwind().await {
        Ok(status) => status,
        Err(_panic) => {
            tracing::error!(
                phase_run_id = %phase_run_id,
                "drain task panicked — surfacing as DrainTerminated{{Panicked}}",
            );
            DrainStatus::Panicked
        }
    };

    // The drain's LAST act — exactly one `DrainTerminated` per phase run. A
    // closed channel here means the orchestrator already exited; log and move
    // on (there is no one left to notify).
    if daemon_tx
        .send(DaemonNotification::DrainTerminated {
            phase_run_id: phase_run_id.clone(),
            status,
        })
        .await
        .is_err()
    {
        tracing::warn!(
            phase_run_id = %phase_run_id,
            "drain could not send DrainTerminated — orchestrator channel closed",
        );
    }
}

/// The fallible inner body of [`drain_phase`] — separated so the whole thing
/// can be `catch_unwind`-wrapped while still returning a [`DrainStatus`].
async fn drain_body(
    bus: Arc<EventBus>,
    daemon_tx: mpsc::Sender<DaemonNotification>,
    executor: Arc<dyn PhaseExecutor>,
    phase_def: PhaseDef,
    ctx: PhaseContext,
    cancel: CancellationToken,
    phase_run_id: PhaseRunId,
) -> DrainStatus {
    // --- Step 1: emit PhaseStarted (the drain, not run_phase — review C1) ---
    let started = BoiEvent::PhaseStarted {
        phase_run_id: phase_run_id.clone(),
        spec_id: ctx.spec_id.clone(),
        task_id: ctx.task_id.clone(),
        phase: ctx.phase.clone(),
        provider: phase_def.runtime.provider.clone(),
        model: phase_def.runtime.model.clone(),
        iteration: ctx.iteration,
    };
    if let Err(e) = bus.emit(&started).await {
        // The bus rejected `PhaseStarted` — its persist INSERT failed (e.g. a
        // duplicate phase iteration). Loud, and the drain ends with a
        // StreamError so the orchestrator surfaces it.
        tracing::error!(
            phase_run_id = %phase_run_id, error = %e,
            "drain could not emit PhaseStarted",
        );
        return DrainStatus::StreamError(format!("PhaseStarted rejected: {e}"));
    }
    if daemon_tx
        .send(DaemonNotification::Event(started))
        .await
        .is_err()
    {
        return DrainStatus::Canceled; // channel closed — nothing left to drive
    }

    // --- Step 2: relay the executor stream through the bus ---
    // Track whether a terminal `PhaseCompleted` for THIS phase run was relayed.
    // A stream that ends clean with no such event leaves no verdict for the
    // orchestrator to route — the task would otherwise strand `active` forever
    // (review B-svc-1). The drain reports that as `CompletedWithoutVerdict`.
    let mut relayed_verdict = false;
    let mut stream = executor.execute(phase_def, ctx, cancel.clone());

    // OBS-025 fix (2026-05-25). The event-driven heartbeat at the bottom of the
    // loop only fires when a stream event arrives. If the worker is mid-tool-call
    // (a long Read on a big file, an LLM that is composing a multi-write Edit
    // sequence with no token output), the stream goes quiet for >120s and the
    // sweeper marks the worker abandoned — even though it is actively doing real
    // work in the worktree. Reproduced on iii-hex Plan A v4-final Sszq2j8y8: T0's
    // execute phase wrote ~3 files to the worktree without emitting stream
    // events for ~2 minutes; sweeper killed the task; T1-T7 stranded. See
    // `hex/evolution/observations.md` OBS-025.
    //
    // Fix: emit a heartbeat on a 30s tokio interval *regardless* of stream
    // activity, alongside the event-driven path. Both paths use the same
    // `record_heartbeat` UPDATE which is `WHERE completed_at IS NULL`-scoped,
    // so a late tick after PhaseCompleted is a no-op (never clobbers a row the
    // bus already closed).
    //
    // The first tick of `tokio::time::interval` fires immediately; we burn it
    // here so the first real tick is +30s, not "right after PhaseStarted".
    let mut heartbeat_interval = tokio::time::interval(std::time::Duration::from_secs(30));
    heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heartbeat_interval.tick().await; // burn the immediate-fire first tick

    loop {
        tokio::select! {
            // Bias the cancel branch so a fired token wins a ready stream item.
            biased;
            () = cancel.cancelled() => {
                tracing::debug!(phase_run_id = %phase_run_id, "drain canceled mid-stream");
                return DrainStatus::Canceled;
            }
            _ = heartbeat_interval.tick() => {
                // OBS-025: time-driven heartbeat — fires every 30s regardless
                // of stream activity. Robust against long mid-tool-call silences.
                if let Err(e) = repo::phase_runs::record_heartbeat(
                    bus.pool(),
                    &phase_run_id,
                    chrono::Utc::now(),
                )
                .await
                {
                    tracing::warn!(
                        phase_run_id = %phase_run_id, error = %e,
                        "interval heartbeat write failed (risks a false sweep)",
                    );
                }
            }
            item = stream.next() => {
                let Some(event) = item else {
                    // The stream ended. If a terminal `PhaseCompleted` for this
                    // run was relayed, that is the normal path; otherwise the
                    // executor produced no verdict — surface it (B-svc-1).
                    return if relayed_verdict {
                        DrainStatus::Completed
                    } else {
                        DrainStatus::CompletedWithoutVerdict
                    };
                };
                // A terminal `PhaseCompleted` for THIS run is the verdict the
                // orchestrator routes — note it before the relay.
                if matches!(
                    &event,
                    BoiEvent::PhaseCompleted { phase_run_id: prid, .. } if *prid == phase_run_id
                ) {
                    relayed_verdict = true;
                }
                // Emit through the bus (persist → observe → bridge).
                if let Err(e) = bus.emit(&event).await {
                    // The bus DISPOSED — a worker proposed an illegal
                    // transition, or persist faulted. Never a silent `let _`.
                    tracing::error!(
                        phase_run_id = %phase_run_id, error = %e,
                        "drain stream event rejected by the bus",
                    );
                    return DrainStatus::StreamError(e.to_string());
                }
                if daemon_tx
                    .send(DaemonNotification::Event(event))
                    .await
                    .is_err()
                {
                    return DrainStatus::Canceled; // channel closed
                }
                // Liveness ping. The UPDATE is scoped `completed_at IS NULL`
                // (see `record_heartbeat`), so a late ping never clobbers a
                // row the bus already closed; a failed write only risks a
                // false sweep and is `warn!`-logged, never silent.
                if let Err(e) =
                    repo::phase_runs::record_heartbeat(bus.pool(), &phase_run_id, chrono::Utc::now())
                        .await
                {
                    tracing::warn!(
                        phase_run_id = %phase_run_id, error = %e,
                        "drain heartbeat write failed (risks a false sweep)",
                    );
                }
            }
        }
    }
}

/// Test doubles for the [`PhaseExecutor`] port.
///
/// Gated on `#[cfg(any(test, feature = "testkit"))]` (G16.1): a plain
/// `cargo test` compiles this so the in-crate `#[cfg(test)]`
/// orchestrator/routing tests can drive the orchestrator deterministically;
/// a separate `tests/integration/` crate enables the non-default `testkit`
/// feature to reach `boi::service::testkit::MockExecutor`. The crate's
/// *default* public surface never exposes a `Mock*` type.
#[cfg(any(test, feature = "testkit"))]
pub mod testkit {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use futures::stream::{self, BoxStream, StreamExt};
    use tokio_util::sync::CancellationToken;

    use super::PhaseExecutor;
    use crate::config::PhaseDef;
    use crate::types::context::PhaseContext;
    use crate::types::event::BoiEvent;
    use crate::types::verdict::{Evidence, VerdictOutcome, WorkerVerdict};

    /// One scripted step a [`MockExecutor`] replays for a phase.
    ///
    /// `Complete` is the common case — the executor builds the terminal
    /// `PhaseCompleted` from the *runtime* `PhaseContext` (so its
    /// `phase_run_id` matches the row the orchestrator minted; a pre-baked id
    /// would not). `Raw` injects an arbitrary `BoiEvent` verbatim — used to
    /// script an illegal transition or a mid-phase observational event.
    #[derive(Clone)]
    pub enum ScriptedEvent {
        /// Terminate the phase with this verdict outcome, bound to the run.
        Complete(VerdictOutcome),
        /// Yield this event verbatim.
        Raw(BoiEvent),
    }

    /// A [`PhaseExecutor`] double that replays a scripted `phase name →
    /// Vec<ScriptedEvent>` map as a stream.
    ///
    /// `execute` looks the phase name up in the script and yields the steps as
    /// a stream (terminating after the last). A phase with no script entry
    /// yields an empty stream — the drain then ends `Completed` with no
    /// `PhaseCompleted`, exercising the "drain terminated without a routed
    /// completion" path.
    #[derive(Clone, Default)]
    pub struct MockExecutor {
        script: Arc<HashMap<String, Vec<ScriptedEvent>>>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl MockExecutor {
        /// A mock with the given `phase name → scripted steps` map.
        pub fn new(script: HashMap<String, Vec<ScriptedEvent>>) -> Self {
            Self {
                script: Arc::new(script),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// A mock that ends EVERY phase `Passing` — the happy-path executor for
        /// the full-pipeline walk.
        pub fn all_passing() -> Self {
            Self::default()
        }

        /// The phase names `execute` was called with, in call order.
        ///
        /// A poisoned lock is recovered via `into_inner` rather than panicking
        /// — the `calls` log is append-only test bookkeeping, never load-
        /// bearing state.
        pub fn calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    impl PhaseExecutor for MockExecutor {
        fn execute(
            &self,
            phase: PhaseDef,
            ctx: PhaseContext,
            _cancel: CancellationToken,
        ) -> BoxStream<'static, BoiEvent> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(phase.name.clone());
            // No script entry → end the phase Passing (the `all_passing` /
            // unscripted-phase default — the happy-path executor).
            let steps = self.script.get(&phase.name).cloned().unwrap_or_else(|| {
                vec![ScriptedEvent::Complete(VerdictOutcome::Passing {
                    evidence: Evidence::default(),
                })]
            });
            let events: Vec<BoiEvent> = steps
                .into_iter()
                .map(|step| match step {
                    ScriptedEvent::Complete(outcome) => BoiEvent::PhaseCompleted {
                        phase_run_id: ctx.phase_run_id.clone(),
                        spec_id: ctx.spec_id.clone(),
                        task_id: ctx.task_id.clone(),
                        phase: ctx.phase.clone(),
                        verdict: WorkerVerdict {
                            synopsis: format!("scripted {}", ctx.phase),
                            outcome,
                        },
                        tokens_in: 0,
                        tokens_out: 0,
                        duration_ms: 0,
                    },
                    ScriptedEvent::Raw(ev) => ev,
                })
                .collect();
            stream::iter(events).boxed()
        }
    }

    /// A `Passing` outcome with empty evidence — the scripted happy path.
    pub fn passing() -> VerdictOutcome {
        VerdictOutcome::Passing {
            evidence: Evidence::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PhaseDef;
    use crate::repo::db::connect;
    use crate::repo::spec_versions::{VersionTrigger, append_version};
    use crate::repo::specs::insert_spec;
    use crate::repo::task_runtime::insert_task;
    use crate::service::bus::{EventBus, NoopObserver};
    use crate::types::context::{PhaseContext, SpecContract};
    use crate::types::ids::{SpecId, TaskId};
    use chrono::Utc;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use testkit::{MockExecutor, ScriptedEvent, passing};

    fn spec() -> SpecId {
        SpecId::new("S0000001a").unwrap()
    }
    fn task() -> TaskId {
        TaskId::new("T0000001a").unwrap()
    }

    /// A pool seeded with a spec (specs row + v1 snapshot + `spec_runtime`) and
    /// one `not_started` task — `phase_runs` rows can FK to it.
    async fn seeded_pool() -> sqlx::SqlitePool {
        let pool = connect("sqlite::memory:").await.unwrap();
        insert_spec(&pool, &spec(), Utc::now()).await.unwrap();
        append_version(
            &pool,
            &spec(),
            1,
            &serde_json::json!({ "title": "demo" }),
            VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        repo::spec_runtime::initialize(&pool, &spec(), 1)
            .await
            .unwrap();
        insert_task(&pool, &task(), &spec(), Some("setup"))
            .await
            .unwrap();
        pool
    }

    fn bus_for(pool: sqlx::SqlitePool) -> Arc<EventBus> {
        Arc::new(EventBus::new(pool, vec![Arc::new(NoopObserver)]))
    }

    /// A minimal `execute` worker `PhaseDef` for the named phase.
    fn execute_phase_def() -> PhaseDef {
        // `execute.toml` is the canonical worker phase; parse the fixture.
        let toml = std::fs::read_to_string(format!(
            "{}/tests/fixtures/phases/execute.toml",
            env!("CARGO_MANIFEST_DIR"),
        ))
        .unwrap();
        crate::config::parse_phase(&toml).unwrap()
    }

    fn phase_ctx(phase_run_id: &PhaseRunId) -> PhaseContext {
        PhaseContext {
            spec_id: spec(),
            task_id: Some(task()),
            phase: "execute".into(),
            phase_run_id: phase_run_id.clone(),
            iteration: 0,
            spec_contract: SpecContract {
                scope: "demo".into(),
                workspace: PathBuf::from("/repo"),
                base_branch: "main".into(),
                exclusions: vec![],
                verifications: vec![],
                must_emit: vec![],
            },
            task_contract: None,
            tasks: vec![],
            skills: vec![],
            decisions: vec![],
            prior_phase_runs: vec![],
        }
    }

    /// `MockExecutor` replays its scripted steps, building the terminal
    /// `PhaseCompleted` from the runtime `PhaseContext`.
    #[tokio::test]
    async fn test_l1_mock_executor_replays_scripted_events() {
        let pr = PhaseRunId::new("P0000001a").unwrap();
        let mut script = HashMap::new();
        script.insert(
            "execute".to_owned(),
            vec![ScriptedEvent::Complete(passing())],
        );
        let exec = MockExecutor::new(script);

        let mut stream = exec.execute(
            execute_phase_def(),
            phase_ctx(&pr),
            CancellationToken::new(),
        );
        let first = stream.next().await.expect("one scripted event");
        // The PhaseCompleted carries the runtime ctx's phase_run_id.
        let BoiEvent::PhaseCompleted { phase_run_id, .. } = &first else {
            unreachable!("scripted a Complete step, got {first:?}");
        };
        assert_eq!(phase_run_id, &pr);
        assert!(
            stream.next().await.is_none(),
            "stream ends after the script"
        );
        assert_eq!(exec.calls(), vec!["execute".to_owned()]);
    }

    /// A drain over a scripted stream emits `PhaseStarted` FIRST and sends
    /// `DrainTerminated{Completed}` LAST; the orchestrator-side channel sees
    /// the full sequence.
    #[tokio::test]
    async fn test_l2_drain_emits_phase_started_first_and_terminated_last() {
        let pool = seeded_pool().await;
        let bus = bus_for(pool.clone());
        let pr = PhaseRunId::new("P0000001a").unwrap();
        let mut script = HashMap::new();
        script.insert(
            "execute".to_owned(),
            vec![ScriptedEvent::Complete(passing())],
        );
        let exec: Arc<dyn PhaseExecutor> = Arc::new(MockExecutor::new(script));

        let (tx, mut rx) = mpsc::channel(16);
        drain_phase(
            bus,
            tx,
            exec,
            execute_phase_def(),
            phase_ctx(&pr),
            CancellationToken::new(),
            pr.clone(),
        )
        .await;

        // Sequence: PhaseStarted, PhaseCompleted, DrainTerminated{Completed}.
        let first = rx.recv().await.expect("PhaseStarted");
        assert!(matches!(
            first,
            DaemonNotification::Event(BoiEvent::PhaseStarted { .. })
        ));
        let second = rx.recv().await.expect("PhaseCompleted");
        assert!(matches!(
            second,
            DaemonNotification::Event(BoiEvent::PhaseCompleted { .. })
        ));
        let third = rx.recv().await.expect("DrainTerminated");
        assert!(matches!(
            third,
            DaemonNotification::DrainTerminated {
                status: DrainStatus::Completed,
                ..
            }
        ));
        // The `phase_runs` row was INSERTed by the bus's persist(PhaseStarted).
        assert!(repo::phase_runs::fetch(&pool, &pr).await.is_ok());
    }

    /// A drain whose executor stream emits an illegal transition terminates
    /// with `DrainTerminated{StreamError}` — the bus DISPOSED loudly.
    #[tokio::test]
    async fn test_l2_drain_illegal_transition_terminates_with_stream_error() {
        let pool = seeded_pool().await;
        let bus = bus_for(pool.clone());
        let pr = PhaseRunId::new("P0000001a").unwrap();
        // Script an illegal `TaskPassed` against a `not_started` task — the
        // transition guard rejects `not_started → passing`.
        let illegal = BoiEvent::TaskPassed {
            spec_id: spec(),
            task_id: task(),
            evidence: Default::default(),
        };
        let mut script = HashMap::new();
        script.insert("execute".to_owned(), vec![ScriptedEvent::Raw(illegal)]);
        let exec: Arc<dyn PhaseExecutor> = Arc::new(MockExecutor::new(script));

        let (tx, mut rx) = mpsc::channel(16);
        drain_phase(
            bus,
            tx,
            exec,
            execute_phase_def(),
            phase_ctx(&pr),
            CancellationToken::new(),
            pr.clone(),
        )
        .await;

        // PhaseStarted then DrainTerminated{StreamError} — the illegal
        // TaskPassed was DISPOSED by the bus, not relayed.
        assert!(matches!(
            rx.recv().await.unwrap(),
            DaemonNotification::Event(BoiEvent::PhaseStarted { .. })
        ));
        let term = rx.recv().await.expect("DrainTerminated");
        assert!(
            matches!(
                term,
                DaemonNotification::DrainTerminated {
                    status: DrainStatus::StreamError(_),
                    ..
                }
            ),
            "an illegal transition must terminate the drain with StreamError, got {term:?}",
        );
    }

    /// `cancel` fired before the drain starts → it stops with
    /// `DrainTerminated{Canceled}` and the scripted events are not relayed.
    #[tokio::test]
    async fn test_l2_drain_cancel_terminates_with_canceled() {
        let pool = seeded_pool().await;
        let bus = bus_for(pool.clone());
        let pr = PhaseRunId::new("P0000001a").unwrap();
        let mut script = HashMap::new();
        script.insert(
            "execute".to_owned(),
            vec![ScriptedEvent::Complete(passing())],
        );
        let exec: Arc<dyn PhaseExecutor> = Arc::new(MockExecutor::new(script));

        let cancel = CancellationToken::new();
        cancel.cancel(); // already cancelled before the drain runs

        let (tx, mut rx) = mpsc::channel(16);
        drain_phase(
            bus,
            tx,
            exec,
            execute_phase_def(),
            phase_ctx(&pr),
            cancel,
            pr.clone(),
        )
        .await;

        // PhaseStarted still emits (step 1); then the select! sees the fired
        // token and the drain ends Canceled — the PhaseCompleted is not relayed.
        assert!(matches!(
            rx.recv().await.unwrap(),
            DaemonNotification::Event(BoiEvent::PhaseStarted { .. })
        ));
        let term = rx.recv().await.expect("DrainTerminated");
        assert!(
            matches!(
                term,
                DaemonNotification::DrainTerminated {
                    status: DrainStatus::Canceled,
                    ..
                }
            ),
            "a fired cancel must terminate the drain Canceled, got {term:?}",
        );
        assert!(rx.recv().await.is_none(), "no PhaseCompleted was relayed");
    }

    /// `DrainStatus` round-trips through equality — used by the orchestrator's
    /// `handle_drain_terminated` match.
    #[test]
    fn test_l1_drain_status_equality() {
        assert_eq!(DrainStatus::Completed, DrainStatus::Completed);
        assert_ne!(DrainStatus::Completed, DrainStatus::Canceled);
        assert_eq!(
            DrainStatus::StreamError("x".into()),
            DrainStatus::StreamError("x".into()),
        );
    }

    /// B-svc-1 regression: a drain over a phase scripted with an explicitly
    /// EMPTY step list ends `CompletedWithoutVerdict` — the stream ran clean
    /// but relayed no terminal `PhaseCompleted`, so no verdict ever reached
    /// routing. Before the fix this returned plain `Completed` and the
    /// orchestrator treated it as cleanup-only, stranding the task `active`.
    #[tokio::test]
    async fn test_l2_drain_empty_script_completes_without_verdict() {
        let pool = seeded_pool().await;
        let bus = bus_for(pool.clone());
        let pr = PhaseRunId::new("P0000001a").unwrap();
        // An explicit empty Vec — distinct from an unscripted phase (which
        // defaults to a `Passing` completion).
        let mut script = HashMap::new();
        script.insert("execute".to_owned(), Vec::new());
        let exec: Arc<dyn PhaseExecutor> = Arc::new(MockExecutor::new(script));

        let (tx, mut rx) = mpsc::channel(16);
        drain_phase(
            bus,
            tx,
            exec,
            execute_phase_def(),
            phase_ctx(&pr),
            CancellationToken::new(),
            pr.clone(),
        )
        .await;

        assert!(matches!(
            rx.recv().await.unwrap(),
            DaemonNotification::Event(BoiEvent::PhaseStarted { .. })
        ));
        let term = rx.recv().await.expect("DrainTerminated");
        assert!(
            matches!(
                term,
                DaemonNotification::DrainTerminated {
                    status: DrainStatus::CompletedWithoutVerdict,
                    ..
                }
            ),
            "an empty stream must terminate CompletedWithoutVerdict, got {term:?}",
        );
        // No PhaseCompleted was relayed — the only post-PhaseStarted message
        // is DrainTerminated; the orchestrator surfaces the empty completion.
        assert!(rx.recv().await.is_none());
    }

    /// B-svc-1 corroboration: a drain whose stream relays a terminal
    /// `PhaseCompleted` for ITS `phase_run_id` ends plain `Completed` — a
    /// verdict reached routing, so `handle_drain_terminated` is cleanup-only.
    /// (Pairs with the empty-stream test above: same drain, the presence of a
    /// matching `PhaseCompleted` is the only difference.)
    #[tokio::test]
    async fn test_l2_drain_with_phase_completed_ends_completed() {
        let pool = seeded_pool().await;
        let bus = bus_for(pool.clone());
        let pr = PhaseRunId::new("P0000001a").unwrap();
        let mut script = HashMap::new();
        script.insert(
            "execute".to_owned(),
            vec![ScriptedEvent::Complete(passing())],
        );
        let exec: Arc<dyn PhaseExecutor> = Arc::new(MockExecutor::new(script));

        let (tx, mut rx) = mpsc::channel(16);
        drain_phase(
            bus,
            tx,
            exec,
            execute_phase_def(),
            phase_ctx(&pr),
            CancellationToken::new(),
            pr.clone(),
        )
        .await;

        // PhaseStarted, PhaseCompleted, then DrainTerminated{Completed}.
        assert!(matches!(
            rx.recv().await.unwrap(),
            DaemonNotification::Event(BoiEvent::PhaseStarted { .. })
        ));
        assert!(matches!(
            rx.recv().await.unwrap(),
            DaemonNotification::Event(BoiEvent::PhaseCompleted { .. })
        ));
        let term = rx.recv().await.expect("DrainTerminated");
        assert!(
            matches!(
                term,
                DaemonNotification::DrainTerminated {
                    status: DrainStatus::Completed,
                    ..
                }
            ),
            "a relayed PhaseCompleted must terminate Completed, got {term:?}",
        );
    }
}
