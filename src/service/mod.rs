//! Business logic: event bus + four-phase emit, state machine enforcement,
//! phase orchestrator, plan layer, adjustment side-chain, curator
//! (Q8: all decisions for spec), and `<phase_context>` renderer
//! (§7.5 XML + Markdown-KV, stable/volatile split).
//!
//! Phase 4 landed the first three modules:
//!
//! - [`transitions`] — the runtime state-machine legality guard.
//! - [`bus`] — the [`EventBus`] chokepoint with the four-phase emit, and the
//!   `EmitObserver` port.
//! - [`mcp`] — BOI's MCP tool surface: the 4 worker tools + their handlers.
//!
//! Phase 5c added the `PhaseContext` business core:
//!
//! - [`context`] — `compose` assembles a `PhaseContext` from the Phase 3
//!   composition query + the authored contracts (the §7.0 decision push).
//! - [`renderer`] — `render` turns a `PhaseContext` into the canonical §7.5
//!   `<phase_context>` block (XML record-sets + Markdown-KV, stable/volatile
//!   split).
//!
//! Phase 5b added the two adjustment loops of design §4:
//!
//! - [`adjustment`] — the task-level side-chain (`Fail` → `propose_adjustment`
//!   → `review_adjustment` → re-`execute`), bounded by `CAP_TASK_ADJUST`.
//! - [`plan_layer`] — the plan-level revision (`task_report` → revise the spec
//!   graph → append `spec_versions` → `PlanRevised`).
//!
//! Phase 5a added the orchestrator, routing, scheduler, and sweeper:
//!
//! - [`registry`] — the [`PhaseExecutor`] port + the per-phase-run drain task.
//! - [`orchestrator`] — the event loop driving the `standard` pipeline.
//! - [`routing`] — verdict→next-phase decisions + the iteration caps.
//! - [`scheduler`] — dep-respecting task readiness.
//! - [`sweeper`] — the heartbeat sweeper for abandoned phase runs.
//!
//! ## Re-export notes
//!
//! - `DaemonNotification` joins the surface here (Phase 5a) — the plan's Task
//!   4.6 list named `bus::DaemonNotification`, but Task 4.3 (authoritative)
//!   places it in `registry.rs`, not `bus` (the bus owns no channel — the C1
//!   producer-split).
//! - `render` is re-exported as [`render_phase_context`], NOT as the bare
//!   verb `render` — `render` would collide once `use boi::service::*` lands
//!   in `cli/` (review S12). `compose` keeps its name (no collision).
//! - `CAP_TASK_ADJUST` is owned by [`adjustment`] (G20.1) and re-exported
//!   *through* [`routing`]; the `routing::*` line below carries it onto this
//!   flat surface alongside the other three caps. The `adjustment::*` line
//!   does NOT also list it — one re-export path, no ambiguity.

pub mod adjustment;
pub mod bus;
pub mod command;
pub mod context;
pub mod mcp;
pub mod orchestrator;
pub mod plan_layer;
pub mod registry;
pub mod renderer;
pub mod routing;
pub mod scheduler;
pub mod sweeper;
pub mod transitions;

pub use adjustment::{
    AdjustmentError, AdjustmentRoute, SIDE_CHAIN, is_side_chain_phase, route_after_fail,
    route_after_review_adjustment,
};
pub use bus::{BusError, EmitObserver, EventBus, NoopObserver, ObserverError};
pub use command::{DaemonCommand, DaemonResponse};
pub use context::{ContextError, compose};
pub use mcp::{
    DecisionRecordArgs, McpError, McpHandlers, TaskReportArgs, ToolHostError, VerificationOutput,
    WorkerSession, WorkerToolHost, tool_catalog,
};
pub use orchestrator::{Orchestrator, OrchestratorError, OrchestratorInitError};
// `PlanRevision` / `PlanEdit` are NOT re-exported here — they live in
// `crate::types::plan` (erratum G13.4); only the plan-layer entry points are.
pub use plan_layer::{PlanLayerError, ReportOutcome, apply_revision, on_report};
// `registry` — the `PhaseExecutor` port + the channel-message type. The
// `testkit` test double (`MockExecutor`) is NOT on this surface — it is gated
// behind the non-default `testkit` feature (G16.1).
pub use registry::{DaemonNotification, DrainStatus, PhaseExecutor};
// `render` is re-exported under a non-colliding name — see the module doc.
pub use renderer::render as render_phase_context;
// `routing` — the iteration caps (incl. `CAP_TASK_ADJUST`, re-exported through
// `routing` from `adjustment` per G20.1), `validate_pipeline`, and the error.
pub use routing::{
    CAP_EXECUTE_REVIEW, CAP_PLAN_CRITIQUE, CAP_SPEC_REVIEW, CAP_TASK_ADJUST, RoutingError,
    validate_pipeline,
};
pub use scheduler::{SchedulerError, all_tasks_settled, ready_tasks};
pub use sweeper::{ReclaimError, ReclaimOutcome, SpecReclaimer, Sweeper, SweeperError};
pub use transitions::{TransitionError, check_spec, check_task};

// The `testkit` test doubles, re-exported at the `service` level so an
// external crate reaches them as `boi::service::testkit::MockExecutor`
// (G16.1's documented path). Gated on the non-default `testkit` feature (and
// `test`, for the in-crate `#[cfg(test)]` modules) — `Mock*` is NEVER on the
// default public surface.
#[cfg(any(test, feature = "testkit"))]
pub use registry::testkit;
