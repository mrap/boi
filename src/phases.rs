use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseConfig {
    pub name: String,
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
    ]
}

pub fn user_phases_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".boi").join("phases")
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
name = "code-review"
description = "4-persona code review"

[phase]
name = "code-review"
description = "4-persona code review"
can_add_tasks = false
can_fail_spec = true
requires_claude = true

[completion]
approve_signal = "## Code Review Approved"
reject_signal = "[CODE-REVIEW]"
on_approve = "next"
on_reject = "requeue:execute"

[trigger]
min_lines_changed = 50

[prompt]
template = "Review the code changes."
"###;
        fs::write(dir.join("code-review.phase.toml"), toml_content).unwrap();

        let mut registry = PhaseRegistry::new();
        registry.load_user_phases(&dir);

        let cr = registry.get("code-review").unwrap();
        assert_eq!(cr.description, "4-persona code review");
        assert!(cr.can_fail_spec);
        assert_eq!(cr.reject_signal.as_deref(), Some("[CODE-REVIEW]"));
        assert_eq!(cr.min_lines_changed, Some(50));
        assert!(!registry.is_user_override("code-review"));

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
        assert_eq!(core.len(), 4);
        assert!(core.contains(&"execute"));

        let user = registry.user_names();
        assert!(user.is_empty());
    }

    #[test]
    fn test_load_nonexistent_dir() {
        let mut registry = PhaseRegistry::new();
        registry.load_user_phases(Path::new("/tmp/boi-nonexistent-dir-xyz"));
        assert_eq!(registry.list().len(), 4);
    }
}
