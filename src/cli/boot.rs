//! `boi daemon` — boot the wired graph, run the shutdown supervisor, and
//! perform the daemon-crash restart-recovery pass.
//!
//! ## What `boot` assembles (Task 9.2)
//!
//! `boot` constructs the whole runtime graph **via the G16.2 public
//! constructors** — never a cross-module struct literal:
//!
//! 1. `init_tracing` → an [`OtelGuard`](crate::runtime::OtelGuard) (dropped
//!    last, while the tokio runtime is still alive — review S13).
//! 2. the `repo` connection pool (the SQLite DB is migrated by `repo::connect`).
//! 3. the bounded `mpsc::channel(1024)` — **`boot` owns it** (G16.3); it hands
//!    both halves to `Orchestrator::new` and keeps a `daemon_tx` clone for the
//!    sweeper + the control-socket handler.
//! 4. the [`EventBus`] wired with the production `OtelObserver` (observer-only;
//!    no emit bridges are wired).
//! 5. the **restart-recovery pass** (Task 9.3) — BEFORE the orchestrator is
//!    spawned.
//! 6. the [`Orchestrator`].
//! 7. three supervised tasks — the orchestrator `run`, the
//!    [`Sweeper`], the control-socket listener — each
//!    with its `JoinHandle` retained.
//!
//! `boot` then `tokio::select!`s the three handles against `ctrl_c()` /
//! `SIGTERM`; on a signal it fires a top-level
//! [`CancellationToken`], awaits the
//! handles under a grace timeout, drops the `OtelGuard`, and returns `Ok(())`.
//! A handle that resolved to a *panic* is [`BootError::OrchestratorPanicked`]
//! — never a silent hang (G21.4).
//!
//! ## The executor seam (G16.2)
//!
//! `boot` takes `Arc<dyn PhaseExecutor>` — production passes a
//! `RuntimeExecutor`; a test passes a `MockExecutor`. The signature is fixed
//! so the L2/L3 harness can drive a real boot with a mock executor.

use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::cli::control;
use crate::cli::daemon::DaemonState;
use crate::cli::paths::{self, PathError};
use crate::config::{self, LoadError};
use crate::repo;
use crate::repo::db::RepoError;
use crate::service::registry::DaemonNotification;
use crate::service::{EventBus, Orchestrator, OrchestratorInitError, PhaseExecutor, Sweeper};
use crate::types::event::BoiEvent;
use crate::types::ids::{SpecId, TaskId};
use crate::types::reasons::{BlockedReason, FailureReason};
use crate::types::state::SpecStatus;

