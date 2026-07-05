//! The `phase_runs` table — one row per phase execution, written in two
//! phases.
//!
//! - [`insert_start`] writes the row at `PhaseStarted` with `completed_at`,
//!   `verdict`, and `synopsis` empty/NULL.
//! - [`update_end`] fills `synopsis` / `verdict` / `files_touched` /
//!   `completed_at` at `PhaseCompleted`, matched by primary key.
//!
//! No DELETE (except `boi clean`). The `UNIQUE(spec_id, task_id, phase,
//! phase_iteration)` constraint catches retry storms — Phase 5a respects it.
//!
//! [`find_abandoned`] is the sweeper query (Phase 5a): a worker pings
//! [`record_heartbeat`] every ~30s, and a row whose liveness signal has gone
//! stale while still un-completed is abandoned (B7).
//!
//! [`fetch_latest_open_for_task`] backs `boi status`'s `[phase=…,iter=…]`
//! display; [`fetch_history_for_spec`] backs `boi log`'s phase history —
//! folded G16.7.

use std::path::PathBuf;

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use sqlx::SqlitePool;

use crate::repo::db::RepoError;
use crate::types::ids::{PhaseRunId, SpecId, TaskId};
use crate::types::verdict::WorkerVerdict;

/// A row of the `phase_runs` table.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PhaseRunRow {
    /// The phase run's ID.
    pub id: String,
    /// The spec this run belongs to.
    pub spec_id: String,
    /// The task this run belongs to — `None` for a spec-level phase.
    pub task_id: Option<String>,
    /// The phase name.
    pub phase: String,
    /// Which iteration of the phase this is.
    pub phase_iteration: i64,
    /// Which authored `spec_versions` row this ran against.
    pub spec_version: i64,
    /// The provider that ran it (`claude_code` / `openrouter` / `human`).
    pub provider: String,
    /// The worker process ID, if a worker ran it.
    pub worker_id: Option<String>,
    /// Files the phase touched — JSON array, empty until `update_end`.
    pub files_touched: Value,
    /// The phase synopsis — empty until `update_end`.
    pub synopsis: String,
    /// The `WorkerVerdict` JSON — `None` while in-progress.
    pub verdict: Option<Value>,
    /// Last worker heartbeat — `None` until the first ping.
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    /// When the phase started.
    pub started_at: DateTime<Utc>,
    /// When the phase completed — `None` while in-progress.
    pub completed_at: Option<DateTime<Utc>>,
    /// Input tokens consumed — `None` until `update_end` (G25.1).
    pub tokens_in: Option<i64>,
    /// Output tokens produced — `None` until `update_end` (G25.1).
    pub tokens_out: Option<i64>,
}

impl PhaseRunRow {
    /// Whether this run is still in progress (`completed_at IS NULL`).
    pub fn is_open(&self) -> bool {
        self.completed_at.is_none()
    }

    /// Decode the `verdict` JSON into a typed [`WorkerVerdict`], if present.
    pub fn worker_verdict(&self) -> Result<Option<WorkerVerdict>, RepoError> {
        match &self.verdict {
            Some(j) => Ok(Some(serde_json::from_value(j.clone())?)),
            None => Ok(None),
        }
    }
}

/// Insert a `phase_runs` row at phase start (phase 1 of the two-phase write).
///
/// `completed_at`, `verdict` start NULL; `synopsis` / `files_touched` start
/// empty. A duplicate `(spec_id, task_id, phase, phase_iteration)` hits the
/// UNIQUE constraint and returns [`RepoError::Duplicate`] — this is the
/// retry-storm guard.
#[allow(clippy::too_many_arguments)]
pub async fn insert_start(
    pool: &SqlitePool,
    phase_run_id: &PhaseRunId,
    spec_id: &SpecId,
    task_id: Option<&TaskId>,
    phase: &str,
    phase_iteration: u32,
    spec_version: i64,
    provider: &str,
    worker_id: Option<&str>,
    now: DateTime<Utc>,
) -> Result<(), RepoError> {
    let id = phase_run_id.as_str();
    let sid = spec_id.as_str();
    let tid = task_id.map(TaskId::as_str);
    let iteration = i64::from(phase_iteration);
    let res = sqlx::query!(
        "INSERT INTO phase_runs \
         (id, spec_id, task_id, phase, phase_iteration, spec_version, provider, worker_id, started_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        id,
        sid,
        tid,
        phase,
        iteration,
        spec_version,
        provider,
        worker_id,
        now,
    )
    .execute(pool)
    .await;
    match res {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(RepoError::Duplicate(
            format!("phase_run {phase_run_id} or its (spec,task,phase,iteration) already exists"),
        )),
        Err(e) => Err(RepoError::Sqlx(e)),
    }
}

/// Allocate a fresh [`PhaseRunId`] and insert the phase-start row.
///
/// The generate-and-insert counterpart to [`insert_start`].
///
/// Unlike the other `allocate_*` functions this does NOT collision-retry.
/// `phase_runs` has two UNIQUE constraints — the `id` PK and
/// `(spec_id, task_id, phase, phase_iteration)`. A retry helps only the first;
/// retrying the second would spin on a fresh ID while the real cause (a
/// duplicate phase iteration) never clears. So a single generated ID is used.
///
/// On a unique violation [`insert_start`] returns a [`RepoError::Duplicate`]
/// that cannot itself tell the two constraints apart. This wrapper classifies
/// it (A-SF-5): a follow-up `SELECT id` confirms whether the generated `id`
/// itself already exists — a genuine (astronomically rare) PK collision,
/// surfaced as [`RepoError::IdExhausted`] — versus the expected case, the
/// composite-UNIQUE retry-storm guard, re-raised as a `Duplicate` worded for
/// *that* cause only (no misleading "phase_run {id} or …" prefix).
#[allow(clippy::too_many_arguments)]
pub async fn allocate_phase_run_id(
    pool: &SqlitePool,
    spec_id: &SpecId,
    task_id: Option<&TaskId>,
    phase: &str,
    phase_iteration: u32,
    spec_version: i64,
    provider: &str,
    worker_id: Option<&str>,
    now: DateTime<Utc>,
) -> Result<PhaseRunId, RepoError> {
    let raw = crate::repo::ids::random_id('P');
    let id = PhaseRunId::new(&raw)
        .map_err(|e| RepoError::Duplicate(format!("generated invalid phase_run id: {e}")))?;
    match insert_start(
        pool,
        &id,
        spec_id,
        task_id,
        phase,
        phase_iteration,
        spec_version,
        provider,
        worker_id,
        now,
    )
    .await
    {
        Ok(()) => Ok(id),
        Err(RepoError::Duplicate(_)) => {
            // Disambiguate the two UNIQUE constraints. If a row already holds
            // the generated `id`, the PK itself collided — `IdExhausted` (one
            // attempt; this allocator deliberately does not retry). Otherwise
            // it was the composite `(spec,task,phase,iteration)` UNIQUE.
            // `id` is a non-null PRIMARY KEY; the `"id!"` override tells the
            // compile-time macro so the scalar is `String`, not `Option`.
            let id_collided: Option<String> = sqlx::query_scalar!(
                "SELECT id AS \"id!: String\" FROM phase_runs WHERE id = ?1",
                raw
            )
            .fetch_optional(pool)
            .await?;
            if id_collided.is_some() {
                Err(RepoError::IdExhausted {
                    prefix: 'P',
                    attempts: 1,
                })
            } else {
                Err(RepoError::Duplicate(format!(
                    "phase_run for (spec={spec_id}, task={task_id:?}, phase={phase}, \
                     iteration={phase_iteration}) already exists"
                )))
            }
        }
        Err(e) => Err(e),
    }
}

