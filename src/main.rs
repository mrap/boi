use boi::{config, hooks, queue, spec, worker, worktree};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;
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
            cmd_dispatch(
                &spec_path,
                after.as_deref(),
                priority,
                mode,
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
            cmd_spec(&queue_id, action, db_str);
        }
        Commands::Doctor => {
            cmd_doctor(db_str, &cfg);
        }
        Commands::Version => {
            println!("boi {}", env!("CARGO_PKG_VERSION"));
        }
    }
}

// ─── dispatch ────────────────────────────────────────────────────────────────

fn cmd_dispatch(
    spec_path: &PathBuf,
    after: Option<&str>,
    priority: i64,
    mode: Option<SpecMode>,
    max_iter: i64,
    timeout: u32,
    _no_critic: bool,
    project: Option<&str>,
    dry_run: bool,
    _workspace: Option<&str>,
    db_str: &str,
    hook_cfg: &hooks::HookConfig,
) {
    let content = match std::fs::read_to_string(spec_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot read {:?}: {}", spec_path, e);
            std::process::exit(1);
        }
    };

    let boi_spec = match spec::parse(&content) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: spec validation failed: {}", e);
            std::process::exit(1);
        }
    };

    if dry_run {
        println!("spec valid: {} ({} tasks)", boi_spec.title, boi_spec.tasks.len());
        return;
    }

    ensure_db_dir(db_str);
    let q = match queue::Queue::open(db_str) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("error: cannot open queue: {}", e);
            std::process::exit(1);
        }
    };

    let spec_path_str = spec_path.to_str().unwrap_or("");
    let spec_id = match q.enqueue(&boi_spec, Some(spec_path_str)) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("error: enqueue failed: {}", e);
            std::process::exit(1);
        }
    };

    // Apply CLI overrides via single queue connection
    let mode_str = mode.map(|m| m.to_string());
    let timeout_secs = if timeout != 30 {
        Some(timeout as i64 * 60)
    } else {
        None
    };
    let _ = q.set_spec_fields(
        &spec_id,
        mode_str.as_deref(),
        if max_iter != 30 { Some(max_iter) } else { None },
        project,
        timeout_secs,
    );

    if priority != 100 {
        let _ = q.set_priority(&spec_id, priority);
    }

    if let Some(dep) = after {
        let _ = q.set_depends_on(&spec_id, dep);
    }

    let payload = json!({
        "spec_id": spec_id,
        "title": boi_spec.title,
        "spec_path": spec_path_str,
    });
    let _ = hooks::fire(hook_cfg, hooks::ON_DISPATCH, &payload);

    println!("{}", spec_id);
}

// ─── ANSI helpers ────────────────────────────────────────────────────────────

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";

fn progress_bar(completed: i64, total: i64, width: usize) -> String {
    if total == 0 {
        return format!("{}░{}", DIM, RESET).repeat(width);
    }
    let pct = (completed as f64 / total as f64).min(1.0);
    let filled = (pct * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);
    format!(
        "{}{}{}{}{}{}",
        GREEN,
        "█".repeat(filled),
        DIM,
        "░".repeat(empty),
        RESET,
        ""
    )
}

fn time_ago(ts: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        let now = chrono::Utc::now();
        let dur = now.signed_duration_since(dt.with_timezone(&chrono::Utc));
        let secs = dur.num_seconds();
        if secs < 60 {
            format!("{}s", secs)
        } else if secs < 3600 {
            format!("{}m ago", secs / 60)
        } else if secs < 86400 {
            format!("{}h ago", secs / 3600)
        } else {
            format!("{}d ago", secs / 86400)
        }
    } else {
        ts.to_string()
    }
}

fn elapsed_since(ts: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        let now = chrono::Utc::now();
        let dur = now.signed_duration_since(dt.with_timezone(&chrono::Utc));
        let secs = dur.num_seconds();
        if secs < 60 {
            format!("{}s", secs)
        } else if secs < 3600 {
            format!("{}m", secs / 60)
        } else {
            format!("{}h", secs / 3600)
        }
    } else {
        "?".to_string()
    }
}

// ─── rich status ─────────────────────────────────────────────────────────────