/// `boi daemon` failed to boot or run.
///
/// Every fallible boot step has its own variant — `boot` has no `unwrap()` on
/// a fallible step (`-D warnings` + these `From` impls enforce it).
#[derive(Debug, thiserror::Error)]
pub enum BootError {
    /// The `~/.boi/v2/` path layout could not be resolved.
    #[error(transparent)]
    Path(#[from] PathError),
    /// OTel tracing failed to initialize.
    #[error("OTel init failed: {0}")]
    Otel(#[from] crate::runtime::OtelError),
    /// A repo-layer step failed (connecting the pool, the recovery pass).
    #[error("repo error: {0}")]
    Repo(#[from] RepoError),
    /// The phase / pipeline declarations could not be loaded.
    #[error("config load failed: {0}")]
    Config(#[from] LoadError),
    /// `~/.boi/v2/config.toml`'s `[worktree]` table is malformed — boot
    /// stops rather than running with a retention window the operator
    /// didn't write (SO S6).
    #[error("worktree config invalid: {0}")]
    WorktreeConfig(#[from] crate::config::worktree::WorktreeConfigError),
    /// The orchestrator failed to construct (a malformed pipeline graph).
    #[error("orchestrator init failed: {0}")]
    OrchestratorInit(#[from] OrchestratorInitError),
    /// The control socket could not be bound, or its listener faulted.
    #[error("control socket failed: {0}")]
    Socket(#[from] control::ControlError),
    /// An OS-level signal handler failed to register.
    #[error("signal registration failed: {0}")]
    Signal(std::io::Error),
    /// One of the three supervised daemon tasks panicked.
    ///
    /// The load-bearing "never a silent hang" guarantee (G21.4): a panicked
    /// orchestrator / sweeper / listener `JoinHandle` surfaces as this loud,
    /// non-zero-exit error — not a daemon that wedges with no signal.
    #[error("a supervised daemon task panicked: {0}")]
    OrchestratorPanicked(String),
    /// A `boi daemon start/stop/status/restart` lifecycle action failed —
    /// surfaced by the daemon-green-backed LaunchAgent / systemd-user manager.
    /// The message carries the actionable detail (the command, exit code, and
    /// stderr tail) from `daemon_green::Error`.
    #[error("daemon lifecycle action failed: {0}")]
    Lifecycle(String),
}

/// The grace period the supervisor waits for the three tasks to drain after
/// the shutdown token fires.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// The sweeper's tick interval.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Minimum gap between the sweeper's worktree auto-clean passes (audit C1).
/// The retention window is days; scanning every terminal spec on each 30 s
/// tick is waste — once an hour is plenty.
const AUTO_CLEAN_PASS_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// A `phase_runs` row open longer than this without a heartbeat is abandoned.
const SWEEP_THRESHOLD: Duration = Duration::from_secs(120);

/// The hard per-phase wall-clock budget (default 20 min). A phase open longer
/// than this is reaped REGARDLESS of heartbeat freshness — the backstop for a
/// worker wedged inside a still-heartbeating child (e.g. a hung `cargo` build).
/// Overridable at boot via `BOI_PHASE_WALL_CLOCK_BUDGET_SECS`.
const PHASE_WALL_CLOCK_BUDGET: Duration = Duration::from_secs(20 * 60);

/// The env var overriding [`PHASE_WALL_CLOCK_BUDGET`], in whole seconds.
pub(crate) const PHASE_WALL_CLOCK_BUDGET_ENV: &str = "BOI_PHASE_WALL_CLOCK_BUDGET_SECS";

/// Resolve the per-phase wall-clock budget, honoring a
/// [`PHASE_WALL_CLOCK_BUDGET_ENV`] override read from the process env. A
/// malformed or zero value is logged loudly (S6) and falls back to
/// [`PHASE_WALL_CLOCK_BUDGET`].
///
/// The running daemon resolves this once at boot for the sweeper; the `boi
/// daemon start` CLI resolves it again to bake the value into the installed
/// service spec (so a reinstall never silently reverts an operator override —
/// `build_service_spec`). Both go through [`phase_wall_clock_budget_from`] so
/// the parse + default logic lives in exactly one place.
pub(crate) fn phase_wall_clock_budget() -> Duration {
    phase_wall_clock_budget_from(std::env::var(PHASE_WALL_CLOCK_BUDGET_ENV).ok().as_deref())
}

/// The pure resolver behind [`phase_wall_clock_budget`] — `None` (unset) or a
/// malformed / zero value yields [`PHASE_WALL_CLOCK_BUDGET`]; a positive integer
/// yields that many seconds. Pure so it is testable without mutating the
/// process-global environment (mirrors `goose::attempt_timeout_from`).
pub(crate) fn phase_wall_clock_budget_from(raw: Option<&str>) -> Duration {
    match raw {
        None => PHASE_WALL_CLOCK_BUDGET,
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(secs) if secs > 0 => Duration::from_secs(secs),
            Ok(_) => {
                tracing::warn!(
                    raw = %raw,
                    "{PHASE_WALL_CLOCK_BUDGET_ENV} must be > 0 — using the default",
                );
                PHASE_WALL_CLOCK_BUDGET
            }
            Err(e) => {
                tracing::warn!(
                    raw = %raw, error = %e,
                    "{PHASE_WALL_CLOCK_BUDGET_ENV} is not a u64 — using the default",
                );
                PHASE_WALL_CLOCK_BUDGET
            }
        },
    }
}

/// Boot the BOI daemon and block until graceful shutdown.
///
/// Returns `Ok(())` after a clean `SIGTERM` / `Ctrl-C` shutdown; an `Err` is
/// any boot fault or a panicked supervised task. The `executor` is injected —
/// production passes a `RuntimeExecutor`, a test a `MockExecutor` (G16.2).
pub async fn boot(executor: Arc<dyn PhaseExecutor>) -> Result<(), BootError> {
    // (0) — provision the daemon's `~/.boi/v2/` scratch directories. `boot`
    // owns directory setup: the traces dir (OTel exporter) AND the recipes
    // dir — `GooseRuntime::write_recipe` writes a per-phase-run recipe file
    // there and `std::fs::write` does NOT create missing parents, so a worker
    // phase faults at recipe-write on a fresh `~/.boi/v2/` if the dir is
    // absent (Phase 10 erratum — surfaced by the Docker E2E: every worker
    // phase failed `recipe_write_failed` because `~/.boi/v2/recipes/` was
    // never created).
    let mk_dir = |d: &std::path::Path| -> Result<(), BootError> {
        std::fs::create_dir_all(d).map_err(|e| {
            BootError::Repo(RepoError::NotFound(format!(
                "cannot create {}: {e}",
                d.display()
            )))
        })
    };

    // (0.5) — T8: `tracing-subscriber`'s fmt layer + the cold-boot
    // `boi daemon starting` line are installed in `cli::daemon::run`, BEFORE
    // `build_runtime_executor` runs `repo::connect`. Installing them here
    // would be too late — a `repo::connect` fault on a fresh `~/.boi/v2/`
    // would die before this point and the operator would see an empty
    // daemon.log. The init is idempotent (`try_init` no-ops if a subscriber
    // is already installed), but is deliberately NOT repeated here so the
    // starting log is emitted exactly once, with its socket-path field.

    // (1) — OTel. Held for the daemon's whole life; dropped LAST (review S13).
    let traces_dir = paths::traces_dir()?;
    mk_dir(&traces_dir)?;
    mk_dir(&paths::recipes_dir()?)?;
    let otel_guard = crate::runtime::init_tracing(&traces_dir)?;

    // (2) — the repo pool (migrations run inside `connect`).
    let db_url = paths::boi_db_url()?;
    let pool = repo::connect(&db_url).await?;

    // (3) — the phase + pipeline declarations, and the operator
    // `~/.boi/v2/config.toml` (the design-§5 `[worktree]` retention table —
    // audit C1). An absent config file is the documented default state; a
    // malformed one fails boot here, loudly.
    let phases = config::load_phases(&paths::phases_dir()?)?;
    let pipeline = config::load_pipeline(&paths::pipelines_dir()?, "standard")?;
    let worktree_config = config::worktree::load_worktree_config(&paths::config_file()?)?;

    // (4) — the bounded channel. `boot` OWNS it (G16.3).
    let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonNotification>(1024);

    // (5) — the event bus, wired via the G16.2 public constructor with the
    // production observer. The `OtelObserver` holds a tracer cloned off the
    // `OtelGuard`.
    let bus = Arc::new(EventBus::new(
        pool.clone(),
        vec![Arc::new(crate::runtime::OtelObserver::new(
            otel_guard.tracer(),
        ))],
    ));

    // (6) — the daemon-crash restart-recovery pass — BEFORE the orchestrator
    // spawns (Task 9.3 / design §5).
    recover_after_crash(&bus, &pool).await?;

    // (7) — the orchestrator (G16.3 — `new` is HANDED both channel halves).
    let orchestrator = Orchestrator::new(
        Arc::clone(&bus),
        pool.clone(),
        Arc::clone(&executor),
        pipeline,
        phases.clone(),
        daemon_tx.clone(),
        daemon_rx,
    )?;

    // (8) — the top-level shutdown token.
    let shutdown = CancellationToken::new();

    // (9) — the three supervised tasks.
    let orch_handle = tokio::spawn(orchestrator.run());

    let sweeper = Sweeper {
        bus: Arc::clone(&bus),
        daemon_tx: daemon_tx.clone(),
        pool: pool.clone(),
        threshold: SWEEP_THRESHOLD,
        wall_clock_budget: phase_wall_clock_budget(),
        // The audit-C1 worktree auto-clean pass: reclaim disk from
        // failed/canceled specs older than the design-§5 retention window
        // (`[worktree].auto_clean_canceled_after`, default 7 days).
        reclaimer: Some(Arc::new(crate::runtime::reclaim::SpecWorktreeReclaimer {
            worktree_root: crate::runtime::worktree::default_worktree_root(),
        })),
        auto_clean_after: worktree_config.auto_clean_canceled_after,
        auto_clean_pass_interval: AUTO_CLEAN_PASS_INTERVAL,
        last_auto_clean_pass: std::sync::Mutex::new(None),
        // Locate + WIP-commit a reaped task's worktree so `boi unblock` does
        // not bounce on `workspace_unclean` (OBS-035).
        worktree_root: Some(crate::runtime::worktree::default_worktree_root()),
    };
    let sweeper_handle = tokio::spawn(sweeper.run(SWEEP_INTERVAL, shutdown.clone()));

    // The control-socket listener — its handler carries the daemon's bus +
    // `daemon_tx` + dispatch context (built by `cli::daemon`).
    let socket = paths::control_socket()?;
    let goose_bin = std::path::PathBuf::from("goose");
    let handler: Arc<dyn control::CommandHandler> = Arc::new(DaemonState::new(
        Arc::clone(&bus),
        pool.clone(),
        daemon_tx.clone(),
        phases,
        goose_bin,
        paths::recipes_dir()?,
        // The provider liveness probe (429 hardening) — refuses a dispatch
        // pre-spend when the provider is rate-limiting (HTTP 429) or the
        // credential is rejected (401/403).
        Arc::new(crate::runtime::CurlProviderProbe::new()),
    ));
    let listener_handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { control::serve(socket, handler, shutdown).await }
    });

    tracing::info!("boi daemon ready");

    // (10) — the supervisor: select the three handles against the signals.
    supervise(
        orch_handle,
        sweeper_handle,
        listener_handle,
        shutdown,
        otel_guard,
        Arc::clone(&bus),
        pool.clone(),
    )
    .await
}

/// The shutdown supervisor — `select!` the three task handles against
/// `ctrl_c()` / `SIGTERM`, drive a graceful shutdown, and surface a panicked
/// task loudly.
///
/// `boot` blocks here until shutdown; it does NOT "return once the loop exits".
async fn supervise(
    orch_handle: tokio::task::JoinHandle<()>,
    sweeper_handle: tokio::task::JoinHandle<()>,
    listener_handle: tokio::task::JoinHandle<Result<(), control::ControlError>>,
    shutdown: CancellationToken,
    otel_guard: crate::runtime::OtelGuard,
    bus: Arc<EventBus>,
    pool: SqlitePool,
) -> Result<(), BootError> {
    // A SIGTERM future — `kill <daemon-pid>` (the systemd/launchd path).
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(BootError::Signal)?;

    // Pin the handles so the `select!` can poll them without consuming them —
    // a panicked handle must still be `.await`-able afterwards to extract the
    // `JoinError` (G21.4).
    tokio::pin!(orch_handle);
    tokio::pin!(sweeper_handle);
    tokio::pin!(listener_handle);

    // `graceful` is true ONLY when shutdown was triggered by a signal
    // (SIGTERM / SIGINT) — an intentional restart. An unexpected task exit /
    // panic is a crash: it must NOT run the drain, so its in-flight specs keep
    // the `DaemonCrash` recovery path. This flag IS the graceful-vs-crash
    // signal — no separate marker is persisted.
    let mut graceful = false;
    let mut outcome: Result<(), BootError> = tokio::select! {
        // The orchestrator task ended on its own — only a panic or a
        // channel-closed exit gets here. Either way the daemon cannot run on.
        joined = &mut orch_handle => match joined {
            Ok(()) => {
                tracing::error!("orchestrator task exited unexpectedly (channel closed)");
                Err(BootError::OrchestratorPanicked(
                    "orchestrator exited unexpectedly (channel closed — all daemon_tx clones dropped)".into(),
                ))
            }
            Err(e) => Err(BootError::OrchestratorPanicked(format!("orchestrator: {e}"))),
        },
        joined = &mut sweeper_handle => match joined {
            Ok(()) => {
                tracing::error!("sweeper task exited unexpectedly");
                Err(BootError::OrchestratorPanicked(
                    "sweeper exited unexpectedly".into(),
                ))
            }
            Err(e) => Err(BootError::OrchestratorPanicked(format!("sweeper: {e}"))),
        },
        joined = &mut listener_handle => match joined {
            Ok(Ok(())) => {
                tracing::error!("control listener exited unexpectedly");
                Err(BootError::OrchestratorPanicked(
                    "control listener exited unexpectedly".into(),
                ))
            }
            Ok(Err(e)) => Err(BootError::Socket(e)),
            Err(e) => Err(BootError::OrchestratorPanicked(format!("control listener: {e}"))),
        },
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("SIGINT received — shutting down");
            graceful = true;
            Ok(())
        }
        _ = sigterm.recv() => {
            tracing::info!("SIGTERM received — shutting down");
            graceful = true;
            Ok(())
        }
    };

    // Graceful shutdown — fire the token (stops the sweeper + the listener).
    shutdown.cancel();
    // The orchestrator's `run` holds a permanent `daemon_tx`, so its `recv()`
    // never returns `None` — production shutdown of the orchestrator is
    // `abort()` of its task (G21.4).
    orch_handle.abort();

    // Drain the three tasks under a bounded grace window. A handle that does
    // not finish in time is logged — never an indefinite hang.
    // N1: track panics during the drain; a panic downgrade `outcome` to Err.
    //
    // Guard each await with `is_finished()`: if the select! arm above already
    // consumed a handle's output (panic or unexpected exit), re-polling it would
    // panic with "JoinHandle polled after completion". `is_finished()` returns
    // true once the task's output has been taken, so we skip re-awaiting it.
    let drain = async {
        let mut drain_panics = 0u32;
        // The orchestrator was `abort()`ed — its join is typically `Err(Cancelled)`.
        // Only a *panic* (a non-cancel `JoinError`) is worth logging.
        // Guard with `is_finished()`: if the select! arm already consumed the
        // handle's output, re-polling panics with "polled after completion".
        if !orch_handle.is_finished()
            && let Err(e) = (&mut orch_handle).await
            && !e.is_cancelled()
        {
            tracing::error!(error = %e, "orchestrator task panicked during shutdown");
            drain_panics += 1;
        }
        if !sweeper_handle.is_finished()
            && let Err(e) = (&mut sweeper_handle).await
            && !e.is_cancelled()
        {
            tracing::error!(error = %e, "sweeper task panicked during shutdown");
            drain_panics += 1;
        }
        if !listener_handle.is_finished()
            && let Err(e) = (&mut listener_handle).await
            && !e.is_cancelled()
        {
            tracing::error!(error = %e, "control listener panicked during shutdown");
            drain_panics += 1;
        }
        drain_panics
    };
    let drain_panics = match tokio::time::timeout(SHUTDOWN_GRACE, drain).await {
        Ok(n) => n,
        Err(_) => {
            tracing::error!("daemon shutdown grace window expired — some tasks did not drain");
            0
        }
    };
    if drain_panics > 0 && outcome.is_ok() {
        outcome = Err(BootError::OrchestratorPanicked(format!(
            "{drain_panics} supervised task(s) panicked during shutdown drain"
        )));
    }

    // Graceful restart only: park in-flight specs as blocked{DaemonDraining} so
    // the next boot SPARES them instead of `daemon_crash`ing them. Runs now —
    // after the orchestrator is aborted+joined (no live writer races it) and
    // while the OTel guard is still alive (the parking events flush). A crash
    // path leaves `graceful = false` and skips this, preserving DaemonCrash.
    //
    // GATE on the state-writing supervised tasks having actually stopped, not
    // just on `graceful`: if the grace window expired with the orchestrator OR
    // the sweeper still alive (neither hit an await after `abort()` /
    // `shutdown.cancel()`), draining now would race a live writer that holds
    // `daemon_tx` + the bus and could mutate task/run state concurrently. The
    // detached per-phase `drain_phase` tasks are NOT covered by any handle, but
    // `awaiting_operator_recovery` is robust to their late re-`PhaseStarted`
    // (it ignores open runs owned by a blocked task); the orchestrator + sweeper
    // are the writers that must be quiescent. Skip the drain otherwise — the
    // in-flight specs then take the unchanged crash-recovery path, never worse.
    if graceful {
        if orch_handle.is_finished() && sweeper_handle.is_finished() {
            if let Err(e) = drain_in_flight_specs(&bus, &pool).await {
                tracing::error!(
                    error = %e,
                    "graceful drain failed — in-flight specs may be daemon_crash'd on next boot",
                );
            }
        } else {
            tracing::warn!(
                orchestrator_stopped = orch_handle.is_finished(),
                sweeper_stopped = sweeper_handle.is_finished(),
                "graceful drain skipped — a supervised writer did not stop within the shutdown \
                 grace window; not draining to avoid racing it (in-flight specs take \
                 crash-recovery on the next boot)",
            );
        }
    }

    // Drop the OTel guard LAST — while the tokio runtime is still alive — so
    // the final span batch is force-flushed, not lost (review S13).
    drop(otel_guard);
    tracing::info!("boi daemon stopped");
    outcome
}

/// The daemon-crash restart-recovery pass (Task 9.3 / G16.5 / design §5).
///
/// At boot — *before* the orchestrator spawns — every spec still marked
/// `running` belongs to a daemon that died mid-run: emit
/// `SpecFailed{DaemonCrash}` for each (the real producer of
/// `FailureReason::DaemonCrash`) — EXCEPT a quiescent-blocked spec parked
/// awaiting operator recovery, which is preserved (audit A2 / design §6 —
/// see `awaiting_operator_recovery`). Then reconcile every still-open
/// `phase_runs` row (`completed_at IS NULL`) — on a fresh daemon there is no
/// live worker, so each open row is a crashed phase run; `close_orphaned`
/// stamps `completed_at` so `boi status` / `boi log` do not show it
/// perpetually `[running]`.
///
/// `pub` (Phase 10) so the §13.3 failure-path L3 suite can drive the real
/// `DaemonCrash` producer against a seeded crashed-daemon DB — `boot` itself
/// is too heavy to invoke per test, but `recover_after_crash` is the exact,
/// isolated step that produces `FailureReason::DaemonCrash`.
pub async fn recover_after_crash(bus: &EventBus, pool: &SqlitePool) -> Result<(), BootError> {
    let specs = repo::spec_runtime::all(pool).await?;
    let mut recovered = 0usize;
    let mut failed_recovery = 0usize;
    for row in &specs {
        let status: SpecStatus = match row.status.parse() {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(spec_id = %row.spec_id, error = %e, "corrupt spec status at recovery");
                continue;
            }
        };
        if status != SpecStatus::Running {
            continue;
        }
        let spec_id = match SpecId::new(&row.spec_id) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(spec_id = %row.spec_id, error = %e, "corrupt spec id at recovery");
                continue;
            }
        };
        // AUDIT A2 / design §6: a QUIESCENT-BLOCKED spec — `running` with no
        // open phase run, no `active` task, and ≥ 1 `blocked` task — is not a
        // crash victim. It is parked awaiting operator recovery (`boi unblock`
        // / `boi resolve-conflict`), a legitimate steady state that must
        // SURVIVE a daemon restart: failing it with `DaemonCrash` (terminal,
        // no exit edge) would brick the documented revive loop on every
        // redeploy. Spared LOUDLY — the `warn!` plus the dashboard's blocked
        // surfacing keep it visible, never silent (SO S6). Any in-flight
        // signal (an open row, an `active` task) means a genuine mid-run
        // crash and still fails below.
        if awaiting_operator_recovery(pool, &spec_id).await? {
            tracing::warn!(
                spec_id = %spec_id,
                "spec held `running` with blocked task(s) awaiting operator \
                 recovery — preserved across restart (revive via `boi unblock`, \
                 or close via `boi fail` / `boi cancel`)",
            );
            continue;
        }
        // `running → failed{DaemonCrash}` — a legal terminal transition.
        let event = BoiEvent::SpecFailed {
            spec_id: spec_id.clone(),
            reason: FailureReason::DaemonCrash,
        };
        match bus.emit(&event).await {
            Ok(()) => {
                recovered += 1;
                tracing::warn!(spec_id = %spec_id, "recovered a `running` spec from a prior daemon crash");
            }
            Err(e) => {
                // Loud — a spec the recovery pass could not fail is a real
                // fault the operator must see (SO S6).
                tracing::error!(spec_id = %spec_id, error = %e, "could not fail a crashed-daemon spec");
                failed_recovery += 1;
            }
        }
    }

