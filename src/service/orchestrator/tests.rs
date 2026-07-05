//! Phase 5a.3 / 5a.3a orchestrator tests.
//!
//! The orchestrator is driven through the `testkit::MockExecutor` double — a
//! `PhaseExecutor` with zero `runtime/` dependency. Tests reach the private
//! `handle_event` / `handle_drain_terminated` / `run_phase` via the in-crate
//! [`drive`] helper, which runs the orchestrator to quiescence deterministically
//! (the full `run()` loop is exercised separately by the run-loop tests).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use serde_json::json;
use tokio::sync::mpsc;

use super::Orchestrator;
use crate::config::{PhaseDef, parse_phase, parse_pipeline};
use crate::repo;
use crate::repo::db::connect;
use crate::repo::spec_versions::{VersionTrigger, append_version};
use crate::repo::specs::insert_spec;
use crate::repo::task_runtime::insert_task;
use crate::service::bus::EventBus;
use crate::service::bus::testkit::RecordingObserver;
use crate::service::registry::testkit::{MockExecutor, ScriptedEvent};
use crate::service::registry::{DaemonNotification, PhaseExecutor};
use crate::service::routing::CAP_PLAN_CRITIQUE;
use crate::types::context::{SpecContract, TaskContract, Verification};
use crate::types::event::BoiEvent;
use crate::types::ids::{SpecId, TaskId};
use crate::types::verdict::{Evidence, VerdictOutcome, WorkerVerdict};

fn spec() -> SpecId {
    SpecId::new("S0000001a").unwrap()
}

/// Every `standard`-pipeline phase fixture, name → def.
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

fn standard_pipeline() -> crate::config::PipelineDef {
    let toml = std::fs::read_to_string(format!(
        "{}/tests/fixtures/pipelines/standard.toml",
        env!("CARGO_MANIFEST_DIR"),
    ))
    .unwrap();
    parse_pipeline(&toml).unwrap()
}

fn a_spec_contract() -> SpecContract {
    SpecContract {
        scope: "demo".into(),
        workspace: PathBuf::from("/repo"),
        base_branch: "main".into(),
        exclusions: vec![],
        verifications: vec![],
        must_emit: vec![],
    }
}

fn a_task_contract() -> TaskContract {
    TaskContract {
        behavior: "do the thing".into(),
        verifications: vec![Verification::Command {
            name: None,
            command: "cargo test".into(),
        }],
    }
}

/// Seed a pool with a spec whose v1 snapshot follows the contract-snapshot
/// convention (`spec_contract` + `task_contracts`), `spec_runtime` initialized,
/// and a `not_started` `task_runtime` row per `task_ids`.
async fn seed(task_ids: &[&str]) -> sqlx::SqlitePool {
    let pool = connect("sqlite::memory:").await.unwrap();
    insert_spec(&pool, &spec(), Utc::now()).await.unwrap();

    // The contract-snapshot convention (orchestrator module doc).
    let mut task_contracts = serde_json::Map::new();
    for tid in task_ids {
        task_contracts.insert(
            (*tid).to_owned(),
            serde_json::to_value(a_task_contract()).unwrap(),
        );
    }
    let snapshot = json!({
        "title": "demo",
        "spec_contract": serde_json::to_value(a_spec_contract()).unwrap(),
        "task_contracts": serde_json::Value::Object(task_contracts),
    });
    append_version(
        &pool,
        &spec(),
        1,
        &snapshot,
        VersionTrigger::Dispatch,
        None,
        Utc::now(),
    )
    .await
    .unwrap();
    repo::spec_runtime::initialize(&pool, &spec(), 1)
        .await
        .unwrap();
    for tid in task_ids {
        insert_task(&pool, &TaskId::new(tid).unwrap(), &spec(), None)
            .await
            .unwrap();
    }
    pool
}

/// Build an orchestrator over `pool` + `executor`, returning it plus a
/// `daemon_tx` clone (for the test to seed events) and the bus's recorder.
fn build(
    pool: sqlx::SqlitePool,
    executor: Arc<dyn PhaseExecutor>,
) -> (
    Orchestrator,
    mpsc::Sender<DaemonNotification>,
    RecordingObserver,
) {
    let recorder = RecordingObserver::new();
    let bus = Arc::new(EventBus::new(
        pool.clone(),
        vec![Arc::new(recorder.clone())],
    ));
    let (tx, rx) = mpsc::channel(1024);
    let orch = Orchestrator::new(
        bus,
        pool,
        executor,
        standard_pipeline(),
        all_phases(),
        tx.clone(),
        rx,
    )
    .expect("standard pipeline validates");
    (orch, tx, recorder)
}

/// Drive the orchestrator to quiescence: drain `local` via `handle_event`, then
/// take channel items, until `local` is empty AND no drains are in flight AND
/// the channel has no buffered messages.
///
/// This exercises the same `handle_event` / `handle_drain_terminated` the
/// `run()` loop calls — the run-loop's own structure is tested separately.
async fn drive(orch: &mut Orchestrator) {
    loop {
        // (1) handle-emitted events to fixpoint.
        while let Some(ev) = orch.local.pop_front() {
            if let Err(e) = orch.handle_event(ev).await {
                orch.on_fault(e).await;
            }
        }
        // (2) one channel item — block if a drain is still in flight.
        let msg = if orch.in_flight.is_empty() {
            match orch.daemon_rx.try_recv() {
                Ok(m) => m,
                Err(_) => break, // quiescent: nothing local, nothing in flight
            }
        } else {
            match orch.daemon_rx.recv().await {
                Some(m) => m,
                None => break,
            }
        };
        match msg {
            DaemonNotification::Event(ev) => {
                if let Err(e) = orch.handle_event(ev).await {
                    orch.on_fault(e).await;
                }
            }
            DaemonNotification::DrainTerminated {
                phase_run_id,
                status,
            } => {
                if let Err(e) = orch.handle_drain_terminated(phase_run_id, status).await {
                    orch.on_fault(e).await;
                }
            }
        }
    }
}

