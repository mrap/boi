//! The transition guard — the runtime arbiter of state-machine legality.
//!
//! Phase 1's per-transition `BoiEvent` variants mean an *illegal* transition
//! cannot be **named** in code; this module enforces the other half —
//! *emission-time legality* against the persisted current state. The event bus
//! (`service::bus`) calls [`check_task`] / [`check_spec`] inside its persist
//! phase before writing any state row: workers PROPOSE a transition, the bus
//! DISPOSES (design §6).
//!
//! Both functions are pure — no I/O, no async. They are a small, exhaustively
//! tested legality matrix; per Batch A consensus, this module *is* the real
//! state-machine enforcement, while the type system only buys `match`
//! exhaustiveness.
//!
//! ## Deviation from the plan's Task 4.1 signature
//!
//! The plan gives `check_task(from, to)` / `check_spec(from, to)` — no id
//! parameter — yet its [`TransitionError`] enum carries a `task_id` / `spec_id`
//! field. A pure `(from, to)` function cannot fill that field, so the error
//! would either be incomplete or the bus would have to re-wrap it. The id is an
//! `Arc<str>` newtype (one atomic increment to clone), so this module takes the
//! id by reference and threads it into the error: an illegal-transition error
//! is self-describing — it names *which* task or spec — honouring the no-quiet-
//! failures rule. Documented as a Phase 4 deviation.

use crate::types::ids::{SpecId, TaskId};
use crate::types::state::{SpecStatus, TaskState};

/// An attempted state transition is not legal for the entity's current state.
///
/// Returned by [`check_task`] / [`check_spec`] and wrapped by the event bus
/// into its own error type. Each variant names the entity so the failure is
/// self-describing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransitionError {
    /// A task was asked to move between two states with no legal edge.
    #[error("illegal task transition for {task_id}: {from} -> {to}")]
    IllegalTaskTransition {
        /// The task whose transition was rejected.
        task_id: TaskId,
        /// The task's current (persisted) state.
        from: TaskState,
        /// The state the transition tried to reach.
        to: TaskState,
    },
    /// A spec was asked to move between two statuses with no legal edge.
    #[error("illegal spec transition for {spec_id}: {from} -> {to}")]
    IllegalSpecTransition {
        /// The spec whose transition was rejected.
        spec_id: SpecId,
        /// The spec's current (persisted) status.
        from: SpecStatus,
        /// The status the transition tried to reach.
        to: SpecStatus,
    },
}

/// Check a task state transition for legality (design §6).
///
/// Legal task transitions:
///
/// ```text
/// not_started → active                                   (TaskStarted)
/// active      → blocked                                  (TaskBlocked)
/// blocked     → active                                   (TaskUnblocked)
/// active      → passing   [terminal]                     (TaskPassed)
/// {not_started, active, blocked} → canceled  [terminal]  (TaskCanceled)
/// ```
///
/// `passing` and `canceled` are terminal — every transition *out* of them is
/// rejected. `not_started → passing` is rejected: a task must pass through
/// `active`. A no-op self-transition (`from == to`) is also rejected — the bus
/// maps each lifecycle `BoiEvent` to a genuine state change, so a self-edge is
/// never a legal emission.
pub fn check_task(task_id: &TaskId, from: TaskState, to: TaskState) -> Result<(), TransitionError> {
    use TaskState::{Active, Blocked, Canceled, NotStarted, Passing};
    let legal = matches!(
        (from, to),
        (NotStarted, Active)        // TaskStarted
            | (Active, Blocked)     // TaskBlocked
            | (Blocked, Active)     // TaskUnblocked
            | (Active, Passing)     // TaskPassed
            | (NotStarted, Canceled) // TaskCanceled — from any non-terminal state
            | (Active, Canceled)
            | (Blocked, Canceled)
    );
    if legal {
        Ok(())
    } else {
        Err(TransitionError::IllegalTaskTransition {
            task_id: task_id.clone(),
            from,
            to,
        })
    }
}

