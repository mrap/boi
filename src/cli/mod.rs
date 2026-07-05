//! The `boi` CLI — `clap` derive tree, the command dispatcher, and the
//! top-level error renderer.
//!
//! This is layer 5 of the Layered Domain Architecture — the outermost layer.
//! Nothing imports `cli/` (`module-dep-audit.sh` enforces it); `cli/` imports
//! every layer below it. `cli/` itself spawns NO subprocess
//! (`no-subprocess-outside-runtime.sh`) — the one interactive-shell spawn
//! `boi resolve-conflict` needs lives in `runtime::worktree`.
//!
//! ## Process model (review (a))
//!
//! `boi` is invoked as many short-lived OS processes; the orchestrator + the
//! [`EventBus`](crate::service::EventBus) live in ONE long-running
//! `boi daemon`. They cannot share an in-process bus — they communicate over a
//! Unix-domain control socket (`~/.boi/v2/daemon.sock`).
//!
//! - **Write-side** commands — `dispatch`, `cancel`, `unblock`,
//!   `resolve-conflict`, `fail` — are control-socket clients: they connect,
//!   submit a typed [`DaemonCommand`](crate::service::DaemonCommand), and the
//!   daemon's socket listener translates each into a `daemon_tx.send` so the
//!   event reaches the daemon's own orchestrator/bus and `transitions.rs`
//!   arbitrates. A write-side command with **no daemon running fails loud**
//!   with a non-zero exit (SO S6 — a DB-only flip with a live orphan worker is
//!   forbidden).
//! - **Read-only** commands — `dashboard`, `log`, `traces`, `failures`,
//!   `spec show` — read SQLite / DuckDB directly; no daemon is needed.
//! - `mcp-serve` (Goose spawns it per worker) is likewise a control-socket
//!   client — it forwards each MCP tool call as a `DaemonCommand`.
//!
//! ## Module map (Phase 9)
//!
//! - [`boot`] — the `boi daemon` boot + shutdown supervisor (Task 9.2) and the
//!   daemon-crash restart-recovery pass (Task 9.3).
//! - [`control`] — the Unix-domain control-socket client + server (Task 9.2).
//! - [`daemon`] — the `boi daemon` subcommand handler (Task 9.2).
//! - [`dispatch`] — `boi dispatch` (Task 9.4).
//! - [`log`] — the read-only phase-run log view (Task 9.5).
//! - [`recover`] — `cancel` / `unblock` / `resolve-conflict` / `fail`
//!   (Task 9.6).
//! - [`clean`] / [`spec`] — `boi clean` + `boi spec show` (Task 9.7).
//! - [`traces`] — `boi traces query` + `boi failures top` (Task 9.8).
//! - [`mcp_serve`] — `boi mcp-serve` (Task 9.9).
//! - [`dashboard`] — `boi dashboard` TUI (Task 13).
//! - [`read_error`] — the shared `ReadError` type for read-only commands.

pub mod boot;
pub mod clean;
pub mod control;
pub mod daemon;
pub mod dashboard;
pub mod dispatch;
pub mod log;
pub mod mcp_serve;
pub mod paths;
pub mod read_error;
pub mod recover;
pub mod spec;
pub mod traces;

/// `std`-only throwaway-directory helper for the `cli/` test modules.
///
/// BOI v2 deliberately does not depend on `tempfile` (see `git_ops.rs`); the
/// `cli/` tests need scratch directories for the control socket + SQLite
/// fixtures, so this mirrors `git_ops`'s pattern in one shared place.
#[cfg(test)]
pub(crate) mod testtmp {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A throwaway directory under the system temp dir, removed on drop.
    pub(crate) struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        /// Create a fresh, uniquely-named scratch directory.
        pub(crate) fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("boi-cli-{}-{tag}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }

        /// The directory path.
        pub(crate) fn path(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.path));
        }
    }
}

use clap::{Parser, Subcommand};

