//! Worker-context types — the authored intent and prior-run history handed to
//! a phase at clock-in.
//!
//! [`PhaseContext`] is the full payload the harness assembles at the start of
//! every `phase_runs` row (§7.1). It renders to the `<phase_context>` XML block
//! in Phase 5c. [`SpecContract`] / [`TaskContract`] are the immutable authored
//! intent; [`PhaseRunSummary`] is the digest of one prior phase run.
//!
//! `Verification` is deliberately NOT flat-re-exported from `types/mod.rs` — the
//! bare name collides with `event::VerifyChecked` and `config::RawVerification`
//! at call sites. Reference it qualified: `crate::types::context::Verification`.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A normalized skill reference (§3.7).
///
/// Lives in `types/` so `PhaseContext` doesn't drag a dependency on `config/`
/// (the module-dep-audit forbids `types → config`). `config::SkillRef` is a
/// re-export of this type so parser sites continue to refer to it via
/// `config::SkillRef`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SkillRef {
    /// The skill name — rendered into the Goose recipe `extensions:` field.
    pub name: String,
}
use crate::types::decision::DecisionRecord;
use crate::types::ids::{DecisionId, PhaseRunId, SpecId, TaskId};
use crate::types::reasons::ErrorWhyFix;

/// The spec-level authored contract — immutable for a given run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecContract {
    /// The sprint-contract scope anchor (Lec 11).
    pub scope: String,
    /// The workspace repository root.
    pub workspace: PathBuf,
    /// Base branch for the integration worktree (A6 — required field).
    pub base_branch: String,
    /// Paths or globs the spec must not touch.
    pub exclusions: Vec<String>,
    /// Spec-level verifications.
    pub verifications: Vec<Verification>,
    /// Files the spec must emit.
    pub must_emit: Vec<PathBuf>,
}

/// A single verification — either an intent (LLM-judged) or a command
/// (deterministically run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    /// An intent verification — a worker judges whether the intent holds.
    Intent {
        /// Optional human-readable name.
        name: Option<String>,
        /// The intent statement to verify.
        intent: String,
    },
    /// A command verification — the command is run and its exit code checked.
    Command {
        /// Optional human-readable name.
        name: Option<String>,
        /// The shell command to run.
        command: String,
    },
}

/// The task-level authored contract — immutable for a given run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskContract {
    /// The behavior the task must implement.
    pub behavior: String,
    /// Task-level verifications (at least one — §3.1).
    pub verifications: Vec<Verification>,
}

/// A digest of one authored task in the spec — surfaced to every phase via
/// [`PhaseContext::tasks`] so spec-level workers (plan, critique_plan, review)
/// can survey the task graph without re-parsing the spec snapshot.
///
/// The plan prompt asks the worker to "review `spec_contract.scope` against
/// the declared tasks"; without [`PhaseContext::tasks`] there were no
/// declared tasks in the rendered context (only the current task's
/// `task_contract`, and only on task-level phases at that). A strict worker
/// reading the prompt honestly judged "under-specified" and emitted `fail`;
/// this brief is the data the prompt asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBrief {
    /// The task's id.
    pub task_id: TaskId,
    /// The behavior the task implements.
    pub behavior: String,
    /// Task-level verifications.
    pub verifications: Vec<Verification>,
}

/// A digest of one prior phase run, surfaced to the next phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseRunSummary {
    /// The prior run's ID.
    pub id: PhaseRunId,
    /// The phase name.
    pub phase: String,
    /// Which iteration of the phase this was.
    pub phase_iteration: u32,
    /// The provider that executed it.
    pub provider: String,
    /// The run's synopsis.
    pub synopsis: String,
    /// The verdict outcome string (`passing`/`redo`/`blocked`/`fail`), if any.
    pub verdict_outcome: Option<String>,
    /// Files the run touched.
    pub files_touched: Vec<PathBuf>,
    /// Decisions the run recorded.
    pub decisions_made: Vec<DecisionId>,
    /// When the run completed — `None` while in progress.
    pub completed_at: Option<DateTime<Utc>>,
    /// Error detail — populated when `verdict_outcome` is `fail` or `blocked`
    /// (Q3 patch — required forwarding).
    pub error_why_fix: Option<ErrorWhyFix>,
}

