//! Surface guard for the conflict-resolver foundation (salvaged from spec
//! Syvwx7psx — MergeStrategy registry v1 surface, the foundational types).
//!
//! Originally authored as the TDD red test for that spec; the surface now
//! exists, so this file is GREEN and lives on as the regression guard.
//!
//! Pins the v1 surface of `runtime::merge_strategies` AND the
//! `BlockedReason::MergeConflict` schema change AND the `StepCtxBuilder`
//! W3 forcing-function:
//!
//!   1. `MergeStrategy` trait exists at `boi::runtime::merge_strategies::MergeStrategy`.
//!   2. `StrategyOutcome` enum is the registry decision taxonomy.
//!   3. `ConflictCtx` + `ConflictedFile` carry the per-conflict invocation context.
//!   4. `BlockedReason::MergeConflict` has the exact field set
//!      `{ conflicts, base_sha, head_sha, reason }` — old `attempted_branch`
//!      / `conflict_files` shape removed (compile-fail destructure naming
//!      the moved-from field).
//!   5. `StepCtxBuilder::build()` returns `Result` and demands an explicit
//!      `with_merge_strategies(...)` call before `build()` succeeds — no
//!      `Default`, no implicit empty registry.
//!
//! The two test bodies are trivially true by construction — the guard is
//! the compile: regressing the contracted shape re-breaks compilation in
//! this isolated test binary (a leftover `attempted_branch` /
//! `conflict_files` field surfaces as `E0026`; a missing new field as
//! `E0027`).

use std::sync::Arc;

use boi::runtime::merge_strategies::{ConflictCtx, ConflictedFile, MergeStrategy, StrategyOutcome};
use boi::types::reasons::BlockedReason;
use boi::types::step::StepCtxBuilder;

/// Compile-time witness that the `MergeStrategy` trait is object-safe and
/// the three companion types are nameable from outside the crate.
fn _accepts_strategy_objects(
    _strategies: Arc<Vec<Arc<dyn MergeStrategy>>>,
    _ctx: ConflictCtx,
    _file: ConflictedFile,
    _outcome: StrategyOutcome,
) {
}

#[test]
fn test_l2_blocked_reason_merge_conflict_post_rewrite_field_set_is_exact() {
    // Compile-time witness: `BlockedReason::MergeConflict` carries exactly
    // the four W4 fields. Any leftover `attempted_branch` / `conflict_files`
    // surfaces as `E0026` here (variant has no such field) and any missing
    // new field surfaces as `E0027` naming it.
    fn _bind(r: BlockedReason) {
        if let BlockedReason::MergeConflict {
            conflicts: _,
            base_sha: _,
            head_sha: _,
            reason: _,
        } = r
        {}
    }
}

#[test]
fn test_l2_step_ctx_builder_requires_explicit_merge_strategies() {
    // Compile-time witness: `StepCtxBuilder::build()` returns a `Result`
    // and `with_merge_strategies` accepts the registry type the W3
    // sharpening contracts. Calling `build()` without the explicit
    // `with_merge_strategies(...)` call must be a compile-or-runtime
    // refusal — this witness pins the API shape; the runtime refusal
    // surfaces in the unit tests for the builder itself.
    fn _shape(
        b: StepCtxBuilder,
        strategies: Arc<Vec<Arc<dyn MergeStrategy>>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let _ = b.with_merge_strategies(strategies).build()?;
        Ok(())
    }
}
