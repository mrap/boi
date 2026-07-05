//! `boi daemon` — the subcommand handler — and [`DaemonState`], the
//! control-socket [`CommandHandler`] the
//! daemon serves.
//!
//! ## The handler is a concurrent bus producer
//!
//! Every write-side command arrives here as a [`DaemonCommand`]. `DaemonState`
//! translates it into a [`BoiEvent`], runs
//! emit-Phases 1–3 through the daemon's own [`EventBus`], then — like a drain
//! task or the sweeper, and **unlike** the orchestrator's `handle_*` code (the
//! C1 invariant) — pushes the event onto the orchestrator's bounded channel
//! (`daemon_tx.send`). The orchestrator then routes it: `SpecStarted` runs the
//! first phase, `SpecCanceled` cancels drains, `TaskUnblocked` re-runs the
//! task, etc. `transitions.rs` arbitrates inside the bus emit — an illegal
//! transition is a loud `DaemonResponse::Err`, never a silent flip.
//!
//! ## `Dispatch` ordering (review (c) — the legal `running → failed`)
//!
//! `boi dispatch` persists the structural rows itself; the daemon's `Dispatch`
//! handler emits `SpecStarted` (`queued → running`) **first**, *then* runs
//! `runtime::preflight`. A preflight failure is therefore a **legal
//! `running → failed{PreflightFailed}`** — emitting `failed` on a still-`queued`
//! spec would be the illegal `queued → failed` the transition guard rejects.
//! On a preflight failure the orchestrator is NOT notified of `SpecStarted`, so
//! no phase ever runs (zero `phase_runs` rows — the pre-spend gate).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::sync::mpsc;

use crate::cli::boot;
use crate::cli::control::CommandHandler;
use crate::config::{PhaseDef, SkillRef};
use crate::repo;
use crate::service::registry::DaemonNotification;
use crate::service::{DaemonCommand, DaemonResponse, EventBus};
use crate::types::context::SpecContract;
use crate::types::event::BoiEvent;
use crate::types::ids::{SpecId, TaskId};
use crate::types::reasons::{BlockedReason, CancellationReason, FailureReason};
use crate::types::state::SpecStatus;

/// Run the `boi daemon serve` subcommand — the long-running boot loop.
///
/// Production passes a [`RuntimeExecutor`](crate::runtime::RuntimeExecutor) to
/// [`boot`] — the worker-vs-deterministic `PhaseExecutor` wired into the
/// orchestrator. `boot` blocks until graceful shutdown.
///
/// ## Logging is wired here, NOT in `boot::boot`
///
/// T8: the daemon's `tracing-subscriber` fmt layer is installed at the very
/// top of `run` — before `build_runtime_executor`, which calls
/// `repo::connect` and faults on a fresh `~/.boi/v2/`. If logging were
/// initialized inside `boot::boot` (as it once was), a `repo::connect`
/// failure would die with `eprintln!`-formatted text and no `tracing::info!`
/// would ever reach stderr — operators would see a 0-byte `daemon.log`
/// despite a real boot fault. Installing here guarantees the cold-boot
/// "boi daemon starting" line is the FIRST thing on stderr, naming the
/// version + socket path the daemon will bind.
pub async fn run() -> Result<(), boot::BootError> {
    init_daemon_logging();
    let socket_path = crate::cli::paths::control_socket()?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        socket = %socket_path.display(),
        "boi daemon starting",
    );
    let executor = build_runtime_executor().await?;
    boot::boot(executor).await
}

/// The reverse-DNS label the boi daemon registers as with launchd / systemd.
/// Backs `~/Library/LaunchAgents/com.boi.daemon.plist` on macOS (or the
/// equivalent systemd --user unit on Linux).
pub const BOI_DAEMON_LABEL: &str = "com.boi.daemon";

/// Build the boi daemon's [`daemon_green::ServiceSpec`].
///
/// - `program` = `std::env::current_exe()` — the absolute path to the running
///   `boi` binary (NOT a hard-coded `~/.boi/bin/boi`), so a `cargo run`-style
///   install registers the dev binary and a stable install registers the
///   stable one.
/// - `args` = `["daemon", "serve"]` — the explicit boot-loop subcommand
///   (backward compat: bare `boi daemon` still works, but `serve` is the
///   canonical form newly-deployed plists should ride on).
/// - `env` mirrors the keys the currently-deployed
///   `com.boi.daemon.plist` sets: `HOME`, `PATH` (the operator's tool
///   paths the daemon shells out into — `/opt/homebrew/bin`, `~/.boi/bin`,
///   the standard system dirs), and `RUST_LOG`. Read straight off the
///   current process's env so an operator-overridden `RUST_LOG` propagates
///   without code changes. Also bakes `BOI_PHASE_WALL_CLOCK_BUDGET_SECS` (the
///   resolved per-phase reap budget) so a reinstall persists an operator
///   override instead of reverting it to the default.
/// - `keep_alive` + `run_at_load` = `true` (restart-on-crash; start at login).
/// - `log_path` = `~/.boi/v2/logs/daemon.log` (the plist's
///   StandardOut/StandardError path today).
/// - NO `SessionCreate` — `daemon_green` guarantees its absence (the macOS
///   launchd backend renders a plist that omits the key entirely so the
///   service stays inside the user's login session and can reach the login
///   keychain).
fn build_service_spec() -> Result<daemon_green::ServiceSpec, boot::BootError> {
    use crate::cli::paths;

    let program = std::env::current_exe()
        .map_err(|e| boot::BootError::Lifecycle(format!("could not resolve current_exe(): {e}")))?;
    // `~/.boi/v2/logs/daemon.log` — matches the deployed plist's
    // StandardOut/StandardError path. `paths::boi_root()` is `~/.boi/v2/`.
    let log_dir = paths::boi_root()?.join("logs");
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        return Err(boot::BootError::Lifecycle(format!(
            "could not create log dir {}: {e}",
            log_dir.display()
        )));
    }
    let log_path = log_dir.join("daemon.log");
    let home = std::env::var("HOME").unwrap_or_default();
    let path_env = std::env::var("PATH").unwrap_or_else(|_| {
        // Sensible default mirroring the deployed plist.
        format!("{home}/.boi/bin:{home}/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin")
    });
    let rust_log = std::env::var("RUST_LOG").unwrap_or_else(|_| "info,boi=info".to_owned());
    // Bake the resolved per-phase wall-clock budget into the plist so a
    // `boi daemon start` / reinstall persists an operator override instead of
    // silently reverting it to the default. Before this, `BOI_PHASE_WALL_CLOCK_
    // BUDGET_SECS` lived only as a hand-edit to the deployed plist, which the
    // next reinstall (rebuilding the spec from this fn) clobbered — the
    // 2026-06-11 incident where 3600s reverted to 1200s and starved 3 specs.
    let phase_budget_secs = boot::phase_wall_clock_budget().as_secs().to_string();

    // Secrets are NOT baked into the plist. The daemon reads ~/.boi/v2/secrets/*.env
    // at startup via runtime::secrets::bootstrap_provider_env (called in main()
    // before the tokio runtime starts). The plist carries only non-secret config.
    let spec = daemon_green::ServiceSpec::new(BOI_DAEMON_LABEL, program)
        .args(["daemon", "serve"])
        .env("HOME", home)
        .env("PATH", path_env)
        .env("RUST_LOG", rust_log)
        .env(boot::PHASE_WALL_CLOCK_BUDGET_ENV, phase_budget_secs)
        .keep_alive(true)
        .run_at_load(true)
        .log_path(log_path);
    Ok(spec)
}

