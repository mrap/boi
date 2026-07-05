//! Spec TOML parsing — the raw deserializer (Task 2.1) and the normalized,
//! repo-facing [`Spec`] (Task 2.6).
//!
//! The pipeline is `&str` → [`RawSpec`] (1:1 with the §3 TOML format) →
//! [`validate`](crate::config::validate::validate) → [`Spec`] (typed, uses
//! `crate::types::*` primitives). Workers and the rest of the harness see only
//! [`Spec`]; [`RawSpec`] exists so that `deny_unknown_fields` can reject typos
//! and removed-field drift loudly at parse time.

use std::path::PathBuf;

use serde::Deserialize;

use crate::types::context::{SpecContract, Verification};
use crate::types::decision::RejectedAlternative;

/// Every way spec parsing / validation can fail.
///
/// Parse-stage failures ([`Toml`](ConfigError::Toml)) come from `serde` /
/// `toml` — including the generic "unknown field" error that
/// `#[serde(deny_unknown_fields)]` raises. The remaining variants are the
/// *typed, actionable* validation failures `validate.rs` raises after a clean
/// parse: each names the exact rule that was broken so the operator gets
/// "modes were removed in v1.0" rather than a bare "unknown field `mode`".
///
/// Every variant's message includes a **Fix:** hint so the operator knows how
/// to correct the spec without consulting documentation.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The input was not well-formed TOML, or `deny_unknown_fields` rejected
    /// an unrecognized field (typo / future-field drift).
    ///
    /// The `toml` crate's error already includes line/column for syntax errors
    /// and names the unknown field for `deny_unknown_fields` rejections.
    #[error("{0}\n  Fix: check the TOML syntax and field names around the reported location")]
    Toml(#[from] toml::de::Error),

    /// A required field was missing or empty.
    #[error(
        "required field missing or empty: `{field}`\n  \
         Fix: add `{field} = ...` to your spec"
    )]
    MissingField {
        /// The field that was required.
        field: &'static str,
    },

    /// The spec declared neither or both of `contract.workspace` /
    /// `contract.workspace_rationale`. Exactly one is required (§3.1).
    #[error(
        "contract must set exactly one of `workspace` or `workspace_rationale` \
         (§3.1 workspace XOR)\n  \
         Fix: keep only one — either `workspace = \"/path/to/repo\"` or \
         `workspace_rationale = \"reason\"`"
    )]
    WorkspaceXor,

    /// The `blocked_by` graph contains a cycle.
    #[error(
        "task dependency graph has a cycle involving ref `{task_ref}`\n  \
         Fix: break the circular dependency — tasks must form a directed acyclic graph"
    )]
    DependencyCycle {
        /// A task ref on the detected cycle.
        task_ref: String,
    },

    /// A `blocked_by` entry names a ref that no task declares.
    #[error(
        "task `{task_ref}` is blocked_by `{missing}`, which no task declares\n  \
         Fix: add `ref = \"{missing}\"` to a task, or remove `blocked_by = [\"{missing}\"]`"
    )]
    DanglingDep {
        /// The task carrying the dangling `blocked_by`.
        task_ref: String,
        /// The ref that does not resolve.
        missing: String,
    },

    /// Two tasks share the same `ref`.
    #[error(
        "duplicate task ref `{task_ref}` — refs must be unique\n  \
         Fix: give each task a unique `ref` value"
    )]
    DuplicateRef {
        /// The ref used more than once.
        task_ref: String,
    },

    /// A verification entry set neither or both of `intent` / `command`.
    #[error(
        "verification `{name}` must set exactly one of `intent` or `command`\n  \
         Fix: set only `intent = \"...\"` or only `command = \"...\"` — not both, not neither"
    )]
    VerificationMutex {
        /// The verification's name, or `<unnamed>` if it has none.
        name: String,
    },

    /// The spec used the removed `mode` field (L2 — modes were removed).
    #[error(
        "`mode` was removed in v1.0 — there are no modes, only the `standard` pipeline (L2)\n  \
         Fix: remove the `mode` field from your spec"
    )]
    ModesRemoved,

    /// The spec used the removed `max_iterations` field (S6).
    #[error(
        "`max_iterations` was removed in v1.0 — iteration caps are hard-coded \
         constants, per-spec tuning is deferred to v1.x (S6)\n  \
         Fix: remove the `max_iterations` field from your spec"
    )]
    MaxIterationsHardcoded,

    /// The spec used the removed `clean_state` field (S8).
    #[error(
        "`clean_state` was removed in v1.0 — strict clean-state invariants are \
         enforced unconditionally (S8)\n  \
         Fix: remove the `clean_state` field from your spec"
    )]
    CleanStateStrict,

    /// The spec used the removed `initiative` field (S17).
    #[error(
        "`initiative` was removed in v1.0 — cross-cutting tracking is deferred \
         to v1.x (S17)\n  \
         Fix: remove the `initiative` field from your spec"
    )]
    InitiativeRemoved,

    /// `delivery` was set to a string that is not `merge` / `pr` / `branch-only`.
    #[error(
        "unknown delivery `{got}` — expected one of: merge, pr, branch-only\n  \
         Fix: set `delivery = \"merge\"` (the default), `\"pr\"`, or `\"branch-only\"`"
    )]
    UnknownDelivery {
        /// The unrecognized delivery string.
        got: String,
    },

    /// `pipeline` was set to a string other than `standard`.
    #[error(
        "unknown pipeline `{got}` — `standard` is the only pipeline in v1.0\n  \
         Fix: set `pipeline = \"standard\"` or omit the field entirely"
    )]
    UnknownPipeline {
        /// The unrecognized pipeline string.
        got: String,
    },

    /// `contract.workspace` used `~` but `$HOME` is unset, so the path can't
    /// be expanded. Mirrors `paths::PathError::HomeUnset`: loud-fail rather
    /// than guess. (T7)
    #[error(
        "contract.workspace uses `~` but $HOME is unset — cannot expand\n  \
         Fix: set $HOME, or use an absolute path in `workspace`"
    )]
    WorkspaceHomeUnset,

    /// `contract.workspace`, after `~` expansion, is still not absolute.
    /// Relative paths are rejected — workspace must be absolute. (T7)
    #[error(
        "contract.workspace `{got}` is not an absolute path\n  \
         Fix: use an absolute path (e.g. /Users/you/repo) or `~/repo`"
    )]
    WorkspaceNotAbsolute {
        /// The non-absolute workspace path.
        got: String,
    },
}

