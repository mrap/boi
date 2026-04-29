use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Canonical template variable keys for phase prompts.
/// Using this enum prevents typos and makes it easy to find all usages.
pub enum TemplateVar {
    QueueId,
    SpecPath,
    Iteration,
    PendingCount,
    SpecContent,
    WorkspaceHeader,
    SpecContext,
    TaskTitle,
    TaskSpec,
    TaskVerify,
    TaskDepends,
}

impl TemplateVar {
    pub fn key(&self) -> &'static str {
        match self {
            Self::QueueId => "QUEUE_ID",
            Self::SpecPath => "SPEC_PATH",
            Self::Iteration => "ITERATION",
            Self::PendingCount => "PENDING_COUNT",
            Self::SpecContent => "SPEC_CONTENT",
            Self::WorkspaceHeader => "WORKSPACE_HEADER",
            Self::SpecContext => "SPEC_CONTEXT",
            Self::TaskTitle => "TASK_TITLE",
            Self::TaskSpec => "TASK_SPEC",
            Self::TaskVerify => "TASK_VERIFY",
            Self::TaskDepends => "TASK_DEPENDS",
        }
    }

    /// Required vars that must be present for a valid prompt.
    pub fn required() -> &'static [TemplateVar] {
        &[Self::QueueId]
    }

    /// Validate that all required vars are present. Returns an error if any are missing.
    pub fn validate(vars: &HashMap<String, String>) -> Result<(), String> {
        for v in Self::required() {
            if !vars.contains_key(v.key()) {
                return Err(format!("[boi] required template var '{}' missing from prompt_vars", v.key()));
            }
        }
        Ok(())
    }
}

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
    /// Worker runtime: "claude", "deterministic", or None (defaults to "claude").
    pub runtime: Option<String>,
    /// Builtin handler name for deterministic phases, e.g. "builtin:commit".
    pub completion_handler: Option<String>,
    pub approve_signal: Option<String>,
    pub reject_signal: Option<String>,
    pub on_approve: Option<String>,
    pub on_reject: Option<String>,
    pub on_crash: Option<String>,
    pub min_lines_changed: Option<u32>,
    pub model: Option<String>,
    pub code_model: Option<String>,
    pub effort: Option<String>,
    pub hooks_pre: Vec<String>,
    pub hooks_post: Vec<String>,
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
    #[serde(default)]
    hooks: Option<HooksSection>,
}

#[derive(Debug, Deserialize)]
struct PhaseSection {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    level: Option<PhaseLevel>,
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

#[derive(Debug, Deserialize)]
struct HooksSection {
    #[serde(default)]
    pre: Option<Vec<String>>,
    #[serde(default)]
    post: Option<Vec<String>>,
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

        // Read level from [phase] section; fall back to name-based derivation for
        // user-created phases that don't specify level.
        let level = toml.phase.as_ref().and_then(|p| p.level)
            .unwrap_or_else(|| derive_level(&name));

        // Derive can_add_tasks: explicit [phase] setting wins, else derive from completion_handler
        let can_add_tasks = toml
            .phase.as_ref().and_then(|p| p.can_add_tasks)
            .unwrap_or_else(|| derive_can_add_tasks(&name, toml.completion_handler.as_deref()));

        // Derive can_fail_spec: explicit [phase] setting wins, else derive from name
        let can_fail_spec = toml
            .phase.as_ref().and_then(|p| p.can_fail_spec)
            .unwrap_or_else(|| derive_can_fail_spec(&name));

        let runtime = toml.worker.as_ref().and_then(|w| w.runtime.clone());
        let completion_handler = toml.completion_handler.clone();

        // Derive requires_claude: explicit [phase] setting wins, else derive from worker.runtime.
        // "deterministic" and any non-"claude" value → false.
        let requires_claude = toml
            .phase.as_ref().and_then(|p| p.requires_claude)
            .unwrap_or_else(|| {
                runtime.as_deref()
                    .map(|r| r == "claude")
                    .unwrap_or(true)
            });

        let completion = toml.completion.as_ref();
        let approve_signal = completion.and_then(|c| non_empty(&c.approve_signal));
        let reject_signal = completion.and_then(|c| non_empty(&c.reject_signal));
        let on_approve = completion.and_then(|c| c.on_approve.clone());
        let on_reject = completion.and_then(|c| c.on_reject.clone());
        let on_crash = completion.and_then(|c| c.on_crash.clone());
        let min_lines_changed = toml.trigger.as_ref().and_then(|t| t.min_lines_changed);
        let model = toml.worker.as_ref().and_then(|w| w.model.clone());
        let code_model = toml.worker.as_ref().and_then(|w| w.code_model.clone());
        let effort = toml.worker.as_ref().and_then(|w| w.effort.clone());
        let hooks_pre = toml.hooks.as_ref().and_then(|h| h.pre.clone()).unwrap_or_default();
        let hooks_post = toml.hooks.as_ref().and_then(|h| h.post.clone()).unwrap_or_default();

        Some(PhaseConfig {
            name,
            level,
            description,
            prompt_template,
            timeout_minutes,
            retry_count: None,
            can_add_tasks,
            can_fail_spec,
            requires_claude,
            runtime,
            completion_handler,
            approve_signal,
            reject_signal,
            on_approve,
            on_reject,
            on_crash,
            min_lines_changed,
            model,
            code_model,
            effort,
            hooks_pre,
            hooks_post,
        })
    }
}

/// Derive phase level from name. Spec-level phases: plan-critique, critic, evaluate, review, spec-review/spec-critique/spec-improve.
fn derive_level(name: &str) -> PhaseLevel {
    match name {
        "plan-critique" | "critic" | "evaluate" | "review"
        | "spec-review" | "spec-critique" | "spec-improve" => PhaseLevel::Spec,
        _ => PhaseLevel::Task,
    }
}

