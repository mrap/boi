use boi::cli::bench::try_load_pipeline_config;
use boi::spec::PhaseRuntime;
use std::io::Write as _;

fn write_toml(dir: &std::path::Path, name: &str, content: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(content.as_bytes()).unwrap();
    path
}

fn make_temp_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "boi-phase-override-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// a. Pipeline with no override block → empty map, original behavior
#[test]
fn test_no_phase_overrides_empty_map() {
    let dir = make_temp_dir();
    let path = write_toml(
        &dir,
        "arm0.toml",
        r#"
[pipeline]
name = "arm0"
spec_phases = ["plan-critique"]
task_phases = ["execute"]
post_phases = []
"#,
    );

    let cfg = try_load_pipeline_config(&path).expect("should parse");
    assert!(cfg.phase_overrides.is_empty(), "no overrides expected");
    assert_eq!(cfg.name, "arm0");
    assert_eq!(cfg.spec_phases, vec!["plan-critique"]);
    std::fs::remove_dir_all(&dir).ok();
}

// b. Pipeline with one override → fields populated correctly
#[test]
fn test_one_phase_override_parsed() {
    let dir = make_temp_dir();
    let path = write_toml(
        &dir,
        "arm1.toml",
        r#"
[pipeline]
name = "arm1"
spec_phases = ["critic"]
task_phases = ["execute"]
post_phases = []

[phase_overrides.critic]
model = "claude-haiku-4-5"
"#,
    );

    let cfg = try_load_pipeline_config(&path).expect("should parse");
    assert_eq!(cfg.phase_overrides.len(), 1);
    let ov = cfg.phase_overrides.get("critic").expect("critic override must exist");
    assert_eq!(ov.model.as_deref(), Some("claude-haiku-4-5"));
    assert!(ov.runtime.is_none());
    assert!(ov.effort.is_none());
    assert!(ov.timeout.is_none());
    std::fs::remove_dir_all(&dir).ok();
}

// c. Pipeline with overrides for multiple phases → all parsed
#[test]
fn test_multiple_phase_overrides_all_parsed() {
    let dir = make_temp_dir();
    let path = write_toml(
        &dir,
        "arm2.toml",
        r#"
[pipeline]
name = "arm2"
spec_phases = ["plan-critique", "critic"]
task_phases = ["execute", "task-verify"]
post_phases = []

[phase_overrides.critic]
runtime = "openrouter"
model = "google/gemini-2.0-flash-001"

[phase_overrides.execute]
runtime = "claude"
model = "claude-sonnet-4-6"
effort = "high"
timeout = 3600
"#,
    );

    let cfg = try_load_pipeline_config(&path).expect("should parse");
    assert_eq!(cfg.phase_overrides.len(), 2);

    let critic = cfg.phase_overrides.get("critic").expect("critic must exist");
    assert_eq!(critic.runtime, Some(PhaseRuntime::Openrouter));
    assert_eq!(critic.model.as_deref(), Some("google/gemini-2.0-flash-001"));

    let execute = cfg.phase_overrides.get("execute").expect("execute must exist");
    assert_eq!(execute.runtime, Some(PhaseRuntime::Claude));
    assert_eq!(execute.model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(execute.effort.as_deref(), Some("high"));
    assert_eq!(execute.timeout, Some(3600));
    std::fs::remove_dir_all(&dir).ok();
}

// d. Invalid runtime value → parse error (enum rejects unknown variants)
#[test]
fn test_invalid_runtime_parse_error() {
    let dir = make_temp_dir();
    let path = write_toml(
        &dir,
        "bad.toml",
        r#"
[pipeline]
name = "bad"
spec_phases = []
task_phases = []
post_phases = []

[phase_overrides.execute]
runtime = "not-a-valid-runtime"
model = "some-model"
"#,
    );

    let result = try_load_pipeline_config(&path);
    assert!(result.is_err(), "invalid runtime should cause parse error");
    std::fs::remove_dir_all(&dir).ok();
}

// e. Backward-compatible: existing pipelines without overrides still load
#[test]
fn test_backward_compat_existing_pipeline_loads() {
    let dir = make_temp_dir();
    let path = write_toml(
        &dir,
        "v1.toml",
        r#"
[pipeline]
name = "v1"
spec_phases = ["plan-critique", "spec-critique", "critic"]
task_phases = ["execute", "task-verify"]
post_phases = []
"#,
    );

    let cfg = try_load_pipeline_config(&path).expect("existing pipeline without overrides must load");
    assert_eq!(cfg.name, "v1");
    assert_eq!(cfg.spec_phases.len(), 3);
    assert_eq!(cfg.task_phases.len(), 2);
    assert!(cfg.phase_overrides.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}
