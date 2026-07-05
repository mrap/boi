//! The 5 fixture L3 tests (Task 10.3).
//!
//! One `test_l3_*` per §13 fixture spec, each asserting a **single determined**
//! terminal state — no disjunctions (review S7). The harness ([`super::harness`])
//! dispatches the fixture against a tempdir DB + workspace and drives the real
//! orchestrator / bus / repo to quiescence with a [`MockExecutor`].

use std::collections::HashMap;
use std::sync::Arc;

use boi::repo;
use boi::service::registry::PhaseExecutor;
use boi::types::context::PhaseContext;
use boi::types::event::BoiEvent;
use boi::types::ids::PhaseRunId;
use boi::types::plan::{PlanEdit, PlanRevision};
use boi::types::verdict::{Evidence, VerdictOutcome};
use futures::stream::{self, BoxStream, StreamExt};
use tokio_util::sync::CancellationToken;

use super::harness::{CallIndexedExecutor, all_passing, run_fixture};

// ---------------------------------------------------------------------------
// 01 — the trivial single-task spec.
// ---------------------------------------------------------------------------

/// `01-typo-fix` driven all-passing → the spec ends `completed` and its single
/// task ends `passing`. The shortest happy path through the whole pipeline.
#[tokio::test]
async fn test_l3_fixtures_01_typo_fix_completes() {
    let run = run_fixture("01-typo-fix", all_passing()).await;
    assert_eq!(
        run.spec_status().await,
        "completed",
        "01-typo-fix must end `completed`",
    );
    assert_eq!(run.dispatched.task_ids.len(), 1, "01 has one task");
    assert_eq!(
        run.task_state(0).await,
        "passing",
        "01's single task must end `passing`",
    );
}

// ---------------------------------------------------------------------------
// 02 — the multi-task DAG spec.
// ---------------------------------------------------------------------------

/// `02-multi-task-feature` driven all-passing → every DAG task ends `passing`,
/// and the scheduler observed dependency order: a task never reached `passing`
/// before its `blocked_by` predecessors did.
#[tokio::test]
async fn test_l3_fixtures_02_multi_task_dag_all_pass_in_dep_order() {
    let run = run_fixture("02-multi-task-feature", all_passing()).await;
    assert_eq!(
        run.spec_status().await,
        "completed",
        "02 must end `completed`",
    );
    assert_eq!(run.dispatched.task_ids.len(), 3, "02 is a 3-task DAG");
    // Every DAG task ended `passing`.
    for r in ["setup-middleware", "apply-middleware", "document-headers"] {
        assert_eq!(
            run.task_state_by_ref(r).await,
            "passing",
            "DAG task `{r}` must end `passing`",
        );
    }
    // Dependency order: a dependent task's FIRST phase_runs row must start
    // AFTER its dependency's task lifecycle has fully run. The scheduler only
    // spawns a task once every `blocked_by` predecessor is `passing`, so
    // `apply-middleware`'s earliest run starts strictly after
    // `setup-middleware`'s latest run.
    let history = repo::phase_runs::fetch_history_for_spec(&run.pool, run.spec_id())
        .await
        .expect("phase-run history");
    let setup_id = run.dispatched.ref_to_id["setup-middleware"].as_str();
    let apply_id = run.dispatched.ref_to_id["apply-middleware"].as_str();
    let setup_last = history
        .iter()
        .filter(|r| r.task_id.as_deref() == Some(setup_id))
        .map(|r| r.started_at)
        .max()
        .expect("setup-middleware ran");
    let apply_first = history
        .iter()
        .filter(|r| r.task_id.as_deref() == Some(apply_id))
        .map(|r| r.started_at)
        .min()
        .expect("apply-middleware ran");
    assert!(
        apply_first > setup_last,
        "apply-middleware must start after setup-middleware finishes \
         (dep order) — apply_first {apply_first:?}, setup_last {setup_last:?}",
    );
}

// ---------------------------------------------------------------------------
// 03 — the failure-recovery spec (split into two determined tests — review S7).
// ---------------------------------------------------------------------------

