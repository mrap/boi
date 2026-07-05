//! `NonOverlappingHunkMerger` — v1 strategy #2 (task Tr0zmezdt).
//!
//! The v1-minimum splicing algorithm is implemented (equal line counts
//! across base/ours/theirs; disjoint per-line edits splice, true overlaps
//! decline). Pure insertions/deletions are deferred to the conflict-resolver
//! track's structural merger.

use crate::runtime::merge_strategies::{
    ConflictCtx, ConflictedFile, MergeStrategy, StrategyOutcome,
};

/// Non-overlapping hunk merger.
///
/// Resolves a conflicted file when both sides' edits live in disjoint line
/// ranges, by splicing both edits into the merged tree. When the hunks
/// genuinely overlap it returns [`StrategyOutcome::Decline`].
#[derive(Debug, Default, Clone, Copy)]
pub struct NonOverlappingHunkMerger;

impl MergeStrategy for NonOverlappingHunkMerger {
    fn name(&self) -> &'static str {
        "non_overlapping_hunk"
    }

    fn try_resolve(&self, _ctx: &ConflictCtx, file: &ConflictedFile) -> StrategyOutcome {
        let Some(base) = file.base.as_deref() else {
            return StrategyOutcome::Decline {
                reason: "non_overlapping_hunk_no_merge_base".into(),
            };
        };

        let base_lines = split_lines(base);
        let ours_lines = split_lines(&file.ours);
        let theirs_lines = split_lines(&file.theirs);

        // v1 minimum: require equal line counts across all three sides. Pure
        // insertions/deletions are out of scope for this strategy — a future
        // structural merger will handle them.
        if base_lines.len() != ours_lines.len() || base_lines.len() != theirs_lines.len() {
            return StrategyOutcome::Decline {
                reason: "non_overlapping_hunk_line_count_mismatch".into(),
            };
        }

        let mut out: Vec<&[u8]> = Vec::with_capacity(base_lines.len());
        let mut ours_touched = 0usize;
        let mut theirs_touched = 0usize;
        for i in 0..base_lines.len() {
            let b = base_lines[i];
            let o = ours_lines[i];
            let t = theirs_lines[i];
            let o_diff = o != b;
            let t_diff = t != b;
            if o_diff && t_diff {
                if o == t {
                    // Both sides made the same edit — not a real overlap.
                    out.push(o);
                    ours_touched += 1;
                    theirs_touched += 1;
                } else {
                    return StrategyOutcome::Decline {
                        reason: format!("non_overlapping_hunk_true_overlap_at_line_{}", i + 1),
                    };
                }
            } else if o_diff {
                out.push(o);
                ours_touched += 1;
            } else if t_diff {
                out.push(t);
                theirs_touched += 1;
            } else {
                out.push(b);
            }
        }

        if ours_touched == 0 && theirs_touched == 0 {
            return StrategyOutcome::Decline {
                reason: "non_overlapping_hunk_no_changes_vs_base".into(),
            };
        }

        let mut bytes = Vec::with_capacity(file.ours.len().max(file.theirs.len()));
        for line in out {
            bytes.extend_from_slice(line);
        }

        StrategyOutcome::Resolved {
            bytes,
            note: format!("spliced disjoint hunks (ours={ours_touched}, theirs={theirs_touched})"),
        }
    }
}

/// Split `bytes` into line slices, preserving each line's trailing `\n`.
///
/// The trailing newline (if any) stays attached so concatenating the slices
/// reproduces the original byte stream verbatim.
fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            lines.push(&bytes[start..=i]);
            start = i + 1;
        }
    }
    if start < bytes.len() {
        lines.push(&bytes[start..]);
    }
    lines
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::types::ids::SpecId;

    fn ctx() -> ConflictCtx {
        ConflictCtx {
            spec_id: SpecId::new("Sxk3m9p2q").unwrap(),
            base_sha: "base".into(),
            head_sha: "head".into(),
            worktree_path: PathBuf::from("/tmp"),
            verifications: vec![],
        }
    }

    /// Base has 6 lines. Ours edits the top (line 1). Theirs edits the
    /// bottom (line 6). Hunks are disjoint → splice both → Resolved.
    #[test]
    fn test_l2_non_overlapping_clean_splice_resolves() {
        let base = b"a\nb\nc\nd\ne\nf\n".to_vec();
        let ours = b"A\nb\nc\nd\ne\nf\n".to_vec(); // line 1: a -> A
        let theirs = b"a\nb\nc\nd\ne\nF\n".to_vec(); // line 6: f -> F
        let expected = b"A\nb\nc\nd\ne\nF\n".to_vec();

        let file = ConflictedFile {
            path: PathBuf::from("src/x.rs"),
            ours,
            theirs,
            base: Some(base),
        };
        let outcome = NonOverlappingHunkMerger.try_resolve(&ctx(), &file);
        match outcome {
            StrategyOutcome::Resolved { bytes, .. } => assert_eq!(bytes, expected),
            other => panic!("expected Resolved with spliced bytes, got {:?}", other),
        }
    }

    /// Both sides rewrite the SAME line — true overlap. Must Decline.
    #[test]
    fn test_l2_non_overlapping_true_overlap_declines() {
        let base = b"a\nb\nc\n".to_vec();
        let ours = b"a\nX\nc\n".to_vec(); // line 2: b -> X
        let theirs = b"a\nY\nc\n".to_vec(); // line 2: b -> Y
        let file = ConflictedFile {
            path: PathBuf::from("src/x.rs"),
            ours,
            theirs,
            base: Some(base),
        };
        let outcome = NonOverlappingHunkMerger.try_resolve(&ctx(), &file);
        assert!(
            matches!(outcome, StrategyOutcome::Decline { .. }),
            "expected Decline on true overlap, got {:?}",
            outcome
        );
    }

    /// First and last lines edited by opposite sides — disjoint boundary
    /// edits must still splice cleanly.
    #[test]
    fn test_l2_non_overlapping_single_line_boundary_edits_resolve() {
        let base = b"first\nmid\nlast\n".to_vec();
        let ours = b"FIRST\nmid\nlast\n".to_vec();
        let theirs = b"first\nmid\nLAST\n".to_vec();
        let expected = b"FIRST\nmid\nLAST\n".to_vec();
        let file = ConflictedFile {
            path: PathBuf::from("src/x.rs"),
            ours,
            theirs,
            base: Some(base),
        };
        let outcome = NonOverlappingHunkMerger.try_resolve(&ctx(), &file);
        match outcome {
            StrategyOutcome::Resolved { bytes, .. } => assert_eq!(bytes, expected),
            other => panic!("expected Resolved at file boundaries, got {:?}", other),
        }
    }
}
