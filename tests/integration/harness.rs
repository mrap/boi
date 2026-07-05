//! The L3 orchestrator-integration harness (Task 10.2).
//!
//! Dispatches a fixture spec against a tempdir workspace + a tempdir `boi.db`,
//! drives the **real** orchestrator / bus / repo to quiescence with a
//! [`MockExecutor`], and exposes the resulting DB for assertions.
//!
//! ## What "drive to quiescence" means here
//!
//! Production runs the orchestrator as one long-lived `tokio` task; the
//! orchestrator's own `daemon_tx` clone never drops, so its `run()` loop never
//! sees a closed channel and never returns on its own (`boot` shuts it down
//! with `JoinHandle::abort()`). The L3 harness mirrors that exactly:
//!
//! 1. seed the spec's structural rows (`insert_dispatch` — the same five-table
//!    transaction `boi dispatch` uses);
//! 2. spawn `orchestrator.run()`;
//! 3. emit `SpecStarted` through the bus, then notify the orchestrator (the
//!    daemon's `Dispatch` handler does exactly this two-step);
//! 4. poll `spec_runtime.status` until the spec reaches a terminal state
//!    (`completed` / `failed` / `canceled`) — that IS quiescence: no further
//!    phase will run;
//! 5. `abort()` the orchestrator task.
//!
//! Hermetic — no subprocess, no live LLM. The mock bypasses every `runtime/`
//! provider module by design (review (b)); that is the L3 contract.

#![allow(dead_code)] // each L3 module uses a subset of this harness surface.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use boi::config::{self, PhaseDef, PipelineDef, Spec};
use boi::repo::{self, DispatchDep, DispatchRows, DispatchTask};
use boi::service::registry::testkit::MockExecutor;
use boi::service::registry::{DaemonNotification, PhaseExecutor};
use boi::service::{EventBus, Orchestrator};
use boi::types::context::TaskContract;
use boi::types::event::BoiEvent;
use boi::types::ids::{SpecId, TaskId};
use sqlx::SqlitePool;
use tokio::sync::mpsc;

/// How long the harness waits for a spec to reach a terminal state before it
/// declares the run wedged. The L3 walk is in-memory + mocked — a real run
/// settles in milliseconds; this is a generous backstop, not an expected wait.
const QUIESCENCE_TIMEOUT: Duration = Duration::from_secs(20);

/// The poll interval while waiting for the spec to settle.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A throwaway directory removed on drop — `std`-only (BOI takes no
/// `tempfile` dependency; the in-crate tests use the same pattern).
pub(crate) struct TempDir {
    /// The directory path.
    pub(crate) path: PathBuf,
}

impl TempDir {
    /// Create a uniquely-named throwaway directory under the system temp dir.
    pub(crate) fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("boi-l3-{}-{tag}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.path));
    }
}

/// Every `standard`-pipeline phase fixture, parsed from `tests/fixtures/phases/`.
///
/// Mirrors the in-crate orchestrator tests' `all_phases` — the 16 phase TOMLs
/// (G13.2) the `standard` pipeline can reference.
pub(crate) fn all_phases() -> HashMap<String, PhaseDef> {
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
        .unwrap_or_else(|e| panic!("read phase fixture {name}: {e}"));
        map.insert(
            (*name).to_owned(),
            config::parse_phase(&toml).unwrap_or_else(|e| panic!("parse phase {name}: {e}")),
        );
    }
    map
}

/// The `standard` pipeline, parsed from `tests/fixtures/pipelines/standard.toml`.
pub(crate) fn standard_pipeline() -> PipelineDef {
    let toml = std::fs::read_to_string(format!(
        "{}/tests/fixtures/pipelines/standard.toml",
        env!("CARGO_MANIFEST_DIR"),
    ))
    .expect("read standard.toml");
    config::parse_pipeline(&toml).expect("parse standard.toml")
}

/// Read a §13 integration fixture spec by stem (`tests/fixtures/specs/`).
pub(crate) fn fixture_spec(stem: &str) -> Spec {
    let toml = std::fs::read_to_string(format!(
        "{}/tests/fixtures/specs/{stem}.toml",
        env!("CARGO_MANIFEST_DIR"),
    ))
    .unwrap_or_else(|e| panic!("read fixture spec {stem}: {e}"));
    config::parse_spec(&toml).unwrap_or_else(|e| panic!("parse fixture spec {stem}: {e}"))
}

