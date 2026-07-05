//! The task-level adjustment side-chain — design §4's first adjustment loop.
//!
//! When a worker or validate phase returns `Verdict::Fail`, the task does not
//! halt: it enters the *side-chain*, a two-phase loop that turns Lec 10's
//! ERROR/WHY/FIX into phases.
//!
//! ```text
//! Execute → Validate → Review
//!                       ↓ (failure)
//!                   propose_adjustment  ← emits (error, why, fix)
//!                       ↓
//!                   review_adjustment   ← validates the fix is in-scope
//!                       ↓ (approved)
//!                     Execute           ← applies the fix, restarts the loop
//! ```
//!
//! ## The cap and the one-place-it-is-checked rule (review S2)
//!
//! The side-chain is bounded by [`CAP_TASK_ADJUST`]. The loop has **two** edges
//! that begin a fresh `propose_adjustment` round:
//!
//! - a `Fail` verdict, via [`route_after_fail`];
//! - a `Redo` on `review_adjustment`, via [`route_after_review_adjustment`].
//!
//! Both go through the private `enter_side_chain`, which is the single site
//! that increments and cap-checks the `TaskAdjust` counter. An earlier split
//! that counted only the `Fail` edge let a `Redo` on `review_adjustment` loop
//! `propose ↔ review` forever — an uncapped LLM loop, a cost bomb. Routing every
//! re-entry through `enter_side_chain` closes that.
//!
//! ## Where `CAP_TASK_ADJUST` lives (deviation from the plan — see below)
//!
//! The plan's Task 5b.1 says `adjustment.rs` imports `CAP_TASK_ADJUST` "from
//! `crate::service::routing`", and Task 5a.4 lists it among `routing.rs`'s cap
//! constants — but `routing.rs` is a Phase 5a file built *after* this one (build
//! order is 5c → 5b → 5a). That is an ordering contradiction, and the plan's own
//! Task 5b.1 prose says "all `TaskAdjust` cap accounting lives in
//! `adjustment.rs`". Resolution: the constant is **defined here**, beside the
//! accounting that uses it. Phase 5a's `routing.rs` imports it from
//! `crate::service::adjustment::CAP_TASK_ADJUST`.
//!
//! ## Context forwarding is not this module's job
//!
//! The `error/why/fix` triple from the `Fail` verdict is persisted to
//! `phase_runs` by the bus; the Phase 3 composition query → `PhaseRunSummary`
//! → the Phase 5c renderer carries it into the next phase's context
//! automatically (§7.5 rule 8). This module only *sequences* the side-chain
//! and *holds* the cap — it never re-implements forwarding.

use sqlx::SqlitePool;

use crate::repo;
use crate::repo::db::RepoError;
use crate::repo::task_runtime::IterationCounter;
use crate::types::ids::TaskId;
use crate::types::reasons::{BlockedReason, ErrorWhyFix};
use crate::types::verdict::{VerdictOutcome, WorkerVerdict};

/// The maximum number of `propose_adjustment` rounds a task may take before the
/// side-chain gives up and the task blocks with [`BlockedReason::CapExceeded`]
/// (design §4 — `max_iterations.task_adjust`, default 3).
///
/// Defined here rather than in Phase 5a's `routing.rs` — see the module doc's
/// "Where `CAP_TASK_ADJUST` lives" note. Phase 5a imports this symbol.
pub const CAP_TASK_ADJUST: u32 = 3;

/// The two phases of the task-level adjustment side-chain, in order.
///
/// Neither is in `standard.toml`'s phase lists — the side-chain is inserted
/// dynamically by the orchestrator when a `Fail` verdict arrives.
pub const SIDE_CHAIN: [&str; 2] = ["propose_adjustment", "review_adjustment"];

/// Whether `phase` is one of the side-chain phases ([`SIDE_CHAIN`]).
pub fn is_side_chain_phase(phase: &str) -> bool {
    SIDE_CHAIN.contains(&phase)
}