/// Map a `daemon_green::Error` into a [`boot::BootError::Lifecycle`].
fn lifecycle_err(e: daemon_green::Error) -> boot::BootError {
    boot::BootError::Lifecycle(e.to_string())
}

/// `boi daemon start` — install the LaunchAgent (idempotent) and start it.
pub fn lifecycle_start() -> Result<(), boot::BootError> {
    let spec = build_service_spec()?;
    let mgr = daemon_green::native();
    mgr.install(&spec).map_err(lifecycle_err)?;
    mgr.start(spec.label()).map_err(lifecycle_err)?;
    println!("boi daemon started (label = {BOI_DAEMON_LABEL})");
    Ok(())
}

/// `boi daemon stop` — stop + unload the LaunchAgent.
pub fn lifecycle_stop() -> Result<(), boot::BootError> {
    let mgr = daemon_green::native();
    mgr.stop(BOI_DAEMON_LABEL).map_err(lifecycle_err)?;
    println!("boi daemon stopped (label = {BOI_DAEMON_LABEL})");
    Ok(())
}

/// `boi daemon restart` — restart the LaunchAgent (e.g. to pick up a new binary).
pub fn lifecycle_restart() -> Result<(), boot::BootError> {
    let mgr = daemon_green::native();
    mgr.restart(BOI_DAEMON_LABEL).map_err(lifecycle_err)?;
    println!("boi daemon restarted (label = {BOI_DAEMON_LABEL})");
    Ok(())
}

/// `boi daemon status` — print a one-line human status.
pub fn lifecycle_status() -> Result<(), boot::BootError> {
    let mgr = daemon_green::native();
    let status = mgr.status(BOI_DAEMON_LABEL).map_err(lifecycle_err)?;
    let line = match status {
        daemon_green::ServiceStatus::Running { pid: Some(pid) } => {
            format!("Running (pid {pid})")
        }
        daemon_green::ServiceStatus::Running { pid: None } => "Running".to_owned(),
        daemon_green::ServiceStatus::Stopped => "Stopped".to_owned(),
        daemon_green::ServiceStatus::NotInstalled => "NotInstalled".to_owned(),
        daemon_green::ServiceStatus::Failed { reason } => format!("Failed: {reason}"),
    };
    println!("{BOI_DAEMON_LABEL}: {line}");
    Ok(())
}

/// Wire `tracing-subscriber` to write INFO+ to stderr so launchd's
/// `StandardErrorPath` captures it and `RUST_LOG` actually works.
///
/// `try_init` returns `Err` if a global subscriber is already installed
/// (e.g. a test process set one up first); we honor that and move on
/// silently — the daemon binary itself only ever runs this once at startup.
/// This is composed alongside the OTel `TracerProvider` that
/// [`init_tracing`](crate::runtime::init_tracing) installs later — the two
/// are independent: the OTel SDK speaks directly to its own
/// `TracerProvider`, not through a `tracing_subscriber` layer, so adding the
/// fmt layer does not affect span emission to `~/.boi/v2/traces/`.
fn init_daemon_logging() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    // `with_ansi(false)` is load-bearing: launchd's `StandardErrorPath`
    // redirects daemon stderr to a plain log file, and the fmt layer
    // otherwise paints ANSI escapes around every key/value pair — fine in a
    // terminal, but garbage in `daemon.log`. The escapes also break naive
    // substring scans (e.g. an `INFO` health-check looking for `socket=…`
    // would miss the field because the field name and `=` are wrapped
    // individually in escape sequences).
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_ansi(false);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,boi=info"));
    // `.ok()` (Option) instead of `let _ =` (Result) — the workspace lints
    // ban `clippy::let_underscore_must_use` on must-use Results; converting
    // to Option satisfies it without losing the deliberate "ignore if a
    // global subscriber is already installed" semantics.
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .try_init()
        .ok();
}

/// Construct the production [`RuntimeExecutor`]
/// — the executor seam `boot` is handed (G16.2).
///
/// G23.1 note: skills do not reach `GooseRuntime` through the `PhaseExecutor`
/// port (`PhaseContext` carries no skills). The daemon's preflight checks the
/// spec's skills, but the recipe a worker phase actually runs is built with an
/// empty skill set — the `[[skill]]` Goose extensions are a documented v1.0
/// gap (see the Phase 9 report). The BOI MCP-server extension is always wired.
///
/// `async` — `repo::connect` opens a `sqlx` SQLite pool that needs the tokio
/// reactor; it is `.await`ed on the runtime, never `block_on`-ed (a
/// foreign-executor `block_on` of a tokio-dependent future on a tokio worker
/// thread can deadlock).
async fn build_runtime_executor() -> Result<Arc<dyn crate::service::PhaseExecutor>, boot::BootError>
{
    use crate::cli::paths;
    use crate::runtime::{DeterministicExecutor, GooseRuntime, RuntimeExecutor};

    let db_url = paths::boi_db_url()?;
    let pool = repo::connect(&db_url).await?;
    let bus = Arc::new(EventBus::new(
        pool.clone(),
        // The executor's own bus needs no observers — it only relays
        // through the daemon's bus via the drain. A bare bus suffices.
        vec![],
    ));
    // G26.1 — `GooseRuntime` resolves a worker phase's `prompt_template`
    // FILENAME against the prompts dir. Phase TOMLs and their prompt templates
    // co-locate in `~/.boi/v2/phases/`, so the prompts dir IS `phases_dir()`.
    let goose = GooseRuntime::new(
        PathBuf::from("goose"),
        paths::recipes_dir()?,
        paths::phases_dir()?,
    );
    let deterministic = DeterministicExecutor::new(bus, pool);
    Ok(Arc::new(RuntimeExecutor::new(goose, deterministic)))
}

