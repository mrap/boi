//! The heartbeat sweeper — catches abandoned phase runs (design §5 + B7).
//!
//! A drain task pings [`repo::phase_runs::record_heartbeat`] every ~30 s while
//! a phase run is open. The sweeper periodically looks for `phase_runs` rows
//! that are still open (`completed_at IS NULL`) but whose liveness signal has
//! gone stale — a worker process that died, hung, or had its host crash. For
//! each it emits a `TaskBlocked` (or, for a spec-level phase run, a
//! `SpecFailed`) so the orphaned work surfaces for operator recovery.
//!
//! ## A concurrent producer — it uses the channel
//!
//! Like a drain task (and unlike the orchestrator's `handle_*` code — the C1
//! invariant), the sweeper is a *concurrent producer*: it `bus.emit`s then
//! `daemon_tx.send`s. The orchestrator's `TaskBlocked` arm then cancels the
//! orphaned drain.
//!
//! ## No auto-retry
//!
//! Design S10 cut the auto-failed-blocked-timeout loop. Abandonment surfaces a
//! `blocked` task for *operator* recovery (SO S6) — the sweeper never silently
//! retries. `find_abandoned`'s query is scoped `completed_at IS NULL`, so a
//! just-completed phase is never spuriously swept (review S8); a `threshold`
//! of `≥ 3×` the 30 s heartbeat interval absorbs jitter.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::repo;
use crate::repo::db::RepoError;
use crate::service::bus::EventBus;
use crate::service::registry::DaemonNotification;
use crate::types::context::SpecContract;
use crate::types::event::BoiEvent;
use crate::types::ids::{PhaseRunId, SpecId, TaskId};
use crate::types::reasons::{BlockedReason, FailureReason};
use crate::types::state::SpecStatus;

/// A sweeper pass failed.
#[derive(Debug, thiserror::Error)]
pub enum SweeperError {
    /// A repo-layer query failed (finding abandoned runs, fetching a row).
    #[error("sweeper query failed: {0}")]
    Repo(#[from] RepoError),
}

/// Reclaiming a spec's worktrees failed wholesale (audit C1).
///
/// Per-directory faults are NOT this error — they are entries in
/// [`ReclaimOutcome::failed`] so one bad directory never aborts the rest.
/// This error is reserved for "the reclaim could not run at all" (the spec
/// root unreadable, the blocking task panicked).
#[derive(Debug, thiserror::Error)]
#[error("worktree reclaim failed: {0}")]
pub struct ReclaimError(pub String);

/// What one spec-worktree reclamation did (audit C1).
///
/// Every field is operator-facing evidence: `boi clean` prints it; the
/// sweeper's auto-clean pass logs it. Empty-everything means "nothing on disk
/// for this spec" — the common already-reclaimed case.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReclaimOutcome {
    /// Worktree directories removed from disk (registration pruned too).
    pub removed: Vec<PathBuf>,
    /// Worktree directories SKIPPED because they hold uncommitted changes —
    /// never silently destroy work (audit A1's lesson). Their git
    /// registrations are left intact (the path still exists).
    pub skipped_dirty: Vec<PathBuf>,
    /// Stale `.git/worktrees/<name>` registrations pruned from the operator
    /// workspace — entries whose directory was already gone.
    pub pruned_registrations: Vec<String>,
    /// Per-directory faults (path, why). The reclaim CONTINUED past each —
    /// but every one is reported, never swallowed (SO S6).
    pub failed: Vec<(PathBuf, String)>,
}

impl ReclaimOutcome {
    /// Whether the reclaim found nothing to do (no disk state, no faults).
    pub fn is_noop(&self) -> bool {
        self.removed.is_empty()
            && self.skipped_dirty.is_empty()
            && self.pruned_registrations.is_empty()
            && self.failed.is_empty()
    }
}

/// The worktree-reclamation port (audit C1) — same shape as
/// [`PhaseExecutor`](crate::service::registry::PhaseExecutor): the trait
/// lives in `service/` (the sweeper drives it), the git-touching
/// implementation lives in `runtime/`
/// (`runtime::reclaim::SpecWorktreeReclaimer`), and `cli::boot` wires them
/// together. `service/` itself never touches git2 (LDA layer order).
#[async_trait::async_trait]
pub trait SpecReclaimer: Send + Sync {
    /// Remove the spec's worktree directories from disk and prune their git
    /// registrations in `workspace`, skipping (and reporting) any directory
    /// with uncommitted changes.
    async fn reclaim(
        &self,
        workspace: &Path,
        spec_id: &SpecId,
    ) -> Result<ReclaimOutcome, ReclaimError>;
}

