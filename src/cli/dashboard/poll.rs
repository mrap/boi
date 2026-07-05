//! The ~500 ms poll: rebuild the `DashNode` tree from SQLite + the trace file.
//!
//! Read-only. Opens nothing the daemon needs. The interactive loop (Task 12)
//! calls [`build_snapshot`] on a `tokio::time::interval`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use sqlx::SqlitePool;

use crate::cli::dashboard::model::{self, DashNode, SortMode};
use crate::cli::dashboard::picker::{self, SpecSummary};
use crate::cli::dashboard::trace;
use crate::cli::paths;
use crate::cli::read_error::ReadError;
use crate::repo;
use crate::repo::phase_runs;
use crate::repo::spec_runtime;
use crate::repo::spec_versions;
use crate::repo::task_runtime;
use crate::types::ids::SpecId;

/// The poll interval — design §7 (~500 ms is imperceptibly live).
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Open a read-only SQLite pool at `~/.boi/v2/boi.db`.
///
/// The `mode=rwc` URL is safe for read-only callers — SQLite tolerates it on
/// an existing file and the dashboard never writes.
pub async fn open_pool() -> Result<SqlitePool, ReadError> {
    let db_url = paths::boi_db_url()?;
    let pool = repo::connect(&db_url).await?;
    Ok(pool)
}

/// Resolve the trace JSONL path for a spec.
///
/// Traces are written to `~/.boi/v2/traces/{date}/{trace_id}.jsonl`.  Because
/// the dashboard does not know the trace id up-front (it is embedded in the
/// spans), this function scans every date subdirectory under
/// `~/.boi/v2/traces/` and returns the most-recently-modified `.jsonl` file
/// whose stem matches the spec_id, or — when no spec-id match is found — the
/// single most-recently-modified `.jsonl` file overall.  A missing traces
/// directory is not an error; the caller degrades to an empty leaf log.
pub fn trace_path_for(spec_id: &SpecId) -> Result<PathBuf, ReadError> {
    let traces = paths::traces_dir()?;
    // Collect every `.jsonl` file with its modification time.
    let mut candidates: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    if let Ok(date_dirs) = std::fs::read_dir(&traces) {
        for date_entry in date_dirs.flatten() {
            if let Ok(files) = std::fs::read_dir(date_entry.path()) {
                for file_entry in files.flatten() {
                    let path = file_entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                        let mtime = file_entry
                            .metadata()
                            .and_then(|m| m.modified())
                            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                        candidates.push((path, mtime));
                    }
                }
            }
        }
    }
    // Prefer any file whose stem contains the spec_id.
    let sid = spec_id.as_str();
    let spec_match = candidates
        .iter()
        .filter(|(p, _)| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.contains(sid))
                .unwrap_or(false)
        })
        .max_by_key(|(_, t)| *t)
        .map(|(p, _)| p.clone());

    if let Some(path) = spec_match {
        return Ok(path);
    }

    // Fall back to the most-recently-modified `.jsonl` overall.
    let fallback = candidates
        .into_iter()
        .max_by_key(|(_, t)| *t)
        .map(|(p, _)| p);

    // If no trace files exist at all, return a path that does not exist; the
    // caller (`build_snapshot`) degrades silently via `unwrap_or_default`.
    Ok(fallback.unwrap_or_else(|| traces.join("no-trace.jsonl")))
}

#[cfg(test)]
mod trace_path_tests {
    use super::*;
    use crate::types::ids::SpecId;

    /// When the traces directory does not exist the function still returns a
    /// path (to a non-existent file) without panicking or returning an error.
    #[test]
    fn trace_path_for_missing_dir_returns_ok() {
        // We cannot control `$HOME` cleanly in a unit test, but we can verify
        // the function does not panic and returns `Ok`.
        let spec_id = SpecId::new("S0000001a").unwrap();
        let result = trace_path_for(&spec_id);
        assert!(result.is_ok(), "must not error when traces dir is absent");
    }
}

