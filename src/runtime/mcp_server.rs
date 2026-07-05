//! [`BoiMcpServer`] — the stdio MCP-server transport (Task 7.5, G14.4).
//!
//! Phase 4 built the 4-tool MCP surface (`McpHandlers` — `decision_record`,
//! `task_report`, `verify_run`, `worktree_diff`) and deferred the transport.
//! G14.4 pins it: **stdio, one MCP-server child per worker**. Goose spawns
//! `boi mcp-serve --phase-run <id>` as a recipe extension (Task 7.1);
//! [`BoiMcpServer::serve_stdio`] speaks the `rmcp` stdio transport over that
//! child's own stdin/stdout.
//!
//! ## Session identity is structural (G14.4)
//!
//! There is no shared server and no in-band identity claim. Each worker's
//! `goose` invocation spawns its own `boi mcp-serve --phase-run <id>` child;
//! that process's [`WorkerSession`] is fixed at construction from the
//! `--phase-run` arg the harness wrote into the recipe. A worker cannot act
//! for another task's `phase_run` — the connection *is* the worker.
//!
//! [`WorkerSession`]: crate::service::WorkerSession
//!
//! ## `boi mcp-serve` — the Phase 9 CLI subcommand
//!
//! `boi mcp-serve --phase-run <id>` is a Phase 9 CLI subcommand
//! (forward-referenced). Phase 7 ships [`BoiMcpServer::serve_stdio`]; Phase 9
//! wires the subcommand that constructs a `BoiMcpServer` and calls it.
//!
//! ## Liveness backstop — the orphaned-server leak (review C-rt-3)
//!
//! `boi mcp-serve` is a *grandchild* of the harness — Goose spawns it as a
//! recipe `stdio` extension. If a SIGKILL'd `goose` fails to reap its own
//! extension children, this process is orphaned: a bare `running.waiting()`
//! blocks forever, holding a SQLite pool handle (and a worktree) — an unbounded
//! leak across a long BOI run. [`BoiMcpServer::serve_stdio`] therefore races
//! the transport against a **liveness watchdog**: on a fixed poll interval
//! (`LIVENESS_POLL_INTERVAL`) it checks the bound `phase_run` is still
//! in-flight in the DB (`completed_at IS NULL`). Once the phase run is gone or
//! has completed — its worker no longer exists — the server exits cleanly.
//! This is a DB-poll backstop (portable, unlike a Linux-only
//! `PR_SET_PDEATHSIG`); the normal exit is still the worker's clean
//! disconnect.
//!
//! ## rmcp `ServerHandler`
//!
//! `BoiMcpServer` implements `rmcp::ServerHandler`: `list_tools` returns
//! `service::tool_catalog()`; `call_tool` routes each call to the matching
//! [`McpHandlers`] method. A tool's JSON arguments are deserialized into a
//! small wire struct and handed to the handler; a handler error becomes an
//! `rmcp` tool-call error result (loud, never a swallowed failure).
//!
//! [`McpHandlers`]: crate::service::McpHandlers

use std::sync::Arc;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpRmcpError, RoleServer};
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::repo::{self, RepoError};
use crate::service::{
    DecisionRecordArgs, McpHandlers, TaskReportArgs, WorkerSession, tool_catalog,
};
use crate::types::decision::RejectedAlternative;
use crate::types::ids::DecisionId;

/// How often [`BoiMcpServer::serve_stdio`]'s liveness watchdog polls the DB to
/// confirm the bound `phase_run` is still in-flight (review C-rt-3).
///
/// 30 s is well below any plausible BOI run length, so an orphaned server is
/// reaped promptly, and far above the DB-query cost, so the poll is free.
const LIVENESS_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// [`BoiMcpServer::serve_stdio`] could not run the stdio MCP server.
#[derive(Debug, thiserror::Error)]
pub enum McpServerError {
    /// The `rmcp` stdio transport failed to start or run.
    #[error("MCP stdio transport error: {0}")]
    Transport(String),
}

