//! Integration tests for phase override inheritance.
//!
//! Verifies that user phase TOML overrides inherit missing fields from the matching
//! core phase rather than re-deriving them — fixing the silent `requires_claude` flip
//! that caused task-verify to spawn Claude unnecessarily.
//!
//! Cases:
//!   1. User override with no [phase] section, core has requires_claude=false → inherits false
//!   2. User override with [phase].requires_claude=true, core has false → explicit user wins
//!   3. User override with [worker].timeout=300 only, no [phase] → worker field wins, [phase] inherits
//!   4. User-only phase (no core counterpart) → existing from_toml behavior unchanged
//!   5. WARN log emitted when user override has no [phase] section
//!      (verified via cargo output — fd2/libc dup2 doesn't intercept Rust's
//!      eprintln! under cargo test's Rust-layer stderr capture)

use boi::phases::{PhaseLevel, PhaseRegistry};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

// ─── test helpers ────────────────────────────────────────────────────────────

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn test_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "boi-phase-inherit-{}-{}-{}",
        label,
        std::process::id(),
        n
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create test dir");
    dir
}

/// Build a PhaseRegistry whose *core* is a single TOML written to a fresh temp dir.
fn registry_with_core(filename: &str, core_toml: &str) -> (PhaseRegistry, PathBuf) {
    let dir = test_dir("core");
    fs::write(dir.join(filename), core_toml).unwrap();
    let registry = PhaseRegistry::from_dir(&dir);
    (registry, dir)
}

// ─── TOML fixtures ───────────────────────────────────────────────────────────

// r###"..."### because the approve_signal value contains `"##` which would prematurely
// terminate a r##"..."## raw string. Three hashes prevent the false match.

/// Core task-verify: explicitly requires_claude = false, timeout_minutes = 5.
const CORE_TASK_VERIFY: &str = r###"
name = "task-verify"
description = "Core task-verify: deterministic verify runner"
completion_handler = "builtin:task-verify"

[phase]
name = "task-verify"
level = "task"
requires_claude = false
can_add_tasks = false
can_fail_spec = false
timeout_minutes = 5

[completion]
approve_signal = "## Task Verification Approved"
reject_signal = "[TASK-VERIFY]"
on_approve = "next"
on_reject = "requeue:execute"
"###;

/// User override: NO [phase] section, but [worker].runtime = "claude".
/// Before the fix: requires_claude was re-derived to true via runtime.
/// After the fix:  requires_claude is inherited from core's explicit false.
const USER_NO_PHASE_SECTION: &str = r#"
name = "task-verify"

[worker]
runtime = "claude"
prompt_template = "Custom verify prompt."
"#;

/// User override: explicit [phase].requires_claude = true, overriding core's false.
const USER_EXPLICIT_REQUIRES_CLAUDE_TRUE: &str = r#"
name = "task-verify"

[phase]
name = "task-verify"
requires_claude = true

[worker]
runtime = "claude"
prompt_template = "Custom prompt."
"#;

/// Core phase with timeout_minutes = 30, requires_claude = false.
const CORE_MY_PHASE: &str = r#"
name = "my-phase"
description = "Core my-phase"

[phase]
name = "my-phase"
level = "task"
requires_claude = false
can_add_tasks = false
can_fail_spec = false
timeout_minutes = 30
"#;

/// User override: sets [worker].timeout only (300 s = 5 min). No [phase] section.
/// [phase] fields should all inherit from core.
const USER_WORKER_TIMEOUT_ONLY: &str = r#"
name = "my-phase"

[worker]
timeout = 300
prompt_template = "User prompt"
"#;

/// A phase that exists only in the user dir (no core counterpart).
const USER_ONLY_PHASE: &str = r#"
name = "my-custom-phase"
description = "User-defined custom phase"

[phase]
name = "my-custom-phase"
level = "task"
requires_claude = true
can_add_tasks = false
can_fail_spec = false

[prompt]
template = "Do the custom thing."
"#;

// ─── Case 1: The Bug Case ─────────────────────────────────────────────────────

