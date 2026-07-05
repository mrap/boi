//! Failure-path L3 coverage (Task 10.4).
//!
//! Per §13.3 (Lec 10) every *producible* `BlockedReason` / `FailureReason`
//! gets a producing L3 test, each naming its producer. The 11 variants and
//! their producers:
//!
//! | reason | kind | producer |
//! |---|---|---|
//! | `CapExceeded` | Blocked | the side-chain iteration cap (routing) |
//! | `MergeConflict` | Blocked | `merge_to_integration` Fail (deterministic) |
//! | `WorkspaceUnclean` | Blocked | `workspace_verify_in` Fail (deterministic) |
//! | `ProviderFailed` | Blocked | a verdict-less drain (`handle_drain_terminated`) |
//! | `PlanRevisionPending` | Blocked | `on_report` (a blocking `task_report`) |
//! | `Manual` | Blocked | a worker phase returning a `Blocked` verdict |
//! | `PreflightFailed` | Failure | the orchestrator `Halt` path (a spec phase Fail) |
//! | `DaemonCrash` | Failure | `recover_after_crash` (the restart sweep) |
//! | `SpecReviewExhausted` | Failure | the spec-`review` cap (`route_spec`) |
//! | `OperatorMarkedFailed` | Failure | `boi fail` (`DaemonState::handle_fail`) |
//!
//! **`AwaitingDeps` is the one documented exemption** (G16.6) — no v1.0
//! component produces `TaskBlocked{AwaitingDeps}` (the scheduler enforces
//! dep-readiness by *not spawning*, never by emitting a block). The §13.3
//! gate excludes it explicitly; `scripts/checks/test-coverage.sh` records the
//! exemption.

use std::collections::HashMap;
use std::sync::Arc;

use boi::config::PhaseDef;
use boi::repo;
use boi::service::registry::PhaseExecutor;
use boi::service::registry::testkit::{MockExecutor, ScriptedEvent};
use boi::types::context::PhaseContext;
use boi::types::event::BoiEvent;
use boi::types::reasons::{BlockedReason, FailureReason};
use boi::types::verdict::{Evidence, VerdictOutcome, WorkerVerdict};
use futures::stream::{self, BoxStream, StreamExt};
use tokio_util::sync::CancellationToken;

use super::harness::{
    CallIndexedExecutor, fixture_spec, run_spec, run_spec_until_task_blocked, seed_dispatched_spec,
};

/// Fetch the first task's `BlockedReason` from a settled run — panics if the
/// task is not `blocked` or carries no reason.
async fn first_task_blocked_reason(run: &super::harness::L3Run) -> BlockedReason {
    let row = repo::task_runtime::fetch(&run.pool, &run.dispatched.task_ids[0])
        .await
        .expect("task_runtime row exists");
    assert_eq!(row.state, "blocked", "the task must be blocked");
    serde_json::from_value(
        row.blocked_reason
            .expect("a blocked task carries a typed reason"),
    )
    .expect("the blocked reason deserializes")
}

/// The spec's `FailureReason` from a settled run — panics if the spec is not
/// `failed` or carries no reason.
async fn spec_failure_reason(run: &super::harness::L3Run) -> FailureReason {
    let row = repo::spec_runtime::fetch(&run.pool, run.spec_id())
        .await
        .expect("spec_runtime row exists");
    assert_eq!(row.status, "failed", "the spec must be failed");
    serde_json::from_value(
        row.failure_reason
            .expect("a failed spec carries a typed reason"),
    )
    .expect("the failure reason deserializes")
}

// ===========================================================================
// BlockedReason variants
// ===========================================================================

