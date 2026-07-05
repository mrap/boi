//! The Unix-domain control socket connecting short-lived `boi` write-side
//! commands to the long-running `boi daemon`.
//!
//! ## Two halves
//!
//! - **Client** ([`send_command`]) — every write-side command (`dispatch`,
//!   `cancel`, `unblock`, `resolve-conflict`, `fail`) and `boi mcp-serve`
//!   connects to `~/.boi/v2/daemon.sock`, writes one
//!   [`DaemonCommand`] JSON line, and reads one
//!   [`DaemonResponse`] line. **No daemon
//!   listening → [`ControlError::NoDaemon`]** — a loud, non-zero-exit failure
//!   (SO S6); the command never falls back to a DB-only write.
//! - **Server** ([`serve`]) — the daemon's socket-listener task. It binds the
//!   socket, accepts connections, and hands each decoded `DaemonCommand` to a
//!   [`CommandHandler`]; the handler's `DaemonResponse` is written back. The
//!   listener loops until its `shutdown` token fires.
//!
//! ## Framing
//!
//! One JSON object per line each way — request then response, then the
//! connection closes. The simplest correct framing for a synchronous
//! request/reply socket.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

use crate::service::{DaemonCommand, DaemonResponse};

/// A control-socket operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    /// No daemon is listening on the control socket — the connect failed.
    ///
    /// This is the load-bearing failure mode (SO S6): a write-side command
    /// with no daemon fails loud here and exits non-zero, rather than mutating
    /// the DB directly while a live orphan worker still runs.
    #[error(
        "no boi daemon is running (control socket {socket} unreachable) — \
         start one with `boi daemon`"
    )]
    NoDaemon {
        /// The socket path that could not be reached.
        socket: PathBuf,
    },
    /// The socket I/O failed after a connection was established.
    #[error("control-socket I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// A command or response could not be (de)serialized.
    #[error("control-socket protocol error: {0}")]
    Protocol(#[from] serde_json::Error),
    /// The daemon closed the connection before sending a response.
    #[error("the daemon closed the connection without a response")]
    NoResponse,
}

/// Handles one decoded [`DaemonCommand`] daemon-side.
///
/// The daemon's socket listener owns a `CommandHandler`; for each accepted
/// connection it decodes a `DaemonCommand` and calls [`CommandHandler::handle`].
/// The implementation lives in `cli::daemon` (it needs the daemon's bus +
/// `daemon_tx` + dispatch context); keeping the trait here lets [`serve`] stay
/// transport-only.
#[async_trait::async_trait]
pub trait CommandHandler: Send + Sync {
    /// Process one command, returning the reply to write back to the client.
    ///
    /// An `Err` outcome must be encoded as a [`DaemonResponse::Err`] — a
    /// handler never panics a connection; a daemon-side fault is reported to
    /// the operator, not swallowed.
    async fn handle(&self, command: DaemonCommand) -> DaemonResponse;
}

/// Send one [`DaemonCommand`] to the daemon and return its [`DaemonResponse`].
///
/// Connects to `socket`, writes the command as one JSON line, and reads one
/// JSON-line response. A failed *connect* is [`ControlError::NoDaemon`] — the
/// loud "no daemon" signal write-side commands depend on.
pub async fn send_command(
    socket: &Path,
    command: &DaemonCommand,
) -> Result<DaemonResponse, ControlError> {
    let stream = UnixStream::connect(socket).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::ConnectionRefused
            || e.kind() == std::io::ErrorKind::NotFound
        {
            ControlError::NoDaemon {
                socket: socket.to_path_buf(),
            }
        } else {
            ControlError::Io(e)
        }
    })?;
    let (read_half, mut write_half) = stream.into_split();

    // Request — one JSON line.
    let mut line = serde_json::to_string(command)?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    // Half-close the write side so the daemon's `read_line` sees EOF if it
    // ever reads past the single request line. `shutdown()` flushes implicitly.
    write_half.shutdown().await?;

    // Response — one JSON line.
    let mut reader = BufReader::new(read_half);
    let mut resp_line = String::new();
    let n = reader.read_line(&mut resp_line).await?;
    if n == 0 {
        return Err(ControlError::NoResponse);
    }
    Ok(serde_json::from_str(resp_line.trim_end())?)
}