/// User override with no [phase] section and [worker].runtime = "claude".
/// Before the fix: requires_claude was re-derived to `true`.
/// After the fix:  requires_claude is inherited from core's explicit `false`.
#[test]
fn test_user_override_inherits_requires_claude_false_from_core() {
    let (mut registry, core_dir) = registry_with_core("task-verify.phase.toml", CORE_TASK_VERIFY);
    let user_dir = test_dir("user-c1");

    assert!(
        !registry.get("task-verify").unwrap().requires_claude,
        "pre-condition: core has requires_claude=false"
    );

    fs::write(user_dir.join("task-verify.phase.toml"), USER_NO_PHASE_SECTION).unwrap();
    registry.load_user_phases(&user_dir);

    let phase = registry.get("task-verify").unwrap();
    assert!(
        !phase.requires_claude,
        "user override missing [phase] section must inherit requires_claude=false from core"
    );
    assert!(registry.is_user_override("task-verify"));

    let _ = fs::remove_dir_all(&core_dir);
    let _ = fs::remove_dir_all(&user_dir);
}

// ─── Case 2: Explicit User Override Wins ─────────────────────────────────────

/// Explicit [phase].requires_claude = true in user TOML overrides core's false.
#[test]
fn test_explicit_user_requires_claude_true_overrides_core_false() {
    let (mut registry, core_dir) = registry_with_core("task-verify.phase.toml", CORE_TASK_VERIFY);
    let user_dir = test_dir("user-c2");

    fs::write(user_dir.join("task-verify.phase.toml"), USER_EXPLICIT_REQUIRES_CLAUDE_TRUE).unwrap();
    registry.load_user_phases(&user_dir);

    let phase = registry.get("task-verify").unwrap();
    assert!(
        phase.requires_claude,
        "explicit [phase].requires_claude=true in user TOML must win over core's false"
    );

    let _ = fs::remove_dir_all(&core_dir);
    let _ = fs::remove_dir_all(&user_dir);
}

// ─── Case 3: Worker Field Overrides; Phase Fields Inherit ────────────────────

/// User sets [worker].timeout=300 (→ 5 min) with no [phase] section.
/// timeout_minutes uses the user's worker value; [phase] fields all inherit from core.
#[test]
fn test_user_worker_timeout_overrides_while_phase_fields_inherit() {
    let (mut registry, core_dir) = registry_with_core("my-phase.phase.toml", CORE_MY_PHASE);
    let user_dir = test_dir("user-c3");

    fs::write(user_dir.join("my-phase.phase.toml"), USER_WORKER_TIMEOUT_ONLY).unwrap();
    registry.load_user_phases(&user_dir);

    let phase = registry.get("my-phase").unwrap();

    // User's [worker].timeout = 300 s → timeout_minutes = 300 / 60 = 5
    assert_eq!(
        phase.timeout_minutes,
        Some(5),
        "user [worker].timeout=300 must produce timeout_minutes=5"
    );

    // [phase].requires_claude inherited from core (false)
    assert!(
        !phase.requires_claude,
        "requires_claude must be inherited from core (false) when user has no [phase] section"
    );

    // [phase].level inherited from core (task)
    assert_eq!(
        phase.level,
        PhaseLevel::Task,
        "level must be inherited from core"
    );

    // User's prompt_template wins
    assert_eq!(phase.prompt_template, "User prompt");

    let _ = fs::remove_dir_all(&core_dir);
    let _ = fs::remove_dir_all(&user_dir);
}

// ─── Case 4: User-Only Phase Unchanged ───────────────────────────────────────

/// A user phase with no matching core counterpart uses from_toml behavior (unchanged).
#[test]
fn test_user_only_phase_no_core_counterpart_is_unchanged() {
    let (mut registry, core_dir) = registry_with_core("task-verify.phase.toml", CORE_TASK_VERIFY);
    let user_dir = test_dir("user-c4");

    // "my-custom-phase" has no core counterpart
    fs::write(user_dir.join("my-custom-phase.phase.toml"), USER_ONLY_PHASE).unwrap();
    registry.load_user_phases(&user_dir);

    let phase = registry.get("my-custom-phase").unwrap();
    assert_eq!(phase.name, "my-custom-phase");
    assert!(phase.requires_claude);
    assert!(!phase.can_fail_spec);
    assert_eq!(phase.prompt_template, "Do the custom thing.");

    // is_user_override requires the name to exist in BOTH core and user
    assert!(
        !registry.is_user_override("my-custom-phase"),
        "user-only phase must not be flagged as a user override"
    );

    let _ = fs::remove_dir_all(&core_dir);
    let _ = fs::remove_dir_all(&user_dir);
}

// Case 5 (WARN emission) is verified by cargo test output — the WARN appears
// in captured test stderr when a user override has no [phase] section.
// A fd2/libc::dup2 approach to capture it in a file doesn't work under
// cargo's Rust-layer stderr interception, so this case is not unit-tested.
