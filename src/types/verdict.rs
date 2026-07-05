//! Worker verdict types — the structured output a worker phase emits.
//!
//! A worker phase emits a [`WorkerVerdict`]: a mandatory `synopsis` plus a
//! [`VerdictOutcome`] (one of `passing` / `redo` / `blocked` / `fail`). Phase
//! TOMLs route the next phase by matching on the outcome's `type`.
//!
//! ## The `deny_unknown_fields` honesty guarantee (§4)
//!
//! `deny_unknown_fields` on [`WorkerVerdict`] applies ONLY to the *inner worker
//! structured-output payload*, NOT the Goose `stream-json` wrapper (Batch A
//! review — L1). Phase 7's stream parser MUST: (1) parse the `GooseStreamEvent`
//! envelope loosely, (2) extract the worker payload, (3) deserialize *that*
//! strictly into `WorkerVerdict`. Putting `deny_unknown_fields` on the Goose
//! wrapper would reject provider-specific fields and force Phase 7 to strip the
//! lint — losing the guarantee. The 2-retry contract on parse failure lives in
//! Phase 7, not here.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::types::reasons::ErrorWhyFix;

/// A worker's structured verdict for one phase run.
///
/// `synopsis` is a mandatory 1-3 sentence summary (Q3 requirement); the
/// `outcome` carries the routing decision and any payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerVerdict {
    /// Mandatory 1-3 sentence summary of what the phase did.
    pub synopsis: String,
    /// The verdict outcome — drives phase-TOML routing.
    pub outcome: VerdictOutcome,
}

/// The possible outcomes of a worker phase.
///
/// Tagged-union serde (`{"type": "...", ...}`) with `deny_unknown_fields`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum VerdictOutcome {
    /// The phase succeeded; carries the evidence.
    Passing {
        /// Evidence the phase actually did the work.
        evidence: Evidence,
    },
    /// The phase should be retried (bounded by an iteration cap).
    Redo {
        /// Why a retry is needed.
        reason: String,
    },
    /// The phase is blocked and needs intervention.
    Blocked {
        /// Why the phase is blocked.
        reason: String,
        /// Optional error detail.
        error_why_fix: Option<ErrorWhyFix>,
    },
    /// The phase failed; routes to the adjustment side-chain.
    Fail {
        /// What went wrong.
        error: String,
        /// Why it went wrong.
        why: String,
        /// How to fix it.
        fix: String,
    },
    /// The phase was canceled before completion (written by the cancel path,
    /// never by a worker). Only appears in `phase_runs.verdict` for rows closed
    /// by spec/task cancellation — never arrives via routing.
    Canceled,
}

/// Evidence that a phase actually performed its work.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Evidence {
    /// Files the phase modified (from `git diff`).
    pub files_touched: Vec<PathBuf>,
    /// Verification commands the phase ran, with their results.
    pub verifications: Vec<VerificationEvidence>,
    /// Free-text summary of the evidence.
    pub summary: String,
    /// The merge commit SHA produced by a deterministic `merge` / `merge_to_
    /// integration` step's fast-forward (G25.2). `None` for every other phase
    /// — a worker phase never sets it (and never needs to emit it: `#[serde(
    /// default)]` makes it absent-tolerant). The `merge` phase run's evidence
    /// records the merged commit SHA here, which was always `null` before
    /// `ff_merge` started returning the merged `Oid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_commit_sha: Option<String>,
}

/// The result of running one verification command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationEvidence {
    /// Optional human-readable name for the verification.
    pub name: Option<String>,
    /// The command that was run.
    pub command: String,
    /// The command's exit code.
    pub exit_code: i32,
    /// The verification tier (Lec 10).
    pub level: VerifyLevel,
}

