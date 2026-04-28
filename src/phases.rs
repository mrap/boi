use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Whether a phase operates at the whole-spec level or per-task level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PhaseLevel {
    /// Runs once for the entire spec (e.g., plan-critique, critic, evaluate)
    Spec,
    /// Runs once per task (e.g., execute, code-review, task-verify)
    #[default]
    Task,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseConfig {
    pub name: String,
    pub level: PhaseLevel,
    pub description: String,
    pub prompt_template: String,
    pub timeout_minutes: Option<u32>,
    pub retry_count: Option<u32>,
    pub can_add_tasks: bool,
    pub can_fail_spec: bool,
    pub requires_claude: bool,
    pub approve_signal: Option<String>,
    pub reject_signal: Option<String>,
    pub on_approve: Option<String>,
    pub on_reject: Option<String>,
    pub on_crash: Option<String>,
    pub min_lines_changed: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct PhaseToml {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    completion_handler: Option<String>,
    #[serde(default)]
    phase: Option<PhaseSection>,
    #[serde(default)]
    worker: Option<WorkerSection>,
    #[serde(default)]
    prompt: Option<PromptSection>,
    #[serde(default)]
    completion: Option<CompletionSection>,
    #[serde(default)]
    trigger: Option<TriggerSection>,
}

#[derive(Debug, Deserialize)]
struct PhaseSection {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    timeout_minutes: Option<u32>,
    #[serde(default)]
    can_add_tasks: Option<bool>,
    #[serde(default)]
    can_fail_spec: Option<bool>,
    #[serde(default)]
    requires_claude: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct WorkerSection {
    #[serde(default)]
    prompt_template: Option<String>,
    #[serde(default)]
    timeout: Option<u32>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    runtime: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    code_model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PromptSection {
    #[serde(default)]
    template: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CompletionSection {
    #[serde(default)]
    approve_signal: Option<String>,
    #[serde(default)]
    reject_signal: Option<String>,
    #[serde(default)]
    on_approve: Option<String>,
    #[serde(default)]
    on_reject: Option<String>,
    #[serde(default)]
    on_crash: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TriggerSection {
    #[serde(default)]
    min_lines_changed: Option<u32>,
}

impl PhaseConfig {
    fn from_toml(toml: PhaseToml) -> Option<Self> {
        let name = toml
            .phase.as_ref().and_then(|p| p.name.clone())
            .or(toml.name.clone())?;

        let description = toml
            .phase.as_ref().and_then(|p| p.description.clone())
            .or(toml.description.clone())
            .unwrap_or_default();

        let prompt_template = toml
            .prompt.as_ref().and_then(|p| p.template.clone())
            .or_else(|| toml.worker.as_ref().and_then(|w| w.prompt_template.clone()))
            .unwrap_or_default();

        let timeout_minutes = toml
            .phase.as_ref().and_then(|p| p.timeout_minutes)
            .or_else(|| toml.worker.as_ref().and_then(|w| w.timeout.map(|t| t / 60)));

        let can_add_tasks = toml
            .phase.as_ref().and_then(|p| p.can_add_tasks)
            .unwrap_or(false);
        let can_fail_spec = toml
            .phase.as_ref().and_then(|p| p.can_fail_spec)
            .unwrap_or(false);
        let requires_claude = toml
            .phase.as_ref().and_then(|p| p.requires_claude)
            .unwrap_or(true);

        let completion = toml.completion.as_ref();
        let approve_signal = completion.and_then(|c| non_empty(&c.approve_signal));
        let reject_signal = completion.and_then(|c| non_empty(&c.reject_signal));
        let on_approve = completion.and_then(|c| c.on_approve.clone());
        let on_reject = completion.and_then(|c| c.on_reject.clone());
        let on_crash = completion.and_then(|c| c.on_crash.clone());
        let min_lines_changed = toml.trigger.as_ref().and_then(|t| t.min_lines_changed);

        Some(PhaseConfig {
            name,
            level: PhaseLevel::default(),
            description,
            prompt_template,
            timeout_minutes,
            retry_count: None,
            can_add_tasks,
            can_fail_spec,
            requires_claude,
            approve_signal,
            reject_signal,
            on_approve,
            on_reject,
            on_crash,
            min_lines_changed,
        })
    }
}

fn non_empty(opt: &Option<String>) -> Option<String> {
    opt.as_ref().and_then(|s| {
        if s.is_empty() { None } else { Some(s.clone()) }
    })
}

pub struct PhaseRegistry {
    core: HashMap<String, PhaseConfig>,
    user: HashMap<String, PhaseConfig>,
}

impl Default for PhaseRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PhaseRegistry {
    pub fn new() -> Self {
        let mut core = HashMap::new();
        for phase in core_phases() {
            core.insert(phase.name.clone(), phase);
        }
        PhaseRegistry {
            core,
            user: HashMap::new(),
        }
    }

    pub fn load_user_phases(&mut self, dir: &Path) {
        if !dir.is_dir() {
            return;
        }
        let patterns = [
            dir.join("*.phase.toml"),
            dir.join("*.toml"),
        ];
        let mut seen = std::collections::HashSet::new();
        for pattern in &patterns {
            let pat = pattern.to_string_lossy();
            if let Ok(entries) = glob::glob(&pat) {
                for entry in entries.flatten() {
                    if !seen.insert(entry.clone()) {
                        continue;
                    }
                    match load_phase_file(&entry) {
                        Ok(phase) => {
                            self.user.insert(phase.name.clone(), phase);
                        }
                        Err(e) => {
                            eprintln!(
                                "WARN: failed to load phase {}: {}",
                                entry.display(),
                                e
                            );
                        }
                    }
                }
            }
        }
    }

    pub fn get(&self, name: &str) -> Option<&PhaseConfig> {
        self.user.get(name).or_else(|| self.core.get(name))
    }

    pub fn list(&self) -> Vec<&PhaseConfig> {
        let mut merged: HashMap<&str, &PhaseConfig> = HashMap::new();
        for (name, phase) in &self.core {
            merged.insert(name.as_str(), phase);
        }
        for (name, phase) in &self.user {
            merged.insert(name.as_str(), phase);
        }
        let mut phases: Vec<&PhaseConfig> = merged.into_values().collect();
        phases.sort_by(|a, b| a.name.cmp(&b.name));
        phases
    }

    pub fn core_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.core.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    pub fn user_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.user.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    pub fn is_user_override(&self, name: &str) -> bool {
        self.user.contains_key(name) && self.core.contains_key(name)
    }
}

pub fn default_phases(mode: &str) -> Vec<String> {
    match mode {
        "execute" | "challenge" => vec!["execute", "critic"],
        "discover" => vec!["execute", "critic", "evaluate"],
        "generate" => vec!["decompose", "execute", "critic", "evaluate"],
        _ => vec!["execute"],
    }
    .into_iter()
    .map(String::from)
    .collect()
}

/// Pipeline configuration separating spec-level and task-level phases.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineConfig {
    /// Phases that run once for the whole spec (before/after all tasks)
    pub spec_phases: Vec<String>,
    /// Phases that run for each individual task
    pub task_phases: Vec<String>,
}

/// Returns the default pipeline for a given spec mode.
pub fn default_pipeline(mode: &str) -> PipelineConfig {
    match mode {
        "execute" => PipelineConfig {
            spec_phases: vec!["critic".into()],
            task_phases: vec!["execute".into(), "task-verify".into()],
        },
        "challenge" => PipelineConfig {
            spec_phases: vec!["plan-critique".into(), "critic".into()],
            task_phases: vec!["execute".into(), "code-review".into(), "task-verify".into()],
        },
        "discover" => PipelineConfig {
            spec_phases: vec!["plan-critique".into(), "critic".into(), "evaluate".into()],
            task_phases: vec!["execute".into(), "task-verify".into()],
        },
        "generate" => PipelineConfig {
            spec_phases: vec!["plan-critique".into(), "critic".into(), "evaluate".into()],
            task_phases: vec!["decompose".into(), "execute".into(), "task-verify".into()],
        },
        _ => PipelineConfig {
            spec_phases: vec![],
            task_phases: vec!["execute".into()],
        },
    }
}

fn load_phase_file(path: &Path) -> Result<PhaseConfig, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let toml: PhaseToml = toml::from_str(&content)?;
    PhaseConfig::from_toml(toml).ok_or_else(|| {
        format!("phase file missing name: {}", path.display()).into()
    })
}

fn core_phases() -> Vec<PhaseConfig> {
    vec![
        PhaseConfig {
            name: "execute".into(),
            level: PhaseLevel::Task,
            description: "Execute the task specification via claude, verify with verify command".into(),
            prompt_template: String::new(),
            timeout_minutes: Some(30),
            retry_count: None,
            can_add_tasks: false,
            can_fail_spec: false,
            requires_claude: true,
            approve_signal: None,
            reject_signal: None,
            on_approve: None,
            on_reject: None,
            on_crash: None,
            min_lines_changed: None,
        },
        PhaseConfig {
            name: "critic".into(),
            level: PhaseLevel::Spec,
            description: "Review completed spec for quality issues. Add [CRITIC] tasks if problems found.".into(),
            prompt_template: concat!(
                "You are a BOI critic reviewing completed work.\n\n",
                "Review the spec and all completed tasks for:\n",
                "1. Spec integrity — do the outcomes match what was built?\n",
                "2. Weak verifications — are verify commands actually testing the right thing?\n",
                "3. Incomplete work — any tasks that claim DONE but have gaps?\n",
                "4. Quality issues — obvious bugs, missing error handling, dead code?\n\n",
                "If all work is satisfactory, output: ## Critic Approved\n\n",
                "If issues found, output lines starting with [CRITIC] describing each issue.\n",
                "Each [CRITIC] line becomes a new remediation task.",
            ).into(),
            timeout_minutes: Some(15),
            retry_count: None,
            can_add_tasks: true,
            can_fail_spec: true,
            requires_claude: true,
            approve_signal: Some("## Critic Approved".into()),
            reject_signal: Some("[CRITIC]".into()),
            on_approve: Some("next".into()),
            on_reject: Some("requeue:execute".into()),
            on_crash: Some("retry".into()),
            min_lines_changed: None,
        },
        PhaseConfig {
            name: "decompose".into(),
            level: PhaseLevel::Task,
            description: "Break large tasks into subtasks before execution.".into(),
            prompt_template: concat!(
                "You are a BOI decomposer. Break this task into smaller subtasks.\n\n",
                "For each subtask, output a YAML block:\n",
                "```yaml\n",
                "- id: t-N-sub-M\n",
                "  title: \"Subtask title\"\n",
                "  status: PENDING\n",
                "  spec: |\n",
                "    What to do.\n",
                "  verify: \"command that returns 0 on success\"\n",
                "```\n\n",
                "Only decompose if the task genuinely has independent sub-steps.\n",
                "If the task is already atomic, output: ## No Decomposition Needed",
            ).into(),
            timeout_minutes: Some(10),
            retry_count: None,
            can_add_tasks: true,
            can_fail_spec: false,
            requires_claude: true,
            approve_signal: None,
            reject_signal: None,
            on_approve: None,
            on_reject: None,
            on_crash: None,
            min_lines_changed: None,
        },
        PhaseConfig {
            name: "evaluate".into(),
            level: PhaseLevel::Spec,
            description: "Check if generate-mode spec has converged. Add tasks if not.".into(),
            prompt_template: concat!(
                "You are a BOI evaluator checking if this spec has converged.\n\n",
                "Review all completed tasks and outcomes. Determine:\n",
                "1. Are all outcomes satisfied?\n",
                "2. Is the work complete and coherent?\n",
                "3. Are there obvious gaps or missing pieces?\n\n",
                "If converged, output: ## Evaluation Complete\n\n",
                "If not converged, output new tasks as YAML blocks to close the gaps.",
            ).into(),
            timeout_minutes: Some(15),
            retry_count: None,
            can_add_tasks: true,
            can_fail_spec: false,
            requires_claude: true,
            approve_signal: Some("## Evaluation Complete".into()),
            reject_signal: None,
            on_approve: Some("next".into()),
            on_reject: None,
            on_crash: Some("retry".into()),
            min_lines_changed: None,
        },
        PhaseConfig {
            name: "plan-critique".into(),
            level: PhaseLevel::Spec,
            description: "Review spec plan before execution begins. Challenge assumptions and identify risks.".into(),
            prompt_template: concat!(
                "You are a BOI plan critic. Review this spec BEFORE execution begins.\n\n",
                "Evaluate:\n",
                "1. Are tasks well-scoped and independently verifiable?\n",
                "2. Are dependencies correct and complete?\n",
                "3. Are there missing tasks or unrealistic assumptions?\n",
                "4. Do verify commands actually test meaningful outcomes?\n\n",
                "If the plan is sound, output: ## Plan Approved\n\n",
                "If issues found, output lines starting with [PLAN] describing each issue.\n",
                "Each [PLAN] line becomes a new task or modification.",
            ).into(),
            timeout_minutes: Some(10),
            retry_count: None,
            can_add_tasks: true,
            can_fail_spec: false,
            requires_claude: true,
            approve_signal: Some("## Plan Approved".into()),
            reject_signal: Some("[PLAN]".into()),
            on_approve: Some("next".into()),
            on_reject: Some("requeue:plan-critique".into()),
            on_crash: Some("retry".into()),
            min_lines_changed: None,
        },
        PhaseConfig {
            name: "code-review".into(),
            level: PhaseLevel::Task,
            description: "Review code changes after task execution. Flag quality issues.".into(),
            prompt_template: concat!(
                "You are a BOI code reviewer. Review the code changes made for this task.\n\n",
                "Check for:\n",
                "1. Correctness — does the code do what the task spec requires?\n",
                "2. Quality — error handling, edge cases, dead code?\n",
                "3. Style — consistent with the codebase?\n",
                "4. Security — any obvious vulnerabilities?\n\n",
                "If code is acceptable, output: ## Code Review Approved\n\n",
                "If issues found, output lines starting with [CODE-REVIEW] describing each issue.",
            ).into(),
            timeout_minutes: Some(15),
            retry_count: None,
            can_add_tasks: true,
            can_fail_spec: false,
            requires_claude: true,
            approve_signal: Some("## Code Review Approved".into()),
            reject_signal: Some("[CODE-REVIEW]".into()),
            on_approve: Some("next".into()),
            on_reject: Some("requeue:execute".into()),
            on_crash: Some("retry".into()),
            min_lines_changed: Some(10),
        },
        PhaseConfig {
            name: "task-verify".into(),
            level: PhaseLevel::Task,
            description: "Run verification commands for a task without spawning claude.".into(),
            prompt_template: String::new(),
            timeout_minutes: Some(5),
            retry_count: None,
            can_add_tasks: false,
            can_fail_spec: false,
            requires_claude: false,
            approve_signal: None,
            reject_signal: None,
            on_approve: Some("next".into()),
            on_reject: Some("requeue:execute".into()),
            on_crash: None,
            min_lines_changed: None,
        },
    ]
}

pub fn user_phases_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".boi").join("phases")
}

/// Resolve the full pipeline for a spec, considering spec-level overrides.
///
/// Priority:
/// 1. If the spec provides `spec_phases` / `task_phases`, use those.
/// 2. Otherwise, use default_pipeline(mode).
pub fn resolve_pipeline(
    mode: &str,
    spec_phases: Option<&[String]>,
    task_phases: Option<&[String]>,
) -> PipelineConfig {
    let defaults = default_pipeline(mode);
    PipelineConfig {
        spec_phases: spec_phases
            .map(|v| v.to_vec())
            .unwrap_or(defaults.spec_phases),
        task_phases: task_phases
            .map(|v| v.to_vec())
            .unwrap_or(defaults.task_phases),
    }
}

/// Resolve the task-level phases for a specific task.
///
/// Priority:
/// 1. If the task has its own `phases` override, use those.
/// 2. Otherwise, use the pipeline's task_phases.
pub fn resolve_task_phases(
    pipeline: &PipelineConfig,
    task_phases_override: Option<&[String]>,
) -> Vec<String> {
    task_phases_override
        .map(|v| v.to_vec())
        .unwrap_or_else(|| pipeline.task_phases.clone())
}

/// Outcome of running a single phase.
#[derive(Debug, Clone, PartialEq)]
pub enum PhaseOutcome {
    /// Phase approved the work — proceed to next phase.
    Approved,
    /// Phase generated new tasks to add to the spec.
    AddedTasks(Vec<crate::spec::BoiTask>),
    /// Phase requests requeue back to a specific phase.
    Requeue { phase: String },
    /// Phase failed and cannot continue.
    Failed { reason: String },
    /// Phase was skipped (e.g., no changes to review, trigger not met).
    Skipped,
    /// Phase timed out.
    Timeout,
}

/// Build a prompt for a spec-level phase.
pub fn build_phase_prompt(
    phase: &PhaseConfig,
    spec_content: &str,
    task_context: Option<&str>,
) -> String {
    if phase.prompt_template.is_empty() {
        return format!(
            "Phase: {}\n\nSPEC:\n{}\n{}",
            phase.name,
            spec_content,
            task_context.map(|c| format!("\nTASK CONTEXT:\n{}", c)).unwrap_or_default()
        );
    }

    let mut prompt = phase.prompt_template.clone();
    prompt.push_str("\n\n--- SPEC ---\n");
    prompt.push_str(spec_content);
    if let Some(ctx) = task_context {
        prompt.push_str("\n\n--- TASK ---\n");
        prompt.push_str(ctx);
    }
    prompt
}

/// Parse phase output to determine the outcome.
pub fn parse_phase_output(phase: &PhaseConfig, output: &str) -> PhaseOutcome {
    // Check for approve signal first
    if let Some(ref signal) = phase.approve_signal {
        if output.contains(signal) {
            return PhaseOutcome::Approved;
        }
    }

    // Check for reject signal
    if let Some(ref signal) = phase.reject_signal {
        if output.contains(signal) {
            // Determine action from on_reject
            if let Some(ref action) = phase.on_reject {
                if action.starts_with("requeue:") {
                    let target_phase = action.strip_prefix("requeue:").unwrap_or("execute");
                    return PhaseOutcome::Requeue {
                        phase: target_phase.to_string(),
                    };
                }
            }
            return PhaseOutcome::Failed {
                reason: format!("Phase {} rejected: found '{}'", phase.name, signal),
            };
        }
    }

    // No explicit signals — treat as approved (permissive default)
    PhaseOutcome::Approved
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_core_phases_exist() {
        let registry = PhaseRegistry::new();
        assert!(registry.get("execute").is_some());
        assert!(registry.get("critic").is_some());
        assert!(registry.get("decompose").is_some());
        assert!(registry.get("evaluate").is_some());
    }

    #[test]
    fn test_core_phase_properties() {
        let registry = PhaseRegistry::new();

        let exec = registry.get("execute").unwrap();
        assert!(!exec.can_add_tasks);
        assert!(!exec.can_fail_spec);
        assert!(exec.requires_claude);

        let critic = registry.get("critic").unwrap();
        assert!(critic.can_add_tasks);
        assert!(critic.can_fail_spec);
        assert!(critic.requires_claude);

        let decompose = registry.get("decompose").unwrap();
        assert!(decompose.can_add_tasks);
        assert!(!decompose.can_fail_spec);

        let evaluate = registry.get("evaluate").unwrap();
        assert!(evaluate.can_add_tasks);
        assert!(!evaluate.can_fail_spec);
    }

    #[test]
    fn test_default_phases_by_mode() {
        assert_eq!(default_phases("execute"), vec!["execute", "critic"]);
        assert_eq!(default_phases("challenge"), vec!["execute", "critic"]);
        assert_eq!(
            default_phases("discover"),
            vec!["execute", "critic", "evaluate"]
        );
        assert_eq!(
            default_phases("generate"),
            vec!["decompose", "execute", "critic", "evaluate"]
        );
        assert_eq!(default_phases("unknown"), vec!["execute"]);
    }

    #[test]
    fn test_unknown_phase_returns_none() {
        let registry = PhaseRegistry::new();
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn test_user_phase_override() {
        let dir = PathBuf::from(format!("/tmp/boi-phase-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();

        let toml_content = r#"
name = "execute"
description = "Custom execute phase"

[phase]
name = "execute"
description = "Custom execute phase"
timeout_minutes = 60
can_add_tasks = false
can_fail_spec = false
requires_claude = true

[prompt]
template = "Custom prompt for execute"
"#;
        fs::write(dir.join("execute.phase.toml"), toml_content).unwrap();

        let mut registry = PhaseRegistry::new();
        registry.load_user_phases(&dir);

        let exec = registry.get("execute").unwrap();
        assert_eq!(exec.description, "Custom execute phase");
        assert_eq!(exec.timeout_minutes, Some(60));
        assert_eq!(exec.prompt_template, "Custom prompt for execute");
        assert!(registry.is_user_override("execute"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_user_adds_new_phase() {
        let dir = PathBuf::from(format!("/tmp/boi-phase-new-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();

        let toml_content = r###"
name = "custom-lint"
description = "Custom lint phase"

[phase]
name = "custom-lint"
description = "Custom lint phase"
can_add_tasks = false
can_fail_spec = true
requires_claude = true

[completion]
approve_signal = "## Lint Passed"
reject_signal = "[LINT]"
on_approve = "next"
on_reject = "requeue:execute"

[trigger]
min_lines_changed = 50

[prompt]
template = "Lint the code changes."
"###;
        fs::write(dir.join("custom-lint.phase.toml"), toml_content).unwrap();

        let mut registry = PhaseRegistry::new();
        registry.load_user_phases(&dir);

        let cr = registry.get("custom-lint").unwrap();
        assert_eq!(cr.description, "Custom lint phase");
        assert!(cr.can_fail_spec);
        assert_eq!(cr.reject_signal.as_deref(), Some("[LINT]"));
        assert_eq!(cr.min_lines_changed, Some(50));
        assert!(!registry.is_user_override("custom-lint"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_existing_repo_toml() {
        let toml_content = r#"
name = "execute"
description = "Execute tasks from the spec using a worker agent"
completion_handler = "builtin:execute"

[worker]
runtime = "claude"
model = "claude-sonnet-4-6"
code_model = ""
prompt_template = "templates/worker-prompt.md"
effort = "medium"
timeout = 600

[completion]
approve_signal = ""
"#;
        let dir = PathBuf::from(format!("/tmp/boi-phase-repo-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("execute.phase.toml"), toml_content).unwrap();

        let mut registry = PhaseRegistry::new();
        registry.load_user_phases(&dir);

        let exec = registry.get("execute").unwrap();
        assert_eq!(exec.name, "execute");
        assert_eq!(exec.prompt_template, "templates/worker-prompt.md");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_list_returns_merged() {
        let registry = PhaseRegistry::new();
        let list = registry.list();
        assert!(list.len() >= 4);
        let names: Vec<&str> = list.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"execute"));
        assert!(names.contains(&"critic"));
        assert!(names.contains(&"decompose"));
        assert!(names.contains(&"evaluate"));
    }

    #[test]
    fn test_core_and_user_names() {
        let registry = PhaseRegistry::new();
        let core = registry.core_names();
        assert_eq!(core.len(), 7);
        assert!(core.contains(&"execute"));
        assert!(core.contains(&"plan-critique"));
        assert!(core.contains(&"code-review"));
        assert!(core.contains(&"task-verify"));

        let user = registry.user_names();
        assert!(user.is_empty());
    }

    #[test]
    fn test_load_nonexistent_dir() {
        let mut registry = PhaseRegistry::new();
        registry.load_user_phases(Path::new("/tmp/boi-nonexistent-dir-xyz"));
        assert_eq!(registry.list().len(), 7);
    }

    // --- Step 1: PhaseLevel tests ---

    #[test]
    fn test_phase_level_defaults_to_task() {
        assert_eq!(PhaseLevel::default(), PhaseLevel::Task);
    }

    #[test]
    fn test_core_phases_have_correct_levels() {
        let registry = PhaseRegistry::new();

        // Spec-level phases
        assert_eq!(registry.get("critic").unwrap().level, PhaseLevel::Spec);
        assert_eq!(registry.get("evaluate").unwrap().level, PhaseLevel::Spec);
        assert_eq!(registry.get("plan-critique").unwrap().level, PhaseLevel::Spec);

        // Task-level phases
        assert_eq!(registry.get("execute").unwrap().level, PhaseLevel::Task);
        assert_eq!(registry.get("decompose").unwrap().level, PhaseLevel::Task);
        assert_eq!(registry.get("code-review").unwrap().level, PhaseLevel::Task);
        assert_eq!(registry.get("task-verify").unwrap().level, PhaseLevel::Task);
    }

    // --- Step 2: PipelineConfig tests ---

    #[test]
    fn test_default_pipeline_execute() {
        let p = default_pipeline("execute");
        assert_eq!(p.spec_phases, vec!["critic"]);
        assert_eq!(p.task_phases, vec!["execute", "task-verify"]);
    }

    #[test]
    fn test_default_pipeline_challenge() {
        let p = default_pipeline("challenge");
        assert_eq!(p.spec_phases, vec!["plan-critique", "critic"]);
        assert_eq!(p.task_phases, vec!["execute", "code-review", "task-verify"]);
    }

    #[test]
    fn test_default_pipeline_discover() {
        let p = default_pipeline("discover");
        assert_eq!(p.spec_phases, vec!["plan-critique", "critic", "evaluate"]);
        assert_eq!(p.task_phases, vec!["execute", "task-verify"]);
    }

    #[test]
    fn test_default_pipeline_generate() {
        let p = default_pipeline("generate");
        assert_eq!(p.spec_phases, vec!["plan-critique", "critic", "evaluate"]);
        assert_eq!(p.task_phases, vec!["decompose", "execute", "task-verify"]);
    }

    #[test]
    fn test_default_pipeline_unknown_mode() {
        let p = default_pipeline("unknown");
        assert!(p.spec_phases.is_empty());
        assert_eq!(p.task_phases, vec!["execute"]);
    }

    // --- Step 3: New core phases tests ---

    #[test]
    fn test_plan_critique_phase() {
        let registry = PhaseRegistry::new();
        let pc = registry.get("plan-critique").unwrap();
        assert_eq!(pc.level, PhaseLevel::Spec);
        assert!(pc.can_add_tasks);
        assert!(!pc.can_fail_spec);
        assert!(pc.requires_claude);
        assert_eq!(pc.approve_signal.as_deref(), Some("## Plan Approved"));
        assert_eq!(pc.reject_signal.as_deref(), Some("[PLAN]"));
    }

    #[test]
    fn test_code_review_phase() {
        let registry = PhaseRegistry::new();
        let cr = registry.get("code-review").unwrap();
        assert_eq!(cr.level, PhaseLevel::Task);
        assert!(cr.can_add_tasks);
        assert!(!cr.can_fail_spec);
        assert!(cr.requires_claude);
        assert_eq!(cr.approve_signal.as_deref(), Some("## Code Review Approved"));
        assert_eq!(cr.min_lines_changed, Some(10));
    }

    #[test]
    fn test_task_verify_phase() {
        let registry = PhaseRegistry::new();
        let tv = registry.get("task-verify").unwrap();
        assert_eq!(tv.level, PhaseLevel::Task);
        assert!(!tv.requires_claude);
        assert!(!tv.can_add_tasks);
        assert!(!tv.can_fail_spec);
    }

    // --- Step 5: resolve_pipeline / resolve_task_phases tests ---

    #[test]
    fn test_resolve_pipeline_uses_defaults() {
        let p = resolve_pipeline("execute", None, None);
        assert_eq!(p.spec_phases, vec!["critic"]);
        assert_eq!(p.task_phases, vec!["execute", "task-verify"]);
    }

    #[test]
    fn test_resolve_pipeline_spec_override() {
        let spec_override = vec!["plan-critique".to_string(), "critic".to_string()];
        let p = resolve_pipeline("execute", Some(&spec_override), None);
        assert_eq!(p.spec_phases, vec!["plan-critique", "critic"]);
        assert_eq!(p.task_phases, vec!["execute", "task-verify"]); // unchanged
    }

    #[test]
    fn test_resolve_pipeline_task_override() {
        let task_override = vec!["execute".to_string()];
        let p = resolve_pipeline("challenge", None, Some(&task_override));
        assert_eq!(p.spec_phases, vec!["plan-critique", "critic"]); // unchanged
        assert_eq!(p.task_phases, vec!["execute"]); // overridden
    }

    #[test]
    fn test_resolve_pipeline_both_override() {
        let sp = vec!["evaluate".to_string()];
        let tp = vec!["execute".to_string(), "code-review".to_string()];
        let p = resolve_pipeline("execute", Some(&sp), Some(&tp));
        assert_eq!(p.spec_phases, vec!["evaluate"]);
        assert_eq!(p.task_phases, vec!["execute", "code-review"]);
    }

    #[test]
    fn test_resolve_task_phases_no_override() {
        let pipeline = default_pipeline("execute");
        let phases = resolve_task_phases(&pipeline, None);
        assert_eq!(phases, vec!["execute", "task-verify"]);
    }

    #[test]
    fn test_resolve_task_phases_with_override() {
        let pipeline = default_pipeline("execute");
        let override_phases = vec!["execute".to_string()];
        let phases = resolve_task_phases(&pipeline, Some(&override_phases));
        assert_eq!(phases, vec!["execute"]);
    }

    // --- Step 6: PhaseOutcome + build_phase_prompt + parse_phase_output tests ---

    #[test]
    fn test_build_phase_prompt_with_template() {
        let phase = PhaseConfig {
            name: "critic".into(),
            level: PhaseLevel::Spec,
            description: "Test".into(),
            prompt_template: "Review this spec carefully.".into(),
            timeout_minutes: None,
            retry_count: None,
            can_add_tasks: false,
            can_fail_spec: false,
            requires_claude: true,
            approve_signal: None,
            reject_signal: None,
            on_approve: None,
            on_reject: None,
            on_crash: None,
            min_lines_changed: None,
        };
        let prompt = build_phase_prompt(&phase, "title: Test\ntasks: []", None);
        assert!(prompt.contains("Review this spec carefully."));
        assert!(prompt.contains("--- SPEC ---"));
        assert!(prompt.contains("title: Test"));
    }

    #[test]
    fn test_build_phase_prompt_with_task_context() {
        let phase = PhaseConfig {
            name: "code-review".into(),
            level: PhaseLevel::Task,
            description: "".into(),
            prompt_template: "Review code.".into(),
            timeout_minutes: None,
            retry_count: None,
            can_add_tasks: false,
            can_fail_spec: false,
            requires_claude: true,
            approve_signal: None,
            reject_signal: None,
            on_approve: None,
            on_reject: None,
            on_crash: None,
            min_lines_changed: None,
        };
        let prompt = build_phase_prompt(&phase, "spec content", Some("task t-1 details"));
        assert!(prompt.contains("--- TASK ---"));
        assert!(prompt.contains("task t-1 details"));
    }

    #[test]
    fn test_build_phase_prompt_empty_template() {
        let phase = PhaseConfig {
            name: "task-verify".into(),
            level: PhaseLevel::Task,
            description: "".into(),
            prompt_template: String::new(),
            timeout_minutes: None,
            retry_count: None,
            can_add_tasks: false,
            can_fail_spec: false,
            requires_claude: false,
            approve_signal: None,
            reject_signal: None,
            on_approve: None,
            on_reject: None,
            on_crash: None,
            min_lines_changed: None,
        };
        let prompt = build_phase_prompt(&phase, "spec", None);
        assert!(prompt.contains("Phase: task-verify"));
        assert!(prompt.contains("spec"));
    }

    #[test]
    fn test_parse_phase_output_approved() {
        let registry = PhaseRegistry::new();
        let critic = registry.get("critic").unwrap();
        let outcome = parse_phase_output(critic, "Everything looks good.\n\n## Critic Approved\n");
        assert_eq!(outcome, PhaseOutcome::Approved);
    }

    #[test]
    fn test_parse_phase_output_rejected_with_requeue() {
        let registry = PhaseRegistry::new();
        let critic = registry.get("critic").unwrap();
        let outcome = parse_phase_output(
            critic,
            "[CRITIC] Missing error handling in parse_spec()\n[CRITIC] Dead code in worker.rs",
        );
        assert_eq!(
            outcome,
            PhaseOutcome::Requeue {
                phase: "execute".into()
            }
        );
    }

    #[test]
    fn test_parse_phase_output_no_signals() {
        let phase = PhaseConfig {
            name: "execute".into(),
            level: PhaseLevel::Task,
            description: "".into(),
            prompt_template: String::new(),
            timeout_minutes: None,
            retry_count: None,
            can_add_tasks: false,
            can_fail_spec: false,
            requires_claude: true,
            approve_signal: None,
            reject_signal: None,
            on_approve: None,
            on_reject: None,
            on_crash: None,
            min_lines_changed: None,
        };
        let outcome = parse_phase_output(&phase, "Task completed successfully.");
        assert_eq!(outcome, PhaseOutcome::Approved);
    }

    #[test]
    fn test_parse_phase_output_plan_critique_rejected() {
        let registry = PhaseRegistry::new();
        let pc = registry.get("plan-critique").unwrap();
        let outcome = parse_phase_output(pc, "[PLAN] Task t-3 has unrealistic dependency");
        assert_eq!(
            outcome,
            PhaseOutcome::Requeue {
                phase: "plan-critique".into()
            }
        );
    }

    #[test]
    fn test_parse_phase_output_reject_without_requeue_action() {
        let phase = PhaseConfig {
            name: "custom".into(),
            level: PhaseLevel::Task,
            description: "".into(),
            prompt_template: String::new(),
            timeout_minutes: None,
            retry_count: None,
            can_add_tasks: false,
            can_fail_spec: true,
            requires_claude: true,
            approve_signal: Some("## OK".into()),
            reject_signal: Some("[FAIL]".into()),
            on_approve: None,
            on_reject: None, // no requeue action
            on_crash: None,
            min_lines_changed: None,
        };
        let outcome = parse_phase_output(&phase, "Found issue: [FAIL] bad code");
        match outcome {
            PhaseOutcome::Failed { reason } => {
                assert!(reason.contains("[FAIL]"));
            }
            other => panic!("Expected Failed, got {:?}", other),
        }
    }
}
