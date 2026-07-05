//! Typed reasons for the non-happy-path state transitions.
//!
//! Every `blocked`, `canceled`, and `failed` transition carries a mandatory
//! typed reason — no free-text-only status. Three enums:
//!
//! - [`BlockedReason`] — why a task is `blocked` (recoverable).
//! - [`CancellationReason`] — why a task or spec is `canceled` (terminal).
//! - [`FailureReason`] — why a spec is `failed` (terminal).
//!
//! [`ErrorWhyFix`] is the shared error-detail triple (error / why / fix). It is
//! defined here and re-used by `verdict.rs`'s `VerdictOutcome::{Blocked, Fail}`.
//! The dependency is one-directional: `verdict` imports from `reasons`;
//! `reasons` never imports from `verdict` — so there is no cycle.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::types::ids::TaskId;

/// The error / why / fix triple: what went wrong, why, and how to fix it.
///
/// Shared across `reasons.rs` and `verdict.rs` (one-directional import).
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ErrorWhyFix {
    /// What went wrong.
    pub error: String,
    /// Why it went wrong.
    pub why: String,
    /// How to fix it.
    pub fix: String,
}

/// Why a task entered the `blocked` state.
///
/// Tagged-union serde (`{"type": "...", ...}`); deserialization rejects an
/// unknown `type` discriminator.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockedReason {
    /// Task depends on other tasks that are not yet `passing`.
    AwaitingDeps {
        /// The unmet dependency task IDs.
        unmet_deps: Vec<TaskId>,
    },
    /// An iteration loop hit its cap.
    CapExceeded {
        /// Name of the bounded loop (e.g. `execute_review`).
        loop_name: String,
        /// The cap that was hit.
        cap: u32,
        /// The error detail from the final failed iteration.
        last_error_why_fix: ErrorWhyFix,
    },
    /// A merge into the integration branch hit conflicts.
    ///
    /// Post-W4 schema (CRITIC §Weakness 4). Carries the full conflict set,
    /// the base/head sha pair the manifest in
    /// `~/.boi/v2/conflicts/<spec_id>/<timestamp>.json` records, and a
    /// typed `reason` string (e.g. `"GlobalLlmBudgetExhausted"` or
    /// `"AllStrategiesDeclined"`).
    MergeConflict {
        /// Conflicted file paths — covers BOTH resolved and unresolved
        /// files (W4: the manifest records the full conflict set, not
        /// just the unresolved tail).
        conflicts: Vec<PathBuf>,
        /// The base commit sha — the side being merged into.
        base_sha: String,
        /// The head commit sha — the side being merged.
        head_sha: String,
        /// Typed reason — names which gate fired (e.g.
        /// `"AllStrategiesDeclined"`, `"GlobalLlmBudgetExhausted"`).
        reason: String,
    },
    /// A clean-state precondition or postcondition check failed.
    WorkspaceUnclean {
        /// What was unclean.
        details: String,
    },
    /// The LLM provider failed (auth, rate limit, outage, ...).
    ProviderFailed {
        /// The provider that failed.
        provider: String,
        /// The last error returned by the provider.
        last_error: String,
    },
    /// A `task_report` arrived that needs a plan revision before this task
    /// can continue.
    PlanRevisionPending {
        /// The task whose report triggered the pending revision.
        triggered_by: TaskId,
        /// The advisory `kind` field of the report.
        report_kind: String,
    },
    /// An operator manually blocked the task.
    Manual {
        /// Optional operator note.
        operator_note: Option<String>,
    },
    /// The phase ran past its hard wall-clock budget — reaped REGARDLESS of a
    /// fresh heartbeat. A worker stuck inside a child process (e.g. a hung
    /// `cargo` build deadlocked on the package-cache lock) keeps heartbeating,
    /// so the heartbeat sweeper never catches it; this budget is the backstop
    /// that makes such a zombie a loud, terminal failure (SO S6).
    WallClockExceeded {
        /// The phase that exceeded the budget.
        phase: String,
        /// The budget, in seconds.
        budget_secs: u64,
        /// How long the phase had actually run, in seconds.
        elapsed_secs: u64,
    },
    /// The daemon shut down gracefully (SIGTERM / SIGINT — e.g. a binary swap
    /// or restart) while this task was in-flight. Rather than terminally
    /// failing the spec with [`FailureReason::DaemonCrash`] on the next boot,
    /// the graceful-drain pass parks each in-flight task here so the spec
    /// SURVIVES the restart as quiescent-blocked and is revivable via
    /// `boi unblock` (see `cli::boot::drain_in_flight_specs`). A true crash
    /// never runs the drain, so it keeps the `DaemonCrash` path — the drain
    /// executing IS the graceful signal.
    DaemonDraining,
}

/// Why a task or spec entered the terminal `canceled` state.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CancellationReason {
    /// An operator ran `boi cancel`.
    Operator {
        /// Optional operator note.
        note: Option<String>,
    },
    /// A plan revision cut or replaced this task (S9 merged variant).
    ///
    /// `replacement_task = None` → the scope was cut entirely.
    /// `replacement_task = Some(t)` → this task was replaced by task `t`.
    PlanRevisionCanceled {
        /// The task whose report triggered the revision.
        triggered_by: TaskId,
        /// The replacement task, if this task was replaced rather than cut.
        replacement_task: Option<TaskId>,
    },
    /// The parent spec was canceled, cascading to this task.
    SpecCanceled,
}