/// The raw spec, mirroring the §3 TOML format 1:1.
///
/// `deny_unknown_fields` catches typos AND future-field drift loudly. The four
/// explicitly-named ex-fields below (`mode` / `max_iterations` / `clean_state`
/// / `initiative`) are captured as [`toml::Value`] *on purpose*: keeping them
/// in the struct means `validate.rs` can raise a specific [`ConfigError`]
/// instead of the generic unknown-field error.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSpec {
    /// Spec title — required identifier.
    pub title: String,
    /// Pipeline name — defaults to `standard`.
    pub pipeline: Option<String>,
    /// Delivery mode — defaults to `merge`.
    pub delivery: Option<String>,
    /// The sprint contract.
    pub contract: RawContract,
    /// Work units — at least one required.
    #[serde(rename = "tasks")]
    pub tasks: Vec<RawTask>,
    /// Authored decisions (§3.6).
    #[serde(default, rename = "decision")]
    pub decisions: Vec<RawDecision>,
    /// Skill declarations (§3.7).
    #[serde(default, rename = "skill")]
    pub skills: Vec<RawSkill>,

    /// L2 — modes removed. Captured for a typed rejection.
    #[serde(default)]
    pub mode: Option<toml::Value>,
    /// S6 — caps hard-coded. Captured for a typed rejection.
    #[serde(default)]
    pub max_iterations: Option<toml::Value>,
    /// S8 — strict-only at v1.0. Captured for a typed rejection.
    #[serde(default)]
    pub clean_state: Option<toml::Value>,
    /// S17 — field removed. Captured for a typed rejection.
    #[serde(default)]
    pub initiative: Option<toml::Value>,
}

