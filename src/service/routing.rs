//! Verdict routing + iteration caps — the orchestrator's "what runs next"
//! brain, kept out of the orchestrator spine (review S7).
//!
//! [`route_spec`] picks the next spec-level phase; [`route_task`] the next
//! task-level phase, with the bounded-loop cap checks. [`validate_pipeline`]
//! runs once at `Orchestrator::new` and fails loudly on a malformed routing
//! graph — a config typo is a startup rejection, never a mid-run wedge (S5).
//!
//! ## The routing model — resolving the §4 redundancy
//!
//! §4 declares both a pipeline phase list AND per-phase `[on.<verdict>]`
//! routing. **`[on.<verdict>].next` is authoritative** for what runs next; the
//! pipeline list defines the phase *set*, the *entry* phase, and (via the
//! [`PipelinePhase::Tasks`] sentinel) the per-task fan-out boundary. The
//! **terminal task phase** is the one whose `[on.passing].next` is absent
//! (`workspace_verify_out`). A `Redo` on a phase with no re-run path is a loud
//! [`RoutingError`], never a silent stall (review S6).
//!
//! [`PipelinePhase::Tasks`]: crate::config::pipeline::PipelinePhase::Tasks
//!
//! ## The deterministic-phase verdict seam (review C6)
//!
//! The [`PhaseExecutor`](crate::service::registry::PhaseExecutor) adapter LIFTS
//! `StepOutcome → WorkerVerdict` (a deterministic `Pass`/`Fail` becomes a
//! `Passing`/`Fail` verdict), so this module's 4-arm verdict router serves
//! worker AND deterministic phases with one code path.
//!
//! ## Iteration caps
//!
//! Three caps are defined here; the fourth, `CAP_TASK_ADJUST`, is defined in
//! [`crate::service::adjustment`] (G20.1 — the side-chain owns all `TaskAdjust`
//! accounting, S2) and imported. `route_task` delegates the side-chain phases
//! and `Fail` verdicts to `adjustment`, so no routing path can slip that bound.
//!
//! All four caps are enforced. The two *task-level* caps (`CAP_EXECUTE_REVIEW`,
//! `CAP_TASK_ADJUST`) count on `task_runtime` via [`route_task`]. The two
//! *spec-level* caps (`CAP_PLAN_CRITIQUE` for the `plan ↔ critique_plan` loop,
//! `CAP_SPEC_REVIEW` for the spec `review` loop) count on `spec_runtime` via
//! [`route_spec`] — G21.1 made `route_spec` `async` so it can increment the
//! `spec_runtime` iteration columns the `0002` migration added. An over-cap
//! spec-level `Redo` returns [`SpecRoute::CapExceeded`], which the orchestrator
//! turns into a `SpecFailed` — no spec-level loop runs uncapped.

use std::collections::HashMap;

use sqlx::SqlitePool;

use crate::config::{PhaseDef, PhaseKind, PipelineDef, PipelinePhase, VerdictTag};
use crate::repo::db::RepoError;
use crate::repo::spec_runtime::SpecIterationCounter;
use crate::repo::task_runtime::IterationCounter;
use crate::service::adjustment::{self, AdjustmentError, AdjustmentRoute};
// `CAP_TASK_ADJUST` is owned by `adjustment.rs` (G20.1) — imported, not
// redefined here, so the side-chain bound has exactly one definition.
pub use crate::service::adjustment::CAP_TASK_ADJUST;
use crate::types::ids::{SpecId, TaskId};
use crate::types::reasons::{BlockedReason, ErrorWhyFix, FailureReason};
use crate::types::verdict::{VerdictOutcome, WorkerVerdict};

/// Cap for the `plan ↔ critique_plan` loop (design §4 default) — defined for
/// the re-export surface; see the module doc's "Latent gap" note on
/// enforcement.
pub const CAP_PLAN_CRITIQUE: u32 = 3;
/// Cap for the `execute ↔ review` work loop (design §4 default).
pub const CAP_EXECUTE_REVIEW: u32 = 3;
/// Cap for the spec-level `review` loop (design §4 default) — see the module
/// doc's "Latent gap" note.
pub const CAP_SPEC_REVIEW: u32 = 2;

/// The terminal per-task FF-merge phase (review C4) — distinct from the
/// spec-level `merge` phase. Routed to after the terminal task phase passes;
/// its own `Passing` is what produces [`TaskAction::TaskPassed`].
const MERGE_TO_INTEGRATION: &str = "merge_to_integration";

/// A routing decision could not be made.
#[derive(Debug, thiserror::Error)]
pub enum RoutingError {
    /// A phase name resolves to no [`PhaseDef`] — a config inconsistency.
    #[error("routing references unknown phase `{0}`")]
    UnknownPhase(String),
    /// A `Redo` landed on a phase with no re-run path and no iteration counter
    /// — a silent stall would be the only alternative (review S6).
    #[error("phase `{0}` produced a Redo verdict but has no iteration counter / re-run route")]
    NoRedoRoute(String),
    /// `validate_pipeline` found a malformed routing graph at startup.
    #[error("pipeline `{pipeline}` is malformed: {detail}")]
    MalformedPipeline {
        /// The offending pipeline's name.
        pipeline: String,
        /// What was wrong.
        detail: String,
    },
    /// A side-chain delegation to [`crate::service::adjustment`] failed.
    #[error("adjustment side-chain routing failed: {0}")]
    Adjustment(#[from] AdjustmentError),
    /// A repo-layer query failed (incrementing an iteration counter).
    #[error("routing query failed: {0}")]
    Repo(#[from] RepoError),
}

/// Where the orchestrator routes the spec pipeline after a spec-level phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecRoute {
    /// Run this spec-level phase next.
    RunSpecPhase(String),
    /// The `<tasks>` fan-out boundary was reached — spawn the ready tasks.
    FanOutTasks,
    /// The terminal spec phase passed — the spec is complete.
    SpecDone,
    /// Stop advancing the spec pipeline. Produced by a spec-phase `Fail` /
    /// `Blocked` with no onward route; the orchestrator turns this into a
    /// `SpecFailed` (it holds the verdict that caused the `Halt`).
    Halt,
    /// A spec-level bounded loop (`plan ↔ critique_plan` or spec `review`)
    /// exceeded its iteration cap (G21.1). Carries the typed
    /// [`FailureReason`] the orchestrator emits as `SpecFailed` — an uncapped
    /// LLM loop is the cost risk the cap exists to prevent.
    CapExceeded(FailureReason),
}

/// Where the orchestrator routes a task after a task-level phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskAction {
    /// Run this task-level phase next.
    RunPhase(String),
    /// The task is done — all task phases (incl. `merge_to_integration`)
    /// passed. The orchestrator emits `TaskPassed`.
    TaskPassed,
    /// Block the task — a cap was exceeded, a deterministic phase failed, or a
    /// worker declared itself blocked. Carries the [`BlockedReason`].
    TaskBlocked(BlockedReason),
    /// Stop — no further task phase. (Reserved; v1.0 routing always produces
    /// one of the three above.)
    Halt,
}

