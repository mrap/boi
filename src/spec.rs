use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

/// Runtime provider selector for a phase override.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PhaseRuntime {
    Claude,
    Openrouter,
    Codex,
}

/// Per-phase override values from a pipeline TOML's [phase_overrides.<name>] block.
/// All fields are optional; unset fields fall back to the phase TOML default.
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
    pub workspace: Option<String>,
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
    /// Per-phase overrides injected from the pipeline TOML (runner applies these before phase TOML defaults).
    #[serde(default)]
    pub phase_overrides: HashMap<String, PhaseOverride>,
    pub tasks: Vec<BoiTask>,
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

/// Validate a parsed spec. Returns first error encountered.
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
}