/// One stdio MCP server process, bound at spawn to exactly one worker's
/// `phase_run`.
///
/// Hosts the 4 tools from Tasks 4.4/4.5 (`McpHandlers`). Run by the
/// `boi mcp-serve` CLI subcommand (Phase 9).
pub struct BoiMcpServer {
    /// The Phase 4 tool handlers — the worker → harness transport endpoints.
    handlers: Arc<McpHandlers>,
    /// The per-worker session — fixed structurally from the `--phase-run` arg.
    session: WorkerSession,
    /// The SQLite pool — the liveness watchdog polls `phase_runs` through it to
    /// confirm the bound `phase_run` is still in-flight (review C-rt-3).
    pool: SqlitePool,
}

impl BoiMcpServer {
    /// Construct a server bound to one worker's [`WorkerSession`].
    ///
    /// `pool` backs the liveness watchdog (review C-rt-3) — it polls
    /// `phase_runs` for the session's `phase_run_id`.
    pub fn new(handlers: Arc<McpHandlers>, session: WorkerSession, pool: SqlitePool) -> Self {
        Self {
            handlers,
            session,
            pool,
        }
    }

    /// Serve the 4-tool MCP surface over this process's own stdin/stdout
    /// (the `rmcp` stdio transport).
    ///
    /// Returns when the worker (the `goose`-spawned client) disconnects — a
    /// clean exit. An abnormal transport failure is an `error!`-logged
    /// [`McpServerError`].
    ///
    /// A **liveness watchdog** runs alongside the transport: an orphaned
    /// `boi mcp-serve` (a SIGKILL'd `goose` that did not reap its own extension
    /// children) would otherwise block in `waiting()` forever, leaking a pool
    /// handle (review C-rt-3). The watchdog polls `phase_runs` on a fixed
    /// interval (`LIVENESS_POLL_INTERVAL`); once the bound `phase_run` is gone
    /// or completed the server exits cleanly. Whichever finishes first — the
    /// client disconnect or the watchdog — ends the server.
    pub async fn serve_stdio(self) -> Result<(), McpServerError> {
        // Capture what the watchdog needs before `self` is consumed by `serve`.
        let pool = self.pool.clone();
        let phase_run_id = self.session.phase_run_id.clone();

        let running = self
            .serve(rmcp::transport::io::stdio())
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "boi mcp-serve: stdio transport failed to start");
                McpServerError::Transport(e.to_string())
            })?;

        await_with_liveness(running, &pool, &phase_run_id, LIVENESS_POLL_INTERVAL).await
    }
}

/// Block until either the MCP client disconnects (the normal exit) or the
/// liveness watchdog fires (the orphaned-server backstop — review C-rt-3),
/// whichever comes first.
///
/// Factored out of [`BoiMcpServer::serve_stdio`] so a test can drive it over a
/// duplex transport with a short `poll_interval` (the production interval is
/// [`LIVENESS_POLL_INTERVAL`], far too long for a test). `tokio::select!`
/// drops the losing future — dropping `waiting()` or the watchdog is harmless.
async fn await_with_liveness(
    running: rmcp::service::RunningService<RoleServer, BoiMcpServer>,
    pool: &SqlitePool,
    phase_run_id: &crate::types::ids::PhaseRunId,
    poll_interval: Duration,
) -> Result<(), McpServerError> {
    tokio::select! {
        // The normal path — the worker (the goose client) disconnects.
        quit = running.waiting() => match quit {
            Ok(_quit_reason) => Ok(()),
            Err(e) => {
                // An abnormal disconnect — loud, never swallowed.
                tracing::error!(error = %e, "boi mcp-serve: abnormal client disconnect");
                Err(McpServerError::Transport(e.to_string()))
            }
        },
        // The backstop — the bound phase run is no longer in-flight, so the
        // worker this server exists for is gone. A clean exit (review C-rt-3)
        // — never an error: a reaped worker is the expected end.
        () = liveness_watchdog(pool, phase_run_id, poll_interval) => {
            tracing::warn!(
                phase_run = %phase_run_id,
                "boi mcp-serve: bound phase run is no longer in-flight — \
                 the worker is gone; exiting (orphaned-server backstop)",
            );
            Ok(())
        }
    }
}