/// Complete a `phase_runs` row at phase end (phase 2 of the two-phase write).
///
/// UPDATEs `synopsis` / `verdict` / `files_touched` / `completed_at` plus the
/// G25.1 token columns (`tokens_in` / `tokens_out`) on the row matched by
/// `phase_run_id`. The bus's `PhaseCompleted` persist arm passes the values
/// straight off the event — closing the gap where `BoiEvent::PhaseCompleted`
/// carried them but `update_end` dropped them, leaving Phase 8b's `metrics`
/// block shipping zeros.
///
/// Per the 2026-06-01 directive ("strip $ everywhere, keep tokens
/// everywhere"), the per-run dollar column is gone from the schema (migration
/// 0003) and from this signature — tokens stay as the spend-hint signal.
///
/// Returns [`RepoError::NotFound`] if no such row exists (e.g. `update_end`
/// without a prior `insert_start`).
#[allow(clippy::too_many_arguments)]
pub async fn update_end(
    pool: &SqlitePool,
    phase_run_id: &PhaseRunId,
    synopsis: &str,
    verdict: &WorkerVerdict,
    files_touched: &[PathBuf],
    tokens_in: u64,
    tokens_out: u64,
    completed_at: DateTime<Utc>,
) -> Result<(), RepoError> {
    let id = phase_run_id.as_str();
    let verdict_json = serde_json::to_value(verdict)?;
    let files_json = serde_json::to_value(files_touched)?;
    // SQLite INTEGER is i64. Token counts never approach i64::MAX in practice;
    // a value that did is a bug — surface it loudly rather than truncate (S6).
    let tokens_in_i64 = i64::try_from(tokens_in).map_err(|_| {
        RepoError::NotFound(format!(
            "phase_run {phase_run_id}: tokens_in {tokens_in} exceeds i64"
        ))
    })?;
    let tokens_out_i64 = i64::try_from(tokens_out).map_err(|_| {
        RepoError::NotFound(format!(
            "phase_run {phase_run_id}: tokens_out {tokens_out} exceeds i64"
        ))
    })?;
    let affected = sqlx::query!(
        "UPDATE phase_runs \
         SET synopsis = ?2, verdict = ?3, files_touched = ?4, completed_at = ?5, \
             tokens_in = ?6, tokens_out = ?7 \
         WHERE id = ?1",
        id,
        synopsis,
        verdict_json,
        files_json,
        completed_at,
        tokens_in_i64,
        tokens_out_i64,
    )
    .execute(pool)
    .await?
    .rows_affected();
    if affected == 0 {
        return Err(RepoError::NotFound(format!(
            "phase_run {phase_run_id} (update_end with no prior insert_start)"
        )));
    }
    Ok(())
}

