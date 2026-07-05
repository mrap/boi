//! The `BoiEvent` enum — every legal state transition and observational event.
//!
//! `BoiEvent` is the single currency of the event bus (§2). Every notable thing
//! flows through `EventBus::emit()` as one of these variants.
//!
//! ## Construction-site-narrowed transitions
//!
//! The enum has one variant per *legal* transition and no variant that names an
//! illegal one — there is no `TaskUnpassed` / `TaskUnfailed` / `SpecRestarted`,
//! so an illegal transition cannot be written in code.
//!
//! What the type system buys here, honestly (Batch A review — L1 + L4):
//!
//! - **No illegal-transition variant is nameable.** `passing → anything` cannot
//!   be expressed because no variant expresses it.
//! - **`match` exhaustiveness.** Adding a variant produces a compile error in
//!   every consumer until handled.
//!
//! What it does NOT buy: the type system does not know which `TaskId` is
//! currently `passing` — `BoiEvent::TaskBlocked { task_id, .. }` can be
//! *constructed* for any task. Emission-time legality ("is this task in a state
//! that permits `Blocked`?") is enforced at runtime by the bus chokepoint in
//! Phase 4 `transitions.rs`. This is construction-site-narrowed + runtime-
//! enforced — NOT "compile-time impossible".

use serde::{Deserialize, Serialize};

use crate::types::decision::DecisionRecord;
use crate::types::ids::{PhaseRunId, SpecId, TaskId};
use crate::types::reasons::{BlockedReason, CancellationReason, FailureReason};
use crate::types::verdict::{Evidence, WorkerVerdict};

