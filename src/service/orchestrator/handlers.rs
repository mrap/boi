//! The per-event handlers — each turns one routed `BoiEvent` into the next
//! `run_phase` / `emit_local`.
//!
//! `run_loop.rs`'s `handle_event` dispatches to these; the routing decisions
//! themselves live in [`crate::service::routing`].

use super::run_phase::{revision_artifact_path, verdict_evidence};
use super::{Orchestrator, OrchestratorError};
use crate::repo;
use crate::service::routing::{self, SpecRoute, TaskAction};
use crate::service::{plan_layer, scheduler};
use crate::types::event::BoiEvent;
use crate::types::ids::{SpecId, TaskId};
use crate::types::reasons::{CancellationReason, FailureReason};
use crate::types::state::SpecStatus;
use crate::types::verdict::{VerdictOutcome, WorkerVerdict};

impl Orchestrator {
    /// `SpecStarted` → run the first `PipelinePhase::Phase` in `spec_phases`.
    pub(super) async fn on_spec_started(
        &mut self,
        spec_id: &SpecId,
    ) -> Result<(), OrchestratorError> {
        match self.first_spec_phase() {
            Some(phase) => self.run_phase(spec_id, None, &phase).await,
            // A pipeline whose `spec_phases` opens with `<tasks>` (no entry
            // phase) — `validate_pipeline` does not forbid it, so fan out.
            None => self.spawn_ready_tasks(spec_id).await,
        }
    }

    /// A spec-level `PhaseCompleted` — route the spec pipeline.
    pub(super) async fn on_spec_phase_completed(
        &mut self,
        spec_id: &SpecId,
        phase_run_id: &crate::types::ids::PhaseRunId,
        phase: &str,
        verdict: &WorkerVerdict,
    ) -> Result<(), OrchestratorError> {
        // `plan_revision` is a dynamically-inserted phase, not in `spec_phases`;
        // its completion applies the revision rather than advancing the list.
        if phase == "plan_revision" {
            return self
                .on_plan_revision_completed(spec_id, phase_run_id, verdict)
                .await;
        }
        let route = routing::route_spec(
            &self.pool,
            spec_id,
            phase,
            verdict,
            &self.pipeline,
            &self.phases,
        )
        .await
        .map_err(|source| OrchestratorError::Routing {
            spec_id: spec_id.clone(),
            source,
        })?;
        match route {
            SpecRoute::RunSpecPhase(next) => self.run_phase(spec_id, None, &next).await,
            SpecRoute::FanOutTasks => self.spawn_ready_tasks(spec_id).await,
            SpecRoute::SpecDone => {
                let ev = BoiEvent::SpecCompleted {
                    spec_id: spec_id.clone(),
                };
                self.emit_local(ev, spec_id).await
            }
            // A spec-phase `Fail`/`Blocked` with no onward route — fail the
            // spec loudly rather than wedge it silently (review S6).
            SpecRoute::Halt => {
                tracing::error!(
                    spec_id = %spec_id, phase,
                    "spec phase halted with no route — failing the spec",
                );
                let ev = BoiEvent::SpecFailed {
                    spec_id: spec_id.clone(),
                    reason: FailureReason::PreflightFailed {
                        details: format!("spec phase `{phase}` halted: {}", verdict.synopsis),
                    },
                };
                self.emit_local(ev, spec_id).await
            }
            // G21.1: a spec-level bounded loop exceeded its cap — `route_spec`
            // already chose the typed `FailureReason`. Fail the spec loudly;
            // an uncapped `plan ↔ critique_plan` / `review` loop is the cost
            // bomb the cap exists to stop.
            SpecRoute::CapExceeded(reason) => {
                tracing::error!(
                    spec_id = %spec_id, phase,
                    "spec-level loop exceeded its iteration cap — failing the spec",
                );
                let ev = BoiEvent::SpecFailed {
                    spec_id: spec_id.clone(),
                    reason,
                };
                self.emit_local(ev, spec_id).await
            }
        }
    }