/// Bind the control socket and serve [`DaemonCommand`]s until `shutdown`.
///
/// Run as one of the daemon's three supervised tasks (Task 9.2). A stale
/// socket file from a prior daemon is removed before binding — a crashed
/// daemon leaves the socket inode behind and `bind` would otherwise fail
/// `AddrInUse`. Each accepted connection is handled on its own task so a slow
/// handler (an interactive `resolve-conflict` shell) never blocks the accept
/// loop.
///
/// Returns `Ok(())` on a clean `shutdown`; a `bind` failure is a loud `Err`
/// (the daemon cannot run without its control surface).
///
/// Takes the socket path **by value** — `serve` is `tokio::spawn`ed (by
/// `boot`), so its future must be `'static` and cannot borrow the path.
pub async fn serve(
    socket: PathBuf,
    handler: Arc<dyn CommandHandler>,
    shutdown: CancellationToken,
) -> Result<(), ControlError> {
    let socket = socket.as_path();
    // A crashed daemon leaves a stale socket inode — remove it before bind.
    let mut stale_removal_err: Option<std::io::Error> = None;
    if socket.exists() {
        if let Err(e) = std::fs::remove_file(socket) {
            tracing::warn!(socket = %socket.display(), error = %e, "could not remove a stale control socket");
            stale_removal_err = Some(e);
        }
    }
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(socket).map_err(|bind_err| {
        if let Some(ref removal_err) = stale_removal_err {
            tracing::error!(
                socket = %socket.display(),
                bind_error = %bind_err,
                removal_error = %removal_err,
                "control socket bind failed; stale socket removal also failed (EADDRINUSE likely)",
            );
        }
        ControlError::Io(bind_err)
    })?;
    tracing::info!(socket = %socket.display(), "control socket listening");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let handler = Arc::clone(&handler);
                        let jh = tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, handler).await {
                                // A per-connection fault is loud but never
                                // takes the daemon down (SO S6).
                                tracing::error!(error = %e, "control-socket connection failed");
                            }
                        });
                        // Log any panic from the connection handler so it is
                        // never silently swallowed by a dropped JoinHandle.
                        tokio::spawn(async move {
                            if let Err(e) = jh.await {
                                if !e.is_cancelled() {
                                    tracing::error!(error = %e, "control-socket connection handler panicked");
                                }
                            }
                        });
                    }
                    Err(e) => {
                        // An accept error is loud; the loop continues — a
                        // single bad accept must not kill the listener.
                        tracing::error!(error = %e, "control-socket accept failed");
                    }
                }
            }
            () = shutdown.cancelled() => {
                tracing::debug!("control socket shutting down");
                // Best-effort socket cleanup so the next daemon binds clean.
                if let Err(e) = std::fs::remove_file(socket) {
                    tracing::debug!(error = %e, "control socket file already gone at shutdown");
                }
                return Ok(());
            }
        }
    }
}

