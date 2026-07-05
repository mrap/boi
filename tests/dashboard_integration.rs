//! End-to-end: seed `phase_runs`, build a dashboard snapshot, assert shape.
//!
//! Seeds an in-memory SQLite with a known spec — one task, two phases (one
//! closed, one open) — then asserts that [`build_snapshot`] produces the
//! expected tree shape. Exercises `repo` + `cli::dashboard::model` +
//! `cli::dashboard::poll` together.
//!
//! ## Lint posture
//!
//! `Cargo.toml`'s `unwrap_used` / `expect_used` / `panic` are `warn` lints;
//! `clippy --all-targets -D warnings` escalates them. The `clippy.toml`
//! `allow-*-in-tests` keys exempt test-attribute bodies — so `.unwrap()` and
//! `.expect()` inside the test fns are clean. The pool-builder helper is not
//! a test fn, so it carries an explicit crate-level `allow`.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;

use chrono::{TimeZone, Utc};
use sqlx::SqlitePool;

use boi::cli::dashboard::model::{NodeKind, SortMode};
use boi::cli::dashboard::poll::{build_snapshot, build_spec_list};
use boi::repo::db::connect;
use boi::repo::{phase_runs, spec_runtime, spec_versions, specs, task_runtime};
use boi::types::ids::{PhaseRunId, SpecId, TaskId};
use boi::types::state::SpecStatus;
use boi::types::verdict::{Evidence, VerdictOutcome, WorkerVerdict};

/// Seed an in-memory pool with one spec, one task, and two phase_runs:
/// - `plan`   — closed after 60 s
/// - `implement` — still open
///
/// Returns the pool and the `SpecId` that was inserted.
async fn seeded_pool() -> (SqlitePool, SpecId) {
    let pool = connect("sqlite::memory:").await.expect("in-memory pool");

    let spec_id = SpecId::new("S0000001a").unwrap();
    let task_id = TaskId::new("T0000001a").unwrap();
    let t0 = Utc.timestamp_opt(1_700_000_000, 0).unwrap();

    // Parent rows required by phase_runs FKs.
    specs::insert_spec(&pool, &spec_id, t0).await.unwrap();
    spec_versions::append_version(
        &pool,
        &spec_id,
        1,
        &serde_json::json!({"snapshot_v": 1}),
        spec_versions::VersionTrigger::Dispatch,
        None,
        t0,
    )
    .await
    .unwrap();
    spec_runtime::initialize(&pool, &spec_id, 1).await.unwrap();
    task_runtime::insert_task(&pool, &task_id, &spec_id, None)
        .await
        .unwrap();

    // Phase 1: `plan` — closed after 60 s.
    let plan_id = PhaseRunId::new("P0000001a").unwrap();
    phase_runs::insert_start(
        &pool,
        &plan_id,
        &spec_id,
        Some(&task_id),
        "plan",
        1,
        1,
        "claude_code",
        None,
        t0,
    )
    .await
    .unwrap();
    let passing_verdict = WorkerVerdict {
        synopsis: "plan phase completed".to_string(),
        outcome: VerdictOutcome::Passing {
            evidence: Evidence {
                files_touched: vec![],
                verifications: vec![],
                summary: "ok".to_string(),
                merge_commit_sha: None,
            },
        },
    };
    phase_runs::update_end(
        &pool,
        &plan_id,
        "plan done",
        &passing_verdict,
        &[],
        0,
        0,
        t0 + chrono::Duration::seconds(60),
    )
    .await
    .unwrap();

    // Phase 2: `implement` — still open (no update_end call).
    let impl_id = PhaseRunId::new("P0000002b").unwrap();
    phase_runs::insert_start(
        &pool,
        &impl_id,
        &spec_id,
        Some(&task_id),
        "implement",
        1,
        1,
        "claude_code",
        None,
        t0 + chrono::Duration::seconds(60),
    )
    .await
    .unwrap();

    (pool, spec_id)
}

/// `build_snapshot` over a seeded pool produces a spec node whose tree shape
/// reflects exactly the inserted rows: one task with two phases, one closed
/// and one open.
#[tokio::test]
async fn test_l1_snapshot_reflects_seeded_phase_runs() {
    let (pool, spec_id) = seeded_pool().await;
    let missing_trace = Path::new("/nonexistent/trace.jsonl");

    let tree = build_snapshot(&pool, &spec_id, missing_trace, SortMode::Duration)
        .await
        .unwrap();

    assert_eq!(tree.kind, NodeKind::Spec);
    assert_eq!(
        tree.status, "running",
        "one open phase => spec status must be 'running'"
    );
    assert_eq!(tree.children.len(), 1, "exactly one task");

    let task = &tree.children[0];
    assert_eq!(task.kind, NodeKind::Task);
    assert_eq!(task.label, "T0000001a");
    assert_eq!(task.status, "active", "task with an open phase is 'active'");
    assert_eq!(task.children.len(), 2, "two phases");

    // With SortMode::Duration the longer-running phase comes first.
    // `plan` ran 60 s (closed). `implement` is open so its duration is wall
    // clock from 60 s ago — always > 60 s at test time.  Either way, both
    // phases must appear.
    let phase_labels: Vec<&str> = task.children.iter().map(|p| p.label.as_str()).collect();
    assert!(
        phase_labels.iter().any(|l| l.starts_with("plan")),
        "expected a 'plan' phase, got {phase_labels:?}"
    );
    assert!(
        phase_labels.iter().any(|l| l.starts_with("implement")),
        "expected an 'implement' phase, got {phase_labels:?}"
    );

    // The closed `plan` phase must show `done`; the open `implement` must show
    // `active`.
    let plan_node = task
        .children
        .iter()
        .find(|p| p.label.starts_with("plan"))
        .unwrap();
    let impl_node = task
        .children
        .iter()
        .find(|p| p.label.starts_with("implement"))
        .unwrap();
    assert_eq!(plan_node.status, "done");
    assert_eq!(impl_node.status, "active");
    assert!(
        impl_node.completed_at.is_none(),
        "open phase has no completed_at"
    );
}