/// The structural rows of a dispatched fixture — the minted ids + the
/// G21.3-shaped v1 snapshot.
pub(crate) struct DispatchedSpec {
    /// The minted spec id.
    pub(crate) spec_id: SpecId,
    /// The minted task ids, in author order.
    pub(crate) task_ids: Vec<TaskId>,
    /// Author-ref → minted-task-id (only refs the spec named).
    pub(crate) ref_to_id: HashMap<String, TaskId>,
}

/// Build the `boi dispatch` structural payload for a parsed [`Spec`].
///
/// This replicates `cli::dispatch::build_dispatch` (which is private to the
/// `cli` module): mint a [`SpecId`] + a [`TaskId`] per task, resolve each
/// authored `blocked_by` ref to its minted id, and build the G21.3 snapshot
/// `{ spec_contract, task_contracts }` the orchestrator's `run_phase`
/// re-hydrates against. Only `boi`'s public API is used.
pub(crate) fn build_dispatch_rows(spec: &Spec) -> (DispatchRows, DispatchedSpec) {
    let spec_id = SpecId::new(repo::random_id('S')).expect("minted spec id is valid");

    let mut task_ids = Vec::with_capacity(spec.tasks.len());
    let mut ref_to_id: HashMap<String, TaskId> = HashMap::new();
    for task in &spec.tasks {
        let task_id = TaskId::new(repo::random_id('T')).expect("minted task id is valid");
        if let Some(r) = &task.task_ref {
            ref_to_id.insert(r.clone(), task_id.clone());
        }
        task_ids.push(task_id);
    }

    let mut task_contracts = serde_json::Map::new();
    let mut dispatch_tasks = Vec::with_capacity(task_ids.len());
    let mut deps = Vec::new();
    for (idx, task_def) in spec.tasks.iter().enumerate() {
        let task_id = task_ids[idx].clone();
        let contract = TaskContract {
            behavior: task_def.behavior.clone(),
            verifications: task_def.verifications.clone(),
        };
        task_contracts.insert(
            task_id.as_str().to_owned(),
            serde_json::to_value(&contract).expect("task contract serializes"),
        );
        dispatch_tasks.push(DispatchTask {
            task_id: task_id.clone(),
            task_ref: task_def.task_ref.clone(),
        });
        for dep_ref in &task_def.blocked_by {
            let depends_on = ref_to_id
                .get(dep_ref)
                .unwrap_or_else(|| panic!("blocked_by `{dep_ref}` resolves — validated at parse"))
                .clone();
            deps.push(DispatchDep {
                task_id: task_id.clone(),
                depends_on,
            });
        }
    }

    let delivery_str = match spec.delivery {
        boi::config::Delivery::Merge => "merge",
        boi::config::Delivery::Pr => "pr",
        boi::config::Delivery::BranchOnly => "branch-only",
    };
    let snapshot = serde_json::json!({
        "title": spec.title,
        "delivery": delivery_str,
        "spec_contract": serde_json::to_value(&spec.contract).expect("spec contract serializes"),
        "task_contracts": serde_json::Value::Object(task_contracts),
    });

    let rows = DispatchRows {
        spec_id: spec_id.clone(),
        snapshot,
        tasks: dispatch_tasks,
        deps,
    };
    let dispatched = DispatchedSpec {
        spec_id,
        task_ids,
        ref_to_id,
    };
    (rows, dispatched)
}

/// A live L3 run — the tempdir-backed DB pool plus the dispatched-spec id map.
///
/// The `TempDir`s are held so the workspace + DB directory survive for the
/// run's lifetime and are cleaned on drop.
pub(crate) struct L3Run {
    /// The SQLite pool over the tempdir `boi.db`.
    pub(crate) pool: SqlitePool,
    /// The dispatched spec's minted ids.
    pub(crate) dispatched: DispatchedSpec,
    _db_dir: TempDir,
    _workspace_dir: TempDir,
}

impl L3Run {
    /// The dispatched spec's id.
    pub(crate) fn spec_id(&self) -> &SpecId {
        &self.dispatched.spec_id
    }

    /// The current `spec_runtime.status` string.
    pub(crate) async fn spec_status(&self) -> String {
        repo::spec_runtime::fetch(&self.pool, self.spec_id())
            .await
            .expect("spec_runtime row exists")
            .status
    }

