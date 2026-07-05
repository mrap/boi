//! Deterministic-step types — the I/O surface of the `DETERMINISTIC_STEPS`
//! fn-pointer table.
//!
//! Six of the eight standard phases (`kind = "deterministic"`: workspace
//! verify, validate, commit, merge, teardown) are native Rust functions, not
//! LLM phases. [`StepCtx`] is their input; [`StepOutcome`] is their output;
//! [`StepError`] is the error a step body returns. Phase 6 populates the
//! table — this module defines its surface.
//!
//! ## `StepError` is self-contained (G14.1)
//!
//! `StepError` lives in `types/` (layer 0) and MUST be self-contained —
//! string-carrying variants, NO `#[from]` on any `runtime/` type
//! (`GitError`/`ValidateError`/`WorktreeError` are layer 4; a `#[from]` would
//! be a backward import `module-dep-audit.sh` rejects). Phase 6's modules
//! convert `GitError → StepError::Git(e.to_string())` at their own boundary.

use std::any::Any;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::types::context::{SpecContract, TaskContract};
use crate::types::ids::{PhaseRunId, SpecId, TaskId};
use crate::types::reasons::ErrorWhyFix;
use crate::types::verdict::Evidence;

/// Input handed to a deterministic step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepCtx {
    /// The spec being run.
    pub spec_id: SpecId,
    /// The task being run — `None` for spec-level steps.
    pub task_id: Option<TaskId>,
    /// This phase run's ID.
    pub phase_run_id: PhaseRunId,
    /// The phase name.
    pub phase: String,
    /// Path to the worktree the step operates in.
    pub worktree_path: PathBuf,
    /// The git ref the worktree is on.
    pub branch_ref: String,
    /// The spec-level authored contract.
    pub spec_contract: SpecContract,
    /// The task-level authored contract — `None` for spec-level steps.
    pub task_contract: Option<TaskContract>,
}

/// Output of a deterministic step.
///
/// A deterministic step either passes (with evidence) or fails (with an
/// error/why/fix triple). The deterministic adapter lifts this into a
/// `WorkerVerdict` at the `PhaseExecutor` boundary (Batch C).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StepOutcome {
    /// The step succeeded.
    Pass {
        /// Evidence the step did its work.
        evidence: Evidence,
    },
    /// The step failed.
    Fail {
        /// What went wrong, why, and how to fix it.
        error_why_fix: ErrorWhyFix,
    },
}

/// An error raised inside a deterministic step body.
///
/// Self-contained — every variant carries a `String`, never a `runtime/` error
/// type (see the module doc, G14.1).
#[derive(Debug, thiserror::Error)]
pub enum StepError {
    /// The phase name does not resolve to any entry in `DETERMINISTIC_STEPS`.
    #[error("unknown deterministic phase: {0}")]
    UnknownDeterministicPhase(String),
    /// A git operation failed.
    #[error("git: {0}")]
    Git(String),
    /// A validation command failed to run.
    #[error("validate: {0}")]
    Validate(String),
    /// A worktree operation failed.
    #[error("worktree: {0}")]
    Worktree(String),
}

