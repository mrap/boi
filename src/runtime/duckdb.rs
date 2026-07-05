//! The bundled-DuckDB query layer (Phase 8c).
//!
//! BOI keeps two telemetry stores: the SQLite `boi.db` (the harness's
//! source-of-truth state) and the OTel trace JSONL under
//! `~/.boi/v2/traces/{date}/{trace_id}.jsonl` (canonical OTLP/JSON, written by
//! [`super::otel_export`]). Neither is convenient to query ad hoc. This module
//! is the read side: an in-process [DuckDB](https://duckdb.org) connection that
//!
//! 1. **`ATTACH`es `boi.db` read-only** — `boi.db`'s tables become queryable as
//!    `boi_db.<table>` without DuckDB ever being able to mutate harness state;
//!    and
//! 2. **makes the OTel JSONL queryable** via `read_otlp_traces` — the `otlp`
//!    DuckDB community extension, which parses canonical OTLP/JSON.
//!
//! [`open_duckdb`] builds that connection; [`query`] runs arbitrary read-only
//! SQL for the Phase 9 `boi traces query <SQL>` CLI; [`failures_top`] runs the
//! fixed §8/§9 failure-fingerprint aggregation behind `boi failures top`.
//!
//! ## Feature-gated (`duckdb`)
//!
//! Bundled DuckDB compiles ~3 MB of C++ and adds ~30 s to a build. The whole
//! module — and its `runtime/mod.rs` re-export — sits behind the **non-default
//! `duckdb` cargo feature** so a dev `cargo check --no-default-features` skips
//! it (Phase 0 Task 0.1 / review S11). The CI `no-default-features` job keeps
//! the gate from rotting.
//!
//! ## Blocking — Phase 9 must `spawn_blocking` (review S2)
//!
//! The `duckdb` crate is **synchronous**: every function in this module is a
//! plain `fn`, not `async`. A DuckDB query can run for seconds; calling one
//! directly on a tokio worker thread would stall the runtime. **Phase 9's async
//! CLI MUST call [`open_duckdb`] / [`query`] / [`failures_top`] inside
//! [`tokio::task::spawn_blocking`].** A `duckdb-calls-spawn-blocking.sh` lint is
//! flagged for Batch D to enforce this mechanically.
//!
//! [`DuckHandle`] is `Send` but **not `Sync`** (DuckDB's `Connection` is) — open
//! a fresh handle per query; never wrap one in an `Arc` shared across tasks.

use std::path::Path;

use duckdb::Connection;

/// A failure in the DuckDB query layer.
///
/// Loud by construction (SO S6): every variant carries the underlying DuckDB
/// message or the offending path. A *bad-SQL* error is a [`DuckError::Query`] —
/// it is surfaced to the `boi traces query` CLI verbatim, never swallowed and
/// never a panic (Phase 8c exit gate).
#[derive(Debug, thiserror::Error)]
pub enum DuckError {
    /// The in-process DuckDB connection could not be opened.
    #[error("could not open DuckDB connection: {0}")]
    Open(String),
    /// `INSTALL otlp FROM community` / `LOAD otlp` failed — the OTLP community
    /// extension could not be fetched or loaded.
    ///
    /// `INSTALL` reaches the network the first time for a given DuckDB version;
    /// once fetched it is cached under `~/.duckdb/extensions/`. A failure here
    /// most often means no network on a cold cache.
    #[error("could not install/load the otlp DuckDB extension: {0}")]
    Extension(String),
    /// `ATTACH '<boi.db>'` failed — the path is missing or not a SQLite DB.
    ///
    /// The DuckDB message is held as a plain `String`, not a `#[source]` — the
    /// field is named `detail` (not the `thiserror`-magic `source`) so it is a
    /// message fragment, not a typed error-chain link.
    #[error("could not ATTACH boi.db at {path}: {detail}")]
    Attach {
        /// The `boi.db` path `open_duckdb` tried to attach.
        path: String,
        /// The underlying DuckDB error message.
        detail: String,
    },
    /// A SQL statement failed — a syntax error, an unknown column, a type
    /// mismatch. For `boi traces query` this is the bad-SQL path: the message
    /// is the DuckDB diagnostic, surfaced to the operator unaltered.
    #[error("query failed: {0}")]
    Query(String),
}

