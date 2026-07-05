//! The orchestrator run-loop (Task 5a.3a) + the `handle_event` dispatch.
//!
//! The run-loop is the most central function in `service/` — it must never
//! silently swallow a fault (review C2): every `handle_*` `Err` routes through
//! `on_fault`, and the channel-closed exit logs (a silent loop exit is itself a
//! quiet failure).

use super::{Orchestrator, OrchestratorError};
use crate::service::bus::BusError;
use crate::service::registry::{DaemonNotification, DrainStatus};
use crate::types::event::BoiEvent;
use crate::types::ids::{PhaseRunId, SpecId, TaskId};
use crate::types::reasons::{BlockedReason, FailureReason};

impl Orchestrator {
    /// The orchestrator run-loop (Task 5a.3a).
    ///
    /// Each turn: (1) drain the `handle`-emitted `local` queue to fixpoint,
    /// then (2) take exactly one channel item. Every `handle_*` `Err` routes
    /// through `on_fault` — a loud `error!` plus a `SpecFailed` for the
    /// affected spec. A closed channel logs and exits. A `handle` *panic* (not
    /// `Err`) unwinds this task — Phase 9, which spawns `run`, MUST own the
    /// `JoinHandle` and surface the panic.
    pub async fn run(mut self) {
        loop {
            // (1) handle-emitted events to fixpoint — never the channel (C1).
            while let Some(ev) = self.local.pop_front() {
                if let Err(e) = self.handle_event(ev).await {
                    self.on_fault(e).await;
                }
            }
            // (2) one channel item.
            match self.daemon_rx.recv().await {
                Some(DaemonNotification::Event(ev)) => {
                    if let Err(e) = self.handle_event(ev).await {
                        self.on_fault(e).await;
                    }
                }
                Some(DaemonNotification::DrainTerminated {
                    phase_run_id,
                    status,
                }) => {
                    if let Err(e) = self.handle_drain_terminated(phase_run_id, status).await {
                        self.on_fault(e).await;
                    }
                }
                None => {
                    tracing::error!("daemon channel closed — orchestrator exiting");
                    break;
                }
            }
        }
    }

    /// Handle one fault: `error!` it, then `SpecFailed` the affected spec.
    ///
    /// One orchestrator drives all specs, so a fault fails only the *affected*
    /// spec (the [`OrchestratorError`] carries `spec_id`) — siblings continue.
    ///
    /// The `failed` reason is [`FailureReason::PreflightFailed`] carrying the
    /// fault's `Display` string (review B-orch-S1). An orchestration fault is
    /// NOT a daemon crash — the earlier `DaemonCrash` reuse cited a
    /// non-existent "`FailureReason` has no orchestration variant" constraint,
    /// but `PreflightFailed { details }` exists, is already used for the
    /// spec-`Halt` case, and lets the reason *name the real cause* instead of
    /// mislabelling every routing/contract/repo fault a daemon crash.
    ///
    /// If the `SpecFailed` emit itself fails the response depends on *why*
    /// (review B-svc-S6): a [`BusError::IllegalTransition`] means the spec is
    /// already terminal (a sibling fault already failed it) — benign, a
    /// `debug!`. A [`BusError::Persist`] is a real DB fault — the spec is
    /// NOT known-failed and the audit log will not show it; that is loud
    /// (`error!`), never quietly dropped as if handled.
    pub(super) async fn on_fault(&mut self, fault: OrchestratorError) {
        let spec_id = fault.spec_id().clone();
        tracing::error!(spec_id = %spec_id, error = %fault, "orchestration fault");
        // `PreflightFailed` names the real cause — see the fn doc (B-orch-S1).
        let failed = BoiEvent::SpecFailed {
            spec_id: spec_id.clone(),
            reason: FailureReason::PreflightFailed {
                details: format!("orchestration fault: {fault}"),
            },
        };
        match self.bus.emit(&failed).await {
            Ok(()) => self.local.push_back(failed),
            // The spec is already terminal — a sibling fault won the race.
            // Benign: the spec IS failed, just not by this emit.
            Err(BusError::IllegalTransition(e)) => tracing::debug!(
                spec_id = %spec_id, error = %e,
                "SpecFailed for a faulted spec rejected — spec already terminal",
            ),
            // A real persist fault — the spec is NOT known-failed and the
            // failure is not in the audit log. Loud (review B-svc-S6).
            Err(BusError::Persist(e)) => tracing::error!(
                spec_id = %spec_id, error = %e,
                "DB FAULT emitting SpecFailed for a faulted spec — \
                 the spec's failure may be unrecorded",
            ),
        }
    }

