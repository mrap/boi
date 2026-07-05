//! [`RuntimeExecutor`] — the unified [`PhaseExecutor`] the orchestrator holds
//! (Task 7.6).
//!
//! The orchestrator (Phase 5a) drives every phase through one
//! `Arc<dyn PhaseExecutor>`. `RuntimeExecutor` is that one executor: it
//! dispatches by [`PhaseDef.kind`] — a `worker` phase to [`GooseRuntime`]
//! (Task 7.3), a `deterministic` phase to [`DeterministicExecutor`] (Phase 6).
//!
//! [`PhaseDef.kind`]: crate::config::PhaseDef::kind
//!
//! ## Ports and adapters — the assembled adapter
//!
//! `service/` defines the [`PhaseExecutor`] port; Phases 6 and 7 supply the two
//! per-kind adapters; `RuntimeExecutor` composes them into the single adapter
//! the orchestrator is wired with at boot (Phase 9).
//!
//! ## The exhaustive `match` (review S18)
//!
//! `execute` matches `phase.kind` with NO `_` arm — adding a new [`PhaseKind`]
//! breaks this build until a dispatch arm is written. `PhaseKind` is the ONLY
//! discriminant `RuntimeExecutor` switches on; the spec-vs-task `PhaseLevel`
//! distinction is handled *inside* `DeterministicExecutor` (Phase 6 Task 6.5),
//! not here.

use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use crate::config::{PhaseDef, PhaseKind};
use crate::runtime::goose::GooseRuntime;
use crate::runtime::steps_executor::DeterministicExecutor;
use crate::service::registry::PhaseExecutor;
use crate::types::context::PhaseContext;
use crate::types::event::BoiEvent;

/// The single [`PhaseExecutor`] the orchestrator holds — dispatches a phase to
/// the worker or deterministic runtime by its [`PhaseKind`].
pub struct RuntimeExecutor {
    /// The worker-phase runtime (`kind = "worker"`).
    goose: GooseRuntime,
    /// The deterministic-phase runtime (`kind = "deterministic"`).
    deterministic: DeterministicExecutor,
}

impl RuntimeExecutor {
    /// Compose the two per-kind adapters into the unified executor.
    ///
    /// G16.2 — `boot` (Phase 9) needs a public constructor; the fields are
    /// private. Production passes a `RuntimeExecutor`; the L3 test harness
    /// passes a `MockExecutor`.
    pub fn new(goose: GooseRuntime, deterministic: DeterministicExecutor) -> Self {
        Self {
            goose,
            deterministic,
        }
    }
}

impl PhaseExecutor for RuntimeExecutor {
    /// Dispatch `phase` by its [`PhaseKind`] to the worker or deterministic
    /// runtime.
    ///
    /// The `match` is exhaustive — no `_` arm; a new `PhaseKind` is a compile
    /// error here until a dispatch arm is added (review S18).
    fn execute(
        &self,
        phase: PhaseDef,
        ctx: PhaseContext,
        cancel: CancellationToken,
    ) -> BoxStream<'static, BoiEvent> {
        match phase.kind {
            PhaseKind::Worker => self.goose.run_phase(phase, ctx, cancel),
            PhaseKind::Deterministic => self.deterministic.execute(phase, ctx, cancel),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::db::connect;
    use crate::runtime::steps_executor::DeterministicExecutor;
    use crate::service::bus::{EventBus, NoopObserver};
    use crate::types::context::{SpecContract, TaskContract, Verification};
    use crate::types::event::BoiEvent;
    use crate::types::ids::{PhaseRunId, SpecId, TaskId};
    use crate::types::verdict::VerdictOutcome;
    use futures::stream::StreamExt;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A throwaway directory removed on drop.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("boi-rt-exec-{}-{tag}-{n}", std::process::id()));
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
    fn phase_run() -> PhaseRunId {
        PhaseRunId::new("P0000001a").unwrap()
    }

