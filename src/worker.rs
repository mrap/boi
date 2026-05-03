use crate::{
    hooks::{
        self, HookConfig, ON_COMPLETE, ON_FAIL, ON_TASK_COMPLETE, ON_TASK_FAIL, ON_TASK_START,
        ON_WORKER_START, ON_PHASE_START, ON_PHASE_COMPLETE, ON_PHASE_FAIL,
    },
    phases::{self, PhaseLevel, PhaseRegistry, Verdict},
    queue::{FullTaskRecord, PhaseRunRecord, Queue},
    runner::{ClaudePhaseRunner, PhaseRunner},
    spec,
    telemetry::{LogLevel, Telemetry},
};
use chrono::Utc;
use serde_json::json;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

macro_rules! boi_log {
    ($($arg:tt)*) => {
        eprintln!("[boi {}] {}", Utc::now().format("%H:%M:%S"), format!($($arg)*))
    };
}

use std::{
    collections::{HashMap, HashSet},
    process::Command,
    sync::Arc,
    time::Instant,
};

pub use crate::spawn::{ClaudeResult, pid_dir, pid_file_for, spawn_claude};
pub use crate::prompt::build_prompt;

pub struct WorkerConfig {
    pub max_workers: u32,
    pub task_timeout_secs: u64,
    pub retry_count: u32,
    pub cleanup_on_failure: bool,
    pub claude_bin: String,
    pub models: Option<HashMap<String, String>>,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        WorkerConfig {
            max_workers: 5,
            task_timeout_secs: 1800,
            retry_count: 3,
            cleanup_on_failure: false,
            claude_bin: std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string()),
            models: None,
        }
    }
}

/// Extract JSON content from Claude output, handling markdown code fences.
fn extract_json_from_output(output: &str) -> String {
    // Try ```json fence first
    if let Some(start) = output.find("```json") {
        let after = &output[start + 7..];
        if let Some(end) = after.find("```") {
            return after[..end].trim().to_string();
        }
    }
    // Try generic ``` fence containing JSON
    if let Some(start) = output.find("```") {
        let after = &output[start + 3..];
        if let Some(end) = after.find("```") {
            let candidate = after[..end].trim();
            if candidate.starts_with('[') || candidate.starts_with('{') {
                return candidate.to_string();
            }
        }
    }
    // Fall back to first '[' for a bare JSON array
    if let Some(idx) = output.find('[') {
        return output[idx..].to_string();
    }
    output.to_string()
}

/// Parse spec-review JSON output and apply suggested changes to the DB.
/// All changes are best-effort: failures are logged but never block execution.
pub(crate) fn apply_spec_review_output(
    queue: &crate::queue::Queue,
    spec_id: &str,
    yaml_to_canonical: &HashMap<String, String>,
    output: &str,
) {
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Change {
        task_id: String,
        change_type: String,
        content: Option<String>,
        new_tasks: Option<Vec<NewTask>>,
        deps: Option<Vec<String>>,
    }

    #[derive(Deserialize)]
    struct NewTask {
        title: String,
        spec: Option<String>,
        verify: Option<String>,
        depends: Option<Vec<String>>,
    }

    #[derive(Deserialize)]
    struct ReviewOutput {
        changes: Vec<Change>,
    }

    // Try parsing directly first (bare array or wrapped object), then fall back to extraction
    let changes: Vec<Change> = if let Ok(arr) = serde_json::from_str::<Vec<Change>>(output) {
        arr
    } else if let Ok(wrapped) = serde_json::from_str::<ReviewOutput>(output) {
        wrapped.changes
    } else {
        let json_str = extract_json_from_output(output);
        if let Ok(arr) = serde_json::from_str::<Vec<Change>>(&json_str) {
            arr
        } else if let Ok(wrapped) = serde_json::from_str::<ReviewOutput>(&json_str) {
            wrapped.changes
        } else {
            boi_log!("spec-review: no valid JSON changes in output ({} chars)", output.len());
            return;
        }
    };

    boi_log!("spec-review: applying {} suggested changes", changes.len());

    for change in &changes {
        let canonical_id = yaml_to_canonical
            .get(&change.task_id)
            .map(|s| s.as_str())
            .unwrap_or(change.task_id.as_str());

        match change.change_type.as_str() {
            "rewrite_spec" => {
                if let Some(ref content) = change.content {
                    match queue.update_task_spec_content(spec_id, canonical_id, content) {
                        Ok(_) => boi_log!("spec-review: rewrote spec for {}", change.task_id),
                        Err(e) => boi_log!("spec-review: failed to rewrite spec for {}: {}", change.task_id, e),
                    }
                }
            }
            "rewrite_verify" | "add_verify" => {
                if let Some(ref content) = change.content {
                    match queue.update_task_verify_content(spec_id, canonical_id, content) {
                        Ok(_) => boi_log!("spec-review: updated verify for {} ({})", change.task_id, change.change_type),
                        Err(e) => boi_log!("spec-review: failed to update verify for {}: {}", change.task_id, e),
                    }
                }
            }
            "add_dep" => {
                if let Some(ref deps) = change.deps {
                    for dep_yaml_id in deps {
                        let dep_canonical = yaml_to_canonical
                            .get(dep_yaml_id)
                            .map(|s| s.as_str())
                            .unwrap_or(dep_yaml_id.as_str());
                        match queue.block_task(spec_id, canonical_id, dep_canonical) {
                            Ok(_) => boi_log!("spec-review: added dep {} → {} for {}", dep_yaml_id, change.task_id, change.task_id),
                            Err(e) => boi_log!("spec-review: failed to add dep for {}: {}", change.task_id, e),
                        }
                    }
                }
            }
            "split" => {
                if let Some(ref new_tasks) = change.new_tasks {
                    for nt in new_tasks {
                        let deps: Vec<String> = nt.depends.as_deref().unwrap_or(&[])
                            .iter()
                            .map(|d| yaml_to_canonical.get(d).cloned().unwrap_or_else(|| d.clone()))
                            .collect();
                        match queue.add_task(spec_id, "", &nt.title, nt.spec.as_deref(), nt.verify.as_deref(), &deps) {
                            Ok(new_id) => boi_log!("spec-review: split {} → new task {} ({})", change.task_id, new_id, nt.title),
                            Err(e) => boi_log!("spec-review: failed to add split task for {}: {}", change.task_id, e),
                        }
                    }
                }
            }
            other => {
                boi_log!("spec-review: unknown change_type '{}' for {}", other, change.task_id);
            }
        }
    }
}