/// Record a worker liveness heartbeat on an *open* phase run.
///
/// The UPDATE is scoped `AND completed_at IS NULL` (review C3 / plan Task
/// 5a.1): a heartbeat that arrives after the bus already closed the row — a
/// benign worker/bus race — must not mutate the closed row. Without the
/// predicate a late ping resurrects `last_heartbeat_at` on a finished run,
/// silently falsifying the C3 invariant the sweeper relies on. A heartbeat
/// for an already-closed (or absent) row is a no-op, not an error — the
/// sweeper only inspects open rows, so there is nothing to keep alive.
pub async fn record_heartbeat(
    pool: &SqlitePool,
    phase_run_id: &PhaseRunId,
    now: DateTime<Utc>,
) -> Result<(), RepoError> {
    let id = phase_run_id.as_str();
    sqlx::query!(
        "UPDATE phase_runs SET last_heartbeat_at = ?2 \
         WHERE id = ?1 AND completed_at IS NULL",
        id,
        now,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Find abandoned phase runs — open rows whose liveness signal has gone stale.
///
/// A row is abandoned when `completed_at IS NULL` and its most recent liveness
/// signal — the heartbeat, or the start time if it never heartbeated — is
/// older than `now - threshold`. `COALESCE(last_heartbeat_at, started_at)`
/// means a worker that died before its first ping is still detected, while a
/// just-started run inside the window is not.
pub async fn find_abandoned(
    pool: &SqlitePool,
    threshold: Duration,
    now: DateTime<Utc>,
) -> Result<Vec<PhaseRunId>, RepoError> {
    let cutoff = now - threshold;
    // `id` is a non-null PRIMARY KEY; the `"id!"` override tells the
    // compile-time macro so it yields `String`, not `Option<String>`.
    let rows = sqlx::query_scalar!(
        "SELECT id AS \"id!: String\" FROM phase_runs \
         WHERE completed_at IS NULL \
           AND COALESCE(last_heartbeat_at, started_at) < ?1",
        cutoff,
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|s| {
            PhaseRunId::new(&s)
                .map_err(|e| RepoError::NotFound(format!("corrupt phase_run id: {e}")))
        })
        .collect()
}

/// Open `phase_runs` rows that have run past a hard wall-clock `budget`,
/// REGARDLESS of heartbeat freshness.
///
/// This is the backstop the heartbeat sweeper cannot provide: a worker wedged
/// inside a still-running child (e.g. a hung `cargo` build) keeps heartbeating,
/// so `find_abandoned` never catches it. A row is over budget when
/// `completed_at IS NULL` and `started_at < now - budget` — the heartbeat is
/// deliberately ignored.
pub async fn find_over_budget(
    pool: &SqlitePool,
    budget: Duration,
    now: DateTime<Utc>,
) -> Result<Vec<PhaseRunId>, RepoError> {
    let cutoff = now - budget;
    let rows = sqlx::query_scalar!(
        "SELECT id AS \"id!: String\" FROM phase_runs \
         WHERE completed_at IS NULL \
           AND started_at < ?1",
        cutoff,
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|s| {
            PhaseRunId::new(&s)
                .map_err(|e| RepoError::NotFound(format!("corrupt phase_run id: {e}")))
        })
        .collect()
}

/// Close every still-open `phase_runs` row, stamping `completed_at`.
///
/// The daemon-crash restart-recovery primitive (Phase 9 Task 9.3 / design §5):
/// at daemon boot every `completed_at IS NULL` row belongs to a *prior* daemon
/// process — the freshly-booting daemon has spawned no worker yet. Each such
/// row is a phase run whose worker died with the old daemon; it is reconciled
/// here so `boi status` / `boi log` show it ended rather than perpetually
/// `[running]`.
///
/// Returns the number of rows closed. `synopsis` / `verdict` are left as-is
/// (the recovery pass also emits `SpecFailed{DaemonCrash}` for the owning
/// spec, which is the operator-facing signal — this UPDATE only reconciles the
/// `phase_runs` wall-clock so a stale row is not mistaken for a live one).
pub async fn close_orphaned(pool: &SqlitePool, now: DateTime<Utc>) -> Result<u64, RepoError> {
    let affected = sqlx::query!(
        "UPDATE phase_runs SET completed_at = ?1 WHERE completed_at IS NULL",
        now,
    )
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected)
}

/// Mark every still-open `phase_runs` row for a spec as canceled.
///
/// Sets `completed_at = now`, `synopsis`, and `verdict` (a `Canceled`-flavored
/// [`WorkerVerdict`]) on every row where `completed_at IS NULL AND spec_id = ?`.
/// Called by the cancel path so the dashboard's `any_open` invariant holds:
/// after this call `SELECT COUNT(*) FROM phase_runs WHERE spec_id = ? AND
/// completed_at IS NULL` = 0.
///
/// Uses the same columns as [`update_end`] — no independent "phase ends" path.
/// Returns the number of rows closed.
pub async fn cancel_open_phase_runs_for_spec(
    pool: &SqlitePool,
    spec_id: &SpecId,
    now: DateTime<Utc>,
) -> Result<u64, RepoError> {
    let sid = spec_id.as_str();
    let verdict = WorkerVerdict {
        synopsis: "phase canceled (spec cancellation)".to_owned(),
        outcome: crate::types::verdict::VerdictOutcome::Canceled,
    };
    let verdict_json = serde_json::to_value(&verdict)?;
    let synopsis = &verdict.synopsis;
    let affected = sqlx::query(
        "UPDATE phase_runs \
         SET completed_at = ?1, synopsis = ?2, verdict = ?3 \
         WHERE spec_id = ?4 AND completed_at IS NULL",
    )
    .bind(now)
    .bind(synopsis.as_str())
    .bind(verdict_json)
    .bind(sid)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected)
}

/// Mark an abandoned `phase_runs` row as closed (the sweeper's post-emit close).
///
/// The sweeper-side counterpart to [`cancel_open_phase_runs_for_task`]: after
/// [`crate::service::sweeper::Sweeper::tick`] detects an abandoned row and emits
/// its block event, it closes the row here so subsequent sweeps don't
/// re-discover it. Without this close the row stays `completed_at IS NULL`
/// forever — and every 30 s the sweeper re-emits the same `TaskBlocked`, which
/// the bus rejects (illegal `Blocked → Blocked`) in a perpetual error loop,
/// while the dashboard's `any_open` derivation reports the spec as `running`
/// indefinitely (OBS-019, OBS-025).
///
/// The UPDATE is scoped `AND completed_at IS NULL` for idempotency against the
/// benign race where a late drain or worker-side close beat the sweeper to it.
/// Stamps a `Blocked`-flavored [`WorkerVerdict`] so the row's history surfaces
/// the abandonment cause rather than a bare close. Returns the number of rows
/// closed (`0` on the race, `1` otherwise).
pub async fn mark_abandoned(
    pool: &SqlitePool,
    phase_run_id: &PhaseRunId,
    threshold: std::time::Duration,
    now: DateTime<Utc>,
) -> Result<u64, RepoError> {
    let id = phase_run_id.as_str();
    let synopsis = format!(
        "phase abandoned — worker died with heartbeat stale > {}s",
        threshold.as_secs(),
    );
    let verdict = WorkerVerdict {
        synopsis: synopsis.clone(),
        outcome: crate::types::verdict::VerdictOutcome::Blocked {
            reason: "worker abandoned (sweeper detected stale heartbeat)".to_owned(),
            error_why_fix: None,
        },
    };
    let verdict_json = serde_json::to_value(&verdict)?;
    let affected = sqlx::query(
        "UPDATE phase_runs \
         SET completed_at = ?1, synopsis = ?2, verdict = ?3 \
         WHERE id = ?4 AND completed_at IS NULL",
    )
    .bind(now)
    .bind(synopsis.as_str())
    .bind(verdict_json)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected)
}

/// Close a single still-open `phase_runs` row that blew its wall-clock budget.
///
/// The wall-clock counterpart to [`mark_abandoned`]: it stamps a
/// `Blocked`-flavored verdict whose synopsis names the budget (not a stale
/// heartbeat), so the row's history records that the phase was reaped for
/// running too long DESPITE a fresh heartbeat. Scoped `AND completed_at IS NULL`
/// for idempotency. Returns the number of rows closed (`0` on a race, else `1`).
pub async fn mark_over_budget(
    pool: &SqlitePool,
    phase_run_id: &PhaseRunId,
    budget: std::time::Duration,
    now: DateTime<Utc>,
) -> Result<u64, RepoError> {
    let id = phase_run_id.as_str();
    let synopsis = format!(
        "phase reaped — exceeded wall-clock budget of {}s (heartbeat was fresh)",
        budget.as_secs(),
    );
    let verdict = WorkerVerdict {
        synopsis: synopsis.clone(),
        outcome: crate::types::verdict::VerdictOutcome::Blocked {
            reason: "phase exceeded wall-clock budget (sweeper hard cap)".to_owned(),
            error_why_fix: None,
        },
    };
    let verdict_json = serde_json::to_value(&verdict)?;
    let affected = sqlx::query(
        "UPDATE phase_runs \
         SET completed_at = ?1, synopsis = ?2, verdict = ?3 \
         WHERE id = ?4 AND completed_at IS NULL",
    )
    .bind(now)
    .bind(synopsis.as_str())
    .bind(verdict_json)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected)
}

