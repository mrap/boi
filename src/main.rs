use boi::cli::bench::cmd_bench;
use boi::cli::cancel::cmd_cancel;
use boi::cli::dashboard::run_dashboard;
use boi::cli::config_cmd::cmd_config;
use boi::cli::daemon::{cmd_daemon, cmd_restart, cmd_start, cmd_stop};
use boi::cli::dispatch::cmd_dispatch;
use boi::cli::doctor::cmd_doctor;
use boi::cli::log::cmd_log;
use boi::cli::outputs::cmd_outputs;
use boi::cli::phases_cmd::{cmd_phase_runs, cmd_phases_list, cmd_phases_show};
use boi::cli::prune::{cmd_prune_orphans, PruneConfig};
use boi::cli::providers::cmd_providers_list;
use boi::cli::research::cmd_research;
use boi::cli::spec_mgmt::{cmd_spec, SpecActionData};
use boi::cli::status::{cmd_status, cmd_status_json, cmd_status_watch};
use boi::cli::telemetry_cmd::cmd_telemetry;
use boi::cli::workers::cmd_workers;
use boi::{config, hooks};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{generate, Shell};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "boi", about = "Beginning of Infinity — self-evolving agent fleet", version = env!("CARGO_PKG_VERSION"))]
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
        /// Verbose: show runtime + model for running phase
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
    /// List or inspect phases, or show phase invocations for a spec
    Phases {
        /// Phase name to show details for (omit to list all)
        name: Option<String>,
        /// Show all phase invocations for the given spec_id
        #[arg(long)]
        spec: Option<String>,
        /// Show all fields in phase invocation table (requires --spec)
        #[arg(long)]
        full: bool,
    },
    /// Manage and inspect runtime providers
    Providers {
        #[command(subcommand)]
        action: Option<ProvidersAction>,
    },
    /// Health check
    Doctor,
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
    /// Launch interactive TUI dashboard
    Dashboard,
    /// Generate shell completion script
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },
    /// Identify and optionally kill orphaned worker processes
    PruneOrphans {
        /// Show candidates without killing (default)
        #[arg(long, conflicts_with = "apply")]
        dry_run: bool,
        /// Kill the identified orphan processes
        #[arg(long, conflicts_with = "dry_run")]
        apply: bool,
        /// Skip confirmation prompt (required with --apply in non-TTY)
        #[arg(long)]
        yes: bool,
        /// Override the empty-protected-set safety check
        #[arg(long)]
        force: bool,
        /// Minimum idle seconds before considering a process orphaned (default 600)
        #[arg(long, default_value = "600")]
        max_idle_secs: u64,
        /// Exclude processes whose cmdline contains this string (repeatable)
        #[arg(long = "exclude-pattern")]
        exclude_patterns: Vec<String>,
        /// Machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Dispatch a multi-angle research DAG from a brief
    Research {
        /// Path to the research brief (markdown with Question/Angles/Deliverable sections)
        brief: PathBuf,
        /// Number of parallel research angles (default 3)
        #[arg(long, default_value = "3")]
        threads: usize,
        /// Project name to tag all dispatched specs with
        #[arg(long)]
        project: Option<String>,
    },
}

#[derive(Subcommand)]
enum ProvidersAction {
    /// List registered and disabled providers
    List,
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start the daemon in the background
    Start,
    /// Stop the running daemon
    Stop {
        /// Cancel all running specs before stopping (DESTRUCTIVE — destroys all in-flight work)
        #[arg(long)]
        destroy_running: bool,
        /// Skip confirmation prompt (required in non-TTY environments)
        #[arg(long)]
        yes: bool,
    },
    /// Restart the daemon (stop + start)
    Restart {
        /// Cancel all running specs before restarting (DESTRUCTIVE — destroys all in-flight work)
        #[arg(long)]
        destroy_running: bool,
        /// Skip confirmation prompt (required in non-TTY environments)
        #[arg(long)]
        yes: bool,
    },
    /// Run the daemon in the foreground (default)
    Foreground,
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
    /// Tail worker stdout for a task (use --follow for live streaming)
    Tail {
        task_id: String,
        /// Follow output as it is written
        #[arg(long, short = 'f')]
        follow: bool,
        /// Start at this byte offset (resume point)
        #[arg(long, default_value = "0")]
        since_bytes: u64,
        /// Cap the number of bytes returned (0 = unlimited)
        #[arg(long, default_value = "0")]
        max_bytes: u64,
        /// Print the final byte offset to stderr after streaming
        #[arg(long)]
        print_offset: bool,
    },
}

fn main() {
    // Auto-load ~/.boi/.env before any env vars are consumed (dotenvy::from_path; existing env wins).
    boi::load_boi_env();

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
                DaemonAction::Stop { destroy_running, yes } => cmd_stop(destroy_running, yes),
                DaemonAction::Restart { destroy_running, yes } => cmd_restart(destroy_running, yes),
                DaemonAction::Foreground => cmd_daemon(db_str, hook_cfg, &cfg),
            }
        }
        Commands::Config { key, value } => {
            cmd_config(key.as_deref(), value.as_deref(), &cfg);
        }
        Commands::Workers => {
            cmd_workers(db_str, &cfg);
        }
        Commands::Stop => {
            cmd_stop(false, false);
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
                Some(SpecAction::Tail { task_id, follow, since_bytes, max_bytes, print_offset }) => {
                    SpecActionData::Tail { task_id, follow, since_bytes, max_bytes, print_offset }
                }
            };
            cmd_spec(&queue_id, action_data, db_str);
        }
        Commands::Phases { name, spec, full } => {
            if let Some(sid) = spec {
                cmd_phase_runs(&sid, full, db_str);
            } else {
                match name {
                    Some(n) => cmd_phases_show(&n),
                    None => cmd_phases_list(),
                }
            }
        }
        Commands::Providers { action } => {
            match action.unwrap_or(ProvidersAction::List) {
                ProvidersAction::List => cmd_providers_list(),
            }
        }
        Commands::Doctor => {
            cmd_doctor(db_str, &cfg);
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
        Commands::Dashboard => {
            run_dashboard(db_str);
        }
        Commands::Completions { shell } => {
            generate(shell, &mut Cli::command(), "boi", &mut std::io::stdout());
        }
        Commands::PruneOrphans {
            dry_run,
            apply,
            yes,
            force,
            max_idle_secs,
            exclude_patterns,
            json,
        } => {
            let prune_cfg = PruneConfig {
                dry_run: dry_run || !apply,
                apply,
                yes,
                force,
                max_idle_secs,
                exclude_patterns,
                json,
            };
            cmd_prune_orphans(&prune_cfg, db_str);
        }
        Commands::Research { brief, threads, project } => {
            cmd_research(&brief, threads, project.as_deref(), db_str, &hook_cfg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap_complete::Shell;

    #[test]
    fn completions_zsh_contains_boi_function() {
        let mut buf = Vec::new();
        generate(Shell::Zsh, &mut Cli::command(), "boi", &mut buf);
        let output = String::from_utf8(buf).unwrap();
        assert!(!output.is_empty(), "zsh completion output should not be empty");
        assert!(output.contains("_boi"), "zsh completion should contain '_boi'");
    }
}











