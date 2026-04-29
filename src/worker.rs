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
}

impl Default for WorkerConfig {
    fn default() -> Self {
        WorkerConfig {
            max_workers: 5,
            task_timeout_secs: 1800,
            retry_count: 3,
            cleanup_on_failure: false,
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
        .args(["-p", prompt, "--dangerously-skip-permissions"])
        .current_dir(worktree_path)
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

    // --- State machine ---
    let mut state = WorkerState::SpecPhase { phase_idx: 0 };

    loop {
        match state {
            WorkerState::SpecPhase { phase_idx } => {
                if phase_idx >= pre_spec_phases.len() {
                    state = WorkerState::TaskSelect;
                    continue;
                }
                let phase_name = pre_spec_phases[phase_idx];
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
                let _ = hooks::fire(hook_config, ON_PHASE_START, &phase_payload);

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
                );
                let elapsed_ms = phase_start.elapsed().as_millis() as i64;
                record_phase_run(&queue, spec_id, None, phase_name, "spec", &verdict, &phase_started_at, elapsed_ms);

                emit_phase_verdict(telemetry, spec_id, None, phase_name, &verdict, elapsed_ms);

                match &verdict {
                    Verdict::Proceed => {
                        let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload);
                        state = WorkerState::SpecPhase { phase_idx: phase_idx + 1 };
                    }
                    Verdict::Redo { tasks } => {
                        let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload);
                        // Inject tasks if any, then go to TaskSelect
                        if !tasks.is_empty() {
                            for t in tasks {
                                let _ = queue.add_task(
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
                        eprintln!("[boi] pre-task spec phase '{}' failed: {}", phase_name, reason);
                        let _ = hooks::fire(hook_config, ON_PHASE_FAIL, &phase_payload);
                        if phase.can_fail_spec {
                            state = WorkerState::Failed {
                                reason: format!("pre-task phase '{}' failed: {}", phase_name, reason),
                            };
                        } else {
                            state = WorkerState::SpecPhase { phase_idx: phase_idx + 1 };
                        }
                    }
                    Verdict::Done { success: true, reason } => {
                        let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload);
                        eprintln!("[boi] pre-task spec phase '{}' done: {}", phase_name, reason);
                        state = WorkerState::SpecPhase { phase_idx: phase_idx + 1 };
                    }
                }
            }

            WorkerState::TaskSelect => {
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
                    if task.status != spec::TaskStatus::Pending {
                        continue;
                    }

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
                    queue.update_task(spec_id, &task.id, "RUNNING")?;
                    let _ = hooks::fire(hook_config, ON_TASK_START, &task_payload);

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
                        state = WorkerState::PostTaskSpecPhase { phase_idx: 0 };
                    } else {
                        // Some tasks are still pending but none are ready — possible deadlock
                        // or DB-level deps not yet satisfied. Re-scan up to max passes.
                        task_select_passes += 1;
                        if task_select_passes > max_task_select_passes {
                            state = WorkerState::Failed {
                                reason: "deadlock: pending tasks but none ready".to_string(),
                            };
                        } else {
                            // Re-scan (same state) — done_ids may have grown from a previous pass
                            // but if not, the pass counter will catch it
                            state = WorkerState::Failed {
                                reason: "deadlock: pending tasks but none ready".to_string(),
                            };
                        }
                    }
                }
            }

            WorkerState::TaskPhase { ref task_id, phase_idx, requeue_attempts } => {
                let task_id_owned = task_id.clone();
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

                if phase_idx >= task_phases.len() {
                    // All phases done for this task — mark DONE
                    queue.update_task(spec_id, &task.id, "DONE")?;
                    done_ids.insert(task.id.clone());
                    let task_payload = json!({
                        "spec_id": spec_id,
                        "task_id": task.id,
                        "task_title": task.title,
                    });
                    let _ = hooks::fire(hook_config, ON_TASK_COMPLETE, &task_payload);
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
                        eprintln!("[boi] unknown phase '{}' in task {} — skipping", phase_name, task.id);
                        state = WorkerState::TaskPhase {
                            task_id: task_id_owned,
                            phase_idx: phase_idx + 1,
                            requeue_attempts,
                        };
                        continue;
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

                let phase_start = Instant::now();
                let phase_started_at = Utc::now().to_rfc3339();
                let verdict = runner.run_phase(
                    phase,
                    &spec_content,
                    Some(task),
                    &worktree_path,
                    config.task_timeout_secs,
                );
                let elapsed_ms = phase_start.elapsed().as_millis() as i64;
                record_phase_run(&queue, spec_id, Some(&task.id), phase_name, "task", &verdict, &phase_started_at, elapsed_ms);

                emit_phase_verdict(telemetry, spec_id, Some(&task.id), phase_name, &verdict, elapsed_ms);

                match &verdict {
                    Verdict::Proceed => {
                        let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload);
                        state = WorkerState::TaskPhase {
                            task_id: task_id_owned,
                            phase_idx: phase_idx + 1,
                            requeue_attempts,
                        };
                    }
                    Verdict::Redo { tasks } => {
                        if tasks.is_empty() {
                            // Redo with no new tasks = requeue back to execute
                            eprintln!("[boi] phase '{}' requests redo for task {}", phase_name, task.id);
                            state = WorkerState::TaskRequeue {
                                task_id: task_id_owned,
                                target_phase: "execute".to_string(),
                                attempts: requeue_attempts + 1,
                            };
                        } else {
                            // Inject tasks and go to TaskSelect
                            let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload);
                            for t in tasks {
                                let _ = queue.add_task(
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
                        eprintln!("[boi] phase '{}' failed for task {}: {}", phase_name, task.id, reason);
                        let _ = hooks::fire(hook_config, ON_PHASE_FAIL, &phase_payload);
                        let max_attempts = phase.retry_count.unwrap_or(config.retry_count);
                        if max_attempts > 0 {
                            state = WorkerState::TaskPhaseRetry {
                                task_id: task_id_owned,
                                phase_idx,
                                attempt: 1,
                            };
                        } else {
                            // No retries — fail the task
                            queue.update_task(spec_id, &task.id, "FAILED")?;
                            let task_payload = json!({
                                "spec_id": spec_id,
                                "task_id": task.id,
                                "task_title": task.title,
                            });
                            let _ = hooks::fire(hook_config, ON_TASK_FAIL, &task_payload);
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
                        let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload);
                        eprintln!("[boi] phase '{}' done for task {}: {}", phase_name, task.id, reason);
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
                let phase = registry.get(phase_name).unwrap(); // safe: we checked in TaskPhase
                let max_attempts = phase.retry_count.unwrap_or(config.retry_count);

                if attempt >= max_attempts {
                    // Exhausted retries — fail the task
                    queue.update_task(spec_id, &task.id, "FAILED")?;
                    let task_payload = json!({
                        "spec_id": spec_id,
                        "task_id": task.id,
                        "task_title": task.title,
                    });
                    let _ = hooks::fire(hook_config, ON_TASK_FAIL, &task_payload);
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
                );
                let elapsed_ms = phase_start.elapsed().as_millis() as i64;
                record_phase_run(&queue, spec_id, Some(&task.id), phase_name, "task", &retry_verdict, &phase_started_at, elapsed_ms);

                emit_phase_verdict(telemetry, spec_id, Some(&task.id), phase_name, &retry_verdict, elapsed_ms);

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
                    eprintln!("[boi] requeue limit ({}) exceeded for task {}", config.retry_count, task_id_owned);
                    queue.update_task(spec_id, &task_id_owned, "FAILED")?;
                    let task_payload = json!({
                        "spec_id": spec_id,
                        "task_id": task_id_owned,
                        "task_title": task_title,
                    });
                    let _ = hooks::fire(hook_config, ON_TASK_FAIL, &task_payload);
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
                    state = WorkerState::Complete;
                    continue;
                }

                let phase_name = post_spec_phases[phase_idx];
                let phase = match registry.get(phase_name) {
                    Some(p) => p,
                    None => {
                        state = WorkerState::PostTaskSpecPhase { phase_idx: phase_idx + 1 };
                        continue;
                    }
                };

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
                let verdict = runner.run_phase(
                    phase,
                    &spec_content,
                    None,
                    &worktree_path,
                    config.task_timeout_secs,
                );
                let elapsed_ms = phase_start.elapsed().as_millis() as i64;
                record_phase_run(&queue, spec_id, None, phase_name, "spec", &verdict, &phase_started_at, elapsed_ms);

                emit_phase_verdict(telemetry, spec_id, None, phase_name, &verdict, elapsed_ms);

                match &verdict {
                    Verdict::Proceed => {
                        let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload);
                        state = WorkerState::PostTaskSpecPhase { phase_idx: phase_idx + 1 };
                    }
                    Verdict::Redo { tasks } => {
                        let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload);
                        // Inject tasks if any, then re-enter task loop
                        if !tasks.is_empty() {
                            for t in tasks {
                                let _ = queue.add_task(
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
                        if spec_redo_count > max_spec_redos {
                            eprintln!("[boi] spec redo limit ({}) exceeded — completing despite critic feedback", max_spec_redos);
                            state = WorkerState::Complete;
                        } else {
                            eprintln!("[boi] critic requests redo ({}/{})", spec_redo_count, max_spec_redos);
                            state = WorkerState::TaskSelect;
                        }
                    }
                    Verdict::Pause { prompt } => {
                        state = WorkerState::Paused { prompt: prompt.clone() };
                    }
                    Verdict::Done { success: false, reason } => {
                        eprintln!("[boi] post-task spec phase '{}' failed: {}", phase_name, reason);
                        let _ = hooks::fire(hook_config, ON_PHASE_FAIL, &phase_payload);
                        if phase.can_fail_spec {
                            state = WorkerState::Failed {
                                reason: format!("post-task phase '{}' failed: {}", phase_name, reason),
                            };
                        } else {
                            state = WorkerState::PostTaskSpecPhase { phase_idx: phase_idx + 1 };
                        }
                    }
                    Verdict::Done { success: true, reason } => {
                        let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload);
                        eprintln!("[boi] post-task spec phase '{}' done: {}", phase_name, reason);
                        state = WorkerState::PostTaskSpecPhase { phase_idx: phase_idx + 1 };
                    }
                }
            }

            WorkerState::Paused { ref prompt } => {
                let prompt_owned = prompt.clone();
                eprintln!("[boi] spec {} paused: {}", spec_id, prompt_owned);
                queue.update_spec(spec_id, "paused")?;
                let _ = hooks::fire(hook_config, hooks::ON_SPEC_PAUSED, &json!({
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
                queue.update_spec(spec_id, "completed")?;
                let _ = hooks::fire(hook_config, ON_COMPLETE, &json!({ "spec_id": spec_id }));
                telemetry.emit("boi.spec.completed", LogLevel::Info, &json!({
                    "spec_id": spec_id,
                    "status": "completed",
                    "message": format!("spec {} completed", spec_id),
                }));
                state = WorkerState::Cleanup { success: true };
            }

            WorkerState::Failed { ref reason } => {
                let reason_owned = reason.clone();
                eprintln!("[boi] spec {} failed: {}", spec_id, reason_owned);
                queue.update_spec(spec_id, "failed")?;
                let _ = hooks::fire(hook_config, ON_FAIL, &json!({ "spec_id": spec_id }));
                telemetry.emit("boi.spec.failed", LogLevel::Info, &json!({
                    "spec_id": spec_id,
                    "status": "failed",
                    "message": format!("spec {} failed: {}", spec_id, reason_owned),
                }));
                if config.cleanup_on_failure {
                    state = WorkerState::Cleanup { success: false };
                } else {
                    eprintln!("[boi] worktree preserved for inspection (cleanup_on_failure=false)");
                    break;
                }
            }

            WorkerState::Cleanup { success: _ } => {
                let _ = crate::worktree::cleanup(spec_id);
                let tmp = std::env::temp_dir().join(format!("boi-{}", spec_id));
                if tmp.exists() {
                    let _ = std::fs::remove_dir_all(&tmp);
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
                                    cleanup_on_failure: false,
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
            cleanup_on_failure: false,
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
            cleanup_on_failure: false,
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
            cleanup_on_failure: false,
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
