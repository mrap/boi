use boi::queue::Queue;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

// Serializes tests that mutate env vars (HOME, BOI_PIPELINES_FILE).
static ENV_LOCK: Mutex<()> = Mutex::new(());
static COUNTER: AtomicU64 = AtomicU64::new(0);

fn pipelines_file() -> String {
    format!("{}/phases/pipelines.toml", env!("CARGO_MANIFEST_DIR"))
}

fn test_file(label: &str, ext: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "boi-guards-{}-{}-{}.{}",
        label,
        std::process::id(),
        n,
        ext
    ))
}

fn test_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "boi-guards-{}-{}-{}",
        label,
        std::process::id(),
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

/// Mock claude binary that outputs "## Critic Approved" and exits 0.
/// Sufficient for discover/execute mode: critic needs the signal, evaluate/execute
/// need nothing (no approve_signal configured).
fn mock_approving_claude(label: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = test_file(label, "sh");
    std::fs::write(&path, "#!/bin/sh\necho '## Critic Approved'\nexit 0\n")
        .expect("write mock claude script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod mock claude script");
    path
}

/// Write YAML to a temp file, open a DB, enqueue the spec.
/// Returns (queue, spec_id, db_path, spec_path).
fn setup(label: &str, yaml: &str) -> (Queue, String, String, String) {
    let spec_file = test_file(label, "yaml");
    std::fs::write(&spec_file, yaml).expect("write spec yaml");

    let db_file = test_file(label, "db");
    let _ = std::fs::remove_file(&db_file);
    let queue = Queue::open(db_file.to_str().unwrap()).expect("open queue");
    let boi_spec = boi::spec::parse(yaml).expect("parse spec yaml");
    let spec_id = queue.enqueue(&boi_spec, spec_file.to_str()).expect("enqueue");

    (
        queue,
        spec_id,
        db_file.to_str().unwrap().to_string(),
        spec_file.to_str().unwrap().to_string(),
    )
}

/// Mark all tasks for a spec as DONE in the queue, simulating tasks already completed.
fn mark_all_tasks_done(queue: &Queue, spec_id: &str) {
    let tasks = queue.get_tasks(spec_id).expect("get_tasks");
    for task in tasks {
        queue
            .update_task(spec_id, &task.id, "DONE")
            .expect("update_task DONE");
    }
}

/// Run run_worker_with_phases with the given mock claude binary.
/// Caller must hold ENV_LOCK before calling.
fn run_worker_impl(spec_id: &str, spec_path: &str, db_path: &str, claude_bin: &str) {
    let telemetry = boi::telemetry::Telemetry::new(test_file("tel", "db"));
    let runner =
        boi::runner::ClaudePhaseRunner::new(telemetry.clone(), claude_bin.to_string());
    let registry = boi::phases::PhaseRegistry::new();
    let config = boi::worker::WorkerConfig {
        task_timeout_secs: 30,
        retry_count: 0,
        cleanup_on_failure: true,
        claude_bin: claude_bin.to_string(),
        ..Default::default()
    };

    boi::worker::run_worker_with_phases(
        spec_id,
        spec_path,
        db_path,
        &boi::hooks::HookConfig::default(),
        &config,
        &registry,
        &runner,
        &telemetry,
    )
    .expect("run_worker_with_phases");
}

/// Set env vars, run f, restore env vars. Caller must hold ENV_LOCK.
fn with_env<F: FnOnce()>(fake_home: &str, f: F) {
    let old_home = std::env::var("HOME").ok();
    let old_pipelines = std::env::var("BOI_PIPELINES_FILE").ok();
    // SAFETY: ENV_LOCK is held by the caller; no concurrent env access.
    unsafe {
        std::env::set_var("HOME", fake_home);
        std::env::set_var("BOI_PIPELINES_FILE", pipelines_file());
    }
    f();
    // SAFETY: ENV_LOCK is held.
    unsafe {
        match old_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match old_pipelines {
            Some(v) => std::env::set_var("BOI_PIPELINES_FILE", v),
            None => std::env::remove_var("BOI_PIPELINES_FILE"),
        }
    }
}

// ============================================================
// PRE-REGISTRATION VALIDATION — pure spec::parse unit tests
// ============================================================

const DISCOVER_REQUIRED_YAML: &str = r#"
title: "Exp"
mode: discover
hypothesis: "H"
success_criteria: "S"
key_artifacts:
  - path: "/tmp/result.md"
tasks:
  - id: t-1
    title: "Task"