/// Poll `phase_runs` until the bound `phase_run` is no longer in-flight, then
/// return — the liveness backstop for an orphaned `boi mcp-serve` (review
/// C-rt-3).
///
/// "No longer in-flight" = the row is gone (`RepoError::NotFound`) OR
/// `completed_at IS NOT NULL` ([`PhaseRunRow::is_open`] is false). A transient
/// query error is logged and the poll retried — a flaky DB read must not
/// kill a server whose worker is still alive.
async fn liveness_watchdog(
    pool: &SqlitePool,
    phase_run_id: &crate::types::ids::PhaseRunId,
    poll_interval: Duration,
) {
    loop {
        tokio::time::sleep(poll_interval).await;
        match repo::phase_runs::fetch(pool, phase_run_id).await {
            Ok(row) if row.is_open() => {
                // Still in-flight — the worker is alive; keep serving.
            }
            Ok(_completed) => {
                // The phase run reached a terminal state — the worker is done.
                return;
            }
            Err(RepoError::NotFound(_)) => {
                // The row is gone (a retention prune, a torn-down spec) — the
                // worker cannot still be running. Stop.
                return;
            }
            Err(e) => {
                // A transient DB error — log and retry; do NOT exit a server
                // whose worker may still be alive on a flaky read.
                tracing::warn!(
                    phase_run = %phase_run_id, error = %e,
                    "boi mcp-serve liveness poll failed — retrying",
                );
            }
        }
    }
}

impl ServerHandler for BoiMcpServer {
    /// Server identity + capabilities — BOI advertises a `tools` capability.
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo` (= `InitializeResult`) is `#[non_exhaustive]` — build it
        // via `::new`, not a struct-update literal.
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    /// `tools/list` — the 4 BOI worker tools (`service::tool_catalog`).
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpRmcpError> {
        Ok(ListToolsResult::with_all_items(tool_catalog()))
    }

    /// `tools/call` — route the call to the matching [`McpHandlers`] method.
    ///
    /// The call's JSON arguments are deserialized into a wire struct; a handler
    /// error becomes an `rmcp` error tool result (loud — never swallowed).
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpRmcpError> {
        let args = request.arguments.unwrap_or_default();
        let args_value = serde_json::Value::Object(args);

        match request.name.as_ref() {
            "decision_record" => self.handle_decision_record(args_value).await,
            "task_report" => self.handle_task_report(args_value).await,
            "verify_run" => self.handle_verify_run(args_value).await,
            "worktree_diff" => self.handle_worktree_diff().await,
            other => Err(McpRmcpError::invalid_params(
                format!("unknown tool `{other}`"),
                None,
            )),
        }
    }
}

impl BoiMcpServer {
    /// Route `decision_record` → [`McpHandlers::decision_record`].
    async fn handle_decision_record(
        &self,
        args: serde_json::Value,
    ) -> Result<CallToolResult, McpRmcpError> {
        let wire: DecisionRecordWire = parse_args(args)?;
        let supersedes = match wire.supersedes {
            Some(s) => Some(
                DecisionId::new(&s)
                    .map_err(|e| McpRmcpError::invalid_params(format!("supersedes: {e}"), None))?,
            ),
            None => None,
        };
        let result = self
            .handlers
            .decision_record(
                &self.session,
                DecisionRecordArgs {
                    title: wire.title,
                    summary: wire.summary,
                    rationale: wire.rationale,
                    alternatives: wire.alternatives,
                    supersedes,
                },
            )
            .await;
        match result {
            Ok(id) => Ok(ok_result(format!("recorded decision {}", id.as_str()))),
            Err(e) => Ok(handler_error(&e.to_string())),
        }
    }

