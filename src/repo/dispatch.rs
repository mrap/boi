//! The `boi dispatch` structural insert — `specs` + `spec_versions` +
//! `spec_runtime` + `task_runtime` + `task_deps` in ONE transaction.
//!
//! ## Why one transaction (review S1)
//!
//! `boi dispatch` persists five tables' worth of a brand-new spec. If any
//! insert fails — a duplicate id, an FK violation, a bad dep ref — the whole
//! spec must roll back: a half-inserted spec (a `specs` row with no
//! `spec_runtime`) breaks every later status read. The atomic boundary is
//! inherently a repo concern, so the multi-table transaction lives here
//! (mirroring [`crate::repo::clean`]'s cascade transaction), not in the
//! `cli/dispatch` orchestration.
//!
//! The five inserts run in FK-dependency order: `specs` →
//! `spec_versions` (the `spec_runtime.current_version` FK target) →
//! `spec_runtime` → `task_runtime` → `task_deps` (FK to two `task_runtime`
//! rows).
//!
//! The spec lands in `spec_runtime` as `queued`; the daemon's `Dispatch`
//! command emits `SpecStarted` to move it to `running`.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::SqlitePool;

use crate::repo::db::RepoError;
use crate::repo::spec_versions::SNAPSHOT_VERSION;
use crate::types::ids::{SpecId, TaskId};
use crate::types::state::{SpecStatus, TaskState};

/// One task to insert — its minted id plus the optional author-ref slug.
#[derive(Debug, Clone)]
pub struct DispatchTask {
    /// The freshly-minted task id.
    pub task_id: TaskId,
    /// The author's `ref` slug, if the spec gave one.
    pub task_ref: Option<String>,
}

/// One dependency edge to insert — `task_id` depends on `depends_on`.
///
/// Both endpoints are *minted* [`TaskId`]s (the `boi dispatch` path resolves
/// each authored `blocked_by` ref to its minted id before calling this).
#[derive(Debug, Clone)]
pub struct DispatchDep {
    /// The dependent task.
    pub task_id: TaskId,
    /// The task it depends on.
    pub depends_on: TaskId,
}

/// The complete structural payload of one `boi dispatch`.
#[derive(Debug, Clone)]
pub struct DispatchRows {
    /// The minted spec id.
    pub spec_id: SpecId,
    /// The v1 `spec_versions` snapshot JSON — the G21.3-shaped object
    /// `{ spec_contract, task_contracts }` (plus the injected `snapshot_v`).
    pub snapshot: Value,
    /// Every task.
    pub tasks: Vec<DispatchTask>,
    /// Every dependency edge.
    pub deps: Vec<DispatchDep>,
}

/// Insert a dispatched spec's five tables in one transaction.
///
/// The spec is version 1, status `queued`, every task `not_started`. On any
/// failure the transaction rolls back and a typed [`RepoError`] is returned —
/// never a partially-inserted spec.
pub async fn insert_dispatch(
    pool: &SqlitePool,
    rows: &DispatchRows,
    now: DateTime<Utc>,
) -> Result<(), RepoError> {
    match insert_dispatch_inner(pool, rows, now).await {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::warn!(
                spec_id = %rows.spec_id,
                error = ?e,
                "dispatch insert failed — transaction rolled back, no rows persisted",
            );
            Err(e)
        }
    }
}

/// The transaction body — separated so [`insert_dispatch`] wraps every exit in
/// one `warn!` (mirrors `clean.rs`'s `_inner` split).
async fn insert_dispatch_inner(
    pool: &SqlitePool,
    rows: &DispatchRows,
    now: DateTime<Utc>,
) -> Result<(), RepoError> {
    let sid = rows.spec_id.as_str();
    let mut tx = pool.begin().await?;

    // 1. specs — the immutable identity row.
    let res = sqlx::query!(
        "INSERT INTO specs (spec_id, created_at) VALUES (?1, ?2)",
        sid,
        now,
    )
    .execute(&mut *tx)
    .await;
    if let Err(e) = res {
        return Err(unique_or_sqlx(e, format!("spec {sid} already exists")));
    }

    // 2. spec_versions — version 1, the dispatch snapshot. Ensure the
    // self-describing `snapshot_v` key is present (mirrors `append_version`).
    let mut snapshot = rows.snapshot.clone();
    if let Value::Object(map) = &mut snapshot {
        map.entry("snapshot_v")
            .or_insert_with(|| Value::from(SNAPSHOT_VERSION));
    }
    let version: i64 = 1;
    let trigger = "dispatch";
    let res = sqlx::query!(
        "INSERT INTO spec_versions (spec_id, version, snapshot, trigger, trigger_meta, created_at) \
         VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
        sid,
        version,
        snapshot,
        trigger,
        now,
    )
    .execute(&mut *tx)
    .await;
    if let Err(e) = res {
        return Err(unique_or_sqlx(
            e,
            format!("spec {sid} version 1 already exists"),
        ));
    }

    // 3. spec_runtime — queued, pointing at version 1 (the FK target above).
    let queued = SpecStatus::Queued.as_str();
    let res = sqlx::query!(
        "INSERT INTO spec_runtime (spec_id, current_version, status) VALUES (?1, ?2, ?3)",
        sid,
        version,
        queued,
    )
    .execute(&mut *tx)
    .await;
    if let Err(e) = res {
        return Err(unique_or_sqlx(
            e,
            format!("spec_runtime for {sid} already exists"),
        ));
    }

    // 4. task_runtime — one not_started row per task.
    let not_started = TaskState::NotStarted.as_str();
    for task in &rows.tasks {
        let tid = task.task_id.as_str();
        let task_ref = task.task_ref.as_deref();
        let res = sqlx::query!(
            "INSERT INTO task_runtime (task_id, spec_id, ref, state) VALUES (?1, ?2, ?3, ?4)",
            tid,
            sid,
            task_ref,
            not_started,
        )
        .execute(&mut *tx)
        .await;
        if let Err(e) = res {
            return Err(unique_or_sqlx(
                e,
                format!("task_runtime for {tid} already exists"),
            ));
        }
    }

    // 5. task_deps — the dependency edges (FK to two `task_runtime` rows above).
    for dep in &rows.deps {
        let tid = dep.task_id.as_str();
        let depends_on = dep.depends_on.as_str();
        let res = sqlx::query!(
            "INSERT INTO task_deps (task_id, depends_on) VALUES (?1, ?2)",
            tid,
            depends_on,
        )
        .execute(&mut *tx)
        .await;
        if let Err(e) = res {
            return Err(unique_or_sqlx(
                e,
                format!("task_deps edge {tid} → {depends_on} already exists"),
            ));
        }
    }

    tx.commit().await?;
    Ok(())
}

