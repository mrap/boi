//! `run_phase` — the phase clock-in — plus its supporting helpers.
//!
//! `run_phase` mints a [`PhaseRunId`], re-hydrates the authored contracts,
//! composes the `PhaseContext`, and spawns the drain. The remaining helpers
//! (snapshot re-hydration, ready-task fan-out, pipeline-position lookups,
//! error wrapping) back the `on_*` handlers in
//! [`super::handlers`](super::handlers).

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::{Orchestrator, OrchestratorError, REVISIONS_DIR};
use crate::config::{PipelinePhase, SkillRef};
use crate::repo;
use crate::service::registry::{InFlight, drain_phase};
use crate::service::{context, plan_layer, scheduler};
use crate::types::context::{SpecContract, TaskBrief, TaskContract};
use crate::types::event::BoiEvent;
use crate::types::ids::{PhaseRunId, SpecId, TaskId};
use crate::types::verdict::{Evidence, VerdictOutcome, WorkerVerdict};

impl Orchestrator {
    /// The phase clock-in: mint a [`PhaseRunId`], re-hydrate the contracts,
    /// compose the `PhaseContext`, and spawn the drain.
    ///
    /// `run_phase` does NOT emit `PhaseStarted` — the drain does (review C1), so
    /// `handle` is never a channel producer. The id is minted with the RAW
    /// generator [`repo::ids::random_id`] (G19.2 — NOT `allocate_phase_run_id`,
    /// which would also INSERT the row and make the bus's later `insert_start`
    /// a duplicate).
    pub(super) async fn run_phase(
        &mut self,
        spec_id: &SpecId,
        task_id: Option<&TaskId>,
        phase: &str,
    ) -> Result<(), OrchestratorError> {
        // (1) — mint the id RAW (no DB write — the bus's persist inserts).
        let phase_run_id = PhaseRunId::new(repo::ids::random_id('P')).map_err(|e| {
            OrchestratorError::Contract {
                spec_id: spec_id.clone(),
                detail: format!("generated invalid phase-run id: {e}"),
            }
        })?;

        // (2) — the phase iteration: count this task+phase's prior runs.
        let history = repo::phase_runs::fetch_history_for_spec(&self.pool, spec_id)
            .await
            .map_err(|source| self.repo_fault(spec_id, source))?;
        let task_str = task_id.map(TaskId::as_str);
        let prior_runs = history
            .iter()
            .filter(|r| r.phase == phase && r.task_id.as_deref() == task_str)
            .count();
        // A `usize → u32` overflow here would mean a single (task, phase) ran
        // > 4 billion times — only reachable if every iteration cap was broken
        // (review B-svc-S2). The earlier `unwrap_or(u32::MAX)` silently clamped
        // it; that is a quiet failure. Surface it loudly instead — a saturated
        // iteration count is never a legitimate runtime state, and a typed
        // fault routes through `on_fault` to a visible `SpecFailed` rather
        // than panicking the whole orchestrator task.
        let iteration = u32::try_from(prior_runs).map_err(|_| OrchestratorError::Contract {
            spec_id: spec_id.clone(),
            detail: format!(
                "phase `{phase}` iteration count ({prior_runs}) overflowed u32 — \
                     iteration cap enforcement is broken"
            ),
        })?;

        // (3) — the live spec version.
        let spec_version = repo::spec_runtime::fetch(&self.pool, spec_id)
            .await
            .map_err(|source| self.repo_fault(spec_id, source))?
            .current_version;

        // (4) — resolve the phase definition, then overlay any pipeline
        // `[overrides.<phase>.runtime]` (G26 Phase-10 erratum — see
        // `apply_pipeline_override`).
        let mut phase_def = self
            .phases
            .get(phase)
            .ok_or_else(|| OrchestratorError::UnknownPhase {
                spec_id: spec_id.clone(),
                phase: phase.to_owned(),
            })?
            .clone();
        self.apply_pipeline_override(&mut phase_def);

        // (5) — re-hydrate the authored contracts from the snapshot (S4).
        let (spec_contract, task_contract, tasks, skills) = self
            .rehydrate_contracts(spec_id, spec_version, task_id)
            .await?;

        // (6) — compose the PhaseContext (5c).
        let ctx = context::compose(
            &self.pool,
            spec_contract,
            task_contract,
            tasks,
            skills,
            spec_id,
            task_id,
            phase,
            &phase_run_id,
            iteration,
        )
        .await
        .map_err(|context::ContextError::Repo(source)| self.repo_fault(spec_id, source))?;

        // (7) — spawn the drain. `Arc::clone` everything BEFORE `tokio::spawn`
        // so the spawned future is `'static` and borrows nothing from `self`
        // (review S1 — otherwise E0521/E0759).
        let bus = Arc::clone(&self.bus);
        let executor = Arc::clone(&self.executor);
        let daemon_tx = self.daemon_tx.clone();
        let cancel = CancellationToken::new();
        tokio::spawn(drain_phase(
            bus,
            daemon_tx,
            executor,
            phase_def,
            ctx,
            cancel.clone(),
            phase_run_id.clone(),
        ));
        self.in_flight.insert(
            phase_run_id,
            InFlight {
                cancel,
                spec_id: spec_id.clone(),
                task_id: task_id.cloned(),
                phase: phase.to_owned(),
            },
        );
        Ok(())
    }