/// A single event on the BOI event bus.
///
/// Internally tagged on a `type` field. All variants derive `Debug`, `Clone`,
/// `Serialize`, `Deserialize`. The runtime transition guard is Phase 4's job;
/// this type only defines the variant set.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BoiEvent {
    // ---- Spec-level lifecycle — one variant per legal transition ----
    /// `queued → running`.
    SpecStarted {
        /// The spec that started.
        spec_id: SpecId,
    },
    /// `running → completed` (terminal).
    SpecCompleted {
        /// The spec that completed.
        spec_id: SpecId,
    },
    /// `running → failed` (terminal).
    SpecFailed {
        /// The spec that failed.
        spec_id: SpecId,
        /// Why it failed.
        reason: FailureReason,
    },
    /// `running → canceled` (terminal).
    SpecCanceled {
        /// The spec that was canceled.
        spec_id: SpecId,
        /// Why it was canceled.
        reason: CancellationReason,
    },

    // ---- Task-level lifecycle — one variant per legal transition ----
    /// `not_started → active`.
    TaskStarted {
        /// The owning spec.
        spec_id: SpecId,
        /// The task that started.
        task_id: TaskId,
    },
    /// `active → blocked`.
    TaskBlocked {
        /// The owning spec.
        spec_id: SpecId,
        /// The task that blocked.
        task_id: TaskId,
        /// Why it blocked.
        reason: BlockedReason,
    },
    /// `blocked → active`.
    TaskUnblocked {
        /// The owning spec.
        spec_id: SpecId,
        /// The task that unblocked.
        task_id: TaskId,
    },
    /// `active → passing` (terminal, irreversible).
    TaskPassed {
        /// The owning spec.
        spec_id: SpecId,
        /// The task that passed.
        task_id: TaskId,
        /// Evidence the task passed.
        evidence: Evidence,
    },
    /// `any → canceled` (terminal).
    TaskCanceled {
        /// The owning spec.
        spec_id: SpecId,
        /// The task that was canceled.
        task_id: TaskId,
        /// Why it was canceled.
        reason: CancellationReason,
    },

    // ---- Plan ----
    /// The spec's plan was revised — `spec_versions` gets a new append-only row.
    PlanRevised {
        /// The spec whose plan was revised.
        spec_id: SpecId,
        /// The revision diff.
        diff: serde_json::Value,
        /// What triggered the revision.
        trigger: String,
        /// Structured metadata about the trigger.
        trigger_meta: serde_json::Value,
    },

    // ---- Observational — also drive `phase_runs` table writes (Phase 3) ----
    /// A phase run started — the bus INSERTs a `phase_runs` row.
    PhaseStarted {
        /// The phase run this event opens. The Phase 5a orchestrator allocates
        /// the id in `run_phase` (raw, no insert); the bus's `persist` does the
        /// `phase_runs` INSERT keyed on this id. `PhaseCompleted` carries the
        /// same id so the two-phase write (insert at start, update at end)
        /// correlates. (Phase 4 erratum — Phase 1 shipped these two variants
        /// with no `phase_run_id`, leaving `phase_runs` start↔end uncorrelatable
        /// and the persist table's `insert_start` call unwritable.)
        phase_run_id: PhaseRunId,
        /// The owning spec.
        spec_id: SpecId,
        /// The owning task — `None` for spec-level phases.
        task_id: Option<TaskId>,
        /// The phase name.
        phase: String,
        /// The provider executing the phase.
        provider: String,
        /// The model executing the phase.
        model: String,
        /// Which iteration of the phase this is (G14.2 — populated by the
        /// Phase 5a drain task from `PhaseContext.iteration`).
        iteration: u32,
    },
    /// A phase run completed — the bus UPDATEs the `phase_runs` row.
    PhaseCompleted {
        /// The phase run this event closes — the same id its opening
        /// `PhaseStarted` carried, so the bus's `persist` can `update_end` the
        /// correct `phase_runs` row (Phase 4 erratum — see `PhaseStarted`).
        phase_run_id: PhaseRunId,
        /// The owning spec.
        spec_id: SpecId,
        /// The owning task — `None` for spec-level phases.
        task_id: Option<TaskId>,
        /// The phase name.
        phase: String,
        /// The worker's verdict.
        verdict: WorkerVerdict,
        /// Input tokens consumed.
        tokens_in: u64,
        /// Output tokens produced.
        tokens_out: u64,
        /// Wall-clock duration, milliseconds.
        duration_ms: u64,
    },
    /// A worker recorded a decision.
    DecisionMade {
        /// The recorded decision.
        decision: DecisionRecord,
    },
    /// A worker filed a `task_report` (S4 — carries the `blocking` flag).
    ReportReceived {
        /// The owning spec.
        spec_id: SpecId,
        /// The reporting task.
        task_id: TaskId,
        /// Advisory report kind (a `String`, not an enum — S12).
        kind: String,
        /// The report payload.
        payload: serde_json::Value,
        /// Whether the report blocks the task pending a plan revision.
        blocking: bool,
    },
    /// A verification command was checked.
    VerifyChecked {
        /// The owning spec.
        spec_id: SpecId,
        /// The task the verification ran for.
        task_id: TaskId,
        /// The verification level (`l1`/`l2`/`l3`).
        level: String,
        /// The command that ran.
        command: String,
        /// The command's exit code.
        exit_code: i32,
        /// An excerpt of the command's stdout.
        stdout_excerpt: String,
    },
    /// An MCP tool was invoked.
    ToolInvoked {
        /// The owning spec.
        spec_id: SpecId,
        /// The owning task — `None` for spec-level invocations.
        task_id: Option<TaskId>,
        /// The tool name.
        tool: String,
        /// A summary of the call arguments.
        args_summary: String,
        /// A summary of the call result.
        result_summary: String,
    },
    /// An error was encountered.
    ErrorEncountered {
        /// The owning spec.
        spec_id: SpecId,
        /// The owning task — `None` for spec-level errors.
        task_id: Option<TaskId>,
        /// The phase the error occurred in (G24.1). Carried so the 8a OTel
        /// observer can stamp the `boi.error` span event with `boi.phase`
        /// and `boi failures top`'s PHASE column resolves against real
        /// traces. The emit sites know the phase (the deterministic/Goose
        /// executors and the MCP handlers all hold it).
        phase: String,
        /// What went wrong.
        error: String,
        /// Why it went wrong.
        why: String,
        /// A proposed fix, if one is known.
        fix_proposed: Option<String>,
        /// The failure fingerprint, for grouping recurrences.
        fingerprint: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ids::SpecId;
    use crate::types::reasons::ErrorWhyFix;
    use crate::types::verdict::VerdictOutcome;

    fn spec() -> SpecId {
        SpecId::new("S0000001a").unwrap()
    }
    fn task() -> TaskId {
        TaskId::new("T0000001a").unwrap()
    }
    fn phase_run() -> PhaseRunId {
        PhaseRunId::new("P0000001a").unwrap()
    }

    fn roundtrip(event: &BoiEvent) -> BoiEvent {
        let json = serde_json::to_string(event).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn spec_lifecycle_variant_roundtrips() {
        let e = BoiEvent::SpecFailed {
            spec_id: spec(),
            reason: FailureReason::DaemonCrash,
        };
        let back = roundtrip(&e);
        assert!(matches!(
            back,
            BoiEvent::SpecFailed {
                reason: FailureReason::DaemonCrash,
                ..
            }
        ));
        // The `type` tag is present and snake_case.
        let v: serde_json::Value = serde_json::to_value(&e).unwrap();
        assert_eq!(v.get("type").and_then(|t| t.as_str()), Some("spec_failed"));
    }

    #[test]
    fn task_lifecycle_variant_roundtrips() {
        let e = BoiEvent::TaskBlocked {
            spec_id: spec(),
            task_id: task(),
            reason: BlockedReason::WorkspaceUnclean {
                details: "dirty tree".into(),
            },
        };
        let back = roundtrip(&e);
        assert!(matches!(back, BoiEvent::TaskBlocked { .. }));
    }

    #[test]
    fn plan_variant_roundtrips() {
        let e = BoiEvent::PlanRevised {
            spec_id: spec(),
            diff: serde_json::json!({"edits": []}),
            trigger: "task_report".into(),
            trigger_meta: serde_json::json!({"task_id": "T0000001a"}),
        };
        let back = roundtrip(&e);
        let BoiEvent::PlanRevised { diff, trigger, .. } = back else {
            unreachable!("roundtripped a PlanRevised");
        };
        assert_eq!(trigger, "task_report");
        assert!(diff.get("edits").is_some());
    }

    #[test]
    fn observational_variant_roundtrips() {
        // PhaseStarted carries the G14.2 `iteration` field and the Phase 4
        // erratum `phase_run_id`.
        let e = BoiEvent::PhaseStarted {
            phase_run_id: phase_run(),
            spec_id: spec(),
            task_id: Some(task()),
            phase: "execute".into(),
            provider: "claude_code".into(),
            model: "claude-opus-4-7".into(),
            iteration: 2,
        };
        let back = roundtrip(&e);
        let BoiEvent::PhaseStarted {
            iteration,
            phase_run_id,
            ..
        } = back
        else {
            unreachable!("roundtripped a PhaseStarted");
        };
        assert_eq!(iteration, 2);
        assert_eq!(phase_run_id, phase_run());
    }

    #[test]
    fn phase_completed_carries_verdict() {
        let e = BoiEvent::PhaseCompleted {
            phase_run_id: phase_run(),
            spec_id: spec(),
            task_id: None,
            phase: "validate".into(),
            verdict: WorkerVerdict {
                synopsis: "all checks green".into(),
                outcome: VerdictOutcome::Passing {
                    evidence: Evidence::default(),
                },
            },
            tokens_in: 1000,
            tokens_out: 200,
            duration_ms: 4200,
        };
        let back = roundtrip(&e);
        assert!(matches!(back, BoiEvent::PhaseCompleted { .. }));
    }

    /// Exhaustive match over every `BoiEvent` variant.
    ///
    /// Intentional canary: adding a variant to `BoiEvent` breaks this test
    /// until the new variant is added here — proving every variant is
    /// reachable and forcing a conscious decision on each one.
    #[test]
    fn every_variant_is_reachable() {
        fn discriminant(e: &BoiEvent) -> &'static str {
            match e {
                BoiEvent::SpecStarted { .. } => "spec_started",
                BoiEvent::SpecCompleted { .. } => "spec_completed",
                BoiEvent::SpecFailed { .. } => "spec_failed",
                BoiEvent::SpecCanceled { .. } => "spec_canceled",
                BoiEvent::TaskStarted { .. } => "task_started",
                BoiEvent::TaskBlocked { .. } => "task_blocked",
                BoiEvent::TaskUnblocked { .. } => "task_unblocked",
                BoiEvent::TaskPassed { .. } => "task_passed",
                BoiEvent::TaskCanceled { .. } => "task_canceled",
                BoiEvent::PlanRevised { .. } => "plan_revised",
                BoiEvent::PhaseStarted { .. } => "phase_started",
                BoiEvent::PhaseCompleted { .. } => "phase_completed",
                BoiEvent::DecisionMade { .. } => "decision_made",
                BoiEvent::ReportReceived { .. } => "report_received",
                BoiEvent::VerifyChecked { .. } => "verify_checked",
                BoiEvent::ToolInvoked { .. } => "tool_invoked",
                BoiEvent::ErrorEncountered { .. } => "error_encountered",
            }
        }

        let ewf = || ErrorWhyFix {
            error: "e".into(),
            why: "w".into(),
            fix: "f".into(),
        };
        let all = [
            BoiEvent::SpecStarted { spec_id: spec() },
            BoiEvent::SpecCompleted { spec_id: spec() },
            BoiEvent::SpecFailed {
                spec_id: spec(),
                reason: FailureReason::DaemonCrash,
            },
            BoiEvent::SpecCanceled {
                spec_id: spec(),
                reason: CancellationReason::SpecCanceled,
            },
            BoiEvent::TaskStarted {
                spec_id: spec(),
                task_id: task(),
            },
            BoiEvent::TaskBlocked {
                spec_id: spec(),
                task_id: task(),
                reason: BlockedReason::Manual {
                    operator_note: None,
                },
            },
            BoiEvent::TaskUnblocked {
                spec_id: spec(),
                task_id: task(),
            },
            BoiEvent::TaskPassed {
                spec_id: spec(),
                task_id: task(),
                evidence: Evidence::default(),
            },
            BoiEvent::TaskCanceled {
                spec_id: spec(),
                task_id: task(),
                reason: CancellationReason::SpecCanceled,
            },
            BoiEvent::PlanRevised {
                spec_id: spec(),
                diff: serde_json::Value::Null,
                trigger: "t".into(),
                trigger_meta: serde_json::Value::Null,
            },
            BoiEvent::PhaseStarted {
                phase_run_id: phase_run(),
                spec_id: spec(),
                task_id: None,
                phase: "p".into(),
                provider: "claude_code".into(),
                model: "m".into(),
                iteration: 0,
            },
            BoiEvent::PhaseCompleted {
                phase_run_id: phase_run(),
                spec_id: spec(),
                task_id: None,
                phase: "p".into(),
                verdict: WorkerVerdict {
                    synopsis: "s".into(),
                    outcome: VerdictOutcome::Redo { reason: "r".into() },
                },
                tokens_in: 0,
                tokens_out: 0,
                duration_ms: 0,
            },
            BoiEvent::DecisionMade {
                decision: DecisionRecord::new_authored(
                    crate::types::ids::DecisionId::new("D0000001a").unwrap(),
                    spec(),
                    None,
                    "t".into(),
                    "s".into(),
                    "r".into(),
                    vec![],
                    None,
                    chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
                )
                .unwrap(),
            },
            BoiEvent::ReportReceived {
                spec_id: spec(),
                task_id: task(),
                kind: "scope_gap".into(),
                payload: serde_json::Value::Null,
                blocking: true,
            },
            BoiEvent::VerifyChecked {
                spec_id: spec(),
                task_id: task(),
                level: "l1".into(),
                command: "cargo test".into(),
                exit_code: 0,
                stdout_excerpt: "ok".into(),
            },
            BoiEvent::ToolInvoked {
                spec_id: spec(),
                task_id: None,
                tool: "decision_record".into(),
                args_summary: "{}".into(),
                result_summary: "ok".into(),
            },
            BoiEvent::ErrorEncountered {
                spec_id: spec(),
                task_id: None,
                phase: "execute".into(),
                error: ewf().error,
                why: ewf().why,
                fix_proposed: Some(ewf().fix),
                fingerprint: "abc123".into(),
            },
        ];

        // 17 variants — every one constructed and routed through the match.
        assert_eq!(all.len(), 17);
        let mut seen = std::collections::HashSet::new();
        for e in &all {
            let tag = discriminant(e);
            // The discriminant string matches serde's `type` tag.
            let v: serde_json::Value = serde_json::to_value(e).unwrap();
            assert_eq!(v.get("type").and_then(|t| t.as_str()), Some(tag));
            assert!(seen.insert(tag), "duplicate variant tag {tag}");
        }
        assert_eq!(seen.len(), 17, "every BoiEvent variant must be distinct");
    }
}