fn render_status(spec_id: Option<&str>, all: bool, db_str: &str) -> String {
    ensure_db_dir(db_str);
    let q = match queue::Queue::open(db_str) {
        Ok(q) => q,
        Err(e) => return format!("error: cannot open queue: {}", e),
    };

    if let Some(id) = spec_id {
        return render_single_spec(&q, id);
    }

    let specs = match q.status_all() {
        Ok(s) => s,
        Err(e) => return format!("error: {}", e),
    };

    if specs.is_empty() {
        return "queue is empty".to_string();
    }

    let mut out = String::new();

    // Partition by status
    let running: Vec<&queue::SpecRecord> = specs.iter().filter(|s| s.status == "running").collect();
    let queued: Vec<&queue::SpecRecord> = specs.iter().filter(|s| s.status == "queued").collect();

    let six_hours_ago = chrono::Utc::now() - chrono::Duration::hours(6);
    let finished: Vec<&queue::SpecRecord> = specs
        .iter()
        .filter(|s| {
            (s.status == "completed" || s.status == "failed" || s.status == "cancelled")
                && (all
                    || s.completed_at.as_ref().map_or(false, |ts| {
                        chrono::DateTime::parse_from_rfc3339(ts)
                            .map(|dt| dt.with_timezone(&chrono::Utc) > six_hours_ago)
                            .unwrap_or(false)
                    }))
        })
        .collect();

    // Running section
    if !running.is_empty() {
        out.push_str(&format!("{}{}RUNNING{}\n", BOLD, YELLOW, RESET));
        for s in &running {
            let total = s.total_tasks.unwrap_or(0);
            let outcomes = if let Ok(Some(st)) = q.status(&s.id) {
                st.tasks
                    .iter()
                    .filter(|t| t.status == "DONE")
                    .count()
            } else {
                0
            };
            let elapsed = s
                .started_at
                .as_deref()
                .map(elapsed_since)
                .unwrap_or_else(|| "?".to_string());

            out.push_str(&format!(
                "{}▸ {:8}{}  {:<40}  {}/{} · {} outcomes  {}\n",
                YELLOW,
                s.id,
                RESET,
                truncate(&s.title, 40),
                s.completed_tasks,
                total,
                outcomes,
                elapsed,
            ));

            // Show current task
            if let Ok(Some(st)) = q.status(&s.id) {
                if let Some(running_task) = st.tasks.iter().find(|t| t.status == "RUNNING") {
                    out.push_str(&format!(
                        "         {}→ {}: {}{}\n",
                        DIM, running_task.id, running_task.title, RESET
                    ));
                }
            }

            // Progress bar
            let pct = if total > 0 {
                (s.completed_tasks as f64 / total as f64 * 100.0) as u32
            } else {
                0
            };
            out.push_str(&format!(
                "         {}  {}%\n",
                progress_bar(s.completed_tasks, total, 25),
                pct
            ));
        }
        out.push('\n');
    }

    // Queued section
    if !queued.is_empty() {
        out.push_str(&format!("{}{}QUEUED{}\n", BOLD, CYAN, RESET));
        for s in &queued {
            let total = s.total_tasks.unwrap_or(0);
            let dep_str = s
                .depends_on
                .as_deref()
                .map(|d| format!(" {}(after {}){}", DIM, d, RESET))
                .unwrap_or_default();
            out.push_str(&format!(
                "{}◦ {:8}{}  {:<40}  0/{}{}\n",
                CYAN,
                s.id,
                RESET,
                truncate(&s.title, 40),
                total,
                dep_str,
            ));
        }
        out.push('\n');
    }

    // Recently finished section
    if !finished.is_empty() {
        out.push_str(&format!("{}RECENTLY FINISHED{}\n", BOLD, RESET));
        for s in &finished {
            let total = s.total_tasks.unwrap_or(0);
            let ago = s
                .completed_at
                .as_deref()
                .map(time_ago)
                .unwrap_or_else(|| "?".to_string());

            let (icon, color) = match s.status.as_str() {
                "completed" => ("✓", GREEN),
                "failed" => ("✗", RED),
                "cancelled" => ("⊘", DIM),
                _ => ("?", RESET),
            };

            out.push_str(&format!(
                "{}{} {:8}{}  {:<40}  {}/{}  {}\n",
                color,
                icon,
                s.id,
                RESET,
                truncate(&s.title, 40),
                s.completed_tasks,
                total,
                ago,
            ));
        }
        out.push('\n');
    }

    // Summary line
    let busy = running.len();
    let failed_count = specs.iter().filter(|s| s.status == "failed").count();
    let completed_count = specs.iter().filter(|s| s.status == "completed").count();
    let total_shown = running.len() + queued.len() + finished.len();

    out.push_str(&format!(
        "{}{}/{} busy{} | {}{}✗{}  {}{}✓{}\n",
        BOLD,
        busy,
        specs.iter().filter(|s| s.status != "cancelled").count(),
        RESET,
        RED,
        failed_count,
        RESET,
        GREEN,
        completed_count,
        RESET,
    ));

    if !all {
        out.push_str(&format!(
            "{}Showing {} of {} specs (running + last 6h). Use --all for full history.{}\n",
            DIM,
            total_shown,
            specs.len(),
            RESET,
        ));
    }

    // Daemon heartbeat detection
    if let Ok(last_update) = q.last_spec_update() {
        if let Some(ts) = last_update {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&ts) {
                let age = chrono::Utc::now()
                    .signed_duration_since(dt.with_timezone(&chrono::Utc))
                    .num_seconds();
                if age > 3600 {
                    out.push_str(&format!(
                        "\n{}{}⚠  Daemon may be stuck — no spec updated in {}{}{}",
                        BOLD, YELLOW, time_ago(&ts), RESET, "\n"
                    ));
                }
            }
        }
    }

    // Also check pidfile-based heartbeat
    let heartbeat_path = daemon_heartbeat_path();
    if heartbeat_path.exists() {
        if let Ok(ts_str) = std::fs::read_to_string(&heartbeat_path) {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts_str.trim()) {
                let age = chrono::Utc::now()
                    .signed_duration_since(dt.with_timezone(&chrono::Utc))
                    .num_seconds();
                if age > 3600 {
                    out.push_str(&format!(
                        "{}{}⚠  Daemon heartbeat stale ({} old){}\n",
                        BOLD,
                        YELLOW,
                        time_ago(ts_str.trim()),
                        RESET,
                    ));
                }
            }
        }
    } else if !running.is_empty() {
        out.push_str(&format!(
            "{}{}⚠  No daemon heartbeat file found — daemon may not be running{}\n",
            BOLD, YELLOW, RESET,
        ));
    }

    out
}

