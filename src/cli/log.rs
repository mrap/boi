//! `boi log <spec_id>` — the read-only phase-run history of one spec.
//!
//! Read-only — SQLite-direct, no daemon. Reads
//! `repo::phase_runs::fetch_history_for_spec` (G16.7), which already returns
//! rows `ORDER BY started_at, phase_iteration`; in-flight rows
//! (`completed_at IS NULL`) are marked `[running]`.
//!
//! ## Labels next to IDs (task T10vhjs33)
//!
//! The header line carries the spec's human-readable `title` next to its ID
//! (e.g. `Spec S479p5wxb "Show titles + task labels next to IDs"`), and every
//! phase-run row for a task carries that task's `ref` (or the first ~30 chars
//! of `behavior` when `ref` is `None`) next to the task ID. The truncation
//! policy (`…`) and the ref-vs-behavior decision are delegated to the shared
//! [`crate::cli::dashboard::render::task_label`] /
//! [`crate::cli::dashboard::render::truncate_with_ellipsis`] helpers so the
//! dashboard and `boi log` agree on a single rendering convention.

use std::collections::HashMap;

use serde_json::Value;
use sqlx::SqlitePool;

use crate::cli::dashboard::render::{
    TASK_LABEL_FALLBACK_WIDTH, task_label, truncate_with_ellipsis,
};
use crate::cli::paths;
use crate::cli::read_error::ReadError;
use crate::repo;
use crate::types::ids::SpecId;

/// Width of the task-label column rendered between the task ID and `iter=`
/// in each phase-run row. Matches [`TASK_LABEL_FALLBACK_WIDTH`] so a fully
/// truncated behavior-prefix fits without further ellipsis.
const TASK_LABEL_COL_WIDTH: usize = TASK_LABEL_FALLBACK_WIDTH;

/// Render `boi log <spec_id>` to stdout.
pub async fn run(spec_id: &str) -> Result<(), ReadError> {
    let db_url = paths::boi_db_url()?;
    let pool = repo::connect(&db_url).await?;
    let report = render(&pool, spec_id).await?;
    print!("{report}");
    Ok(())
}

/// Build the phase-run history report for `spec_id`.
///
/// Factored out of [`run`] so the deterministic-ordering L2 test drives it
/// against an in-memory pool.
pub async fn render(pool: &SqlitePool, spec_id: &str) -> Result<String, ReadError> {
    let sid = SpecId::new(spec_id).map_err(|e| ReadError::BadId(e.to_string()))?;
    // Surfaces `RepoError::NotFound` for an unknown spec — loud, not empty.
    let runtime = repo::spec_runtime::fetch(pool, &sid).await?;

    // Best-effort: a missing snapshot just leaves the title `None` and every
    // task label empty. We never fail the report on a label-lookup miss.
    let snapshot = repo::spec_versions::fetch_snapshot(pool, &sid, runtime.current_version)
        .await
        .ok();
    let title = snapshot
        .as_ref()
        .and_then(|s| s.get("title"))
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let task_labels = build_task_labels(pool, &sid, snapshot.as_ref()).await;

    let mut out = String::new();
    // Header: spec ID + (optional) title. The title sits in quotes so it
    // visually nests inside the ID, mirroring the dashboard header convention.
    match &title {
        Some(t) => out.push_str(&format!("Spec {spec_id} \"{t}\"\n")),
        None => out.push_str(&format!("Spec {spec_id}\n")),
    }

    let history = repo::phase_runs::fetch_history_for_spec(pool, &sid).await?;
    if history.is_empty() {
        out.push_str(&format!("Spec {spec_id} has no phase runs yet.\n"));
        return Ok(out);
    }

    out.push_str(&format!("Phase-run history for {spec_id}:\n"));
    for run in &history {
        let scope = run.task_id.as_deref().unwrap_or("(spec-level)");
        // Each task row carries the task's human-readable label (ref, or
        // behavior-prefix when ref is None). Spec-level rows leave the
        // label column blank — they're already identified by `(spec-level)`.
        let raw_label = run
            .task_id
            .as_deref()
            .and_then(|id| task_labels.get(id))
            .cloned()
            .unwrap_or_default();
        let label_col = truncate_with_ellipsis(&raw_label, TASK_LABEL_COL_WIDTH);
        let state = if run.is_open() {
            "[running]".to_owned()
        } else {
            // A completed run — show the verdict outcome if one was recorded.
            match run.worker_verdict() {
                Ok(Some(v)) => format!("[{}]", verdict_tag(&v)),
                Ok(None) => "[done]".to_owned(),
                Err(_) => "[done]".to_owned(),
            }
        };
        // OBS-024 — tokens column. An in-flight row shows `…`
        // placeholders so it's CLEAR the data lands at completion (not a
        // silent zero). A completed row shows the real `tokens_in/tokens_out`
        // figures persisted by `update_end`.
        //
        // The `tokens_out=0` case is honest: Goose's session record
        // sometimes carries `output_tokens: null` (spike §Q4), so BOI
        // reports input-only rather than fabricating a split. (Per the
        // 2026-06-01 strip-dollars directive no per-phase cost figure is
        // rendered.)
        let metrics = format_metrics(run);
        // Pre-pad the label column to TASK_LABEL_COL_WIDTH so the format
        // string below avoids the dynamic-width `{:<W}` named-argument
        // syntax (which would require a dollar-sign — forbidden in this
        // file per the 2026-06-01 strip-dollars directive).
        let label_col_padded = pad_right(&label_col, TASK_LABEL_COL_WIDTH);
        // Column layout (chars never wrap — fixed widths everywhere):
        //   <ts> <phase:14> <scope:12> <label:LABEL_W> iter=<n:3> <state:10> <provider:14> <metrics>
        out.push_str(&format!(
            "  {} {:<14} {:<12} {} iter={:<3} {:<10} {:<14} {}\n",
            run.started_at.format("%Y-%m-%dT%H:%M:%SZ"),
            run.phase,
            scope,
            label_col_padded,
            run.phase_iteration,
            state,
            run.provider,
            metrics,
        ));
    }
    Ok(out)
}

