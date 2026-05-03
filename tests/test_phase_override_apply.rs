use boi::phases::{PhaseConfig, PhaseLevel, PhaseRegistry};
use boi::runner::apply_phase_overrides_from_map;
use boi::spec::{PhaseOverride, PhaseRuntime};
use boi::telemetry::Telemetry;
use boi::worker::effective_timeout;
use std::collections::HashMap;
use std::path::PathBuf;

fn test_db(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("boi-override-apply-{label}-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("tel.db")
}

fn make_telemetry(label: &str) -> Telemetry {
    let db = test_db(label);
    Telemetry::new(db)
}

fn make_phase(name: &str, model: Option<&str>, runtime: Option<&str>) -> PhaseConfig {
    let registry = PhaseRegistry::new();
    // Use the execute phase as a base if available, or construct one from scratch
    let mut p = if let Some(base) = registry.get("execute") {
        base.clone()
    } else {
        PhaseConfig {
            name: name.into(),
            level: PhaseLevel::Task,
            description: "test phase".into(),
            prompt_template: "Do the thing.".into(),
            timeout_minutes: Some(30),
            retry_count: None,
            can_add_tasks: false,
            can_fail_spec: false,
            requires_claude: true,
            runtime: None,
            completion_handler: None,
            approve_signal: None,
            reject_signal: None,
            on_approve: None,
            on_reject: None,
            on_crash: None,
            min_lines_changed: None,
            model: None,
            code_model: None,
            effort: None,
            hooks_pre: vec![],
            hooks_post: vec![],
        }
    };
    p.name = name.into();
    p.model = model.map(|s| s.to_string());
    p.runtime = runtime.map(|s| s.to_string());
    p.timeout_minutes = Some(30);
    p
}

// a. No override → resolves to phase default model
#[test]
fn test_no_override_returns_phase_default() {
    let tel = make_telemetry("no_override");
    let phase = make_phase("execute", Some("claude-sonnet-4-6"), Some("claude"));
    let overrides: HashMap<String, PhaseOverride> = HashMap::new();

    let effective = apply_phase_overrides_from_map(&phase, &overrides, "execute", &tel, "spec-test");

    assert_eq!(effective.model.as_deref(), Some("claude-sonnet-4-6"), "model should be unchanged");
    assert_eq!(effective.runtime.as_deref(), Some("claude"), "runtime should be unchanged");

    // No override_applied event should be emitted
    let events = tel.by_type("boi.phase.override_applied");
    assert!(events.is_empty(), "no override event expected");
}

// b. Pipeline override → resolves to override model
#[test]
fn test_pipeline_override_resolves_to_override_model() {
    let tel = make_telemetry("override_model");
    let phase = make_phase("critic", Some("claude-sonnet-4-6"), Some("claude"));

    let mut overrides: HashMap<String, PhaseOverride> = HashMap::new();
    overrides.insert("critic".to_string(), PhaseOverride {
        model: Some("claude-haiku-4-5".to_string()),
        runtime: None,
        effort: None,
        timeout: None,
    });

    let effective = apply_phase_overrides_from_map(&phase, &overrides, "critic", &tel, "spec-test");

    assert_eq!(effective.model.as_deref(), Some("claude-haiku-4-5"), "model must be overridden to haiku");
    // runtime unchanged
    assert_eq!(effective.runtime.as_deref(), Some("claude"), "runtime should be unchanged");
}

// c. Pipeline override with runtime=openrouter → runtime resolved correctly
#[test]
fn test_pipeline_override_openrouter_runtime() {
    let tel = make_telemetry("override_runtime");
    let phase = make_phase("plan-critique", Some("claude-sonnet-4-6"), Some("claude"));

    let mut overrides: HashMap<String, PhaseOverride> = HashMap::new();
    overrides.insert("plan-critique".to_string(), PhaseOverride {
        runtime: Some(PhaseRuntime::Openrouter),
        model: Some("google/gemini-2.0-flash-001".to_string()),
        effort: None,
        timeout: None,
    });

    let effective = apply_phase_overrides_from_map(&phase, &overrides, "plan-critique", &tel, "spec-test");

    assert_eq!(effective.runtime.as_deref(), Some("openrouter"), "runtime must be openrouter");
    assert_eq!(effective.model.as_deref(), Some("google/gemini-2.0-flash-001"), "model must be gemini");
}

// d. Override only model, not runtime → runtime falls back to default
#[test]
fn test_override_model_only_runtime_falls_back() {
    let tel = make_telemetry("model_only");
    let phase = make_phase("execute", Some("claude-sonnet-4-6"), Some("claude"));

    let mut overrides: HashMap<String, PhaseOverride> = HashMap::new();
    overrides.insert("execute".to_string(), PhaseOverride {
        runtime: None,
        model: Some("claude-haiku-4-5".to_string()),
        effort: None,
        timeout: None,
    });

    let effective = apply_phase_overrides_from_map(&phase, &overrides, "execute", &tel, "spec-test");

    assert_eq!(effective.model.as_deref(), Some("claude-haiku-4-5"), "model overridden");
    assert_eq!(effective.runtime.as_deref(), Some("claude"), "runtime falls back to phase default");
}

// e. Telemetry/log shows the override applied
#[test]
fn test_telemetry_emitted_when_override_applied() {
    let tel = make_telemetry("telemetry_check");
    let phase = make_phase("task-verify", Some("claude-sonnet-4-6"), Some("claude"));

    let mut overrides: HashMap<String, PhaseOverride> = HashMap::new();
    overrides.insert("task-verify".to_string(), PhaseOverride {
        runtime: None,
        model: Some("claude-haiku-4-5".to_string()),
        effort: Some("low".to_string()),
        timeout: None,
    });

    let effective = apply_phase_overrides_from_map(&phase, &overrides, "task-verify", &tel, "spec-xyz");

    assert_eq!(effective.model.as_deref(), Some("claude-haiku-4-5"));
    assert_eq!(effective.effort.as_deref(), Some("low"));

    let events = tel.by_type("boi.phase.override_applied");
    assert_eq!(events.len(), 1, "exactly one override_applied event expected");
    let ev = &events[0];
    let msg = ev.message.as_deref().unwrap_or("");
    let data = ev.data.as_deref().unwrap_or("");
    assert!(
        msg.contains("task-verify") || data.contains("task-verify"),
        "event must reference the phase name; message={msg:?} data={data:?}"
    );
}

// bonus: effective_timeout uses phase timeout_minutes when set by override
#[test]
fn test_effective_timeout_uses_phase_override() {
    let tel = make_telemetry("timeout_override");
    let phase = make_phase("execute", Some("claude-sonnet-4-6"), Some("claude"));
    // phase has timeout_minutes = Some(30) from make_phase

    let mut overrides: HashMap<String, PhaseOverride> = HashMap::new();
    overrides.insert("execute".to_string(), PhaseOverride {
        runtime: None,
        model: None,
        effort: None,
        timeout: Some(7200), // 7200 seconds = 120 minutes
    });

    let effective = apply_phase_overrides_from_map(&phase, &overrides, "execute", &tel, "spec-test");
    // 7200 / 60 = 120 minutes
    assert_eq!(effective.timeout_minutes, Some(120), "timeout_minutes must reflect 7200s override");

    let secs = effective_timeout(&effective, 1800);
    assert_eq!(secs, 7200, "effective_timeout must return 7200 when phase.timeout_minutes=120");
}

// bonus: effective_timeout falls back to global config when no timeout override
#[test]
fn test_effective_timeout_falls_back_to_global() {
    let tel = make_telemetry("timeout_fallback");
    let mut phase = make_phase("execute", Some("claude-sonnet-4-6"), Some("claude"));
    phase.timeout_minutes = None; // no per-phase timeout

    let overrides: HashMap<String, PhaseOverride> = HashMap::new();
    let effective = apply_phase_overrides_from_map(&phase, &overrides, "execute", &tel, "spec-test");

    let secs = effective_timeout(&effective, 1800);
    assert_eq!(secs, 1800, "should fall back to global config timeout");
}