/// `CapExceeded` — producer: the side-chain iteration cap (routing, 5a.4).
///
/// `execute` fails on every call and `review_adjustment` always `Redo`s, so
/// the adjustment side-chain re-enters `propose_adjustment` until
/// `CAP_TASK_ADJUST` is crossed and the task ends `blocked{CapExceeded}`.
#[tokio::test]
async fn test_l3_failures_cap_exceeded_from_the_side_chain() {
    let mut script = HashMap::new();
    script.insert(
        "execute".to_owned(),
        vec![VerdictOutcome::Fail {
            error: "build broke".into(),
            why: "still broken".into(),
            fix: "try again".into(),
        }],
    );
    script.insert(
        "review_adjustment".to_owned(),
        vec![VerdictOutcome::Redo {
            reason: "fix incomplete".into(),
        }],
    );
    let exec: Arc<dyn PhaseExecutor> = Arc::new(CallIndexedExecutor::new(script));
    let run = run_spec_until_task_blocked(fixture_spec("01-typo-fix"), exec).await;

    let reason = first_task_blocked_reason(&run).await;
    let BlockedReason::CapExceeded { loop_name, .. } = reason else {
        panic!("expected CapExceeded, got {reason:?}");
    };
    assert_eq!(
        loop_name, "task_adjust",
        "the tripped cap is the side-chain's"
    );
}

/// `MergeConflict` — producer: `merge_to_integration` (a deterministic phase)
/// returning a `Fail` verdict (6.2 — `deterministic_fail_reason`).
#[tokio::test]
async fn test_l3_failures_merge_conflict_from_merge_to_integration() {
    let mut script = HashMap::new();
    script.insert(
        "merge_to_integration".to_owned(),
        vec![VerdictOutcome::Fail {
            error: "merge conflict".into(),
            why: "branches diverged".into(),
            fix: "rebase and re-merge".into(),
        }],
    );
    let exec: Arc<dyn PhaseExecutor> = Arc::new(CallIndexedExecutor::new(script));
    let run = run_spec_until_task_blocked(fixture_spec("01-typo-fix"), exec).await;

    let reason = first_task_blocked_reason(&run).await;
    assert!(
        matches!(reason, BlockedReason::MergeConflict { .. }),
        "a merge_to_integration Fail must block with MergeConflict, got {reason:?}",
    );
}

/// `WorkspaceUnclean` — producer: `workspace_verify_in` (a deterministic
/// phase) returning a `Fail` verdict (6.2 — `deterministic_fail_reason`'s
/// non-`merge_to_integration` arm).
#[tokio::test]
async fn test_l3_failures_workspace_unclean_from_workspace_verify_in() {
    let mut script = HashMap::new();
    script.insert(
        "workspace_verify_in".to_owned(),
        vec![VerdictOutcome::Fail {
            error: "dirty tree".into(),
            why: "uncommitted changes present".into(),
            fix: "stash or commit".into(),
        }],
    );
    let exec: Arc<dyn PhaseExecutor> = Arc::new(CallIndexedExecutor::new(script));
    let run = run_spec_until_task_blocked(fixture_spec("01-typo-fix"), exec).await;

    let reason = first_task_blocked_reason(&run).await;
    assert!(
        matches!(reason, BlockedReason::WorkspaceUnclean { .. }),
        "a workspace_verify_in Fail must block with WorkspaceUnclean, got {reason:?}",
    );
}

/// `ProviderFailed` — producer: a verdict-less drain. A worker phase whose
/// executor stream ends *clean* with no terminal `PhaseCompleted` →
/// `DrainStatus::CompletedWithoutVerdict` → `handle_drain_terminated` surfaces
/// `TaskBlocked{ProviderFailed}`.
///
/// (The plan also names the sweeper, 5a.5, as a `ProviderFailed` producer —
/// it produces the same variant from a stale heartbeat. Both are producers;
/// the verdict-less drain is the one an L3 test can drive hermetically.)
#[tokio::test]
async fn test_l3_failures_provider_failed_from_a_verdict_less_drain() {
    // `workspace_verify_in` is scripted with an explicitly EMPTY step list —
    // `MockExecutor` then yields an empty stream, the drain runs clean but
    // relays no `PhaseCompleted`.
    let mut script = HashMap::new();
    script.insert(
        "workspace_verify_in".to_owned(),
        Vec::<ScriptedEvent>::new(),
    );
    let exec: Arc<dyn PhaseExecutor> = Arc::new(MockExecutor::new(script));
    let run = run_spec_until_task_blocked(fixture_spec("01-typo-fix"), exec).await;

    let reason = first_task_blocked_reason(&run).await;
    assert!(
        matches!(reason, BlockedReason::ProviderFailed { .. }),
        "a verdict-less drain must block with ProviderFailed, got {reason:?}",
    );
}