/// The raw `[contract]` block.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawContract {
    /// The sprint-contract scope anchor.
    pub scope: String,
    /// The workspace repo root — XOR with `workspace_rationale` (§3.1).
    pub workspace: Option<PathBuf>,
    /// Rationale for an absent workspace — XOR with `workspace` (§3.1).
    pub workspace_rationale: Option<String>,
    /// Base branch for the integration worktree (A6 — required).
    pub base_branch: String,
    /// Paths / globs the spec must not touch.
    #[serde(default)]
    pub exclusions: Vec<String>,
    /// Spec-level verifications.
    #[serde(default)]
    pub verifications: Vec<RawVerification>,
    /// Files the spec must emit.
    #[serde(default)]
    pub must_emit: Vec<PathBuf>,
}

/// A raw `[[tasks]]` entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawTask {
    /// Optional user-authored slug for dependency refs.
    #[serde(rename = "ref")]
    pub task_ref: Option<String>,
    /// The behavior the task must implement.
    pub behavior: String,
    /// Refs of tasks that must finish first.
    #[serde(default)]
    pub blocked_by: Vec<String>,
    /// Task-level verifications — at least one.
    pub verifications: Vec<RawVerification>,
}

/// A raw verification entry — `intent` XOR `command` (mutex enforced in
/// `validate.rs`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawVerification {
    /// Optional human-readable name for reporting.
    pub name: Option<String>,
    /// An LLM-judged intent — mutex with `command`.
    pub intent: Option<String>,
    /// A deterministically-run command — mutex with `intent`.
    pub command: Option<String>,
}

/// A raw `[[decision]]` block (§3.6).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDecision {
    /// Short decision title.
    pub title: String,
    /// 1-3 sentence summary.
    pub summary: String,
    /// Why this choice over the alternatives.
    pub rationale: String,
    /// Alternatives that were considered and rejected.
    #[serde(default)]
    pub alternatives: Vec<RawAlternative>,
}

/// A raw rejected-alternative entry inside a `[[decision]]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawAlternative {
    /// Name of the rejected alternative.
    pub name: String,
    /// Why it was rejected.
    pub reason: String,
}

/// A raw `[[skill]]` block (§3.7).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSkill {
    /// The skill name — passed through to the Goose recipe `extensions:`.
    pub name: String,
}

impl RawSpec {
    /// Parse a TOML string into a [`RawSpec`].
    ///
    /// This is the parse stage only — no validation. `deny_unknown_fields`
    /// rejects unrecognized fields here; the typed-rule validation
    /// ([`validate`](crate::config::validate::validate)) runs separately.
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(input)?)
    }
}

// --- Task 2.6: normalization to the repo-facing `Spec` ---

/// How the integration branch ships once all tasks settle (§3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Fast-forward merge to the base branch.
    Merge,
    /// Open a pull request.
    Pr,
    /// Leave the integration branch in place — no merge, no PR.
    BranchOnly,
}

impl Delivery {
    /// Parse a `delivery` string. `None` input → the [`Merge`](Delivery::Merge)
    /// default (§3.5).
    fn parse(raw: Option<&str>) -> Result<Self, ConfigError> {
        match raw {
            None | Some("merge") => Ok(Delivery::Merge),
            Some("pr") => Ok(Delivery::Pr),
            Some("branch-only") => Ok(Delivery::BranchOnly),
            Some(other) => Err(ConfigError::UnknownDelivery {
                got: other.to_owned(),
            }),
        }
    }
}

/// A normalized task.
///
/// Task refs remain `String`s here; the spec→DB dispatch in Phase 9
/// (`boi dispatch`) mints `TaskId`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDef {
    /// The optional user-authored ref slug.
    pub task_ref: Option<String>,
    /// The behavior the task must implement.
    pub behavior: String,
    /// Refs of tasks that must finish first.
    pub blocked_by: Vec<String>,
    /// Task-level verifications, in author order.
    pub verifications: Vec<Verification>,
}

/// A normalized authored decision (§3.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoredDecision {
    /// Short decision title.
    pub title: String,
    /// 1-3 sentence summary.
    pub summary: String,
    /// Why this choice over the alternatives.
    pub rationale: String,
    /// Alternatives that were considered and rejected.
    pub alternatives: Vec<RejectedAlternative>,
}

// `SkillRef` lives in `crate::types::context` so `PhaseContext` can use it
// without a config→types layer inversion. Re-exported below.
pub use crate::types::context::SkillRef;

