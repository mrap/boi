//! The plan layer — design §4's second adjustment loop.
//!
//! The task-level side-chain ([`crate::service::adjustment`]) fixes a problem
//! *within* one task. The plan layer is the *cross-cutting* loop: any task may
//! file a `task_report`, and the plan layer — not the task — decides whether to
//! revise the spec's task graph. **Tasks REPORT; the plan DECIDES** (design §4).
//!
//! ## Two halves: intake and application
//!
//! - [`on_report`] (Task 5b.2) is the *intake*: a `ReportReceived` event with
//!   `blocking = true` blocks the reporting task and tells the orchestrator to
//!   run the `plan_revision` worker phase; `blocking = false` is advisory.
//! - [`apply_revision`] (Task 5b.3) is the *application*: it reads the
//!   `plan_revision` worker's [`PlanRevision`] artifact and rewrites the graph —
//!   a new append-only `spec_versions` row, `task_runtime` rows for added tasks,
//!   `task_deps` edges, and `TaskCanceled` for removed tasks.
//!
//! `plan_revision` is a spec-level worker phase (`kind = "worker"`, fixture
//! `tests/fixtures/phases/plan_revision.toml`) that is NOT in `standard.toml`'s
//! phase lists — the orchestrator inserts it dynamically when `on_report`
//! returns [`ReportOutcome::RunPlanRevision`].
//!
//! ## `PlanRevision`/`PlanEdit` are imported, never defined here
//!
//! Per erratum G13.4 the plan-revision data shapes live in
//! [`crate::types::plan`] — worker output that `runtime/` must also deserialize
//! belongs in `types/`, not `service/`. This module imports them.

use std::path::Path;

use chrono::Utc;
use sqlx::SqlitePool;

use crate::repo;
use crate::repo::db::RepoError;
use crate::repo::spec_versions::VersionTrigger;
use crate::repo::task_runtime::TaskRuntimeRow;
use crate::service::bus::{BusError, EventBus};
use crate::types::event::BoiEvent;
use crate::types::ids::{PhaseRunId, SpecId, TaskId};
use crate::types::plan::{PlanEdit, PlanRevision};
use crate::types::reasons::{BlockedReason, CancellationReason};
use crate::types::state::TaskState;

/// What [`on_report`] decided for a `task_report`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportOutcome {
    /// A blocking report: the reporting task was blocked with
    /// [`BlockedReason::PlanRevisionPending`], and the orchestrator should now
    /// run the `plan_revision` worker phase.
    RunPlanRevision {
        /// The task that filed the blocking report (it now sits `blocked`).
        reporting_task: TaskId,
    },
    /// A non-blocking, advisory report: it is already in the OTel event log;
    /// no state changed and the spec does not halt.
    Advisory,
}

