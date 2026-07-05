//! Decision records — the durable "why we did it this way" log.
//!
//! Workers EMIT decisions via the `decision_record(...)` MCP tool; they never
//! query them. The harness pushes ALL decisions for a spec into `PhaseContext`
//! at clock-in (Q8) — every decision is visible to every worker.
//!
//! ## The origin / phase_run_id mutex
//!
//! A decision's [`DecisionOrigin`] constrains its `phase_run_id`:
//!
//! - `Authored` → `phase_run_id` is `None` (no synthetic dispatch phase run).
//! - `Runtime` / `Human` → `phase_run_id` is `Some` (always has a parent run).
//!
//! The three constructors ([`DecisionRecord::new_authored`],
//! [`new_runtime`](DecisionRecord::new_runtime),
//! [`new_human`](DecisionRecord::new_human)) enforce this; a `DB CHECK`
//! constraint backstops it in Phase 3.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::ids::{DecisionId, PhaseRunId, SpecId};

/// Where a decision came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionOrigin {
    /// Authored in the spec — no parent phase run.
    Authored,
    /// Recorded by a worker at runtime — has a parent phase run.
    Runtime,
    /// Recorded by a human operator — has a parent phase run.
    Human,
}

/// An alternative that was considered and rejected, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedAlternative {
    /// Name of the rejected alternative.
    pub name: String,
    /// Why it was rejected.
    pub reason: String,
}

/// A single recorded decision.
///
/// Construct via [`DecisionRecord::new_authored`] / `new_runtime` / `new_human`
/// — direct struct literals are possible within the crate but bypass the
/// origin/phase_run_id mutex; prefer the constructors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionRecord {
    /// This decision's ID.
    pub id: DecisionId,
    /// The spec this decision belongs to (denormalized per C7).
    pub spec_id: SpecId,
    /// The phase run that produced this decision — `None` for `Authored`.
    pub phase_run_id: Option<PhaseRunId>,
    /// Where the decision came from.
    pub origin: DecisionOrigin,
    /// Short decision title.
    pub title: String,
    /// 1-3 sentence summary.
    pub summary: String,
    /// Why this choice over the alternatives.
    pub rationale: String,
    /// Alternatives that were considered and rejected.
    pub alternatives: Vec<RejectedAlternative>,
    /// A prior decision this one supersedes, if any.
    pub supersedes: Option<DecisionId>,
    /// When the decision was recorded.
    pub created_at: DateTime<Utc>,
}

/// A [`DecisionRecord`] constructor was called with arguments that violate the
/// origin / phase_run_id mutex.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecisionError {
    /// An `Authored` decision was given a `phase_run_id` — it must have none.
    #[error("authored decision must not have a phase_run_id")]
    AuthoredWithPhaseRun,
    /// A `Runtime`/`Human` decision was given no `phase_run_id` — it must
    /// have one.
    #[error("{origin:?} decision requires a phase_run_id")]
    NonAuthoredWithoutPhaseRun {
        /// The origin that was missing its phase run.
        origin: DecisionOrigin,
    },
}

impl DecisionRecord {
    /// Construct an `Authored` decision.
    ///
    /// `phase_run_id` MUST be `None`; passing `Some` is a
    /// [`DecisionError::AuthoredWithPhaseRun`].
    #[allow(clippy::too_many_arguments)]
    pub fn new_authored(
        id: DecisionId,
        spec_id: SpecId,
        phase_run_id: Option<PhaseRunId>,
        title: String,
        summary: String,
        rationale: String,
        alternatives: Vec<RejectedAlternative>,
        supersedes: Option<DecisionId>,
        created_at: DateTime<Utc>,
    ) -> Result<Self, DecisionError> {
        if phase_run_id.is_some() {
            return Err(DecisionError::AuthoredWithPhaseRun);
        }
        Ok(Self {
            id,
            spec_id,
            phase_run_id: None,
            origin: DecisionOrigin::Authored,
            title,
            summary,
            rationale,
            alternatives,
            supersedes,
            created_at,
        })
    }

    /// Construct a `Runtime` decision.
    ///
    /// `phase_run_id` MUST be `Some`; passing `None` is a
    /// [`DecisionError::NonAuthoredWithoutPhaseRun`].
    #[allow(clippy::too_many_arguments)]
    pub fn new_runtime(
        id: DecisionId,
        spec_id: SpecId,
        phase_run_id: Option<PhaseRunId>,
        title: String,
        summary: String,
        rationale: String,
        alternatives: Vec<RejectedAlternative>,
        supersedes: Option<DecisionId>,
        created_at: DateTime<Utc>,
    ) -> Result<Self, DecisionError> {
        Self::new_non_authored(
            DecisionOrigin::Runtime,
            id,
            spec_id,
            phase_run_id,
            title,
            summary,
            rationale,
            alternatives,
            supersedes,
            created_at,
        )
    }