    /// Route `task_report` → [`McpHandlers::task_report`].
    async fn handle_task_report(
        &self,
        args: serde_json::Value,
    ) -> Result<CallToolResult, McpRmcpError> {
        let wire: TaskReportWire = parse_args(args)?;
        let result = self
            .handlers
            .task_report(
                &self.session,
                TaskReportArgs {
                    kind: wire.kind,
                    payload: wire.payload,
                    // §7.7 default: `blocking` absent ⇒ true. The rmcp
                    // transport layer applies the default here (the MCP
                    // input_schema only documents it).
                    blocking: wire.blocking.unwrap_or(true),
                },
            )
            .await;
        match result {
            Ok(()) => Ok(ok_result("report received")),
            Err(e) => Ok(handler_error(&e.to_string())),
        }
    }

    /// Route `verify_run` → [`McpHandlers::verify_run`].
    async fn handle_verify_run(
        &self,
        args: serde_json::Value,
    ) -> Result<CallToolResult, McpRmcpError> {
        let wire: VerifyRunWire = parse_args(args)?;
        match self.handlers.verify_run(&self.session, wire.command).await {
            Ok(output) => Ok(ok_result(format!(
                "exit={} stdout:\n{}",
                output.exit_code, output.stdout,
            ))),
            Err(e) => Ok(handler_error(&e.to_string())),
        }
    }

    /// Route `worktree_diff` → [`McpHandlers::worktree_diff`].
    async fn handle_worktree_diff(&self) -> Result<CallToolResult, McpRmcpError> {
        match self.handlers.worktree_diff(&self.session).await {
            Ok(diff) => Ok(ok_result(diff)),
            Err(e) => Ok(handler_error(&e.to_string())),
        }
    }
}

/// Deserialize a tool's JSON arguments into its wire struct.
///
/// A malformed-argument call is an `rmcp` `invalid_params` error — loud, not a
/// swallowed failure.
fn parse_args<T: for<'de> Deserialize<'de>>(args: serde_json::Value) -> Result<T, McpRmcpError> {
    serde_json::from_value(args)
        .map_err(|e| McpRmcpError::invalid_params(format!("invalid tool arguments: {e}"), None))
}

/// A successful tool result carrying one text content block.
fn ok_result(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![Content::text(text.into())])
}

/// An error tool result — a handler failure surfaced to the worker as an
/// `isError` tool result (loud; the worker sees the failure, not a silent ok).
fn handler_error(message: &str) -> CallToolResult {
    tracing::error!(error = %message, "boi mcp-serve: tool handler failed");
    CallToolResult::error(vec![Content::text(format!("BOI tool error: {message}"))])
}

// --- Wire structs: the tool JSON argument shapes (§7.7) ---

/// `decision_record` arguments — the JSON the worker sends.
#[derive(Debug, Deserialize)]
struct DecisionRecordWire {
    /// Short decision title.
    title: String,
    /// 1-3 sentence summary.
    summary: String,
    /// Why this choice over the alternatives.
    rationale: String,
    /// Alternatives considered and rejected.
    #[serde(default)]
    alternatives: Vec<RejectedAlternative>,
    /// A prior decision id this one supersedes, if any.
    #[serde(default)]
    supersedes: Option<String>,
}

/// `task_report` arguments — the JSON the worker sends.
#[derive(Debug, Deserialize)]
struct TaskReportWire {
    /// Advisory report kind.
    kind: String,
    /// The report payload.
    payload: serde_json::Value,
    /// Whether the report blocks the task — absent ⇒ `true` (§7.7 default).
    #[serde(default)]
    blocking: Option<bool>,
}

/// `verify_run` arguments — the JSON the worker sends.
#[derive(Debug, Deserialize)]
struct VerifyRunWire {
    /// The verification command to run.
    command: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo;
    use crate::repo::db::connect;
    use crate::service::bus::EventBus;
    use crate::service::bus::testkit::RecordingObserver;
    use crate::service::{ToolHostError, VerificationOutput, WorkerToolHost};
    use crate::types::event::BoiEvent;
    use crate::types::ids::{PhaseRunId, SpecId, TaskId};
    use crate::types::verdict::WorkerVerdict;
    use async_trait::async_trait;
    use chrono::Utc;
    use rmcp::model::CallToolRequestParams;
    use rmcp::service::ServiceExt;
    use rmcp::transport::async_rw::AsyncRwTransport;

