//! BOI's MCP tool surface — the 4 worker tools (design §7.7).
//!
//! This file is built in two tasks. **Part 1 (Task 4.4 — this section to the
//! tests)** defines the per-worker [`WorkerSession`], the [`WorkerToolHost`]
//! port (for the two tools that need a runtime capability), and
//! [`tool_catalog`] — the 4 tools as `rmcp` `Tool` descriptors. **Part 2
//! (Task 4.5)** defines the handler struct that routes each tool call through
//! [`EventBus::emit`].
//!
//! [`EventBus::emit`]: crate::service::bus::EventBus::emit
//!
//! ## The 4 tools
//!
//! `decision_record`, `task_report`, `verify_run`, `worktree_diff` — the entire
//! BOI-defined worker surface. Provider-native tools (Claude Code's
//! Read/Edit/Bash, ...) sit alongside but are captured into the bus by the
//! provider driver, not this MCP server. There is deliberately no
//! `decision.query` / `phase_run.query` / DB-read tool (§7.6) — a worker
//! missing context fails loudly rather than reaching back into the DB.
//!
//! ## Ports and adapters
//!
//! `verify_run` and `worktree_diff` need a *runtime* capability — subprocess
//! execution and `git diff`. LDA (§13) forbids `service/` from doing either, so
//! `service/` defines the [`WorkerToolHost`] port here and `runtime/` provides
//! the adapter (Phase 6 — `runtime::validate` + `runtime::git_ops`). Phase 4
//! ships a `#[cfg(test)]` `MockToolHost`.
//!
//! ## rmcp API note
//!
//! The plan's `rmcp::model::Tool` usage predates an `rmcp` API survey; the
//! pinned `rmcp` 1.7.0 builds a `Tool` via `Tool::new(name, description,
//! input_schema)` where `input_schema: Into<Arc<JsonObject>>` and `JsonObject`
//! is `serde_json::Map<String, Value>`. [`tool_catalog`] uses
//! `rmcp::model::object` to turn a `serde_json::json!` schema into that map.
//!
//! ## Spec patch G12
//!
//! §7.7 gives `decision_record`'s `supersedes` param the JSON-schema pattern
//! `^D[0-9]{8}$` — a *numeric* id, predating the Crockford-base32 ID decision.
//! The `pattern` is dropped here: the `DecisionId` newtype (`DecisionId::new`)
//! is the authoritative validator, and the MCP `pattern` was only advisory.
//! Documented as the G12 application.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use rmcp::model::Tool;

use crate::repo;
use crate::repo::db::RepoError;
use crate::service::bus::{BusError, EventBus};
use crate::types::decision::{DecisionError, DecisionRecord, RejectedAlternative};
use crate::types::event::BoiEvent;
use crate::types::ids::{DecisionId, PhaseRunId, SpecId, TaskId};

/// The per-worker context bound to one MCP connection.
///
/// Phase 7's `GooseRuntime` binds a session to a transport connection when it
/// spawns a worker; Phase 4 tests construct one directly. `task_id` is `None`
/// for spec-level worker phases (`plan`, `critique_plan`) — the two worktree
/// tools reject a `None` task (see Task 4.5).
#[derive(Debug, Clone)]
pub struct WorkerSession {
    /// The spec the worker is running.
    pub spec_id: SpecId,
    /// The task the worker is running — `None` for spec-level phases.
    pub task_id: Option<TaskId>,
    /// The phase run this worker is executing.
    pub phase_run_id: PhaseRunId,
}

/// The output of a verification command run by a [`WorkerToolHost`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationOutput {
    /// The command's exit code.
    pub exit_code: i32,
    /// The command's captured stdout.
    pub stdout: String,
    /// The command's captured stderr.
    pub stderr: String,
}

/// A [`WorkerToolHost`] operation failed.
///
/// The single message-carrying variant is deliberate — the real failure
/// taxonomy (a missing worktree, a spawn failure, a git error) belongs to the
/// Phase 6 `runtime/` adapter, not to this port definition.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("worker-tool host error: {0}")]
pub struct ToolHostError(pub String);