    /// Overlay the running pipeline's `[overrides.<phase>.runtime]` onto a
    /// resolved [`PhaseDef`](crate::config::PhaseDef).
    ///
    /// ## Plan defect (Phase 10 erratum — pipeline overrides were dead)
    ///
    /// A pipeline TOML may carry `[overrides.<phase>.runtime]` to run one phase
    /// against a different provider/model than the phase TOML declares — the
    /// `standard` pipeline ships exactly this for `critique_plan`
    /// (`provider = "openrouter"`, the cross-model-critique design point, §4).
    /// `config::pipeline` parses `PipelineDef.overrides`, but no phase ever
    /// *applied* it: the orchestrator ran `critique_plan` with `claude_code`
    /// (the phase TOML's own provider), silently ignoring the override.
    ///
    /// This overlay closes that — a per-phase `RuntimeOverride` replaces the
    /// `phase_def.runtime.provider` / `.model` it sets (each field is
    /// independent; an override may set only one). The overlaid `PhaseDef`
    /// then flows into the drain, so the `phase_runs` row + the `PhaseStarted`
    /// event record the *effective* provider. Surfaced by Phase 10's
    /// `04-multi-provider` L3 fixture, whose distinct-provider assertion is
    /// the override's coverage.
    fn apply_pipeline_override(&self, phase_def: &mut crate::config::PhaseDef) {
        if let Some(over) = self.pipeline.overrides.get(&phase_def.name) {
            if let Some(provider) = &over.runtime.provider {
                phase_def.runtime.provider = provider.clone();
            }
            if let Some(model) = &over.runtime.model {
                phase_def.runtime.model = model.clone();
            }
        }
    }