    fn spec_id() -> SpecId {
        SpecId::new("S0000001a").unwrap()
    }
    fn task_id() -> TaskId {
        TaskId::new("T0000001a").unwrap()
    }
    fn phase_run_id() -> PhaseRunId {
        PhaseRunId::new("P0000001a").unwrap()
    }

    /// A `WorkerToolHost` double — scripted verification + diff, no subprocess.
    #[derive(Debug, Clone)]
    struct MockToolHost;

    #[async_trait]
    impl WorkerToolHost for MockToolHost {
        async fn run_verification(
            &self,
            _task_id: &TaskId,
            _command: &str,
        ) -> Result<VerificationOutput, ToolHostError> {
            Ok(VerificationOutput {
                exit_code: 0,
                stdout: "all checks green".to_owned(),
                stderr: String::new(),
            })
        }

        async fn worktree_diff(&self, _task_id: &TaskId) -> Result<String, ToolHostError> {
            Ok("diff --git a/x b/x\n+ a line\n".to_owned())
        }
    }

    /// A pool seeded with a spec (+ v1 snapshot + `spec_runtime`), a task, and
    /// one open phase run — the FK target for runtime decisions/events.
    async fn seeded_pool() -> sqlx::SqlitePool {
        let pool = connect("sqlite::memory:").await.unwrap();
        repo::insert_spec(&pool, &spec_id(), Utc::now())
            .await
            .unwrap();
        repo::append_version(
            &pool,
            &spec_id(),
            1,
            &serde_json::json!({ "title": "demo" }),
            repo::VersionTrigger::Dispatch,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        repo::spec_runtime::initialize(&pool, &spec_id(), 1)
            .await
            .unwrap();
        repo::task_runtime::insert_task(&pool, &task_id(), &spec_id(), Some("setup"))
            .await
            .unwrap();
        repo::phase_runs::insert_start(
            &pool,
            &phase_run_id(),
            &spec_id(),
            Some(&task_id()),
            "execute",
            0,
            1,
            "claude_code",
            None,
            Utc::now(),
        )
        .await
        .unwrap();
        pool
    }

    /// Build a `BoiMcpServer` over a seeded pool + a `RecordingObserver`-backed
    /// bus + a `MockToolHost`. Returns the server + the recorder.
    async fn server() -> (BoiMcpServer, RecordingObserver) {
        let (srv, rec, _pool) = server_with_pool().await;
        (srv, rec)
    }

    /// Like [`server`] but also returns the seeded pool — the C-rt-3 liveness
    /// test marks the phase run completed through it.
    async fn server_with_pool() -> (BoiMcpServer, RecordingObserver, sqlx::SqlitePool) {
        let pool = seeded_pool().await;
        let rec = RecordingObserver::new();
        let bus = Arc::new(EventBus::new(pool.clone(), vec![Arc::new(rec.clone())]));
        let host: Arc<dyn WorkerToolHost> = Arc::new(MockToolHost);
        let handlers = Arc::new(McpHandlers::new(bus, host, pool.clone()));
        let session = WorkerSession {
            spec_id: spec_id(),
            task_id: Some(task_id()),
            phase_run_id: phase_run_id(),
        };
        (
            BoiMcpServer::new(handlers, session, pool.clone()),
            rec,
            pool,
        )
    }

    /// Run a `BoiMcpServer` over an in-memory duplex transport and return a
    /// connected test client. The server runs in a background task; dropping
    /// the returned client disconnects it (the server task then ends cleanly).
    async fn connected_client(
        srv: BoiMcpServer,
    ) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
        // A bidirectional in-memory pipe — server end ⇄ client end.
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let (s_read, s_write) = tokio::io::split(server_io);
        let (c_read, c_write) = tokio::io::split(client_io);

        // The server speaks over its half of the duplex (the stdio transport
        // shape — an AsyncRead + AsyncWrite pair — without real stdio).
        tokio::spawn(async move {
            let transport = AsyncRwTransport::new_server(s_read, s_write);
            if let Ok(running) = srv.serve(transport).await {
                // `waiting()` returns a `Result` — `drop` is the explicit
                // must-use consumer (a test-only background task; the quit
                // reason is not asserted here, the dedicated test does that).
                drop(running.waiting().await);
            }
        });

        // The test client.
        let transport = AsyncRwTransport::new_client(c_read, c_write);
        ().serve(transport)
            .await
            .expect("test client connects to the BoiMcpServer")
    }