/// Mark every still-open `phase_runs` row for a specific task as canceled.
///
/// Task-scoped counterpart to [`cancel_open_phase_runs_for_spec`] — enforces
/// the same invariant for a single task's rows rather than the whole spec.
/// Returns the number of rows closed.
pub async fn cancel_open_phase_runs_for_task(
    pool: &SqlitePool,
    spec_id: &SpecId,
    task_id: &TaskId,
    now: DateTime<Utc>,
) -> Result<u64, RepoError> {
    let sid = spec_id.as_str();
    let tid = task_id.as_str();
    let verdict = WorkerVerdict {
        synopsis: "phase canceled (task cancellation)".to_owned(),
        outcome: crate::types::verdict::VerdictOutcome::Canceled,
    };
    let verdict_json = serde_json::to_value(&verdict)?;
    let synopsis = &verdict.synopsis;
    let affected = sqlx::query(
        "UPDATE phase_runs \
         SET completed_at = ?1, synopsis = ?2, verdict = ?3 \
         WHERE spec_id = ?4 AND task_id = ?5 AND completed_at IS NULL",
    )
    .bind(now)
    .bind(synopsis.as_str())
    .bind(verdict_json)
    .bind(sid)
    .bind(tid)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected)
}

/// Fetch a phase run by ID.
///
/// Returns [`RepoError::NotFound`] if no such row exists.
pub async fn fetch(pool: &SqlitePool, phase_run_id: &PhaseRunId) -> Result<PhaseRunRow, RepoError> {
    let id = phase_run_id.as_str();
    let row = sqlx::query_as::<_, PhaseRunRow>(SELECT_COLUMNS_FROM)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.ok_or_else(|| RepoError::NotFound(format!("phase_run {phase_run_id}")))
}

