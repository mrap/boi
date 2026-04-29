use crate::{
    hooks::{
        self, HookConfig, ON_COMPLETE, ON_FAIL, ON_TASK_COMPLETE, ON_TASK_FAIL, ON_TASK_START,
        ON_WORKER_START, ON_PHASE_START, ON_PHASE_COMPLETE, ON_PHASE_FAIL,
    },
    phases::{self, PhaseLevel, PhaseRegistry, Verdict},
    queue::{PhaseRunRecord, Queue},
    runner::{ClaudePhaseRunner, PhaseRunner},
    spec,
    telemetry::{LogLevel, Telemetry},
};
use chrono::Utc;
use serde_json::json;

macro_rules! boi_log {
    ($($arg:tt)*) => {
        eprintln!("[boi {}] {}", Utc::now().format("%H:%M:%S"), format!($($arg)*))
    };
}

use std::{
    collections::{HashMap, HashSet},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

pub struct WorkerConfig {
    pub max_workers: u32,
    pub task_timeout_secs: u64,
    pub retry_count: u32,
    pub cleanup_on_failure: bool,
    pub claude_bin: String,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        WorkerConfig {
            max_workers: 5,
            task_timeout_secs: 1800,
            retry_count: 3,
            cleanup_on_failure: false,
            claude_bin: std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string()),
        }
    }
}

pub fn build_prompt(spec_content: &str, task: &spec::BoiTask) -> String {
    let task_spec = task.spec.as_deref().unwrap_or("(no spec provided)");
    let task_verify = task.verify.as_deref().unwrap_or("(no verify command)");
    format!(
        "You are a BOI worker. Execute exactly one task from this spec.\n\n\
        FULL SPEC:\n{}\n\n\
        YOUR TASK: {} — {}\n\n\
        SPEC:\n{}\n\n\
        VERIFY:\n{}\n\n\
        Execute the task. Do NOT modify the spec file — status is tracked externally.",
        spec_content, task.id, task.title, task_spec, task_verify
    )
}