    /// Re-hydrate `(SpecContract, Option<TaskContract>)` from the
    /// `spec_versions` snapshot — see the orchestrator module doc's
    /// contract-snapshot convention. A missing/malformed key is a loud
    /// [`OrchestratorError::Contract`].
    async fn rehydrate_contracts(
        &self,
        spec_id: &SpecId,
        spec_version: i64,
        task_id: Option<&TaskId>,
    ) -> Result<
        (
            SpecContract,
            Option<TaskContract>,
            Vec<TaskBrief>,
            Vec<SkillRef>,
        ),
        OrchestratorError,
    > {
        let snapshot = repo::spec_versions::fetch_snapshot(&self.pool, spec_id, spec_version)
            .await
            .map_err(|source| self.repo_fault(spec_id, source))?;
        let contract_err = |detail: String| OrchestratorError::Contract {
            spec_id: spec_id.clone(),
            detail,
        };

        let spec_value = snapshot
            .get("spec_contract")
            .ok_or_else(|| contract_err("snapshot has no `spec_contract` key".to_owned()))?;
        let spec_contract: SpecContract = serde_json::from_value(spec_value.clone())
            .map_err(|e| contract_err(format!("`spec_contract` is malformed: {e}")))?;

        // Build the `Vec<TaskBrief>` once from `task_contracts` — every phase
        // gets the full task survey, not just the one the current phase runs
        // on (the plan / critique_plan / review prompts read it to confirm the
        // declared tasks cover the scope).
        let tasks: Vec<TaskBrief> = match snapshot.get("task_contracts") {
            Some(serde_json::Value::Object(map)) => {
                let mut briefs = Vec::with_capacity(map.len());
                for (id_str, value) in map.iter() {
                    let tid = TaskId::new(id_str).map_err(|e| {
                        contract_err(format!(
                            "task_contracts key `{id_str}` is not a valid TaskId: {e}"
                        ))
                    })?;
                    let tc: TaskContract = serde_json::from_value(value.clone()).map_err(|e| {
                        contract_err(format!("task_contracts[{id_str}] is malformed: {e}"))
                    })?;
                    briefs.push(TaskBrief {
                        task_id: tid,
                        behavior: tc.behavior,
                        verifications: tc.verifications,
                    });
                }
                // Deterministic order — sorted by task_id (the snapshot map's
                // iteration order is unspecified).
                briefs.sort_by(|a, b| a.task_id.as_str().cmp(b.task_id.as_str()));
                briefs
            }
            None => Vec::new(),
            Some(_) => {
                return Err(contract_err(
                    "`task_contracts` exists but is not a JSON object".to_owned(),
                ));
            }
        };

        let task_contract = match task_id {
            None => None,
            Some(tid) => {
                let value = snapshot
                    .get("task_contracts")
                    .and_then(|m| m.get(tid.as_str()))
                    .ok_or_else(|| {
                        contract_err(format!("snapshot has no task_contracts entry for {tid}"))
                    })?;
                Some(serde_json::from_value(value.clone()).map_err(|e| {
                    contract_err(format!("task_contracts[{tid}] is malformed: {e}"))
                })?)
            }
        };
        // Skills: optional at the snapshot top level — a pre-skills snapshot
        // legitimately has no `skills` key (treated as empty). A present-but-
        // malformed value is loud, not silent (S6 — no quiet failures).
        let skills: Vec<SkillRef> = match snapshot.get("skills") {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(value) => serde_json::from_value(value.clone())
                .map_err(|e| contract_err(format!("`skills` is malformed: {e}")))?,
        };

        Ok((spec_contract, task_contract, tasks, skills))
    }

    /// Spawn every currently-ready task via an `emit_local(TaskStarted)`.
    pub(super) async fn spawn_ready_tasks(
        &mut self,
        spec_id: &SpecId,
    ) -> Result<(), OrchestratorError> {
        let ready = scheduler::ready_tasks(&self.pool, spec_id)
            .await
            .map_err(|e| self.scheduler_fault(spec_id, e))?;
        for task_id in ready {
            let ev = BoiEvent::TaskStarted {
                spec_id: spec_id.clone(),
                task_id,
            };
            self.emit_local(ev, spec_id).await?;
        }
        Ok(())
    }

    /// The first named phase in `spec_phases` (the pipeline entry phase).
    pub(super) fn first_spec_phase(&self) -> Option<String> {
        self.pipeline.spec_phases.iter().find_map(|p| match p {
            PipelinePhase::Phase(n) => Some(n.clone()),
            PipelinePhase::Tasks => None,
        })
    }