/// Decode one request, dispatch it to `handler`, write the response back.
async fn handle_connection(
    stream: UnixStream,
    handler: Arc<dyn CommandHandler>,
) -> Result<(), ControlError> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut req_line = String::new();
    let n = reader.read_line(&mut req_line).await?;
    if n == 0 {
        // An empty connection (a probe / a client that hung up) — nothing to
        // do; not an error.
        return Ok(());
    }
    let response = match serde_json::from_str::<DaemonCommand>(req_line.trim_end()) {
        Ok(command) => handler.handle(command).await,
        Err(e) => DaemonResponse::Err {
            detail: format!("malformed command: {e}"),
        },
    };
    let mut resp_line = serde_json::to_string(&response)?;
    resp_line.push('\n');
    write_half.write_all(resp_line.as_bytes()).await?;
    write_half.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::testtmp::TempDir;

    /// A handler that echoes the command's discriminant back as an `Ok`.
    struct EchoHandler;

    #[async_trait::async_trait]
    impl CommandHandler for EchoHandler {
        async fn handle(&self, command: DaemonCommand) -> DaemonResponse {
            DaemonResponse::Ok {
                detail: match command {
                    DaemonCommand::Dispatch { spec_id, .. } => format!("dispatched {spec_id}"),
                    DaemonCommand::Cancel { id, .. } => format!("canceled {id}"),
                    DaemonCommand::Unblock { task_id, .. } => format!("unblocked {task_id}"),
                    DaemonCommand::ResolveConflict { task_id } => format!("resolved {task_id}"),
                    DaemonCommand::Fail { spec_id, .. } => format!("failed {spec_id}"),
                    DaemonCommand::ForwardEvent { .. } => "forwarded".to_owned(),
                },
            }
        }
    }

    /// `send_command` against no daemon is a loud [`ControlError::NoDaemon`] —
    /// never a hang, never a silent success.
    #[tokio::test]
    async fn test_l2_send_command_with_no_daemon_is_no_daemon_error() {
        let dir = TempDir::new("control");
        let socket = dir.path().join("absent.sock");
        let err = send_command(
            &socket,
            &DaemonCommand::Cancel {
                id: "S0000001a".to_owned(),
                reason: "x".to_owned(),
            },
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ControlError::NoDaemon { .. }),
            "expected NoDaemon, got {err:?}",
        );
    }

    /// A *non-missing* connect failure (EACCES from an unsearchable parent dir)
    /// must surface as a real [`ControlError::Io`] — NOT be masked as
    /// `NoDaemon`. This locks in the S1 silent-failure fix
    /// (`docs/reviews/final-cli-silent-failure.md`): only `NotFound` /
    /// `ConnectionRefused` mean "no daemon"; every other OS error (a
    /// permission/config fault) is its own loud signal, so the operator isn't
    /// sent hunting for a daemon that may actually be running.
    ///
    /// Verified that with the pre-fix `.map_err(|_| NoDaemon)` this test fails
    /// (the EACCES would be masked as `NoDaemon`).
    #[cfg(unix)]
    #[tokio::test]
    async fn test_l2_send_command_permission_error_is_io_not_no_daemon() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("control-eacces");
        // A subdirectory with no search/exec bit: connecting to a socket path
        // *inside* it fails with EACCES (PermissionDenied), not ENOENT — the
        // path resolution needs `x` on the parent, which 0o000 denies.
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).expect("create locked dir");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("chmod 000");
        let socket = locked.join("daemon.sock");

        let result = send_command(
            &socket,
            &DaemonCommand::Cancel {
                id: "S0000001a".to_owned(),
                reason: "x".to_owned(),
            },
        )
        .await;

        // Restore perms so the TempDir `Drop` can recurse and clean up,
        // regardless of how the assertions below go. `drop(...)` (not `let _ =`)
        // to consume the `#[must_use]` Result without tripping
        // `clippy::let_underscore_must_use`, matching the crate idiom.
        drop(std::fs::set_permissions(
            &locked,
            std::fs::Permissions::from_mode(0o755),
        ));

        let err = result.expect_err("connect into an unsearchable dir must fail");
        assert!(
            !matches!(err, ControlError::NoDaemon { .. }),
            "EACCES must NOT be masked as NoDaemon (S1 regression), got {err:?}",
        );
        match err {
            ControlError::Io(e) => assert_eq!(
                e.kind(),
                std::io::ErrorKind::PermissionDenied,
                "expected PermissionDenied, got {e:?}",
            ),
            other => panic!("expected ControlError::Io(PermissionDenied), got {other:?}"),
        }
    }

    /// A round-trip: `serve` accepts, the handler runs, `send_command` gets the
    /// reply; `shutdown` then stops the listener.
    #[tokio::test]
    async fn test_l2_serve_and_send_command_roundtrip() {
        let dir = TempDir::new("control");
        let socket = dir.path().join("daemon.sock");
        let shutdown = CancellationToken::new();

        let server = tokio::spawn(serve(
            socket.clone(),
            Arc::new(EchoHandler),
            shutdown.clone(),
        ));

        // Wait for the socket to appear (the listener bound).
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let resp = send_command(
            &socket,
            &DaemonCommand::Dispatch {
                spec_id: "S0000001a".to_owned(),
                skills: vec![],
                spec_file: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            resp,
            DaemonResponse::Ok {
                detail: "dispatched S0000001a".to_owned(),
            },
        );

        shutdown.cancel();
        // The listener returns Ok on a clean shutdown.
        server.await.unwrap().unwrap();
    }

    /// A malformed request line gets a `DaemonResponse::Err`, not a dropped
    /// connection — a bad client is told why.
    #[tokio::test]
    async fn test_l2_malformed_request_gets_an_error_response() {
        let dir = TempDir::new("control");
        let socket = dir.path().join("daemon.sock");
        let shutdown = CancellationToken::new();
        let server = tokio::spawn(serve(
            socket.clone(),
            Arc::new(EchoHandler),
            shutdown.clone(),
        ));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // Write raw garbage, read the reply.
        let stream = UnixStream::connect(&socket).await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        write_half.write_all(b"not json\n").await.unwrap();
        write_half.flush().await.unwrap();
        write_half.shutdown().await.unwrap();
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let resp: DaemonResponse = serde_json::from_str(line.trim_end()).unwrap();
        assert!(
            matches!(resp, DaemonResponse::Err { .. }),
            "a malformed request must get an Err response, got {resp:?}",
        );

        shutdown.cancel();
        server.await.unwrap().unwrap();
    }
}