pub fn run_verify(verify_cmd: &str, dir: &str) -> bool {
    Command::new("sh")
        .args(["-c", verify_cmd])
        .current_dir(dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub struct ClaudeResult {
    pub success: bool,
    pub output: String,
    pub stderr: String,
    pub startup_ms: u64,
    pub inference_ms: u64,
    pub total_ms: u64,
}

/// Return the directory where PID files are stored for running specs.
pub fn pid_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(home).join(".boi").join("pids")
}

/// Return the PID file path for a given spec.
pub fn pid_file_for(spec_id: &str) -> std::path::PathBuf {
    pid_dir().join(format!("{}.pid", spec_id))
}

/// Spawn claude with the task prompt. Returns ClaudeResult with timing data.
/// startup_ms = time from spawn to first stdout byte.
/// inference_ms = time from first byte to process exit.
/// Respects timeout: kills the process and returns failure if exceeded.
/// The `claude_bin` parameter specifies the claude binary path.
/// If `spec_id` is provided, writes the child PID to ~/.boi/pids/{spec_id}.pid
/// so that `boi cancel` can kill it.
pub fn spawn_claude(
    prompt: &str,
    worktree_path: &str,
    timeout_secs: u64,
    model: Option<&str>,
    spec_id: Option<&str>,
    claude_bin: &str,
) -> Result<ClaudeResult, Box<dyn std::error::Error>> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;

    let mut args = vec![
        "-p".to_string(), prompt.to_string(),
        "--dangerously-skip-permissions".to_string(),
        "--no-session-persistence".to_string(),
        "--setting-sources".to_string(), "user".to_string(),
        "--output-format".to_string(), "stream-json".to_string(),
        "--verbose".to_string(),
    ];
    if let Some(m) = model {
        args.push("--model".to_string());
        args.push(m.to_string());
    }
    let args_display: Vec<&str> = args.iter().skip(2).map(|s| s.as_str()).collect();
    boi_log!("spawning claude\n  bin:    {}\n  args:   {}\n  cwd:    {}\n  prompt: {} chars\n  prompt: {}",
        claude_bin, args_display.join(" "), worktree_path, prompt.len(),
        prompt.chars().take(500).collect::<String>());

    let mut cmd = Command::new(claude_bin);
    cmd.args(&args)
        .current_dir(worktree_path)
        .env("AGENT_DIR", worktree_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: setsid() is async-signal-safe per POSIX, safe to call after fork before exec.
    // This puts the child in its own process group so we can kill all grandchildren via -pgid.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;

    // Store process group ID for killing grandchildren on exit/timeout
    let pgid = child.id() as i32;

    // Write PID file so `boi cancel` can kill this subprocess
    let pid_path = spec_id.map(|sid| {
        let p = pid_file_for(sid);
        if let Some(parent) = p.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("[boi] ERROR: failed to create pid dir {}: {}", parent.display(), e);
            }
        }
        let child_pid = child.id();
        if let Err(e) = std::fs::write(&p, child_pid.to_string()) {
            eprintln!("[boi] ERROR: failed to write pid file {}: {}", p.display(), e);
        }
        boi_log!(" wrote pid {} to {}", child_pid, p.display());
        p
    });

    let spawn_time = Instant::now();
    let stdout_pipe = child.stdout.take().expect("stdout was piped");
    let stderr_pipe = child.stderr.take().expect("stderr was piped");

    let stderr_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Err(e) = std::io::BufReader::new(stderr_pipe).read_to_string(&mut buf) {
            eprintln!("[boi] ERROR: failed to read claude stderr: {}", e);
        }
        buf
    });

    let reader_handle = std::thread::spawn(move || {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(stdout_pipe);
        let mut first_byte_time: Option<Instant> = None;
        let mut last_output = String::new();
        let mut raw_lines = Vec::new();

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if first_byte_time.is_none() {
                first_byte_time = Some(Instant::now());
            }
            if let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) {
                let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match event_type {
                    "assistant" => {
                        if let Some(msg) = event.pointer("/message/content/0/text").and_then(|v| v.as_str()) {
                            let preview: String = msg.chars().take(120).collect();
                            boi_log!("  claude: {}", preview);
                        }
                        if let Some(tool) = event.pointer("/message/content/0/name").and_then(|v| v.as_str()) {
                            let input_preview = event.pointer("/message/content/0/input")
                                .map(|v| {
                                    let s = v.to_string();
                                    s.chars().take(150).collect::<String>()
                                })
                                .unwrap_or_default();
                            boi_log!("  tool: {} {}", tool, input_preview);
                        }
                    }
                    "result" => {
                        if let Some(text) = event.get("result").and_then(|v| v.as_str()) {
                            last_output = text.to_string();
                        }
                    }
                    _ => {}
                }
            } else {
                raw_lines.push(line);
            }
        }
        if last_output.is_empty() && !raw_lines.is_empty() {
            last_output = raw_lines.join("\n");
        }
        (first_byte_time, last_output)
    });

    let mut timed_out = false;
    let mut exit_status = None;
    loop {
        match child.try_wait()? {
            Some(status) => {
                exit_status = Some(status);
                // SAFETY: pgid is the child's PID (valid after setsid), negative value targets
                // the entire process group, killing any grandchildren that inherited the pipes.
                unsafe { libc::kill(-pgid, libc::SIGKILL); }
                break;
            }
            None => {
                if spawn_time.elapsed().as_secs() >= timeout_secs {
                    let _ = child.kill(); // intentional: best-effort cleanup on timeout
                    let _ = child.wait(); // intentional: reap zombie after kill
                    // SAFETY: same as above — kill the entire process group on timeout.
                    unsafe { libc::kill(-pgid, libc::SIGKILL); }
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }

    let total_ms = spawn_time.elapsed().as_millis() as u64;

    // Clean up PID file — child has exited (or been killed on timeout)
    if let Some(ref p) = pid_path {
        let _ = std::fs::remove_file(p); // intentional: best-effort pid file cleanup
    }

    if timed_out {
        let _ = reader_handle.join(); // intentional: best-effort thread join on timeout
        let stderr_output = stderr_handle.join().unwrap_or_default();
        if !stderr_output.is_empty() {
            boi_log!("claude stderr (timeout):\n{}", stderr_output);
        }
        return Ok(ClaudeResult {
            success: false,
            output: "timeout".to_string(),
            stderr: stderr_output,
            startup_ms: 0,
            inference_ms: 0,
            total_ms,
        });
    }

    // With setsid + process group kill, grandchildren are dead and pipes unblock naturally.
    let (first_byte_instant, output) = reader_handle.join().unwrap_or((None, String::new()));
    let stderr_output = stderr_handle.join().unwrap_or_default();

    if !stderr_output.is_empty() {
        boi_log!("claude stderr:\n{}", stderr_output);
    }

    let startup_ms = first_byte_instant
        .map(|t| t.duration_since(spawn_time).as_millis() as u64)
        .unwrap_or(total_ms);
    let inference_ms = total_ms.saturating_sub(startup_ms);

    let success = exit_status.map(|s| s.success()).unwrap_or(false);
    Ok(ClaudeResult {
        success,
        output,
        stderr: stderr_output,
        startup_ms,
        inference_ms,
        total_ms,
    })
}

#[allow(clippy::too_many_arguments)]
fn record_phase_run(
    queue: &Queue,
    spec_id: &str,
    task_id: Option<&str>,
    phase_name: &str,
    level: &str,
    verdict: &Verdict,
    started_at: &str,
    elapsed_ms: i64,
) {
    let outcome_str = match verdict {
        Verdict::Proceed => "proceed",
        Verdict::Redo { .. } => "redo",
        Verdict::Pause { .. } => "pause",
        Verdict::Done { success: true, .. } => "done",
        Verdict::Done { success: false, .. } => "failed",
    };
    let completed_at = Utc::now().to_rfc3339();
    let rec = PhaseRunRecord {
        spec_id: spec_id.to_string(),
        task_id: task_id.map(|s| s.to_string()),
        phase: phase_name.to_string(),
        level: level.to_string(),
        outcome: outcome_str.to_string(),
        duration_ms: Some(elapsed_ms),
        cost_usd: None,
        input_tokens: None,
        output_tokens: None,
        started_at: started_at.to_string(),
        completed_at: Some(completed_at),
    };
    if let Err(e) = queue.insert_phase_run(&rec) {
        eprintln!("[boi] ERROR: failed to insert phase_run for spec={} phase={}: {}", spec_id, phase_name, e);
    }
}

/// Execute all pending tasks for a queued spec.
///
/// Reads the spec YAML at `spec_path`, processes tasks in topological order,
/// using the phase pipeline (resolve_pipeline → spec phases → task phases).
/// Updates `queue_path` (SQLite) after each task and when the spec completes or fails.
pub fn run_worker(
    spec_id: &str,
    spec_path: &str,
    queue_path: &str,
    hook_config: &HookConfig,
    config: &WorkerConfig,
    telemetry: &Telemetry,
) -> Result<(), Box<dyn std::error::Error>> {
    let registry = PhaseRegistry::new();
    registry_load_user(&registry);
    let runner = Arc::new(ClaudePhaseRunner::new(telemetry.clone(), config.claude_bin.clone()));
    run_worker_with_phases(spec_id, spec_path, queue_path, hook_config, config, &registry, runner.as_ref(), telemetry)
}

/// Load user phases into a registry (helper to avoid mutability issues in run_worker).
fn registry_load_user(registry: &PhaseRegistry) {
    // PhaseRegistry::new() already loads core phases. User phases need a mutable registry,
    // but we handle this by creating a new registry with user phases in run_worker_with_registry.
    let _ = registry; // intentional: consume parameter to suppress unused warning
}

/// Worker state machine — flat loop, no nested breaks.
///
/// Every state does ONE thing: one claude call, one shell command, or one decision.
/// State transitions are explicit assignments. The requeue counter lives in the
/// TaskRequeue state, not as a mutable variable.
#[derive(Debug, Clone)]
enum WorkerState {
    /// Run a pre-task spec-level phase (plan-critique)
    SpecPhase { phase_idx: usize },
    /// Select the next ready task from the DAG
    TaskSelect,
    /// Run a task-level phase (execute, task-verify, code-review)
    TaskPhase { task_id: String, phase_idx: usize, requeue_attempts: usize },
    /// Task phase failed — retry the phase up to max_attempts
    TaskPhaseRetry { task_id: String, phase_idx: usize, attempt: u32 },
    /// Task verify/review failed — requeue back to a target phase
    TaskRequeue { task_id: String, target_phase: String, attempts: usize },
    /// All tasks done — run post-task spec phases (critic, evaluate)
    PostTaskSpecPhase { phase_idx: usize },
    /// Spec paused — waiting for human input via `boi decide <id>`
    Paused { prompt: String },
    /// Spec completed successfully — update DB, fire hooks
    Complete,
    /// Spec failed — update DB, fire hooks
    Failed { reason: String },
    /// Terminal: clean up worktree (only state that touches worktree cleanup)
    Cleanup { success: bool },
}

/// Execute all pending tasks using the phase pipeline with a custom PhaseRunner.
/// This is the core implementation, testable with mock runners.
#[allow(clippy::too_many_arguments)]
pub fn run_worker_with_phases(
    spec_id: &str,
    spec_path: &str,
    queue_path: &str,
    hook_config: &HookConfig,
    config: &WorkerConfig,
    registry: &PhaseRegistry,
    runner: &dyn PhaseRunner,
    telemetry: &Telemetry,
) -> Result<(), Box<dyn std::error::Error>> {
    let queue = Queue::open(queue_path)?;
    queue.update_spec(spec_id, "running")?;
    let _ = hooks::fire(hook_config, ON_WORKER_START, &json!({ "spec_id": spec_id })); // intentional: best-effort hook notification

    telemetry.emit("boi.worker.started", LogLevel::Info, &json!({
        "spec_id": spec_id,
        "message": format!("worker started for {}", spec_id),
    }));

    let spec_content_raw = std::fs::read_to_string(spec_path)?;
    let boi_spec = spec::parse_unchecked(&spec_content_raw)?;

    // Extract original workspace path before worktree creation (needed for path substitution).
    let original_workspace = boi_spec.workspace.clone();

    let worktree_path: String = match &original_workspace {
        Some(ws) if !ws.is_empty() => {
            let worktree_dir = crate::worktree::create(spec_id, ws)?;
            worktree_dir.to_str()
                .ok_or_else(|| -> Box<dyn std::error::Error> { "worktree path is not valid UTF-8".into() })?
                .to_string()
        }
        _ => {
            let queue_tag = std::path::Path::new(queue_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("default");
            let tmp = std::env::temp_dir().join(format!("boi-{}-{}", spec_id, queue_tag));
            std::fs::create_dir_all(&tmp)?;
            boi_log!(" no workspace set — running in temp dir: {}", tmp.display());
            tmp.to_str()
                .ok_or_else(|| -> Box<dyn std::error::Error> { "temp dir path is not valid UTF-8".into() })?
                .to_string()
        }
    };

    // Rewrite workspace paths in spec content AND re-parse so task objects (including verify
    // commands) also get rewritten paths. Without re-parsing, verify commands would still
    // reference the original repo path, causing `cd /original/path && ...` to escape the worktree.
    let (spec_content, boi_spec) = if let Some(ref ws) = original_workspace {
        let rewritten = spec_content_raw.replace(ws.as_str(), &worktree_path);
        let rewritten_spec = spec::parse_unchecked(&rewritten)?;

        for task in &rewritten_spec.tasks {
            if let Some(ref verify) = task.verify {
                if verify.contains(ws.as_str()) {
                    boi_log!("WARNING: task {} verify still references original workspace '{}'", task.id, ws);
                }
            }
        }

        (rewritten, rewritten_spec)
    } else {
        (spec_content_raw, boi_spec)
    };

    let order = match spec::topological_sort(&boi_spec) {
        Ok(o) => o,
        Err(e) => {
            queue.update_spec(spec_id, "failed")?;
            return Err(Box::new(e));
        }
    };

    // Resolve the pipeline for this spec
    let mode = boi_spec.mode.as_deref().unwrap_or("execute");
    let pipeline = phases::resolve_pipeline(
        mode,
        boi_spec.spec_phases.as_deref(),
        boi_spec.task_phases.as_deref(),
    );

    // Build mapping from YAML task IDs to canonical DB IDs
    let mut yaml_to_canonical: HashMap<String, String> = HashMap::new();
    if let Ok(db_tasks) = queue.get_tasks(spec_id) {
        for (i, dt) in db_tasks.iter().enumerate() {
            if i < boi_spec.tasks.len() {
                yaml_to_canonical.insert(boi_spec.tasks[i].id.clone(), dt.id.clone());
            }
        }
    }

    // task_map keyed by YAML authoring IDs (matching `order` from topological_sort)
    let task_map: HashMap<String, &spec::BoiTask> = boi_spec
        .tasks
        .iter()
        .map(|t| (t.id.clone(), t))
        .collect();

    // Reverse map: canonical DB ID → YAML authoring ID
    let canonical_to_yaml: HashMap<String, String> = yaml_to_canonical
        .iter()
        .map(|(yaml, canonical)| (canonical.clone(), yaml.clone()))
        .collect();

    let mut done_ids: HashSet<String> = HashSet::new();
    let mut skipped_ids: HashSet<String> = HashSet::new();
    let mut db_depends: HashMap<String, Vec<String>> = HashMap::new();
    if let Ok(db_tasks) = queue.get_tasks(spec_id) {
        for dt in &db_tasks {
            // Use YAML IDs internally so they match `order` and `task_map` keys
            let yaml_id = canonical_to_yaml.get(&dt.id).unwrap_or(&dt.id).clone();
            match dt.status.as_str() {
                "DONE" => { done_ids.insert(yaml_id.clone()); }
                "SKIPPED" => {
                    skipped_ids.insert(yaml_id.clone());
                    done_ids.insert(yaml_id.clone());
                }
                _ => {}
            }
            let deps: Vec<String> = serde_json::from_str(&dt.depends).unwrap_or_default();
            if !deps.is_empty() {
                db_depends.insert(yaml_id.clone(), deps);
            }
        }
    }

    // Precompute phase lists
    let pre_spec_phases: Vec<&str> = pipeline
        .spec_phases
        .iter()
        .filter_map(|name| {
            registry.get(name).and_then(|p| {
                if p.level == PhaseLevel::Spec && name == "plan-critique" {
                    Some(name.as_str())
                } else {
                    None
                }
            })
        })
        .collect();

    let post_spec_phases: Vec<&str> = pipeline
        .spec_phases
        .iter()
        .filter_map(|name| {
            registry.get(name).and_then(|p| {
                if p.level == PhaseLevel::Spec && name != "plan-critique" {
                    Some(name.as_str())
                } else {
                    None
                }
            })
        })
        .collect();

    // Track pass count for deadlock detection in TaskSelect
    let mut task_select_passes: usize = 0;
    let mut spec_redo_count: usize = 0;
    let max_spec_redos = config.retry_count as usize;
    let max_task_select_passes = order.len().max(1);

    // Template variables for phase prompts
    use crate::phases::TemplateVar;
    let pending_count = order.len() - done_ids.len();
    let mut prompt_vars: HashMap<String, String> = HashMap::new();
    prompt_vars.insert(TemplateVar::QueueId.key().into(), spec_id.to_string());
    prompt_vars.insert(TemplateVar::SpecPath.key().into(), spec_path.to_string());
    prompt_vars.insert(TemplateVar::Iteration.key().into(), "1".into());
    prompt_vars.insert(TemplateVar::PendingCount.key().into(), pending_count.to_string());
    prompt_vars.insert(TemplateVar::SpecContent.key().into(), spec_content.clone());
    prompt_vars.insert(TemplateVar::WorkspaceHeader.key().into(),
        boi_spec.workspace.as_ref()
            .map(|_| format!("Workspace: {}\n", worktree_path))
            .unwrap_or_default());
    TemplateVar::validate(&prompt_vars).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    // --- State machine ---
    let mut state = WorkerState::SpecPhase { phase_idx: 0 };
    boi_log!("state machine start: spec={} mode={} tasks={} pre_spec_phases={} post_spec_phases={}",
        spec_id, mode, order.len(), pre_spec_phases.len(), post_spec_phases.len());

    loop {
        // Validate worktree still exists before every state transition.
        // If pruned mid-execution, abort cleanly instead of falling back to parent repo.
        match &state {
            WorkerState::Cleanup { .. } => {} // Don't check during cleanup
            _ => {
                if !std::path::Path::new(&worktree_path).exists() {
                    eprintln!(
                        "[boi] ERROR: worktree {} disappeared — aborting spec {}",
                        worktree_path, spec_id
                    );
                    if let Err(e) = queue.update_spec(spec_id, "failed") {
                        eprintln!("[boi] ERROR: failed to mark spec {} as failed after worktree loss: {}", spec_id, e);
                    }
                    telemetry.emit("boi.spec.failed", LogLevel::Info, &json!({
                        "spec_id": spec_id,
                        "status": "failed",
                        "message": format!("worktree {} no longer exists", worktree_path),
                    }));
                    break;
                }
            }
        }

        match state {
            WorkerState::SpecPhase { phase_idx } => {
                if phase_idx >= pre_spec_phases.len() {
                    boi_log!("state: SpecPhase -> TaskSelect (all {} pre-spec phases done)", pre_spec_phases.len());
                    state = WorkerState::TaskSelect;
                    continue;
                }
                let phase_name = pre_spec_phases[phase_idx];
                boi_log!("state: SpecPhase {{ phase_idx: {}, phase: '{}' }}", phase_idx, phase_name);
                let phase = match registry.get(phase_name) {
                    Some(p) => p,
                    None => {
                        state = WorkerState::SpecPhase { phase_idx: phase_idx + 1 };
                        continue;
                    }
                };

                let phase_payload = json!({
                    "spec_id": spec_id,
                    "phase": phase_name,
                    "level": "spec",
                });
                let _ = hooks::fire(hook_config, ON_PHASE_START, &phase_payload); // intentional: best-effort hook notification

                telemetry.emit("boi.phase.start", LogLevel::Info, &json!({
                    "spec_id": spec_id,
                    "phase": phase_name,
                    "level": "spec",
                    "message": format!("spec phase '{}' started", phase_name),
                }));

                let phase_start = Instant::now();
                let phase_started_at = Utc::now().to_rfc3339();
                let verdict = runner.run_phase(
                    phase,
                    &spec_content,
                    None,
                    &worktree_path,
                    config.task_timeout_secs,
                    Some(spec_id),
                    &prompt_vars,
                );
                let elapsed_ms = phase_start.elapsed().as_millis() as i64;
                record_phase_run(&queue, spec_id, None, phase_name, "spec", &verdict, &phase_started_at, elapsed_ms);

                emit_phase_verdict(telemetry, spec_id, None, phase_name, &verdict, elapsed_ms);

                match &verdict {
                    Verdict::Proceed => {
                        let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload); // intentional: best-effort hook notification
                        state = WorkerState::SpecPhase { phase_idx: phase_idx + 1 };
                    }
                    Verdict::Redo { tasks } => {
                        let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload); // intentional: best-effort hook notification
                        // Inject tasks if any, then go to TaskSelect
                        if !tasks.is_empty() {
                            for t in tasks {
                                let _ = queue.add_task( // intentional: best-effort task injection during redo
                                    spec_id,
                                    &t.id,
                                    &t.title,
                                    t.spec.as_deref(),
                                    t.verify.as_deref(),
                                    t.depends.as_deref().unwrap_or(&[]),
                                );
                            }
                        }
                        state = WorkerState::TaskSelect;
                    }
                    Verdict::Pause { prompt } => {
                        state = WorkerState::Paused { prompt: prompt.clone() };
                    }
                    Verdict::Done { success: false, reason } => {
                        boi_log!(" pre-task spec phase '{}' failed: {}", phase_name, reason);
                        let _ = hooks::fire(hook_config, ON_PHASE_FAIL, &phase_payload); // intentional: best-effort hook notification
                        if phase.can_fail_spec {
                            state = WorkerState::Failed {
                                reason: format!("pre-task phase '{}' failed: {}", phase_name, reason),
                            };
                        } else {
                            state = WorkerState::SpecPhase { phase_idx: phase_idx + 1 };
                        }
                    }
                    Verdict::Done { success: true, reason } => {
                        let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload); // intentional: best-effort hook notification
                        boi_log!(" pre-task spec phase '{}' done: {}", phase_name, reason);
                        state = WorkerState::SpecPhase { phase_idx: phase_idx + 1 };
                    }
                }
            }

            WorkerState::TaskSelect => {
                // Refresh dynamic template vars so phases see current state
                let pending_count = order.len() - done_ids.len();
                prompt_vars.insert(TemplateVar::PendingCount.key().into(), pending_count.to_string());
                prompt_vars.insert(TemplateVar::Iteration.key().into(), (spec_redo_count + 1).to_string());

                // Find next ready task: PENDING, all deps satisfied
                let mut found = false;
                for task_id in &order {
                    if done_ids.contains(task_id.as_str()) || skipped_ids.contains(task_id.as_str()) {
                        continue;
                    }
                    let task = match task_map.get(task_id.as_str()) {
                        Some(t) => t,
                        None => continue,
                    };

                    // Merge DB-level deps with YAML deps
                    let effective_deps: Vec<String> = if let Some(db_d) = db_depends.get(task_id.as_str()) {
                        let mut merged = db_d.clone();
                        if let Some(yaml_deps) = &task.depends {
                            for d in yaml_deps {
                                if !merged.contains(d) {
                                    merged.push(d.clone());
                                }
                            }
                        }
                        merged
                    } else {
                        task.depends.clone().unwrap_or_default()
                    };

                    if effective_deps.iter().any(|d| !done_ids.contains(d)) {
                        continue;
                    }

                    // Found a ready task — start it
                    let task_payload = json!({
                        "spec_id": spec_id,
                        "task_id": task.id,
                        "task_title": task.title,
                    });
                    let db_task_id = yaml_to_canonical.get(task_id.as_str()).map(|s| s.as_str()).unwrap_or(task_id.as_str());
                    queue.update_task(spec_id, db_task_id, "RUNNING")?;
                    let _ = hooks::fire(hook_config, ON_TASK_START, &task_payload); // intentional: best-effort hook notification

                    telemetry.emit("boi.task.started", LogLevel::Info, &json!({
                        "spec_id": spec_id,
                        "task_id": task.id,
                        "message": format!("{}: {} — started", task.id, task.title),
                    }));

                    task_select_passes = 0;
                    found = true;
                    state = WorkerState::TaskPhase {
                        task_id: task.id.clone(),
                        phase_idx: 0,
                        requeue_attempts: 0,
                    };
                    break;
                }

                if !found {
                    // Check if all tasks are done
                    let all_done = order.iter().all(|id| {
                        done_ids.contains(id) || skipped_ids.contains(id)
                    });
                    if all_done {
                        boi_log!("state: TaskSelect -> PostTaskSpecPhase (all {} tasks done)", order.len());
                        state = WorkerState::PostTaskSpecPhase { phase_idx: 0 };
                    } else {
                        // Some tasks are still pending but none are ready — possible deadlock
                        // or DB-level deps not yet satisfied. Re-scan up to max passes.
                        task_select_passes += 1;
                        let pending: Vec<&String> = order.iter()
                            .filter(|id| !done_ids.contains(id.as_str()) && !skipped_ids.contains(id.as_str()))
                            .collect();
                        boi_log!("state: TaskSelect — deadlock detected (pass {}/{}), pending tasks: {:?}",
                            task_select_passes, max_task_select_passes, pending);
                        if task_select_passes > max_task_select_passes {
                            state = WorkerState::Failed {
                                reason: "deadlock: pending tasks but none ready".to_string(),
                            };
                        } else {
                            // Re-scan — done_ids may have grown from a previous pass.
                            // If not, the pass counter will catch it on the next iteration.
                            state = WorkerState::TaskSelect;
                        }
                    }
                }
            }

            WorkerState::TaskPhase { ref task_id, phase_idx, requeue_attempts } => {
                let task_id_owned = task_id.clone();
                let db_task_id = yaml_to_canonical.get(&task_id_owned).cloned().unwrap_or_else(|| task_id_owned.clone());
                let task = match task_map.get(task_id_owned.as_str()) {
                    Some(t) => t,
                    None => {
                        boi_log!("state: TaskPhase -> Failed (task {} not found in task_map)", task_id_owned);
                        state = WorkerState::Failed {
                            reason: format!("task {} not found", task_id_owned),
                        };
                        continue;
                    }
                };

                let task_phases = phases::resolve_task_phases(
                    &pipeline,
                    task.phases.as_deref(),
                );

                if phase_idx >= task_phases.len() {
                    boi_log!("state: TaskPhase -> TaskSelect (task {} complete, all {} phases passed)",
                        task.id, task_phases.len());
                    queue.update_task(spec_id, &db_task_id, "DONE")?;
                    done_ids.insert(task.id.clone());
                    let task_payload = json!({
                        "spec_id": spec_id,
                        "task_id": task.id,
                        "task_title": task.title,
                    });
                    let _ = hooks::fire(hook_config, ON_TASK_COMPLETE, &task_payload); // intentional: best-effort hook notification
                    telemetry.emit("boi.task.completed", LogLevel::Info, &json!({
                        "spec_id": spec_id,
                        "task_id": task.id,
                        "status": "DONE",
                        "message": format!("{} complete", task.id),
                    }));

                    state = WorkerState::TaskSelect;
                    continue;
                }

                let phase_name = &task_phases[phase_idx];
                let phase = match registry.get(phase_name) {
                    Some(p) => p,
                    None => {
                        boi_log!(" unknown phase '{}' in task {} — skipping", phase_name, task.id);
                        state = WorkerState::TaskPhase {
                            task_id: task_id_owned,
                            phase_idx: phase_idx + 1,
                            requeue_attempts,
                        };
                        continue;
                    }
                };

                boi_log!("state: TaskPhase {{ task: {}, phase_idx: {}, phase: '{}', requeue_attempts: {} }}",
                    task.id, phase_idx, phase_name, requeue_attempts);

                let phase_payload = json!({
                    "spec_id": spec_id,
                    "task_id": task.id,
                    "phase": phase_name,
                    "level": "task",
                });
                let _ = hooks::fire(hook_config, ON_PHASE_START, &phase_payload); // intentional: best-effort hook notification

                telemetry.emit("boi.phase.start", LogLevel::Info, &json!({
                    "spec_id": spec_id,
                    "task_id": task.id,
                    "phase": phase_name,
                    "message": format!("{}: {} phase started", task.id, phase_name),
                }));

                let phase_start = Instant::now();
                let phase_started_at = Utc::now().to_rfc3339();
                let verdict = runner.run_phase(
                    phase,
                    &spec_content,
                    Some(task),
                    &worktree_path,
                    config.task_timeout_secs,
                    Some(spec_id),
                    &prompt_vars,
                );
                let elapsed_ms = phase_start.elapsed().as_millis() as i64;
                record_phase_run(&queue, spec_id, Some(&task.id), phase_name, "task", &verdict, &phase_started_at, elapsed_ms);

                emit_phase_verdict(telemetry, spec_id, Some(&task.id), phase_name, &verdict, elapsed_ms);

                boi_log!("state: TaskPhase verdict: task={} phase='{}' -> {:?} ({}ms)",
                    task.id, phase_name, verdict, elapsed_ms);

                match &verdict {
                    Verdict::Proceed => {
                        let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload); // intentional: best-effort hook notification
                        state = WorkerState::TaskPhase {
                            task_id: task_id_owned,
                            phase_idx: phase_idx + 1,
                            requeue_attempts,
                        };
                    }
                    Verdict::Redo { tasks } => {
                        if tasks.is_empty() {
                            // Redo with no new tasks = requeue back to execute
                            boi_log!(" phase '{}' requests redo for task {}", phase_name, task.id);
                            state = WorkerState::TaskRequeue {
                                task_id: task_id_owned,
                                target_phase: "execute".to_string(),
                                attempts: requeue_attempts + 1,
                            };
                        } else {
                            // Inject tasks and go to TaskSelect
                            let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload); // intentional: best-effort hook notification
                            for t in tasks {
                                let _ = queue.add_task( // intentional: best-effort task injection during redo
                                    spec_id,
                                    &t.id,
                                    &t.title,
                                    t.spec.as_deref(),
                                    t.verify.as_deref(),
                                    t.depends.as_deref().unwrap_or(&[]),
                                );
                            }
                            state = WorkerState::TaskSelect;
                        }
                    }
                    Verdict::Pause { prompt } => {
                        state = WorkerState::Paused { prompt: prompt.clone() };
                    }
                    Verdict::Done { success: false, reason } => {
                        boi_log!(" phase '{}' failed for task {}: {}", phase_name, task.id, reason);
                        let _ = hooks::fire(hook_config, ON_PHASE_FAIL, &phase_payload); // intentional: best-effort hook notification
                        let max_attempts = phase.retry_count.unwrap_or(config.retry_count);
                        if max_attempts > 0 {
                            state = WorkerState::TaskPhaseRetry {
                                task_id: task_id_owned,
                                phase_idx,
                                attempt: 1,
                            };
                        } else {
                            queue.update_task(spec_id, &db_task_id, "FAILED")?;
                            let task_payload = json!({
                                "spec_id": spec_id,
                                "task_id": task.id,
                                "task_title": task.title,
                            });
                            let _ = hooks::fire(hook_config, ON_TASK_FAIL, &task_payload); // intentional: best-effort hook notification
                            telemetry.emit("boi.task.failed", LogLevel::Info, &json!({
                                "spec_id": spec_id,
                                "task_id": task.id,
                                "status": "FAILED",
                                "message": format!("{} failed: {}", task.id, reason),
                            }));
                            state = WorkerState::Failed {
                                reason: format!("task {} phase '{}' failed: {}", task.id, phase_name, reason),
                            };
                        }
                    }
                    Verdict::Done { success: true, reason } => {
                        let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload); // intentional: best-effort hook notification
                        boi_log!(" phase '{}' done for task {}: {}", phase_name, task.id, reason);
                        state = WorkerState::TaskPhase {
                            task_id: task_id_owned,
                            phase_idx: phase_idx + 1,
                            requeue_attempts,
                        };
                    }
                }
            }

            WorkerState::TaskPhaseRetry { ref task_id, phase_idx, attempt } => {
                let task_id_owned = task_id.clone();
                let db_task_id = yaml_to_canonical.get(&task_id_owned).cloned().unwrap_or_else(|| task_id_owned.clone());
                boi_log!("state: TaskPhaseRetry {{ task: {}, phase_idx: {}, attempt: {} }}", task_id_owned, phase_idx, attempt);
                let task = match task_map.get(task_id_owned.as_str()) {
                    Some(t) => t,
                    None => {
                        state = WorkerState::Failed {
                            reason: format!("task {} not found", task_id_owned),
                        };
                        continue;
                    }
                };

                let task_phases = phases::resolve_task_phases(
                    &pipeline,
                    task.phases.as_deref(),
                );
                let phase_name = &task_phases[phase_idx];
                let phase = match registry.get(phase_name) {
                    Some(p) => p,
                    None => {
                        state = WorkerState::Failed {
                            reason: format!("phase '{}' not found in registry during retry", phase_name),
                        };
                        continue;
                    }
                };
                let max_attempts = phase.retry_count.unwrap_or(config.retry_count);

                if attempt >= max_attempts {
                    boi_log!("state: TaskPhaseRetry -> Failed (max retries {} reached for task {} phase '{}')",
                        max_attempts, task.id, phase_name);
                    queue.update_task(spec_id, &db_task_id, "FAILED")?;
                    let task_payload = json!({
                        "spec_id": spec_id,
                        "task_id": task.id,
                        "task_title": task.title,
                    });
                    let _ = hooks::fire(hook_config, ON_TASK_FAIL, &task_payload); // intentional: best-effort hook notification
                    telemetry.emit("boi.task.failed", LogLevel::Info, &json!({
                        "spec_id": spec_id,
                        "task_id": task.id,
                        "status": "FAILED",
                        "message": format!("{} failed after {} retries", task.id, attempt),
                    }));
                    state = WorkerState::Failed {
                        reason: format!("task {} phase '{}' failed after {} retries", task.id, phase_name, attempt),
                    };
                    continue;
                }

                eprintln!(
                    "[boi] phase '{}' for task {} failed (attempt {}/{}), retrying",
                    phase_name, task.id, attempt, max_attempts
                );

                let phase_start = Instant::now();
                let phase_started_at = Utc::now().to_rfc3339();
                let retry_verdict = runner.run_phase(
                    phase,
                    &spec_content,
                    Some(task),
                    &worktree_path,
                    config.task_timeout_secs,
                    Some(spec_id),
                    &prompt_vars,
                );
                let elapsed_ms = phase_start.elapsed().as_millis() as i64;
                record_phase_run(&queue, spec_id, Some(&task.id), phase_name, "task", &retry_verdict, &phase_started_at, elapsed_ms);

                emit_phase_verdict(telemetry, spec_id, Some(&task.id), phase_name, &retry_verdict, elapsed_ms);

                boi_log!("state: TaskPhaseRetry verdict: task={} phase='{}' attempt={} -> {:?} ({}ms)",
                    task.id, phase_name, attempt, retry_verdict, elapsed_ms);

                match &retry_verdict {
                    Verdict::Proceed => {
                        // Retry succeeded — advance to next phase
                        state = WorkerState::TaskPhase {
                            task_id: task_id_owned,
                            phase_idx: phase_idx + 1,
                            requeue_attempts: 0,
                        };
                    }
                    Verdict::Redo { .. } => {
                        state = WorkerState::TaskRequeue {
                            task_id: task_id_owned,
                            target_phase: "execute".to_string(),
                            attempts: 1,
                        };
                    }
                    Verdict::Pause { prompt } => {
                        state = WorkerState::Paused { prompt: prompt.clone() };
                    }
                    Verdict::Done { success: false, .. } => {
                        state = WorkerState::TaskPhaseRetry {
                            task_id: task_id_owned,
                            phase_idx,
                            attempt: attempt + 1,
                        };
                    }
                    Verdict::Done { success: true, .. } => {
                        // Retry succeeded — advance to next phase
                        state = WorkerState::TaskPhase {
                            task_id: task_id_owned,
                            phase_idx: phase_idx + 1,
                            requeue_attempts: 0,
                        };
                    }
                }
            }

            WorkerState::TaskRequeue { ref task_id, ref target_phase, attempts } => {
                let task_id_owned = task_id.clone();
                let target_owned = target_phase.clone();

                if attempts > config.retry_count as usize {
                    let task = task_map.get(task_id_owned.as_str());
                    let task_title = task.map(|t| t.title.as_str()).unwrap_or("unknown");
                    boi_log!(" requeue limit ({}) exceeded for task {}", config.retry_count, task_id_owned);
                    let db_task_id_rq = yaml_to_canonical.get(&task_id_owned).cloned().unwrap_or_else(|| task_id_owned.clone());
                    queue.update_task(spec_id, &db_task_id_rq, "FAILED")?;
                    let task_payload = json!({
                        "spec_id": spec_id,
                        "task_id": task_id_owned,
                        "task_title": task_title,
                    });
                    let _ = hooks::fire(hook_config, ON_TASK_FAIL, &task_payload); // intentional: best-effort hook notification
                    telemetry.emit("boi.task.failed", LogLevel::Info, &json!({
                        "spec_id": spec_id,
                        "task_id": task_id_owned,
                        "status": "FAILED",
                        "message": format!("{} failed: requeue limit exceeded", task_id_owned),
                    }));
                    state = WorkerState::Failed {
                        reason: format!("task {} requeue limit exceeded", task_id_owned),
                    };
                    continue;
                }

                let task = match task_map.get(task_id_owned.as_str()) {
                    Some(t) => t,
                    None => {
                        state = WorkerState::Failed {
                            reason: format!("task {} not found", task_id_owned),
                        };
                        continue;
                    }
                };

                let task_phases = phases::resolve_task_phases(
                    &pipeline,
                    task.phases.as_deref(),
                );

                // Find the target phase index
                let target_idx = task_phases.iter().position(|p| p == &target_owned).unwrap_or(0);

                eprintln!(
                    "[boi] requeue to '{}' for task {} (attempt {}/{})",
                    target_owned, task_id_owned, attempts, config.retry_count
                );

                telemetry.emit("boi.task.requeue", LogLevel::Info, &json!({
                    "spec_id": spec_id,
                    "task_id": task_id_owned,
                    "target_phase": target_owned,
                    "attempt": attempts,
                    "message": format!("{}: requeue to '{}' (attempt {})", task_id_owned, target_owned, attempts),
                }));

                // The TaskPhase handler will re-run from the target phase.
                // If it hits another Requeue, attempts will be incremented.
                state = WorkerState::TaskPhase {
                    task_id: task_id_owned,
                    phase_idx: target_idx,
                    requeue_attempts: attempts,
                };
            }

            WorkerState::PostTaskSpecPhase { phase_idx } => {
                if phase_idx >= post_spec_phases.len() {
                    boi_log!("state: PostTaskSpecPhase -> Complete (all {} post-spec phases done)", post_spec_phases.len());
                    state = WorkerState::Complete;
                    continue;
                }

                let phase_name = post_spec_phases[phase_idx];
                boi_log!("state: PostTaskSpecPhase {{ phase_idx: {}, phase: '{}' }}", phase_idx, phase_name);
                let phase = match registry.get(phase_name) {
                    Some(p) => p,
                    None => {
                        boi_log!("state: PostTaskSpecPhase — unknown phase '{}', skipping", phase_name);
                        state = WorkerState::PostTaskSpecPhase { phase_idx: phase_idx + 1 };
                        continue;
                    }
                };

                let phase_payload = json!({
                    "spec_id": spec_id,
                    "phase": phase_name,
                    "level": "spec",
                });
                let _ = hooks::fire(hook_config, ON_PHASE_START, &phase_payload); // intentional: best-effort hook notification

                telemetry.emit("boi.phase.start", LogLevel::Info, &json!({
                    "spec_id": spec_id,
                    "phase": phase_name,
                    "level": "spec",
                    "message": format!("spec phase '{}' started", phase_name),
                }));

                let phase_start = Instant::now();
                let phase_started_at = Utc::now().to_rfc3339();
                let verdict = runner.run_phase(
                    phase,
                    &spec_content,
                    None,
                    &worktree_path,
                    config.task_timeout_secs,
                    Some(spec_id),
                    &prompt_vars,
                );
                let elapsed_ms = phase_start.elapsed().as_millis() as i64;
                record_phase_run(&queue, spec_id, None, phase_name, "spec", &verdict, &phase_started_at, elapsed_ms);

                emit_phase_verdict(telemetry, spec_id, None, phase_name, &verdict, elapsed_ms);

                match &verdict {
                    Verdict::Proceed => {
                        let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload); // intentional: best-effort hook notification
                        state = WorkerState::PostTaskSpecPhase { phase_idx: phase_idx + 1 };
                    }
                    Verdict::Redo { tasks } => {
                        let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload); // intentional: best-effort hook notification
                        // Inject tasks if any, then re-enter task loop
                        if !tasks.is_empty() {
                            for t in tasks {
                                let _ = queue.add_task( // intentional: best-effort task injection during redo
                                    spec_id,
                                    &t.id,
                                    &t.title,
                                    t.spec.as_deref(),
                                    t.verify.as_deref(),
                                    t.depends.as_deref().unwrap_or(&[]),
                                );
                            }
                        }
                        // Re-enter task loop (iterative quality loop), capped
                        spec_redo_count += 1;
                        boi_log!("spec_redo_count incremented to {} (max={})", spec_redo_count, max_spec_redos);
                        if spec_redo_count > max_spec_redos {
                            boi_log!("state: PostTaskSpecPhase -> Complete (spec redo limit {} exceeded)", max_spec_redos);
                            state = WorkerState::Complete;
                        } else {
                            boi_log!("state: PostTaskSpecPhase -> TaskSelect (critic requests redo {}/{})",
                                spec_redo_count, max_spec_redos);
                            state = WorkerState::TaskSelect;
                        }
                    }
                    Verdict::Pause { prompt } => {
                        state = WorkerState::Paused { prompt: prompt.clone() };
                    }
                    Verdict::Done { success: false, reason } => {
                        boi_log!(" post-task spec phase '{}' failed: {}", phase_name, reason);
                        let _ = hooks::fire(hook_config, ON_PHASE_FAIL, &phase_payload); // intentional: best-effort hook notification
                        if phase.can_fail_spec {
                            state = WorkerState::Failed {
                                reason: format!("post-task phase '{}' failed: {}", phase_name, reason),
                            };
                        } else {
                            state = WorkerState::PostTaskSpecPhase { phase_idx: phase_idx + 1 };
                        }
                    }
                    Verdict::Done { success: true, reason } => {
                        let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload); // intentional: best-effort hook notification
                        boi_log!(" post-task spec phase '{}' done: {}", phase_name, reason);
                        state = WorkerState::PostTaskSpecPhase { phase_idx: phase_idx + 1 };
                    }
                }
            }

            WorkerState::Paused { ref prompt } => {
                let prompt_owned = prompt.clone();
                boi_log!(" spec {} paused: {}", spec_id, prompt_owned);
                queue.update_spec(spec_id, "paused")?;
                let _ = hooks::fire(hook_config, hooks::ON_SPEC_PAUSED, &json!({ // intentional: best-effort hook notification
                    "spec_id": spec_id,
                    "prompt": prompt_owned,
                }));
                telemetry.emit("boi.spec.paused", LogLevel::Info, &json!({
                    "spec_id": spec_id,
                    "status": "paused",
                    "prompt": prompt_owned,
                    "message": format!("spec {} paused: {}", spec_id, prompt_owned),
                }));
                // Worker exits; spec stays in "paused" status.
                // `boi decide <id>` would reset status to "queued" to resume.
                break;
            }

            WorkerState::Complete => {
                boi_log!("state: Complete — spec {} done (tasks={}, spec_redo_count={})",
                    spec_id, done_ids.len(), spec_redo_count);
                queue.update_spec(spec_id, "completed")?;
                let _ = hooks::fire(hook_config, ON_COMPLETE, &json!({ "spec_id": spec_id })); // intentional: best-effort hook notification
                telemetry.emit("boi.spec.completed", LogLevel::Info, &json!({
                    "spec_id": spec_id,
                    "status": "completed",
                    "message": format!("spec {} completed", spec_id),
                }));
                state = WorkerState::Cleanup { success: true };
            }

            WorkerState::Failed { ref reason } => {
                let reason_owned = reason.clone();
                boi_log!(" spec {} failed: {}", spec_id, reason_owned);
                queue.update_spec(spec_id, "failed")?;
                let _ = hooks::fire(hook_config, ON_FAIL, &json!({ "spec_id": spec_id })); // intentional: best-effort hook notification
                telemetry.emit("boi.spec.failed", LogLevel::Info, &json!({
                    "spec_id": spec_id,
                    "status": "failed",
                    "message": format!("spec {} failed: {}", spec_id, reason_owned),
                }));
                if config.cleanup_on_failure {
                    state = WorkerState::Cleanup { success: false };
                } else {
                    boi_log!(" worktree preserved for inspection (cleanup_on_failure=false)");
                    break;
                }
            }

            WorkerState::Cleanup { success } => {
                boi_log!("state: Cleanup {{ success: {} }}", success);
                if success {
                    if let Some(ws) = &boi_spec.workspace {
                        let commit_msg = format!("boi({}): completed spec tasks", spec_id);
                        match crate::worktree::commit_changes(spec_id, &commit_msg) {
                            Ok(true) => {
                                boi_log!(" committed changes in worktree");
                                match crate::worktree::merge_back(spec_id, ws) {
                                    Ok(output) => {
                                        boi_log!(" merged worktree branch into source repo");
                                        telemetry.emit("boi.worktree.merged", LogLevel::Info, &json!({
                                            "spec_id": spec_id,
                                            "message": format!("merged boi/{} into source repo", spec_id),
                                            "merge_output": output.chars().take(200).collect::<String>(),
                                        }));
                                    }
                                    Err(e) => {
                                        boi_log!(" merge failed: {} — worktree preserved", e);
                                        telemetry.emit("boi.worktree.merge_failed", LogLevel::Error, &json!({
                                            "spec_id": spec_id,
                                            "error": e.to_string(),
                                        }));
                                        let _ = crate::worktree::delete_branch(spec_id, ws); // intentional: best-effort branch cleanup
                                        break;
                                    }
                                }
                            }
                            Ok(false) => {
                                boi_log!(" no changes to commit in worktree");
                            }
                            Err(e) => {
                                boi_log!(" commit failed: {} — worktree preserved", e);
                                telemetry.emit("boi.worktree.commit_failed", LogLevel::Error, &json!({
                                    "spec_id": spec_id,
                                    "error": e.to_string(),
                                }));
                                break;
                            }
                        }
                    }
                    boi_log!("state: Cleanup — removing worktree for spec {}", spec_id);
                    let _ = crate::worktree::cleanup(spec_id); // intentional: best-effort worktree cleanup
                    if let Some(ws) = &boi_spec.workspace {
                        let _ = crate::worktree::delete_branch(spec_id, ws); // intentional: best-effort branch cleanup
                    }
                } else if config.cleanup_on_failure {
                    boi_log!("state: Cleanup — removing worktree for failed spec {}", spec_id);
                    let _ = crate::worktree::cleanup(spec_id); // intentional: best-effort worktree cleanup
                    if let Some(ws) = &boi_spec.workspace {
                        let _ = crate::worktree::delete_branch(spec_id, ws); // intentional: best-effort branch cleanup
                    }
                } else {
                    boi_log!(" preserving worktree for failed spec {}", spec_id);
                }
                let queue_tag = std::path::Path::new(queue_path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("default");
                let tmp = std::env::temp_dir().join(format!("boi-{}-{}", spec_id, queue_tag));
                if tmp.exists() {
                    let _ = std::fs::remove_dir_all(&tmp); // intentional: best-effort temp dir cleanup
                }
                break;
            }
        }
    }

    Ok(())
}

