//! `boi mcp-serve --phase-run <id>` — run one stdio MCP server bound to a
//! single worker's phase run (Task 9.9 / G14.4 / G23.3).
//!
//! Goose spawns this per worker as a recipe `extensions:` entry; it is not
//! normally invoked by hand. The process's [`WorkerSession`] is fixed
//! *structurally* from the `--phase-run` arg — there is no in-band identity
//! claim, so a worker cannot act for another task's phase run.
//!
//! ## How an MCP tool call reaches the daemon's orchestrator (review (a)(iii))
//!
//! `boi mcp-serve` is a **separate process** from `boi daemon`; they share no
//! in-process [`EventBus`]. The four MCP tools
//! (`decision_record`, `task_report`, `verify_run`, `worktree_diff`) each emit
//! a [`BoiEvent`] — that event must reach the
//! daemon's orchestrator (a `task_report` can trigger a plan revision).
//!
//! The wiring, without rewriting the Phase 4 `McpHandlers`:
//!
//! 1. `boi mcp-serve` builds `McpHandlers` over an `EventBus` whose single
//!    observer is a `DaemonForwardObserver`. The bus's *persist* phase runs
//!    against a **throwaway in-memory SQLite** (discarded at process exit) —
//!    so the local emit does not double-write the real `boi.db`.
//! 2. Every emitted event hits the observer (emit-Phase 2), which forwards it
//!    to the daemon as a [`DaemonCommand::ForwardEvent`] over the control
//!    socket.
//! 3. The daemon's `ForwardEvent` handler runs the **authoritative** emit on
//!    the daemon's own bus — the real persist (to `boi.db`), OTel observation,
//!    AND the orchestrator notification.
//!
//! The `verify_run` / `worktree_diff` runtime capability (`RuntimeToolHost`)
//! is given the **real** `boi.db` pool — it must resolve real task worktree
//! paths. `McpHandlers`'s own `decision_record` id-allocation likewise checks
//! the real DB for a free id, so the daemon's authoritative persist never
//! collides.

use std::sync::Arc;

use crate::cli::control;
use crate::cli::paths::{self, PathError};
use crate::repo;
use crate::repo::db::RepoError;
use crate::service::bus::{EmitObserver, ObserverError};
use crate::service::mcp::{McpHandlers, WorkerSession};
use crate::service::{DaemonCommand, EventBus};
use crate::types::event::BoiEvent;
use crate::types::ids::PhaseRunId;