/// Map a `sqlx` error: a UNIQUE violation → [`RepoError::Duplicate`] with the
/// given message; anything else → [`RepoError::Sqlx`].
fn unique_or_sqlx(e: sqlx::Error, dup_msg: String) -> RepoError {
    match e {
        sqlx::Error::Database(db) if db.is_unique_violation() => RepoError::Duplicate(dup_msg),
        other => RepoError::Sqlx(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::db::connect;

    fn rows() -> DispatchRows {
        DispatchRows {
            spec_id: SpecId::new("S0000001a").unwrap(),
            snapshot: serde_json::json!({
                "spec_contract": { "title": "demo" },
                "task_contracts": {},
            }),
            tasks: vec![
                DispatchTask {
                    task_id: TaskId::new("T0000001a").unwrap(),
                    task_ref: Some("setup".to_owned()),
                },
                DispatchTask {
                    task_id: TaskId::new("T0000002b").unwrap(),
                    task_ref: None,
                },
            ],
            deps: vec![DispatchDep {
                task_id: TaskId::new("T0000002b").unwrap(),
                depends_on: TaskId::new("T0000001a").unwrap(),
            }],
        }
    }

    /// `insert_dispatch` persists all five tables atomically.
    #[tokio::test]
    async fn test_l2_insert_dispatch_persists_all_tables() {
        let pool = connect("sqlite::memory:").await.unwrap();
        insert_dispatch(&pool, &rows(), Utc::now()).await.unwrap();

        // The spec exists, queued, version 1.
        let runtime = crate::repo::spec_runtime::fetch(&pool, &SpecId::new("S0000001a").unwrap())
            .await
            .unwrap();
        assert_eq!(runtime.status, "queued");
        assert_eq!(runtime.current_version, 1);
        // Both tasks exist.
        let tasks =
            crate::repo::task_runtime::tasks_for_spec(&pool, &SpecId::new("S0000001a").unwrap())
                .await
                .unwrap();
        assert_eq!(tasks.len(), 2, "both tasks inserted");
        // The dependency edge exists.
        let deps = crate::repo::task_deps::deps_of(&pool, &TaskId::new("T0000002b").unwrap())
            .await
            .unwrap();
        assert_eq!(deps.len(), 1, "the dependency edge inserted");
        // The snapshot got a `snapshot_v`.
        let snap = crate::repo::spec_versions::fetch_snapshot(
            &pool,
            &SpecId::new("S0000001a").unwrap(),
            1,
        )
        .await
        .unwrap();
        assert!(snap.get("snapshot_v").is_some(), "snapshot_v injected");
    }

    /// A duplicate dispatch is a loud `RepoError::Duplicate` and rolls back —
    /// the second call leaves no partial rows.
    #[tokio::test]
    async fn test_l2_duplicate_dispatch_rolls_back() {
        let pool = connect("sqlite::memory:").await.unwrap();
        insert_dispatch(&pool, &rows(), Utc::now()).await.unwrap();
        let err = insert_dispatch(&pool, &rows(), Utc::now())
            .await
            .unwrap_err();
        assert!(matches!(err, RepoError::Duplicate(_)), "got {err:?}");
    }

    /// A dep edge naming a task that is not in the payload is an FK violation
    /// — the whole transaction rolls back (no spec persisted).
    #[tokio::test]
    async fn test_l2_dangling_dep_rolls_back_everything() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let mut bad = rows();
        bad.deps = vec![DispatchDep {
            task_id: TaskId::new("T0000001a").unwrap(),
            depends_on: TaskId::new("T9999999z").unwrap(), // not in `tasks`
        }];
        let err = insert_dispatch(&pool, &bad, Utc::now()).await.unwrap_err();
        assert!(
            matches!(err, RepoError::Sqlx(_)),
            "FK violation, got {err:?}"
        );
        // The spec must NOT exist — the whole transaction rolled back.
        assert!(
            !crate::repo::specs::exists(&pool, &SpecId::new("S0000001a").unwrap())
                .await
                .unwrap(),
            "a dangling dep rolls back the entire dispatch",
        );
    }
}