/// The heartbeat sweeper.
///
/// Construct directly (the fields are `pub(crate)`-free — `boot`, Phase 9,
/// builds one with a `daemon_tx` clone). [`Sweeper::tick`] runs one pass;
/// [`Sweeper::run`] loops it on an interval until a shutdown token fires.
pub struct Sweeper {
    /// The event bus — emit-Phases 1–3 for every swept `TaskBlocked`.
    pub bus: Arc<EventBus>,
    /// A clone of the orchestrator's channel — the sweeper is a concurrent
    /// producer (emit-Phase 4), like a drain task.
    pub daemon_tx: mpsc::Sender<DaemonNotification>,
    /// The connection pool.
    pub pool: SqlitePool,
    /// A phase run open longer than this without a heartbeat is abandoned.
    pub threshold: StdDuration,
    /// A hard per-phase wall-clock budget. A phase open longer than this is
    /// reaped REGARDLESS of heartbeat freshness — the backstop for a worker
    /// wedged inside a still-heartbeating child (e.g. a hung `cargo` build that
    /// deadlocks on the package-cache lock). Without it such a zombie runs
    /// forever (SO S6 — no quiet failure).
    pub wall_clock_budget: StdDuration,
    /// The worktree-reclamation port for the auto-clean pass (audit C1;
    /// design §5 `auto_clean_canceled_after`). `None` disables the pass —
    /// only constructions with no disk to manage (unit tests, `mcp-serve`'s
    /// throwaway bus) use `None`; `cli::boot` always wires
    /// `runtime::reclaim::SpecWorktreeReclaimer`.
    pub reclaimer: Option<Arc<dyn SpecReclaimer>>,
    /// The design-§5 retention window: a failed/canceled spec older than this
    /// (by `spec_runtime.completed_at`) has its worktrees reclaimed. Read
    /// from `[worktree].auto_clean_canceled_after` (default 7 days); applied
    /// to FAILED specs too (audit C1 operator decision). `failed` is TERMINAL
    /// — §6 gives it no exit edge and the daemon refuses `boi unblock` under
    /// it (review M1 finding 2 corrected this comment's earlier "A2 keeps
    /// failed specs revivable" claim; A2 keeps *blocked tasks under a
    /// `running` spec* revivable). The window is the operator's SALVAGE grace
    /// period: the worktrees and the surviving `spec/<id>/integration` branch
    /// back the manual-merge / inspection / re-dispatch recovery.
    pub auto_clean_after: StdDuration,
    /// Minimum gap between auto-clean passes. The sweeper ticks every ~30 s;
    /// scanning every terminal spec that often is waste, so the pass runs at
    /// most once per this interval (the "cheap time check").
    pub auto_clean_pass_interval: StdDuration,
    /// When the last auto-clean pass ran — the gate for
    /// `auto_clean_pass_interval`. Construct with `Mutex::new(None)` (the
    /// first tick always runs a pass).
    pub last_auto_clean_pass: Mutex<Option<DateTime<Utc>>>,
    /// The worktree root (`~/.boi/v2/worktrees`) used to locate a reaped
    /// task's worktree and WIP-commit it before the task blocks — a dirty
    /// worktree left by a reap would otherwise bounce `boi unblock` on
    /// `workspace_unclean` (OBS-035, 2026-06-11). `None` disables the
    /// WIP-commit (unit tests with no disk); `cli::boot` always wires
    /// `runtime::worktree::default_worktree_root()`.
    pub worktree_root: Option<PathBuf>,
}

impl Sweeper {
    /// Run one sweep pass.
    ///
    /// Finds every `phase_runs` row with `completed_at IS NULL` whose liveness
    /// signal (`COALESCE(last_heartbeat_at, started_at)`) is older than
    /// `now - threshold`, and for each emits a block event. Returns the count
    /// swept.
    ///
    /// An individual emit `Err` (the task/spec already moved to a state that
    /// forbids the transition, e.g. it completed between the query and the
    /// emit) is `error!`-logged and the pass *continues* — one un-emittable
    /// block must not abort the sweep of the others, and a loud log is never a
    /// silent swallow.
    pub async fn tick(&self, now: DateTime<Utc>) -> Result<usize, SweeperError> {
        // Pass 1 — heartbeat-stale abandonment (a worker that died/hung with no
        // live heartbeat). Pass 2 — the wall-clock budget backstop (a worker
        // wedged inside a still-heartbeating child). Pass 2 is scoped to
        // exclude anything pass 1 already closed, so a single over-budget *and*
        // stale row is swept exactly once. Pass 3 — the audit-C1 worktree
        // auto-clean (design §5 `auto_clean_canceled_after`): reclaim disk
        // from failed/canceled specs older than the retention window. Pass 3
        // reports through its own logging, not the swept count — it closes no
        // phase runs.
        let mut swept = self.sweep_heartbeat_stale(now).await?;
        swept += self.sweep_over_budget(now).await?;
        self.sweep_reclaimable_worktrees(now).await?;
        Ok(swept)
    }