/// Build a `task_id → human-readable label` map for the rows in this report.
///
/// Strategy:
///   * A task with a `ref` in `task_runtime` is matched to a snapshot task
///     with the same `ref` and uses that snapshot task's `behavior`.
///   * A task with `ref = None` is matched positionally against the snapshot
///     tasks that also lack a `ref` (both lists are walked in their natural
///     order — `task_runtime` rows by `task_id`, snapshot tasks by array
///     index). This keeps un-slugged tasks labelled at all, mirroring the
///     dashboard's intent.
///
/// The label itself is built via [`task_label`] so the ref-vs-behavior
/// decision lives in exactly one place. Best-effort: any missing piece
/// (no snapshot, no `tasks` array, an unmatchable runtime row) simply omits
/// that task from the map — the renderer falls back to an empty label
/// column, never a panic.
async fn build_task_labels(
    pool: &SqlitePool,
    spec_id: &SpecId,
    snapshot: Option<&Value>,
) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    let runtime_rows = match repo::task_runtime::tasks_for_spec(pool, spec_id).await {
        Ok(rs) => rs,
        Err(_) => return out,
    };

    let snap_tasks: Vec<&Value> = snapshot
        .and_then(|s| s.get("tasks"))
        .and_then(|t| t.as_array())
        .map(|a| a.iter().collect())
        .unwrap_or_default();

    // `ref` slug → snapshot behavior, for ref-matched runtime tasks.
    let snap_by_ref: HashMap<&str, &str> = snap_tasks
        .iter()
        .filter_map(|t| {
            let r = t.get("ref").and_then(Value::as_str)?;
            let b = t.get("behavior").and_then(Value::as_str)?;
            Some((r, b))
        })
        .collect();
    // Behaviors of snapshot tasks WITHOUT a ref, in array order — these get
    // matched positionally against runtime tasks that also lack a ref.
    let snap_unrefed_behaviors: Vec<&str> = snap_tasks
        .iter()
        .filter(|t| t.get("ref").and_then(Value::as_str).is_none())
        .filter_map(|t| t.get("behavior").and_then(Value::as_str))
        .collect();

    let mut unrefed_cursor = 0usize;
    for r in &runtime_rows {
        let behavior: Option<String> = if let Some(slug) = r.r#ref.as_deref() {
            snap_by_ref.get(slug).copied().map(str::to_owned)
        } else {
            let b = snap_unrefed_behaviors
                .get(unrefed_cursor)
                .copied()
                .map(str::to_owned);
            unrefed_cursor += 1;
            b
        };
        let label = task_label(r.r#ref.as_deref(), behavior.as_deref().unwrap_or(""));
        if !label.is_empty() {
            out.insert(r.task_id.clone(), label);
        }
    }
    out
}

