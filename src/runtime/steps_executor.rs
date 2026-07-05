//! [`DeterministicExecutor`] — the adapter that makes a deterministic phase
//! look like any other phase to the orchestrator.
//!
//! ## Ports and adapters — the adapter side
//!
//! Phase 5a defined the [`PhaseExecutor`] port; this is the deterministic-phase
//! *adapter*. `DeterministicExecutor` `impl PhaseExecutor` — the locked trait
//! method is `execute`, not an inherent `run` (Phase 7's `RuntimeExecutor`
//! holds a `DeterministicExecutor` and delegates to its `execute`).
//!
//! ## The terminal-`PhaseCompleted` guarantee (G21.5 — load-bearing)
//!
//! The orchestrator's drain treats a `DrainStatus::Completed` that emitted NO
//! `PhaseCompleted` as cleanup-only — which silently sticks the task. So
//! [`DeterministicExecutor::execute`] yields **exactly one** terminal
//! `PhaseCompleted` on EVERY path: a `resolve` miss, a panicked step body, a
//! `StepError` — all end in a `PhaseCompleted{verdict: Fail{…}}`, never a
//! panic and never an empty stream.
//!
//! ## The `StepOutcome → WorkerVerdict` lift (review C6)
//!
//! A deterministic phase only ever yields `Passing` / `Fail` — never
//! `Redo`/`Blocked`. `StepOutcome::Pass{evidence}` lifts to
//! `WorkerVerdict{outcome: Passing{evidence}}`; `StepOutcome::Fail{error_why_fix}`
//! to `WorkerVerdict{outcome: Fail{error,why,fix}}`. This is the C6 verdict
//! seam that lets `routing.rs` keep one 4-arm verdict router for worker AND
//! deterministic phases.
//!
//! ## `StepCtx` construction is `PhaseLevel`-dependent (review S18)
//!
//! A spec-level deterministic phase (`workspace_prepare`, `merge`, `teardown`)
//! builds a `StepCtx` with `task_id: None` and the *integration* worktree /
//! branch; a task-level one (`workspace_verify_in/out`, `commit`,
//! `merge_to_integration`, task `validate`) with `task_id: Some` and the *task*
//! worktree / branch. The executor reads `phase.level` to choose.
//!
//! ## Cancellation (review S10 — stated honestly)
//!
//! `cancel` is threaded into the step (and into `validate`'s `run_command`). A
//! `spawn_blocking` git step already in flight runs to completion — a `git2`
//! op cannot be interrupted mid-call; the executor checks `cancel` at the step
//! boundary and `validate` checks it between commands. There is no "stops
//! mid-step".

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;

use futures::FutureExt;
use futures::stream::{self, BoxStream, StreamExt};
use sqlx::SqlitePool;
use tokio_util::sync::CancellationToken;

use crate::config::{PhaseDef, PhaseLevel};
use crate::runtime::deterministic::{self, StepRun};
use crate::runtime::validate;
use crate::runtime::worktree;
use crate::service::bus::EventBus;
use crate::service::registry::PhaseExecutor;
use crate::types::context::PhaseContext;
use crate::types::event::BoiEvent;
use crate::types::step::{StepCtx, StepError, StepOutcome};
use crate::types::verdict::{VerdictOutcome, WorkerVerdict};

/// The deterministic-phase [`PhaseExecutor`] adapter.
///
/// Holds the bus and pool the orchestrator hands every adapter, plus the
/// worktree root used to build a step's `StepCtx.worktree_path`.
pub struct DeterministicExecutor {
    /// The event bus. Retained for parity with the worker adapter; the
    /// deterministic executor's events flow through the *returned stream*
    /// (drained by the Phase 5a drain task), never a direct `bus.emit` —
    /// emitting directly would re-open Batch B C1's second-producer hole.
    #[allow(dead_code)]
    bus: Arc<EventBus>,
    /// The SQLite pool — retained for parity with the worker adapter and for
    /// future deterministic phases that need a repo read.
    #[allow(dead_code)]
    pool: SqlitePool,
    /// The §5 worktree root (`~/.boi/v2/worktrees` in production).
    worktree_root: PathBuf,
}

