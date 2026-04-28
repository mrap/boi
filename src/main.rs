use boi::cli::cancel::cmd_cancel;
use boi::cli::config_cmd::cmd_config;
use boi::cli::daemon::{cmd_daemon, cmd_stop};
use boi::cli::dispatch::cmd_dispatch;
use boi::cli::doctor::cmd_doctor;
use boi::cli::log::cmd_log;
use boi::cli::outputs::cmd_outputs;
use boi::cli::spec_mgmt::{cmd_spec, SpecActionData};
use boi::cli::status::{cmd_status, cmd_status_json, cmd_status_watch};
use boi::cli::telemetry_cmd::cmd_telemetry;
use boi::cli::workers::cmd_workers;
use boi::{config, hooks};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "boi", about = "Beginning of Infinity — self-evolving agent fleet")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum SpecMode {
    #[value(alias = "e")]
    Execute,
    #[value(alias = "c")]
    Challenge,
    #[value(alias = "d")]
    Discover,
    #[value(alias = "g")]
    Generate,
}

impl std::fmt::Display for SpecMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpecMode::Execute => write!(f, "execute"),
            SpecMode::Challenge => write!(f, "challenge"),
            SpecMode::Discover => write!(f, "discover"),
            SpecMode::Generate => write!(f, "generate"),
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Dispatch a spec to the queue
    Dispatch {
        spec_path: PathBuf,
        #[arg(long)]
        after: Option<String>,
        #[arg(long, default_value = "100")]
        priority: i64,
        /// Spec mode (execute, challenge, discover, generate) — also accepts e, c, d, g
        #[arg(long, short = 'm', value_enum)]
        mode: Option<SpecMode>,
        /// Maximum iterations (default 30)
        #[arg(long, default_value = "30")]
        max_iter: i64,
        /// Task timeout in minutes (default 30)
        #[arg(long, default_value = "30")]
        timeout: u32,
        /// Disable critic pass
        #[arg(long)]
        no_critic: bool,
        /// Project name
        #[arg(long)]
        project: Option<String>,
        /// Validate spec but don't enqueue
        #[arg(long)]
        dry_run: bool,
        /// Override workspace path for spec
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Show queue status
    Status {
        spec_id: Option<String>,
        #[arg(long)]
        all: bool,
        /// Auto-refresh every 2 seconds
        #[arg(long)]
        watch: bool,
        /// Machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// View worker output log
    Log {
        spec_id: String,
        #[arg(long)]
        full: bool,
    },
    /// Cancel a queued or running spec
    Cancel { spec_id: String },
    /// List output files for a spec
    Outputs { spec_id: String },
    /// Run the BOI daemon
    Daemon {
        #[arg(long)]
        foreground: bool,
    },
    /// Show or set config values
    Config {
        key: Option<String>,
        value: Option<String>,
    },
    /// Show worktree status for each worker slot
    Workers,
    /// Stop the daemon and all worker subprocesses
    Stop,
    /// Show per-iteration telemetry for a spec
    Telemetry {
        spec_id: String,
    },
    /// Manage tasks within a spec
    Spec {
        queue_id: String,
        #[command(subcommand)]
        action: Option<SpecAction>,
    },
    /// Health check
    Doctor,
    /// Print version
    Version,
}

#[derive(Subcommand)]
enum SpecAction {
    /// Add a task to the spec
    Add {
        title: String,
        #[arg(long)]
        spec: Option<String>,
        #[arg(long)]
        verify: Option<String>,
        #[arg(long)]
        depends: Vec<String>,
    },
    /// Skip a task
    Skip { task_id: String },
    /// Block a task on a dependency
    Block {
        task_id: String,
        #[arg(long)]
        on: String,
    },
}

fn main() {
    let cli = Cli::parse();
    let cfg = config::load();

    let db_path = cfg.db_path();
    let db_str = db_path.to_str().unwrap_or("/tmp/boi.db");

    let hook_cfg = hooks::HookConfig {
        hooks: cfg.hooks.clone(),
    };

    match cli.command {
        Commands::Dispatch {
            spec_path,
            after,
            priority,
            mode,
            max_iter,
            timeout,
            no_critic,
            project,
            dry_run,
            workspace,
        } => {
            let mode_str = mode.map(|m| m.to_string());
            cmd_dispatch(
                &spec_path,
                after.as_deref(),
                priority,
                mode_str.as_deref(),
                max_iter,
                timeout,
                no_critic,
                project.as_deref(),
                dry_run,
                workspace.as_deref(),
                db_str,
                &hook_cfg,
            );
        }
        Commands::Status {
            spec_id,
            all,
            watch,
            json,
        } => {
            if watch {
                cmd_status_watch(spec_id.as_deref(), all, db_str);
            } else if json {
                cmd_status_json(spec_id.as_deref(), all, db_str);
            } else {
                cmd_status(spec_id.as_deref(), all, db_str);
            }
        }
        Commands::Log { spec_id, full } => {
            cmd_log(&spec_id, full, &cfg);
        }
        Commands::Cancel { spec_id } => {
            cmd_cancel(&spec_id, db_str, &hook_cfg);
        }
        Commands::Outputs { spec_id } => {
            cmd_outputs(&spec_id, &cfg);
        }
        Commands::Daemon { foreground } => {
            if !foreground {
                eprintln!("[boi] note: daemon always runs in foreground (use LaunchAgent/systemd for background)");
            }
            cmd_daemon(db_str, hook_cfg, &cfg);
        }
        Commands::Config { key, value } => {
            cmd_config(key.as_deref(), value.as_deref(), &cfg);
        }
        Commands::Workers => {
            cmd_workers(db_str, &cfg);
        }
        Commands::Stop => {
            cmd_stop();
        }
        Commands::Telemetry { spec_id } => {
            cmd_telemetry(&spec_id, db_str);
        }
        Commands::Spec { queue_id, action } => {
            let action_data = match action {
                None => SpecActionData::Show,
                Some(SpecAction::Add { title, spec, verify, depends }) => {
                    SpecActionData::Add { title, spec, verify, depends }
                }
                Some(SpecAction::Skip { task_id }) => SpecActionData::Skip { task_id },
                Some(SpecAction::Block { task_id, on }) => SpecActionData::Block { task_id, on },
            };
            cmd_spec(&queue_id, action_data, db_str);
        }
        Commands::Doctor => {
            cmd_doctor(db_str, &cfg);
        }
        Commands::Version => {
            println!("boi {}", env!("CARGO_PKG_VERSION"));
        }
    }
}