/// The control-socket command handler — the daemon's bus + channel + dispatch
/// context.
///
/// Holds an [`Arc<EventBus>`] (the daemon's own bus — emit-Phases 1–3), a
/// `daemon_tx` clone (emit-Phase 4 — the concurrent-producer send), the loaded
/// phase definitions + the `goose` binary path + the provider liveness probe
/// (all three for `Dispatch` preflight), and the recipe directory.
pub struct DaemonState {
    bus: Arc<EventBus>,
    pool: SqlitePool,
    daemon_tx: mpsc::Sender<DaemonNotification>,
    phases: HashMap<String, PhaseDef>,
    goose_bin: PathBuf,
    #[allow(dead_code)] // wired for parity with the recipe path; v1.0 unused here
    recipes_dir: PathBuf,
    /// The provider liveness probe the `Dispatch` preflight runs (429
    /// hardening) — `CurlProviderProbe` in production, a stub in tests.
    probe: Arc<dyn crate::runtime::ProviderProbe>,
}

impl DaemonState {
    /// Construct the handler. Called by [`boot`].
    pub fn new(
        bus: Arc<EventBus>,
        pool: SqlitePool,
        daemon_tx: mpsc::Sender<DaemonNotification>,
        phases: HashMap<String, PhaseDef>,
        goose_bin: PathBuf,
        recipes_dir: PathBuf,
        probe: Arc<dyn crate::runtime::ProviderProbe>,
    ) -> Self {
        Self {
            bus,
            pool,
            daemon_tx,
            phases,
            goose_bin,
            recipes_dir,
            probe,
        }
    }

    /// Emit an event through the bus, then notify the orchestrator.
    ///
    /// The concurrent-producer pattern (like the sweeper): `bus.emit` runs
    /// emit-Phases 1–3 — the persist + the transition guard; on `Ok` the event
    /// goes onto the bounded channel so the orchestrator routes it. A bus
    /// `Err` (an illegal transition, a persist fault) is returned so the
    /// handler can surface a `DaemonResponse::Err`.
    async fn emit_and_notify(&self, event: BoiEvent) -> Result<(), String> {
        self.bus
            .emit(&event)
            .await
            .map_err(|e| format!("event rejected: {e}"))?;
        if self
            .daemon_tx
            .send(DaemonNotification::Event(event))
            .await
            .is_err()
        {
            return Err("the orchestrator channel is closed — the daemon is shutting down".into());
        }
        Ok(())
    }

    /// Handle `boi dispatch`: emit `SpecStarted`, run preflight, route the spec.
    async fn handle_dispatch(
        &self,
        spec_id: &str,
        skills: &[String],
        _spec_file: Option<&str>,
    ) -> DaemonResponse {
        let sid = match SpecId::new(spec_id) {
            Ok(s) => s,
            Err(e) => {
                return DaemonResponse::Err {
                    detail: format!("invalid spec id `{spec_id}`: {e}"),
                };
            }
        };

        // (1) — `SpecStarted` (`queued → running`). Emit on the bus FIRST; do
        // NOT notify the orchestrator yet — preflight gates whether a phase
        // ever runs.
        let started = BoiEvent::SpecStarted {
            spec_id: sid.clone(),
        };
        if let Err(e) = self.bus.emit(&started).await {
            return DaemonResponse::Err {
                detail: format!("could not start spec {spec_id}: {e}"),
            };
        }

        // (2a) — the GitFlow branch-policy backstop (Layer 2, R-B7), AFTER
        // `SpecStarted` like the goose preflight, so a failure is a legal
        // `running → failed{PreflightFailed}`. It runs FIRST (no subprocess,
        // no provider spend) and catches what the Layer-1 CLI gate cannot
        // see: direct control-socket dispatches and specs persisted by a
        // pre-gate binary. The contract comes from the persisted snapshot;
        // the policy itself is re-read fresh from the committed tree (D-13).
        if let Err(detail) = self.branch_policy_preflight(&sid).await {
            let failed = BoiEvent::SpecFailed {
                spec_id: sid.clone(),
                reason: FailureReason::PreflightFailed {
                    details: detail.clone(),
                },
            };
            return match self.bus.emit(&failed).await {
                Ok(()) => DaemonResponse::Err {
                    detail: format!("preflight failed for {spec_id}: {detail}"),
                },
                Err(emit_err) => DaemonResponse::Err {
                    detail: format!(
                        "preflight failed for {spec_id} ({detail}), \
                         and the SpecFailed emit also failed: {emit_err}"
                    ),
                },
            };
        }

        // (2) — preflight, AFTER `SpecStarted`, so a failure is a legal
        // `running → failed` (review (c)).
        let phase_defs: Vec<PhaseDef> = self.phases.values().cloned().collect();
        let skill_refs: Vec<SkillRef> = skills
            .iter()
            .map(|name| SkillRef { name: name.clone() })
            .collect();
        if let Err(e) =
            crate::runtime::preflight(&self.goose_bin, &phase_defs, &skill_refs, &*self.probe).await
        {
            // A legal `running → failed{PreflightFailed}` — no phase ran.
            let failed = BoiEvent::SpecFailed {
                spec_id: sid.clone(),
                reason: FailureReason::PreflightFailed {
                    details: e.to_string(),
                },
            };
            return match self.bus.emit(&failed).await {
                Ok(()) => DaemonResponse::Err {
                    detail: format!("preflight failed for {spec_id}: {e}"),
                },
                Err(emit_err) => DaemonResponse::Err {
                    detail: format!(
                        "preflight failed for {spec_id} ({e}), \
                         and the SpecFailed emit also failed: {emit_err}"
                    ),
                },
            };
        }

        // (3) — preflight passed: notify the orchestrator of `SpecStarted` so
        // it routes the spec into its pipeline.
        if self
            .daemon_tx
            .send(DaemonNotification::Event(started))
            .await
            .is_err()
        {
            return DaemonResponse::Err {
                detail: "the orchestrator channel is closed — cannot start the spec".into(),
            };
        }
        DaemonResponse::Ok {
            detail: format!("spec {spec_id} started"),
        }
    }

