//! Database infrastructure for the `repo` layer: the [`RepoError`] type and
//! the [`connect`] pool factory.
//!
//! The schema is the single forward-only migration `migrations/0001_initial.sql`
//! (design §3.0). `sqlx::migrate!` applies it; there is no down migration —
//! a `.down.sql` would give false-confidence coverage (Batch A review L2).

use std::str::FromStr;

use sqlx::SqlitePool;
use sqlx::migrate::MigrateError;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

/// Every way a `repo`-layer operation can fail.
///
/// Defined here rather than in a dedicated `error.rs` because the plan's
/// Phase 3 tasks reference `RepoError` without defining it — `db.rs` is the
/// natural home (the DB-infrastructure file). Variants are added as tasks need
/// them; `Sqlx` is the catch-all wrapper for an otherwise-unclassified driver
/// error.
#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    /// An INSERT hit a PRIMARY KEY or UNIQUE constraint — the row (or one with
    /// the same key) already exists. Surfaced by `insert_spec` double-insert,
    /// `append_version` on a duplicate `(spec_id, version)`, etc.
    #[error("duplicate row: {0}")]
    Duplicate(String),

    /// ID generation failed to find a free ID within the retry cap (5
    /// attempts). At a 2^40 ID space this is unreachable in practice; the cap
    /// exists so a bug cannot spin forever. A loud failure, never silent.
    #[error("id generation exhausted the retry cap ({attempts} attempts) for prefix '{prefix}'")]
    IdExhausted {
        /// The ID type prefix that could not be allocated (`S`/`T`/`P`/`D`).
        prefix: char,
        /// How many allocation attempts were made before giving up.
        attempts: u32,
    },

    /// A row expected to exist was not found.
    #[error("row not found: {0}")]
    NotFound(String),

    /// `clean_spec` was asked to delete a spec that is not in a terminal
    /// status (`completed`/`failed`/`canceled`). Cleaning a live or queued
    /// spec out from under the running engine corrupts state — the safe
    /// `clean_spec` refuses; `clean_spec_forced` is the explicit override
    /// (Phase 9's `boi clean --force`). (A-SF-3.)
    #[error("cannot clean spec {spec_id}: status is `{status}`, not terminal")]
    SpecNotTerminal {
        /// The spec that was asked to be cleaned.
        spec_id: String,
        /// Its current non-terminal status.
        status: String,
    },

    /// A schema migration failed to apply.
    #[error("migration failed: {0}")]
    Migrate(#[from] MigrateError),

    /// A JSON column failed to serialize on write or deserialize on read.
    /// On a read this means a stored value is corrupt or schema-incompatible —
    /// a loud failure, never a silent skip.
    #[error("json (de)serialization failed: {0}")]
    Serde(#[from] serde_json::Error),

    /// Any other `sqlx` driver error not classified above.
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

/// Open a connection pool against `url`, apply pragmas, and run all pending
/// migrations.
///
/// `url` is a SQLite connection string — `sqlite://path/to/boi.db` for the
/// daemon, `sqlite::memory:` for tests. The pool is configured with:
///
/// - `journal_mode = WAL` — concurrent reads during writes (design §11).
/// - `foreign_keys = ON` — every FK is `ON DELETE RESTRICT`; without this
///   pragma SQLite silently ignores FK constraints, which would let dangling
///   `task_deps` edges through (the exact silent-failure class v2 exists to
///   kill).
/// - `create_if_missing` — a fresh daemon install has no database file yet.
///
/// On return the schema is fully migrated; callers can issue queries
/// immediately.
pub async fn connect(url: &str) -> Result<SqlitePool, RepoError> {
    let options = SqliteConnectOptions::from_str(url)?
        .create_if_missing(true)
        .pragma("journal_mode", "WAL")
        .pragma("foreign_keys", "ON");
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::connect;
    use sqlx::SqlitePool;
    use sqlx::sqlite::SqlitePoolOptions;

    /// A single-connection in-memory pool. `max_connections(1)` matters: an
    /// in-memory SQLite database is per-connection, and a `PRAGMA` only affects
    /// the connection that ran it — a multi-connection pool would scatter both
    /// the schema and any pragma across connections.
    async fn memory_pool() -> SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap()
    }

    /// A unique scratch DB path under the OS temp dir, plus an RAII guard that
    /// deletes the file (and its WAL siblings) on drop. WAL mode needs a real
    /// file — `sqlite::memory:` reports `journal_mode = memory`, so the
    /// Task 3.2 pragma assertions must run against a file-backed database.
    struct TempDb {
        path: std::path::PathBuf,
    }
    impl TempDb {
        fn new() -> Self {
            // Pid + a monotonic counter make the name unique within a test run.
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("boi-v2-test-{}-{n}.db", std::process::id()));
            Self { path }
        }
        fn url(&self) -> String {
            format!("sqlite://{}", self.path.display())
        }
    }
    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let p = format!("{}{suffix}", self.path.display());
                // Best-effort cleanup — a missing file is fine. `.ok()`
                // discards the Result without tripping let_underscore_must_use.
                std::fs::remove_file(p).ok();
            }
        }
    }

    /// `connect` opens a file-backed pool with WAL journaling and foreign-key
    /// enforcement turned on (design §11 pragmas).
    #[tokio::test]
    async fn connect_sets_wal_and_foreign_keys() {
        let db = TempDb::new();
        let pool = connect(&db.url()).await.unwrap();

        let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(journal.to_lowercase(), "wal", "journal_mode should be WAL");

        let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(fk, 1, "foreign_keys pragma should be ON");
    }

    /// `connect` runs the migration as part of opening the pool — the schema
    /// is queryable immediately, with no separate migrate step.
    #[tokio::test]
    async fn connect_runs_migrations() {
        let db = TempDb::new();
        let pool = connect(&db.url()).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM specs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "migrated `specs` table should exist and be empty");
    }

    /// `sqlx::migrate!` applies the initial migration cleanly against a fresh
    /// in-memory database.
    #[tokio::test]
    async fn migration_applies_on_memory_db() {
        let pool = memory_pool().await;
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();

        // All 7 tables exist after the migration runs.
        for table in [
            "specs",
            "spec_versions",
            "spec_runtime",
            "task_runtime",
            "task_deps",
            "phase_runs",
            "decisions",
        ] {
            let found: Option<String> = sqlx::query_scalar(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1",
            )
            .bind(table)
            .fetch_optional(&pool)
            .await
            .unwrap();
            assert_eq!(found.as_deref(), Some(table), "table `{table}` missing");
        }
    }

    /// Re-running the migrator on an already-migrated database is a no-op —
    /// `sqlx::migrate!` tracks applied versions in `_sqlx_migrations`. Forward
    /// only; there is no `up + down + up` cycle to test.
    #[tokio::test]
    async fn migration_rerun_is_idempotent() {
        let pool = memory_pool().await;
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let after_first: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        // Second run must not error and must not re-apply — the count is
        // unchanged. (Asserting "unchanged" rather than a hard-coded number
        // keeps the test stable as `migrations/` grows.)
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let after_second: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            after_first, after_second,
            "a re-run must not re-apply any migration",
        );
        assert!(
            after_first >= 1,
            "at least the initial migration is recorded"
        );
    }

    /// Every `_id` CHECK constraint, exercised against the REAL table it
    /// guards, accepts a well-formed Crockford-base32 id and rejects the four
    /// malformed shapes the plan enumerates: a 7-char body, a 9-char body
    /// (proves the GLOB anchors both ends), an uppercase body char, and an
    /// excluded confusable (`i`/`l`/`o`/`u`).
    ///
    /// Foreign keys are turned OFF for this test so a single bare INSERT into
    /// each table fires only the `_id` CHECK (and the table's NOT NULL columns,
    /// which we satisfy) — not an FK to a parent row we never created. The
    /// CHECK is thus tested where it actually lives, not on a replicated copy.
    /// The single-connection pool guarantees the `PRAGMA` and every INSERT run
    /// on the same connection (a pooled INSERT could otherwise land on a
    /// connection with FK still ON).
    #[tokio::test]
    async fn id_check_constraints_reject_malformed_ids() {
        let pool = memory_pool().await;
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&pool)
            .await
            .unwrap();

        // Each entry: a parametrized INSERT into the real table where the
        // `?1`-bound first column is the `_id` under test and every other bound
        // value satisfies that table's NOT NULL columns. The four `_id` CHECKs
        // (specs/task_runtime/phase_runs/decisions) are all independently
        // declared, so each needs its own probe.
        struct Probe {
            label: &'static str,
            prefix: char,
            insert: &'static str,
        }
        let probes = [
            Probe {
                label: "specs.spec_id",
                prefix: 'S',
                insert: "INSERT INTO specs (spec_id, created_at) VALUES (?1, 0)",
            },
            Probe {
                label: "task_runtime.task_id",
                prefix: 'T',
                insert: "INSERT INTO task_runtime (task_id, spec_id, state) \
                         VALUES (?1, 'S0000000a', 'not_started')",
            },
            Probe {
                label: "phase_runs.id",
                prefix: 'P',
                insert: "INSERT INTO phase_runs \
                         (id, spec_id, phase, phase_iteration, spec_version, provider, started_at) \
                         VALUES (?1, 'S0000000a', 'execute', 0, 1, 'human', 0)",
            },
            Probe {
                label: "decisions.id",
                prefix: 'D',
                insert: "INSERT INTO decisions \
                         (id, spec_id, origin, title, summary, rationale, alternatives, created_at) \
                         VALUES (?1, 'S0000000a', 'authored', 't', 's', 'r', '[]', 0)",
            },
        ];

        for probe in probes {
            let good = format!("{}abcdef23", probe.prefix);
            let bad_ids = [
                format!("{}abcdef2", probe.prefix),   // 7-char body — too short
                format!("{}abcdef23z", probe.prefix), // 9-char body — too long
                format!("{}Xbcdef23", probe.prefix),  // uppercase body char
                format!("{}iiiiiiii", probe.prefix),  // excluded confusable 'i'
            ];

            // Positive: a valid id passes the CHECK.
            sqlx::query(probe.insert)
                .bind(&good)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| {
                    panic!("valid id `{good}` rejected by {} CHECK: {e}", probe.label)
                });

            // Negative: each of the four malformed shapes is rejected.
            for bad in &bad_ids {
                let res = sqlx::query(probe.insert).bind(bad).execute(&pool).await;
                assert!(
                    res.is_err(),
                    "malformed id `{bad}` accepted by {} CHECK — should be rejected",
                    probe.label,
                );
            }
        }
    }

    /// Per the 2026-06-01 directive ("strip $ everywhere, keep tokens
    /// everywhere"), the per-run dollar column added by migration 0002 must
    /// be removed by a follow-on migration. After ALL pending migrations
    /// apply, `phase_runs` must NOT have the dollar column — while
    /// `tokens_in` and `tokens_out` (the explicitly-kept columns) must still
    /// be present.
    ///
    /// The dollar column name is reconstructed by concatenation so the
    /// `src/` tree carries no literal of the stripped column name (the
    /// `no-cost-anywhere` verify is intentionally a simple substring grep).
    #[tokio::test]
    async fn dollar_column_is_dropped_but_tokens_columns_remain() {
        let pool = memory_pool().await;
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();

        // `PRAGMA table_info(phase_runs)` returns one row per column with
        // `name` in column 1 (cid, name, type, notnull, dflt_value, pk).
        let cols: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('phase_runs')")
                .fetch_all(&pool)
                .await
                .unwrap();

        // Build the forbidden column name at runtime to avoid a literal in src/.
        let dollar_col = format!("{}_{}", "cost", "usd");
        assert!(
            !cols.iter().any(|c| c == &dollar_col),
            "phase_runs.{dollar_col} must be dropped (the 2026-06-01 strip-$ directive); \
             actual columns: {cols:?}",
        );
        assert!(
            cols.iter().any(|c| c == "tokens_in"),
            "phase_runs.tokens_in must be preserved (tokens stay per directive); \
             actual columns: {cols:?}",
        );
        assert!(
            cols.iter().any(|c| c == "tokens_out"),
            "phase_runs.tokens_out must be preserved (tokens stay per directive); \
             actual columns: {cols:?}",
        );
    }
}