    /// The current `task_runtime.state` of the task at author index `idx`.
    pub(crate) async fn task_state(&self, idx: usize) -> String {
        repo::task_runtime::fetch(&self.pool, &self.dispatched.task_ids[idx])
            .await
            .expect("task_runtime row exists")
            .state
    }

    /// The current `task_runtime.state` of the task with author ref `r`.
    pub(crate) async fn task_state_by_ref(&self, r: &str) -> String {
        let tid = self
            .dispatched
            .ref_to_id
            .get(r)
            .unwrap_or_else(|| panic!("no task with ref `{r}`"));
        repo::task_runtime::fetch(&self.pool, tid)
            .await
            .expect("task_runtime row exists")
            .state
    }
}

/// Dispatch a fixture and drive the orchestrator to quiescence with the given
/// [`PhaseExecutor`] (a [`MockExecutor`] in every L3 test).
///
/// Returns the settled [`L3Run`]. Panics if the spec does not reach a terminal
/// state within [`QUIESCENCE_TIMEOUT`] — a wedged orchestrator is a hard test
/// failure, never a silent hang.
pub(crate) async fn run_fixture(fixture_stem: &str, executor: Arc<dyn PhaseExecutor>) -> L3Run {
    run_spec(fixture_spec(fixture_stem), executor).await
}

/// Dispatch an already-parsed [`Spec`] and drive to quiescence.
///
/// Factored out of [`run_fixture`] so a test can mutate a fixture before
/// dispatch (e.g. the failure-path tests that need a bespoke task graph).
pub(crate) async fn run_spec(spec: Spec, executor: Arc<dyn PhaseExecutor>) -> L3Run {
    run_inner(spec, executor, false).await
}

/// A spec dispatched into a tempdir DB — the structural rows seeded, the
/// orchestrator NOT run.
///
/// For failure-path tests that drive a recovery / operator-command path
/// directly (`recover_after_crash`, a `DaemonState` command) rather than the
/// orchestrator's pipeline. The `TempDir` is held so the DB survives the run.
pub(crate) struct SeededSpec {
    /// The tempdir-backed pool.
    pub(crate) pool: SqlitePool,
    /// The dispatched spec's id.
    pub(crate) spec_id: SpecId,
    _db_dir: TempDir,
}

/// Dispatch a fixture's structural rows into a fresh tempdir DB — `specs` +
/// `spec_versions` + `spec_runtime` + `task_runtime` + `task_deps`, exactly the
/// `insert_dispatch` transaction — WITHOUT running the orchestrator.
pub(crate) async fn seed_dispatched_spec(fixture_stem: &str) -> SeededSpec {
    let db_dir = TempDir::new("seed-db");
    let db_path = db_dir.path.join("boi.db");
    let db_url = format!("sqlite://{}?mode=rwc", db_path.display());
    let pool = repo::connect(&db_url)
        .await
        .expect("connect + migrate the tempdir boi.db");
    let (rows, dispatched) = build_dispatch_rows(&fixture_spec(fixture_stem));
    repo::insert_dispatch(&pool, &rows, chrono::Utc::now())
        .await
        .expect("insert_dispatch seeds the five tables");
    SeededSpec {
        pool,
        spec_id: dispatched.spec_id,
        _db_dir: db_dir,
    }
}

/// Dispatch a [`Spec`], drive to quiescence, AND inject one blocking
/// `task_report` for the spec's first task as soon as it reaches `active`.
///
/// The L3 [`MockExecutor`] has no worker that calls the `task_report` MCP
/// tool, so a plan-revision flow cannot otherwise be exercised. This stages
/// the report exactly: a watcher polls the first task until it is `active`,
/// then emits a `ReportReceived { blocking: true }` through the bus + channel
/// — the identical two-step the daemon's `mcp-serve`-forwarded report takes.
/// The orchestrator then blocks the task and runs `plan_revision`.
pub(crate) async fn run_spec_with_blocking_report(
    spec: Spec,
    executor: Arc<dyn PhaseExecutor>,
) -> L3Run {
    run_inner(spec, executor, true).await
}