/// Why a spec entered the terminal `failed` state.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FailureReason {
    /// The spec-level review loop exhausted its iteration budget.
    SpecReviewExhausted {
        /// How many review iterations ran.
        iterations: u32,
        /// The critique from the final iteration.
        last_critique: String,
    },
    /// The daemon crashed mid-run.
    DaemonCrash,
    /// An operator ran `boi` and marked the spec failed.
    OperatorMarkedFailed {
        /// Optional operator note.
        note: Option<String>,
    },
    /// A pre-flight check failed before the spec could start (coverage-audit
    /// gap fix).
    PreflightFailed {
        /// What the pre-flight check found.
        details: String,
    },
    /// A spec-level phase ran past its hard wall-clock budget — reaped
    /// REGARDLESS of a fresh heartbeat (the spec-level analogue of
    /// [`BlockedReason::WallClockExceeded`]). The backstop for a spec-level
    /// phase whose worker is wedged inside a still-heartbeating child (SO S6).
    WallClockExceeded {
        /// The phase that exceeded the budget.
        phase: String,
        /// The budget, in seconds.
        budget_secs: u64,
        /// How long the phase had actually run, in seconds.
        elapsed_secs: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err_why_fix() -> ErrorWhyFix {
        ErrorWhyFix {
            error: "build failed".into(),
            why: "missing import".into(),
            fix: "add `use std::fmt;`".into(),
        }
    }

    fn task(id: &str) -> TaskId {
        TaskId::new(id).unwrap()
    }

    #[test]
    fn blocked_reason_variants_carry_type_discriminator() {
        for reason in [
            BlockedReason::AwaitingDeps {
                unmet_deps: vec![task("Tabc12345")],
            },
            BlockedReason::CapExceeded {
                loop_name: "execute_review".into(),
                cap: 3,
                last_error_why_fix: err_why_fix(),
            },
            BlockedReason::MergeConflict {
                conflicts: vec![PathBuf::from("src/config.rs")],
                base_sha: "deadbeef".into(),
                head_sha: "cafef00d".into(),
                reason: "AllStrategiesDeclined".into(),
            },
            BlockedReason::WorkspaceUnclean {
                details: "uncommitted changes".into(),
            },
            BlockedReason::ProviderFailed {
                provider: "claude_code".into(),
                last_error: "429".into(),
            },
            BlockedReason::PlanRevisionPending {
                triggered_by: task("Tdef12345"),
                report_kind: "scope_gap".into(),
            },
            BlockedReason::Manual {
                operator_note: None,
            },
            BlockedReason::WallClockExceeded {
                phase: "execute".into(),
                budget_secs: 1200,
                elapsed_secs: 1500,
            },
        ] {
            let v: serde_json::Value = serde_json::to_value(&reason).unwrap();
            assert!(
                v.get("type").and_then(|t| t.as_str()).is_some(),
                "variant {reason:?} serialized without a `type` tag",
            );
            // Roundtrip.
            let back: BlockedReason = serde_json::from_value(v).unwrap();
            assert_eq!(back, reason);
        }
    }

    #[test]
    fn cancellation_and_failure_variants_roundtrip() {
        let cancels = [
            CancellationReason::Operator {
                note: Some("scope cut".into()),
            },
            CancellationReason::PlanRevisionCanceled {
                triggered_by: task("Tabc12345"),
                replacement_task: Some(task("Tdef12345")),
            },
            CancellationReason::SpecCanceled,
        ];
        for c in cancels {
            let v = serde_json::to_value(&c).unwrap();
            assert_eq!(serde_json::from_value::<CancellationReason>(v).unwrap(), c);
        }

        let fails = [
            FailureReason::SpecReviewExhausted {
                iterations: 5,
                last_critique: "still broken".into(),
            },
            FailureReason::DaemonCrash,
            FailureReason::OperatorMarkedFailed { note: None },
            FailureReason::WallClockExceeded {
                phase: "plan".into(),
                budget_secs: 1200,
                elapsed_secs: 2000,
            },
        ];
        for f in fails {
            let v = serde_json::to_value(&f).unwrap();
            assert_eq!(serde_json::from_value::<FailureReason>(v).unwrap(), f);
        }
    }

    #[test]
    fn preflight_failed_roundtrips() {
        let f = FailureReason::PreflightFailed {
            details: "base_branch 'main' does not exist".into(),
        };
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains("\"type\":\"preflight_failed\""));
        assert_eq!(serde_json::from_str::<FailureReason>(&json).unwrap(), f);
    }

    #[test]
    fn deserialization_rejects_unknown_type() {
        assert!(serde_json::from_str::<BlockedReason>(r#"{"type":"no_such_reason"}"#).is_err());
        assert!(serde_json::from_str::<CancellationReason>(r#"{"type":"bogus"}"#).is_err());
        assert!(serde_json::from_str::<FailureReason>(r#"{"type":"bogus"}"#).is_err());
    }
}