    /// The spec phase immediately after the `<tasks>` boundary — where the
    /// pipeline resumes once every task settles.
    pub(super) fn phase_after_tasks(&self) -> Option<String> {
        let phases = &self.pipeline.spec_phases;
        let tasks_idx = phases
            .iter()
            .position(|p| matches!(p, PipelinePhase::Tasks))?;
        phases.get(tasks_idx + 1).and_then(|p| match p {
            PipelinePhase::Phase(n) => Some(n.clone()),
            PipelinePhase::Tasks => None,
        })
    }

    /// Parse a stored `task_id` string into a [`TaskId`].
    pub(super) fn parse_task_id(
        &self,
        spec_id: &SpecId,
        s: &str,
    ) -> Result<TaskId, OrchestratorError> {
        TaskId::new(s).map_err(|e| OrchestratorError::Contract {
            spec_id: spec_id.clone(),
            detail: format!("corrupt task id in task_runtime: {e}"),
        })
    }

    /// Wrap a [`RepoError`](crate::repo::db::RepoError) as an
    /// [`OrchestratorError`].
    pub(super) fn repo_fault(
        &self,
        spec_id: &SpecId,
        source: crate::repo::db::RepoError,
    ) -> OrchestratorError {
        OrchestratorError::Repo {
            spec_id: spec_id.clone(),
            source,
        }
    }

    /// Wrap a [`scheduler::SchedulerError`] as an [`OrchestratorError`].
    pub(super) fn scheduler_fault(
        &self,
        spec_id: &SpecId,
        e: scheduler::SchedulerError,
    ) -> OrchestratorError {
        match e {
            scheduler::SchedulerError::Repo(source) => self.repo_fault(spec_id, source),
        }
    }

    /// Wrap a [`plan_layer::PlanLayerError`] as an [`OrchestratorError`].
    pub(super) fn plan_layer_fault(
        &self,
        spec_id: &SpecId,
        e: plan_layer::PlanLayerError,
    ) -> OrchestratorError {
        match e {
            plan_layer::PlanLayerError::Repo(source) => self.repo_fault(spec_id, source),
            plan_layer::PlanLayerError::Bus(source) => OrchestratorError::Bus {
                spec_id: spec_id.clone(),
                source,
            },
            plan_layer::PlanLayerError::Artifact(detail) => OrchestratorError::Contract {
                spec_id: spec_id.clone(),
                detail,
            },
        }
    }
}

/// The filesystem path of a `plan_revision` worker's artifact —
/// `<REVISIONS_DIR>/<phase_run_id>.json`, with `~` expanded to `$HOME`.
///
/// `$HOME` unset is a loud failure (review B-svc-S3): the earlier
/// `unwrap_or(".")` fallback silently resolved the artifact under the process
/// CWD, so a plan revision would read (or fail to read) the wrong file with no
/// signal. A missing `$HOME` is an environment fault — surface it as an
/// [`OrchestratorError::Contract`] (the orchestrator turns that into a visible
/// `SpecFailed`) rather than guessing a path.
pub(super) fn revision_artifact_path(
    spec_id: &SpecId,
    revision_run: &PhaseRunId,
) -> Result<std::path::PathBuf, OrchestratorError> {
    let home = std::env::var("HOME").map_err(|_| OrchestratorError::Contract {
        spec_id: spec_id.clone(),
        detail: "cannot resolve the plan-revision artifact path: $HOME is unset".to_owned(),
    })?;
    Ok(std::path::PathBuf::from(REVISIONS_DIR.replace('~', &home))
        .join(format!("{revision_run}.json")))
}

/// The [`Evidence`] carried by a `TaskPassed`, extracted from the terminal
/// verdict (`Passing` carries it; otherwise empty).
pub(super) fn verdict_evidence(verdict: &WorkerVerdict) -> Evidence {
    match &verdict.outcome {
        VerdictOutcome::Passing { evidence } => evidence.clone(),
        VerdictOutcome::Redo { .. }
        | VerdictOutcome::Blocked { .. }
        | VerdictOutcome::Fail { .. }
        | VerdictOutcome::Canceled => Evidence::default(),
    }
}