    /// `tools/list` over a real rmcp client → the 4 BOI tools.
    #[tokio::test]
    async fn test_l2_list_tools_returns_the_four_boi_tools() {
        let (srv, _rec) = server().await;
        let client = connected_client(srv).await;

        let tools = client
            .list_tools(Default::default())
            .await
            .expect("list_tools succeeds");
        let names: Vec<&str> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(
            names,
            vec![
                "decision_record",
                "task_report",
                "verify_run",
                "worktree_diff",
            ],
        );
        drop(client);
    }

    /// Build a `CallToolRequestParams` — the struct is `#[non_exhaustive]`, so
    /// it must be built via `::new` + `with_arguments`, not a struct literal.
    fn call_params(name: &'static str, args: serde_json::Value) -> CallToolRequestParams {
        let params = CallToolRequestParams::new(name);
        match args.as_object() {
            Some(obj) => params.with_arguments(obj.clone()),
            None => params,
        }
    }

    /// `decision_record` over a real rmcp client reaches
    /// `McpHandlers::decision_record` and the bus observes `DecisionMade`.
    #[tokio::test]
    async fn test_l2_decision_record_reaches_handlers_and_observes() {
        let (srv, rec) = server().await;
        let client = connected_client(srv).await;

        let result = client
            .call_tool(call_params(
                "decision_record",
                serde_json::json!({
                    "title": "Use sqlx",
                    "summary": "Compile-checked queries.",
                    "rationale": "Type safety.",
                    "alternatives": [{ "name": "diesel", "reason": "heavy macros" }],
                }),
            ))
            .await
            .expect("call_tool succeeds");

        assert_ne!(result.is_error, Some(true), "the tool call must succeed");
        // The bus observed exactly one DecisionMade.
        let seen = rec.seen();
        assert_eq!(seen.len(), 1);
        assert!(matches!(seen[0], BoiEvent::DecisionMade { .. }));
        drop(client);
    }

    /// `verify_run` over a real rmcp client reaches the `WorkerToolHost`
    /// (the mock) and the bus observes `VerifyChecked`.
    #[tokio::test]
    async fn test_l2_verify_run_reaches_tool_host_and_observes() {
        let (srv, rec) = server().await;
        let client = connected_client(srv).await;

        let result = client
            .call_tool(call_params(
                "verify_run",
                serde_json::json!({ "command": "cargo test" }),
            ))
            .await
            .expect("call_tool succeeds");
        assert_ne!(result.is_error, Some(true));

        let seen = rec.seen();
        assert_eq!(seen.len(), 1);
        let BoiEvent::VerifyChecked { command, .. } = &seen[0] else {
            unreachable!("verify_run must emit VerifyChecked, got {:?}", seen[0]);
        };
        assert_eq!(command, "cargo test");
        drop(client);
    }

    /// An unknown tool name → an `rmcp` error (loud — not a swallowed ok).
    #[tokio::test]
    async fn test_l2_unknown_tool_is_an_error() {
        let (srv, _rec) = server().await;
        let client = connected_client(srv).await;

        let result = client
            .call_tool(call_params("no.such.tool", serde_json::Value::Null))
            .await;
        assert!(
            result.is_err(),
            "an unknown tool must surface as an error, got {result:?}",
        );
        drop(client);
    }