/// `Manual` — producer: a *worker* phase returning a `Blocked` verdict
/// (`route_task`'s `worker_blocked_reason`). `BlockedReason` has no dedicated
/// worker-self-block variant, so a worker `Blocked` verdict maps to `Manual`
/// carrying the worker's free-text reason.
#[tokio::test]
async fn test_l3_failures_manual_from_a_worker_blocked_verdict() {
    let mut script = HashMap::new();
    // `execute` is a worker phase — a `Blocked` verdict from it.
    script.insert(
        "execute".to_owned(),
        vec![VerdictOutcome::Blocked {
            reason: "the worker cannot proceed without operator input".into(),
            error_why_fix: None,
        }],
    );
    let exec: Arc<dyn PhaseExecutor> = Arc::new(CallIndexedExecutor::new(script));
    let run = run_spec_until_task_blocked(fixture_spec("01-typo-fix"), exec).await;

    let reason = first_task_blocked_reason(&run).await;
    assert!(
        matches!(reason, BlockedReason::Manual { .. }),
        "a worker Blocked verdict must block with Manual, got {reason:?}",
    );
}

/// A [`PhaseExecutor`] for the `PlanRevisionPending` test: the first `execute`
/// call files a blocking `task_report` then parks on cancel; the
/// `plan_revision` phase then FAILS (a non-`Passing` verdict) so the
/// triggering task stays `blocked{PlanRevisionPending}`; every other phase
/// passes.
struct ReportThenFailRevisionExecutor {
    execute_calls: Arc<std::sync::Mutex<usize>>,
}

impl ReportThenFailRevisionExecutor {
    fn new() -> Self {
        Self {
            execute_calls: Arc::new(std::sync::Mutex::new(0)),
        }
    }

    fn completion(ctx: &PhaseContext, outcome: VerdictOutcome) -> BoiEvent {
        BoiEvent::PhaseCompleted {
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
        }
    }
}

impl PhaseExecutor for ReportThenFailRevisionExecutor {
    fn execute(
        &self,
        phase: PhaseDef,
        ctx: PhaseContext,
        cancel: CancellationToken,
    ) -> BoxStream<'static, BoiEvent> {
        match phase.name.as_str() {
            // The `plan_revision` phase FAILS — the triggering task stays
            // `blocked{PlanRevisionPending}` (the orchestrator's
            // `on_plan_revision_completed` fails the spec, never unblocks).
            "plan_revision" => stream::iter(vec![Self::completion(
                &ctx,
                VerdictOutcome::Fail {
                    error: "cannot revise".into(),
                    why: "the report is contradictory".into(),
                    fix: "operator must intervene".into(),
                },
            )])
            .boxed(),
            "execute" => {
                let first = {
                    let mut n = self
                        .execute_calls
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let first = *n == 0;
                    *n += 1;
                    first
                };
                if !first {
                    return stream::iter(vec![Self::completion(
                        &ctx,
                        VerdictOutcome::Passing {
                            evidence: Evidence::default(),
                        },
                    )])
                    .boxed();
                }
                // First execute — file the blocking report, then park on cancel.
                let task_id = ctx.task_id.expect("execute is a task phase");
                let report = BoiEvent::ReportReceived {
                    spec_id: ctx.spec_id,
                    task_id,
                    kind: "scope_gap".to_owned(),
                    payload: serde_json::json!({ "detail": "a prerequisite is needed" }),
                    blocking: true,
                };
                let park = stream::once(async move {
                    cancel.cancelled().await;
                    None
                })
                .filter_map(|x| async move { x });
                stream::iter(vec![report]).chain(park).boxed()
            }
            _ => stream::iter(vec![Self::completion(
                &ctx,
                VerdictOutcome::Passing {
                    evidence: Evidence::default(),
                },
            )])
            .boxed(),
        }
    }
}