pub fn run_verify(verify_cmd: &str, dir: &str) -> bool {
    run_verify_with_code(verify_cmd, dir).0
}

/// Returns (success, exit_code). exit_code is None if the process could not be spawned.
pub fn run_verify_with_code(verify_cmd: &str, dir: &str) -> (bool, Option<i64>) {
    match Command::new("sh").args(["-c", verify_cmd]).current_dir(dir).output() {
        Ok(o) => {
            let code = o.status.code().map(|c| c as i64);
            (o.status.success(), code)
        }
        Err(_) => (false, None),
    }
}

/// Apply pipeline phase overrides to a PhaseConfig (delegates to runner::apply_phase_overrides_from_map).
pub fn apply_phase_override(
    phase: &phases::PhaseConfig,
    overrides: &std::collections::HashMap<String, spec::PhaseOverride>,
    phase_name: &str,
    telemetry: &Telemetry,
    spec_id: &str,
) -> phases::PhaseConfig {
    crate::runner::apply_phase_overrides_from_map(phase, overrides, phase_name, telemetry, spec_id)
}

/// Returns the effective timeout_secs for a phase: uses phase.timeout_minutes if set by an
/// override, otherwise falls back to the global config timeout.
pub fn effective_timeout(phase: &phases::PhaseConfig, config_timeout_secs: u64) -> u64 {
    phase.timeout_minutes
        .filter(|&m| m > 0)  // guard: 0-minute values (e.g. from integer division) fall through to global default
        .map(|m| m as u64 * 60)
        .unwrap_or(config_timeout_secs)
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
    metrics: &crate::runner::PhaseMetrics,
    attempt: i64,
    pipeline_id: Option<&str>,
    loop_iteration: Option<i64>,
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
        cost_usd: metrics.cost_usd,
        input_tokens: metrics.input_tokens,
        output_tokens: metrics.output_tokens,
        started_at: started_at.to_string(),
        completed_at: Some(completed_at),
        model: metrics.model.clone(),
        runtime: metrics.runtime.clone(),
        pipeline_id: pipeline_id.map(|s| s.to_string()),
        attempt,
        failure_mode: metrics.failure_mode.clone(),
        cold_start_ms: metrics.cold_start_ms,
        inference_ms: metrics.inference_ms,
        cache_read_tokens: metrics.cache_read_tokens,
        cache_creation_tokens: metrics.cache_creation_tokens,
        tool_call_count: Some(metrics.tool_call_count),
        tool_calls_by_type: metrics.tool_calls_by_type.clone(),
        ttft_ms: metrics.ttft_ms,
        loop_iteration,
        verify_exit_code: metrics.verify_exit_code,
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
    let mut registry = PhaseRegistry::new();
    if let Some(ref models) = config.models {
        registry.apply_model_overrides(models);
    }
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

    // Load spec and task data from DB — YAML file is not read at runtime.
    let spec_rec = queue.status(spec_id)?
        .ok_or_else(|| -> Box<dyn std::error::Error> {
            format!("spec {} not found in DB", spec_id).into()
        })?
        .spec;

    let original_workspace = spec_rec.workspace.clone();

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
    // Tracks whether a builtin:cleanup phase already removed the worktree.
    // When true, the loop-level worktree-existence check is suppressed.
    let mut worktree_removed = false;

    // Load tasks from DB and rewrite workspace paths in spec/verify content.
    let mut db_tasks_full: Vec<FullTaskRecord> = queue.get_tasks_full(spec_id)?;
    if let Some(ref ws) = original_workspace {
        for t in &mut db_tasks_full {
            if let Some(ref mut s) = t.spec_content {
                *s = s.replace(ws.as_str(), &worktree_path);
            }
            if let Some(ref mut v) = t.verify_content {
                *v = v.replace(ws.as_str(), &worktree_path);
                if v.contains(ws.as_str()) {
                    boi_log!("WARNING: task {} verify still references original workspace '{}'", t.id, ws);
                }
            }
        }
    }

    // Build BoiTask objects and BoiSpec from DB data.
    let tasks: Vec<spec::BoiTask> = db_tasks_full.iter().map(|t| spec::BoiTask {
        id: t.id.clone(),
        title: t.title.clone(),
        status: match t.status.as_str() {
            "DONE" => spec::TaskStatus::Done,
            "FAILED" => spec::TaskStatus::Failed,
            "SKIPPED" => spec::TaskStatus::Skipped,
            "RUNNING" => spec::TaskStatus::Running,
            _ => spec::TaskStatus::Pending,
        },
        depends: {
            match serde_json::from_str::<Vec<String>>(&t.depends) {
                Ok(deps) => if deps.is_empty() { None } else { Some(deps) },
                Err(e) => {
                    boi_log!(" WARNING: task {} has corrupted depends JSON '{}': {} — will be caught during dep validation", t.id, t.depends, e);
                    None
                }
            }
        },
        spec: t.spec_content.clone(),
        verify: t.verify_content.clone(),
        verify_prompt: None,
        phases: None,
    }).collect();

    // Load phase_overrides from spec file if available (bench runner injects them there).
    // The DB does not store phase_overrides directly; the spec file is the source of truth.
    let spec_phase_overrides: std::collections::HashMap<String, spec::PhaseOverride> =
        if let Ok(content) = std::fs::read_to_string(spec_path) {
            spec::parse(&content)
                .map(|s| s.phase_overrides)
                .unwrap_or_default()
        } else {
            std::collections::HashMap::new()
        };

    let mut boi_spec = spec::BoiSpec {
        title: spec_rec.title.clone(),
        mode: Some(spec_rec.mode.clone()),
        workspace: original_workspace.clone(),
        initiative: None,
        context: spec_rec.context.clone(),
        outcomes: None,
        spec_phases: None,
        task_phases: None,
        context_files: None,
        phase_overrides: spec_phase_overrides,
        tasks,
    };

    // Reconstruct full spec content from DB fields for spec-level phase runners.
    let spec_content = {
        let mut s = format!("title: \"{}\"\nmode: {}\n", boi_spec.title, spec_rec.mode);
        if let Some(ctx) = &spec_rec.context {
            s.push_str("context: |\n");
            for line in ctx.lines() {
                s.push_str(&format!("  {}\n", line));
            }
        }
        s.push_str("\ntasks:\n");
        for t in &boi_spec.tasks {
            s.push_str(&format!("  - id: {}\n    title: \"{}\"\n", t.id, t.title));
            if let Some(ref spec) = t.spec {
                s.push_str("    spec: |\n");
                for line in spec.lines() {
                    s.push_str(&format!("      {}\n", line));
                }
            }
            if let Some(ref verify) = t.verify {
                s.push_str(&format!("    verify: \"{}\"\n", verify.replace('"', "\\\"")));
            }
            if let Some(ref deps) = t.depends {
                if !deps.is_empty() {
                    s.push_str(&format!("    depends: {:?}\n", deps));
                }
            }
        }
        s
    };

    let mut order = match spec::topological_sort(&boi_spec) {
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

    // Compute a short pipeline fingerprint: hash(mode + phase lists + binary version).
    // This lets telemetry attribute phase_run rows to a specific pipeline configuration.
    let pipeline_id: String = {
        let mut h = DefaultHasher::new();
        mode.hash(&mut h);
        pipeline.spec_pre_phases.hash(&mut h);
        pipeline.spec_phases.hash(&mut h);
        pipeline.spec_post_phases.hash(&mut h);
        pipeline.task_phases.hash(&mut h);
        env!("CARGO_PKG_VERSION").hash(&mut h);
        format!("{:016x}", h.finish())
    };

    // All task IDs are canonical (loaded from DB). No YAML-to-DB mapping needed.
    let mut task_map: HashMap<String, spec::BoiTask> = boi_spec
        .tasks
        .iter()
        .map(|t| (t.id.clone(), t.clone()))
        .collect();

    let mut done_ids: HashSet<String> = HashSet::new();
    let mut skipped_ids: HashSet<String> = HashSet::new();
    let mut db_depends: HashMap<String, Vec<String>> = HashMap::new();
    for dt in &db_tasks_full {
        match dt.status.as_str() {
            "DONE" => { done_ids.insert(dt.id.clone()); }
            "SKIPPED" => {
                skipped_ids.insert(dt.id.clone());
                done_ids.insert(dt.id.clone());
            }
            _ => {}
        }
        match serde_json::from_str::<Vec<String>>(&dt.depends) {
            Ok(raw_deps) => {
                if !raw_deps.is_empty() {
                    boi_log!("  dep-map: id={} deps={:?}", dt.id, raw_deps);
                    db_depends.insert(dt.id.clone(), raw_deps);
                }
            }
            Err(e) => {
                let msg = format!(
                    "task {} has corrupted depends JSON '{}': {}",
                    dt.id, dt.depends, e
                );
                boi_log!(" ERROR: {}", msg);
                let _ = queue.update_task(spec_id, &dt.id, "FAILED");
                queue.update_spec(spec_id, "failed")?;
                return Err(msg.into());
            }
        }
    }

    // Precompute phase lists.
    // v2+ modes declare explicit spec_pre_phases/spec_post_phases; legacy modes derive from spec_phases.
    let pre_spec_phases: Vec<&str> = if !pipeline.spec_pre_phases.is_empty() {
        // v2+: use the declared spec_pre_phases directly.
        pipeline.spec_pre_phases.iter()
            .filter_map(|name| registry.get(name).map(|_| name.as_str()))
            .collect()
    } else {
        // Legacy: spec-review and plan-critique run before tasks.
        pipeline.spec_phases.iter()
            .filter_map(|name| {
                registry.get(name).and_then(|p| {
                    if p.level == PhaseLevel::Spec && matches!(name.as_str(), "spec-review" | "plan-critique") {
                        Some(name.as_str())
                    } else {
                        None
                    }
                })
            })
            .collect()
    };

    let post_spec_phases: Vec<&str> = if !pipeline.spec_pre_phases.is_empty() {
        // v2+: use the declared spec_post_phases directly.
        pipeline.spec_post_phases.iter()
            .filter_map(|name| registry.get(name).map(|_| name.as_str()))
            .collect()
    } else {
        // Legacy: everything spec-level except plan-critique runs after tasks.
        pipeline.spec_phases.iter()
            .filter_map(|name| {
                registry.get(name).and_then(|p| {
                    if p.level == PhaseLevel::Spec && name != "plan-critique" {
                        Some(name.as_str())
                    } else {
                        None
                    }
                })
            })
            .collect()
    };

    // Track pass count for deadlock detection in TaskSelect
    let mut task_select_passes: usize = 0;
    let mut spec_redo_count: usize = 0;
    let max_spec_redos = config.retry_count as usize;
    // Quality loop counter: how many times plan-critique has looped back to spec-review
    let mut spec_loop_count: usize = 0;
    let mut max_task_select_passes = order.len().max(1);

    // Template variables for phase prompts
    use crate::phases::TemplateVar;
    let pending_count = order.len() - done_ids.len();
    let mut prompt_vars: HashMap<String, String> = HashMap::new();
    prompt_vars.insert(TemplateVar::QueueId.key().into(), spec_id.to_string());
    prompt_vars.insert(TemplateVar::Iteration.key().into(), "1".into());
    prompt_vars.insert(TemplateVar::PendingCount.key().into(), pending_count.to_string());
    prompt_vars.insert("SPEC_PATH".into(), spec_path.to_string());
    prompt_vars.insert("SPEC_CONTENT".into(), spec_content.clone());
    prompt_vars.insert(TemplateVar::WorkspaceHeader.key().into(),
        boi_spec.workspace.as_ref()
            .map(|_| format!("Workspace: {}\n", worktree_path))
            .unwrap_or_default());
    prompt_vars.insert(TemplateVar::SpecContext.key().into(),
        boi_spec.context.as_deref().unwrap_or("").to_string());
    // Per-task vars initialized empty; updated before each task phase
    prompt_vars.insert(TemplateVar::TaskTitle.key().into(), String::new());
    prompt_vars.insert(TemplateVar::TaskSpec.key().into(), String::new());
    prompt_vars.insert(TemplateVar::TaskVerify.key().into(), String::new());
    prompt_vars.insert(TemplateVar::TaskDepends.key().into(), String::new());
    // Populated when plan-critique loops back to spec-review with rejection feedback
    prompt_vars.insert("CRITIQUE_FEEDBACK".into(), String::new());
    // Project context injected from config.context.always_include and spec.context_files
    prompt_vars.insert("PROJECT_CONTEXT".into(),
        spec_rec.project_context.as_deref().unwrap_or("").to_string());
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
                if !worktree_removed && !std::path::Path::new(&worktree_path).exists() {
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
                let effective_phase = apply_phase_override(phase, &boi_spec.phase_overrides, phase_name, telemetry, spec_id);
                let phase_timeout_secs = effective_timeout(&effective_phase, config.task_timeout_secs);

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
                let (verdict, phase_output, metrics) = runner.run_phase_full(
                    &effective_phase,
                    &spec_content,
                    None,
                    &worktree_path,
                    phase_timeout_secs,
                    Some(spec_id),
                    &prompt_vars,
                );
                let elapsed_ms = phase_start.elapsed().as_millis() as i64;
                record_phase_run(&queue, spec_id, None, phase_name, "spec", &verdict, &phase_started_at, elapsed_ms, &metrics, 1, Some(&pipeline_id), Some((spec_loop_count as i64) + 1));

                emit_phase_verdict(telemetry, spec_id, None, phase_name, &verdict, elapsed_ms);

                // Apply spec-review JSON suggestions to the DB before task execution begins.
                // IDs are already canonical (loaded from DB), so no YAML-to-DB mapping needed.
                if phase_name == "spec-review" && matches!(&verdict, Verdict::Proceed) {
                    let identity_map: HashMap<String, String> = boi_spec.tasks.iter()
                        .map(|t| (t.id.clone(), t.id.clone()))
                        .collect();
                    apply_spec_review_output(&queue, spec_id, &identity_map, &phase_output);
                }

                match &verdict {
                    Verdict::Proceed => {
                        let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload); // intentional: best-effort hook notification
                        state = WorkerState::SpecPhase { phase_idx: phase_idx + 1 };
                    }
                    Verdict::Redo { tasks } => {
                        let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload); // intentional: best-effort hook notification
                        // Inject tasks if any
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
                            refresh_task_state(&queue, spec_id, &original_workspace, &worktree_path,
                                &mut boi_spec, &mut order, &mut task_map, &mut db_depends, &mut max_task_select_passes);
                        }
                        // Quality loop: if this phase has on_reject = "requeue:<target>" and
                        // <target> is a pre-spec phase, loop back with critique feedback
                        // rather than jumping to TaskSelect. Cap at 3 loops to prevent deadlock.
                        let requeue_target = phase.on_reject.as_deref()
                            .and_then(|a| a.strip_prefix("requeue:"))
                            .filter(|target| pre_spec_phases.contains(target));
                        if let Some(target) = requeue_target {
                            let max_spec_loops = 3usize;
                            if spec_loop_count < max_spec_loops {
                                spec_loop_count += 1;
                                let feedback = format!(
                                    "## Plan Critique Feedback (loop {})\n\n{}\n\n---\n\n",
                                    spec_loop_count, phase_output
                                );
                                prompt_vars.insert("CRITIQUE_FEEDBACK".into(), feedback);
                                let target_idx = pre_spec_phases.iter()
                                    .position(|&n| n == target)
                                    .unwrap_or(0);
                                boi_log!("quality loop: '{}' rejected → loop back to '{}' ({}/{})",
                                    phase_name, target, spec_loop_count, max_spec_loops);
                                state = WorkerState::SpecPhase { phase_idx: target_idx };
                            } else {
                                boi_log!("quality loop: max {} loops exceeded for '{}', proceeding to TaskSelect",
                                    max_spec_loops, phase_name);
                                state = WorkerState::TaskSelect;
                            }
                        } else {
                            state = WorkerState::TaskSelect;
                        }
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
                    let db_task_id = task_id.as_str();
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
                let db_task_id = task_id_owned.clone();
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
                let effective_phase = apply_phase_override(phase, &boi_spec.phase_overrides, phase_name, telemetry, spec_id);
                let phase_timeout_secs = effective_timeout(&effective_phase, config.task_timeout_secs);

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

                // Update per-task template vars from DB-stored task fields
                prompt_vars.insert(TemplateVar::TaskTitle.key().into(), task.title.clone());
                prompt_vars.insert(TemplateVar::TaskSpec.key().into(),
                    task.spec.as_deref().unwrap_or("").to_string());
                prompt_vars.insert(TemplateVar::TaskVerify.key().into(),
                    task.verify.as_deref().unwrap_or("").to_string());
                prompt_vars.insert(TemplateVar::TaskDepends.key().into(),
                    task.depends.as_ref().map(|d| d.join(", ")).unwrap_or_default());

                let phase_start = Instant::now();
                let phase_started_at = Utc::now().to_rfc3339();
                let (verdict, _output, metrics) = runner.run_phase_full(
                    &effective_phase,
                    &spec_content,
                    Some(task),
                    &worktree_path,
                    phase_timeout_secs,
                    Some(spec_id),
                    &prompt_vars,
                );
                let elapsed_ms = phase_start.elapsed().as_millis() as i64;
                record_phase_run(&queue, spec_id, Some(&task.id), phase_name, "task", &verdict, &phase_started_at, elapsed_ms, &metrics, 1, Some(&pipeline_id), Some(1));

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
                            refresh_task_state(&queue, spec_id, &original_workspace, &worktree_path,
                                &mut boi_spec, &mut order, &mut task_map, &mut db_depends, &mut max_task_select_passes);
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
                let db_task_id = task_id_owned.clone();
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
                let effective_phase = apply_phase_override(phase, &boi_spec.phase_overrides, phase_name, telemetry, spec_id);
                let phase_timeout_secs = effective_timeout(&effective_phase, config.task_timeout_secs);
                let max_attempts = effective_phase.retry_count.unwrap_or(config.retry_count);

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

                // Update per-task template vars for retry
                prompt_vars.insert(TemplateVar::TaskTitle.key().into(), task.title.clone());
                prompt_vars.insert(TemplateVar::TaskSpec.key().into(),
                    task.spec.as_deref().unwrap_or("").to_string());
                prompt_vars.insert(TemplateVar::TaskVerify.key().into(),
                    task.verify.as_deref().unwrap_or("").to_string());
                prompt_vars.insert(TemplateVar::TaskDepends.key().into(),
                    task.depends.as_ref().map(|d| d.join(", ")).unwrap_or_default());

                let phase_start = Instant::now();
                let phase_started_at = Utc::now().to_rfc3339();
                let (retry_verdict, _output, retry_metrics) = runner.run_phase_full(
                    &effective_phase,
                    &spec_content,
                    Some(task),
                    &worktree_path,
                    phase_timeout_secs,
                    Some(spec_id),
                    &prompt_vars,
                );
                let elapsed_ms = phase_start.elapsed().as_millis() as i64;
                record_phase_run(&queue, spec_id, Some(&task.id), phase_name, "task", &retry_verdict, &phase_started_at, elapsed_ms, &retry_metrics, attempt as i64, Some(&pipeline_id), Some(attempt as i64 + 1));

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
                    let db_task_id_rq = task_id_owned.clone();
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
                let effective_phase = apply_phase_override(phase, &boi_spec.phase_overrides, phase_name, telemetry, spec_id);
                let phase_timeout_secs = effective_timeout(&effective_phase, config.task_timeout_secs);

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
                let (verdict, _output, metrics) = runner.run_phase_full(
                    &effective_phase,
                    &spec_content,
                    None,
                    &worktree_path,
                    phase_timeout_secs,
                    Some(spec_id),
                    &prompt_vars,
                );
                let elapsed_ms = phase_start.elapsed().as_millis() as i64;
                record_phase_run(&queue, spec_id, None, phase_name, "spec", &verdict, &phase_started_at, elapsed_ms, &metrics, 1, Some(&pipeline_id), Some((spec_redo_count as i64) + 1));

                emit_phase_verdict(telemetry, spec_id, None, phase_name, &verdict, elapsed_ms);

                match &verdict {
                    Verdict::Proceed => {
                        let _ = hooks::fire(hook_config, ON_PHASE_COMPLETE, &phase_payload); // intentional: best-effort hook notification
                        if effective_phase.completion_handler.as_deref() == Some("builtin:cleanup") {
                            worktree_removed = true;
                        }
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
                            refresh_task_state(&queue, spec_id, &original_workspace, &worktree_path,
                                &mut boi_spec, &mut order, &mut task_map, &mut db_depends, &mut max_task_select_passes);
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
                    // Only attempt commit/merge if the worktree still exists.
                    // v2 pipelines run builtin:merge+builtin:cleanup as phases, so by the time
                    // we reach this state the worktree may already be gone.
                    if let Some(ws) = &boi_spec.workspace {
                        if std::path::Path::new(&worktree_path).exists() {
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

/// Reload task state from DB after a Verdict::Redo injects new tasks.
/// Updates order, task_map, boi_spec.tasks, db_depends, and max_task_select_passes
/// so that TaskSelect can see the newly-added tasks.
#[allow(clippy::too_many_arguments)]
fn refresh_task_state(
    queue: &crate::queue::Queue,
    spec_id: &str,
    original_workspace: &Option<String>,
    worktree_path: &str,
    boi_spec: &mut spec::BoiSpec,
    order: &mut Vec<String>,
    task_map: &mut HashMap<String, spec::BoiTask>,
    db_depends: &mut HashMap<String, Vec<String>>,
    max_task_select_passes: &mut usize,
) {
    match queue.get_tasks_full(spec_id) {
        Ok(mut fresh_tasks) => {
            if let Some(ref ws) = original_workspace {
                for t in &mut fresh_tasks {
                    if let Some(ref mut s) = t.spec_content {
                        *s = s.replace(ws.as_str(), worktree_path);
                    }
                    if let Some(ref mut v) = t.verify_content {
                        *v = v.replace(ws.as_str(), worktree_path);
                    }
                }
            }
            boi_spec.tasks = fresh_tasks.iter().map(|t| spec::BoiTask {
                id: t.id.clone(),
                title: t.title.clone(),
                status: match t.status.as_str() {
                    "DONE" => spec::TaskStatus::Done,
                    "FAILED" => spec::TaskStatus::Failed,
                    "SKIPPED" => spec::TaskStatus::Skipped,
                    "RUNNING" => spec::TaskStatus::Running,
                    _ => spec::TaskStatus::Pending,
                },
                depends: {
                    match serde_json::from_str::<Vec<String>>(&t.depends) {
                        Ok(deps) => if deps.is_empty() { None } else { Some(deps) },
                        Err(_) => None,
                    }
                },
                spec: t.spec_content.clone(),
                verify: t.verify_content.clone(),
                verify_prompt: None,
                phases: None,
            }).collect();
            match spec::topological_sort(boi_spec) {
                Ok(new_order) => {
                    *order = new_order;
                    *max_task_select_passes = order.len().max(1);
                }
                Err(e) => {
                    boi_log!(" Redo refresh: topological sort failed after task injection: {}", e);
                }
            }
            *task_map = boi_spec.tasks.iter().map(|t| (t.id.clone(), t.clone())).collect();
            for dt in &fresh_tasks {
                if let Ok(raw_deps) = serde_json::from_str::<Vec<String>>(&dt.depends) {
                    if !raw_deps.is_empty() {
                        db_depends.insert(dt.id.clone(), raw_deps);
                    }
                }
            }
        }
        Err(e) => {
            boi_log!(" Redo: failed to reload tasks from DB: {}", e);
        }
    }
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
    use std::sync::Mutex;

    use crate::test_utils;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn test_telemetry() -> Telemetry {
        let db = test_utils::test_file("worker-tel", "db");
        let _ = std::fs::remove_file(&db);
        Telemetry::new(db)
    }

    /// Run `f` with CLAUDE_BIN and BOI_REPO set, holding ENV_LOCK.
    fn with_test_env<F: FnOnce()>(bin_path: &str, repo_path: &str, f: F) {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

    fn setup_test_repo(label: &str) -> std::path::PathBuf {
        test_utils::test_git_repo(label)
    }

    fn mock_claude(exit_code: u8, label: &str) -> std::path::PathBuf {
        test_utils::mock_claude_script(exit_code, label)
    }

    #[test]
    fn test_default_config() {
        let cfg = WorkerConfig::default();
        assert_eq!(cfg.max_workers, 5);
        assert_eq!(cfg.retry_count, 3);
        assert_eq!(cfg.task_timeout_secs, 1800);
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

    fn mock_claude_with_stderr(exit_code: u8, stdout_msg: &str, stderr_msg: &str, label: &str) -> std::path::PathBuf {
        test_utils::mock_claude_script_with_output(exit_code, stdout_msg, stderr_msg, label)
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

    fn setup_test_db(label: &str, spec_yaml: &str) -> (Queue, String, String, String) {
        let spec_file = test_utils::test_file(label, "yaml");
        std::fs::write(&spec_file, spec_yaml).unwrap();

        let db_file = test_utils::test_file(label, "db");
        let _ = std::fs::remove_file(&db_file);
        let _ = std::fs::remove_file(db_file.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_file.with_extension("db-shm"));
        let queue = Queue::open(db_file.to_str().unwrap()).unwrap();
        let boi_spec = spec::parse(spec_yaml).unwrap();
        let spec_id = queue.enqueue(&boi_spec, spec_file.to_str()).unwrap();

        (queue, spec_id, db_file.to_str().unwrap().to_string(), spec_file.to_str().unwrap().to_string())
    }

    #[test]
    fn test_run_worker_completes_on_success() {
        let script = mock_claude(0, "worker_ok");
        let repo = setup_test_repo("worker_ok");
        let spec_yaml =
            "title: \"Worker Test\"
tasks:\n  - id: t-1\n    title: \"Step\"\n    status: PENDING\n    spec: \"Do it\"\n";
        let (queue, spec_id, db_path, spec_path) = setup_test_db("worker_ok", spec_yaml);
        let config = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: 10,
            retry_count: 0,
            cleanup_on_failure: false,
            claude_bin: script.to_str().unwrap().to_string(),
            models: None,
        };

        let tel = test_telemetry();
        with_test_env(script.to_str().unwrap(), repo.to_str().unwrap(), || {
            run_worker(
                &spec_id,
                &spec_path,
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
        let (queue, spec_id, db_path, spec_path) = setup_test_db("worker_fail", spec_yaml);
        let config = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: 10,
            retry_count: 0,
            cleanup_on_failure: false,
            claude_bin: script.to_str().unwrap().to_string(),
            models: None,
        };

        let tel = test_telemetry();
        with_test_env(script.to_str().unwrap(), repo.to_str().unwrap(), || {
            let _ = run_worker(
                &spec_id,
                &spec_path,
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
        let (queue, spec_id, db_path, spec_path) = setup_test_db("worker_skip", spec_yaml);
        // Pre-mark t-1 as DONE in the DB so worker skips it
        let pre_st = queue.status(&spec_id).unwrap().unwrap();
        let t1_canonical = pre_st.tasks[0].id.clone();
        queue.update_task(&spec_id, &t1_canonical, "DONE").unwrap();
        let config = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: 10,
            retry_count: 0,
            cleanup_on_failure: false,
            claude_bin: script.to_str().unwrap().to_string(),
            models: None,
        };

        let tel = test_telemetry();
        with_test_env(script.to_str().unwrap(), repo.to_str().unwrap(), || {
            run_worker(
                &spec_id,
                &spec_path,
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
        label: &str,
        spec_yaml: &str,
    ) -> (Queue, String, String, String, std::path::PathBuf) {
        let repo = setup_test_repo(label);
        let spec_file = test_utils::test_file(label, "yaml");
        std::fs::write(&spec_file, spec_yaml).unwrap();
        let db_file = test_utils::test_file(label, "db");
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
            models: None,
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
            models: None,
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
            models: None,
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
            models: None,
        };
        let registry = PhaseRegistry::new();
        // spec-review runs first (pre-spec phase), then execute times out
        let mock = crate::runner::MockPhaseRunner::new(vec![
            Verdict::Proceed, // spec-review succeeds
            Verdict::Done { success: false, reason: "timeout".into() }, // execute phase times out
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
            models: None,
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
            models: None,
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

    #[test]
    fn test_redo_tasks_are_executed() {
        // BUG M-5: When Verdict::Redo injects new tasks via queue.add_task(), the
        // in-memory `order` and `task_map` (built once at startup) are never updated.
        // New tasks are invisible to TaskSelect and never run, leaving them PENDING.
        let yaml = "title: \"Redo Tasks Test\"\nmode: execute\ntasks:\n  - id: t-1\n    title: \"Original\"\n    status: PENDING\n";
        let (queue, spec_id, db_path, spec_path, repo) = setup_phase_test("redo_tasks", yaml);
        let config = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: 10,
            retry_count: 0,
            cleanup_on_failure: false,
            claude_bin: "true".to_string(),
            models: None,
        };
        let registry = PhaseRegistry::new();
        let new_task = spec::BoiTask {
            id: "injected".into(),
            title: "Injected by Redo".into(),
            status: spec::TaskStatus::Pending,
            depends: None,
            spec: None,
            verify: None,
            verify_prompt: None,
            phases: None,
        };
        // First call (execute phase for t-1) returns Redo with a new task.
        // All subsequent calls return Proceed (MockPhaseRunner default when list exhausted).
        let mock = crate::runner::MockPhaseRunner::new(vec![
            Verdict::Redo { tasks: vec![new_task] },
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
        assert_eq!(st.tasks.len(), 2, "injected task should be added to DB");
        let injected = st.tasks.iter().find(|t| t.title == "Injected by Redo")
            .expect("injected task not found in DB");
        assert_eq!(injected.status, "DONE",
            "injected task should be DONE — was never executed (ghost task bug M-5)");
        assert_eq!(st.spec.status, "completed");
    }

    // --- apply_spec_review_output tests ---

    fn setup_review_db(label: &str) -> (Queue, String, String, String) {
        let spec_yaml = "title: \"Review Test\"\ntasks:\n  - id: t-1\n    title: \"Task One\"\n    status: PENDING\n  - id: t-2\n    title: \"Task Two\"\n    status: PENDING\n";
        setup_test_db(&format!("review_{}", label), spec_yaml)
    }

    #[test]
    fn test_apply_spec_review_rewrite_spec() {
        let (queue, spec_id, _, _) = setup_review_db("rewrite_spec");
        let st = queue.status(&spec_id).unwrap().unwrap();
        let t1_canonical = st.tasks[0].id.clone();

        let mut yaml_to_canonical = HashMap::new();
        yaml_to_canonical.insert("t-1".to_string(), t1_canonical.clone());

        let output = r#"[{"task_id":"t-1","change_type":"rewrite_spec","content":"Updated spec content"}]"#.to_string();
        apply_spec_review_output(&queue, &spec_id, &yaml_to_canonical, &output);

        // Verify the spec_content was updated
        let tasks = queue.get_tasks(&spec_id).unwrap();
        let t1 = tasks.iter().find(|t| t.id == t1_canonical).unwrap();
        // spec_content is stored but not in TaskRecord — verify via raw SQL is not accessible here.
        // We verify indirectly: the function ran without panic and other tasks are untouched.
        assert_eq!(tasks.len(), 2);
        let _ = t1; // used to confirm task exists
    }

    #[test]
    fn test_apply_spec_review_rewrite_verify() {
        let (queue, spec_id, _, _) = setup_review_db("rewrite_verify");
        let st = queue.status(&spec_id).unwrap().unwrap();
        let t1_canonical = st.tasks[0].id.clone();

        let mut yaml_to_canonical = HashMap::new();
        yaml_to_canonical.insert("t-1".to_string(), t1_canonical.clone());

        let output = r#"[{"task_id":"t-1","change_type":"rewrite_verify","content":"grep -q 'ok' output.txt"}]"#;
        apply_spec_review_output(&queue, &spec_id, &yaml_to_canonical, output);

        let tasks = queue.get_tasks(&spec_id).unwrap();
        assert_eq!(tasks.len(), 2); // no tasks added/removed
    }

    #[test]
    fn test_apply_spec_review_add_dep() {
        let (queue, spec_id, _, _) = setup_review_db("add_dep");
        let st = queue.status(&spec_id).unwrap().unwrap();
        let t1_canonical = st.tasks[0].id.clone();
        let t2_canonical = st.tasks[1].id.clone();

        let mut yaml_to_canonical = HashMap::new();
        yaml_to_canonical.insert("t-1".to_string(), t1_canonical.clone());
        yaml_to_canonical.insert("t-2".to_string(), t2_canonical.clone());

        let output = r#"[{"task_id":"t-2","change_type":"add_dep","deps":["t-1"]}]"#;
        apply_spec_review_output(&queue, &spec_id, &yaml_to_canonical, output);

        // t-2 should now depend on t-1
        let tasks = queue.get_tasks(&spec_id).unwrap();
        let t2 = tasks.iter().find(|t| t.id == t2_canonical).unwrap();
        let deps: Vec<String> = serde_json::from_str(&t2.depends).unwrap_or_default();
        assert!(deps.contains(&t1_canonical), "t-2 should depend on t-1, deps={:?}", deps);
    }

    #[test]
    fn test_apply_spec_review_split() {
        let (queue, spec_id, _, _) = setup_review_db("split");
        let st = queue.status(&spec_id).unwrap().unwrap();
        let t1_canonical = st.tasks[0].id.clone();

        let mut yaml_to_canonical = HashMap::new();
        yaml_to_canonical.insert("t-1".to_string(), t1_canonical);

        let output = r#"[{"task_id":"t-1","change_type":"split","new_tasks":[{"title":"Split Part A","spec":"Do part A","verify":"true"},{"title":"Split Part B","verify":"true"}]}]"#;
        apply_spec_review_output(&queue, &spec_id, &yaml_to_canonical, output);

        let tasks = queue.get_tasks(&spec_id).unwrap();
        // 2 original + 2 split = 4 tasks
        assert_eq!(tasks.len(), 4, "expected 4 tasks after split, got {}", tasks.len());
        let titles: Vec<&str> = tasks.iter().map(|t| t.title.as_str()).collect();
        assert!(titles.contains(&"Split Part A"));
        assert!(titles.contains(&"Split Part B"));
    }

    #[test]
    fn test_apply_spec_review_wrapped_json() {
        let (queue, spec_id, _, _) = setup_review_db("wrapped_json");
        let st = queue.status(&spec_id).unwrap().unwrap();
        let t1_canonical = st.tasks[0].id.clone();

        let mut yaml_to_canonical = HashMap::new();
        yaml_to_canonical.insert("t-1".to_string(), t1_canonical);

        // Wrapped format: {"changes": [...]}
        let output = r#"{"changes":[{"task_id":"t-1","change_type":"split","new_tasks":[{"title":"New Sub-Task","verify":"true"}]}]}"#;
        apply_spec_review_output(&queue, &spec_id, &yaml_to_canonical, output);

        let tasks = queue.get_tasks(&spec_id).unwrap();
        assert_eq!(tasks.len(), 3, "expected 3 tasks after wrapped-format split");
    }

    #[test]
    fn test_apply_spec_review_malformed_json() {
        let (queue, spec_id, _, _) = setup_review_db("malformed");
        let yaml_to_canonical = HashMap::new();
        // Should not panic on malformed output
        apply_spec_review_output(&queue, &spec_id, &yaml_to_canonical, "not json at all");
        apply_spec_review_output(&queue, &spec_id, &yaml_to_canonical, "");
        apply_spec_review_output(&queue, &spec_id, &yaml_to_canonical, "## Spec Review Complete\n\nNo changes needed.");
        let tasks = queue.get_tasks(&spec_id).unwrap();
        assert_eq!(tasks.len(), 2); // untouched
    }

    #[test]
    fn test_apply_spec_review_json_in_code_fence() {
        let (queue, spec_id, _, _) = setup_review_db("code_fence");
        let st = queue.status(&spec_id).unwrap().unwrap();
        let t1_canonical = st.tasks[0].id.clone();

        let mut yaml_to_canonical = HashMap::new();
        yaml_to_canonical.insert("t-1".to_string(), t1_canonical);

        let output = "## Spec Review Complete\n\n```json\n[{\"task_id\":\"t-1\",\"change_type\":\"split\",\"new_tasks\":[{\"title\":\"Extracted Task\",\"verify\":\"true\"}]}]\n```\n";
        apply_spec_review_output(&queue, &spec_id, &yaml_to_canonical, output);

        let tasks = queue.get_tasks(&spec_id).unwrap();
        assert_eq!(tasks.len(), 3, "expected 3 tasks after code-fence JSON split");
    }

    #[test]
    fn test_corrupted_deps() {
        // RED: With unwrap_or_default(), corrupted depends JSON is silently treated as no deps,
        // causing t-2 to run without waiting for t-1. The spec must FAIL instead.
        let yaml = concat!(
            "title: \"Corrupted Deps Test\"\n",
            "mode: execute\n",
            "tasks:\n",
            "  - id: t-1\n",
            "    title: \"First\"\n",
            "    status: PENDING\n",
            "  - id: t-2\n",
            "    title: \"Second\"\n",
            "    status: PENDING\n",
            "    depends: [t-1]\n",
        );
        let (queue, spec_id, db_path, spec_path, repo) = setup_phase_test("corrupted_deps", yaml);

        // Get canonical t-2 ID before corrupting
        let pre_st = queue.status(&spec_id).unwrap().unwrap();
        let t2_id = pre_st.tasks.iter().find(|t| t.title == "Second").unwrap().id.clone();

        // Corrupt the depends column for t-2 to invalid JSON via a direct DB connection
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "UPDATE tasks SET depends = 'NOT_JSON' WHERE id = ?1 AND spec_id = ?2",
                (t2_id.as_str(), spec_id.as_str()),
            ).unwrap();
        }

        let config = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: 10,
            retry_count: 0,
            cleanup_on_failure: false,
            claude_bin: "true".to_string(),
            models: None,
        };
        let registry = PhaseRegistry::new();
        let mock = crate::runner::MockPhaseRunner::new(vec![
            Verdict::Proceed,
            Verdict::Proceed,
            Verdict::Proceed,
            Verdict::Proceed,
        ]);
        let tel = test_telemetry();

        with_test_env("true", repo.to_str().unwrap(), || {
            let _ = run_worker_with_phases(
                &spec_id,
                &spec_path,
                &db_path,
                &HookConfig::default(),
                &config,
                &registry,
                &mock,
                &tel,
            );
        });

        let st = queue.status(&spec_id).unwrap().unwrap();
        assert_eq!(
            st.spec.status, "failed",
            "spec should fail when a task has corrupted depends JSON, got: '{}'",
            st.spec.status
        );
    }

    // --- Quality loop tests ---

    #[test]
    fn test_quality_loop_plan_critique_loops_back_to_spec_review() {
        // challenge mode: pre_spec_phases = [spec-review, plan-critique]
        // plan-critique rejects once → loops back to spec-review → approved on second pass
        let yaml = "title: \"Quality Loop Test\"\nmode: challenge\ntasks:\n  - id: t-1\n    title: \"Task\"\n    status: PENDING\n";
        let (queue, spec_id, db_path, spec_path, repo) = setup_phase_test("quality_loop", yaml);
        let config = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: 10,
            retry_count: 0,
            cleanup_on_failure: false,
            claude_bin: "true".to_string(),
            models: None,
        };
        let registry = PhaseRegistry::new();
        // Phase call order:
        //   spec-review(pre), plan-critique(pre→Redo→loop), spec-review(pre loop1),
        //   plan-critique(pre loop1), execute, task-verify, spec-review(post), critic(post→exhausted)
        let mock = crate::runner::MockPhaseRunner::new(vec![
            Verdict::Proceed,                      // spec-review (pre)
            Verdict::Redo { tasks: vec![] },       // plan-critique rejects → quality loop
            Verdict::Proceed,                      // spec-review (loop 1)
            Verdict::Proceed,                      // plan-critique (loop 1 — approved)
            Verdict::Proceed,                      // execute
            Verdict::Proceed,                      // task-verify
            Verdict::Proceed,                      // spec-review (post)
            // critic (post) → exhausted, MockPhaseRunner returns Proceed automatically
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
        // Verify all 7 verdicts were consumed, proving the quality loop ran
        let remaining = mock.verdicts.lock().unwrap().len();
        assert_eq!(remaining, 0, "all verdicts should be consumed — quality loop must have run (got {} remaining)", remaining);
    }

    #[test]
    fn test_quality_loop_max_exceeded_proceeds_to_task_select() {
        // plan-critique rejects 3 times — after 3 loops (max), proceed anyway
        let yaml = "title: \"Max Loop Test\"\nmode: challenge\ntasks:\n  - id: t-1\n    title: \"Task\"\n    status: PENDING\n";
        let (queue, spec_id, db_path, spec_path, repo) = setup_phase_test("quality_loop_max", yaml);
        let config = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: 10,
            retry_count: 0,
            cleanup_on_failure: false,
            claude_bin: "true".to_string(),
            models: None,
        };
        let registry = PhaseRegistry::new();
        // With max_spec_loops=3: loops 1,2,3 retry; on loop 3's rejection spec_loop_count=3
        // which is NOT < 3, so proceed to TaskSelect.
        // Phase call order:
        //   sr(pre), pc(rej→loop1), sr, pc(rej→loop2), sr, pc(rej→loop3), sr, pc(rej→proceed!),
        //   execute, task-verify, sr(post), critic(post→exhausted)
        let mock = crate::runner::MockPhaseRunner::new(vec![
            Verdict::Proceed,                      // spec-review (pre)
            Verdict::Redo { tasks: vec![] },       // plan-critique → loop 1
            Verdict::Proceed,                      // spec-review (loop 1)
            Verdict::Redo { tasks: vec![] },       // plan-critique → loop 2
            Verdict::Proceed,                      // spec-review (loop 2)
            Verdict::Redo { tasks: vec![] },       // plan-critique → loop 3
            Verdict::Proceed,                      // spec-review (loop 3)
            Verdict::Redo { tasks: vec![] },       // plan-critique → max exceeded, proceed anyway
            Verdict::Proceed,                      // execute
            Verdict::Proceed,                      // task-verify
            Verdict::Proceed,                      // spec-review (post)
            // critic (post) → exhausted
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
        // Spec must complete even though plan-critique never approved
        assert_eq!(st.spec.status, "completed");
        assert_eq!(st.tasks[0].status, "DONE");
    }

    #[test]
    fn test_pipeline_id_populated_in_phase_runs() {
        let yaml = "title: \"Pipeline ID Test\"\nmode: execute\ntasks:\n  - id: t-1\n    title: \"Task\"\n    status: PENDING\n";
        let (queue, spec_id, db_path, spec_path, repo) = setup_phase_test("pipeline_id_test", yaml);
        let config = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: 10,
            retry_count: 0,
            cleanup_on_failure: false,
            claude_bin: "true".to_string(),
            models: None,
        };
        let registry = PhaseRegistry::new();
        let mock = crate::runner::MockPhaseRunner::new(vec![
            Verdict::Proceed, // spec-review
            Verdict::Proceed, // execute
            Verdict::Proceed, // task-verify
            Verdict::Proceed, // post spec-review / critic
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

        // Open a direct connection to verify phase_run rows (queue.conn is private)
        let raw = rusqlite::Connection::open(&db_path).unwrap();
        let null_count: i64 = raw.query_row(
            "SELECT COUNT(*) FROM phase_runs WHERE spec_id = ?1 AND pipeline_id IS NULL",
            rusqlite::params![spec_id],
            |r| r.get::<_, i64>(0),
        ).unwrap();
        let total_count: i64 = raw.query_row(
            "SELECT COUNT(*) FROM phase_runs WHERE spec_id = ?1",
            rusqlite::params![spec_id],
            |r| r.get::<_, i64>(0),
        ).unwrap();
        drop(raw);
        assert!(total_count > 0, "phase_runs must exist after worker run");
        assert_eq!(null_count, 0, "all phase_run rows must have a non-NULL pipeline_id; {} of {} were NULL", null_count, total_count);
    }
}