"#;

#[test]
fn test_discover_missing_hypothesis_rejected() {
    let yaml = r#"
title: "Exp"
mode: discover
success_criteria: "S"
key_artifacts:
  - path: "/tmp/result.md"
tasks:
  - id: t-1
    title: "Task"
"#;
    let err = boi::spec::parse(yaml).unwrap_err();
    assert!(
        err.to_string().contains("hypothesis"),
        "expected 'hypothesis' in error, got: {}",
        err
    );
}

#[test]
fn test_discover_missing_success_criteria_rejected() {
    let yaml = r#"
title: "Exp"
mode: discover
hypothesis: "H"
key_artifacts:
  - path: "/tmp/result.md"
tasks:
  - id: t-1
    title: "Task"
"#;
    let err = boi::spec::parse(yaml).unwrap_err();
    assert!(
        err.to_string().contains("success_criteria"),
        "expected 'success_criteria' in error, got: {}",
        err
    );
}

#[test]
fn test_discover_missing_key_artifacts_rejected() {
    let yaml = r#"
title: "Exp"
mode: discover
hypothesis: "H"
success_criteria: "S"
tasks:
  - id: t-1
    title: "Task"
"#;
    let err = boi::spec::parse(yaml).unwrap_err();
    assert!(
        err.to_string().contains("key_artifacts"),
        "expected 'key_artifacts' in error, got: {}",
        err
    );
}

#[test]
fn test_discover_missing_all_fields_error_names_them_all() {
    let yaml = r#"
title: "Exp"
mode: discover
tasks:
  - id: t-1
    title: "Task"
"#;
    let err = boi::spec::parse(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("hypothesis"), "missing 'hypothesis' in: {}", msg);
    assert!(msg.contains("success_criteria"), "missing 'success_criteria' in: {}", msg);
    assert!(msg.contains("key_artifacts"), "missing 'key_artifacts' in: {}", msg);
}

#[test]
fn test_discover_all_required_fields_accepted() {
    let spec = boi::spec::parse(DISCOVER_REQUIRED_YAML).expect("should parse discover spec");
    assert_eq!(spec.mode.as_deref(), Some("discover"));
    assert_eq!(spec.hypothesis.as_deref(), Some("H"));
    assert_eq!(spec.success_criteria.as_deref(), Some("S"));
    assert!(spec.key_artifacts.is_some());
}

#[test]
fn test_generate_all_required_fields_accepted() {
    let yaml = r#"
title: "Gen"
mode: generate
hypothesis: "We think X"
success_criteria: "File exists and has data"
key_artifacts:
  - path: "/tmp/result.md"
tasks:
  - id: t-1
    title: "Task"
"#;
    let spec = boi::spec::parse(yaml).expect("should parse generate spec");
    assert_eq!(spec.mode.as_deref(), Some("generate"));
}

#[test]
fn test_generate_missing_hypothesis_rejected() {
    let yaml = r#"
title: "Gen"
mode: generate
success_criteria: "S"
key_artifacts:
  - path: "/tmp/result.md"
tasks:
  - id: t-1
    title: "Task"
"#;
    let err = boi::spec::parse(yaml).unwrap_err();
    assert!(
        err.to_string().contains("hypothesis"),
        "expected 'hypothesis' in error, got: {}",
        err
    );
}

#[test]
fn test_execute_spec_no_experiment_fields_accepted() {
    let yaml = r#"
title: "Exec"
mode: execute
tasks:
  - id: t-1
    title: "Task"
"#;
    let spec = boi::spec::parse(yaml).expect("execute spec should parse without experiment fields");
    assert!(spec.hypothesis.is_none());
    assert!(spec.key_artifacts.is_none());
}

#[test]
fn test_execute_spec_with_experiment_fields_accepted() {
    let yaml = r#"
title: "Exec with extras"
mode: execute
hypothesis: "optional"
key_artifacts:
  - path: "/tmp/out.md"
tasks:
  - id: t-1
    title: "Task"
"#;
    let spec =
        boi::spec::parse(yaml).expect("execute spec with extra experiment fields should parse");
    assert_eq!(spec.mode.as_deref(), Some("execute"));
    assert!(spec.hypothesis.is_some());
}

// ============================================================
// ARTIFACT-GATED COMPLETION TESTS — integration
// ============================================================