/// Dispatch a [`Spec`] and drive until its FIRST task reaches `blocked`.
///
/// A permanently-`blocked` task is *recoverable* (an operator runs
/// `boi unblock`), so the orchestrator deliberately leaves the spec `running`
/// — `all_tasks_settled` is false while any task is `blocked`, so the spec
/// never resumes past `<tasks>`. "Spec terminal" is therefore the wrong
/// quiescence gate for a block-and-stay fixture (e.g. `03`'s `CapExceeded`
/// case); this variant gates on the task reaching `blocked` instead.
pub(crate) async fn run_spec_until_task_blocked(
    spec: Spec,
    executor: Arc<dyn PhaseExecutor>,
) -> L3Run {
    run_inner_until(spec, executor, false, Quiescence::FirstTaskBlocked).await
}

/// What [`run_inner_until`] polls for as the run's quiescence signal.
#[derive(Clone, Copy)]
enum Quiescence {
    /// The spec reached a terminal status (`completed` / `failed` / `canceled`).
    SpecTerminal,
    /// The spec's first task reached `blocked` (the spec stays `running` — a
    /// blocked task is operator-recoverable, not terminal).
    FirstTaskBlocked,
}

/// The shared run body for [`run_spec`] / [`run_spec_with_blocking_report`].
///
/// `inject_report` gates the plan-revision report watcher (step 5b).
async fn run_inner(spec: Spec, executor: Arc<dyn PhaseExecutor>, inject_report: bool) -> L3Run {
    run_inner_until(spec, executor, inject_report, Quiescence::SpecTerminal).await
}

/// A LIVE L3 run — dispatched and started, the real orchestrator `run()` loop
/// still executing.
///
/// The settled-run harness ([`run_inner_until`]) is a thin wrapper over this;
/// the operator-recovery L3 tests (audit A2 — design §6 recovery table) use it
/// directly to drive a mid-run `boi unblock` against a blocked-but-`running`
/// spec, then wait for the revived run to reach a terminal state.
pub(crate) struct LiveL3Run {
    /// The SQLite pool over the tempdir `boi.db`.
    pub(crate) pool: SqlitePool,
    /// The dispatched spec's minted ids.
    pub(crate) dispatched: DispatchedSpec,
    /// The run's event bus — operator-command mirrors emit through it.
    pub(crate) bus: Arc<EventBus>,
    /// The orchestrator-notification sender (the daemon's half).
    pub(crate) daemon_tx: mpsc::Sender<DaemonNotification>,
    orch_handle: tokio::task::JoinHandle<()>,
    _db_dir: TempDir,
    _workspace_dir: TempDir,
}

impl LiveL3Run {
    /// Dispatch `spec` into a fresh tempdir DB and start the REAL orchestrator
    /// run-loop — steps (1)–(5) of the settled-run harness, without waiting
    /// for any quiescence signal.
    pub(crate) async fn start(spec: Spec, executor: Arc<dyn PhaseExecutor>) -> Self {
        let db_dir = TempDir::new("db");
        let workspace_dir = TempDir::new("workspace");

        // (1) — a tempdir-backed SQLite DB. `repo::connect` runs the migrations.
        let db_path = db_dir.path.join("boi.db");
        let db_url = format!("sqlite://{}?mode=rwc", db_path.display());
        let pool = repo::connect(&db_url)
            .await
            .expect("connect + migrate the tempdir boi.db");

        // (2) — the structural insert (the same five-table transaction
        // `boi dispatch` runs).
        let (mut rows, dispatched) = build_dispatch_rows(&spec);
        // Point the snapshot's workspace at the tempdir so a phase that reads
        // it never touches a real repo.
        if let Some(contract) = rows.snapshot.get_mut("spec_contract")
            && let Some(obj) = contract.as_object_mut()
        {
            obj.insert(
                "workspace".to_owned(),
                serde_json::Value::String(workspace_dir.path.display().to_string()),
            );
        }
        repo::insert_dispatch(&pool, &rows, chrono::Utc::now())
            .await
            .expect("insert_dispatch seeds the five tables");

        // (3) — the event bus + the orchestrator. `boot` owns the channel and
        // hands both halves to `Orchestrator::new` (G16.3); the harness is
        // `boot`.
        let bus = Arc::new(EventBus::new(
            pool.clone(),
            vec![Arc::new(boi::service::NoopObserver)],
        ));
        let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonNotification>(1024);
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

        // (4) — spawn the REAL run-loop.
        let orch_handle = tokio::spawn(orchestrator.run());

        // (5) — start the spec. The daemon's `Dispatch` handler emits
        // `SpecStarted` on the bus (`queued → running`) THEN notifies the
        // orchestrator — the harness does the identical two-step.
        let started = BoiEvent::SpecStarted {
            spec_id: dispatched.spec_id.clone(),
        };
        bus.emit(&started)
            .await
            .expect("SpecStarted is a legal queued → running transition");
        daemon_tx
            .send(DaemonNotification::Event(started))
            .await
            .expect("the orchestrator channel accepts SpecStarted");

        Self {
            pool,
            dispatched,
            bus,
            daemon_tx,
            orch_handle,
            _db_dir: db_dir,
            _workspace_dir: workspace_dir,
        }
    }