    /// A task-level `PhaseCompleted` — route the task via [`routing::route_task`].
    pub(super) async fn on_task_phase_completed(
        &mut self,
        spec_id: &SpecId,
        task_id: &TaskId,
        phase: &str,
        verdict: &WorkerVerdict,
    ) -> Result<(), OrchestratorError> {
        let action = routing::route_task(&self.pool, phase, task_id, verdict, &self.phases)
            .await
            .map_err(|source| OrchestratorError::Routing {
                spec_id: spec_id.clone(),
                source,
            })?;
        match action {
            TaskAction::RunPhase(next) => self.run_phase(spec_id, Some(task_id), &next).await,
            TaskAction::TaskPassed => {
                let ev = BoiEvent::TaskPassed {
                    spec_id: spec_id.clone(),
                    task_id: task_id.clone(),
                    evidence: verdict_evidence(verdict),
                };
                self.emit_local(ev, spec_id).await
            }
            TaskAction::TaskBlocked(reason) => {
                // Block the task (it stays visibly `blocked` in `boi status` /
                // the dashboard) AND cancel its in-flight drain. The full block
                // reason is recorded on the task row.
                let ev = BoiEvent::TaskBlocked {
                    spec_id: spec_id.clone(),
                    task_id: task_id.clone(),
                    reason,
                };
                self.emit_local(ev, spec_id).await?;
                self.cancel_task_drains(spec_id, task_id);
                // AUDIT A2 (2026-06-10): the spec is deliberately NOT failed
                // here. A verdict-routed block — `CapExceeded`,
                // `MergeConflict`, `WorkspaceUnclean`, a worker self-block —
                // is exactly the class design §6's recovery table routes to
                // the operator (`boi unblock [--reset-counter]`,
                // `boi resolve-conflict`). The earlier `SpecFailed` emission
                // (commit 2ec1fec, the cap-exhaustion-stall fix) made that
                // loop unwinnable: `failed` is terminal with no exit edge, so
                // even a perfect re-entry could never complete the spec — the
                // spec was bricked at block time. Holding the spec `running`
                // is NOT the silent stall 2ec1fec closed: the blocked task is
                // surfaced by the dashboard, `boi status` names the pending
                // recovery command (§6), and the `error!` below is loud
                // (SO S6). `all_tasks_settled` stays false while a task is
                // `blocked`, so the pipeline cannot resume past `<tasks>`
                // under it; a spec the operator will NOT revive is closed via
                // `boi fail` / `boi cancel` (both legal from `running`).
                tracing::error!(
                    spec_id = %spec_id, task_id = %task_id, phase,
                    "task blocked after phase `{phase}` — spec held `running` \
                     awaiting operator recovery (`boi unblock {task_id} \
                     [--reset-counter]`, `boi resolve-conflict {task_id}`, or \
                     `boi fail {spec_id}` to close it)",
                );
                Ok(())
            }
            // `TaskAction::Halt` is documented as reserved — v1.0 `route_task`
            // always returns `RunPhase` / `TaskPassed` / `TaskBlocked`. If
            // routing ever produces it the task would otherwise stall with no
            // signal (review B-svc-S5 — the earlier silent `Ok(())`). Fail the
            // spec loudly: a routing path that produced an unhandled `Halt` is
            // a real bug, not a benign no-op.
            TaskAction::Halt => {
                tracing::error!(
                    spec_id = %spec_id, task_id = %task_id, phase,
                    "route_task returned the reserved TaskAction::Halt — failing the spec",
                );
                let ev = BoiEvent::SpecFailed {
                    spec_id: spec_id.clone(),
                    reason: FailureReason::PreflightFailed {
                        details: format!(
                            "routing produced the unreachable TaskAction::Halt for task \
                             {task_id} after phase `{phase}` — a routing bug"
                        ),
                    },
                };
                self.emit_local(ev, spec_id).await
            }
        }
    }

    /// `TaskStarted` → run the first task phase for that task.
    pub(super) async fn on_task_started(
        &mut self,
        spec_id: &SpecId,
        task_id: &TaskId,
    ) -> Result<(), OrchestratorError> {
        // Clone the phase name first — `run_phase` needs `&mut self`.
        match self.pipeline.task_phases.first().cloned() {
            Some(first) => self.run_phase(spec_id, Some(task_id), &first).await,
            None => Ok(()), // a task-phase-less pipeline — nothing to run
        }
    }

