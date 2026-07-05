//! The `DETERMINISTIC_STEPS` fn-pointer table — phase name → native-Rust step
//! body, for the 8 deterministic phases (no LLM) (Q1/Q2).
//!
//! ## Two-task module
//!
//! - **Task 6.2** defined the executor↔step *contract* — [`StepRun`] and the
//!   [`DetStep`] fn-pointer type — here, because Task 6.2's `worktree.rs` step
//!   signatures (`fn(Arc<StepCtx>) -> BoxFuture<…, Result<StepRun, _>>`) must
//!   already name them. (The plan's Task 6.4 nominally "defines `StepRun`"; but
//!   `worktree.rs` in Task 6.2 *returns* it, and Rust needs the type to exist
//!   before `worktree.rs` compiles — so the type contract lands with 6.2. See
//!   the Task 6.2 commit-message deviation note.)
//! - **Task 6.4** adds the [`resolve`] table — phase name → step body — once
//!   `validate.rs` (Task 6.3) and all of `worktree.rs` exist.
//!
//! ## The deterministic-step shape (review disagreement (a))
//!
//! A [`DetStep`] is a SYNCHRONOUS `fn` returning a boxed future. An `async fn`
//! does not coerce to a `fn` pointer, so each step body in `worktree.rs` /
//! `validate.rs` is written as a `fn` *item* and the item populates the table
//! directly.

use std::sync::Arc;

use futures::future::BoxFuture;

use crate::types::event::BoiEvent;
use crate::types::step::{StepCtx, StepError, StepOutcome};

/// The executor↔step contract: a step's terminal outcome plus the intermediate
/// [`BoiEvent`]s it produced.
///
/// `validate` (Task 6.3) produces one `VerifyChecked` per verification command;
/// the worktree steps produce none. This type *widens* the step return WITHOUT
/// touching the locked [`StepOutcome`] (review (d)) — a `runtime/`-internal
/// type. The Task 6.5 executor splices `events` into the stream *before* the
/// terminal `PhaseCompleted`, so they travel the one stream the drain task
/// drains (never a direct `bus.emit`, which would re-open Batch B C1's
/// second-producer hole).
#[derive(Debug, Clone)]
pub struct StepRun {
    /// The step's terminal outcome.
    pub outcome: StepOutcome,
    /// Intermediate events the step produced (e.g. `validate`'s per-command
    /// `VerifyChecked`s). Empty for the worktree steps.
    pub events: Vec<BoiEvent>,
}

/// A deterministic-phase body — a SYNCHRONOUS `fn` returning a boxed future.
///
/// An `async fn` does NOT coerce to this `fn`-pointer type (review (a)); each
/// step body is a plain `fn` item, and the item is what [`resolve`] returns.
pub type DetStep = fn(Arc<StepCtx>) -> BoxFuture<'static, Result<StepRun, StepError>>;

/// The 8 deterministic phase names, in `DETERMINISTIC_STEPS` table order.
///
/// `merge_to_integration` is the implicit terminal task phase (Batch B's C4
/// fix) — it is NOT in `standard.toml`'s `task_phases`; `route_task` (5a.4)
/// names it directly.
pub const DETERMINISTIC_PHASES: &[&str] = &[
    "workspace_prepare",
    "workspace_verify_in",
    "workspace_verify_out",
    "commit",
    "merge_to_integration",
    "merge",
    "teardown",
    "validate",
];

/// Resolve a phase name to its [`DetStep`] body — the 8 deterministic phases
/// (Q1/Q2).
///
/// The 8 entries:
///
/// | phase | body |
/// |---|---|
/// | `workspace_prepare` | `worktree::prepare_spec` |
/// | `workspace_verify_in` | `worktree::verify_in` |
/// | `workspace_verify_out` | `worktree::verify_out` |
/// | `commit` | `worktree::commit` |
/// | `merge_to_integration` | `worktree::merge_to_integration` |
/// | `merge` | `worktree::merge_spec` |
/// | `teardown` | `worktree::teardown` |
/// | `validate` | `validate::validate` |
///
/// Non-`validate` steps return `StepRun { outcome, events: vec![] }`. A lookup
/// miss returns `None`; the caller (Task 6.5 `DeterministicExecutor`) surfaces
/// it loud as `StepError::UnknownDeterministicPhase` — never a panic.
pub fn resolve(phase: &str) -> Option<DetStep> {
    use crate::runtime::{validate, worktree};
    match phase {
        "workspace_prepare" => Some(worktree::prepare_spec as DetStep),
        "workspace_verify_in" => Some(worktree::verify_in as DetStep),
        "workspace_verify_out" => Some(worktree::verify_out as DetStep),
        "commit" => Some(worktree::commit as DetStep),
        "merge_to_integration" => Some(worktree::merge_to_integration as DetStep),
        "merge" => Some(worktree::merge_spec as DetStep),
        "teardown" => Some(worktree::teardown as DetStep),
        "validate" => Some(validate::validate as DetStep),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `DetStep`-typed parameter — calling it with each step body proves the
    /// `fn`-pointer coercion holds (review (a): an `async fn` would not coerce).
    fn _assert_detstep(_: DetStep) {}

    #[test]
    fn test_l1_every_deterministic_phase_resolves() {
        for phase in DETERMINISTIC_PHASES {
            assert!(
                resolve(phase).is_some(),
                "deterministic phase {phase} must resolve to a DetStep",
            );
        }
    }

    #[test]
    fn test_l1_an_unknown_phase_resolves_to_none() {
        // A worker phase is not deterministic — it does not resolve.
        assert!(resolve("execute").is_none());
        assert!(resolve("plan").is_none());
        assert!(resolve("review").is_none());
        assert!(resolve("nonsense").is_none());
        assert!(resolve("").is_none());
    }

    #[test]
    fn test_l1_the_table_has_exactly_eight_entries() {
        assert_eq!(
            DETERMINISTIC_PHASES.len(),
            8,
            "8 deterministic phases (Q1/Q2)"
        );
        // Every name in the list resolves, and the list has no duplicates —
        // so the resolvable set is exactly these 8.
        let mut seen = std::collections::HashSet::new();
        for phase in DETERMINISTIC_PHASES {
            assert!(resolve(phase).is_some(), "{phase} resolves");
            assert!(seen.insert(*phase), "duplicate phase name {phase}");
        }
        assert_eq!(seen.len(), 8);
    }

    #[test]
    fn test_l1_each_step_body_coerces_to_a_fn_pointer() {
        // Each of the 8 bodies passed where a `DetStep` is expected — this
        // only compiles if the `fn`-pointer coercion holds for every body
        // (review (a): an `async fn` would NOT compile here).
        use crate::runtime::{validate, worktree};
        _assert_detstep(worktree::prepare_spec);
        _assert_detstep(worktree::verify_in);
        _assert_detstep(worktree::verify_out);
        _assert_detstep(worktree::commit);
        _assert_detstep(worktree::merge_to_integration);
        _assert_detstep(worktree::merge_spec);
        _assert_detstep(worktree::teardown);
        _assert_detstep(validate::validate);
    }
}