    // Reconcile every open `phase_runs` row.
    let closed = repo::phase_runs::close_orphaned(pool, chrono::Utc::now()).await?;
    if recovered > 0 || closed > 0 {
        tracing::info!(
            recovered_specs = recovered,
            closed_phase_runs = closed,
            "daemon-crash restart-recovery complete",
        );
    }
    // S4: if any spec could not be failed, the daemon must not start — those specs
    // remain `running` and would corrupt the orchestrator's view of state.
    if failed_recovery > 0 {
        tracing::error!(
            failed = failed_recovery,
            "daemon-crash recovery: {failed_recovery} spec(s) could not be failed — \
             they remain `running`; manual intervention required",
        );
        return Err(BootError::Repo(RepoError::NotFound(format!(
            "{failed_recovery} spec(s) could not be marked failed after daemon crash"
        ))));
    }
    Ok(())
}

/// Whether `spec_id` is parked awaiting operator recovery (audit A2 / §6):
/// no `active` task, ≥1 `blocked` task, and no DISQUALIFYING open `phase_runs`
/// row.
///
/// An open run owned by a `blocked` task is NOT disqualifying — it is a parked
/// task's orphaned run. The graceful drain parks a task WITHOUT closing its run
/// (closing it would race a still-live detached `drain_phase` task that can
/// re-`PhaseStarted` a fresh open row after the close — the run-reopen bug a
/// 2026-06-12 review caught); recovery's own `close_orphaned` reconciles these
/// AFTER this spare decision. An open run owned by an `active` task, or by NO
/// task (a spec-level phase), is genuine mid-run crash debris and still
/// disqualifies — so a real crash (active task + open run) still `DaemonCrash`s,
/// and a spec-level in-flight phase (`task_id = None`) still `DaemonCrash`s.
async fn awaiting_operator_recovery(
    pool: &SqlitePool,
    spec_id: &SpecId,
) -> Result<bool, BootError> {
    let tasks = repo::task_runtime::tasks_for_spec(pool, spec_id).await?;
    let blocked: std::collections::HashSet<&str> = tasks
        .iter()
        .filter(|t| t.state == "blocked")
        .map(|t| t.task_id.as_str())
        .collect();
    let any_active = tasks.iter().any(|t| t.state == "active");
    let any_blocked = !blocked.is_empty();

    let history = repo::phase_runs::fetch_history_for_spec(pool, spec_id).await?;
    let any_disqualifying_open = history.iter().any(|r| {
        r.is_open()
            && match &r.task_id {
                Some(tid) => !blocked.contains(tid.as_str()),
                None => true, // a spec-level open run is genuine crash debris
            }
    });
    if any_disqualifying_open {
        return Ok(false);
    }
    Ok(!any_active && any_blocked)
}