    /// Pass 3 — the worktree auto-clean (audit C1; design §5
    /// `auto_clean_canceled_after`).
    ///
    /// Gated to at most one pass per `auto_clean_pass_interval`. Scans
    /// `spec_runtime` for terminal `failed` / `canceled` specs whose
    /// `completed_at` is older than `auto_clean_after`, resolves each spec's
    /// workspace from its current snapshot, and asks the [`SpecReclaimer`]
    /// port to remove the worktree directories + prune the registrations.
    /// Every reclamation is logged loudly; per-spec faults are `error!`-logged
    /// and the pass continues (one broken spec must not leak every other
    /// spec's gigabytes — SO S6, no quiet failure either way).
    async fn sweep_reclaimable_worktrees(&self, now: DateTime<Utc>) -> Result<(), SweeperError> {
        let Some(reclaimer) = &self.reclaimer else {
            return Ok(()); // no port wired (unit tests / mcp-serve) — pass disabled
        };
        // The cheap time check: at most one pass per `auto_clean_pass_interval`.
        // A poisoned mutex means a previous tick panicked mid-pass — take the
        // inner value and keep sweeping (the timestamp is just a gate, and a
        // dead auto-clean would silently re-open the C1 leak).
        {
            let mut last = match self.last_auto_clean_pass.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if let Some(prev) = *last {
                if now.signed_duration_since(prev) < chrono_threshold(self.auto_clean_pass_interval)
                {
                    return Ok(());
                }
            }
            *last = Some(now);
        }

        let cutoff = now - chrono_threshold(self.auto_clean_after);
        for row in repo::spec_runtime::all(&self.pool).await? {
            // Only TERMINAL failed/canceled specs are eligible — completed
            // specs ran `teardown` on the success pipeline, and non-terminal
            // specs own live worktrees. A corrupt status is loud, never a
            // silent skip-forever.
            let status: SpecStatus = match row.status.parse() {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(
                        spec_id = %row.spec_id, status = %row.status, error = %e,
                        "auto-clean: corrupt spec status — skipping this spec",
                    );
                    continue;
                }
            };
            if !matches!(status, SpecStatus::Failed | SpecStatus::Canceled) {
                continue;
            }
            // The window keys off `completed_at` — when the spec went
            // terminal. Inside the window the worktrees stay on disk for
            // operator SALVAGE (manual merge of the integration branch,
            // inspection, re-dispatch reference) — NOT revival: a terminal
            // spec has no exit edge (§6; review M1 finding 2). A terminal
            // row missing the stamp is a data anomaly worth shouting about,
            // not deleting over.
            match row.completed_at {
                Some(t) if t <= cutoff => {}
                Some(_) => continue,
                None => {
                    tracing::warn!(
                        spec_id = %row.spec_id, status = %row.status,
                        "auto-clean: terminal spec has no completed_at — skipping",
                    );
                    continue;
                }
            }
            let spec_id = match SpecId::new(&row.spec_id) {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(
                        spec_id = %row.spec_id, error = %e,
                        "auto-clean: corrupt spec id — skipping",
                    );
                    continue;
                }
            };
            // The workspace comes from the spec's CURRENT snapshot — the
            // same `spec_contract` convention `run_phase` rehydrates from.
            let workspace = match self.spec_workspace(&spec_id, row.current_version).await {
                Ok(ws) => ws,
                Err(why) => {
                    tracing::error!(
                        spec_id = %spec_id, why = %why,
                        "auto-clean: cannot resolve the spec workspace — skipping",
                    );
                    continue;
                }
            };
            match reclaimer.reclaim(&workspace, &spec_id).await {
                Ok(outcome) => log_reclaim_outcome(&spec_id, &outcome),
                Err(e) => {
                    // One broken spec must not leak every other spec's
                    // gigabytes — loud, then continue (SO S6).
                    tracing::error!(spec_id = %spec_id, error = %e, "auto-clean: reclaim failed");
                }
            }
        }
        Ok(())
    }

    /// Resolve a spec's workspace path from its current snapshot
    /// (`spec_versions` → `spec_contract.workspace`).
    async fn spec_workspace(
        &self,
        spec_id: &SpecId,
        current_version: i64,
    ) -> Result<PathBuf, String> {
        let snapshot = repo::spec_versions::fetch_snapshot(&self.pool, spec_id, current_version)
            .await
            .map_err(|e| format!("cannot load snapshot v{current_version}: {e}"))?;
        let contract = snapshot
            .get("spec_contract")
            .ok_or_else(|| "snapshot has no `spec_contract` key".to_owned())?;
        let contract: SpecContract = serde_json::from_value(contract.clone())
            .map_err(|e| format!("`spec_contract` is malformed: {e}"))?;
        Ok(contract.workspace)
    }

    /// Pass 1 — reap rows whose heartbeat has gone stale (`find_abandoned`).
    async fn sweep_heartbeat_stale(&self, now: DateTime<Utc>) -> Result<usize, SweeperError> {
        let threshold = chrono_threshold(self.threshold);
        let abandoned = repo::phase_runs::find_abandoned(&self.pool, threshold, now).await?;
        let mut swept = 0usize;
        for phase_run_id in abandoned {
            // Fetch the row to learn whether it is a task- or spec-level phase.
            let row = repo::phase_runs::fetch(&self.pool, &phase_run_id).await?;
            let event = abandonment_event(&row, self.threshold)?;
            // Block FIRST — the orchestrator's `TaskBlocked` arm cancels the
            // orphaned drain, tearing down the worker — THEN WIP-commit, so the
            // commit races a worker that is being torn down rather than one
            // running free. Best-effort (cancellation is async — see
            // `wip_commit_reaped_worktree`); on the abandoned path the worker is
            // already presumed dead (stale heartbeat).
            if self.emit_block(&phase_run_id, event).await {
                swept += 1;
            }
            // Preserve any uncommitted work the reaped worker left behind so a
            // later `boi unblock` does not bounce on `workspace_unclean`.
            self.wip_commit_reaped_worktree(&row).await;
            // Close the `phase_runs` row regardless of the emit outcome (OBS-019).
            //
            // Without this close the row stays `completed_at IS NULL` forever:
            //   * On a successful emit the bus flips the task to `blocked`, but
            //     `TaskBlocked`/`SpecFailed` handlers don't reconcile the row —
            //     so `any_open` keeps reporting the spec as `running`.
            //   * On every subsequent sweep `find_abandoned` re-discovers the
            //     same row and the bus rejects the `Blocked → Blocked` retransition,
            //     producing the 30 s ERROR loop OBS-025 called out.
            //
            // Closing here breaks both. The UPDATE is scoped `completed_at IS
            // NULL` so a benign late-drain/worker close that beat us is a no-op.
            // A close-side error is `error!`-logged and the loop continues —
            // one un-closable row must not abort the sweep of the others (same
            // policy as the emit-error arm above).
            if let Err(e) =
                repo::phase_runs::mark_abandoned(&self.pool, &phase_run_id, self.threshold, now)
                    .await
            {
                tracing::error!(
                    phase_run_id = %phase_run_id, error = %e,
                    "sweeper could not close the abandoned phase_run row",
                );
            }
        }
        Ok(swept)
    }

    /// Pass 2 — reap rows past the hard wall-clock budget, REGARDLESS of
    /// heartbeat freshness. This is the only thing that catches a worker stuck
    /// inside a child process that keeps the heartbeat fresh (a hung `cargo`
    /// build). The failure is loud and terminal (SO S6): a typed
    /// `WallClockExceeded` block/fail, logged, and dashboard-visible.
    async fn sweep_over_budget(&self, now: DateTime<Utc>) -> Result<usize, SweeperError> {
        let budget = chrono_threshold(self.wall_clock_budget);
        let over = repo::phase_runs::find_over_budget(&self.pool, budget, now).await?;
        let mut swept = 0usize;
        for phase_run_id in over {
            let row = repo::phase_runs::fetch(&self.pool, &phase_run_id).await?;
            let elapsed = (now - row.started_at)
                .to_std()
                .unwrap_or(self.wall_clock_budget);
            let event = over_budget_event(&row, self.wall_clock_budget, elapsed)?;
            tracing::error!(
                phase_run_id = %phase_run_id, phase = %row.phase,
                budget_secs = self.wall_clock_budget.as_secs(),
                elapsed_secs = elapsed.as_secs(),
                "phase exceeded its wall-clock budget — reaping (heartbeat was fresh)",
            );
            // Block FIRST so the orchestrator cancels the worker's drain, THEN
            // WIP-commit. Critical on THIS path: the over-budget worker's
            // heartbeat was FRESH (it is alive — a wedged build), so the commit
            // can capture in-flight/partial writes; the message says so, and
            // blocking first gives the cancellation a head start.
            if self.emit_block(&phase_run_id, event).await {
                swept += 1;
            }
            // Preserve any uncommitted work the reaped worker left behind so a
            // later `boi unblock` does not bounce on `workspace_unclean`.
            self.wip_commit_reaped_worktree(&row).await;
            // Close the row (same OBS-019 rationale as the heartbeat pass) with
            // a budget-specific synopsis so the history records the real cause.
            if let Err(e) = repo::phase_runs::mark_over_budget(
                &self.pool,
                &phase_run_id,
                self.wall_clock_budget,
                now,
            )
            .await
            {
                tracing::error!(
                    phase_run_id = %phase_run_id, error = %e,
                    "sweeper could not close the over-budget phase_run row",
                );
            }
        }
        Ok(swept)
    }

    /// WIP-commit the worktree of a reaped TASK-level phase run so the work the
    /// worker left uncommitted is preserved AND the tree is clean — without
    /// this a later `boi unblock` bounces on `workspace_unclean` (the manual
    /// WIP-commit-then-unblock recovery operators did by hand on 2026-06-11).
    ///
    /// The WIP commit is a NON-correctness-bearing safety net: `boi unblock`
    /// re-runs the phase from the blocked state, so the commit only has to make
    /// the tree clean + preserve bytes for inspection — it does NOT have to be a
    /// coherent snapshot. That matters because the over-budget reap path fires
    /// while the worker is still ALIVE (its heartbeat was fresh — a wedged
    /// build), so `commit_all` (`add_all`) can capture in-flight / partial
    /// writes. The caller blocks first to start tearing the worker down, but
    /// cancellation is async, so the commit message says the capture may include
    /// partial work rather than overclaiming a clean snapshot (S6).
    ///
    /// Best-effort and never fatal: a spec-level run (no `task_id`) has no
    /// resumable worktree and is skipped; a `None` `worktree_root` (unit tests)
    /// or an absent worktree dir is skipped; a git error is logged loud (S6)
    /// but never aborts the sweep. `commit_all` makes NO commit when the tree
    /// is already clean (no empty WIP commits). git2 runs inside
    /// `spawn_blocking` (the `git2-calls-spawn-blocking` rule).
    async fn wip_commit_reaped_worktree(&self, row: &repo::PhaseRunRow) {
        let Some(worktree_root) = &self.worktree_root else {
            return; // WIP-commit disabled (no disk — unit tests).
        };
        let Some(task_id_raw) = &row.task_id else {
            return; // spec-level run — no task worktree to preserve.
        };
        let (Ok(spec_id), Ok(task_id)) = (SpecId::new(&row.spec_id), TaskId::new(task_id_raw))
        else {
            tracing::error!(
                spec_id = %row.spec_id, task_id = %task_id_raw,
                "sweeper WIP-commit: corrupt spec/task id — skipping",
            );
            return;
        };
        let worktree = crate::runtime::worktree::task_worktree(worktree_root, &spec_id, &task_id);
        if !worktree.exists() {
            tracing::debug!(worktree = %worktree.display(), "sweeper WIP-commit: no worktree on disk — skipping");
            return;
        }
        let message = format!(
            "wip: reaped phase `{}` (sweeper) — preserving the worktree for `boi unblock`; \
             MAY include the worker's in-flight/partial writes (reaped while possibly live)",
            row.phase,
        );
        let wt = worktree.clone();
        match tokio::task::spawn_blocking(move || {
            crate::runtime::worktree::commit_all(&wt, &message)
        })
        .await
        {
            Ok(Ok(Some(n))) => tracing::info!(
                worktree = %worktree.display(), files = n,
                "sweeper WIP-committed a reaped worktree — preserved for `boi unblock`",
            ),
            Ok(Ok(None)) => tracing::debug!(
                worktree = %worktree.display(),
                "sweeper WIP-commit: worktree already clean — nothing to commit",
            ),
            Ok(Err(e)) => tracing::error!(
                worktree = %worktree.display(), error = %e,
                "sweeper WIP-commit FAILED — `boi unblock` may bounce on workspace_unclean",
            ),
            Err(e) => tracing::error!(
                worktree = %worktree.display(), error = %e,
                "sweeper WIP-commit panicked in spawn_blocking",
            ),
        }
    }

    /// Emit a block/fail event (emit-Phases 1–3) then notify the orchestrator
    /// (emit-Phase 4). Returns `true` iff the bus accepted the emit. An emit
    /// `Err` (the entity already moved to a state forbidding the transition) is
    /// `error!`-logged and swallowed — one un-emittable row must not abort the
    /// sweep of the others, and a loud log is never a silent swallow (SO S6).
    async fn emit_block(&self, phase_run_id: &PhaseRunId, event: BoiEvent) -> bool {
        match self.bus.emit(&event).await {
            Ok(()) => {
                if self
                    .daemon_tx
                    .send(DaemonNotification::Event(event))
                    .await
                    .is_err()
                {
                    tracing::warn!(
                        phase_run_id = %phase_run_id,
                        "sweeper could not notify the orchestrator — channel closed",
                    );
                }
                true
            }
            Err(e) => {
                tracing::error!(
                    phase_run_id = %phase_run_id, error = %e,
                    "sweeper could not emit a block (entity already terminal?)",
                );
                false
            }
        }
    }

    /// Loop [`Sweeper::tick`] on a `tokio` interval until `shutdown` fires.
    ///
    /// An in-process BOI-daemon concern. The
    /// `shutdown: CancellationToken` (G16.4) is mandatory: without it `run`
    /// is an unstoppable infinite loop whose `daemon_tx` clone never drops,
    /// blocking the daemon's graceful shutdown (and the orchestrator's
    /// channel-closed exit).
    pub async fn run(self, interval: StdDuration, shutdown: CancellationToken) {
        let mut ticker = tokio::time::interval(interval);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(e) = self.tick(Utc::now()).await {
                        // A failed *query* (not an individual emit) — loud, and
                        // the loop continues to the next interval.
                        tracing::error!(error = %e, "sweeper tick failed");
                    }
                }
                () = shutdown.cancelled() => {
                    tracing::debug!("sweeper shutting down");
                    break;
                }
            }
        }
    }
}

