use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

/// Which runtime to use for a phase override.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PhaseRuntime {
    Claude,
    Openrouter,
    Codex,
    Deterministic,
}

/// Per-phase override: swap runtime, model, effort, or timeout for a named phase.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct PhaseOverride {
    pub runtime: Option<PhaseRuntime>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub timeout: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct BoiSpec {
    pub title: String,
    pub mode: Option<String>,
    /// Target git repo for this spec. When set, the worker creates a worktree
    /// here and merges back on completion. Either `workspace` OR
    /// `workspace_rationale` must be set — see [validate].
    pub workspace: Option<String>,
    /// Required when `workspace` is null: a non-empty rationale explaining why
    /// this spec touches no git repo. Forces every spec author to make a
    /// conscious choice rather than silently dropping into the `/tmp/` +
    /// abs-path-write pattern that stranded 49 files in the BOI main checkout
    /// (incident 2026-05-12). The string is logged in `boi log <id>`.
    #[serde(default)]
    pub workspace_rationale: Option<String>,
    pub initiative: Option<String>,
    pub context: Option<String>,
    pub outcomes: Option<Vec<Outcome>>,
    /// Override spec-level phases (replaces default_pipeline().spec_phases)
    #[serde(default)]
    pub spec_phases: Option<Vec<String>>,
    /// Override task-level phases (replaces default_pipeline().task_phases)
    #[serde(default)]
    pub task_phases: Option<Vec<String>>,
    /// Context files to inject into every worker prompt for this spec
    #[serde(default)]
    pub context_files: Option<Vec<String>>,
    /// Per-phase runtime/model overrides applied to every task in this spec
    #[serde(default)]
    pub phase_overrides: HashMap<String, PhaseOverride>,
    /// Named worker pool to use for this spec. None → use the registry default.
    /// Pool-name existence is validated at dispatch time, not here.
    #[serde(default)]
    pub worker_pool: Option<String>,
    /// Maximum total cost in USD for this spec. None → no ceiling enforced.
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
    /// Paths that must exist on disk before emitting boi.spec.completed.
    /// If any path is missing the spec transitions to Failed instead.
    #[serde(default)]
    pub key_artifacts: Option<Vec<String>>,
    pub tasks: Vec<BoiTask>,
}