fn render_single_spec(q: &queue::Queue, id: &str) -> String {
    match q.status(id) {
        Ok(Some(st)) => {
            let mut out = String::new();
            let total = st.spec.total_tasks.unwrap_or(0);
            let (icon, color) = match st.spec.status.as_str() {
                "running" => ("▸", YELLOW),
                "completed" => ("✓", GREEN),
                "failed" => ("✗", RED),
                "queued" => ("◦", CYAN),
                _ => ("?", RESET),
            };

            out.push_str(&format!(
                "{}{}{} {}  {}  [{}/{}]\n",
                color, BOLD, icon, st.spec.id, st.spec.title, st.spec.completed_tasks, total
            ));
            out.push_str(&format!(
                "{}mode: {}  status: {}  iteration: {}/{}{}\n",
                RESET, st.spec.mode, st.spec.status, st.spec.iteration, st.spec.max_iterations, RESET
            ));
            if let Some(ref p) = st.spec.project {
                out.push_str(&format!("project: {}\n", p));
            }
            out.push('\n');

            for task in &st.tasks {
                let (ticon, tcolor) = match task.status.as_str() {
                    "DONE" => ("✓", GREEN),
                    "FAILED" => ("✗", RED),
                    "RUNNING" => ("▸", YELLOW),
                    "SKIPPED" => ("⊘", DIM),
                    _ => ("○", RESET),
                };
                out.push_str(&format!(
                    "  {}{} {:8}{} {} — {}{}\n",
                    tcolor, ticon, task.id, RESET, task.title,
                    if !task.depends.is_empty() && task.depends != "[]" {
                        format!(" {}(depends: {}){}", DIM, task.depends, RESET)
                    } else {
                        String::new()
                    },
                    ""
                ));
            }

            out
        }
        Ok(None) => format!("error: spec '{}' not found", id),
        Err(e) => format!("error: {}", e),
    }
}

fn cmd_status(spec_id: Option<&str>, all: bool, db_str: &str) {
    print!("{}", render_status(spec_id, all, db_str));
}

