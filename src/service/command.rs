//! [`DaemonCommand`] — the typed message a short-lived `boi` write-side
//! process sends the long-running `boi daemon` over the control socket.
//!
//! ## Why this type exists (the process-model seam — review (a))
//!
//! `boi` is invoked as many short-lived OS processes; the orchestrator + the
//! [`EventBus`](crate::service::bus::EventBus) live in ONE long-running
//! `boi daemon`. A write-side command (`dispatch`, `cancel`, `unblock`,
//! `resolve-conflict`, `fail`) cannot mutate state itself — a DB-only flip
//! while the daemon holds a live orphan worker is forbidden (SO S6). Instead it
//! serializes a `DaemonCommand`, sends it down `~/.boi/v2/daemon.sock`, and the
//! daemon's socket listener (`cli::control`) translates it into a
//! `daemon_tx.send` so the resulting [`BoiEvent`](crate::types::event::BoiEvent)
//! reaches the daemon's own orchestrator/bus and `transitions.rs` arbitrates.
//!
//! `DaemonCommand` lives in `service/` — NOT `cli/` — because the daemon's
//! command handler (which is `cli/`-side) and a future non-CLI client both
//! need it, and a layer-3 type is reachable from layer 5; the reverse is not.
//!
//! ## Wire format
//!
//! One JSON object per line (`serde_json`), newline-framed — the simplest
//! framing for a request/response Unix-domain socket. The daemon replies with a
//! [`DaemonResponse`], also one JSON line.

use serde::{Deserialize, Serialize};

/// A command from a short-lived `boi` process to the running daemon.
///
/// Every variant is a *write-side* operation — read-only commands (`status`,
/// `log`, `traces`, `failures`, `spec show`) never reach the daemon; they read
/// SQLite / DuckDB directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum DaemonCommand {
    /// `boi dispatch` — the spec's structural rows are ALREADY persisted by the
    /// CLI (the `specs` + `spec_versions` + `spec_runtime` + `task_runtime` +
    /// `task_deps` one-transaction insert). This command tells the daemon to
    /// start the spec: emit `SpecStarted` (`queued → running`), then run
    /// preflight, then route the spec into its pipeline.
    Dispatch {
        /// The freshly-persisted spec's id (already in `spec_runtime` as
        /// `queued`).
        spec_id: String,
        /// The spec's `[[skill]]` names. Carried on the command — NOT in the
        /// `spec_versions` snapshot (G21.3 fixes that shape to
        /// `{spec_contract, task_contracts}`) — so the daemon's preflight can
        /// check the union of skill-backed Goose extensions (G23.1).
        skills: Vec<String>,
        /// Absolute path to the spec YAML on disk; `None` for non-CLI origins
        /// (MCP, API).
        #[serde(default)]
        spec_file: Option<String>,
    },

    /// `boi cancel <id>` — cancel a spec or a single task. The daemon resolves
    /// whether `id` names a spec or a task and emits `SpecCanceled` /
    /// `TaskCanceled` accordingly.
    Cancel {
        /// The spec or task id to cancel.
        id: String,
        /// The operator's cancellation note.
        reason: String,
    },

    /// `boi unblock <task_id>` — force a blocked task back to `active`.
    Unblock {
        /// The blocked task to unblock.
        task_id: String,
        /// Also zero the task's iteration counters (extends the cap).
        reset_counter: bool,
    },

    /// `boi resolve-conflict <task_id>` — the daemon re-creates the task's
    /// merge conflict, drops the operator into an interactive shell, and on a
    /// clean exit emits `TaskUnblocked`.
    ResolveConflict {
        /// The merge-conflicted task.
        task_id: String,
    },

    /// `boi fail <spec_id>` — the G16.6 operator command: mark a spec
    /// `failed{OperatorMarkedFailed}`.
    Fail {
        /// The spec to fail.
        spec_id: String,
        /// The operator's failure note.
        reason: String,
    },

    /// A `boi mcp-serve` worker forwarded one MCP tool call — the daemon
    /// re-emits the carried [`BoiEvent`](crate::types::event::BoiEvent) on its
    /// own bus so the orchestrator sees it (review (a)(iii) — there is no
    /// shared in-process bus; the daemon owns it).
    ///
    /// The event is carried as a serialized JSON value rather than a typed
    /// `BoiEvent` field so this command type does not have to re-export the
    /// whole event enum into its wire contract; the daemon deserializes it.
    ForwardEvent {
        /// The serialized `BoiEvent` the worker's MCP tool call produced.
        event: serde_json::Value,
    },
}

/// The daemon's reply to a [`DaemonCommand`].
///
/// A command is acknowledged only after the daemon has *accepted* it (the
/// event was emitted / queued) — so a `boi cancel` that returns `Ok` means the
/// cancellation genuinely reached the bus, not merely that the socket write
/// succeeded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DaemonResponse {
    /// The command was accepted; the optional message is operator-facing
    /// detail (e.g. "spec S… started").
    Ok {
        /// Human-readable detail for the CLI to print.
        detail: String,
    },
    /// The command was rejected — the detail explains why (an unknown id, an
    /// illegal transition, a daemon-side fault). The CLI exits non-zero.
    Err {
        /// Why the command failed.
        detail: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `DaemonCommand` variant round-trips through the newline-JSON wire
    /// format unchanged.
    #[test]
    fn test_l1_daemon_command_roundtrips() {
        let cases = [
            DaemonCommand::Dispatch {
                spec_id: "S0000001a".to_owned(),
                skills: vec!["context7".to_owned()],
                spec_file: None,
            },
            DaemonCommand::Cancel {
                id: "T0000001a".to_owned(),
                reason: "scope cut".to_owned(),
            },
            DaemonCommand::Unblock {
                task_id: "T0000001a".to_owned(),
                reset_counter: true,
            },
            DaemonCommand::ResolveConflict {
                task_id: "T0000001a".to_owned(),
            },
            DaemonCommand::Fail {
                spec_id: "S0000001a".to_owned(),
                reason: "abandoned".to_owned(),
            },
            DaemonCommand::ForwardEvent {
                event: serde_json::json!({ "type": "decision_made" }),
            },
        ];
        for cmd in cases {
            let line = serde_json::to_string(&cmd).unwrap();
            assert!(!line.contains('\n'), "the wire form must be one line");
            let back: DaemonCommand = serde_json::from_str(&line).unwrap();
            assert_eq!(back, cmd);
        }
    }

    /// A `DaemonResponse` round-trips both ways.
    #[test]
    fn test_l1_daemon_response_roundtrips() {
        for resp in [
            DaemonResponse::Ok {
                detail: "spec S0000001a started".to_owned(),
            },
            DaemonResponse::Err {
                detail: "no such spec".to_owned(),
            },
        ] {
            let line = serde_json::to_string(&resp).unwrap();
            let back: DaemonResponse = serde_json::from_str(&line).unwrap();
            assert_eq!(back, resp);
        }
    }
}
