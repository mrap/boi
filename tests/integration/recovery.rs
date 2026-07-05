//! Operator-recovery L3 coverage (audit A2, 2026-06-10 — design §6 recovery
//! table).
//!
//! Design §6 documents `boi unblock <task_id> [--reset-counter]` as the
//! operator's revive path for a blocked task. There are two distinct block
//! classes, and BOTH must complete the loop end-to-end:
//!
//! | class | producer | resume arm of `on_task_unblocked` |
//! |---|---|---|
//! | verdict-routed block (`CapExceeded`, `MergeConflict`, worker self-block) | `route_task` → `TaskAction::TaskBlocked` — every `phase_runs` row CLOSED | `None` arm — restart at the first task phase |
//! | drain-failure block (`ProviderFailed`) | `handle_drain_terminated` — the dead run's row left OPEN | `Some` arm — re-run the open phase |
//!
//! Before the A2 fix the verdict-routed class was bricked AT BLOCK TIME:
//! `on_task_phase_completed` emitted `SpecFailed` on every routed block, and
//! `failed` is terminal with no exit edge — so `boi unblock --reset-counter`
//! (whose `daemon.rs` comment names the `CapExceeded` revive as its designed
//! primary use) could never complete the spec. These tests drive the REAL
//! run-loop (the same harness tier as `fixtures.rs` / `failures.rs`) through
//! both classes: block → spec still `running` → unblock → spec `completed`.

use std::collections::HashMap;
use std::sync::Arc;

use boi::repo;
use boi::service::registry::PhaseExecutor;
use boi::types::event::BoiEvent;
use boi::types::reasons::BlockedReason;
use boi::types::verdict::{Evidence, VerdictOutcome, WorkerVerdict};
use futures::stream::{self, BoxStream, StreamExt};
use tokio_util::sync::CancellationToken;

use super::harness::{CallIndexedExecutor, LiveL3Run, fixture_spec};

/// The first task's `BlockedReason` — panics if the task is not `blocked`.
async fn first_task_blocked_reason(live: &LiveL3Run) -> BlockedReason {
    let row = repo::task_runtime::fetch(&live.pool, &live.dispatched.task_ids[0])
        .await
        .expect("task_runtime row exists");
    assert_eq!(row.state, "blocked", "the task must be blocked");
    serde_json::from_value(
        row.blocked_reason
            .expect("a blocked task carries a typed reason"),
    )
    .expect("the blocked reason deserializes")
}

/// The spec's current `spec_runtime.status`.
async fn spec_status(live: &LiveL3Run) -> String {
    repo::spec_runtime::fetch(&live.pool, &live.dispatched.spec_id)
        .await
        .expect("spec_runtime row exists")
        .status
}

/// AUDIT A2 — the headline operator-recovery loop, verdict-routed class.
///
/// `execute` fails once → the adjustment side-chain loops (`review_adjustment`
/// always `Redo`s) until `CAP_TASK_ADJUST` trips → the task ends
/// `blocked{CapExceeded}` with every `phase_runs` row closed. The spec must
/// be left REVIVABLE — `running`, never `failed` — so that
/// `boi unblock --reset-counter` (mirrored exactly by [`LiveL3Run::unblock`])
/// restarts the task at the first task phase (`on_task_unblocked`'s `None`
/// arm), the task completes, and the spec completes.
///
/// RED gate: before the fix, the routed block emitted `SpecFailed` — the
/// `running` assertion below fails with `failed`, and the revive could never
/// complete (terminal `failed` has no exit edge).
#[tokio::test]
async fn test_l3_orchestrator_unblock_after_cap_exceeded_revives_and_completes_the_spec() {
    let mut script = HashMap::new();
    // Call-indexed: the FIRST `execute` call fails (entering the side-chain);
    // the SECOND — the post-unblock re-entry — passes.
    script.insert(
        "execute".to_owned(),
        vec![
            VerdictOutcome::Fail {
                error: "build broke".into(),
                why: "still broken".into(),
                fix: "try again".into(),
            },
            VerdictOutcome::Passing {
                evidence: Evidence::default(),
            },
        ],
    );
    // `review_adjustment` ALWAYS `Redo`s — the side-chain loops to its cap.
    script.insert(
        "review_adjustment".to_owned(),
        vec![VerdictOutcome::Redo {
            reason: "fix incomplete".into(),
        }],
    );
    let exec: Arc<dyn PhaseExecutor> = Arc::new(CallIndexedExecutor::new(script));
    let live = LiveL3Run::start(fixture_spec("01-typo-fix"), exec).await;

    // (1) — the task blocks at the cap.
    assert!(
        live.wait_task_blocked(0).await,
        "the task must reach blocked{{CapExceeded}}",
    );
    let reason = first_task_blocked_reason(&live).await;
    assert!(
        matches!(reason, BlockedReason::CapExceeded { .. }),
        "the block is the side-chain cap, got {reason:?}",
    );

    // (2) — THE A2 POLICY: the spec is still `running` (design §6 — a blocked
    // task is operator-recoverable; the spec must stay revivable). A beat of
    // settling time so a SpecFailed cascade — the pre-fix behavior this test
    // pins out — would have landed before the assertion.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        spec_status(&live).await,
        "running",
        "a verdict-routed block must leave the spec revivable — `failed` is \
         terminal with no exit edge and bricks `boi unblock` (audit A2)",
    );

    // (3) — the operator revives: `boi unblock <task> --reset-counter`.
    live.unblock(0, true).await;

    // (4) — the task re-enters at the first task phase, completes, and the
    // spec completes.
    assert!(
        live.wait_spec_terminal().await,
        "the revived spec must reach a terminal state",
    );
    let run = live.settle();
    assert_eq!(
        run.spec_status().await,
        "completed",
        "the revived spec must complete — the documented §6 recovery loop",
    );
    assert_eq!(run.task_state(0).await, "passing", "the task passes");

    // (5) — the `None`-arm restart re-entered at the FIRST task phase: a
    // second `workspace_verify_in` run exists for the task.
    let history = repo::phase_runs::fetch_history_for_spec(&run.pool, run.spec_id())
        .await
        .expect("phase history");
    let verify_in_runs = history
        .iter()
        .filter(|r| {
            r.phase == "workspace_verify_in"
                && r.task_id.as_deref() == Some(run.dispatched.task_ids[0].as_str())
        })
        .count();
    assert!(
        verify_in_runs >= 2,
        "the unblock restart must re-enter at workspace_verify_in \
         (got {verify_in_runs} run(s))",
    );
}