fn cmd_status_watch(spec_id: Option<&str>, all: bool, db_str: &str) {
    loop {
        // Clear screen
        print!("\x1b[2J\x1b[H");
        print!("{}", render_status(spec_id, all, db_str));
        let now = chrono::Utc::now().format("%H:%M:%S");
        println!("\n{}Updated at {} — Ctrl+C to exit{}", DIM, now, RESET);
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

fn cmd_status_json(spec_id: Option<&str>, all: bool, db_str: &str) {
    ensure_db_dir(db_str);
    let q = match queue::Queue::open(db_str) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("error: cannot open queue: {}", e);
            std::process::exit(1);
        }
    };

    if let Some(id) = spec_id {
        match q.status(id) {
            Ok(Some(st)) => {
                let tasks: Vec<serde_json::Value> = st
                    .tasks
                    .iter()
                    .map(|t| {
                        json!({
                            "id": t.id,
                            "title": t.title,
                            "status": t.status,
                            "depends": t.depends,
                        })
                    })
                    .collect();
                let out = json!({
                    "id": st.spec.id,
                    "title": st.spec.title,
                    "mode": st.spec.mode,
                    "status": st.spec.status,
                    "completed_tasks": st.spec.completed_tasks,
                    "total_tasks": st.spec.total_tasks,
                    "iteration": st.spec.iteration,
                    "max_iterations": st.spec.max_iterations,
                    "project": st.spec.project,
                    "tasks": tasks,
                });
                println!("{}", serde_json::to_string_pretty(&out).unwrap());
            }
            Ok(None) => {
                eprintln!("error: spec '{}' not found", id);
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
        }
        return;
    }

    let specs = match q.status_all() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };

    let six_hours_ago = chrono::Utc::now() - chrono::Duration::hours(6);
    let filtered: Vec<&queue::SpecRecord> = if all {
        specs.iter().collect()
    } else {
        specs
            .iter()
            .filter(|s| {
                s.status == "running"
                    || s.status == "queued"
                    || s.completed_at.as_ref().map_or(false, |ts| {
                        chrono::DateTime::parse_from_rfc3339(ts)
                            .map(|dt| dt.with_timezone(&chrono::Utc) > six_hours_ago)
                            .unwrap_or(false)
                    })
            })
            .collect()
    };

    let items: Vec<serde_json::Value> = filtered
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "title": s.title,
                "mode": s.mode,
                "status": s.status,
                "completed_tasks": s.completed_tasks,
                "total_tasks": s.total_tasks,
                "iteration": s.iteration,
                "max_iterations": s.max_iterations,
                "project": s.project,
                "queued_at": s.queued_at,
                "started_at": s.started_at,
                "completed_at": s.completed_at,
            })
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&json!({ "specs": items })).unwrap());
}

// ─── log ─────────────────────────────────────────────────────────────────────

fn cmd_log(spec_id: &str, full: bool, cfg: &config::Config) {
    let logs_dir = cfg.logs_dir();
    let spec_log_dir = logs_dir.join(spec_id);

    if !spec_log_dir.exists() {
        println!("no logs found for {}", spec_id);
        return;
    }

    let mut entries: Vec<_> = match std::fs::read_dir(&spec_log_dir) {
        Ok(e) => e.filter_map(|e| e.ok()).collect(),
        Err(e) => {
            eprintln!("error reading log dir: {}", e);
            std::process::exit(1);
        }
    };

    entries.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());

    let log_file = if let Some(last) = entries.last() {
        last.path()
    } else {
        println!("no log files found for {}", spec_id);
        return;
    };

    let content = match std::fs::read_to_string(&log_file) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error reading {}: {}", log_file.display(), e);
            std::process::exit(1);
        }
    };

    if full {
        print!("{}", content);
    } else {
        let lines: Vec<&str> = content.lines().collect();
        let start = if lines.len() > 50 { lines.len() - 50 } else { 0 };
        for line in &lines[start..] {
            println!("{}", line);
        }
    }
}

// ─── cancel ──────────────────────────────────────────────────────────────────

fn cmd_cancel(spec_id: &str, db_str: &str, hook_cfg: &hooks::HookConfig) {
    ensure_db_dir(db_str);
    let q = match queue::Queue::open(db_str) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("error: cannot open queue: {}", e);
            std::process::exit(1);
        }
    };

    match q.status(spec_id) {
        Ok(None) => {
            eprintln!("error: spec '{}' not found", spec_id);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
        Ok(Some(_)) => {}
    }

    if let Err(e) = q.cancel(spec_id) {
        eprintln!("error: cancel failed: {}", e);
        std::process::exit(1);
    }

    let _ = worktree::cleanup(spec_id);

    let payload = json!({ "spec_id": spec_id });
    let _ = hooks::fire(hook_cfg, hooks::ON_CANCEL, &payload);

    println!("cancelled {}", spec_id);
}

// ─── outputs ─────────────────────────────────────────────────────────────────

fn cmd_outputs(spec_id: &str, cfg: &config::Config) {
    let worktrees_dir = cfg.worktrees_dir();
    let spec_wt = worktrees_dir.join(spec_id);
    let logs_dir = cfg.logs_dir().join(spec_id);

    let mut found = false;

    if spec_wt.exists() {
        println!("worktree: {}", spec_wt.display());
        found = true;
    }

    if logs_dir.exists() {
        println!("logs: {}", logs_dir.display());
        if let Ok(entries) = std::fs::read_dir(&logs_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                println!("  {}", entry.path().display());
            }
        }
        found = true;
    }

    if !found {
        println!("no outputs found for {}", spec_id);
    }
}

// ─── daemon ──────────────────────────────────────────────────────────────────

fn daemon_pid_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".boi").join("daemon.pid")
}

fn daemon_heartbeat_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".boi").join("daemon.heartbeat")
}

fn check_existing_daemon(pid_path: &std::path::Path) -> bool {
    if let Ok(content) = std::fs::read_to_string(pid_path) {
        if let Ok(pid) = content.trim().parse::<i32>() {
            unsafe { libc::kill(pid, 0) == 0 }
        } else {
            false
        }
    } else {
        false
    }
}

