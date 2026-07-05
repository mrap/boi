//! The `specs` table — immutable spec identity.
//!
//! `specs` holds one row per spec: the `spec_id` PK and `created_at`. It is
//! the FK anchor for every other table and is never pruned by `boi clean`
//! (audit identity, design §11).
//!
//! [`insert_spec`] persists an externally-allocated `SpecId`; the production
//! dispatch path more often uses [`crate::repo::ids::allocate_spec_id`], which
//! generates the ID and inserts in one collision-retried step. The two
//! coexist: `allocate_spec_id` is generate+insert, `insert_spec` is
//! insert-a-given-id.

use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use crate::repo::db::RepoError;
use crate::types::ids::SpecId;

/// Insert a spec's immutable identity row.
///
/// `spec_id` is an already-validated [`SpecId`]. A second insert of the same
/// `spec_id` hits the PRIMARY KEY constraint and returns
/// [`RepoError::Duplicate`] — never a silent overwrite.
pub async fn insert_spec(
    pool: &SqlitePool,
    spec_id: &SpecId,
    now: DateTime<Utc>,
) -> Result<(), RepoError> {
    let id = spec_id.as_str();
    let res = sqlx::query!(
        "INSERT INTO specs (spec_id, created_at) VALUES (?1, ?2)",
        id,
        now
    )
    .execute(pool)
    .await;
    match res {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(RepoError::Duplicate(
            format!("spec {spec_id} already exists"),
        )),
        Err(e) => Err(RepoError::Sqlx(e)),
    }
}

/// Whether a spec identity row exists.
///
/// Probes the `spec_id` column directly rather than `SELECT 1` — a bare
/// integer literal has no column type, which the compile-time `query_scalar!`
/// macro cannot map.
pub async fn exists(pool: &SqlitePool, spec_id: &SpecId) -> Result<bool, RepoError> {
    let id = spec_id.as_str();
    let row = sqlx::query_scalar!("SELECT spec_id FROM specs WHERE spec_id = ?1", id)
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::db::connect;

    fn spec(id: &str) -> SpecId {
        SpecId::new(id).unwrap()
    }

    /// Insert then `exists` round-trips; an un-inserted spec does not exist.
    #[tokio::test]
    async fn insert_then_exists() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let id = spec("S0000001a");

        assert!(!exists(&pool, &id).await.unwrap(), "absent before insert");
        insert_spec(&pool, &id, Utc::now()).await.unwrap();
        assert!(exists(&pool, &id).await.unwrap(), "present after insert");

        // A different spec is still absent.
        assert!(!exists(&pool, &spec("S0000002b")).await.unwrap());
    }

    /// A double insert of the same spec_id is a loud `RepoError::Duplicate`,
    /// not a silent no-op or overwrite.
    #[tokio::test]
    async fn double_insert_raises_duplicate() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let id = spec("S0000001a");
        insert_spec(&pool, &id, Utc::now()).await.unwrap();

        let err = insert_spec(&pool, &id, Utc::now()).await.unwrap_err();
        assert!(
            matches!(err, RepoError::Duplicate(_)),
            "expected Duplicate, got {err:?}",
        );
    }
}