/// A [`PhaseExecutor`] whose FIRST `workspace_verify_in` call yields a
/// verdict-less EMPTY stream — the drain ends `CompletedWithoutVerdict`, the
/// orchestrator surfaces `TaskBlocked{ProviderFailed}`, and the dead run's
/// `phase_runs` row is left OPEN (the drain-failure block class). Every other
/// call (including the post-unblock `workspace_verify_in` re-run) passes.
struct VerdictlessFirstVerifyIn {
    verify_in_calls: Arc<std::sync::Mutex<usize>>,
}

impl VerdictlessFirstVerifyIn {
    fn new() -> Self {
        Self {
            verify_in_calls: Arc::new(std::sync::Mutex::new(0)),
        }
    }
}

impl PhaseExecutor for VerdictlessFirstVerifyIn {
    fn execute(
        &self,
        phase: boi::config::PhaseDef,
        ctx: boi::types::context::PhaseContext,
        _cancel: CancellationToken,
    ) -> BoxStream<'static, BoiEvent> {
        if phase.name == "workspace_verify_in" {
            let first = {
                let mut n = self
                    .verify_in_calls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let first = *n == 0;
                *n += 1;
                first
            };
            if first {
                // An empty stream — the drain relays no PhaseCompleted.
                return stream::iter(Vec::<BoiEvent>::new()).boxed();
            }
        }
        let completed = BoiEvent::PhaseCompleted {
            phase_run_id: ctx.phase_run_id.clone(),
            spec_id: ctx.spec_id.clone(),
            task_id: ctx.task_id.clone(),
            phase: ctx.phase.clone(),
            verdict: WorkerVerdict {
                synopsis: format!("scripted {}", ctx.phase),
                outcome: VerdictOutcome::Passing {
                    evidence: Evidence::default(),
                },
            },
            tokens_in: 0,
            tokens_out: 0,
            duration_ms: 0,
        };
        stream::iter(vec![completed]).boxed()
    }
}

/// The drain-failure class (audit A2 scope note): a block whose `phase_runs`
/// row is left OPEN resumes via `on_task_unblocked`'s `Some` arm — re-running
/// the open phase — and completes the spec. This class worked before the A2
/// fix (no `SpecFailed` is emitted on the drain-failure path); the test pins
/// it so the recovery loop's BOTH arms hold and stay held.
#[tokio::test]
async fn test_l3_orchestrator_unblock_after_drain_failure_resumes_the_open_phase() {
    let exec: Arc<dyn PhaseExecutor> = Arc::new(VerdictlessFirstVerifyIn::new());
    let live = LiveL3Run::start(fixture_spec("01-typo-fix"), exec).await;

    // (1) — the verdict-less drain blocks the task; the row stays open.
    assert!(
        live.wait_task_blocked(0).await,
        "the verdict-less drain must block the task",
    );
    let reason = first_task_blocked_reason(&live).await;
    assert!(
        matches!(reason, BlockedReason::ProviderFailed { .. }),
        "a verdict-less drain blocks with ProviderFailed, got {reason:?}",
    );
    let history = repo::phase_runs::fetch_history_for_spec(&live.pool, &live.dispatched.spec_id)
        .await
        .expect("phase history");
    assert!(
        history
            .iter()
            .any(|r| r.phase == "workspace_verify_in" && r.is_open()),
        "the dead run's workspace_verify_in row is left open (the Some-arm input)",
    );

    // (2) — the spec stays `running` (this class never failed the spec).
    assert_eq!(spec_status(&live).await, "running");

    // (3) — `boi unblock <task>` (no counter reset needed for this class).
    live.unblock(0, false).await;

    // (4) — the Some-arm re-runs the open phase; the spec completes.
    assert!(
        live.wait_spec_terminal().await,
        "the revived spec must reach a terminal state",
    );
    let run = live.settle();
    assert_eq!(
        run.spec_status().await,
        "completed",
        "unblock-after-drain-failure must resume the open phase and complete",
    );
    assert_eq!(run.task_state(0).await, "passing", "the task passes");
}
