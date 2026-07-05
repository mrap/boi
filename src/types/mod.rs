//! BOI v2 types layer.
//!
//! Pure data types — IDs, enums, `BoiEvent`, structs. No I/O. No async runtime.
//! No internal deps; depended on by every other layer (`config` → `repo` →
//! `service` → `runtime` → `cli`).

pub mod context;
pub mod decision;
pub mod event;
pub mod ids;
pub mod plan;
pub mod reasons;
pub mod state;
pub mod step;
pub mod verdict;

// Convenience re-exports
pub use context::{PhaseContext, PhaseRunSummary, SpecContract, TaskContract};
pub use decision::{DecisionOrigin, DecisionRecord, RejectedAlternative};
pub use ids::{DecisionId, IdError, PhaseRunId, SpecId, TaskId};
pub use reasons::{BlockedReason, CancellationReason, ErrorWhyFix, FailureReason};
pub use state::{SpecStatus, TaskState};
pub use verdict::{Evidence, VerdictOutcome, WorkerVerdict};
// NOTE: `context::Verification` is deliberately NOT flat-re-exported — the bare
// name collides with `event::VerifyChecked` and `config::RawVerification` at
// call sites. Reference it qualified: `boi::types::context::Verification`.
// (Batch A review — L1)
pub use step::{StepCtx, StepError, StepOutcome};
// NOTE: `StepError` is re-exported per the G14.1 erratum (Phase 6's
// deterministic-step bodies return it); Task 1.9's original mod.rs block
// listed only `StepCtx, StepOutcome` because it predates the G14.1 fold.
pub use event::BoiEvent;
pub use plan::{PlanEdit, PlanRevision};