/// Derive can_add_tasks from completion_handler or name.
fn derive_can_add_tasks(name: &str, completion_handler: Option<&str>) -> bool {
    if let Some(handler) = completion_handler {
        if handler == "builtin:decompose" {
            return true;
        }
    }
    // Phases that structurally add tasks: critic, decompose, evaluate, plan-critique, code-review, review, spec-review/spec-critique
    matches!(name, "critic" | "decompose" | "evaluate" | "plan-critique" | "code-review" | "review" | "spec-review" | "spec-critique")
}

/// Derive can_fail_spec from name.
fn derive_can_fail_spec(name: &str) -> bool {
    matches!(name, "plan-critique" | "critic")
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
        let phases = match core_phases_dir() {
            Some(phases_dir) => {
                let loaded = load_phases_from_dir(&phases_dir);
                if loaded.is_empty() {
                    fallback_core_phases()
                } else {
                    loaded
                }
            }
            None => fallback_core_phases(),
        };

        let mut core = HashMap::new();
        for phase in phases {
            core.insert(phase.name.clone(), phase);
        }
        let mut registry = PhaseRegistry {
            core,
            user: HashMap::new(),
        };

        let user_dir = user_phases_dir();
        if user_dir.is_dir() {
            registry.load_user_phases(&user_dir);
        }

        registry
    }

    /// Create a registry loading phases from a specific directory.
    /// The directory should contain *.phase.toml files directly.
    pub fn from_dir(phases_dir: &Path) -> Self {
        let phases = load_phases_from_dir(phases_dir);
        let phases = if phases.is_empty() {
            fallback_core_phases()
        } else {
            phases
        };
        let mut core = HashMap::new();
        for phase in phases {
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
        // "spec-review" is a backward-compat alias for "spec-critique".
        let resolved = if name == "spec-review" { "spec-critique" } else { name };
        self.user.get(resolved).or_else(|| self.core.get(resolved))
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

    /// Apply per-phase model overrides from config.yaml.
    /// config.models entries override the TOML's model field at runtime.
    pub fn apply_model_overrides(&mut self, models: &HashMap<String, String>) {
        for (phase_name, model) in models {
            let resolved = if phase_name == "spec-review" { "spec-critique" } else { phase_name.as_str() };
            if let Some(phase) = self.core.get_mut(resolved) {
                phase.model = Some(model.clone());
            }
            if let Some(phase) = self.user.get_mut(resolved) {
                phase.model = Some(model.clone());
            }
        }
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
    /// Legacy: phases run once for the whole spec. If spec_post_phases is empty,
    /// spec_phases is used as spec_post_phases (backward compat).
    #[serde(default)]
    pub spec_phases: Vec<String>,
    /// Phases that run before task execution (spec-pre loop, e.g. spec-critique ↔ spec-improve).
    #[serde(default)]
    pub spec_pre_phases: Vec<String>,
    /// Phases that run after all tasks complete (e.g. doc-update, critic, merge, cleanup).
    #[serde(default)]
    pub spec_post_phases: Vec<String>,
    /// Phases that run for each individual task.
    #[serde(default)]
    pub task_phases: Vec<String>,
    /// Max iterations for the spec-pre loop before proceeding to task execution.
    #[serde(default = "default_max_loops")]
    pub max_loops: u32,
}

fn default_max_loops() -> u32 {
    3
}

#[derive(Debug, Deserialize)]
struct PipelinesToml {
    mode: HashMap<String, PipelineModeToml>,
}

#[derive(Debug, Deserialize)]
struct PipelineModeToml {
    #[serde(default)]
    spec_phases: Vec<String>,
    #[serde(default)]
    spec_pre_phases: Vec<String>,
    #[serde(default)]
    spec_post_phases: Vec<String>,
    #[serde(default)]
    task_phases: Vec<String>,
    #[serde(default = "default_max_loops")]
    max_loops: u32,
}

/// Find the pipelines.toml file.
/// Priority: BOI_PIPELINES_FILE env > ~/.boi/pipelines.toml > None
fn find_pipelines_file() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("BOI_PIPELINES_FILE") {
        let p = PathBuf::from(&path);
        if p.is_file() {
            return Some(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let user_path = PathBuf::from(&home).join(".boi").join("pipelines.toml");
    if user_path.is_file() {
        return Some(user_path);
    }
    None
}

fn load_pipeline_from_file(path: &Path, mode: &str) -> Option<PipelineConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    let parsed: PipelinesToml = toml::from_str(&content).ok()?;
    let key = match mode {
        "execute" | "" => "default",
        other => other,
    };
    parsed.mode.get(key).map(|m| {
        // Backward compat: if spec_post_phases not provided, use spec_phases as spec_post_phases.
        let spec_post = if !m.spec_post_phases.is_empty() {
            m.spec_post_phases.clone()
        } else {
            m.spec_phases.clone()
        };
        PipelineConfig {
            spec_phases: m.spec_phases.clone(),
            spec_pre_phases: m.spec_pre_phases.clone(),
            spec_post_phases: spec_post,
            task_phases: m.task_phases.clone(),
            max_loops: m.max_loops,
        }
    })
}

/// Returns the default pipeline for a given spec mode.
/// Loads from pipelines.toml (BOI_PIPELINES_FILE env > ~/.boi/pipelines.toml),
/// falling back to hardcoded defaults if no file found.
pub fn default_pipeline(mode: &str) -> PipelineConfig {
    if let Some(path) = find_pipelines_file() {
        if let Some(config) = load_pipeline_from_file(&path, mode) {
            return config;
        }
    }
    fallback_pipeline(mode)
}

pub(crate) fn fallback_pipeline(mode: &str) -> PipelineConfig {
    match mode {
        "execute" => PipelineConfig {
            spec_phases: vec!["spec-review".into(), "critic".into()],
            spec_pre_phases: vec![],
            spec_post_phases: vec!["spec-review".into(), "critic".into()],
            task_phases: vec!["execute".into(), "task-verify".into()],
            max_loops: 3,
        },
        "challenge" => PipelineConfig {
            spec_phases: vec!["spec-review".into(), "plan-critique".into(), "critic".into()],
            spec_pre_phases: vec![],
            spec_post_phases: vec!["spec-review".into(), "plan-critique".into(), "critic".into()],
            task_phases: vec!["execute".into(), "task-verify".into()],
            max_loops: 3,
        },
        "discover" => PipelineConfig {
            spec_phases: vec!["spec-review".into(), "critic".into(), "evaluate".into()],
            spec_pre_phases: vec![],
            spec_post_phases: vec!["spec-review".into(), "critic".into(), "evaluate".into()],
            task_phases: vec!["execute".into(), "task-verify".into()],
            max_loops: 3,
        },
        "generate" => PipelineConfig {
            spec_phases: vec!["spec-review".into(), "critic".into(), "evaluate".into()],
            spec_pre_phases: vec![],
            spec_post_phases: vec!["spec-review".into(), "critic".into(), "evaluate".into()],
            task_phases: vec!["decompose".into(), "execute".into(), "code-review".into(), "task-verify".into()],
            max_loops: 3,
        },
        _ => PipelineConfig {
            spec_phases: vec![],
            spec_pre_phases: vec![],
            spec_post_phases: vec![],
            task_phases: vec!["execute".into()],
            max_loops: 3,
        },
    }
}

fn load_phase_file(path: &Path) -> Result<PhaseConfig, Box<dyn std::error::Error>> {
    load_phase_file_with_base(path, None)
}

/// Load a phase TOML file, optionally resolving prompt_template paths relative to base_dir.
fn load_phase_file_with_base(path: &Path, base_dir: Option<&Path>) -> Result<PhaseConfig, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let toml_parsed: PhaseToml = toml::from_str(&content)?;
    let mut phase = PhaseConfig::from_toml(toml_parsed).ok_or_else(|| {
        format!("phase file missing name: {}", path.display())
    })?;

    // If prompt_template is a file path (not inline content), resolve and read it.
    // Search order: ~/.boi/ (user override) → repo root (default)
    if !phase.prompt_template.is_empty()
        && !phase.prompt_template.contains('\n')
        && phase.prompt_template.ends_with(".md")
    {
        let template_ref = &phase.prompt_template;
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let user_path = PathBuf::from(&home).join(".boi").join(template_ref);
        let repo_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(template_ref);
        let base_path = base_dir.map(|b| b.join(template_ref));

        let resolved = if user_path.is_file() {
            Some(user_path)
        } else if repo_path.is_file() {
            Some(repo_path)
        } else if let Some(ref bp) = base_path {
            if bp.is_file() { Some(bp.clone()) } else { None }
        } else {
            None
        };

        if let Some(template_path) = resolved {
            match std::fs::read_to_string(&template_path) {
                Ok(template_content) => {
                    phase.prompt_template = template_content;
                }
                Err(e) => {
                    eprintln!(
                        "WARN: failed to read prompt template {}: {}",
                        template_path.display(),
                        e
                    );
                }
            }
        }
    }

    Ok(phase)
}

/// Determine the core phases directory from the repo.
///
/// Priority:
/// 1. `BOI_PHASES_DIR` env var (tests, development)
/// 2. Repo root compiled at build time via CARGO_MANIFEST_DIR
/// 3. None (use fallback defaults)
///
/// ~/.boi/phases/ is NOT checked here — it's loaded separately as user
/// overrides via load_user_phases() in PhaseRegistry::new().
fn core_phases_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("BOI_PHASES_DIR") {
        let path = PathBuf::from(&dir);
        if path.is_dir() {
            return Some(path);
        }
    }

    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_phases = repo_root.join("phases");
    if repo_phases.is_dir() {
        return Some(repo_phases);
    }

    None
}


/// Load core phases from TOML files in the phases/ directory.
/// Returns empty vec if no directory found (caller should use fallback).
fn load_phases_from_dir(phases_dir: &Path) -> Vec<PhaseConfig> {
    if !phases_dir.is_dir() {
        return Vec::new();
    }

    let mut phases = Vec::new();
    let patterns = [
        phases_dir.join("*.phase.toml"),
        phases_dir.join("*.toml"),
    ];
    let mut seen = std::collections::HashSet::new();

    for pattern in &patterns {
        let pat = pattern.to_string_lossy();
        if let Ok(entries) = glob::glob(&pat) {
            for entry in entries.flatten() {
                if !seen.insert(entry.clone()) {
                    continue;
                }
                match load_phase_file_with_base(&entry, phases_dir.parent()) {
                    Ok(phase) => {
                        phases.push(phase);
                    }
                    Err(e) => {
                        eprintln!(
                            "WARN: failed to load core phase {}: {}",
                            entry.display(),
                            e
                        );
                    }
                }
            }
        }
    }

    phases
}

/// Minimal fallback phases when no TOML files are found (fresh install, tests).
/// Just "execute" and "task-verify" — enough to work without files.
fn fallback_core_phases() -> Vec<PhaseConfig> {
    vec![
        PhaseConfig {
            name: "execute".into(),
            level: PhaseLevel::Task,
            description: "Execute tasks from the spec".into(),
            prompt_template: String::new(),
            timeout_minutes: Some(10),
            retry_count: None,
            can_add_tasks: false,
            can_fail_spec: false,
            requires_claude: true,
            runtime: Some("claude".into()),
            completion_handler: None,
            approve_signal: None,
            reject_signal: None,
            on_approve: None,
            on_reject: None,
            on_crash: None,
            min_lines_changed: None,
            model: None,
            code_model: None,
            effort: None,
            hooks_pre: vec![],
            hooks_post: vec![],
        },
        PhaseConfig {
            name: "task-verify".into(),
            level: PhaseLevel::Task,
            description: "Run verification commands for a task".into(),
            prompt_template: String::new(),
            timeout_minutes: Some(5),
            retry_count: None,
            can_add_tasks: false,
            can_fail_spec: false,
            requires_claude: false,
            runtime: None,
            completion_handler: None,
            approve_signal: None,
            reject_signal: None,
            on_approve: Some("next".into()),
            on_reject: Some("requeue:execute".into()),
            on_crash: None,
            min_lines_changed: None,
            model: None,
            code_model: None,
            effort: None,
            hooks_pre: vec![],
            hooks_post: vec![],
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
    let resolved_spec = spec_phases
        .map(|v| v.to_vec())
        .unwrap_or(defaults.spec_phases.clone());
    // When spec_phases is overridden directly, treat it as spec_post_phases (backward compat).
    let resolved_post = if spec_phases.is_some() {
        resolved_spec.clone()
    } else {
        defaults.spec_post_phases
    };
    PipelineConfig {
        spec_phases: resolved_spec,
        spec_pre_phases: defaults.spec_pre_phases,
        spec_post_phases: resolved_post,
        task_phases: task_phases
            .map(|v| v.to_vec())
            .unwrap_or(defaults.task_phases),
        max_loops: defaults.max_loops,
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

/// The control flow decision from a phase. Metadata/findings go into telemetry separately.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Move to next phase or next task
    Proceed,
    /// Go back to TaskSelect. Optional new tasks to add first.
    Redo { tasks: Vec<crate::spec::BoiTask> },
    /// Pause spec, wait for human input via `boi decide <id>`
    Pause { prompt: String },
    /// End the spec
    Done { success: bool, reason: String },
}

/// Build a prompt for a spec-level phase.
pub fn build_phase_prompt(
    phase: &PhaseConfig,
    spec_content: &str,
    task_context: Option<&str>,
    vars: &std::collections::HashMap<String, String>,
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

    // Substitute template variables
    for (key, value) in vars {
        prompt = prompt.replace(&format!("{{{{{}}}}}", key), value);
    }
    // Strip any remaining unsubstituted {{VAR}}
    while let Some(start) = prompt.find("{{") {
        if let Some(end) = prompt[start..].find("}}") {
            prompt.replace_range(start..start + end + 2, "");
        } else {
            break;
        }
    }

    prompt.push_str("\n\n--- SPEC ---\n");
    prompt.push_str(spec_content);
    if let Some(ctx) = task_context {
        prompt.push_str("\n\n--- TASK ---\n");
        prompt.push_str(ctx);
    }
    prompt
}

/// Parse phase output to determine the verdict.
pub fn parse_phase_output(phase: &PhaseConfig, output: &str) -> Verdict {
    // Check for approve signal first
    if let Some(ref signal) = phase.approve_signal {
        if output.contains(signal) {
            return Verdict::Proceed;
        }
    }

    // Check for reject signal
    if let Some(ref signal) = phase.reject_signal {
        if output.contains(signal) {
            // Determine action from on_reject
            if let Some(ref action) = phase.on_reject {
                if action.starts_with("requeue:") {
                    return Verdict::Redo { tasks: vec![] };
                }
            }
            return Verdict::Done {
                success: false,
                reason: format!("Phase {} rejected: found '{}'", phase.name, signal),
            };
        }
    }

    // No explicit signals — treat as proceed (permissive default)
    Verdict::Proceed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils;
    use std::fs;

    /// Find the BOI repo root directory for tests.
    /// Uses CARGO_MANIFEST_DIR which points to the crate root during `cargo test`.
    fn repo_root() -> PathBuf {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        if manifest.join("phases").is_dir() {
            return manifest;
        }
        let mut dir = manifest.clone();
        for _ in 0..5 {
            if dir.join("phases").is_dir() {
                return dir;
            }
            if !dir.pop() {
                break;
            }
        }
        manifest
    }

    /// Build a PhaseRegistry from the repo's TOML phase files.
    fn test_registry() -> PhaseRegistry {
        PhaseRegistry::from_dir(&repo_root().join("phases"))
    }

    #[test]
    fn test_core_phases_exist() {
        let registry = test_registry();
        assert!(registry.get("execute").is_some());
        assert!(registry.get("critic").is_some());
        assert!(registry.get("decompose").is_some());
        assert!(registry.get("evaluate").is_some());
    }

    #[test]
    fn test_core_phase_properties() {
        let registry = test_registry();

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

        // spec-critique is the canonical name; spec-review is a backward-compat alias.
        // spec-critique rejects+requeues rather than adding tasks directly.
        let spec_critique = registry.get("spec-critique").unwrap();
        assert!(!spec_critique.can_add_tasks);
        assert!(!spec_critique.can_fail_spec);
        assert!(spec_critique.requires_claude);

        // alias still resolves
        let via_alias = registry.get("spec-review").unwrap();
        assert_eq!(via_alias.name, "spec-critique");
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
        let registry = test_registry();
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn test_user_phase_override() {
        let dir = test_utils::test_dir("phase-override");


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

        let mut registry = test_registry();
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
        let dir = test_utils::test_dir("phase-new");


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

        let mut registry = test_registry();
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
        let dir = test_utils::test_dir("phase-repo");

        fs::write(dir.join("execute.phase.toml"), toml_content).unwrap();

        let mut registry = test_registry();
        registry.load_user_phases(&dir);

        let exec = registry.get("execute").unwrap();
        assert_eq!(exec.name, "execute");
        assert!(exec.prompt_template.contains("BOI Worker"), "template should be resolved, got: {}...", &exec.prompt_template[..80.min(exec.prompt_template.len())]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_list_returns_merged() {
        let registry = test_registry();
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
        let registry = test_registry();
        let core = registry.core_names();
        assert_eq!(core.len(), 14);
        assert!(core.contains(&"execute"));
        assert!(core.contains(&"plan-critique"));
        assert!(core.contains(&"code-review"));
        assert!(core.contains(&"task-verify"));
        assert!(core.contains(&"spec-critique"));
        assert!(core.contains(&"spec-improve"));
        assert!(core.contains(&"commit"));
        assert!(core.contains(&"merge"));
        assert!(core.contains(&"cleanup"));

        let user = registry.user_names();
        assert!(user.is_empty());
    }

    #[test]
    fn test_load_nonexistent_dir() {
        let mut registry = test_registry();
        let nonexistent = test_utils::test_file("nonexistent-dir", "xyz");
        let _ = std::fs::remove_file(&nonexistent);
        registry.load_user_phases(&nonexistent);
        assert_eq!(registry.list().len(), 14);
    }

    // --- Step 1: PhaseLevel tests ---

    #[test]
    fn test_phase_level_defaults_to_task() {
        assert_eq!(PhaseLevel::default(), PhaseLevel::Task);
    }

    #[test]
    fn test_core_phases_have_correct_levels() {
        let registry = test_registry();

        // Spec-level phases
        assert_eq!(registry.get("critic").unwrap().level, PhaseLevel::Spec);
        assert_eq!(registry.get("evaluate").unwrap().level, PhaseLevel::Spec);
        assert_eq!(registry.get("plan-critique").unwrap().level, PhaseLevel::Spec);
        assert_eq!(registry.get("spec-critique").unwrap().level, PhaseLevel::Spec);
        assert_eq!(registry.get("spec-review").unwrap().level, PhaseLevel::Spec); // alias
        assert_eq!(registry.get("spec-improve").unwrap().level, PhaseLevel::Spec);

        // Task-level phases
        assert_eq!(registry.get("execute").unwrap().level, PhaseLevel::Task);
        assert_eq!(registry.get("decompose").unwrap().level, PhaseLevel::Task);
        assert_eq!(registry.get("code-review").unwrap().level, PhaseLevel::Task);
        assert_eq!(registry.get("task-verify").unwrap().level, PhaseLevel::Task);
    }

    // --- Step 2: PipelineConfig tests ---

    #[test]
    fn test_default_pipeline_execute() {
        let p = fallback_pipeline("execute");
        assert_eq!(p.spec_phases, vec!["spec-review", "critic"]);
        assert_eq!(p.task_phases, vec!["execute", "task-verify"]);
    }

    #[test]
    fn test_default_pipeline_challenge() {
        let p = fallback_pipeline("challenge");
        assert_eq!(p.spec_phases, vec!["spec-review", "plan-critique", "critic"]);
        assert_eq!(p.task_phases, vec!["execute", "task-verify"]);
    }

    #[test]
    fn test_default_pipeline_discover() {
        let p = fallback_pipeline("discover");
        assert_eq!(p.spec_phases, vec!["spec-review", "critic", "evaluate"]);
        assert_eq!(p.task_phases, vec!["execute", "task-verify"]);
    }

    #[test]
    fn test_default_pipeline_generate() {
        let p = fallback_pipeline("generate");
        assert_eq!(p.spec_phases, vec!["spec-review", "critic", "evaluate"]);
        assert_eq!(p.task_phases, vec!["decompose", "execute", "code-review", "task-verify"]);
    }

    #[test]
    fn test_default_pipeline_unknown_mode() {
        let p = fallback_pipeline("unknown");
        assert!(p.spec_phases.is_empty());
        assert_eq!(p.task_phases, vec!["execute"]);
    }

    #[test]
    fn test_spec_review_is_first_spec_phase() {
        for mode in &["execute", "challenge", "discover", "generate"] {
            let p = fallback_pipeline(mode);
            assert_eq!(
                p.spec_phases.first().map(|s| s.as_str()),
                Some("spec-review"),
                "spec-review must be first spec phase in mode '{mode}'"
            );
        }
    }

    // --- Step 3: New core phases tests ---

    #[test]
    fn test_plan_critique_phase() {
        let registry = test_registry();
        let pc = registry.get("plan-critique").unwrap();
        assert_eq!(pc.level, PhaseLevel::Spec);
        assert!(pc.can_add_tasks);
        assert!(pc.can_fail_spec);
        assert!(pc.requires_claude);
        assert_eq!(pc.approve_signal.as_deref(), Some("## Plan Approved"));
        assert_eq!(pc.reject_signal.as_deref(), Some("[PLAN-CRITIQUE]"));
    }

    #[test]
    fn test_code_review_phase() {
        let registry = test_registry();
        let cr = registry.get("code-review").unwrap();
        assert_eq!(cr.level, PhaseLevel::Task);
        assert!(cr.can_add_tasks);
        assert!(!cr.can_fail_spec);
        assert!(cr.requires_claude);
        assert_eq!(cr.approve_signal.as_deref(), Some("## Code Review Approved"));
        assert_eq!(cr.min_lines_changed, Some(50));
    }

    #[test]
    fn test_task_verify_phase() {
        let registry = test_registry();
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
        let expected = default_pipeline("execute");
        assert_eq!(p.spec_phases, expected.spec_phases);
        assert_eq!(p.task_phases, expected.task_phases);
    }

    #[test]
    fn test_resolve_pipeline_spec_override() {
        let spec_override = vec!["plan-critique".to_string(), "critic".to_string()];
        let p = resolve_pipeline("execute", Some(&spec_override), None);
        assert_eq!(p.spec_phases, vec!["plan-critique", "critic"]);
        // task_phases unchanged — must match the default for this environment
        let default = default_pipeline("execute");
        assert_eq!(p.task_phases, default.task_phases);
    }

    #[test]
    fn test_resolve_pipeline_task_override() {
        let task_override = vec!["execute".to_string()];
        let p = resolve_pipeline("challenge", None, Some(&task_override));
        // spec_phases unchanged — must match the default for this environment
        let default = default_pipeline("challenge");
        assert_eq!(p.spec_phases, default.spec_phases);
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
        assert_eq!(phases, pipeline.task_phases);
    }

    #[test]
    fn test_resolve_task_phases_with_override() {
        let pipeline = default_pipeline("execute");
        let override_phases = vec!["execute".to_string()];
        let phases = resolve_task_phases(&pipeline, Some(&override_phases));
        assert_eq!(phases, vec!["execute"]);
    }

    // --- Step 6: Verdict + build_phase_prompt + parse_phase_output tests ---

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
            runtime: None,
            completion_handler: None,
            approve_signal: None,
            reject_signal: None,
            on_approve: None,
            on_reject: None,
            on_crash: None,
            min_lines_changed: None,
            model: None,
            code_model: None,
            effort: None,
            hooks_pre: vec![],
            hooks_post: vec![],
        };
        let prompt = build_phase_prompt(&phase, "title: Test\ntasks: []", None, &std::collections::HashMap::new());
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
            runtime: None,
            completion_handler: None,
            approve_signal: None,
            reject_signal: None,
            on_approve: None,
            on_reject: None,
            on_crash: None,
            min_lines_changed: None,
            model: None,
            code_model: None,
            effort: None,
            hooks_pre: vec![],
            hooks_post: vec![],
        };
        let prompt = build_phase_prompt(&phase, "spec content", Some("task t-1 details"), &std::collections::HashMap::new());
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
            runtime: None,
            completion_handler: None,
            approve_signal: None,
            reject_signal: None,
            on_approve: None,
            on_reject: None,
            on_crash: None,
            min_lines_changed: None,
            model: None,
            code_model: None,
            effort: None,
            hooks_pre: vec![],
            hooks_post: vec![],
        };
        let prompt = build_phase_prompt(&phase, "spec", None, &std::collections::HashMap::new());
        assert!(prompt.contains("Phase: task-verify"));
        assert!(prompt.contains("spec"));
    }

    #[test]
    fn test_parse_phase_output_approved() {
        let registry = test_registry();
        let critic = registry.get("critic").unwrap();
        let outcome = parse_phase_output(critic, "Everything looks good.\n\n## Critic Approved\n");
        assert_eq!(outcome, Verdict::Proceed);
    }

    #[test]
    fn test_parse_phase_output_rejected_with_requeue() {
        let registry = test_registry();
        let critic = registry.get("critic").unwrap();
        let outcome = parse_phase_output(
            critic,
            "[CRITIC] Missing error handling in parse_spec()\n[CRITIC] Dead code in worker.rs",
        );
        assert_eq!(
            outcome,
            Verdict::Redo { tasks: vec![] }
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
            runtime: None,
            completion_handler: None,
            approve_signal: None,
            reject_signal: None,
            on_approve: None,
            on_reject: None,
            on_crash: None,
            min_lines_changed: None,
            model: None,
            code_model: None,
            effort: None,
            hooks_pre: vec![],
            hooks_post: vec![],
        };
        let outcome = parse_phase_output(&phase, "Task completed successfully.");
        assert_eq!(outcome, Verdict::Proceed);
    }

    #[test]
    fn test_parse_phase_output_plan_critique_rejected() {
        let registry = test_registry();
        let pc = registry.get("plan-critique").unwrap();
        let outcome = parse_phase_output(pc, "[PLAN-CRITIQUE] Task t-3 has unrealistic dependency");
        // on_reject = "requeue:spec-review" → loops back instead of hard-failing
        assert_eq!(outcome, Verdict::Redo { tasks: vec![] });
    }

    #[test]
    fn test_apply_model_overrides() {
        let mut registry = test_registry();

        // Before override: execute phase may have a model or may not
        let before = registry.get("execute").unwrap().model.clone();

        let mut overrides = HashMap::new();
        overrides.insert("execute".to_string(), "claude-opus-4-7".to_string());
        overrides.insert("spec-review".to_string(), "claude-haiku-4-5-20251001".to_string());
        overrides.insert("nonexistent-phase".to_string(), "some-model".to_string()); // should not panic

        registry.apply_model_overrides(&overrides);

        assert_eq!(
            registry.get("execute").unwrap().model.as_deref(),
            Some("claude-opus-4-7"),
            "execute model should be overridden"
        );
        assert_eq!(
            registry.get("spec-review").unwrap().model.as_deref(),
            Some("claude-haiku-4-5-20251001"),
            "spec-review model should be overridden"
        );

        // Phase not in overrides should be unchanged
        let critic_model = registry.get("critic").unwrap().model.clone();
        // critic model should not have been touched by the overrides above
        let _ = (before, critic_model); // just ensure we can access them without panic
    }

    #[test]
    fn test_apply_model_overrides_preserves_toml_defaults_for_unmentioned_phases() {
        let mut registry = test_registry();
        let critic_model_before = registry.get("critic").unwrap().model.clone();

        let mut overrides = HashMap::new();
        overrides.insert("execute".to_string(), "claude-opus-4-7".to_string());
        registry.apply_model_overrides(&overrides);

        // critic model must not change
        assert_eq!(
            registry.get("critic").unwrap().model,
            critic_model_before,
            "phases not in overrides must keep their TOML model"
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
            runtime: None,
            completion_handler: None,
            approve_signal: Some("## OK".into()),
            reject_signal: Some("[FAIL]".into()),
            on_approve: None,
            on_reject: None, // no requeue action
            on_crash: None,
            min_lines_changed: None,
            model: None,
            code_model: None,
            effort: None,
            hooks_pre: vec![],
            hooks_post: vec![],
        };
        let outcome = parse_phase_output(&phase, "Found issue: [FAIL] bad code");
        match outcome {
            Verdict::Done { success, reason } => {
                assert!(!success);
                assert!(reason.contains("[FAIL]"));
            }
            other => panic!("Expected Done with success=false, got {:?}", other),
        }
    }

    // --- Fixture-based tests (t-4) ---

    #[test]
    fn test_spec_review_finds_issues() {
        let fixtures_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/phase_fixtures");
        let spec_content = fs::read_to_string(fixtures_dir.join("spec_with_issues.yaml"))
            .expect("spec_with_issues.yaml must exist");

        let registry = test_registry();
        // spec-review is now a backward-compat alias for spec-critique
        let spec_critique = registry.get("spec-review").expect("spec-review alias must exist");

        let prompt = build_phase_prompt(spec_critique, &spec_content, None, &std::collections::HashMap::new());

        assert!(prompt.contains("Set up CSV ingestion"), "prompt must contain task title from fixture");
        assert!(prompt.contains("Optimize database writes"), "prompt must contain second task title");
        assert!(
            prompt.to_lowercase().contains("verify"),
            "spec-critique prompt must reference verify command validation"
        );
    }

    #[test]
    fn test_phase_level_from_toml() {
        let registry = PhaseRegistry::from_dir(&repo_root().join("phases"));

        // spec-review resolves via alias to spec-critique
        let spec_phases = ["spec-review", "spec-critique", "spec-improve", "plan-critique", "critic", "evaluate", "review"];
        for name in &spec_phases {
            let phase = registry.get(name).unwrap_or_else(|| panic!("phase '{name}' not found"));
            assert_eq!(
                phase.level,
                PhaseLevel::Spec,
                "phase '{name}' should be Spec-level (from TOML level field)"
            );
        }

        let task_phases = ["execute", "task-verify", "code-review", "doc-update", "decompose"];
        for name in &task_phases {
            let phase = registry.get(name).unwrap_or_else(|| panic!("phase '{name}' not found"));
            assert_eq!(
                phase.level,
                PhaseLevel::Task,
                "phase '{name}' should be Task-level (from TOML level field)"
            );
        }
    }

    #[test]
    fn test_pipeline_contains_all_levels() {
        let registry = test_registry();

        for mode in &["execute", "challenge", "discover", "generate"] {
            let pipeline = fallback_pipeline(mode);

            let has_spec_phase = pipeline.spec_phases.iter().any(|name| {
                registry.get(name).map(|p| p.level == PhaseLevel::Spec).unwrap_or(false)
            });
            assert!(
                has_spec_phase,
                "mode '{mode}' pipeline must include at least one Spec-level phase in spec_phases"
            );

            let has_task_phase = pipeline.task_phases.iter().any(|name| {
                registry.get(name).map(|p| p.level == PhaseLevel::Task).unwrap_or(false)
            });
            assert!(
                has_task_phase,
                "mode '{mode}' pipeline must include at least one Task-level phase in task_phases"
            );
        }
    }

    #[test]
    fn test_user_phase_preserves_level() {
        let dir = test_utils::test_dir("user-phase-level");

        let toml_content = r#"
name = "my-custom-phase"
description = "Custom phase with explicit spec level"

[phase]
name = "my-custom-phase"
level = "spec"
can_add_tasks = false
can_fail_spec = false
requires_claude = true

[prompt]
template = "Do something at the spec level."
"#;
        fs::write(dir.join("my-custom-phase.phase.toml"), toml_content).unwrap();

        let mut registry = test_registry();
        registry.load_user_phases(&dir);

        let phase = registry.get("my-custom-phase").expect("user phase must be loaded");
        assert_eq!(
            phase.level,
            PhaseLevel::Spec,
            "user phase with level='spec' in TOML must load as PhaseLevel::Spec"
        );

        // The name alone would derive to Task — this confirms TOML field wins over name derivation
        assert_eq!(
            derive_level("my-custom-phase"),
            PhaseLevel::Task,
            "name-derived level for 'my-custom-phase' should be Task"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // --- spec_improve: signal parsing + phase properties ---

    #[test]
    fn test_spec_improve_phase_exists_and_is_spec_level() {
        let registry = test_registry();
        let phase = registry.get("spec-improve").expect("spec-improve phase must exist");
        assert_eq!(phase.level, PhaseLevel::Spec);
        assert!(!phase.can_add_tasks);
        assert!(!phase.can_fail_spec);
        assert!(phase.requires_claude);
    }

    #[test]
    fn test_spec_improve_approve_signal_is_proceed() {
        let registry = test_registry();
        let phase = registry.get("spec-improve").unwrap();
        assert_eq!(phase.approve_signal.as_deref(), Some("## Spec Improved"));
        let verdict = parse_phase_output(phase, "Work done.\n\n## Spec Improved\n");
        assert_eq!(verdict, Verdict::Proceed, "approved output must yield Proceed");
    }

    #[test]
    fn test_spec_improve_on_approve_requeues_spec_critique() {
        let registry = test_registry();
        let phase = registry.get("spec-improve").unwrap();
        assert_eq!(
            phase.on_approve.as_deref(),
            Some("requeue:spec-critique"),
            "spec-improve on_approve must be requeue:spec-critique"
        );
    }

    #[test]
    fn test_spec_critique_phase_signals() {
        let registry = test_registry();
        let phase = registry.get("spec-critique").unwrap();
        assert_eq!(phase.approve_signal.as_deref(), Some("## Spec Approved"));
        assert_eq!(phase.reject_signal.as_deref(), Some("[CRITIQUE]"));
        assert_eq!(phase.on_reject.as_deref(), Some("requeue:spec-improve"));
    }

    #[test]
    fn test_spec_critique_approve_signal_is_proceed() {
        let registry = test_registry();
        let phase = registry.get("spec-critique").unwrap();
        let verdict = parse_phase_output(phase, "All checks passed.\n\n## Spec Approved\n");
        assert_eq!(verdict, Verdict::Proceed);
    }

    #[test]
    fn test_spec_critique_reject_signal_is_redo() {
        let registry = test_registry();
        let phase = registry.get("spec-critique").unwrap();
        // [CRITIQUE] in output + on_reject = "requeue:spec-improve" → Redo
        let verdict = parse_phase_output(phase, "### [CRITIQUE] 1\n\nTask t-3 verify is broken.\n");
        assert!(
            matches!(verdict, Verdict::Redo { .. }),
            "critique rejection must yield Redo (requeue), got {:?}", verdict
        );
    }

    #[test]
    fn test_spec_review_alias_signals_match_spec_critique() {
        let registry = test_registry();
        let via_alias = registry.get("spec-review").unwrap();
        let direct = registry.get("spec-critique").unwrap();
        assert_eq!(via_alias.approve_signal, direct.approve_signal);
        assert_eq!(via_alias.reject_signal, direct.reject_signal);
        assert_eq!(via_alias.on_reject, direct.on_reject);
    }

    // --- Pipeline v2: deterministic commit / merge / cleanup phases ---

    #[test]
    fn test_pipeline_v2_phase_configs() {
        let registry = test_registry();

        let commit = registry.get("commit").expect("commit phase must exist");
        assert_eq!(commit.level, PhaseLevel::Task);
        assert_eq!(commit.runtime.as_deref(), Some("deterministic"));
        assert_eq!(commit.completion_handler.as_deref(), Some("builtin:commit"));
        assert!(!commit.requires_claude);

        let merge = registry.get("merge").expect("merge phase must exist");
        assert_eq!(merge.level, PhaseLevel::Spec);
        assert_eq!(merge.runtime.as_deref(), Some("deterministic"));
        assert_eq!(merge.completion_handler.as_deref(), Some("builtin:merge"));
        assert!(!merge.requires_claude);

        let cleanup = registry.get("cleanup").expect("cleanup phase must exist");
        assert_eq!(cleanup.level, PhaseLevel::Spec);
        assert_eq!(cleanup.runtime.as_deref(), Some("deterministic"));
        assert_eq!(cleanup.completion_handler.as_deref(), Some("builtin:cleanup"));
        assert!(!cleanup.requires_claude);
    }

    #[test]
    fn test_pipeline_v2_end_to_end() {
        use crate::builtins::{run_builtin, BuiltinContext, BuiltinResult};

        let _guard = test_utils::HOME_LOCK.lock().unwrap();
        let repo = test_utils::test_git_repo("pv2-e2e");
        let home = test_utils::test_dir("pv2-e2e-home");
        std::env::set_var("HOME", home.to_str().unwrap());

        let registry = test_registry();
        let spec_id = "pv2-e2e-001";

        let dest = crate::worktree::create(spec_id, repo.to_str().unwrap()).unwrap();
        std::fs::write(dest.join("output.txt"), "v2 output").unwrap();

        // commit phase: commits changes in the worktree
        let commit = registry.get("commit").unwrap();
        let handler = commit.completion_handler.as_deref().unwrap();
        let ctx = BuiltinContext { spec_id, task_title: "Add output", repo_path: repo.to_str().unwrap() };
        assert!(matches!(run_builtin(handler, &ctx), BuiltinResult::Success(_)), "commit phase failed");

        // merge phase: brings worktree branch into the main branch
        let merge = registry.get("merge").unwrap();
        let handler = merge.completion_handler.as_deref().unwrap();
        let ctx = BuiltinContext { spec_id, task_title: "", repo_path: repo.to_str().unwrap() };
        assert!(matches!(run_builtin(handler, &ctx), BuiltinResult::Success(_)), "merge phase failed");
        assert!(repo.join("output.txt").exists(), "merged file must appear in main repo");

        // cleanup phase: removes worktree dir and deletes branch
        let cleanup = registry.get("cleanup").unwrap();
        let handler = cleanup.completion_handler.as_deref().unwrap();
        let ctx = BuiltinContext { spec_id, task_title: "", repo_path: repo.to_str().unwrap() };
        assert!(matches!(run_builtin(handler, &ctx), BuiltinResult::Success(_)), "cleanup phase failed");
        assert!(!dest.exists(), "worktree must be removed after cleanup");
    }

    mod pipeline_parse {
        use super::*;

        fn toml_file(content: &str) -> std::path::PathBuf {
            let path = test_utils::test_file("pipeline-parse", "toml");
            std::fs::write(&path, content).unwrap();
            path
        }

        #[test]
        fn test_v2_mode_all_fields() {
            let path = toml_file(r#"
[mode.v2]
spec_pre_phases  = ["spec-critique", "spec-improve"]
task_phases      = ["execute", "review", "commit"]
spec_post_phases = ["doc-update", "critic", "merge", "cleanup"]
max_loops        = 3
"#);
            let cfg = load_pipeline_from_file(&path, "v2").unwrap();
            assert_eq!(cfg.spec_pre_phases, vec!["spec-critique", "spec-improve"]);
            assert_eq!(cfg.task_phases, vec!["execute", "review", "commit"]);
            assert_eq!(cfg.spec_post_phases, vec!["doc-update", "critic", "merge", "cleanup"]);
            assert_eq!(cfg.max_loops, 3);
        }

        #[test]
        fn test_backward_compat_spec_phases_becomes_spec_post() {
            let path = toml_file(r#"
[mode.default]
spec_phases = ["critic"]
task_phases = ["execute", "task-verify"]
"#);
            let cfg = load_pipeline_from_file(&path, "execute").unwrap();
            assert_eq!(cfg.spec_post_phases, vec!["critic"]);
            assert!(cfg.spec_pre_phases.is_empty());
        }

        #[test]
        fn test_max_loops_defaults_to_3() {
            let path = toml_file(r#"
[mode.v2]
spec_pre_phases  = ["spec-critique"]
task_phases      = ["execute"]
spec_post_phases = ["critic"]
"#);
            let cfg = load_pipeline_from_file(&path, "v2").unwrap();
            assert_eq!(cfg.max_loops, 3);
        }

        #[test]
        fn test_mode_v2_from_pipelines_toml() {
            let pipelines_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("phases")
                .join("pipelines.toml");
            let cfg = load_pipeline_from_file(&pipelines_path, "v2")
                .expect("mode.v2 must exist in phases/pipelines.toml");
            assert!(!cfg.spec_pre_phases.is_empty(), "spec_pre_phases must not be empty");
            assert!(!cfg.spec_post_phases.is_empty(), "spec_post_phases must not be empty");
            assert!(!cfg.task_phases.is_empty(), "task_phases must not be empty");
            assert!(cfg.spec_pre_phases.contains(&"spec-critique".to_string()));
            assert!(cfg.spec_post_phases.contains(&"critic".to_string()));
        }

        #[test]
        fn test_pre_and_post_are_distinct() {
            let path = toml_file(r#"
[mode.v2]
spec_pre_phases  = ["spec-critique", "spec-improve"]
task_phases      = ["execute", "review", "commit"]
spec_post_phases = ["doc-update", "critic", "merge", "cleanup"]
"#);
            let cfg = load_pipeline_from_file(&path, "v2").unwrap();
            for pre in &cfg.spec_pre_phases {
                assert!(
                    !cfg.spec_post_phases.contains(pre),
                    "phase '{pre}' must not appear in both pre and post"
                );
            }
        }

        #[test]
        fn test_mode_not_found_returns_none() {
            let path = toml_file(r#"
[mode.default]
spec_phases = ["critic"]
task_phases = ["execute"]
"#);
            assert!(load_pipeline_from_file(&path, "nonexistent").is_none());
        }
    }
}