fn cmd_daemon(db_str: &str, hook_cfg: hooks::HookConfig, cfg: &config::Config) {
    ensure_db_dir(db_str);

    // Singleton guard: check if a daemon is already running
    let pid_path = daemon_pid_path();
    if check_existing_daemon(&pid_path) {
        if let Ok(content) = std::fs::read_to_string(&pid_path) {
            eprintln!(
                "[boi daemon] error: daemon already running (pid {})",
                content.trim()
            );
        } else {
            eprintln!("[boi daemon] error: daemon already running");
        }
        std::process::exit(1);
    }

    // Write PID file
    let pid = std::process::id();
    if let Some(parent) = pid_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&pid_path, pid.to_string()).unwrap_or_else(|e| {
        eprintln!("warning: could not write PID file: {}", e);
    });

    // Register SIGTERM handler via atomic flag
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let r = running.clone();

    // Install SIGTERM + SIGINT handler
    unsafe {
        let r2 = running.clone();
        signal_hook::low_level::register(signal_hook::consts::SIGTERM, move || {
            r2.store(false, std::sync::atomic::Ordering::SeqCst);
        })
        .ok();

        let r3 = r.clone();
        signal_hook::low_level::register(signal_hook::consts::SIGINT, move || {
            r3.store(false, std::sync::atomic::Ordering::SeqCst);
        })
        .ok();
    }

    let wc = worker::WorkerConfig {
        max_workers: cfg.max_workers(),
        task_timeout_secs: cfg.task_timeout_secs(),
        retry_count: cfg.retry_count(),
    };

    // Crash recovery: reset any specs stuck in 'running' or 'assigning' back to 'queued'
    if let Ok(q) = queue::Queue::open(db_str) {
        match q.recover_stuck_specs() {
            Ok(0) => {}
            Ok(n) => eprintln!("[boi daemon] recovered {} stuck spec(s) back to queued", n),
            Err(e) => eprintln!("[boi daemon] warning: crash recovery failed: {}", e),
        }
    }

    eprintln!("[boi daemon] started (pid {}), max_workers={}", pid, wc.max_workers);

    let active: std::sync::Arc<std::sync::Mutex<Vec<std::thread::JoinHandle<()>>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    while running.load(std::sync::atomic::Ordering::SeqCst) {
        // Write heartbeat
        let heartbeat_path = daemon_heartbeat_path();
        let _ = std::fs::write(&heartbeat_path, chrono::Utc::now().to_rfc3339());

        {
            let mut workers = active.lock().unwrap();
            workers.retain(|h| !h.is_finished());

            if workers.len() < wc.max_workers as usize {
                match queue::Queue::open(db_str) {
                    Ok(queue) => match queue.dequeue() {
                        Ok(Some(rec)) => {
                            let spec_id = rec.id.clone();
                            let spec_path = match rec.spec_path.as_deref() {
                                Some(p) if !p.is_empty() => p.to_string(),
                                _ => {
                                    eprintln!(
                                        "[boi daemon] spec {} has no spec_path — marking failed",
                                        spec_id
                                    );
                                    if let Ok(q2) = queue::Queue::open(db_str) {
                                        let _ = q2.update_spec(&spec_id, "failed");
                                    }
                                    continue;
                                }
                            };
                            let qpath = db_str.to_string();
                            let hc = hook_cfg.clone();
                            let timeout = wc.task_timeout_secs;
                            let retries = wc.retry_count;

                            // Use per-spec timeout if set, otherwise default
                            let spec_timeout = rec
                                .worker_timeout_seconds
                                .map(|t| t as u64)
                                .unwrap_or(timeout);

                            eprintln!("[boi daemon] starting worker for {}", spec_id);
                            let handle = std::thread::spawn(move || {
                                let wc = worker::WorkerConfig {
                                    max_workers: 1,
                                    task_timeout_secs: spec_timeout,
                                    retry_count: retries,
                                };
                                if let Err(e) =
                                    worker::run_worker(&spec_id, &spec_path, &qpath, &hc, &wc)
                                {
                                    eprintln!("[boi daemon] worker error for {}: {}", spec_id, e);
                                }
                            });
                            workers.push(handle);
                        }
                        Ok(None) => {}
                        Err(e) => eprintln!("[boi daemon] dequeue error: {}", e),
                    },
                    Err(e) => eprintln!("[boi daemon] queue open error: {}", e),
                }
            }
        }

        // Sleep in small increments so SIGTERM is responsive
        for _ in 0..10 {
            if !running.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    eprintln!("[boi daemon] shutting down...");

    // Wait for workers to finish (with timeout)
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    {
        let mut workers = active.lock().unwrap();
        while !workers.is_empty() && std::time::Instant::now() < deadline {
            workers.retain(|h| !h.is_finished());
            if !workers.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }

    // Clean up pidfile and heartbeat
    let _ = std::fs::remove_file(&pid_path);
    let _ = std::fs::remove_file(&daemon_heartbeat_path());

    eprintln!("[boi daemon] stopped");
}

// ─── config ──────────────────────────────────────────────────────────────────

fn cmd_config(key: Option<&str>, value: Option<&str>, cfg: &config::Config) {
    match (key, value) {
        (None, _) => {
            println!("max_workers:          {}", cfg.max_workers());
            println!("task_timeout_minutes: {}", cfg.task_timeout_secs() / 60);
            println!("retry_count:          {}", cfg.retry_count());
            println!("db_path:              {}", cfg.db_path().display());
            println!("telemetry_path:       {}", cfg.telemetry_path().display());
            println!("worktrees_dir:        {}", cfg.worktrees_dir().display());
            println!("logs_dir:             {}", cfg.logs_dir().display());
            let config_path = config::default_config_path();
            println!("config_file:          {}", config_path.display());
            if cfg.hooks.is_some() {
                let hook_names: Vec<&str> = cfg
                    .hooks
                    .as_ref()
                    .unwrap()
                    .keys()
                    .map(|k| k.as_str())
                    .collect();
                println!("hooks:                {}", hook_names.join(", "));
            } else {
                println!("hooks:                (none configured)");
            }
        }
        (Some(k), None) => {
            let val = match k {
                "max_workers" => cfg.max_workers().to_string(),
                "task_timeout_minutes" => (cfg.task_timeout_secs() / 60).to_string(),
                "retry_count" => cfg.retry_count().to_string(),
                "db_path" => cfg.db_path().display().to_string(),
                "telemetry_path" => cfg.telemetry_path().display().to_string(),
                "worktrees_dir" => cfg.worktrees_dir().display().to_string(),
                "logs_dir" => cfg.logs_dir().display().to_string(),
                _ => {
                    eprintln!("unknown config key: {}", k);
                    std::process::exit(1);
                }
            };
            println!("{}", val);
        }
        (Some(_k), Some(_v)) => {
            eprintln!(
                "note: config set is not yet persisted — edit {} directly",
                config::default_config_path().display()
            );
            std::process::exit(1);
        }
    }
}

// ─── workers ─────────────────────────────────────────────────────────────────

fn cmd_workers(db_str: &str, cfg: &config::Config) {
    let worktrees_dir = cfg.worktrees_dir();

    // Try to get worker records from DB
    ensure_db_dir(db_str);
    if let Ok(q) = queue::Queue::open(db_str) {
        if let Ok(workers) = q.get_workers() {
            if !workers.is_empty() {
                println!("{}{}WORKERS{}", BOLD, CYAN, RESET);
                for w in &workers {
                    let status = if let Some(ref sid) = w.current_spec_id {
                        let pid_alive = w
                            .current_pid
                            .map(|p| is_pid_alive(p as u32))
                            .unwrap_or(false);
                        if pid_alive {
                            format!(
                                "{}active{} — {} (pid {})",
                                GREEN,
                                RESET,
                                sid,
                                w.current_pid.unwrap_or(0)
                            )
                        } else {
                            format!("{}stale{} — {} (process gone)", YELLOW, RESET, sid)
                        }
                    } else {
                        format!("{}idle{}", DIM, RESET)
                    };
                    let wt = w
                        .worktree_path
                        .as_deref()
                        .unwrap_or("(no worktree)");
                    println!("  {:12}  {}  {}{}{}", w.id, status, DIM, wt, RESET);
                }
                return;
            }
        }
    }

    // Fallback: scan worktrees directory
    if !worktrees_dir.exists() {
        println!("no worktrees directory found at {}", worktrees_dir.display());
        return;
    }

    let mut entries: Vec<_> = match std::fs::read_dir(&worktrees_dir) {
        Ok(e) => e.filter_map(|e| e.ok()).collect(),
        Err(e) => {
            eprintln!("error reading worktrees dir: {}", e);
            return;
        }
    };
    entries.sort_by_key(|e| e.file_name());

    if entries.is_empty() {
        println!("no active worktrees");
        return;
    }

    println!("{}{}WORKTREES{}", BOLD, CYAN, RESET);
    for entry in &entries {
        let name = entry.file_name();
        let path = entry.path();
        let has_git = path.join(".git").exists();
        let status = if has_git {
            format!("{}active{}", GREEN, RESET)
        } else {
            format!("{}stale{}", RED, RESET)
        };
        println!(
            "  {:<20}  {}  {}",
            name.to_string_lossy(),
            status,
            path.display()
        );
    }
}

// ─── stop ────────────────────────────────────────────────────────────────────

fn cmd_stop() {
    let pid_path = daemon_pid_path();

    if !pid_path.exists() {
        eprintln!("no daemon PID file found at {}", pid_path.display());
        std::process::exit(1);
    }

    let pid_str = match std::fs::read_to_string(&pid_path) {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            eprintln!("error reading PID file: {}", e);
            std::process::exit(1);
        }
    };

    let pid: u32 = match pid_str.parse() {
        Ok(p) => p,
        Err(_) => {
            eprintln!("invalid PID in file: {}", pid_str);
            std::process::exit(1);
        }
    };

    if !is_pid_alive(pid) {
        eprintln!("daemon process {} is not running (stale PID file)", pid);
        let _ = std::fs::remove_file(&pid_path);
        let _ = std::fs::remove_file(&daemon_heartbeat_path());
        return;
    }

    // Send SIGTERM
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }

    println!("sent SIGTERM to daemon (pid {})", pid);

    // Wait briefly for graceful shutdown
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if !is_pid_alive(pid) {
            println!("daemon stopped");
            return;
        }
    }

    println!("daemon still running after 10s — sending SIGKILL");
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    let _ = std::fs::remove_file(&pid_path);
    let _ = std::fs::remove_file(&daemon_heartbeat_path());
}