    /// `TaskPassed` → spawn newly-unlocked tasks; if every task has settled,
    /// resume the spec pipeline at the phase after the `<tasks>` boundary.
    pub(super) async fn on_task_passed(
        &mut self,
        spec_id: &SpecId,
    ) -> Result<(), OrchestratorError> {
        self.spawn_ready_tasks(spec_id).await?;
        let settled = scheduler::all_tasks_settled(&self.pool, spec_id)
            .await
            .map_err(|e| self.scheduler_fault(spec_id, e))?;
        if settled {
            self.resume_spec_after_tasks(spec_id).await?;
        }
        Ok(())
    }

    /// `TaskCanceled` → cancel the task's drains, close its open `phase_runs`
    /// rows, then RE-EVALUATE settlement (audit A3 — 2026-06-10).
    ///
    /// A cancel can be the event that settles the spec's task set, and before
    /// this handler existed only [`Self::on_task_passed`] checked
    /// [`scheduler::all_tasks_settled`] (which counts `canceled` as settled —
    /// resume-after-cancel is the design intent, §6): canceling the LAST
    /// outstanding task closed rows and returned, no event ever resumed the
    /// pipeline, and the spec sat `running` forever, silently — the same stall
    /// class as the TaskBlocked→SpecFailed fixes (OBS-019). Three outcomes,
    /// none a silent wedge (SO S6):
    ///
    /// - the spec is already terminal (this cancel is the `SpecCanceled` /
    ///   terminal-sweep cascade, §6 recovery) → routing-only, the terminal
    ///   spec event already exists;
    /// - settled with ≥ 1 `passing` task → resume the spec pipeline past the
    ///   `<tasks>` boundary and merge the good work (the primary `boi cancel
    ///   <task-id>` use case: cancel the straggler, keep the rest);
    /// - settled with NO `passing` task (every task ended `canceled`) →
    ///   nothing to merge — emit a terminal `SpecCanceled`, propagating this
    ///   final task's `CancellationReason` (`running → canceled` and
    ///   `queued → canceled` are the legal §6 edges; a queued spec's tasks
    ///   can never be `passing`, so the resume arm is unreachable for it).
    pub(super) async fn on_task_canceled(
        &mut self,
        spec_id: &SpecId,
        task_id: &TaskId,
        reason: CancellationReason,
    ) -> Result<(), OrchestratorError> {
        self.cancel_task_drains(spec_id, task_id);
        // Close open phase_run rows for this task so the dashboard's
        // `any_open` invariant holds after cancellation.
        repo::phase_runs::cancel_open_phase_runs_for_task(
            &self.pool,
            spec_id,
            task_id,
            chrono::Utc::now(),
        )
        .await
        .map_err(|source| self.repo_fault(spec_id, source))?;

        // Already-terminal spec → this is a cascade cancel; emitting another
        // terminal event would be an illegal §6 edge. Routing-only, by design.
        let spec_row = repo::spec_runtime::fetch(&self.pool, spec_id)
            .await
            .map_err(|source| self.repo_fault(spec_id, source))?;
        let status: SpecStatus =
            spec_row
                .status
                .parse()
                .map_err(|e| OrchestratorError::Contract {
                    spec_id: spec_id.clone(),
                    detail: format!("corrupt spec_runtime.status: {e}"),
                })?;
        if matches!(
            status,
            SpecStatus::Completed | SpecStatus::Failed | SpecStatus::Canceled
        ) {
            tracing::debug!(
                spec_id = %spec_id, task_id = %task_id,
                "task canceled under an already-terminal spec — cascade, no settlement check",
            );
            return Ok(());
        }
        let settled = scheduler::all_tasks_settled(&self.pool, spec_id)
            .await
            .map_err(|e| self.scheduler_fault(spec_id, e))?;
        if !settled {
            return Ok(()); // siblings still outstanding — nothing settles yet
        }
        let any_passing = scheduler::any_task_passing(&self.pool, spec_id)
            .await
            .map_err(|e| self.scheduler_fault(spec_id, e))?;
        if any_passing {
            // ≥ 1 task passed — resume the pipeline and merge the good work.
            self.resume_spec_after_tasks(spec_id).await
        } else {
            // EVERY task ended canceled — nothing to merge. Terminate the
            // spec loudly, propagating the final task's cancellation reason.
            tracing::info!(
                spec_id = %spec_id, task_id = %task_id,
                "all tasks ended canceled — canceling the spec (nothing to merge)",
            );
            let ev = BoiEvent::SpecCanceled {
                spec_id: spec_id.clone(),
                reason,
            };
            self.emit_local(ev, spec_id).await
        }
    }