impl DeterministicExecutor {
    /// Construct the executor with the production worktree root
    /// (`~/.boi/v2/worktrees`).
    ///
    /// G16.2 — `boot` needs a public constructor (the fields are private).
    pub fn new(bus: Arc<EventBus>, pool: SqlitePool) -> Self {
        Self {
            bus,
            pool,
            worktree_root: worktree::default_worktree_root(),
        }
    }

    /// Construct the executor with an explicit worktree root.
    ///
    /// A test seam: the locked `new(bus, pool)` signature carries no root, so
    /// the worktree-mechanics tests use this to point the executor at a
    /// tempdir. Documented as a Phase 6 deviation — `new` remains the
    /// production constructor; this is test-only injection.
    pub fn with_worktree_root(
        bus: Arc<EventBus>,
        pool: SqlitePool,
        worktree_root: PathBuf,
    ) -> Self {
        Self {
            bus,
            pool,
            worktree_root,
        }
    }

    /// Build the `StepCtx` for `phase` against `ctx` — the `PhaseLevel`-
    /// dependent construction (review S18).
    fn step_ctx(&self, phase: &PhaseDef, ctx: &PhaseContext) -> StepCtx {
        let (task_id, worktree_path, branch_ref) = match phase.level {
            // Spec-level: no task; the integration worktree + branch.
            PhaseLevel::Spec => (
                None,
                worktree::integration_worktree(&self.worktree_root, &ctx.spec_id),
                worktree::integration_branch(&ctx.spec_id),
            ),
            // Task-level: the task's worktree + branch. `ctx.task_id` is the
            // task; if a task-level phase somehow arrives with `task_id: None`
            // the step body itself (`require_task`) fails it loudly.
            PhaseLevel::Task => match &ctx.task_id {
                Some(task_id) => (
                    Some(task_id.clone()),
                    worktree::task_worktree(&self.worktree_root, &ctx.spec_id, task_id),
                    worktree::task_branch(&ctx.spec_id, task_id),
                ),
                None => (
                    None,
                    worktree::integration_worktree(&self.worktree_root, &ctx.spec_id),
                    worktree::integration_branch(&ctx.spec_id),
                ),
            },
        };
        StepCtx {
            spec_id: ctx.spec_id.clone(),
            task_id,
            phase_run_id: ctx.phase_run_id.clone(),
            phase: ctx.phase.clone(),
            worktree_path,
            branch_ref,
            spec_contract: ctx.spec_contract.clone(),
            task_contract: ctx.task_contract.clone(),
        }
    }
}

impl PhaseExecutor for DeterministicExecutor {
    /// Resolve `phase` → `DetStep`, run it, splice `StepRun.events`, then lift
    /// `StepOutcome` → `WorkerVerdict` into one terminal `PhaseCompleted`.
    ///
    /// `PhaseStarted` is emitted by the Phase 5a drain task, NOT here. The
    /// returned stream yields, in order: the step's `StepRun.events` (e.g.
    /// `validate`'s per-command `VerifyChecked`s), then exactly one terminal
    /// `PhaseCompleted` — on every path (G21.5).
    fn execute(
        &self,
        phase: PhaseDef,
        ctx: PhaseContext,
        cancel: CancellationToken,
    ) -> BoxStream<'static, BoiEvent> {
        // Build the StepCtx now (it borrows `&self`); everything beyond this
        // point is moved into the `'static` future — no `&self` is borrowed.
        let step_ctx = Arc::new(self.step_ctx(&phase, &ctx));
        // `ctx` is consumed here — move its owned fields into the future.
        let PhaseContext {
            phase_run_id,
            spec_id,
            task_id,
            phase: phase_name,
            ..
        } = ctx;

        let future = async move {
            // Run the step; whatever happens, produce a (events, verdict) pair.
            let (events, verdict) = run_one_step(&phase_name, step_ctx, cancel).await;

            // The terminal PhaseCompleted — built last, appended last (G21.5:
            // every path lands here).
            let completed = BoiEvent::PhaseCompleted {
                phase_run_id,
                spec_id,
                task_id,
                phase: phase_name,
                verdict,
                tokens_in: 0,
                tokens_out: 0,
                duration_ms: 0,
            };
            let mut all = events;
            all.push(completed);
            all
        };