    /// An in-memory bus.
    async fn bus() -> Arc<EventBus> {
        let pool = connect("sqlite::memory:").await.unwrap();
        Arc::new(EventBus::new(pool, vec![Arc::new(NoopObserver)]))
    }

    /// Parse a phase fixture by stem.
    fn phase(name: &str) -> PhaseDef {
        let toml = std::fs::read_to_string(format!(
            "{}/tests/fixtures/phases/{name}.toml",
            env!("CARGO_MANIFEST_DIR"),
        ))
        .unwrap();
        crate::config::parse_phase(&toml).unwrap()
    }

    /// A `PhaseContext` for the given phase.
    fn phase_ctx(phase_name: &str, task: Option<TaskId>, workspace: &Path) -> PhaseContext {
        PhaseContext {
            spec_id: spec_id(),
            task_id: task,
            phase: phase_name.to_owned(),
            phase_run_id: phase_run(),
            iteration: 0,
            spec_contract: SpecContract {
                scope: "demo".into(),
                workspace: workspace.to_path_buf(),
                base_branch: "main".into(),
                exclusions: vec![],
                verifications: vec![],
                must_emit: vec![],
            },
            task_contract: Some(TaskContract {
                behavior: "do it".into(),
                verifications: vec![Verification::Command {
                    name: None,
                    command: "true".into(),
                }],
            }),
            tasks: vec![],
            skills: vec![],
            decisions: vec![],
            prior_phase_runs: vec![],
        }
    }