    /// Resume the spec pipeline at the phase after the `<tasks>` boundary, if
    /// the pipeline defines one — the shared settled-task-set resume used by
    /// [`Self::on_task_passed`] and [`Self::on_task_canceled`] (audit A3: both
    /// settling events must take the identical resume path).
    ///
    /// IDEMPOTENT (review M1 finding 3): the settled-check can legitimately
    /// fire on more than one event. Both production shapes persist a batch of
    /// `TaskCanceled` state flips BEFORE any of them routes — a plan revision
    /// removing ≥ 2 tasks (`apply_revision` bus-emits every cancel, then
    /// `on_plan_revision_completed` routes them one by one) and quick
    /// successive `boi cancel` commands (persisted on the control-socket
    /// task, concurrent with this loop) — so the FIRST routed event already
    /// sees the set settled and resumes, and each LATER one re-enters here
    /// under a still-`running` spec. `run_phase` is NOT idempotent (fresh
    /// `PhaseRunId`, fresh drain every call): without this guard two
    /// spec-level pipeline walks ran concurrently — duplicate `merge` drains
    /// racing libgit2 against the SAME operator repo (the OBS-030 class) and
    /// the loser's Fail marking a spec whose merge landed as terminally
    /// failed. Two checks, both under the run-loop's serialization:
    ///
    /// 1. `in_flight` — catches a resume drain SPAWNED but not yet started
    ///    (its `phase_runs` row is INSERTed by the spawned drain's
    ///    `PhaseStarted`, which may not have run yet);
    /// 2. a spec-level `phase_runs` row for the resume phase — catches a
    ///    resume that already started or finished (its drain may have left
    ///    `in_flight`). `task_id IS NULL` is load-bearing: dual-level phases
    ///    (`validate`, `review`) also have task-level rows.
    ///
    /// A skipped duplicate is logged loudly — a silent no-op here would hide
    /// the double-settle edge this guard exists for (SO S6).
    async fn resume_spec_after_tasks(&mut self, spec_id: &SpecId) -> Result<(), OrchestratorError> {
        let Some(resume) = self.phase_after_tasks() else {
            return Ok(());
        };
        let resume_in_flight = self
            .in_flight
            .values()
            .any(|run| run.spec_id == *spec_id && run.task_id.is_none() && run.phase == resume);
        let resume_already_ran = if resume_in_flight {
            true
        } else {
            repo::phase_runs::fetch_history_for_spec(&self.pool, spec_id)
                .await
                .map_err(|source| self.repo_fault(spec_id, source))?
                .iter()
                .any(|row| row.phase == resume && row.task_id.is_none())
        };
        if resume_already_ran {
            tracing::warn!(
                spec_id = %spec_id, phase = %resume,
                "post-<tasks> resume already started — skipping the duplicate \
                 (a second settling event re-triggered the resume; M1 finding 3)",
            );
            return Ok(());
        }
        self.run_phase(spec_id, None, &resume).await
    }

    /// `TaskUnblocked` → resume by re-running the task's latest open phase.
    pub(super) async fn on_task_unblocked(
        &mut self,
        spec_id: &SpecId,
        task_id: &TaskId,
    ) -> Result<(), OrchestratorError> {
        let open = repo::phase_runs::fetch_latest_open_for_task(&self.pool, spec_id, task_id)
            .await
            .map_err(|source| self.repo_fault(spec_id, source))?;
        match open {
            Some(row) => self.run_phase(spec_id, Some(task_id), &row.phase).await,
            // No open phase run — resume at the first task phase.
            None => self.on_task_started(spec_id, task_id).await,
        }
    }