/// Helper: seed a minimal spec (with no tasks or phase runs) and set its
/// status. Returns the seeded `SpecId`.
async fn seed_spec(
    pool: &SqlitePool,
    id: &str,
    status: SpecStatus,
    t0: chrono::DateTime<Utc>,
) -> SpecId {
    let spec_id = SpecId::new(id).unwrap();
    specs::insert_spec(pool, &spec_id, t0).await.unwrap();
    spec_versions::append_version(
        pool,
        &spec_id,
        1,
        &serde_json::json!({"snapshot_v": 1}),
        spec_versions::VersionTrigger::Dispatch,
        None,
        t0,
    )
    .await
    .unwrap();
    spec_runtime::initialize(pool, &spec_id, 1).await.unwrap();
    spec_runtime::update_status(pool, &spec_id, SpecStatus::Running, None, t0)
        .await
        .unwrap();
    if !matches!(status, SpecStatus::Running) {
        spec_runtime::update_status(
            pool,
            &spec_id,
            status,
            None,
            t0 + chrono::Duration::seconds(120),
        )
        .await
        .unwrap();
    }
    spec_id
}

/// Helper: insert a closed phase run for a spec (no task).
///
/// (Per the 2026-06-01 strip-$ directive the per-run dollar column is
/// gone — the rollup the picker reads is now phase-count only.)
async fn seed_phase_run(
    pool: &SqlitePool,
    phase_id: &str,
    spec_id: &SpecId,
    t0: chrono::DateTime<Utc>,
) {
    let pid = PhaseRunId::new(phase_id).unwrap();
    phase_runs::insert_start(
        pool,
        &pid,
        spec_id,
        None,
        "plan",
        1,
        1,
        "claude_code",
        None,
        t0,
    )
    .await
    .unwrap();
    let verdict = WorkerVerdict {
        synopsis: "done".to_string(),
        outcome: VerdictOutcome::Passing {
            evidence: Evidence {
                files_touched: vec![],
                verifications: vec![],
                summary: "ok".to_string(),
                merge_commit_sha: None,
            },
        },
    };
    phase_runs::update_end(
        pool,
        &pid,
        "done",
        &verdict,
        &[],
        0,
        0,
        t0 + chrono::Duration::seconds(30),
    )
    .await
    .unwrap();
}

/// `build_spec_list` returns the running spec first, and phase counts are
/// correctly populated from the seeded `phase_runs` rows. (Per the
/// 2026-06-01 strip-$ directive the per-spec dollar total is gone — the
/// phase count is now the sole spend-hint signal on the rollup.)
#[tokio::test]
async fn test_l1_picker_lists_specs_running_first() {
    let pool = connect("sqlite::memory:").await.expect("in-memory pool");
    let t0 = Utc.timestamp_opt(1_700_000_000, 0).unwrap();

    // Spec A: completed, seeded before spec B — two phase runs.
    let spec_a = seed_spec(&pool, "S0000003c", SpecStatus::Completed, t0).await;
    seed_phase_run(&pool, "P0000003c", &spec_a, t0).await;
    seed_phase_run(
        &pool,
        "P0000004d",
        &spec_a,
        t0 + chrono::Duration::seconds(35),
    )
    .await;

    // Spec B: running, seeded after spec A — one phase run.
    let t1 = t0 + chrono::Duration::seconds(200);
    let spec_b = seed_spec(&pool, "S0000005e", SpecStatus::Running, t1).await;
    seed_phase_run(&pool, "P0000005e", &spec_b, t1).await;

    let list = build_spec_list(&pool).await.unwrap();

    assert!(!list.is_empty(), "should have at least two specs");

    // Running spec must sort first.
    assert_eq!(
        list[0].spec_id,
        "S0000005e",
        "running spec must be first; got {:?}",
        list.iter().map(|s| s.spec_id.as_str()).collect::<Vec<_>>()
    );
    assert_eq!(list[0].status, "running");
    assert_eq!(list[0].phase_count, 1, "running spec has 1 phase run");

    // Completed spec is next.
    let completed = list.iter().find(|s| s.spec_id == "S0000003c").unwrap();
    assert_eq!(completed.status, "completed");
    assert_eq!(completed.phase_count, 2, "completed spec has 2 phase runs");
}