/// Inject an externally-produced event into the orchestrator.
///
/// `SpecStarted` (and a manually-injected `TaskStarted`) are events the
/// orchestrator *receives* already-emitted — in production the dispatcher
/// (Phase 9) emits them through the bus and sends them on the channel. A test
/// injecting one directly must therefore `bus.emit` it FIRST (so the
/// `queued → running` / `not_started → active` state flip lands) before
/// pushing it onto `local` for routing.
async fn inject(orch: &mut Orchestrator, event: BoiEvent) {
    orch.bus.emit(&event).await.expect("inject: bus.emit");
    orch.local.push_back(event);
}

/// Inject `SpecStarted` for the test spec (emit through the bus, then route).
async fn seed_spec_started(orch: &mut Orchestrator) {
    inject(orch, BoiEvent::SpecStarted { spec_id: spec() }).await;
}

// ---------------------------------------------------------------------------
// L2 — the full `standard` pipeline walk against `MockExecutor`.
// ---------------------------------------------------------------------------

/// `SpecStarted` → a `workspace_prepare` `phase_runs` row appears (the bus's
/// persist(PhaseStarted) INSERTed it).
#[tokio::test]
async fn test_l2_spec_started_runs_workspace_prepare() {
    let pool = seed(&["T0000001a"]).await;
    // Script ONLY workspace_prepare empty so the walk stops after one phase.
    let mut script = HashMap::new();
    script.insert("workspace_prepare".to_owned(), Vec::new()); // empty → no completion
    let exec = MockExecutor::new(script);
    let (mut orch, _tx, _rec) = build(pool.clone(), Arc::new(exec));

    seed_spec_started(&mut orch).await;
    drive(&mut orch).await;

    let history = repo::phase_runs::fetch_history_for_spec(&pool, &spec())
        .await
        .unwrap();
    assert!(
        history.iter().any(|r| r.phase == "workspace_prepare"),
        "SpecStarted must run workspace_prepare — a phase_runs row should exist",
    );
}

/// The full SPEC walk + TASK walk: a single-task spec scripted all-`Passing`
/// runs the spec phases, fans out, walks the task lifecycle through
/// `merge_to_integration`, resumes the post-`<tasks>` spec phases, and ends
/// `SpecCompleted` with the task `passing`.
#[tokio::test]
async fn test_l2_full_standard_pipeline_walk_completes_spec() {
    let pool = seed(&["T0000001a"]).await;
    let exec = MockExecutor::all_passing(); // every phase ends Passing
    let exec_handle = exec.clone();
    let (mut orch, _tx, recorder) = build(pool.clone(), Arc::new(exec));

    seed_spec_started(&mut orch).await;
    drive(&mut orch).await;

    // The spec completed.
    let spec_row = repo::spec_runtime::fetch(&pool, &spec()).await.unwrap();
    assert_eq!(spec_row.status, "completed", "the spec must complete");
    // The task passed.
    let task_row = repo::task_runtime::fetch(&pool, &TaskId::new("T0000001a").unwrap())
        .await
        .unwrap();
    assert_eq!(task_row.state, "passing", "the task must pass");

    // The SPEC walk ran every spec phase (incl. the post-<tasks> resume).
    let run = exec_handle.calls();
    for phase in [
        "workspace_prepare",
        "plan",
        "critique_plan",
        "merge",
        "teardown",
    ] {
        assert!(
            run.contains(&phase.to_owned()),
            "spec phase {phase} did not run",
        );
    }
    // The TASK walk ran every task phase, incl. the per-task FF-merge.
    for phase in [
        "workspace_verify_in",
        "write_red_tests",
        "execute",
        "commit",
        "workspace_verify_out",
        "merge_to_integration",
    ] {
        assert!(
            run.contains(&phase.to_owned()),
            "task phase {phase} did not run",
        );
    }
    // `SpecCompleted` was emitted.
    assert!(
        recorder
            .seen()
            .iter()
            .any(|e| matches!(e, BoiEvent::SpecCompleted { .. })),
        "a SpecCompleted event must be emitted",
    );
}

/// A scripted spec-phase `Passing` chain advances to the `<tasks>` boundary and
/// emits a `TaskStarted` per ready task.
#[tokio::test]
async fn test_l2_spec_phases_advance_to_tasks_and_fan_out() {
    let pool = seed(&["T000000aa", "T000000bb"]).await;
    // Script the TASK phases empty so the walk stops right after fan-out.
    let mut script = HashMap::new();
    for task_phase in [
        "workspace_verify_in",
        "write_red_tests",
        "execute",
        "validate",
        "review",
        "commit",
        "workspace_verify_out",
    ] {
        script.insert(task_phase.to_owned(), Vec::new());
    }
    let exec = MockExecutor::new(script); // spec phases unscripted → Passing
    let (mut orch, _tx, recorder) = build(pool.clone(), Arc::new(exec));

    seed_spec_started(&mut orch).await;
    drive(&mut orch).await;

    // Both tasks were started at the fan-out.
    let started = recorder
        .seen()
        .into_iter()
        .filter(|e| matches!(e, BoiEvent::TaskStarted { .. }))
        .count();
    assert_eq!(started, 2, "both ready tasks must be started at fan-out");
}

/// `merge_to_integration` `Fail` → the task ends `blocked{MergeConflict}` (the
/// §13.3 `MergeConflict` failure-path test, driven end-to-end).
#[tokio::test]
async fn test_l2_merge_conflict_blocks_the_task() {
    let pool = seed(&["T0000001a"]).await;
    let mut script = HashMap::new();
    // Every phase passes EXCEPT the per-task FF-merge, which fails.
    script.insert(
        "merge_to_integration".to_owned(),
        vec![ScriptedEvent::Complete(VerdictOutcome::Fail {
            error: "merge conflict".into(),
            why: "branches diverged".into(),
            fix: "resolve and re-merge".into(),
        })],
    );
    let exec = MockExecutor::new(script);
    let (mut orch, _tx, _rec) = build(pool.clone(), Arc::new(exec));

    seed_spec_started(&mut orch).await;
    drive(&mut orch).await;

    let task_row = repo::task_runtime::fetch(&pool, &TaskId::new("T0000001a").unwrap())
        .await
        .unwrap();
    assert_eq!(
        task_row.state, "blocked",
        "a merge conflict must block the task",
    );
    let reason: crate::types::reasons::BlockedReason =
        serde_json::from_value(task_row.blocked_reason.unwrap()).unwrap();
    assert!(
        matches!(
            reason,
            crate::types::reasons::BlockedReason::MergeConflict { .. }
        ),
        "the block reason must be MergeConflict, got {reason:?}",
    );
}