fn discover_yaml_with_artifacts(artifact_paths: &[&str]) -> String {
    let artifacts: String = artifact_paths
        .iter()
        .map(|p| format!("  - path: \"{}\"", p))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"
title: "Artifact Test"
mode: discover
hypothesis: "files prove signal"
success_criteria: "all artifacts present and valid"
key_artifacts:
{artifacts}
tasks:
  - id: t-1
    title: "Create artifacts"
"#,
        artifacts = artifacts
    )
}

#[test]
fn test_all_artifacts_valid_spec_completed() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = test_dir("arti-valid-home");

    let artifact = test_file("artifact-valid", "md");
    std::fs::write(&artifact, "# Result\nsome signal content\n").expect("write artifact");

    let yaml = discover_yaml_with_artifacts(&[artifact.to_str().unwrap()]);
    let (queue, spec_id, db_path, spec_path) = setup("artifact-valid", &yaml);
    mark_all_tasks_done(&queue, &spec_id);

    let mock = mock_approving_claude("arti-valid-claude");
    with_env(fake_home.to_str().unwrap(), || {
        run_worker_impl(&spec_id, &spec_path, &db_path, mock.to_str().unwrap());
    });

    let st = queue.status(&spec_id).unwrap().unwrap();
    assert_eq!(
        st.spec.status, "completed",
        "all-valid artifacts should → completed; got: {} error: {:?}",
        st.spec.status, st.spec.error
    );
}

#[test]
fn test_artifact_missing_spec_inconclusive() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = test_dir("arti-missing-home");

    let artifact = test_file("artifact-missing", "md");
    // Intentionally NOT creating the file.

    let yaml = discover_yaml_with_artifacts(&[artifact.to_str().unwrap()]);
    let (queue, spec_id, db_path, spec_path) = setup("artifact-missing", &yaml);
    mark_all_tasks_done(&queue, &spec_id);

    let mock = mock_approving_claude("arti-missing-claude");
    with_env(fake_home.to_str().unwrap(), || {
        run_worker_impl(&spec_id, &spec_path, &db_path, mock.to_str().unwrap());
    });

    let st = queue.status(&spec_id).unwrap().unwrap();
    assert_eq!(
        st.spec.status, "inconclusive",
        "missing artifact should → inconclusive; got: {}",
        st.spec.status
    );
    let diagnosis = st.spec.error.as_deref().unwrap_or("");
    assert!(
        diagnosis.contains("not found"),
        "diagnosis should mention 'not found', got: {}",
        diagnosis
    );
}

#[test]
fn test_artifact_empty_spec_inconclusive() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = test_dir("arti-empty-home");

    let artifact = test_file("artifact-empty", "md");
    std::fs::write(&artifact, "").expect("write empty artifact");

    let yaml = discover_yaml_with_artifacts(&[artifact.to_str().unwrap()]);
    let (queue, spec_id, db_path, spec_path) = setup("artifact-empty", &yaml);
    mark_all_tasks_done(&queue, &spec_id);

    let mock = mock_approving_claude("arti-empty-claude");
    with_env(fake_home.to_str().unwrap(), || {
        run_worker_impl(&spec_id, &spec_path, &db_path, mock.to_str().unwrap());
    });

    let st = queue.status(&spec_id).unwrap().unwrap();
    assert_eq!(
        st.spec.status, "inconclusive",
        "empty artifact should → inconclusive; got: {}",
        st.spec.status
    );
    let diagnosis = st.spec.error.as_deref().unwrap_or("");
    assert!(
        diagnosis.contains("empty"),
        "diagnosis should mention 'empty', got: {}",
        diagnosis
    );
}

#[test]
fn test_artifact_validate_fails_spec_inconclusive() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = test_dir("arti-valfail-home");

    let artifact = test_file("artifact-valfail", "json");
    std::fs::write(&artifact, "{\"no_accuracy\": true}").expect("write artifact without accuracy");

    let path_str = artifact.to_str().unwrap();
    let yaml = format!(
        r#"
title: "Artifact Validate Test"
mode: discover
hypothesis: "json has accuracy key"
success_criteria: "accuracy key is present"
key_artifacts:
  - path: "{path}"
    validate: "python3 -c \"import json; d=json.load(open('{path}')); assert 'accuracy' in d\""
tasks:
  - id: t-1
    title: "Create json"
"#,
        path = path_str
    );

    let (queue, spec_id, db_path, spec_path) = setup("artifact-valfail", &yaml);
    mark_all_tasks_done(&queue, &spec_id);

    let mock = mock_approving_claude("arti-valfail-claude");
    with_env(fake_home.to_str().unwrap(), || {
        run_worker_impl(&spec_id, &spec_path, &db_path, mock.to_str().unwrap());
    });

    let st = queue.status(&spec_id).unwrap().unwrap();
    assert_eq!(
        st.spec.status, "inconclusive",
        "failed validate command should → inconclusive; got: {}",
        st.spec.status
    );
    let diagnosis = st.spec.error.as_deref().unwrap_or("");
    assert!(
        diagnosis.contains("validate command failed"),
        "diagnosis should mention 'validate command failed', got: {}",
        diagnosis
    );
}