/// Verification tier — `L1` unit, `L2` integration, `L3` end-to-end (Lec 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerifyLevel {
    /// Level 1 — unit / fast checks.
    L1,
    /// Level 2 — integration checks.
    L2,
    /// Level 3 — end-to-end checks.
    L3,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_outcome_from_fixture_json() {
        // passing
        let v: WorkerVerdict = serde_json::from_str(
            r#"{
                "synopsis": "Added the middleware module.",
                "outcome": {
                    "type": "passing",
                    "evidence": {
                        "files_touched": ["src/mw.rs"],
                        "verifications": [
                            {"name":"unit","command":"cargo test","exit_code":0,"level":"l1"}
                        ],
                        "summary": "all green"
                    }
                }
            }"#,
        )
        .unwrap();
        assert!(matches!(v.outcome, VerdictOutcome::Passing { .. }));

        // redo
        let v: WorkerVerdict = serde_json::from_str(
            r#"{"synopsis":"retry","outcome":{"type":"redo","reason":"flaky test"}}"#,
        )
        .unwrap();
        assert!(matches!(v.outcome, VerdictOutcome::Redo { .. }));

        // blocked — error_why_fix omitted is fine (Option)
        let v: WorkerVerdict = serde_json::from_str(
            r#"{"synopsis":"stuck","outcome":{"type":"blocked","reason":"need creds"}}"#,
        )
        .unwrap();
        assert!(matches!(
            v.outcome,
            VerdictOutcome::Blocked {
                error_why_fix: None,
                ..
            }
        ));

        // fail
        let v: WorkerVerdict = serde_json::from_str(
            r#"{"synopsis":"broke","outcome":{"type":"fail","error":"e","why":"w","fix":"f"}}"#,
        )
        .unwrap();
        assert!(matches!(v.outcome, VerdictOutcome::Fail { .. }));
    }

    #[test]
    fn deny_unknown_fields_rejects_extra_keys() {
        // Extra key on the WorkerVerdict envelope.
        assert!(
            serde_json::from_str::<WorkerVerdict>(
                r#"{"synopsis":"x","outcome":{"type":"redo","reason":"r"},"bogus":1}"#,
            )
            .is_err()
        );
        // Extra key inside a VerdictOutcome variant.
        assert!(
            serde_json::from_str::<WorkerVerdict>(
                r#"{"synopsis":"x","outcome":{"type":"redo","reason":"r","extra":true}}"#,
            )
            .is_err()
        );
    }

    #[test]
    fn missing_synopsis_rejected() {
        assert!(
            serde_json::from_str::<WorkerVerdict>(r#"{"outcome":{"type":"redo","reason":"r"}}"#)
                .is_err()
        );
    }

    #[test]
    fn fail_without_all_three_fields_rejected() {
        // Missing `fix`.
        assert!(
            serde_json::from_str::<WorkerVerdict>(
                r#"{"synopsis":"x","outcome":{"type":"fail","error":"e","why":"w"}}"#,
            )
            .is_err()
        );
        // Missing `why` and `fix`.
        assert!(
            serde_json::from_str::<WorkerVerdict>(
                r#"{"synopsis":"x","outcome":{"type":"fail","error":"e"}}"#,
            )
            .is_err()
        );
    }

    #[test]
    fn verdict_serde_roundtrip() {
        let original = WorkerVerdict {
            synopsis: "did the thing".into(),
            outcome: VerdictOutcome::Passing {
                evidence: Evidence {
                    files_touched: vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")],
                    verifications: vec![VerificationEvidence {
                        name: None,
                        command: "cargo test".into(),
                        exit_code: 0,
                        level: VerifyLevel::L2,
                    }],
                    summary: "ok".into(),
                    merge_commit_sha: None,
                },
            },
        };
        let json = serde_json::to_string(&original).unwrap();
        let back: WorkerVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(back.synopsis, original.synopsis);
        // `let else` keeps the test free of `panic!` (clippy.toml exempts
        // unwrap/expect in tests but not the `panic` lint).
        let VerdictOutcome::Passing { evidence } = back.outcome else {
            unreachable!("roundtripped a Passing verdict, got a non-Passing outcome");
        };
        assert_eq!(evidence.files_touched.len(), 2);
        assert_eq!(evidence.verifications[0].level, VerifyLevel::L2);
    }

    #[test]
    fn evidence_default_is_empty() {
        let e = Evidence::default();
        assert!(e.files_touched.is_empty());
        assert!(e.verifications.is_empty());
        assert!(e.summary.is_empty());
    }
}