/// A deterministic `workspace_verify_in` `Fail` → the task ends
/// `blocked{WorkspaceUnclean}` (the §13.3 `WorkspaceUnclean` failure-path test).
#[tokio::test]
async fn test_l2_workspace_unclean_blocks_the_task() {
    let pool = seed(&["T0000001a"]).await;
    let mut script = HashMap::new();
    script.insert(
        "workspace_verify_in".to_owned(),
        vec![ScriptedEvent::Complete(VerdictOutcome::Fail {
            error: "dirty tree".into(),
            why: "uncommitted changes present".into(),
            fix: "stash or commit".into(),
        })],
    );
    let exec = MockExecutor::new(script);
    let (mut orch, _tx, _rec) = build(pool.clone(), Arc::new(exec));

    seed_spec_started(&mut orch).await;
    drive(&mut orch).await;

    let task_row = repo::task_runtime::fetch(&pool, &TaskId::new("T0000001a").unwrap())
        .await
        .unwrap();
    assert_eq!(task_row.state, "blocked");
    let reason: crate::types::reasons::BlockedReason =
        serde_json::from_value(task_row.blocked_reason.unwrap()).unwrap();
    assert!(
        matches!(
            reason,
            crate::types::reasons::BlockedReason::WorkspaceUnclean { .. }
        ),
        "the block reason must be WorkspaceUnclean, got {reason:?}",
    );
}

/// B-orch-S4 regression: the adjustment side-chain end-to-end through the
/// orchestrator — an `execute` `Fail` routes into `propose_adjustment` →
/// `review_adjustment`, and a `Passing` review exits the side-chain back to
/// `execute`. This is the orchestrator-level coverage whose absence let
/// B-orch-2 (the `propose_adjustment` misroute) ship.
#[tokio::test]
async fn test_l2_adjustment_side_chain_runs_end_to_end() {
    let pool = seed(&["T0000001a"]).await;
    let mut script = HashMap::new();
    // `execute` fails the FIRST time, then passes — so the side-chain runs
    // once and the task can complete on the re-execute.
    script.insert(
        "execute".to_owned(),
        vec![
            ScriptedEvent::Complete(VerdictOutcome::Fail {
                error: "build broke".into(),
                why: "missing import".into(),
                fix: "add the use".into(),
            }),
            ScriptedEvent::Complete(VerdictOutcome::Passing {
                evidence: Evidence::default(),
            }),
        ],
    );
    // The side-chain phases pass — `propose_adjustment` → `review_adjustment`
    // → (Passing) → re-`execute`.
    let exec = MockExecutor::new(script);
    let exec_handle = exec.clone();
    let (mut orch, _tx, _rec) = build(pool.clone(), Arc::new(exec));

    seed_spec_started(&mut orch).await;
    drive(&mut orch).await;

    // The side-chain phases ran in the orchestrator (not just `routing.rs`).
    let calls = exec_handle.calls();
    assert!(
        calls.contains(&"propose_adjustment".to_owned()),
        "the execute Fail must route into propose_adjustment, calls: {calls:?}",
    );
    assert!(
        calls.contains(&"review_adjustment".to_owned()),
        "propose_adjustment must route on to review_adjustment, calls: {calls:?}",
    );
    // `execute` ran twice — the original failure + the post-side-chain retry.
    assert!(
        calls.iter().filter(|p| *p == "execute").count() >= 2,
        "execute must re-run after the side-chain exits, calls: {calls:?}",
    );
    // The task completed.
    let task_row = repo::task_runtime::fetch(&pool, &TaskId::new("T0000001a").unwrap())
        .await
        .unwrap();
    assert_eq!(
        task_row.state, "passing",
        "the task passes after the side-chain resolves the failure",
    );
}

/// B-orch-S4 regression: a task whose `execute` keeps failing trips the
/// `TaskAdjust` cap — the side-chain re-enters `propose_adjustment` until
/// `CAP_TASK_ADJUST` is crossed, then the task ends `blocked{CapExceeded}`.
/// No edge of the side-chain slips the bound (review S2).
#[tokio::test]
async fn test_l2_adjustment_side_chain_caps_a_persistently_failing_task() {
    let pool = seed(&["T0000001a"]).await;
    let mut script = HashMap::new();
    // `execute` ALWAYS fails — each failure re-enters the side-chain.
    script.insert(
        "execute".to_owned(),
        vec![ScriptedEvent::Complete(VerdictOutcome::Fail {
            error: "build broke".into(),
            why: "still broken".into(),
            fix: "try again".into(),
        })],
    );
    // `review_adjustment` ALWAYS Redo — re-enters `propose_adjustment`, the
    // edge that must still be cap-counted.
    script.insert(
        "review_adjustment".to_owned(),
        vec![ScriptedEvent::Complete(VerdictOutcome::Redo {
            reason: "fix incomplete".into(),
        })],
    );
    let exec = MockExecutor::new(script);
    let (mut orch, _tx, _rec) = build(pool.clone(), Arc::new(exec));

    seed_spec_started(&mut orch).await;
    drive(&mut orch).await;

    // The task ended blocked with CapExceeded — the side-chain did not loop
    // `propose ↔ review` forever.
    let task_row = repo::task_runtime::fetch(&pool, &TaskId::new("T0000001a").unwrap())
        .await
        .unwrap();
    assert_eq!(
        task_row.state, "blocked",
        "a persistently failing task must block, not loop the side-chain forever",
    );
    let reason: crate::types::reasons::BlockedReason =
        serde_json::from_value(task_row.blocked_reason.unwrap()).unwrap();
    let crate::types::reasons::BlockedReason::CapExceeded { loop_name, .. } = reason else {
        unreachable!("the side-chain cap must block with CapExceeded, got {reason:?}");
    };
    assert_eq!(
        loop_name, "task_adjust",
        "the tripped cap is the side-chain's"
    );
    // AUDIT A2: the cap-exhausted block leaves the spec RUNNING — visibly
    // blocked and operator-revivable per design §6's recovery table
    // (`boi unblock --reset-counter` is the documented CapExceeded revive).
    // The earlier SpecFailed-on-block made that loop unwinnable: `failed` is
    // terminal with no exit edge, so the spec was bricked at block time.
    let spec_row = repo::spec_runtime::fetch(&pool, &spec()).await.unwrap();
    assert_eq!(
        spec_row.status, "running",
        "an exhausted adjustment cap must leave the spec revivable (`running`), \
         never terminally `failed` (audit A2)",
    );
}

