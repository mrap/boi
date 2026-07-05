//! The orchestrator — the event loop that drives the `standard` pipeline
//! end-to-end (design §4 / §5 / §6).
//!
//! Per review S7 the orchestrator is split so no one file is large:
//!
//! - `orchestrator.rs` (this file) — the [`Orchestrator`] struct, its
//!   [`OrchestratorError`], and [`Orchestrator::new`].
//! - `run_loop` — the run-loop (Task 5a.3a), `handle_event` dispatch,
//!   `handle_drain_terminated`, `on_fault`, `emit_local`.
//! - `run_phase` — `run_phase` (the phase clock-in) + its supporting helpers.
//! - `handlers` — the per-event `on_*` handlers.
//!
//! Verdict→next-phase logic is a further layer out, in
//! [`crate::service::routing`]; the [`PhaseExecutor`] port + drain machinery
//! in [`crate::service::registry`].
//!
//! ## The C1 emit / notify invariant (load-bearing concurrency decision)
//!
//! [`EventBus::emit`](crate::service::bus::EventBus::emit) runs emit-Phases 1–3
//! and owns no channel. The orchestrator owns both `daemon_tx` (to clone for
//! drain tasks + the sweeper) and `daemon_rx` (to consume) — and **never sends
//! on `daemon_tx` itself**. When a `handle_*` method emits, it calls
//! `emit_local`: `bus.emit(&ev)` then push the event onto an in-loop
//! `VecDeque` — never the channel. The run-loop drains that `VecDeque` to
//! fixpoint between channel `recv()`s. Drain tasks and the sweeper are the
//! *only* producers into the bounded `mpsc::channel(1024)`. Violating this
//! self-deadlocks the orchestrator — `handle` is the channel's sole consumer,
//! and if `handle` also produced into a full channel it would block on itself.
//!
//! ## The contract-snapshot convention (a Phase 5a decision — flagged for Phase 9)
//!
//! Design §3.0: the spec TOML is unreferenced after dispatch — the orchestrator
//! holds `pipeline` + `phases`, never the parsed `config::Spec`. `run_phase`
//! re-hydrates [`SpecContract`](crate::types::context::SpecContract) /
//! [`TaskContract`](crate::types::context::TaskContract) from the
//! `spec_versions` snapshot (review S4). The plan does not pin the snapshot's
//! JSON shape, so Phase 5a fixes a convention the dispatch path (Phase 9) must
//! honour: the snapshot is a JSON object carrying `spec_contract` (a serialized
//! `SpecContract`) and `task_contracts` (an object mapping each `task_id`
//! string to a serialized `TaskContract`). A missing / malformed key is a loud
//! [`OrchestratorError::Contract`] — never a silent default.

mod handlers;
mod run_loop;
mod run_phase;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::sync::mpsc;

use crate::config::{PhaseDef, PipelineDef};
use crate::repo::db::RepoError;
use crate::service::bus::{BusError, EventBus};
use crate::service::registry::{DaemonNotification, InFlight, PhaseExecutor};
use crate::service::routing::{self, RoutingError};
use crate::types::event::BoiEvent;
use crate::types::ids::{PhaseRunId, SpecId};

/// Where the harness writes a `plan_revision` worker's `PlanRevision` artifact
/// (review D3 — the typed artifact channel). The file stem is the phase-run id.
const REVISIONS_DIR: &str = "~/.boi/v2/revisions";

/// An orchestration step failed.
///
/// Every variant carries the `spec_id` of the affected spec, so `on_fault` can
/// fail *that* spec (loud, operator-visible) while siblings continue.
#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    /// A phase name does not resolve to any [`PhaseDef`].
    #[error("spec {spec_id}: unknown phase `{phase}`")]
    UnknownPhase {
        /// The affected spec.
        spec_id: SpecId,
        /// The unresolved phase name.
        phase: String,
    },
    /// The `spec_versions` snapshot could not be re-hydrated into contracts —
    /// a missing/malformed `spec_contract` or `task_contracts` entry.
    #[error("spec {spec_id}: contract re-hydration failed: {detail}")]
    Contract {
        /// The affected spec.
        spec_id: SpecId,
        /// What was wrong with the snapshot.
        detail: String,
    },
    /// A repo-layer query failed.
    #[error("spec {spec_id}: repo query failed: {source}")]
    Repo {
        /// The affected spec.
        spec_id: SpecId,
        /// The underlying repo error.
        source: RepoError,
    },
    /// An [`EventBus::emit`](crate::service::bus::EventBus::emit) failed — a
    /// rejected transition or a persist fault.
    #[error("spec {spec_id}: event emit failed: {source}")]
    Bus {
        /// The affected spec.
        spec_id: SpecId,
        /// The underlying bus error.
        source: BusError,
    },
    /// A routing decision failed.
    #[error("spec {spec_id}: routing failed: {source}")]
    Routing {
        /// The affected spec.
        spec_id: SpecId,
        /// The underlying routing error.
        source: RoutingError,
    },
}

