use crate::{
    hooks::{
        self, HookConfig, ON_COMPLETE, ON_FAIL, ON_TASK_COMPLETE, ON_TASK_FAIL, ON_TASK_START,
        ON_WORKER_START, ON_PHASE_START, ON_PHASE_COMPLETE, ON_PHASE_FAIL, ON_PHASE_SKIP,
    },
    phases::{self, PhaseLevel, PhaseOutcome, PhaseRegistry},
    queue::{PhaseRunRecord, Queue},
    runner::{ClaudePhaseRunner, PhaseRunner},
    spec,
    telemetry::{LogLevel, Telemetry},
};
use chrono::Utc;
use serde_json::json;
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
}

impl Default for WorkerConfig {
    fn default() -> Self {
        WorkerConfig {
            max_workers: 5,
            task_timeout_secs: 1800,
            retry_count: 3,
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
        Execute the task completely. Mark it status: DONE in the spec file when done.",
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

/// Spawn claude with the task prompt. Returns (success, stdout).
/// Respects timeout: kills the process and returns (false, "timeout") if exceeded.
/// Override the claude binary via CLAUDE_BIN env var (useful for tests).
pub fn spawn_claude(
    prompt: &str,
    worktree_path: &str,
    timeout_secs: u64,
) -> Result<(bool, String), Box<dyn std::error::Error>> {
    let claude_bin = std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());
    let mut child = Command::new(&claude_bin)
        .args(["-p", prompt, "--output-format", "json", "--dangerously-skip-permissions"])
        .env("AGENT_DIR", worktree_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let start = Instant::now();
    loop {
        match child.try_wait()? {
            Some(_) => break,
            None => {
                if start.elapsed().as_secs() >= timeout_secs {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok((false, "timeout".to_string()));
                }
                // Claude sessions run for minutes; 2s poll is responsive enough
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Ok((output.status.success(), stdout))
}

fn record_phase_run(
    queue: &Queue,
    spec_id: &str,
    task_id: Option<&str>,
    phase_name: &str,
    level: &str,
    outcome: &PhaseOutcome,
    started_at: &str,
    elapsed_ms: i64,
) {
    let outcome_str = match outcome {
        PhaseOutcome::Approved => "approved",
        PhaseOutcome::Skipped => "skipped",
        PhaseOutcome::Failed { .. } => "failed",
        PhaseOutcome::Timeout => "timeout",
        PhaseOutcome::Requeue { .. } => "requeue",
        PhaseOutcome::AddedTasks(_) => "added_tasks",
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
    let _ = queue.insert_phase_run(&rec);
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
    let runner = Arc::new(ClaudePhaseRunner::new(telemetry.clone()));
    run_worker_with_phases(spec_id, spec_path, queue_path, hook_config, config, &registry, runner.as_ref(), telemetry)
}

/// Load user phases into a registry (helper to avoid mutability issues in run_worker).
fn registry_load_user(registry: &PhaseRegistry) {
    // PhaseRegistry::new() already loads core phases. User phases need a mutable registry,
    // but we handle this by creating a new registry with user phases in run_worker_with_registry.
    let _ = registry;
}

/// Execute all pending tasks using the phase pipeline with a custom PhaseRunner.
/// This is the core implementation, testable with mock runners.
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
    let _ = hooks::fire(hook_config, ON_WORKER_START, &json!({ "spec_id": spec_id }));

    telemetry.emit("boi.worker.started", LogLevel::Info, &json!({
        "spec_id": spec_id,
        "message": format!("worker started for {}", spec_id),
    }));

    let spec_content = std::fs::read_to_string(spec_path)?;
    let boi_spec = spec::parse_unchecked(&spec_content)?;

    let worktree_path = match &boi_spec.workspace {
        Some(ws) if !ws.is_empty() => {
            let worktree_dir = crate::worktree::create(spec_id, ws)?;
            worktree_dir.to_str().unwrap_or("/tmp").to_string()
        }
        _ => {
            let tmp = std::env::temp_dir().join(format!("boi-{}", spec_id));
            std::fs::create_dir_all(&tmp)?;
            eprintln!("[boi] no workspace set — running in temp dir: {}", tmp.display());
            tmp.to_str().unwrap_or("/tmp").to_string()
        }
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

    let task_map: HashMap<&str, &spec::BoiTask> =
        boi_spec.tasks.iter().map(|t| (t.id.as_str(), t)).collect();

    // Track which tasks we've completed this run (supplements spec YAML state)
    let mut done_ids: HashSet<String> = boi_spec
        .tasks
        .iter()
        .filter(|t| t.status == spec::TaskStatus::Done)
        .map(|t| t.id.clone())
        .collect();

    // Overlay DB state: tasks may have been SKIPPED or had deps added via `boi spec` mutations
    let mut skipped_ids: HashSet<String> = HashSet::new();
    let mut db_depends: HashMap<String, Vec<String>> = HashMap::new();
    if let Ok(db_tasks) = queue.get_tasks(spec_id) {
        for dt in &db_tasks {
            if dt.status == "SKIPPED" {
                skipped_ids.insert(dt.id.clone());
                done_ids.insert(dt.id.clone());
            }
            let deps: Vec<String> = serde_json::from_str(&dt.depends).unwrap_or_default();
            if !deps.is_empty() {
                db_depends.insert(dt.id.clone(), deps);
            }
        }
    }

    // --- Pre-task spec phases (e.g., plan-critique) ---
    let pre_spec_phases: Vec<&str> = pipeline
        .spec_phases
        .iter()
        .filter_map(|name| {
            registry.get(name).and_then(|p| {
                // Pre-task spec phases are those that make sense before execution
                if p.level == PhaseLevel::Spec && (name == "plan-critique") {
                    Some(name.as_str())
                } else {
                    None
                }
            })
        })
        .collect();

    for phase_name in &pre_spec_phases {
        if let Some(phase) = registry.get(phase_name) {
            let phase_payload = json!({
                "spec_id": spec_id,
                "phase": phase_name,
                "level": "spec",
            });
            let _ = hooks::fire(hook_config, ON_PHASE_START, &phase_payload);

            telemetry.emit("boi.phase.start", LogLevel::Info, &json!({
                "spec_id": spec_id,
                "phase": phase_name,
                "level": "spec",
                "message": format!("spec phase '{}' started", phase_name),
            }));

            let phase_start = Instant::now();
            let phase_started_at = Utc::now().to_rfc3339();
            let outcome = runner.run_phase(
                phase,
                &spec_content,
                None,
                &worktree_path,
                config.task_timeout_secs,
            );
            let elapsed_ms = phase_start.elapsed().as_millis() as i64;
            record_phase_run(&queue, spec_id, None, phase_name, "spec", &outcome, &phase_started_at, elapsed_ms);

            let outcome_label = match &outcome {
                PhaseOutcome::Approved => "approved",
                PhaseOutcome::Skipped => "skipped",
                PhaseOutcome::Failed { .. } => "failed",
                PhaseOutcome::Timeout => "timeout",
                PhaseOutcome::Requeue { .. } => "requeue",
                PhaseOutcome::AddedTasks(_) => "added_tasks",
            };
            telemetry.emit("boi.phase.outcome", LogLevel::Info, &json!({
                "spec_id": spec_id,
                "phase": phase_name,
                "outcome": outcome_label,
                "duration_ms": elapsed_ms,
                "message": format!("spec phase '{}' {} ({}ms)", phase_name, outcome_label, elapsed_ms),
            }));

            match &outcome {
                PhaseOutcome::Approved => {
                    let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload);
                }
                PhaseOutcome::Skipped => {
                    let _ = hooks::fire(hook_config, ON_PHASE_SKIP, &phase_payload);
                }
                PhaseOutcome::Failed { reason } => {
                    eprintln!("[boi] pre-task spec phase '{}' failed: {}", phase_name, reason);
                    let _ = hooks::fire(hook_config, ON_PHASE_FAIL, &phase_payload);
                    if phase.can_fail_spec {
                        telemetry.emit("boi.spec.failed", LogLevel::Info, &json!({
                            "spec_id": spec_id,
                            "message": format!("spec failed: pre-task phase '{}' failed", phase_name),
                        }));
                        queue.update_spec(spec_id, "failed")?;
                        let _ = hooks::fire(hook_config, ON_FAIL, &json!({ "spec_id": spec_id }));
                        let _ = crate::worktree::cleanup(spec_id);
                        return Ok(());
                    }
                }
                PhaseOutcome::Timeout => {
                    eprintln!("[boi] pre-task spec phase '{}' timed out", phase_name);
                    let _ = hooks::fire(hook_config, ON_PHASE_FAIL, &phase_payload);
                }
                _ => {
                    let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload);
                }
            }
        }
    }

    // --- Process tasks through their phase pipelines ---
    // Retry loop: DB-level deps (from `boi spec block`) may not align with YAML topo order,
    // so we re-scan until no new tasks complete in a pass.
    let mut overall_success = true;
    let pending_count = order.len() - done_ids.len() - skipped_ids.len();

    'retry: for _pass in 0..pending_count.max(1) {
        let before = done_ids.len();

    'tasks: for task_id in &order {
        let task = match task_map.get(task_id.as_str()) {
            Some(t) => t,
            None => continue,
        };

        if done_ids.contains(task_id.as_str()) || skipped_ids.contains(task_id.as_str()) {
            continue;
        }

        if task.status != spec::TaskStatus::Pending {
            continue;
        }

        // Check DB-level deps (from `boi spec block`) merged with YAML deps
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

        let task_payload = json!({
            "spec_id": spec_id,
            "task_id": task.id,
            "task_title": task.title,
        });

        queue.update_task(spec_id, &task.id, "RUNNING")?;
        let _ = hooks::fire(hook_config, ON_TASK_START, &task_payload);

        telemetry.emit("boi.task.started", LogLevel::Info, &json!({
            "spec_id": spec_id,
            "task_id": task.id,
            "message": format!("{}: {} — started", task.id, task.title),
        }));

        // Resolve task phases (task override > pipeline default)
        let task_phases = phases::resolve_task_phases(
            &pipeline,
            task.phases.as_deref(),
        );

        let mut task_success = false;

        // Run each task phase in sequence
        'phases: for phase_name in &task_phases {
            let phase = match registry.get(phase_name) {
                Some(p) => p,
                None => {
                    eprintln!("[boi] unknown phase '{}' in task {} — skipping", phase_name, task.id);
                    continue 'phases;
                }
            };

            let phase_payload = json!({
                "spec_id": spec_id,
                "task_id": task.id,
                "phase": phase_name,
                "level": "task",
            });
            let _ = hooks::fire(hook_config, ON_PHASE_START, &phase_payload);

            telemetry.emit("boi.phase.start", LogLevel::Info, &json!({
                "spec_id": spec_id,
                "task_id": task.id,
                "phase": phase_name,
                "message": format!("{}: {} phase started", task.id, phase_name),
            }));

            // Retry loop for the phase
            let mut phase_outcome = PhaseOutcome::Failed {
                reason: "no attempts made".into(),
            };

            let phase_start = Instant::now();
            let phase_started_at = Utc::now().to_rfc3339();
            let max_attempts = phase.retry_count.unwrap_or(config.retry_count) + 1;
            for attempt in 0..max_attempts {
                phase_outcome = runner.run_phase(
                    phase,
                    &spec_content,
                    Some(task),
                    &worktree_path,
                    config.task_timeout_secs,
                );

                match &phase_outcome {
                    PhaseOutcome::Approved | PhaseOutcome::Skipped | PhaseOutcome::AddedTasks(_) => {
                        break;
                    }
                    PhaseOutcome::Failed { .. } | PhaseOutcome::Timeout => {
                        if attempt + 1 < max_attempts {
                            eprintln!(
                                "[boi] phase '{}' for task {} failed (attempt {}/{}), retrying",
                                phase_name, task.id, attempt + 1, max_attempts
                            );
                        }
                    }
                    PhaseOutcome::Requeue { .. } => {
                        break; // Requeue doesn't retry
                    }
                }
            }
            let elapsed_ms = phase_start.elapsed().as_millis() as i64;
            record_phase_run(&queue, spec_id, Some(&task.id), phase_name, "task", &phase_outcome, &phase_started_at, elapsed_ms);

            let outcome_label = match &phase_outcome {
                PhaseOutcome::Approved => "approved",
                PhaseOutcome::Skipped => "skipped",
                PhaseOutcome::Failed { .. } => "failed",
                PhaseOutcome::Timeout => "timeout",
                PhaseOutcome::Requeue { .. } => "requeue",
                PhaseOutcome::AddedTasks(_) => "added_tasks",
            };
            telemetry.emit("boi.phase.outcome", LogLevel::Info, &json!({
                "spec_id": spec_id,
                "task_id": task.id,
                "phase": phase_name,
                "outcome": outcome_label,
                "duration_ms": elapsed_ms,
                "message": format!("{}: {} phase {} ({}ms)", task.id, phase_name, outcome_label, elapsed_ms),
            }));

            // Handle the final phase outcome
            match &phase_outcome {
                PhaseOutcome::Approved => {
                    let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload);
                    // Continue to next phase
                }
                PhaseOutcome::Skipped => {
                    let _ = hooks::fire(hook_config, ON_PHASE_SKIP, &phase_payload);
                    // Continue to next phase
                }
                PhaseOutcome::AddedTasks(_) => {
                    let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload);
                    // Continue to next phase (tasks would be injected into the spec in production)
                }
                PhaseOutcome::Requeue { phase: target } => {
                    eprintln!("[boi] phase '{}' requests requeue to '{}' for task {}", phase_name, target, task.id);
                    let _ = hooks::fire(hook_config, ON_PHASE_FAIL, &phase_payload);
                    // Mark task failed — it needs re-execution
                    task_success = false;
                    break 'phases;
                }
                PhaseOutcome::Failed { reason } => {
                    eprintln!("[boi] phase '{}' failed for task {}: {}", phase_name, task.id, reason);
                    let _ = hooks::fire(hook_config, ON_PHASE_FAIL, &phase_payload);
                    task_success = false;
                    break 'phases;
                }
                PhaseOutcome::Timeout => {
                    eprintln!("[boi] phase '{}' timed out for task {}", phase_name, task.id);
                    let _ = hooks::fire(hook_config, ON_PHASE_FAIL, &phase_payload);
                    task_success = false;
                    break 'phases;
                }
            }

            // If we made it through all phases, task succeeded
            task_success = true;
        }

        if task_success {
            queue.update_task(spec_id, &task.id, "DONE")?;
            done_ids.insert(task.id.clone());
            let _ = hooks::fire(hook_config, ON_TASK_COMPLETE, &task_payload);
            telemetry.emit("boi.task.completed", LogLevel::Info, &json!({
                "spec_id": spec_id,
                "task_id": task.id,
                "status": "DONE",
                "message": format!("{} complete", task.id),
            }));
        } else {
            queue.update_task(spec_id, &task.id, "FAILED")?;
            let _ = hooks::fire(hook_config, ON_TASK_FAIL, &task_payload);
            telemetry.emit("boi.task.failed", LogLevel::Info, &json!({
                "spec_id": spec_id,
                "task_id": task.id,
                "status": "FAILED",
                "message": format!("{} failed", task.id),
            }));
            overall_success = false;
            break 'retry;
        }
    }

        if !overall_success || done_ids.len() == before {
            break 'retry;
        }
    }