/// `SpecCanceled` cancels in-flight phases AND cascades `TaskCanceled` to every
/// non-`passing` task (§6 recovery table; review S13).
///
/// Strengthened for audit A3: the cascade's `TaskCanceled` events route back
/// through `on_task_canceled` under an ALREADY-terminal spec — the handler's
/// terminal-spec guard must treat them as routing-only. No phase may run (no
/// pipeline resume off a canceled spec) and no second terminal spec event may
/// be emitted.
#[tokio::test]
async fn test_l2_spec_canceled_cascades_to_non_passing_tasks() {
    let pool = seed(&["T000000aa", "T000000bb", "T000000cc"]).await;
    // Drive task aa to `passing` (it must NOT be cascaded), bb to `active`
    // (the bus's stranded-active sweep closes it), and leave cc `not_started`
    // (only the orchestrator's cascade arm closes it).
    repo::task_runtime::update_state(
        &pool,
        &TaskId::new("T000000aa").unwrap(),
        crate::types::state::TaskState::Passing,
        None,
        None,
        Utc::now(),
    )
    .await
    .unwrap();
    repo::task_runtime::update_state(
        &pool,
        &TaskId::new("T000000bb").unwrap(),
        crate::types::state::TaskState::Active,
        None,
        None,
        Utc::now(),
    )
    .await
    .unwrap();
    // Move the spec to `running` so `SpecCanceled` is a legal transition.
    repo::spec_runtime::update_status(
        &pool,
        &spec(),
        crate::types::state::SpecStatus::Running,
        None,
        Utc::now(),
    )
    .await
    .unwrap();

    let exec = MockExecutor::all_passing();
    let exec_handle = exec.clone();
    let (mut orch, _tx, recorder) = build(pool.clone(), Arc::new(exec));
    // `inject` (bus.emit first) — in production `SpecCanceled` is persisted
    // (spec → `canceled`) BEFORE the orchestrator routes it (the C1 split).
    inject(
        &mut orch,
        BoiEvent::SpecCanceled {
            spec_id: spec(),
            reason: crate::types::reasons::CancellationReason::Operator { note: None },
        },
    )
    .await;
    drive(&mut orch).await;

    // Task aa stayed `passing`; tasks bb and cc were cascaded to `canceled`.
    assert_eq!(
        repo::task_runtime::fetch(&pool, &TaskId::new("T000000aa").unwrap())
            .await
            .unwrap()
            .state,
        "passing",
        "a passing task must NOT be cascade-canceled",
    );
    for tid in ["T000000bb", "T000000cc"] {
        assert_eq!(
            repo::task_runtime::fetch(&pool, &TaskId::new(tid).unwrap())
                .await
                .unwrap()
                .state,
            "canceled",
            "a non-passing task must be cascade-canceled",
        );
    }
    // A3 guard: the cascade under a terminal spec is routing-only — no phase
    // runs (no resume off a canceled spec)…
    assert!(
        exec_handle.calls().is_empty(),
        "no phase may run under a canceled spec, ran: {:?}",
        exec_handle.calls(),
    );
    // …and exactly one terminal spec event exists (no second `SpecCanceled`,
    // no fault-path `SpecFailed`).
    let seen = recorder.seen();
    assert_eq!(
        seen.iter()
            .filter(|e| matches!(e, BoiEvent::SpecCanceled { .. }))
            .count(),
        1,
        "exactly one SpecCanceled",
    );
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, BoiEvent::SpecFailed { .. })),
        "the cascade must not route through the fault path",
    );
}

/// B-svc-1 regression: a task whose executor stream ends *clean* but relays no
/// terminal `PhaseCompleted` must end `blocked` — NOT strand `active` forever.
///
/// Before the fix the drain reported `DrainStatus::Completed` for a
/// verdict-less stream, `handle_drain_terminated` treated `Completed` as
/// cleanup-only, and the task sat `active` with `all_tasks_settled` false
/// forever — the spec could never complete. The fix: a verdict-less clean
/// stream → `CompletedWithoutVerdict` → the visible-failure arm → `TaskBlocked`.
#[tokio::test]
async fn test_l2_drain_without_verdict_blocks_the_task_not_strands_it() {
    let pool = seed(&["T0000001a"]).await;
    // Script the FIRST task phase with an explicitly empty step list — the
    // executor stream runs clean and ends, relaying no `PhaseCompleted`.
    let mut script = HashMap::new();
    script.insert("workspace_verify_in".to_owned(), Vec::new());
    let exec = MockExecutor::new(script);
    let (mut orch, _tx, _rec) = build(pool.clone(), Arc::new(exec));

    seed_spec_started(&mut orch).await;
    drive(&mut orch).await;

    let task_row = repo::task_runtime::fetch(&pool, &TaskId::new("T0000001a").unwrap())
        .await
        .unwrap();
    assert_eq!(
        task_row.state, "blocked",
        "a verdict-less drain must surface a visible TaskBlocked, not strand the task active",
    );
    let reason: crate::types::reasons::BlockedReason =
        serde_json::from_value(task_row.blocked_reason.unwrap()).unwrap();
    assert!(
        matches!(
            reason,
            crate::types::reasons::BlockedReason::ProviderFailed { .. }
        ),
        "a verdict-less drain blocks with ProviderFailed, got {reason:?}",
    );
}