/// The repo-facing, validated spec — the form the rest of the harness uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// Spec title.
    pub title: String,
    /// Pipeline name (`standard` if the spec omitted it).
    pub pipeline: String,
    /// Delivery mode.
    pub delivery: Delivery,
    /// The sprint contract — a `crate::types` primitive.
    pub contract: SpecContract,
    /// Work units.
    pub tasks: Vec<TaskDef>,
    /// Authored decisions.
    pub authored_decisions: Vec<AuthoredDecision>,
    /// Declared skills.
    pub skills: Vec<SkillRef>,
}

/// Convert a raw verification into a typed [`Verification`].
///
/// The intent/command mutex is assumed to already hold —
/// [`validate`](crate::config::validate::validate) ran first. The
/// `(None, None)` arm is unreachable post-validation; it maps to a
/// stable empty-intent value rather than panicking, so a future caller that
/// skips validation degrades gracefully instead of crashing.
fn normalize_verification(raw: &RawVerification) -> Verification {
    match (&raw.intent, &raw.command) {
        (Some(intent), _) => Verification::Intent {
            name: raw.name.clone(),
            intent: intent.clone(),
        },
        (None, Some(command)) => Verification::Command {
            name: raw.name.clone(),
            command: command.clone(),
        },
        (None, None) => Verification::Intent {
            name: raw.name.clone(),
            intent: String::new(),
        },
    }
}

/// Normalize a validated [`RawSpec`] into the repo-facing [`Spec`].
///
/// A pure transform — every structural check has already run in
/// [`validate`](crate::config::validate::validate). The one fallible step is
/// `delivery` parsing, which validation does not
/// cover (the raw `delivery` is a free-form `Option<String>`).
/// Expand a leading `~` in the workspace path against `$HOME` and assert
/// the result is absolute. T7: BOI v2 used to pass `~/...` literally to
/// `git_ops`, which doesn't expand `~` — workspace_prepare failed 0ms with
/// no diagnostic.
fn expand_workspace(path: PathBuf) -> Result<PathBuf, ConfigError> {
    expand_workspace_with_home(path, std::env::var("HOME").ok())
}

/// Test seam for [`expand_workspace`]: takes `$HOME` explicitly so tests
/// don't touch process-global env (which would require `unsafe` under the
/// crate's lints).
fn expand_workspace_with_home(path: PathBuf, home: Option<String>) -> Result<PathBuf, ConfigError> {
    let s = path.to_string_lossy().into_owned();
    let expanded: PathBuf = if s == "~" {
        let home = home.ok_or(ConfigError::WorkspaceHomeUnset)?;
        PathBuf::from(home)
    } else if let Some(rest) = s.strip_prefix("~/") {
        let home = home.ok_or(ConfigError::WorkspaceHomeUnset)?;
        PathBuf::from(home).join(rest)
    } else {
        path
    };

    if !expanded.is_absolute() {
        return Err(ConfigError::WorkspaceNotAbsolute {
            got: expanded.to_string_lossy().into_owned(),
        });
    }
    Ok(expanded)
}

fn normalize(raw: RawSpec) -> Result<Spec, ConfigError> {
    let delivery = Delivery::parse(raw.delivery.as_deref())?;

    // Workspace XOR was validated; `workspace` is `Some` here. Fall back to an
    // empty path rather than `unwrap` so a caller that skips validation
    // degrades gracefully.
    let workspace = expand_workspace(raw.contract.workspace.clone().unwrap_or_default())?;

    let contract = SpecContract {
        scope: raw.contract.scope,
        workspace,
        base_branch: raw.contract.base_branch,
        exclusions: raw.contract.exclusions,
        verifications: raw
            .contract
            .verifications
            .iter()
            .map(normalize_verification)
            .collect(),
        must_emit: raw.contract.must_emit,
    };

    let tasks = raw
        .tasks
        .into_iter()
        .map(|t| TaskDef {
            task_ref: t.task_ref,
            behavior: t.behavior,
            blocked_by: t.blocked_by,
            verifications: t.verifications.iter().map(normalize_verification).collect(),
        })
        .collect();

    let authored_decisions = raw
        .decisions
        .into_iter()
        .map(|d| AuthoredDecision {
            title: d.title,
            summary: d.summary,
            rationale: d.rationale,
            alternatives: d
                .alternatives
                .into_iter()
                .map(|a| RejectedAlternative {
                    name: a.name,
                    reason: a.reason,
                })
                .collect(),
        })
        .collect();

    let skills = raw
        .skills
        .into_iter()
        .map(|s| SkillRef { name: s.name })
        .collect();

    Ok(Spec {
        title: raw.title,
        // `validate`'s `check_pipeline` already proved this is `standard` or
        // absent — the default is the only legal value, never a silent override.
        pipeline: raw.pipeline.unwrap_or_else(|| "standard".to_owned()),
        delivery,
        contract,
        tasks,
        authored_decisions,
        skills,
    })
}

