//! [`build_recipe`] — assemble a Goose recipe for one worker phase (amendment
//! §6, corrected by the Goose research spike `docs/research/goose-spike-2026-05-20.md`).
//!
//! A Goose recipe is the YAML file `goose run --recipe <file>.yaml` consumes.
//! It carries the provider/model, the rendered worker instructions, and the
//! extension list. BOI generates one fresh recipe per worker phase run.
//!
//! ## Goose-spike corrections folded in here
//!
//! The plan's original recipe shape predated a source survey of `block/goose`.
//! The spike (`docs/research/goose-spike-2026-05-20.md` §Q2) found two field
//! errors this module is built against the *corrected* form of:
//!
//! - **provider/model live under `settings`**, not as top-level keys: a recipe
//!   carries `settings: { goose_provider, goose_model }`.
//! - **a `stdio` extension is `cmd: String` + `args: Vec<String>`** — two
//!   SEPARATE fields, never a single `command` array.
//!
//! ## The BOI MCP server is a recipe-declared `stdio` extension (G14.4)
//!
//! Every worker recipe carries the BOI MCP server as one `stdio` extension:
//! `cmd: "boi"`, `args: ["mcp-serve", "--phase-run", "<id>"]`. Goose spawns
//! that child per session; the worker's [`WorkerSession`] identity is fixed
//! structurally from the `--phase-run` arg (no in-band identity claim). A
//! recipe-declared stdio extension needs NO pre-registration — Goose spawns it
//! from the recipe at runtime (spike §Q2).
//!
//! [`WorkerSession`]: crate::service::WorkerSession
//!
//! ## `<phase_context>` goes to `instructions`; the prompt template goes to `prompt`
//!
//! Goose distinguishes two recipe fields:
//! - `instructions` — the agent's **standing context** (persists across turns).
//!   BOI sets this to the rendered `<phase_context>` block only
//!   (`service::render_phase_context`).
//! - `prompt` — the **initial task message** Goose delivers in headless mode.
//!   BOI sets this to the resolved prompt-template body (the worker directive).
//!   Goose 1.34.1 headless mode REQUIRES a non-empty `prompt`; without one it
//!   exits 1 with "Error: no text provided for prompt in headless mode".
//!
//! ## The prompt body is RESOLVED CONTENT, not a filename (G26.1)
//!
//! `PhaseDef::prompt_template` is a *filename* (`"execute.md"`) — a phase
//! declaration names a template file, it does not inline the prompt. Until
//! G26.1 `build_recipe` appended that filename verbatim into `instructions`,
//! so a worker phase ran with the literal string `execute.md` as its prompt
//! instead of the template's content — worker phases were not functionally
//! correct. `build_recipe` now takes the **resolved template content** as a
//! `prompt_body: Option<&str>` parameter: the caller ([`GooseRuntime`]) reads
//! `<prompts_dir>/<prompt_template>` and threads the content here. A
//! `deterministic` phase (no `prompt_template`) passes `None` → `prompt` is
//! the empty string; a worker phase whose template file is missing is a loud
//! terminal `Fail` at the caller, never a silent empty prompt.
//!
//! [`GooseRuntime`]: crate::runtime::goose::GooseRuntime
//!
//! ## `plan_revision` carries the `BOI_REVISION_ARTIFACT` env (review D3)
//!
//! A `plan_revision` worker writes its revised plan to a JSON artifact rather
//! than into the stream; the recipe sets `BOI_REVISION_ARTIFACT` so the worker
//! knows where. The path is `~/.boi/v2/revisions/<phase_run_id>.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::{PhaseDef, SkillRef};
use crate::service::render_phase_context;
use crate::types::context::PhaseContext;
use crate::types::ids::PhaseRunId;

/// The recipe file-format version BOI emits.
///
/// Goose's recipe `version` field is itself independently semver'd (spike §Q1
/// — `recipe/mod.rs` `default_version()`); BOI sets it explicitly rather than
/// relying on Goose's default, so a Goose default-version bump never silently
/// changes BOI's recipes.
const RECIPE_FORMAT_VERSION: &str = "1.0.0";

