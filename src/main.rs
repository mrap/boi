use boi::cli::bench::cmd_bench;
use boi::cli::cancel::cmd_cancel;
use boi::cli::config_cmd::cmd_config;
use boi::cli::daemon::{cmd_daemon, cmd_reload, cmd_restart, cmd_start, cmd_stop};
use boi::cli::dispatch::cmd_dispatch;
use boi::cli::dispatch_many::cmd_dispatch_many;
use boi::cli::doctor::cmd_doctor;
use boi::cli::log::cmd_log;
use boi::cli::outputs::cmd_outputs;
use boi::cli::phases_cmd::{cmd_phases_list, cmd_phases_show};
use boi::cli::plan::cmd_plan;
use boi::cli::spec_mgmt::{cmd_spec, SpecActionData};
use boi::cli::status::{cmd_status, cmd_status_json, cmd_status_watch};
use boi::cli::telemetry_cmd::cmd_telemetry;
use boi::cli::why::cmd_why;
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
        /// Skip the implicit-dep DAG check (use when you know the ordering is correct)
        #[arg(long)]
        skip_plan: bool,
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
        /// Show full FailureReason detail for failed specs
        #[arg(long, short = 'v')]
        verbose: bool,
    },
    /// View worker output log
    Log {
        spec_id: String,
        #[arg(long)]
        full: bool,
        /// Show debug-level events (claude output, verify results)
        #[arg(long)]
        debug: bool,
        /// Tail the daemon log file filtered to this spec (live follow)
        #[arg(long, short = 'f')]
        follow: bool,
    },
    /// Cancel a queued or running spec
    Cancel { spec_id: String },
    /// List output files for a spec
    Outputs { spec_id: String },
    /// Manage the BOI daemon
    Daemon {
        #[command(subcommand)]
        action: Option<DaemonAction>,
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
    /// List or inspect phases
    Phases {
        /// Phase name to show details for (omit to list all)
        name: Option<String>,
    },
    /// Health check
    Doctor,
    /// Plan dispatch order + LLM critique for in-flight and/or new specs
    Plan {
        /// Additional spec files to include in DAG analysis (in-flight specs are loaded automatically)
        specs: Vec<PathBuf>,
        /// Force re-run LLM critique even if a cached result exists
        #[arg(long)]
        force_refresh: bool,
    },
    /// Dispatch multiple specs in DAG order after an LLM critique gate
    DispatchMany {
        /// Spec files to dispatch (dispatched in dependency order)
        specs: Vec<PathBuf>,
        /// Auto-approve the dispatch prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Auto-approve warn-level concerns (does not override blocks)
        #[arg(long, short = 'f')]
        force: bool,
        #[arg(long, default_value = "100")]
        priority: i64,
        #[arg(long, short = 'm', value_enum)]
        mode: Option<SpecMode>,
        /// Maximum iterations (default 30)
        #[arg(long, default_value = "30")]
        max_iter: i64,
        /// Task timeout in minutes (default 30)
        #[arg(long, default_value = "30")]
        timeout: u32,
        /// Project name
        #[arg(long)]
        project: Option<String>,
    },
    /// Print full failure detail for a spec — fast forensics when boi status shows an error
    Why {
        /// Spec ID (e.g. SA015)
        spec_id: String,
    },
    /// Print version
    Version,
    /// Benchmark N pipelines across a spec or battery of specs
    Bench {
        /// Benchmark a single phase in isolation (requires --spec; conflicts with --battery and --pipeline)
        #[arg(long, conflicts_with_all = ["battery", "pipelines"])]
        phase: Option<String>,
        /// Single spec file to benchmark
        #[arg(long, conflicts_with = "battery")]
        spec: Option<PathBuf>,
        /// Directory of .yaml spec files (battery mode)
        #[arg(long, conflicts_with = "spec")]
        battery: Option<PathBuf>,
        /// Pipeline config as "name:path/to/pipeline.toml" (repeatable)
        #[arg(long = "pipeline")]
        pipelines: Vec<String>,
        /// Number of runs per (spec, pipeline) pair
        #[arg(long, default_value = "1")]
        runs: u32,
        /// Output results as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start the daemon in the background
    Start,
    /// Stop the running daemon
    Stop,
    /// Restart the daemon (stop + start)
    Restart,
    /// Run the daemon in the foreground (default)
    Foreground,
    /// Send SIGHUP to the running daemon to reload max_workers, spawns_per_tick, and claude_bin
    Reload,
}

#[derive(Subcommand)]
enum SpecAction {
    /// Show spec as YAML reconstruction from DB
    Show,
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

    let hook_cfg = hooks::load_user_or_default();

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
            skip_plan,
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
                skip_plan,
                db_str,
                &hook_cfg,
            );
        }
        Commands::Status {
            spec_id,
            all,
            watch,
            json,
            verbose,
        } => {
            if watch {
                cmd_status_watch(spec_id.as_deref(), all, verbose, db_str);
            } else if json {
                cmd_status_json(spec_id.as_deref(), all, db_str);
            } else {
                cmd_status(spec_id.as_deref(), all, verbose, db_str);
            }
        }
        Commands::Log { spec_id, full, debug, follow } => {
            cmd_log(&spec_id, full, debug, follow, db_str, &cfg);
        }
        Commands::Cancel { spec_id } => {
            cmd_cancel(&spec_id, db_str, &hook_cfg);
        }
        Commands::Outputs { spec_id } => {
            cmd_outputs(&spec_id, &cfg);
        }
        Commands::Daemon { action } => {
            match action.unwrap_or(DaemonAction::Foreground) {
                DaemonAction::Start => cmd_start(),
                DaemonAction::Stop => cmd_stop(),
                DaemonAction::Restart => cmd_restart(),
                DaemonAction::Foreground => cmd_daemon(db_str, hook_cfg, &cfg),
                DaemonAction::Reload => cmd_reload(),
            }
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
                Some(SpecAction::Show) => SpecActionData::ShowYaml,
                Some(SpecAction::Add { title, spec, verify, depends }) => {
                    SpecActionData::Add { title, spec, verify, depends }
                }
                Some(SpecAction::Skip { task_id }) => SpecActionData::Skip { task_id },
                Some(SpecAction::Block { task_id, on }) => SpecActionData::Block { task_id, on },
            };
            cmd_spec(&queue_id, action_data, db_str);
        }
        Commands::Phases { name } => {
            match name {
                Some(n) => cmd_phases_show(&n),
                None => cmd_phases_list(),
            }
        }
        Commands::Doctor => {
            cmd_doctor(db_str, &cfg);
        }
        Commands::Plan { specs, force_refresh } => {
            let exit_code = cmd_plan(&specs, db_str, force_refresh);
            std::process::exit(exit_code);
        }
        Commands::DispatchMany {
            specs,
            yes,
            force,
            priority,
            mode,
            max_iter,
            timeout,
            project,
        } => {
            let mode_str = mode.map(|m| m.to_string());
            let exit_code = cmd_dispatch_many(
                &specs,
                yes,
                force,
                priority,
                mode_str.as_deref(),
                max_iter,
                timeout,
                project.as_deref(),
                db_str,
                &hook_cfg,
            );
            std::process::exit(exit_code);
        }
        Commands::Why { spec_id } => {
            cmd_why(&spec_id, db_str);
        }
        Commands::Version => {
            println!("boi {}", env!("CARGO_PKG_VERSION"));
        }
        Commands::Bench { phase, spec, battery, pipelines, runs, json } => {
            if let Some(phase_name) = phase {
                let spec_path = spec.unwrap_or_else(|| {
                    eprintln!("error: --phase requires --spec <file>");
                    std::process::exit(1);
                });
                boi::cli::bench::cmd_bench_phase(&phase_name, &spec_path, runs);
                return;
            }

            let spec_paths: Vec<std::path::PathBuf> = if let Some(dir) = battery {
                let mut paths: Vec<std::path::PathBuf> = match std::fs::read_dir(&dir) {
                    Ok(rd) => rd
                        .filter_map(|e| e.ok())
                        .map(|e| e.path())
                        .filter(|p| p.extension().map(|x| x == "yaml").unwrap_or(false))
                        .collect(),
                    Err(e) => {
                        eprintln!("error: cannot read battery dir: {e}");
                        std::process::exit(1);
                    }
                };
                paths.sort();
                paths
            } else if let Some(p) = spec {
                vec![p]
            } else {
                eprintln!("error: must provide --spec or --battery");
                std::process::exit(1);
            };

            let pipeline_entries: Vec<(String, std::path::PathBuf)> = pipelines
                .iter()
                .map(|s| {
                    let mut parts = s.splitn(2, ':');
                    let name = parts.next().unwrap_or("").to_string();
                    let path = parts.next().unwrap_or("");
                    if name.is_empty() || path.is_empty() {
                        eprintln!("error: --pipeline must be name:path, got: {s}");
                        std::process::exit(1);
                    }
                    (name, std::path::PathBuf::from(path))
                })
                .collect();

            if pipeline_entries.is_empty() {
                eprintln!("error: at least one --pipeline name:path required");
                std::process::exit(1);
            }

            cmd_bench(&spec_paths, &pipeline_entries, runs, db_str, json);
        }
    }
}











