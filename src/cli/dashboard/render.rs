//! `ratatui` rendering for the dashboard.
//!
//! This module is split into a pure layout half (the bar string builder,
//! unit-tested below) and a `ratatui`-widget half (Task 8/9).

use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::cli::dashboard::model::{DashNode, NodeKind, SortMode, TimeSplit};
use crate::cli::dashboard::picker::SpecSummary;
use crate::cli::dashboard::state::{PickerScreen, SpecScreen};

/// The style for a status cell in the drill-in tree.
///
/// `blocked` is drawn LOUD — bold yellow — so a wedged task never blends into
/// the running rows (the 2026-06-11 incident where a blocked spec rendered
/// indistinguishable from a running one). Every other status inherits the
/// row's base style unchanged.
fn status_cell_style(base: Style, status: &str) -> Style {
    if status == "blocked" {
        base.fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        base
    }
}

/// Build a fixed-`width` bar string for a node.
///
/// `node_ms` is the node's duration; `max_ms` is the widest sibling's
/// duration (the full-width reference). The filled cells are split think
/// (`█`) / do (`▓`) / idle (`▒`) in proportion to `split`; the remainder is
/// `░`. A `max_ms` of 0 yields an all-empty bar.
pub fn bar(node_ms: u64, max_ms: u64, split: TimeSplit, width: usize) -> String {
    if max_ms == 0 || width == 0 {
        return "░".repeat(width);
    }
    let filled = ((node_ms as f64 / max_ms as f64) * width as f64).round() as usize;
    let filled = filled.min(width);

    let accounted = split.total_ms().max(1);
    let think = (filled as f64 * split.think_ms as f64 / accounted as f64).round() as usize;
    let do_ = (filled as f64 * split.do_ms as f64 / accounted as f64).round() as usize;
    let think = think.min(filled);
    let do_ = do_.min(filled - think);
    let idle = filled - think - do_;
    let empty = width - filled;

    let mut s = String::with_capacity(width);
    s.extend(std::iter::repeat_n('█', think));
    s.extend(std::iter::repeat_n('▓', do_));
    s.extend(std::iter::repeat_n('▒', idle));
    s.extend(std::iter::repeat_n('░', empty));
    s
}

/// Format a token count compactly: `4.8k` / `950` / `—` (zero → `—`).
pub fn fmt_tokens(tokens: u64) -> String {
    if tokens == 0 {
        "—".to_string()
    } else if tokens >= 1_000 {
        #[allow(clippy::cast_precision_loss)] // token counts fit comfortably in f64 mantissa
        let k = tokens as f64 / 1_000.0;
        format!("{k:.1}k")
    } else {
        tokens.to_string()
    }
}

/// Format a millisecond duration as `14m22s` / `4.2s` / `—`.
pub fn fmt_ms(ms: u64) -> String {
    if ms == 0 {
        return "—".to_string();
    }
    let secs = ms / 1000;
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}.{}s", secs, (ms % 1000) / 100)
    }
}

/// The breadcrumb line, e.g. `S0042a › T2 implement-api › P5 implement`.
pub fn breadcrumb(spec: &SpecScreen) -> String {
    let mut parts = vec![spec.tree.label.clone()];
    let mut node = &spec.tree;
    for &idx in &spec.drill {
        node = &node.children[idx];
        parts.push(node.label.clone());
    }
    parts.join(" › ")
}