/// The default per-extension timeout (seconds) BOI writes for the BOI MCP
/// server stdio extension.
const MCP_EXTENSION_TIMEOUT_SECS: u64 = 300;

/// The Goose builtin extension that gives a worker its file-editor + shell
/// tools. Without it a worker phase cannot change code and hallucinates a
/// passing verdict (RC1 — root-cause analysis 2026-05-21).
const DEVELOPER_EXTENSION: &str = "developer";

/// [`write_recipe`] could not serialize or write the recipe file.
#[derive(Debug, thiserror::Error)]
pub enum RecipeError {
    /// `serde_yaml_ng` failed to serialize the [`GooseRecipe`].
    #[error("recipe serialization failed: {0}")]
    Serialize(#[from] serde_yaml_ng::Error),
    /// The recipe file could not be written to disk.
    #[error("writing recipe to {path}: {source}")]
    Write {
        /// The path the write was attempted at.
        path: PathBuf,
        /// The underlying IO error.
        source: std::io::Error,
    },
}

/// A Goose recipe — the YAML `goose run --recipe` consumes.
///
/// Field names + nesting mirror Goose's own recipe schema (spike §Q2): provider
/// and model are nested under [`RecipeSettings`]; `extensions` is a list of
/// [`GooseExtension`]; `env` is a flat string map. `serde_yaml_ng` serializes
/// this directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GooseRecipe {
    /// The recipe file-format version (spike §Q1 — set explicitly).
    pub version: String,
    /// A short recipe title — Goose surfaces it in logs.
    pub title: String,
    /// A one-line recipe description.
    pub description: String,
    /// Provider + model, nested under `settings` (spike §Q2 correction).
    pub settings: RecipeSettings,
    /// The agent's standing context — the rendered `<phase_context>` block only.
    pub instructions: String,
    /// The initial task message Goose delivers in headless mode (required by
    /// Goose 1.34.1 — without it Goose exits 1 before any LLM call).
    pub prompt: String,
    /// The extension list — every `[[skill]]` plus the BOI MCP server.
    pub extensions: Vec<GooseExtension>,
    /// Recipe-level environment variables. Empty for most phases; a
    /// `plan_revision` recipe carries `BOI_REVISION_ARTIFACT`.
    ///
    /// Serialized only when non-empty — an empty `env:` key is noise.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// A recipe's `settings` block — provider + model.
///
/// Goose reads provider/model from `settings.goose_provider` /
/// `settings.goose_model` (spike §Q2); the `goose_`-prefixed field names are
/// Goose's, mirrored here verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecipeSettings {
    /// The provider name (e.g. `claude_code`, `openrouter`).
    pub goose_provider: String,
    /// The model identifier (e.g. `claude-opus-4-7`).
    pub goose_model: String,
}

/// One Goose recipe extension entry.
///
/// Two shapes (Goose 1.34.1 recipe schema, verified empirically):
/// - `builtin` — a Goose builtin extension by name. `developer` provides the
///   file-editor + shell tools a worker needs to change code (RC1). Serializes
///   to `type: builtin` + `name`.
/// - `stdio` — an MCP-server extension Goose spawns as a child: `cmd` + `args`
///   as two SEPARATE fields, never a `command` array (spike §Q2). Serializes
///   to `type: stdio` + `name`/`description`/`cmd`/`args`/`timeout`.
///
/// `#[serde(tag = "type", rename_all = "lowercase")]` emits the `type:`
/// discriminator Goose's recipe loader keys on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum GooseExtension {
    /// A Goose builtin extension, referenced by name (e.g. `developer`).
    Builtin {
        /// The builtin's name.
        name: String,
    },
    /// An stdio MCP-server extension Goose spawns as a child process.
    Stdio {
        /// The extension name Goose registers it under.
        name: String,
        /// A human-readable description.
        description: String,
        /// The executable to spawn.
        cmd: String,
        /// The arguments passed to `cmd` — SEPARATE from `cmd` (spike §Q2).
        args: Vec<String>,
        /// The per-extension spawn timeout, seconds.
        timeout: u64,
    },
}