/// A plan-layer operation failed.
#[derive(Debug, thiserror::Error)]
pub enum PlanLayerError {
    /// A repo-layer query failed.
    #[error("plan-layer repo query failed: {0}")]
    Repo(#[from] RepoError),
    /// An [`EventBus::emit`] failed — a rejected transition or a persist fault.
    #[error("plan-layer event emit failed: {0}")]
    Bus(#[from] BusError),
    /// The `plan_revision` worker's [`PlanRevision`] artifact is missing or
    /// malformed — a loud failure (the orchestrator turns this into a
    /// `TaskBlocked` for the reporting task; never silent — review D3).
    #[error("plan-revision artifact unusable: {0}")]
    Artifact(String),
}

/// Handle a `ReportReceived` event (design §4's plan-level loop).
///
/// Called by the orchestrator. The reporting task and the `triggered_by` of the
/// resulting block are the same task — a task's own report is what blocks it.
///
/// - `blocking = true` → emit `TaskBlocked { PlanRevisionPending }` for the
///   reporting task *here*, and return [`ReportOutcome::RunPlanRevision`] so the
///   orchestrator runs the `plan_revision` worker phase.
/// - `blocking = false` → advisory: the report is already in the OTel log via
///   the `ReportReceived` event; return [`ReportOutcome::Advisory`] with no
///   state change.
///
/// `on_report` *decides*; the orchestrator *executes* the phase — the §4
/// "tasks REPORT, plan DECIDES" split.
// `_pool` and `_payload` are deliberately unused at v1.0. `on_report` only
// decides (block-or-advisory) and emits the `TaskBlocked` — it touches no DB
// row of its own, and the `payload` already rode to OTel on the
// `ReportReceived` event. They are kept in the signature because the plan's
// Task 5b.2 pins it and the apply half (`apply_revision`, which DOES need both)
// is the natural place the planner would later thread payload-driven routing —
// dropping them now would force a signature change then. Leading-underscore
// names mark the intentional non-use (no dead-code warning, no silent drop).
pub async fn on_report(
    bus: &EventBus,
    _pool: &SqlitePool,
    spec_id: &SpecId,
    reporting_task: &TaskId,
    kind: &str,
    _payload: &serde_json::Value,
    blocking: bool,
) -> Result<ReportOutcome, PlanLayerError> {
    if !blocking {
        // Advisory — the `ReportReceived` event already carried it to OTel.
        return Ok(ReportOutcome::Advisory);
    }
    // Blocking — halt the reporting task pending the revision. The transition
    // guard inside `emit` enforces `active → blocked`; the bus DISPOSES.
    bus.emit(&BoiEvent::TaskBlocked {
        spec_id: spec_id.clone(),
        task_id: reporting_task.clone(),
        reason: BlockedReason::PlanRevisionPending {
            triggered_by: reporting_task.clone(),
            report_kind: kind.to_owned(),
        },
    })
    .await?;
    Ok(ReportOutcome::RunPlanRevision {
        reporting_task: reporting_task.clone(),
    })
}

/// Apply a plan revision (design §4's plan-level loop, application half).
///
/// Called by the orchestrator on the `plan_revision` phase's
/// `PhaseCompleted { Passing }`. `artifact` is the harness-designated path
/// `~/.boi/v2/revisions/<phase_run_id>.json` the worker wrote its
/// [`PlanRevision`] to (review D3 — the typed artifact channel).
///
/// Steps:
///
/// 1. **Read + strict-parse** the artifact into a [`PlanRevision`]. A missing or
///    malformed artifact is [`PlanLayerError::Artifact`] — loud, never silent;
///    the orchestrator turns it into a `TaskBlocked` for the reporting task.
/// 2. **Guard each `RemoveTask` by the target's current `task_runtime` state**
///    (review C5) — one illegal target must NOT abort the whole revision:
///    - `not_started` / `active` / `blocked` → a `TaskCanceled` is queued;
///    - `passing` → no-op; the work already merged, a `passing → canceled`
///      transition is illegal — a `PlanRevisionRetainedMergedTask` runtime
///      decision is recorded instead, to audit the planner's intent;
///    - `canceled` → no-op (idempotent).
/// 3. **Structural writes**: append a new `spec_versions` row
///    (`trigger = PlanRevised`), insert `task_runtime` rows for `AddTask`
///    edits, and apply `RetargetDeps` / `AddTask` `task_deps` edges.
/// 4. **Through the bus**: the step-2 `TaskCanceled`s, then the
///    `PlanRevised { spec_id, diff, trigger, trigger_meta }` event.
///
/// ## Atomicity / the crash window (review C5)
///
/// Steps 3 and 4 are sequential, but no torn state silently persists: the
/// step-2 state-guard makes every `TaskCanceled` a legal transition (`emit`
/// never aborts mid-loop on an `IllegalTransition`); a crash *between* steps 3
/// and 4 leaves the spec `running`, and design §5's daemon-crash recovery marks
/// every `running` spec `failed { DaemonCrash }` on restart — a torn revision
/// fails the whole spec loudly. `spec_versions` stays append-only (§3.0): a
/// revision only ever appends version N+1.
///
/// **Deviation from the plan (Task 5b.3 step 3 — "one transaction").** The plan
/// says step 3's writes run in "one transaction", but it also constrains
/// Phase 5b to *call Phase 3's repo functions* and add *no new `sqlx::query!`
/// macros*. Phase 3's repo functions each take `&SqlitePool` (their own implicit
/// transaction); there is no transaction-accepting variant, and adding one would
/// be a new `sqlx::query!` macro. So step 3 composes the existing repo calls
/// sequentially. The plan's own atomicity argument already leans on this: it
/// closes the crash window with §5's "crash ⇒ spec fails" rule and *"no bespoke
/// transactional outbox"*. The §5 coarse rule is what guarantees no torn
/// revision continues silently.
pub async fn apply_revision(
    bus: &EventBus,
    pool: &SqlitePool,
    spec_id: &SpecId,
    artifact: &Path,
    current_version: i64,
    triggered_by: &TaskId,
) -> Result<Vec<BoiEvent>, PlanLayerError> {
    // --- Step 1: read + strict-parse the artifact ---
    let revision = read_revision_artifact(artifact).await?;

    // The `plan_revision` phase run id — the artifact's file stem. A
    // `RemoveTask` on a `passing` target records a *runtime* decision, which
    // needs a parent `phase_run_id`; the producing run is this `plan_revision`
    // phase, whose row already exists.
    let revision_run = phase_run_id_from_artifact(artifact)?;

    // --- Step 2: classify each RemoveTask by its target's current state ---
    let mut cancellations: Vec<(TaskId, Option<TaskId>)> = Vec::new();
    for edit in &revision.edits {
        if let PlanEdit::RemoveTask {
            task_id,
            replacement,
        } = edit
        {
            let row = repo::task_runtime::fetch(pool, task_id).await?;
            let state: TaskState = parse_state(&row)?;
            match state {
                TaskState::NotStarted | TaskState::Active | TaskState::Blocked => {
                    cancellations.push((task_id.clone(), replacement.clone()));
                }
                TaskState::Passing => {
                    // `passing → canceled` is illegal (the work merged) — keep
                    // the task, but record the planner's intent for the audit
                    // trail. One illegal target must not abort the revision.
                    record_retained_merged_task(
                        bus,
                        pool,
                        spec_id,
                        &revision_run,
                        task_id,
                        replacement.as_ref(),
                    )
                    .await?;
                }
                TaskState::Canceled => { /* already canceled — idempotent no-op */ }
            }
        }
    }

    // --- Step 3: structural writes ---
    //
    // Mint a `TaskId` for every `AddTask` edit UP FRONT — the v2 snapshot must
    // carry each added task's contract keyed by its minted id (so the
    // orchestrator's `run_phase` can re-hydrate it), and `apply_structural_edits`
    // then inserts the `task_runtime` row under that same id.
    let mut added_task_ids: Vec<TaskId> = Vec::new();
    for edit in &revision.edits {
        if matches!(edit, PlanEdit::AddTask { .. }) {
            let id = TaskId::new(repo::random_id('T')).map_err(|e| {
                PlanLayerError::Artifact(format!("generated invalid task id for an AddTask: {e}"))
            })?;
            added_task_ids.push(id);
        }
    }

    let new_version = current_version + 1;
    let diff = serde_json::to_value(&revision).map_err(RepoError::from)?;
    // The v2 `spec_versions` snapshot is a FULL `{spec_contract, task_contracts}`
    // snapshot — the current snapshot PLUS each added task's contract. The
    // orchestrator's `run_phase` re-hydrates phase contracts from the snapshot
    // AT `spec_runtime.current_version`; storing only the revision DIFF here
    // would fault `run_phase` for any revision-added task (it would not appear
    // in `task_contracts`). The revision DIFF still rides on the `PlanRevised`
    // event below for the audit trail. (Phase 10 erratum — `apply_revision`
    // formerly stored the diff AS the snapshot and never advanced
    // `current_version`, so a revision-added task could never run.)
    let v2_snapshot =
        build_revised_snapshot(pool, spec_id, current_version, &revision, &added_task_ids).await?;
    repo::spec_versions::append_version(
        pool,
        spec_id,
        new_version,
        &v2_snapshot,
        VersionTrigger::PlanRevised,
        Some(serde_json::json!({ "triggered_by": triggered_by.as_str() })),
        Utc::now(),
    )
    .await?;
    // Advance the live version pointer so `run_phase` re-hydrates against the
    // v2 snapshot (which carries the added tasks' contracts).
    repo::spec_runtime::update_current_version(pool, spec_id, new_version).await?;
    apply_structural_edits(pool, spec_id, &revision, &added_task_ids).await?;

    // --- Step 4: emit the queued cancellations, the trigger unblock, then
    //     PlanRevised — collecting each into `routed` for the caller.
    //
    // `apply_revision` runs INSIDE the orchestrator's `on_plan_revision_completed`
    // handler. `bus.emit` here runs emit-Phases 1–3 (persist → observe →
    // bridge) but NOT emit-Phase 4 (notify the orchestrator) — so an event
    // emitted here is persisted but never *routed*. The orchestrator must
    // route the consequences (spawn the revision's new tasks via `PlanRevised`,
    // resume the unblocked trigger via `TaskUnblocked`, cancel removed tasks'
    // drains via `TaskCanceled`). So every routing-relevant event is collected
    // into `routed` and returned; `on_plan_revision_completed` pushes them onto
    // the orchestrator's `local` queue (the C1-correct route-after-emit).
    // (Plan defect — Phase 10 erratum: before this, `apply_revision`'s
    // `bus.emit`-ed `PlanRevised` / `TaskUnblocked` / `TaskCanceled` were
    // persisted but never routed, so a landed revision left the new tasks
    // unspawned and the trigger stuck — the spec could never complete.)
    let mut routed: Vec<BoiEvent> = Vec::new();

    // Whether the triggering task is itself one of the cancellations — a
    // revision MAY remove the very task that reported (a `scope_gap` whose
    // resolution supersedes the reporter). If so it must NOT also be unblocked
    // below: `canceled` is terminal and a `canceled → active` `TaskUnblocked`
    // is illegal.
    let trigger_was_cancelled = cancellations.iter().any(|(t, _)| t == triggered_by);
    for (task_id, replacement) in cancellations {
        let ev = BoiEvent::TaskCanceled {
            spec_id: spec_id.clone(),
            task_id,
            reason: CancellationReason::PlanRevisionCanceled {
                triggered_by: triggered_by.clone(),
                replacement_task: replacement,
            },
        };
        bus.emit(&ev).await?;
        routed.push(ev);
    }

    // Unblock the triggering task. It sits `blocked{PlanRevisionPending}` —
    // `on_report` blocked it pending exactly this revision; the revision has
    // now landed, so the report's concern is addressed and the task may
    // proceed. The orchestrator's `on_task_unblocked` resumes it at its latest
    // open phase. Skipped when the revision itself cancelled the trigger
    // (above) — `canceled` is terminal.
    if !trigger_was_cancelled {
        let ev = BoiEvent::TaskUnblocked {
            spec_id: spec_id.clone(),
            task_id: triggered_by.clone(),
        };
        bus.emit(&ev).await?;
        routed.push(ev);
    }
    let plan_revised = BoiEvent::PlanRevised {
        spec_id: spec_id.clone(),
        diff,
        trigger: "task_report".to_owned(),
        trigger_meta: serde_json::json!({
            "triggered_by": triggered_by.as_str(),
            "version": new_version,
        }),
    };
    bus.emit(&plan_revised).await?;
    routed.push(plan_revised);
    Ok(routed)
}

/// Read and strict-parse the `plan_revision` worker's artifact.
///
/// `deny_unknown_fields` on [`PlanRevision`] (G13.4) is the strict-parse
/// guarantee. A missing file or a parse error is [`PlanLayerError::Artifact`].
async fn read_revision_artifact(artifact: &Path) -> Result<PlanRevision, PlanLayerError> {
    let bytes = tokio::fs::read(artifact).await.map_err(|e| {
        PlanLayerError::Artifact(format!(
            "cannot read plan-revision artifact {}: {e}",
            artifact.display(),
        ))
    })?;
    serde_json::from_slice::<PlanRevision>(&bytes).map_err(|e| {
        PlanLayerError::Artifact(format!(
            "plan-revision artifact {} is malformed JSON: {e}",
            artifact.display(),
        ))
    })
}

/// Recover the `plan_revision` phase-run id from the artifact path.
///
/// The harness writes the artifact to `~/.boi/v2/revisions/<phase_run_id>.json`,
/// so the file stem IS the `PhaseRunId`. A path that does not yield a valid id
/// is [`PlanLayerError::Artifact`] — the artifact channel is broken.
fn phase_run_id_from_artifact(artifact: &Path) -> Result<PhaseRunId, PlanLayerError> {
    let stem = artifact
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| {
            PlanLayerError::Artifact(format!(
                "plan-revision artifact path {} has no file stem",
                artifact.display(),
            ))
        })?;
    PhaseRunId::new(stem).map_err(|e| {
        PlanLayerError::Artifact(format!(
            "plan-revision artifact stem {stem:?} is not a valid phase-run id: {e}",
        ))
    })
}

/// Build the v2 `spec_versions` snapshot for a revision — the current
/// snapshot's `{spec_contract, task_contracts}` PLUS each `AddTask`'s contract.
///
/// `added_task_ids` is the minted-id list, in `AddTask`-edit order (one id per
/// `AddTask`). The orchestrator's `run_phase` re-hydrates a phase's contract
/// from this snapshot, so every task that can run — original AND
/// revision-added — must appear in `task_contracts`. The current snapshot is
/// expected to follow the `{spec_contract, task_contracts}` convention (the
/// dispatch path / a prior revision produced it); a snapshot missing
/// `task_contracts` is a loud [`PlanLayerError::Artifact`] — never silently
/// defaulted.
async fn build_revised_snapshot(
    pool: &SqlitePool,
    spec_id: &SpecId,
    current_version: i64,
    revision: &PlanRevision,
    added_task_ids: &[TaskId],
) -> Result<serde_json::Value, PlanLayerError> {
    let mut snapshot = repo::spec_versions::fetch_snapshot(pool, spec_id, current_version).await?;
    let obj = snapshot.as_object_mut().ok_or_else(|| {
        PlanLayerError::Artifact(format!(
            "spec {spec_id} v{current_version} snapshot is not a JSON object"
        ))
    })?;
    let task_contracts = obj
        .get_mut("task_contracts")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| {
            PlanLayerError::Artifact(format!(
                "spec {spec_id} v{current_version} snapshot has no `task_contracts` object — \
                 cannot merge a revision's added tasks"
            ))
        })?;
    // One minted id per AddTask edit, in order — add each added task's
    // `{behavior, verifications}` contract to `task_contracts`. `add_idx`
    // walks `added_task_ids` in lockstep with the AddTask edits; both were
    // counted identically by the caller, so `added_task_ids.get` is always
    // `Some` — a `None` would be an internal counting bug, surfaced as a loud
    // typed error rather than a panic (`service/` forbids `.expect()`).
    let mut add_idx = 0usize;
    for edit in &revision.edits {
        if let PlanEdit::AddTask {
            behavior,
            verifications,
            blocked_by: _,
        } = edit
        {
            let task_id = added_task_ids.get(add_idx).ok_or_else(|| {
                PlanLayerError::Artifact(format!(
                    "internal: AddTask edit #{add_idx} has no minted id — \
                     id-minting and edit iteration disagree"
                ))
            })?;
            add_idx += 1;
            let contract = crate::types::context::TaskContract {
                behavior: behavior.clone(),
                verifications: verifications.clone(),
            };
            task_contracts.insert(
                task_id.as_str().to_owned(),
                serde_json::to_value(&contract).map_err(RepoError::from)?,
            );
        }
    }
    Ok(snapshot)
}