/// Render the bar-tree pane: the focused node's children, one row each,
/// sorted, with the selected row highlighted.
///
/// The header row shows the spec's title (when known) next to the spec ID,
/// styled dim so the eye still finds the ID quickly when correlating with
/// the DB or other tools. Each task child row shows its `ref`/behavior label
/// next to the task ID with the same dim styling.
pub fn draw_tree(
    frame: &mut Frame,
    area: Rect,
    spec: &SpecScreen,
    sort: SortMode,
    last_poll_error: Option<&str>,
    now: DateTime<Utc>,
) {
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(area);

    let sort_label = match sort {
        SortMode::Waterfall => "waterfall",
        SortMode::Duration => "duration",
    };
    let mut header_spans: Vec<Span<'static>> = Vec::new();
    header_spans.push(Span::raw(" boi  "));
    header_spans.push(Span::raw(spec.tree.label.clone()));
    // Title: shown immediately next to the spec ID in dim style.
    if let Some(title) = spec.title.as_deref().filter(|t| !t.is_empty()) {
        header_spans.push(Span::styled(
            format!("  {title}"),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    // Drill path (everything past the root): keeps the breadcrumb context.
    let path_after_root = drill_path_labels(spec);
    if !path_after_root.is_empty() {
        header_spans.push(Span::raw(format!(" › {}", path_after_root.join(" › "))));
    }
    header_spans.push(Span::raw(format!("    [{sort_label}]")));
    if let Some(msg) = last_poll_error {
        header_spans.push(Span::styled(
            format!(" ⚠ {msg}"),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(header_spans)), chunks[0]);

    let focused = spec.focused();
    let max_ms = focused
        .children
        .iter()
        .map(|c| c.duration_ms(now))
        .max()
        .unwrap_or(0);
    let bar_width = 20;
    // leading_space(1) + gap(1) + status(8) + space(1) + bar + "  " + dur(7) + "  " + cost(7)
    let row_fixed = 1 + 1 + 8 + 1 + bar_width + 2 + 7 + 2 + 7;
    let label_w = (area.width as usize).saturating_sub(row_fixed).max(8);

    let lines: Vec<Line> = focused
        .children
        .iter()
        .enumerate()
        .map(|(i, child)| {
            let dur = child.duration_ms(now);
            let style = if i == spec.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let dim = if i == spec.selected {
                style // selected row stays reversed end-to-end
            } else {
                Style::default().add_modifier(Modifier::DIM)
            };
            let (id_text, label_text) = id_and_label(child, label_w);

            // Right-half fixed columns: status, bar, duration, tokens.
            // (Per the 2026-06-01 strip-$ directive the cost column is
            // gone — total-token count rides instead as the spend-hint
            // signal.) The status cell is a separate span so a `blocked` node
            // can be drawn LOUD — a wedged task must not blend into the
            // running rows (2026-06-11).
            let status_cell = format!(" {:<8}", child.status);
            let tail = format!(
                " {}  {:>7}  {:>7}",
                bar(dur, max_ms, child.split, bar_width),
                fmt_ms(dur),
                fmt_tokens(child.cost.total_tokens()),
            );

            // Pad the (id + 2-char gap + label) block out to label_w so the
            // status / bar / duration / tokens columns stay aligned across rows
            // — and so the row spans the full terminal width.
            let id_vis = id_text.chars().count();
            let label_block_vis = if label_text.is_empty() {
                0
            } else {
                2 + label_text.chars().count()
            };
            let pad = label_w.saturating_sub(id_vis + label_block_vis);

            let mut spans: Vec<Span<'static>> = Vec::new();
            spans.push(Span::styled(" ", style));
            spans.push(Span::styled(id_text, style));
            if !label_text.is_empty() {
                spans.push(Span::styled(format!("  {label_text}"), dim));
            }
            spans.push(Span::styled(" ".repeat(pad), style));
            spans.push(Span::styled(
                status_cell,
                status_cell_style(style, &child.status),
            ));
            spans.push(Span::styled(tail, style));
            Line::from(spans)
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::NONE)),
        chunks[1],
    );
}

/// Build the breadcrumb path labels for nodes past the spec root.
fn drill_path_labels(spec: &SpecScreen) -> Vec<String> {
    let mut parts = Vec::new();
    let mut node = &spec.tree;
    for &idx in &spec.drill {
        node = &node.children[idx];
        parts.push(node.label.clone());
    }
    parts
}

/// Width in characters reserved for the ID column in the task table. The
/// renderer always preserves the full ID (truncating with `…` only if the
/// terminal is narrower than this) so the column stays correlation-friendly.
const TREE_ID_COL_WIDTH: usize = 14;

/// Compute the `<id>` and `<label>` text shown next to each other in the
/// task-table label column.
///
/// - `id_text` is the node's `label` (the task ID for `Task` nodes),
///   truncated to fit [`TREE_ID_COL_WIDTH`] or the available `label_w` —
///   whichever is smaller. The ID always stays left-aligned and never wraps.
/// - `label_text` is the human-readable label (from `task_ref`/`behavior` via
///   [`task_label`]), truncated to fill whatever width is left. Empty when
///   no ref/behavior is known so the renderer can skip the gap entirely.
fn id_and_label(child: &DashNode, label_w: usize) -> (String, String) {
    let id_w = TREE_ID_COL_WIDTH.min(label_w);
    let id_text = truncate_with_ellipsis(&child.label, id_w);
    let id_vis = id_text.chars().count();

    // Only Task nodes carry a separate human label. Other kinds — phases,
    // leaves, the spec root — leave the label column to just the ID.
    if child.kind != NodeKind::Task {
        return (id_text, String::new());
    }
    let raw_label = task_label(
        child.task_ref.as_deref(),
        child.behavior.as_deref().unwrap_or(""),
    );
    if raw_label.is_empty() {
        return (id_text, String::new());
    }
    // Two-char gap between ID and label.
    let remaining = label_w.saturating_sub(id_vis + 2);
    let label_text = truncate_with_ellipsis(&raw_label, remaining);
    (id_text, label_text)
}

/// Render the leaf streaming log for a focused `Phase` node: one line per
/// event child, expanded lines show detail, the active line gets a spinner.
pub fn draw_log(
    frame: &mut Frame,
    area: Rect,
    spec: &SpecScreen,
    last_poll_error: Option<&str>,
    now: DateTime<Utc>,
) {
    let focused = spec.focused();
    let mut lines: Vec<Line> = Vec::new();

    // Header: breadcrumb + optional poll error indicator.
    let mut header_spans = vec![Span::raw(format!(" boi  {}", breadcrumb(spec)))];
    if let Some(msg) = last_poll_error {
        header_spans.push(Span::styled(
            format!(" ⚠ {msg}"),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::from(header_spans));

    for (i, ev) in focused.children.iter().enumerate() {
        let marker = if i == spec.selected { "▾" } else { "▸" };
        let glyph = if ev.completed_at.is_none() {
            "⠿"
        } else {
            " "
        };
        let row = format!(
            "{glyph}{marker} {:<6} {:<28} {:>8}",
            kind_word(ev.kind),
            truncate(&ev.label, 28),
            fmt_ms(ev.duration_ms(now)),
        );
        let style = if i == spec.selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(row, style)));
        if spec.expanded.contains(&i) {
            for d in event_detail(ev).lines() {
                lines.push(Line::from(format!("       {d}")));
            }
        }
    }

    // Footer: the phase's think/do split, plus total tokens. (Per the
    // 2026-06-01 strip-$ directive the dollar column is gone — tokens stay.)
    let s = focused.split;
    let total = s.total_ms().max(1);
    lines.push(Line::from(format!(
        " phase {}  ·  think {} ({}%)  ·  tools {} ({}%)  ·  {} tok",
        fmt_ms(focused.duration_ms(now)),
        fmt_ms(s.think_ms),
        s.think_ms * 100 / total,
        fmt_ms(s.do_ms),
        s.do_ms * 100 / total,
        fmt_tokens(focused.cost.total_tokens()),
    )));

    frame.render_widget(Paragraph::new(lines), area);
}

// ─── Overview (v1-style RUNNING / QUEUED / FINISHED) ─────────────────────────

/// ID column width in the overview (matches the existing picker layout).
const OVERVIEW_ID_W: usize = 11;

/// Prefix visible width: cursor(1) + space(1) + id_field + 2 spaces.
const OVERVIEW_PREFIX_VIS: usize = 1 + 1 + OVERVIEW_ID_W + 2;

/// Format elapsed milliseconds as a compact "time ago" string.
fn time_ago(elapsed_ms: u64) -> String {
    let secs = elapsed_ms / 1000;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Status icon and color for a finished spec.
fn finished_icon(status: &str) -> (&'static str, Color) {
    match status {
        "completed" => ("✓", Color::Green),
        "failed" => ("✗", Color::Red),
        _ => ("⊘", Color::DarkGray),
    }
}

/// Build a `Line` for a running spec in the overview.
fn overview_running_row(
    s: &SpecSummary,
    selected: bool,
    width: usize,
    now: DateTime<Utc>,
) -> Line<'static> {
    let cursor = if selected { "▶" } else { "▸" };
    let elapsed = s.started_at.map_or_else(
        || "—".to_string(),
        |t| fmt_ms((now - t).num_milliseconds().max(0) as u64),
    );
    // Per the 2026-06-01 strip-$ directive the per-spec dollar total is
    // gone from the picker — only the phase count and elapsed time ride.
    let right = format!("{}ph  {}", s.phase_count, elapsed);
    let right_vis = right.chars().count();
    let (title_text, title_vis) = overview_title_segment(s, width, right_vis);
    let pad = width.saturating_sub(OVERVIEW_PREFIX_VIS + title_vis + right_vis);

    let id_field = format!(
        " {:<w$}  ",
        truncate(&s.spec_id, OVERVIEW_ID_W),
        w = OVERVIEW_ID_W
    );

    let (glyph_style, base_style) = if selected {
        let rev = Style::default().add_modifier(Modifier::REVERSED);
        (rev, rev)
    } else {
        (Style::default().fg(Color::Yellow), Style::default())
    };
    let title_style = if selected {
        base_style
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };

    Line::from(vec![
        Span::styled(cursor.to_string(), glyph_style),
        Span::styled(id_field, glyph_style),
        Span::styled(title_text, title_style),
        Span::styled(" ".repeat(pad), base_style),
        Span::styled(right, base_style),
    ])
}

/// Build the `<title>` segment inserted after the ID column in overview rows.
///
/// Returns `(text, visual_width)`. The visual width is what the renderer
/// must subtract from the right-edge padding so the right column stays
/// right-aligned. Empty title ⇒ empty text, zero width — the layout is
/// identical to the pre-title rendering.
fn overview_title_segment(
    s: &SpecSummary,
    total_width: usize,
    right_vis: usize,
) -> (String, usize) {
    let Some(title) = s.title.as_deref().filter(|t| !t.is_empty()) else {
        return (String::new(), 0);
    };
    // Reserve at least 2 spaces of breathing room between the title and the
    // right column. Truncate the title to whatever fits in the middle.
    let middle = total_width.saturating_sub(OVERVIEW_PREFIX_VIS + right_vis + 2);
    if middle == 0 {
        return (String::new(), 0);
    }
    let trimmed = truncate_with_ellipsis(title, middle);
    let vis = trimmed.chars().count();
    (trimmed, vis)
}

/// Build a `Line` for a queued spec in the overview.
fn overview_queued_row(s: &SpecSummary, selected: bool, width: usize) -> Line<'static> {
    let cursor = if selected { "▶" } else { "◦" };
    let id_field = format!(
        " {:<w$}  ",
        truncate(&s.spec_id, OVERVIEW_ID_W),
        w = OVERVIEW_ID_W
    );
    // Queued rows have no right column ⇒ all remaining width feeds the title.
    let (title_text, title_vis) = overview_title_segment(s, width, 0);
    let pad = width.saturating_sub(OVERVIEW_PREFIX_VIS + title_vis);

    let (glyph_style, base_style) = if selected {
        let rev = Style::default().add_modifier(Modifier::REVERSED);
        (rev, rev)
    } else {
        (Style::default().fg(Color::Cyan), Style::default())
    };
    let title_style = if selected {
        base_style
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };

    Line::from(vec![
        Span::styled(cursor.to_string(), glyph_style),
        Span::styled(id_field, glyph_style),
        Span::styled(title_text, title_style),
        Span::styled(" ".repeat(pad), base_style),
    ])
}

/// Build a `Line` for a finished spec in the overview.
fn overview_finished_row(
    s: &SpecSummary,
    selected: bool,
    width: usize,
    now: DateTime<Utc>,
) -> Line<'static> {
    let cursor = if selected { "▶" } else { " " };
    let (icon, icon_color) = finished_icon(&s.status);
    let ago = s.completed_at.map_or_else(
        || "—".to_string(),
        |t| time_ago((now - t).num_milliseconds().max(0) as u64),
    );
    let right_suffix = format!("  {ago}");
    let right_vis = icon.chars().count() + right_suffix.chars().count();
    let (title_text, title_vis) = overview_title_segment(s, width, right_vis);
    let pad = width.saturating_sub(OVERVIEW_PREFIX_VIS + title_vis + right_vis);

    let id_field = format!(
        " {:<w$}  ",
        truncate(&s.spec_id, OVERVIEW_ID_W),
        w = OVERVIEW_ID_W
    );

    let dim = Style::default().add_modifier(Modifier::DIM);
    let (cursor_style, id_style, pad_style) = if selected {
        let rev = Style::default().add_modifier(Modifier::REVERSED);
        (rev, rev, rev)
    } else {
        (dim, dim, dim)
    };
    let icon_style = if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default().fg(icon_color)
    };
    let title_style = if selected { id_style } else { dim };

    Line::from(vec![
        Span::styled(cursor.to_string(), cursor_style),
        Span::styled(id_field, id_style),
        Span::styled(title_text, title_style),
        Span::styled(" ".repeat(pad), pad_style),
        Span::styled(icon.to_string(), icon_style),
        Span::styled(right_suffix, dim),
    ])
}