    /// Write a fake-`goose` script that emits a passing verdict + `complete`.
    fn fake_goose_passing(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let verdict = serde_json::json!({
            "type": "message",
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "text",
                    "text": "{\"synopsis\":\"did it\",\"outcome\":{\"type\":\"passing\",\"evidence\":{\"files_touched\":[],\"verifications\":[],\"summary\":\"ok\"}}}"
                }]
            }
        })
        .to_string();
        let complete = serde_json::json!({ "type": "complete" }).to_string();
        // The verdict + complete JSONL is written to a file the script cats —
        // no shell quoting of JSON.
        std::fs::write(dir.join("out.jsonl"), format!("{verdict}\n{complete}\n")).unwrap();
        let bin = dir.join("fake-goose.sh");
        std::fs::write(
            &bin,
            format!("#!/bin/sh\ncat '{}/out.jsonl'\n", dir.display()),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();
        bin
    }

    /// Drain a `BoxStream<BoiEvent>` into a `Vec`.
    async fn collect(mut stream: BoxStream<'static, BoiEvent>) -> Vec<BoiEvent> {
        let mut out = Vec::new();
        while let Some(e) = stream.next().await {
            out.push(e);
        }
        out
    }

    /// A `kind="worker"` phase routes to `GooseRuntime` — proven by a mock
    /// `goose` emitting a verdict; the stream ends in `PhaseCompleted`.
    #[tokio::test]
    async fn test_l2_worker_phase_routes_to_goose_runtime() {
        let dir = TempDir::new("worker");
        let goose_bin = fake_goose_passing(&dir.path);
        // The `execute` worker phase needs its prompt template (G26.1) — write
        // it into the same dir, which doubles as the prompts dir.
        std::fs::write(dir.path.join("execute.md"), "Implement the task.").unwrap();
        // The worker's `goose` runs in its task worktree (RC1) — pre-create it.
        std::fs::create_dir_all(crate::runtime::worktree::task_worktree(
            &dir.path,
            &spec_id(),
            &task_id(),
        ))
        .unwrap();
        let goose = GooseRuntime::with_worktree_root(
            goose_bin,
            dir.path.clone(),
            dir.path.clone(),
            dir.path.clone(),
        );
        let deterministic =
            DeterministicExecutor::with_worktree_root(bus().await, pool().await, dir.path.clone());
        let exec = RuntimeExecutor::new(goose, deterministic);

        // `execute` is a worker phase (kind=worker in the fixture).
        let events = collect(exec.execute(
            phase("execute"),
            phase_ctx("execute", Some(task_id()), &dir.path),
            CancellationToken::new(),
        ))
        .await;

        // The worker stream ended in PhaseCompleted{Passing} — the fake goose
        // ran (a deterministic dispatch would never spawn a subprocess).
        let last = events.last().expect("the stream is non-empty");
        let BoiEvent::PhaseCompleted { verdict, .. } = last else {
            unreachable!("a worker phase must end in PhaseCompleted, got {last:?}");
        };
        assert!(
            matches!(verdict.outcome, VerdictOutcome::Passing { .. }),
            "the fake goose's passing verdict must come through, got {verdict:?}",
        );
    }

    /// A `kind="deterministic"` phase routes to `DeterministicExecutor` —
    /// proven by a real `validate` step over a passing contract.
    #[tokio::test]
    async fn test_l2_deterministic_phase_routes_to_deterministic_executor() {
        let dir = TempDir::new("det");
        let workspace = dir.path.join("repo");
        std::fs::create_dir_all(&workspace).unwrap();
        let worktree_root = dir.path.join("worktrees");
        // A task-level validate runs in the task worktree — pre-create it.
        std::fs::create_dir_all(crate::runtime::worktree::task_worktree(
            &worktree_root,
            &spec_id(),
            &task_id(),
        ))
        .unwrap();

        // A goose pointed at a nonexistent bin — if the dispatch wrongly went
        // to goose, the test would see a goose_spawn_failed, not a real
        // validate Pass.
        let goose = GooseRuntime::new(
            PathBuf::from("/nonexistent/goose"),
            dir.path.clone(),
            dir.path.clone(),
        );
        let deterministic =
            DeterministicExecutor::with_worktree_root(bus().await, pool().await, worktree_root);
        let exec = RuntimeExecutor::new(goose, deterministic);

        // `validate` is a deterministic phase (kind=deterministic in the
        // fixture). A `true`-command contract → a Passing verdict.
        let mut ctx = phase_ctx("validate", Some(task_id()), &workspace);
        ctx.task_contract = Some(TaskContract {
            behavior: "x".into(),
            verifications: vec![Verification::Command {
                name: None,
                command: "true".into(),
            }],
        });
        let events = collect(exec.execute(phase("validate"), ctx, CancellationToken::new())).await;

        let last = events.last().expect("the stream is non-empty");
        let BoiEvent::PhaseCompleted { verdict, .. } = last else {
            unreachable!("a deterministic phase must end in PhaseCompleted, got {last:?}");
        };
        // A Passing verdict proves the deterministic `validate` ran — a goose
        // misroute (bad bin) would have produced a Fail{goose_spawn_failed}.
        assert!(
            matches!(verdict.outcome, VerdictOutcome::Passing { .. }),
            "the deterministic validate must Pass — a goose misroute would Fail; got {verdict:?}",
        );
    }

    /// `RuntimeExecutor` is a concrete `PhaseExecutor` — the orchestrator holds
    /// it as `Arc<dyn PhaseExecutor>` (the boot wiring shape).
    #[tokio::test]
    async fn test_l1_runtime_executor_is_a_phase_executor() {
        let dir = TempDir::new("dyn");
        let goose = GooseRuntime::new(
            PathBuf::from("/nonexistent/goose"),
            dir.path.clone(),
            dir.path.clone(),
        );
        let deterministic =
            DeterministicExecutor::with_worktree_root(bus().await, pool().await, dir.path.clone());
        // The boot-shape coercion: a `RuntimeExecutor` IS an `Arc<dyn PhaseExecutor>`.
        let _executor: Arc<dyn PhaseExecutor> =
            Arc::new(RuntimeExecutor::new(goose, deterministic));
    }

    /// A separate in-memory pool for the `DeterministicExecutor`.
    async fn pool() -> sqlx::SqlitePool {
        connect("sqlite::memory:").await.unwrap()
    }
}