/// B-svc-1 regression (spec-level): a *spec* phase whose executor stream ends
/// clean with no `PhaseCompleted` must fail the spec — not leave it `running`
/// forever. Mirrors the task-level test above on the `None`-task arm of
/// `handle_drain_terminated`.
#[tokio::test]
async fn test_l2_spec_phase_drain_without_verdict_fails_the_spec() {
    let pool = seed(&["T0000001a"]).await;
    // The spec's entry phase `workspace_prepare` is scripted empty.
    let mut script = HashMap::new();
    script.insert("workspace_prepare".to_owned(), Vec::new());
    let exec = MockExecutor::new(script);
    let (mut orch, _tx, _rec) = build(pool.clone(), Arc::new(exec));

    seed_spec_started(&mut orch).await;
    drive(&mut orch).await;

    let spec_row = repo::spec_runtime::fetch(&pool, &spec()).await.unwrap();
    assert_eq!(
        spec_row.status, "failed",
        "a verdict-less spec-phase drain must fail the spec, not leave it running forever",
    );
}

/// G21.1 regression: a spec-level `plan ↔ critique_plan` loop that never
/// converges trips `CAP_PLAN_CRITIQUE` and FAILS THE SPEC — driven end-to-end
/// through the orchestrator. Before G21.1 `route_spec` was synchronous and
/// could not touch a counter, so this loop ran uncapped forever.
#[tokio::test]
async fn test_l2_spec_plan_critique_loop_caps_and_fails_the_spec() {
    let pool = seed(&["T0000001a"]).await;
    // `critique_plan` ALWAYS Redo — its `on.redo.next` re-runs `plan`, and
    // `plan` (unscripted) passes → back to `critique_plan` → Redo → … . The
    // spec-level cap is what must stop this.
    let mut script = HashMap::new();
    script.insert(
        "critique_plan".to_owned(),
        vec![ScriptedEvent::Complete(VerdictOutcome::Redo {
            reason: "the plan still has gaps".into(),
        })],
    );
    let exec = MockExecutor::new(script);
    let (mut orch, _tx, _rec) = build(pool.clone(), Arc::new(exec));

    seed_spec_started(&mut orch).await;
    drive(&mut orch).await;

    // The uncapped loop did NOT run forever — the spec failed at the cap.
    let spec_row = repo::spec_runtime::fetch(&pool, &spec()).await.unwrap();
    assert_eq!(
        spec_row.status, "failed",
        "an unconverging plan↔critique_plan loop must fail the spec at the cap",
    );
    // The spec-level counter recorded the iterations.
    assert!(
        spec_row.iterations_plan_critique >= CAP_PLAN_CRITIQUE.into(),
        "the plan_critique counter must reach the cap, got {}",
        spec_row.iterations_plan_critique,
    );
}

/// A panicking drain → `handle_drain_terminated` surfaces a visible
/// `TaskBlocked` (the task does not stick silently — review C3).
#[tokio::test]
async fn test_l2_panicking_drain_surfaces_a_visible_task_blocked() {
    let pool = seed(&["T0000001a"]).await;
    let (mut orch, _tx, _rec) = build(pool.clone(), Arc::new(MockExecutor::all_passing()));
    // Inject TaskStarted — emit it (so the task flips `not_started → active`,
    // making the later `active → blocked` legal) and route it.
    inject(
        &mut orch,
        BoiEvent::TaskStarted {
            spec_id: spec(),
            task_id: TaskId::new("T0000001a").unwrap(),
        },
    )
    .await;
    // Drain `local` so the TaskStarted is routed and a drain registered.
    while let Some(ev) = orch.local.pop_front() {
        orch.handle_event(ev).await.unwrap();
    }
    // Pick the in-flight phase run and synthesise a Panicked DrainTerminated.
    let pr_id = orch
        .in_flight
        .keys()
        .next()
        .cloned()
        .expect("a drain is in flight after TaskStarted");
    orch.handle_drain_terminated(pr_id, crate::service::registry::DrainStatus::Panicked)
        .await
        .unwrap();
    // Process the emitted TaskBlocked.
    while let Some(ev) = orch.local.pop_front() {
        orch.handle_event(ev).await.unwrap();
    }

    let task_row = repo::task_runtime::fetch(&pool, &TaskId::new("T0000001a").unwrap())
        .await
        .unwrap();
    assert_eq!(
        task_row.state, "blocked",
        "a panicking drain must surface a visible TaskBlocked, not stick the task",
    );
}

/// B-svc-S4 / B-orch-S2 / B-orch-S3 regression: a `plan_revision` phase that
/// returns a non-`Passing` verdict must FAIL THE SPEC — not leave the
/// triggering task `blocked{PlanRevisionPending}` forever.
///
/// Before the fix `on_plan_revision_completed`'s non-`Passing` arm logged
/// `error!` and returned `Ok(())`: the blocking report blocked the task, the
/// revision worker failed, and nothing ever moved the task again. The fix
/// surfaces a visible `SpecFailed`. The test also exercises B-orch-S2 (the
/// `phase_run_id` threaded from the event) and B-orch-S3 (the reporting task
/// carried forward from `on_report`).
#[tokio::test]
async fn test_l2_failed_plan_revision_fails_the_spec_not_strands_the_task() {
    let pool = seed(&["T0000001a"]).await;
    let task = TaskId::new("T0000001a").unwrap();
    // Drive the spec `running` and the task `active` so the blocking report's
    // `active → blocked` transition is legal.
    repo::spec_runtime::update_status(
        &pool,
        &spec(),
        crate::types::state::SpecStatus::Running,
        None,
        Utc::now(),
    )
    .await
    .unwrap();
    repo::task_runtime::update_state(
        &pool,
        &task,
        crate::types::state::TaskState::Active,
        None,
        None,
        Utc::now(),
    )
    .await
    .unwrap();

    // The `plan_revision` phase FAILS (a non-Passing verdict).
    let mut script = HashMap::new();
    script.insert(
        "plan_revision".to_owned(),
        vec![ScriptedEvent::Complete(VerdictOutcome::Fail {
            error: "cannot revise".into(),
            why: "the report is contradictory".into(),
            fix: "operator must intervene".into(),
        })],
    );
    let exec = MockExecutor::new(script);
    let (mut orch, _tx, _rec) = build(pool.clone(), Arc::new(exec));

    // A blocking `task_report` from the task.
    inject(
        &mut orch,
        BoiEvent::ReportReceived {
            spec_id: spec(),
            task_id: task.clone(),
            kind: "scope_gap".into(),
            payload: json!({ "detail": "needs a new task" }),
            blocking: true,
        },
    )
    .await;
    drive(&mut orch).await;

    // The triggering task was blocked by the report...
    let task_row = repo::task_runtime::fetch(&pool, &task).await.unwrap();
    assert_eq!(
        task_row.state, "blocked",
        "the blocking report must block the reporting task",
    );
    // ...and the failed plan_revision FAILED THE SPEC — it did not silently
    // leave the task stranded `blocked` with the spec still `running`.
    let spec_row = repo::spec_runtime::fetch(&pool, &spec()).await.unwrap();
    assert_eq!(
        spec_row.status, "failed",
        "a failed plan_revision must fail the spec, not strand the blocked task",
    );
}