// ─── telemetry ───────────────────────────────────────────────────────────────

fn cmd_telemetry(spec_id: &str, db_str: &str) {
    ensure_db_dir(db_str);
    let q = match queue::Queue::open(db_str) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("error: cannot open queue: {}", e);
            std::process::exit(1);
        }
    };

    let iterations = match q.get_iterations(spec_id) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };

    if iterations.is_empty() {
        println!("no iteration records for {}", spec_id);
        return;
    }

    println!(
        "{}{}Iterations for {}{}\n",
        BOLD, CYAN, spec_id, RESET
    );
    println!(
        "  {:>4}  {:>10}  {:>8}  {:>8}  {:>6}  {}",
        "ITER", "PHASE", "TASKS+", "DONE", "EXIT", "DURATION"
    );
    println!("  {}", "-".repeat(60));

    for rec in &iterations {
        let phase = rec.phase.as_deref().unwrap_or("?");
        let duration = rec
            .duration_seconds
            .map(|d| {
                if d < 60.0 {
                    format!("{:.0}s", d)
                } else {
                    format!("{:.1}m", d / 60.0)
                }
            })
            .unwrap_or_else(|| "?".to_string());
        let exit = rec
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".to_string());

        println!(
            "  {:>4}  {:>10}  {:>8}  {:>8}  {:>6}  {}",
            rec.iteration, phase, rec.tasks_added, rec.tasks_completed, exit, duration
        );
    }
}