/// The graceful-shutdown drain — park in-flight specs so a daemon restart does
/// NOT terminally fail them (the 2026-06-11 incident: a binary swap / budget
/// change `daemon_crash`ed every running spec; quiescent-blocked specs were the
/// only survivors).
///
/// Called ONLY from the graceful shutdown path (`supervise`'s SIGTERM / SIGINT
/// arms), after the supervised tasks have stopped. For every `running` spec
/// with an `active` task it emits `TaskBlocked{DaemonDraining}` (the bus guard
/// enforces the legal `active → blocked`), turning the spec quiescent-blocked
/// so the next boot's `recover_after_crash` SPARES it instead of
/// `DaemonCrash`ing it. The spec then survives the restart, surfaces as
/// `[blocked]` in the dashboard, and revives via `boi unblock`.
///
/// The drain does NOT close phase runs. Closing them here (an earlier design)
/// raced a still-live DETACHED `drain_phase` task — `orch_handle.abort()` stops
/// the orchestrator's run loop but NOT the per-phase worker tasks it spawned
/// (their cancel tokens live in the now-orphaned `in_flight` registry), so one
/// can re-`PhaseStarted` a fresh open row AFTER the close and flip the spec
/// back to `DaemonCrash` on boot (a 2026-06-12 review finding). Instead,
/// `awaiting_operator_recovery` ignores an open run owned by a `blocked` task;
/// recovery's own `close_orphaned` reconciles those rows after sparing.
///
/// Accounting is computed by re-running the recovery predicate itself
/// (`awaiting_operator_recovery`) per spec AFTER parking, so the `parked` /
/// `unsparable` tallies can never drift from what recovery will actually decide
/// (S6 — no false "survived" / "will crash" logs).
///
/// LIMITATION (known follow-up): a spec interrupted mid SPEC-LEVEL phase
/// (`plan` / `integrate`, `task_id = None`, every task terminal) has no task to
/// block — there is no spec-level `blocked` state — so it stays `unsparable`
/// and `recover_after_crash` still `DaemonCrash`es it on boot, exactly as
/// before this change (an incomplete fix, not a regression). Full spec-level
/// survival needs a durable graceful marker (a `spec_runtime` column or a
/// spec-level `DaemonDraining` event recovery honors); deferred.
///
/// A true crash (SIGKILL / panic / power loss) never reaches this code, so the
/// drain *executing* is itself the graceful signal — no persisted marker.
pub async fn drain_in_flight_specs(bus: &EventBus, pool: &SqlitePool) -> Result<(), BootError> {
    let specs = repo::spec_runtime::all(pool).await?;
    // Specs made recoverable by parking (recovery will SPARE) vs. specs the
    // drain could not make recoverable (recovery will `DaemonCrash`). Both
    // tallies are decided by `awaiting_operator_recovery` itself, below.
    let mut parked = 0usize;
    let mut unsparable = 0usize;
    for row in &specs {
        let status: SpecStatus = match row.status.parse() {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(spec_id = %row.spec_id, error = %e, "corrupt spec status at drain");
                continue;
            }
        };
        if status != SpecStatus::Running {
            continue;
        }
        let spec_id = match SpecId::new(&row.spec_id) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(spec_id = %row.spec_id, error = %e, "corrupt spec id at drain");
                continue;
            }
        };

        let tasks = repo::task_runtime::tasks_for_spec(pool, &spec_id).await?;
        let active: Vec<TaskId> = tasks
            .iter()
            .filter(|t| t.state == "active")
            .filter_map(|t| TaskId::new(&t.task_id).ok())
            .collect();
        let history = repo::phase_runs::fetch_history_for_spec(pool, &spec_id).await?;
        let has_open = history.iter().any(|r| r.is_open());
        // Nothing in flight (no active task, no open run) ⇒ a quiescent-blocked
        // spec that already survives a restart untouched. Leave it alone.
        if active.is_empty() && !has_open {
            continue;
        }
        // Park every active task as blocked{DaemonDraining} through the bus so
        // the transition is guarded + event-sourced (mirrors how
        // `recover_after_crash` emits SpecFailed on the bus).
        for tid in &active {
            let event = BoiEvent::TaskBlocked {
                spec_id: spec_id.clone(),
                task_id: tid.clone(),
                reason: BlockedReason::DaemonDraining,
            };
            if let Err(e) = bus.emit(&event).await {
                // Loud — a task we cannot park likely leaves the spec
                // unsparable below, the outcome the drain exists to prevent.
                tracing::error!(
                    spec_id = %spec_id, task_id = %tid, error = %e,
                    "graceful drain: could not park an active task as blocked",
                );
            }
        }
        // Did parking make it recoverable? Ask recovery's OWN predicate so the
        // tally is exactly what the next boot will decide — never a guess.
        if awaiting_operator_recovery(pool, &spec_id).await? {
            parked += 1;
        } else {
            unsparable += 1;
            tracing::warn!(
                spec_id = %spec_id,
                "graceful drain: in-flight spec could not be made recoverable (a \
                 spec-level phase with no blockable task, or a failed park) — it will \
                 be recovered as DaemonCrash on the next boot",
            );
        }
    }

    if parked > 0 {
        tracing::info!(
            parked_specs = parked,
            "graceful drain: in-flight specs parked blocked (DaemonDraining) — \
             they survive the restart and revive via `boi unblock`",
        );
    }
    if unsparable > 0 {
        tracing::warn!(
            unsparable_specs = unsparable,
            "graceful drain: {unsparable} in-flight spec(s) could not be made \
             recoverable and will be recovered as DaemonCrash on the next boot",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::testtmp::TempDir;
    use crate::config::{parse_phase, parse_pipeline};
    use crate::repo::db::connect;
    use crate::repo::spec_versions::VersionTrigger;
    use crate::service::bus::EventBus;
    use crate::service::bus::testkit::RecordingObserver;
    use crate::service::registry::testkit::MockExecutor;
    use crate::service::{DaemonCommand, DaemonResponse};
    use crate::types::ids::PhaseRunId;
    use chrono::Utc;
    use std::collections::HashMap;
    use std::time::Duration;

    #[test]
    fn phase_budget_unset_is_the_default() {
        assert_eq!(phase_wall_clock_budget_from(None), PHASE_WALL_CLOCK_BUDGET);
    }

    #[test]
    fn phase_budget_positive_override_is_honored() {
        assert_eq!(
            phase_wall_clock_budget_from(Some("3600")),
            Duration::from_secs(3600),
        );
        // Whitespace is trimmed.
        assert_eq!(
            phase_wall_clock_budget_from(Some("  900 ")),
            Duration::from_secs(900),
        );
    }

    #[test]
    fn phase_budget_zero_falls_back_to_default() {
        assert_eq!(
            phase_wall_clock_budget_from(Some("0")),
            PHASE_WALL_CLOCK_BUDGET,
        );
    }

    #[test]
    fn phase_budget_malformed_falls_back_to_default() {
        assert_eq!(
            phase_wall_clock_budget_from(Some("abc")),
            PHASE_WALL_CLOCK_BUDGET,
        );
        assert_eq!(
            phase_wall_clock_budget_from(Some("")),
            PHASE_WALL_CLOCK_BUDGET,
        );
    }

    /// Seed a spec in the `running` status, plus one still-open `phase_runs`
    /// row — the exact state a crashed daemon leaves.
    async fn seed_crashed() -> (SqlitePool, SpecId) {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        repo::specs::insert_spec(&pool, &spec, Utc::now())
            .await
            .unwrap();
        repo::spec_versions::append_version(
            &pool,
            &spec,
            1,
            &serde_json::json!({ "title": "demo" }),
            VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        repo::spec_runtime::initialize(&pool, &spec, 1)
            .await
            .unwrap();
        // queued → running.
        repo::spec_runtime::update_status(&pool, &spec, SpecStatus::Running, None, Utc::now())
            .await
            .unwrap();
        // An open phase run (no `update_end`) — a crashed worker.
        let pr = PhaseRunId::new("P0000001a").unwrap();
        repo::phase_runs::insert_start(
            &pool,
            &pr,
            &spec,
            None,
            "plan",
            0,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        (pool, spec)
    }

    /// The restart-recovery pass fails a prior `running` spec with
    /// `DaemonCrash` and closes its orphaned phase run.
    #[tokio::test]
    async fn test_l2_recover_after_crash_fails_running_specs() {
        let (pool, spec) = seed_crashed().await;
        let recorder = RecordingObserver::new();
        let bus = EventBus::new(pool.clone(), vec![Arc::new(recorder.clone())]);

        recover_after_crash(&bus, &pool).await.unwrap();

        // The spec is now `failed{DaemonCrash}`.
        let row = repo::spec_runtime::fetch(&pool, &spec).await.unwrap();
        assert_eq!(row.status, "failed", "the crashed spec is failed");
        let reason = row.failure_reason.expect("a failure reason was written");
        assert_eq!(
            reason.get("type").and_then(|t| t.as_str()),
            Some("daemon_crash"),
            "the reason is DaemonCrash, got {reason:?}",
        );
        // A `SpecFailed` was observed on the bus.
        assert!(
            recorder
                .seen()
                .iter()
                .any(|e| matches!(e, BoiEvent::SpecFailed { .. })),
            "recovery emits SpecFailed",
        );
        // The orphaned phase run is closed.
        let pr = PhaseRunId::new("P0000001a").unwrap();
        let run = repo::phase_runs::fetch(&pool, &pr).await.unwrap();
        assert!(!run.is_open(), "the orphaned phase run is closed");
    }

    /// A non-`running` spec is left untouched by the recovery pass.
    #[tokio::test]
    async fn test_l2_recover_after_crash_leaves_queued_specs_alone() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        repo::specs::insert_spec(&pool, &spec, Utc::now())
            .await
            .unwrap();
        repo::spec_versions::append_version(
            &pool,
            &spec,
            1,
            &serde_json::json!({ "title": "demo" }),
            VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        // `queued` — never started.
        repo::spec_runtime::initialize(&pool, &spec, 1)
            .await
            .unwrap();
        let bus = EventBus::new(pool.clone(), vec![Arc::new(RecordingObserver::new())]);
        recover_after_crash(&bus, &pool).await.unwrap();
        let row = repo::spec_runtime::fetch(&pool, &spec).await.unwrap();
        assert_eq!(row.status, "queued", "a queued spec is not recovered");
    }

    /// Seed an IN-FLIGHT spec — `running`, one `active` task, an OPEN
    /// `phase_runs` row owned by that task (the exact state a graceful restart
    /// interrupts and which today gets `daemon_crash`ed on the next boot).
    async fn seed_in_flight() -> (SqlitePool, SpecId, TaskId) {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        repo::specs::insert_spec(&pool, &spec, Utc::now())
            .await
            .unwrap();
        repo::spec_versions::append_version(
            &pool,
            &spec,
            1,
            &serde_json::json!({ "title": "demo" }),
            VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        repo::spec_runtime::initialize(&pool, &spec, 1)
            .await
            .unwrap();
        repo::spec_runtime::update_status(&pool, &spec, SpecStatus::Running, None, Utc::now())
            .await
            .unwrap();
        let task = TaskId::new("T0000001a").unwrap();
        repo::task_runtime::insert_task(&pool, &task, &spec, None)
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
        // An OPEN phase run owned by the active task.
        let pr = PhaseRunId::new("P0000001a").unwrap();
        repo::phase_runs::insert_start(
            &pool,
            &pr,
            &spec,
            Some(&task),
            "execute",
            0,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        (pool, spec, task)
    }

    /// The graceful drain parks an in-flight spec as `blocked{DaemonDraining}`
    /// (active task → blocked) WITHOUT closing its phase run, so the next boot's
    /// recovery SPARES it (it survives `running`) instead of `daemon_crash`ing
    /// it. End-to-end contract for the 2026-06-11 restart-kills-in-flight-specs
    /// incident, with no live daemon.
    #[tokio::test]
    async fn test_drain_then_recover_parks_in_flight_spec_instead_of_failing_it() {
        let (pool, spec, task) = seed_in_flight().await;
        let recorder = RecordingObserver::new();
        let bus = EventBus::new(pool.clone(), vec![Arc::new(recorder.clone())]);

        drain_in_flight_specs(&bus, &pool).await.unwrap();

        // The active task is parked blocked{DaemonDraining}.
        let trow = repo::task_runtime::fetch(&pool, &task).await.unwrap();
        assert_eq!(trow.state, "blocked", "the active task is parked blocked");
        let reason = trow.blocked_reason.expect("a blocked reason was written");
        assert_eq!(
            reason.get("type").and_then(|t| t.as_str()),
            Some("daemon_draining"),
            "the reason is DaemonDraining, got {reason:?}",
        );
        // The drain does NOT close the run (closing would race a detached
        // drain_phase re-open). The run stays open, owned by the now-blocked
        // task — which recovery tolerates.
        let pr = PhaseRunId::new("P0000001a").unwrap();
        assert!(
            repo::phase_runs::fetch(&pool, &pr).await.unwrap().is_open(),
            "the drain leaves the phase run open (recovery tolerates a blocked task's open run)",
        );
        // A TaskBlocked was observed on the bus (event-sourced, guarded).
        assert!(
            recorder
                .seen()
                .iter()
                .any(|e| matches!(e, BoiEvent::TaskBlocked { .. })),
            "the drain emits TaskBlocked",
        );

        // The next boot's recovery now SPARES it — survives `running`, NOT
        // failed{DaemonCrash} — even though the run is still open, because that
        // open run is owned by a blocked task.
        recover_after_crash(&bus, &pool).await.unwrap();
        let srow = repo::spec_runtime::fetch(&pool, &spec).await.unwrap();
        assert_eq!(
            srow.status, "running",
            "a drained spec survives the restart (spared), not failed",
        );
        assert!(
            srow.failure_reason.is_none(),
            "no DaemonCrash failure is written for a gracefully-drained spec",
        );
    }

    /// `awaiting_operator_recovery` (via `recover_after_crash`) SPARES a spec
    /// whose only open run is owned by a `blocked` task — the parked-but-not-
    /// closed shape the drain now produces. Locks the run-reopen-race fix: a
    /// late detached `drain_phase` re-`PhaseStarted` (an open run under the
    /// blocked task) must NOT flip the spec to `DaemonCrash`. Review finding.
    #[tokio::test]
    async fn test_recover_spares_a_blocked_spec_whose_open_run_is_blocked_task_owned() {
        let (pool, spec, task) = seed_in_flight().await; // running, active task, open run
        // Park the task blocked (what the drain does) — leave the run OPEN.
        repo::task_runtime::update_state(
            &pool,
            &task,
            crate::types::state::TaskState::Blocked,
            None,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        assert!(
            repo::phase_runs::fetch(&pool, &PhaseRunId::new("P0000001a").unwrap())
                .await
                .unwrap()
                .is_open(),
            "precondition: the blocked task's run is still open",
        );

        let bus = EventBus::new(pool.clone(), vec![Arc::new(RecordingObserver::new())]);
        recover_after_crash(&bus, &pool).await.unwrap();

        let srow = repo::spec_runtime::fetch(&pool, &spec).await.unwrap();
        assert_eq!(
            srow.status, "running",
            "an open run owned by a blocked task does NOT disqualify the spare",
        );
    }

    /// A spec interrupted mid SPEC-LEVEL phase (open run, `task_id = None`, no
    /// active task) cannot be made quiescent-blocked — the drain must NOT park
    /// it and must NOT claim it survived; recovery still `DaemonCrash`es it.
    /// Locks the documented spec-level limitation honestly (review finding).
    #[tokio::test]
    async fn test_drain_does_not_falsely_spare_a_spec_level_in_flight_spec() {
        let (pool, spec) = seed_crashed().await; // running, open spec-level run, no task
        let recorder = RecordingObserver::new();
        let bus = EventBus::new(pool.clone(), vec![Arc::new(recorder.clone())]);

        drain_in_flight_specs(&bus, &pool).await.unwrap();

        // Nothing to park — no TaskBlocked emitted.
        assert!(
            !recorder
                .seen()
                .iter()
                .any(|e| matches!(e, BoiEvent::TaskBlocked { .. })),
            "a spec-level in-flight spec has no task to park",
        );

        // Recovery still fails it — the drain honestly did not spare it.
        recover_after_crash(&bus, &pool).await.unwrap();
        let srow = repo::spec_runtime::fetch(&pool, &spec).await.unwrap();
        assert_eq!(
            srow.status, "failed",
            "a spec-level in-flight spec is still DaemonCrashed (documented limit)",
        );
    }

    /// The drain leaves a spec with no in-flight work untouched — a quiescent
    /// blocked spec already survives a restart and must not be re-parked.
    #[tokio::test]
    async fn test_drain_skips_a_spec_with_no_in_flight_work() {
        let (pool, _spec) = seed_blocked_awaiting_operator().await;
        let recorder = RecordingObserver::new();
        let bus = EventBus::new(pool.clone(), vec![Arc::new(recorder.clone())]);

        drain_in_flight_specs(&bus, &pool).await.unwrap();

        // No new TaskBlocked — the already-blocked task is not re-parked.
        assert!(
            !recorder
                .seen()
                .iter()
                .any(|e| matches!(e, BoiEvent::TaskBlocked { .. })),
            "a quiescent-blocked spec is not re-drained",
        );
    }

    /// Seed a QUIESCENT-BLOCKED spec — `running`, one `blocked` task, every
    /// `phase_runs` row CLOSED (the state a verdict-routed block parks a spec
    /// in while it awaits `boi unblock`, audit A2 / design §6).
    async fn seed_blocked_awaiting_operator() -> (SqlitePool, SpecId) {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        repo::specs::insert_spec(&pool, &spec, Utc::now())
            .await
            .unwrap();
        repo::spec_versions::append_version(
            &pool,
            &spec,
            1,
            &serde_json::json!({ "title": "demo" }),
            VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        repo::spec_runtime::initialize(&pool, &spec, 1)
            .await
            .unwrap();
        repo::spec_runtime::update_status(&pool, &spec, SpecStatus::Running, None, Utc::now())
            .await
            .unwrap();
        // One task, parked `blocked` (not_started → active → blocked).
        let task = crate::types::ids::TaskId::new("T0000001a").unwrap();
        repo::task_runtime::insert_task(&pool, &task, &spec, None)
            .await
            .unwrap();
        for state in [
            crate::types::state::TaskState::Active,
            crate::types::state::TaskState::Blocked,
        ] {
            repo::task_runtime::update_state(&pool, &task, state, None, None, Utc::now())
                .await
                .unwrap();
        }
        // A CLOSED phase run — the routed block closed every row.
        let pr = PhaseRunId::new("P0000001a").unwrap();
        repo::phase_runs::insert_start(
            &pool,
            &pr,
            &spec,
            Some(&task),
            "execute",
            0,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        repo::phase_runs::update_end(
            &pool,
            &pr,
            "blocked",
            &crate::types::verdict::WorkerVerdict {
                synopsis: "blocked".into(),
                outcome: crate::types::verdict::VerdictOutcome::Fail {
                    error: "cap".into(),
                    why: "exceeded".into(),
                    fix: "unblock".into(),
                },
            },
            &[],
            0,
            0,
            Utc::now(),
        )
        .await
        .unwrap();
        (pool, spec)
    }

    /// AUDIT A2 — a QUIESCENT-BLOCKED spec (`running`, no open phase run, no
    /// `active` task, ≥ 1 `blocked` task) is NOT a crash victim: it is parked
    /// awaiting operator recovery (`boi unblock`), and that steady state must
    /// SURVIVE a daemon restart — failing it with `DaemonCrash` would brick
    /// the documented §6 revive loop on every redeploy.
    #[tokio::test]
    async fn test_l2_recover_after_crash_preserves_a_blocked_spec_awaiting_operator() {
        let (pool, spec) = seed_blocked_awaiting_operator().await;
        let recorder = RecordingObserver::new();
        let bus = EventBus::new(pool.clone(), vec![Arc::new(recorder.clone())]);

        recover_after_crash(&bus, &pool).await.unwrap();

        let row = repo::spec_runtime::fetch(&pool, &spec).await.unwrap();
        assert_eq!(
            row.status, "running",
            "a blocked spec awaiting operator recovery must survive a restart",
        );
        assert!(
            !recorder
                .seen()
                .iter()
                .any(|e| matches!(e, BoiEvent::SpecFailed { .. })),
            "no SpecFailed may be emitted for an operator-parked spec",
        );
    }

    /// AUDIT A2 (the conservative arm) — a `running` spec with a blocked task
    /// but ALSO a still-open phase run crashed MID-FLIGHT (some other phase
    /// was live when the daemon died): it is still failed with `DaemonCrash`.
    /// Only the fully-quiescent blocked state is spared.
    #[tokio::test]
    async fn test_l2_recover_after_crash_still_fails_a_blocked_spec_with_an_open_run() {
        let (pool, spec) = seed_blocked_awaiting_operator().await;
        // An additional OPEN phase run — in-flight work at crash time.
        let pr = PhaseRunId::new("P0000002b").unwrap();
        repo::phase_runs::insert_start(
            &pool,
            &pr,
            &spec,
            None,
            "plan",
            0,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        let bus = EventBus::new(pool.clone(), vec![Arc::new(RecordingObserver::new())]);

        recover_after_crash(&bus, &pool).await.unwrap();

        let row = repo::spec_runtime::fetch(&pool, &spec).await.unwrap();
        assert_eq!(
            row.status, "failed",
            "an open phase run means a genuine mid-run crash — still failed",
        );
    }

    fn fixture_phases() -> HashMap<String, crate::config::PhaseDef> {
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

    fn fixture_pipeline() -> crate::config::PipelineDef {
        let toml = std::fs::read_to_string(format!(
            "{}/tests/fixtures/pipelines/standard.toml",
            env!("CARGO_MANIFEST_DIR"),
        ))
        .unwrap();
        parse_pipeline(&toml).unwrap()
    }

    /// B2 (1): `boot` with a `MockExecutor` reaches a running orchestrator +
    /// a live control socket — the G16.2 executor seam is wired correctly.
    #[tokio::test]
    async fn test_l2_boot_reaches_orchestrator_and_live_socket() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let phases = fixture_phases();
        let pipeline = fixture_pipeline();
        let (daemon_tx, daemon_rx) = mpsc::channel(1024);
        let bus = Arc::new(EventBus::new(
            pool.clone(),
            vec![Arc::new(RecordingObserver::new())],
        ));
        let shutdown = CancellationToken::new();
        let executor: Arc<dyn PhaseExecutor> = Arc::new(MockExecutor::all_passing());
        let orchestrator = Orchestrator::new(
            Arc::clone(&bus),
            pool.clone(),
            Arc::clone(&executor),
            pipeline,
            phases,
            daemon_tx.clone(),
            daemon_rx,
        )
        .unwrap();
        let orch_handle = tokio::spawn(orchestrator.run());

        let sweeper_shutdown = shutdown.clone();
        let sweeper_handle = tokio::spawn(async move {
            sweeper_shutdown.cancelled().await;
        });

        struct EchoHandler;
        #[async_trait::async_trait]
        impl control::CommandHandler for EchoHandler {
            async fn handle(&self, _cmd: DaemonCommand) -> DaemonResponse {
                DaemonResponse::Ok {
                    detail: "pong".to_owned(),
                }
            }
        }
        let dir = TempDir::new("boot-sock-1");
        let socket = dir.path().join("daemon.sock");
        let listener_handle = tokio::spawn(control::serve(
            socket.clone(),
            Arc::new(EchoHandler),
            shutdown.clone(),
        ));

        // Wait for the socket to appear — confirms the listener is up.
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(socket.exists(), "control socket must appear after boot");

        // Verify the socket is reachable end-to-end.
        let resp = control::send_command(
            &socket,
            &DaemonCommand::Cancel {
                id: "S0000001a".to_owned(),
                reason: "test".to_owned(),
            },
        )
        .await
        .expect("send_command must succeed when daemon socket is live");
        assert!(
            matches!(resp, DaemonResponse::Ok { .. }),
            "expected Ok from live socket, got {resp:?}",
        );

        // Clean up: cancel shutdown token (stops listener + sweeper), abort orchestrator.
        shutdown.cancel();
        orch_handle.abort();
        listener_handle.await.ok();
        sweeper_handle.await.ok();
    }

    /// B2 (2): a `SIGTERM` causes `supervise` to return `Ok(())` — clean
    /// shutdown — with the `OtelGuard` dropped (flushed) before returning.
    #[tokio::test]
    async fn test_l2_sigterm_causes_clean_shutdown() {
        let traces = TempDir::new("boot-traces-2");
        let otel_guard = crate::runtime::init_tracing(traces.path()).unwrap();
        let shutdown = CancellationToken::new();

        let orch_handle = tokio::spawn(async {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
        let sweeper_shutdown = shutdown.clone();
        let sweeper_handle = tokio::spawn(async move {
            sweeper_shutdown.cancelled().await;
        });

        struct NoopHandler;
        #[async_trait::async_trait]
        impl control::CommandHandler for NoopHandler {
            async fn handle(&self, _cmd: DaemonCommand) -> DaemonResponse {
                DaemonResponse::Ok {
                    detail: "ok".to_owned(),
                }
            }
        }
        let dir = TempDir::new("boot-sock-2");
        let socket = dir.path().join("d.sock");
        let listener_handle = tokio::spawn(control::serve(
            socket.clone(),
            Arc::new(NoopHandler),
            shutdown.clone(),
        ));

        // Wait for the socket to appear, confirming all handles are running.
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(socket.exists(), "socket must appear before SIGTERM");

        // Start the supervisor. The graceful drain runs on the SIGTERM path
        // against an empty pool — a no-op that proves the shutdown still
        // completes cleanly with the drain wired in.
        let pool = connect("sqlite::memory:").await.unwrap();
        let bus = Arc::new(EventBus::new(
            pool.clone(),
            vec![Arc::new(RecordingObserver::new())],
        ));
        let supervise_task = tokio::spawn(supervise(
            orch_handle,
            sweeper_handle,
            listener_handle,
            shutdown,
            otel_guard,
            bus,
            pool,
        ));

        // Give supervise time to register the SIGTERM handler.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Send SIGTERM via POSIX kill(2) — avoids subprocess lint (subprocess
        // use is restricted to src/runtime/). tokio intercepts the signal; the
        // process does NOT die because tokio replaced the default handler.
        #[allow(unsafe_code)]
        unsafe {
            unsafe extern "C" {
                fn kill(pid: i32, sig: i32) -> i32;
            }
            kill(std::process::id() as i32, 15 /* SIGTERM */);
        }

        // supervise must return Ok(()) (clean shutdown) within the grace window.
        let result = tokio::time::timeout(Duration::from_secs(15), supervise_task)
            .await
            .expect("supervise must not hang after SIGTERM")
            .expect("join must succeed");
        assert!(result.is_ok(), "SIGTERM → Ok(()), got {result:?}",);
    }

    /// B2 (3) / B1 regression: a panicked orchestrator handle surfaces as
    /// `Err(BootError::OrchestratorPanicked)`, never a hang or an `Ok(())` exit.
    ///
    /// This test fails before the B1/S3 fix (the `Ok(())` arm would have caused
    /// `supervise` to return `Ok(())`) and passes after.
    #[tokio::test]
    async fn test_l2_panicked_orchestrator_surfaces_as_boot_error() {
        let traces = TempDir::new("boot-traces-3");
        let otel_guard = crate::runtime::init_tracing(traces.path()).unwrap();
        let shutdown = CancellationToken::new();

        // Orchestrator that panics immediately.
        let orch_handle = tokio::spawn(async {
            panic!("intentional orchestrator panic for B2/B1 regression test");
        });
        // Give the panic time to propagate into the JoinHandle.
        tokio::time::sleep(Duration::from_millis(10)).await;

        let sweeper_shutdown = shutdown.clone();
        let sweeper_handle = tokio::spawn(async move {
            sweeper_shutdown.cancelled().await;
        });
        let listener_shutdown = shutdown.clone();
        let listener_handle: tokio::task::JoinHandle<Result<(), control::ControlError>> =
            tokio::spawn(async move {
                listener_shutdown.cancelled().await;
                Ok(())
            });

        // Wrap in timeout — the test MUST NOT hang (G21.4). A panicked
        // orchestrator is the CRASH path (graceful = false) — the drain does
        // not run, so the bus/pool here are never touched.
        let pool = connect("sqlite::memory:").await.unwrap();
        let bus = Arc::new(EventBus::new(
            pool.clone(),
            vec![Arc::new(RecordingObserver::new())],
        ));
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            supervise(
                orch_handle,
                sweeper_handle,
                listener_handle,
                shutdown,
                otel_guard,
                bus,
                pool,
            ),
        )
        .await
        .expect("supervise must not hang when orchestrator panics");

        assert!(
            matches!(result, Err(BootError::OrchestratorPanicked(_))),
            "panicked orchestrator → Err(OrchestratorPanicked), got {result:?}",
        );
    }
}
