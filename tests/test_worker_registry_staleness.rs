//! Reproducer for Bug B: worker registry staleness on file MODIFICATION.
//!
//! `run_worker` (worker.rs:273) calls `PhaseRegistry::new()` once at worker start.
//! All tasks in a spec share that single in-memory registry. The `get()` function
//! checks if the *source file was deleted* and falls back to core in that case —
//! but it does NOT re-read the file on modification. If a user phase TOML is
//! modified mid-spec, the running worker never picks up the change.
//!
//! Test isolation (C1-C4 hard constraints):
//!   - Uses std::env::temp_dir() only — no system paths touched
//!   - Pure in-process test — no daemon spawn, no subprocess
//!   - No binary deployment to any system path
//!
//! RED: this test fails on the unfixed codebase (get() doesn't re-read files).
//! GREEN: this test passes once the worker refreshes the registry per-task, or
//!        once get() detects mtime changes and re-reads modified files.

use boi::phases::PhaseRegistry;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_dir(label: &str) -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir()
        .join(format!("boi-staleness-{}-{}-{}", label, std::process::id(), n));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create tmp dir");
    dir
}

/// Core phase: safe default — deterministic, no Claude needed.
const CORE_T_VERIFY: &str = r#"
name = "t-verify"

[phase]
name = "t-verify"
level = "task"
requires_claude = false
timeout_minutes = 5

[worker]
runtime = "deterministic"
"#;

/// User override v1: forces Claude for every t-verify run.
const USER_OVERRIDE_CLAUDE_TRUE: &str = r#"
name = "t-verify"

[phase]
requires_claude = true

[worker]
runtime = "claude"
model = "claude-sonnet-4-6"
"#;

/// User override v2: user decides they no longer need Claude for t-verify.
const USER_OVERRIDE_CLAUDE_FALSE: &str = r#"
name = "t-verify"

[phase]
requires_claude = false

[worker]
runtime = "deterministic"
"#;

/// Prove that once a PhaseRegistry is constructed, modifying a user phase file
/// mid-spec is NOT reflected in registry lookups (the staleness bug).
///
/// The test simulates the worker lifecycle:
///   - Registry built once at worker startup (mirrors `PhaseRegistry::new()` at worker.rs:273).
///   - User modifies their override file mid-spec (changes requires_claude true→false).
///   - The same registry is consulted for the next task.
///
/// Bug: the registry still returns the old (stale) requires_claude=true, because
/// it loaded the HashMap once and `get()` only checks for deleted files — it does
/// NOT re-read files that still exist but have changed content.
///
/// This assertion FAILS on unfixed code: we assert requires_claude=false (the new
/// file content), but get() returns requires_claude=true (the stale cached value).
#[test]
fn test_worker_registry_staleness_after_user_file_modified() {
    let core_dir = tmp_dir("core");
    let user_dir = tmp_dir("user");

    fs::write(core_dir.join("t-verify.phase.toml"), CORE_T_VERIFY).unwrap();
    fs::write(user_dir.join("t-verify.phase.toml"), USER_OVERRIDE_CLAUDE_TRUE).unwrap();

    // ── Worker startup: registry loaded exactly once (mirrors run_worker line 273) ──
    let mut registry = PhaseRegistry::from_dir(&core_dir);
    registry.load_user_phases(&user_dir);

    // Pre-condition: user override v1 is active at task 1 (requires_claude=true).
    let phase_task1 = registry.get("t-verify").unwrap();
    assert!(
        phase_task1.requires_claude,
        "pre-condition: user override must make requires_claude=true initially"
    );

    // ── Mid-spec: user modifies their override (still exists, but changed content) ──
    // This simulates a user updating their phase config while a spec is running.
    fs::write(user_dir.join("t-verify.phase.toml"), USER_OVERRIDE_CLAUDE_FALSE).unwrap();

    // ── Task 2: same worker, same registry — no reload on the unfixed code ──
    //
    // Desired (bug-free): after the file changes, the registry should return the
    // updated requires_claude=false so no unnecessary Claude is spawned.
    //
    // Actual (buggy): the in-memory HashMap still holds the v1 entry with
    // requires_claude=true, because get() only checks file existence (not mtime
    // or content). This assertion FAILS on the unfixed codebase.
    let phase_task2 = registry.get("t-verify").unwrap();
    assert_eq!(
        phase_task2.requires_claude,
        false,
        "staleness bug: after modifying user override mid-spec (requires_claude true→false), \
         the worker registry still returns requires_claude=true (stale cached value). \
         Fix: reload registry per-task or detect mtime change in get()."
    );

    let _ = fs::remove_dir_all(&core_dir);
    let _ = fs::remove_dir_all(&user_dir);
}