/// The block event for one abandoned phase run.
///
/// A task-level run → `TaskBlocked { ProviderFailed }` (the orchestrator's
/// `TaskBlocked` arm then cancels the orphaned drain). A spec-level run has no
/// task to block → `SpecFailed { DaemonCrash }` (a spec-level phase whose
/// worker died takes the spec down).
fn abandonment_event(
    row: &repo::PhaseRunRow,
    threshold: StdDuration,
) -> Result<BoiEvent, SweeperError> {
    let spec_id = SpecId::new(&row.spec_id)
        .map_err(|e| RepoError::NotFound(format!("corrupt spec id in phase_runs: {e}")))?;
    let last_error = format!(
        "phase `{}` heartbeat stale > {}s — worker abandoned",
        row.phase,
        threshold.as_secs(),
    );
    match &row.task_id {
        Some(tid) => {
            let task_id = TaskId::new(tid)
                .map_err(|e| RepoError::NotFound(format!("corrupt task id in phase_runs: {e}")))?;
            Ok(BoiEvent::TaskBlocked {
                spec_id,
                task_id,
                reason: BlockedReason::ProviderFailed {
                    provider: row.provider.clone(),
                    last_error,
                },
            })
        }
        None => Ok(BoiEvent::SpecFailed {
            spec_id,
            reason: FailureReason::DaemonCrash,
        }),
    }
}