/// Check that all paths listed in `spec.key_artifacts` exist on disk.
/// Returns `Ok(())` if key_artifacts is absent, null, or empty.
/// Returns `Err(missing)` with the list of missing paths if any are absent.
pub fn check_key_artifacts(spec: &BoiSpec) -> Result<(), Vec<PathBuf>> {
    let paths = match spec.key_artifacts.as_ref() {
        None => return Ok(()),
        Some(v) if v.is_empty() => return Ok(()),
        Some(v) => v,
    };
    let missing: Vec<PathBuf> = paths
        .iter()
        .map(PathBuf::from)
        .filter(|p| !p.exists())
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing)
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Outcome {
    pub description: String,
    pub verify: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub enum TaskStatus {
    #[serde(rename = "PENDING")]
    #[default]
    Pending,
    #[serde(rename = "RUNNING")]
    Running,
    #[serde(rename = "DONE")]
    Done,
    #[serde(rename = "FAILED")]
    Failed,
    #[serde(rename = "SKIPPED")]
    Skipped,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskStatus::Pending => write!(f, "PENDING"),
            TaskStatus::Running => write!(f, "RUNNING"),
            TaskStatus::Done => write!(f, "DONE"),
            TaskStatus::Failed => write!(f, "FAILED"),
            TaskStatus::Skipped => write!(f, "SKIPPED"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct BoiTask {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub status: TaskStatus,
    pub depends: Option<Vec<String>>,
    pub spec: Option<String>,
    pub verify: Option<String>,
    #[serde(default)]
    pub verify_prompt: Option<String>,
    /// Override task-level phases for this specific task
    #[serde(default)]
    pub phases: Option<Vec<String>>,
}

#[derive(Debug)]
pub enum ValidationError {
    MissingTitle,
    NoTasks,
    DuplicateTaskId(String),
    UnknownDependency { task_id: String, dep_id: String },
    CircularDependency(Vec<String>),
    /// Neither `workspace` nor `workspace_rationale` was declared. Layer 4
    /// (2026-05-12): every spec must make an explicit choice.
    MissingWorkspaceAndRationale,
    /// `workspace_rationale` was set but empty / whitespace-only.
    EmptyWorkspaceRationale,
    /// Both `workspace` and `workspace_rationale` were declared. Mutually
    /// exclusive — a rationale exists to explain absence of workspace.
    WorkspaceAndRationaleBothDeclared,
    /// `workspace` points to a path that is not a git repo (no `.git` dir
    /// at the root).
    WorkspaceNotAGitRepo(String),
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationError::MissingTitle => write!(f, "spec title is required"),
            ValidationError::NoTasks => write!(f, "spec must have at least one task"),
            ValidationError::DuplicateTaskId(id) => write!(f, "duplicate task ID: {}", id),
            ValidationError::UnknownDependency { task_id, dep_id } => {
                write!(f, "task {} depends on unknown task {}", task_id, dep_id)
            }
            ValidationError::CircularDependency(cycle) => {
                write!(f, "circular dependency: {}", cycle.join(", "))
            }
            ValidationError::MissingWorkspaceAndRationale => write!(
                f,
                "spec must declare either `workspace: <path-to-git-repo>` or \
                 `workspace_rationale: <non-empty reason>` — neither was provided. \
                 If this spec touches a git repo, declare it explicitly so a worktree \
                 is created. If it genuinely doesn't, explain why."
            ),
            ValidationError::EmptyWorkspaceRationale => write!(
                f,
                "`workspace_rationale` is set but empty or whitespace-only. Provide a \
                 non-empty explanation of why this spec needs no git workspace."
            ),
            ValidationError::WorkspaceAndRationaleBothDeclared => write!(
                f,
                "spec declares both `workspace` and `workspace_rationale`. These are \
                 mutually exclusive — `workspace_rationale` exists to explain the \
                 ABSENCE of a workspace. Pick one."
            ),
            ValidationError::WorkspaceNotAGitRepo(path) => write!(
                f,
                "workspace `{}` is not a git repo (no .git found at the root). \
                 Point `workspace` at a real git repo or use `workspace_rationale` to \
                 explain that this spec has no repo target.",
                path
            ),
        }
    }
}

impl std::error::Error for ValidationError {}

/// Parse and validate a BOI spec from YAML content.
pub fn parse(content: &str) -> Result<BoiSpec, Box<dyn std::error::Error>> {
    let spec: BoiSpec = serde_yml::from_str(content)?;
    validate(&spec)?;
    Ok(spec)
}

/// Parse without validation — useful for reading in-progress specs.
pub fn parse_unchecked(content: &str) -> Result<BoiSpec, Box<dyn std::error::Error>> {
    let spec: BoiSpec = serde_yml::from_str(content)?;
    Ok(spec)
}

/// Layer 4 — workspace OR rationale must be declared, exclusively.
///
/// Every spec must explicitly state its target git repo (`workspace:`) or
/// declare with a rationale that it touches no repo (`workspace_rationale:`).
/// This prevents the silent-drift pattern that stranded 49 uncommitted files
/// in the BOI main checkout (incident 2026-05-12): specs without `workspace:`
/// fell through to a `/tmp/` scratch dir, and any task that wrote to absolute
/// paths in the source repo did so outside any worktree → no commit → drift.
fn validate_workspace_or_rationale(spec: &BoiSpec) -> Result<(), ValidationError> {
    let ws = spec.workspace.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let rationale = spec.workspace_rationale.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    match (ws, spec.workspace_rationale.as_deref(), rationale) {
        (Some(_), Some(_), Some(_)) => Err(ValidationError::WorkspaceAndRationaleBothDeclared),
        (Some(path), _, _) => {
            // workspace is set — verify it points at a real git repo.
            let p = std::path::Path::new(path);
            let git_dir = p.join(".git");
            if !git_dir.exists() {
                return Err(ValidationError::WorkspaceNotAGitRepo(path.to_string()));
            }
            Ok(())
        }
        (None, Some(raw), None) if raw.trim().is_empty() => {
            // workspace_rationale was provided but empty/whitespace.
            Err(ValidationError::EmptyWorkspaceRationale)
        }
        (None, None, _) => Err(ValidationError::MissingWorkspaceAndRationale),
        (None, _, Some(_)) => Ok(()),
        // Defensive: should be unreachable given the filter above, but the
        // exhaustiveness check keeps the match honest if shapes drift.
        (None, Some(_), None) => Err(ValidationError::EmptyWorkspaceRationale),
    }
}

/// Strict validation invoked at dispatch time. Runs `validate` first, then
/// enforces the Layer 4 workspace-or-rationale gate. Use this from
/// `cmd_dispatch` and any other path that enqueues new specs.
pub fn validate_for_dispatch(spec: &BoiSpec) -> Result<(), ValidationError> {
    validate(spec)?;
    validate_workspace_or_rationale(spec)?;
    Ok(())
}

/// Validate a parsed spec. Returns first error encountered.
///
/// Baseline structural checks only. Does NOT include the Layer 4 workspace
/// gate — that lives in [validate_for_dispatch] so it fires at the dispatch
/// entry point (`cmd_dispatch`) without breaking in-flight readers / test
/// fixtures that legitimately parse partial specs.
pub fn validate(spec: &BoiSpec) -> Result<(), ValidationError> {
    if spec.title.trim().is_empty() {
        return Err(ValidationError::MissingTitle);
    }
    if spec.tasks.is_empty() {
        return Err(ValidationError::NoTasks);
    }

    let mut seen_ids: HashSet<&str> = HashSet::new();
    for task in &spec.tasks {
        if !seen_ids.insert(task.id.as_str()) {
            return Err(ValidationError::DuplicateTaskId(task.id.clone()));
        }
    }

    let all_ids: HashSet<&str> = spec.tasks.iter().map(|t| t.id.as_str()).collect();
    for task in &spec.tasks {
        for dep in task.depends.as_deref().unwrap_or(&[]) {
            if !all_ids.contains(dep.as_str()) {
                return Err(ValidationError::UnknownDependency {
                    task_id: task.id.clone(),
                    dep_id: dep.clone(),
                });
            }
        }
    }

    topological_sort(spec)?;
    Ok(())
}

/// Returns task IDs in topological order (dependencies before dependents).
/// Errors if a cycle is detected.
pub fn topological_sort(spec: &BoiSpec) -> Result<Vec<String>, ValidationError> {
    let mut in_degree: HashMap<&str, usize> =
        spec.tasks.iter().map(|t| (t.id.as_str(), 0usize)).collect();

    // adjacency: dep -> [tasks that depend on dep]
    let mut adj: HashMap<&str, Vec<&str>> =
        spec.tasks.iter().map(|t| (t.id.as_str(), vec![])).collect();

    for task in &spec.tasks {
        for dep in task.depends.as_deref().unwrap_or(&[]) {
            adj.get_mut(dep.as_str())
                .expect("dep validated against task ID set")
                .push(task.id.as_str());
            *in_degree
                .get_mut(task.id.as_str())
                .expect("task ID validated against task ID set") += 1;
        }
    }

    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(&id, _)| id)
        .collect();

    let mut order: Vec<String> = Vec::with_capacity(spec.tasks.len());
    while let Some(id) = queue.pop_front() {
        order.push(id.to_string());
        for &dependent in &adj[id] {
            let deg = in_degree
                .get_mut(dependent)
                .expect("dependent from adj must exist in in_degree");
            *deg -= 1;
            if *deg == 0 {
                queue.push_back(dependent);
            }
        }
    }

    if order.len() != spec.tasks.len() {
        let cyclic: Vec<String> = in_degree
            .iter()
            .filter(|(_, &d)| d > 0)
            .map(|(&id, _)| id.to_string())
            .collect();
        return Err(ValidationError::CircularDependency(cyclic));
    }

    Ok(order)
}