/// Pad `s` on the right with spaces so the result is exactly `width`
/// chars wide. Inputs already at or past `width` pass through unchanged.
///
/// Used instead of Rust's `{:<W}` dynamic-width format syntax (which
/// requires a dollar sign) so this module stays free of literal
/// dollar-sign artifacts — see the 2026-06-01 strip-dollars directive.
fn pad_right(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count >= width {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len() + (width - count));
    out.push_str(s);
    for _ in 0..(width - count) {
        out.push(' ');
    }
    out
}

/// Render the tokens column for one phase-run row.
///
/// In-flight rows (`completed_at IS NULL`) show `tokens=…/…` placeholders
/// — the values land at `PhaseCompleted`. Completed rows show the real
/// figures persisted by `repo::phase_runs::update_end` (G25.1 columns).
/// A `tokens_out=0` figure is honest, not a default — see the OBS-024
/// caveat in [`render`].
///
/// Per the 2026-06-01 directive ("strip dollars everywhere, keep tokens
/// everywhere") the cost column is gone — tokens stay as the spend-hint
/// signal.
fn format_metrics(run: &crate::repo::phase_runs::PhaseRunRow) -> String {
    if run.is_open() {
        // Data lands at completion — explicit placeholder, never zero.
        return "tokens=…/…".to_owned();
    }
    let tokens_in = run.tokens_in.unwrap_or(0);
    let tokens_out = run.tokens_out.unwrap_or(0);
    format!("tokens={tokens_in}/{tokens_out}")
}