    /// GitFlow Layer 2 (R-B7): load the spec's contract from its persisted
    /// snapshot and evaluate the workspace branch policy
    /// (`runtime::branch_policy_gate`).
    ///
    /// `Err` carries the fully-rendered failure detail for
    /// `FailureReason::PreflightFailed`. An unreadable snapshot is as loud as
    /// a policy violation (S6) — a backstop that silently skipped evaluation
    /// when it cannot see the contract would not be a backstop.
    async fn branch_policy_preflight(&self, sid: &SpecId) -> Result<(), String> {
        let version = repo::spec_runtime::fetch(&self.pool, sid)
            .await
            .map_err(|e| format!("cannot read spec_runtime for the branch-policy preflight: {e}"))?
            .current_version;
        let snapshot = repo::spec_versions::fetch_snapshot(&self.pool, sid, version)
            .await
            .map_err(|e| {
                format!("cannot read the spec snapshot for the branch-policy preflight: {e}")
            })?;
        let contract_value = snapshot.get("spec_contract").cloned().ok_or_else(|| {
            "the spec snapshot has no `spec_contract` key — \
             cannot evaluate the workspace branch policy"
                .to_owned()
        })?;
        let contract: SpecContract = serde_json::from_value(contract_value).map_err(|e| {
            format!(
                "the snapshot `spec_contract` is malformed — \
                 cannot evaluate the workspace branch policy: {e}"
            )
        })?;
        crate::runtime::branch_policy_gate(contract.workspace, contract.base_branch)
            .await
            .map_err(|e| e.to_string())
    }

    /// Handle `boi cancel <id>` — resolve whether `id` names a spec or a task.
    async fn handle_cancel(&self, id: &str, reason: &str) -> DaemonResponse {
        let note = Some(reason.to_owned());
        // Try a spec id first.
        if let Ok(sid) = SpecId::new(id) {
            match repo::specs::exists(&self.pool, &sid).await {
                Ok(true) => {
                    let event = BoiEvent::SpecCanceled {
                        spec_id: sid,
                        reason: CancellationReason::Operator { note },
                    };
                    return match self.emit_and_notify(event).await {
                        Ok(()) => DaemonResponse::Ok {
                            detail: format!("spec {id} canceled"),
                        },
                        Err(e) => DaemonResponse::Err { detail: e },
                    };
                }
                Ok(false) => {} // fall through to task check
                Err(e) => {
                    tracing::error!(spec_id = %id, error = %e, "handle_cancel: DB error checking spec existence");
                    return DaemonResponse::Err {
                        detail: format!("could not look up spec `{id}`: {e}"),
                    };
                }
            }
        }
        // Otherwise a task id.
        if let Ok(tid) = TaskId::new(id) {
            match repo::task_runtime::fetch(&self.pool, &tid).await {
                Ok(task) => {
                    let spec_id = match SpecId::new(&task.spec_id) {
                        Ok(s) => s,
                        Err(e) => {
                            return DaemonResponse::Err {
                                detail: format!("task {id} has a corrupt spec id: {e}"),
                            };
                        }
                    };
                    let event = BoiEvent::TaskCanceled {
                        spec_id,
                        task_id: tid,
                        reason: CancellationReason::Operator { note },
                    };
                    return match self.emit_and_notify(event).await {
                        Ok(()) => DaemonResponse::Ok {
                            detail: format!("task {id} canceled"),
                        },
                        Err(e) => DaemonResponse::Err { detail: e },
                    };
                }
                Err(e) => {
                    tracing::error!(task_id = %id, error = %e, "handle_cancel: DB error fetching task");
                    return DaemonResponse::Err {
                        detail: format!("could not look up task `{id}`: {e}"),
                    };
                }
            }
        }
        DaemonResponse::Err {
            detail: format!("no spec or task with id `{id}`"),
        }
    }

    /// Handle `boi unblock <task_id> [--reset-counter]`.
    async fn handle_unblock(&self, task_id: &str, reset_counter: bool) -> DaemonResponse {
        let tid = match TaskId::new(task_id) {
            Ok(t) => t,
            Err(e) => {
                return DaemonResponse::Err {
                    detail: format!("invalid task id `{task_id}`: {e}"),
                };
            }
        };
        let task = match repo::task_runtime::fetch(&self.pool, &tid).await {
            Ok(t) => t,
            Err(e) => {
                return DaemonResponse::Err {
                    detail: format!("no such task `{task_id}`: {e}"),
                };
            }
        };
        let spec_id = match SpecId::new(&task.spec_id) {
            Ok(s) => s,
            Err(e) => {
                return DaemonResponse::Err {
                    detail: format!("task {task_id} has a corrupt spec id: {e}"),
                };
            }
        };

        // AUDIT A2 — truthful operator-command validation (SO S6). A
        // non-`blocked` task cannot be unblocked: without this check the bus's
        // transition guard rejects the emit with a cryptic "illegal task
        // transition" that sends the operator nowhere. Name the real state.
        if task.state != "blocked" {
            return DaemonResponse::Err {
                detail: format!(
                    "task {task_id} is not blocked (state: `{}`) — \
                     `boi unblock` only revives a blocked task",
                    task.state
                ),
            };
        }
        // A TERMINAL spec cannot be revived (§6 — terminal statuses have no
        // exit edge): without this check the `blocked → active` task edge is
        // legal, the command reports a falsely-green "unblocked", and the
        // orchestrator runs phases under a dead spec whose terminal events
        // are all rejected. Loud and truthful instead.
        let spec_row = match repo::spec_runtime::fetch(&self.pool, &spec_id).await {
            Ok(r) => r,
            Err(e) => {
                return DaemonResponse::Err {
                    detail: format!("no spec_runtime row for {spec_id}: {e}"),
                };
            }
        };
        let spec_status = match spec_row.status.parse::<SpecStatus>() {
            Ok(s) => s,
            Err(e) => {
                return DaemonResponse::Err {
                    detail: format!("spec {spec_id} has a corrupt status: {e}"),
                };
            }
        };
        if matches!(
            spec_status,
            SpecStatus::Completed | SpecStatus::Failed | SpecStatus::Canceled
        ) {
            return DaemonResponse::Err {
                detail: format!(
                    "task {task_id}'s spec {spec_id} is terminal (`{}`) — a \
                     terminal spec cannot be revived; re-dispatch the spec",
                    spec_row.status
                ),
            };
        }

        // `--reset-counter` zeroes the iteration counters BEFORE the unblock
        // so a `CapExceeded` task does not immediately re-block.
        if reset_counter
            && let Err(e) = repo::task_runtime::reset_iterations(&self.pool, &tid).await
        {
            return DaemonResponse::Err {
                detail: format!("could not reset {task_id}'s iteration counters: {e}"),
            };
        }

        let event = BoiEvent::TaskUnblocked {
            spec_id,
            task_id: tid,
        };
        match self.emit_and_notify(event).await {
            Ok(()) => DaemonResponse::Ok {
                detail: if reset_counter {
                    format!("task {task_id} unblocked (iteration counters reset)")
                } else {
                    format!("task {task_id} unblocked")
                },
            },
            Err(e) => DaemonResponse::Err { detail: e },
        }
    }