/// Port for the two worker tools that need a runtime capability — subprocess
/// execution (`verify_run`) and `git diff` (`worktree_diff`).
///
/// The adapter is Phase 6 (`runtime::validate` + `runtime::git_ops`); Phase 4
/// ships a `#[cfg(test)]` `MockToolHost`.
#[async_trait]
pub trait WorkerToolHost: Send + Sync {
    /// Run a verification command in the given task's worktree.
    async fn run_verification(
        &self,
        task_id: &TaskId,
        command: &str,
    ) -> Result<VerificationOutput, ToolHostError>;

    /// Return the `git diff` of the given task's worktree against its branch
    /// base.
    async fn worktree_diff(&self, task_id: &TaskId) -> Result<String, ToolHostError>;
}

/// The 4 BOI worker tools as `rmcp` `Tool` descriptors (design §7.7).
///
/// Returns exactly four tools — `decision_record`, `task_report`, `verify_run`,
/// `worktree_diff` — in that order. Each `input_schema` is a JSON Schema object
/// matching §7.7; `decision_record`'s `supersedes` carries no `pattern` (spec
/// patch G12 — the `DecisionId` newtype validates).
// Tool names use underscores, not dots — OpenAI's function-name regex `^[a-zA-Z0-9_-]+$` forbids periods; dotted names returned HTTP 400 on every non-claude_code provider (fixed 2026-05-23).
pub fn tool_catalog() -> Vec<Tool> {
    vec![
        decision_record_tool(),
        task_report_tool(),
        verify_run_tool(),
        worktree_diff_tool(),
    ]
}

/// The `decision_record` tool descriptor.
fn decision_record_tool() -> Tool {
    let schema = rmcp::model::object(serde_json::json!({
        "type": "object",
        "properties": {
            "title":     { "type": "string", "description": "Short decision title." },
            "summary":   {
                "type": "string",
                "maxLength": 280,
                "description": "1-3 sentence summary of the decision.",
            },
            "rationale": { "type": "string", "description": "Why this choice over the alternatives." },
            "alternatives": {
                "type": "array",
                "description": "Alternatives considered and rejected.",
                "items": {
                    "type": "object",
                    "properties": {
                        "name":   { "type": "string" },
                        "reason": { "type": "string" },
                    },
                    "required": ["name", "reason"],
                },
            },
            // G12: no `pattern` — the DecisionId newtype is the validator.
            "supersedes": {
                "type": "string",
                "description": "A prior decision id this one supersedes (optional).",
            },
        },
        "required": ["title", "summary", "rationale", "alternatives"],
    }));
    Tool::new(
        "decision_record",
        "Record a non-trivial decision made during this phase. Use when picking \
         between plausible alternatives, setting a convention, or making a \
         hard-to-reverse choice.",
        schema,
    )
}

/// The `task_report` tool descriptor.
fn task_report_tool() -> Tool {
    let schema = rmcp::model::object(serde_json::json!({
        "type": "object",
        "properties": {
            "kind": {
                "type": "string",
                "description": "Advisory report kind (e.g. scope_creep, spec_bug, \
                                blocked_by_missing, needs_replan). Routing is uniform at v1.0.",
            },
            "payload": { "type": "object", "description": "The report payload." },
            "blocking": {
                "type": "boolean",
                "default": true,
                "description": "true (default) when the task is truly blocked; \
                                false for an advisory escalation that does not halt the task.",
            },
        },
        "required": ["kind", "payload"],
    }));
    Tool::new(
        "task_report",
        "Escalate to the plan layer. Use when the task's contract appears wrong, \
         scope must change, or a dependency is missing. Set blocking=false for \
         advisory escalations that do not halt the task; omit or set blocking=true \
         when the task is truly blocked.",
        schema,
    )
}

/// The `verify_run` tool descriptor.
fn verify_run_tool() -> Tool {
    let schema = rmcp::model::object(serde_json::json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "description": "The verification command to run." },
        },
        "required": ["command"],
    }));
    Tool::new(
        "verify_run",
        "Execute a verification command in the task worktree. Emits a typed \
         VerifyChecked event — prefer this over scraping ToolInvoked for \
         verification results.",
        schema,
    )
}