/// An open in-process DuckDB connection — `boi.db` attached read-only, the
/// `otlp` extension loaded so the trace JSONL is queryable.
///
/// Construct with [`open_duckdb`]. Pass `&DuckHandle` to [`query`] /
/// [`failures_top`].
///
/// **`Send` but not `Sync`** — DuckDB's `Connection` is single-threaded for
/// reads. A handle may be *moved* into a [`tokio::task::spawn_blocking`]
/// closure (`Send`); it may **not** be shared by reference across tasks
/// (`!Sync`). Phase 9 opens a fresh handle per query rather than sharing one.
pub struct DuckHandle {
    /// The live DuckDB connection. Private — the only ways to use it are the
    /// [`query`] / [`failures_top`] free functions, which keep every SQL string
    /// in this module.
    conn: Connection,
    /// The trace-JSONL glob this handle is "for" — the argument [`open_duckdb`]
    /// was given. [`failures_top`] reads it to build its `read_otlp_traces(...)`
    /// call (its plan signature carries no glob — the handle does). A
    /// `boi traces query` SQL string supplies its own glob, so [`query`] does
    /// not consult this.
    traces_glob: String,
}

impl std::fmt::Debug for DuckHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DuckHandle")
    }
}

// `DuckHandle` must be `Send` (moved into `spawn_blocking`) but is deliberately
// NOT asserted `Sync` — DuckDB's `Connection` is `Send + !Sync`, and a
// compile-time `_assert_send` documents the half of the contract Phase 9 relies
// on. A future `Sync` field would not break this; the `!Sync` half is a doc
// contract, not a type assertion.
const _: () = {
    fn _assert_send<T: Send>() {}
    fn _check() {
        _assert_send::<DuckHandle>();
    }
};

/// Open an in-process DuckDB connection wired to BOI's two telemetry stores.
///
/// Runs, in order:
///
/// ```sql
/// INSTALL otlp FROM community;   -- the OTLP community extension
/// LOAD otlp;
/// ATTACH '<boi_db>' AS boi_db (READ_ONLY);
/// ```
///
/// After this returns the connection can:
/// - read `boi.db`'s tables as `boi_db.<table>` (read-only — a query can never
///   mutate harness state); and
/// - read the OTel trace JSONL via `read_otlp_traces('<traces_glob>')`.
///
/// `traces_glob` (e.g. `~/.boi/v2/traces/*/*.jsonl`) is stored on the returned
/// [`DuckHandle`]: [`failures_top`] reads it to build its `read_otlp_traces`
/// call — its plan signature carries no glob argument, the handle does. A
/// `boi traces query` SQL string instead names its own glob inline, so
/// [`query`] never consults the stored one.
///
/// **Blocking** — see the module header; Phase 9 wraps this in
/// `spawn_blocking`.
///
/// # Errors
///
/// [`DuckError::Open`] if the connection cannot be created;
/// [`DuckError::Extension`] if `INSTALL`/`LOAD otlp` fails (most often a cold
/// extension cache with no network); [`DuckError::Attach`] if `boi_db` is
/// missing or not a SQLite database.
pub fn open_duckdb(boi_db: &Path, traces_glob: &str) -> Result<DuckHandle, DuckError> {
    let conn = Connection::open_in_memory().map_err(|e| DuckError::Open(e.to_string()))?;

    // The OTLP community extension — `read_otlp_traces`. `INSTALL ... FROM
    // community` fetches it once per DuckDB version (then cached under
    // ~/.duckdb/extensions/); `LOAD` activates it for this connection.
    conn.execute_batch("INSTALL otlp FROM community; LOAD otlp;")
        .map_err(|e| DuckError::Extension(e.to_string()))?;

    // ATTACH the SQLite boi.db READ_ONLY — DuckDB's SQLite scanner auto-loads;
    // READ_ONLY guarantees a `boi traces query` can never write harness state.
    // The path is single-quoted; a `'` in a real path would break the SQL, but
    // boi.db lives at a BOI-controlled path with no quotes (`~/.boi/v2/boi.db`).
    let attach = format!("ATTACH '{}' AS boi_db (READ_ONLY);", boi_db.display());
    conn.execute_batch(&attach).map_err(|e| DuckError::Attach {
        path: boi_db.display().to_string(),
        detail: e.to_string(),
    })?;

    Ok(DuckHandle {
        conn,
        traces_glob: traces_glob.to_owned(),
    })
}

