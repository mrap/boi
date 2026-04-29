use crate::{
    hooks::{
        self, HookConfig, ON_COMPLETE, ON_FAIL, ON_TASK_COMPLETE, ON_TASK_FAIL, ON_TASK_START,
        ON_WORKER_START, ON_PHASE_START, ON_PHASE_COMPLETE, ON_PHASE_FAIL,
    },
    phases::{self, PhaseLevel, PhaseRegistry, TemplateVar, Verdict},
    queue::{PhaseRunRecord, Queue},
    runner::PhaseRunner,
    spec,
    telemetry::{LogLevel, Telemetry},
    worker::WorkerConfig,
};
use chrono::Utc;
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

macro_rules! boi_log {
    ($($arg:tt)*) => {
        eprintln!("[boi {}] {}", Utc::now().format("%H:%M:%S"), format!($($arg)*))
    };
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
            let raw_deps: Vec<String> = serde_json::from_str(&dt.depends).unwrap_or_default();
            let deps: Vec<String> = raw_deps.iter()
                .map(|d| canonical_to_yaml.get(d).cloned().unwrap_or_else(|| d.clone()))
                .collect();
            boi_log!("  dep-map: yaml_id={} canonical={} raw_deps={:?} mapped_deps={:?}", yaml_id, dt.id, raw_deps, deps);
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
                boi_log!("state: TaskSelect — order={:?} done={:?} skipped={:?}", order, done_ids, skipped_ids);
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

                    let blocked_by: Vec<&String> = effective_deps.iter().filter(|d| !done_ids.contains(d.as_str())).collect();
                    if !blocked_by.is_empty() {
                        boi_log!("state: TaskSelect — {} blocked by {:?}", task_id, blocked_by);
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