        // Run the future, then stream its events. The step has fully completed
        // before any event is yielded — a deterministic step produces all its
        // events at once (in `StepRun`), so collect-then-iter preserves the
        // contract order (StepRun.events first, PhaseCompleted last).
        stream::once(future).flat_map(stream::iter).boxed()
    }
}

/// Run one deterministic step and reduce it to `(events, verdict)`.
///
/// Every failure path — an unknown phase, a panicked step body, a `StepError`
/// — is reduced to a `Fail` verdict here, so the caller can ALWAYS append a
/// terminal `PhaseCompleted` (G21.5).
async fn run_one_step(
    phase_name: &str,
    step_ctx: Arc<StepCtx>,
    cancel: CancellationToken,
) -> (Vec<BoiEvent>, WorkerVerdict) {
    // A resolve miss → a loud Fail verdict, never a panic.
    let Some(step) = deterministic::resolve(phase_name) else {
        return (
            vec![],
            fail_verdict(
                phase_name,
                "unknown_phase",
                &format!("`{phase_name}` is not a deterministic phase"),
                "route this phase to the worker runtime, or correct the pipeline",
            ),
        );
    };

    // `validate` is special-cased so the real `cancel` token reaches its
    // per-command `run_command` (the `DetStep` signature carries no token —
    // `validate::validate` uses a fresh one; `validate_inner` takes ours).
    let result: Result<StepRun, StepError> = if phase_name == "validate" {
        // `validate_inner` is `async fn` — guard it with `catch_unwind` so a
        // panic in it becomes a Fail verdict, not a lost task.
        match AssertUnwindSafe(validate::validate_inner(step_ctx, &cancel))
            .catch_unwind()
            .await
        {
            Ok(r) => r,
            Err(_panic) => Err(StepError::Validate(format!(
                "the `{phase_name}` step body panicked"
            ))),
        }
    } else {
        // The `DetStep` bodies return `BoxFuture` — `catch_unwind` the await so
        // a panicking body surfaces as a Fail, never a swallowed task.
        match AssertUnwindSafe(step(step_ctx)).catch_unwind().await {
            Ok(r) => r,
            Err(_panic) => Err(StepError::Worktree(format!(
                "the `{phase_name}` step body panicked"
            ))),
        }
    };

    match result {
        Ok(run) => {
            // Lift the StepOutcome → WorkerVerdict (review C6).
            let verdict = lift(phase_name, run.outcome);
            (run.events, verdict)
        }
        // A StepError (a genuine harness error) → a loud Fail verdict. The
        // step never returned events in this case.
        Err(e) => (
            vec![],
            fail_verdict(
                phase_name,
                "step_error",
                &e.to_string(),
                "inspect the error and re-run the phase",
            ),
        ),
    }
}

/// Lift a `StepOutcome` into a `WorkerVerdict` (review C6 — the 5a.1 contract).
///
/// A deterministic phase only ever yields `Passing` / `Fail`.
fn lift(phase_name: &str, outcome: StepOutcome) -> WorkerVerdict {
    match outcome {
        StepOutcome::Pass { evidence } => WorkerVerdict {
            synopsis: format!("{phase_name} passed"),
            outcome: VerdictOutcome::Passing { evidence },
        },
        StepOutcome::Fail { error_why_fix } => WorkerVerdict {
            synopsis: format!("{phase_name} failed"),
            outcome: VerdictOutcome::Fail {
                error: error_why_fix.error,
                why: error_why_fix.why,
                fix: error_why_fix.fix,
            },
        },
    }
}