/// A tabular query result — column names plus rows of already-stringified
/// cells.
///
/// Every cell is a `String`: `boi traces query` prints a text table, so the
/// `query` layer stringifies each DuckDB value once (see `value_to_string`)
/// rather than handing the CLI a dynamically-typed cell to format. `rows` and
/// every inner row are parallel to `columns` — `rows[r][c]` is the cell under
/// `columns[c]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryResult {
    /// The result-set column names, left-to-right.
    pub columns: Vec<String>,
    /// One `Vec<String>` per result row; each is `columns.len()` cells wide.
    pub rows: Vec<Vec<String>>,
}

/// Run arbitrary read-only SQL — the engine behind the Phase 9
/// `boi traces query <SQL>` CLI.
///
/// `sql` is the operator's query verbatim. It can reference both telemetry
/// stores [`open_duckdb`] wired up: `read_otlp_traces('<glob>')` for the OTel
/// trace JSONL and `boi_db.<table>` for the attached SQLite harness state.
/// `boi_db` is attached `READ_ONLY`, so the query cannot mutate harness state
/// even if the SQL says `UPDATE` — the write simply fails.
///
/// Cells are stringified once here (`value_to_string`) into a [`QueryResult`]
/// the CLI prints as a text table.
///
/// **Blocking** — see the module header; Phase 9 wraps this in
/// `spawn_blocking`.
///
/// # Errors
///
/// [`DuckError::Query`] carrying the DuckDB diagnostic for *any* SQL failure —
/// a syntax error, an unknown column/table, a type mismatch. This is the
/// `boi traces query` bad-SQL path: the message reaches the operator
/// unaltered, loud, never a panic (Phase 8c exit gate).
pub fn query(handle: &DuckHandle, sql: &str) -> Result<QueryResult, DuckError> {
    let mut stmt = handle
        .conn
        .prepare(sql)
        .map_err(|e| DuckError::Query(e.to_string()))?;

    // `query` steps the statement; `column_names`/`column_count` are only valid
    // *after* the first step, so the column metadata is read off `Rows`.
    let mut rows = stmt
        .query([])
        .map_err(|e| DuckError::Query(e.to_string()))?;

    let mut columns: Vec<String> = Vec::new();
    let mut out: Vec<Vec<String>> = Vec::new();
    // `Rows` is a fallible streaming iterator — `.next()` yields
    // `Result<Option<&Row>>`; a step error (e.g. a cast failure surfacing only
    // mid-scan) becomes a `DuckError::Query`.
    while let Some(row) = rows.next().map_err(|e| DuckError::Query(e.to_string()))? {
        if columns.is_empty() {
            columns = row.as_ref().column_names();
        }
        let width = columns.len();
        let mut cells: Vec<String> = Vec::with_capacity(width);
        for idx in 0..width {
            // `get::<_, duckdb::types::Value>` is the universal extractor —
            // `Value`'s `FromSql` accepts any DuckDB column type, so an
            // arbitrary operator query never hits a "wrong Rust type" error.
            let value: duckdb::types::Value =
                row.get(idx).map_err(|e| DuckError::Query(e.to_string()))?;
            cells.push(value_to_string(&value));
        }
        out.push(cells);
    }

    // A statement that yields zero rows (a valid `SELECT` with an empty result,
    // or a DDL/DML statement) has no row to read column names off. Re-derive
    // them from the prepared statement, which knows its result schema.
    if columns.is_empty() {
        columns = stmt.column_names();
    }

    Ok(QueryResult { columns, rows: out })
}

/// Render one DuckDB [`Value`](duckdb::types::Value) as the text a
/// [`QueryResult`] cell holds.
///
/// `boi traces query` is a human-facing text table, so the rendering favors
/// legibility: a `NULL` is the literal `NULL`; a string is itself (unquoted); a
/// blob is a short `<N bytes>` placeholder rather than raw bytes. Scalar
/// numbers / booleans use their natural `Display`. Composite values (list /
/// struct / map / and the rest) fall back to `{:?}` — they are rare in the
/// trace schema and a debug rendering beats losing the cell.
fn value_to_string(value: &duckdb::types::Value) -> String {
    use duckdb::types::Value;
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Boolean(b) => b.to_string(),
        Value::TinyInt(n) => n.to_string(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::HugeInt(n) => n.to_string(),
        Value::UTinyInt(n) => n.to_string(),
        Value::USmallInt(n) => n.to_string(),
        Value::UInt(n) => n.to_string(),
        Value::UBigInt(n) => n.to_string(),
        Value::Float(n) => n.to_string(),
        Value::Double(n) => n.to_string(),
        Value::Decimal(d) => d.to_string(),
        Value::Text(s) => s.clone(),
        Value::Enum(s) => s.clone(),
        Value::Blob(b) => format!("<{} bytes>", b.len()),
        // Timestamp / date / time / interval and the composite kinds are
        // uncommon in the OTLP-trace schema (which is mostly varchar/bigint);
        // a `Debug` rendering keeps the cell legible without a per-kind
        // formatter for each.
        other => format!("{other:?}"),
    }
}

