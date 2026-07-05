//! Smoke test for T8 — `boi daemon serve` must emit a startup INFO log to
//! stderr.
//!
//! Without the `tracing-subscriber` fmt layer wired into `cli::daemon::run`,
//! `tracing::info!` calls in the daemon disappear silently (the OTel JSONL
//! exporter is the only consumer of the tracing graph). Operators see a
//! 0-byte `daemon.log` despite a healthy boot, RUST_LOG is silently
//! ignored, and any cold-boot diagnostic — including the load-bearing
//! "spec workspace not absolute" surface T7 added — is invisible.
//!
//! This test cold-boots the real `boi` binary, captures stderr, and asserts:
//!
//! 1. A line containing `boi daemon starting` appears within a few seconds
//!    of spawn (the contract target is 1 s — the 5 s budget here absorbs
//!    cold-binary launch + CI jitter without flake).
//! 2. The same line names the **socket path** the daemon will bind to.
//!    Item 3 of the task contract is explicit:
//!    `tracing::info!(version = %VERSION, socket = %socket_path.display(), "boi daemon starting")`
//!    — i.e. the cold-boot log proves logging is alive AND names the
//!    operationally critical socket the daemon will listen on. A bare
//!    "boi daemon starting" with no socket field is not enough.
//!
//! ## Hermeticity
//!
//! `HOME` is rewritten to a per-test temp dir so the spawned daemon never
//! touches the real `~/.boi/v2/`. The daemon will eventually fault at the
//! first step that needs a populated layout (`config::load_phases` reading
//! an empty `~/.boi/v2/phases/`), but only AFTER `cli::daemon::run` has
//! installed the fmt layer and emitted the starting line. The child is
//! killed immediately after the assertions are evaluated.
//!
//! ## Lint posture
//!
//! Matches the other `tests/*.rs` test-binary crates: `.unwrap()` /
//! `.expect()` are the right loud-fail in test setup, so the crate-wide
//! allow keeps the `-D warnings` build clean. The `tokio::process::Command`
//! import is permitted here because `scripts/checks/no-subprocess-outside-runtime.sh`
//! exempts `tests/`.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

/// Build a unique per-test scratch directory under the system temp dir; the
/// caller passes its path as the daemon's `$HOME`.
fn temp_home(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "boi-daemon-logging-{}-{tag}-{n}",
        std::process::id(),
    ));
    std::fs::create_dir_all(&p).expect("create temp home");
    p
}

/// Cold-boot the daemon binary and prove its first INFO line on stderr
/// announces `boi daemon starting` AND names the socket path it will bind.
///
/// Without the fmt layer in `cli::daemon::run`, stderr stays empty — the
/// test times out waiting for any line at all and fails. With only the fmt
/// layer but no `socket = %…` field on the starting log (the partial state
/// before this task lands), the timeout finds the "boi daemon starting"
/// line but the `socket=` assertion fires. Once both are in place, the
/// test is green.
#[tokio::test]
async fn test_l3_daemon_cold_boot_emits_starting_info_with_socket_to_stderr() {
    let home = temp_home("starting");

    // `env!("CARGO_BIN_EXE_boi")` is Cargo's standard hook for integration
    // tests — it expands at compile time to the absolute path of the built
    // `boi` binary, so the test does not need to invoke `cargo` itself.
    let bin = env!("CARGO_BIN_EXE_boi");
    let mut child = Command::new(bin)
        // `daemon serve` — the explicit boot-loop subcommand. A bare
        // `boi daemon` is `arg_required_else_help` on current main and
        // exits with help instead of booting.
        .args(["daemon", "serve"])
        .env("HOME", &home)
        // Belt-and-suspenders: even if the surrounding test runner set
        // RUST_LOG to a level that would suppress INFO, the default
        // EnvFilter ("info,boi=info") in daemon.rs is what we're proving.
        .env_remove("RUST_LOG")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        // The daemon blocks until SIGTERM; `kill_on_drop` guarantees the
        // child is reaped even if the assertion panics mid-test.
        .kill_on_drop(true)
        .spawn()
        .expect("spawn boi daemon");

    let stderr = child.stderr.take().expect("piped stderr");
    let mut lines = BufReader::new(stderr).lines();

    // Read stderr lines until we find one containing "boi daemon starting"
    // or hit the budget. The line itself is the result; later lines (OTel
    // init, repo connect, eventual phase-load fault) are noise we discard.
    let starting_line: Option<String> = timeout(Duration::from_secs(5), async {
        while let Ok(Some(line)) = lines.next_line().await {
            if line.contains("boi daemon starting") {
                return Some(line);
            }
        }
        None
    })
    .await
    .unwrap_or(None);

    // Kill the child regardless of outcome so the assertions below fire
    // cleanly without leaving a stray daemon process behind. `.ok()` (not
    // `let _ =`) keeps `clippy::let_underscore_must_use` happy on the
    // must-use Results — converting to Option discards without lint.
    child.start_kill().ok();
    child.wait().await.ok();
    drop(std::fs::remove_dir_all(&home));

    let line = starting_line.expect(
        "expected a `boi daemon starting` INFO line on stderr within 5s of cold boot — \
         the daemon either never emitted it (fmt layer not wired) or the binary failed \
         to spawn at all",
    );

    // Item 3 of the task contract: the cold-boot log must name the socket
    // path. The `socket = %socket_path.display()` field renders as
    // `socket=<path>` in the default fmt layer formatter.
    assert!(
        line.contains("socket="),
        "the cold-boot `boi daemon starting` log must name the socket path \
         (item 3 of the T8 contract), got: {line}",
    );
}