/// The `worktree_diff` tool descriptor.
fn worktree_diff_tool() -> Tool {
    // No required params — always diffs the current task worktree.
    let schema = rmcp::model::object(serde_json::json!({
        "type": "object",
        "properties": {},
    }));
    Tool::new(
        "worktree_diff",
        "Return git diff of the task worktree against the task branch base. Use \
         for worker self-check before emitting a Passing verdict.",
        schema,
    )
}

// ---------------------------------------------------------------------------
// Part 2 (Task 4.5) — the MCP tool handlers.
// ---------------------------------------------------------------------------

/// Arguments for the `decision_record` tool (design §7.7).
#[derive(Debug, Clone)]
pub struct DecisionRecordArgs {
    /// Short decision title.
    pub title: String,
    /// 1-3 sentence summary (≤ 280 chars per §7.7; not re-enforced here — the
    /// MCP `input_schema` carries the `maxLength`).
    pub summary: String,
    /// Why this choice over the alternatives.
    pub rationale: String,
    /// Alternatives considered and rejected.
    pub alternatives: Vec<RejectedAlternative>,
    /// A prior decision this one supersedes, if any.
    pub supersedes: Option<DecisionId>,
}

/// Arguments for the `task_report` tool (design §7.7).
#[derive(Debug, Clone)]
pub struct TaskReportArgs {
    /// Advisory report kind — routing is uniform at v1.0.
    pub kind: String,
    /// The report payload.
    pub payload: serde_json::Value,
    /// Whether the report blocks the task pending a plan revision (§7.7 default
    /// `true`; the rmcp transport layer in Phase 7 applies the default when the
    /// field is absent).
    pub blocking: bool,
}