/// Build the ordered spec-picker list — `spec_runtime::all` joined with the
/// grouped `phase_runs` cost/count rollup. The Picker-mode poll calls this.
///
/// Each summary is enriched with the spec's title (sourced from
/// `spec_versions.snapshot.title` at the spec's `current_version`) so the
/// renderer can show the title next to the ID. A spec whose snapshot is
/// missing or omits a title keeps `title = None` — the renderer falls back
/// to ID-only rendering.
pub async fn build_spec_list(pool: &SqlitePool) -> Result<Vec<SpecSummary>, ReadError> {
    let specs = spec_runtime::all(pool).await?;
    let rollups = phase_runs::cost_and_count_by_spec(pool).await?;
    let mut summaries = picker::build_spec_list(&specs, &rollups);

    // Fetch the title for each spec that landed in the picker — best-effort:
    // a missing snapshot leaves `title = None`, which the renderer tolerates.
    let mut titles: HashMap<String, String> = HashMap::new();
    for s in &specs {
        let Ok(sid) = SpecId::new(&s.spec_id) else {
            continue;
        };
        if let Ok(snapshot) = spec_versions::fetch_snapshot(pool, &sid, s.current_version).await {
            if let Some(t) = snapshot
                .get("title")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
            {
                titles.insert(s.spec_id.clone(), t);
            }
        }
    }
    picker::enrich_with_titles(&mut summaries, &titles);
    Ok(summaries)
}

/// Build one fresh, sorted, event-merged tree snapshot for `spec_id`.
///
/// `trace_path` is the spec's trace JSONL; a missing file degrades the leaf
/// log to empty (loud-but-non-fatal — SO S6) while the structural tree from
/// SQLite still renders.
///
/// Task nodes are also enriched with their author-supplied `ref` (from
/// `task_runtime.ref`) and `behavior` (from the spec snapshot at the spec's
/// `current_version`) so the renderer can show a human-readable label next
/// to each task ID.
pub async fn build_snapshot(
    pool: &SqlitePool,
    spec_id: &SpecId,
    trace_path: &Path,
    sort: SortMode,
) -> Result<DashNode, ReadError> {
    let rows = phase_runs::fetch_history_for_spec(pool, spec_id).await?;
    let mut tree = model::build_tree(spec_id.as_str(), &rows);

    let now = Utc::now();
    let jsonl = std::fs::read_to_string(trace_path).unwrap_or_default();
    let events = trace::parse_trace(&jsonl);
    model::merge_events(&mut tree, &events, now);

    // Annotate task nodes with their ref + behavior so the renderer can show
    // a label next to each task ID. Best-effort: a missing task_runtime row
    // or snapshot just leaves the fields as None, and the renderer omits the
    // label for that task.
    let task_meta = fetch_task_meta(pool, spec_id).await;
    attach_task_meta(&mut tree, &task_meta);

    // Override task/spec status with the authoritative blocked signal from
    // task_runtime — the phase_runs-derived tree cannot see a blocked task and
    // would render a wedged spec as "all done" (2026-06-11). Gated on the real
    // spec status: only a `running` spec is overridden, so a terminal spec that
    // retains a blocked task row is not mislabeled `[blocked]`. A failed/absent
    // status fetch defaults to `Running` (apply) — surfacing a live wedge is
    // the feature's whole point; the terminal-mislabel edge is the lesser risk.
    let task_states = fetch_task_states(pool, spec_id).await;
    let spec_status = fetch_spec_status(pool, spec_id).await;
    model::apply_task_states(&mut tree, &task_states, spec_status);

    model::sort_tree(&mut tree, sort, now);
    Ok(tree)
}

