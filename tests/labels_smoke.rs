//! Smoke test for **spec title + task ref labels** across `boi log` and the
//! non-interactive dashboard snapshot render (spec Smjbkcm2d, task Tr5qm9vad).
//!
//! The CLI surfaces that identify a spec or a task by ID must also display
//! the spec's human-readable `title` and each task's `ref` (or, when `ref` is
//! unset, the first ~30 chars of `behavior`). This smoke test pins both
//! surfaces against a fixture spec with a known title and a known task ref so
//! a future renderer refactor cannot silently drop the label.
//!
//! ## Hermeticity
//!
//! Per `me/learnings.md` (2026-05-03): the daemon-management plist and its
//! loader are off-limits to tests. This smoke test never spawns the daemon,
//! never touches the per-user agent directory, and never mutates any global
//! environment. It seeds a per-test in-memory SQLite pool and drives the
//! render functions directly — the tightest possible "per-test temp
//! BOI_HOME" the learnings demand.
//!
//! ## Lint posture
//!
//! Matches the other `tests/*.rs` test-binary crates: `.unwrap()` / `.expect()`
//! are the right loud-fail in test setup, so the crate-wide allow keeps the
//! `-D warnings` build clean.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;

use chrono::{Duration, Utc};
use sqlx::SqlitePool;

use boi::cli::dashboard::model::SortMode;
use boi::cli::dashboard::poll::{build_snapshot, fetch_spec_title};
use boi::cli::dashboard::snapshot::render_text_with_title;
use boi::cli::log;
use boi::repo;
use boi::repo::db::connect;
use boi::types::ids::{PhaseRunId, SpecId, TaskId};
use boi::types::verdict::{Evidence, VerdictOutcome, WorkerVerdict};

/// The known fixture title — load-bearing for the title-appears assertion.
const FIXTURE_TITLE: &str = "labels-smoke-fixture-title";
/// The known fixture task ref — load-bearing for the ref-appears assertion.
const FIXTURE_TASK_REF: &str = "labels-smoke-task-ref";

/// Seed an in-memory pool with a fixture spec + one task carrying a known
/// `ref` + one completed phase run. Returns `(pool, spec_id)`.
async fn seed_fixture() -> (SqlitePool, SpecId) {
    let pool = connect("sqlite::memory:").await.expect("in-memory pool");

    let spec_id = SpecId::new("S0000001a").unwrap();
    let task_id = TaskId::new("T0000001a").unwrap();
    let t0 = Utc::now();

    repo::specs::insert_spec(&pool, &spec_id, t0).await.unwrap();
    repo::spec_versions::append_version(
        &pool,
        &spec_id,
        1,
        &serde_json::json!({
            "title": FIXTURE_TITLE,
            "tasks": [
                {
                    "ref": FIXTURE_TASK_REF,
                    "behavior": "the smoke fixture's task behavior",
                },
            ],
        }),
        repo::VersionTrigger::Dispatch,
        None,
        t0,
    )
    .await
    .unwrap();
    repo::spec_runtime::initialize(&pool, &spec_id, 1)
        .await
        .unwrap();
    repo::task_runtime::insert_task(&pool, &task_id, &spec_id, Some(FIXTURE_TASK_REF))
        .await
        .unwrap();

    // One completed phase run for that task — enough for `boi log` to render a
    // row and for `build_snapshot` to surface the Task node under the spec.
    let pr = PhaseRunId::new("P0000001a").unwrap();
    repo::phase_runs::insert_start(
        &pool,
        &pr,
        &spec_id,
        Some(&task_id),
        "execute",
        0,
        1,
        "claude_code",
        None,
        t0,
    )
    .await
    .unwrap();
    let passing = WorkerVerdict {
        synopsis: "ok".into(),
        outcome: VerdictOutcome::Passing {
            evidence: Evidence::default(),
        },
    };
    repo::phase_runs::update_end(
        &pool,
        &pr,
        "done",
        &passing,
        &[],
        0,
        0,
        t0 + Duration::seconds(5),
    )
    .await
    .unwrap();

    (pool, spec_id)
}

/// `boi log <spec_id>` and the non-interactive dashboard snapshot render both
/// surface the spec title and the task ref next to their IDs.
///
/// Test name carries `labels` so `cargo test ... -- labels` filters to it.
#[tokio::test]
async fn test_l2_labels_appear_in_log_and_dashboard_snapshot() {
    let (pool, spec_id) = seed_fixture().await;

    // ── boi log ──────────────────────────────────────────────────────────────
    let log_out = log::render(&pool, spec_id.as_str())
        .await
        .expect("render log");
    assert!(
        log_out.contains(FIXTURE_TITLE),
        "boi log must include the spec title {FIXTURE_TITLE:?}; \
         output was:\n{log_out}",
    );
    assert!(
        log_out.contains(FIXTURE_TASK_REF),
        "boi log must include the task ref {FIXTURE_TASK_REF:?} next to the \
         task ID; output was:\n{log_out}",
    );

    // ── boi dashboard <spec_id> (non-interactive snapshot) ───────────────────
    // `snapshot::run` resolves `paths::boi_db_url()` from $HOME, which we cannot
    // safely mutate from a test; drive the same render path through its public
    // building blocks instead — `build_snapshot` + `fetch_spec_title` →
    // `render_text_with_title`. This is the exact pair `snapshot::run` calls.
    let missing_trace = Path::new("/nonexistent/trace.jsonl");
    let tree = build_snapshot(&pool, &spec_id, missing_trace, SortMode::Duration)
        .await
        .expect("build snapshot");
    let title = fetch_spec_title(&pool, &spec_id).await;
    let dash_out = render_text_with_title(&tree, title.as_deref());
    assert!(
        dash_out.contains(FIXTURE_TITLE),
        "dashboard snapshot must include the spec title {FIXTURE_TITLE:?} \
         next to the spec ID; output was:\n{dash_out}",
    );
    assert!(
        dash_out.contains(FIXTURE_TASK_REF),
        "dashboard snapshot must include the task ref {FIXTURE_TASK_REF:?} \
         next to the task ID; output was:\n{dash_out}",
    );
}