/// The full context payload handed to a phase at clock-in (§7.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseContext {
    /// The spec being run.
    pub spec_id: SpecId,
    /// The task being run — `None` for spec-level phases.
    pub task_id: Option<TaskId>,
    /// The phase name.
    pub phase: String,
    /// This phase run's ID.
    pub phase_run_id: PhaseRunId,
    /// Which iteration of the phase this is.
    pub iteration: u32,
    /// The spec-level authored contract.
    pub spec_contract: SpecContract,
    /// The task-level authored contract — `None` for spec-level phases.
    pub task_contract: Option<TaskContract>,
    /// EVERY authored task in the spec — the digest the plan / critique_plan /
    /// review prompts ask the worker to survey. Empty for specs with no
    /// authored tasks (a degenerate but legal shape).
    pub tasks: Vec<TaskBrief>,
    /// Declared `[[skill]]` blocks from the spec — rendered into the Goose
    /// recipe `extensions:` field for worker phases (G23.1 → G26.3). Empty
    /// when the spec authored no skills.
    pub skills: Vec<SkillRef>,
    /// ALL decisions for the spec, pushed at clock-in (Q8).
    pub decisions: Vec<DecisionRecord>,
    /// Every prior phase run for this task (or spec).
    pub prior_phase_runs: Vec<PhaseRunSummary>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ids::SpecId;

    fn spec_contract() -> SpecContract {
        SpecContract {
            scope: "rate limiting".into(),
            workspace: PathBuf::from("/repo"),
            base_branch: "main".into(),
            exclusions: vec!["vendor/".into()],
            verifications: vec![
                Verification::Intent {
                    name: Some("scoped".into()),
                    intent: "stays within the api crate".into(),
                },
                Verification::Command {
                    name: None,
                    command: "cargo test".into(),
                },
            ],
            must_emit: vec![PathBuf::from("src/mw.rs")],
        }
    }

    fn roundtrip<T>(value: &T)
    where
        T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, value);
    }

    #[test]
    fn spec_contract_roundtrip() {
        roundtrip(&spec_contract());
    }

    #[test]
    fn task_contract_roundtrip() {
        roundtrip(&TaskContract {
            behavior: "add token bucket".into(),
            verifications: vec![Verification::Command {
                name: None,
                command: "cargo clippy".into(),
            }],
        });
    }

    #[test]
    fn verification_mixed_list_supports_intent_and_command() {
        // Both variants must coexist in one Vec and roundtrip together.
        let mixed = vec![
            Verification::Intent {
                name: None,
                intent: "no panics on the hot path".into(),
            },
            Verification::Command {
                name: Some("lint".into()),
                command: "cargo clippy -- -D warnings".into(),
            },
        ];
        let json = serde_json::to_string(&mixed).unwrap();
        let back: Vec<Verification> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, mixed);
        assert!(matches!(back[0], Verification::Intent { .. }));
        assert!(matches!(back[1], Verification::Command { .. }));
    }

    #[test]
    fn phase_run_summary_roundtrip() {
        roundtrip(&PhaseRunSummary {
            id: PhaseRunId::new("P0000001a").unwrap(),
            phase: "execute".into(),
            phase_iteration: 2,
            provider: "claude_code".into(),
            synopsis: "implemented the module".into(),
            verdict_outcome: Some("fail".into()),
            files_touched: vec![PathBuf::from("src/mw.rs")],
            decisions_made: vec![DecisionId::new("D0000001a").unwrap()],
            completed_at: DateTime::from_timestamp(1_700_000_000, 0),
            error_why_fix: Some(ErrorWhyFix {
                error: "test failed".into(),
                why: "off-by-one".into(),
                fix: "use <= ".into(),
            }),
        });
    }

    #[test]
    fn phase_context_roundtrip() {
        roundtrip(&PhaseContext {
            spec_id: SpecId::new("S0000001a").unwrap(),
            task_id: Some(TaskId::new("T0000001a").unwrap()),
            phase: "execute".into(),
            phase_run_id: PhaseRunId::new("P0000001a").unwrap(),
            iteration: 1,
            spec_contract: spec_contract(),
            task_contract: Some(TaskContract {
                behavior: "do it".into(),
                verifications: vec![Verification::Command {
                    name: None,
                    command: "cargo test".into(),
                }],
            }),
            tasks: vec![],
            skills: vec![],
            decisions: vec![],
            prior_phase_runs: vec![],
        });
    }
}
