//! The `task_deps` table — the task-dependency DAG (Batch A review L2).
//!
//! v1 stored a task's blockers in an un-validated `blocked_by_task_ids` JSON
//! array; a plan revision could write an ID that named no real task. v2
//! promotes the edges to a real table with a foreign key on BOTH columns, so a
//! dangling dependency is *impossible* — an [`add_dep`] naming an absent task
//! is rejected by the FK.
//!
//! Phase 5a's scheduler reads [`deps_of`] / [`dependents_of`] (indexed lookups)
//! instead of JSON-parsing every tick; Phase 5b's plan revision mutates the
//! DAG via [`add_dep`] / [`remove_dep`].
//!
//! FK enforcement requires `PRAGMA foreign_keys = ON` — [`crate::repo::connect`]
//! sets it. `INSERT OR IGNORE` is deliberately NOT used: it would swallow the
//! FK rejection too, turning a dangling-dep bug into a silent no-op.

use sqlx::SqlitePool;

use crate::repo::db::RepoError;
use crate::types::ids::TaskId;

/// Add a dependency edge: `task_id` depends on `depends_on`.
///
/// Idempotent on a duplicate edge — re-adding the same `(task_id, depends_on)`
/// pair is a no-op `Ok(())` (the edge set is unchanged). But a *dangling* edge
/// — either endpoint naming a task with no `task_runtime` row — is rejected by
/// the foreign key and surfaces loudly as [`RepoError::Sqlx`]. The two are
/// distinguished by the database error kind, never conflated.
pub async fn add_dep(
    pool: &SqlitePool,
    task_id: &TaskId,
    depends_on: &TaskId,
) -> Result<(), RepoError> {
    let tid = task_id.as_str();
    let dep = depends_on.as_str();
    let res = sqlx::query!(
        "INSERT INTO task_deps (task_id, depends_on) VALUES (?1, ?2)",
        tid,
        dep,
    )
    .execute(pool)
    .await;
    match res {
        Ok(_) => Ok(()),
        // Duplicate edge — the DAG is unchanged, treat as idempotent success.
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Ok(()),
        // FK violation (dangling endpoint) or anything else — loud failure.
        Err(e) => Err(RepoError::Sqlx(e)),
    }
}

/// Remove a dependency edge.
///
/// Idempotent — removing an edge that does not exist is a no-op `Ok(())`.
pub async fn remove_dep(
    pool: &SqlitePool,
    task_id: &TaskId,
    depends_on: &TaskId,
) -> Result<(), RepoError> {
    let tid = task_id.as_str();
    let dep = depends_on.as_str();
    sqlx::query!(
        "DELETE FROM task_deps WHERE task_id = ?1 AND depends_on = ?2",
        tid,
        dep,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The tasks `task_id` depends on (its blockers).
pub async fn deps_of(pool: &SqlitePool, task_id: &TaskId) -> Result<Vec<TaskId>, RepoError> {
    let tid = task_id.as_str();
    let rows = sqlx::query_scalar!(
        "SELECT depends_on FROM task_deps WHERE task_id = ?1 ORDER BY depends_on",
        tid,
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(parse_task_id).collect()
}

/// The tasks that depend on `task_id` (its dependents).
pub async fn dependents_of(pool: &SqlitePool, task_id: &TaskId) -> Result<Vec<TaskId>, RepoError> {
    let tid = task_id.as_str();
    let rows = sqlx::query_scalar!(
        "SELECT task_id FROM task_deps WHERE depends_on = ?1 ORDER BY task_id",
        tid,
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(parse_task_id).collect()
}

/// Parse a stored task-id string into a [`TaskId`].
///
/// A stored value that fails validation means DB corruption — surfaced loudly
/// as [`RepoError::NotFound`], never silently dropped.
fn parse_task_id(s: String) -> Result<TaskId, RepoError> {
    TaskId::new(&s).map_err(|e| RepoError::NotFound(format!("corrupt task id in task_deps: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::db::connect;
    use crate::repo::specs::insert_spec;
    use crate::repo::task_runtime::insert_task;
    use crate::types::ids::SpecId;
    use chrono::Utc;

    /// A pool with a spec and three tasks A/B/C (no edges yet).
    async fn seeded_pool() -> (SqlitePool, TaskId, TaskId, TaskId) {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        insert_spec(&pool, &spec, Utc::now()).await.unwrap();
        let a = TaskId::new("T000000aa").unwrap();
        let b = TaskId::new("T000000bb").unwrap();
        let c = TaskId::new("T000000cc").unwrap();
        for t in [&a, &b, &c] {
            insert_task(&pool, t, &spec, None).await.unwrap();
        }
        (pool, a, b, c)
    }

    /// `add_dep` then `deps_of` / `dependents_of` round-trip the edge.
    #[tokio::test]
    async fn add_dep_roundtrips() {
        let (pool, a, b, c) = seeded_pool().await;
        // A depends on B and C.
        add_dep(&pool, &a, &b).await.unwrap();
        add_dep(&pool, &a, &c).await.unwrap();

        let deps = deps_of(&pool, &a).await.unwrap();
        assert_eq!(deps, vec![b.clone(), c.clone()]); // ORDER BY depends_on

        // B and C each see A as a dependent.
        assert_eq!(dependents_of(&pool, &b).await.unwrap(), vec![a.clone()]);
        assert_eq!(dependents_of(&pool, &c).await.unwrap(), vec![a.clone()]);
        // A itself has no dependents.
        assert!(dependents_of(&pool, &a).await.unwrap().is_empty());
    }

    /// An edge whose `depends_on` names no real task is rejected by the FK —
    /// a dangling dependency cannot be written (vs. v1's silent JSON array).
    #[tokio::test]
    async fn dangling_dep_rejected_by_fk() {
        let (pool, a, _b, _c) = seeded_pool().await;
        let ghost = TaskId::new("T000000zz").unwrap(); // never inserted
        let err = add_dep(&pool, &a, &ghost).await.unwrap_err();
        assert!(
            matches!(err, RepoError::Sqlx(_)),
            "dangling dep must be a loud FK error, got {err:?}",
        );
        // The edge was not written.
        assert!(deps_of(&pool, &a).await.unwrap().is_empty());
    }

    /// `add_dep` is idempotent — re-adding the same edge is a silent success
    /// and does not duplicate the row.
    #[tokio::test]
    async fn add_dep_is_idempotent() {
        let (pool, a, b, _c) = seeded_pool().await;
        add_dep(&pool, &a, &b).await.unwrap();
        add_dep(&pool, &a, &b).await.unwrap(); // re-add — no error
        assert_eq!(deps_of(&pool, &a).await.unwrap(), vec![b]);
    }

    /// `remove_dep` is idempotent — removing a present edge then removing it
    /// again both succeed.
    #[tokio::test]
    async fn remove_dep_is_idempotent() {
        let (pool, a, b, _c) = seeded_pool().await;
        add_dep(&pool, &a, &b).await.unwrap();

        remove_dep(&pool, &a, &b).await.unwrap();
        assert!(deps_of(&pool, &a).await.unwrap().is_empty());
        // Removing the already-absent edge is still Ok.
        remove_dep(&pool, &a, &b).await.unwrap();
    }
}