/// The block/fail event for a phase that blew its wall-clock budget.
///
/// A task-level run → `TaskBlocked { WallClockExceeded }`; a spec-level run (no
/// `task_id`) → `SpecFailed { WallClockExceeded }`. Distinct from the heartbeat
/// path's `ProviderFailed` / `DaemonCrash` so the operator (and dashboard) can
/// tell a hung-build wall-clock reap apart from a dead-worker abandonment.
fn over_budget_event(
    row: &repo::PhaseRunRow,
    budget: StdDuration,
    elapsed: StdDuration,
) -> Result<BoiEvent, SweeperError> {
    let spec_id = SpecId::new(&row.spec_id)
        .map_err(|e| RepoError::NotFound(format!("corrupt spec id in phase_runs: {e}")))?;
    match &row.task_id {
        Some(tid) => {
            let task_id = TaskId::new(tid)
                .map_err(|e| RepoError::NotFound(format!("corrupt task id in phase_runs: {e}")))?;
            Ok(BoiEvent::TaskBlocked {
                spec_id,
                task_id,
                reason: BlockedReason::WallClockExceeded {
                    phase: row.phase.clone(),
                    budget_secs: budget.as_secs(),
                    elapsed_secs: elapsed.as_secs(),
                },
            })
        }
        None => Ok(BoiEvent::SpecFailed {
            spec_id,
            reason: FailureReason::WallClockExceeded {
                phase: row.phase.clone(),
                budget_secs: budget.as_secs(),
                elapsed_secs: elapsed.as_secs(),
            },
        }),
    }
}

/// Convert the `std::time::Duration` threshold into the `chrono::Duration`
/// `find_abandoned` takes. A threshold too large for `chrono` is clamped to
/// the chrono max rather than panicking.
fn chrono_threshold(threshold: StdDuration) -> ChronoDuration {
    ChronoDuration::from_std(threshold).unwrap_or(ChronoDuration::MAX)
}

