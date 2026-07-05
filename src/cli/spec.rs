//! `boi spec show <spec_id> [--version N]` — print a stored `spec_versions`
//! snapshot.
//!
//! Read-only — SQLite-direct, no daemon. The default is the spec's *latest*
//! version (`spec_runtime.current_version`); `--version N` prints version `N`,
//! and an out-of-range `N` is a loud [`ReadError::NoSuchVersion`].

use sqlx::SqlitePool;

use crate::cli::paths;
use crate::cli::read_error::ReadError;
use crate::repo;
use crate::repo::db::RepoError;
use crate::types::ids::SpecId;

/// Run `boi spec show`.
pub async fn show(spec_id: &str, version: Option<i64>) -> Result<(), ReadError> {
    let db_url = paths::boi_db_url()?;
    let pool = repo::connect(&db_url).await?;
    let snapshot = render(&pool, spec_id, version).await?;
    println!("{snapshot}");
    Ok(())
}

/// Fetch + pretty-print the requested `spec_versions` snapshot.
///
/// Factored out of [`show`] so the L2 test drives it against an in-memory pool.
pub async fn render(
    pool: &SqlitePool,
    spec_id: &str,
    version: Option<i64>,
) -> Result<String, ReadError> {
    let sid = SpecId::new(spec_id).map_err(|e| ReadError::BadId(e.to_string()))?;
    let runtime = repo::spec_runtime::fetch(pool, &sid).await?;
    let target = version.unwrap_or(runtime.current_version);

    let snapshot = match repo::spec_versions::fetch_snapshot(pool, &sid, target).await {
        Ok(snap) => snap,
        // An absent version is a precise, loud error — not a generic
        // "not found" (`fetch_snapshot` returns `NotFound` for a missing
        // `(spec, version)` pair).
        Err(RepoError::NotFound(_)) => {
            return Err(ReadError::NoSuchVersion {
                spec_id: spec_id.to_owned(),
                version: target,
            });
        }
        Err(other) => return Err(ReadError::Repo(other)),
    };

    serde_json::to_string_pretty(&snapshot).map_err(|e| {
        ReadError::Repo(RepoError::NotFound(format!(
            "snapshot for {spec_id} v{target} could not be rendered: {e}"
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::db::connect;
    use chrono::Utc;

    /// Seed a spec with two `spec_versions` rows.
    async fn seed_two_versions() -> SqlitePool {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        repo::specs::insert_spec(&pool, &spec, Utc::now())
            .await
            .unwrap();
        repo::spec_versions::append_version(
            &pool,
            &spec,
            1,
            &serde_json::json!({ "title": "v1 title" }),
            repo::VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        repo::spec_versions::append_version(
            &pool,
            &spec,
            2,
            &serde_json::json!({ "title": "v2 title" }),
            repo::VersionTrigger::PlanRevised,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        // current_version → 2.
        repo::spec_runtime::initialize(&pool, &spec, 2)
            .await
            .unwrap();
        pool
    }

    /// `spec show` with no `--version` prints the latest snapshot.
    #[tokio::test]
    async fn test_l2_spec_show_prints_latest_snapshot() {
        let pool = seed_two_versions().await;
        let out = render(&pool, "S0000001a", None).await.unwrap();
        assert!(out.contains("v2 title"), "latest snapshot, got:\n{out}");
    }

    /// `spec show --version 1` prints that specific snapshot.
    #[tokio::test]
    async fn test_l2_spec_show_prints_named_version() {
        let pool = seed_two_versions().await;
        let out = render(&pool, "S0000001a", Some(1)).await.unwrap();
        assert!(out.contains("v1 title"), "v1 snapshot, got:\n{out}");
    }

    /// `spec show --version 99` is a loud `NoSuchVersion`.
    #[tokio::test]
    async fn test_l2_spec_show_out_of_range_version_is_loud() {
        let pool = seed_two_versions().await;
        let err = render(&pool, "S0000001a", Some(99)).await.unwrap_err();
        assert!(
            matches!(err, ReadError::NoSuchVersion { version: 99, .. }),
            "got {err:?}",
        );
    }
}