/// Render the v1-style top-level overview: RUNNING / QUEUED / FINISHED sections.
///
/// Groups `picker.specs` into three colored sections separated by blank lines.
/// Each row is: `{cursor} {id:<OVERVIEW_ID_W}  {gap}  {right_col}` with the
/// right column right-aligned to `area.width`.
pub fn draw_overview(frame: &mut Frame, area: Rect, picker: &PickerScreen, now: DateTime<Utc>) {
    let width = area.width as usize;
    let mut lines: Vec<Line> = Vec::new();

    // Header: "BOI" (bold cyan) left, key hints (dim) right.
    let hints = "↑↓ · ⏎ open · q quit";
    let boi = "BOI";
    let gap = width.saturating_sub(boi.len() + hints.len());
    lines.push(Line::from(vec![
        Span::styled(
            boi,
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Cyan),
        ),
        Span::raw(" ".repeat(gap)),
        Span::styled(hints, Style::default().add_modifier(Modifier::DIM)),
    ]));
    lines.push(Line::from(Span::raw("")));

    // Partition specs preserving their flat-list index for selection tracking.
    let mut running_rows: Vec<(usize, &SpecSummary)> = Vec::new();
    let mut queued_rows: Vec<(usize, &SpecSummary)> = Vec::new();
    let mut finished_rows: Vec<(usize, &SpecSummary)> = Vec::new();
    for (idx, s) in picker.specs.iter().enumerate() {
        match s.status.as_str() {
            "running" => running_rows.push((idx, s)),
            "queued" => queued_rows.push((idx, s)),
            _ => finished_rows.push((idx, s)),
        }
    }

    if !running_rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "RUNNING",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        for (idx, s) in &running_rows {
            lines.push(overview_running_row(s, *idx == picker.selected, width, now));
        }
        lines.push(Line::from(Span::raw("")));
    }

    if !queued_rows.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("QUEUED ({})", queued_rows.len()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for (idx, s) in &queued_rows {
            lines.push(overview_queued_row(s, *idx == picker.selected, width));
        }
        lines.push(Line::from(Span::raw("")));
    }

    if !finished_rows.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("FINISHED  ({})", finished_rows.len()),
            Style::default().add_modifier(Modifier::DIM | Modifier::BOLD),
        )));
        for (idx, s) in &finished_rows {
            lines.push(overview_finished_row(
                s,
                *idx == picker.selected,
                width,
                now,
            ));
        }
        lines.push(Line::from(Span::raw("")));
    }

    if picker.specs.is_empty() {
        lines.push(Line::from(Span::raw("  (no specs found)")));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

// ─── Picker (legacy flat list, kept for tests) ────────────────────────────────

/// Render the spec-picker screen — one row per spec, selected row reversed.
///
/// Each row shows `<ID>  <title>` (dim title) ahead of the status/age/phase
/// columns. When `title` is `None` the row collapses to the legacy ID-only
/// form. (Per the 2026-06-01 strip-$ directive the per-spec dollar column
/// is gone — only phase count survives as the spend-hint signal.)
pub fn draw_picker(frame: &mut Frame, area: Rect, picker: &PickerScreen, now: DateTime<Utc>) {
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(area);
    frame.render_widget(
        Paragraph::new(" boi dashboard — specs    ↑↓ move · ⏎ open · q quit"),
        chunks[0],
    );

    // Reserve a column for the title so columns stay aligned across rows.
    // " {ID:<11} {title:<W} {status:<10} {age:>8}  {phases:>3} phases".
    let id_w = 11;
    let status_w = 10;
    let age_w = 8;
    let phase_w = 3;
    // 1 (leading space) + id_w + 1 (gap) + (title_w + 1 if showing)
    //   + status_w + 1 + age_w + 2 + phase_w + 7 (" phases")
    let fixed_no_title = 1 + id_w + 1 + status_w + 1 + age_w + 2 + phase_w + 7;
    let total_w = area.width as usize;
    // Reserve up to 36 chars for the title; never more than what's free.
    let title_w = total_w.saturating_sub(fixed_no_title + 1).min(36);

    let lines: Vec<Line> = picker
        .specs
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let age = s.started_at.map_or_else(
                || "—".to_string(),
                |t| fmt_ms((now - t).num_milliseconds().max(0) as u64),
            );
            let style = if i == picker.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let dim = if i == picker.selected {
                style
            } else {
                Style::default().add_modifier(Modifier::DIM)
            };
            let id_text = format!(" {:<w$}", truncate(&s.spec_id, id_w), w = id_w);
            let title_text = s
                .title
                .as_deref()
                .filter(|t| !t.is_empty() && title_w > 0)
                .map(|t| truncate_with_ellipsis(t, title_w))
                .unwrap_or_default();
            let title_field = if title_w == 0 {
                String::new()
            } else {
                format!(" {title_text:<title_w$}")
            };
            let tail = format!(
                " {:<sw$} {:>aw$}  {:>pw$} phases",
                s.status,
                age,
                s.phase_count,
                sw = status_w,
                aw = age_w,
                pw = phase_w,
            );
            Line::from(vec![
                Span::styled(id_text, style),
                Span::styled(title_field, dim),
                Span::styled(tail, style),
            ])
        })
        .collect();

    let body = if lines.is_empty() {
        Paragraph::new("  (no specs found)")
    } else {
        Paragraph::new(lines)
    };
    frame.render_widget(body, chunks[1]);
}