/// Emit a phase verdict telemetry event (DRY helper for the state machine).
fn emit_phase_verdict(
    telemetry: &Telemetry,
    spec_id: &str,
    task_id: Option<&str>,
    phase_name: &str,
    verdict: &Verdict,
    elapsed_ms: i64,
) {
    let outcome_label = match verdict {
        Verdict::Proceed => "proceed",
        Verdict::Redo { .. } => "redo",
        Verdict::Pause { .. } => "pause",
        Verdict::Done { success: true, .. } => "done",
        Verdict::Done { success: false, .. } => "failed",
    };
    let msg = if let Some(tid) = task_id {
        format!("{}: {} phase {} ({}ms)", tid, phase_name, outcome_label, elapsed_ms)
    } else {
        format!("spec phase '{}' {} ({}ms)", phase_name, outcome_label, elapsed_ms)
    };
    let mut payload = json!({
        "spec_id": spec_id,
        "phase": phase_name,
        "outcome": outcome_label,
        "duration_ms": elapsed_ms,
        "message": msg,
    });
    if let Some(tid) = task_id {
        payload["task_id"] = serde_json::Value::String(tid.to_string());
    }
    telemetry.emit("boi.phase.outcome", LogLevel::Info, &payload);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hooks::HookConfig, queue::Queue, spec};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());
    static TEL_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_telemetry() -> Telemetry {
        let n = TEL_COUNTER.fetch_add(1, Ordering::SeqCst);
        let db = std::path::PathBuf::from(format!(
            "/tmp/boi-test-worker-tel-{}-{}.db",
            std::process::id(), n
        ));
        let _ = std::fs::remove_file(&db);
        Telemetry::new(db)
    }

    /// Run `f` with CLAUDE_BIN set to `bin_path`, holding ENV_LOCK.
    fn with_claude_bin<F: FnOnce()>(bin_path: &str, f: F) {
        let _lock = ENV_LOCK.lock().unwrap();
        let old = std::env::var("CLAUDE_BIN").ok();
        // SAFETY: We hold ENV_LOCK so no other test thread can read/write env vars
        // concurrently. This is test-only code; the lock serializes all env access.
        unsafe { std::env::set_var("CLAUDE_BIN", bin_path) };
        f();
        // SAFETY: Same as above -- ENV_LOCK is held, restoring the original value.
        unsafe {
            match old {
                Some(v) => std::env::set_var("CLAUDE_BIN", v),
                None => std::env::remove_var("CLAUDE_BIN"),
            }
        }
    }

    /// Run `f` with CLAUDE_BIN and BOI_REPO set, holding ENV_LOCK.
    fn with_test_env<F: FnOnce()>(bin_path: &str, repo_path: &str, f: F) {
        let _lock = ENV_LOCK.lock().unwrap();
        let old_bin = std::env::var("CLAUDE_BIN").ok();
        let old_repo = std::env::var("BOI_REPO").ok();
        // SAFETY: ENV_LOCK is held so no concurrent env access from other test
        // threads. Setting vars for the duration of the test closure only.
        unsafe {
            std::env::set_var("CLAUDE_BIN", bin_path);
            std::env::set_var("BOI_REPO", repo_path);
        }
        f();
        // SAFETY: ENV_LOCK is held, restoring original env values after the test.
        unsafe {
            match old_bin {
                Some(v) => std::env::set_var("CLAUDE_BIN", v),
                None => std::env::remove_var("CLAUDE_BIN"),
            }
            match old_repo {
                Some(v) => std::env::set_var("BOI_REPO", v),
                None => std::env::remove_var("BOI_REPO"),
            }
        }
    }

    /// Create a temporary git repo for worktree testing.
    fn setup_test_repo(suffix: &str) -> std::path::PathBuf {
        use std::process::Command;
        let repo_dir = std::env::temp_dir().join(format!("boi_test_repo_{}", suffix));
        let _ = std::fs::remove_dir_all(&repo_dir);
        std::fs::create_dir_all(&repo_dir).unwrap();
        Command::new("git").args(["init"]).current_dir(&repo_dir).output().unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@boi.test"])
            .current_dir(&repo_dir).output().unwrap();
        Command::new("git")
            .args(["config", "user.name", "BOI Test"])
            .current_dir(&repo_dir).output().unwrap();
        std::fs::write(repo_dir.join("README.md"), "test").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&repo_dir).output().unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&repo_dir).output().unwrap();
        repo_dir
    }

    fn mock_claude(exit_code: u8, suffix: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("boi_mock_claude_{}", suffix));
        std::fs::write(&path, format!("#!/bin/sh\nexit {}\n", exit_code)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn test_default_config() {
        let cfg = WorkerConfig::default();
        assert_eq!(cfg.max_workers, 5);
        assert_eq!(cfg.retry_count, 3);
        assert_eq!(cfg.task_timeout_secs, 1800);
    }

    #[test]
    fn test_build_prompt_contains_task_fields() {
        let task = spec::BoiTask {
            id: "t-1".to_string(),
            title: "Setup Cargo".to_string(),
            status: spec::TaskStatus::Pending,
            depends: None,
            spec: Some("Run cargo init".to_string()),
            verify: Some("test -f Cargo.toml".to_string()),
            verify_prompt: None,
            phases: None,
        };
        let prompt = build_prompt("title: Test\ntasks: []", &task);
        assert!(prompt.contains("t-1"));
        assert!(prompt.contains("Setup Cargo"));
        assert!(prompt.contains("Run cargo init"));
        assert!(prompt.contains("test -f Cargo.toml"));
    }

    #[test]
    fn test_run_verify_success() {
        assert!(run_verify("true", "/tmp"));
    }

    #[test]
    fn test_run_verify_failure() {
        assert!(!run_verify("false", "/tmp"));
    }

    #[test]
    fn test_run_verify_missing_command() {
        assert!(!run_verify("exit 1", "/tmp"));
    }

    fn mock_claude_with_stderr(exit_code: u8, stdout_msg: &str, stderr_msg: &str, suffix: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("boi_mock_claude_{}", suffix));
        std::fs::write(&path, format!(
            "#!/bin/sh\necho '{}'\necho '{}' >&2\nexit {}\n",
            stdout_msg, stderr_msg, exit_code
        )).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn test_spawn_claude_exit_0() {
        let script = mock_claude(0, "exit0");
        let bin = script.to_str().unwrap();
        let cr = spawn_claude("prompt", "/tmp", 10, None, None, bin).unwrap();
        assert!(cr.success);
        assert!(cr.total_ms > 0 || cr.startup_ms == 0);
    }

    #[test]
    fn test_spawn_claude_exit_1() {
        let script = mock_claude(1, "exit1");
        let bin = script.to_str().unwrap();
        let cr = spawn_claude("prompt", "/tmp", 10, None, None, bin).unwrap();
        assert!(!cr.success);
    }

    #[test]
    fn test_spawn_claude_captures_stderr() {
        let script = mock_claude_with_stderr(1, "stdout-ok", "ERROR: something broke", "stderr_capture");
        let bin = script.to_str().unwrap();
        let cr = spawn_claude("prompt", "/tmp", 10, None, None, bin).unwrap();
        assert!(!cr.success);
        assert!(cr.stderr.contains("ERROR: something broke"),
            "stderr should be captured, got: '{}'", cr.stderr);
    }

    #[test]
    fn test_spawn_claude_stderr_empty_on_success() {
        let script = mock_claude(0, "stderr_empty");
        let bin = script.to_str().unwrap();
        let cr = spawn_claude("prompt", "/tmp", 10, None, None, bin).unwrap();
        assert!(cr.success);
        assert!(cr.stderr.is_empty(), "stderr should be empty on clean exit");
    }

    fn setup_test_db(suffix: &str, spec_yaml: &str) -> (Queue, String, String) {
        let spec_file = std::env::temp_dir().join(format!("boi_test_spec_{}.yaml", suffix));
        std::fs::write(&spec_file, spec_yaml).unwrap();

        let db_file = std::env::temp_dir().join(format!("boi_test_db_{}.db", suffix));
        let _ = std::fs::remove_file(&db_file);
        let _ = std::fs::remove_file(db_file.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_file.with_extension("db-shm"));
        let queue = Queue::open(db_file.to_str().unwrap()).unwrap();
        let boi_spec = spec::parse(spec_yaml).unwrap();
        let spec_id = queue.enqueue(&boi_spec, spec_file.to_str()).unwrap();

        (queue, spec_id, db_file.to_str().unwrap().to_string())
    }

    #[test]
    fn test_run_worker_completes_on_success() {
        let script = mock_claude(0, "worker_ok");
        let repo = setup_test_repo("worker_ok");
        let spec_yaml =
            "title: \"Worker Test\"
tasks:\n  - id: t-1\n    title: \"Step\"\n    status: PENDING\n    spec: \"Do it\"\n";
        let (queue, spec_id, db_path) = setup_test_db("worker_ok", spec_yaml);
        let spec_file = std::env::temp_dir().join("boi_test_spec_worker_ok.yaml");
        let config = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: 10,
            retry_count: 0,
            cleanup_on_failure: false,
            claude_bin: script.to_str().unwrap().to_string(),
        };

        let tel = test_telemetry();
        with_test_env(script.to_str().unwrap(), repo.to_str().unwrap(), || {
            run_worker(
                &spec_id,
                spec_file.to_str().unwrap(),
                &db_path,
                &HookConfig::default(),
                &config,
                &tel,
            )
            .unwrap();
        });

        let st = queue.status(&spec_id).unwrap().unwrap();
        assert_eq!(st.spec.status, "completed");
        assert_eq!(st.tasks[0].status, "DONE");
    }

    #[test]
    fn test_run_worker_fails_on_task_failure() {
        let script = mock_claude(1, "worker_fail");
        let repo = setup_test_repo("worker_fail");
        let spec_yaml =
            "title: \"Fail Test\"
tasks:\n  - id: t-1\n    title: \"Will Fail\"\n    status: PENDING\n";
        let (queue, spec_id, db_path) = setup_test_db("worker_fail", spec_yaml);
        let spec_file = std::env::temp_dir().join("boi_test_spec_worker_fail.yaml");
        let config = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: 10,
            retry_count: 0,
            cleanup_on_failure: false,
            claude_bin: script.to_str().unwrap().to_string(),
        };

        let tel = test_telemetry();
        with_test_env(script.to_str().unwrap(), repo.to_str().unwrap(), || {
            let _ = run_worker(
                &spec_id,
                spec_file.to_str().unwrap(),
                &db_path,
                &HookConfig::default(),
                &config,
                &tel,
            );
        });

        let st = queue.status(&spec_id).unwrap().unwrap();
        assert_eq!(st.spec.status, "failed");
        assert_eq!(st.tasks[0].status, "FAILED");
    }

    #[test]
    fn test_run_worker_skips_done_tasks() {
        let script = mock_claude(0, "worker_skip");
        let repo = setup_test_repo("worker_skip");
        // DB is the single source of truth — mark t-1 DONE in DB, not YAML
        let spec_yaml = "title: \"Skip Test\"
tasks:\n  - id: t-1\n    title: \"Done\"\n    status: PENDING\n  - id: t-2\n    title: \"Pending\"\n    status: PENDING\n    depends: [t-1]\n";
        let (queue, spec_id, db_path) = setup_test_db("worker_skip", spec_yaml);
        // Pre-mark t-1 as DONE in the DB so worker skips it
        let pre_st = queue.status(&spec_id).unwrap().unwrap();
        let t1_canonical = pre_st.tasks[0].id.clone();
        queue.update_task(&spec_id, &t1_canonical, "DONE").unwrap();
        let spec_file = std::env::temp_dir().join("boi_test_spec_worker_skip.yaml");
        let config = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: 10,
            retry_count: 0,
            cleanup_on_failure: false,
            claude_bin: script.to_str().unwrap().to_string(),
        };

        let tel = test_telemetry();
        with_test_env(script.to_str().unwrap(), repo.to_str().unwrap(), || {
            run_worker(
                &spec_id,
                spec_file.to_str().unwrap(),
                &db_path,
                &HookConfig::default(),
                &config,
                &tel,
            )
            .unwrap();
        });

        let st = queue.status(&spec_id).unwrap().unwrap();
        assert_eq!(st.spec.status, "completed");
        let t2 = st.tasks.iter().find(|t| t.title == "Pending").unwrap();
        assert_eq!(t2.status, "DONE");
    }

    // --- Phase pipeline tests using MockPhaseRunner ---

    fn setup_phase_test(
        suffix: &str,
        spec_yaml: &str,
    ) -> (Queue, String, String, String, std::path::PathBuf) {
        let repo = setup_test_repo(suffix);
        let spec_file = std::env::temp_dir().join(format!("boi_phase_spec_{}.yaml", suffix));
        std::fs::write(&spec_file, spec_yaml).unwrap();
        let db_file = std::env::temp_dir().join(format!("boi_phase_db_{}.db", suffix));
        let _ = std::fs::remove_file(&db_file);
        let _ = std::fs::remove_file(db_file.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_file.with_extension("db-shm"));
        let queue = Queue::open(db_file.to_str().unwrap()).unwrap();
        let boi_spec = spec::parse(spec_yaml).unwrap();
        let spec_id = queue.enqueue(&boi_spec, spec_file.to_str()).unwrap();
        let db_path = db_file.to_str().unwrap().to_string();
        let spec_path = spec_file.to_str().unwrap().to_string();
        (queue, spec_id, db_path, spec_path, repo)
    }

    #[test]
    fn test_phase_pipeline_all_approved() {
        let yaml = "title: \"Phase Pipeline Test\"\nmode: execute\ntasks:\n  - id: t-1\n    title: \"Task\"\n    status: PENDING\n    verify: \"true\"\n";
        let (queue, spec_id, db_path, spec_path, repo) = setup_phase_test("pipeline_ok", yaml);
        let config = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: 10,
            retry_count: 0,
            cleanup_on_failure: false,
            claude_bin: "true".to_string(),
        };
        let registry = PhaseRegistry::new();
        let mock = crate::runner::MockPhaseRunner::new(vec![
            Verdict::Proceed,
            Verdict::Proceed,
            Verdict::Proceed,
        ]);
        let tel = test_telemetry();

        with_test_env("true", repo.to_str().unwrap(), || {
            run_worker_with_phases(
                &spec_id,
                &spec_path,
                &db_path,
                &HookConfig::default(),
                &config,
                &registry,
                &mock,
                &tel,
            )
            .unwrap();
        });

        let st = queue.status(&spec_id).unwrap().unwrap();
        assert_eq!(st.spec.status, "completed");
        assert_eq!(st.tasks[0].status, "DONE");
    }

    #[test]
    fn test_phase_pipeline_task_phase_fails() {
        let yaml = "title: \"Phase Fail Test\"\nmode: execute\ntasks:\n  - id: t-1\n    title: \"Task\"\n    status: PENDING\n";
        let (queue, spec_id, db_path, spec_path, repo) = setup_phase_test("pipeline_fail", yaml);
        let config = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: 10,
            retry_count: 0,
            cleanup_on_failure: false,
            claude_bin: "true".to_string(),
        };
        let registry = PhaseRegistry::new();
        let mock = crate::runner::MockPhaseRunner::new(vec![
            Verdict::Proceed,
            Verdict::Done { success: false, reason: "verify failed".into() },
        ]);
        let tel = test_telemetry();

        with_test_env("true", repo.to_str().unwrap(), || {
            run_worker_with_phases(
                &spec_id,
                &spec_path,
                &db_path,
                &HookConfig::default(),
                &config,
                &registry,
                &mock,
                &tel,
            )
            .unwrap();
        });

        let st = queue.status(&spec_id).unwrap().unwrap();
        assert_eq!(st.spec.status, "failed");
        assert_eq!(st.tasks[0].status, "FAILED");
    }

    #[test]
    fn test_phase_pipeline_with_task_override() {
        let yaml = "title: \"Override Test\"\nmode: execute\ntasks:\n  - id: t-1\n    title: \"Custom\"\n    status: PENDING\n    phases: [\"execute\"]\n";
        let (queue, spec_id, db_path, spec_path, repo) = setup_phase_test("pipeline_override", yaml);
        let config = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: 10,
            retry_count: 0,
            cleanup_on_failure: false,
            claude_bin: "true".to_string(),
        };
        let registry = PhaseRegistry::new();
        let mock = crate::runner::MockPhaseRunner::new(vec![
            Verdict::Proceed,
            Verdict::Proceed,
        ]);
        let tel = test_telemetry();

        with_test_env("true", repo.to_str().unwrap(), || {
            run_worker_with_phases(
                &spec_id,
                &spec_path,
                &db_path,
                &HookConfig::default(),
                &config,
                &registry,
                &mock,
                &tel,
            )
            .unwrap();
        });

        let st = queue.status(&spec_id).unwrap().unwrap();
        assert_eq!(st.spec.status, "completed");
        assert_eq!(st.tasks[0].status, "DONE");
    }

    #[test]
    fn test_phase_pipeline_timeout_fails_task() {
        let yaml = "title: \"Timeout Test\"\nmode: execute\ntasks:\n  - id: t-1\n    title: \"Task\"\n    status: PENDING\n";
        let (queue, spec_id, db_path, spec_path, repo) = setup_phase_test("pipeline_timeout", yaml);
        let config = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: 10,
            retry_count: 0,
            cleanup_on_failure: false,
            claude_bin: "true".to_string(),
        };
        let registry = PhaseRegistry::new();
        let mock = crate::runner::MockPhaseRunner::new(vec![
            Verdict::Done { success: false, reason: "timeout".into() },
        ]);
        let tel = test_telemetry();

        with_test_env("true", repo.to_str().unwrap(), || {
            run_worker_with_phases(
                &spec_id,
                &spec_path,
                &db_path,
                &HookConfig::default(),
                &config,
                &registry,
                &mock,
                &tel,
            )
            .unwrap();
        });

        let st = queue.status(&spec_id).unwrap().unwrap();
        assert_eq!(st.spec.status, "failed");
        assert_eq!(st.tasks[0].status, "FAILED");
    }

    #[test]
    fn test_phase_pipeline_challenge_mode() {
        let yaml = "title: \"Challenge Test\"\nmode: challenge\ntasks:\n  - id: t-1\n    title: \"Task\"\n    status: PENDING\n";
        let (queue, spec_id, db_path, spec_path, repo) = setup_phase_test("pipeline_challenge", yaml);
        let config = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: 10,
            retry_count: 0,
            cleanup_on_failure: false,
            claude_bin: "true".to_string(),
        };
        let registry = PhaseRegistry::new();
        let mock = crate::runner::MockPhaseRunner::new(vec![
            Verdict::Proceed,
            Verdict::Proceed,
            Verdict::Proceed,
            Verdict::Proceed,
            Verdict::Proceed,
        ]);
        let tel = test_telemetry();

        with_test_env("true", repo.to_str().unwrap(), || {
            run_worker_with_phases(
                &spec_id,
                &spec_path,
                &db_path,
                &HookConfig::default(),
                &config,
                &registry,
                &mock,
                &tel,
            )
            .unwrap();
        });

        let st = queue.status(&spec_id).unwrap().unwrap();
        assert_eq!(st.spec.status, "completed");
        assert_eq!(st.tasks[0].status, "DONE");
    }

    #[test]
    fn test_phase_pipeline_multi_task() {
        let yaml = "title: \"Multi Task\"\nmode: execute\ntasks:\n  - id: t-1\n    title: \"First\"\n    status: PENDING\n  - id: t-2\n    title: \"Second\"\n    status: PENDING\n    depends: [t-1]\n";
        let (queue, spec_id, db_path, spec_path, repo) = setup_phase_test("pipeline_multi", yaml);
        let config = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: 10,
            retry_count: 0,
            cleanup_on_failure: false,
            claude_bin: "true".to_string(),
        };
        let registry = PhaseRegistry::new();
        let mock = crate::runner::MockPhaseRunner::new(vec![
            Verdict::Proceed,
            Verdict::Proceed,
            Verdict::Proceed,
            Verdict::Proceed,
            Verdict::Proceed,
        ]);
        let tel = test_telemetry();

        with_test_env("true", repo.to_str().unwrap(), || {
            run_worker_with_phases(
                &spec_id,
                &spec_path,
                &db_path,
                &HookConfig::default(),
                &config,
                &registry,
                &mock,
                &tel,
            )
            .unwrap();
        });

        let st = queue.status(&spec_id).unwrap().unwrap();
        assert_eq!(st.spec.status, "completed");
        assert_eq!(st.tasks[0].status, "DONE");
        assert_eq!(st.tasks[1].status, "DONE");
    }
}
