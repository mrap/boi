//! Regression test for the first-run crash (adversarial-review item): any
//! DB-opening command run before `~/.boi/v2/` exists (i.e. before the daemon
//! has ever started, which is the only thing that used to create it — see
//! `cli::daemon::run`'s `create_dir_all` of the log dir) used to fail with a
//! raw sqlite "unable to open database file" error, because `mode=rwc`
//! creates the database *file* but never its parent directory
//! (`cli::paths::boi_db_url`).
//!
//! This cold-boots the real `boi` binary against a `$HOME` with no
//! `~/.boi/` at all and proves `boi spec show <id>` (a read-only, no-daemon
//! command — see `src/cli/spec.rs`) now:
//!
//! 1. Does NOT surface the raw sqlite "unable to open database file" text.
//! 2. Fails with the expected, clean "not found" error instead (the spec id
//!    is well-formed but does not exist — the schema still has to migrate
//!    successfully first).
//! 3. Actually created `~/.boi/v2/boi.db` on disk, proving the parent
//!    directory got created rather than the command failing before ever
//!    reaching sqlite.
//!
//! ## Hermeticity
//!
//! Mirrors `tests/daemon_logging_smoke.rs`: `HOME` is set only on the
//! spawned child's environment (`Command::env`), never the test process's
//! own env — no `unsafe { std::env::set_var }`, no cross-test races.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::process::Command;

/// Build a unique per-test scratch directory under the system temp dir; the
/// caller passes its path as the child's `$HOME`. The directory itself
/// exists (a real `$HOME` always does) but its `.boi/` subdirectory does
/// NOT — that absence is exactly the first-run condition under test.
fn temp_home(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("boi-first-run-{}-{tag}-{n}", std::process::id(),));
    std::fs::create_dir_all(&p).expect("create temp home");
    p
}

#[tokio::test]
async fn test_l1_spec_show_before_any_boi_dir_exists_creates_it_instead_of_crashing() {
    let home = temp_home("spec-show");
    assert!(
        !home.join(".boi").exists(),
        "precondition: no ~/.boi/ must exist yet"
    );

    let bin = env!("CARGO_BIN_EXE_boi");
    let output = Command::new(bin)
        // A well-formed but nonexistent spec id (9 chars: `S` + 8-char
        // Crockford-base32 body — see `types::ids`) so validation passes
        // and the command reaches the database.
        .args(["spec", "show", "S0000001a"])
        .env("HOME", &home)
        .env_remove("RUST_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("spawn boi spec show");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // The pre-fix crash signature must be gone.
    assert!(
        !stderr.contains("unable to open database file"),
        "first-run crash regressed: `~/.boi/v2/` was not created before the \
         database was opened; stderr: {stderr}\nstdout: {stdout}",
    );

    // The command must still fail — the spec genuinely does not exist — but
    // with the clean, structured "not found" error, not a driver-level fault.
    assert!(!output.status.success(), "expected a non-zero exit");
    assert!(
        stderr.contains("row not found") || stderr.contains("not found"),
        "expected a clean not-found error, got stderr: {stderr}",
    );

    // The parent directory + migrated database must now exist on disk —
    // proof the fix's `create_dir_all` actually ran on this code path.
    assert!(
        home.join(".boi").join("v2").is_dir(),
        "expected ~/.boi/v2/ to have been created by `boi spec show`"
    );
    assert!(
        home.join(".boi").join("v2").join("boi.db").is_file(),
        "expected ~/.boi/v2/boi.db to have been created (and migrated) by `boi spec show`"
    );

    drop(std::fs::remove_dir_all(&home));
}