// ─── spec management ─────────────────────────────────────────────────────────

fn cmd_spec(queue_id: &str, action: Option<SpecAction>, db_str: &str) {
    ensure_db_dir(db_str);
    let q = match queue::Queue::open(db_str) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("error: cannot open queue: {}", e);
            std::process::exit(1);
        }
    };

    match action {
        None => {
            // Show tasks
            match q.status(queue_id) {
                Ok(Some(_)) => {
                    print!("{}", render_single_spec(&q, queue_id));
                }
                Ok(None) => {
                    eprintln!("error: spec '{}' not found", queue_id);
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Some(SpecAction::Add {
            title,
            spec,
            verify,
            depends,
        }) => {
            // Generate a task ID
            let existing = q.status(queue_id);
            let task_num = match &existing {
                Ok(Some(st)) => st.tasks.len() + 1,
                _ => 1,
            };
            let task_id = format!("t-{}", task_num);

            match q.add_task(
                queue_id,
                &task_id,
                &title,
                spec.as_deref(),
                verify.as_deref(),
                &depends,
            ) {
                Ok(()) => println!("added {} to {}", task_id, queue_id),
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Some(SpecAction::Skip { task_id }) => match q.skip_task(queue_id, &task_id) {
            Ok(()) => println!("skipped {} in {}", task_id, queue_id),
            Err(e) => {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
        },
        Some(SpecAction::Block { task_id, on }) => {
            match q.block_task(queue_id, &task_id, &on) {
                Ok(()) => println!("blocked {} on {} in {}", task_id, on, queue_id),
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                }
            }
        }
    }
}

// ─── doctor ──────────────────────────────────────────────────────────────────

fn cmd_doctor(db_str: &str, cfg: &config::Config) {
    let mut issues = 0;

    println!("{}{}BOI Doctor{}\n", BOLD, CYAN, RESET);

    // 1. Daemon running?
    let pid_path = daemon_pid_path();
    if pid_path.exists() {
        if let Ok(pid_str) = std::fs::read_to_string(&pid_path) {
            if let Ok(pid) = pid_str.trim().parse::<u32>() {
                if is_pid_alive(pid) {
                    println!("  {}✓{} Daemon running (pid {})", GREEN, RESET, pid);
                } else {
                    println!(
                        "  {}✗{} Daemon PID file exists but process {} is dead",
                        RED, RESET, pid
                    );
                    issues += 1;
                }
            } else {
                println!("  {}✗{} Invalid PID file content", RED, RESET);
                issues += 1;
            }
        }
    } else {
        println!("  {}⊘{} Daemon not running (no PID file)", YELLOW, RESET);
    }

    // 2. Heartbeat fresh?
    let hb_path = daemon_heartbeat_path();
    if hb_path.exists() {
        if let Ok(ts_str) = std::fs::read_to_string(&hb_path) {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts_str.trim()) {
                let age = chrono::Utc::now()
                    .signed_duration_since(dt.with_timezone(&chrono::Utc))
                    .num_seconds();
                if age < 30 {
                    println!("  {}✓{} Heartbeat fresh ({}s ago)", GREEN, RESET, age);
                } else if age < 3600 {
                    println!("  {}⊘{} Heartbeat {}s ago", YELLOW, RESET, age);
                } else {
                    println!(
                        "  {}✗{} Heartbeat stale ({})",
                        RED,
                        RESET,
                        time_ago(ts_str.trim())
                    );
                    issues += 1;
                }
            }
        }
    } else {
        println!("  {}⊘{} No heartbeat file", YELLOW, RESET);
    }

    // 3. DB accessible?
    ensure_db_dir(db_str);
    match queue::Queue::open(db_str) {
        Ok(q) => {
            println!("  {}✓{} Database accessible ({})", GREEN, RESET, db_str);

            // Count specs
            match q.status_all() {
                Ok(specs) => {
                    let running = specs.iter().filter(|s| s.status == "running").count();
                    let queued = specs.iter().filter(|s| s.status == "queued").count();
                    let completed = specs.iter().filter(|s| s.status == "completed").count();
                    let failed = specs.iter().filter(|s| s.status == "failed").count();
                    println!(
                        "  {}✓{} Queue: {} running, {} queued, {} completed, {} failed",
                        GREEN, RESET, running, queued, completed, failed
                    );
                }
                Err(e) => {
                    println!("  {}✗{} Cannot query specs: {}", RED, RESET, e);
                    issues += 1;
                }
            }
        }
        Err(e) => {
            println!("  {}✗{} Database error: {}", RED, RESET, e);
            issues += 1;
        }
    }

    // 4. Worktrees healthy?
    let wt_dir = cfg.worktrees_dir();
    if wt_dir.exists() {
        match std::fs::read_dir(&wt_dir) {
            Ok(entries) => {
                let entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
                let stale = entries
                    .iter()
                    .filter(|e| !e.path().join(".git").exists())
                    .count();
                if stale == 0 {
                    println!(
                        "  {}✓{} Worktrees: {} active, 0 stale",
                        GREEN,
                        RESET,
                        entries.len()
                    );
                } else {
                    println!(
                        "  {}⊘{} Worktrees: {} active, {} stale (run cleanup)",
                        YELLOW,
                        RESET,
                        entries.len() - stale,
                        stale
                    );
                }
            }
            Err(e) => {
                println!("  {}✗{} Cannot read worktrees: {}", RED, RESET, e);
                issues += 1;
            }
        }
    } else {
        println!("  {}✓{} No worktrees directory (clean)", GREEN, RESET);
    }

    // 5. Config valid?
    let config_path = config::default_config_path();
    if config_path.exists() {
        match std::fs::read_to_string(&config_path) {
            Ok(content) => match serde_yaml::from_str::<config::Config>(&content) {
                Ok(_) => println!("  {}✓{} Config valid ({})", GREEN, RESET, config_path.display()),
                Err(e) => {
                    println!(
                        "  {}✗{} Config parse error: {}",
                        RED, RESET, e
                    );
                    issues += 1;
                }
            },
            Err(e) => {
                println!("  {}✗{} Cannot read config: {}", RED, RESET, e);
                issues += 1;
            }
        }
    } else {
        println!("  {}✓{} Using default config (no config file)", GREEN, RESET);
    }

    println!();
    if issues == 0 {
        println!("{}All checks passed.{}", GREEN, RESET);
    } else {
        println!("{}{} issue(s) found.{}", RED, issues, RESET);
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────────

fn ensure_db_dir(db_str: &str) {
    if let Some(parent) = std::path::Path::new(db_str).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
}

fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let truncated: String = chars[..max - 1].iter().collect();
        format!("{}…", truncated)
    }
}

fn is_pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}