/// `boi` — the v2 single-binary delegation engine.
#[derive(Debug, Parser)]
#[command(
    name = "boi",
    version,
    about = "BOI v2 — orchestrate LLM-powered software-engineering tasks.",
    long_about = None,
)]
pub struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Every `boi` subcommand.
///
/// `traces` and `failures` are **always present** in the tree (review D9) —
/// only their *handlers* are `#[cfg(feature = "duckdb")]`-split, never the
/// `clap` variant. A build without the `duckdb` feature still parses
/// `boi traces query …`; it just prints a loud "built without duckdb" and
/// exits non-zero.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Manage the BOI background daemon (serve / install / start / stop /
    /// status / restart). A bare `boi daemon` prints help; the boot loop is
    /// the explicit `boi daemon serve` (which the deployed LaunchAgent plist
    /// already rides on).
    #[command(arg_required_else_help = true)]
    Daemon {
        /// The `daemon` subcommand (required — use `serve` to run the boot loop).
        #[command(subcommand)]
        command: DaemonCommand,
    },

    /// Dispatch a spec TOML file — parse, validate, persist, and start it.
    Dispatch {
        /// Path to the spec `.toml` file.
        spec: std::path::PathBuf,
    },

    /// Open the spec-observability TUI dashboard.
    Dashboard {
        /// The spec to open. Omit to open the recent-specs picker.
        spec_id: Option<String>,
    },

    /// Show the phase-run history of one spec.
    Log {
        /// The spec to show.
        spec_id: String,
    },

    /// Cancel a spec or a single task.
    Cancel {
        /// The spec or task id to cancel.
        id: String,
        /// The mandatory cancellation reason.
        #[arg(long)]
        reason: String,
    },

    /// Force a blocked task back to active.
    Unblock {
        /// The blocked task id.
        task_id: String,
        /// Also zero the task's iteration counter (extends the cap).
        #[arg(long)]
        reset_counter: bool,
    },

    /// Resolve a task's merge conflict in an interactive shell.
    ///
    /// There is intentionally **no `--ai` flag** (review (d)) — LLM-driven
    /// conflict resolution is deferred to v1.x. The flag is not offered; this
    /// is not a stub.
    ResolveConflict {
        /// The blocked task id.
        task_id: String,
    },

    /// Mark a spec failed with an operator-supplied reason (G16.6).
    Fail {
        /// The spec id to fail.
        spec_id: String,
        /// The mandatory failure reason.
        #[arg(long)]
        reason: String,
    },

    /// Delete a spec and its cascade (the `boi clean` retention command).
    Clean {
        /// The spec to clean.
        spec_id: String,
        /// Clean even a non-terminal spec (skip the terminal-state guard).
        #[arg(long)]
        force: bool,
        /// Instead of a full clean, delete only this spec's completed
        /// `phase_runs` rows older than the given duration (e.g. `90d`, `2w`).
        #[arg(long, value_name = "DURATION")]
        phase_runs_older_than: Option<String>,
    },

    /// Spec inspection.
    Spec {
        /// The spec subcommand.
        #[command(subcommand)]
        command: SpecCommand,
    },

    /// OTel-trace queries (requires the `duckdb` build feature).
    Traces {
        /// The traces subcommand.
        #[command(subcommand)]
        command: TracesCommand,
    },

    /// Recurring-failure aggregation (requires the `duckdb` build feature).
    Failures {
        /// The failures subcommand.
        #[command(subcommand)]
        command: FailuresCommand,
    },

    /// Run one stdio MCP server bound to a single worker's phase run.
    ///
    /// Goose spawns this per worker as a recipe `extensions:` entry; it is not
    /// normally invoked by hand.
    McpServe {
        /// The phase run this server is bound to.
        #[arg(long)]
        phase_run: String,
    },

    /// Generate shell completions (e.g. `source <(boi completions zsh)`).
    Completions {
        /// The shell to emit a completion script for.
        shell: clap_complete::Shell,
    },
}

/// `boi daemon <…>` subcommands. `serve` runs the long-running boot loop; the
/// other four install + manage the daemon as a per-user LaunchAgent /
/// systemd-user unit via `daemon-green` (Tq0p1hxjt). A bare `boi daemon` (no
/// subcommand) prints help rather than running anything.
#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Run the long-running BOI boot loop (invoked by launchd; hidden from --help).
    #[command(hide = true)]
    Serve,
    /// Install (if needed) and start the BOI daemon as a per-user
    /// background service.
    Start,
    /// Stop the BOI daemon background service.
    Stop,
    /// Print the daemon background service's current status.
    Status,
    /// Restart the BOI daemon background service (e.g. to pick up a new
    /// binary).
    Restart,
}

/// `boi spec <…>` subcommands.
#[derive(Debug, Subcommand)]
pub enum SpecCommand {
    /// Print a spec's stored `spec_versions` snapshot.
    Show {
        /// The spec id.
        spec_id: String,
        /// The snapshot version to print (default: the latest).
        #[arg(long)]
        version: Option<i64>,
    },
}

/// `boi traces <…>` subcommands.
#[derive(Debug, Subcommand)]
pub enum TracesCommand {
    /// Run a read-only SQL query over the OTel JSONL traces.
    Query {
        /// The SQL to run.
        sql: String,
    },
}