/// A `boi mcp-serve` failure.
#[derive(Debug, thiserror::Error)]
pub enum McpServeError {
    /// The `~/.boi/v2/` path layout could not be resolved.
    #[error(transparent)]
    Path(#[from] PathError),
    /// The `--phase-run` argument is not a well-formed phase-run id.
    #[error("invalid --phase-run id `{got}`: {detail}")]
    BadPhaseRun {
        /// The malformed id.
        got: String,
        /// The validator's message.
        detail: String,
    },
    /// A repo-layer query failed (resolving the phase run, opening a pool).
    #[error("repo error: {0}")]
    Repo(#[from] RepoError),
    /// The stdio MCP-server transport faulted.
    #[error("MCP server transport failed: {0}")]
    Server(String),
}

/// Run `boi mcp-serve --phase-run <id>`.
///
/// Resolves the `WorkerSession` structurally from `<id>` (G14.4), wires the
/// daemon-forwarding bus, and serves the 4-tool MCP surface over this
/// process's stdin/stdout until the worker (the Goose client) disconnects.
pub async fn run(phase_run: &str) -> Result<(), McpServeError> {
    let phase_run_id = PhaseRunId::new(phase_run).map_err(|e| McpServeError::BadPhaseRun {
        got: phase_run.to_owned(),
        detail: e.to_string(),
    })?;

    // The real harness DB — for the structural session lookup + the runtime
    // tool host.
    let db_url = paths::boi_db_url()?;
    let real_pool = repo::connect(&db_url).await?;

    // Structural identity: the session IS the bound phase run (G14.4).
    let row = repo::phase_runs::fetch(&real_pool, &phase_run_id).await?;
    let session = build_session(&row)?;

    // The handler set — see the module doc. The bus persists to a throwaway
    // in-memory DB; a `DaemonForwardObserver` forwards every event to the
    // daemon, which performs the authoritative emit.
    let throwaway_pool = repo::connect("sqlite::memory:").await?;
    let socket = paths::control_socket()?;
    let forward = Arc::new(DaemonForwardObserver { socket });
    let bus = Arc::new(EventBus::new(throwaway_pool, vec![forward]));
    let host = Arc::new(crate::runtime::RuntimeToolHost::new(real_pool.clone()));
    let handlers = Arc::new(McpHandlers::new(bus, host, real_pool.clone()));

    let server = crate::runtime::BoiMcpServer::new(handlers, session, real_pool);
    server
        .serve_stdio()
        .await
        .map_err(|e| McpServeError::Server(e.to_string()))
}

/// Build the [`WorkerSession`] from a resolved `phase_runs` row.
///
/// The session's `spec_id` / `task_id` / `phase_run_id` come straight off the
/// row — structural identity, no in-band claim (G14.4).
fn build_session(row: &repo::PhaseRunRow) -> Result<WorkerSession, McpServeError> {
    let spec_id =
        crate::types::ids::SpecId::new(&row.spec_id).map_err(|e| McpServeError::BadPhaseRun {
            got: row.id.clone(),
            detail: format!("phase run has a corrupt spec id: {e}"),
        })?;
    let task_id = match &row.task_id {
        Some(t) => {
            Some(
                crate::types::ids::TaskId::new(t).map_err(|e| McpServeError::BadPhaseRun {
                    got: row.id.clone(),
                    detail: format!("phase run has a corrupt task id: {e}"),
                })?,
            )
        }
        None => None,
    };
    let phase_run_id = PhaseRunId::new(&row.id).map_err(|e| McpServeError::BadPhaseRun {
        got: row.id.clone(),
        detail: format!("phase run has a corrupt id: {e}"),
    })?;
    Ok(WorkerSession {
        spec_id,
        task_id,
        phase_run_id,
    })
}

/// An [`EmitObserver`] that forwards every observed event to the running
/// daemon over the control socket.
///
/// This is the seam that lets a `boi mcp-serve` worker's MCP tool calls reach
/// the daemon's orchestrator without a shared in-process bus. An observer
/// (emit-Phase 2) is best-effort — a forwarding failure returns
/// [`ObserverError`], which the bus logs `warn!`; but the `boi mcp-serve`
/// process should not silently lose a worker's decision/report, so the failure
/// is also `error!`-logged here (SO S6).
struct DaemonForwardObserver {
    socket: std::path::PathBuf,
}

#[async_trait::async_trait]
impl EmitObserver for DaemonForwardObserver {
    async fn observe(&self, event: &BoiEvent) -> Result<(), ObserverError> {
        let json = serde_json::to_value(event)
            .map_err(|e| ObserverError(format!("could not serialize event for forwarding: {e}")))?;
        let command = DaemonCommand::ForwardEvent { event: json };
        match control::send_command(&self.socket, &command).await {
            Ok(crate::service::DaemonResponse::Ok { .. }) => Ok(()),
            Ok(crate::service::DaemonResponse::Err { detail }) => {
                tracing::error!(detail, "daemon rejected a forwarded MCP event");
                Err(ObserverError(format!(
                    "daemon rejected the event: {detail}"
                )))
            }
            Err(e) => {
                tracing::error!(error = %e, "could not forward an MCP event to the daemon");
                Err(ObserverError(format!("could not reach the daemon: {e}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::testtmp::TempDir;
    use crate::repo::db::connect;
    use crate::repo::spec_versions::VersionTrigger;
    use crate::types::ids::{SpecId, TaskId};
    use chrono::Utc;
    use sqlx::SqlitePool;
    use tokio_util::sync::CancellationToken;

    /// Seed a `phase_runs` row for a task and return its pool + id.
    async fn seed_phase_run() -> (SqlitePool, PhaseRunId) {
        let pool = connect("sqlite::memory:").await.unwrap();
        let spec = SpecId::new("S0000001a").unwrap();
        let task = TaskId::new("T0000001a").unwrap();
        repo::specs::insert_spec(&pool, &spec, Utc::now())
            .await
            .unwrap();
        repo::spec_versions::append_version(
            &pool,
            &spec,
            1,
            &serde_json::json!({ "title": "demo" }),
            VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        repo::spec_runtime::initialize(&pool, &spec, 1)
            .await
            .unwrap();
        repo::task_runtime::insert_task(&pool, &task, &spec, None)
            .await
            .unwrap();
        let pr = PhaseRunId::new("P0000001a").unwrap();
        repo::phase_runs::insert_start(
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
        (pool, pr)
    }

    /// `build_session` derives the session structurally from the phase-run row
    /// — the `spec_id` / `task_id` / `phase_run_id` come straight off the row.
    #[tokio::test]
    async fn test_l2_build_session_is_structural() {
        let (pool, pr) = seed_phase_run().await;
        let row = repo::phase_runs::fetch(&pool, &pr).await.unwrap();
        let session = build_session(&row).unwrap();
        assert_eq!(session.spec_id.as_str(), "S0000001a");
        assert_eq!(
            session.task_id.as_ref().map(|t| t.as_str()),
            Some("T0000001a")
        );
        assert_eq!(session.phase_run_id.as_str(), "P0000001a");
    }

    /// `DaemonForwardObserver` forwards an observed event to the daemon as a
    /// `ForwardEvent` command — driven against a real control socket.
    #[tokio::test]
    async fn test_l2_forward_observer_sends_forward_event() {
        use crate::cli::control::CommandHandler;
        use crate::service::{DaemonCommand, DaemonResponse};

        // A handler that records the forwarded command.
        struct Capture {
            seen: std::sync::Mutex<Vec<DaemonCommand>>,
        }
        #[async_trait::async_trait]
        impl CommandHandler for Capture {
            async fn handle(&self, command: DaemonCommand) -> DaemonResponse {
                self.seen.lock().unwrap().push(command);
                DaemonResponse::Ok {
                    detail: "ok".to_owned(),
                }
            }
        }

        let dir = TempDir::new("mcp-forward");
        let socket = dir.path().join("daemon.sock");
        let shutdown = CancellationToken::new();
        let handler = Arc::new(Capture {
            seen: std::sync::Mutex::new(Vec::new()),
        });
        tokio::spawn(control::serve(
            socket.clone(),
            Arc::clone(&handler) as Arc<dyn CommandHandler>,
            shutdown.clone(),
        ));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let observer = DaemonForwardObserver {
            socket: socket.clone(),
        };
        let event = BoiEvent::ToolInvoked {
            spec_id: SpecId::new("S0000001a").unwrap(),
            task_id: Some(TaskId::new("T0000001a").unwrap()),
            tool: "worktree_diff".to_owned(),
            args_summary: "T0000001a".to_owned(),
            result_summary: "diff".to_owned(),
        };
        observer.observe(&event).await.unwrap();

        let seen = handler.seen.lock().unwrap();
        assert!(
            matches!(seen.as_slice(), [DaemonCommand::ForwardEvent { .. }]),
            "the observer forwarded a ForwardEvent command",
        );
        shutdown.cancel();
    }

    /// `boi mcp-serve` with no daemon: the forward observer's failure is loud
    /// (an `ObserverError`), never a silent swallow.
    #[tokio::test]
    async fn test_l2_forward_observer_with_no_daemon_is_loud() {
        let dir = TempDir::new("mcp-nodaemon");
        let observer = DaemonForwardObserver {
            socket: dir.path().join("absent.sock"),
        };
        let event = BoiEvent::ToolInvoked {
            spec_id: SpecId::new("S0000001a").unwrap(),
            task_id: Some(TaskId::new("T0000001a").unwrap()),
            tool: "verify_run".to_owned(),
            args_summary: "cargo test".to_owned(),
            result_summary: "ok".to_owned(),
        };
        let err = observer.observe(&event).await.unwrap_err();
        assert!(
            matches!(err, ObserverError(_)),
            "a forward failure is a loud ObserverError, got {err:?}",
        );
    }
}
