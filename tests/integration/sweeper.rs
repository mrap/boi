//! Sweeper integration coverage for OBS-019 — the "worker died before its
//! first heartbeat" reconciliation gap.
//!
//! ## What this test proves end-to-end
//!
//! When an `execute` phase's worker process dies *before* it has emitted a
//! single heartbeat, the resulting `phase_runs` row sits with
//! `last_heartbeat_at IS NULL` and an older `started_at`. The OBS-019 incident
//! left such a row open for 88 minutes — the spec read as `running` while no
//! one was working on it. This integration test stands the real orchestrator,
//! real event bus, and a real (tempdir-backed) SQLite repo up against the
//! `boi::service::Sweeper`, runs a "trivial spec" (`01-typo-fix`) through the
//! happy path until the `execute` phase opens its row, simulates the worker
//! dying (the phase's stream parks on cancel — it never emits a verdict and
//! never heartbeats), then virtualises the sweeper's clock past the
//! abandonment threshold and asserts:
//!
//!   (a) the sweeper emitted a `TaskBlocked` for the dead phase (observed via
//!       a recording `EmitObserver` installed on the live bus);
//!   (b) the task surfaces as `blocked` AND the `phase_runs` row is closed
//!       (`completed_at IS NOT NULL`) — once the row is closed the dashboard's
//!       `any_open` derivation drops the spec off `running`, which is the
//!       contract's "surfaces the phase as `blocked`" disposition; AND
//!   (c) the whole test runs in under 10 s — the sweeper's
//!       `tick(virtual_future_now)` seam means the heartbeat threshold is
//!       satisfied virtually, never by a real-time wait.
//!
//! ## No daemon subprocess — per `hex me/learnings.md 2026-05-03`
//!
//! "Spawning the daemon against a per-test BOI_HOME" here means the in-process
//! `boi::service::{EventBus, Orchestrator, Sweeper}` triple wired up against a
//! tempdir-backed SQLite DB — exactly the L3 harness shape. No host
//! system-service mutation, no plist touch, no host-daemon process. The HARD
//! RULE from the learnings file is preserved.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

use async_trait::async_trait;
use boi::config::PhaseDef;
use boi::repo;
use boi::service::registry::{DaemonNotification, PhaseExecutor};
use boi::service::{EmitObserver, EventBus, ObserverError, Orchestrator, Sweeper};
use boi::types::context::PhaseContext;
use boi::types::event::BoiEvent;
use boi::types::ids::{PhaseRunId, SpecId};
use boi::types::verdict::{Evidence, VerdictOutcome, WorkerVerdict};
use chrono::{Duration as ChronoDuration, Utc};
use futures::stream::{self, BoxStream, StreamExt};
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::harness::{all_phases, build_dispatch_rows, fixture_spec, standard_pipeline};

/// The OBS-019 gap manifested with an effective 90 s threshold (3× heartbeat).
/// The test's virtual "now" jumps far past that so a single sweep tick catches
/// the row deterministically (no jitter, no edge case).
const SWEEP_THRESHOLD: StdDuration = StdDuration::from_secs(90);

/// How far into the future the sweeper's clock is virtualized. Anything
/// > `SWEEP_THRESHOLD` is enough; 600 s is comfortably over the cliff.
const VIRTUAL_ADVANCE: ChronoDuration = ChronoDuration::seconds(600);

/// Bound on how long the test polls for the orchestrator to reach `execute`.
const EXECUTE_START_DEADLINE: StdDuration = StdDuration::from_secs(8);

/// The contract's "under 10 seconds" wall-clock bound for the full test.
const HARD_BUDGET: StdDuration = StdDuration::from_secs(10);

// ---------------------------------------------------------------------------
// Per-test BOI_HOME — a std-only tempdir, cleaned on drop. Mirrors the helper
// in `harness.rs`; duplicated here so the sweeper test stays self-contained
// and the harness's struct visibility doesn't need widening.
// ---------------------------------------------------------------------------

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("boi-sweeper-it-{}-{tag}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.path));
    }
}

// ---------------------------------------------------------------------------
// A recording `EmitObserver` — visible from the integration crate. The
// in-crate `RecordingObserver` is `pub(crate)`, so we roll a thin equivalent
// here. The observer is registered on the LIVE bus the orchestrator + sweeper
// share, so every emit (sweeper-produced or otherwise) is recorded in order.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Recorder {
    events: Arc<Mutex<Vec<BoiEvent>>>,
}

impl Recorder {
    fn new() -> Self {
        Self::default()
    }