    /// `PlanRevised` → spawn any newly-ready tasks the revision unlocked.
    pub(super) async fn on_plan_revised(
        &mut self,
        spec_id: &SpecId,
    ) -> Result<(), OrchestratorError> {
        self.spawn_ready_tasks(spec_id).await
    }

    /// `SpecCanceled` → cancel in-flight drains, mark open phase_runs terminal,
    /// AND cascade `TaskCanceled` to every non-terminal task (§6 recovery
    /// table; review S13).
    pub(super) async fn on_spec_canceled(
        &mut self,
        spec_id: &SpecId,
    ) -> Result<(), OrchestratorError> {
        self.cancel_spec_drains(spec_id);
        // Close every open phase_run row for this spec so `any_open` becomes
        // false and the dashboard's derived status transitions to `done`.
        repo::phase_runs::cancel_open_phase_runs_for_spec(&self.pool, spec_id, chrono::Utc::now())
            .await
            .map_err(|source| self.repo_fault(spec_id, source))?;
        let tasks = repo::task_runtime::tasks_for_spec(&self.pool, spec_id)
            .await
            .map_err(|source| self.repo_fault(spec_id, source))?;
        for row in tasks {
            // Skip already-terminal tasks — `passing`/`canceled` cannot move.
            if row.state == "passing" || row.state == "canceled" {
                continue;
            }
            let task_id = self.parse_task_id(spec_id, &row.task_id)?;
            let ev = BoiEvent::TaskCanceled {
                spec_id: spec_id.clone(),
                task_id,
                reason: CancellationReason::SpecCanceled,
            };
            self.emit_local(ev, spec_id).await?;
        }
        Ok(())
    }

    /// `ReportReceived` → the plan layer decides (a blocking report runs the
    /// `plan_revision` worker phase; an advisory one is a no-op).
    pub(super) async fn on_report_received(
        &mut self,
        spec_id: &SpecId,
        task_id: &TaskId,
        kind: &str,
        payload: &serde_json::Value,
        blocking: bool,
    ) -> Result<(), OrchestratorError> {
        let outcome = plan_layer::on_report(
            &self.bus, &self.pool, spec_id, task_id, kind, payload, blocking,
        )
        .await
        .map_err(|e| self.plan_layer_fault(spec_id, e))?;
        match outcome {
            plan_layer::ReportOutcome::RunPlanRevision { reporting_task } => {
                // Carry the reporting task forward (review B-orch-S3) so
                // `on_plan_revision_completed` names the *correct* task —
                // not a "first PlanRevisionPending task by id" scan that
                // picks wrong when two reports block concurrently.
                self.plan_revision_trigger
                    .insert(spec_id.clone(), reporting_task);
                self.run_phase(spec_id, None, "plan_revision").await
            }
            plan_layer::ReportOutcome::Advisory => Ok(()),
        }
    }