/// One-word kind label for a leaf node.
fn kind_word(kind: crate::cli::dashboard::model::NodeKind) -> &'static str {
    use crate::cli::dashboard::model::NodeKind;
    match kind {
        NodeKind::LlmTurn => "llm",
        NodeKind::ToolCall => "tool",
        _ => "node",
    }
}

/// Detail text for an expanded log line.
fn event_detail(node: &crate::cli::dashboard::model::DashNode) -> &str {
    &node.detail
}

/// Truncate `s` to `max` chars (no ellipsis — width is tight).
fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Visual width (in chars) of the behavior-prefix fallback used by
/// [`task_label`] when a task has no `ref`. The result always fits in this
/// many chars, with `…` consuming the last cell when the behavior overflows.
pub const TASK_LABEL_FALLBACK_WIDTH: usize = 30;

/// Truncate `s` to fit `max` characters of visual width.
///
/// - Short or exact-fit strings pass through unchanged (no spurious `…`).
/// - Overflows are truncated to `max - 1` chars and end with the single-char
///   `…`, so the result always counts exactly `max` chars.
/// - `max == 0` returns an empty string.
///
/// Width is measured in chars (not bytes), so multi-byte UTF-8 sequences are
/// counted correctly. This is the shared truncation primitive every BOI
/// surface that renders human-readable labels next to IDs reuses — keep the
/// truncation policy here so the dashboard, `boi log`, and any future
/// renderer agree on a single ellipsis convention.
pub fn truncate_with_ellipsis(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    // `max - 1` chars + `…` ⇒ exactly `max` cells.
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Human-readable label for a task, paired with its task ID in every
/// dashboard surface.
///
/// - When `task_ref` is `Some`, returns the ref verbatim — the
///   author-supplied slug is already terse and recognizable.
/// - When `task_ref` is `None`, falls back to the first
///   [`TASK_LABEL_FALLBACK_WIDTH`] chars of `behavior` (followed by `…` only
///   when behavior actually overflows). This keeps the label readable when
///   a spec omits the optional `ref`.
pub fn task_label(task_ref: Option<&str>, behavior: &str) -> String {
    if let Some(r) = task_ref {
        return r.to_string();
    }
    truncate_with_ellipsis(behavior, TASK_LABEL_FALLBACK_WIDTH)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `blocked` status cell is drawn loud (bold yellow); everything else
    /// inherits the row's base style untouched.
    #[test]
    fn blocked_status_cell_is_loud() {
        let base = Style::default();
        let blocked = status_cell_style(base, "blocked");
        assert_eq!(blocked.fg, Some(Color::Yellow), "blocked is yellow");
        assert!(
            blocked.add_modifier.contains(Modifier::BOLD),
            "blocked is bold",
        );

        let running = status_cell_style(base, "running");
        assert_eq!(running.fg, None, "non-blocked status is not recolored");
        assert!(
            !running.add_modifier.contains(Modifier::BOLD),
            "non-blocked status is not bolded",
        );
    }

    // ─── RED tests for the labels-next-to-IDs feature ──────────────────────
    // Task Trftrpvt5: every CLI surface that identifies a spec/task by ID
    // must show its human-readable label next to the ID, truncated with `…`
    // to fit available width. These tests pin two new helpers the renderer
    // must expose and use:
    //
    //   * `truncate_with_ellipsis(s, max)` — preserves short / exact-fit
    //     labels unchanged; overflows end in the single-char `…`.
    //   * `task_label(task_ref, behavior)` — returns the ref verbatim when
    //     set; falls back to the first ~30 chars of `behavior` (followed by
    //     `…` only when behavior actually overflows).
    //
    // These intentionally reference the (not-yet-existing) helpers so the
    // RED step is "won't compile" — implementation adds the helpers and
    // wires them into draw_tree / draw_overview / draw_picker.

    /// Short labels pass through unchanged — no ellipsis, no truncation.
    #[test]
    fn truncate_with_ellipsis_short_label_unchanged() {
        assert_eq!(truncate_with_ellipsis("abc", 10), "abc");
    }

    /// Exact-fit labels also pass through unchanged.
    #[test]
    fn truncate_with_ellipsis_exact_fit_unchanged() {
        let label = "abcdef";
        let out = truncate_with_ellipsis(label, 6);
        assert_eq!(out, label, "max == len → identity, no `…`");
        assert!(
            !out.ends_with('…'),
            "exact-fit must NOT have an ellipsis suffix, got {out:?}",
        );
    }

    /// Overflowing labels are truncated and end with the single-char `…`.
    /// Width is measured in chars (not bytes); the result fits in `max`.
    #[test]
    fn truncate_with_ellipsis_overflow_ends_with_ellipsis() {
        let out = truncate_with_ellipsis("abcdefghij", 5);
        assert_eq!(
            out.chars().count(),
            5,
            "result must fit in `max` chars (got {out:?})",
        );
        assert!(
            out.ends_with('…'),
            "overflow must end with `…`, got {out:?}",
        );
    }

    /// `task_label` returns the ref verbatim when one is set.
    #[test]
    fn task_label_returns_ref_when_set() {
        assert_eq!(
            task_label(Some("dashboard-tui-labels"), "ignored behavior text"),
            "dashboard-tui-labels",
        );
    }

    /// `task_label` falls back to the first ~30 chars of `behavior` (followed
    /// by `…`) when `ref` is None — this is the verification's
    /// `fallback_to_behavior` case.
    #[test]
    fn task_label_falls_back_to_behavior_prefix_when_ref_missing() {
        let behavior =
            "Update src/cli/dashboard/render.rs and the supporting model/state to display titles";
        let label = task_label(None, behavior);
        // ~30 chars (the contract says "first ~30 chars … followed by `…`").
        // Accept any reasonable bound 28..=32 chars to keep the test robust.
        let len = label.chars().count();
        assert!(
            (28..=32).contains(&len),
            "behavior fallback should be ~30 chars total, got len={len} label={label:?}",
        );
        assert!(
            label.ends_with('…'),
            "long behavior must end with `…`, got {label:?}",
        );
        assert!(
            label.starts_with("Update src/cli/dashboard"),
            "fallback should preserve the behavior prefix, got {label:?}",
        );
    }

    /// `task_label` with a short `behavior` and no `ref` returns the behavior
    /// unchanged — no spurious ellipsis when the string already fits.
    #[test]
    fn task_label_short_behavior_has_no_ellipsis() {
        let label = task_label(None, "add config flag");
        assert_eq!(label, "add config flag");
        assert!(!label.ends_with('…'), "short label keeps no ellipsis");
    }

    #[test]
    fn bar_width_is_always_exact() {
        let split = TimeSplit {
            think_ms: 50,
            do_ms: 50,
            idle_ms: 0,
        };
        for node_ms in [0_u64, 25, 50, 100] {
            let b = bar(node_ms, 100, split, 20);
            assert_eq!(b.chars().count(), 20, "bar must be exactly `width` cells");
        }
    }

    #[test]
    fn longest_node_fills_the_whole_bar() {
        let split = TimeSplit {
            think_ms: 100,
            do_ms: 0,
            idle_ms: 0,
        };
        let b = bar(100, 100, split, 10);
        assert_eq!(b, "██████████", "node == max => all filled, all think");
    }

    #[test]
    fn zero_max_yields_empty_bar() {
        let b = bar(0, 0, TimeSplit::default(), 8);
        assert_eq!(b, "░░░░░░░░");
    }

    #[test]
    fn draw_tree_renders_each_child_row() {
        use crate::cli::dashboard::model::{DashNode, NodeKind, SortMode, build_tree};
        use crate::cli::dashboard::state::SpecScreen;
        use crate::types::ids::SpecId;
        use chrono::Utc;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::collections::HashSet;

        let mut tree = build_tree("S0042a", &[]);
        tree.children = vec![DashNode {
            kind: NodeKind::Task,
            label: "T1-routing".into(),
            status: "done".into(),
            started_at: Utc::now(),
            completed_at: Some(Utc::now()),
            split: Default::default(),
            phase_key: None,
            detail: String::new(),
            cost: Default::default(),
            task_ref: None,
            behavior: None,
            children: vec![],
        }];
        let spec = SpecScreen {
            spec_id: SpecId::new("S0000001a").unwrap(),
            title: None,
            tree,
            drill: vec![],
            selected: 0,
            expanded: HashSet::new(),
        };

        let backend = TestBackend::new(80, 10);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw_tree(f, f.area(), &spec, SortMode::Waterfall, None, Utc::now()))
            .unwrap();

        let buf = term.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("T1-routing"), "child row is drawn");
        assert!(text.contains("waterfall"), "sort mode shown in header");
    }

    #[test]
    fn draw_tree_shows_tokens_column() {
        use crate::cli::dashboard::model::{DashNode, NodeKind, SortMode, Tokens, build_tree};
        use crate::cli::dashboard::state::SpecScreen;
        use crate::types::ids::SpecId;
        use chrono::Utc;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::collections::HashSet;

        let mut tree = build_tree("S0042a000", &[]);
        tree.children = vec![DashNode {
            kind: NodeKind::Task,
            label: "T1".into(),
            status: "done".into(),
            started_at: Utc::now(),
            completed_at: Some(Utc::now()),
            split: Default::default(),
            phase_key: None,
            detail: String::new(),
            cost: Tokens {
                tokens_in: 4_000,
                tokens_out: 800,
            },
            task_ref: None,
            behavior: None,
            children: vec![],
        }];
        let spec = SpecScreen {
            spec_id: SpecId::new("S0000001a").unwrap(),
            title: None,
            tree,
            drill: vec![],
            selected: 0,
            expanded: HashSet::new(),
        };

        let backend = TestBackend::new(90, 8);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw_tree(f, f.area(), &spec, SortMode::Waterfall, None, Utc::now()))
            .unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        // 4000 + 800 = 4800 tokens → `4.8k` (per `fmt_tokens`'s formatting).
        // Per the 2026-06-01 strip-$ directive the tokens column replaces
        // the dollar column — no `$` should appear.
        assert!(text.contains("4.8k"), "tokens column rendered: {text}");
        assert!(!text.contains('$'), "no dollar should remain: {text}");
    }

    #[test]
    fn draw_log_shows_event_and_expanded_detail() {
        use crate::cli::dashboard::model::{DashNode, NodeKind, build_tree};
        use crate::cli::dashboard::state::SpecScreen;
        use crate::cli::dashboard::trace::PhaseKey;
        use crate::types::ids::SpecId;
        use chrono::Utc;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::collections::HashSet;

        let mut phase = DashNode {
            kind: NodeKind::Phase,
            label: "implement".into(),
            status: "active".into(),
            started_at: Utc::now(),
            completed_at: None,
            split: Default::default(),
            phase_key: Some(PhaseKey {
                task_id: Some("T1".into()),
                phase: "implement".into(),
                iteration: 1,
            }),
            detail: String::new(),
            cost: Default::default(),
            task_ref: None,
            behavior: None,
            children: vec![DashNode {
                kind: NodeKind::ToolCall,
                label: "cargo test".into(),
                status: "done".into(),
                started_at: Utc::now(),
                completed_at: Some(Utc::now()),
                split: Default::default(),
                phase_key: None,
                detail: "test foo ... FAILED".into(),
                cost: Default::default(),
                task_ref: None,
                behavior: None,
                children: vec![],
            }],
        };
        let mut tree = build_tree("S0042a", &[]);
        tree.children = vec![phase.clone()];
        let mut expanded = HashSet::new();
        expanded.insert(0_usize);
        let spec = SpecScreen {
            spec_id: SpecId::new("S0000001a").unwrap(),
            title: None,
            tree,
            drill: vec![0], // focus the phase
            selected: 0,
            expanded,
        };

        let backend = TestBackend::new(80, 12);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw_log(f, f.area(), &spec, None, Utc::now()))
            .unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("cargo test"));
        assert!(text.contains("FAILED"), "expanded detail is shown");
        let _ = &mut phase;
    }

    #[test]
    fn draw_picker_renders_a_spec_row() {
        use crate::cli::dashboard::picker::SpecSummary;
        use crate::cli::dashboard::state::PickerScreen;
        use chrono::Utc;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let picker = PickerScreen {
            specs: vec![SpecSummary {
                spec_id: "S0000001a".into(),
                title: None,
                status: "running".into(),
                started_at: Some(Utc::now()),
                completed_at: None,
                phase_count: 3,
            }],
            selected: 0,
        };
        let backend = TestBackend::new(80, 6);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw_picker(f, f.area(), &picker, Utc::now()))
            .unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("S0000001a"));
        assert!(text.contains("3 phases"));
        // Per the 2026-06-01 strip-$ directive no `$` appears in the picker.
        assert!(!text.contains('$'), "no dollar should remain: {text:?}");
    }

    /// Build the multi-spec fixture used by several tests below.
    fn multi_spec_fixture() -> Vec<SpecSummary> {
        use chrono::{TimeZone, Utc};
        vec![
            SpecSummary {
                spec_id: "S1RUNNING".into(),
                title: None,
                status: "running".into(),
                started_at: Some(Utc.with_ymd_and_hms(2026, 5, 23, 11, 30, 0).unwrap()),
                completed_at: None,
                phase_count: 5,
            },
            SpecSummary {
                spec_id: "S2RUNNING".into(),
                title: None,
                status: "running".into(),
                started_at: Some(Utc.with_ymd_and_hms(2026, 5, 23, 11, 45, 0).unwrap()),
                completed_at: None,
                phase_count: 2,
            },
            SpecSummary {
                spec_id: "S3RUNNING".into(),
                title: None,
                status: "running".into(),
                started_at: Some(Utc.with_ymd_and_hms(2026, 5, 23, 11, 58, 0).unwrap()),
                completed_at: None,
                phase_count: 1,
            },
            SpecSummary {
                spec_id: "S4QUEUED".into(),
                title: None,
                status: "queued".into(),
                started_at: None,
                completed_at: None,
                phase_count: 0,
            },
            SpecSummary {
                spec_id: "S5QUEUED".into(),
                title: None,
                status: "queued".into(),
                started_at: None,
                completed_at: None,
                phase_count: 0,
            },
            SpecSummary {
                spec_id: "S6FINISH".into(),
                title: None,
                status: "completed".into(),
                started_at: Some(Utc.with_ymd_and_hms(2026, 5, 23, 10, 0, 0).unwrap()),
                completed_at: Some(Utc.with_ymd_and_hms(2026, 5, 23, 11, 0, 0).unwrap()),
                phase_count: 8,
            },
            SpecSummary {
                spec_id: "S7FINISH".into(),
                title: None,
                status: "completed".into(),
                started_at: Some(Utc.with_ymd_and_hms(2026, 5, 23, 9, 0, 0).unwrap()),
                completed_at: Some(Utc.with_ymd_and_hms(2026, 5, 23, 10, 30, 0).unwrap()),
                phase_count: 12,
            },
            SpecSummary {
                spec_id: "S8FAILED".into(),
                title: None,
                status: "failed".into(),
                started_at: Some(Utc.with_ymd_and_hms(2026, 5, 23, 8, 0, 0).unwrap()),
                completed_at: Some(Utc.with_ymd_and_hms(2026, 5, 23, 8, 45, 0).unwrap()),
                phase_count: 3,
            },
            SpecSummary {
                spec_id: "S9FINISH".into(),
                title: None,
                status: "completed".into(),
                started_at: Some(Utc.with_ymd_and_hms(2026, 5, 22, 20, 0, 0).unwrap()),
                completed_at: Some(Utc.with_ymd_and_hms(2026, 5, 22, 22, 0, 0).unwrap()),
                phase_count: 15,
            },
            SpecSummary {
                spec_id: "SAFINISH".into(),
                title: None,
                status: "completed".into(),
                started_at: Some(Utc.with_ymd_and_hms(2026, 5, 22, 14, 0, 0).unwrap()),
                completed_at: Some(Utc.with_ymd_and_hms(2026, 5, 22, 16, 0, 0).unwrap()),
                phase_count: 10,
            },
        ]
    }

    #[test]
    fn draw_overview_three_sections_at_120x40() {
        use crate::cli::dashboard::state::PickerScreen;
        use chrono::{TimeZone, Utc};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let now = Utc.with_ymd_and_hms(2026, 5, 23, 12, 0, 0).unwrap();
        let picker = PickerScreen {
            specs: multi_spec_fixture(),
            selected: 0,
        };
        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw_overview(f, f.area(), &picker, now))
            .unwrap();

        let buf = term.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();

        // Section headers.
        assert!(text.contains("RUNNING"), "RUNNING section header present");
        assert!(text.contains("QUEUED"), "QUEUED section header present");
        assert!(text.contains("FINISHED"), "FINISHED section header present");

        // Every spec ID.
        for id in &[
            "S1RUNNING",
            "S2RUNNING",
            "S3RUNNING",
            "S4QUEUED",
            "S5QUEUED",
            "S6FINISH",
            "S7FINISH",
            "S8FAILED",
            "S9FINISH",
            "SAFINISH",
        ] {
            assert!(text.contains(id), "spec ID {id} is rendered");
        }

        // Status glyphs — one per state.
        assert!(text.contains('▸'), "running glyph ▸ present");
        assert!(text.contains('◦'), "queued glyph ◦ present");
        assert!(text.contains('✓'), "completed glyph ✓ present");
        assert!(text.contains('✗'), "failed glyph ✗ present");
    }

    #[test]
    fn draw_tree_spans_full_width_at_120x40() {
        use crate::cli::dashboard::model::{DashNode, NodeKind, SortMode, Tokens, build_tree};
        use crate::cli::dashboard::state::SpecScreen;
        use crate::types::ids::SpecId;
        use chrono::Utc;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::collections::HashSet;

        let mut tree = build_tree("S0042a000", &[]);
        tree.children = vec![DashNode {
            kind: NodeKind::Task,
            label: "T1-implement-the-core-api-endpoint-for-spec-management-flow".into(),
            status: "running".into(),
            started_at: Utc::now(),
            completed_at: None,
            split: Default::default(),
            phase_key: None,
            detail: String::new(),
            cost: Tokens {
                tokens_in: 4_000,
                tokens_out: 800,
            },
            task_ref: None,
            behavior: None,
            children: vec![],
        }];
        let spec = SpecScreen {
            spec_id: SpecId::new("S0000001a").unwrap(),
            title: None,
            tree,
            drill: vec![],
            selected: 0,
            expanded: HashSet::new(),
        };

        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw_tree(f, f.area(), &spec, SortMode::Waterfall, None, Utc::now()))
            .unwrap();

        let buf = term.backend().buffer().clone();
        // Row 1 is the first child row (row 0 is the header).
        let last_non_space = (0..120_u16)
            .rev()
            .find(|&col| {
                buf.cell((col, 1))
                    .map(|c| c.symbol() != " ")
                    .unwrap_or(false)
            })
            .unwrap_or(0);
        assert!(
            last_non_space >= 100,
            "tree row should span full width; last non-space col = {last_non_space}",
        );
    }

    // ─── Snapshot tests for the labels-next-to-IDs feature (Trftrpvt5) ────
    //
    // These pin the renderer's behavior for a fixture spec with a known
    // title and a mix of task children — one with `ref` set, one with `ref`
    // = None (forcing the behavior-prefix fallback). Together they cover the
    // `render_uses_label`, `fallback_to_behavior`, and `truncation_unit_test`
    // verifications: the header shows the title next to the spec ID, the task
    // table shows ref/behavior labels next to task IDs, and the
    // ellipsis-truncation policy is consistent across all surfaces.

    /// Build a fixture `SpecScreen` with a known title and two task
    /// children — one with `ref`, one without — so a single snapshot covers
    /// both branches of [`task_label`].
    fn labeled_spec_screen() -> SpecScreen {
        use crate::cli::dashboard::model::{DashNode, NodeKind, Tokens, build_tree};
        use crate::types::ids::SpecId;
        use chrono::{TimeZone, Utc};
        use std::collections::HashSet;

        let started = Utc.with_ymd_and_hms(2026, 5, 23, 11, 0, 0).unwrap();
        let mut tree = build_tree("S479p5wxb", &[]);
        tree.children = vec![
            DashNode {
                kind: NodeKind::Task,
                label: "T10vhjs33".into(),
                status: "done".into(),
                started_at: started,
                completed_at: Some(started + chrono::Duration::seconds(30)),
                split: Default::default(),
                phase_key: None,
                detail: String::new(),
                cost: Tokens::default(),
                task_ref: Some("dashboard-tui-labels".into()),
                behavior: Some("Update src/cli/dashboard/render.rs to display labels next to IDs.".into()),
                children: vec![],
            },
            DashNode {
                kind: NodeKind::Task,
                label: "Trftrpvt5".into(),
                status: "active".into(),
                started_at: started,
                completed_at: None,
                split: Default::default(),
                phase_key: None,
                detail: String::new(),
                cost: Tokens::default(),
                task_ref: None,
                behavior: Some(
                    "Update src/cli/dashboard/render.rs (and the supporting model/state in src/cli/dashboard/model.rs / state.rs if needed) to display titles and task labels next to IDs everywhere."
                        .into(),
                ),
                children: vec![],
            },
        ];
        SpecScreen {
            spec_id: SpecId::new("S479p5wxb").unwrap(),
            title: Some("Show titles + task labels next to IDs in the dashboard".into()),
            tree,
            drill: vec![],
            selected: 0,
            expanded: HashSet::new(),
        }
    }

    /// Collect the buffer to a single newline-joined string for content
    /// assertions.
    fn buffer_to_text(buf: &ratatui::buffer::Buffer) -> String {
        let area = buf.area();
        let mut out = String::new();
        for row in 0..area.height {
            for col in 0..area.width {
                if let Some(c) = buf.cell((col, row)) {
                    out.push_str(c.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

    /// The header of `draw_tree` shows the spec title next to the spec ID.
    #[test]
    fn draw_tree_header_includes_spec_title() {
        use chrono::Utc;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let spec = labeled_spec_screen();
        let backend = TestBackend::new(120, 8);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw_tree(f, f.area(), &spec, SortMode::Waterfall, None, Utc::now()))
            .unwrap();
        let buf = term.backend().buffer().clone();
        let text = buffer_to_text(&buf);
        let header = text.lines().next().expect("at least one line rendered");
        assert!(
            header.contains("S479p5wxb"),
            "header still shows the spec ID, got: {header:?}",
        );
        assert!(
            header.contains("Show titles + task labels next to IDs"),
            "header shows the spec title next to the ID, got: {header:?}",
        );
    }

    /// Each task row in `draw_tree` shows the task ID followed by the task
    /// label — `ref` verbatim for one child, behavior-prefix fallback for the
    /// other (with `…` since the behavior overflows).
    #[test]
    fn draw_tree_task_rows_show_id_and_label() {
        use chrono::Utc;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let spec = labeled_spec_screen();
        let backend = TestBackend::new(160, 8);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw_tree(f, f.area(), &spec, SortMode::Waterfall, None, Utc::now()))
            .unwrap();
        let buf = term.backend().buffer().clone();
        let text = buffer_to_text(&buf);
        assert!(
            text.contains("T10vhjs33"),
            "task ID is rendered, text: {text:?}",
        );
        assert!(
            text.contains("dashboard-tui-labels"),
            "task ref label is rendered next to the ID, text: {text:?}",
        );
        assert!(
            text.contains("Trftrpvt5"),
            "second task ID is rendered, text: {text:?}",
        );
        // Behavior-prefix fallback: starts with the behavior prefix and ends
        // in `…` somewhere on the row.
        assert!(
            text.contains("Update src/cli/dashboard"),
            "behavior-prefix fallback appears for the ref-less task, text: {text:?}",
        );
        assert!(
            text.contains('…'),
            "behavior overflow uses `…`, text: {text:?}",
        );
    }

    /// The overview picker row shows the spec title next to the ID in dim
    /// style; the ID column never gets pushed off-screen.
    #[test]
    fn draw_overview_row_shows_title_next_to_id() {
        use crate::cli::dashboard::picker::SpecSummary;
        use crate::cli::dashboard::state::PickerScreen;
        use chrono::{TimeZone, Utc};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let now = Utc.with_ymd_and_hms(2026, 5, 23, 12, 0, 0).unwrap();
        let picker = PickerScreen {
            specs: vec![SpecSummary {
                spec_id: "S479p5wxb".into(),
                title: Some("Show titles + task labels next to IDs".into()),
                status: "running".into(),
                started_at: Some(Utc.with_ymd_and_hms(2026, 5, 23, 11, 30, 0).unwrap()),
                completed_at: None,
                phase_count: 4,
            }],
            selected: 0,
        };
        let backend = TestBackend::new(120, 6);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw_overview(f, f.area(), &picker, now))
            .unwrap();
        let buf = term.backend().buffer().clone();
        let text = buffer_to_text(&buf);
        assert!(text.contains("S479p5wxb"), "ID still rendered: {text:?}");
        assert!(
            text.contains("Show titles + task labels"),
            "title rendered alongside ID: {text:?}",
        );
    }

    /// The legacy `draw_picker` row also shows the spec title next to the ID.
    #[test]
    fn draw_picker_row_shows_title_next_to_id() {
        use crate::cli::dashboard::picker::SpecSummary;
        use crate::cli::dashboard::state::PickerScreen;
        use chrono::{TimeZone, Utc};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let now = Utc.with_ymd_and_hms(2026, 5, 23, 12, 0, 0).unwrap();
        let picker = PickerScreen {
            specs: vec![SpecSummary {
                spec_id: "S479p5wxb".into(),
                title: Some("Add dashboard labels".into()),
                status: "running".into(),
                started_at: Some(Utc.with_ymd_and_hms(2026, 5, 23, 11, 30, 0).unwrap()),
                completed_at: None,
                phase_count: 4,
            }],
            selected: 0,
        };
        let backend = TestBackend::new(140, 4);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw_picker(f, f.area(), &picker, now))
            .unwrap();
        let buf = term.backend().buffer().clone();
        let text = buffer_to_text(&buf);
        assert!(text.contains("S479p5wxb"), "ID still rendered: {text:?}");
        assert!(
            text.contains("Add dashboard labels"),
            "title rendered alongside ID: {text:?}",
        );
    }

    /// `id_and_label` returns the ref-derived label for a task with `ref`
    /// set, and the behavior-prefix fallback (with `…`) when `ref` is None
    /// — pinning the `fallback_to_behavior` verification path used by the
    /// task table renderer.
    #[test]
    fn id_and_label_uses_ref_when_set_else_behavior_prefix() {
        use crate::cli::dashboard::model::{DashNode, NodeKind, Tokens};
        use chrono::Utc;

        let with_ref = DashNode {
            kind: NodeKind::Task,
            label: "T10vhjs33".into(),
            status: "done".into(),
            started_at: Utc::now(),
            completed_at: Some(Utc::now()),
            split: Default::default(),
            phase_key: None,
            detail: String::new(),
            cost: Tokens::default(),
            task_ref: Some("dashboard-tui-labels".into()),
            behavior: Some("ignored".into()),
            children: vec![],
        };
        let (id, label) = id_and_label(&with_ref, 80);
        assert_eq!(id, "T10vhjs33");
        assert_eq!(
            label, "dashboard-tui-labels",
            "ref wins over behavior when set",
        );

        let without_ref = DashNode {
            task_ref: None,
            behavior: Some(
                "Update src/cli/dashboard/render.rs and supporting modules to display labels."
                    .into(),
            ),
            ..with_ref.clone()
        };
        let (_, label) = id_and_label(&without_ref, 80);
        assert!(
            label.starts_with("Update src/cli/dashboard"),
            "behavior fallback preserves the prefix, got {label:?}",
        );
        assert!(
            label.ends_with('…'),
            "long behavior fallback ends with `…`, got {label:?}",
        );
        // The fallback caps at TASK_LABEL_FALLBACK_WIDTH chars (30); when the
        // surrounding label column is wider, the cap still wins (per the
        // contract: "first ~30 chars of behavior").
        assert_eq!(
            label.chars().count(),
            TASK_LABEL_FALLBACK_WIDTH,
            "fallback width is bounded by TASK_LABEL_FALLBACK_WIDTH",
        );

        // A non-Task node never emits a label, even if `behavior` is set
        // (defensive: behavior only applies to tasks).
        let phase = DashNode {
            kind: NodeKind::Phase,
            task_ref: None,
            behavior: Some("not a task — should not appear".into()),
            ..with_ref
        };
        let (_, label) = id_and_label(&phase, 80);
        assert!(label.is_empty(), "non-Task nodes carry no label");
    }

    /// Renders the overview against the multi-spec fixture and writes the
    /// v2-after.txt capture for visual diffing against v2-before.txt.
    #[test]
    fn capture_v2_after_rendering() {
        use crate::cli::dashboard::state::PickerScreen;
        use chrono::{TimeZone, Utc};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::io::Write;

        let now = Utc.with_ymd_and_hms(2026, 5, 23, 12, 0, 0).unwrap();
        let picker = PickerScreen {
            specs: multi_spec_fixture(),
            selected: 0,
        };
        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw_overview(f, f.area(), &picker, now))
            .unwrap();

        let buf = term.backend().buffer().clone();
        let (width, height) = (120usize, 40usize);
        let mut out = String::new();
        for row in 0..height {
            let row_text: String = (0..width)
                .map(|col| {
                    buf.cell((col as u16, row as u16))
                        .map(|c| c.symbol())
                        .unwrap_or(" ")
                })
                .collect();
            out.push_str(row_text.trim_end());
            out.push('\n');
        }

        let path = "docs/design/2026-05-23-dashboard-redesign/v2-after.txt";
        std::fs::create_dir_all("docs/design/2026-05-23-dashboard-redesign").ok();
        let mut f = std::fs::File::create(path).expect("create v2-after.txt");
        f.write_all(out.as_bytes()).expect("write v2-after.txt");
    }

    /// Capture the current v2 picker rendering to a file for the baseline.
    /// Run with: cargo test --release capture_v2_before_baseline -- --nocapture --ignored
    #[test]
    #[ignore]
    fn capture_v2_before_baseline() {
        use crate::cli::dashboard::picker::SpecSummary;
        use crate::cli::dashboard::state::PickerScreen;
        use chrono::{TimeZone, Utc};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::io::Write;

        let now = Utc.with_ymd_and_hms(2026, 5, 23, 12, 0, 0).unwrap();
        let specs = vec![
            SpecSummary {
                spec_id: "S1RUNNING".into(),
                title: None,
                status: "running".into(),
                started_at: Some(Utc.with_ymd_and_hms(2026, 5, 23, 11, 30, 0).unwrap()),
                completed_at: None,
                phase_count: 5,
            },
            SpecSummary {
                spec_id: "S2RUNNING".into(),
                title: None,
                status: "running".into(),
                started_at: Some(Utc.with_ymd_and_hms(2026, 5, 23, 11, 45, 0).unwrap()),
                completed_at: None,
                phase_count: 2,
            },
            SpecSummary {
                spec_id: "S3RUNNING".into(),
                title: None,
                status: "running".into(),
                started_at: Some(Utc.with_ymd_and_hms(2026, 5, 23, 11, 58, 0).unwrap()),
                completed_at: None,
                phase_count: 1,
            },
            SpecSummary {
                spec_id: "S4QUEUED".into(),
                title: None,
                status: "queued".into(),
                started_at: None,
                completed_at: None,
                phase_count: 0,
            },
            SpecSummary {
                spec_id: "S5QUEUED".into(),
                title: None,
                status: "queued".into(),
                started_at: None,
                completed_at: None,
                phase_count: 0,
            },
            SpecSummary {
                spec_id: "S6FINISH".into(),
                title: None,
                status: "completed".into(),
                started_at: Some(Utc.with_ymd_and_hms(2026, 5, 23, 10, 0, 0).unwrap()),
                completed_at: Some(Utc.with_ymd_and_hms(2026, 5, 23, 11, 0, 0).unwrap()),
                phase_count: 8,
            },
            SpecSummary {
                spec_id: "S7FINISH".into(),
                title: None,
                status: "completed".into(),
                started_at: Some(Utc.with_ymd_and_hms(2026, 5, 23, 9, 0, 0).unwrap()),
                completed_at: Some(Utc.with_ymd_and_hms(2026, 5, 23, 10, 30, 0).unwrap()),
                phase_count: 12,
            },
            SpecSummary {
                spec_id: "S8FAILED".into(),
                title: None,
                status: "failed".into(),
                started_at: Some(Utc.with_ymd_and_hms(2026, 5, 23, 8, 0, 0).unwrap()),
                completed_at: Some(Utc.with_ymd_and_hms(2026, 5, 23, 8, 45, 0).unwrap()),
                phase_count: 3,
            },
            SpecSummary {
                spec_id: "S9FINISH".into(),
                title: None,
                status: "completed".into(),
                started_at: Some(Utc.with_ymd_and_hms(2026, 5, 22, 20, 0, 0).unwrap()),
                completed_at: Some(Utc.with_ymd_and_hms(2026, 5, 22, 22, 0, 0).unwrap()),
                phase_count: 15,
            },
            SpecSummary {
                spec_id: "SAFINISH".into(),
                title: None,
                status: "completed".into(),
                started_at: Some(Utc.with_ymd_and_hms(2026, 5, 22, 14, 0, 0).unwrap()),
                completed_at: Some(Utc.with_ymd_and_hms(2026, 5, 22, 16, 0, 0).unwrap()),
                phase_count: 10,
            },
        ];

        let picker = PickerScreen { specs, selected: 0 };
        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw_picker(f, f.area(), &picker, now))
            .unwrap();

        let buf = term.backend().buffer().clone();
        let width = 120usize;
        let height = 40usize;
        let mut out = String::new();
        for row in 0..height {
            let row_text: String = (0..width)
                .map(|col| {
                    buf.cell((col as u16, row as u16))
                        .map(|c| c.symbol())
                        .unwrap_or(" ")
                })
                .collect();
            out.push_str(row_text.trim_end());
            out.push('\n');
        }

        let path = "docs/design/2026-05-23-dashboard-redesign/v2-before.txt";
        let mut f = std::fs::File::create(path).expect("create v2-before.txt");
        f.write_all(out.as_bytes()).expect("write v2-before.txt");
        println!("Written to {path}");
        println!("--- begin ---");
        println!("{out}");
        println!("--- end ---");
    }
}