    /// Handle `boi fail <spec_id> --reason` (G16.6 — `OperatorMarkedFailed`).
    async fn handle_fail(&self, spec_id: &str, reason: &str) -> DaemonResponse {
        let sid = match SpecId::new(spec_id) {
            Ok(s) => s,
            Err(e) => {
                return DaemonResponse::Err {
                    detail: format!("invalid spec id `{spec_id}`: {e}"),
                };
            }
        };
        let event = BoiEvent::SpecFailed {
            spec_id: sid,
            reason: FailureReason::OperatorMarkedFailed {
                note: Some(reason.to_owned()),
            },
        };
        match self.emit_and_notify(event).await {
            Ok(()) => DaemonResponse::Ok {
                detail: format!("spec {spec_id} marked failed"),
            },
            Err(e) => DaemonResponse::Err { detail: e },
        }
    }

    /// Handle `boi resolve-conflict <task_id>` — the interactive shell flow.
    async fn handle_resolve_conflict(&self, task_id: &str) -> DaemonResponse {
        let tid = match TaskId::new(task_id) {
            Ok(t) => t,
            Err(e) => {
                return DaemonResponse::Err {
                    detail: format!("invalid task id `{task_id}`: {e}"),
                };
            }
        };
        let task = match repo::task_runtime::fetch(&self.pool, &tid).await {
            Ok(t) => t,
            Err(e) => {
                return DaemonResponse::Err {
                    detail: format!("no such task `{task_id}`: {e}"),
                };
            }
        };
        let spec_id = match SpecId::new(&task.spec_id) {
            Ok(s) => s,
            Err(e) => {
                return DaemonResponse::Err {
                    detail: format!("task {task_id} has a corrupt spec id: {e}"),
                };
            }
        };

        // The task must be `blocked` with a `MergeConflict` reason.
        let conflict_branch = match &task.blocked_reason {
            Some(j) => match serde_json::from_value::<BlockedReason>(j.clone()) {
                Ok(BlockedReason::MergeConflict { .. }) => {
                    // Post-schema-rework (conflict-resolver track) the
                    // reason no longer names a branch; the worktree +
                    // integration branch are derived from the task's
                    // spec_id + task_id below. This string is log-only.
                    format!("spec/{spec_id}/{tid}")
                }
                Ok(other) => {
                    return DaemonResponse::Err {
                        detail: format!(
                            "task {task_id} is not merge-conflicted (blocked: {other:?}) — \
                             `boi resolve-conflict` only applies to a MergeConflict"
                        ),
                    };
                }
                Err(e) => {
                    return DaemonResponse::Err {
                        detail: format!("task {task_id} has a corrupt blocked reason: {e}"),
                    };
                }
            },
            None => {
                return DaemonResponse::Err {
                    detail: format!("task {task_id} is not blocked — nothing to resolve"),
                };
            }
        };

        // Derive the task worktree from the §5 layout (G22.2 — the
        // `task_runtime.worktree_path` column is documented dead at v1.0; the
        // §5-layout derivation is canonical, matching `RuntimeToolHost`).
        let worktree = crate::runtime::worktree::task_worktree(
            &crate::runtime::worktree::default_worktree_root(),
            &spec_id,
            &tid,
        );
        let integration = crate::runtime::worktree::integration_branch(&spec_id);
        tracing::info!(task = %task_id, branch = %conflict_branch, "resolve-conflict: opening interactive shell");

        match crate::runtime::resolve_interactively(&worktree, &integration).await {
            Ok(crate::runtime::ResolveOutcome::Resolved) => {
                // A clean resolution — `blocked → active`.
                let event = BoiEvent::TaskUnblocked {
                    spec_id,
                    task_id: tid,
                };
                match self.emit_and_notify(event).await {
                    Ok(()) => DaemonResponse::Ok {
                        detail: format!("task {task_id} conflict resolved and unblocked"),
                    },
                    Err(e) => DaemonResponse::Err { detail: e },
                }
            }
            Ok(crate::runtime::ResolveOutcome::StillConflicted { detail }) => {
                // A half-resolved rebase re-blocks: emit NOTHING, the task
                // stays `blocked`.
                DaemonResponse::Err {
                    detail: format!(
                        "task {task_id} still conflicted ({detail}) — \
                         left blocked; re-run `boi resolve-conflict {task_id}`"
                    ),
                }
            }
            Err(e) => DaemonResponse::Err {
                detail: format!("resolve-conflict for {task_id} failed: {e}"),
            },
        }
    }

    /// Handle a `boi mcp-serve`-forwarded `BoiEvent` — re-emit it on the
    /// daemon's bus so the orchestrator sees it.
    async fn handle_forward_event(&self, event_json: serde_json::Value) -> DaemonResponse {
        let event: BoiEvent = match serde_json::from_value(event_json) {
            Ok(e) => e,
            Err(e) => {
                return DaemonResponse::Err {
                    detail: format!("forwarded event is malformed: {e}"),
                };
            }
        };
        match self.emit_and_notify(event).await {
            Ok(()) => DaemonResponse::Ok {
                detail: "event forwarded".to_owned(),
            },
            Err(e) => DaemonResponse::Err { detail: e },
        }
    }
}