    /// `bus.emit` (emit-Phases 1–3) then push the event onto `local`.
    ///
    /// NEVER touches the channel — the C1 invariant. The run-loop drains
    /// `local` to fixpoint. An emit `Err` (a rejected transition / persist
    /// fault) is propagated so the caller routes it through `on_fault`.
    pub(super) async fn emit_local(
        &mut self,
        event: BoiEvent,
        spec_id: &SpecId,
    ) -> Result<(), OrchestratorError> {
        self.bus
            .emit(&event)
            .await
            .map_err(|source| OrchestratorError::Bus {
                spec_id: spec_id.clone(),
                source,
            })?;
        self.local.push_back(event);
        Ok(())
    }

    /// Route one `BoiEvent` — the spec / task lifecycle dispatch.
    pub(super) async fn handle_event(&mut self, event: BoiEvent) -> Result<(), OrchestratorError> {
        match event {
            // ---- Spec lifecycle ----
            BoiEvent::SpecStarted { spec_id } => self.on_spec_started(&spec_id).await,
            BoiEvent::PhaseCompleted {
                spec_id,
                task_id: None,
                phase_run_id,
                phase,
                verdict,
                ..
            } => {
                self.on_spec_phase_completed(&spec_id, &phase_run_id, &phase, &verdict)
                    .await
            }
            BoiEvent::PhaseCompleted {
                spec_id,
                task_id: Some(task_id),
                phase,
                verdict,
                ..
            } => {
                self.on_task_phase_completed(&spec_id, &task_id, &phase, &verdict)
                    .await
            }
            BoiEvent::TaskPassed { spec_id, .. } => self.on_task_passed(&spec_id).await,
            BoiEvent::SpecCompleted { spec_id } | BoiEvent::SpecFailed { spec_id, .. } => {
                self.cancel_spec_drains(&spec_id);
                Ok(())
            }
            BoiEvent::SpecCanceled { spec_id, .. } => self.on_spec_canceled(&spec_id).await,
            // ---- Task lifecycle ----
            BoiEvent::TaskStarted { spec_id, task_id } => {
                self.on_task_started(&spec_id, &task_id).await
            }
            BoiEvent::TaskBlocked {
                spec_id, task_id, ..
            } => {
                // Block already persisted by the bus — cancel the task's drain.
                self.cancel_task_drains(&spec_id, &task_id);
                Ok(())
            }
            BoiEvent::TaskUnblocked { spec_id, task_id } => {
                self.on_task_unblocked(&spec_id, &task_id).await
            }
            BoiEvent::TaskCanceled {
                spec_id,
                task_id,
                reason,
            } => self.on_task_canceled(&spec_id, &task_id, reason).await,
            // ---- Plan ----
            BoiEvent::PlanRevised { spec_id, .. } => self.on_plan_revised(&spec_id).await,
            BoiEvent::ReportReceived {
                spec_id,
                task_id,
                kind,
                payload,
                blocking,
                ..
            } => {
                self.on_report_received(&spec_id, &task_id, &kind, &payload, blocking)
                    .await
            }
            // ---- Observational — the bus already persisted/observed these;
            //      no orchestration action. `PhaseStarted` is the drain's
            //      "phase is running" signal — the row is already INSERTed. ----
            BoiEvent::PhaseStarted { .. }
            | BoiEvent::DecisionMade { .. }
            | BoiEvent::VerifyChecked { .. }
            | BoiEvent::ToolInvoked { .. }
            | BoiEvent::ErrorEncountered { .. } => Ok(()),
        }
    }