/// The short verdict-outcome tag (`passing` / `redo` / `blocked` / `fail`).
fn verdict_tag(verdict: &crate::types::verdict::WorkerVerdict) -> &'static str {
    use crate::types::verdict::VerdictOutcome;
    match verdict.outcome {
        VerdictOutcome::Passing { .. } => "passing",
        VerdictOutcome::Redo { .. } => "redo",
        VerdictOutcome::Blocked { .. } => "blocked",
        VerdictOutcome::Fail { .. } => "fail",
        VerdictOutcome::Canceled => "canceled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::db::connect;
    use crate::types::ids::{PhaseRunId, TaskId};
    use crate::types::verdict::{Evidence, VerdictOutcome, WorkerVerdict};
    use chrono::{Duration, Utc};

    fn passing() -> WorkerVerdict {
        WorkerVerdict {
            synopsis: "ok".into(),
            outcome: VerdictOutcome::Passing {
                evidence: Evidence::default(),
            },
        }
    }

    /// `log` orders rows by `(started_at, phase_iteration)` and marks an
    /// in-flight row `[running]`.
    #[tokio::test]
    async fn test_l2_log_orders_deterministically_and_marks_running() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        let task = TaskId::new("T0000001a").unwrap();
        repo::specs::insert_spec(&pool, &spec, Utc::now())
            .await
            .unwrap();
        repo::spec_versions::append_version(
            &pool,
            &spec,
            1,
            &serde_json::json!({ "title": "demo" }),
            repo::VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        repo::spec_runtime::initialize(&pool, &spec, 1)
            .await
            .unwrap();
        repo::task_runtime::insert_task(&pool, &task, &spec, None)
            .await
            .unwrap();

        let t0 = Utc::now();
        // An earlier, completed run.
        let pr1 = PhaseRunId::new("P0000001a").unwrap();
        repo::phase_runs::insert_start(
            &pool,
            &pr1,
            &spec,
            Some(&task),
            "execute",
            0,
            1,
            "claude_code",
            None,
            t0,
        )
        .await
        .unwrap();
        repo::phase_runs::update_end(
            &pool,
            &pr1,
            "done",
            &passing(),
            &[],
            0,
            0,
            t0 + Duration::seconds(10),
        )
        .await
        .unwrap();
        // A later, still-open run.
        let pr2 = PhaseRunId::new("P0000002b").unwrap();
        repo::phase_runs::insert_start(
            &pool,
            &pr2,
            &spec,
            Some(&task),
            "review",
            0,
            1,
            "claude_code",
            None,
            t0 + Duration::seconds(20),
        )
        .await
        .unwrap();

        let report = render(&pool, "S0000001a").await.unwrap();
        let execute_at = report.find("execute").unwrap();
        let review_at = report.find("review").unwrap();
        assert!(execute_at < review_at, "ordered by started_at");
        assert!(report.contains("[passing]"), "the completed run's verdict");
        assert!(
            report.contains("[running]"),
            "the open run is marked running"
        );
    }

    /// `log` for an unknown spec is a loud `RepoError::NotFound`, never an
    /// empty success.
    #[tokio::test]
    async fn test_l2_log_unknown_spec_is_not_found() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let err = render(&pool, "S9999999z").await.unwrap_err();
        assert!(matches!(err, ReadError::Repo(_)), "got {err:?}");
    }

    /// OBS-024 regression: `boi log` renders the tokens column from the
    /// row's G25.1 columns. A completed row shows real figures; an
    /// in-flight row shows `tokens=…/…` placeholders — never silent
    /// zeros. (Per the 2026-06-01 directive the per-run dollar column
    /// is gone — tokens stay as the spend-hint signal.)
    #[tokio::test]
    async fn test_l2_log_renders_tokens_columns() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        let task = TaskId::new("T0000001a").unwrap();
        repo::specs::insert_spec(&pool, &spec, Utc::now())
            .await
            .unwrap();
        repo::spec_versions::append_version(
            &pool,
            &spec,
            1,
            &serde_json::json!({ "title": "demo" }),
            repo::VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        repo::spec_runtime::initialize(&pool, &spec, 1)
            .await
            .unwrap();
        repo::task_runtime::insert_task(&pool, &task, &spec, None)
            .await
            .unwrap();

        let t0 = Utc::now();
        // A completed `plan` phase with real token figures (OBS-024 live
        // shape: 1825 input, 0 output — a Goose session-record limitation
        // case, spike §Q4).
        let pr1 = PhaseRunId::new("P0000001a").unwrap();
        repo::phase_runs::insert_start(
            &pool,
            &pr1,
            &spec,
            Some(&task),
            "plan",
            0,
            1,
            "claude_code",
            None,
            t0,
        )
        .await
        .unwrap();
        repo::phase_runs::update_end(
            &pool,
            &pr1,
            "plan done",
            &passing(),
            &[],
            1_825,
            0,
            t0 + Duration::seconds(5),
        )
        .await
        .unwrap();
        // A still-running `execute` phase — placeholders, not zeros.
        let pr2 = PhaseRunId::new("P0000002b").unwrap();
        repo::phase_runs::insert_start(
            &pool,
            &pr2,
            &spec,
            Some(&task),
            "execute",
            0,
            1,
            "claude_code",
            None,
            t0 + Duration::seconds(10),
        )
        .await
        .unwrap();

        let report = render(&pool, "S0000001a").await.unwrap();

        // The completed row carries real tokens.
        assert!(
            report.contains("tokens=1825/0"),
            "completed row must show real tokens, got: {report}",
        );
        // The running row carries placeholders — never silent zeros.
        assert!(
            report.contains("tokens=…/…"),
            "running row must show placeholders, got: {report}",
        );
        // Per the 2026-06-01 strip-dollars directive no cost column
        // rides along with the tokens. The source-level guard for that
        // invariant lives in tests/log_strip_dollar_red.rs — pulling the
        // literal dollar-sign char out of this assertion lets THIS file
        // also stay free of the very character it forbids.
    }

    /// RED: `boi log` must include the spec's human-readable title near the
    /// top of the output, and every phase-run row for a task must carry the
    /// task's `ref` (or behavior-prefix, when `ref` is `None`) next to the
    /// task ID. Pins T10vhjs33 in spec Smjbkcm2d — fails today because the
    /// renderer prints only `Phase-run history for <ID>:` (no title) and the
    /// row's `scope` is the bare task_id (no ref/behavior label).
    #[tokio::test]
    async fn test_l2_log_shows_spec_title_and_task_labels() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        // One task carrying an author-supplied `ref` — the renderer must
        // show that ref verbatim next to the task ID.
        let task_with_ref = TaskId::new("T0000001a").unwrap();
        // One task with NO `ref` — the renderer must fall back to the first
        // ~30 chars of `behavior` from the spec snapshot.
        let task_no_ref = TaskId::new("T0000002b").unwrap();

        repo::specs::insert_spec(&pool, &spec, Utc::now())
            .await
            .unwrap();
        // Snapshot carries the title + per-task behaviors. The first task is
        // matched by `ref`; the second has no ref slug, so the renderer must
        // pull its label from the snapshot's `behavior` instead.
        repo::spec_versions::append_version(
            &pool,
            &spec,
            1,
            &serde_json::json!({
                "title": "demo-spec-title",
                "tasks": [
                    {
                        "ref": "setup-the-thing",
                        "behavior": "set up the thing",
                    },
                    {
                        "behavior": "an unusually-long behavior text that overflows truncation",
                    },
                ],
            }),
            repo::VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        repo::spec_runtime::initialize(&pool, &spec, 1)
            .await
            .unwrap();
        repo::task_runtime::insert_task(&pool, &task_with_ref, &spec, Some("setup-the-thing"))
            .await
            .unwrap();
        repo::task_runtime::insert_task(&pool, &task_no_ref, &spec, None)
            .await
            .unwrap();

        let t0 = Utc::now();
        // One completed phase-run per task — enough to exercise the row
        // renderer twice.
        let pr1 = PhaseRunId::new("P0000001a").unwrap();
        repo::phase_runs::insert_start(
            &pool,
            &pr1,
            &spec,
            Some(&task_with_ref),
            "execute",
            0,
            1,
            "claude_code",
            None,
            t0,
        )
        .await
        .unwrap();
        repo::phase_runs::update_end(
            &pool,
            &pr1,
            "done",
            &passing(),
            &[],
            0,
            0,
            t0 + Duration::seconds(5),
        )
        .await
        .unwrap();
        let pr2 = PhaseRunId::new("P0000002b").unwrap();
        repo::phase_runs::insert_start(
            &pool,
            &pr2,
            &spec,
            Some(&task_no_ref),
            "execute",
            0,
            1,
            "claude_code",
            None,
            t0 + Duration::seconds(10),
        )
        .await
        .unwrap();
        repo::phase_runs::update_end(
            &pool,
            &pr2,
            "done",
            &passing(),
            &[],
            0,
            0,
            t0 + Duration::seconds(15),
        )
        .await
        .unwrap();

        let report = render(&pool, "S0000001a").await.unwrap();

        // (1) The spec title must appear near the top of the output — within
        // the first three lines is "near the top" for a small report.
        let head: String = report.lines().take(3).collect::<Vec<_>>().join("\n");
        assert!(
            head.contains("demo-spec-title"),
            "spec title must appear near the top, got head: {head:?}\nfull: {report}",
        );

        // (2) The task with a `ref` must show that ref on the same line as
        // its task ID — `boi log` currently prints the bare ID, so this is
        // the RED expectation.
        let line_with_ref = report
            .lines()
            .find(|l| l.contains("T0000001a"))
            .unwrap_or_else(|| panic!("expected a row mentioning T0000001a in: {report}"));
        assert!(
            line_with_ref.contains("setup-the-thing"),
            "task with a ref must show the ref next to the task ID, \
             got line: {line_with_ref:?}\nfull: {report}",
        );

        // (3) The task with NO `ref` must fall back to the behavior-prefix.
        // We check for a stable prefix substring (truncation policy will end
        // it in `…`, but the prefix itself is the load-bearing content).
        let line_no_ref = report
            .lines()
            .find(|l| l.contains("T0000002b"))
            .unwrap_or_else(|| panic!("expected a row mentioning T0000002b in: {report}"));
        assert!(
            line_no_ref.contains("an unusually-long behavior"),
            "task without a ref must show the behavior-prefix next to the \
             task ID, got line: {line_no_ref:?}\nfull: {report}",
        );
    }

    /// A known spec with no phase runs renders the empty-history line.
    #[tokio::test]
    async fn test_l2_log_spec_with_no_runs() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        repo::specs::insert_spec(&pool, &spec, Utc::now())
            .await
            .unwrap();
        repo::spec_versions::append_version(
            &pool,
            &spec,
            1,
            &serde_json::json!({ "title": "demo" }),
            repo::VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        repo::spec_runtime::initialize(&pool, &spec, 1)
            .await
            .unwrap();
        let report = render(&pool, "S0000001a").await.unwrap();
        assert!(report.contains("no phase runs yet"), "got {report}");
    }
}
