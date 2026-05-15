//! Reproducer for Bug A: phase override inheritance.
//!
//! When a user override TOML has [worker].runtime="claude" but NO [phase] section,
//! requires_claude is re-derived to `true` via the runtime-based fallback, even when
//! the matching core phase explicitly declares [phase].requires_claude = false.
//!
//! RED: this test fails on the unfixed codebase.
//! GREEN: this test passes once load_user_phases inherits missing [phase] fields from core.

use boi::phases::PhaseRegistry;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_dir(label: &str) -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir()
        .join(format!("boi-inherit-{}-{}-{}", label, std::process::id(), n));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create tmp dir");
    dir
}

/// Core phase: explicitly requires_claude = false.
const CORE_TASK_VERIFY: &str = r#"
name = "t-verify"

[phase]
name = "t-verify"
level = "task"
requires_claude = false
timeout_minutes = 5
can_add_tasks = false
can_fail_spec = false
"#;

/// User override: ONLY [worker] section — no [phase] section.
/// This is the bug shape: the override changes runtime/model but has no [phase] block,
/// so the deserializer re-derives requires_claude from runtime ("claude" → true),
/// silently overriding the core's explicit false.
const USER_NO_PHASE_SECTION: &str = r#"
name = "t-verify"

[worker]
runtime = "claude"
model = "claude-sonnet-4-6"
"#;

#[test]
fn test_user_override_missing_phase_inherits_requires_claude_from_core() {
    let core_dir = tmp_dir("core");
    let user_dir = tmp_dir("user");

    fs::write(core_dir.join("t-verify.phase.toml"), CORE_TASK_VERIFY).unwrap();
    let mut registry = PhaseRegistry::from_dir(&core_dir);

    // Pre-condition: core phase has requires_claude = false.
    assert!(
        !registry.get("t-verify").unwrap().requires_claude,
        "pre-condition: core t-verify must have requires_claude=false"
    );

    fs::write(user_dir.join("t-verify.phase.toml"), USER_NO_PHASE_SECTION).unwrap();
    registry.load_user_phases(&user_dir);

    let resolved = registry.get("t-verify").unwrap();
    assert_eq!(
        resolved.requires_claude,
        false,
        "inheritance bug: user override missing [phase] should inherit \
         core's requires_claude=false, not flip to true via runtime fallback"
    );

    let _ = fs::remove_dir_all(&core_dir);
    let _ = fs::remove_dir_all(&user_dir);
}
