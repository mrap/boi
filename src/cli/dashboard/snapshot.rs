//! Non-TTY fallback: a one-shot static tree print.
//!
//! `boi dashboard` calls this when stdout is not a TTY (pipe / CI). It is the
//! scripting path the removed `boi status` used to serve.

use chrono::Utc;

use crate::cli::dashboard::model::{DashNode, NodeKind};
use crate::cli::dashboard::picker::SpecSummary;
use crate::cli::dashboard::poll;
use crate::cli::dashboard::render::{fmt_ms, task_label};
use crate::cli::read_error::ReadError;
use crate::types::ids::SpecId;

/// Print a static snapshot of `spec_id`, or the spec list when no id given.
pub(super) async fn run(spec_id: Option<&str>) -> Result<(), ReadError> {
    let Some(raw) = spec_id else {
        // No SPEC_ID — open the pool and print a plain-text spec list.
        let pool = poll::open_pool().await?;
        let specs = poll::build_spec_list(&pool).await?;
        print!("{}", render_spec_list(&specs));
        return Ok(());
    };
    let spec_id = SpecId::new(raw).map_err(|_| ReadError::BadId(raw.to_string()))?;

    let pool = poll::open_pool().await?;
    let trace_path = poll::trace_path_for(&spec_id)?;
    let tree = poll::build_snapshot(
        &pool,
        &spec_id,
        &trace_path,
        crate::cli::dashboard::model::SortMode::Duration,
    )
    .await?;
    // The spec's human-readable title lives in `spec_versions.snapshot.title`,
    // NOT on the `DashNode` tree (which is keyed off `phase_runs`). Fetch it
    // separately so the non-interactive snapshot can surface it next to the
    // spec ID — matching what the TUI dashboard's header row does.
    let title = poll::fetch_spec_title(&pool, &spec_id).await;

    print!("{}", render_text_with_title(&tree, title.as_deref()));
    Ok(())
}

/// Render the spec list as plain text — one line per spec.
///
/// Format: `spec_id  [status]  N phases`. (Per the 2026-06-01 strip-$
/// directive the per-spec dollar column is gone — phase count survives as
/// the spend-hint signal.)
pub fn render_spec_list(specs: &[SpecSummary]) -> String {
    let mut out = String::new();
    for s in specs {
        out.push_str(&format!(
            "{}  [{}]  {} phases\n",
            s.spec_id, s.status, s.phase_count,
        ));
    }
    out
}

/// Render a tree as indented plain text — pure, unit-tested.
///
/// Equivalent to [`render_text_with_title`] with no title. Kept as the
/// title-less ergonomic entry point for callers that don't carry the spec
/// title alongside the tree.
pub fn render_text(tree: &DashNode) -> String {
    render_text_with_title(tree, None)
}

/// Render a tree as indented plain text, prefixing the spec root's label with
/// its human-readable `title` (when present) — pure, unit-tested.
///
/// Matches the TUI dashboard's header row: `<spec_id>  <title>` for the spec
/// root, and `<task_id>  <ref-or-behavior>` for every `Task` row. Per the
/// spec contract, IDs always stay left-aligned and the label augments — never
/// replaces — them so they remain the correlation key with the DB and other
/// tools.
pub fn render_text_with_title(tree: &DashNode, title: Option<&str>) -> String {
    let mut out = String::new();
    write_node(&mut out, tree, 0, title);
    out
}

fn write_node(out: &mut String, node: &DashNode, depth: usize, root_title: Option<&str>) {
    let now = Utc::now();
    out.push_str(&"  ".repeat(depth));

    // Build the label portion: ID, then optional human-readable label inline.
    let mut label = node.label.clone();
    if depth == 0 && node.kind == NodeKind::Spec {
        if let Some(t) = root_title.filter(|t| !t.is_empty()) {
            label.push_str(&format!("  {t}"));
        }
    } else if node.kind == NodeKind::Task {
        let task_lbl = task_label(
            node.task_ref.as_deref(),
            node.behavior.as_deref().unwrap_or(""),
        );
        if !task_lbl.is_empty() {
            label.push_str(&format!("  {task_lbl}"));
        }
    }

    out.push_str(&format!(
        "{} [{}] {}\n",
        label,
        node.status,
        fmt_ms(node.duration_ms(now)),
    ));
    for child in &node.children {
        write_node(out, child, depth + 1, root_title);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::dashboard::model::build_tree;
    use crate::cli::dashboard::picker::SpecSummary;

    #[test]
    fn render_text_indents_each_level() {
        let mut tree = build_tree("S0042a", &[]);
        tree.status = "running".into();
        let text = render_text(&tree);
        assert!(text.starts_with("S0042a [running]"));
    }

    /// `render_text_with_title` appends the spec's human-readable title to the
    /// root row, leaving the ID first (the correlation key with the DB).
    /// Mirrors the TUI dashboard header's `<id>  <title>` ordering.
    #[test]
    fn render_text_with_title_appends_title_to_the_spec_root() {
        let mut tree = build_tree("S0042a", &[]);
        tree.status = "running".into();
        let text = render_text_with_title(&tree, Some("my-spec-title"));
        assert!(
            text.starts_with("S0042a  my-spec-title [running]"),
            "title must follow the ID with a 2-space gap; got: {text:?}",
        );
    }

    /// A `blocked` spec status surfaces verbatim in the non-TTY snapshot, so
    /// scripts and `boi dashboard | cat` see a wedged spec as `[blocked]`
    /// rather than `[running]`/`[done]`.
    #[test]
    fn render_text_surfaces_blocked_status() {
        let mut tree = build_tree("S0042a", &[]);
        tree.status = "blocked".into();
        let text = render_text(&tree);
        assert!(
            text.contains("[blocked]"),
            "blocked status must render in the snapshot; got: {text:?}",
        );
    }

    /// An absent title leaves the root row unchanged — `render_text` and
    /// `render_text_with_title(_, None)` produce the same bytes.
    #[test]
    fn render_text_with_title_none_matches_render_text() {
        let mut tree = build_tree("S0042a", &[]);
        tree.status = "running".into();
        assert_eq!(render_text(&tree), render_text_with_title(&tree, None));
    }

    #[test]
    fn render_spec_list_formats_each_spec_as_one_line() {
        let specs = vec![
            SpecSummary {
                spec_id: "S0000001a".into(),
                title: None,
                status: "running".into(),
                started_at: None,
                completed_at: None,
                phase_count: 3,
            },
            SpecSummary {
                spec_id: "S0000002b".into(),
                title: None,
                status: "completed".into(),
                started_at: None,
                completed_at: None,
                phase_count: 5,
            },
        ];
        let text = render_spec_list(&specs);
        assert!(
            text.contains("S0000001a  [running]  3 phases"),
            "running spec line: {text}",
        );
        assert!(
            text.contains("S0000002b  [completed]  5 phases"),
            "completed spec line: {text}",
        );
        // Per the 2026-06-01 strip-$ directive no `$` appears in output.
        assert!(
            !text.contains('$'),
            "no `$` should appear in the snapshot list, got: {text}",
        );
    }
}