/// The Phase-3 [`IterationCounter`] a bounded-loop phase increments, if any.
///
/// The `execute ↔ review` work loop (`write_red_tests` / `execute` / `review`)
/// → [`IterationCounter::ExecuteReview`]; the `plan ↔ critique_plan` loop →
/// [`IterationCounter::PlanCritique`]. Side-chain and deterministic phases
/// return `None` — the side-chain owns its own `TaskAdjust` counter, and a
/// deterministic phase never `Redo`s (C6).
pub fn counter_for(phase: &str) -> Option<IterationCounter> {
    match phase {
        "write_red_tests" | "execute" | "review" => Some(IterationCounter::ExecuteReview),
        "plan" | "critique_plan" => Some(IterationCounter::PlanCritique),
        _ => None,
    }
}

/// The hard cap for an [`IterationCounter`].
fn cap_for(counter: IterationCounter) -> u32 {
    match counter {
        IterationCounter::PlanCritique => CAP_PLAN_CRITIQUE,
        IterationCounter::TaskAdjust => CAP_TASK_ADJUST,
        IterationCounter::ExecuteReview => CAP_EXECUTE_REVIEW,
        IterationCounter::SpecReview => CAP_SPEC_REVIEW,
    }
}

/// Validate a pipeline's routing graph — run once at `Orchestrator::new`.
///
/// Fails loudly (review S5) on the first violation of: (1) every `spec_phases`
/// / `task_phases` entry resolves to a [`PhaseDef`]; (2) every
/// `[on.<verdict>].next` names a known phase; (3) every `kind = "worker"`
/// phase's `on` table covers all four verdict tags. Deterministic phases are
/// exempt from (3) — the C6 lift means they only yield `Passing` / `Fail`.
pub fn validate_pipeline(
    p: &PipelineDef,
    phases: &HashMap<String, PhaseDef>,
) -> Result<(), RoutingError> {
    let malformed = |detail: String| RoutingError::MalformedPipeline {
        pipeline: p.name.clone(),
        detail,
    };

    // (1) — every pipeline-listed phase exists.
    for entry in &p.spec_phases {
        if let PipelinePhase::Phase(name) = entry
            && !phases.contains_key(name)
        {
            return Err(malformed(format!(
                "spec_phases entry `{name}` has no PhaseDef"
            )));
        }
    }
    for name in &p.task_phases {
        if !phases.contains_key(name) {
            return Err(malformed(format!(
                "task_phases entry `{name}` has no PhaseDef"
            )));
        }
    }

    // (2) + (3) — every phase's routing table is well-formed.
    for (name, def) in phases {
        // (2) every `next` resolves.
        for (tag, rule) in &def.on {
            if let Some(next) = &rule.next
                && !phases.contains_key(next)
            {
                return Err(malformed(format!(
                    "phase `{name}` routes {tag:?}.next to unknown phase `{next}`"
                )));
            }
        }
        // (3) a worker phase must route all four verdicts.
        if def.kind == PhaseKind::Worker {
            for tag in [
                VerdictTag::Passing,
                VerdictTag::Redo,
                VerdictTag::Blocked,
                VerdictTag::Fail,
            ] {
                if !def.on.contains_key(&tag) {
                    return Err(malformed(format!(
                        "worker phase `{name}` is missing routing for {tag:?}"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// The spec-level [`SpecIterationCounter`] a bounded spec-loop phase
/// increments on a `Redo`, if any (G21.1).
///
/// `plan` / `critique_plan` form the `plan ↔ critique_plan` loop →
/// [`SpecIterationCounter::PlanCritique`]; the spec-level `review` phase →
/// [`SpecIterationCounter::SpecReview`]. Every other spec phase
/// (`workspace_prepare`, `merge`, `teardown`, …) is not in a bounded loop and
/// returns `None` — a `Redo` there has no cap (and `validate_pipeline` would
/// have rejected an `on.redo` route it cannot follow anyway).
fn spec_counter_for(phase: &str) -> Option<SpecIterationCounter> {
    match phase {
        "plan" | "critique_plan" => Some(SpecIterationCounter::PlanCritique),
        "review" => Some(SpecIterationCounter::SpecReview),
        _ => None,
    }
}

/// The hard cap for a [`SpecIterationCounter`].
fn spec_cap_for(counter: SpecIterationCounter) -> u32 {
    match counter {
        SpecIterationCounter::PlanCritique => CAP_PLAN_CRITIQUE,
        SpecIterationCounter::SpecReview => CAP_SPEC_REVIEW,
    }
}

/// The typed [`FailureReason`] for a spec-level loop that exceeded its cap.
///
/// The spec `review` loop has a dedicated [`FailureReason::SpecReviewExhausted`]
/// (design §6); the `plan ↔ critique_plan` loop has no dedicated variant, so
/// [`FailureReason::PreflightFailed`] carries an honest description. Either way
/// the reason names the real cause — never a silent uncapped loop.
fn spec_cap_failure(
    counter: SpecIterationCounter,
    count: u32,
    verdict: &WorkerVerdict,
) -> FailureReason {
    match counter {
        SpecIterationCounter::SpecReview => FailureReason::SpecReviewExhausted {
            iterations: count,
            last_critique: verdict.synopsis.clone(),
        },
        SpecIterationCounter::PlanCritique => FailureReason::PreflightFailed {
            details: format!(
                "the plan ↔ critique_plan loop exceeded its cap of {CAP_PLAN_CRITIQUE} \
                 iterations without converging: {}",
                verdict.synopsis,
            ),
        },
    }
}

/// Route the spec pipeline after a spec-level `PhaseCompleted`.
///
/// `async` (G21.1): a spec-level `Redo` increments + cap-checks an iteration
/// counter on `spec_runtime`, so the function needs `&SqlitePool` and a
/// `&SpecId` — `route_spec` was synchronous in the plan's Task 5a.4 sketch,
/// which is exactly why the spec caps shipped unenforced.
///
/// `Passing` advances by `spec_phases` *list position* (see the G17.2 note);
/// `Redo` → increment the spec loop counter, then `on.redo.next` (else re-run
/// `phase`) while at/under the cap, or [`SpecRoute::CapExceeded`] once it is
/// crossed; `Fail` → `on.fail.next` if a spec-level route exists, else
/// [`SpecRoute::Halt`]; `Blocked` → `Halt`. The caller turns a `Halt`-from-
/// `Fail`/`Blocked` and a `CapExceeded` into a loud `SpecFailed`.
///
/// **G17.2 deliberate resolution.** `validate` / `review` appear in BOTH the
/// `spec_phases` and `task_phases` lists but resolve to ONE `PhaseDef` (Phase 2
/// set `level = "task"`), so their `on.passing.next` points along the *task*
/// pipeline (`review.on.passing.next = "commit"`) — wrong for the spec
/// pipeline. `route_spec`'s `Passing` arm therefore advances by the
/// `spec_phases` list. The `Redo` / `Fail` `on` routes (`critique_plan.on.fail
/// → plan`) point *within* the spec pipeline and are kept.
pub async fn route_spec(
    pool: &SqlitePool,
    spec_id: &SpecId,
    phase: &str,
    verdict: &WorkerVerdict,
    p: &PipelineDef,
    phases: &HashMap<String, PhaseDef>,
) -> Result<SpecRoute, RoutingError> {
    let def = phases
        .get(phase)
        .ok_or_else(|| RoutingError::UnknownPhase(phase.to_owned()))?;

    match &verdict.outcome {
        // Advance by `spec_phases` list position (the G17.2 resolution).
        VerdictOutcome::Passing { .. } => Ok(next_spec_phase(phase, p)),
        VerdictOutcome::Redo { .. } => {
            // Re-run target: `on.redo.next` IF it names a spec phase, else the
            // phase itself.
            //
            // G17.2 (Phase 10 erratum): `validate` / `review` are ONE
            // `PhaseDef` shared by both the spec and task lists, so their
            // `on.redo.next` is the *task*-pipeline route (`review.on.redo.next
            // = "execute"`). The `Passing` arm already resolves this by
            // advancing along `spec_phases`; the `Redo` arm must too — a
            // spec-level `review` Redo following `on.redo.next` would jump to
            // `execute` (a task phase), escape the spec pipeline, and the spec
            // would terminate after ONE iteration — so `CAP_SPEC_REVIEW` could
            // never trip and `FailureReason::SpecReviewExhausted` was
            // unreachable. A spec-level Redo whose `on.redo.next` is NOT a spec
            // phase therefore re-runs the phase ITSELF (the spec-loop). A
            // `critique_plan` Redo (`on.redo.next = "plan"`, a real spec phase)
            // still follows its route — the `plan ↔ critique_plan` loop holds.
            let next = match route_next(def, VerdictTag::Redo) {
                Some(target) if is_spec_phase(&target, p) => target,
                _ => phase.to_owned(),
            };
            // G21.1: a spec-level bounded loop increments + cap-checks its
            // `spec_runtime` counter (the spec-level analogue of `route_task`'s
            // task-cap check). A spec phase not in a bounded loop has no
            // counter — it re-runs uncapped exactly as before.
            match spec_counter_for(phase) {
                Some(counter) => {
                    let new_count = spec_repo_increment(pool, spec_id, counter).await?;
                    if new_count > spec_cap_for(counter) {
                        // Over cap — fail loudly, never an uncapped LLM loop.
                        Ok(SpecRoute::CapExceeded(spec_cap_failure(
                            counter, new_count, verdict,
                        )))
                    } else {
                        // At/under the cap — the CAP-th retry still re-runs.
                        Ok(SpecRoute::RunSpecPhase(next))
                    }
                }
                None => Ok(SpecRoute::RunSpecPhase(next)),
            }
        }
        VerdictOutcome::Fail { .. } => match route_next(def, VerdictTag::Fail) {
            // RC2 — only follow `on.fail.next` when it names a real spec phase.
            // A dual-level phase's `on.fail.next` is its *task* route
            // (`validate.on.fail.next = "propose_adjustment"`, the task-scoped
            // adjustment side-chain). A spec-level failure has no adjustment
            // loop — it must Halt. Mirrors the `Redo` arm's G17.2 guard.
            Some(next) if is_spec_phase(&next, p) => Ok(SpecRoute::RunSpecPhase(next)),
            // No onward spec route — the orchestrator turns this into a loud
            // `SpecFailed`.
            _ => Ok(SpecRoute::Halt),
        },
        // A worker declared the spec phase blocked — the pipeline halts; the
        // orchestrator surfaces it (a spec phase has no `blocked` recovery).
        VerdictOutcome::Blocked { .. } => Ok(SpecRoute::Halt),
        // Written only by the cancel path, never by a worker. Should never
        // reach routing; treat as Halt so the spec doesn't stall silently.
        VerdictOutcome::Canceled => Ok(SpecRoute::Halt),
    }
}

/// Increment a spec-level `counter` for `spec_id`, returning the new count.
async fn spec_repo_increment(
    pool: &SqlitePool,
    spec_id: &SpecId,
    counter: SpecIterationCounter,
) -> Result<u32, RoutingError> {
    let raw = crate::repo::spec_runtime::increment_iteration(pool, spec_id, counter).await?;
    // The counter is small and non-negative — clamp defensively rather than
    // panicking on an (impossible) negative, mirroring `repo_increment`.
    Ok(u32::try_from(raw).unwrap_or(u32::MAX))
}

/// Whether `phase` names a phase in the pipeline's `spec_phases` list.
///
/// Used by [`route_spec`]'s `Redo` arm (G17.2): a spec-level Redo only follows
/// `on.redo.next` when that target is itself a spec phase — otherwise the
/// `on.redo.next` is a dual-level phase's *task*-pipeline route and the
/// spec-level Redo must re-run the phase itself instead.
fn is_spec_phase(phase: &str, p: &PipelineDef) -> bool {
    p.spec_phases
        .iter()
        .any(|pp| matches!(pp, PipelinePhase::Phase(n) if n == phase))
}

/// The spec route after `phase` passes — the entry *after* it in `spec_phases`:
/// `<tasks>` → [`SpecRoute::FanOutTasks`]; another phase →
/// [`SpecRoute::RunSpecPhase`]; the list end → [`SpecRoute::SpecDone`]. A
/// `phase` absent from `spec_phases` → `SpecDone` defensively (the orchestrator
/// routes dynamic phases before reaching `route_spec`).
fn next_spec_phase(phase: &str, p: &PipelineDef) -> SpecRoute {
    let Some(idx) = p
        .spec_phases
        .iter()
        .position(|pp| matches!(pp, PipelinePhase::Phase(n) if n == phase))
    else {
        // `phase` is not in `spec_phases` — it cannot advance the spec
        // pipeline. `plan_revision` is special-cased upstream; nothing else
        // should reach here. Halt loudly rather than silently `SpecDone`-ing
        // the spec (RC2 — the silent-completion hole).
        return SpecRoute::Halt;
    };
    match p.spec_phases.get(idx + 1) {
        Some(PipelinePhase::Phase(next)) => SpecRoute::RunSpecPhase(next.clone()),
        Some(PipelinePhase::Tasks) => SpecRoute::FanOutTasks,
        // No following entry — `phase` is the terminal spec phase (`teardown`).
        None => SpecRoute::SpecDone,
    }
}

/// Route a task after a task-level `PhaseCompleted`.
///
/// `review_adjustment` delegates *entirely* to [`crate::service::adjustment`]
/// (G20.1 / S2 — the side-chain owns all `TaskAdjust` accounting). Otherwise:
/// `Passing` → `RunPhase(on.passing.next)`, or — on the terminal task phase —
/// `RunPhase("merge_to_integration")`, and on `merge_to_integration` itself →
/// [`TaskAction::TaskPassed`] (the FF-merge runs *before* `TaskPassed`, C4);
/// `Redo` → increment + cap-check the loop counter (CAP-th retry re-runs,
/// CAP+1-th trips `CapExceeded` — item 21), a side-chain `Redo` re-enters via
/// `adjustment`; `Fail` → the side-chain when `on.fail.next ==
/// propose_adjustment`, else `TaskBlocked` (a deterministic phase failed);
/// `Blocked` → `TaskBlocked`.
pub async fn route_task(
    pool: &SqlitePool,
    phase: &str,
    task_id: &TaskId,
    verdict: &WorkerVerdict,
    phases: &HashMap<String, PhaseDef>,
) -> Result<TaskAction, RoutingError> {
    // `review_adjustment` is routed wholly by the side-chain — its verdict
    // decides re-enter / exit / block, and that path owns the cap.
    if phase == "review_adjustment" {
        let route = adjustment::route_after_review_adjustment(pool, task_id, verdict).await?;
        return Ok(adjustment_route_to_action(route));
    }

    let def = phases
        .get(phase)
        .ok_or_else(|| RoutingError::UnknownPhase(phase.to_owned()))?;

    match &verdict.outcome {
        VerdictOutcome::Passing { .. } => {
            if phase == MERGE_TO_INTEGRATION {
                // The FF-merge passed — NOW the task is truthfully done.
                return Ok(TaskAction::TaskPassed);
            }
            match route_next(def, VerdictTag::Passing) {
                Some(next) => Ok(TaskAction::RunPhase(next)),
                // Terminal task phase (`workspace_verify_out`) — run the
                // per-task FF-merge before `TaskPassed` (review C4).
                None => Ok(TaskAction::RunPhase(MERGE_TO_INTEGRATION.to_owned())),
            }
        }
        VerdictOutcome::Redo { .. } => {
            // A side-chain `Redo` re-enters the side-chain — `adjustment` owns
            // the `TaskAdjust` counter (never the generic cap path).
            if adjustment::is_side_chain_phase(phase) {
                let route = adjustment::route_after_fail(pool, task_id).await?;
                return Ok(adjustment_route_to_action(route));
            }
            // A work-loop `Redo` — increment + cap-check the loop counter.
            let counter =
                counter_for(phase).ok_or_else(|| RoutingError::NoRedoRoute(phase.to_owned()))?;
            let new_count = repo_increment(pool, task_id, counter).await?;
            if new_count > cap_for(counter) {
                Ok(TaskAction::TaskBlocked(BlockedReason::CapExceeded {
                    loop_name: loop_name_of(counter).to_owned(),
                    cap: cap_for(counter),
                    last_error_why_fix: redo_reason(verdict),
                }))
            } else {
                // CAP retries are permitted — the CAP-th re-runs (item 21).
                Ok(TaskAction::RunPhase(phase.to_owned()))
            }
        }
        VerdictOutcome::Fail { error, why, fix } => {
            let ewf = ErrorWhyFix {
                error: error.clone(),
                why: why.clone(),
                fix: fix.clone(),
            };
            match route_next(def, VerdictTag::Fail) {
                // The worker-phase failure path — into the adjustment side-chain.
                Some(next) if next == "propose_adjustment" => {
                    let route = adjustment::route_after_fail(pool, task_id).await?;
                    Ok(adjustment_route_to_action(route))
                }
                // A worker phase that routes `fail` somewhere else — honour it.
                Some(next) => Ok(TaskAction::RunPhase(next)),
                // No onward `next` route. The reason MUST match the phase
                // kind (review B-orch-2): a `kind = "worker"` side-chain phase
                // (`propose_adjustment`) declares `[on.fail]` with no `next`
                // by design — a `Fail` there is a worker verdict failure, NOT
                // a git/workspace problem, so `deterministic_fail_reason`'s
                // `WorkspaceUnclean` would mis-name the cause and send the
                // operator's recovery the wrong way. Block worker phases with
                // an honest worker reason; only a genuine deterministic phase
                // (no `on.fail` route at all) gets `WorkspaceUnclean`.
                None if def.kind == PhaseKind::Worker || adjustment::is_side_chain_phase(phase) => {
                    Ok(TaskAction::TaskBlocked(worker_fail_reason(phase, &ewf)))
                }
                // No onward route — a deterministic phase failed. Block the
                // task with the phase-specific (git/workspace) reason.
                None => Ok(TaskAction::TaskBlocked(deterministic_fail_reason(
                    phase, ewf,
                ))),
            }
        }
        VerdictOutcome::Blocked {
            reason,
            error_why_fix,
        } => Ok(TaskAction::TaskBlocked(worker_blocked_reason(
            reason,
            error_why_fix.as_ref(),
        ))),
        // Written only by the cancel path, never by a worker. Should never
        // reach routing; Halt so no silent stall.
        VerdictOutcome::Canceled => Ok(TaskAction::Halt),
    }
}

/// The `on.<tag>.next` target for a phase, if a route is declared.
fn route_next(def: &PhaseDef, tag: VerdictTag) -> Option<String> {
    def.on.get(&tag).and_then(|rule| rule.next.clone())
}

/// Increment `counter` for `task_id`, returning the new count as a `u32`.
async fn repo_increment(
    pool: &SqlitePool,
    task_id: &TaskId,
    counter: IterationCounter,
) -> Result<u32, RoutingError> {
    let raw = crate::repo::task_runtime::increment_iteration(pool, task_id, counter).await?;
    // The counter is small and non-negative; clamp defensively rather than
    // panicking on an (impossible) negative value.
    Ok(u32::try_from(raw).unwrap_or(u32::MAX))
}

/// Map an [`AdjustmentRoute`] (side-chain decision) onto a [`TaskAction`].
fn adjustment_route_to_action(route: AdjustmentRoute) -> TaskAction {
    match route {
        AdjustmentRoute::RunPhase(p) => TaskAction::RunPhase(p),
        AdjustmentRoute::Block(reason) => TaskAction::TaskBlocked(reason),
    }
}

/// The `loop_name` string for a [`BlockedReason::CapExceeded`].
fn loop_name_of(counter: IterationCounter) -> &'static str {
    match counter {
        IterationCounter::PlanCritique => "plan_critique",
        IterationCounter::TaskAdjust => "task_adjust",
        IterationCounter::ExecuteReview => "execute_review",
        IterationCounter::SpecReview => "spec_review",
    }
}

/// The [`ErrorWhyFix`] to cite in a `CapExceeded` reason from a `Redo` verdict.
///
/// A `Redo` carries only a free-text `reason`, so the triple is synthesised
/// from it — never an empty/silent value.
fn redo_reason(verdict: &WorkerVerdict) -> ErrorWhyFix {
    let reason = match &verdict.outcome {
        VerdictOutcome::Redo { reason } => reason.clone(),
        _ => verdict.synopsis.clone(),
    };
    ErrorWhyFix {
        error: "iteration cap exceeded".to_owned(),
        why: format!("the bounded retry loop did not converge: {reason}"),
        fix: "inspect the phase history and intervene manually".to_owned(),
    }
}

/// The [`BlockedReason`] for a *deterministic* phase that returned `Fail`.
///
/// A deterministic phase has no `on.fail.next` route, so a `Fail` blocks the
/// task. `merge_to_integration` → [`BlockedReason::MergeConflict`]; every other
/// deterministic phase (`workspace_verify_in/out`, `commit`, …) →
/// [`BlockedReason::WorkspaceUnclean`] — a deterministic phase only touches
/// git / filesystem state, so a routeless failure is a workspace problem.
fn deterministic_fail_reason(phase: &str, ewf: ErrorWhyFix) -> BlockedReason {
    match phase {
        MERGE_TO_INTEGRATION => BlockedReason::MergeConflict {
            conflicts: Vec::new(),
            base_sha: String::new(),
            head_sha: String::new(),
            reason: format!("{phase}: {} — {}", ewf.error, ewf.why),
        },
        _ => BlockedReason::WorkspaceUnclean {
            details: format!("{phase} failed: {} — {}", ewf.error, ewf.why),
        },
    }
}

/// The [`BlockedReason`] for a *worker* phase that returned `Fail` with no
/// onward `on.fail.next` route (review B-orch-2).
///
/// The only worker phase that legitimately reaches here is `propose_adjustment`
/// — a side-chain phase whose `[on.fail]` carries no `next` (the side-chain has
/// no recovery for a failed proposal). A `Fail` there is a *worker* failure
/// (verdict-parse, the model could not produce a fix), NOT a git/workspace
/// problem, so [`deterministic_fail_reason`]'s `WorkspaceUnclean` would lie
/// about the cause. `BlockedReason` has no worker-fail variant, so
/// [`BlockedReason::Manual`] carries the worker's honest error/why/fix triple.
fn worker_fail_reason(phase: &str, ewf: &ErrorWhyFix) -> BlockedReason {
    BlockedReason::Manual {
        operator_note: Some(format!(
            "worker phase `{phase}` failed with no recovery route: {} — {} (fix: {})",
            ewf.error, ewf.why, ewf.fix,
        )),
    }
}

/// The [`BlockedReason`] for a *worker* phase that returned a `Blocked` verdict.
///
/// `BlockedReason` has no dedicated worker-self-block variant, so
/// [`BlockedReason::Manual`] carries the worker's free-text reason — the honest
/// generic for "a worker declared itself unable to continue".
fn worker_blocked_reason(reason: &str, error_why_fix: Option<&ErrorWhyFix>) -> BlockedReason {
    let note = match error_why_fix {
        Some(ewf) => format!("{reason} — {} ({})", ewf.error, ewf.why),
        None => reason.to_owned(),
    };
    BlockedReason::Manual {
        operator_note: Some(note),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{parse_phase, parse_pipeline};
    use crate::repo::db::connect;
    use crate::repo::specs::insert_spec;
    use crate::repo::task_runtime::insert_task;
    use crate::types::ids::SpecId;
    use crate::types::verdict::{Evidence, VerdictOutcome, WorkerVerdict};
    use chrono::Utc;

    /// Load every `standard`-pipeline phase fixture into a name→def map.
    fn all_phases() -> HashMap<String, PhaseDef> {
        const NAMES: &[&str] = &[
            "workspace_prepare",
            "plan",
            "critique_plan",
            "workspace_verify_in",
            "write_red_tests",
            "execute",
            "validate",
            "review",
            "propose_adjustment",
            "review_adjustment",
            "commit",
            "merge",
            "teardown",
            "workspace_verify_out",
            "merge_to_integration",
            "plan_revision",
        ];
        let mut map = HashMap::new();
        for name in NAMES {
            let toml = std::fs::read_to_string(format!(
                "{}/tests/fixtures/phases/{name}.toml",
                env!("CARGO_MANIFEST_DIR"),
            ))
            .unwrap();
            map.insert((*name).to_owned(), parse_phase(&toml).unwrap());
        }
        map
    }

    fn standard_pipeline() -> PipelineDef {
        let toml = std::fs::read_to_string(format!(
            "{}/tests/fixtures/pipelines/standard.toml",
            env!("CARGO_MANIFEST_DIR"),
        ))
        .unwrap();
        parse_pipeline(&toml).unwrap()
    }

    fn passing() -> WorkerVerdict {
        WorkerVerdict {
            synopsis: "ok".into(),
            outcome: VerdictOutcome::Passing {
                evidence: Evidence::default(),
            },
        }
    }
    fn redo() -> WorkerVerdict {
        WorkerVerdict {
            synopsis: "again".into(),
            outcome: VerdictOutcome::Redo {
                reason: "flaky".into(),
            },
        }
    }
    fn fail() -> WorkerVerdict {
        WorkerVerdict {
            synopsis: "broke".into(),
            outcome: VerdictOutcome::Fail {
                error: "E".into(),
                why: "W".into(),
                fix: "F".into(),
            },
        }
    }

    fn spec() -> SpecId {
        SpecId::new("S0000001a").unwrap()
    }
    fn task() -> TaskId {
        TaskId::new("T0000001a").unwrap()
    }

    /// A pool with a spec (specs row + v1 snapshot + initialized
    /// `spec_runtime`) and one task — ready for task-level AND spec-level
    /// counter increments (G21.1's `route_spec` cap-check needs `spec_runtime`).
    async fn seeded() -> SqlitePool {
        use crate::repo::spec_versions::{VersionTrigger, append_version};
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
        crate::repo::spec_runtime::initialize(&pool, &spec(), 1)
            .await
            .unwrap();
        insert_task(&pool, &task(), &spec(), None).await.unwrap();
        pool
    }

    /// The four caps are the design §4 defaults.
    #[test]
    fn test_l1_caps_are_design_defaults() {
        assert_eq!(CAP_PLAN_CRITIQUE, 3);
        assert_eq!(CAP_EXECUTE_REVIEW, 3);
        assert_eq!(CAP_SPEC_REVIEW, 2);
        // CAP_TASK_ADJUST is re-exported from adjustment.rs (G20.1).
        assert_eq!(CAP_TASK_ADJUST, 3);
    }

    /// `validate_pipeline` accepts the shipped `standard` pipeline + fixtures.
    #[test]
    fn test_l1_validate_pipeline_accepts_standard() {
        assert!(validate_pipeline(&standard_pipeline(), &all_phases()).is_ok());
    }

    /// `validate_pipeline` rejects a dangling `on.<verdict>.next`.
    #[test]
    fn test_l2_validate_pipeline_rejects_dangling_next() {
        let pipeline = standard_pipeline();
        let mut phases = all_phases();
        // Point `execute.on.fail.next` at a phase that does not exist.
        let execute = phases.get_mut("execute").unwrap();
        execute.on.get_mut(&VerdictTag::Fail).unwrap().next = Some("no_such_phase".to_owned());
        let err = validate_pipeline(&pipeline, &phases).unwrap_err();
        assert!(
            matches!(err, RoutingError::MalformedPipeline { .. }),
            "a dangling on.fail.next must be rejected, got {err:?}",
        );
    }

    /// `validate_pipeline` rejects a `spec_phases` entry with no `PhaseDef`.
    #[test]
    fn test_l2_validate_pipeline_rejects_unknown_spec_phase() {
        let mut pipeline = standard_pipeline();
        pipeline
            .spec_phases
            .push(PipelinePhase::Phase("ghost_phase".to_owned()));
        let err = validate_pipeline(&pipeline, &all_phases()).unwrap_err();
        assert!(matches!(err, RoutingError::MalformedPipeline { .. }));
    }

    /// `route_spec`: `Passing` on `critique_plan` (the phase before `<tasks>`)
    /// fans out, NOT advances to `critique_plan.on.passing.next` (`validate`).
    #[tokio::test]
    async fn test_l2_route_spec_critique_plan_passing_fans_out() {
        let pool = seeded().await;
        let route = route_spec(
            &pool,
            &spec(),
            "critique_plan",
            &passing(),
            &standard_pipeline(),
            &all_phases(),
        )
        .await
        .unwrap();
        assert_eq!(route, SpecRoute::FanOutTasks);
    }

    /// `route_spec`: `Passing` advances by `spec_phases` LIST position —
    /// `workspace_prepare` → `plan`. (Not `on.passing.next`; see G17.2.)
    #[tokio::test]
    async fn test_l2_route_spec_passing_advances_by_list_position() {
        let pool = seeded().await;
        let route = route_spec(
            &pool,
            &spec(),
            "workspace_prepare",
            &passing(),
            &standard_pipeline(),
            &all_phases(),
        )
        .await
        .unwrap();
        assert_eq!(route, SpecRoute::RunSpecPhase("plan".to_owned()));
    }

    /// `route_spec`: the G17.2 case — spec-level `review` `Passing` advances to
    /// `merge` (the `spec_phases` list successor), NOT to `commit`
    /// (`review.on.passing.next`, which is the *task* pipeline's successor).
    #[tokio::test]
    async fn test_l2_route_spec_review_advances_to_merge_not_commit() {
        let pool = seeded().await;
        let route = route_spec(
            &pool,
            &spec(),
            "review",
            &passing(),
            &standard_pipeline(),
            &all_phases(),
        )
        .await
        .unwrap();
        assert_eq!(
            route,
            SpecRoute::RunSpecPhase("merge".to_owned()),
            "spec-level review must advance by the spec_phases list (→ merge), \
             not review.on.passing.next (→ commit)",
        );
    }

    /// `route_spec`: `Passing` on the terminal `teardown` phase → `SpecDone`.
    #[tokio::test]
    async fn test_l2_route_spec_teardown_passing_is_spec_done() {
        let pool = seeded().await;
        let route = route_spec(
            &pool,
            &spec(),
            "teardown",
            &passing(),
            &standard_pipeline(),
            &all_phases(),
        )
        .await
        .unwrap();
        assert_eq!(route, SpecRoute::SpecDone);
    }

    /// `route_spec`: a `Fail` on `workspace_prepare` (no `on.fail.next`) →
    /// `Halt` (the orchestrator turns this into `SpecFailed`).
    #[tokio::test]
    async fn test_l2_route_spec_fail_with_no_route_halts() {
        let pool = seeded().await;
        let route = route_spec(
            &pool,
            &spec(),
            "workspace_prepare",
            &fail(),
            &standard_pipeline(),
            &all_phases(),
        )
        .await
        .unwrap();
        assert_eq!(route, SpecRoute::Halt);
    }

    /// RC2 regression: a spec-level `Fail` whose `on.fail.next` is a task-scoped
    /// phase (`validate.on.fail.next = "propose_adjustment"`) must `Halt`, not
    /// route into the task-only adjustment side-chain — which then silently
    /// `SpecDone`-d the spec (the false green).
    #[tokio::test]
    async fn test_l2_route_spec_fail_into_task_phase_halts() {
        let pool = seeded().await;
        let route = route_spec(
            &pool,
            &spec(),
            "validate",
            &fail(),
            &standard_pipeline(),
            &all_phases(),
        )
        .await
        .unwrap();
        assert_eq!(
            route,
            SpecRoute::Halt,
            "a spec-level validate failure must Halt — propose_adjustment is task-scoped",
        );
    }

    /// RC2 regression: `next_spec_phase` for a phase absent from `spec_phases`
    /// `Halt`s — it must never silently `SpecDone` the spec.
    #[test]
    fn test_l2_next_spec_phase_absent_phase_halts() {
        assert_eq!(
            next_spec_phase("propose_adjustment", &standard_pipeline()),
            SpecRoute::Halt,
        );
    }

    /// G21.1 regression: a spec-level `plan ↔ critique_plan` loop is CAPPED.
    /// Three `Redo`s on `critique_plan` re-run `plan`; the 4th crosses
    /// `CAP_PLAN_CRITIQUE` (3) → `SpecRoute::CapExceeded` — no uncapped loop.
    ///
    /// Before G21.1 `route_spec` was synchronous and could not touch a
    /// counter, so a `plan ↔ critique_plan` redo loop ran forever.
    #[tokio::test]
    async fn test_l2_route_spec_plan_critique_redo_caps() {
        let pool = seeded().await;
        let phases = all_phases();
        // Rounds 1–3 (≤ CAP) each re-run `plan` (critique_plan.on.redo.next).
        for round in 1..=3 {
            let route = route_spec(
                &pool,
                &spec(),
                "critique_plan",
                &redo(),
                &standard_pipeline(),
                &phases,
            )
            .await
            .unwrap();
            assert_eq!(
                route,
                SpecRoute::RunSpecPhase("plan".to_owned()),
                "Redo round {round} (≤ CAP) must re-run plan",
            );
        }
        // The spec-level counter is now at CAP_PLAN_CRITIQUE.
        let row = crate::repo::spec_runtime::fetch(&pool, &spec())
            .await
            .unwrap();
        assert_eq!(row.iterations_plan_critique, 3);
        assert_eq!(row.iterations_spec_review, 0, "only one spec counter moved");

        // Round 4 crosses the cap → CapExceeded.
        let route = route_spec(
            &pool,
            &spec(),
            "critique_plan",
            &redo(),
            &standard_pipeline(),
            &phases,
        )
        .await
        .unwrap();
        assert!(
            matches!(route, SpecRoute::CapExceeded(_)),
            "the 4th plan-critique Redo must trip CapExceeded, got {route:?}",
        );
    }

    /// G21.1 + G17.2 regression: the spec-level `review` loop is CAPPED with
    /// its own counter, and over-cap fails with the typed `SpecReviewExhausted`
    /// reason. `CAP_SPEC_REVIEW` is 2, so the 3rd `Redo` trips the cap.
    ///
    /// The G17.2 part (Phase 10 erratum): a spec-level `review` Redo must
    /// re-run **`review` itself** — NOT follow `review.on.redo.next` (which is
    /// `"execute"`, the dual-level phase's TASK route). Before the fix the
    /// spec-`review` Redo jumped to `execute`, escaped the spec pipeline, and
    /// the spec terminated after one iteration — so this cap was unreachable
    /// when driven through the orchestrator (the in-isolation call here
    /// counted up regardless, masking it).
    #[tokio::test]
    async fn test_l2_route_spec_review_redo_caps_with_spec_review_exhausted() {
        let pool = seeded().await;
        let phases = all_phases();
        // Rounds 1–2 (≤ CAP_SPEC_REVIEW) re-run `review` ITSELF — a spec-level
        // Redo whose `on.redo.next` (`execute`) is NOT a spec phase re-runs
        // the phase, keeping the loop inside the spec pipeline (G17.2).
        for round in 1..=2 {
            let route = route_spec(
                &pool,
                &spec(),
                "review",
                &redo(),
                &standard_pipeline(),
                &phases,
            )
            .await
            .unwrap();
            assert_eq!(
                route,
                SpecRoute::RunSpecPhase("review".to_owned()),
                "Redo round {round} (≤ CAP) must re-run `review` ITSELF — not \
                 escape to `execute` via the task-level on.redo.next (G17.2)",
            );
        }
        // Round 3 crosses CAP_SPEC_REVIEW (2) → CapExceeded with the typed
        // SpecReviewExhausted reason naming the iteration count.
        let route = route_spec(
            &pool,
            &spec(),
            "review",
            &redo(),
            &standard_pipeline(),
            &phases,
        )
        .await
        .unwrap();
        let SpecRoute::CapExceeded(FailureReason::SpecReviewExhausted { iterations, .. }) = route
        else {
            unreachable!(
                "the 3rd spec-review Redo must trip CapExceeded(SpecReviewExhausted), got {route:?}",
            );
        };
        assert_eq!(iterations, 3, "the over-cap iteration count is reported");
    }

    /// `route_task`: a scripted `Passing` chain advances
    /// `workspace_verify_in → write_red_tests → … → workspace_verify_out`,
    /// then the terminal phase routes to `merge_to_integration`, whose
    /// `Passing` is `TaskPassed`.
    #[tokio::test]
    async fn test_l2_route_task_passing_chain_through_merge_to_task_passed() {
        let pool = seeded().await;
        let phases = all_phases();
        let steps = [
            ("workspace_verify_in", "write_red_tests"),
            ("write_red_tests", "execute"),
            ("execute", "validate"),
            ("validate", "review"),
            ("review", "commit"),
            ("commit", "workspace_verify_out"),
        ];
        for (phase, expected_next) in steps {
            let action = route_task(&pool, phase, &task(), &passing(), &phases)
                .await
                .unwrap();
            assert_eq!(
                action,
                TaskAction::RunPhase(expected_next.to_owned()),
                "{phase} Passing should advance to {expected_next}",
            );
        }
        // The terminal task phase routes to the per-task FF-merge.
        let action = route_task(&pool, "workspace_verify_out", &task(), &passing(), &phases)
            .await
            .unwrap();
        assert_eq!(
            action,
            TaskAction::RunPhase("merge_to_integration".to_owned()),
            "the terminal task phase runs merge_to_integration before TaskPassed",
        );
        // merge_to_integration Passing → TaskPassed.
        let action = route_task(&pool, "merge_to_integration", &task(), &passing(), &phases)
            .await
            .unwrap();
        assert_eq!(action, TaskAction::TaskPassed);
    }

    /// `route_task`: `merge_to_integration` `Fail` → `TaskBlocked{MergeConflict}`
    /// (the §13.3 failure-path test for `MergeConflict`).
    #[tokio::test]
    async fn test_l2_route_task_merge_to_integration_fail_is_merge_conflict() {
        let pool = seeded().await;
        let action = route_task(
            &pool,
            "merge_to_integration",
            &task(),
            &fail(),
            &all_phases(),
        )
        .await
        .unwrap();
        assert!(
            matches!(
                action,
                TaskAction::TaskBlocked(BlockedReason::MergeConflict { .. })
            ),
            "merge_to_integration Fail must block with MergeConflict, got {action:?}",
        );
    }

    /// `route_task`: a deterministic `workspace_verify_in` `Fail` →
    /// `TaskBlocked{WorkspaceUnclean}` (the §13.3 test for `WorkspaceUnclean`).
    #[tokio::test]
    async fn test_l2_route_task_workspace_verify_in_fail_is_workspace_unclean() {
        let pool = seeded().await;
        let action = route_task(
            &pool,
            "workspace_verify_in",
            &task(),
            &fail(),
            &all_phases(),
        )
        .await
        .unwrap();
        assert!(
            matches!(
                action,
                TaskAction::TaskBlocked(BlockedReason::WorkspaceUnclean { .. })
            ),
            "workspace_verify_in Fail must block with WorkspaceUnclean, got {action:?}",
        );
    }

    /// `route_task`: a `Redo` re-runs the SAME phase and increments only that
    /// phase's counter; the 3rd `Redo` on `execute` still re-runs, the 4th
    /// trips `CapExceeded` (review item 21 — CAP retries permitted).
    #[tokio::test]
    async fn test_l2_route_task_redo_caps_on_fourth() {
        let pool = seeded().await;
        let phases = all_phases();
        // Rounds 1–3: each Redo re-runs `execute`.
        for round in 1..=3 {
            let action = route_task(&pool, "execute", &task(), &redo(), &phases)
                .await
                .unwrap();
            assert_eq!(
                action,
                TaskAction::RunPhase("execute".to_owned()),
                "Redo round {round} (≤ CAP) must re-run execute",
            );
        }
        // The counter is at CAP_EXECUTE_REVIEW (3); only execute_review moved.
        let row = crate::repo::task_runtime::fetch(&pool, &task())
            .await
            .unwrap();
        assert_eq!(row.iterations_execute_review, 3);
        assert_eq!(row.iterations_plan_critique, 0, "only one counter moved");

        // Round 4 crosses the cap → TaskBlocked(CapExceeded).
        let action = route_task(&pool, "execute", &task(), &redo(), &phases)
            .await
            .unwrap();
        let TaskAction::TaskBlocked(BlockedReason::CapExceeded { loop_name, cap, .. }) = action
        else {
            unreachable!("the 4th Redo must trip CapExceeded, got {action:?}");
        };
        assert_eq!(loop_name, "execute_review");
        assert_eq!(cap, CAP_EXECUTE_REVIEW);
    }

    /// `route_task`: a worker `Blocked` verdict → `TaskBlocked` carrying the
    /// worker's reason.
    #[tokio::test]
    async fn test_l2_route_task_blocked_verdict_blocks_task() {
        let pool = seeded().await;
        let blocked = WorkerVerdict {
            synopsis: "stuck".into(),
            outcome: VerdictOutcome::Blocked {
                reason: "needs credentials".into(),
                error_why_fix: None,
            },
        };
        let action = route_task(&pool, "execute", &task(), &blocked, &all_phases())
            .await
            .unwrap();
        let TaskAction::TaskBlocked(BlockedReason::Manual { operator_note }) = action else {
            unreachable!("a Blocked verdict must block the task, got {action:?}");
        };
        assert!(operator_note.unwrap().contains("needs credentials"));
    }

    /// `route_task`: a worker-phase `Fail` on `execute` (whose
    /// `on.fail.next = propose_adjustment`) enters the side-chain — the first
    /// failure routes to `propose_adjustment`.
    #[tokio::test]
    async fn test_l2_route_task_worker_fail_enters_side_chain() {
        let pool = seeded().await;
        let action = route_task(&pool, "execute", &task(), &fail(), &all_phases())
            .await
            .unwrap();
        assert_eq!(
            action,
            TaskAction::RunPhase("propose_adjustment".to_owned()),
            "an execute Fail enters the adjustment side-chain",
        );
        // The side-chain's TaskAdjust counter incremented (adjustment owns it).
        let row = crate::repo::task_runtime::fetch(&pool, &task())
            .await
            .unwrap();
        assert_eq!(row.iterations_task_adjust, 1);
    }

    /// B-orch-2 regression: a `Fail` on `propose_adjustment` (a `kind=worker`
    /// side-chain phase whose `[on.fail]` carries no `next`) must block the
    /// task with an honest *worker* reason — NOT `WorkspaceUnclean`.
    ///
    /// Before the fix the `None`-route branch fell straight to
    /// `deterministic_fail_reason`, which labels every routeless failure
    /// `WorkspaceUnclean` — a git/workspace reason. A worker verdict-parse
    /// failure on `propose_adjustment` is not a dirty tree; the wrong reason
    /// sends the operator's recovery the wrong way.
    #[tokio::test]
    async fn test_l2_route_task_propose_adjustment_fail_is_not_workspace_unclean() {
        let pool = seeded().await;
        let action = route_task(&pool, "propose_adjustment", &task(), &fail(), &all_phases())
            .await
            .unwrap();
        // It must block the task...
        let TaskAction::TaskBlocked(reason) = action else {
            unreachable!("a propose_adjustment Fail with no route must block, got {action:?}");
        };
        // ...but NOT with the git/workspace reason.
        assert!(
            !matches!(reason, BlockedReason::WorkspaceUnclean { .. }),
            "a worker-phase Fail must NOT be mislabelled WorkspaceUnclean, got {reason:?}",
        );
        // The honest reason is a Manual note naming the worker phase.
        let BlockedReason::Manual { operator_note } = reason else {
            unreachable!("expected a Manual reason for a worker-phase fail, got {reason:?}");
        };
        assert!(
            operator_note.unwrap().contains("propose_adjustment"),
            "the block note must name the failing worker phase",
        );
    }

    /// `route_task`: `review_adjustment` delegates entirely to `adjustment` —
    /// a `Passing` verdict is the side-chain exit back to `execute`.
    #[tokio::test]
    async fn test_l2_route_task_review_adjustment_passing_exits_to_execute() {
        let pool = seeded().await;
        let action = route_task(
            &pool,
            "review_adjustment",
            &task(),
            &passing(),
            &all_phases(),
        )
        .await
        .unwrap();
        assert_eq!(action, TaskAction::RunPhase("execute".to_owned()));
    }

    /// B-orch-S7 regression: a *deterministic* phase that returns `Redo` is a
    /// C6-contract violation (the `StepOutcome → WorkerVerdict` lift only ever
    /// produces `Passing` / `Fail` for a deterministic phase) — `route_task`
    /// must fail LOUDLY with `NoRedoRoute`, never silently re-run or stall.
    ///
    /// `MockExecutor` only ever emits worker-shaped `PhaseCompleted`, so the
    /// orchestrator-level walk never exercises this seam; this routing test
    /// drives it directly. `workspace_verify_in` is a deterministic phase with
    /// no `on.redo` route and no iteration counter.
    #[tokio::test]
    async fn test_l2_route_task_deterministic_phase_redo_is_loud_no_redo_route() {
        let pool = seeded().await;
        let err = route_task(
            &pool,
            "workspace_verify_in",
            &task(),
            &redo(),
            &all_phases(),
        )
        .await
        .unwrap_err();
        let RoutingError::NoRedoRoute(phase) = err else {
            unreachable!(
                "a deterministic-phase Redo must be a loud NoRedoRoute error, got {err:?}",
            );
        };
        assert_eq!(phase, "workspace_verify_in");
    }

    /// `counter_for` maps the work-loop phases and returns `None` for
    /// deterministic / side-chain phases.
    #[test]
    fn test_l1_counter_for_maps_work_loop_phases() {
        assert_eq!(
            counter_for("execute"),
            Some(IterationCounter::ExecuteReview)
        );
        assert_eq!(
            counter_for("write_red_tests"),
            Some(IterationCounter::ExecuteReview),
        );
        assert_eq!(counter_for("plan"), Some(IterationCounter::PlanCritique));
        // Deterministic + side-chain phases have no generic counter.
        assert_eq!(counter_for("workspace_verify_in"), None);
        assert_eq!(counter_for("propose_adjustment"), None);
    }
}