/// Error raised by [`StepCtxBuilder::build`] when a required field was not
/// set — the W3 forcing function from CRITIC §Weakness 3.
#[derive(Debug, thiserror::Error)]
pub enum StepCtxBuilderError {
    /// `with_merge_strategies` was never called on the builder.
    ///
    /// The W3 sharpening removes the `Default` impl and the implicit
    /// empty-registry construction so test doubles cannot leak past the
    /// real strategy pipeline — every caller MUST set the registry
    /// explicitly.
    #[error("StepCtxBuilder: with_merge_strategies(...) was not called")]
    MergeStrategiesNotSet,
    /// A required `StepCtx` field was missing.
    #[error("StepCtxBuilder: required field `{0}` not set")]
    MissingField(&'static str),
}

/// Explicit builder for [`StepCtx`] — the W3 forcing function.
///
/// Per CRITIC §Weakness 3 (sharpening), this builder DELIBERATELY:
///
/// - does NOT implement [`Default`] — every construction names the spec,
///   task, phase, and worktree it operates on,
/// - REQUIRES a [`Self::with_merge_strategies`] call before
///   [`Self::build`] returns `Ok`,
///
/// so the L3 test for the registry plumbing cannot accidentally run with
/// a stub-only registry, and so production code paths cannot drop the
/// real registry by omission.
#[must_use = "a StepCtxBuilder does nothing until `.build()` is called"]
pub struct StepCtxBuilder {
    spec_id: Option<SpecId>,
    task_id: Option<TaskId>,
    phase_run_id: Option<PhaseRunId>,
    phase: Option<String>,
    worktree_path: Option<PathBuf>,
    branch_ref: Option<String>,
    spec_contract: Option<SpecContract>,
    task_contract: Option<TaskContract>,
    merge_strategies: Option<Arc<dyn Any + Send + Sync>>,
}

impl StepCtxBuilder {
    /// Start a fresh builder. No `Default` impl by design (W3) — the
    /// forcing function depends on every construction path naming its
    /// fields, including `with_merge_strategies`.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            spec_id: None,
            task_id: None,
            phase_run_id: None,
            phase: None,
            worktree_path: None,
            branch_ref: None,
            spec_contract: None,
            task_contract: None,
            merge_strategies: None,
        }
    }

    /// Set the spec being run.
    pub fn with_spec_id(mut self, spec_id: SpecId) -> Self {
        self.spec_id = Some(spec_id);
        self
    }

    /// Set the optional task being run.
    pub fn with_task_id(mut self, task_id: Option<TaskId>) -> Self {
        self.task_id = task_id;
        self
    }

    /// Set this phase run's id.
    pub fn with_phase_run_id(mut self, phase_run_id: PhaseRunId) -> Self {
        self.phase_run_id = Some(phase_run_id);
        self
    }

    /// Set the phase name.
    pub fn with_phase(mut self, phase: impl Into<String>) -> Self {
        self.phase = Some(phase.into());
        self
    }

    /// Set the worktree path the step operates in.
    pub fn with_worktree_path(mut self, path: PathBuf) -> Self {
        self.worktree_path = Some(path);
        self
    }

    /// Set the git ref the worktree is on.
    pub fn with_branch_ref(mut self, branch_ref: impl Into<String>) -> Self {
        self.branch_ref = Some(branch_ref.into());
        self
    }

    /// Set the spec-level authored contract.
    pub fn with_spec_contract(mut self, contract: SpecContract) -> Self {
        self.spec_contract = Some(contract);
        self
    }

    /// Set the task-level authored contract.
    pub fn with_task_contract(mut self, contract: Option<TaskContract>) -> Self {
        self.task_contract = contract;
        self
    }

    /// Install the `MergeStrategy` registry — REQUIRED (W3).
    ///
    /// Generic over the trait object so this layer-0 builder does not
    /// import the layer-4 [`crate::runtime::merge_strategies::MergeStrategy`]
    /// trait (G14 / `module-dep-audit.sh`). Sibling tasks (t-7) refine the
    /// stored type when wiring the registry into `StepCtx` proper; this
    /// task only pins the API shape and the build-time refusal.
    pub fn with_merge_strategies<S: ?Sized + Send + Sync + 'static>(
        mut self,
        strategies: Arc<Vec<Arc<S>>>,
    ) -> Self {
        self.merge_strategies = Some(Arc::new(strategies) as Arc<dyn Any + Send + Sync>);
        self
    }

    /// Materialize a [`StepCtx`] or report what was missing.
    ///
    /// W3 forcing function: `Err(MergeStrategiesNotSet)` if
    /// [`Self::with_merge_strategies`] was never called.
    pub fn build(self) -> Result<StepCtx, StepCtxBuilderError> {
        if self.merge_strategies.is_none() {
            return Err(StepCtxBuilderError::MergeStrategiesNotSet);
        }
        Ok(StepCtx {
            spec_id: self
                .spec_id
                .ok_or(StepCtxBuilderError::MissingField("spec_id"))?,
            task_id: self.task_id,
            phase_run_id: self
                .phase_run_id
                .ok_or(StepCtxBuilderError::MissingField("phase_run_id"))?,
            phase: self
                .phase
                .ok_or(StepCtxBuilderError::MissingField("phase"))?,
            worktree_path: self
                .worktree_path
                .ok_or(StepCtxBuilderError::MissingField("worktree_path"))?,
            branch_ref: self
                .branch_ref
                .ok_or(StepCtxBuilderError::MissingField("branch_ref"))?,
            spec_contract: self
                .spec_contract
                .ok_or(StepCtxBuilderError::MissingField("spec_contract"))?,
            task_contract: self.task_contract,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::context::SpecContract;

    fn spec_contract() -> SpecContract {
        SpecContract {
            scope: "scope".into(),
            workspace: PathBuf::from("/repo"),
            base_branch: "main".into(),
            exclusions: vec![],
            verifications: vec![],
            must_emit: vec![],
        }
    }

    fn step_ctx() -> StepCtx {
        StepCtx {
            spec_id: SpecId::new("S0000001a").unwrap(),
            task_id: Some(TaskId::new("T0000001a").unwrap()),
            phase_run_id: PhaseRunId::new("P0000001a").unwrap(),
            phase: "commit".into(),
            worktree_path: PathBuf::from("/repo/.worktrees/T0000001a"),
            branch_ref: "spec/S0000001a/T0000001a".into(),
            spec_contract: spec_contract(),
            task_contract: None,
        }
    }

    #[test]
    fn step_ctx_constructs_and_roundtrips() {
        let ctx = step_ctx();
        let json = serde_json::to_string(&ctx).unwrap();
        let back: StepCtx = serde_json::from_str(&json).unwrap();
        assert_eq!(back.spec_id, ctx.spec_id);
        assert_eq!(back.phase, "commit");
        assert_eq!(back.branch_ref, ctx.branch_ref);
    }

    #[test]
    fn step_outcome_pass_roundtrips() {
        let outcome = StepOutcome::Pass {
            evidence: Evidence {
                files_touched: vec![PathBuf::from("src/a.rs")],
                verifications: vec![],
                summary: "committed".into(),
                merge_commit_sha: None,
            },
        };
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(json.contains("\"type\":\"pass\""));
        let back: StepOutcome = serde_json::from_str(&json).unwrap();
        let StepOutcome::Pass { evidence } = back else {
            unreachable!("serialized a Pass, expected a Pass back");
        };
        assert_eq!(evidence.files_touched.len(), 1);
    }

    #[test]
    fn step_outcome_fail_roundtrips() {
        let outcome = StepOutcome::Fail {
            error_why_fix: ErrorWhyFix {
                error: "merge conflict".into(),
                why: "diverged".into(),
                fix: "rebase".into(),
            },
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let back: StepOutcome = serde_json::from_str(&json).unwrap();
        let StepOutcome::Fail { error_why_fix } = back else {
            unreachable!("serialized a Fail, expected a Fail back");
        };
        assert_eq!(error_why_fix.error, "merge conflict");
    }

    #[test]
    fn test_l1_step_ctx_builder_refuses_without_merge_strategies() {
        let err = StepCtxBuilder::new()
            .with_spec_id(SpecId::new("S0000001a").unwrap())
            .with_phase_run_id(PhaseRunId::new("P0000001a").unwrap())
            .with_phase("commit")
            .with_worktree_path(PathBuf::from("/w"))
            .with_branch_ref("b")
            .with_spec_contract(spec_contract())
            .build()
            .expect_err("builder must refuse without with_merge_strategies");
        assert!(matches!(err, StepCtxBuilderError::MergeStrategiesNotSet));
    }

    #[test]
    fn test_l1_step_ctx_builder_succeeds_with_merge_strategies() {
        let reg: Arc<Vec<Arc<()>>> = Arc::new(vec![]);
        let ctx = StepCtxBuilder::new()
            .with_spec_id(SpecId::new("S0000001a").unwrap())
            .with_phase_run_id(PhaseRunId::new("P0000001a").unwrap())
            .with_phase("commit")
            .with_worktree_path(PathBuf::from("/w"))
            .with_branch_ref("b")
            .with_spec_contract(spec_contract())
            .with_merge_strategies(reg)
            .build()
            .expect("builder must accept once strategies are set");
        assert_eq!(ctx.phase, "commit");
    }

    #[test]
    fn step_error_display_is_prefixed() {
        assert_eq!(
            StepError::UnknownDeterministicPhase("nope".into()).to_string(),
            "unknown deterministic phase: nope",
        );
        assert_eq!(
            StepError::Git("detached HEAD".into()).to_string(),
            "git: detached HEAD"
        );
        assert_eq!(
            StepError::Validate("exit 1".into()).to_string(),
            "validate: exit 1"
        );
        assert_eq!(
            StepError::Worktree("locked".into()).to_string(),
            "worktree: locked",
        );
    }
}