/// `03-failure-recovery`: `execute` fails ONCE then passes — the adjustment
/// side-chain (propose_adjustment → review_adjustment → re-execute) resolves
/// the failure and the task ends `passing`, the spec `completed`.
#[tokio::test]
async fn test_l3_fixtures_03_recovers_via_side_chain() {
    // The FIRST `execute` call fails, the SECOND passes — `CallIndexedExecutor`
    // gives per-call scripting (one outcome per phase run); the side-chain
    // phases (unscripted) pass, so the task recovers on the re-execute.
    let mut script = HashMap::new();
    script.insert(
        "execute".to_owned(),
        vec![
            VerdictOutcome::Fail {
                error: "build broke".into(),
                why: "missing import".into(),
                fix: "add the use".into(),
            },
            VerdictOutcome::Passing {
                evidence: Evidence::default(),
            },
        ],
    );
    let exec: Arc<dyn PhaseExecutor> = Arc::new(CallIndexedExecutor::new(script));
    let run = run_fixture("03-failure-recovery", exec).await;

    assert_eq!(
        run.spec_status().await,
        "completed",
        "03 recovers — the side-chain resolves the execute failure",
    );
    assert_eq!(
        run.task_state(0).await,
        "passing",
        "03's task ends `passing` after the side-chain re-execute",
    );
}

/// `03-failure-recovery`: `execute` fails PERSISTENTLY and `review_adjustment`
/// always `Redo`s — the side-chain re-enters `propose_adjustment` until
/// `CAP_TASK_ADJUST` is crossed, then the task ends `blocked{CapExceeded}`.
///
/// A `blocked` task is *recoverable* (an operator runs `boi unblock`), so the
/// orchestrator deliberately leaves the spec `running` — `all_tasks_settled`
/// stays false. The determined outcome here is the TASK's state, not the
/// spec's; the harness drives until the task reaches `blocked`.
#[tokio::test]
async fn test_l3_fixtures_03_caps_a_persistently_failing_task() {
    let mut script = HashMap::new();
    // `execute` ALWAYS fails — each call (every re-entry) fails.
    script.insert(
        "execute".to_owned(),
        vec![VerdictOutcome::Fail {
            error: "build broke".into(),
            why: "still broken".into(),
            fix: "try again".into(),
        }],
    );
    // `review_adjustment` ALWAYS Redo — re-enters `propose_adjustment`, the
    // edge the cap must still count.
    script.insert(
        "review_adjustment".to_owned(),
        vec![VerdictOutcome::Redo {
            reason: "fix incomplete".into(),
        }],
    );
    let exec: Arc<dyn PhaseExecutor> = Arc::new(CallIndexedExecutor::new(script));
    let run = super::harness::run_spec_until_task_blocked(
        super::harness::fixture_spec("03-failure-recovery"),
        exec,
    )
    .await;

    // The task ended `blocked{CapExceeded}` — the determined outcome.
    assert_eq!(
        run.task_state(0).await,
        "blocked",
        "a persistently failing task must block at the side-chain cap",
    );
    let reason: boi::types::reasons::BlockedReason = serde_json::from_value(
        repo::task_runtime::fetch(&run.pool, &run.dispatched.task_ids[0])
            .await
            .expect("task row")
            .blocked_reason
            .expect("a blocked task carries a reason"),
    )
    .expect("the blocked reason deserializes");
    let boi::types::reasons::BlockedReason::CapExceeded { loop_name, .. } = reason else {
        panic!("the side-chain cap must block with CapExceeded, got {reason:?}");
    };
    assert_eq!(
        loop_name, "task_adjust",
        "the tripped cap is the side-chain's"
    );
}

// ---------------------------------------------------------------------------
// 04 — the cross-provider spec.
// ---------------------------------------------------------------------------