/// Apply the structural (non-`RemoveTask`) edits of a revision to the repo.
///
/// `AddTask` inserts a fresh `task_runtime` row under its pre-minted id (from
/// `added_task_ids`, in `AddTask`-edit order — the SAME ids
/// [`build_revised_snapshot`] keyed the v2 snapshot's `task_contracts` by) and
/// wires its `blocked_by` edges; `RetargetDeps` replaces a task's dependency
/// edges. `RemoveTask` is handled separately (step 2 of [`apply_revision`]).
async fn apply_structural_edits(
    pool: &SqlitePool,
    spec_id: &SpecId,
    revision: &PlanRevision,
    added_task_ids: &[TaskId],
) -> Result<(), PlanLayerError> {
    let mut add_idx = 0usize;
    for edit in &revision.edits {
        match edit {
            PlanEdit::AddTask {
                behavior: _,
                verifications: _,
                blocked_by,
            } => {
                // A new task starts `not_started`, inserted under its
                // pre-minted id (the id `build_revised_snapshot` keyed the v2
                // snapshot's contract by — so `run_phase` re-hydrates it).
                // `added_task_ids.get` is always `Some` (the caller counted
                // AddTask edits identically); a `None` is a loud typed error,
                // never a panic (`service/` forbids `.expect()`).
                let new_task = added_task_ids.get(add_idx).ok_or_else(|| {
                    PlanLayerError::Artifact(format!(
                        "internal: AddTask edit #{add_idx} has no minted id"
                    ))
                })?;
                add_idx += 1;
                repo::task_runtime::insert_task(pool, new_task, spec_id, None).await?;
                for dep in blocked_by {
                    repo::task_deps::add_dep(pool, new_task, dep).await?;
                }
            }
            PlanEdit::RetargetDeps { task_id, new_deps } => {
                // Replace the edge set: drop edges no longer wanted, add the
                // new ones. `add_dep`/`remove_dep` are both idempotent.
                let current = repo::task_deps::deps_of(pool, task_id).await?;
                for old in &current {
                    if !new_deps.contains(old) {
                        repo::task_deps::remove_dep(pool, task_id, old).await?;
                    }
                }
                for dep in new_deps {
                    repo::task_deps::add_dep(pool, task_id, dep).await?;
                }
            }
            // `RemoveTask` is handled by `apply_revision`'s step 2 state-guard.
            PlanEdit::RemoveTask { .. } => {}
        }
    }
    Ok(())
}