    /// A drain ended — remove its registry entry; on any status that did NOT
    /// relay a terminal `PhaseCompleted` (`Panicked`, `StreamError`,
    /// `CompletedWithoutVerdict`) surface a visible `TaskBlocked` /
    /// `SpecFailed`, never a silently-stuck spec (reviews C3, B-svc-1).
    pub(super) async fn handle_drain_terminated(
        &mut self,
        phase_run_id: PhaseRunId,
        status: DrainStatus,
    ) -> Result<(), OrchestratorError> {
        let Some(entry) = self.in_flight.remove(&phase_run_id) else {
            // Already removed (e.g. a cancel cleared it) — benign.
            return Ok(());
        };
        match status {
            // Clean completion (a verdict WAS routed) / cancellation —
            // registry cleanup only.
            DrainStatus::Completed | DrainStatus::Canceled => Ok(()),
            // No terminal `PhaseCompleted` reached routing — surface the
            // failure. `CompletedWithoutVerdict` is a clean stream that
            // produced no verdict (B-svc-1); it routes through the same arm
            // as a panic / stream error so the task never stalls silently.
            DrainStatus::Panicked
            | DrainStatus::StreamError(_)
            | DrainStatus::CompletedWithoutVerdict => {
                let detail = match &status {
                    DrainStatus::StreamError(e) => e.clone(),
                    DrainStatus::CompletedWithoutVerdict => {
                        "executor stream ended without a PhaseCompleted verdict".to_owned()
                    }
                    _ => "drain task panicked".to_owned(),
                };
                tracing::error!(
                    phase_run_id = %phase_run_id, detail,
                    "drain ended without a verdict — surfacing a visible failure",
                );
                match entry.task_id {
                    Some(task_id) => {
                        let ev = BoiEvent::TaskBlocked {
                            spec_id: entry.spec_id.clone(),
                            task_id,
                            reason: BlockedReason::ProviderFailed {
                                provider: "drain".to_owned(),
                                last_error: detail,
                            },
                        };
                        self.surface_drain_failure(ev, &entry.spec_id).await
                    }
                    None => {
                        // A spec-level phase's drain failed. `PreflightFailed`
                        // names the real cause (review B-orch-S1) — a drain
                        // that ended without a verdict is not a daemon crash.
                        let ev = BoiEvent::SpecFailed {
                            spec_id: entry.spec_id.clone(),
                            reason: FailureReason::PreflightFailed {
                                details: format!(
                                    "spec-level phase drain ended without a verdict: {detail}"
                                ),
                            },
                        };
                        self.surface_drain_failure(ev, &entry.spec_id).await
                    }
                }
            }
        }
    }

    /// Emit a drain-failure event. The emit `Err` is not propagated — the
    /// drain is already gone, so there is nothing left to fail (the `Ok(())`
    /// return is deliberate) — but it IS classified the same way `on_fault`
    /// classifies its own emit failure (review B-svc-S6 / the cheap NIT): an
    /// `IllegalTransition` means the entity is already terminal (benign,
    /// `debug!`); a `Persist` is a real DB fault and is loud (`error!`).
    async fn surface_drain_failure(
        &mut self,
        event: BoiEvent,
        spec_id: &SpecId,
    ) -> Result<(), OrchestratorError> {
        match self.emit_local(event, spec_id).await {
            Ok(()) => {}
            Err(OrchestratorError::Bus {
                source: BusError::IllegalTransition(e),
                ..
            }) => tracing::debug!(
                spec_id = %spec_id, error = %e,
                "drain-failure event rejected — entity already terminal",
            ),
            Err(OrchestratorError::Bus {
                source: BusError::Persist(e),
                ..
            }) => tracing::error!(
                spec_id = %spec_id, error = %e,
                "DB FAULT surfacing a drain failure — the failure may be unrecorded",
            ),
            // `emit_local` only ever returns `OrchestratorError::Bus`, but the
            // match must be total — any other variant is still loud.
            Err(e) => tracing::error!(
                spec_id = %spec_id, error = %e,
                "could not surface a drain failure",
            ),
        }
        Ok(())
    }

    /// Fire the `cancel` token of every in-flight drain for `spec_id`.
    pub(super) fn cancel_spec_drains(&mut self, spec_id: &SpecId) {
        for entry in self.in_flight.values() {
            if &entry.spec_id == spec_id {
                entry.cancel.cancel();
            }
        }
    }

    /// Fire the `cancel` token of every in-flight drain for one task.
    pub(super) fn cancel_task_drains(&mut self, spec_id: &SpecId, task_id: &TaskId) {
        for entry in self.in_flight.values() {
            if &entry.spec_id == spec_id && entry.task_id.as_ref() == Some(task_id) {
                entry.cancel.cancel();
            }
        }
    }
}