/// `04-multi-provider` driven all-passing → the orchestrator records DISTINCT
/// `provider` values across `phase_runs` rows.
///
/// The `standard` pipeline's `[overrides.critique_plan.runtime]` runs
/// `critique_plan` against `openrouter`; every other phase runs `claude_code`.
/// This is a provider-routing-config assertion — real cross-provider EXECUTION
/// is the Docker E2E's domain (review (b) / S8).
#[tokio::test]
async fn test_l3_fixtures_04_records_distinct_providers() {
    let run = run_fixture("04-multi-provider", all_passing()).await;
    assert_eq!(
        run.spec_status().await,
        "completed",
        "04 must end `completed`",
    );
    let history = repo::phase_runs::fetch_history_for_spec(&run.pool, run.spec_id())
        .await
        .expect("phase-run history");
    let providers: std::collections::BTreeSet<&str> =
        history.iter().map(|r| r.provider.as_str()).collect();
    assert!(
        providers.len() >= 2,
        "04 must record at least two distinct providers across phase_runs — \
         the standard pipeline's critique_plan override makes one phase \
         cross-provider; got {providers:?}",
    );
    // The specific routing: `critique_plan` is the overridden one.
    assert!(
        history
            .iter()
            .any(|r| r.phase == "critique_plan" && r.provider == "openrouter"),
        "critique_plan must run against the overridden `openrouter` provider",
    );
}

// ---------------------------------------------------------------------------
// 05 — the plan-revision spec.
// ---------------------------------------------------------------------------

/// A [`PhaseExecutor`] that stages a deterministic plan-revision cycle for the
/// `05` fixture.
///
/// It models what a real worker fleet does, without racing a watcher:
///
/// - **`execute`, FIRST call** — the worker mid-phase files a blocking
///   `task_report`. The stream yields a `ReportReceived { blocking: true }`
///   and then *waits on the cancel token*: the orchestrator's `on_report`
///   blocks the task and cancels this very drain, so the stream ends
///   `Canceled` (a clean drain outcome — no spurious failure). This is
///   exactly a worker that calls the `task_report` MCP tool and is then
///   interrupted by the orchestrator.
/// - **`execute`, later calls** — the task was unblocked by the landed
///   revision and re-runs `execute`; it now passes.
/// - **`plan_revision`** — writes a [`PlanRevision`] artifact (one `AddTask`
///   edit) to the path the orchestrator reads
///   (`~/.boi/v2/revisions/<phase_run_id>.json`), then passes. A real
///   `plan_revision` worker writes that file via the `BOI_REVISION_ARTIFACT`
///   env (review D3); the L3 mock bypasses the worker, so the write is staged
///   here.
/// - **every other phase** — passes.
///
/// The report is filed BY the executor, deterministically, while the task is
/// genuinely mid-`execute` — no watcher racing the (near-instant) mocked
/// lifecycle.
struct RevisionWritingExecutor {
    /// `execute`-phase call counter — the first call files the report.
    execute_calls: Arc<std::sync::Mutex<usize>>,
}

impl RevisionWritingExecutor {
    fn new() -> Self {
        Self {
            execute_calls: Arc::new(std::sync::Mutex::new(0)),
        }
    }

    /// The path the orchestrator reads a `plan_revision` artifact from —
    /// `$HOME/.boi/v2/revisions/<phase_run_id>.json` (mirrors the orchestrator's
    /// own `revision_artifact_path`; `REVISIONS_DIR` is `~/.boi/v2/revisions`).
    fn artifact_path(phase_run_id: &PhaseRunId) -> std::path::PathBuf {
        let home = std::env::var("HOME").expect("$HOME is set");
        std::path::PathBuf::from(home)
            .join(".boi/v2/revisions")
            .join(format!("{phase_run_id}.json"))
    }

    /// A `PhaseCompleted { Passing }` stream event bound to `ctx`'s run.
    fn passing_completion(ctx: &PhaseContext) -> BoiEvent {
        BoiEvent::PhaseCompleted {
            phase_run_id: ctx.phase_run_id.clone(),
            spec_id: ctx.spec_id.clone(),
            task_id: ctx.task_id.clone(),
            phase: ctx.phase.clone(),
            verdict: boi::types::verdict::WorkerVerdict {
                synopsis: format!("scripted {}", ctx.phase),
                outcome: VerdictOutcome::Passing {
                    evidence: Evidence::default(),
                },
            },
            tokens_in: 0,
            tokens_out: 0,
            duration_ms: 0,
        }
    }
}