    /// Construct a `Human` decision.
    ///
    /// `phase_run_id` MUST be `Some`; passing `None` is a
    /// [`DecisionError::NonAuthoredWithoutPhaseRun`].
    #[allow(clippy::too_many_arguments)]
    pub fn new_human(
        id: DecisionId,
        spec_id: SpecId,
        phase_run_id: Option<PhaseRunId>,
        title: String,
        summary: String,
        rationale: String,
        alternatives: Vec<RejectedAlternative>,
        supersedes: Option<DecisionId>,
        created_at: DateTime<Utc>,
    ) -> Result<Self, DecisionError> {
        Self::new_non_authored(
            DecisionOrigin::Human,
            id,
            spec_id,
            phase_run_id,
            title,
            summary,
            rationale,
            alternatives,
            supersedes,
            created_at,
        )
    }

    /// Shared body for the `Runtime` / `Human` constructors.
    #[allow(clippy::too_many_arguments)]
    fn new_non_authored(
        origin: DecisionOrigin,
        id: DecisionId,
        spec_id: SpecId,
        phase_run_id: Option<PhaseRunId>,
        title: String,
        summary: String,
        rationale: String,
        alternatives: Vec<RejectedAlternative>,
        supersedes: Option<DecisionId>,
        created_at: DateTime<Utc>,
    ) -> Result<Self, DecisionError> {
        if phase_run_id.is_none() {
            return Err(DecisionError::NonAuthoredWithoutPhaseRun { origin });
        }
        Ok(Self {
            id,
            spec_id,
            phase_run_id,
            origin,
            title,
            summary,
            rationale,
            alternatives,
            supersedes,
            created_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec_id() -> DecisionId {
        DecisionId::new("D0000001a").unwrap()
    }
    fn spec_id() -> SpecId {
        SpecId::new("S0000001a").unwrap()
    }
    fn phase_run() -> PhaseRunId {
        PhaseRunId::new("P0000001a").unwrap()
    }
    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    #[test]
    fn authored_constructor_rejects_some_phase_run() {
        let err = DecisionRecord::new_authored(
            dec_id(),
            spec_id(),
            Some(phase_run()),
            "t".into(),
            "s".into(),
            "r".into(),
            vec![],
            None,
            now(),
        )
        .unwrap_err();
        assert_eq!(err, DecisionError::AuthoredWithPhaseRun);
    }

    #[test]
    fn authored_constructor_accepts_none_phase_run() {
        let d = DecisionRecord::new_authored(
            dec_id(),
            spec_id(),
            None,
            "t".into(),
            "s".into(),
            "r".into(),
            vec![],
            None,
            now(),
        )
        .unwrap();
        assert_eq!(d.origin, DecisionOrigin::Authored);
        assert!(d.phase_run_id.is_none());
    }

    #[test]
    fn runtime_constructor_rejects_none_phase_run() {
        let err = DecisionRecord::new_runtime(
            dec_id(),
            spec_id(),
            None,
            "t".into(),
            "s".into(),
            "r".into(),
            vec![],
            None,
            now(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            DecisionError::NonAuthoredWithoutPhaseRun {
                origin: DecisionOrigin::Runtime,
            }
        );
    }

    #[test]
    fn human_constructor_rejects_none_phase_run() {
        let err = DecisionRecord::new_human(
            dec_id(),
            spec_id(),
            None,
            "t".into(),
            "s".into(),
            "r".into(),
            vec![],
            None,
            now(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            DecisionError::NonAuthoredWithoutPhaseRun {
                origin: DecisionOrigin::Human,
            }
        );
    }

    #[test]
    fn runtime_constructor_accepts_some_phase_run() {
        let d = DecisionRecord::new_runtime(
            dec_id(),
            spec_id(),
            Some(phase_run()),
            "t".into(),
            "s".into(),
            "r".into(),
            vec![RejectedAlternative {
                name: "alt".into(),
                reason: "slower".into(),
            }],
            None,
            now(),
        )
        .unwrap();
        assert_eq!(d.origin, DecisionOrigin::Runtime);
        assert_eq!(d.phase_run_id, Some(phase_run()));
    }

    #[test]
    fn decision_serde_roundtrip() {
        let original = DecisionRecord::new_human(
            dec_id(),
            spec_id(),
            Some(phase_run()),
            "Use sqlx".into(),
            "Chose sqlx for compile-checked queries.".into(),
            "Type safety outweighs the build-time cost.".into(),
            vec![RejectedAlternative {
                name: "diesel".into(),
                reason: "heavier macro surface".into(),
            }],
            Some(DecisionId::new("D0000000z").unwrap()),
            now(),
        )
        .unwrap();
        let json = serde_json::to_string(&original).unwrap();
        let back: DecisionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);
    }
}