/// A handler-level MCP tool call failed.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The bus rejected the emit (an illegal transition, a repo failure).
    #[error("tool emit failed: {0}")]
    Bus(#[from] BusError),
    /// The [`WorkerToolHost`] (subprocess / git) failed.
    #[error("tool host failed: {0}")]
    Host(#[from] ToolHostError),
    /// Allocating a fresh [`DecisionId`] failed at the repo layer.
    #[error("decision id allocation failed: {0}")]
    IdAllocation(RepoError),
    /// A [`DecisionRecord`] could not be constructed (origin / phase-run mutex).
    #[error("decision record invalid: {0}")]
    Decision(#[from] DecisionError),
    /// `verify_run` / `worktree_diff` were called from a spec-level phase — a
    /// worktree-scoped tool has no task worktree to act on (§7.6 fail-loud).
    #[error("tool '{tool}' requires a task worktree but the session is spec-level")]
    NotInTaskWorktree {
        /// The tool that was called without a task.
        tool: &'static str,
    },
    /// `task_report` was called from a spec-level phase. `BoiEvent::ReportReceived`
    /// carries a non-optional `TaskId`, so a spec-level worker structurally
    /// cannot file one — the plan's Task 4.5 flagged only verify/worktree as
    /// task-scoped, but the event type makes `task_report` task-scoped too.
    #[error("task_report requires a task but the session is spec-level")]
    ReportRequiresTask,
}

/// The MCP tool handlers — the worker → harness transport endpoints (§7.6).
///
/// Each handler constructs a typed [`BoiEvent`] and routes it through
/// [`EventBus::emit`]. The MCP tool is *only* transport; the bus, not the tool,
/// performs the four-phase emit.
///
/// ## Deviation — the `pool` field
///
/// The plan's Task 4.5 struct is `McpHandlers { bus, host }`. But
/// `decision_record` must mint a fresh [`DecisionId`] via
/// `repo::decisions::allocate_decision_id`, which needs the `SqlitePool`, and
/// [`EventBus`]'s pool is private (the chokepoint hides it). ID allocation is a
/// pure `repo` call — not a state mutation — consistent with Task 4.3's
/// persistence-chokepoint boundary note ("creation rows are written by `repo`
/// calls"). So `McpHandlers` carries the `pool`; `SqlitePool` is cheap to clone
/// (an internal `Arc`). Documented as a Phase 4 deviation.
pub struct McpHandlers {
    bus: Arc<EventBus>,
    host: Arc<dyn WorkerToolHost>,
    pool: sqlx::SqlitePool,
}

impl McpHandlers {
    /// Construct the handler set.
    pub fn new(bus: Arc<EventBus>, host: Arc<dyn WorkerToolHost>, pool: sqlx::SqlitePool) -> Self {
        Self { bus, host, pool }
    }

    /// `decision_record` — mint a [`DecisionId`], build a runtime
    /// [`DecisionRecord`], emit [`BoiEvent::DecisionMade`]. Returns the new id.
    pub async fn decision_record(
        &self,
        session: &WorkerSession,
        args: DecisionRecordArgs,
    ) -> Result<DecisionId, McpError> {
        let id = repo::decisions::allocate_decision_id(&self.pool)
            .await
            .map_err(McpError::IdAllocation)?;
        // origin = Runtime → phase_run_id is Some (the worker's current run).
        let decision = DecisionRecord::new_runtime(
            id.clone(),
            session.spec_id.clone(),
            Some(session.phase_run_id.clone()),
            args.title,
            args.summary,
            args.rationale,
            args.alternatives,
            args.supersedes,
            Utc::now(),
        )?;
        self.bus.emit(&BoiEvent::DecisionMade { decision }).await?;
        Ok(id)
    }

    /// `task_report` — emit [`BoiEvent::ReportReceived`]; the plan layer
    /// (Phase 5b) reacts.
    pub async fn task_report(
        &self,
        session: &WorkerSession,
        args: TaskReportArgs,
    ) -> Result<(), McpError> {
        let task_id = session
            .task_id
            .clone()
            .ok_or(McpError::ReportRequiresTask)?;
        self.bus
            .emit(&BoiEvent::ReportReceived {
                spec_id: session.spec_id.clone(),
                task_id,
                kind: args.kind,
                payload: args.payload,
                blocking: args.blocking,
            })
            .await?;
        Ok(())
    }

    /// `verify_run` — run the command through the [`WorkerToolHost`], emit
    /// [`BoiEvent::VerifyChecked`], and return the command's output.
    ///
    /// Requires a task worktree — a spec-level session yields
    /// [`McpError::NotInTaskWorktree`].
    pub async fn verify_run(
        &self,
        session: &WorkerSession,
        command: String,
    ) -> Result<VerificationOutput, McpError> {
        let task_id = session
            .task_id
            .as_ref()
            .ok_or(McpError::NotInTaskWorktree { tool: "verify_run" })?;
        let output = self.host.run_verification(task_id, &command).await?;
        self.bus
            .emit(&BoiEvent::VerifyChecked {
                spec_id: session.spec_id.clone(),
                task_id: task_id.clone(),
                // The `verify_run` tool input carries no level — the worker is
                // running an ad-hoc command. `unspecified` is honest: it does
                // not fabricate an `l1`/`l2`/`l3` tier the call never declared.
                level: "unspecified".to_owned(),
                command,
                exit_code: output.exit_code,
                stdout_excerpt: excerpt(&output.stdout),
            })
            .await?;
        Ok(output)
    }

    /// `worktree_diff` — get the worktree diff through the [`WorkerToolHost`],
    /// emit [`BoiEvent::ToolInvoked`], and return the diff.
    ///
    /// Requires a task worktree — a spec-level session yields
    /// [`McpError::NotInTaskWorktree`].
    ///
    /// ## `ToolInvoked` for a BOI-defined tool — a deliberate plan-over-§7.6
    /// choice (review B-bus-S2)
    ///
    /// Design §7.6's worker→bus table maps `BoiEvent::ToolInvoked` to
    /// *provider-native* tools (Read/Edit/Bash, captured by the provider
    /// driver); the three other BOI MCP tools each emit a *typed* event
    /// (`DecisionMade`, `ReportReceived`, `VerifyChecked`). `worktree_diff` is
    /// a BOI-defined tool, so it strictly falls outside §7.6's `ToolInvoked`
    /// row. The plan's Task 4.5 nonetheless routes it through `ToolInvoked`
    /// **on purpose**: `worktree_diff` is a read-only self-check producing no
    /// state transition, so it warrants no dedicated `BoiEvent` variant — and
    /// `ToolInvoked` means exactly "a tool ran, here is a summary". Adding a
    /// `WorktreeDiffed` variant for one read-only tool would bloat the locked
    /// `BoiEvent` enum (Phase 1) for no routing benefit. The `tool` attribute
    /// (`"worktree_diff"`) is what distinguishes this BOI-tool `ToolInvoked`
    /// from a provider-native one for any 8a/8c provenance query — a consumer
    /// keys on `tool`, never on the variant alone.
    pub async fn worktree_diff(&self, session: &WorkerSession) -> Result<String, McpError> {
        let task_id = session
            .task_id
            .as_ref()
            .ok_or(McpError::NotInTaskWorktree {
                tool: "worktree_diff",
            })?;
        let diff = self.host.worktree_diff(task_id).await?;
        self.bus
            .emit(&BoiEvent::ToolInvoked {
                spec_id: session.spec_id.clone(),
                task_id: Some(task_id.clone()),
                // The `tool` attribute is the §7.6-reconciliation seam: a
                // consumer distinguishes this BOI-tool invocation from a
                // provider-native one by this name (review B-bus-S2).
                tool: "worktree_diff".to_owned(),
                args_summary: "{}".to_owned(),
                result_summary: format!("{} bytes of diff", diff.len()),
            })
            .await?;
        Ok(diff)
    }
}

/// Truncate a command's stdout to a bounded excerpt for an event payload.
///
/// The full output travels in the returned [`VerificationOutput`]; the
/// [`BoiEvent::VerifyChecked`] event only needs a bounded sample for OTel.
fn excerpt(text: &str) -> String {
    const MAX: usize = 2000;
    if text.len() <= MAX {
        return text.to_owned();
    }
    // Truncate on a char boundary, not a byte index.
    let cut = text
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= MAX)
        .last()
        .unwrap_or(0);
    format!("{}… [truncated]", &text[..cut])
}

/// Test doubles for the MCP tool layer.
///
/// Behind `#[cfg(test)]` and `pub(crate)` — Task 4.5 and Phase 5 test modules
/// consume `MockToolHost`, but the crate's public surface never exposes it.
#[cfg(test)]
pub(crate) mod testkit {
    use super::{ToolHostError, VerificationOutput, WorkerToolHost};
    use crate::types::ids::TaskId;
    use async_trait::async_trait;

    /// A [`WorkerToolHost`] with scripted outputs — no real subprocess or git.
    #[derive(Debug, Clone)]
    pub(crate) struct MockToolHost {
        /// The [`VerificationOutput`] every `run_verification` call returns.
        pub(crate) verification: VerificationOutput,
        /// The diff string every `worktree_diff` call returns.
        pub(crate) diff: String,
    }

    impl MockToolHost {
        /// A mock that returns `exit_code 0` verifications and a fixed diff.
        pub(crate) fn passing() -> Self {
            Self {
                verification: VerificationOutput {
                    exit_code: 0,
                    stdout: "all checks green".to_owned(),
                    stderr: String::new(),
                },
                diff: "diff --git a/src/a.rs b/src/a.rs\n+ added a line\n".to_owned(),
            }
        }
    }

    #[async_trait]
    impl WorkerToolHost for MockToolHost {
        async fn run_verification(
            &self,
            _task_id: &TaskId,
            _command: &str,
        ) -> Result<VerificationOutput, ToolHostError> {
            Ok(self.verification.clone())
        }

        async fn worktree_diff(&self, _task_id: &TaskId) -> Result<String, ToolHostError> {
            Ok(self.diff.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `tool_catalog()` returns exactly the 4 expected tools, in order.
    #[test]
    fn test_l1_tool_catalog_has_four_named_tools() {
        let tools = tool_catalog();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(
            names,
            vec![
                "decision_record",
                "task_report",
                "verify_run",
                "worktree_diff"
            ],
        );
    }

    /// Every tool's `input_schema` is a JSON Schema *object* with a
    /// `properties` map (the structural minimum for a valid tool schema).
    #[test]
    fn test_l1_every_tool_schema_is_a_valid_object() {
        for tool in tool_catalog() {
            let schema = &*tool.input_schema;
            assert_eq!(
                schema.get("type").and_then(|t| t.as_str()),
                Some("object"),
                "tool {} schema is not type=object",
                tool.name,
            );
            assert!(
                schema.get("properties").map(|p| p.is_object()) == Some(true),
                "tool {} schema has no properties object",
                tool.name,
            );
            // Every tool carries a non-empty description.
            assert!(
                tool.description.as_ref().is_some_and(|d| !d.is_empty()),
                "tool {} has no description",
                tool.name,
            );
        }
    }

    /// `decision_record` requires title / summary / rationale / alternatives,
    /// and `summary` is capped at 280 chars.
    #[test]
    fn test_l1_decision_record_required_fields_and_summary_cap() {
        let tool = tool_catalog()
            .into_iter()
            .find(|t| t.name == "decision_record")
            .expect("decision_record is in the catalog");
        let schema = &*tool.input_schema;

        let required: Vec<&str> = schema
            .get("required")
            .and_then(|r| r.as_array())
            .expect("decision_record schema has a `required` array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        for field in ["title", "summary", "rationale", "alternatives"] {
            assert!(
                required.contains(&field),
                "decision_record must require `{field}`",
            );
        }

        // summary maxLength is 280 (§7.7).
        let summary_max = schema
            .get("properties")
            .and_then(|p| p.get("summary"))
            .and_then(|s| s.get("maxLength"))
            .and_then(|m| m.as_u64());
        assert_eq!(
            summary_max,
            Some(280),
            "summary must be capped at 280 chars"
        );

        // G12: `supersedes` carries NO `pattern` — the newtype validates.
        let supersedes_pattern = schema
            .get("properties")
            .and_then(|p| p.get("supersedes"))
            .and_then(|s| s.get("pattern"));
        assert!(
            supersedes_pattern.is_none(),
            "G12: decision_record `supersedes` must carry no JSON-schema pattern",
        );
    }

    /// `task_report` requires kind + payload; `blocking` is optional with a
    /// `true` default.
    #[test]
    fn test_l1_task_report_required_fields_and_blocking_default() {
        let tool = tool_catalog()
            .into_iter()
            .find(|t| t.name == "task_report")
            .expect("task_report is in the catalog");
        let schema = &*tool.input_schema;

        let required: Vec<&str> = schema
            .get("required")
            .and_then(|r| r.as_array())
            .expect("task_report schema has a `required` array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(required, vec!["kind", "payload"]);

        let blocking_default = schema
            .get("properties")
            .and_then(|p| p.get("blocking"))
            .and_then(|b| b.get("default"))
            .and_then(|d| d.as_bool());
        assert_eq!(blocking_default, Some(true), "blocking defaults to true");
    }

    /// `verify_run` requires `command`; `worktree_diff` requires nothing.
    #[test]
    fn test_l1_verify_run_and_worktree_diff_param_shapes() {
        let catalog = tool_catalog();

        let verify = catalog.iter().find(|t| t.name == "verify_run").unwrap();
        let verify_required: Vec<&str> = verify
            .input_schema
            .get("required")
            .and_then(|r| r.as_array())
            .expect("verify_run has a `required` array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(verify_required, vec!["command"]);

        // worktree_diff has no required params (the `required` key is absent).
        let diff = catalog.iter().find(|t| t.name == "worktree_diff").unwrap();
        assert!(
            diff.input_schema.get("required").is_none(),
            "worktree_diff takes no required params",
        );
    }

    /// The `MockToolHost` double returns its scripted verification + diff.
    #[tokio::test]
    async fn test_l1_mock_tool_host_returns_scripted_output() {
        use testkit::MockToolHost;
        let host = MockToolHost::passing();
        let task = TaskId::new("T0000001a").unwrap();

        let out = host.run_verification(&task, "cargo test").await.unwrap();
        assert_eq!(out.exit_code, 0);
        let diff = host.worktree_diff(&task).await.unwrap();
        assert!(diff.contains("diff --git"));
    }

    // -----------------------------------------------------------------------
    // Task 4.5 L2 tests — real pool, RecordingObserver, MockToolHost.
    // -----------------------------------------------------------------------

    use crate::repo::db::connect;
    use crate::service::bus::EventBus;
    use crate::service::bus::testkit::RecordingObserver;
    use crate::types::event::BoiEvent;
    use testkit::MockToolHost;

    fn spec_id() -> SpecId {
        SpecId::new("S0000001a").unwrap()
    }
    fn task_id() -> TaskId {
        TaskId::new("T0000001a").unwrap()
    }
    fn phase_run_id() -> PhaseRunId {
        PhaseRunId::new("P0000001a").unwrap()
    }

    /// An in-memory pool seeded with a spec (+ v1 snapshot + `spec_runtime`), a
    /// task, and one open phase run — the FK target for runtime decisions.
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

    /// Build `McpHandlers` over a seeded pool with a `RecordingObserver`-backed
    /// bus and a passing `MockToolHost`. Returns the handlers + the recorder.
    async fn handlers() -> (McpHandlers, RecordingObserver) {
        let pool = seeded_pool().await;
        let rec = RecordingObserver::new();
        let bus = Arc::new(EventBus::new(pool.clone(), vec![Arc::new(rec.clone())]));
        let host: Arc<dyn WorkerToolHost> = Arc::new(MockToolHost::passing());
        (McpHandlers::new(bus, host, pool), rec)
    }

    /// A task-scoped session for the seeded spec/task.
    fn task_session() -> WorkerSession {
        WorkerSession {
            spec_id: spec_id(),
            task_id: Some(task_id()),
            phase_run_id: phase_run_id(),
        }
    }

    /// A spec-level session — no task — for the worktree-tool rejection tests.
    fn spec_session() -> WorkerSession {
        WorkerSession {
            spec_id: spec_id(),
            task_id: None,
            phase_run_id: phase_run_id(),
        }
    }

    /// `decision_record` mints a well-formed `DecisionId`, inserts a
    /// `decisions` row (verified via the handler's own pool), and the observer
    /// sees a `DecisionMade` event.
    #[tokio::test]
    async fn test_l2_decision_record_inserts_row_and_observes() {
        // Build the handler set explicitly so the test keeps the pool handle
        // and can read the `decisions` row back.
        let pool = seeded_pool().await;
        let rec = RecordingObserver::new();
        let bus = Arc::new(EventBus::new(pool.clone(), vec![Arc::new(rec.clone())]));
        let host: Arc<dyn WorkerToolHost> = Arc::new(MockToolHost::passing());
        let h = McpHandlers::new(bus, host, pool.clone());

        let id = h
            .decision_record(
                &task_session(),
                DecisionRecordArgs {
                    title: "Use sqlx".into(),
                    summary: "Compile-checked queries.".into(),
                    rationale: "Type safety.".into(),
                    alternatives: vec![RejectedAlternative {
                        name: "diesel".into(),
                        reason: "heavier macros".into(),
                    }],
                    supersedes: None,
                },
            )
            .await
            .unwrap();

        // The returned id is a well-formed DecisionId (re-parses).
        assert!(DecisionId::new(id.as_str()).is_ok());
        // A `decisions` row genuinely exists for that id.
        let row = repo::fetch_by_id(&pool, &id).await.unwrap();
        assert_eq!(row.title, "Use sqlx");
        assert_eq!(row.origin, crate::types::decision::DecisionOrigin::Runtime);
        // The observer saw exactly one DecisionMade carrying that id.
        let seen = rec.seen();
        assert_eq!(seen.len(), 1);
        let BoiEvent::DecisionMade { decision } = &seen[0] else {
            unreachable!("expected DecisionMade");
        };
        assert_eq!(decision.id, id);
    }

    /// `task_report` with `blocking = false` emits a `ReportReceived` carrying
    /// `blocking = false`.
    #[tokio::test]
    async fn test_l2_task_report_emits_report_received_non_blocking() {
        let (h, rec) = handlers().await;

        h.task_report(
            &task_session(),
            TaskReportArgs {
                kind: "scope_creep".into(),
                payload: serde_json::json!({ "detail": "extra work found" }),
                blocking: false,
            },
        )
        .await
        .unwrap();

        let seen = rec.seen();
        assert_eq!(seen.len(), 1);
        let BoiEvent::ReportReceived { blocking, kind, .. } = &seen[0] else {
            unreachable!("expected ReportReceived");
        };
        assert!(!blocking, "blocking=false must be carried through");
        assert_eq!(kind, "scope_creep");
    }

    /// `verify_run` runs the host command, emits `VerifyChecked`, and returns
    /// the command's output (MockToolHost scripts exit 0).
    #[tokio::test]
    async fn test_l2_verify_run_emits_verify_checked_and_returns_output() {
        let (h, rec) = handlers().await;

        let out = h
            .verify_run(&task_session(), "cargo test".into())
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout, "all checks green");

        let seen = rec.seen();
        assert_eq!(seen.len(), 1);
        let BoiEvent::VerifyChecked {
            command, exit_code, ..
        } = &seen[0]
        else {
            unreachable!("expected VerifyChecked");
        };
        assert_eq!(command, "cargo test");
        assert_eq!(*exit_code, 0);
    }

    /// `verify_run` from a spec-level session (no task) is rejected loudly with
    /// `McpError::NotInTaskWorktree` — no event is emitted.
    #[tokio::test]
    async fn test_l2_verify_run_without_task_is_not_in_task_worktree() {
        let (h, rec) = handlers().await;

        let err = h
            .verify_run(&spec_session(), "cargo test".into())
            .await
            .unwrap_err();
        assert!(
            matches!(err, McpError::NotInTaskWorktree { tool: "verify_run" }),
            "got {err:?}",
        );
        // The failure was loud and pre-emptive — nothing was emitted.
        assert_eq!(rec.count(), 0);
    }

    /// `worktree_diff` returns the mock diff and emits a `ToolInvoked` event.
    #[tokio::test]
    async fn test_l2_worktree_diff_returns_diff_and_emits_tool_invoked() {
        let (h, rec) = handlers().await;

        let diff = h.worktree_diff(&task_session()).await.unwrap();
        assert!(diff.contains("diff --git"), "mock diff returned");

        let seen = rec.seen();
        assert_eq!(seen.len(), 1);
        let BoiEvent::ToolInvoked { tool, .. } = &seen[0] else {
            unreachable!("expected ToolInvoked");
        };
        assert_eq!(tool, "worktree_diff");
    }

    /// `worktree_diff` from a spec-level session is rejected with
    /// `NotInTaskWorktree`.
    #[tokio::test]
    async fn test_l2_worktree_diff_without_task_is_rejected() {
        let (h, _rec) = handlers().await;
        let err = h.worktree_diff(&spec_session()).await.unwrap_err();
        assert!(matches!(
            err,
            McpError::NotInTaskWorktree {
                tool: "worktree_diff"
            }
        ));
    }

    /// `task_report` from a spec-level session is rejected with
    /// `ReportRequiresTask` — `BoiEvent::ReportReceived` forces a `TaskId`.
    #[tokio::test]
    async fn test_l2_task_report_without_task_is_rejected() {
        let (h, _rec) = handlers().await;
        let err = h
            .task_report(
                &spec_session(),
                TaskReportArgs {
                    kind: "spec_bug".into(),
                    payload: serde_json::Value::Null,
                    blocking: true,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::ReportRequiresTask), "got {err:?}");
    }
}