impl PhaseExecutor for RevisionWritingExecutor {
    fn execute(
        &self,
        phase: boi::config::PhaseDef,
        ctx: PhaseContext,
        cancel: CancellationToken,
    ) -> BoxStream<'static, BoiEvent> {
        match phase.name.as_str() {
            "plan_revision" => {
                // Write the artifact the orchestrator will read, then pass.
                let revision = PlanRevision {
                    edits: vec![PlanEdit::AddTask {
                        behavior: "the prerequisite the report asked for".into(),
                        verifications: vec![],
                        blocked_by: vec![],
                    }],
                };
                let path = Self::artifact_path(&ctx.phase_run_id);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).expect("create the revisions dir");
                }
                std::fs::write(
                    &path,
                    serde_json::to_vec(&revision).expect("PlanRevision serializes"),
                )
                .expect("write the plan-revision artifact");
                stream::iter(vec![Self::passing_completion(&ctx)]).boxed()
            }
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
                    // The post-revision re-execute — pass.
                    return stream::iter(vec![Self::passing_completion(&ctx)]).boxed();
                }
                // The first execute: the worker files a blocking report, then
                // the stream waits to be canceled (the orchestrator's
                // `on_report` blocks the task and cancels this drain). `ctx`
                // is not used past here in this branch — its fields are moved.
                let task_id = ctx.task_id.expect("execute is a task phase");
                let report = BoiEvent::ReportReceived {
                    spec_id: ctx.spec_id,
                    task_id,
                    kind: "scope_gap".to_owned(),
                    payload: serde_json::json!({ "detail": "a prerequisite is needed" }),
                    blocking: true,
                };
                // Yield the report, THEN park on the cancel token: the drain's
                // `select!` ends the stream `Canceled` once the orchestrator
                // cancels the task's drains (which `on_report` → `TaskBlocked`
                // does). `stream::once` of a future that resolves only on
                // cancel — after the report element — gives exactly that.
                let report_stream = stream::iter(vec![report]);
                let park = stream::once(async move {
                    cancel.cancelled().await;
                    // This element is produced only AFTER cancel — by then the
                    // drain's biased `select!` has already taken the cancel
                    // branch, so this is never actually yielded. It exists so
                    // the stream stays pending until cancel.
                    None
                })
                .filter_map(|x| async move { x });
                report_stream.chain(park).boxed()
            }
            // Every other phase — pass.
            _ => stream::iter(vec![Self::passing_completion(&ctx)]).boxed(),
        }
    }
}

/// `05-plan-revision`: the task files a blocking `task_report` mid-`execute`;
/// the plan layer runs the `plan_revision` worker (which writes an `AddTask`
/// revision); the revision lands, the triggering task is unblocked, both the
/// original and the added task end `passing`, the spec ends `completed`, and a
/// `spec_versions` v2 row exists.
///
/// The [`RevisionWritingExecutor`] files the report itself, deterministically,
/// from inside the first `execute` phase — no watcher racing the mocked
/// lifecycle.
#[tokio::test]
async fn test_l3_fixtures_05_plan_revision_adds_a_task_and_completes() {
    use super::harness::{fixture_spec, run_spec};

    let exec: Arc<dyn PhaseExecutor> = Arc::new(RevisionWritingExecutor::new());
    let run = run_spec(fixture_spec("05-plan-revision"), exec).await;

    assert_eq!(
        run.spec_status().await,
        "completed",
        "05 must end `completed` after the revision lands",
    );
    // The original (reporting) task ended `passing`.
    assert_eq!(
        run.task_state_by_ref("add-healthz").await,
        "passing",
        "05's original task must end `passing` once the revision unblocks it",
    );
    // The revision appended a second task — it too ended `passing`.
    let tasks = repo::task_runtime::tasks_for_spec(&run.pool, run.spec_id())
        .await
        .expect("task rows");
    assert_eq!(tasks.len(), 2, "the revision added a second task");
    for t in &tasks {
        assert_eq!(
            t.state, "passing",
            "every 05 task (original + added) must end `passing`",
        );
    }
    // A `spec_versions` v2 row exists — the revision appended it.
    assert!(
        repo::spec_versions::fetch_snapshot(&run.pool, run.spec_id(), 2)
            .await
            .is_ok(),
        "the plan revision must append a spec_versions v2 row",
    );
}