/// `boi failures <…>` subcommands.
#[derive(Debug, Subcommand)]
pub enum FailuresCommand {
    /// Print the top-N recurring failure fingerprints.
    Top {
        /// The look-back window (e.g. `7d`); defaults to 7 days.
        #[arg(long, value_name = "DURATION")]
        last: Option<String>,
        /// How many rows to print (default: 10).
        #[arg(long)]
        n: Option<u32>,
    },
}

/// A top-level CLI failure — rendered by [`report_error`].
///
/// A separate type from every layer-error below it: `cli::run` collapses each
/// subcommand's typed error into one `CliError` so `main.rs` stays tiny and
/// the rendering is testable (the plan's Task 9.1 — `report_error` is a
/// testable fn, not inline `eprintln!`s scattered across `main`).
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// A `boi daemon` boot/run failure.
    #[error("daemon: {0}")]
    Daemon(#[from] boot::BootError),
    /// A `boi dispatch` failure.
    #[error("dispatch: {0}")]
    Dispatch(#[from] dispatch::DispatchError),
    /// A read-only log / spec-show / dashboard failure.
    #[error("{0}")]
    Read(#[from] read_error::ReadError),
    /// A recovery-command (`cancel` / `unblock` / `resolve-conflict` /
    /// `fail`) failure.
    #[error("{0}")]
    Recover(#[from] recover::RecoverError),
    /// A `boi clean` / `boi spec show` failure.
    #[error("{0}")]
    Manage(#[from] clean::ManageError),
    /// A `boi traces` / `boi failures` failure.
    #[error("{0}")]
    Traces(#[from] traces::TracesError),
    /// A `boi mcp-serve` failure.
    #[error("mcp-serve: {0}")]
    McpServe(#[from] mcp_serve::McpServeError),
}

/// Dispatch the parsed [`Cli`] to its subcommand handler.
///
/// Returns `Ok(())` on success; every failure is one [`CliError`] variant.
/// `main.rs` renders it via [`report_error`] and exits non-zero — `cli::run`
/// itself never calls `std::process::exit`, so it stays testable.
pub async fn run(cli: Cli) -> Result<(), CliError> {
    match cli.command {
        Command::Daemon { command } => match command {
            DaemonCommand::Serve => daemon::run().await.map_err(CliError::from),
            DaemonCommand::Start => daemon::lifecycle_start().map_err(CliError::from),
            DaemonCommand::Stop => daemon::lifecycle_stop().map_err(CliError::from),
            DaemonCommand::Status => daemon::lifecycle_status().map_err(CliError::from),
            DaemonCommand::Restart => daemon::lifecycle_restart().map_err(CliError::from),
        },
        Command::Dispatch { spec } => dispatch::run(&spec).await.map_err(CliError::from),
        Command::Dashboard { spec_id } => dashboard::run(spec_id.as_deref())
            .await
            .map_err(CliError::from),
        Command::Log { spec_id } => log::run(&spec_id).await.map_err(CliError::from),
        Command::Cancel { id, reason } => {
            recover::cancel(&id, &reason).await.map_err(CliError::from)
        }
        Command::Unblock {
            task_id,
            reset_counter,
        } => recover::unblock(&task_id, reset_counter)
            .await
            .map_err(CliError::from),
        Command::ResolveConflict { task_id } => recover::resolve_conflict(&task_id)
            .await
            .map_err(CliError::from),
        Command::Fail { spec_id, reason } => recover::fail(&spec_id, &reason)
            .await
            .map_err(CliError::from),
        Command::Clean {
            spec_id,
            force,
            phase_runs_older_than,
        } => clean::run(&spec_id, force, phase_runs_older_than.as_deref())
            .await
            .map_err(CliError::from),
        Command::Spec {
            command: SpecCommand::Show { spec_id, version },
        } => spec::show(&spec_id, version).await.map_err(CliError::from),
        Command::Traces {
            command: TracesCommand::Query { sql },
        } => traces::query(&sql).await.map_err(CliError::from),
        Command::Failures {
            command: FailuresCommand::Top { last, n },
        } => traces::failures_top(last.as_deref(), n)
            .await
            .map_err(CliError::from),
        Command::McpServe { phase_run } => mcp_serve::run(&phase_run).await.map_err(CliError::from),
        Command::Completions { shell } => {
            use clap::CommandFactory;
            clap_complete::generate(shell, &mut Cli::command(), "boi", &mut std::io::stdout());
            Ok(())
        }
    }
}

/// Render a top-level [`CliError`] to a string for stderr.
///
/// A standalone testable fn — `main.rs` calls it and then exits non-zero. The
/// rendering is a single `error:`-prefixed line; the `Display` chain of each
/// wrapped layer error carries the detail.
pub fn report_error(err: &CliError) -> String {
    format!("error: {err}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// `boi --help` lists every subcommand, including `daemon` and `fail`
    /// (the Task 9.1 L2 gate).
    #[test]
    fn test_l2_help_lists_every_subcommand() {
        let mut cmd = Cli::command();
        let help = cmd.render_long_help().to_string();
        for sub in [
            "daemon",
            "dashboard",
            "dispatch",
            "log",
            "cancel",
            "unblock",
            "resolve-conflict",
            "fail",
            "clean",
            "spec",
            "traces",
            "failures",
            "mcp-serve",
        ] {
            assert!(
                help.contains(sub),
                "`boi --help` must list the `{sub}` subcommand",
            );
        }
    }

    /// `boi daemon` is a group with `serve` / `start` / `stop` / `status` /
    /// `restart` subcommands (the Tq0p1hxjt task — daemon-green lifecycle).
    /// `serve` is the default subcommand so a bare `boi daemon` still runs the
    /// long-running boot loop (backward compat with the deployed plist).
    #[test]
    fn test_l2_daemon_lifecycle_subcommands_exist() {
        let mut cmd = Cli::command();
        let help = cmd.render_long_help().to_string();
        // Locate the `daemon` subcommand and render its own help.
        let daemon = cmd
            .find_subcommand_mut("daemon")
            .expect("the `daemon` subcommand must exist");
        let daemon_help = daemon.render_long_help().to_string();
        for sub in ["serve", "start", "stop", "status", "restart"] {
            assert!(
                daemon_help.contains(sub),
                "`boi daemon --help` must list the `{sub}` subcommand; got:\n{daemon_help}",
            );
        }
        // A bare `boi daemon` (no subcommand) must NOT run anything — it
        // prints help (`arg_required_else_help`). The deployed LaunchAgent
        // plist invokes the explicit `boi daemon serve`, so nothing relies on
        // bare == serve anymore.
        let bare = Cli::try_parse_from(["boi", "daemon"]);
        assert!(
            bare.is_err(),
            "a bare `boi daemon` must print help (parse error), not silently serve",
        );
        assert_eq!(
            bare.unwrap_err().kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand,
            "bare `boi daemon` must trigger help-on-missing-subcommand",
        );
        // The subcommands must parse.
        for sub in ["serve", "start", "stop", "status", "restart"] {
            Cli::try_parse_from(["boi", "daemon", sub])
                .unwrap_or_else(|e| panic!("`boi daemon {sub}` must parse: {e}"));
        }
        // Silence the unused `help` binding from the surrounding test pattern.
        let _ = help;
    }

    /// `boi completions <shell>` emits a completion script that covers the
    /// daemon subcommand group — including `status`, a subcommand once found
    /// missing here. Guards against the completion tree silently drifting from the
    /// clap command tree.
    #[test]
    fn test_completions_cover_daemon_subcommands() {
        let mut buf: Vec<u8> = Vec::new();
        clap_complete::generate(
            clap_complete::Shell::Zsh,
            &mut Cli::command(),
            "boi",
            &mut buf,
        );
        let script = String::from_utf8(buf).expect("completion script is UTF-8");
        for needle in ["daemon", "serve", "status", "restart", "completions"] {
            assert!(
                script.contains(needle),
                "zsh completions must mention `{needle}`",
            );
        }
    }

    /// An unknown subcommand is a parse error (the CLI exits non-zero — `clap`
    /// returns an `Err` that `main` turns into a non-zero exit).
    #[test]
    fn test_l2_unknown_subcommand_is_a_parse_error() {
        let parsed = Cli::try_parse_from(["boi", "no-such-command"]);
        assert!(parsed.is_err(), "an unknown subcommand must fail to parse");
    }

    /// `boi resolve-conflict` accepts a task id and has NO `--ai` flag
    /// (review (d) — LLM resolution is v1.x).
    #[test]
    fn test_l2_resolve_conflict_has_no_ai_flag() {
        // The bare form parses.
        let ok = Cli::try_parse_from(["boi", "resolve-conflict", "T0000001a"]);
        assert!(ok.is_ok(), "resolve-conflict <task> must parse");
        // `--ai` is not a known flag.
        let with_ai = Cli::try_parse_from(["boi", "resolve-conflict", "T0000001a", "--ai"]);
        assert!(
            with_ai.is_err(),
            "resolve-conflict must NOT accept --ai (v1.x)",
        );
    }

    /// `report_error` renders an `error:`-prefixed line.
    #[test]
    fn test_l2_report_error_renders_a_prefixed_line() {
        let err = CliError::Dispatch(dispatch::DispatchError::NoDaemon);
        let rendered = report_error(&err);
        assert!(rendered.starts_with("error: "), "got {rendered:?}");
        assert!(rendered.contains("dispatch"), "got {rendered:?}");
    }
}