    /// Poll until the task at author index `idx` is stably `blocked`. Returns
    /// `false` on timeout (the caller asserts — a silent wedge is never ok).
    pub(crate) async fn wait_task_blocked(&self, idx: usize) -> bool {
        wait_for_task_blocked(&self.pool, &self.dispatched.task_ids[idx]).await
    }

    /// Poll until the spec reaches a terminal status. Returns `false` on
    /// timeout.
    pub(crate) async fn wait_spec_terminal(&self) -> bool {
        wait_for_terminal(&self.pool, &self.dispatched.spec_id).await
    }

    /// Mirror `boi unblock <task_id> [--reset-counter]` — exactly the daemon
    /// `handle_unblock` sequence: optionally zero the task's iteration
    /// counters FIRST (so a `CapExceeded` task does not immediately
    /// re-block), then emit `TaskUnblocked` on the bus and notify the
    /// orchestrator (the emit-then-notify two-step every operator command
    /// takes).
    pub(crate) async fn unblock(&self, idx: usize, reset_counter: bool) {
        let task_id = self.dispatched.task_ids[idx].clone();
        if reset_counter {
            repo::task_runtime::reset_iterations(&self.pool, &task_id)
                .await
                .expect("reset_iterations succeeds for an existing task");
        }
        let event = BoiEvent::TaskUnblocked {
            spec_id: self.dispatched.spec_id.clone(),
            task_id,
        };
        self.bus
            .emit(&event)
            .await
            .expect("TaskUnblocked is a legal blocked → active transition");
        self.daemon_tx
            .send(DaemonNotification::Event(event))
            .await
            .expect("the orchestrator channel accepts TaskUnblocked");
    }

    /// Shut the orchestrator down and settle into an [`L3Run`] for assertions
    /// (production `boot` does the same `abort()`; the orchestrator's own
    /// `daemon_tx` clone never drops, so `run()` never returns on its own).
    pub(crate) fn settle(self) -> L3Run {
        self.orch_handle.abort();
        drop(self.orch_handle);
        L3Run {
            pool: self.pool,
            dispatched: self.dispatched,
            _db_dir: self._db_dir,
            _workspace_dir: self._workspace_dir,
        }
    }
}

/// The shared run body, parameterized on the quiescence signal.
async fn run_inner_until(
    spec: Spec,
    executor: Arc<dyn PhaseExecutor>,
    inject_report: bool,
    quiescence: Quiescence,
) -> L3Run {
    let live = LiveL3Run::start(spec, executor).await;

    // (5b) — optionally, the plan-revision report injector.
    let report_watcher = if inject_report {
        Some(tokio::spawn(inject_blocking_report(
            Arc::clone(&live.bus),
            live.daemon_tx.clone(),
            live.pool.clone(),
            live.dispatched.spec_id.clone(),
            live.dispatched.task_ids[0].clone(),
        )))
    } else {
        None
    };

    // (6) — poll for the configured quiescence signal.
    let settled = match quiescence {
        Quiescence::SpecTerminal => live.wait_spec_terminal().await,
        Quiescence::FirstTaskBlocked => live.wait_task_blocked(0).await,
    };

    // (7) — shut the orchestrator down. The report watcher is aborted too —
    // if the task never reached `active` it is still polling.
    if let Some(w) = report_watcher {
        w.abort();
        drop(w);
    }
    let run = live.settle();

    if !settled {
        // On a wedge, dump the spec/task/phase-run state so the failure is
        // diagnosable rather than a bare "wedged".
        if let Ok(s) = repo::spec_runtime::fetch(&run.pool, &run.dispatched.spec_id).await {
            eprintln!(
                "WEDGE spec status={} failure={:?}",
                s.status, s.failure_reason
            );
        }
        if let Ok(tasks) =
            repo::task_runtime::tasks_for_spec(&run.pool, &run.dispatched.spec_id).await
        {
            for t in tasks {
                eprintln!(
                    "WEDGE task {} state={} blocked={:?}",
                    t.task_id, t.state, t.blocked_reason
                );
            }
        }
        if let Ok(hist) =
            repo::phase_runs::fetch_history_for_spec(&run.pool, &run.dispatched.spec_id).await
        {
            for h in hist {
                eprintln!(
                    "WEDGE phase={} iter={} task={:?} open={}",
                    h.phase,
                    h.phase_iteration,
                    h.task_id,
                    h.is_open()
                );
            }
        }
    }
    assert!(
        settled,
        "the run did not reach quiescence within {QUIESCENCE_TIMEOUT:?} — \
         the orchestrator is wedged",
    );

    run
}

