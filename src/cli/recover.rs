//! The recovery commands — `boi cancel` / `unblock` / `resolve-conflict` /
//! `fail` (design §6) — all control-socket clients.
//!
//! Every recovery command is a *write-side* operation: it cannot mutate the DB
//! itself. It connects to `~/.boi/v2/daemon.sock`, submits a typed
//! [`DaemonCommand`], and the daemon's handler emits the transition through
//! the bus (so `transitions.rs` arbitrates). **No daemon → fail loud,
//! non-zero exit** ([`RecoverError::NoDaemon`]) — never a DB-only flip while a
//! live orphan worker still runs (SO S6).
//!
//! `boi resolve-conflict` has intentionally **no `--ai` flag** (review (d)) —
//! LLM-driven conflict resolution is deferred to v1.x. The daemon-side handler
//! re-creates the conflict and opens an interactive shell
//! (`runtime::resolve_interactively`).

use crate::cli::control::{self, ControlError};
use crate::cli::paths::{self, PathError};
use crate::service::{DaemonCommand, DaemonResponse};

/// A recovery command failed.
#[derive(Debug, thiserror::Error)]
pub enum RecoverError {
    /// The `~/.boi/v2/` path layout could not be resolved.
    #[error(transparent)]
    Path(#[from] PathError),
    /// No daemon is running — the recovery command cannot take effect.
    #[error(
        "no boi daemon is running — recovery commands require a live daemon \
         (a DB-only flip with a live orphan worker is forbidden)"
    )]
    NoDaemon,
    /// The control socket faulted after a connection was made.
    #[error("control socket error: {0}")]
    Socket(String),
    /// The daemon rejected the command — the detail explains why.
    #[error("{0}")]
    Rejected(String),
}

/// Send a recovery [`DaemonCommand`] to the daemon and print its reply.
///
/// Shared by all four recovery commands — the only difference between them is
/// which `DaemonCommand` they build. Resolves the control-socket path, then
/// delegates to [`send_to`].
async fn send(command: DaemonCommand) -> Result<(), RecoverError> {
    let socket = paths::control_socket()?;
    send_to(&socket, command).await
}

/// Send a recovery command to a *specific* socket path.
///
/// The testable seam — a test drives it against an absent socket to assert the
/// loud [`RecoverError::NoDaemon`] without mutating the process `$HOME`.
async fn send_to(socket: &std::path::Path, command: DaemonCommand) -> Result<(), RecoverError> {
    match control::send_command(socket, &command).await {
        Ok(DaemonResponse::Ok { detail }) => {
            println!("{detail}");
            Ok(())
        }
        Ok(DaemonResponse::Err { detail }) => Err(RecoverError::Rejected(detail)),
        Err(ControlError::NoDaemon { .. }) => Err(RecoverError::NoDaemon),
        Err(other) => Err(RecoverError::Socket(other.to_string())),
    }
}

/// `boi cancel <id> --reason "…"` — cancel a spec or a single task.
///
/// The daemon resolves whether `id` names a spec or a task.
pub async fn cancel(id: &str, reason: &str) -> Result<(), RecoverError> {
    send(DaemonCommand::Cancel {
        id: id.to_owned(),
        reason: reason.to_owned(),
    })
    .await
}

/// `boi unblock <task_id> [--reset-counter]` — force a blocked task to active.
pub async fn unblock(task_id: &str, reset_counter: bool) -> Result<(), RecoverError> {
    send(DaemonCommand::Unblock {
        task_id: task_id.to_owned(),
        reset_counter,
    })
    .await
}

/// `boi resolve-conflict <task_id>` — interactively resolve a task's merge
/// conflict.
///
/// The daemon re-creates the conflict and opens an interactive shell; the
/// command blocks for the duration of that shell session (the control-socket
/// response arrives only when the daemon-side handler — including the shell —
/// finishes).
pub async fn resolve_conflict(task_id: &str) -> Result<(), RecoverError> {
    send(DaemonCommand::ResolveConflict {
        task_id: task_id.to_owned(),
    })
    .await
}