/// Parse, validate, and normalize a spec TOML string.
///
/// The single public entry point for spec ingestion: `&str` → [`RawSpec`] →
/// [`validate`](crate::config::validate::validate) → [`Spec`]. Any stage's
/// failure surfaces as a typed [`ConfigError`].
pub fn parse_spec(input: &str) -> Result<Spec, ConfigError> {
    let raw = RawSpec::from_toml(input)?;
    crate::config::validate::validate(&raw)?;
    normalize(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loads a spec fixture by stem from `tests/fixtures/specs/`.
    pub(super) fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/specs/{name}.toml",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read fixture {path}: {e}"))
    }

    #[test]
    fn raw_parse_minimum_fixture() {
        let raw = RawSpec::from_toml(&fixture("01_minimum")).unwrap();
        assert_eq!(raw.title, "Add rate limiting to API");
        assert_eq!(raw.pipeline, None); // default applied at normalize time
        assert_eq!(raw.tasks.len(), 1);
        assert_eq!(raw.contract.base_branch, "develop");
        assert!(raw.contract.workspace.is_some());
    }

    #[test]
    fn raw_parse_multi_task_dag_fixture() {
        let raw = RawSpec::from_toml(&fixture("02_multi_task_dag")).unwrap();
        assert_eq!(raw.tasks.len(), 3);
        assert_eq!(raw.tasks[1].blocked_by, vec!["setup-middleware"]);
        assert_eq!(raw.contract.verifications.len(), 2);
        assert_eq!(raw.contract.must_emit.len(), 1);
    }

    #[test]
    fn raw_parse_authored_decisions_fixture() {
        let raw = RawSpec::from_toml(&fixture("03_with_authored_decisions")).unwrap();
        assert_eq!(raw.decisions.len(), 2);
        assert_eq!(raw.decisions[0].alternatives.len(), 3);
    }

    #[test]
    fn raw_parse_skills_fixture() {
        let raw = RawSpec::from_toml(&fixture("04_with_skills")).unwrap();
        assert_eq!(raw.skills.len(), 2);
        assert_eq!(raw.skills[0].name, "test-driven-development");
    }

    #[test]
    fn raw_parse_captures_rejected_ex_fields() {
        // The four ex-fields parse into Option<toml::Value> (not unknown-field
        // errors) so validate.rs can raise typed rejections.
        assert!(
            RawSpec::from_toml(&fixture("05_rejects_modes"))
                .unwrap()
                .mode
                .is_some()
        );
        assert!(
            RawSpec::from_toml(&fixture("06_rejects_max_iterations"))
                .unwrap()
                .max_iterations
                .is_some()
        );
        assert!(
            RawSpec::from_toml(&fixture("07_rejects_clean_state"))
                .unwrap()
                .clean_state
                .is_some()
        );
        assert!(
            RawSpec::from_toml(&fixture("09_rejects_initiative"))
                .unwrap()
                .initiative
                .is_some()
        );
    }

    #[test]
    fn raw_parse_rejects_genuinely_unknown_field() {
        // `flavor` is not a named ex-field — deny_unknown_fields rejects it at
        // parse time with a generic ConfigError::Toml.
        let err = RawSpec::from_toml(&fixture("14_rejects_unknown_field")).unwrap_err();
        assert!(matches!(err, ConfigError::Toml(_)));
    }

    #[test]
    fn raw_parse_verification_mutex_both_set_parses_at_raw_stage() {
        // The intent/command mutex is a VALIDATION rule, not a parse rule —
        // a raw verification with both set still deserializes cleanly.
        let raw = RawSpec::from_toml(&fixture("13_rejects_verification_mutex")).unwrap();
        let v = &raw.tasks[0].verifications[0];
        assert!(v.intent.is_some() && v.command.is_some());
    }

    // --- Task 2.6: parse → validate → normalize ---

    #[test]
    fn delivery_parse_covers_every_variant() {
        assert_eq!(Delivery::parse(None).unwrap(), Delivery::Merge);
        assert_eq!(Delivery::parse(Some("merge")).unwrap(), Delivery::Merge);
        assert_eq!(Delivery::parse(Some("pr")).unwrap(), Delivery::Pr);
        assert_eq!(
            Delivery::parse(Some("branch-only")).unwrap(),
            Delivery::BranchOnly
        );
        assert!(matches!(
            Delivery::parse(Some("teleport")).unwrap_err(),
            ConfigError::UnknownDelivery { .. }
        ));
    }

    #[test]
    fn parse_spec_minimum_produces_typed_spec_with_defaults() {
        let spec = parse_spec(&fixture("01_minimum")).unwrap();
        assert_eq!(spec.title, "Add rate limiting to API");
        assert_eq!(spec.pipeline, "standard"); // omitted → default
        assert_eq!(spec.delivery, Delivery::Merge); // omitted → default
        assert_eq!(spec.tasks.len(), 1);
        assert_eq!(spec.contract.base_branch, "develop");
        assert_eq!(spec.tasks[0].task_ref.as_deref(), Some("setup-middleware"));
        assert!(matches!(
            spec.tasks[0].verifications[0],
            Verification::Intent { .. }
        ));
    }

    #[test]
    fn parse_spec_preserves_dag_and_verification_order() {
        let spec = parse_spec(&fixture("02_multi_task_dag")).unwrap();
        assert_eq!(spec.pipeline, "standard");
        assert_eq!(spec.delivery, Delivery::Merge);
        assert_eq!(spec.tasks.len(), 3);
        assert_eq!(
            spec.tasks[2].blocked_by,
            vec!["setup-middleware", "apply-middleware"]
        );
        // Contract verification list: intent then command — order preserved.
        assert!(matches!(
            spec.contract.verifications[0],
            Verification::Intent { .. }
        ));
        assert!(matches!(
            spec.contract.verifications[1],
            Verification::Command { .. }
        ));
        // The first task carries a mixed intent+command list — order preserved.
        let v = &spec.tasks[0].verifications;
        assert!(matches!(v[0], Verification::Intent { .. }));
        assert!(matches!(v[1], Verification::Command { .. }));
    }

    #[test]
    fn parse_spec_normalizes_authored_decisions() {
        let spec = parse_spec(&fixture("03_with_authored_decisions")).unwrap();
        assert_eq!(spec.authored_decisions.len(), 2);
        assert_eq!(spec.authored_decisions[0].title, "Use TOML for all config");
        assert_eq!(spec.authored_decisions[0].alternatives.len(), 3);
        assert_eq!(spec.authored_decisions[0].alternatives[0].name, "YAML");
    }

    #[test]
    fn parse_spec_normalizes_skills() {
        let spec = parse_spec(&fixture("04_with_skills")).unwrap();
        assert_eq!(spec.skills.len(), 2);
        assert_eq!(spec.skills[1].name, "verification-before-completion");
    }

    #[test]
    fn parse_spec_normalization_is_deterministic() {
        // Same input twice → an identical typed Spec.
        let a = parse_spec(&fixture("02_multi_task_dag")).unwrap();
        let b = parse_spec(&fixture("02_multi_task_dag")).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn parse_spec_propagates_validation_rejections() {
        // A rejection from the validate stage surfaces through parse_spec.
        assert!(matches!(
            parse_spec(&fixture("05_rejects_modes")).unwrap_err(),
            ConfigError::ModesRemoved
        ));
        assert!(matches!(
            parse_spec(&fixture("08_invalid_cycle")).unwrap_err(),
            ConfigError::DependencyCycle { .. }
        ));
    }

    #[test]
    fn parse_spec_propagates_parse_stage_rejections() {
        // A genuinely-unknown field is rejected at the parse stage, before
        // validate ever runs.
        assert!(matches!(
            parse_spec(&fixture("14_rejects_unknown_field")).unwrap_err(),
            ConfigError::Toml(_)
        ));
    }

    #[test]
    fn parse_spec_rejects_unknown_pipeline() {
        // A-SF-2 regression: a non-`standard` pipeline must surface a typed
        // UnknownPipeline through parse_spec — never a silent `standard`
        // override at normalize time.
        match parse_spec(&fixture("15_rejects_unknown_pipeline")).unwrap_err() {
            ConfigError::UnknownPipeline { got } => assert_eq!(got, "turbo"),
            other => panic!("expected UnknownPipeline, got {other:?}"),
        }
    }

    // --- Task 10.1 — the 5 §13 integration fixtures parse cleanly ---

    /// All 5 §13 integration fixtures (`tests/fixtures/specs/0N-*.toml`) parse,
    /// validate, and normalize through `parse_spec`. The L3 harness dispatches
    /// these; a fixture that does not parse would fail the whole L3 tier.
    #[test]
    fn test_l2_all_five_integration_fixtures_parse() {
        for name in [
            "01-typo-fix",
            "02-multi-task-feature",
            "03-failure-recovery",
            "04-multi-provider",
            "05-plan-revision",
        ] {
            let spec = parse_spec(&fixture(name))
                .unwrap_or_else(|e| panic!("integration fixture {name} failed to parse: {e}"));
            assert!(
                !spec.tasks.is_empty(),
                "fixture {name} must declare at least one task",
            );
        }
    }

    /// The `02-multi-task-feature` DAG fixture's `blocked_by` graph is
    /// ACYCLIC — `parse_spec`'s `check_dependency_graph` would reject a cycle,
    /// so a clean parse IS the acyclicity proof. The test also asserts the
    /// expected edge structure so a fixture edit that drops the DAG is caught.
    #[test]
    fn test_l2_multi_task_fixture_dag_is_acyclic() {
        let spec = parse_spec(&fixture("02-multi-task-feature"))
            .expect("the DAG fixture parses — `parse_spec` rejects any cycle");
        assert_eq!(spec.tasks.len(), 3, "the DAG fixture has three tasks");
        // `apply-middleware` waits on `setup-middleware`.
        let apply = spec
            .tasks
            .iter()
            .find(|t| t.task_ref.as_deref() == Some("apply-middleware"))
            .expect("apply-middleware task present");
        assert_eq!(apply.blocked_by, vec!["setup-middleware"]);
        // `document-headers` waits on BOTH predecessors.
        let document = spec
            .tasks
            .iter()
            .find(|t| t.task_ref.as_deref() == Some("document-headers"))
            .expect("document-headers task present");
        assert_eq!(
            document.blocked_by,
            vec!["setup-middleware", "apply-middleware"],
        );
    }

    // T7 regression tests — `~` in `[contract].workspace` must be expanded at
    // parse time, $HOME unset must loud-fail, relative paths must be rejected.

    #[test]
    fn t7_tilde_slash_expands_against_home() {
        let got =
            expand_workspace_with_home(PathBuf::from("~/foo/bar"), Some("/tmp/t7-home".into()))
                .expect("should expand");
        assert_eq!(got, PathBuf::from("/tmp/t7-home/foo/bar"));
    }

    #[test]
    fn t7_bare_tilde_expands_to_home() {
        let got = expand_workspace_with_home(PathBuf::from("~"), Some("/tmp/t7-home-2".into()))
            .expect("should expand");
        assert_eq!(got, PathBuf::from("/tmp/t7-home-2"));
    }

    #[test]
    fn t7_tilde_with_home_unset_loud_fails() {
        let err = expand_workspace_with_home(PathBuf::from("~/foo"), None)
            .expect_err("HOME unset must error");
        assert!(matches!(err, ConfigError::WorkspaceHomeUnset));
    }

    #[test]
    fn t7_relative_path_rejected() {
        let err = expand_workspace_with_home(PathBuf::from("relative/path"), Some("/tmp".into()))
            .expect_err("relative path must be rejected");
        assert!(matches!(err, ConfigError::WorkspaceNotAbsolute { .. }));
    }

    #[test]
    fn t7_absolute_path_passes_through() {
        let got =
            expand_workspace_with_home(PathBuf::from("/Users/test/repo"), Some("/tmp".into()))
                .expect("absolute should pass");
        assert_eq!(got, PathBuf::from("/Users/test/repo"));
    }
}
