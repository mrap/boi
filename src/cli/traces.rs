//! `boi traces query <SQL>` + `boi failures top` — the OTel-trace query CLI
//! (design §8 / §9).
//!
//! Read-only — no daemon; the queries read the OTel JSONL traces (and the
//! read-only-`ATTACH`ed `boi.db`) directly through bundled DuckDB.
//!
//! ## The `duckdb` feature split (review D9 / Phase 8c)
//!
//! The `traces` / `failures` `clap` variants are **always present** in the CLI
//! tree (see `cli::mod`). Only the *handlers* here are feature-split:
//!
//! - with `feature = "duckdb"` — the real bodies, calling `runtime::open_duckdb`
//!   / `query` / `failures_top` inside `tokio::task::spawn_blocking` (the
//!   `duckdb` crate is synchronous — Phase 8c review S2);
//! - without it — a loud "built without the duckdb feature" stub that exits
//!   non-zero (via [`TracesError::NoDuckdb`]).
//!
//! A CI job builds `--no-default-features`, so the gate cannot rot untested.

use crate::cli::paths::PathError;

/// A `boi traces` / `boi failures` command failed.
#[derive(Debug, thiserror::Error)]
pub enum TracesError {
    /// The binary was built without the `duckdb` feature, so the trace-query
    /// surface is unavailable.
    #[error(
        "this `boi` was built without the `duckdb` feature — \
         rebuild with `--features duckdb` to query traces"
    )]
    NoDuckdb,
    /// The `~/.boi/v2/` path layout could not be resolved.
    #[error(transparent)]
    Path(#[from] PathError),
    /// `--last` could not be parsed as a duration.
    #[error("invalid --last duration `{got}`: {detail}")]
    BadDuration {
        /// The unparseable duration string.
        got: String,
        /// The parser's message.
        detail: String,
    },
    /// A DuckDB query failed.
    #[error("{0}")]
    Duck(String),
    /// The blocking DuckDB task panicked or was cancelled.
    #[error("trace query task failed: {0}")]
    Join(String),
}

/// `boi traces query <SQL>` — run a read-only SQL query over the OTel traces.
#[cfg(feature = "duckdb")]
pub async fn query(sql: &str) -> Result<(), TracesError> {
    use crate::cli::paths;
    use crate::runtime::{open_duckdb, query as duck_query};

    let boi_db = paths::boi_db()?;
    let traces_glob = paths::traces_glob()?;
    let sql = sql.to_owned();

    // The `duckdb` crate is synchronous — run it off the async runtime
    // (Phase 8c review S2). A fresh `DuckHandle` per query (it is `!Sync`).
    let result = tokio::task::spawn_blocking(move || {
        let handle = open_duckdb(&boi_db, &traces_glob).map_err(|e| e.to_string())?;
        duck_query(&handle, &sql).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| TracesError::Join(e.to_string()))?
    .map_err(TracesError::Duck)?;

    print_table(&result.columns, &result.rows);
    Ok(())
}

/// `boi traces query` — the no-`duckdb` stub: a loud failure, non-zero exit.
#[cfg(not(feature = "duckdb"))]
pub async fn query(_sql: &str) -> Result<(), TracesError> {
    Err(TracesError::NoDuckdb)
}

/// `boi failures top [--last Nd] [--n N]` — the top-N recurring failure
/// fingerprints (§9 — the v1.0 observability anchor).
#[cfg(feature = "duckdb")]
pub async fn failures_top(last: Option<&str>, n: Option<u32>) -> Result<(), TracesError> {
    use crate::cli::paths;
    use crate::runtime::{failures_top as duck_failures_top, open_duckdb};

    let window_days = parse_window_days(last)?;
    let n = n.unwrap_or(10);
    let boi_db = paths::boi_db()?;
    let traces_glob = paths::traces_glob()?;

    let rows = tokio::task::spawn_blocking(move || {
        let handle = open_duckdb(&boi_db, &traces_glob).map_err(|e| e.to_string())?;
        duck_failures_top(&handle, window_days, n).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| TracesError::Join(e.to_string()))?
    .map_err(TracesError::Duck)?;

    // The §9 table.
    println!(
        "{:<32} {:>6}  {:<22} PHASE",
        "FINGERPRINT", "COUNT", "LAST SEEN"
    );
    for row in &rows {
        println!(
            "{:<32} {:>6}  {:<22} {}",
            row.fingerprint, row.count, row.last_seen, row.phase,
        );
    }
    Ok(())
}

/// `boi failures top` — the no-`duckdb` stub: a loud failure, non-zero exit.
#[cfg(not(feature = "duckdb"))]
pub async fn failures_top(_last: Option<&str>, _n: Option<u32>) -> Result<(), TracesError> {
    Err(TracesError::NoDuckdb)
}

/// Parse a `--last` window into a whole-day count for `failures_top`.
///
/// `None` → 7 days (the §9 default). A non-day-granular or malformed value is
/// a loud [`TracesError::BadDuration`].
#[cfg(feature = "duckdb")]
fn parse_window_days(last: Option<&str>) -> Result<u32, TracesError> {
    match last {
        None => Ok(7),
        Some(s) => {
            let dur = humantime::parse_duration(s).map_err(|e| TracesError::BadDuration {
                got: s.to_owned(),
                detail: e.to_string(),
            })?;
            let days = dur.as_secs() / 86_400;
            u32::try_from(days.max(1)).map_err(|_| TracesError::BadDuration {
                got: s.to_owned(),
                detail: "window too large".to_owned(),
            })
        }
    }
}

/// Print a [`QueryResult`](crate::runtime::QueryResult)-shaped table to stdout.
#[cfg(feature = "duckdb")]
fn print_table(columns: &[String], rows: &[Vec<String>]) {
    if columns.is_empty() {
        println!("(no columns)");
        return;
    }
    println!("{}", columns.join(" | "));
    for row in rows {
        println!("{}", row.join(" | "));
    }
    println!("({} row(s))", rows.len());
}

#[cfg(all(test, feature = "duckdb"))]
mod tests {
    use super::*;

    /// `parse_window_days` defaults to 7 and rejects garbage loudly.
    #[test]
    fn test_l1_parse_window_days_default_and_rejection() {
        assert_eq!(parse_window_days(None).unwrap(), 7, "default window");
        assert_eq!(parse_window_days(Some("30d")).unwrap(), 30);
        let err = parse_window_days(Some("garbage")).unwrap_err();
        assert!(
            matches!(err, TracesError::BadDuration { .. }),
            "garbage is a loud BadDuration, got {err:?}",
        );
    }
}

#[cfg(all(test, not(feature = "duckdb")))]
mod no_duckdb_tests {
    use super::*;

    /// Without the `duckdb` feature, `traces query` is a loud `NoDuckdb` —
    /// never a silent success.
    #[tokio::test]
    async fn test_l2_traces_query_without_duckdb_is_loud() {
        let err = query("SELECT 1").await.unwrap_err();
        assert!(matches!(err, TracesError::NoDuckdb), "got {err:?}");
    }

    /// Without the `duckdb` feature, `failures top` is a loud `NoDuckdb`.
    #[tokio::test]
    async fn test_l2_failures_top_without_duckdb_is_loud() {
        let err = failures_top(None, None).await.unwrap_err();
        assert!(matches!(err, TracesError::NoDuckdb), "got {err:?}");
    }
}