/// `run_phase` re-hydrates the authored contracts from the `spec_versions`
/// snapshot — a snapshot MISSING the `spec_contract` key faults loudly
/// (`on_fault` → the spec ends `failed`), never a silent default.
#[tokio::test]
async fn test_l2_run_phase_faults_on_a_snapshot_missing_contracts() {
    let pool = connect("sqlite::memory:").await.unwrap();
    insert_spec(&pool, &spec(), Utc::now()).await.unwrap();
    // A snapshot with NO `spec_contract` key — the convention is violated.
    append_version(
        &pool,
        &spec(),
        1,
        &json!({ "title": "demo" }),
        VersionTrigger::Dispatch,
        None,
        Utc::now(),
    )
    .await
    .unwrap();
    repo::spec_runtime::initialize(&pool, &spec(), 1)
        .await
        .unwrap();
    repo::spec_runtime::update_status(
        &pool,
        &spec(),
        crate::types::state::SpecStatus::Running,
        None,
        Utc::now(),
    )
    .await
    .unwrap();

    let (mut orch, _tx, _rec) = build(pool.clone(), Arc::new(MockExecutor::all_passing()));
    // A spec-phase `Passing` → route_spec → RunSpecPhase(plan) → run_phase(plan)
    // → rehydrate_contracts → the snapshot has no `spec_contract` → Contract fault.
    orch.local.push_back(BoiEvent::PhaseCompleted {
        phase_run_id: crate::types::ids::PhaseRunId::new("P0000001a").unwrap(),
        spec_id: spec(),
        task_id: None,
        phase: "workspace_prepare".into(),
        verdict: WorkerVerdict {
            synopsis: "x".into(),
            outcome: VerdictOutcome::Passing {
                evidence: Evidence::default(),
            },
        },
        tokens_in: 0,
        tokens_out: 0,
        duration_ms: 0,
    });
    drive(&mut orch).await;

    let spec_row = repo::spec_runtime::fetch(&pool, &spec()).await.unwrap();
    assert_eq!(
        spec_row.status, "failed",
        "a snapshot missing `spec_contract` must fault the spec, not be defaulted",
    );
}

// ---------------------------------------------------------------------------
// L3 — the run-loop fault path.
// ---------------------------------------------------------------------------

/// Forcing `run_phase` to `Err` for a spec → `on_fault` ends THAT spec
/// `failed{PreflightFailed}` (review B-orch-S1 — an orchestration fault is
/// surfaced as `PreflightFailed{details}` naming the real cause, NOT
/// mislabelled `DaemonCrash`).
#[tokio::test]
async fn test_l3_orchestrator_handle_event_err_fails_the_affected_spec() {
    // A spec whose snapshot is missing `spec_contract` — `run_phase` faults.
    let pool = connect("sqlite::memory:").await.unwrap();
    insert_spec(&pool, &spec(), Utc::now()).await.unwrap();
    append_version(
        &pool,
        &spec(),
        1,
        &json!({ "title": "no contract here" }),
        VersionTrigger::Dispatch,
        None,
        Utc::now(),
    )
    .await
    .unwrap();
    repo::spec_runtime::initialize(&pool, &spec(), 1)
        .await
        .unwrap();
    repo::spec_runtime::update_status(
        &pool,
        &spec(),
        crate::types::state::SpecStatus::Running,
        None,
        Utc::now(),
    )
    .await
    .unwrap();

    let (mut orch, _tx, recorder) = build(pool.clone(), Arc::new(MockExecutor::all_passing()));
    // Directly run a phase that will fault in rehydrate_contracts.
    let err = orch.run_phase(&spec(), None, "plan").await.unwrap_err();
    orch.on_fault(err).await;
    // Drain `local` (the on_fault-emitted SpecFailed) — any further fault is
    // routed through on_fault too, never silently discarded.
    while let Some(ev) = orch.local.pop_front() {
        if let Err(e) = orch.handle_event(ev).await {
            orch.on_fault(e).await;
        }
    }

    let spec_row = repo::spec_runtime::fetch(&pool, &spec()).await.unwrap();
    assert_eq!(spec_row.status, "failed");
    let reason: crate::types::reasons::FailureReason =
        serde_json::from_value(spec_row.failure_reason.unwrap()).unwrap();
    assert!(
        matches!(
            reason,
            crate::types::reasons::FailureReason::PreflightFailed { .. }
        ),
        "an orchestration fault is surfaced as PreflightFailed, got {reason:?}",
    );
    // The fault surfaced a `SpecFailed` event.
    assert!(
        recorder
            .seen()
            .iter()
            .any(|e| matches!(e, BoiEvent::SpecFailed { .. })),
    );
}

/// `run()` exits — with an error log, not silently — when the daemon channel
/// closes. The orchestrator is handed a `daemon_rx` whose every sender is
/// already dropped, so the first `recv()` yields `None`.
#[tokio::test]
async fn test_l3_orchestrator_run_exits_on_closed_channel() {
    let pool = seed(&["T0000001a"]).await;
    let bus = Arc::new(EventBus::new(
        pool.clone(),
        vec![Arc::new(RecordingObserver::new())],
    ));
    // A channel whose only sender is dropped → `recv()` returns `None`.
    let (closed_tx, closed_rx) = mpsc::channel::<DaemonNotification>(8);
    drop(closed_tx);
    // The orchestrator's `daemon_tx` field is a throwaway sender — this test
    // never spawns a drain, so `daemon_tx` is unused; `daemon_rx` is the
    // pre-closed half, which drives `run()` straight to the `None` arm.
    let (throwaway_tx, _throwaway_rx) = mpsc::channel::<DaemonNotification>(1);
    let orch = Orchestrator::new(
        bus,
        pool,
        Arc::new(MockExecutor::all_passing()),
        standard_pipeline(),
        all_phases(),
        throwaway_tx,
        closed_rx,
    )
    .unwrap();

    // `run()` must RETURN (it `break`s on the `None` arm), not hang.
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), orch.run()).await;
    assert!(
        result.is_ok(),
        "run() must exit when the channel closes — it hung instead",
    );
}