    /// The server exits cleanly when the client disconnects — `serve` over a
    /// duplex, then drop the client; the server task ends without error.
    #[tokio::test]
    async fn test_l2_server_exits_cleanly_on_client_disconnect() {
        let (srv, _rec) = server().await;
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let (s_read, s_write) = tokio::io::split(server_io);
        let (c_read, c_write) = tokio::io::split(client_io);

        // The server records its quit reason.
        let server_task = tokio::spawn(async move {
            let transport = AsyncRwTransport::new_server(s_read, s_write);
            let running = srv.serve(transport).await.expect("server starts");
            running.waiting().await
        });

        // Connect, then disconnect the client.
        let transport = AsyncRwTransport::new_client(c_read, c_write);
        let client = ().serve(transport).await.expect("client connects");
        drop(client);

        // The server task ends — cleanly (a client disconnect is not an error
        // from the server's perspective).
        let quit = tokio::time::timeout(std::time::Duration::from_secs(5), server_task)
            .await
            .expect("the server must exit promptly after the client disconnects")
            .expect("the server task did not panic");
        assert!(
            quit.is_ok(),
            "a client disconnect is a clean server exit, got {quit:?}",
        );
    }

    /// Regression test for C-rt-3 — the orphaned-server leak.
    ///
    /// `boi mcp-serve` is a grandchild of the harness; a SIGKILL'd `goose` that
    /// fails to reap its own extension children orphans it. The OLD
    /// `serve_stdio` did a bare `running.waiting().await` — with the client
    /// (the dead `goose`) still nominally connected, `waiting()` NEVER returns,
    /// so the server sits forever holding a SQLite pool handle: an unbounded
    /// leak. The fix adds a liveness watchdog that polls `phase_runs`.
    ///
    /// This test keeps the MCP client **connected throughout** — so a bare
    /// `waiting()` would block forever — and drives `await_with_liveness` with
    /// a short poll interval. It asserts (a) while the phase run is in-flight
    /// the server does NOT exit, then (b) once the phase run is marked complete
    /// the watchdog fires and the server exits cleanly. Revert the watchdog
    /// (a bare `waiting()`) and step (b) hangs until the outer timeout fails
    /// it — a genuine fail-before / pass-after.
    #[tokio::test]
    async fn test_l2_liveness_watchdog_exits_an_orphaned_server() {
        let (srv, _rec, pool) = server_with_pool().await;
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let (s_read, s_write) = tokio::io::split(server_io);
        let (c_read, c_write) = tokio::io::split(client_io);

        // The server runs `await_with_liveness` with a short (50 ms) poll
        // interval — the production interval is far too long for a test.
        let watch_pool = pool.clone();
        let server_task = tokio::spawn(async move {
            let transport = AsyncRwTransport::new_server(s_read, s_write);
            let running = srv.serve(transport).await.expect("server starts");
            await_with_liveness(
                running,
                &watch_pool,
                &phase_run_id(),
                Duration::from_millis(50),
            )
            .await
        });

        // Connect a client and KEEP it connected — a bare `waiting()` would
        // therefore never return. The watchdog is the only thing that can end
        // the server.
        let transport = AsyncRwTransport::new_client(c_read, c_write);
        let _client = ().serve(transport).await.expect("client connects");

        // (a) The phase run is still in-flight (open) — the server must NOT
        //     exit. Give the watchdog several poll cycles to (correctly) decide
        //     to keep serving.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !server_task.is_finished(),
            "the server must keep serving while its phase run is in-flight",
        );

        // (b) Mark the phase run completed — the worker this server exists for
        //     is now gone. The watchdog must fire and the server exit cleanly.
        repo::phase_runs::update_end(
            &pool,
            &phase_run_id(),
            "done",
            &WorkerVerdict {
                synopsis: "done".to_owned(),
                outcome: crate::types::verdict::VerdictOutcome::Passing {
                    evidence: crate::types::verdict::Evidence::default(),
                },
            },
            &[],
            0,
            0,
            Utc::now(),
        )
        .await
        .expect("mark the phase run completed");

        let result = tokio::time::timeout(Duration::from_secs(10), server_task)
            .await
            .expect(
                "C-rt-3 regression: the server did NOT exit after its phase run \
                 completed — with the client still connected a bare `waiting()` \
                 blocks forever (the orphaned-server leak)",
            )
            .expect("the server task did not panic");
        assert!(
            result.is_ok(),
            "a no-longer-in-flight phase run is a clean server exit, got {result:?}",
        );
    }
}
