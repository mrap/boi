use boi::phases::PhaseRegistry;
use boi::worker::phase_override_warning_message;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_dir(label: &str) -> std::path::PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir()
        .join(format!("boi-warn-test-{}-{}-{}", label, std::process::id(), n));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create tmp dir");
    dir
}

const FAKE_USER_PHASE: &str = r#"
name = "execute"

[phase]
level = "task"
can_add_tasks = false
can_fail_spec = false
requires_claude = true

[worker]
runtime = "claude"
model = "claude-sonnet-4-6"
"#;

#[test]
fn test_phase_override_startup_warning() {
    let core_dir = tmp_dir("core");
    let user_dir = tmp_dir("user");

    // Write a fake user override for the "execute" phase
    fs::write(user_dir.join("execute.phase.toml"), FAKE_USER_PHASE)
        .expect("write user phase file");

    let mut registry = PhaseRegistry::from_dir(&core_dir);
    registry.load_user_phases(&user_dir);

    let msg = phase_override_warning_message(&registry);
    assert!(msg.is_some(), "expected a warning message when user overrides are active");

    let msg = msg.unwrap();
    assert!(
        msg.contains("[WARN] Phase overrides active:"),
        "message should contain '[WARN] Phase overrides active:', got: {msg}"
    );
    assert!(
        msg.contains("execute"),
        "message should list the overridden phase name, got: {msg}"
    );

    // No warning when no user overrides
    let empty_dir = tmp_dir("empty");
    let mut clean_registry = PhaseRegistry::from_dir(&core_dir);
    clean_registry.load_user_phases(&empty_dir);
    assert!(
        phase_override_warning_message(&clean_registry).is_none(),
        "expected no warning when no user overrides are active"
    );

    let _ = fs::remove_dir_all(&core_dir);
    let _ = fs::remove_dir_all(&user_dir);
    let _ = fs::remove_dir_all(&empty_dir);
}