/// A3 regression (audit 2026-06-10): canceling the LAST outstanding task of a
/// running spec — every sibling already `passing` (the primary use case:
/// cancel the straggler, merge the good work) — must RESUME the spec pipeline
/// past the `<tasks>` boundary and complete the spec.
///
/// Before the fix `all_tasks_settled` was evaluated ONLY in `on_task_passed`;
/// the `TaskCanceled` arm closed phase_run rows and returned, so no event ever
/// resumed the pipeline and the spec sat `running` forever, silently — the
/// same stall class as the TaskBlocked→SpecFailed fixes (OBS-019). `canceled`
/// counts as settled by design (§6 — resume-after-cancel is the intent).
#[tokio::test]
async fn test_l3_orchestrator_cancel_of_last_outstanding_task_resumes_and_completes_the_spec() {
    let pool = seed(&["T000000aa", "T000000bb"]).await;
    // Sibling aa already passed; bb is the active straggler.
    repo::task_runtime::update_state(
        &pool,
        &TaskId::new("T000000aa").unwrap(),
        crate::types::state::TaskState::Passing,
        None,
        None,
        Utc::now(),
    )
    .await
    .unwrap();
    repo::task_runtime::update_state(
        &pool,
        &TaskId::new("T000000bb").unwrap(),
        crate::types::state::TaskState::Active,
        None,
        None,
        Utc::now(),
    )
    .await
    .unwrap();
    // The spec is mid-run at the `<tasks>` boundary — `running`.
    repo::spec_runtime::update_status(
        &pool,
        &spec(),
        crate::types::state::SpecStatus::Running,
        None,
        Utc::now(),
    )
    .await
    .unwrap();

    let exec = MockExecutor::all_passing();
    let exec_handle = exec.clone();
    let (mut orch, _tx, recorder) = build(pool.clone(), Arc::new(exec));

    // The operator cancels the straggler (`boi cancel <task-id>`).
    inject(
        &mut orch,
        BoiEvent::TaskCanceled {
            spec_id: spec(),
            task_id: TaskId::new("T000000bb").unwrap(),
            reason: crate::types::reasons::CancellationReason::Operator {
                note: Some("straggler — merge the good work".into()),
            },
        },
    )
    .await;
    drive(&mut orch).await;

    // The spec resumed past `<tasks>` (the post-boundary `merge` phase ran)…
    let calls = exec_handle.calls();
    assert!(
        calls.contains(&"merge".to_owned()),
        "canceling the last outstanding task must resume the spec pipeline at \
         the post-<tasks> phase, calls: {calls:?}",
    );
    // …and COMPLETED — not wedged `running` forever.
    let spec_row = repo::spec_runtime::fetch(&pool, &spec()).await.unwrap();
    assert_eq!(
        spec_row.status, "completed",
        "the spec must merge the good work and complete — not wedge `running`",
    );
    assert!(
        recorder
            .seen()
            .iter()
            .any(|e| matches!(e, BoiEvent::SpecCompleted { .. })),
        "a terminal SpecCompleted must be emitted",
    );
}

/// A3 regression (audit 2026-06-10), all-canceled variant: when canceling the
/// final non-terminal task leaves EVERY task `canceled` (nothing passed —
/// nothing to merge), the spec must reach a TERMINAL status with a typed
/// reason — never a silent `running` wedge. The legal §6 edge is
/// `running → canceled` (`SpecCanceled`), propagating the final task's
/// `CancellationReason`.
#[tokio::test]
async fn test_l3_orchestrator_cancel_of_every_task_cancels_the_spec_terminally() {
    let pool = seed(&["T000000aa", "T000000bb"]).await;
    // Task aa was already canceled earlier; bb is the last non-terminal task.
    repo::task_runtime::update_state(
        &pool,
        &TaskId::new("T000000aa").unwrap(),
        crate::types::state::TaskState::Canceled,
        None,
        None,
        Utc::now(),
    )
    .await
    .unwrap();
    repo::task_runtime::update_state(
        &pool,
        &TaskId::new("T000000bb").unwrap(),
        crate::types::state::TaskState::Active,
        None,
        None,
        Utc::now(),
    )
    .await
    .unwrap();
    repo::spec_runtime::update_status(
        &pool,
        &spec(),
        crate::types::state::SpecStatus::Running,
        None,
        Utc::now(),
    )
    .await
    .unwrap();

    let (mut orch, _tx, recorder) = build(pool.clone(), Arc::new(MockExecutor::all_passing()));
    inject(
        &mut orch,
        BoiEvent::TaskCanceled {
            spec_id: spec(),
            task_id: TaskId::new("T000000bb").unwrap(),
            reason: crate::types::reasons::CancellationReason::Operator {
                note: Some("scope cut entirely".into()),
            },
        },
    )
    .await;
    drive(&mut orch).await;

    // The spec is terminally `canceled` — with the final task's reason.
    let spec_row = repo::spec_runtime::fetch(&pool, &spec()).await.unwrap();
    assert_eq!(
        spec_row.status, "canceled",
        "all tasks ended canceled — the spec must terminate, not wedge `running`",
    );
    let reason: crate::types::reasons::CancellationReason =
        serde_json::from_value(spec_row.cancellation_reason.unwrap()).unwrap();
    assert!(
        matches!(
            reason,
            crate::types::reasons::CancellationReason::Operator { .. }
        ),
        "the spec's CancellationReason must propagate the final task's, got {reason:?}",
    );
    // A terminal spec event was emitted — never a silent return.
    assert!(
        recorder
            .seen()
            .iter()
            .any(|e| matches!(e, BoiEvent::SpecCanceled { .. })),
        "a terminal SpecCanceled must be emitted",
    );
}

