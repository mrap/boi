//! Plan-revision types — the typed payload of a `plan_revision` worker phase.
//!
//! Plan revision is in v1.0 (Q4): a `plan_revision` worker can add, remove, or
//! retarget tasks. [`PlanRevision`] is a batch of [`PlanEdit`]s.
//!
//! ## Transport (G13.4 + review D3)
//!
//! These types live in `crate::types::plan` (layer 0). The `plan_revision`
//! worker delivers its output via a *typed artifact channel*: the phase's
//! prompt template instructs the worker to write its `PlanRevision` JSON to a
//! harness-designated path (`~/.boi/v2/revisions/<phase_run_id>.json`), and the
//! worker emits a normal `WorkerVerdict`. `deny_unknown_fields` on
//! `PlanRevision` is the strict-parse guarantee for that artifact channel.
//! Phase 5b imports these from here — it never defines them.

use serde::{Deserialize, Serialize};

use crate::types::context;
use crate::types::ids::TaskId;

/// A batch of plan edits produced by a `plan_revision` worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanRevision {
    /// The edits to apply, in order.
    pub edits: Vec<PlanEdit>,
}

/// A single edit to a spec's task plan.
///
/// Tagged-union serde (`{"type": "...", ...}`) with `deny_unknown_fields`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanEdit {
    /// Add a new task to the plan.
    AddTask {
        /// The behavior the new task implements.
        behavior: String,
        /// The new task's verifications.
        verifications: Vec<context::Verification>,
        /// Tasks the new task is blocked by.
        blocked_by: Vec<TaskId>,
    },
    /// Remove a task from the plan.
    RemoveTask {
        /// The task to remove.
        task_id: TaskId,
        /// A replacement task, if the removed task is being replaced rather
        /// than cut entirely.
        replacement: Option<TaskId>,
    },
    /// Retarget a task's dependency edges.
    RetargetDeps {
        /// The task whose dependencies change.
        task_id: TaskId,
        /// The new set of dependency task IDs.
        new_deps: Vec<TaskId>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::context::Verification;

    fn task(id: &str) -> TaskId {
        TaskId::new(id).unwrap()
    }

    #[test]
    fn plan_revision_roundtrips() {
        let rev = PlanRevision {
            edits: vec![
                PlanEdit::AddTask {
                    behavior: "add config flag".into(),
                    verifications: vec![Verification::Command {
                        name: None,
                        command: "cargo test".into(),
                    }],
                    blocked_by: vec![task("T0000001a")],
                },
                PlanEdit::RemoveTask {
                    task_id: task("T0000002a"),
                    replacement: Some(task("T0000003a")),
                },
                PlanEdit::RetargetDeps {
                    task_id: task("T0000004a"),
                    new_deps: vec![task("T0000001a"), task("T0000003a")],
                },
            ],
        };
        let json = serde_json::to_string(&rev).unwrap();
        let back: PlanRevision = serde_json::from_str(&json).unwrap();
        assert_eq!(back.edits.len(), 3);
        assert!(matches!(back.edits[0], PlanEdit::AddTask { .. }));
        assert!(matches!(back.edits[1], PlanEdit::RemoveTask { .. }));
        assert!(matches!(back.edits[2], PlanEdit::RetargetDeps { .. }));
    }

    #[test]
    fn plan_edit_variants_carry_type_discriminator() {
        let edit = PlanEdit::RemoveTask {
            task_id: task("T0000001a"),
            replacement: None,
        };
        let v: serde_json::Value = serde_json::to_value(&edit).unwrap();
        assert_eq!(v.get("type").and_then(|t| t.as_str()), Some("remove_task"));
    }

    #[test]
    fn deny_unknown_fields_rejects_extra_keys() {
        // Extra key on PlanRevision.
        assert!(serde_json::from_str::<PlanRevision>(r#"{"edits":[],"bogus":1}"#).is_err());
        // Extra key inside a PlanEdit variant.
        assert!(
            serde_json::from_str::<PlanEdit>(
                r#"{"type":"remove_task","task_id":"T0000001a","extra":true}"#,
            )
            .is_err()
        );
        // Unknown PlanEdit discriminator.
        assert!(serde_json::from_str::<PlanEdit>(r#"{"type":"no_such_edit"}"#).is_err());
    }
}