    fn seen(&self) -> Vec<BoiEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl EmitObserver for Recorder {
    async fn observe(&self, event: &BoiEvent) -> Result<(), ObserverError> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event.clone());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// A `PhaseExecutor` whose `execute` stream PARKS — the OBS-019 "the worker
// died before its first heartbeat" shape. Every other phase ends `Passing` so
// the orchestrator drives the spec all the way to `execute` before stalling.
//
// Why park, not return an empty stream: an empty stream would end the drain
// `CompletedWithoutVerdict`, which the orchestrator translates into a
// `TaskBlocked{ProviderFailed}` *itself* — not the sweeper. We want the
// sweeper to be the one that surfaces the block, so the test must hang the
// drain such that only the sweeper's `tick` advances state. The cancel branch
// lets the orchestrator's `cancel_task_drains` (run after the sweeper's
// `TaskBlocked`) tear the parked drain down cleanly.
// ---------------------------------------------------------------------------

struct DeadWorkerExecutor;

impl PhaseExecutor for DeadWorkerExecutor {
    fn execute(
        &self,
        phase: PhaseDef,
        ctx: PhaseContext,
        cancel: CancellationToken,
    ) -> BoxStream<'static, BoiEvent> {
        if phase.name == "execute" {
            // Park indefinitely — wake only on cancel. The drain has already
            // emitted `PhaseStarted` (the bus inserts the `phase_runs` row),
            // so the row exists with `started_at = now` and
            // `last_heartbeat_at = NULL`. No heartbeat will ever be recorded
            // for this run — the OBS-019 fingerprint.
            return stream::once(async move {
                cancel.cancelled().await;
                None
            })
            .filter_map(|x| async move { x })
            .boxed();
        }
        // Happy path for every other phase — emit one `PhaseCompleted{Passing}`.
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

// ---------------------------------------------------------------------------
// The test.
// ---------------------------------------------------------------------------

/// **OBS-019 integration regression.** A dispatched spec whose `execute` phase
/// worker dies before its first heartbeat must be swept and reconciled —
/// `TaskBlocked` emitted, task `blocked`, `phase_runs` row closed — within a
/// virtualised clock window, in under 10 s of wall time.
#[tokio::test]
async fn test_l2_sweeper_kills_worker_before_first_heartbeat_obs019() {
    let start = Instant::now();

    // (1) Per-test BOI_HOME — a tempdir-backed SQLite db + a tempdir
    // workspace. No host system-service mutation, no plist touch.
    let db_dir = TempDir::new("db");
    let workspace_dir = TempDir::new("workspace");
    let db_path = db_dir.path.join("boi.db");
    let db_url = format!("sqlite://{}?mode=rwc", db_path.display());
    let pool = repo::connect(&db_url).await.expect("tempdir db connects");

    // (2) Dispatch the trivial single-task fixture spec (`01-typo-fix`) — the
    // same five-table transaction `boi dispatch` runs in production.
    let spec = fixture_spec("01-typo-fix");
    let (mut rows, dispatched) = build_dispatch_rows(&spec);
    if let Some(contract) = rows.snapshot.get_mut("spec_contract")
        && let Some(obj) = contract.as_object_mut()
    {
        obj.insert(
            "workspace".to_owned(),
            serde_json::Value::String(workspace_dir.path.display().to_string()),
        );
    }
    repo::insert_dispatch(&pool, &rows, Utc::now())
        .await
        .expect("insert_dispatch seeds the five tables");

    let spec_id = dispatched.spec_id.clone();
    let task_id = dispatched.task_ids[0].clone();

    // (3) Real event bus, with a recording observer installed so the sweeper-
    // produced events become inspectable.
    let recorder = Recorder::new();
    let bus = Arc::new(EventBus::new(
        pool.clone(),
        vec![Arc::new(recorder.clone())],
    ));

    // (4) Real orchestrator + the DeadWorkerExecutor.
    let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonNotification>(1024);
    let executor: Arc<dyn PhaseExecutor> = Arc::new(DeadWorkerExecutor);
    let orchestrator = Orchestrator::new(
        Arc::clone(&bus),
        pool.clone(),
        executor,
        standard_pipeline(),
        all_phases(),
        daemon_tx.clone(),
        daemon_rx,
    )
    .expect("the standard pipeline validates");
    let orch_handle = tokio::spawn(orchestrator.run());

    // (5) Kick the spec off — the two-step the daemon's `Dispatch` handler
    // takes (emit on the bus, then notify the orchestrator).
    let started = BoiEvent::SpecStarted {
        spec_id: spec_id.clone(),
    };
    bus.emit(&started)
        .await
        .expect("SpecStarted is a legal queued → running transition");
    daemon_tx
        .send(DaemonNotification::Event(started))
        .await
        .expect("the orchestrator channel accepts SpecStarted");

    // (6) Poll for the orchestrator to walk through the spec-level phases and
    // open the task's `execute` phase_run — that is the OBS-019 entry point.
    // The poll deadline is short (8 s) because the spec-level phases all end
    // Passing in microseconds; if it ever exceeds this, the harness is wedged
    // and the test should fail loudly rather than sleep its way through.
    let execute_pr_id = wait_for_open_execute_phase_run(&pool, &spec_id).await;

    // Sanity: pre-tick the row is open AND its heartbeat is NULL — the exact
    // OBS-019 fingerprint the diagnosis pinpoints.
    let pre = repo::phase_runs::fetch(&pool, &execute_pr_id)
        .await
        .expect("execute phase_run row is fetchable");
    assert!(pre.is_open(), "pre-sweep: the execute phase_run is open");
    assert!(
        pre.last_heartbeat_at.is_none(),
        "pre-sweep: last_heartbeat_at IS NULL (worker died before first heartbeat \
         — OBS-019 fingerprint)",
    );

    // (7) The sweeper, bound to the SAME bus + DB + daemon_tx the orchestrator
    // is using. `tick(virtual_now)` is the clock-injection seam (`now` is
    // already a `DateTime<Utc>` parameter — no real-time wait needed). Push
    // `now` far past the threshold so the sweep is deterministic.
    let sweeper = Sweeper {
        bus: Arc::clone(&bus),
        daemon_tx: daemon_tx.clone(),
        pool: pool.clone(),
        threshold: SWEEP_THRESHOLD,
        // This test exercises the heartbeat-stale path only; set the wall-clock
        // budget far above VIRTUAL_ADVANCE so the over-budget pass never fires
        // here (it has its own unit-test coverage) and the row is swept once.
        wall_clock_budget: StdDuration::from_secs(86_400),
        // No reclaimer — the auto-clean pass has its own L3 coverage
        // (`tests/integration/reclaim.rs`).
        reclaimer: None,
        auto_clean_after: StdDuration::from_secs(7 * 24 * 60 * 60),
        auto_clean_pass_interval: StdDuration::ZERO,
        last_auto_clean_pass: std::sync::Mutex::new(None),
        worktree_root: None,
    };
    let virtual_now = Utc::now() + VIRTUAL_ADVANCE;
    let swept = sweeper
        .tick(virtual_now)
        .await
        .expect("sweeper tick succeeds");
    assert_eq!(
        swept, 1,
        "the sweeper must find and act on exactly one abandoned phase_run",
    );

    // (a) The sweeper emitted a `TaskBlocked` for OUR dead task.
    let task_blocked_for_ours = recorder.seen().iter().any(|e| {
        matches!(
            e,
            BoiEvent::TaskBlocked {
                task_id: t,
                spec_id: s,
                ..
            } if t == &task_id && s == &spec_id
        )
    });
    assert!(
        task_blocked_for_ours,
        "(a) the sweeper must emit `TaskBlocked` for the dead `execute` phase \
         — recorded events: {:?}",
        recorder.seen(),
    );

    // (b) The task surfaces as `blocked` AND the phase_run row is closed.
    // Once the row is closed, the dashboard's `any_open` derivation drops the
    // spec off `running` — the contract's "surfaces the phase as `blocked`"
    // path. (The literal `spec_runtime.status` move is out of scope for the
    // OBS-019 fix; see the diagnosis doc.)
    let task_state = repo::task_runtime::fetch(&pool, &task_id)
        .await
        .expect("task_runtime row exists")
        .state;
    assert_eq!(
        task_state, "blocked",
        "(b) the dead-worker task must be surfaced as blocked",
    );
    let post = repo::phase_runs::fetch(&pool, &execute_pr_id)
        .await
        .expect("phase_run row is still fetchable");
    assert!(
        post.completed_at.is_some(),
        "(b) the swept phase_run row must be closed — pre-fix this stays NULL \
         and the spec sits `running` indefinitely (OBS-019)",
    );

    // (8) Tear the orchestrator down. (Same pattern as the L3 harness — the
    // orchestrator's own `daemon_tx` clone never drops, so `run()` never
    // returns on its own; the test owns shutdown via `abort()`.)
    orch_handle.abort();
    drop(orch_handle);

    // (c) The whole test, including the simulated 600 s clock advance, fit
    // inside a 10 s wall-clock budget — the clock-injection seam works.
    let elapsed = start.elapsed();
    assert!(
        elapsed < HARD_BUDGET,
        "(c) the test must finish in < {HARD_BUDGET:?} — actually took {elapsed:?}",
    );
}

/// Poll `phase_runs` for the FIRST open `execute` row of `spec_id`. Returns
/// once one exists; panics loud (rather than sleeps quietly) on timeout.
async fn wait_for_open_execute_phase_run(pool: &SqlitePool, spec_id: &SpecId) -> PhaseRunId {
    let deadline = Instant::now() + EXECUTE_START_DEADLINE;
    loop {
        let history = repo::phase_runs::fetch_history_for_spec(pool, spec_id)
            .await
            .expect("phase_runs history is fetchable");
        for row in history {
            if row.phase == "execute" && row.is_open() {
                return PhaseRunId::new(&row.id).expect("phase_run id is a valid PhaseRunId");
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "the `execute` phase never opened within {EXECUTE_START_DEADLINE:?} \
                 — orchestrator wedged before reaching the OBS-019 entry point",
            );
        }
        tokio::time::sleep(StdDuration::from_millis(20)).await;
    }
}