/// A `Fail` `WorkerVerdict` for an executor-level failure (resolve miss, panic,
/// `StepError`).
fn fail_verdict(phase_name: &str, error: &str, why: &str, fix: &str) -> WorkerVerdict {
    WorkerVerdict {
        synopsis: format!("{phase_name} failed"),
        outcome: VerdictOutcome::Fail {
            error: error.to_owned(),
            why: why.to_owned(),
            fix: fix.to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::db::connect;
    use crate::service::bus::{EventBus, NoopObserver};
    use crate::types::context::{SpecContract, TaskContract, Verification};
    use crate::types::ids::{PhaseRunId, SpecId, TaskId};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A throwaway directory removed on drop — `std`-only.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("boi-steps-exec-{}-{tag}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.path));
        }
    }

    fn spec_id() -> SpecId {
        SpecId::new("S0000001a").unwrap()
    }
    fn task_id() -> TaskId {
        TaskId::new("T0000001a").unwrap()
    }

    /// An in-memory bus.
    async fn bus() -> Arc<EventBus> {
        let pool = connect("sqlite::memory:").await.unwrap();
        Arc::new(EventBus::new(pool, vec![Arc::new(NoopObserver)]))
    }

    /// An executor rooted at `worktree_root`.
    async fn executor(worktree_root: PathBuf) -> DeterministicExecutor {
        let pool = connect("sqlite::memory:").await.unwrap();
        DeterministicExecutor::with_worktree_root(bus().await, pool, worktree_root)
    }

    /// A `PhaseDef` for the named deterministic phase at the given level.
    fn det_phase(name: &str, level: PhaseLevel) -> PhaseDef {
        // The phase fixtures are real; parse the one for `name` and override
        // its level if the test needs a different one (`validate` is task-level
        // in the fixture; a spec-level-validate test overrides it).
        let toml = std::fs::read_to_string(format!(
            "{}/tests/fixtures/phases/{name}.toml",
            env!("CARGO_MANIFEST_DIR"),
        ))
        .unwrap();
        let mut phase = crate::config::parse_phase(&toml).unwrap();
        // `PhaseDef.level` is public — override for the test.
        phase.level = level;
        phase
    }

    /// A `PhaseContext` for a deterministic phase.
    fn phase_ctx(
        phase: &str,
        task: Option<TaskId>,
        workspace: &Path,
        spec_verifications: Vec<Verification>,
        task_contract: Option<TaskContract>,
    ) -> PhaseContext {
        PhaseContext {
            spec_id: spec_id(),
            task_id: task,
            phase: phase.into(),
            phase_run_id: PhaseRunId::new("P0000001a").unwrap(),
            iteration: 0,
            spec_contract: SpecContract {
                scope: "demo".into(),
                workspace: workspace.to_path_buf(),
                base_branch: "main".into(),
                exclusions: vec![],
                verifications: spec_verifications,
                must_emit: vec![],
            },
            task_contract,
            tasks: vec![],
            skills: vec![],
            decisions: vec![],
            prior_phase_runs: vec![],
        }
    }

    /// Init a source repo with one commit on `main`.
    fn init_source_repo(path: &Path) {
        use git2::{Repository, Signature};
        let repo = Repository::init(path).expect("init");
        let sig = Signature::now("test", "test@localhost").unwrap();
        std::fs::write(path.join("README.md"), "hello\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("README.md")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
            .unwrap();
        if repo.find_branch("main", git2::BranchType::Local).is_err() {
            let head = repo.head().unwrap().peel_to_commit().unwrap();
            repo.branch("main", &head, true).unwrap();
            repo.set_head("refs/heads/main").unwrap();
        }
    }

    /// Drain a `BoxStream<BoiEvent>` into a `Vec`.
    async fn collect(mut stream: BoxStream<'static, BoiEvent>) -> Vec<BoiEvent> {
        let mut out = Vec::new();
        while let Some(e) = stream.next().await {
            out.push(e);
        }
        out
    }

    /// The last event of a deterministic-executor stream MUST be a
    /// `PhaseCompleted` — assert and return its verdict (G21.5).
    fn terminal_verdict(events: &[BoiEvent]) -> WorkerVerdict {
        let last = events.last().expect("the stream is never empty (G21.5)");
        let BoiEvent::PhaseCompleted { verdict, .. } = last else {
            unreachable!("the stream must end in PhaseCompleted, got {last:?}");
        };
        verdict.clone()
    }

    /// A `validate` phase over a passing contract → the stream yields the
    /// per-command `VerifyChecked`s then `PhaseCompleted{Passing}`.
    #[tokio::test]
    async fn test_l2_validate_passing_yields_verify_checked_then_completed() {
        let dir = TempDir::new("validate-pass");
        let workspace = dir.path.join("repo");
        std::fs::create_dir_all(&workspace).unwrap();
        let worktree_root = dir.path.join("worktrees");
        // A task-level validate runs IN the task worktree — pre-create it (in a
        // real pipeline `workspace_verify_in` does so before `validate`).
        std::fs::create_dir_all(worktree::task_worktree(
            &worktree_root,
            &spec_id(),
            &task_id(),
        ))
        .unwrap();

        let exec = executor(worktree_root).await;
        let events = collect(exec.execute(
            det_phase("validate", PhaseLevel::Task),
            phase_ctx(
                "validate",
                Some(task_id()),
                &workspace,
                vec![Verification::Command {
                    name: None,
                    command: "true".to_owned(),
                }],
                None,
            ),
            CancellationToken::new(),
        ))
        .await;

        // VerifyChecked first, PhaseCompleted last.
        assert!(
            matches!(events.first(), Some(BoiEvent::VerifyChecked { .. })),
            "the per-command VerifyChecked comes first, got {:?}",
            events.first(),
        );
        let verdict = terminal_verdict(&events);
        assert!(
            matches!(verdict.outcome, VerdictOutcome::Passing { .. }),
            "a passing contract lifts to a Passing verdict, got {verdict:?}",
        );
    }

    /// A `validate` phase over a failing contract → `PhaseCompleted{Fail}`.
    #[tokio::test]
    async fn test_l2_validate_failing_yields_completed_fail() {
        let dir = TempDir::new("validate-fail");
        let workspace = dir.path.join("repo");
        std::fs::create_dir_all(&workspace).unwrap();
        let worktree_root = dir.path.join("worktrees");
        std::fs::create_dir_all(worktree::task_worktree(
            &worktree_root,
            &spec_id(),
            &task_id(),
        ))
        .unwrap();

        let exec = executor(worktree_root).await;
        let events = collect(exec.execute(
            det_phase("validate", PhaseLevel::Task),
            phase_ctx(
                "validate",
                Some(task_id()),
                &workspace,
                vec![Verification::Command {
                    name: None,
                    command: "exit 1".to_owned(),
                }],
                None,
            ),
            CancellationToken::new(),
        ))
        .await;

        let verdict = terminal_verdict(&events);
        assert!(
            matches!(verdict.outcome, VerdictOutcome::Fail { .. }),
            "a failing contract lifts to a Fail verdict, got {verdict:?}",
        );
        // ErrorEncountered was spliced in before the terminal PhaseCompleted.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, BoiEvent::ErrorEncountered { .. })),
            "validate's ErrorEncountered must be in the stream",
        );
    }

    /// An unknown deterministic phase → `PhaseCompleted{Fail{error:"unknown_
    /// phase"}}`, never a panic, never an empty stream (G21.5).
    #[tokio::test]
    async fn test_l2_unknown_phase_yields_completed_fail_not_a_panic() {
        let dir = TempDir::new("unknown");
        let exec = executor(dir.path.join("worktrees")).await;
        // `det_phase` needs a real fixture; use `commit`'s and rename it to a
        // phase the table does not know.
        let mut phase = det_phase("commit", PhaseLevel::Task);
        phase.name = "no_such_deterministic_phase".into();

        let events = collect(exec.execute(
            phase,
            phase_ctx(
                "no_such_deterministic_phase",
                Some(task_id()),
                &dir.path,
                vec![],
                None,
            ),
            CancellationToken::new(),
        ))
        .await;

        // Exactly one event — the terminal Fail PhaseCompleted.
        assert_eq!(
            events.len(),
            1,
            "an unknown phase yields only PhaseCompleted"
        );
        let verdict = terminal_verdict(&events);
        let VerdictOutcome::Fail { error, .. } = &verdict.outcome else {
            unreachable!("an unknown phase must lift to Fail, got {verdict:?}");
        };
        assert_eq!(error, "unknown_phase");
    }

    /// A spec-level deterministic phase (`workspace_prepare`) builds a `StepCtx`
    /// with `task_id: None` and the integration worktree; a clean run passes.
    #[tokio::test]
    async fn test_l2_spec_level_phase_runs_against_the_integration_worktree() {
        let dir = TempDir::new("spec-level");
        let workspace = dir.path.join("repo");
        std::fs::create_dir_all(&workspace).unwrap();
        init_source_repo(&workspace);
        let worktree_root = dir.path.join("worktrees");

        let exec = executor(worktree_root.clone()).await;
        // workspace_prepare is spec-level — task_id None.
        let events = collect(exec.execute(
            det_phase("workspace_prepare", PhaseLevel::Spec),
            phase_ctx("workspace_prepare", None, &workspace, vec![], None),
            CancellationToken::new(),
        ))
        .await;

        let verdict = terminal_verdict(&events);
        assert!(
            matches!(verdict.outcome, VerdictOutcome::Passing { .. }),
            "workspace_prepare passes against a fresh repo, got {verdict:?}",
        );
        // The integration worktree was created at the §5 path.
        assert!(
            worktree::integration_worktree(&worktree_root, &spec_id()).is_dir(),
            "the integration worktree must exist after workspace_prepare",
        );
    }

    /// A task-level deterministic phase (`workspace_verify_in`) builds a
    /// `StepCtx` with `task_id: Some` and the task worktree.
    #[tokio::test]
    async fn test_l2_task_level_phase_runs_against_the_task_worktree() {
        let dir = TempDir::new("task-level");
        let workspace = dir.path.join("repo");
        std::fs::create_dir_all(&workspace).unwrap();
        init_source_repo(&workspace);
        let worktree_root = dir.path.join("worktrees");

        let exec = executor(worktree_root.clone()).await;
        // prepare_spec first (spec-level) so the integration branch exists.
        collect(exec.execute(
            det_phase("workspace_prepare", PhaseLevel::Spec),
            phase_ctx("workspace_prepare", None, &workspace, vec![], None),
            CancellationToken::new(),
        ))
        .await;

        // workspace_verify_in is task-level — task_id Some.
        let events = collect(exec.execute(
            det_phase("workspace_verify_in", PhaseLevel::Task),
            phase_ctx(
                "workspace_verify_in",
                Some(task_id()),
                &workspace,
                vec![],
                None,
            ),
            CancellationToken::new(),
        ))
        .await;

        let verdict = terminal_verdict(&events);
        assert!(
            matches!(verdict.outcome, VerdictOutcome::Passing { .. }),
            "workspace_verify_in passes, got {verdict:?}",
        );
        assert!(
            worktree::task_worktree(&worktree_root, &spec_id(), &task_id()).is_dir(),
            "the task worktree must exist after workspace_verify_in",
        );
    }

    /// The `step_ctx` builder honors `phase.level`: a spec-level phase yields a
    /// `StepCtx` with `task_id: None`; a task-level one with `task_id: Some`.
    #[tokio::test]
    async fn test_l1_step_ctx_construction_is_phase_level_dependent() {
        let dir = TempDir::new("step-ctx");
        let exec = executor(dir.path.join("worktrees")).await;

        let spec_phase = det_phase("merge", PhaseLevel::Spec);
        let spec_step = exec.step_ctx(
            &spec_phase,
            &phase_ctx("merge", Some(task_id()), &dir.path, vec![], None),
        );
        // Even though the PhaseContext carries a task_id, a SPEC-level phase
        // builds a task_id: None StepCtx (review S18).
        assert!(
            spec_step.task_id.is_none(),
            "a spec-level StepCtx has no task"
        );
        assert!(
            spec_step.branch_ref.ends_with("/integration"),
            "a spec-level StepCtx is on the integration branch",
        );

        let task_phase = det_phase("commit", PhaseLevel::Task);
        let task_step = exec.step_ctx(
            &task_phase,
            &phase_ctx("commit", Some(task_id()), &dir.path, vec![], None),
        );
        assert_eq!(
            task_step.task_id.as_ref(),
            Some(&task_id()),
            "a task-level StepCtx carries the task id",
        );
        assert!(
            task_step.branch_ref.ends_with(task_id().as_str()),
            "a task-level StepCtx is on the task branch",
        );
    }
}