    // --- Post-task spec phases (e.g., critic, evaluate) ---
    if overall_success {
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

        for phase_name in &post_spec_phases {
            if let Some(phase) = registry.get(phase_name) {
                let phase_payload = json!({
                    "spec_id": spec_id,
                    "phase": phase_name,
                    "level": "spec",
                });
                let _ = hooks::fire(hook_config, ON_PHASE_START, &phase_payload);

                telemetry.emit("boi.phase.start", LogLevel::Info, &json!({
                    "spec_id": spec_id,
                    "phase": phase_name,
                    "level": "spec",
                    "message": format!("spec phase '{}' started", phase_name),
                }));

                let phase_start = Instant::now();
                let phase_started_at = Utc::now().to_rfc3339();
                let outcome = runner.run_phase(
                    phase,
                    &spec_content,
                    None,
                    &worktree_path,
                    config.task_timeout_secs,
                );
                let elapsed_ms = phase_start.elapsed().as_millis() as i64;
                record_phase_run(&queue, spec_id, None, phase_name, "spec", &outcome, &phase_started_at, elapsed_ms);

                let outcome_label = match &outcome {
                    PhaseOutcome::Approved => "approved",
                    PhaseOutcome::Skipped => "skipped",
                    PhaseOutcome::Failed { .. } => "failed",
                    PhaseOutcome::Timeout => "timeout",
                    PhaseOutcome::Requeue { .. } => "requeue",
                    PhaseOutcome::AddedTasks(_) => "added_tasks",
                };
                telemetry.emit("boi.phase.outcome", LogLevel::Info, &json!({
                    "spec_id": spec_id,
                    "phase": phase_name,
                    "outcome": outcome_label,
                    "duration_ms": elapsed_ms,
                    "message": format!("spec phase '{}' {} ({}ms)", phase_name, outcome_label, elapsed_ms),
                }));

                match &outcome {
                    PhaseOutcome::Approved => {
                        let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload);
                    }
                    PhaseOutcome::Skipped => {
                        let _ = hooks::fire(hook_config, ON_PHASE_SKIP, &phase_payload);
                    }
                    PhaseOutcome::Failed { reason } => {
                        eprintln!("[boi] post-task spec phase '{}' failed: {}", phase_name, reason);
                        let _ = hooks::fire(hook_config, ON_PHASE_FAIL, &phase_payload);
                        if phase.can_fail_spec {
                            overall_success = false;
                            break;
                        }
                    }
                    PhaseOutcome::Timeout => {
                        eprintln!("[boi] post-task spec phase '{}' timed out", phase_name);
                        let _ = hooks::fire(hook_config, ON_PHASE_FAIL, &phase_payload);
                    }
                    PhaseOutcome::Requeue { phase: target } => {
                        eprintln!("[boi] post-task spec phase '{}' requests requeue to '{}'", phase_name, target);
                        let _ = hooks::fire(hook_config, ON_PHASE_FAIL, &phase_payload);
                        if phase.can_fail_spec {
                            overall_success = false;
                            break;
                        }
                    }
                    _ => {
                        let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload);
                    }
                }
            }
        }
    }

    if overall_success {
        queue.update_spec(spec_id, "completed")?;
        let _ = hooks::fire(hook_config, ON_COMPLETE, &json!({ "spec_id": spec_id }));
        telemetry.emit("boi.spec.completed", LogLevel::Info, &json!({
            "spec_id": spec_id,
            "status": "completed",
            "message": format!("spec {} completed", spec_id),
        }));
    } else {
        queue.update_spec(spec_id, "failed")?;
        let _ = hooks::fire(hook_config, ON_FAIL, &json!({ "spec_id": spec_id }));
        telemetry.emit("boi.spec.failed", LogLevel::Info, &json!({
            "spec_id": spec_id,
            "status": "failed",
            "message": format!("spec {} failed", spec_id),
        }));
    }

    let _ = crate::worktree::cleanup(spec_id);

    Ok(())
}

