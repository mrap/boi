//! Pipeline parsing — `~/.boi/v2/pipelines/<name>.toml` per §4.
//!
//! A pipeline composes phase *names*; the meaning lives in the referenced
//! [`PhaseDef`](crate::config::phase::PhaseDef)s, so this parser is a thin
//! wrapper. v1.0 ships the `standard` pipeline only (L1); custom pipelines via
//! a `pipeline = "./my-pipeline.toml"` path remain a user extension point.
//!
//! The one structural subtlety is the `<tasks>` boundary sentinel (G13.3): a
//! pipeline's `spec_phases` list contains the literal token `<tasks>` to mark
//! where the orchestrator (Phase 5a) fans out the per-task lifecycle. The
//! parser maps that token to [`PipelinePhase::Tasks`] and every other entry to
//! [`PipelinePhase::Phase`].

use std::collections::HashMap;

use serde::Deserialize;

use crate::config::spec::ConfigError;

/// The literal token in a pipeline's `spec_phases` list that marks the
/// per-task fan-out boundary.
const TASKS_SENTINEL: &str = "<tasks>";

/// One entry in a pipeline's spec-phase sequence: either a named phase or the
/// per-task fan-out boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelinePhase {
    /// A named spec-level phase.
    Phase(String),
    /// The per-task fan-out boundary — the orchestrator runs the task
    /// lifecycle (×N, in parallel) here, then resumes the remaining
    /// spec phases.
    Tasks,
}

/// A per-phase runtime override block — `[overrides.<phase>.runtime]`.
///
/// Lets a pipeline run one phase against a different provider/model than the
/// phase TOML declares (e.g. cross-model critique). Both fields are optional:
/// an override may set only the model, only the provider, or both.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RuntimeOverride {
    /// Overridden provider, if set.
    #[serde(default)]
    pub provider: Option<String>,
    /// Overridden model, if set.
    #[serde(default)]
    pub model: Option<String>,
}

/// A per-phase override — `[overrides.<phase>]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PhaseOverride {
    /// Runtime (provider/model) override for the phase.
    #[serde(default)]
    pub runtime: RuntimeOverride,
}

/// A parsed pipeline definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineDef {
    /// The pipeline name.
    pub name: String,
    /// The spec-phase sequence, with the `<tasks>` boundary resolved to
    /// [`PipelinePhase::Tasks`].
    pub spec_phases: Vec<PipelinePhase>,
    /// The per-task phase sequence (all plain phase names — the fan-out has
    /// no nested boundary).
    pub task_phases: Vec<String>,
    /// Per-phase runtime overrides, keyed by phase name.
    pub overrides: HashMap<String, PhaseOverride>,
}

/// The raw pipeline TOML — `spec_phases` is a flat `Vec<String>` here; the
/// `<tasks>` sentinel is resolved into [`PipelinePhase`] during normalization.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPipelineDef {
    name: String,
    spec_phases: Vec<String>,
    task_phases: Vec<String>,
    #[serde(default)]
    overrides: HashMap<String, PhaseOverride>,
}

impl PipelineDef {
    /// Parse a pipeline TOML string into a [`PipelineDef`].
    ///
    /// Maps each `spec_phases` entry: the literal `<tasks>` token →
    /// [`PipelinePhase::Tasks`], everything else → [`PipelinePhase::Phase`].
    /// `deny_unknown_fields` rejects typos at parse time.
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        let raw: RawPipelineDef = toml::from_str(input)?;
        let spec_phases = raw
            .spec_phases
            .into_iter()
            .map(|p| {
                if p == TASKS_SENTINEL {
                    PipelinePhase::Tasks
                } else {
                    PipelinePhase::Phase(p)
                }
            })
            .collect();
        Ok(PipelineDef {
            name: raw.name,
            spec_phases,
            task_phases: raw.task_phases,
            overrides: raw.overrides,
        })
    }
}

/// Parse a pipeline TOML string into a [`PipelineDef`]. Free-function alias of
/// [`PipelineDef::from_toml`] for the `config` module's public surface.
pub fn parse_pipeline(input: &str) -> Result<PipelineDef, ConfigError> {
    PipelineDef::from_toml(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loads the pipeline fixture from `tests/fixtures/pipelines/`.
    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/pipelines/{name}.toml",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read fixture {path}: {e}"))
    }

    #[test]
    fn standard_pipeline_fixture_parses() {
        let p = parse_pipeline(&fixture("standard")).unwrap();
        assert_eq!(p.name, "standard");
        assert_eq!(p.spec_phases.len(), 8);
        assert_eq!(p.task_phases.len(), 7);
    }

    #[test]
    fn tasks_sentinel_resolves_to_pipeline_phase_tasks() {
        let p = parse_pipeline(&fixture("standard")).unwrap();
        // The 4th spec phase is the fan-out boundary.
        assert_eq!(p.spec_phases[3], PipelinePhase::Tasks);
        // Exactly one Tasks boundary.
        let boundaries = p
            .spec_phases
            .iter()
            .filter(|ph| matches!(ph, PipelinePhase::Tasks))
            .count();
        assert_eq!(boundaries, 1);
        // The phases bracketing the boundary are plain named phases.
        assert_eq!(
            p.spec_phases[2],
            PipelinePhase::Phase("critique_plan".to_owned())
        );
        assert_eq!(
            p.spec_phases[4],
            PipelinePhase::Phase("validate".to_owned())
        );
    }

    #[test]
    fn spec_phase_order_is_preserved() {
        let p = parse_pipeline(&fixture("standard")).unwrap();
        let names: Vec<&str> = p
            .spec_phases
            .iter()
            .map(|ph| match ph {
                PipelinePhase::Phase(n) => n.as_str(),
                PipelinePhase::Tasks => "<tasks>",
            })
            .collect();
        assert_eq!(
            names,
            [
                "workspace_prepare",
                "plan",
                "critique_plan",
                "<tasks>",
                "validate",
                "review",
                "merge",
                "teardown",
            ]
        );
    }

    #[test]
    fn task_phases_are_plain_names_in_order() {
        let p = parse_pipeline(&fixture("standard")).unwrap();
        assert_eq!(
            p.task_phases,
            [
                "workspace_verify_in",
                "write_red_tests",
                "execute",
                "validate",
                "review",
                "commit",
                "workspace_verify_out",
            ]
        );
    }

    #[test]
    fn cross_model_critique_override_is_recognized() {
        let p = parse_pipeline(&fixture("standard")).unwrap();
        let critique = p
            .overrides
            .get("critique_plan")
            .expect("critique_plan override must be present");
        assert_eq!(critique.runtime.provider.as_deref(), Some("openrouter"));
        assert_eq!(critique.runtime.model.as_deref(), Some("openai/gpt-5"));
    }

    #[test]
    fn unknown_field_in_pipeline_toml_is_rejected() {
        let bad = r#"
name = "standard"
spec_phases = ["plan"]
task_phases = ["execute"]
flavor = "spicy"
"#;
        assert!(matches!(
            parse_pipeline(bad).unwrap_err(),
            ConfigError::Toml(_)
        ));
    }

    #[test]
    fn pipeline_with_no_overrides_parses() {
        let minimal = r#"
name = "minimal"
spec_phases = ["plan", "<tasks>", "merge"]
task_phases = ["execute"]
"#;
        let p = parse_pipeline(minimal).unwrap();
        assert!(p.overrides.is_empty());
        assert_eq!(p.spec_phases[1], PipelinePhase::Tasks);
    }
}