/// `PlanRevisionPending` — producer: `on_report` (5b.2) for a blocking
/// `task_report`. The reporting task is blocked with
/// `PlanRevisionPending`; here the subsequent `plan_revision` worker fails,
/// so the task STAYS `blocked{PlanRevisionPending}` while the spec fails.
#[tokio::test]
async fn test_l3_failures_plan_revision_pending_from_on_report() {
    let exec: Arc<dyn PhaseExecutor> = Arc::new(ReportThenFailRevisionExecutor::new());
    let run = run_spec(fixture_spec("01-typo-fix"), exec).await;

    // The reporting task is blocked with PlanRevisionPending — the producer is
    // `on_report`, which blocked it the moment the blocking report arrived.
    let reason = first_task_blocked_reason(&run).await;
    assert!(
        matches!(reason, BlockedReason::PlanRevisionPending { .. }),
        "a blocking task_report must block the task with PlanRevisionPending, got {reason:?}",
    );
    // N3: the failed plan_revision must fail the spec — a future regression where
    // the spec hangs in `running` would be caught here rather than waiting for the
    // run timeout.
    assert_eq!(
        run.spec_status().await,
        "failed",
        "a failed plan_revision must fail the spec (fail_plan_revision path)",
    );
}

// ===========================================================================
// FailureReason variants
// ===========================================================================

/// `PreflightFailed` — producer: the orchestrator's `Halt` path. A spec-level
/// worker phase (`plan`) whose `[on.fail]` carries no onward `next` returns a
/// `Fail` verdict → `route_spec` → `SpecRoute::Halt` → `on_spec_phase_completed`
/// fails the spec `failed{PreflightFailed}`.
///
/// (`PreflightFailed` has several producers — `boi dispatch`'s preflight gate,
/// every orchestration `on_fault`, a verdict-less spec-phase drain. The
/// spec-phase `Halt` is the one an L3 test drives directly.)
#[tokio::test]
async fn test_l3_failures_preflight_failed_from_a_spec_phase_halt() {
    let mut script = HashMap::new();
    // `plan` is a spec-level phase; its `[on.fail]` has NO `next` → Halt.
    script.insert(
        "plan".to_owned(),
        vec![VerdictOutcome::Fail {
            error: "cannot plan".into(),
            why: "the contract is contradictory".into(),
            fix: "revise the spec".into(),
        }],
    );
    let exec: Arc<dyn PhaseExecutor> = Arc::new(CallIndexedExecutor::new(script));
    let run = run_spec(fixture_spec("01-typo-fix"), exec).await;

    let reason = spec_failure_reason(&run).await;
    assert!(
        matches!(reason, FailureReason::PreflightFailed { .. }),
        "a spec-phase Halt must fail the spec with PreflightFailed, got {reason:?}",
    );
}