/// Poll the queue every 5 seconds and spawn workers up to `config.max_workers`.
/// Runs until the process is killed.
pub fn run_daemon(queue_path: &str, hook_config: HookConfig, config: WorkerConfig) {
    use std::sync::{Arc, Mutex};

    let active: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>> =
        Arc::new(Mutex::new(Vec::new()));
    let telemetry = Arc::new(Telemetry::new(
        std::path::PathBuf::from(queue_path).to_path_buf(),
    ));

    eprintln!("[boi daemon] started, max_workers={}", config.max_workers);

    loop {
        {
            let mut workers = active.lock().unwrap();
            workers.retain(|h| !h.is_finished());

            if workers.len() < config.max_workers as usize {
                match Queue::open(queue_path) {
                    Ok(queue) => match queue.dequeue() {
                        Ok(Some(rec)) => {
                            let spec_id = rec.id.clone();
                            let spec_path = rec.spec_path.clone().unwrap_or_default();
                            let qpath = queue_path.to_string();
                            let hc = hook_config.clone();
                            let timeout = config.task_timeout_secs;
                            let retries = config.retry_count;
                            let tel = telemetry.clone();

                            eprintln!("[boi daemon] starting worker for {}", spec_id);
                            let handle = std::thread::spawn(move || {
                                let wc = WorkerConfig {
                                    max_workers: 1,
                                    task_timeout_secs: timeout,
                                    retry_count: retries,
                                };
                                if let Err(e) =
                                    run_worker(&spec_id, &spec_path, &qpath, &hc, &wc, &tel)
                                {
                                    eprintln!(
                                        "[boi daemon] worker error for {}: {}",
                                        spec_id, e
                                    );
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

        std::thread::sleep(Duration::from_secs(5));
    }
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
        unsafe { std::env::set_var("CLAUDE_BIN", bin_path) };
        f();
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
        unsafe {
            std::env::set_var("CLAUDE_BIN", bin_path);
            std::env::set_var("BOI_REPO", repo_path);
        }
        f();
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

    #[test]
    fn test_spawn_claude_exit_0() {
        let script = mock_claude(0, "exit0");
        with_claude_bin(script.to_str().unwrap(), || {
            let (ok, _) = spawn_claude("prompt", "/tmp", 10).unwrap();
            assert!(ok);
        });
    }

    #[test]
    fn test_spawn_claude_exit_1() {
        let script = mock_claude(1, "exit1");
        with_claude_bin(script.to_str().unwrap(), || {
            let (ok, _) = spawn_claude("prompt", "/tmp", 10).unwrap();
            assert!(!ok);
        });
    }

    fn setup_test_db(suffix: &str, spec_yaml: &str) -> (Queue, String, String) {
        let spec_file = std::env::temp_dir().join(format!("boi_test_spec_{}.yaml", suffix));
        std::fs::write(&spec_file, spec_yaml).unwrap();

        let db_file = std::env::temp_dir().join(format!("boi_test_db_{}.db", suffix));
        let _ = std::fs::remove_file(&db_file);
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
        // t-1 is already DONE in YAML; only t-2 should be executed
        let spec_yaml = "title: \"Skip Test\"
tasks:\n  - id: t-1\n    title: \"Done\"\n    status: DONE\n  - id: t-2\n    title: \"Pending\"\n    status: PENDING\n    depends: [t-1]\n";
        let (queue, spec_id, db_path) = setup_test_db("worker_skip", spec_yaml);
        let spec_file = std::env::temp_dir().join("boi_test_spec_worker_skip.yaml");
        let config = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: 10,
            retry_count: 0,
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
        let t2 = st.tasks.iter().find(|t| t.id == "t-2").unwrap();
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
        };
        let registry = PhaseRegistry::new();
        let mock = crate::runner::MockPhaseRunner::new(vec![
            PhaseOutcome::Approved,
            PhaseOutcome::Approved,
            PhaseOutcome::Approved,
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
        };
        let registry = PhaseRegistry::new();
        let mock = crate::runner::MockPhaseRunner::new(vec![
            PhaseOutcome::Approved,
            PhaseOutcome::Failed { reason: "verify failed".into() },
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
        };
        let registry = PhaseRegistry::new();
        let mock = crate::runner::MockPhaseRunner::new(vec![
            PhaseOutcome::Approved,
            PhaseOutcome::Approved,
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
        };
        let registry = PhaseRegistry::new();
        let mock = crate::runner::MockPhaseRunner::new(vec![
            PhaseOutcome::Timeout,
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
        };
        let registry = PhaseRegistry::new();
        let mock = crate::runner::MockPhaseRunner::new(vec![
            PhaseOutcome::Approved,
            PhaseOutcome::Approved,
            PhaseOutcome::Approved,
            PhaseOutcome::Approved,
            PhaseOutcome::Approved,
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
        };
        let registry = PhaseRegistry::new();
        let mock = crate::runner::MockPhaseRunner::new(vec![
            PhaseOutcome::Approved,
            PhaseOutcome::Approved,
            PhaseOutcome::Approved,
            PhaseOutcome::Approved,
            PhaseOutcome::Approved,
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