/// Poll `task_id` until it is `active`, then emit one blocking
/// `ReportReceived` for it through the bus + the orchestrator channel.
///
/// This is what the daemon's `mcp-serve`-forwarded `task_report` does — emit
/// the event on the bus, then notify the orchestrator. The orchestrator's
/// `on_report_received` then blocks the task and runs `plan_revision`.
async fn inject_blocking_report(
    bus: Arc<EventBus>,
    daemon_tx: mpsc::Sender<DaemonNotification>,
    pool: SqlitePool,
    spec_id: SpecId,
    task_id: TaskId,
) {
    // Wait for the task to reach `active` — a report can only block an
    // `active` task (`active → blocked` is the legal transition).
    let deadline = std::time::Instant::now() + QUIESCENCE_TIMEOUT;
    loop {
        if let Ok(row) = repo::task_runtime::fetch(&pool, &task_id).await
            && row.state == "active"
        {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return; // the task never activated — the run will time out loudly
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    let report = BoiEvent::ReportReceived {
        spec_id,
        task_id,
        kind: "scope_gap".to_owned(),
        payload: serde_json::json!({ "detail": "a prerequisite task is needed" }),
        blocking: true,
    };
    // The two-step: emit on the bus, then notify the orchestrator. A failure
    // here is benign — the run will simply time out, surfacing loudly — but
    // the failed send is logged, never silently discarded.
    if bus.emit(&report).await.is_ok()
        && daemon_tx
            .send(DaemonNotification::Event(report))
            .await
            .is_err()
    {
        eprintln!("report injector: orchestrator channel closed before the report landed");
    }
}

/// Poll `spec_runtime.status` until it is terminal or [`QUIESCENCE_TIMEOUT`]
/// elapses. Returns `true` if the spec settled, `false` on timeout.
async fn wait_for_terminal(pool: &SqlitePool, spec_id: &SpecId) -> bool {
    let deadline = std::time::Instant::now() + QUIESCENCE_TIMEOUT;
    loop {
        let status = repo::spec_runtime::fetch(pool, spec_id)
            .await
            .expect("spec_runtime row exists")
            .status;
        if matches!(status.as_str(), "completed" | "failed" | "canceled") {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Poll `task_runtime.state` until `task_id` is `blocked` or
/// [`QUIESCENCE_TIMEOUT`] elapses. Returns `true` if it blocked.
///
/// `blocked` is also reached transiently in some flows (a `MergeConflict` that
/// a later `boi unblock` would clear), so this is paired with a settling
/// re-check: once `blocked` is observed, a second poll one interval later
/// confirms it stayed `blocked` — a still-`blocked` task with no in-flight
/// progress is genuinely quiescent.
async fn wait_for_task_blocked(pool: &SqlitePool, task_id: &TaskId) -> bool {
    let deadline = std::time::Instant::now() + QUIESCENCE_TIMEOUT;
    loop {
        let state = repo::task_runtime::fetch(pool, task_id)
            .await
            .expect("task_runtime row exists")
            .state;
        if state == "blocked" {
            // Confirm it stays blocked across one more interval.
            tokio::time::sleep(POLL_INTERVAL).await;
            let again = repo::task_runtime::fetch(pool, task_id)
                .await
                .expect("task_runtime row exists")
                .state;
            if again == "blocked" {
                return true;
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// A [`MockExecutor`] that ends every phase `Passing` — the happy-path
/// executor for the full-pipeline walk.
pub(crate) fn all_passing() -> Arc<dyn PhaseExecutor> {
    Arc::new(MockExecutor::all_passing())
}

/// A [`PhaseExecutor`] whose per-phase script is consumed **per call** — the
/// Nth call to a phase emits the Nth scripted outcome (the last entry is
/// reused for any further calls).
///
/// ## Why not `MockExecutor`
///
/// `service::testkit::MockExecutor` emits a phase's *entire* `Vec<ScriptedEvent>`
/// as ONE stream — so scripting `execute` with `[Fail, Passing]` makes a
/// SINGLE `execute` run yield two terminal `PhaseCompleted`s, spawning two
/// concurrent task lifecycles. Under the in-crate tests' synchronous `drive()`
/// helper that happens to converge; under the real `run()` loop with real
/// spawned drains it is a race. The plan-mandated `03` outcome ("`execute`
/// fails ONCE, then passes") needs *call*-indexed scripting — the first
/// `execute` CALL fails, the second passes. `CallIndexedExecutor` is that:
/// one outcome per call, exactly the orchestrator's per-phase-run semantics.
pub(crate) struct CallIndexedExecutor {
    /// Phase name → the per-call outcome sequence.
    script: HashMap<String, Vec<boi::types::verdict::VerdictOutcome>>,
    /// Per-phase call counter.
    calls: Arc<std::sync::Mutex<HashMap<String, usize>>>,
}

impl CallIndexedExecutor {
    /// A new executor with the given `phase name → per-call outcomes` script.
    /// A phase with no script entry ends `Passing` on every call (the
    /// happy-path default).
    pub(crate) fn new(script: HashMap<String, Vec<boi::types::verdict::VerdictOutcome>>) -> Self {
        Self {
            script,
            calls: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }
}

impl PhaseExecutor for CallIndexedExecutor {
    fn execute(
        &self,
        phase: boi::config::PhaseDef,
        ctx: boi::types::context::PhaseContext,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> futures::stream::BoxStream<'static, BoiEvent> {
        use boi::types::verdict::{Evidence, VerdictOutcome, WorkerVerdict};
        use futures::stream::StreamExt;

        // The call index for this phase (0-based), then bump it.
        let idx = {
            let mut calls = self
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let n = calls.entry(phase.name.clone()).or_insert(0);
            let idx = *n;
            *n += 1;
            idx
        };
        // The Nth call's outcome — the last entry is reused past the end; an
        // unscripted phase is `Passing`.
        let outcome = self
            .script
            .get(&phase.name)
            .and_then(|seq| seq.get(idx).or_else(|| seq.last()).cloned())
            .unwrap_or(VerdictOutcome::Passing {
                evidence: Evidence::default(),
            });
        let completed = BoiEvent::PhaseCompleted {
            phase_run_id: ctx.phase_run_id.clone(),
            spec_id: ctx.spec_id.clone(),
            task_id: ctx.task_id.clone(),
            phase: ctx.phase.clone(),
            verdict: WorkerVerdict {
                synopsis: format!("call-indexed {}#{idx}", ctx.phase),
                outcome,
            },
            tokens_in: 0,
            tokens_out: 0,
            duration_ms: 0,
        };
        futures::stream::iter(vec![completed]).boxed()
    }
}

// ---------------------------------------------------------------------------
// Harness self-tests — the harness itself is L3-tested before any fixture
// relies on it (a broken harness would silently false-green every L3 test).
// ---------------------------------------------------------------------------

/// The harness drives the trivial single-task fixture (`01-typo-fix`) to a
/// `completed` spec — proving the dispatch → run → quiescence loop works
/// end-to-end against the real orchestrator/bus/repo.
#[tokio::test]
async fn test_l3_harness_drives_a_fixture_to_completion() {
    let run = run_fixture("01-typo-fix", all_passing()).await;
    assert_eq!(
        run.spec_status().await,
        "completed",
        "the harness must drive 01-typo-fix to a completed spec",
    );
}

/// The harness seeds every structural row (`insert_dispatch`) before the
/// orchestrator runs — a `task_runtime` row exists for the fixture's task.
#[tokio::test]
async fn test_l3_harness_seeds_structural_rows() {
    let run = run_fixture("01-typo-fix", all_passing()).await;
    assert_eq!(run.dispatched.task_ids.len(), 1, "one task was dispatched");
    // The task row is reachable — `insert_dispatch` inserted it.
    let state = run.task_state(0).await;
    assert!(
        matches!(state.as_str(), "passing"),
        "the single task settled `passing`, got {state}",
    );
}