#[test]
fn test_execute_mode_no_artifact_guard_runs() {
    // mode=execute: artifact guard only fires for discover/generate, so execute always completes.
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = test_dir("exec-no-guard-home");

    // A nonexistent artifact path — if the guard ran, this would be inconclusive.
    let nonexistent = test_file("exec-no-guard-nonexistent", "md");

    let yaml = format!(
        r#"
title: "Execute Mode Test"
mode: execute
key_artifacts:
  - path: "{}"
tasks:
  - id: t-1
    title: "Task"
"#,
        nonexistent.to_str().unwrap()
    );

    let (queue, spec_id, db_path, spec_path) = setup("exec-no-guard", &yaml);
    mark_all_tasks_done(&queue, &spec_id);

    let mock = mock_approving_claude("exec-no-guard-claude");
    with_env(fake_home.to_str().unwrap(), || {
        run_worker_impl(&spec_id, &spec_path, &db_path, mock.to_str().unwrap());
    });

    let st = queue.status(&spec_id).unwrap().unwrap();
    assert_eq!(
        st.spec.status, "completed",
        "execute mode should complete without artifact guard; got: {} error: {:?}",
        st.spec.status, st.spec.error
    );
}

#[test]
fn test_inconclusive_diagnosis_names_failed_artifact() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = test_dir("diag-home");

    let artifact1 = test_file("diag-a1", "md");
    let artifact2 = test_file("diag-a2", "md");
    std::fs::write(&artifact1, "# Result\ncontent").expect("write artifact1");
    // artifact2 intentionally missing

    let yaml = discover_yaml_with_artifacts(&[
        artifact1.to_str().unwrap(),
        artifact2.to_str().unwrap(),
    ]);
    let (queue, spec_id, db_path, spec_path) = setup("inconclusive-diag", &yaml);
    mark_all_tasks_done(&queue, &spec_id);

    let mock = mock_approving_claude("inconclusive-diag-claude");
    with_env(fake_home.to_str().unwrap(), || {
        run_worker_impl(&spec_id, &spec_path, &db_path, mock.to_str().unwrap());
    });

    let st = queue.status(&spec_id).unwrap().unwrap();
    assert_eq!(st.spec.status, "inconclusive");

    let diagnosis = st.spec.error.as_deref().unwrap_or("");
    assert!(
        diagnosis.contains("INCONCLUSIVE"),
        "diagnosis must start with INCONCLUSIVE header: {}",
        diagnosis
    );
    // Diagnosis must mention which artifact failed.
    assert!(
        diagnosis.contains(artifact2.to_str().unwrap())
            || (diagnosis.contains("not found") && !diagnosis.contains(artifact1.to_str().unwrap())),
        "diagnosis must identify artifact2 as the failed artifact: {}",
        diagnosis
    );
    // Artifact1 succeeded — its path must not appear as a failure.
    assert!(
        !diagnosis.contains(&format!("{}: file not found", artifact1.to_str().unwrap())),
        "artifact1 should not be listed as failed: {}",
        diagnosis
    );
}

// ============================================================
// PRECONDITION TESTS — integration
// ============================================================

#[test]
fn test_precondition_fails_spec_inconclusive() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = test_dir("precond-fail-home");

    let yaml = r#"
title: "Precondition Test"
mode: discover
hypothesis: "H"
success_criteria: "S"
key_artifacts:
  - path: "/tmp/some-artifact.md"
preconditions:
  - description: "Always fails precondition"
    verify: "false"
tasks:
  - id: t-1
    title: "Should not run"