/// Record a `PlanRevisionRetainedMergedTask` runtime decision.
///
/// A `RemoveTask` whose target is `passing` cannot be canceled (`passing` is
/// terminal). The work already merged into integration — the decision log keeps
/// the planner's *intent* to remove it, so the audit trail is honest about what
/// the revision asked for vs. what was legal.
async fn record_retained_merged_task(
    bus: &EventBus,
    pool: &SqlitePool,
    spec_id: &SpecId,
    revision_run: &PhaseRunId,
    target: &TaskId,
    replacement: Option<&TaskId>,
) -> Result<(), PlanLayerError> {
    let decision_id = repo::allocate_decision_id(pool).await?;
    let replacement_note = match replacement {
        Some(r) => format!(" the planner named {r} as a replacement, but"),
        None => String::new(),
    };
    let decision = crate::types::decision::DecisionRecord::new_runtime(
        decision_id,
        spec_id.clone(),
        Some(revision_run.clone()),
        "PlanRevisionRetainedMergedTask".to_owned(),
        format!(
            "plan revision asked to remove task {target}, but it is already \
             `passing` (merged) — the task was retained."
        ),
        format!(
            "a `passing → canceled` transition is illegal;{replacement_note} \
             the merged work stays. Recorded so the planner's intent is auditable."
        ),
        Vec::new(),
        None,
        Utc::now(),
    )
    .map_err(|e| {
        // A constructor failure here is a programming error, not bad input.
        PlanLayerError::Artifact(format!("could not build retained-task decision: {e}"))
    })?;
    bus.emit(&BoiEvent::DecisionMade { decision }).await?;
    Ok(())
}