/// One row of the `boi failures top` table — a recurring failure fingerprint
/// and how often it has fired in the window.
///
/// Maps to the §9 CLI columns `FINGERPRINT / COUNT / LAST SEEN / PHASE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureRow {
    /// The `boi.failure_fingerprint` value — the stable identity of a failure
    /// mode (e.g. `verify/cargo-clippy-exit-1`).
    pub fingerprint: String,
    /// How many `boi.error` events carried this fingerprint in the window.
    pub count: u64,
    /// The most recent occurrence, as an ISO-8601 UTC string
    /// (`2026-05-15T14:32:00Z`).
    pub last_seen: String,
    /// The `boi.phase` of one occurrence — the phase a fix would target. The
    /// empty string if no occurrence carried a `boi.phase` attribute.
    pub phase: String,
}

/// The query behind `boi failures top` (§9) — the top-N recurring failure
/// fingerprints over a recency window.
///
/// `failures_top` aggregates the `boi.error` **span events** the
/// [`OtelObserver`](super::otel::OtelObserver) writes from every
/// `ErrorEncountered` bus event. It
///
/// 1. `UNNEST`s each span's `events_json` array (the `otlp` extension exposes
///    span events as a JSON-text column, not separate rows — see the schema
///    note below);
/// 2. keeps the `boi.error` events whose occurrence is within `window_days` of
///    now;
/// 3. `GROUP BY`s the [`FAILURE_FINGERPRINT_ATTR`](super::otel::FAILURE_FINGERPRINT_ATTR)
///    key — the **same `pub const`** Task 8a.2's emit uses, so the written key
///    and the grouped key cannot drift (review item 30);
/// 4. keeps groups with `count >= 3` (the §9 "recurring" floor);
/// 5. orders by `count` descending and returns the top `n`.
///
/// **Where the `boi.error` events live (G24.1).** `ErrorEncountered` carries no
/// `phase_run_id`, so Task 8a.2 attaches its `boi.error` span event to the
/// **spec-root (`invoke_workflow`) span**, not the phase span. This query does
/// not filter by span name — it `UNNEST`s `events_json` for *every* span and
/// selects on the event `name`, so it finds `boi.error` events wherever the
/// observer placed them.
///
/// **Schema note (Phase 8c plan deviation).** Design §8's example query
/// `GROUP BY failure_fingerprint` as if it were a top-level column. It is not:
/// the `otlp` community extension's `read_otlp_traces` exposes `events_json` as
/// a `VARCHAR` of JSON text; the fingerprint is a key *inside* each event's
/// `attributes` object. Hence the `UNNEST(from_json(...))` +
/// `json_extract_string` shape.
///
/// **Blocking** — see the module header; Phase 9 wraps this in
/// `spawn_blocking`.
///
/// # Errors
///
/// [`DuckError::Query`] if the aggregation SQL fails (e.g. the `otlp` extension
/// is not loaded, or the trace glob matches a malformed file).
pub fn failures_top(
    handle: &DuckHandle,
    window_days: u32,
    n: u32,
) -> Result<Vec<FailureRow>, DuckError> {
    // The fingerprint key — the SHARED const from Task 8a.2. It is interpolated
    // into the JSON path so the emit key and the GROUP BY key are one symbol;
    // the const's value (`boi.failure_fingerprint`) contains only
    // `[a-z._]` — safe inside a `'$."..."'` JSON path with no escaping needed.
    let fp_key = crate::runtime::otel::FAILURE_FINGERPRINT_ATTR;
    // The trace glob is the one `open_duckdb` was handed and stored.
    let traces_glob = &handle.traces_glob;

    // `window_days` / `n` are `u32` from the CLI — interpolated as plain
    // integers (a `u32` cannot carry SQL-injection text). `traces_glob` is a
    // BOI-controlled path; it is single-quoted like every other path here.
    //
    // `time_unix_nano` is nanoseconds; `make_timestamp` takes MICROseconds, so
    // the value is divided by 1000 with DuckDB's `//` integer-division operator
    // (`/` would yield a `DOUBLE`, which `make_timestamp` rejects). `now()` is
    // `TIMESTAMP WITH TIME ZONE` while `make_timestamp` yields a plain
    // `TIMESTAMP` — the bundled DuckDB (1.4.x) will not subtract an `INTERVAL`
    // from / compare the two without an explicit `CAST(now() AS TIMESTAMP)`.
    let sql = format!(
        "WITH error_events AS (
             SELECT
                 json_extract_string(ev.unnest, '$.name') AS event_name,
                 json_extract_string(ev.unnest, '$.attributes.\"{fp_key}\"') AS fingerprint,
                 COALESCE(json_extract_string(ev.unnest, '$.attributes.\"boi.phase\"'), '') AS phase,
                 CAST(json_extract(ev.unnest, '$.time_unix_nano') AS BIGINT) // 1000 AS event_micros
             FROM read_otlp_traces('{traces_glob}') AS t,
                  UNNEST(from_json(t.events_json, '[\"json\"]')) AS ev
             WHERE t.events_json IS NOT NULL
         )
         SELECT
             fingerprint,
             COUNT(*) AS n,
             strftime(make_timestamp(MAX(event_micros)), '%Y-%m-%dT%H:%M:%SZ') AS last_seen,
             ANY_VALUE(phase) AS phase
         FROM error_events
         WHERE event_name = 'boi.error'
           AND fingerprint IS NOT NULL
           AND make_timestamp(event_micros) >= (CAST(now() AS TIMESTAMP) - INTERVAL {window_days} DAY)
         GROUP BY fingerprint
         HAVING COUNT(*) >= 3
         ORDER BY n DESC, fingerprint
         LIMIT {n}"
    );

    let mut stmt = handle
        .conn
        .prepare(&sql)
        .map_err(|e| DuckError::Query(e.to_string()))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| DuckError::Query(e.to_string()))?;

    let mut out: Vec<FailureRow> = Vec::new();
    while let Some(row) = rows.next().map_err(|e| DuckError::Query(e.to_string()))? {
        let fingerprint: String = row.get(0).map_err(|e| DuckError::Query(e.to_string()))?;
        // `COUNT(*)` comes back as a signed `BIGINT`; it is never negative —
        // clamp a (impossible) negative to 0 rather than panicking on the cast.
        let count_i64: i64 = row.get(1).map_err(|e| DuckError::Query(e.to_string()))?;
        let count = u64::try_from(count_i64).unwrap_or(0);
        let last_seen: String = row.get(2).map_err(|e| DuckError::Query(e.to_string()))?;
        let phase: String = row.get(3).map_err(|e| DuckError::Query(e.to_string()))?;
        out.push(FailureRow {
            fingerprint,
            count,
            last_seen,
            phase,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A throwaway directory removed on drop — the `runtime/` test convention
    /// (`otel.rs` / `goose.rs`).
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("boi-duckdb-{}-{tag}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.path));
        }
    }

    /// The repo-relative path of the shared cross-phase OTLP/JSON fixture
    /// (committed by Phase 8a.1; the Phase 8c query tests read this exact file).
    fn sample_traces() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/traces/sample_otlp.jsonl")
    }

    /// Write a minimal SQLite `boi.db` — a `phase_runs` table with one row — so
    /// the ATTACH has a real database to bind. DuckDB's own SQLite scanner
    /// writes a valid SQLite file via `ATTACH ... (TYPE SQLITE)`; using DuckDB
    /// to mint it keeps the test free of an `rusqlite` dev-dependency.
    fn write_boi_db(dir: &Path) -> PathBuf {
        let db = dir.join("boi.db");
        let conn = Connection::open_in_memory().expect("scratch duckdb");
        conn.execute_batch(&format!(
            "ATTACH '{}' AS seed (TYPE SQLITE);
             CREATE TABLE seed.phase_runs (phase_run_id VARCHAR, phase VARCHAR);
             INSERT INTO seed.phase_runs VALUES ('P0000001a', 'execute');
             DETACH seed;",
            db.display()
        ))
        .expect("seed a SQLite boi.db");
        db
    }

    /// `open_duckdb` over a tempdir `boi.db` + the shared `sample_otlp.jsonl`
    /// fixture: the connection opens, the `otlp` extension loads, and a trivial
    /// `SELECT` over the trace JSONL succeeds (the exit-gate "OTel JSONL
    /// queryable via read_otlp_traces" check).
    #[test]
    fn test_l2_open_duckdb_makes_otel_jsonl_queryable() {
        let tmp = TempDir::new("open-traces");
        let boi_db = write_boi_db(&tmp.path);
        let traces = sample_traces();
        let traces_glob = traces.display().to_string();

        let handle = open_duckdb(&boi_db, &traces_glob).expect("open_duckdb");

        // A trivial SELECT over the OTLP fixture — 4 spans in `sample_otlp.jsonl`.
        let n: i64 = handle
            .conn
            .query_row(
                &format!("SELECT COUNT(*) FROM read_otlp_traces('{traces_glob}')"),
                [],
                |r| r.get(0),
            )
            .expect("read_otlp_traces over the fixture");
        assert_eq!(n, 4, "the sample fixture has 4 spans");
    }

    /// The attached `boi_db` is queryable — `boi_db.phase_runs` resolves and
    /// returns the seeded row.
    #[test]
    fn test_l2_open_duckdb_attaches_boi_db() {
        let tmp = TempDir::new("attach");
        let boi_db = write_boi_db(&tmp.path);
        let handle = open_duckdb(&boi_db, "unused-glob").expect("open_duckdb");

        let phase: String = handle
            .conn
            .query_row("SELECT phase FROM boi_db.phase_runs", [], |r| r.get(0))
            .expect("query the attached boi_db");
        assert_eq!(phase, "execute", "the attached boi.db row is readable");
    }

    /// `boi_db` is attached `READ_ONLY` — a write against it is rejected, so a
    /// `boi traces query` can never mutate harness state.
    #[test]
    fn test_l2_open_duckdb_attaches_boi_db_read_only() {
        let tmp = TempDir::new("readonly");
        let boi_db = write_boi_db(&tmp.path);
        let handle = open_duckdb(&boi_db, "unused-glob").expect("open_duckdb");

        // An INSERT into the READ_ONLY-attached DB must fail.
        let write = handle
            .conn
            .execute_batch("INSERT INTO boi_db.phase_runs VALUES ('P0000002a', 'commit')");
        assert!(
            write.is_err(),
            "a write to the READ_ONLY-attached boi_db must be rejected"
        );
    }

    /// A missing `boi.db` path → a loud [`DuckError::Attach`], not a panic.
    #[test]
    fn test_l2_open_duckdb_missing_boi_db_is_attach_error() {
        let tmp = TempDir::new("missing");
        let absent = tmp.path.join("does-not-exist.db");
        let err =
            open_duckdb(&absent, "unused-glob").expect_err("a missing boi.db must error, not open");
        assert!(
            matches!(err, DuckError::Attach { .. }),
            "a missing boi.db is a DuckError::Attach, got {err:?}"
        );
    }

    // ---- Task 8c.2 — `query` -------------------------------------------------

    /// A `SELECT` over `sample_otlp.jsonl` returns the expected columns and
    /// rows, addressing a span attribute by key.
    ///
    /// Plan deviation (8c.2): the plan said attributes are addressed as
    /// `attributes['boi.failure_fingerprint']` — a DuckDB `MAP` access. The
    /// `otlp` community extension does **not** expose attributes as a `MAP`: a
    /// `DESCRIBE SELECT * FROM read_otlp_traces(...)` shows `span_attributes`
    /// as a **`VARCHAR`** holding a JSON object. The correct access is
    /// `json_extract_string(span_attributes, '$."key"')` (or the `->>`
    /// shorthand). This test uses the real expression.
    #[test]
    fn test_l2_query_selects_otlp_rows_addressing_an_attribute() {
        let tmp = TempDir::new("query-otlp");
        let boi_db = write_boi_db(&tmp.path);
        let traces_glob = sample_traces().display().to_string();
        let handle = open_duckdb(&boi_db, &traces_glob).expect("open_duckdb");

        // The `invoke_agent` span carries `boi.phase = 'execute'` in its
        // JSON `span_attributes`; pull span name + that attribute.
        let sql = format!(
            "SELECT span_name, \
                    json_extract_string(span_attributes, '$.\"boi.phase\"') AS phase \
             FROM read_otlp_traces('{traces_glob}') \
             WHERE span_name = 'invoke_agent boi.worker'"
        );
        let result = query(&handle, &sql).expect("a valid SELECT");

        assert_eq!(
            result.columns,
            vec!["span_name".to_owned(), "phase".to_owned()],
            "columns echo the SELECT list"
        );
        assert_eq!(result.rows.len(), 1, "one invoke_agent span in the fixture");
        assert_eq!(result.rows[0][0], "invoke_agent boi.worker");
        assert_eq!(
            result.rows[0][1], "execute",
            "the addressed boi.phase attribute is extracted"
        );
    }

    /// A query over the attached `boi_db` returns its rows — `query` reaches
    /// the SQLite harness state, not just the trace JSONL.
    #[test]
    fn test_l2_query_reads_the_attached_boi_db() {
        let tmp = TempDir::new("query-attached");
        let boi_db = write_boi_db(&tmp.path);
        let handle = open_duckdb(&boi_db, "unused-glob").expect("open_duckdb");

        let result = query(&handle, "SELECT phase_run_id, phase FROM boi_db.phase_runs")
            .expect("a valid SELECT");
        assert_eq!(
            result.columns,
            vec!["phase_run_id".to_owned(), "phase".to_owned()]
        );
        assert_eq!(
            result.rows,
            vec![vec!["P0000001a".to_owned(), "execute".to_owned()]]
        );
    }

    /// Malformed SQL → a loud [`DuckError::Query`] carrying the DuckDB
    /// diagnostic, never a panic (the `boi traces query` bad-SQL contract).
    #[test]
    fn test_l2_query_malformed_sql_is_a_loud_error_not_a_panic() {
        let tmp = TempDir::new("query-bad");
        let boi_db = write_boi_db(&tmp.path);
        let handle = open_duckdb(&boi_db, "unused-glob").expect("open_duckdb");

        let err = query(&handle, "SELEKT * FROM nowhere")
            .expect_err("malformed SQL must return Err, not panic");
        assert!(
            matches!(err, DuckError::Query(_)),
            "a SQL error is a DuckError::Query, got {err:?}"
        );
        // The DuckDB diagnostic is carried through to the operator.
        assert!(
            !err.to_string().is_empty(),
            "the error carries the DuckDB message"
        );
    }

    /// A NULL cell renders as the literal `NULL`; integer columns stringify to
    /// their decimal form — the `value_to_string` contract a text table needs.
    #[test]
    fn test_l2_query_stringifies_null_and_integer_cells() {
        let tmp = TempDir::new("query-types");
        let boi_db = write_boi_db(&tmp.path);
        let handle = open_duckdb(&boi_db, "unused-glob").expect("open_duckdb");

        let result =
            query(&handle, "SELECT NULL AS a, 42 AS b, 'hi' AS c").expect("a constant SELECT");
        assert_eq!(
            result.rows,
            vec![vec!["NULL".to_owned(), "42".to_owned(), "hi".to_owned(),]]
        );
    }

    // ---- Task 8c.3 — `failures_top` -----------------------------------------

    /// The repo-relative path of the failure-fingerprint fixture.
    ///
    /// A dedicated file (`sample_otlp.jsonl` carries no `boi.error` events, and
    /// Phase 8a's exporter test pins its exact shape — it must not be touched).
    /// `sample_failures.jsonl` has 4 spec-root spans whose `events_json` carry
    /// `boi.error` span events: `verify/cargo-clippy-exit-1` × 5,
    /// `merge-conflict/src/config.rs` × 1, `stale/2010-fingerprint` × 4 (dated
    /// 2010 — used to exercise the recency window). The two non-stale
    /// fingerprints are far-future-dated so they sit inside *any* window
    /// regardless of the test's wall-clock run date — a deterministic fixture
    /// that does not rot.
    fn failures_traces() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/traces/sample_failures.jsonl")
    }

    /// A fingerprint at 5× clears the `>= 3` floor and is returned; a
    /// fingerprint at 1× does not; ordering is descending by count.
    #[test]
    fn test_l2_failures_top_returns_recurring_fingerprints_above_the_floor() {
        let tmp = TempDir::new("failures-floor");
        let boi_db = write_boi_db(&tmp.path);
        let traces_glob = failures_traces().display().to_string();
        let handle = open_duckdb(&boi_db, &traces_glob).expect("open_duckdb");

        // A wide window (100 yr) so the assertion is on the floor + ordering,
        // not on wall-clock recency — every fixture event is in range.
        let rows = failures_top(&handle, 36_500, 10).expect("failures_top");

        // `verify/cargo-clippy-exit-1` (5x) and `stale/2010-fingerprint` (4x)
        // clear `>= 3`; `merge-conflict/src/config.rs` (1x) does not.
        assert_eq!(rows.len(), 2, "two fingerprints clear the >= 3 floor");
        assert_eq!(
            rows[0].fingerprint, "verify/cargo-clippy-exit-1",
            "the 5x fingerprint is first"
        );
        assert_eq!(rows[0].count, 5);
        assert_eq!(rows[1].fingerprint, "stale/2010-fingerprint");
        assert_eq!(rows[1].count, 4);
        // Descending by count.
        assert!(
            rows[0].count >= rows[1].count,
            "rows are ordered by descending count"
        );
        assert!(
            !rows
                .iter()
                .any(|r| r.fingerprint == "merge-conflict/src/config.rs"),
            "the 1x fingerprint is below the >= 3 floor and excluded"
        );
    }

    /// A `FailureRow` carries the occurrence count, the phase, and a non-empty
    /// `last_seen` timestamp string — the §9 CLI columns.
    #[test]
    fn test_l2_failures_top_row_carries_count_phase_and_last_seen() {
        let tmp = TempDir::new("failures-fields");
        let boi_db = write_boi_db(&tmp.path);
        let traces_glob = failures_traces().display().to_string();
        let handle = open_duckdb(&boi_db, &traces_glob).expect("open_duckdb");

        let rows = failures_top(&handle, 36_500, 10).expect("failures_top");
        let clippy = rows
            .iter()
            .find(|r| r.fingerprint == "verify/cargo-clippy-exit-1")
            .expect("the clippy fingerprint is present");
        assert_eq!(clippy.count, 5, "5 boi.error events carried it");
        assert_eq!(clippy.phase, "execute", "the boi.phase of an occurrence");
        assert!(
            !clippy.last_seen.is_empty() && clippy.last_seen.ends_with('Z'),
            "last_seen is an ISO-8601 UTC string, got {:?}",
            clippy.last_seen
        );
    }

    /// `n` caps the result count — top-N, not all-N.
    #[test]
    fn test_l2_failures_top_limit_caps_the_row_count() {
        let tmp = TempDir::new("failures-limit");
        let boi_db = write_boi_db(&tmp.path);
        let traces_glob = failures_traces().display().to_string();
        let handle = open_duckdb(&boi_db, &traces_glob).expect("open_duckdb");

        // Two fingerprints clear the floor; `n = 1` returns only the top one.
        let rows = failures_top(&handle, 36_500, 1).expect("failures_top");
        assert_eq!(rows.len(), 1, "n=1 caps the result at one row");
        assert_eq!(rows[0].fingerprint, "verify/cargo-clippy-exit-1");
    }

    /// The recency window excludes old fingerprints — a 7-day window drops the
    /// 2010-dated `stale/2010-fingerprint` (4×) that a 100-year window keeps.
    #[test]
    fn test_l2_failures_top_window_excludes_stale_fingerprints() {
        let tmp = TempDir::new("failures-window");
        let boi_db = write_boi_db(&tmp.path);
        let traces_glob = failures_traces().display().to_string();
        let handle = open_duckdb(&boi_db, &traces_glob).expect("open_duckdb");

        let recent = failures_top(&handle, 7, 10).expect("failures_top 7d");
        assert!(
            !recent
                .iter()
                .any(|r| r.fingerprint == "stale/2010-fingerprint"),
            "the 2010-dated fingerprint is outside a 7-day window"
        );
        // The far-future-dated clippy fingerprint stays in range of any window.
        assert!(
            recent
                .iter()
                .any(|r| r.fingerprint == "verify/cargo-clippy-exit-1"),
            "the in-window fingerprint is still returned"
        );
    }

    /// `failures_top` over traces with no `boi.error` events returns an empty
    /// vec — not an error. (`sample_otlp.jsonl` carries only a
    /// `boi.decision_recorded` event.)
    #[test]
    fn test_l2_failures_top_no_error_events_is_empty_not_error() {
        let tmp = TempDir::new("failures-none");
        let boi_db = write_boi_db(&tmp.path);
        let traces_glob = sample_traces().display().to_string();
        let handle = open_duckdb(&boi_db, &traces_glob).expect("open_duckdb");

        let rows =
            failures_top(&handle, 36_500, 10).expect("failures_top is Ok on no-error traces");
        assert!(rows.is_empty(), "no boi.error events → no failure rows");
    }
}