"#;

    let (queue, spec_id, db_path, spec_path) = setup("precond-fail", yaml);
    // Do NOT mark tasks done — precondition check fires before tasks.

    let mock = mock_approving_claude("precond-fail-claude");
    with_env(fake_home.to_str().unwrap(), || {
        run_worker_impl(&spec_id, &spec_path, &db_path, mock.to_str().unwrap());
    });

    let st = queue.status(&spec_id).unwrap().unwrap();
    assert_eq!(
        st.spec.status, "inconclusive",
        "failed precondition should → inconclusive; got: {} error: {:?}",
        st.spec.status, st.spec.error
    );
    let diagnosis = st.spec.error.as_deref().unwrap_or("");
    assert!(
        diagnosis.contains("PRECONDITION_FAILED"),
        "diagnosis must say PRECONDITION_FAILED, got: {}",
        diagnosis
    );
    assert!(
        diagnosis.contains("Always fails precondition"),
        "diagnosis must name the failing precondition, got: {}",
        diagnosis
    );
}

#[test]
fn test_precondition_passes_spec_continues_to_complete() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = test_dir("precond-pass-home");

    let artifact = test_file("precond-pass-art", "md");
    std::fs::write(&artifact, "# Precondition pass result").expect("write artifact");

    let yaml = format!(
        r#"
title: "Precondition Pass Test"
mode: discover
hypothesis: "H"
success_criteria: "S"
key_artifacts:
  - path: "{}"
preconditions:
  - description: "Always passes"
    verify: "true"
tasks:
  - id: t-1
    title: "Task"
"#,
        artifact.to_str().unwrap()
    );

    let (queue, spec_id, db_path, spec_path) = setup("precond-pass", &yaml);
    mark_all_tasks_done(&queue, &spec_id);

    let mock = mock_approving_claude("precond-pass-claude");
    with_env(fake_home.to_str().unwrap(), || {
        run_worker_impl(&spec_id, &spec_path, &db_path, mock.to_str().unwrap());
    });

    let st = queue.status(&spec_id).unwrap().unwrap();
    assert_eq!(
        st.spec.status, "completed",
        "passing precondition should allow spec to complete; got: {} error: {:?}",
        st.spec.status, st.spec.error
    );
}

// ============================================================
// NO REGRESSION TESTS
// ============================================================

#[test]
fn test_execute_mode_completes_normally_no_regression() {
    // Verify mode=execute (no experiment fields) still dispatches and completes normally.
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = test_dir("regression-exec-home");

    let yaml = r#"
title: "Regression — execute no experiment fields"
mode: execute
tasks:
  - id: t-1
    title: "Simple task with shell verify"
    verify: "true"
"#;

    let (queue, spec_id, db_path, spec_path) = setup("regression-execute", yaml);
    // No pre-marking — let the worker run the full execute → task-verify → critic pipeline.

    let mock = mock_approving_claude("regression-exec-claude");
    with_env(fake_home.to_str().unwrap(), || {
        run_worker_impl(&spec_id, &spec_path, &db_path, mock.to_str().unwrap());
    });

    let st = queue.status(&spec_id).unwrap().unwrap();
    assert_eq!(
        st.spec.status, "completed",
        "execute mode should complete without any experiment guard interference"
    );
    assert_eq!(st.tasks[0].status, "DONE");
}

#[test]
fn test_discover_with_all_valid_artifacts_runs_all_phases() {
    // Verify ship phase (post-spec phases: critic, evaluate) still runs alongside artifact guard.
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fake_home = test_dir("ship-guard-home");

    let artifact = test_file("ship-guard-art", "md");
    std::fs::write(&artifact, "# Signal content").expect("write artifact");

    let yaml = discover_yaml_with_artifacts(&[artifact.to_str().unwrap()]);
    let (queue, spec_id, db_path, spec_path) = setup("ship-guard", &yaml);
    mark_all_tasks_done(&queue, &spec_id);

    let mock = mock_approving_claude("ship-guard-claude");
    with_env(fake_home.to_str().unwrap(), || {
        run_worker_impl(&spec_id, &spec_path, &db_path, mock.to_str().unwrap());
    });

    let st = queue.status(&spec_id).unwrap().unwrap();
    assert_eq!(
        st.spec.status, "completed",
        "discover spec with valid artifacts should complete after ship phases; got: {} error: {:?}",
        st.spec.status, st.spec.error
    );

    // Verify that post-spec phases (critic, evaluate) actually ran — not just skipped.
    let phases = queue
        .phase_cost_summary(&spec_id)
        .expect("phase_cost_summary");
    let phase_names: std::collections::HashSet<String> =
        phases.iter().map(|p| p.phase.clone()).collect();
    assert!(
        phase_names.contains("critic"),
        "critic post-spec phase should have run; got: {:?}",
        phase_names
    );
}