/// The most recent still-open phase run for a task, if any (folded G16.7 —
/// backs `boi status`'s `[phase=…,iter=…]`).
pub async fn fetch_latest_open_for_task(
    pool: &SqlitePool,
    spec_id: &SpecId,
    task_id: &TaskId,
) -> Result<Option<PhaseRunRow>, RepoError> {
    let sid = spec_id.as_str();
    let tid = task_id.as_str();
    let row = sqlx::query_as::<_, PhaseRunRow>(
        "SELECT id, spec_id, task_id, phase, phase_iteration, spec_version, provider, worker_id, \
                files_touched, synopsis, verdict, last_heartbeat_at, started_at, completed_at, \
                tokens_in, tokens_out \
         FROM phase_runs \
         WHERE spec_id = ?1 AND task_id = ?2 AND completed_at IS NULL \
         ORDER BY started_at DESC, phase_iteration DESC LIMIT 1",
    )
    .bind(sid)
    .bind(tid)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Every phase run for a spec, oldest first (folded G16.7 — backs `boi log`'s
/// phase history). Ordered by `started_at, phase_iteration`.
pub async fn fetch_history_for_spec(
    pool: &SqlitePool,
    spec_id: &SpecId,
) -> Result<Vec<PhaseRunRow>, RepoError> {
    let sid = spec_id.as_str();
    let rows = sqlx::query_as::<_, PhaseRunRow>(
        "SELECT id, spec_id, task_id, phase, phase_iteration, spec_version, provider, worker_id, \
                files_touched, synopsis, verdict, last_heartbeat_at, started_at, completed_at, \
                tokens_in, tokens_out \
         FROM phase_runs WHERE spec_id = ?1 \
         ORDER BY started_at, phase_iteration",
    )
    .bind(sid)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Shared `SELECT … FROM phase_runs WHERE id = ?1` column list for [`fetch`].
const SELECT_COLUMNS_FROM: &str = "SELECT id, spec_id, task_id, phase, phase_iteration, spec_version, provider, worker_id, \
            files_touched, synopsis, verdict, last_heartbeat_at, started_at, completed_at, \
            tokens_in, tokens_out \
     FROM phase_runs WHERE id = ?1";

/// Per-spec rollup of `phase_runs` — phase count only. The dashboard
/// spec-picker uses this to annotate each spec row without an
/// N-query-per-spec scan.
///
/// Per the 2026-06-01 directive the per-spec dollar total is gone; tokens
/// remain on the per-run rows for the spend-hint signal.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SpecPhaseRollup {
    /// The spec.
    pub spec_id: String,
    /// How many `phase_runs` rows the spec has.
    pub phase_count: i64,
}

/// One grouped query: every spec's phase count, keyed by spec.
///
/// This is a runtime-checked `query_as` (the [`aggregate_metrics_for_spec`]
/// precedent) — no `sqlx::query!` macro, no offline-cache regeneration.
pub async fn cost_and_count_by_spec(pool: &SqlitePool) -> Result<Vec<SpecPhaseRollup>, RepoError> {
    let rows = sqlx::query_as::<_, SpecPhaseRollup>(
        "SELECT spec_id, COUNT(*) AS phase_count \
         FROM phase_runs GROUP BY spec_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Spec-scoped aggregate over `phase_runs` rows — the per-spec metrics
/// (phase count, duration, token totals).
///
/// As of the `0002` migration (G25.1) `phase_runs` carries `tokens_in` /
/// `tokens_out`, written by [`update_end`] from the
/// `BoiEvent::PhaseCompleted` event — so this aggregate now sums real
/// token totals rather than the zeros shipped before the
/// migration. Per the 2026-06-01 directive the per-run dollar column is
/// stripped (migration 0003); tokens stay as the spend-hint signal.
///
/// `duration_ms` is sourced from `phase_runs`, not `spec_runtime`: the
/// `phase_runs` time span (`MAX(completed_at) − MIN(started_at)`) is an
/// accurate proxy and is robust to a spec with only an in-flight run.
///
/// This is a runtime-checked `query_as` (the [`tasks_for_spec`] precedent) —
/// no `sqlx::query!` macro, no offline-cache regeneration.
///
/// [`tasks_for_spec`]: crate::repo::task_runtime::tasks_for_spec
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpecPhaseMetrics {
    /// How many `phase_runs` rows the spec has — every phase execution,
    /// in-flight or completed.
    pub phases_run: i64,
    /// Wall-clock span of the spec's phase runs in milliseconds: the gap from
    /// the earliest `started_at` to the latest `completed_at`. `None` when the
    /// spec has no phase runs, or none has completed yet.
    pub duration_ms: Option<i64>,
    /// Sum of `tokens_in` across the spec's phase runs (G25.1). `0` when no
    /// completed run recorded a token count.
    pub total_tokens_in: i64,
    /// Sum of `tokens_out` across the spec's phase runs (G25.1).
    pub total_tokens_out: i64,
}

/// A single-row [`sqlx::FromRow`] target for the [`aggregate_metrics_for_spec`]
/// aggregate `SELECT`. The `julianday` delta yields fractional days; the caller
/// converts to whole milliseconds.
#[derive(Debug, Clone, sqlx::FromRow)]
struct MetricsAggRow {
    /// `COUNT(*)` of the spec's `phase_runs` rows.
    phases_run: i64,
    /// `(MAX(completed_at) − MIN(started_at))` expressed in days, or `NULL`
    /// when no row of the spec has completed.
    span_days: Option<f64>,
    /// `SUM(tokens_in)` — `NULL` when no row recorded one (G25.1).
    total_tokens_in: Option<i64>,
    /// `SUM(tokens_out)` — `NULL` when no row recorded one (G25.1).
    total_tokens_out: Option<i64>,
}

/// Aggregate the spec's `phase_runs` rows into a [`SpecPhaseMetrics`].
///
/// `phases_run` counts every row for the spec; `duration_ms` is the wall-clock
/// span from the earliest start to the latest completion; the token totals
/// are `SUM`s over the G25.1 columns. A spec with no phase runs yields
/// `phases_run: 0, duration_ms: None` and zero totals — not an error.
pub async fn aggregate_metrics_for_spec(
    pool: &SqlitePool,
    spec_id: &SpecId,
) -> Result<SpecPhaseMetrics, RepoError> {
    let sid = spec_id.as_str();
    // `julianday` returns a fractional day count; the delta is `NULL` if no
    // row has a non-NULL `completed_at`. One round trip for every aggregate.
    let row = sqlx::query_as::<_, MetricsAggRow>(
        "SELECT COUNT(*) AS phases_run, \
                julianday(MAX(completed_at)) - julianday(MIN(started_at)) AS span_days, \
                SUM(tokens_in) AS total_tokens_in, \
                SUM(tokens_out) AS total_tokens_out \
         FROM phase_runs WHERE spec_id = ?1",
    )
    .bind(sid)
    .fetch_one(pool)
    .await?;
    // 86_400_000 ms per day; round to the nearest whole millisecond.
    let duration_ms = row
        .span_days
        .map(|days| (days * 86_400_000.0).round() as i64);
    Ok(SpecPhaseMetrics {
        phases_run: row.phases_run,
        duration_ms,
        // `SUM` over zero non-NULL rows is `NULL` — present it as 0.
        total_tokens_in: row.total_tokens_in.unwrap_or(0),
        total_tokens_out: row.total_tokens_out.unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::db::connect;
    use crate::repo::spec_versions::{VersionTrigger, append_version};
    use crate::repo::specs::insert_spec;
    use crate::repo::task_runtime::insert_task;
    use crate::types::verdict::{Evidence, VerdictOutcome};
    use serde_json::json;

    /// A pool with a spec (version 1) and one task — phase_runs can FK to it.
    async fn seeded_pool() -> (SqlitePool, SpecId, TaskId) {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        let task = TaskId::new("T0000001a").unwrap();
        insert_spec(&pool, &spec, Utc::now()).await.unwrap();
        append_version(
            &pool,
            &spec,
            1,
            &json!({ "title": "demo" }),
            VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        insert_task(&pool, &task, &spec, None).await.unwrap();
        (pool, spec, task)
    }

    fn passing_verdict() -> WorkerVerdict {
        WorkerVerdict {
            synopsis: "did the work".into(),
            outcome: VerdictOutcome::Passing {
                evidence: Evidence::default(),
            },
        }
    }

    /// `update_end` with zero token figures — the common test shape for
    /// the cases that exercise the verdict/timestamp path, not the G25.1
    /// token columns. The dedicated `update_end_persists_token_columns` test
    /// passes real figures.
    async fn end_no_cost(
        pool: &SqlitePool,
        pr: &PhaseRunId,
        synopsis: &str,
        verdict: &WorkerVerdict,
        files: &[PathBuf],
        completed_at: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        update_end(pool, pr, synopsis, verdict, files, 0, 0, completed_at).await
    }

    /// `insert_start` then `update_end` round-trip — the row starts open
    /// (completed_at NULL, verdict NULL) and ends closed with the verdict.
    #[tokio::test]
    async fn start_then_end_roundtrips() {
        let (pool, spec, task) = seeded_pool().await;
        let pr = PhaseRunId::new("P0000001a").unwrap();

        insert_start(
            &pool,
            &pr,
            &spec,
            Some(&task),
            "execute",
            0,
            1,
            "claude_code",
            Some("worker-7"),
            Utc::now(),
        )
        .await
        .unwrap();

        let open = fetch(&pool, &pr).await.unwrap();
        assert!(open.is_open(), "row should be open after insert_start");
        assert!(open.verdict.is_none());
        assert_eq!(open.phase, "execute");

        end_no_cost(
            &pool,
            &pr,
            "execute done",
            &passing_verdict(),
            &[PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")],
            Utc::now(),
        )
        .await
        .unwrap();

        let closed = fetch(&pool, &pr).await.unwrap();
        assert!(!closed.is_open(), "row should be closed after update_end");
        assert_eq!(closed.synopsis, "execute done");
        assert!(matches!(
            closed.worker_verdict().unwrap(),
            Some(WorkerVerdict {
                outcome: VerdictOutcome::Passing { .. },
                ..
            }),
        ));
        let files = closed.files_touched.as_array().unwrap();
        assert_eq!(files.len(), 2);
    }

    /// `allocate_phase_run_id` generates a valid PhaseRunId and inserts the
    /// phase-start row; a second call with the SAME (spec,task,phase,iteration)
    /// surfaces the composite-UNIQUE violation immediately (no retry spin).
    ///
    /// A-SF-5: the composite-UNIQUE rejection must NOT carry the misleading
    /// "phase_run {id} or …" prefix — the generated id is fine, the iteration
    /// is the duplicate, and the message must say so.
    #[tokio::test]
    async fn allocate_phase_run_id_generates_and_guards_iteration() {
        let (pool, spec, task) = seeded_pool().await;

        let pr = allocate_phase_run_id(
            &pool,
            &spec,
            Some(&task),
            "execute",
            0,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        assert!(fetch(&pool, &pr).await.unwrap().is_open());

        // Same (spec,task,phase,iteration) — the composite UNIQUE fires.
        let err = allocate_phase_run_id(
            &pool,
            &spec,
            Some(&task),
            "execute",
            0,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap_err();
        match err {
            RepoError::Duplicate(msg) => {
                assert!(
                    !msg.contains(" or "),
                    "A-SF-5: composite-UNIQUE message must not hedge with the \
                     misleading id-or-iteration prefix, got: {msg}",
                );
                assert!(
                    msg.contains("iteration="),
                    "the message must name the composite key, got: {msg}",
                );
            }
            other => panic!("expected Duplicate, got {other:?}"),
        }
    }

    /// A duplicate `(spec_id, task_id, phase, phase_iteration)` is rejected by
    /// the UNIQUE constraint — the retry-storm guard.
    #[tokio::test]
    async fn duplicate_phase_iteration_rejected() {
        let (pool, spec, task) = seeded_pool().await;
        insert_start(
            &pool,
            &PhaseRunId::new("P0000001a").unwrap(),
            &spec,
            Some(&task),
            "execute",
            0,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap();

        // Different phase_run id, SAME (spec, task, phase, iteration).
        let err = insert_start(
            &pool,
            &PhaseRunId::new("P0000002b").unwrap(),
            &spec,
            Some(&task),
            "execute",
            0,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RepoError::Duplicate(_)), "got {err:?}");
    }

    /// `update_end` without a prior `insert_start` is `RepoError::NotFound` —
    /// it does not silently succeed against zero rows.
    #[tokio::test]
    async fn update_end_without_start_is_not_found() {
        let (pool, _spec, _task) = seeded_pool().await;
        let err = end_no_cost(
            &pool,
            &PhaseRunId::new("P0000009z").unwrap(),
            "x",
            &passing_verdict(),
            &[],
            Utc::now(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RepoError::NotFound(_)), "got {err:?}");
    }

    /// G25.1 regression: `update_end` persists the token columns the
    /// `0002` migration added. Before the migration `BoiEvent::PhaseCompleted`
    /// carried these values but `update_end` had no columns to write them to,
    /// so they were dropped on the floor — Phase 8b's `metrics` block shipped
    /// zeros. A fresh row has both NULL; after `update_end` they hold the
    /// passed figures.
    ///
    /// Per the 2026-06-01 directive the per-run dollar column is gone
    /// (migration 0003); this test now covers tokens only.
    #[tokio::test]
    async fn update_end_persists_token_columns() {
        let (pool, spec, task) = seeded_pool().await;
        let pr = PhaseRunId::new("P0000001a").unwrap();
        insert_start(
            &pool,
            &pr,
            &spec,
            Some(&task),
            "execute",
            0,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        // Open row: no token figures yet.
        let open = fetch(&pool, &pr).await.unwrap();
        assert!(open.tokens_in.is_none() && open.tokens_out.is_none());

        update_end(
            &pool,
            &pr,
            "execute done",
            &passing_verdict(),
            &[],
            12_345,
            6_789,
            Utc::now(),
        )
        .await
        .unwrap();
        let closed = fetch(&pool, &pr).await.unwrap();
        assert_eq!(closed.tokens_in, Some(12_345), "tokens_in persisted");
        assert_eq!(closed.tokens_out, Some(6_789), "tokens_out persisted");
    }

    /// `record_heartbeat` updates the liveness timestamp.
    #[tokio::test]
    async fn heartbeat_updates_timestamp() {
        let (pool, spec, task) = seeded_pool().await;
        let pr = PhaseRunId::new("P0000001a").unwrap();
        insert_start(
            &pool,
            &pr,
            &spec,
            Some(&task),
            "execute",
            0,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        assert!(fetch(&pool, &pr).await.unwrap().last_heartbeat_at.is_none());

        record_heartbeat(&pool, &pr, Utc::now()).await.unwrap();
        assert!(fetch(&pool, &pr).await.unwrap().last_heartbeat_at.is_some());
    }

    /// B-orch-1 regression: `record_heartbeat` is scoped `AND completed_at IS
    /// NULL`. A heartbeat that lands after the bus closed the row — a benign
    /// worker/bus race — must NOT mutate the finished row's `last_heartbeat_at`.
    /// Before the fix the UPDATE matched on `id` alone and silently resurrected
    /// the liveness signal on a completed run, falsifying the audited C3
    /// invariant `registry.rs` ships a comment asserting.
    #[tokio::test]
    async fn heartbeat_on_completed_run_is_a_noop() {
        let (pool, spec, task) = seeded_pool().await;
        let pr = PhaseRunId::new("P0000001a").unwrap();
        let started = Utc::now() - Duration::hours(1);
        insert_start(
            &pool,
            &pr,
            &spec,
            Some(&task),
            "execute",
            0,
            1,
            "claude_code",
            None,
            started,
        )
        .await
        .unwrap();
        // Close the row.
        end_no_cost(&pool, &pr, "done", &passing_verdict(), &[], Utc::now())
            .await
            .unwrap();
        let closed = fetch(&pool, &pr).await.unwrap();
        assert!(closed.completed_at.is_some(), "row is closed");
        assert!(
            closed.last_heartbeat_at.is_none(),
            "no heartbeat was ever recorded",
        );

        // A late heartbeat lands AFTER the close — it must not touch the row.
        record_heartbeat(&pool, &pr, Utc::now()).await.unwrap();
        assert!(
            fetch(&pool, &pr).await.unwrap().last_heartbeat_at.is_none(),
            "a heartbeat on a completed run must NOT mutate last_heartbeat_at",
        );
    }

    /// `find_abandoned` returns open rows whose liveness signal is older than
    /// the threshold, and excludes both fresh rows and completed rows.
    #[tokio::test]
    async fn find_abandoned_returns_stale_open_rows() {
        let (pool, spec, task) = seeded_pool().await;
        let now = Utc::now();
        let long_ago = now - Duration::hours(2);

        // Abandoned: started 2h ago, never heartbeated, still open.
        let stale = PhaseRunId::new("P0000st1a").unwrap();
        insert_start(
            &pool,
            &stale,
            &spec,
            Some(&task),
            "execute",
            0,
            1,
            "claude_code",
            None,
            long_ago,
        )
        .await
        .unwrap();

        // Fresh: started just now — must NOT be flagged.
        let fresh = PhaseRunId::new("P0000fr2b").unwrap();
        insert_start(
            &pool,
            &fresh,
            &spec,
            Some(&task),
            "plan",
            0,
            1,
            "claude_code",
            None,
            now,
        )
        .await
        .unwrap();

        // Completed: started 2h ago but closed — must NOT be flagged.
        let done = PhaseRunId::new("P0000dn3c").unwrap();
        insert_start(
            &pool,
            &done,
            &spec,
            Some(&task),
            "review",
            0,
            1,
            "claude_code",
            None,
            long_ago,
        )
        .await
        .unwrap();
        end_no_cost(&pool, &done, "ok", &passing_verdict(), &[], now)
            .await
            .unwrap();

        let abandoned = find_abandoned(&pool, Duration::minutes(5), now)
            .await
            .unwrap();
        assert_eq!(
            abandoned,
            vec![stale],
            "only the stale open row is abandoned"
        );
    }

    /// `fetch_latest_open_for_task` returns the most recent open run and skips
    /// completed ones; `fetch_history_for_spec` returns all runs oldest-first.
    #[tokio::test]
    async fn latest_open_and_history_queries() {
        let (pool, spec, task) = seeded_pool().await;
        let t0 = Utc::now() - Duration::minutes(10);
        let t1 = Utc::now() - Duration::minutes(5);

        // An older, completed run.
        let done = PhaseRunId::new("P0000dn1a").unwrap();
        insert_start(
            &pool,
            &done,
            &spec,
            Some(&task),
            "plan",
            0,
            1,
            "claude_code",
            None,
            t0,
        )
        .await
        .unwrap();
        end_no_cost(&pool, &done, "planned", &passing_verdict(), &[], t1)
            .await
            .unwrap();

        // A newer, still-open run.
        let open = PhaseRunId::new("P0000xp2b").unwrap();
        insert_start(
            &pool,
            &open,
            &spec,
            Some(&task),
            "execute",
            1,
            1,
            "claude_code",
            None,
            t1,
        )
        .await
        .unwrap();

        let latest = fetch_latest_open_for_task(&pool, &spec, &task)
            .await
            .unwrap()
            .expect("an open run exists");
        assert_eq!(latest.id, open.as_str());
        assert_eq!(latest.phase, "execute");

        let history = fetch_history_for_spec(&pool, &spec).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].id, done.as_str(), "history is oldest-first");
        assert_eq!(history[1].id, open.as_str());
    }

    /// `aggregate_metrics_for_spec` counts every `phase_runs` row of the spec
    /// (in-flight and completed alike), excludes another spec's rows, derives
    /// `duration_ms` from the `phase_runs` time span, and sums the G25.1
    /// cost/token columns; an empty spec yields `phases_run: 0, duration_ms:
    /// None` and zero totals, not an error.
    #[tokio::test]
    async fn aggregate_metrics_counts_runs_and_spans_duration() {
        let (pool, spec, task) = seeded_pool().await;
        // A second spec whose phase runs must NOT leak into the count.
        let other_spec = SpecId::new("S000000zz").unwrap();
        insert_spec(&pool, &other_spec, Utc::now()).await.unwrap();
        append_version(
            &pool,
            &other_spec,
            1,
            &json!({ "title": "other" }),
            VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();

        // Empty spec → 0 runs, no duration, zero token totals.
        assert_eq!(
            aggregate_metrics_for_spec(&pool, &spec).await.unwrap(),
            SpecPhaseMetrics {
                phases_run: 0,
                duration_ms: None,
                total_tokens_in: 0,
                total_tokens_out: 0,
            },
        );

        // Two completed runs (with tokens) + one still-open run → 3.
        let t0 = Utc::now() - Duration::hours(1);
        let done = PhaseRunId::new("P0000dn1a").unwrap();
        insert_start(
            &pool,
            &done,
            &spec,
            Some(&task),
            "plan",
            0,
            1,
            "claude_code",
            None,
            t0,
        )
        .await
        .unwrap();
        update_end(
            &pool,
            &done,
            "planned",
            &passing_verdict(),
            &[],
            1_000,
            300,
            t0 + Duration::hours(1),
        )
        .await
        .unwrap();
        // A second completed run with its own tokens — proves the SUM.
        let done2 = PhaseRunId::new("P0000dn2b").unwrap();
        insert_start(
            &pool,
            &done2,
            &spec,
            Some(&task),
            "execute",
            0,
            1,
            "claude_code",
            None,
            t0,
        )
        .await
        .unwrap();
        update_end(
            &pool,
            &done2,
            "executed",
            &passing_verdict(),
            &[],
            500,
            200,
            t0 + Duration::minutes(30),
        )
        .await
        .unwrap();
        let open = PhaseRunId::new("P0000pn3c").unwrap();
        insert_start(
            &pool,
            &open,
            &spec,
            Some(&task),
            "review",
            0,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap();

        // A run on the *other* spec — must not be counted for `spec`.
        let elsewhere = PhaseRunId::new("P0000ew4d").unwrap();
        insert_start(
            &pool,
            &elsewhere,
            &other_spec,
            None,
            "validate",
            0,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap();

        let metrics = aggregate_metrics_for_spec(&pool, &spec).await.unwrap();
        assert_eq!(
            metrics.phases_run, 3,
            "all 3 of spec's runs counted; the other spec's run excluded",
        );
        // span = MIN(started_at)=t0 → MAX(completed_at)=t0+1h ⇒ ~3_600_000 ms.
        // `julianday` is fractional-day precision; allow a small tolerance.
        let dur = metrics
            .duration_ms
            .expect("a completed run spans a duration");
        assert!(
            (dur - 3_600_000).abs() < 1_000,
            "duration_ms ≈ 1h, got {dur}",
        );
        // G25.1: the cost/token totals are SUMs over the two completed runs;
        // the open run contributed NULLs that `SUM` ignores.
        assert_eq!(metrics.total_tokens_in, 1_500, "1000 + 500");
        assert_eq!(metrics.total_tokens_out, 500, "300 + 200");
        assert_eq!(
            aggregate_metrics_for_spec(&pool, &other_spec)
                .await
                .unwrap()
                .phases_run,
            1,
        );
    }

    /// `cost_and_count_by_spec` returns one rollup row per spec, grouping the
    /// phase count across all of that spec's `phase_runs` rows. Spec A has 2
    /// phases; spec B has 1 phase. (Per the 2026-06-01 directive the
    /// per-spec dollar total is gone — the rollup now carries the phase count
    /// only.)
    #[tokio::test]
    async fn cost_and_count_by_spec_groups_per_spec() {
        let (pool, spec_a, task_a) = seeded_pool().await;

        // Seed spec B with its own task.
        let spec_b = SpecId::new("S0000002b").unwrap();
        let task_b = TaskId::new("T0000002b").unwrap();
        insert_spec(&pool, &spec_b, Utc::now()).await.unwrap();
        append_version(
            &pool,
            &spec_b,
            1,
            &json!({ "title": "spec-b" }),
            VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        insert_task(&pool, &task_b, &spec_b, None).await.unwrap();

        // Spec A — phase 1.
        let pr_a1 = PhaseRunId::new("P0000a11a").unwrap();
        insert_start(
            &pool,
            &pr_a1,
            &spec_a,
            Some(&task_a),
            "plan",
            0,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        update_end(
            &pool,
            &pr_a1,
            "planned",
            &passing_verdict(),
            &[],
            100,
            50,
            Utc::now(),
        )
        .await
        .unwrap();

        // Spec A — phase 2.
        let pr_a2 = PhaseRunId::new("P0000a22b").unwrap();
        insert_start(
            &pool,
            &pr_a2,
            &spec_a,
            Some(&task_a),
            "execute",
            0,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        update_end(
            &pool,
            &pr_a2,
            "executed",
            &passing_verdict(),
            &[],
            200,
            100,
            Utc::now(),
        )
        .await
        .unwrap();

        // Spec B — 1 phase.
        let pr_b1 = PhaseRunId::new("P0000b11c").unwrap();
        insert_start(
            &pool,
            &pr_b1,
            &spec_b,
            Some(&task_b),
            "plan",
            0,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        update_end(
            &pool,
            &pr_b1,
            "planned",
            &passing_verdict(),
            &[],
            80,
            40,
            Utc::now(),
        )
        .await
        .unwrap();

        let rollups = cost_and_count_by_spec(&pool).await.unwrap();

        let a = rollups
            .iter()
            .find(|r| r.spec_id == spec_a.as_str())
            .expect("spec A rollup present");
        assert_eq!(a.phase_count, 2, "spec A has 2 phases");

        let b = rollups
            .iter()
            .find(|r| r.spec_id == spec_b.as_str())
            .expect("spec B rollup present");
        assert_eq!(b.phase_count, 1, "spec B has 1 phase");
    }

    /// L2 regression: `cancel_open_phase_runs_for_spec` closes every open row
    /// for the spec and stamps a `Canceled`-flavored verdict.
    ///
    /// Invariant enforced: after `SpecCanceled`, every `phase_runs` row for the
    /// spec has `completed_at IS NOT NULL` and a `Canceled` verdict outcome.
    /// Pre-fix: the cancel path did NOT write `completed_at`, so `any_open`
    /// stayed true and the dashboard kept the spec `[running]` indefinitely.
    #[tokio::test]
    async fn cancel_open_for_spec_marks_rows_terminal() {
        let (pool, spec, task) = seeded_pool().await;
        let now = Utc::now();

        // Insert one open phase_run (simulates mid-phase task).
        let pr = PhaseRunId::new("P0000cx1a").unwrap();
        insert_start(
            &pool,
            &pr,
            &spec,
            Some(&task),
            "execute",
            0,
            1,
            "claude_code",
            Some("worker-x"),
            now,
        )
        .await
        .unwrap();
        // Verify pre-condition: the row is open.
        assert!(
            fetch(&pool, &pr).await.unwrap().is_open(),
            "row must be open before cancel",
        );

        // Fire the cancel-close.
        let closed = cancel_open_phase_runs_for_spec(&pool, &spec, Utc::now())
            .await
            .unwrap();
        assert_eq!(closed, 1, "exactly one row should have been closed");

        // Post-condition: completed_at IS NOT NULL, verdict is Canceled.
        let row = fetch(&pool, &pr).await.unwrap();
        assert!(
            row.completed_at.is_some(),
            "completed_at must be set after cancel",
        );
        let verdict = row
            .worker_verdict()
            .expect("verdict deserializes")
            .expect("verdict is present");
        assert!(
            matches!(
                verdict.outcome,
                crate::types::verdict::VerdictOutcome::Canceled
            ),
            "verdict outcome must be Canceled, got: {:?}",
            verdict.outcome,
        );

        // Second call is idempotent — no open rows remain.
        let again = cancel_open_phase_runs_for_spec(&pool, &spec, Utc::now())
            .await
            .unwrap();
        assert_eq!(again, 0, "second call closes zero rows (idempotent)");
    }

    /// L2 regression: `cancel_open_phase_runs_for_task` closes only the rows
    /// belonging to the specified task, leaving other tasks' rows untouched.
    #[tokio::test]
    async fn cancel_open_for_task_is_task_scoped() {
        let (pool, spec, task) = seeded_pool().await;

        // A second task on the same spec.
        let task2 = TaskId::new("T0000002b").unwrap();
        crate::repo::task_runtime::insert_task(&pool, &task2, &spec, None)
            .await
            .unwrap();

        let now = Utc::now();
        let pr1 = PhaseRunId::new("P0000cx2a").unwrap();
        let pr2 = PhaseRunId::new("P0000cx2b").unwrap();

        // One open run per task.
        for (pr, tid, phase) in [(&pr1, &task, "execute"), (&pr2, &task2, "plan")] {
            insert_start(
                &pool,
                pr,
                &spec,
                Some(tid),
                phase,
                0,
                1,
                "claude_code",
                None,
                now,
            )
            .await
            .unwrap();
        }

        // Cancel task1 only.
        let closed = cancel_open_phase_runs_for_task(&pool, &spec, &task, Utc::now())
            .await
            .unwrap();
        assert_eq!(closed, 1, "only task1's row closed");

        // task1's row is terminal with Canceled verdict.
        let r1 = fetch(&pool, &pr1).await.unwrap();
        assert!(r1.completed_at.is_some(), "task1 row closed");
        assert!(
            matches!(
                r1.worker_verdict().unwrap().unwrap().outcome,
                crate::types::verdict::VerdictOutcome::Canceled,
            ),
            "task1 verdict is Canceled",
        );

        // task2's row is still open.
        let r2 = fetch(&pool, &pr2).await.unwrap();
        assert!(r2.completed_at.is_none(), "task2 row still open");
    }
}