/// Returns groups of task IDs that can run in parallel.
/// Tasks within the same group have no dependencies on each other.
/// Groups are ordered so earlier groups must complete before later ones.
pub fn parallel_groups(spec: &BoiSpec) -> Result<Vec<Vec<String>>, ValidationError> {
    let order = topological_sort(spec)?;
    let task_map: HashMap<&str, &BoiTask> = spec.tasks.iter().map(|t| (t.id.as_str(), t)).collect();

    let mut levels: HashMap<&str, usize> = HashMap::new();
    for id in &order {
        let task = task_map[id.as_str()];
        let level = task
            .depends
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|dep| levels.get(dep.as_str()).copied().unwrap_or(0) + 1)
            .max()
            .unwrap_or(0);
        levels.insert(id.as_str(), level);
    }

    let max_level = levels.values().copied().max().unwrap_or(0);
    let mut groups: Vec<Vec<String>> = vec![Vec::new(); max_level + 1];
    for id in &order {
        groups[levels[id.as_str()]].push(id.clone());
    }
    groups.retain(|g| !g.is_empty());
    Ok(groups)
}

/// Returns the next PENDING tasks that are ready to run (all deps DONE).
pub fn ready_tasks(spec: &BoiSpec) -> Vec<&BoiTask> {
    let done_ids: HashSet<&str> = spec
        .tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Done)
        .map(|t| t.id.as_str())
        .collect();

    spec.tasks
        .iter()
        .filter(|t| {
            t.status == TaskStatus::Pending
                && t.depends
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .all(|dep| done_ids.contains(dep.as_str()))
        })
        .collect()
}