/// Fetch the spec's human-readable title from the snapshot at its current
/// version. Returns `None` if the snapshot is missing or omits a title — the
/// renderer treats that as ID-only.
pub async fn fetch_spec_title(pool: &SqlitePool, spec_id: &SpecId) -> Option<String> {
    let runtime = spec_runtime::fetch(pool, spec_id).await.ok()?;
    let snapshot = spec_versions::fetch_snapshot(pool, spec_id, runtime.current_version)
        .await
        .ok()?;
    snapshot
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

/// Look up `(ref, behavior)` per `task_id` for one spec.
async fn fetch_task_meta(
    pool: &SqlitePool,
    spec_id: &SpecId,
) -> HashMap<String, (Option<String>, Option<String>)> {
    let mut out: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    let rows = match task_runtime::tasks_for_spec(pool, spec_id).await {
        Ok(rs) => rs,
        Err(_) => return out,
    };
    // First, seed the map from task_runtime — gives us the ref slug.
    for r in &rows {
        out.insert(r.task_id.clone(), (r.r#ref.clone(), None));
    }
    // Then, layer in the behavior from the spec snapshot's `tasks` array,
    // matched by `ref`. Tasks without a ref in the snapshot simply don't get
    // a behavior — the renderer shows only the ID for them.
    if let Ok(Some(current_version)) = spec_runtime::fetch(pool, spec_id)
        .await
        .map(|s| Some(s.current_version))
    {
        if let Ok(snapshot) = spec_versions::fetch_snapshot(pool, spec_id, current_version).await {
            if let Some(tasks) = snapshot.get("tasks").and_then(|v| v.as_array()) {
                // Build a ref → behavior map from the snapshot.
                let snap_by_ref: HashMap<String, String> = tasks
                    .iter()
                    .filter_map(|t| {
                        let r = t.get("ref")?.as_str()?.to_owned();
                        let b = t.get("behavior")?.as_str()?.to_owned();
                        Some((r, b))
                    })
                    .collect();
                for (_, (r#ref, behavior)) in out.iter_mut() {
                    if let Some(slug) = r#ref.as_deref() {
                        if let Some(b) = snap_by_ref.get(slug) {
                            *behavior = Some(b.clone());
                        }
                    }
                }
            }
        }
    }
    out
}

/// Look up `task_id` → `task_runtime.state` (raw string) for one spec.
///
/// Best-effort: a failed query yields an empty map, leaving the structural
/// statuses untouched. `model::apply_task_states` compares the raw value
/// against `TaskState::Blocked.as_str()`. A query error is logged LOUDLY (S6):
/// an empty map silently disables blocked-surfacing — a wedged spec would then
/// render as `done`, the exact failure this feature exists to fix.
async fn fetch_task_states(pool: &SqlitePool, spec_id: &SpecId) -> HashMap<String, String> {
    match task_runtime::tasks_for_spec(pool, spec_id).await {
        Ok(rows) => rows.into_iter().map(|r| (r.task_id, r.state)).collect(),
        Err(e) => {
            tracing::warn!(
                spec_id = %spec_id, error = %e,
                "dashboard: could not fetch task states — blocked-surfacing disabled this poll",
            );
            HashMap::new()
        }
    }
}

/// The authoritative `spec_runtime.status` for `spec_id`, parsed. Defaults to
/// `SpecStatus::Running` when the row is missing or the status is unparseable —
/// surfacing a live wedge (the feature's purpose) outweighs the terminal-spec
/// mislabel edge — logging the fallback loudly (S6).
async fn fetch_spec_status(pool: &SqlitePool, spec_id: &SpecId) -> crate::types::state::SpecStatus {
    use crate::types::state::SpecStatus;
    match spec_runtime::fetch(pool, spec_id).await {
        Ok(row) => row.status.parse().unwrap_or_else(|_| {
            tracing::warn!(
                spec_id = %spec_id, status = %row.status,
                "dashboard: unparseable spec status — assuming `running` for blocked-surfacing",
            );
            SpecStatus::Running
        }),
        Err(e) => {
            tracing::warn!(
                spec_id = %spec_id, error = %e,
                "dashboard: could not fetch spec status — assuming `running` for blocked-surfacing",
            );
            SpecStatus::Running
        }
    }
}

/// Walk `tree` and write the matching `(task_ref, behavior)` into every
/// `Task` node whose `label` (= task id) appears in `meta`.
fn attach_task_meta(tree: &mut DashNode, meta: &HashMap<String, (Option<String>, Option<String>)>) {
    if tree.kind == model::NodeKind::Task {
        if let Some((r, b)) = meta.get(&tree.label) {
            tree.task_ref = r.clone();
            tree.behavior = b.clone();
        }
    }
    for child in &mut tree.children {
        attach_task_meta(child, meta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::db::connect;

    /// Apply the embedded migrations to a fresh in-memory pool.
    async fn memory_pool() -> SqlitePool {
        connect("sqlite::memory:").await.unwrap()
    }

    #[tokio::test]
    async fn build_snapshot_of_an_empty_spec_yields_a_bare_spec_node() {
        let pool = memory_pool().await;
        // No phase_runs rows inserted; fetch returns empty.
        let spec_id = SpecId::new("S0000001a").unwrap();
        let missing = Path::new("/nonexistent/trace.jsonl");
        let tree = build_snapshot(&pool, &spec_id, missing, SortMode::Waterfall)
            .await
            .unwrap();
        assert_eq!(tree.label, "S0000001a");
        assert!(tree.children.is_empty(), "no rows => no children");
    }

    /// A task in the `blocked` state must surface as `blocked` at the spec
    /// root, overriding the `running` the open phase would otherwise yield.
    /// Without the `task_runtime` join the dashboard renders a wedged spec as
    /// `running`/`done` (the 2026-06-11 incident).
    #[tokio::test]
    async fn build_snapshot_reports_blocked_when_a_task_is_blocked() {
        use crate::repo::spec_versions::VersionTrigger;
        use crate::repo::{phase_runs, spec_runtime, specs, task_runtime};
        use crate::types::ids::TaskId;
        use crate::types::state::TaskState;

        let pool = memory_pool().await;
        let spec = SpecId::new("S0000001a").unwrap();
        let task = TaskId::new("T0000001a").unwrap();
        specs::insert_spec(&pool, &spec, Utc::now()).await.unwrap();
        spec_versions::append_version(
            &pool,
            &spec,
            1,
            &serde_json::json!({ "title": "demo" }),
            VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        spec_runtime::initialize(&pool, &spec, 1).await.unwrap();
        // The spec must be `running` for the blocked override to apply (a
        // terminal spec keeps its structural status).
        spec_runtime::update_status(
            &pool,
            &spec,
            crate::types::state::SpecStatus::Running,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        task_runtime::insert_task(&pool, &task, &spec, None)
            .await
            .unwrap();
        // An open phase ⇒ build_tree alone would report the spec `running`.
        phase_runs::allocate_phase_run_id(
            &pool,
            &spec,
            Some(&task),
            "implement",
            1,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        // Block the task.
        task_runtime::update_state(&pool, &task, TaskState::Blocked, None, None, Utc::now())
            .await
            .unwrap();

        let missing = Path::new("/nonexistent/trace.jsonl");
        let tree = build_snapshot(&pool, &spec, missing, SortMode::Waterfall)
            .await
            .unwrap();
        assert_eq!(
            tree.status, "blocked",
            "a blocked task must surface as blocked at the spec, not running"
        );
    }

    /// Regression: a spec with zero phase_runs is `queued`, NOT `done`.
    /// The previous `any_open ? running : done` derivation reported vacuous
    /// `[done]` for newly-dispatched specs, racing every E2E waiter. Caught
    /// 2026-05-24 by the OpenRouter Docker E2E after the queued-spec
    /// status arm landed.
    #[tokio::test]
    async fn build_snapshot_of_a_freshly_queued_spec_reports_queued_not_done() {
        let pool = memory_pool().await;
        let spec_id = SpecId::new("S0000001a").unwrap();
        let missing = Path::new("/nonexistent/trace.jsonl");
        let tree = build_snapshot(&pool, &spec_id, missing, SortMode::Waterfall)
            .await
            .unwrap();
        assert_eq!(
            tree.status, "queued",
            "zero phase_runs ⇒ queued (vacuous-truth bug)"
        );
    }
}