/// `boi fail <spec_id> --reason "…"` — mark a spec failed (G16.6 —
/// `OperatorMarkedFailed`).
pub async fn fail(spec_id: &str, reason: &str) -> Result<(), RecoverError> {
    send(DaemonCommand::Fail {
        spec_id: spec_id.to_owned(),
        reason: reason.to_owned(),
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::control::CommandHandler;
    use crate::cli::testtmp::TempDir;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    /// A handler that records the command it was given and replies `Ok`.
    struct RecordingHandler {
        seen: std::sync::Mutex<Vec<DaemonCommand>>,
    }
    #[async_trait::async_trait]
    impl CommandHandler for RecordingHandler {
        async fn handle(&self, command: DaemonCommand) -> DaemonResponse {
            self.seen.lock().unwrap().push(command);
            DaemonResponse::Ok {
                detail: "done".to_owned(),
            }
        }
    }

    /// Run a real control socket with `handler`, returning the socket path and
    /// the shutdown token.
    async fn serve(
        handler: Arc<RecordingHandler>,
    ) -> (std::path::PathBuf, CancellationToken, TempDir) {
        let dir = TempDir::new("recover");
        let socket = dir.path().join("daemon.sock");
        let shutdown = CancellationToken::new();
        tokio::spawn(control::serve(socket.clone(), handler, shutdown.clone()));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        (socket, shutdown, dir)
    }

    /// `cancel` with a running daemon submits a `Cancel` command and lands.
    #[tokio::test]
    async fn test_l2_cancel_submits_cancel_command() {
        let handler = Arc::new(RecordingHandler {
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let (socket, shutdown, _dir) = serve(Arc::clone(&handler)).await;

        let resp = control::send_command(
            &socket,
            &DaemonCommand::Cancel {
                id: "S0000001a".to_owned(),
                reason: "scope cut".to_owned(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(resp, DaemonResponse::Ok { .. }));
        let seen = handler.seen.lock().unwrap();
        assert!(
            matches!(seen.as_slice(), [DaemonCommand::Cancel { .. }]),
            "the daemon received a Cancel command",
        );
        shutdown.cancel();
    }

    /// Every recovery command with NO daemon is a loud `RecoverError::NoDaemon`
    /// — the load-bearing SO S6 guarantee. Driven through `send_to` against an
    /// absent socket (no `$HOME` mutation — race-free).
    #[tokio::test]
    async fn test_l2_recovery_with_no_daemon_is_loud() {
        let dir = TempDir::new("recover-nodaemon");
        let absent = dir.path().join("absent.sock");
        for command in [
            DaemonCommand::Cancel {
                id: "S0000001a".to_owned(),
                reason: "x".to_owned(),
            },
            DaemonCommand::Unblock {
                task_id: "T0000001a".to_owned(),
                reset_counter: false,
            },
            DaemonCommand::Fail {
                spec_id: "S0000001a".to_owned(),
                reason: "x".to_owned(),
            },
            DaemonCommand::ResolveConflict {
                task_id: "T0000001a".to_owned(),
            },
        ] {
            let err = send_to(&absent, command).await.unwrap_err();
            assert!(
                matches!(err, RecoverError::NoDaemon),
                "a recovery command with no daemon is loud, got {err:?}",
            );
        }
    }

    /// `unblock --reset-counter` carries the `reset_counter` flag through.
    #[tokio::test]
    async fn test_l2_unblock_carries_reset_counter_flag() {
        let handler = Arc::new(RecordingHandler {
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let (socket, shutdown, _dir) = serve(Arc::clone(&handler)).await;

        control::send_command(
            &socket,
            &DaemonCommand::Unblock {
                task_id: "T0000001a".to_owned(),
                reset_counter: true,
            },
        )
        .await
        .unwrap();
        let seen = handler.seen.lock().unwrap();
        assert!(
            matches!(
                seen.as_slice(),
                [DaemonCommand::Unblock {
                    reset_counter: true,
                    ..
                }]
            ),
            "the reset_counter flag is carried",
        );
        shutdown.cancel();
    }
}