/// `DaemonCrash` — producer: `recover_after_crash` (the G16.5 restart sweep).
///
/// Seeds the exact state a crashed daemon leaves — a spec in `running` status
/// plus an open `phase_runs` row — then drives `recover_after_crash`, the
/// isolated step `boot` runs before spawning the orchestrator. It is the only
/// v1.0 producer of `FailureReason::DaemonCrash`.
#[tokio::test]
async fn test_l3_failures_daemon_crash_from_recover_after_crash() {
    let seeded = seed_dispatched_spec("01-typo-fix").await;
    // Drive the spec to `running` — a crashed daemon left it mid-run.
    repo::spec_runtime::update_status(
        &seeded.pool,
        &seeded.spec_id,
        boi::types::state::SpecStatus::Running,
        None,
        chrono::Utc::now(),
    )
    .await
    .expect("queued → running");
    // An open `phase_runs` row — a crashed worker's phase run.
    let pr = boi::types::ids::PhaseRunId::new("P0000001a").expect("valid phase-run id");
    repo::phase_runs::insert_start(
        &seeded.pool,
        &pr,
        &seeded.spec_id,
        None,
        "plan",
        0,
        1,
        "claude_code",
        None,
        chrono::Utc::now(),
    )
    .await
    .expect("insert an open phase run");

    // The bus the recovery pass emits through.
    let bus = boi::service::EventBus::new(
        seeded.pool.clone(),
        vec![Arc::new(boi::service::NoopObserver)],
    );
    boi::cli::boot::recover_after_crash(&bus, &seeded.pool)
        .await
        .expect("recover_after_crash succeeds");

    // The crashed `running` spec is now `failed{DaemonCrash}`.
    let row = repo::spec_runtime::fetch(&seeded.pool, &seeded.spec_id)
        .await
        .expect("spec_runtime row");
    assert_eq!(row.status, "failed", "the crashed spec is failed");
    let reason: FailureReason = serde_json::from_value(
        row.failure_reason
            .expect("a failed spec carries a typed reason"),
    )
    .expect("the failure reason deserializes");
    assert!(
        matches!(reason, FailureReason::DaemonCrash),
        "recover_after_crash must fail a crashed-daemon spec with DaemonCrash, got {reason:?}",
    );
}

/// A [`PhaseExecutor`] for the `SpecReviewExhausted` test: the SPEC-level
/// `review` phase (`ctx.task_id` is `None`) always returns `Redo`; every other
/// phase — including the task-level `review` (`ctx.task_id` is `Some`) —
/// passes. The spec-`review` Redo loop trips `CAP_SPEC_REVIEW`.
struct SpecReviewRedoExecutor {
    inner: MockExecutor,
}

impl SpecReviewRedoExecutor {
    fn new() -> Self {
        Self {
            inner: MockExecutor::all_passing(),
        }
    }
}

impl PhaseExecutor for SpecReviewRedoExecutor {
    fn execute(
        &self,
        phase: PhaseDef,
        ctx: PhaseContext,
        cancel: CancellationToken,
    ) -> BoxStream<'static, BoiEvent> {
        // The SPEC-level `review` (no task) always Redos; everything else
        // (incl. the task-level `review`) passes via the all-passing mock.
        // `ctx` is consumed here — this branch `return`s, so its fields move.
        if phase.name == "review" && ctx.task_id.is_none() {
            let redo = BoiEvent::PhaseCompleted {
                phase_run_id: ctx.phase_run_id,
                spec_id: ctx.spec_id,
                task_id: None,
                phase: ctx.phase,
                verdict: WorkerVerdict {
                    synopsis: "the spec review still finds gaps".to_owned(),
                    outcome: VerdictOutcome::Redo {
                        reason: "spec-level review not satisfied".into(),
                    },
                },
                tokens_in: 0,
                tokens_out: 0,
                duration_ms: 0,
            };
            return stream::iter(vec![redo]).boxed();
        }
        self.inner.execute(phase, ctx, cancel)
    }
}

/// `SpecReviewExhausted` — producer: the spec-level `review` iteration cap in
/// `route_spec`. The spec-`review` phase keeps returning `Redo`; `route_spec`
/// increments the `SpecReview` counter each time and, once `CAP_SPEC_REVIEW`
/// is crossed, fails the spec `failed{SpecReviewExhausted}`.
#[tokio::test]
async fn test_l3_failures_spec_review_exhausted_from_the_spec_review_cap() {
    let exec: Arc<dyn PhaseExecutor> = Arc::new(SpecReviewRedoExecutor::new());
    let run = run_spec(fixture_spec("01-typo-fix"), exec).await;

    let reason = spec_failure_reason(&run).await;
    assert!(
        matches!(reason, FailureReason::SpecReviewExhausted { .. }),
        "an unconverging spec-review loop must fail with SpecReviewExhausted, got {reason:?}",
    );
}

