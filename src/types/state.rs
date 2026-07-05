//! Task and spec lifecycle state enums.
//!
//! Both enums have a stable lowercase string form (via [`TaskState::as_str`] /
//! [`SpecStatus::as_str`]) used as the SQLite `TEXT` storage value, and a
//! matching [`FromStr`] for reads.
//!
//! ## Stable string forms
//!
//! `TaskState`: `not_started` · `active` · `blocked` · `passing` · `canceled`.
//!
//! `SpecStatus`: `queued` · `running` · `completed` · `failed` · `canceled`.
//!
//! These strings are a storage contract — the SQLite `CHECK` constraints in
//! Phase 3 reference exactly these values. Do not rename a variant's string
//! form without a migration.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Lifecycle state of a single task.
///
/// `not_started → active → passing` is the happy path; `passing` is terminal
/// and irreversible (there is no `BoiEvent` that leaves it). `active` and
/// `blocked` transition into each other; `canceled` is terminal from any state.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Task exists but no phase has run yet.
    NotStarted,
    /// A phase is running (or about to).
    Active,
    /// Halted with a `BlockedReason`; needs intervention or a dep to clear.
    Blocked,
    /// Terminal, irreversible — all phases passed.
    Passing,
    /// Terminal — canceled with a `CancellationReason`.
    Canceled,
}

impl TaskState {
    /// Stable lowercase string form for SQLite `TEXT` storage.
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskState::NotStarted => "not_started",
            TaskState::Active => "active",
            TaskState::Blocked => "blocked",
            TaskState::Passing => "passing",
            TaskState::Canceled => "canceled",
        }
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskState {
    type Err = ParseStateError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "not_started" => Ok(TaskState::NotStarted),
            "active" => Ok(TaskState::Active),
            "blocked" => Ok(TaskState::Blocked),
            "passing" => Ok(TaskState::Passing),
            "canceled" => Ok(TaskState::Canceled),
            other => Err(ParseStateError {
                kind: "TaskState",
                got: other.to_owned(),
            }),
        }
    }
}

/// Lifecycle status of a spec.
///
/// `queued → running → completed` is the happy path; `failed` (carries a
/// `FailureReason`) and `canceled` (carries a `CancellationReason`) are the
/// other two terminal states.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpecStatus {
    /// Dispatched, not yet picked up.
    Queued,
    /// Actively executing.
    Running,
    /// Terminal — every task passed.
    Completed,
    /// Terminal — failed with a `FailureReason`.
    Failed,
    /// Terminal — canceled with a `CancellationReason`.
    Canceled,
}

impl SpecStatus {
    /// Stable lowercase string form for SQLite `TEXT` storage.
    pub fn as_str(&self) -> &'static str {
        match self {
            SpecStatus::Queued => "queued",
            SpecStatus::Running => "running",
            SpecStatus::Completed => "completed",
            SpecStatus::Failed => "failed",
            SpecStatus::Canceled => "canceled",
        }
    }
}

impl fmt::Display for SpecStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SpecStatus {
    type Err = ParseStateError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "queued" => Ok(SpecStatus::Queued),
            "running" => Ok(SpecStatus::Running),
            "completed" => Ok(SpecStatus::Completed),
            "failed" => Ok(SpecStatus::Failed),
            "canceled" => Ok(SpecStatus::Canceled),
            other => Err(ParseStateError {
                kind: "SpecStatus",
                got: other.to_owned(),
            }),
        }
    }
}

/// A string did not match any [`TaskState`] / [`SpecStatus`] variant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("'{got}' is not a valid {kind}")]
pub struct ParseStateError {
    /// The enum being parsed (`"TaskState"` or `"SpecStatus"`).
    kind: &'static str,
    /// The unrecognized input.
    got: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_state_as_str_is_expected_lowercase() {
        assert_eq!(TaskState::NotStarted.as_str(), "not_started");
        assert_eq!(TaskState::Active.as_str(), "active");
        assert_eq!(TaskState::Blocked.as_str(), "blocked");
        assert_eq!(TaskState::Passing.as_str(), "passing");
        assert_eq!(TaskState::Canceled.as_str(), "canceled");
    }

    #[test]
    fn spec_status_as_str_is_expected_lowercase() {
        assert_eq!(SpecStatus::Queued.as_str(), "queued");
        assert_eq!(SpecStatus::Running.as_str(), "running");
        assert_eq!(SpecStatus::Completed.as_str(), "completed");
        assert_eq!(SpecStatus::Failed.as_str(), "failed");
        assert_eq!(SpecStatus::Canceled.as_str(), "canceled");
    }

    #[test]
    fn fromstr_is_inverse_of_as_str() {
        for s in [
            TaskState::NotStarted,
            TaskState::Active,
            TaskState::Blocked,
            TaskState::Passing,
            TaskState::Canceled,
        ] {
            assert_eq!(s.as_str().parse::<TaskState>().unwrap(), s);
        }
        for s in [
            SpecStatus::Queued,
            SpecStatus::Running,
            SpecStatus::Completed,
            SpecStatus::Failed,
            SpecStatus::Canceled,
        ] {
            assert_eq!(s.as_str().parse::<SpecStatus>().unwrap(), s);
        }
        // Unknown strings are rejected, not silently mapped.
        assert!("nonsense".parse::<TaskState>().is_err());
        assert!("nonsense".parse::<SpecStatus>().is_err());
    }

    #[test]
    fn serde_roundtrips_both_enums() {
        // serde uses the same snake_case strings as as_str.
        let js = serde_json::to_string(&TaskState::NotStarted).unwrap();
        assert_eq!(js, "\"not_started\"");
        assert_eq!(
            serde_json::from_str::<TaskState>(&js).unwrap(),
            TaskState::NotStarted
        );

        let js = serde_json::to_string(&SpecStatus::Canceled).unwrap();
        assert_eq!(js, "\"canceled\"");
        assert_eq!(
            serde_json::from_str::<SpecStatus>(&js).unwrap(),
            SpecStatus::Canceled
        );
    }
}