    /// The `plan_revision` phase completed — apply the revision (`Passing`) or
    /// surface its failure as a `TaskBlocked` for the reporting task.
    ///
    /// `phase_run_id` is the completing `plan_revision` run, threaded from the
    /// `PhaseCompleted` event (review B-orch-S2 — no string-scan re-derivation).
    /// The triggering task is the one `on_report_received` recorded in
    /// `plan_revision_trigger` (review B-orch-S3 — the real reporting task,
    /// not a first-by-id guess).
    async fn on_plan_revision_completed(
        &mut self,
        spec_id: &SpecId,
        phase_run_id: &crate::types::ids::PhaseRunId,
        verdict: &WorkerVerdict,
    ) -> Result<(), OrchestratorError> {
        // The task whose blocking report triggered this revision. It is
        // `blocked{PlanRevisionPending}` and stays there until this handler
        // unblocks it (a passing revision emits `TaskUnblocked` via
        // `apply_revision`'s `PlanRevised`) or blocks it loudly. `take` —
        // the revision is resolved here, the entry should not linger.
        let triggered_by = self.plan_revision_trigger.remove(spec_id);

        // A non-`Passing` plan_revision verdict: the revision worker itself
        // failed. The triggering task sits `blocked{PlanRevisionPending}` and
        // would stay there forever (review B-svc-S4 — the earlier code logged
        // `error!` then returned `Ok(())`). Surface a visible failure.
        let VerdictOutcome::Passing { .. } = &verdict.outcome else {
            tracing::error!(
                spec_id = %spec_id,
                synopsis = %verdict.synopsis,
                "plan_revision did not pass — surfacing a visible failure",
            );
            return self
                .fail_plan_revision(
                    spec_id,
                    triggered_by.as_ref(),
                    format!(
                        "the plan_revision phase did not pass ({}): the blocked task's \
                         task graph could not be revised",
                        verdict.synopsis
                    ),
                )
                .await;
        };
        // B-orch-S2: the artifact stem IS this `plan_revision` run's id — use
        // the event's `phase_run_id` directly, never a `latest_phase_run` scan.
        let artifact = revision_artifact_path(spec_id, phase_run_id)?;
        let version = repo::spec_runtime::fetch(&self.pool, spec_id)
            .await
            .map_err(|source| self.repo_fault(spec_id, source))?
            .current_version;
        // `apply_revision` needs the triggering task (it stamps the
        // `PlanRevisionCanceled` reason / the `PlanRevised` trigger_meta). If
        // the trigger was somehow not recorded, that is a real orchestrator
        // bug — fail loudly rather than guessing.
        let Some(triggered_by) = triggered_by else {
            return Err(OrchestratorError::Contract {
                spec_id: spec_id.clone(),
                detail: "plan_revision completed but no triggering task was recorded".to_owned(),
            });
        };
        match plan_layer::apply_revision(
            &self.bus,
            &self.pool,
            spec_id,
            &artifact,
            version,
            &triggered_by,
        )
        .await
        {
            // `apply_revision` `bus.emit`-ed its `TaskCanceled` / `TaskUnblocked`
            // / `PlanRevised` (emit-Phases 1–3 — persist/observe/bridge), but a
            // `bus.emit` inside a handler does NOT route the event (the C1
            // emit/notify split). The orchestrator must route the consequences
            // — spawn the revision's new tasks, resume the unblocked trigger,
            // cancel removed tasks' drains — so the returned events go onto
            // `local`, which the run-loop drains to fixpoint. They are NOT
            // re-emitted: `handle_event`'s `PlanRevised` / `TaskUnblocked` /
            // `TaskCanceled` arms are route-only (no `bus.emit` of the event
            // itself). (Phase 10 erratum — `apply_revision`'s events were
            // formerly emitted-but-unrouted; a landed revision then left the
            // new tasks unspawned and the trigger stuck.)
            Ok(routed) => {
                self.local.extend(routed);
                Ok(())
            }
            // A bad artifact — block the reporting task; never a silent stall.
            Err(e) => {
                tracing::error!(spec_id = %spec_id, error = %e, "plan revision failed");
                self.fail_plan_revision(
                    spec_id,
                    Some(&triggered_by),
                    format!("plan revision failed: {e}"),
                )
                .await
            }
        }
    }

    /// Surface a failed `plan_revision` as a visible signal (review B-svc-S4).
    ///
    /// The triggering task sits `blocked{PlanRevisionPending}` — a *recoverable*
    /// state, but a failed revision means the graph the task needs cannot be
    /// repaired, so there is no recovery. The task cannot legally re-`blocked`
    /// (a `blocked → blocked` self-edge is rejected by the transition guard) and
    /// canceling it would silently drop the escalation — so the honest, loud
    /// signal is to **fail the spec** (`running → failed`, a legal transition).
    /// The blocked task stays visibly `blocked` in `boi status`; the spec is
    /// `failed{PreflightFailed}` naming the real cause. Either way nothing
    /// stalls silently — replacing the earlier `error!`-then-`Ok(())`.
    async fn fail_plan_revision(
        &mut self,
        spec_id: &SpecId,
        triggered_by: Option<&TaskId>,
        detail: String,
    ) -> Result<(), OrchestratorError> {
        let details = match triggered_by {
            Some(task_id) => format!("plan revision for task {task_id} failed: {detail}"),
            None => format!("plan revision failed: {detail}"),
        };
        let ev = BoiEvent::SpecFailed {
            spec_id: spec_id.clone(),
            reason: FailureReason::PreflightFailed { details },
        };
        self.emit_local(ev, spec_id).await
    }
}