impl OrchestratorError {
    /// The spec a fault should be charged to.
    fn spec_id(&self) -> &SpecId {
        match self {
            OrchestratorError::UnknownPhase { spec_id, .. }
            | OrchestratorError::Contract { spec_id, .. }
            | OrchestratorError::Repo { spec_id, .. }
            | OrchestratorError::Bus { spec_id, .. }
            | OrchestratorError::Routing { spec_id, .. } => spec_id,
        }
    }
}

/// [`Orchestrator::new`] failed at construction — *before* any spec is running.
///
/// A separate type from [`OrchestratorError`] (review B-orch-S5): every
/// `OrchestratorError` carries the `spec_id` of a *real affected spec* so
/// `on_fault` can fail that spec. A pipeline-config fault at `new` time has no
/// spec to charge — the earlier code fabricated a structurally-valid sentinel
/// `SpecId("S00000000")` to satisfy `OrchestratorError::Routing`, an id that
/// names no real spec. This type holds no `spec_id`, so the
/// "every `OrchestratorError` names a real spec" invariant is no longer a lie.
#[derive(Debug, thiserror::Error)]
pub enum OrchestratorInitError {
    /// The `standard` pipeline's routing graph is malformed — a config typo is
    /// a startup rejection, never a mid-run wedge (review S5).
    #[error("pipeline validation failed at orchestrator startup: {0}")]
    PipelineValidation(#[from] RoutingError),
}

/// The orchestrator — one event loop driving every spec.
///
/// Construct via [`Orchestrator::new`]; drive via
/// [`Orchestrator::run`](Orchestrator::run).
pub struct Orchestrator {
    bus: Arc<EventBus>,
    pool: SqlitePool,
    executor: Arc<dyn PhaseExecutor>,
    /// The `standard` pipeline — `spec_phases: Vec<PipelinePhase>`.
    pipeline: PipelineDef,
    /// Phase name → definition.
    phases: HashMap<String, PhaseDef>,
    /// In-flight phase runs, keyed by [`PhaseRunId`].
    in_flight: HashMap<PhaseRunId, InFlight>,
    /// Per-spec: the task whose blocking `task_report` triggered an in-flight
    /// `plan_revision` phase. Recorded by `on_report_received` and consumed by
    /// `on_plan_revision_completed` (review B-orch-S3) — carrying the real
    /// reporting task forward, rather than the orchestrator re-deriving it by
    /// a "first `PlanRevisionPending` task by id" scan that names the wrong
    /// task when two reports block concurrently.
    plan_revision_trigger: HashMap<SpecId, crate::types::ids::TaskId>,
    /// `handle`-emitted events — drained to fixpoint, NEVER the channel (C1).
    local: VecDeque<BoiEvent>,
    /// Cloned for drain tasks / the sweeper; the orchestrator never sends.
    daemon_tx: mpsc::Sender<DaemonNotification>,
    /// The sole consumer of the bounded channel.
    daemon_rx: mpsc::Receiver<DaemonNotification>,
}

impl Orchestrator {
    /// Construct an orchestrator.
    ///
    /// `daemon_tx` + `daemon_rx` are taken as parameters — `boot` (Phase 9)
    /// creates the `mpsc::channel(1024)` and hands both halves (G16.3); the
    /// orchestrator owns them thereafter and never sends on `daemon_tx`.
    ///
    /// Runs [`routing::validate_pipeline`] once — a malformed routing graph is
    /// a startup rejection, never a mid-run wedge (review S5).
    ///
    /// Returns [`OrchestratorInitError`], NOT [`OrchestratorError`] (review
    /// B-orch-S5): a construction-time config fault has no running spec to
    /// charge, so it cannot honestly carry a `spec_id` — a separate error type
    /// keeps the "every `OrchestratorError` names a real spec" invariant true.
    pub fn new(
        bus: Arc<EventBus>,
        pool: SqlitePool,
        executor: Arc<dyn PhaseExecutor>,
        pipeline: PipelineDef,
        phases: HashMap<String, PhaseDef>,
        daemon_tx: mpsc::Sender<DaemonNotification>,
        daemon_rx: mpsc::Receiver<DaemonNotification>,
    ) -> Result<Self, OrchestratorInitError> {
        // A malformed routing graph is a startup rejection — no fabricated
        // sentinel spec id (the `?` lifts `RoutingError` into the spec-free
        // `OrchestratorInitError`).
        routing::validate_pipeline(&pipeline, &phases)?;
        Ok(Self {
            bus,
            pool,
            executor,
            pipeline,
            phases,
            in_flight: HashMap::new(),
            plan_revision_trigger: HashMap::new(),
            local: VecDeque::new(),
            daemon_tx,
            daemon_rx,
        })
    }
}