/// Log one auto-clean reclamation, loudly (SO S6 — every reclaim visible,
/// every skip/fault shouted). A no-op outcome (the common already-reclaimed
/// case) logs nothing — there was nothing on disk to report.
fn log_reclaim_outcome(spec_id: &SpecId, outcome: &ReclaimOutcome) {
    if outcome.is_noop() {
        return;
    }
    if !outcome.removed.is_empty() || !outcome.pruned_registrations.is_empty() {
        tracing::info!(
            spec_id = %spec_id,
            removed = ?outcome.removed,
            pruned_registrations = ?outcome.pruned_registrations,
            "auto-clean: reclaimed worktrees past the retention window",
        );
    }
    for path in &outcome.skipped_dirty {
        tracing::warn!(
            spec_id = %spec_id, path = %path.display(),
            "auto-clean: SKIPPED dirty worktree (uncommitted changes) — \
             commit/stash or remove it manually",
        );
    }
    for (path, why) in &outcome.failed {
        tracing::error!(
            spec_id = %spec_id, path = %path.display(), why = %why,
            "auto-clean: could not reclaim path",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::db::connect;
    use crate::repo::phase_runs::{insert_start, record_heartbeat, update_end};
    use crate::repo::spec_versions::{VersionTrigger, append_version};
    use crate::repo::specs::insert_spec;
    use crate::repo::task_runtime::{insert_task, update_state};
    use crate::service::bus::EventBus;
    use crate::service::bus::testkit::RecordingObserver;
    use crate::types::ids::PhaseRunId;
    use crate::types::state::TaskState;
    use crate::types::verdict::{Evidence, VerdictOutcome, WorkerVerdict};

    fn spec() -> SpecId {
        SpecId::new("S0000001a").unwrap()
    }
    fn task() -> TaskId {
        TaskId::new("T0000001a").unwrap()
    }

    /// A pool with a spec (v1 snapshot + `spec_runtime`) and one `active` task
    /// — `active` so an abandonment `TaskBlocked` (`active → blocked`) is legal.
    async fn seeded() -> SqlitePool {
        let pool = connect("sqlite::memory:").await.unwrap();
        insert_spec(&pool, &spec(), Utc::now()).await.unwrap();
        append_version(
            &pool,
            &spec(),
            1,
            &serde_json::json!({ "title": "demo" }),
            VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        repo::spec_runtime::initialize(&pool, &spec(), 1)
            .await
            .unwrap();
        insert_task(&pool, &task(), &spec(), None).await.unwrap();
        update_state(&pool, &task(), TaskState::Active, None, None, Utc::now())
            .await
            .unwrap();
        pool
    }

    /// A sweeper over `pool` with a 5-minute threshold; returns it plus the
    /// channel receiver and the bus's recorder.
    fn sweeper_for(
        pool: SqlitePool,
    ) -> (
        Sweeper,
        mpsc::Receiver<DaemonNotification>,
        RecordingObserver,
    ) {
        let recorder = RecordingObserver::new();
        let bus = Arc::new(EventBus::new(
            pool.clone(),
            vec![Arc::new(recorder.clone())],
        ));
        let (tx, rx) = mpsc::channel(16);
        let sweeper = Sweeper {
            bus,
            daemon_tx: tx,
            pool,
            threshold: StdDuration::from_secs(300),
            // A wall-clock budget far larger than the heartbeat threshold so the
            // existing heartbeat-pass tests are unaffected; the budget-specific
            // tests build their own sweeper with a tight budget.
            wall_clock_budget: StdDuration::from_secs(3600),
            // No reclaimer — the heartbeat/budget unit tests manage no disk;
            // the auto-clean pass is covered by `test_l3_reclaim_*`.
            reclaimer: None,
            auto_clean_after: StdDuration::from_secs(7 * 24 * 60 * 60),
            auto_clean_pass_interval: StdDuration::ZERO,
            last_auto_clean_pass: Mutex::new(None),
            worktree_root: None,
        };
        (sweeper, rx, recorder)
    }

    fn passing_verdict() -> WorkerVerdict {
        WorkerVerdict {
            synopsis: "ok".into(),
            outcome: VerdictOutcome::Passing {
                evidence: Evidence::default(),
            },
        }
    }

    /// An open `phase_runs` row with a stale `last_heartbeat_at` is swept:
    /// `tick` returns 1 and emits one `TaskBlocked`.
    #[tokio::test]
    async fn test_l2_stale_open_run_is_swept() {
        let pool = seeded().await;
        let now = Utc::now();
        let long_ago = now - ChronoDuration::hours(2);
        let pr = PhaseRunId::new("P0000st1a").unwrap();
        // Started 2h ago; one heartbeat, also 2h ago → stale.
        insert_start(
            &pool,
            &pr,
            &spec(),
            Some(&task()),
            "execute",
            0,
            1,
            "claude_code",
            None,
            long_ago,
        )
        .await
        .unwrap();
        record_heartbeat(&pool, &pr, long_ago).await.unwrap();

        let (sweeper, mut rx, recorder) = sweeper_for(pool.clone());
        let swept = sweeper.tick(now).await.unwrap();
        assert_eq!(swept, 1, "the one stale open run is swept");

        // It emitted a TaskBlocked — observed AND sent on the channel.
        assert!(
            recorder
                .seen()
                .iter()
                .any(|e| matches!(e, BoiEvent::TaskBlocked { .. })),
            "the sweep must emit a TaskBlocked",
        );
        let notified = rx
            .try_recv()
            .expect("the sweeper notified the orchestrator");
        assert!(matches!(
            notified,
            DaemonNotification::Event(BoiEvent::TaskBlocked { .. })
        ));
        // The task is now `blocked` (the bus's persist flipped it).
        assert_eq!(
            repo::task_runtime::fetch(&pool, &task())
                .await
                .unwrap()
                .state,
            "blocked",
        );
    }

    /// A *completed* row with a stale heartbeat is NOT swept — `find_abandoned`
    /// is scoped `completed_at IS NULL` (review S8).
    #[tokio::test]
    async fn test_l2_completed_run_with_stale_heartbeat_is_not_swept() {
        let pool = seeded().await;
        let now = Utc::now();
        let long_ago = now - ChronoDuration::hours(2);
        let pr = PhaseRunId::new("P0000dn2b").unwrap();
        insert_start(
            &pool,
            &pr,
            &spec(),
            Some(&task()),
            "execute",
            0,
            1,
            "claude_code",
            None,
            long_ago,
        )
        .await
        .unwrap();
        // Closed — even though its heartbeat is ancient.
        update_end(&pool, &pr, "done", &passing_verdict(), &[], 0, 0, long_ago)
            .await
            .unwrap();

        let (sweeper, _rx, _rec) = sweeper_for(pool);
        assert_eq!(
            sweeper.tick(now).await.unwrap(),
            0,
            "a completed row is never swept",
        );
    }

    /// A fresh-heartbeat open run is untouched — `tick` sweeps 0.
    #[tokio::test]
    async fn test_l2_fresh_heartbeat_run_is_not_swept() {
        let pool = seeded().await;
        let now = Utc::now();
        let pr = PhaseRunId::new("P0000fr3c").unwrap();
        // Started just now → liveness signal is fresh.
        insert_start(
            &pool,
            &pr,
            &spec(),
            Some(&task()),
            "execute",
            0,
            1,
            "claude_code",
            None,
            now,
        )
        .await
        .unwrap();
        record_heartbeat(&pool, &pr, now).await.unwrap();

        let (sweeper, _rx, _rec) = sweeper_for(pool);
        assert_eq!(
            sweeper.tick(now).await.unwrap(),
            0,
            "a fresh-heartbeat run is not swept",
        );
    }

    /// **OBS-019 regression** — a phase_run with `last_heartbeat_at = NULL`
    /// (worker died before its first heartbeat) and an old `started_at` is
    /// detected by `find_abandoned` via the `COALESCE(last_heartbeat_at,
    /// started_at)` fallback, but the structural gap is downstream of the
    /// emit: the sweeper never closes the `phase_runs` row, so on every
    /// subsequent tick the same row is re-discovered, the bus rejects the
    /// `Blocked → Blocked` transition, and the spec stays "stuck running"
    /// forever (the dashboard's `any_open` derivation keeps reporting
    /// `running` while any phase_run is open).
    ///
    /// The fix must (a) close the swept row's `completed_at` after a
    /// successful emit AND (b) make a second tick a no-op (the row drops out
    /// of `find_abandoned` because `completed_at IS NOT NULL`).
    #[tokio::test]
    async fn test_l2_obs019_null_heartbeat_and_old_started_at_closes_phase_run() {
        let pool = seeded().await;
        let now = Utc::now();
        // started 400s ago (past the test sweeper's 300s threshold), NEVER
        // heartbeated — the OBS-019 fingerprint.
        let started_at = now - ChronoDuration::seconds(400);
        let pr = PhaseRunId::new("P0000bs5e").unwrap();
        insert_start(
            &pool,
            &pr,
            &spec(),
            Some(&task()),
            "execute",
            0,
            1,
            "claude_code",
            None,
            started_at,
        )
        .await
        .unwrap();
        // NB: NO `record_heartbeat` — `last_heartbeat_at` stays NULL. This is
        //     the scenario the OBS-019 diagnosis pinpoints.

        // Sanity: pre-tick the row is open with a NULL heartbeat.
        let pre = repo::phase_runs::fetch(&pool, &pr).await.unwrap();
        assert!(pre.completed_at.is_none(), "pre-tick: row is open");
        assert!(
            pre.last_heartbeat_at.is_none(),
            "pre-tick: last_heartbeat_at IS NULL (OBS-019 fingerprint)",
        );

        let (sweeper, mut rx, _recorder) = sweeper_for(pool.clone());

        // First tick: finds the row via COALESCE fallback, emits TaskBlocked.
        let swept_first = sweeper.tick(now).await.unwrap();
        assert_eq!(
            swept_first, 1,
            "the COALESCE(last_heartbeat_at, started_at) fallback finds the row",
        );
        // Drain the orchestrator notification so the channel stays open.
        rx.try_recv().ok();

        // (a) The structural gap fix: the row must be closed after the sweep.
        let post = repo::phase_runs::fetch(&pool, &pr).await.unwrap();
        assert!(
            post.completed_at.is_some(),
            "OBS-019: the swept phase_run must be closed (completed_at IS NOT \
             NULL) — pre-fix this stays NULL and the spec sits 'running' forever",
        );

        // (b) A second tick a moment later is a no-op — the closed row drops
        //     out of `find_abandoned`, so the every-30s `Blocked → Blocked`
        //     error loop (OBS-025) no longer fires.
        let later = now + ChronoDuration::seconds(30);
        let swept_second = sweeper.tick(later).await.unwrap();
        assert_eq!(
            swept_second, 0,
            "a re-tick finds no abandoned rows — the closed row is invisible \
             to `find_abandoned` (completed_at IS NULL filter)",
        );
    }

    /// A stale *spec-level* abandoned run (no `task_id`) is swept as a
    /// `SpecFailed` — there is no task to block.
    #[tokio::test]
    async fn test_l2_stale_spec_level_run_sweeps_to_spec_failed() {
        let pool = seeded().await;
        // The spec must be `running` for `running → failed` to be legal.
        repo::spec_runtime::update_status(
            &pool,
            &spec(),
            crate::types::state::SpecStatus::Running,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        let now = Utc::now();
        let long_ago = now - ChronoDuration::hours(2);
        let pr = PhaseRunId::new("P0000sp4d").unwrap();
        // A spec-level phase run (task_id = None).
        insert_start(
            &pool,
            &pr,
            &spec(),
            None,
            "plan",
            0,
            1,
            "claude_code",
            None,
            long_ago,
        )
        .await
        .unwrap();

        let (sweeper, _rx, recorder) = sweeper_for(pool.clone());
        assert_eq!(sweeper.tick(now).await.unwrap(), 1);
        assert!(
            recorder
                .seen()
                .iter()
                .any(|e| matches!(e, BoiEvent::SpecFailed { .. })),
            "a spec-level abandoned run sweeps to SpecFailed",
        );
    }

    /// A sweeper with a generous heartbeat threshold (so the heartbeat pass
    /// never fires) but a TIGHT wall-clock `budget` — isolates the wall-clock
    /// backstop under test.
    fn sweeper_with_budget(
        pool: SqlitePool,
        budget: StdDuration,
    ) -> (
        Sweeper,
        mpsc::Receiver<DaemonNotification>,
        RecordingObserver,
    ) {
        let recorder = RecordingObserver::new();
        let bus = Arc::new(EventBus::new(
            pool.clone(),
            vec![Arc::new(recorder.clone())],
        ));
        let (tx, rx) = mpsc::channel(16);
        let sweeper = Sweeper {
            bus,
            daemon_tx: tx,
            pool,
            // Heartbeat threshold huge → only the wall-clock pass can fire.
            threshold: StdDuration::from_secs(86_400),
            wall_clock_budget: budget,
            // No reclaimer — this constructor exercises the wall-clock pass.
            reclaimer: None,
            auto_clean_after: StdDuration::from_secs(7 * 24 * 60 * 60),
            auto_clean_pass_interval: StdDuration::ZERO,
            last_auto_clean_pass: Mutex::new(None),
            worktree_root: None,
        };
        (sweeper, rx, recorder)
    }

    /// **The zombie case** — a phase whose `started_at` is past the wall-clock
    /// budget but whose heartbeat is FRESH (a worker wedged inside a still-
    /// heartbeating child, e.g. a hung `cargo` build) MUST be reaped. The
    /// heartbeat pass alone would let it run forever (SO S6 violation); the
    /// wall-clock backstop catches it, emits a typed `WallClockExceeded` block,
    /// and closes the row.
    #[tokio::test]
    async fn test_l2_over_budget_with_fresh_heartbeat_is_reaped() {
        let pool = seeded().await;
        let now = Utc::now();
        // Started 25 min ago — past a 20-min budget — but heartbeating NOW.
        let started_at = now - ChronoDuration::minutes(25);
        let pr = PhaseRunId::new("P0000wc1a").unwrap();
        insert_start(
            &pool,
            &pr,
            &spec(),
            Some(&task()),
            "execute",
            0,
            1,
            "claude_code",
            None,
            started_at,
        )
        .await
        .unwrap();
        // A FRESH heartbeat — the heartbeat sweeper would NEVER catch this.
        record_heartbeat(&pool, &pr, now).await.unwrap();

        let (sweeper, mut rx, recorder) =
            sweeper_with_budget(pool.clone(), StdDuration::from_secs(20 * 60));
        let swept = sweeper.tick(now).await.unwrap();
        assert_eq!(
            swept, 1,
            "the over-budget zombie is reaped despite a fresh heartbeat"
        );

        // A typed WallClockExceeded TaskBlocked was emitted (loud + dashboard-visible).
        assert!(
            recorder.seen().iter().any(|e| matches!(
                e,
                BoiEvent::TaskBlocked {
                    reason: BlockedReason::WallClockExceeded { .. },
                    ..
                }
            )),
            "the reap emits a typed WallClockExceeded block",
        );
        let notified = rx
            .try_recv()
            .expect("the sweeper notified the orchestrator");
        assert!(matches!(
            notified,
            DaemonNotification::Event(BoiEvent::TaskBlocked {
                reason: BlockedReason::WallClockExceeded { .. },
                ..
            })
        ));

        // The row is closed — a re-tick is a no-op (no perpetual error loop).
        let post = repo::phase_runs::fetch(&pool, &pr).await.unwrap();
        assert!(post.completed_at.is_some(), "the reaped row is closed");
        assert_eq!(
            sweeper.tick(now).await.unwrap(),
            0,
            "a re-tick finds nothing — the closed row drops out",
        );
    }

    /// A normal in-budget phase with a fresh heartbeat is NOT reaped by the
    /// wall-clock backstop — the budget only fires past `started_at + budget`.
    #[tokio::test]
    async fn test_l2_in_budget_phase_is_not_reaped() {
        let pool = seeded().await;
        let now = Utc::now();
        // Started 5 min ago — well inside a 20-min budget.
        let started_at = now - ChronoDuration::minutes(5);
        let pr = PhaseRunId::new("P0000wc2b").unwrap();
        insert_start(
            &pool,
            &pr,
            &spec(),
            Some(&task()),
            "execute",
            0,
            1,
            "claude_code",
            None,
            started_at,
        )
        .await
        .unwrap();
        record_heartbeat(&pool, &pr, now).await.unwrap();

        let (sweeper, _rx, recorder) =
            sweeper_with_budget(pool.clone(), StdDuration::from_secs(20 * 60));
        assert_eq!(
            sweeper.tick(now).await.unwrap(),
            0,
            "an in-budget phase is never reaped",
        );
        assert!(
            recorder.seen().is_empty(),
            "no block event for an in-budget phase",
        );
        let post = repo::phase_runs::fetch(&pool, &pr).await.unwrap();
        assert!(post.completed_at.is_none(), "the in-budget row stays open");
    }

    /// A `PhaseRunRow` fixture for the WIP-commit tests.
    fn phase_run_row(task_id: Option<TaskId>) -> repo::PhaseRunRow {
        repo::PhaseRunRow {
            id: "P0000001a".to_string(),
            spec_id: spec().as_str().to_string(),
            task_id: task_id.map(|t| t.as_str().to_string()),
            phase: "execute".to_string(),
            phase_iteration: 0,
            spec_version: 1,
            provider: "claude_code".to_string(),
            worker_id: None,
            files_touched: serde_json::json!([]),
            synopsis: String::new(),
            verdict: None,
            last_heartbeat_at: None,
            started_at: Utc::now(),
            completed_at: None,
            tokens_in: None,
            tokens_out: None,
        }
    }

    /// A unique temp directory under the system temp root. Local to this
    /// module so `src/service` never imports `crate::cli` (the layer-dep lint)
    /// — mirrors the `runtime::git_ops` test helper.
    fn temp_root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("boi-sweeper-{}-{tag}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    /// Init a git repo at `worktree` with one seed commit, then leave an
    /// uncommitted (dirty) change behind — the state a reaped worker leaves.
    fn init_dirty_worktree(worktree: &Path) {
        std::fs::create_dir_all(worktree).unwrap();
        let repo = git2::Repository::init(worktree).unwrap();
        std::fs::write(worktree.join("seed.txt"), "seed").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new("seed.txt")).unwrap();
        idx.write().unwrap();
        let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("boi", "boi@localhost").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "seed", &tree, &[])
            .unwrap();
        // A dirty, uncommitted change — the worker's in-progress work.
        std::fs::write(worktree.join("work.txt"), "in-progress work").unwrap();
    }

    /// A reaped TASK-level worktree is WIP-committed, so the worker's
    /// uncommitted work is preserved AND the tree is clean — a later
    /// `boi unblock` no longer bounces on `workspace_unclean` (OBS-035).
    #[tokio::test]
    async fn test_l2_wip_commit_cleans_a_reaped_task_worktree() {
        let root = temp_root("clean").join("worktrees");
        let worktree = crate::runtime::worktree::task_worktree(&root, &spec(), &task());
        init_dirty_worktree(&worktree);
        let repo = git2::Repository::open(&worktree).unwrap();
        assert!(
            !repo.statuses(None).unwrap().is_empty(),
            "precondition: the reaped worktree is dirty",
        );

        let pool = connect("sqlite::memory:").await.unwrap();
        let (mut sweeper, _rx, _rec) = sweeper_for(pool);
        sweeper.worktree_root = Some(root.clone());

        sweeper
            .wip_commit_reaped_worktree(&phase_run_row(Some(task())))
            .await;

        // The worktree is clean — the dirty work was WIP-committed, not lost.
        assert!(
            repo.statuses(None).unwrap().is_empty(),
            "the worktree is clean after the WIP-commit",
        );
        assert!(
            worktree.join("work.txt").exists(),
            "the worker's in-progress file is preserved",
        );
    }

    /// A spec-level reap (no `task_id`) has no resumable worktree — the
    /// WIP-commit is a no-op and never panics.
    #[tokio::test]
    async fn test_l2_wip_commit_skips_a_spec_level_reap() {
        let root = temp_root("spec-level").join("worktrees");
        let pool = connect("sqlite::memory:").await.unwrap();
        let (mut sweeper, _rx, _rec) = sweeper_for(pool);
        sweeper.worktree_root = Some(root);
        // task_id = None ⇒ no worktree to touch; must not panic.
        sweeper
            .wip_commit_reaped_worktree(&phase_run_row(None))
            .await;
    }

    /// An absent worktree on disk is a quiet no-op (best-effort) — never a
    /// panic or a failed sweep.
    #[tokio::test]
    async fn test_l2_wip_commit_skips_an_absent_worktree() {
        let root = temp_root("absent").join("worktrees"); // nothing created under it
        let pool = connect("sqlite::memory:").await.unwrap();
        let (mut sweeper, _rx, _rec) = sweeper_for(pool);
        sweeper.worktree_root = Some(root);
        sweeper
            .wip_commit_reaped_worktree(&phase_run_row(Some(task())))
            .await;
    }
}