/// Parse a `task_runtime` row's `state` column into a typed [`TaskState`].
///
/// A value that does not parse means DB corruption — surfaced loudly as
/// [`RepoError::NotFound`], never silently treated as some default state.
fn parse_state(row: &TaskRuntimeRow) -> Result<TaskState, PlanLayerError> {
    row.state.parse::<TaskState>().map_err(|e| {
        PlanLayerError::Repo(RepoError::NotFound(format!(
            "corrupt task_runtime.state for {}: {e}",
            row.task_id,
        )))
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use chrono::Utc;
    use serde_json::json;

    use super::*;
    use crate::repo::db::connect;
    use crate::repo::phase_runs::insert_start;
    use crate::repo::spec_versions::append_version;
    use crate::repo::specs::insert_spec;
    use crate::repo::task_runtime::{insert_task, update_state};
    use crate::service::bus::NoopObserver;
    use crate::types::context::Verification;
    use crate::types::ids::PhaseRunId;

    fn spec() -> SpecId {
        SpecId::new("S0000001a").unwrap()
    }

    /// A bus over `pool` wired with the production null adapters.
    fn bus_for(pool: SqlitePool) -> EventBus {
        EventBus::new(pool, vec![Arc::new(NoopObserver)])
    }

    /// A pool with a spec (specs row + v1 snapshot + `spec_runtime`).
    ///
    /// The v1 snapshot follows the `{spec_contract, task_contracts}` convention
    /// the orchestrator's `run_phase` re-hydrates against — and which
    /// `apply_revision`'s `build_revised_snapshot` merges a revision's added
    /// tasks INTO. `task_contracts` is pre-seeded with a contract for every
    /// task id the `apply_revision` tests use, so `build_revised_snapshot`
    /// finds a base `task_contracts` object to extend.
    async fn seeded_spec() -> SqlitePool {
        let pool = connect("sqlite::memory:").await.unwrap();
        insert_spec(&pool, &spec(), Utc::now()).await.unwrap();
        let mut task_contracts = serde_json::Map::new();
        for tid in [
            "T0000001a",
            "T0000002b",
            "T000000aa",
            "T000000bb",
            "T000000cc",
        ] {
            task_contracts.insert(
                tid.to_owned(),
                json!({ "behavior": "seed task", "verifications": [] }),
            );
        }
        let snapshot = json!({
            "title": "demo",
            "spec_contract": {
                "scope": "demo",
                "workspace": "/repo",
                "base_branch": "main",
                "exclusions": [],
                "verifications": [],
                "must_emit": [],
            },
            "task_contracts": serde_json::Value::Object(task_contracts),
        });
        append_version(
            &pool,
            &spec(),
            1,
            &snapshot,
            VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        repo::spec_runtime::initialize(&pool, &spec(), 1)
            .await
            .unwrap();
        pool
    }

    /// Insert a task and drive it to `state`.
    async fn task_in_state(pool: &SqlitePool, id: &str, state: TaskState) -> TaskId {
        let task = TaskId::new(id).unwrap();
        insert_task(pool, &task, &spec(), None).await.unwrap();
        if state != TaskState::NotStarted {
            update_state(pool, &task, state, None, None, Utc::now())
                .await
                .unwrap();
        }
        task
    }

    /// A `plan_revision` phase run + the artifact path whose stem is its id.
    /// Returns the path; the file is written by the caller.
    ///
    /// `tag` is a per-test unique 5-char Crockford-base32 string — it makes the
    /// `PhaseRunId` and the temp directory unique so parallel test bodies never
    /// collide on the same artifact file.
    async fn revision_run(pool: &SqlitePool, tag: &str) -> (PhaseRunId, PathBuf) {
        let pr = PhaseRunId::new(format!("P000{tag}")).unwrap();
        insert_start(
            pool,
            &pr,
            &spec(),
            None,
            "plan_revision",
            0,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        let dir = std::env::temp_dir().join(format!("boi-v2-rev-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{pr}.json"));
        (pr, path)
    }

    /// `on_report` with `blocking = true` blocks the reporting task with
    /// `PlanRevisionPending` and returns `RunPlanRevision`.
    #[tokio::test]
    async fn test_l2_on_report_blocking_blocks_task_and_runs_revision() {
        let pool = seeded_spec().await;
        let task = task_in_state(&pool, "T0000001a", TaskState::Active).await;
        let bus = bus_for(pool.clone());

        let outcome = on_report(
            &bus,
            &pool,
            &spec(),
            &task,
            "scope_gap",
            &json!({ "detail": "needs a new task" }),
            true,
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            ReportOutcome::RunPlanRevision {
                reporting_task: task.clone(),
            },
        );
        let row = repo::task_runtime::fetch(&pool, &task).await.unwrap();
        assert_eq!(row.state, "blocked");
        let reason: BlockedReason = serde_json::from_value(row.blocked_reason.unwrap()).unwrap();
        assert!(
            matches!(&reason, BlockedReason::PlanRevisionPending { report_kind, .. } if report_kind == "scope_gap"),
            "expected PlanRevisionPending(scope_gap), got {reason:?}",
        );
    }

    /// `on_report` with `blocking = false` is advisory — no state change,
    /// returns `Advisory`.
    #[tokio::test]
    async fn test_l2_on_report_advisory_changes_no_state() {
        let pool = seeded_spec().await;
        let task = task_in_state(&pool, "T0000001a", TaskState::Active).await;
        let bus = bus_for(pool.clone());

        let outcome = on_report(
            &bus,
            &pool,
            &spec(),
            &task,
            "fyi",
            &json!({ "note": "just letting you know" }),
            false,
        )
        .await
        .unwrap();

        assert_eq!(outcome, ReportOutcome::Advisory);
        // The task is untouched — still `active`.
        let row = repo::task_runtime::fetch(&pool, &task).await.unwrap();
        assert_eq!(row.state, "active");
    }

    /// `apply_revision` with one `AddTask`: a new `spec_versions` row
    /// (version + 1), a `not_started` `task_runtime` row, `task_deps` edges,
    /// and a `PlanRevised` event.
    #[tokio::test]
    async fn test_l2_apply_revision_add_task_appends_version_and_inserts_task() {
        let pool = seeded_spec().await;
        let dep = task_in_state(&pool, "T0000001a", TaskState::Passing).await;
        let trigger = task_in_state(&pool, "T0000002b", TaskState::Blocked).await;
        let (_pr, artifact) = revision_run(&pool, "addt1").await;

        let revision = PlanRevision {
            edits: vec![PlanEdit::AddTask {
                behavior: "add the config flag".into(),
                verifications: vec![Verification::Command {
                    name: None,
                    command: "cargo test".into(),
                }],
                blocked_by: vec![dep.clone()],
            }],
        };
        std::fs::write(&artifact, serde_json::to_vec(&revision).unwrap()).unwrap();

        let observer = crate::service::bus::testkit::RecordingObserver::new();
        let bus = EventBus::new(pool.clone(), vec![Arc::new(observer.clone())]);

        apply_revision(&bus, &pool, &spec(), &artifact, 1, &trigger)
            .await
            .unwrap();

        // A new spec_versions row at version 2 — a FULL `{spec_contract,
        // task_contracts}` snapshot (NOT the raw diff), carrying the added
        // task's contract so `run_phase` can re-hydrate it (Phase 10 erratum).
        let snap = repo::spec_versions::fetch_snapshot(&pool, &spec(), 2)
            .await
            .unwrap();
        let v2_contracts = snap
            .get("task_contracts")
            .and_then(|v| v.as_object())
            .expect("v2 snapshot carries a task_contracts object");
        // v1 seeded 5 task contracts; v2 has those plus the one AddTask.
        assert_eq!(
            v2_contracts.len(),
            6,
            "v2 snapshot merges the added task's contract into task_contracts",
        );
        assert!(
            v2_contracts
                .values()
                .any(|c| c.get("behavior").and_then(|b| b.as_str()) == Some("add the config flag")),
            "the added task's contract is in the v2 snapshot",
        );
        // The spec advanced its live version pointer to v2.
        assert_eq!(
            repo::spec_runtime::fetch(&pool, &spec())
                .await
                .unwrap()
                .current_version,
            2,
            "apply_revision advances current_version to the revised snapshot",
        );

        // Exactly one new `not_started` task (the AddTask), wired to `dep`.
        let task_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task_runtime")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(task_count, 3, "dep + trigger + the added task");
        let added: Option<String> =
            sqlx::query_scalar("SELECT task_id FROM task_runtime WHERE state = 'not_started'")
                .fetch_optional(&pool)
                .await
                .unwrap();
        let added = TaskId::new(added.expect("an added not_started task")).unwrap();
        assert_eq!(
            repo::task_deps::deps_of(&pool, &added).await.unwrap(),
            vec![dep],
            "the added task's blocked_by edge is wired",
        );

        // `PlanRevised` was emitted.
        assert!(
            observer
                .seen()
                .iter()
                .any(|e| matches!(e, BoiEvent::PlanRevised { .. })),
            "apply_revision must emit PlanRevised",
        );
        // The triggering task — `blocked{PlanRevisionPending}` — is unblocked.
        assert_eq!(
            repo::task_runtime::fetch(&pool, &trigger)
                .await
                .unwrap()
                .state,
            "active",
            "a landed revision must unblock the triggering task",
        );

        std::fs::remove_file(&artifact).ok();
    }

    /// Phase-10 erratum regression: `apply_revision` UNBLOCKS the triggering
    /// task. The triggering task sits `blocked{PlanRevisionPending}` (where
    /// `on_report` left it); a landed revision addresses the report's concern,
    /// so the task must return to `active`. Before the fix it stayed `blocked`
    /// forever — the orchestrator's `on_plan_revision_completed` doc claimed
    /// `apply_revision` emitted `TaskUnblocked`, but it never did.
    #[tokio::test]
    async fn test_l2_apply_revision_unblocks_the_triggering_task() {
        let pool = seeded_spec().await;
        let trigger = task_in_state(&pool, "T0000002b", TaskState::Blocked).await;
        let (_pr, artifact) = revision_run(&pool, "tnbk7").await;
        // A revision that adds an unrelated task — it does NOT touch the
        // trigger, so the only thing that can return the trigger to `active`
        // is the explicit unblock.
        let revision = PlanRevision {
            edits: vec![PlanEdit::AddTask {
                behavior: "the prerequisite the report asked for".into(),
                verifications: vec![],
                blocked_by: vec![],
            }],
        };
        std::fs::write(&artifact, serde_json::to_vec(&revision).unwrap()).unwrap();
        let observer = crate::service::bus::testkit::RecordingObserver::new();
        let bus = EventBus::new(pool.clone(), vec![Arc::new(observer.clone())]);

        apply_revision(&bus, &pool, &spec(), &artifact, 1, &trigger)
            .await
            .unwrap();

        // A `TaskUnblocked` for the trigger was emitted...
        assert!(
            observer.seen().iter().any(|e| matches!(
                e,
                BoiEvent::TaskUnblocked { task_id, .. } if task_id == &trigger
            )),
            "apply_revision must emit TaskUnblocked for the triggering task",
        );
        // ...and it landed — the trigger is `active` again.
        assert_eq!(
            repo::task_runtime::fetch(&pool, &trigger)
                .await
                .unwrap()
                .state,
            "active",
        );

        std::fs::remove_file(&artifact).ok();
    }

    /// Phase-10 erratum corollary: a revision that itself REMOVES the
    /// triggering task does NOT also emit `TaskUnblocked` for it — `canceled`
    /// is terminal and a `canceled → active` transition is illegal. The
    /// trigger ends `canceled`, the revision succeeds (no IllegalTransition).
    #[tokio::test]
    async fn test_l2_apply_revision_removing_the_trigger_does_not_unblock_it() {
        let pool = seeded_spec().await;
        let trigger = task_in_state(&pool, "T0000002b", TaskState::Blocked).await;
        let (_pr, artifact) = revision_run(&pool, "rmtr8").await;
        // The revision removes the very task that reported.
        let revision = PlanRevision {
            edits: vec![PlanEdit::RemoveTask {
                task_id: trigger.clone(),
                replacement: None,
            }],
        };
        std::fs::write(&artifact, serde_json::to_vec(&revision).unwrap()).unwrap();
        let observer = crate::service::bus::testkit::RecordingObserver::new();
        let bus = EventBus::new(pool.clone(), vec![Arc::new(observer.clone())]);

        // No IllegalTransition — apply_revision succeeds.
        apply_revision(&bus, &pool, &spec(), &artifact, 1, &trigger)
            .await
            .unwrap();

        // The trigger is `canceled`, NOT `active` — it was removed, not unblocked.
        assert_eq!(
            repo::task_runtime::fetch(&pool, &trigger)
                .await
                .unwrap()
                .state,
            "canceled",
        );
        // No `TaskUnblocked` for a removed trigger.
        assert!(
            !observer.seen().iter().any(|e| matches!(
                e,
                BoiEvent::TaskUnblocked { task_id, .. } if task_id == &trigger
            )),
            "a revision that removes the trigger must not also unblock it",
        );

        std::fs::remove_file(&artifact).ok();
    }

    /// `apply_revision` with a `RemoveTask` on an `active` task →
    /// `TaskCanceled { PlanRevisionCanceled }`.
    #[tokio::test]
    async fn test_l2_apply_revision_remove_active_task_cancels_it() {
        let pool = seeded_spec().await;
        let victim = task_in_state(&pool, "T0000001a", TaskState::Active).await;
        let trigger = task_in_state(&pool, "T0000002b", TaskState::Blocked).await;
        let (_pr, artifact) = revision_run(&pool, "rmac2").await;

        let revision = PlanRevision {
            edits: vec![PlanEdit::RemoveTask {
                task_id: victim.clone(),
                replacement: None,
            }],
        };
        std::fs::write(&artifact, serde_json::to_vec(&revision).unwrap()).unwrap();
        let bus = bus_for(pool.clone());

        apply_revision(&bus, &pool, &spec(), &artifact, 1, &trigger)
            .await
            .unwrap();

        let row = repo::task_runtime::fetch(&pool, &victim).await.unwrap();
        assert_eq!(row.state, "canceled");
        let reason: CancellationReason =
            serde_json::from_value(row.cancellation_reason.unwrap()).unwrap();
        assert!(matches!(
            reason,
            CancellationReason::PlanRevisionCanceled { .. }
        ));

        std::fs::remove_file(&artifact).ok();
    }

    /// `apply_revision` with a `RemoveTask` on a `passing` task: NO illegal
    /// transition — the task stays `passing`, a `PlanRevisionRetainedMergedTask`
    /// decision is recorded, and the rest of the revision still lands.
    #[tokio::test]
    async fn test_l2_apply_revision_remove_passing_task_retains_and_audits() {
        let pool = seeded_spec().await;
        let merged = task_in_state(&pool, "T0000001a", TaskState::Passing).await;
        let trigger = task_in_state(&pool, "T0000002b", TaskState::Blocked).await;
        let (revision_run_id, artifact) = revision_run(&pool, "rmps3").await;

        // RemoveTask(merged) + an AddTask — the AddTask must still land.
        let revision = PlanRevision {
            edits: vec![
                PlanEdit::RemoveTask {
                    task_id: merged.clone(),
                    replacement: None,
                },
                PlanEdit::AddTask {
                    behavior: "follow-up work".into(),
                    verifications: vec![],
                    blocked_by: vec![],
                },
            ],
        };
        std::fs::write(&artifact, serde_json::to_vec(&revision).unwrap()).unwrap();
        let bus = bus_for(pool.clone());

        // No IllegalTransition error — apply_revision succeeds.
        apply_revision(&bus, &pool, &spec(), &artifact, 1, &trigger)
            .await
            .unwrap();

        // The merged task stayed `passing`.
        assert_eq!(
            repo::task_runtime::fetch(&pool, &merged)
                .await
                .unwrap()
                .state,
            "passing",
        );
        // A retained-merged-task decision was recorded against the revision run.
        let decisions = repo::fetch_by_spec(&pool, &spec()).await.unwrap();
        assert!(
            decisions
                .iter()
                .any(|d| d.title == "PlanRevisionRetainedMergedTask"
                    && d.phase_run_id.as_ref() == Some(&revision_run_id)),
            "a PlanRevisionRetainedMergedTask runtime decision must be recorded",
        );
        // The rest of the revision still landed — the AddTask created a task.
        let task_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task_runtime")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(task_count, 3, "merged + trigger + the added task");

        std::fs::remove_file(&artifact).ok();
    }

    /// `apply_revision` with a `RetargetDeps`: the task's `task_deps` edges are
    /// replaced and the FK still holds.
    #[tokio::test]
    async fn test_l2_apply_revision_retarget_deps_replaces_edges() {
        let pool = seeded_spec().await;
        let a = task_in_state(&pool, "T000000aa", TaskState::Passing).await;
        let b = task_in_state(&pool, "T000000bb", TaskState::Passing).await;
        let subject = task_in_state(&pool, "T000000cc", TaskState::NotStarted).await;
        let trigger = task_in_state(&pool, "T0000002b", TaskState::Blocked).await;
        // subject initially depends on `a`.
        repo::task_deps::add_dep(&pool, &subject, &a).await.unwrap();
        let (_pr, artifact) = revision_run(&pool, "rtgt4").await;

        // Retarget subject to depend on `b` instead.
        let revision = PlanRevision {
            edits: vec![PlanEdit::RetargetDeps {
                task_id: subject.clone(),
                new_deps: vec![b.clone()],
            }],
        };
        std::fs::write(&artifact, serde_json::to_vec(&revision).unwrap()).unwrap();
        let bus = bus_for(pool.clone());

        apply_revision(&bus, &pool, &spec(), &artifact, 1, &trigger)
            .await
            .unwrap();

        // `a` edge dropped, `b` edge added.
        assert_eq!(
            repo::task_deps::deps_of(&pool, &subject).await.unwrap(),
            vec![b],
            "deps retargeted from a to b",
        );

        std::fs::remove_file(&artifact).ok();
    }

    /// `spec_versions` is append-only across a revision: version 1 is unchanged
    /// after `apply_revision` appends version 2.
    #[tokio::test]
    async fn test_l2_apply_revision_keeps_spec_versions_append_only() {
        let pool = seeded_spec().await;
        let trigger = task_in_state(&pool, "T0000002b", TaskState::Blocked).await;
        let (_pr, artifact) = revision_run(&pool, "apnd5").await;
        let revision = PlanRevision {
            edits: vec![PlanEdit::AddTask {
                behavior: "x".into(),
                verifications: vec![],
                blocked_by: vec![],
            }],
        };
        std::fs::write(&artifact, serde_json::to_vec(&revision).unwrap()).unwrap();
        let bus = bus_for(pool.clone());

        apply_revision(&bus, &pool, &spec(), &artifact, 1, &trigger)
            .await
            .unwrap();

        // Version 1's snapshot is exactly the original `{ "title": "demo" }`.
        let v1 = repo::spec_versions::fetch_snapshot(&pool, &spec(), 1)
            .await
            .unwrap();
        assert_eq!(v1["title"], "demo", "v1 snapshot untouched by the revision");
        // Two distinct version rows now exist.
        let versions: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM spec_versions WHERE spec_id = ?1")
                .bind(spec().as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(versions, 2, "append-only — v1 and v2 both present");

        std::fs::remove_file(&artifact).ok();
    }

    /// A missing artifact → `apply_revision` returns `Err(Artifact)` — the
    /// loud failure the orchestrator turns into a `TaskBlocked` (§13.3
    /// artifact-channel failure-path test).
    #[tokio::test]
    async fn test_l3_plan_layer_missing_artifact_errors_loudly() {
        let pool = seeded_spec().await;
        let trigger = task_in_state(&pool, "T0000002b", TaskState::Blocked).await;
        let (_pr, artifact) = revision_run(&pool, "mssg6").await;
        // Deliberately do NOT write the artifact file.
        let bus = bus_for(pool.clone());

        let err = apply_revision(&bus, &pool, &spec(), &artifact, 1, &trigger)
            .await
            .unwrap_err();
        assert!(
            matches!(err, PlanLayerError::Artifact(_)),
            "a missing artifact must be a loud Artifact error, got {err:?}",
        );
        // No version 2 was appended — the revision did not partially apply.
        assert!(
            repo::spec_versions::fetch_snapshot(&pool, &spec(), 2)
                .await
                .is_err(),
            "a failed read must not append a version",
        );
    }

    /// A malformed artifact (unknown field — `deny_unknown_fields`) →
    /// `Err(Artifact)`.
    #[tokio::test]
    async fn test_l3_plan_layer_malformed_artifact_errors_loudly() {
        let pool = seeded_spec().await;
        let trigger = task_in_state(&pool, "T0000002b", TaskState::Blocked).await;
        let (_pr, artifact) = revision_run(&pool, "bdjsn").await;
        // `deny_unknown_fields` rejects the bogus key.
        std::fs::write(&artifact, br#"{"edits":[],"bogus_field":true}"#).unwrap();
        let bus = bus_for(pool.clone());

        let err = apply_revision(&bus, &pool, &spec(), &artifact, 1, &trigger)
            .await
            .unwrap_err();
        assert!(
            matches!(err, PlanLayerError::Artifact(_)),
            "a malformed artifact must be a loud Artifact error, got {err:?}",
        );

        std::fs::remove_file(&artifact).ok();
    }
}