#[async_trait::async_trait]
impl CommandHandler for DaemonState {
    async fn handle(&self, command: DaemonCommand) -> DaemonResponse {
        match command {
            DaemonCommand::Dispatch {
                spec_id,
                skills,
                spec_file,
            } => {
                self.handle_dispatch(&spec_id, &skills, spec_file.as_deref())
                    .await
            }
            DaemonCommand::Cancel { id, reason } => self.handle_cancel(&id, &reason).await,
            DaemonCommand::Unblock {
                task_id,
                reset_counter,
            } => self.handle_unblock(&task_id, reset_counter).await,
            DaemonCommand::ResolveConflict { task_id } => {
                self.handle_resolve_conflict(&task_id).await
            }
            DaemonCommand::Fail { spec_id, reason } => self.handle_fail(&spec_id, &reason).await,
            DaemonCommand::ForwardEvent { event } => self.handle_forward_event(event).await,
        }
    }
}

/// A spec's terminal status, for a one-line check.
///
/// Small helper used by tests + reused conceptually by recovery — kept here so
/// `daemon` does not import `SpecStatus`-parsing logic twice.
#[allow(dead_code)]
fn is_terminal(status: &str) -> bool {
    matches!(
        status.parse::<SpecStatus>(),
        Ok(SpecStatus::Completed | SpecStatus::Failed | SpecStatus::Canceled)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::db::connect;
    use crate::repo::spec_versions::VersionTrigger;
    use crate::service::bus::testkit::RecordingObserver;
    use crate::types::reasons::BlockedReason;
    use crate::types::state::TaskState;
    use chrono::Utc;

    /// The installed service spec must carry the phase wall-clock budget in its
    /// env, so a `boi daemon start` / reinstall bakes the operator's configured
    /// budget into the plist instead of silently reverting it to the default
    /// (the 2026-06-11 wedge: a manual-plist 3600s was clobbered back to 1200s
    /// on the next reinstall, starving long phases).
    #[test]
    fn build_service_spec_carries_phase_budget_env() {
        let spec = build_service_spec().expect("service spec builds");
        let got = spec
            .env
            .get(boot::PHASE_WALL_CLOCK_BUDGET_ENV)
            .expect("phase wall-clock budget env is present on the ServiceSpec");
        // The baked value matches the resolver the running daemon uses, so the
        // plist and the daemon agree on the budget.
        assert_eq!(*got, boot::phase_wall_clock_budget().as_secs().to_string());
    }

    /// A [`crate::runtime::ProviderProbe`] stub that always passes — daemon
    /// tests never hit a real provider.
    struct OkProbe;

    impl crate::runtime::ProviderProbe for OkProbe {
        fn probe<'a>(
            &'a self,
            _provider: &'a str,
            _model: &'a str,
        ) -> futures::future::BoxFuture<'a, crate::runtime::ProbeOutcome> {
            Box::pin(async { crate::runtime::ProbeOutcome::Ok })
        }
    }

    /// Build a `DaemonState` over an in-memory pool, plus the channel receiver
    /// and the bus's recorder.
    fn state_for(
        pool: SqlitePool,
    ) -> (
        DaemonState,
        mpsc::Receiver<DaemonNotification>,
        RecordingObserver,
    ) {
        let recorder = RecordingObserver::new();
        let bus = Arc::new(EventBus::new(
            pool.clone(),
            vec![Arc::new(recorder.clone())],
        ));
        let (tx, rx) = mpsc::channel(64);
        let state = DaemonState::new(
            bus,
            pool,
            tx,
            HashMap::new(),
            PathBuf::from("goose"),
            PathBuf::from("/tmp/recipes"),
            Arc::new(OkProbe),
        );
        (state, rx, recorder)
    }

    /// Seed a spec + one task in the given state.
    async fn seed(pool: &SqlitePool, task_state: TaskState, blocked: Option<BlockedReason>) {
        let spec = SpecId::new("S0000001a").unwrap();
        let task = TaskId::new("T0000001a").unwrap();
        repo::specs::insert_spec(pool, &spec, Utc::now())
            .await
            .unwrap();
        repo::spec_versions::append_version(
            pool,
            &spec,
            1,
            &serde_json::json!({ "title": "demo" }),
            VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        repo::spec_runtime::initialize(pool, &spec, 1)
            .await
            .unwrap();
        repo::task_runtime::insert_task(pool, &task, &spec, None)
            .await
            .unwrap();
        if task_state != TaskState::NotStarted {
            repo::task_runtime::update_state(
                pool,
                &task,
                TaskState::Active,
                None,
                None,
                Utc::now(),
            )
            .await
            .unwrap();
        }
        if task_state == TaskState::Blocked {
            repo::task_runtime::update_state(
                pool,
                &task,
                TaskState::Blocked,
                None,
                blocked,
                Utc::now(),
            )
            .await
            .unwrap();
        }
    }

    /// `boi cancel <spec>` → a `SpecCanceled` lands on the bus and the channel.
    #[tokio::test]
    async fn test_l2_cancel_spec_emits_spec_canceled() {
        let pool = connect("sqlite::memory:").await.unwrap();
        seed(&pool, TaskState::Active, None).await;
        // The spec must be `running` for `running → canceled` to be legal.
        repo::spec_runtime::update_status(
            &pool,
            &SpecId::new("S0000001a").unwrap(),
            SpecStatus::Running,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        let (state, mut rx, recorder) = state_for(pool);

        let resp = state
            .handle(DaemonCommand::Cancel {
                id: "S0000001a".to_owned(),
                reason: "scope cut".to_owned(),
            })
            .await;
        assert!(matches!(resp, DaemonResponse::Ok { .. }), "got {resp:?}");
        assert!(
            recorder
                .seen()
                .iter()
                .any(|e| matches!(e, BoiEvent::SpecCanceled { .. })),
            "a SpecCanceled was emitted",
        );
        assert!(
            matches!(
                rx.try_recv(),
                Ok(DaemonNotification::Event(BoiEvent::SpecCanceled { .. }))
            ),
            "the orchestrator was notified",
        );
    }

    /// `boi cancel` with an unknown id is a loud `DaemonResponse::Err`.
    #[tokio::test]
    async fn test_l2_cancel_unknown_id_is_error() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let (state, _rx, _rec) = state_for(pool);
        let resp = state
            .handle(DaemonCommand::Cancel {
                id: "S9999999z".to_owned(),
                reason: "x".to_owned(),
            })
            .await;
        assert!(matches!(resp, DaemonResponse::Err { .. }), "got {resp:?}");
    }

    /// `boi fail <spec>` → a `SpecFailed{OperatorMarkedFailed}` lands.
    #[tokio::test]
    async fn test_l2_fail_emits_operator_marked_failed() {
        let pool = connect("sqlite::memory:").await.unwrap();
        seed(&pool, TaskState::Active, None).await;
        repo::spec_runtime::update_status(
            &pool,
            &SpecId::new("S0000001a").unwrap(),
            SpecStatus::Running,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        let (state, _rx, _rec) = state_for(pool.clone());

        let resp = state
            .handle(DaemonCommand::Fail {
                spec_id: "S0000001a".to_owned(),
                reason: "abandoned".to_owned(),
            })
            .await;
        assert!(matches!(resp, DaemonResponse::Ok { .. }), "got {resp:?}");
        let row = repo::spec_runtime::fetch(&pool, &SpecId::new("S0000001a").unwrap())
            .await
            .unwrap();
        assert_eq!(row.status, "failed");
        assert_eq!(
            row.failure_reason
                .and_then(|j| j.get("type").and_then(|t| t.as_str()).map(str::to_owned)),
            Some("operator_marked_failed".to_owned()),
        );
    }

    /// `boi unblock --reset-counter` zeroes the task's iteration counters and
    /// emits `TaskUnblocked`.
    #[tokio::test]
    async fn test_l2_unblock_reset_counter_zeroes_counters() {
        let pool = connect("sqlite::memory:").await.unwrap();
        seed(
            &pool,
            TaskState::Blocked,
            Some(BlockedReason::Manual {
                operator_note: None,
            }),
        )
        .await;
        let task = TaskId::new("T0000001a").unwrap();
        // Bump a counter so the reset is observable.
        repo::task_runtime::increment_iteration(
            &pool,
            &task,
            repo::task_runtime::IterationCounter::ExecuteReview,
        )
        .await
        .unwrap();

        let (state, _rx, _rec) = state_for(pool.clone());
        let resp = state
            .handle(DaemonCommand::Unblock {
                task_id: "T0000001a".to_owned(),
                reset_counter: true,
            })
            .await;
        assert!(matches!(resp, DaemonResponse::Ok { .. }), "got {resp:?}");
        let row = repo::task_runtime::fetch(&pool, &task).await.unwrap();
        assert_eq!(row.state, "active", "the task is unblocked");
        assert_eq!(
            row.iterations_execute_review, 0,
            "the counter was reset to 0",
        );
    }

    /// AUDIT A2 — `boi unblock` on a task that is NOT `blocked` is a loud,
    /// TRUTHFUL error naming the task's real state — never a cryptic
    /// transition-guard rejection, and never an emitted `TaskUnblocked`.
    #[tokio::test]
    async fn test_l2_unblock_of_a_non_blocked_task_is_a_truthful_error() {
        let pool = connect("sqlite::memory:").await.unwrap();
        seed(&pool, TaskState::Active, None).await;
        let (state, _rx, recorder) = state_for(pool);

        let resp = state
            .handle(DaemonCommand::Unblock {
                task_id: "T0000001a".to_owned(),
                reset_counter: false,
            })
            .await;
        let DaemonResponse::Err { detail } = resp else {
            panic!("unblocking a non-blocked task must be an error, got {resp:?}");
        };
        assert!(
            detail.contains("not blocked") && detail.contains("active"),
            "the error must truthfully name the task's state, got: {detail}",
        );
        assert!(
            !recorder
                .seen()
                .iter()
                .any(|e| matches!(e, BoiEvent::TaskUnblocked { .. })),
            "no TaskUnblocked may be emitted for a non-blocked task",
        );
    }

    /// AUDIT A2 — `boi unblock` under a TERMINAL spec is a loud error: a
    /// terminal spec has no exit edge, so "unblocking" its task would be a
    /// falsely-green no-op revive (the orchestrator would run phases under a
    /// dead spec and every terminal event would be rejected). The task stays
    /// `blocked` and nothing is emitted.
    #[tokio::test]
    async fn test_l2_unblock_under_a_terminal_spec_is_a_loud_error() {
        let pool = connect("sqlite::memory:").await.unwrap();
        seed(
            &pool,
            TaskState::Blocked,
            Some(BlockedReason::Manual {
                operator_note: None,
            }),
        )
        .await;
        // Drive the spec to terminal `failed` (queued → running → failed; the
        // schema CHECK requires a failure reason on a `failed` row).
        let spec = SpecId::new("S0000001a").unwrap();
        repo::spec_runtime::update_status(&pool, &spec, SpecStatus::Running, None, Utc::now())
            .await
            .unwrap();
        repo::spec_runtime::update_status(
            &pool,
            &spec,
            SpecStatus::Failed,
            Some(repo::spec_runtime::TerminalReason::Failure(
                FailureReason::DaemonCrash,
            )),
            Utc::now(),
        )
        .await
        .unwrap();
        let (state, _rx, recorder) = state_for(pool.clone());

        let resp = state
            .handle(DaemonCommand::Unblock {
                task_id: "T0000001a".to_owned(),
                reset_counter: false,
            })
            .await;
        let DaemonResponse::Err { detail } = resp else {
            panic!("unblocking under a terminal spec must be an error, got {resp:?}");
        };
        assert!(
            detail.contains("terminal") && detail.contains("failed"),
            "the error must truthfully name the terminal spec status, got: {detail}",
        );
        // The task is untouched — still `blocked` — and nothing was emitted.
        let row = repo::task_runtime::fetch(&pool, &TaskId::new("T0000001a").unwrap())
            .await
            .unwrap();
        assert_eq!(row.state, "blocked", "the task stays blocked");
        assert!(
            !recorder
                .seen()
                .iter()
                .any(|e| matches!(e, BoiEvent::TaskUnblocked { .. })),
            "no TaskUnblocked may be emitted under a terminal spec",
        );
    }

    /// `boi resolve-conflict` on a non-blocked task is a loud error — it never
    /// emits a transition.
    #[tokio::test]
    async fn test_l2_resolve_conflict_on_unblocked_task_is_error() {
        let pool = connect("sqlite::memory:").await.unwrap();
        seed(&pool, TaskState::Active, None).await;
        let (state, _rx, _rec) = state_for(pool);
        let resp = state
            .handle(DaemonCommand::ResolveConflict {
                task_id: "T0000001a".to_owned(),
            })
            .await;
        assert!(
            matches!(resp, DaemonResponse::Err { .. }),
            "an unblocked task cannot be resolved, got {resp:?}",
        );
    }

    /// Regression for B1: a DB error in `handle_cancel` must surface as a loud
    /// `DaemonResponse::Err` naming the fault, never silently fall through to
    /// "no spec or task with id".
    ///
    /// Verified that without the fix (using `unwrap_or(false)`), this test fails
    /// because the DB error is swallowed and the response detail is "no spec or
    /// task with id `S0000001a`". With the fix, the error is loud.
    #[tokio::test]
    async fn test_l2_cancel_db_error_surfaces_as_loud_error_not_spec_not_found() {
        let pool = connect("sqlite::memory:").await.unwrap();
        seed(&pool, TaskState::Active, None).await;
        repo::spec_runtime::update_status(
            &pool,
            &SpecId::new("S0000001a").unwrap(),
            SpecStatus::Running,
            None,
            Utc::now(),
        )
        .await
        .unwrap();

        // Corrupt the DB: rename the specs table so `repo::specs::exists` returns
        // an Err (table not found), simulating a DB fault.
        {
            let mut conn = pool.acquire().await.unwrap();
            sqlx::query("PRAGMA foreign_keys = OFF")
                .execute(&mut *conn)
                .await
                .unwrap();
            sqlx::query("DROP TABLE specs")
                .execute(&mut *conn)
                .await
                .unwrap();
        }

        let (state, _rx, _rec) = state_for(pool);
        let resp = state
            .handle(DaemonCommand::Cancel {
                id: "S0000001a".to_owned(),
                reason: "test".to_owned(),
            })
            .await;

        match resp {
            DaemonResponse::Err { ref detail } => {
                assert!(
                    detail.contains("could not look up spec"),
                    "must name the DB fault, got: {detail}",
                );
                assert!(
                    !detail.contains("no spec or task"),
                    "must NOT silently fall through to 'no spec or task', got: {detail}",
                );
            }
            _ => panic!("expected DaemonResponse::Err, got {resp:?}"),
        }
    }

    /// `is_terminal` recognises the three terminal spec statuses.
    #[test]
    fn test_l1_is_terminal_recognises_terminal_statuses() {
        assert!(is_terminal("completed"));
        assert!(is_terminal("failed"));
        assert!(is_terminal("canceled"));
        assert!(!is_terminal("running"));
        assert!(!is_terminal("queued"));
    }

    /// GitFlow Layer 2 (R-B7): a queued spec whose persisted snapshot targets
    /// a protected base (gitflow marker, `base_branch = "main"`) fails the
    /// daemon's branch-policy preflight — a legal
    /// `running → failed{PreflightFailed}` carrying the policy detail, the
    /// orchestrator is never notified, and no phase runs. This is the
    /// backstop for dispatches that bypassed the CLI gate (direct socket
    /// clients, snapshots persisted by a pre-gate binary — the AC-6 stale
    /// -snapshot class).
    #[tokio::test]
    async fn test_l2_dispatch_branch_policy_preflight_fails_protected_base() {
        use crate::cli::testtmp::TempDir;
        use crate::runtime::branch_policy::testkit;

        let dir = TempDir::new("daemon-policy-preflight");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        testkit::init_repo_on_main(&workspace);
        testkit::commit_on_branch(
            &workspace,
            "main",
            &[(".boi-policy.toml", testkit::GITFLOW_MARKER)],
        );

        // Seed a queued spec whose snapshot contract targets the protected
        // base — persisted as if by a gate-less dispatcher.
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        repo::specs::insert_spec(&pool, &spec, Utc::now())
            .await
            .unwrap();
        let contract = SpecContract {
            scope: "demo".into(),
            workspace: workspace.clone(),
            base_branch: "main".into(),
            exclusions: vec![],
            verifications: vec![],
            must_emit: vec![],
        };
        repo::spec_versions::append_version(
            &pool,
            &spec,
            1,
            &serde_json::json!({
                "title": "demo",
                "delivery": "merge",
                "spec_contract": serde_json::to_value(&contract).unwrap(),
                "task_contracts": {},
            }),
            VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        repo::spec_runtime::initialize(&pool, &spec, 1)
            .await
            .unwrap();

        let main_oid_before = testkit::branch_oid(&workspace, "main");
        let (state, mut rx, recorder) = state_for(pool.clone());
        let resp = state
            .handle(DaemonCommand::Dispatch {
                spec_id: "S0000001a".to_owned(),
                skills: vec![],
                spec_file: None,
            })
            .await;

        // The response is a loud Err carrying the policy detail.
        let DaemonResponse::Err { detail } = resp else {
            panic!("a protected base must fail the dispatch, got {resp:?}");
        };
        assert!(detail.contains("preflight failed"), "{detail}");
        assert!(detail.contains("protected"), "{detail}");

        // A SpecFailed{PreflightFailed} was emitted; the spec is `failed`.
        assert!(
            recorder.seen().iter().any(|e| matches!(
                e,
                BoiEvent::SpecFailed {
                    reason: FailureReason::PreflightFailed { .. },
                    ..
                }
            )),
            "a SpecFailed{{PreflightFailed}} was emitted",
        );
        let row = repo::spec_runtime::fetch(&pool, &spec).await.unwrap();
        assert_eq!(row.status, "failed");

        // The orchestrator was never notified — no phase will ever run.
        assert!(
            rx.try_recv().is_err(),
            "the orchestrator must not be notified of a preflight-failed spec",
        );

        // And the protected ref never moved (the policy refused pre-spend).
        assert_eq!(
            testkit::branch_oid(&workspace, "main"),
            main_oid_before,
            "refs/heads/main must be byte-identical after the refusal",
        );
    }
}