impl GooseExtension {
    /// The extension name — present in both variants.
    pub fn name(&self) -> &str {
        match self {
            GooseExtension::Builtin { name } => name,
            GooseExtension::Stdio { name, .. } => name,
        }
    }
}

/// Translate BOI's internal provider identifier to the name Goose expects.
///
/// BOI uses `claude_code` (underscore); Goose's provider is `claude-code`
/// (hyphen) and rejects the underscore form with `Unknown provider`. Other
/// provider names already match Goose verbatim.
fn goose_provider_name(provider: &str) -> &str {
    match provider {
        "claude_code" => "claude-code",
        other => other,
    }
}

/// Environment variable keys a given provider requires.
///
/// Used for startup validation — if a required key is absent after
/// `runtime::secrets::bootstrap_provider_env` runs, the daemon can emit a
/// clear error (e.g. "CLAUDE_CODE_OAUTH_TOKEN missing; add it to
/// ~/.boi/v2/secrets/claude.env") rather than a cryptic provider auth failure.
///
/// Secrets are loaded from `~/.boi/v2/secrets/*.env` at daemon startup, not
/// baked into the plist. See `runtime::secrets` for the bootstrap mechanism.
pub fn provider_required_env(provider: &str) -> &'static [&'static str] {
    match provider {
        // The claude-code Goose provider authenticates via this token.
        "claude_code" | "claude-code" => &["CLAUDE_CODE_OAUTH_TOKEN"],
        _ => &[],
    }
}

/// Build a Goose recipe for one worker phase (amendment §6).
///
/// - `settings.goose_provider` / `settings.goose_model` ← `phase.runtime`.
/// - `instructions` ← `render_phase_context(ctx)` ONLY (the `<phase_context>`
///   standing context block — NOT the prompt body).
/// - `prompt` ← the resolved `prompt_body` (the worker directive). Goose
///   1.34.1 headless mode requires a non-empty `prompt`; `None` → `""`.
/// - `extensions` ← one [`GooseExtension`] per `[[skill]]` via
///   `skill_to_extension`, plus the BOI MCP server as a concrete `stdio`
///   entry carrying `phase_run_id` in `args` (G14.4).
/// - a `plan_revision` phase additionally sets `BOI_REVISION_ARTIFACT` in
///   `env` (review D3).
///
/// `prompt_body` is the **resolved template content** (G26.1) — the caller
/// ([`GooseRuntime`](crate::runtime::goose::GooseRuntime)) reads the file
/// `phase.prompt_template` names and threads its content here. A worker phase
/// passes `Some(content)`; a `deterministic` phase passes `None`. A worker
/// phase whose template file is missing is rejected at the caller before
/// `build_recipe` runs — never a silent empty prompt.
pub fn build_recipe(
    phase: &PhaseDef,
    ctx: &PhaseContext,
    skills: &[SkillRef],
    phase_run_id: &PhaseRunId,
    prompt_body: Option<&str>,
) -> GooseRecipe {
    // `instructions` = standing agent context only; `prompt` = the task message.
    // Goose 1.34.1 headless mode requires a non-empty `prompt` field.
    let instructions = render_phase_context(ctx);
    let prompt = prompt_body.unwrap_or("").to_owned();

    // The worker's tool surface: the `developer` builtin (file-editor + shell —
    // RC1), one extension per declared skill, then the BOI MCP server.
    let mut extensions: Vec<GooseExtension> = vec![GooseExtension::Builtin {
        name: DEVELOPER_EXTENSION.to_owned(),
    }];
    extensions.extend(skills.iter().map(skill_to_extension));
    extensions.push(boi_mcp_extension(phase_run_id));

    // A `plan_revision` worker writes its plan to a JSON artifact (review D3).
    let mut env = BTreeMap::new();
    if phase.name == "plan_revision" {
        env.insert(
            "BOI_REVISION_ARTIFACT".to_owned(),
            revision_artifact_path(phase_run_id),
        );
    }

    GooseRecipe {
        version: RECIPE_FORMAT_VERSION.to_owned(),
        title: format!("boi {} :: {}", phase.name, phase_run_id.as_str()),
        description: format!(
            "BOI worker recipe for phase `{}` (phase run {})",
            phase.name,
            phase_run_id.as_str(),
        ),
        settings: RecipeSettings {
            goose_provider: goose_provider_name(&phase.runtime.provider).to_owned(),
            goose_model: phase.runtime.model.clone(),
        },
        instructions,
        prompt,
        extensions,
        env,
    }
}