/// Check a spec status transition for legality (design §6).
///
/// Legal spec transitions:
///
/// ```text
/// queued  → running                                      (SpecStarted)
/// running → completed | failed | canceled  [terminal]    (SpecCompleted / SpecFailed / SpecCanceled)
/// queued  → canceled  [terminal]                         (SpecCanceled)
/// ```
///
/// `completed`, `failed`, `canceled` are terminal. Note the asymmetry pinned by
/// review item 33: `queued → canceled` is legal (a spec can be canceled before
/// it ever runs) but `queued → failed` is **not** — a spec only fails while
/// running.
pub fn check_spec(
    spec_id: &SpecId,
    from: SpecStatus,
    to: SpecStatus,
) -> Result<(), TransitionError> {
    use SpecStatus::{Canceled, Completed, Failed, Queued, Running};
    let legal = matches!(
        (from, to),
        (Queued, Running)         // SpecStarted
            | (Running, Completed) // SpecCompleted
            | (Running, Failed)    // SpecFailed
            | (Running, Canceled)  // SpecCanceled — while running
            | (Queued, Canceled) // SpecCanceled — before running (asymmetry: no Queued→Failed)
    );
    if legal {
        Ok(())
    } else {
        Err(TransitionError::IllegalSpecTransition {
            spec_id: spec_id.clone(),
            from,
            to,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> TaskId {
        TaskId::new("T0000001a").unwrap()
    }
    fn spec() -> SpecId {
        SpecId::new("S0000001a").unwrap()
    }

    /// Every legal task edge is accepted; every other pair (including each
    /// self-edge and every transition out of a terminal state) is rejected.
    #[test]
    fn test_l1_task_matrix_is_exhaustive() {
        use TaskState::{Active, Blocked, Canceled, NotStarted, Passing};
        let all = [NotStarted, Active, Blocked, Passing, Canceled];
        let legal: &[(TaskState, TaskState)] = &[
            (NotStarted, Active),
            (Active, Blocked),
            (Blocked, Active),
            (Active, Passing),
            (NotStarted, Canceled),
            (Active, Canceled),
            (Blocked, Canceled),
        ];
        let mut legal_count = 0;
        for &from in &all {
            for &to in &all {
                let want_ok = legal.contains(&(from, to));
                let got = check_task(&task(), from, to);
                assert_eq!(
                    got.is_ok(),
                    want_ok,
                    "check_task({from}, {to}) legality mismatch",
                );
                if want_ok {
                    legal_count += 1;
                }
            }
        }
        // Exactly 7 legal edges out of the 25-pair matrix.
        assert_eq!(legal_count, 7, "expected exactly 7 legal task edges");
    }

    /// `passing` is terminal — no transition leaves it.
    #[test]
    fn test_l1_task_passing_is_terminal() {
        use TaskState::{Active, Blocked, Canceled, NotStarted, Passing};
        for to in [NotStarted, Active, Blocked, Passing, Canceled] {
            let err = check_task(&task(), Passing, to).unwrap_err();
            assert!(
                matches!(
                    err,
                    TransitionError::IllegalTaskTransition { from: Passing, .. }
                ),
                "passing -> {to} must be rejected, got {err:?}",
            );
        }
    }

    /// `canceled` is terminal — no transition leaves it.
    #[test]
    fn test_l1_task_canceled_is_terminal() {
        use TaskState::{Active, Blocked, Canceled, NotStarted, Passing};
        for to in [NotStarted, Active, Blocked, Passing, Canceled] {
            assert!(
                check_task(&task(), Canceled, to).is_err(),
                "canceled -> {to} must be rejected",
            );
        }
    }

    /// A task cannot jump straight from `not_started` to `passing` — it must
    /// pass through `active`.
    #[test]
    fn test_l1_task_not_started_to_passing_rejected() {
        let err = check_task(&task(), TaskState::NotStarted, TaskState::Passing).unwrap_err();
        assert!(matches!(
            err,
            TransitionError::IllegalTaskTransition {
                from: TaskState::NotStarted,
                to: TaskState::Passing,
                ..
            }
        ));
        // The error names the offending task.
        let TransitionError::IllegalTaskTransition { task_id, .. } = err else {
            unreachable!("matched IllegalTaskTransition above");
        };
        assert_eq!(task_id, task());
    }

    /// Every legal spec edge is accepted; every other pair is rejected.
    #[test]
    fn test_l1_spec_matrix_is_exhaustive() {
        use SpecStatus::{Canceled, Completed, Failed, Queued, Running};
        let all = [Queued, Running, Completed, Failed, Canceled];
        let legal: &[(SpecStatus, SpecStatus)] = &[
            (Queued, Running),
            (Running, Completed),
            (Running, Failed),
            (Running, Canceled),
            (Queued, Canceled),
        ];
        let mut legal_count = 0;
        for &from in &all {
            for &to in &all {
                let want_ok = legal.contains(&(from, to));
                assert_eq!(
                    check_spec(&spec(), from, to).is_ok(),
                    want_ok,
                    "check_spec({from}, {to}) legality mismatch",
                );
                if want_ok {
                    legal_count += 1;
                }
            }
        }
        assert_eq!(legal_count, 5, "expected exactly 5 legal spec edges");
    }

    /// The spec asymmetry pinned by review item 33: `queued → canceled` is
    /// legal, `queued → failed` is not.
    #[test]
    fn test_l1_spec_queued_canceled_legal_but_queued_failed_rejected() {
        assert!(
            check_spec(&spec(), SpecStatus::Queued, SpecStatus::Canceled).is_ok(),
            "queued -> canceled must be legal",
        );
        let err = check_spec(&spec(), SpecStatus::Queued, SpecStatus::Failed).unwrap_err();
        assert!(matches!(
            err,
            TransitionError::IllegalSpecTransition {
                from: SpecStatus::Queued,
                to: SpecStatus::Failed,
                ..
            }
        ));
    }

    /// A spec's terminal statuses (`completed` / `failed` / `canceled`) have no
    /// outgoing edge — `completed → running` and friends are rejected.
    #[test]
    fn test_l1_spec_terminal_states_have_no_exit() {
        use SpecStatus::{Canceled, Completed, Failed, Queued, Running};
        for from in [Completed, Failed, Canceled] {
            for to in [Queued, Running, Completed, Failed, Canceled] {
                assert!(
                    check_spec(&spec(), from, to).is_err(),
                    "{from} -> {to} must be rejected (terminal status)",
                );
            }
        }
    }
}
