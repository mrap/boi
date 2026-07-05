//! The `spec_versions` table — immutable, append-only spec snapshots.
//!
//! Every author-level change to a spec appends a new `(spec_id, version)` row
//! carrying a full TOML snapshot as JSON. Rows are `INSERT`-only — never
//! `UPDATE`d or `DELETE`d (except by `boi clean`). `spec_versions.snapshot` is
//! the one piece of state NOT replayable from OTel, so it is real data
//! (design §11).
//!
//! Each snapshot carries a top-level `snapshot_v` integer for cross-BOI-version
//! replay compatibility (B11); [`append_version`] injects `snapshot_v` =
//! [`SNAPSHOT_VERSION`] if the caller has not already set it.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::SqlitePool;

use crate::repo::db::RepoError;
use crate::types::ids::SpecId;

/// Current snapshot schema version, stamped into every snapshot's top-level
/// `snapshot_v` field (B11). Bump when the snapshot JSON shape changes.
pub const SNAPSHOT_VERSION: i64 = 1;

/// What caused a new spec version to be appended.
///
/// Serializes to the `spec_versions.trigger` TEXT column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionTrigger {
    /// The initial version, written at `boi dispatch`.
    Dispatch,
    /// A later version, written when a plan revision rewrote the spec.
    PlanRevised,
}

impl VersionTrigger {
    /// Stable lowercase string form for the `trigger` column.
    fn as_str(self) -> &'static str {
        match self {
            VersionTrigger::Dispatch => "dispatch",
            VersionTrigger::PlanRevised => "plan_revised",
        }
    }
}

/// Append a new immutable spec version.
///
/// `snapshot` is the full spec serialized as JSON. If it is a JSON object
/// without a `snapshot_v` key, [`SNAPSHOT_VERSION`] is injected so every stored
/// snapshot is self-describing (B11). A duplicate `(spec_id, version)` hits the
/// composite PRIMARY KEY and returns [`RepoError::Duplicate`].
pub async fn append_version(
    pool: &SqlitePool,
    spec_id: &SpecId,
    version: i64,
    snapshot: &Value,
    trigger: VersionTrigger,
    trigger_meta: Option<Value>,
    now: DateTime<Utc>,
) -> Result<(), RepoError> {
    // Ensure the snapshot is self-describing. Inject only when absent so a
    // caller that deliberately sets a different `snapshot_v` is respected.
    let mut snapshot = snapshot.clone();
    if let Value::Object(map) = &mut snapshot {
        map.entry("snapshot_v")
            .or_insert_with(|| Value::from(SNAPSHOT_VERSION));
    }

    let id = spec_id.as_str();
    let trigger_str = trigger.as_str();
    let res = sqlx::query!(
        "INSERT INTO spec_versions (spec_id, version, snapshot, trigger, trigger_meta, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        id,
        version,
        snapshot,
        trigger_str,
        trigger_meta,
        now,
    )
    .execute(pool)
    .await;
    match res {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(RepoError::Duplicate(
            format!("spec {spec_id} version {version} already exists"),
        )),
        Err(e) => Err(RepoError::Sqlx(e)),
    }
}

/// Fetch the snapshot JSON for a `(spec_id, version)` pair.
///
/// Returns [`RepoError::NotFound`] if no such version row exists.
pub async fn fetch_snapshot(
    pool: &SqlitePool,
    spec_id: &SpecId,
    version: i64,
) -> Result<Value, RepoError> {
    let id = spec_id.as_str();
    let row = sqlx::query!(
        "SELECT snapshot AS \"snapshot: Value\" FROM spec_versions \
         WHERE spec_id = ?1 AND version = ?2",
        id,
        version,
    )
    .fetch_optional(pool)
    .await?;
    match row {
        Some(r) => Ok(r.snapshot),
        None => Err(RepoError::NotFound(format!(
            "spec {spec_id} version {version}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::db::connect;
    use crate::repo::specs::insert_spec;
    use serde_json::json;

    async fn seeded_pool() -> (SqlitePool, SpecId) {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        insert_spec(&pool, &spec, Utc::now()).await.unwrap();
        (pool, spec)
    }

    /// append + fetch round-trips the snapshot, and `snapshot_v` is stamped in.
    #[tokio::test]
    async fn append_then_fetch_roundtrips() {
        let (pool, spec) = seeded_pool().await;
        let snapshot = json!({ "title": "demo", "tasks": [] });

        append_version(
            &pool,
            &spec,
            1,
            &snapshot,
            VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();

        let fetched = fetch_snapshot(&pool, &spec, 1).await.unwrap();
        assert_eq!(fetched["title"], "demo");
        // append_version injected snapshot_v.
        assert_eq!(fetched["snapshot_v"], json!(SNAPSHOT_VERSION));
    }

    /// A caller-supplied `snapshot_v` is preserved, not overwritten.
    #[tokio::test]
    async fn explicit_snapshot_v_is_preserved() {
        let (pool, spec) = seeded_pool().await;
        let snapshot = json!({ "title": "demo", "snapshot_v": 99 });
        append_version(
            &pool,
            &spec,
            1,
            &snapshot,
            VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        let fetched = fetch_snapshot(&pool, &spec, 1).await.unwrap();
        assert_eq!(fetched["snapshot_v"], json!(99));
    }

    /// `(spec_id, version)` is UNIQUE — re-appending the same version is a
    /// loud `RepoError::Duplicate`.
    #[tokio::test]
    async fn duplicate_version_raises_duplicate() {
        let (pool, spec) = seeded_pool().await;
        let snapshot = json!({ "title": "demo" });
        append_version(
            &pool,
            &spec,
            1,
            &snapshot,
            VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();

        let err = append_version(
            &pool,
            &spec,
            1,
            &snapshot,
            VersionTrigger::PlanRevised,
            None,
            Utc::now(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, RepoError::Duplicate(_)),
            "expected Duplicate, got {err:?}",
        );
    }

    /// `spec_versions` is treated append-only — the module exposes only
    /// `append_version` / `fetch_snapshot`, no UPDATE path. This test pins the
    /// observable consequence: once a version is appended, the only way to
    /// "change" it is to append a *new* version; the original row is immutable
    /// and a re-fetch always sees the originally-appended snapshot.
    #[tokio::test]
    async fn versions_are_append_only() {
        let (pool, spec) = seeded_pool().await;
        append_version(
            &pool,
            &spec,
            1,
            &json!({ "title": "v1" }),
            VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        // A revision appends version 2 — it does NOT mutate version 1.
        append_version(
            &pool,
            &spec,
            2,
            &json!({ "title": "v2" }),
            VersionTrigger::PlanRevised,
            None,
            Utc::now(),
        )
        .await
        .unwrap();

        // Both versions coexist; version 1's snapshot is exactly as appended.
        assert_eq!(
            fetch_snapshot(&pool, &spec, 1).await.unwrap()["title"],
            "v1"
        );
        assert_eq!(
            fetch_snapshot(&pool, &spec, 2).await.unwrap()["title"],
            "v2"
        );
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM spec_versions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(total, 2, "append-only — every version is a distinct row");
    }

    /// `fetch_snapshot` on a missing version is `RepoError::NotFound`.
    #[tokio::test]
    async fn fetch_missing_version_is_not_found() {
        let (pool, spec) = seeded_pool().await;
        let err = fetch_snapshot(&pool, &spec, 7).await.unwrap_err();
        assert!(
            matches!(err, RepoError::NotFound(_)),
            "expected NotFound, got {err:?}",
        );
    }
}