/// Validate a spec at intake time (pre-dispatch).
/// Rejects specs where any task has a non-PENDING status at creation time.
/// This catches the S1223-class (pre-DONE tasks) and status-enum-mismatch failures
/// before they consume worker budget.
pub fn validate_intake(spec: &BoiSpec) -> Result<(), String> {
    if spec.tasks.is_empty() {
        return Err("empty-task-list: spec has no tasks".to_string());
    }
    for task in &spec.tasks {
        if task.status == TaskStatus::Done {
            return Err(format!(
                "pre-done-task: task {} has status DONE at creation",
                task.id
            ));
        }
        if task.status != TaskStatus::Pending {
            return Err(format!(
                "invalid-create-status: task {} has status {:?}, expected PENDING",
                task.id, task.status
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL_YAML: &str = r#"
title: "Test Spec"
tasks:
  - id: t-1
    title: "First task"
    status: PENDING
"#;

    #[test]
    fn test_parse_minimal() {
        let spec = parse(MINIMAL_YAML).unwrap();
        assert_eq!(spec.title, "Test Spec");
        assert_eq!(spec.tasks.len(), 1);
        assert_eq!(spec.tasks[0].id, "t-1");
        assert_eq!(spec.tasks[0].status, TaskStatus::Pending);
    }

    #[test]
    fn test_parse_full() {
        let yaml = r#"
title: "Full Spec"
mode: execute
initiative: my-project
context: |
  Some context.
outcomes:
  - description: "Binary compiles"
    verify: "cargo build"
tasks:
  - id: t-1
    title: "Setup"
    status: DONE
    spec: "Do setup"
    verify: "test -f Cargo.toml"
  - id: t-2
    title: "Build"
    status: PENDING
    depends: [t-1]
    spec: "Run cargo build"
    verify: "cargo build"
"#;
        let spec = parse(yaml).unwrap();
        assert_eq!(spec.mode.as_deref(), Some("execute"));
        assert_eq!(spec.tasks.len(), 2);
        assert_eq!(spec.tasks[1].status, TaskStatus::Pending);
        assert_eq!(spec.tasks[1].depends, Some(vec!["t-1".to_string()]));
    }

    #[test]
    fn test_validation_missing_title() {
        let yaml = r#"
title: ""
tasks:
  - id: t-1
    title: "Task"
    status: PENDING
"#;
        assert!(matches!(
            parse(yaml),
            Err(e) if e.to_string().contains("title")
        ));
    }

    #[test]
    fn test_validation_no_tasks() {
        let yaml = r#"
title: "Empty"
tasks: []
"#;
        assert!(matches!(
            parse(yaml),
            Err(e) if e.to_string().contains("task")
        ));
    }

    #[test]
    fn test_validation_duplicate_ids() {
        let yaml = r#"
title: "Dup"
tasks:
  - id: t-1
    title: "A"
    status: PENDING
  - id: t-1
    title: "B"
    status: PENDING
"#;
        let err = parse(yaml).unwrap_err();
        assert!(err.to_string().contains("t-1"), "err={}", err);
    }

    #[test]
    fn test_validation_unknown_dep() {
        let yaml = r#"
title: "Bad dep"
tasks:
  - id: t-1
    title: "A"
    status: PENDING
    depends: [t-99]
"#;
        let err = parse(yaml).unwrap_err();
        assert!(err.to_string().contains("t-99"), "err={}", err);
    }

    #[test]
    fn test_topological_sort_linear() {
        let yaml = r#"
title: "Linear"
tasks:
  - id: t-1
    title: "A"
    status: PENDING
  - id: t-2
    title: "B"
    status: PENDING
    depends: [t-1]
  - id: t-3
    title: "C"
    status: PENDING
    depends: [t-2]
"#;
        let spec = parse(yaml).unwrap();
        let order = topological_sort(&spec).unwrap();
        let pos: HashMap<&str, usize> = order
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str(), i))
            .collect();
        assert!(pos["t-1"] < pos["t-2"]);
        assert!(pos["t-2"] < pos["t-3"]);
    }

    #[test]
    fn test_circular_dependency() {
        // Must use parse_unchecked to bypass validation for the cycle test
        let yaml = r#"
title: "Circular"
tasks:
  - id: t-1
    title: "A"
    status: PENDING
    depends: [t-2]
  - id: t-2
    title: "B"
    status: PENDING
    depends: [t-1]
"#;
        let spec = parse_unchecked(yaml).unwrap();
        assert!(matches!(
            topological_sort(&spec),
            Err(ValidationError::CircularDependency(_))
        ));
    }

    #[test]
    fn test_parallel_groups() {
        let yaml = r#"
title: "Diamond"
tasks:
  - id: t-1
    title: "Root"
    status: PENDING
  - id: t-2
    title: "Left"
    status: PENDING
    depends: [t-1]
  - id: t-3
    title: "Right"
    status: PENDING
    depends: [t-1]
  - id: t-4
    title: "Merge"
    status: PENDING
    depends: [t-2, t-3]
"#;
        let spec = parse(yaml).unwrap();
        let groups = parallel_groups(&spec).unwrap();
        assert_eq!(groups.len(), 3, "expected 3 levels, got {:?}", groups);
        assert_eq!(groups[0], vec!["t-1"]);
        // groups[1] should contain t-2 and t-3 (order may vary)
        let mut mid = groups[1].clone();
        mid.sort();
        assert_eq!(mid, vec!["t-2", "t-3"]);
        assert_eq!(groups[2], vec!["t-4"]);
    }

    #[test]
    fn test_ready_tasks() {
        let yaml = r#"
title: "Ready"
tasks:
  - id: t-1
    title: "Done task"
    status: DONE
  - id: t-2
    title: "Ready"
    status: PENDING
    depends: [t-1]
  - id: t-3
    title: "Blocked"
    status: PENDING
    depends: [t-2]
"#;
        let spec = parse(yaml).unwrap();
        let ready = ready_tasks(&spec);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "t-2");
    }

    #[test]
    fn test_task_status_defaults_to_pending() {
        let yaml = r#"
title: "Default Status"
tasks:
  - id: t-1
    title: "No status field"
  - id: t-2
    title: "Explicit pending"
    status: PENDING
"#;
        let spec = parse(yaml).unwrap();
        assert_eq!(spec.tasks[0].status, TaskStatus::Pending);
        assert_eq!(spec.tasks[1].status, TaskStatus::Pending);
    }

    // --- Step 4: spec_phases / task_phases / phases field tests ---

    #[test]
    fn test_parse_spec_with_phase_overrides() {
        let yaml = r#"
title: "Phase Override Spec"
mode: challenge
spec_phases: ["plan-critique", "critic", "evaluate"]
task_phases: ["execute", "code-review"]
tasks:
  - id: t-1
    title: "Task one"
    status: PENDING
"#;
        let spec = parse(yaml).unwrap();
        assert_eq!(
            spec.spec_phases,
            Some(vec![
                "plan-critique".to_string(),
                "critic".to_string(),
                "evaluate".to_string()
            ])
        );
        assert_eq!(
            spec.task_phases,
            Some(vec!["execute".to_string(), "code-review".to_string()])
        );
    }

    #[test]
    fn test_parse_spec_without_phase_overrides() {
        let yaml = r#"
title: "No Override"
tasks:
  - id: t-1
    title: "Task"
    status: PENDING
"#;
        let spec = parse(yaml).unwrap();
        assert_eq!(spec.spec_phases, None);
        assert_eq!(spec.task_phases, None);
    }

    #[test]
    fn test_parse_context_files() {
        let yaml = r#"
title: "Context Files Spec"
context_files:
  - ~/.claude/shared-memory/SHARED.md
  - ~/notes.md
tasks:
  - id: t-1
    title: "Task"
    status: PENDING
"#;
        let spec = parse(yaml).unwrap();
        let files = spec.context_files.as_ref().expect("context_files should be present");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], "~/.claude/shared-memory/SHARED.md");
    }

    #[test]
    fn test_context_files_defaults_to_none() {
        let spec = parse(MINIMAL_YAML).unwrap();
        assert!(spec.context_files.is_none());
    }

    #[test]
    fn test_parse_task_with_phases_override() {
        let yaml = r#"
title: "Task Phases"
tasks:
  - id: t-1
    title: "Custom phases task"
    status: PENDING
    phases: ["execute"]
  - id: t-2
    title: "Default phases task"
    status: PENDING
"#;
        let spec = parse(yaml).unwrap();
        assert_eq!(spec.tasks[0].phases, Some(vec!["execute".to_string()]));
        assert_eq!(spec.tasks[1].phases, None);
    }

    // --- spec_worker_pool tests ---

    #[test]
    fn spec_worker_pool_defaults_to_none() {
        let spec = parse(MINIMAL_YAML).unwrap();
        assert!(spec.worker_pool.is_none());
    }

    #[test]
    fn spec_worker_pool_parses_named_pool() {
        let yaml = r#"
title: "Pool Spec"
worker_pool: fly-runners
tasks:
  - id: t-1
    title: "Task"
    status: PENDING
"#;
        let spec = parse(yaml).unwrap();
        assert_eq!(spec.worker_pool.as_deref(), Some("fly-runners"));
    }

    #[test]
    fn spec_worker_pool_none_when_field_absent() {
        let yaml = r#"
title: "No Pool"
tasks:
  - id: t-1
    title: "Task"
    status: PENDING
"#;
        let spec = parse(yaml).unwrap();
        assert_eq!(spec.worker_pool, None);
    }

    #[test]
    fn spec_worker_pool_roundtrips_through_serde() {
        let yaml = r#"
title: "Roundtrip"
worker_pool: local
tasks:
  - id: t-1
    title: "Task"
    status: PENDING
"#;
        let spec = parse(yaml).unwrap();
        let serialized = serde_yml::to_string(&spec).unwrap();
        let spec2: BoiSpec = serde_yml::from_str(&serialized).unwrap();
        assert_eq!(spec.worker_pool, spec2.worker_pool);
    }

    // --- validate_intake tests ---

    #[test]
    fn test_intake_reject_pre_done_task() {
        let yaml = r#"
title: "Pre-done spec"
tasks:
  - id: T1
    title: "Already done task"
    status: DONE
"#;
        let spec = parse_unchecked(yaml).unwrap();
        let err = validate_intake(&spec).unwrap_err();
        assert!(err.contains("pre-done-task"), "got: {}", err);
    }

    #[test]
    fn test_intake_reject_invalid_status() {
        let yaml = r#"
title: "Running status spec"
tasks:
  - id: T1
    title: "Already running task"
    status: RUNNING
"#;
        let spec = parse_unchecked(yaml).unwrap();
        let err = validate_intake(&spec).unwrap_err();
        assert!(err.contains("invalid-create-status"), "got: {}", err);
    }

    #[test]
    fn test_intake_reject_empty_tasks() {
        let yaml = r#"
title: "Empty spec"
tasks: []
"#;
        let spec = parse_unchecked(yaml).unwrap();
        let err = validate_intake(&spec).unwrap_err();
        assert!(err.contains("empty-task-list"), "got: {}", err);
    }

    #[test]
    fn test_intake_accept_valid_spec() {
        let yaml = r#"
title: "Valid spec"
tasks:
  - id: T1
    title: "Task one"
    status: PENDING
  - id: T2
    title: "Task two"
    status: PENDING
  - id: T3
    title: "Task three"
    status: PENDING
"#;
        let spec = parse_unchecked(yaml).unwrap();
        assert!(validate_intake(&spec).is_ok());
    }

    // ── Layer 4 — workspace OR rationale required at dispatch ───────────────

    mod workspace_or_rationale {
        use super::*;

        fn spec_with(workspace: Option<&str>, rationale: Option<&str>) -> BoiSpec {
            let mut yaml = String::from("title: \"Test\"\ntasks:\n  - id: t-1\n    title: \"x\"\n    status: PENDING\n");
            if let Some(ws) = workspace {
                yaml.push_str(&format!("workspace: \"{}\"\n", ws));
            }
            if let Some(r) = rationale {
                yaml.push_str(&format!("workspace_rationale: \"{}\"\n", r));
            }
            parse_unchecked(&yaml).unwrap()
        }

        #[test]
        fn rejects_missing_both_workspace_and_rationale() {
            let spec = spec_with(None, None);
            let err = validate_for_dispatch(&spec)
                .expect_err("expected MissingWorkspaceAndRationale");
            assert!(
                matches!(err, ValidationError::MissingWorkspaceAndRationale),
                "got: {:?}",
                err
            );
            // Error message must be actionable
            let msg = err.to_string();
            assert!(msg.contains("workspace") && msg.contains("rationale"),
                "error must point at both fields: {}", msg);
        }

        #[test]
        fn rejects_empty_whitespace_rationale() {
            let spec = spec_with(None, Some("   \t  "));
            let err = validate_for_dispatch(&spec)
                .expect_err("expected EmptyWorkspaceRationale");
            assert!(
                matches!(err, ValidationError::EmptyWorkspaceRationale),
                "got: {:?}",
                err
            );
        }

        #[test]
        fn rejects_both_workspace_and_rationale() {
            // Use a path we know is a git repo so the workspace itself passes.
            let repo = env!("CARGO_MANIFEST_DIR");
            let spec = spec_with(Some(repo), Some("ambiguous — both set"));
            let err = validate_for_dispatch(&spec)
                .expect_err("expected WorkspaceAndRationaleBothDeclared");
            assert!(
                matches!(err, ValidationError::WorkspaceAndRationaleBothDeclared),
                "got: {:?}",
                err
            );
        }

        #[test]
        fn rejects_workspace_path_not_a_git_repo() {
            let spec = spec_with(Some("/tmp/definitely-not-a-git-repo-zzz9999"), None);
            let err = validate_for_dispatch(&spec)
                .expect_err("expected WorkspaceNotAGitRepo");
            assert!(
                matches!(err, ValidationError::WorkspaceNotAGitRepo(_)),
                "got: {:?}",
                err
            );
            let msg = err.to_string();
            assert!(msg.contains("not a git repo"),
                "error must say so: {}", msg);
        }

        #[test]
        fn accepts_workspace_pointing_at_git_repo() {
            let repo = env!("CARGO_MANIFEST_DIR");
            let spec = spec_with(Some(repo), None);
            validate_for_dispatch(&spec).expect("Ok expected for valid repo");
        }

        #[test]
        fn accepts_rationale_only() {
            let spec = spec_with(
                None,
                Some("Pure analysis — writes only to projects/, no repo target."),
            );
            validate_for_dispatch(&spec).expect("Ok expected for rationale-only");
        }

        #[test]
        fn baseline_validate_does_not_enforce_workspace() {
            // Layer 4 gate is dispatch-only — baseline validate() stays liberal so
            // in-flight spec readers (worker resume, etc.) don't trip on the field.
            let spec = spec_with(None, None);
            validate(&spec).expect("baseline validate must not enforce workspace gate");
        }
    }
}