/// Serialize the recipe to YAML and write it into `dir`.
///
/// Returns the path written — the path to pass to `goose run --recipe`. The
/// file name is **keyed on `phase_run_id`** (`recipe-<phase_run_id>.yaml`):
/// `GooseRuntime` holds ONE `recipe_dir` shared across every worker phase run,
/// so a fixed `recipe.yaml` would let two concurrent worker phases clobber
/// each other's recipe — one worker then `goose run`s the other's recipe
/// (review C-rt-S3). A per-phase-run file name makes the writes collision-free
/// without needing a per-run directory.
pub fn write_recipe(
    recipe: &GooseRecipe,
    dir: &Path,
    phase_run_id: &PhaseRunId,
) -> Result<PathBuf, RecipeError> {
    let yaml = serde_yaml_ng::to_string(recipe)?;
    let path = dir.join(format!("recipe-{}.yaml", phase_run_id.as_str()));
    std::fs::write(&path, yaml).map_err(|source| RecipeError::Write {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

/// Map a [`SkillRef`] to a Goose `extensions:` entry (amendment §12 Q2).
///
/// v1.0: a skill is referenced by name as a pre-registered Goose extension.
/// The spike (§Q2) confirmed Goose-native "skills" were removed (PR #6964) —
/// a BOI `[[skill]]` maps onto a Goose **extension** (an MCP server). v1.0
/// models a skill as a `stdio` extension whose `cmd` is the skill name; a
/// richer skill→extension mapping (explicit command + args per skill) is v1.x.
fn skill_to_extension(skill: &SkillRef) -> GooseExtension {
    GooseExtension::Stdio {
        name: skill.name.clone(),
        description: format!("BOI skill extension: {}", skill.name),
        cmd: skill.name.clone(),
        args: vec![],
        timeout: MCP_EXTENSION_TIMEOUT_SECS,
    }
}

/// The BOI MCP server as a concrete `stdio` extension entry (G14.4).
///
/// `cmd: <absolute boi path>` + `args: ["mcp-serve", "--phase-run", "<id>"]` —
/// two separate fields (spike §Q2). Goose spawns this child per session; the
/// worker's session identity is fixed from the `--phase-run` arg.
fn boi_mcp_extension(phase_run_id: &PhaseRunId) -> GooseExtension {
    GooseExtension::Stdio {
        name: "boi".to_owned(),
        description: "BOI worker tool surface".to_owned(),
        cmd: boi_mcp_cmd(),
        args: vec![
            "mcp-serve".to_owned(),
            "--phase-run".to_owned(),
            phase_run_id.as_str().to_owned(),
        ],
        timeout: MCP_EXTENSION_TIMEOUT_SECS,
    }
}

/// Absolute path to the running `boi` binary, used as the worker MCP
/// extension's `cmd`. MUST be absolute / PATH-independent: the daemon's PATH
/// does not include `~/.boi/bin`, and `boi` is only an interactive shell alias,
/// so a child Goose process resolving bare `boi` via PATH lookup gets ENOENT —
/// the `boi` extension silently fails to start, the worker loses its tool
/// surface (`task_report` / `verify_run` / `worktree_diff` / `decision_record`),
/// and emits no parseable verdict → `verdict_parse` (incident 2026-06-06:
/// 124/126 worker failures). Resolving the daemon's own executable also makes
/// the spawned worker run the same binary build. Falls back to bare `"boi"`
/// only if `current_exe` fails — degrade to the old behavior, never panic
/// inside recipe construction.
fn boi_mcp_cmd() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or_else(|| "boi".to_owned())
}

/// The `BOI_REVISION_ARTIFACT` path for a `plan_revision` phase run.
fn revision_artifact_path(phase_run_id: &PhaseRunId) -> String {
    // `~` is left literal — Goose expands it, and BOI's own `boi mcp-serve`
    // reads `BOI_REVISION_ARTIFACT` from the same env. A fixed-shape path keyed
    // on the phase run id (review D3 — the artifact channel).
    format!("~/.boi/v2/revisions/{}.json", phase_run_id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::context::{SpecContract, TaskContract, Verification};
    use crate::types::ids::{PhaseRunId, SpecId, TaskId};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A throwaway directory removed on drop — `std`-only.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("boi-recipe-{}-{tag}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.path));
        }
    }

    fn phase_run() -> PhaseRunId {
        PhaseRunId::new("P0000001a").unwrap()
    }

    /// Parse a worker phase fixture by stem.
    fn worker_phase(name: &str) -> PhaseDef {
        let toml = std::fs::read_to_string(format!(
            "{}/tests/fixtures/phases/{name}.toml",
            env!("CARGO_MANIFEST_DIR"),
        ))
        .unwrap();
        crate::config::parse_phase(&toml).unwrap()
    }

    /// A minimal task-level `PhaseContext`.
    fn task_ctx(phase: &str) -> PhaseContext {
        PhaseContext {
            spec_id: SpecId::new("S0000001a").unwrap(),
            task_id: Some(TaskId::new("T0000001a").unwrap()),
            phase: phase.to_owned(),
            phase_run_id: phase_run(),
            iteration: 0,
            spec_contract: SpecContract {
                scope: "demo".into(),
                workspace: PathBuf::from("/repo"),
                base_branch: "main".into(),
                exclusions: vec![],
                verifications: vec![],
                must_emit: vec![],
            },
            task_contract: Some(TaskContract {
                behavior: "do the thing".into(),
                verifications: vec![Verification::Command {
                    name: None,
                    command: "cargo test".into(),
                }],
            }),
            tasks: vec![],
            skills: vec![],
            decisions: vec![],
            prior_phase_runs: vec![],
        }
    }

    /// A spec-level `PhaseContext` (no task) — for the `plan_revision` recipe.
    fn spec_ctx(phase: &str) -> PhaseContext {
        let mut ctx = task_ctx(phase);
        ctx.task_id = None;
        ctx.task_contract = None;
        ctx
    }

    /// `goose_provider_name` maps BOI's `claude_code` to Goose's `claude-code`
    /// and passes every other provider through unchanged.
    #[test]
    fn test_l1_goose_provider_name_translates_claude_code() {
        assert_eq!(goose_provider_name("claude_code"), "claude-code");
        assert_eq!(goose_provider_name("openrouter"), "openrouter");
        assert_eq!(goose_provider_name("deterministic"), "deterministic");
    }

    /// `build_recipe` for an `execute` phase maps provider/model into
    /// `settings`, begins `instructions` with `<phase_context_stable>`, and
    /// carries the BOI MCP server as a `stdio` extension with the
    /// `phase_run_id` inside `args` (not a `command` array).
    #[test]
    fn test_l2_build_recipe_execute_phase_shape() {
        let recipe = build_recipe(
            &worker_phase("execute"),
            &task_ctx("execute"),
            &[],
            &phase_run(),
            Some("Implement the task."),
        );

        // provider/model live under `settings` (spike §Q2 correction). The
        // phase's internal `claude_code` is translated to Goose's `claude-code`.
        assert_eq!(recipe.settings.goose_provider, "claude-code");
        assert_eq!(recipe.settings.goose_model, "claude-opus-4-7");

        // `instructions` is ONLY the `<phase_context>` standing context block.
        assert!(
            recipe
                .instructions
                .trim_start()
                .starts_with("<phase_context"),
            "instructions must begin with the <phase_context> block, got: {}",
            &recipe.instructions[..80.min(recipe.instructions.len())],
        );
        assert!(
            recipe.instructions.contains("<phase_context_stable>"),
            "instructions must carry the stable block",
        );
        // The prompt body goes to `prompt`, not `instructions`.
        assert!(
            !recipe.instructions.contains("Implement the task."),
            "prompt body must not appear in `instructions`",
        );
        assert!(
            recipe.prompt.contains("Implement the task."),
            "prompt body must appear in `prompt`",
        );

        // The BOI MCP server is a stdio extension; `phase_run_id` is in `args`.
        let boi_ext = recipe
            .extensions
            .iter()
            .find(|e| e.name() == "boi")
            .expect("the BOI MCP server extension must be present");
        let GooseExtension::Stdio { cmd, args, .. } = boi_ext else {
            panic!("the boi extension must be a stdio extension");
        };
        assert!(
            std::path::Path::new(cmd).is_absolute(),
            "boi extension cmd must be an absolute, PATH-independent path — a \
             child Goose process cannot resolve a bare `boi` (not on the daemon \
             PATH; only a shell alias) → ENOENT kills the worker tool surface. \
             got: {cmd}",
        );
        assert_eq!(
            args,
            &vec!["mcp-serve", "--phase-run", "P0000001a"],
            "phase_run_id must ride in `args`, not a `command` array",
        );
    }

    /// `instructions` carries ONLY the `<phase_context>` block; `prompt`
    /// carries the resolved worker directive — the two are now split across
    /// separate Goose recipe fields. The body is the RESOLVED content the
    /// caller threaded — not the `prompt_template` filename (G26.1).
    #[test]
    fn test_l2_build_recipe_phase_context_precedes_instructions() {
        let recipe = build_recipe(
            &worker_phase("execute"),
            &task_ctx("execute"),
            &[],
            &phase_run(),
            Some("Write the token-bucket middleware."),
        );
        // `instructions` is ONLY the <phase_context> block — no prompt body.
        assert!(
            recipe
                .instructions
                .trim_start()
                .starts_with("<phase_context"),
            "instructions must begin with the <phase_context> block",
        );
        assert!(
            !recipe
                .instructions
                .contains("Write the token-bucket middleware."),
            "the prompt body must NOT be in instructions (it goes to `prompt`)",
        );

        // G26.1 — the RESOLVED template content lands in `prompt`, not
        // the bare `prompt_template` filename (`execute.md`).
        assert!(
            recipe.prompt.contains("Write the token-bucket middleware."),
            "the resolved prompt body must be in `prompt`",
        );
        assert!(
            !recipe.prompt.contains("execute.md"),
            "the prompt_template FILENAME must never leak into prompt (G26.1)",
        );
        assert!(
            !recipe.instructions.contains("execute.md"),
            "the prompt_template FILENAME must never leak into instructions (G26.1)",
        );
    }

    /// G26.1 — a worker phase with `prompt_body = None` (defensive floor; a
    /// deterministic phase never reaches Goose) emits an empty `prompt` string.
    /// The caller rejects a missing worker template before `build_recipe` runs.
    #[test]
    fn test_l2_build_recipe_none_prompt_body_emits_no_instructions_block() {
        let recipe = build_recipe(
            &worker_phase("execute"),
            &task_ctx("execute"),
            &[],
            &phase_run(),
            None,
        );
        assert!(
            recipe.prompt.is_empty(),
            "a None prompt body must yield an empty `prompt` string",
        );
        assert!(
            !recipe.instructions.contains("execute.md"),
            "the prompt_template filename must never leak (G26.1)",
        );
    }

    /// A `plan_revision` recipe carries `BOI_REVISION_ARTIFACT` in `env`
    /// (review D3 — the artifact channel).
    #[test]
    fn test_l2_build_recipe_plan_revision_carries_revision_artifact_env() {
        let recipe = build_recipe(
            &worker_phase("plan_revision"),
            &spec_ctx("plan_revision"),
            &[],
            &phase_run(),
            Some("Revise the plan."),
        );
        let artifact = recipe
            .env
            .get("BOI_REVISION_ARTIFACT")
            .expect("a plan_revision recipe must set BOI_REVISION_ARTIFACT");
        assert!(
            artifact.ends_with("P0000001a.json"),
            "the artifact path is keyed on the phase_run_id, got {artifact}",
        );
    }

    /// A non-`plan_revision` recipe carries an empty `env` (no artifact key).
    #[test]
    fn test_l2_build_recipe_non_revision_phase_has_empty_env() {
        let recipe = build_recipe(
            &worker_phase("execute"),
            &task_ctx("execute"),
            &[],
            &phase_run(),
            Some("Do the work."),
        );
        assert!(
            recipe.env.is_empty(),
            "only a plan_revision recipe carries env vars",
        );
    }

    /// `skill_to_extension` maps a `SkillRef` to a named stdio extension; the
    /// recipe then carries it alongside the BOI MCP server.
    #[test]
    fn test_l2_skill_to_extension_maps_a_named_extension() {
        let skills = vec![
            SkillRef {
                name: "rust-analyzer".to_owned(),
            },
            SkillRef {
                name: "playwright".to_owned(),
            },
        ];
        let recipe = build_recipe(
            &worker_phase("execute"),
            &task_ctx("execute"),
            &skills,
            &phase_run(),
            Some("Do the work."),
        );
        // developer builtin + two skills + the BOI MCP server = four extensions.
        assert_eq!(recipe.extensions.len(), 4);
        let names: Vec<&str> = recipe.extensions.iter().map(|e| e.name()).collect();
        assert!(names.contains(&"rust-analyzer"));
        assert!(names.contains(&"playwright"));
        assert!(names.contains(&"boi"));
        // Each skill extension is a stdio extension named for the skill.
        let ra = recipe
            .extensions
            .iter()
            .find(|e| e.name() == "rust-analyzer")
            .unwrap();
        let GooseExtension::Stdio { cmd, .. } = ra else {
            panic!("a skill extension must be a stdio extension");
        };
        assert_eq!(cmd, "rust-analyzer");
    }

    /// `write_recipe` serializes a valid YAML recipe whose `settings` block
    /// nests `goose_provider` / `goose_model` (spike §Q2) and re-parses.
    #[test]
    fn test_l2_write_recipe_serializes_valid_yaml() {
        let dir = TempDir::new("write");
        let recipe = build_recipe(
            &worker_phase("execute"),
            &task_ctx("execute"),
            &[],
            &phase_run(),
            Some("Do the work."),
        );
        let path = write_recipe(&recipe, &dir.path, &phase_run()).unwrap();
        assert!(path.is_file(), "the recipe file must exist on disk");

        let yaml = std::fs::read_to_string(&path).unwrap();
        // The corrected `settings`-nested provider/model keys are present.
        assert!(
            yaml.contains("goose_provider:"),
            "the YAML must carry settings.goose_provider",
        );
        assert!(yaml.contains("goose_model:"));
        // The extension carries `cmd:` + `args:` — not a `command:` array.
        // The BOI MCP extension's cmd is an absolute, PATH-independent path
        // (ends in the boi binary name); bare `cmd: boi` would ENOENT in a
        // child Goose process (incident 2026-06-06).
        assert!(yaml.contains("name: boi"));
        assert!(
            yaml.lines().any(|l| l.trim_start().starts_with("cmd: /")),
            "boi extension cmd must serialize as an absolute path"
        );
        assert!(
            !yaml.contains("command:"),
            "a stdio extension must not emit a `command` array (spike §Q2)",
        );

        // The serialized YAML round-trips back through serde as a value.
        let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml).unwrap();
        assert!(parsed.get("settings").is_some());
        assert!(parsed.get("extensions").is_some());
        // Goose 1.34.1 headless mode requires a top-level `prompt:` key.
        assert!(
            parsed.get("prompt").is_some(),
            "serialized YAML must carry a top-level `prompt:` key",
        );
    }

    /// A worker recipe's serialized YAML carries a non-empty top-level `prompt:`
    /// field — required by Goose 1.34.1 headless mode.
    #[test]
    fn test_l2_worker_recipe_yaml_has_non_empty_prompt() {
        let dir = TempDir::new("prompt-field");
        let recipe = build_recipe(
            &worker_phase("execute"),
            &task_ctx("execute"),
            &[],
            &phase_run(),
            Some("Implement the token-bucket middleware."),
        );
        let path = write_recipe(&recipe, &dir.path, &phase_run()).unwrap();
        let yaml = std::fs::read_to_string(&path).unwrap();

        let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&yaml).unwrap();
        let prompt_val = parsed.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            !prompt_val.is_empty(),
            "a worker recipe YAML must carry a non-empty top-level `prompt:` field",
        );
        assert!(
            prompt_val.contains("Implement the token-bucket middleware."),
            "the `prompt:` field must carry the resolved worker directive",
        );
    }

    /// Regression test for C-rt-S3 — two concurrent worker phases sharing one
    /// `recipe_dir` must NOT clobber each other's recipe.
    ///
    /// `GooseRuntime` holds ONE `recipe_dir`; the OLD `write_recipe` wrote a
    /// fixed `recipe.yaml`, so phase run B's `write_recipe` overwrote phase run
    /// A's file — and A's `goose run --recipe` would then execute B's recipe.
    /// The fix keys the file name on `phase_run_id`. This writes two recipes
    /// for two distinct phase runs into the SAME dir and asserts both files
    /// survive at distinct paths with their own content intact.
    #[test]
    fn test_l2_write_recipe_is_collision_free_across_phase_runs() {
        let dir = TempDir::new("write-collision");
        let run_a = PhaseRunId::new("P000000aa").unwrap();
        let run_b = PhaseRunId::new("P000000bb").unwrap();

        let recipe_a = build_recipe(
            &worker_phase("execute"),
            &task_ctx("execute"),
            &[],
            &run_a,
            Some("Do A."),
        );
        let recipe_b = build_recipe(
            &worker_phase("review"),
            &task_ctx("review"),
            &[],
            &run_b,
            Some("Do B."),
        );

        // Both written into the SAME directory — as two concurrent worker
        // phases sharing `GooseRuntime::recipe_dir` would.
        let path_a = write_recipe(&recipe_a, &dir.path, &run_a).unwrap();
        let path_b = write_recipe(&recipe_b, &dir.path, &run_b).unwrap();

        assert_ne!(
            path_a, path_b,
            "two phase runs must write to DISTINCT recipe paths",
        );
        assert!(path_a.is_file() && path_b.is_file(), "both files survive");
        // Phase run A's file still carries A's recipe — B's write did not
        // overwrite it.
        let yaml_a = std::fs::read_to_string(&path_a).unwrap();
        assert!(
            yaml_a.contains("P000000aa"),
            "phase run A's recipe must be intact after phase run B's write",
        );
        let yaml_b = std::fs::read_to_string(&path_b).unwrap();
        assert!(
            yaml_b.contains("P000000bb"),
            "phase run B's recipe is intact"
        );
    }

    /// Every worker recipe carries the Goose `developer` builtin extension —
    /// the file-editor + shell tools (RC1). Without it `execute` cannot change
    /// code and hallucinates a passing verdict.
    #[test]
    fn test_l2_build_recipe_includes_developer_builtin() {
        let recipe = build_recipe(
            &worker_phase("execute"),
            &task_ctx("execute"),
            &[],
            &phase_run(),
            Some("Implement the task."),
        );
        let dev = recipe
            .extensions
            .iter()
            .find(|e| e.name() == "developer")
            .expect("a worker recipe must carry the `developer` builtin extension");
        assert!(
            matches!(dev, GooseExtension::Builtin { .. }),
            "`developer` must be a builtin extension, not stdio",
        );
    }

    /// A worker recipe's YAML carries the `developer` builtin as `type: builtin`.
    #[test]
    fn test_l2_write_recipe_emits_developer_builtin_yaml() {
        let dir = TempDir::new("builtin-yaml");
        let recipe = build_recipe(
            &worker_phase("execute"),
            &task_ctx("execute"),
            &[],
            &phase_run(),
            Some("Do the work."),
        );
        let path = write_recipe(&recipe, &dir.path, &phase_run()).unwrap();
        let yaml = std::fs::read_to_string(&path).unwrap();
        assert!(yaml.contains("type: builtin"), "must emit a builtin entry");
        assert!(
            yaml.contains("name: developer"),
            "the builtin is `developer`"
        );
    }
}