/// Review M1 finding 3 (high) — the settled-check can fire TWICE, spawning
/// duplicate concurrent post-`<tasks>` drains.
///
/// Both production paths persist a batch of `TaskCanceled` events BEFORE any
/// of them is routed: `plan_layer::apply_revision` bus-emits every
/// `RemoveTask` cancel in a loop and `on_plan_revision_completed` then routes
/// them one by one; two quick `boi cancel` commands persist on the
/// control-socket task concurrently with the orchestrator loop. Either way,
/// when the FIRST cancel is routed the task set is ALREADY settled (the
/// second cancel's state flip landed at emit time), so `on_task_canceled`
/// resumes the pipeline — and the SECOND routed cancel sees the spec still
/// `running` and resumes it AGAIN. `run_phase` is not idempotent: each call
/// mints a fresh `PhaseRunId` and spawns a fresh drain, so two spec-level
/// `merge` drains run libgit2 ff-merge/rebase/forced-checkout against the
/// SAME operator repo concurrently (the OBS-030 class), and the loser's Fail
/// verdict routes Halt → a spec whose merge actually landed is marked
/// terminally failed.
///
/// DESIRED: the resume past `<tasks>` is idempotent — exactly ONE `merge`
/// drain, and the spec completes.
#[tokio::test]
async fn test_l3_orchestrator_double_settling_cancel_resumes_the_pipeline_exactly_once() {
    let pool = seed(&["T000000aa", "T000000bb", "T000000cc"]).await;
    // aa already passed (so the resume arm is reachable); bb + cc are active.
    repo::task_runtime::update_state(
        &pool,
        &TaskId::new("T000000aa").unwrap(),
        crate::types::state::TaskState::Passing,
        None,
        None,
        Utc::now(),
    )
    .await
    .unwrap();
    for tid in ["T000000bb", "T000000cc"] {
        repo::task_runtime::update_state(
            &pool,
            &TaskId::new(tid).unwrap(),
            crate::types::state::TaskState::Active,
            None,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
    }
    repo::spec_runtime::update_status(
        &pool,
        &spec(),
        crate::types::state::SpecStatus::Running,
        None,
        Utc::now(),
    )
    .await
    .unwrap();

    let exec = MockExecutor::all_passing();
    let exec_handle = exec.clone();
    let (mut orch, _tx, _recorder) = build(pool.clone(), Arc::new(exec));

    // BOTH cancels are persisted (each `inject` bus-emits first) before
    // either is routed — the plan-revision / racy-double-`boi cancel` shape.
    for tid in ["T000000bb", "T000000cc"] {
        inject(
            &mut orch,
            BoiEvent::TaskCanceled {
                spec_id: spec(),
                task_id: TaskId::new(tid).unwrap(),
                reason: crate::types::reasons::CancellationReason::Operator {
                    note: Some("batch cancel — both persisted before routing".into()),
                },
            },
        )
        .await;
    }
    drive(&mut orch).await;

    // EXACTLY one spec-level `merge` drain — not one per settling cancel.
    let merge_runs = exec_handle
        .calls()
        .iter()
        .filter(|phase| phase.as_str() == "merge")
        .count();
    assert_eq!(
        merge_runs,
        1,
        "a double-settling cancel must resume the pipeline exactly once — \
         duplicate merge drains race the same operator repo (M1 finding 3), \
         calls: {:?}",
        exec_handle.calls(),
    );
    // …and the spec completed (the duplicate-drain race could mark a spec
    // whose merge landed as terminally failed).
    let spec_row = repo::spec_runtime::fetch(&pool, &spec()).await.unwrap();
    assert_eq!(
        spec_row.status, "completed",
        "the spec must complete after the single resume",
    );
}

// ---------------------------------------------------------------------------
// L2 — `Orchestrator::new` validates the pipeline.
// ---------------------------------------------------------------------------

/// Phase-10 erratum regression: a pipeline `[overrides.<phase>.runtime]` is
/// APPLIED to the phase the orchestrator runs. The `standard` pipeline
/// overrides `critique_plan`'s provider to `openrouter`; before the fix the
/// orchestrator ran it with `claude_code` (the phase TOML's own provider),
/// silently dropping the override. The full all-passing walk runs every spec
/// phase — the `critique_plan` `phase_runs` row must record `openrouter`.
#[tokio::test]
async fn test_l2_pipeline_override_is_applied_to_critique_plan() {
    let pool = seed(&["T0000001a"]).await;
    let (mut orch, _tx, _rec) = build(pool.clone(), Arc::new(MockExecutor::all_passing()));

    seed_spec_started(&mut orch).await;
    drive(&mut orch).await;

    let history = repo::phase_runs::fetch_history_for_spec(&pool, &spec())
        .await
        .unwrap();
    let critique = history
        .iter()
        .find(|r| r.phase == "critique_plan")
        .expect("critique_plan ran in the full walk");
    assert_eq!(
        critique.provider, "openrouter",
        "the standard pipeline's critique_plan provider override must be applied",
    );
    // A non-overridden phase keeps its own provider — the overlay is scoped.
    let plan = history
        .iter()
        .find(|r| r.phase == "plan")
        .expect("plan ran in the full walk");
    assert_eq!(
        plan.provider, "claude_code",
        "a phase with no override keeps the phase TOML's provider",
    );
}

/// `Orchestrator::new` rejects a malformed routing graph at construction —
/// a startup rejection, never a mid-run wedge (review S5).
#[tokio::test]
async fn test_l2_new_rejects_a_malformed_pipeline() {
    let pool = seed(&["T0000001a"]).await;
    let bus = Arc::new(EventBus::new(
        pool.clone(),
        vec![Arc::new(RecordingObserver::new())],
    ));
    // Break a phase's routing — `execute.on.fail.next` → a non-existent phase.
    let mut phases = all_phases();
    phases
        .get_mut("execute")
        .unwrap()
        .on
        .get_mut(&crate::config::VerdictTag::Fail)
        .unwrap()
        .next = Some("ghost".to_owned());
    let (tx, rx) = mpsc::channel(8);
    let result = Orchestrator::new(
        bus,
        pool,
        Arc::new(MockExecutor::all_passing()),
        standard_pipeline(),
        phases,
        tx,
        rx,
    );
    assert!(
        result.is_err(),
        "Orchestrator::new must reject a malformed routing graph",
    );
}
