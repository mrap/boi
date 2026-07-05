//! Phase declaration parsing — `~/.boi/v2/phases/<name>.toml` per §4.
//!
//! A phase TOML binds a phase name to a [`PhaseKind`]: `worker` phases route
//! to `GooseRuntime` (LLM); `deterministic` phases resolve to a fn-pointer in
//! the `DETERMINISTIC_STEPS` table (native Rust). Each phase declares verdict
//! routing under `[on.<verdict>]` — there is no v1-style string
//! `approve_signal`/`reject_signal`; workers emit a typed verdict and the
//! phase TOML routes by it.
//!
//! This module parses + shape-checks one phase TOML. It does NOT validate that
//! `on.<verdict>.next` names a real phase — that cross-phase check lives in
//! Phase 5a, which has the full pipeline in hand.

use std::collections::HashMap;

use serde::Deserialize;

use crate::config::spec::ConfigError;

/// Whether a phase runs at the spec level or per-task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseLevel {
    /// Runs once per spec.
    Spec,
    /// Runs once per task, in parallel across tasks.
    Task,
}

/// Whether a phase is executed by the LLM runtime or by native Rust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseKind {
    /// Routed to `GooseRuntime` — an LLM phase.
    Worker,
    /// Resolved to a `DETERMINISTIC_STEPS` fn-pointer — a native phase.
    Deterministic,
}

/// One of the four verdict outcomes a phase can route on.
///
/// Used as a `HashMap` key for the `[on.<verdict>]` routing table; the
/// `snake_case` rename makes the TOML keys `passing` / `redo` / `blocked` /
/// `fail` deserialize into these variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictTag {
    /// The phase succeeded.
    Passing,
    /// The phase wants another attempt (bounded by an iteration cap).
    Redo,
    /// The phase is blocked — the task halts.
    Blocked,
    /// The phase failed — routes into the adjustment side-chain.
    Fail,
}

/// The LLM provider + model a worker phase runs against.
///
/// Structurally present on every [`PhaseDef`] (the field is non-optional);
/// for `deterministic` phases it is inert — those phases never reach the
/// runtime.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseRuntime {
    /// The provider name (e.g. `claude_code`, `openrouter`).
    pub provider: String,
    /// The model identifier (e.g. `claude-opus-4-7`).
    pub model: String,
}

/// What a phase does once it produces a given verdict: which phase to advance
/// to, and which event to emit on the bus.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteRule {
    /// The next phase name. TOML has no `null`, so a *terminating* route
    /// (the lifecycle halts here) is expressed by **omitting** the `next`
    /// key — it deserializes to `None`.
    #[serde(default)]
    pub next: Option<String>,
    /// The bus event emitted when this route is taken.
    pub emit: String,
}

/// A parsed phase declaration.
///
/// Deserializes directly from a `~/.boi/v2/phases/<name>.toml` file with
/// `deny_unknown_fields` — typos and removed fields fail loudly at parse time.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseDef {
    /// The phase name — must match the file stem by convention.
    pub name: String,
    /// Whether the phase runs at spec or task level.
    pub level: PhaseLevel,
    /// Whether the phase is a worker (LLM) or deterministic (native) phase.
    pub kind: PhaseKind,
    /// The prompt template file — present for `worker` phases, absent for
    /// `deterministic` ones (enforced by [`PhaseDef::from_toml`]).
    #[serde(default)]
    pub prompt_template: Option<String>,
    /// The provider + model. Inert for `deterministic` phases.
    pub runtime: PhaseRuntime,
    /// Verdict routing — one [`RouteRule`] per [`VerdictTag`].
    #[serde(default)]
    pub on: HashMap<VerdictTag, RouteRule>,
}

impl PhaseDef {
    /// Parse + shape-check a phase TOML string.
    ///
    /// Beyond `deny_unknown_fields`, this enforces the one cross-field
    /// invariant a single phase TOML can carry: a `deterministic` phase must
    /// NOT declare a `prompt_template` (it has no LLM prompt). The
    /// `on.<verdict>.next`-names-a-real-phase check is deliberately deferred to
    /// Phase 5a, which has the whole pipeline available.
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        let phase: PhaseDef = toml::from_str(input)?;
        if phase.kind == PhaseKind::Deterministic && phase.prompt_template.is_some() {
            return Err(ConfigError::MissingField {
                // A deterministic phase carrying a prompt_template is a
                // contradiction; surface it as a typed config error.
                field: "prompt_template (must be absent for kind=deterministic)",
            });
        }
        Ok(phase)
    }
}