/// Where the orchestrator should route a task after a side-chain step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdjustmentRoute {
    /// Run this phase next — `propose_adjustment`, `review_adjustment`, or
    /// `execute` (the side-chain exit back into the work loop).
    RunPhase(String),
    /// Block the task — the side-chain cap was exceeded, or `review_adjustment`
    /// rejected the proposed fix. Carries the [`BlockedReason`] for the
    /// `TaskBlocked` event the orchestrator emits.
    Block(BlockedReason),
}

/// An adjustment-routing step failed.
///
/// The only failure mode is a repo-layer query (incrementing the counter,
/// reading the task's prior runs). Wrapping `RepoError` rather than re-exporting
/// it keeps the `service`-layer surface from leaking a `repo`-layer type.
#[derive(Debug, thiserror::Error)]
pub enum AdjustmentError {
    /// A repo-layer query failed while routing the side-chain.
    #[error("adjustment routing query failed: {0}")]
    Repo(#[from] RepoError),
}

/// A worker or validate phase returned `Verdict::Fail` — enter the side-chain.
///
/// This is the side-chain's *entry* edge. It routes straight through
/// `enter_side_chain`, so the very first failure both increments and
/// cap-checks the `TaskAdjust` counter.
pub async fn route_after_fail(
    pool: &SqlitePool,
    task_id: &TaskId,
) -> Result<AdjustmentRoute, AdjustmentError> {
    enter_side_chain(pool, task_id).await
}

/// `review_adjustment` finished — route on its verdict.
///
/// - `Passing` → `RunPhase("execute")`: the fix was approved, restart the work
///   loop. (This is the side-chain *exit* — it does NOT touch the counter.)
/// - `Redo` → re-enter the side-chain via `enter_side_chain`: this counts as
///   another `propose_adjustment` round and is cap-checked.
/// - `Blocked` / `Fail` → `Block(...)`: the reviewer rejected the fix outright.
pub async fn route_after_review_adjustment(
    pool: &SqlitePool,
    task_id: &TaskId,
    verdict: &WorkerVerdict,
) -> Result<AdjustmentRoute, AdjustmentError> {
    match &verdict.outcome {
        // Fix approved — leave the side-chain, restart the work loop.
        VerdictOutcome::Passing { .. } => Ok(AdjustmentRoute::RunPhase("execute".to_owned())),
        // Reviewer wants another round — re-enter the side-chain. This is the
        // edge the earlier split missed; routing it through `enter_side_chain`
        // is what bounds the `propose ↔ review` loop.
        VerdictOutcome::Redo { .. } => enter_side_chain(pool, task_id).await,
        // Reviewer rejected the fix — block the task.
        VerdictOutcome::Blocked {
            reason,
            error_why_fix,
        } => Ok(AdjustmentRoute::Block(BlockedReason::Manual {
            operator_note: Some(format!(
                "review_adjustment blocked the fix: {reason}{}",
                error_why_fix
                    .as_ref()
                    .map(|e| format!(" — {}", e.error))
                    .unwrap_or_default(),
            )),
        })),
        VerdictOutcome::Fail { error, why, fix } => {
            Ok(AdjustmentRoute::Block(BlockedReason::CapExceeded {
                loop_name: "task_adjust".to_owned(),
                cap: CAP_TASK_ADJUST,
                last_error_why_fix: ErrorWhyFix {
                    error: error.clone(),
                    why: why.clone(),
                    fix: fix.clone(),
                },
            }))
        }
        // Cancel verdict is written by the cancel path, never a worker verdict.
        // Treat as a terminal block so the task never stalls silently.
        VerdictOutcome::Canceled => Ok(AdjustmentRoute::Block(BlockedReason::Manual {
            operator_note: Some("phase canceled".to_owned()),
        })),
    }
}

/// PRIVATE — the one place the `TaskAdjust` counter is incremented and
/// cap-checked (review S2).
///
/// Both public routers call this on every edge that (re-)enters
/// `propose_adjustment`. It increments `iterations_task_adjust`, then:
///
/// - new count `> CAP_TASK_ADJUST` → `Block(CapExceeded)`;
/// - otherwise → `RunPhase("propose_adjustment")`.
///
/// The `CapExceeded` reason needs a concrete [`ErrorWhyFix`]; it is sourced from
/// the task's most recent `Fail`/`Blocked` phase run (the failure that drove the
/// side-chain). If none is on record a descriptive fallback is used — never a
/// silent empty triple.
async fn enter_side_chain(
    pool: &SqlitePool,
    task_id: &TaskId,
) -> Result<AdjustmentRoute, AdjustmentError> {
    let new_count =
        repo::task_runtime::increment_iteration(pool, task_id, IterationCounter::TaskAdjust)
            .await?;
    // `increment_iteration` returns an i64; the counter is small and
    // non-negative, so the cast is safe — clamp defensively rather than
    // panicking on the (impossible) negative.
    let new_count = u32::try_from(new_count).unwrap_or(u32::MAX);

    if new_count > CAP_TASK_ADJUST {
        let last = latest_failure_detail(pool, task_id).await?;
        Ok(AdjustmentRoute::Block(BlockedReason::CapExceeded {
            loop_name: "task_adjust".to_owned(),
            cap: CAP_TASK_ADJUST,
            last_error_why_fix: last,
        }))
    } else {
        Ok(AdjustmentRoute::RunPhase("propose_adjustment".to_owned()))
    }
}

/// The [`ErrorWhyFix`] of the task's most recent `Fail`/`Blocked` phase run.
///
/// Walks the task's phase-run history newest-first and returns the first
/// failure triple it finds. A `Fail` verdict always carries one; a `Blocked`
/// verdict carries an optional one. If no failure is on record — which should
/// not happen, since the side-chain is only entered after a failure — a
/// descriptive fallback triple is returned so [`BlockedReason::CapExceeded`] is
/// never built from an empty/silent value.
async fn latest_failure_detail(
    pool: &SqlitePool,
    task_id: &TaskId,
) -> Result<ErrorWhyFix, AdjustmentError> {
    let spec_id_str = repo::task_runtime::fetch(pool, task_id).await?.spec_id;
    // `spec_id` came straight out of a `task_runtime` row — it is well-formed;
    // a parse failure would mean DB corruption, surfaced loudly.
    let spec_id = crate::types::ids::SpecId::new(&spec_id_str)
        .map_err(|e| RepoError::NotFound(format!("corrupt spec id in task_runtime: {e}")))?;

    let mut history = repo::phase_runs::fetch_history_for_spec(pool, &spec_id).await?;
    // Newest first.
    history.reverse();
    for run in history {
        if run.task_id.as_deref() != Some(task_id.as_str()) {
            continue;
        }
        if let Some(verdict) = run.worker_verdict()? {
            match verdict.outcome {
                VerdictOutcome::Fail { error, why, fix } => {
                    return Ok(ErrorWhyFix { error, why, fix });
                }
                VerdictOutcome::Blocked {
                    error_why_fix: Some(ewf),
                    ..
                } => {
                    return Ok(ewf);
                }
                VerdictOutcome::Blocked { .. }
                | VerdictOutcome::Passing { .. }
                | VerdictOutcome::Redo { .. }
                | VerdictOutcome::Canceled => {}
            }
        }
    }
    // No failure on record — should be unreachable, but never silent.
    Ok(ErrorWhyFix {
        error: "task_adjust cap exceeded".to_owned(),
        why: format!(
            "the adjustment side-chain ran {CAP_TASK_ADJUST} rounds without resolving the failure"
        ),
        fix: "inspect the task's phase history and intervene manually".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::repo::db::connect;
    use crate::repo::phase_runs::{insert_start, update_end};
    use crate::repo::spec_versions::{VersionTrigger, append_version};
    use crate::repo::specs::insert_spec;
    use crate::repo::task_runtime::insert_task;
    use crate::types::ids::{PhaseRunId, SpecId};
    use crate::types::verdict::Evidence;

    fn spec() -> SpecId {
        SpecId::new("S0000001a").unwrap()
    }
    fn task() -> TaskId {
        TaskId::new("T0000001a").unwrap()
    }

    /// A pool with a spec (specs row + v1 snapshot) + one `not_started` task.
    /// The v1 snapshot satisfies the `phase_runs (spec_id, spec_version)` FK so
    /// a `phase_runs` row can be inserted.
    async fn seeded() -> SqlitePool {
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
        insert_task(&pool, &task(), &spec(), Some("setup"))
            .await
            .unwrap();
        pool
    }

    fn fail_verdict() -> WorkerVerdict {
        WorkerVerdict {
            synopsis: "build broke".into(),
            outcome: VerdictOutcome::Fail {
                error: "E0432".into(),
                why: "missing import".into(),
                fix: "add `use std::fmt;`".into(),
            },
        }
    }

    fn passing_verdict() -> WorkerVerdict {
        WorkerVerdict {
            synopsis: "fix is in scope".into(),
            outcome: VerdictOutcome::Passing {
                evidence: Evidence::default(),
            },
        }
    }

    fn redo_verdict() -> WorkerVerdict {
        WorkerVerdict {
            synopsis: "needs another pass".into(),
            outcome: VerdictOutcome::Redo {
                reason: "fix incomplete".into(),
            },
        }
    }

    /// Seed a completed `Fail` phase run for the task — gives
    /// `latest_failure_detail` something to find when the cap is hit.
    async fn record_fail_run(pool: &SqlitePool, iteration: u32) {
        let pr = PhaseRunId::new(format!("P000000{iteration}a")).unwrap();
        insert_start(
            pool,
            &pr,
            &spec(),
            Some(&task()),
            "execute",
            iteration,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        update_end(
            pool,
            &pr,
            "build broke",
            &fail_verdict(),
            &[],
            0,
            0,
            Utc::now(),
        )
        .await
        .unwrap();
    }

    /// `is_side_chain_phase` recognises exactly the two side-chain phases.
    #[test]
    fn test_l1_is_side_chain_phase() {
        assert!(is_side_chain_phase("propose_adjustment"));
        assert!(is_side_chain_phase("review_adjustment"));
        assert!(!is_side_chain_phase("execute"));
        assert!(!is_side_chain_phase("validate"));
        assert_eq!(SIDE_CHAIN.len(), 2);
    }

    /// The cap constant is 3 (design §4 default).
    #[test]
    fn test_l1_cap_is_three() {
        assert_eq!(CAP_TASK_ADJUST, 3);
    }

    /// First `Fail`: `route_after_fail` enters the side-chain — routes to
    /// `propose_adjustment` and `TaskAdjust` is now 1.
    #[tokio::test]
    async fn test_l2_route_after_fail_first_failure_enters_side_chain() {
        let pool = seeded().await;
        let route = route_after_fail(&pool, &task()).await.unwrap();
        assert_eq!(
            route,
            AdjustmentRoute::RunPhase("propose_adjustment".to_owned()),
        );
        let row = repo::task_runtime::fetch(&pool, &task()).await.unwrap();
        assert_eq!(row.iterations_task_adjust, 1, "the Fail edge counted");
    }

    /// A `Redo` on `review_adjustment` ALSO increments `TaskAdjust` and
    /// re-enters `propose_adjustment` — the edge the earlier split missed.
    #[tokio::test]
    async fn test_l2_redo_on_review_adjustment_increments_and_re_enters() {
        let pool = seeded().await;
        // First failure → round 1.
        route_after_fail(&pool, &task()).await.unwrap();
        // A Redo on review_adjustment → round 2.
        let route = route_after_review_adjustment(&pool, &task(), &redo_verdict())
            .await
            .unwrap();
        assert_eq!(
            route,
            AdjustmentRoute::RunPhase("propose_adjustment".to_owned()),
        );
        let row = repo::task_runtime::fetch(&pool, &task()).await.unwrap();
        assert_eq!(
            row.iterations_task_adjust, 2,
            "the Redo edge counted as another round",
        );
    }

    /// The regression test for the uncapped-loop bug: repeated `Redo`s on
    /// `review_adjustment` hit `Block(CapExceeded)` once the cap is crossed —
    /// no edge slips the bound.
    #[tokio::test]
    async fn test_l2_repeated_redo_hits_cap_exceeded() {
        let pool = seeded().await;
        record_fail_run(&pool, 0).await; // a failure for the cap reason to cite

        // Round 1 (Fail) + rounds 2,3 (Redo) — all at/under the cap.
        route_after_fail(&pool, &task()).await.unwrap();
        for _ in 0..2 {
            let route = route_after_review_adjustment(&pool, &task(), &redo_verdict())
                .await
                .unwrap();
            assert!(
                matches!(route, AdjustmentRoute::RunPhase(ref p) if p == "propose_adjustment"),
                "rounds 2-3 stay in the side-chain, got {route:?}",
            );
        }
        // The 4th round crosses CAP_TASK_ADJUST (3) → Block.
        let route = route_after_review_adjustment(&pool, &task(), &redo_verdict())
            .await
            .unwrap();
        let AdjustmentRoute::Block(BlockedReason::CapExceeded {
            loop_name,
            cap,
            last_error_why_fix,
        }) = route
        else {
            unreachable!("the 4th propose_adjustment round must Block(CapExceeded), got {route:?}");
        };
        assert_eq!(loop_name, "task_adjust");
        assert_eq!(cap, CAP_TASK_ADJUST);
        // The cap reason cites the real failure, not an empty triple.
        assert_eq!(last_error_why_fix.error, "E0432");

        // The counter stopped at 4 — the over-cap round still incremented once,
        // then blocked; no further edge runs.
        let row = repo::task_runtime::fetch(&pool, &task()).await.unwrap();
        assert_eq!(row.iterations_task_adjust, 4);
    }

    /// `route_after_review_adjustment` maps a `Passing` verdict to
    /// `RunPhase("execute")` — the side-chain exit — and does NOT touch the
    /// counter.
    #[tokio::test]
    async fn test_l2_review_adjustment_passing_routes_to_execute() {
        let pool = seeded().await;
        route_after_fail(&pool, &task()).await.unwrap(); // counter now 1

        let route = route_after_review_adjustment(&pool, &task(), &passing_verdict())
            .await
            .unwrap();
        assert_eq!(route, AdjustmentRoute::RunPhase("execute".to_owned()));
        // The exit edge left the counter alone.
        let row = repo::task_runtime::fetch(&pool, &task()).await.unwrap();
        assert_eq!(row.iterations_task_adjust, 1, "the exit edge did not count");
    }

    /// `route_after_review_adjustment` maps a `Blocked` verdict to
    /// `Block(...)` — the reviewer rejected the fix.
    #[tokio::test]
    async fn test_l2_review_adjustment_blocked_routes_to_block() {
        let pool = seeded().await;
        let blocked = WorkerVerdict {
            synopsis: "fix is out of scope".into(),
            outcome: VerdictOutcome::Blocked {
                reason: "fix touches unrelated files".into(),
                error_why_fix: None,
            },
        };
        let route = route_after_review_adjustment(&pool, &task(), &blocked)
            .await
            .unwrap();
        assert!(
            matches!(route, AdjustmentRoute::Block(_)),
            "a Blocked review verdict must Block the task, got {route:?}",
        );
        // No counter change — Block is not a side-chain re-entry.
        let row = repo::task_runtime::fetch(&pool, &task()).await.unwrap();
        assert_eq!(row.iterations_task_adjust, 0);
    }
}
