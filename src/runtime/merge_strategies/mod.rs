//! Merge-strategy registry — v1 foundational surface (Tp61q7q0x).
//!
//! The [`MergeStrategy`] trait is the registry contract that BOI's
//! `merge_spec` / `merge_to_integration` consults at the
//! `RebaseOutcome::Conflicts` arm, AFTER the 3.2.0 FF → rebase → retry-FF
//! ladder gives up.
//!
//! Strategies are tried in order; the first to return [`StrategyOutcome::Resolved`]
//! wins. If every strategy declines, the terminal `OperatorEscalationResolver`
//! writes a manifest and the spec is parked as
//! [`crate::types::BlockedReason::MergeConflict`] for `boi resolve-conflict`.
//!
//! ## Status: staged foundation — NOT yet wired
//!
//! This module is the salvaged foundation of the conflict-resolver track
//! (spec Syvwx7psx, 2026-06-06): the trait + companion types, plus the
//! [`non_overlapping`] strategy. The remaining v1 strategies
//! (IdenticalIntent, AppendOnly, structural mergers, Llm,
//! OperatorEscalation) and the wiring into `worktree.rs`'s
//! `RebaseOutcome::Conflicts` arms are still TODO on that track — nothing
//! in the merge pipeline consults this registry yet. The public surface is
//! pinned by `tests/red_merge_strategies_surface.rs`.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::types::ids::SpecId;

pub mod non_overlapping;

/// A single file the rebase reported as conflicted.
///
/// The strategy receives `ours`/`theirs`/`base` blob contents already
/// extracted by the caller; it does NOT touch git directly. This keeps the
/// trait pure-data and trivially unit-testable.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConflictedFile {
    /// Repo-relative path of the conflicted file.
    pub path: PathBuf,
    /// The "ours" side blob bytes (the side being rebased onto).
    pub ours: Vec<u8>,
    /// The "theirs" side blob bytes (the side being rebased).
    pub theirs: Vec<u8>,
    /// The merge-base blob bytes (`None` if the file was added on both sides).
    pub base: Option<Vec<u8>>,
}

/// Invocation context handed to every strategy on every conflict.
///
/// Carries the identifiers + sha pair the manifest writer needs, the
/// verification commands that the W2 LLM validator re-runs against the
/// merged tree, and the worktree path strategies must NOT mutate outside of
/// their `Resolved` byte payload.
#[derive(Debug, Clone)]
pub struct ConflictCtx {
    /// Spec being merged.
    pub spec_id: SpecId,
    /// The base commit sha (merge target).
    pub base_sha: String,
    /// The head commit sha (the side being merged).
    pub head_sha: String,
    /// Path to the worktree the rebase is running in.
    pub worktree_path: PathBuf,
    /// Verification commands authored by the spec — re-run by the W2
    /// LLM validator on the merged tree (both-parent test).
    pub verifications: Vec<String>,
}

/// What a strategy decided for a single conflicted file.
///
/// Per CRITIC §C-deterministic-toolbox the taxonomy is strictly three-way:
/// `Resolved` (own it), `Decline` (next strategy), `Error` (loud — the
/// caller logs and proceeds to the next strategy after recording the
/// failure).
#[derive(Debug, Clone)]
pub enum StrategyOutcome {
    /// The strategy resolved the conflict; the registry must write
    /// `bytes` to the file and `git add` it.
    Resolved {
        /// The merged file contents.
        bytes: Vec<u8>,
        /// Short note (≤80 chars) — the manifest records it.
        note: String,
    },
    /// The strategy does not apply to this file; try the next strategy.
    Decline {
        /// Typed reason for the decline (S6 loudness — never empty).
        reason: String,
    },
    /// The strategy threw — log it, manifest it, try the next strategy.
    Error {
        /// What went wrong.
        message: String,
    },
}

/// A merge-strategy — the v1 trait contract.
///
/// Strategies are `Send + Sync` so the registry can be stored in an
/// `Arc<Vec<Arc<dyn MergeStrategy>>>` and shared across the rayon-backed
/// per-file iteration the wiring task adds.
pub trait MergeStrategy: Send + Sync {
    /// Stable, lowercase, snake-case name — used in the manifest and in
    /// dashboard surfaces (`Resolved by strategy: <name>`).
    fn name(&self) -> &'static str;

    /// Try to resolve `file` given `ctx`. Must NOT mutate the worktree —
    /// the caller writes the bytes and `git add`s after `Resolved`.
    fn try_resolve(&self, ctx: &ConflictCtx, file: &ConflictedFile) -> StrategyOutcome;
}

/// Convenience alias for the registry shape carried on `StepCtx`.
pub type StrategyRegistry = Arc<Vec<Arc<dyn MergeStrategy>>>;

#[cfg(test)]
mod tests {
    use super::*;

    struct AlwaysDecline;

    impl MergeStrategy for AlwaysDecline {
        fn name(&self) -> &'static str {
            "always_decline"
        }
        fn try_resolve(&self, _ctx: &ConflictCtx, _file: &ConflictedFile) -> StrategyOutcome {
            StrategyOutcome::Decline {
                reason: "test".into(),
            }
        }
    }

    #[test]
    fn test_l1_merge_strategy_trait_is_object_safe() {
        let _reg: StrategyRegistry =
            Arc::new(vec![Arc::new(AlwaysDecline) as Arc<dyn MergeStrategy>]);
    }

    #[test]
    fn test_l1_conflicted_file_roundtrips_through_serde() {
        let f = ConflictedFile {
            path: PathBuf::from("src/a.rs"),
            ours: b"a".to_vec(),
            theirs: b"b".to_vec(),
            base: Some(b"c".to_vec()),
        };
        let json = serde_json::to_string(&f).unwrap();
        let back: ConflictedFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, f);
    }
}