/// Parse a phase TOML string into a [`PhaseDef`]. Free-function alias of
/// [`PhaseDef::from_toml`] for the `config` module's public surface.
pub fn parse_phase(input: &str) -> Result<PhaseDef, ConfigError> {
    PhaseDef::from_toml(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loads a phase fixture by stem from `tests/fixtures/phases/`.
    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/phases/{name}.toml",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read fixture {path}: {e}"))
    }

    /// Every phase fixture the `standard` pipeline can reference (16 — G13.2).
    const ALL_PHASES: &[&str] = &[
        "workspace_prepare",
        "plan",
        "critique_plan",
        "workspace_verify_in",
        "write_red_tests",
        "execute",
        "validate",
        "review",
        "propose_adjustment",
        "review_adjustment",
        "commit",
        "merge",
        "teardown",
        "workspace_verify_out",
        "merge_to_integration",
        "plan_revision",
    ];

    const WORKER_PHASES: &[&str] = &[
        "plan",
        "critique_plan",
        "execute",
        "review",
        "propose_adjustment",
        "review_adjustment",
        "write_red_tests",
        "plan_revision",
    ];

    const DETERMINISTIC_PHASES: &[&str] = &[
        "workspace_prepare",
        "workspace_verify_in",
        "validate",
        "commit",
        "merge",
        "teardown",
        "workspace_verify_out",
        "merge_to_integration",
    ];

    #[test]
    fn all_sixteen_phase_fixtures_parse() {
        assert_eq!(ALL_PHASES.len(), 16);
        for name in ALL_PHASES {
            let phase = parse_phase(&fixture(name))
                .unwrap_or_else(|e| panic!("phase fixture {name} failed to parse: {e}"));
            assert_eq!(&phase.name, name, "phase.name must match file stem");
        }
    }

    #[test]
    fn worker_phases_carry_a_prompt_template() {
        for name in WORKER_PHASES {
            let phase = parse_phase(&fixture(name)).unwrap();
            assert_eq!(phase.kind, PhaseKind::Worker);
            assert!(
                phase.prompt_template.is_some(),
                "worker phase {name} must declare a prompt_template"
            );
        }
    }

    #[test]
    fn deterministic_phases_have_no_prompt_template() {
        for name in DETERMINISTIC_PHASES {
            let phase = parse_phase(&fixture(name)).unwrap();
            assert_eq!(phase.kind, PhaseKind::Deterministic);
            assert!(
                phase.prompt_template.is_none(),
                "deterministic phase {name} must not declare a prompt_template"
            );
        }
    }

    #[test]
    fn routing_table_has_all_four_verdict_tags() {
        // Every fixture routes all four verdicts.
        for name in ALL_PHASES {
            let phase = parse_phase(&fixture(name)).unwrap();
            for tag in [
                VerdictTag::Passing,
                VerdictTag::Redo,
                VerdictTag::Blocked,
                VerdictTag::Fail,
            ] {
                assert!(
                    phase.on.contains_key(&tag),
                    "phase {name} is missing routing for {tag:?}"
                );
            }
        }
    }

    #[test]
    fn execute_phase_routes_passing_to_a_named_phase() {
        let phase = parse_phase(&fixture("execute")).unwrap();
        let passing = &phase.on[&VerdictTag::Passing];
        assert_eq!(passing.next.as_deref(), Some("validate"));
        assert_eq!(passing.emit, "task.execute.completed");
        // blocked halts the task — next is null.
        assert_eq!(phase.on[&VerdictTag::Blocked].next, None);
    }

    #[test]
    fn execute_phase_runtime_is_parsed() {
        let phase = parse_phase(&fixture("execute")).unwrap();
        assert_eq!(phase.runtime.provider, "claude_code");
        assert_eq!(phase.runtime.model, "claude-opus-4-7");
        assert_eq!(phase.level, PhaseLevel::Task);
    }

    #[test]
    fn deterministic_phase_with_prompt_template_is_rejected() {
        // A synthetic phase that contradicts itself — kind=deterministic but
        // carries a prompt_template — must be rejected with a typed error.
        let bad = r#"
name = "validate"
level = "task"
kind = "deterministic"
prompt_template = "validate.md"

[runtime]
provider = "deterministic"
model = "n/a"
"#;
        let err = parse_phase(bad).unwrap_err();
        assert!(matches!(err, ConfigError::MissingField { .. }));
    }

    #[test]
    fn unknown_field_in_phase_toml_is_rejected() {
        let bad = r#"
name = "execute"
level = "task"
kind = "worker"
prompt_template = "execute.md"
flavor = "spicy"

[runtime]
provider = "claude_code"
model = "claude-opus-4-7"
"#;
        assert!(matches!(
            parse_phase(bad).unwrap_err(),
            ConfigError::Toml(_)
        ));
    }
}