/// `OperatorMarkedFailed` — producer: `boi fail` (`DaemonState::handle_fail`,
/// G16.6). The operator command emits `SpecFailed{OperatorMarkedFailed}`.
#[tokio::test]
async fn test_l3_failures_operator_marked_failed_from_boi_fail() {
    use boi::service::{DaemonCommand, DaemonResponse};

    let seeded = seed_dispatched_spec("01-typo-fix").await;
    // `boi fail` fails a `running` spec — drive it to `running` first.
    repo::spec_runtime::update_status(
        &seeded.pool,
        &seeded.spec_id,
        boi::types::state::SpecStatus::Running,
        None,
        chrono::Utc::now(),
    )
    .await
    .expect("queued → running");

    // Build the daemon's control-socket command handler over the seeded DB.
    let bus = Arc::new(boi::service::EventBus::new(
        seeded.pool.clone(),
        vec![Arc::new(boi::service::NoopObserver)],
    ));
    let (daemon_tx, _daemon_rx) = tokio::sync::mpsc::channel(64);
    let state = boi::cli::daemon::DaemonState::new(
        bus,
        seeded.pool.clone(),
        daemon_tx,
        HashMap::new(),
        std::path::PathBuf::from("goose"),
        std::path::PathBuf::from("/tmp/recipes"),
        // Never consulted by `boi fail` — only `Dispatch` runs preflight.
        Arc::new(boi::runtime::CurlProviderProbe::new()),
    );

    // `boi fail <spec> --reason` → SpecFailed{OperatorMarkedFailed}.
    let resp = boi::cli::control::CommandHandler::handle(
        &state,
        DaemonCommand::Fail {
            spec_id: seeded.spec_id.as_str().to_owned(),
            reason: "abandoned by the operator".to_owned(),
        },
    )
    .await;
    assert!(
        matches!(resp, DaemonResponse::Ok { .. }),
        "boi fail must succeed, got {resp:?}",
    );

    let row = repo::spec_runtime::fetch(&seeded.pool, &seeded.spec_id)
        .await
        .expect("spec_runtime row");
    assert_eq!(row.status, "failed", "boi fail fails the spec");
    let reason: FailureReason = serde_json::from_value(
        row.failure_reason
            .expect("a failed spec carries a typed reason"),
    )
    .expect("the failure reason deserializes");
    assert!(
        matches!(reason, FailureReason::OperatorMarkedFailed { .. }),
        "boi fail must fail the spec with OperatorMarkedFailed, got {reason:?}",
    );
}

// ===========================================================================
// AwaitingDeps — the documented §13.3 exemption (G16.6)
// ===========================================================================

/// `AwaitingDeps` is the ONE documented exemption from the §13.3
/// producing-test gate (G16.6). No v1.0 component emits
/// `TaskBlocked{AwaitingDeps}` — the scheduler enforces dependency readiness
/// by *not spawning* a task whose `blocked_by` predecessors are unfinished,
/// never by emitting a block. The variant is reserved for a future explicit
/// dep-block flow.
///
/// This test does not *produce* `AwaitingDeps` (nothing can) — it pins the
/// exemption: the variant constructs and round-trips, so the type stays valid
/// while no producer exists. `scripts/checks/test-coverage.sh` records the
/// exemption so the gate does not demand a producing test for it.
#[test]
fn test_l3_failures_awaiting_deps_is_a_documented_exemption() {
    // The variant is well-formed and serde-round-trips — it is reserved, not
    // removed. No v1.0 producer exists (the §13.3 G16.6 exemption).
    let reason = BlockedReason::AwaitingDeps {
        unmet_deps: vec![boi::types::ids::TaskId::new("T0000001a").expect("valid task id")],
    };
    let json = serde_json::to_value(&reason).expect("serializes");
    let back: BlockedReason = serde_json::from_value(json).expect("round-trips");
    assert!(matches!(back, BlockedReason::AwaitingDeps { .. }));
}
