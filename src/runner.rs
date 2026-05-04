use crate::builtins::{self, BuiltinContext};
use crate::phases::{PhaseConfig, Verdict};
use crate::runtime::{self, InvocationContext, ProviderError, ProviderRegistry};
use crate::runtime::claude::ClaudeCLIProvider;
use crate::spec::{BoiTask, PhaseOverride, PhaseRuntime};
use crate::telemetry::{
    generate_invocation_id, LogLevel, PhaseCompletionFields, PhaseInvocation, Telemetry,
};
use crate::worker;
use chrono::Utc;
use serde_json::json;
use std::time::{Duration, Instant};

/// Telemetry metrics collected during a single phase execution.
/// Populated from ClaudeResult data when available.
#[derive(Debug, Clone, Default)]
pub struct PhaseMetrics {
    pub cost_usd: Option<f64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub cold_start_ms: Option<i64>,
    pub inference_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub tool_call_count: i64,
    pub tool_calls_by_type: Option<String>,
    pub model: Option<String>,
    pub runtime: Option<String>,
    pub failure_mode: Option<String>,
    pub verify_exit_code: Option<i64>,
}

fn verdict_to_exit_status(v: &Verdict) -> String {
    match v {
        Verdict::Proceed => "success".to_string(),
        Verdict::Done { success: true, .. } => "success".to_string(),
        Verdict::Done { success: false, reason } if reason == "timeout" => "timeout".to_string(),
        Verdict::Done { success: false, .. } => "nonzero".to_string(),
        Verdict::Redo { .. } => "nonzero".to_string(),
        Verdict::Pause { .. } => "success".to_string(),
    }
}

fn classify_failure_mode(verdict: &Verdict, cr_output: &str) -> Option<String> {
    match verdict {
        Verdict::Proceed | Verdict::Pause { .. } => None,
        Verdict::Done { success: true, .. } => None,
        Verdict::Redo { .. } => Some("validation_fail".to_string()),
        Verdict::Done { success: false, reason } => {
            if reason.contains("timeout") || cr_output == "timeout" {
                Some("timeout".to_string())
            } else if reason.contains("spawn error") {
                Some("crash".to_string())
            } else if reason.contains("rate limit") || reason.contains("429") {
                Some("rate_limit".to_string())
            } else if reason.contains("context") && reason.contains("overflow") {
                Some("context_overflow".to_string())
            } else {
                Some("unknown".to_string())
            }
        }
    }
}

/// Trait for running a single phase. Allows mocking in tests.
#[allow(clippy::too_many_arguments)]
pub trait PhaseRunner: Send + Sync {
    /// Execute a phase and return the outcome.
    fn run_phase(
        &self,
        phase: &PhaseConfig,
        spec_content: &str,
        task: Option<&BoiTask>,
        worktree_path: &str,
        timeout_secs: u64,
        spec_id: Option<&str>,
        vars: &std::collections::HashMap<String, String>,
    ) -> Verdict;

    /// Execute a phase and return the verdict, raw output text, and telemetry metrics.
    /// Default delegates to `run_phase` with empty output and default metrics.
    fn run_phase_full(
        &self,
        phase: &PhaseConfig,
        spec_content: &str,
        task: Option<&BoiTask>,
        worktree_path: &str,
        timeout_secs: u64,
        spec_id: Option<&str>,
        vars: &std::collections::HashMap<String, String>,
    ) -> (Verdict, String, PhaseMetrics) {
        (self.run_phase(phase, spec_content, task, worktree_path, timeout_secs, spec_id, vars), String::new(), PhaseMetrics::default())
    }
}

/// Apply per-phase overrides from a `HashMap<phase_name, PhaseOverride>` to a `PhaseConfig`.
/// Returns a modified clone of `phase` with overrides applied. Emits telemetry when overrides fire.
pub fn apply_phase_overrides_from_map(
    phase: &PhaseConfig,
    overrides: &std::collections::HashMap<String, PhaseOverride>,
    phase_name: &str,
    telemetry: &Telemetry,
    spec_id: &str,
) -> PhaseConfig {
    let Some(ov) = overrides.get(phase_name) else {
        return phase.clone();
    };

    let mut out = phase.clone();
    let mut applied: Vec<String> = Vec::new();

    if let Some(ref rt) = ov.runtime {
        out.runtime = Some(match rt {
            PhaseRuntime::Claude => "claude".to_string(),
            PhaseRuntime::Openrouter => "openrouter".to_string(),
            PhaseRuntime::Codex => "codex".to_string(),
            PhaseRuntime::Deterministic => "deterministic".to_string(),
        });
        applied.push("runtime".to_string());
    }
    if let Some(ref m) = ov.model {
        out.model = Some(m.clone());
        applied.push("model".to_string());
    }
    if let Some(ref e) = ov.effort {
        out.effort = Some(e.clone());
        applied.push("effort".to_string());
    }
    if let Some(t) = ov.timeout {
        out.timeout_minutes = Some(t as u32);
        applied.push("timeout".to_string());
    }

    if !applied.is_empty() {
        telemetry.emit("boi.phase.override_applied", crate::telemetry::LogLevel::Info, &serde_json::json!({
            "spec_id": spec_id,
            "phase": phase_name,
            "fields": applied,
            "message": format!("phase override applied: {:?}", applied),
        }));
    }

    out
}

/// Production phase runner that dispatches phases through the provider registry.
pub struct ClaudePhaseRunner {
    pub telemetry: Telemetry,
    pub claude_bin: String,
    /// Source repo path for deterministic builtins that need to merge/cleanup.
    /// Empty string disables merge/cleanup builtins.
    pub repo_path: String,
    pub provider_registry: ProviderRegistry,
}

impl ClaudePhaseRunner {
    pub fn new(telemetry: Telemetry, claude_bin: String) -> Self {
        let mut provider_registry = ProviderRegistry::new();
        // Override the default Claude provider with the configured binary.
        provider_registry.register(Box::new(ClaudeCLIProvider::new(claude_bin.clone())));
        ClaudePhaseRunner {
            telemetry,
            claude_bin,
            repo_path: String::new(),
            provider_registry,
        }
    }

    pub fn with_repo_path(mut self, repo_path: impl Into<String>) -> Self {
        self.repo_path = repo_path.into();
        self
    }
}

impl ClaudePhaseRunner {
    /// Inner implementation that returns the verdict, raw output, and telemetry metrics.
    /// Emits `boi.phase.invoked` before branching and `boi.phase.completed` on every exit path.
    #[allow(clippy::too_many_arguments)]
    fn run_phase_inner(
        &self,
        phase: &PhaseConfig,
        spec_content: &str,
        task: Option<&BoiTask>,
        worktree_path: &str,
        timeout_secs: u64,
        spec_id: Option<&str>,
        vars: &std::collections::HashMap<String, String>,
    ) -> (Verdict, String, PhaseMetrics) {
        let start_instant = Instant::now();
        let started_at = Utc::now().to_rfc3339();
        let invocation_id = generate_invocation_id();

        // Determine the resolved runtime before any branching.
        let resolved_runtime = if phase.runtime.as_deref() == Some("deterministic") {
            "deterministic"
        } else if !phase.requires_claude {
            "verify"
        } else {
            phase.runtime.as_deref().unwrap_or("claude")
        };

        // Pre-build the prompt for Claude phases so we can include its length in the invocation
        // record. For deterministic/verify phases this stays None (no prompt is sent).
        let task_context = task.map(|t| {
            format!(
                "Task: {} — {}\nSpec: {}\nVerify: {}",
                t.id,
                t.title,
                t.spec.as_deref().unwrap_or("(none)"),
                t.verify.as_deref().unwrap_or("(none)")
            )
        });
        let prompt_opt: Option<String> =
            if resolved_runtime != "deterministic" && resolved_runtime != "verify" {
                Some(crate::phases::build_phase_prompt(
                    phase,
                    spec_content,
                    task_context.as_deref(),
                    vars,
                ))
            } else {
                None
            };

        // Which API key env var was actually read for auth?
        let api_key_env_used = match resolved_runtime {
            "claude" => ["ANTHROPIC_API_KEY", "CLAUDE_API_KEY"]
                .iter()
                .find(|v| std::env::var(v).is_ok())
                .map(|v| v.to_string()),
            "openrouter" => std::env::var("OPENROUTER_API_KEY")
                .ok()
                .map(|_| "OPENROUTER_API_KEY".to_string()),
            _ => None,
        };

        // Full CLI args vector (omitting the prompt itself — too large for telemetry).
        let cli_args: Option<Vec<String>> = if resolved_runtime == "claude" {
            let mut args = vec![
                "--dangerously-skip-permissions".to_string(),
                "--no-session-persistence".to_string(),
                "--setting-sources".to_string(),
                "user".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--verbose".to_string(),
            ];
            if let Some(m) = &phase.model {
                args.push("--model".to_string());
                args.push(m.clone());
            }
            Some(args)
        } else {
            None
        };

        // Git HEAD at start (best-effort).
        let branch_sha = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(worktree_path)
            .output()
            .ok()
            .and_then(|o| if o.status.success() { String::from_utf8(o.stdout).ok() } else { None })
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        // ── emit boi.phase.invoked ────────────────────────────────────────────
        let inv = PhaseInvocation {
            invocation_id: invocation_id.clone(),
            spec_id: spec_id.map(String::from),
            task_id: task.map(|t| t.id.clone()),
            phase_name: phase.name.clone(),
            phase_level: format!("{:?}", phase.level).to_lowercase(),
            mode: None, // not available at this call site
            runtime: Some(resolved_runtime.to_string()),
            model: phase.model.clone(),
            effort: phase.effort.clone(),
            thinking_enabled: None,
            thinking_budget_tokens: None,
            extended_thinking: None,
            prompt_template_path: None,
            prompt_length_chars: prompt_opt.as_ref().map(|p| p.len() as i64),
            prompt_length_tokens: prompt_opt.as_ref().map(|p| p.len() as i64 / 4),
            timeout_secs: timeout_secs as i64,
            bare_flag: false,
            brain_dir: None,
            api_key_env_used,
            cli_args,
            http_endpoint: None,
            started_at,
            branch_sha,
            host_os: Some(std::env::consts::OS.to_string()),
            host_arch: Some(std::env::consts::ARCH.to_string()),
            daemon_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        };
        self.telemetry.emit_phase_invoked(&inv);

        // Helper: emit boi.phase.completed with timing + outcome + usage.
        let emit_done = |start: &Instant,
                         exit_status: &str,
                         exit_reason: Option<String>,
                         startup_ms: Option<i64>,
                         inference_ms: Option<i64>| {
            self.telemetry.emit_phase_completed(
                &invocation_id,
                &PhaseCompletionFields {
                    completed_at: Utc::now().to_rfc3339(),
                    duration_ms: start.elapsed().as_millis() as i64,
                    startup_ms,
                    inference_ms,
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_tokens: None,
                    cache_creation_tokens: None,
                    cost_usd: None,
                    exit_status: exit_status.to_string(),
                    exit_reason,
                },
            );
        };

        // ── branch to runtime ─────────────────────────────────────────────────

        // Deterministic phases: skip LLM entirely, run a registered builtin handler.
        if phase.runtime.as_deref() == Some("deterministic") {
            let verdict = self.run_deterministic_phase(phase, task, spec_id);
            let exit_status = verdict_to_exit_status(&verdict);
            emit_done(&start_instant, &exit_status, None, None, None);
            let metrics = PhaseMetrics {
                runtime: Some("deterministic".to_string()),
                ..Default::default()
            };
            return (verdict, String::new(), metrics);
        }

        if !phase.requires_claude {
            let (verdict, verify_exit_code) = self.run_verify_phase(phase, task, worktree_path, timeout_secs, spec_id);
            let exit_status = verdict_to_exit_status(&verdict);
            emit_done(&start_instant, &exit_status, None, None, None);
            let metrics = PhaseMetrics {
                runtime: Some("verify".to_string()),
                verify_exit_code,
                ..Default::default()
            };
            return (verdict, String::new(), metrics);
        }

        // LLM phase — dispatch through the provider registry.
        let task_id = task.map(|t| t.id.as_str());
        let spec_id_hint = spec_id.unwrap_or("");
        let prompt = prompt_opt.unwrap_or_else(|| {
            crate::phases::build_phase_prompt(phase, spec_content, task_context.as_deref(), vars)
        });

        let provider_name = phase.runtime.as_deref().unwrap_or("claude");

        self.telemetry.emit(
            "boi.claude.spawn",
            LogLevel::Debug,
            &json!({
                "spec_id": spec_id_hint,
                "task_id": task_id,
                "phase": phase.name,
                "provider": provider_name,
                "prompt_length": prompt.len(),
                "message": format!("spawning {}...", provider_name),
            }),
        );

        // Validation point 3: pre-invocation check.
        let provider = match self.provider_registry.get(provider_name) {
            Some(p) => p,
            None => {
                let error_msg = format!(
                    "provider '{}' not configured: not registered or disabled",
                    provider_name
                );
                self.telemetry.emit(
                    "boi.claude.error",
                    LogLevel::Error,
                    &json!({
                        "spec_id": spec_id_hint,
                        "task_id": task_id,
                        "phase": phase.name,
                        "provider": provider_name,
                        "message": &error_msg,
                    }),
                );
                emit_done(&start_instant, "crashed", Some(error_msg.clone()), None, None);
                return (
                    Verdict::Done { success: false, reason: format!("Phase {} {}", phase.name, error_msg) },
                    String::new(),
                    PhaseMetrics {
                        runtime: Some(provider_name.to_string()),
                        model: phase.model.clone(),
                        failure_mode: Some("crash".to_string()),
                        ..Default::default()
                    },
                );
            }
        };

        if let Err(e) = provider.validate_config(phase) {
            let error_msg = format!("provider '{}' validation failed: {}", provider_name, e);
            self.telemetry.emit(
                "boi.claude.error",
                LogLevel::Error,
                &json!({
                    "spec_id": spec_id_hint,
                    "task_id": task_id,
                    "phase": phase.name,
                    "provider": provider_name,
                    "message": &error_msg,
                }),
            );
            emit_done(&start_instant, "crashed", Some(error_msg.clone()), None, None);
            return (
                Verdict::Done { success: false, reason: format!("Phase {} {}", phase.name, error_msg) },
                String::new(),
                PhaseMetrics {
                    runtime: Some(provider_name.to_string()),
                    model: phase.model.clone(),
                    failure_mode: Some("crash".to_string()),
                    ..Default::default()
                },
            );
        }

        let ctx = InvocationContext {
            phase,
            prompt: &prompt,
            model: phase.model.as_deref().unwrap_or(""),
            timeout: Duration::from_secs(timeout_secs),
            spec_id,
            task_id: task.map(|t| t.id.as_str()),
            worktree_path,
        };

        let build_metrics_from_output =
            |ro: &runtime::RuntimeOutput, verdict: &Verdict| -> PhaseMetrics {
                PhaseMetrics {
                    cost_usd: ro.cost_usd,
                    input_tokens: ro.input_tokens,
                    output_tokens: ro.output_tokens,
                    cache_read_tokens: ro.cache_read_tokens,
                    cache_creation_tokens: ro.cache_creation_tokens,
                    cold_start_ms: Some(ro.startup_ms as i64),
                    inference_ms: Some(ro.inference_ms as i64),
                    ttft_ms: Some(ro.startup_ms as i64),
                    tool_call_count: ro.tool_call_count,
                    tool_calls_by_type: None,
                    model: phase.model.clone(),
                    runtime: Some(provider_name.to_string()),
                    failure_mode: classify_failure_mode(verdict, &ro.output),
                    verify_exit_code: None,
                }
            };

        let emit_done_with_output =
            |start: &Instant, exit_status: &str, ro: &runtime::RuntimeOutput| {
                self.telemetry.emit_phase_completed(
                    &invocation_id,
                    &PhaseCompletionFields {
                        completed_at: Utc::now().to_rfc3339(),
                        duration_ms: start.elapsed().as_millis() as i64,
                        startup_ms: Some(ro.startup_ms as i64),
                        inference_ms: Some(ro.inference_ms as i64),
                        input_tokens: ro.input_tokens,
                        output_tokens: ro.output_tokens,
                        cache_read_tokens: ro.cache_read_tokens,
                        cache_creation_tokens: ro.cache_creation_tokens,
                        cost_usd: ro.cost_usd,
                        exit_status: exit_status.to_string(),
                        exit_reason: None,
                    },
                );
            };

        match provider.invoke(ctx) {
            Ok(ro) if ro.success => {
                let inference_s = ro.inference_ms as f64 / 1000.0;
                let total_s = ro.total_ms as f64 / 1000.0;
                self.telemetry.emit(
                    "boi.claude.exit",
                    LogLevel::Debug,
                    &json!({
                        "spec_id": spec_id_hint,
                        "task_id": task_id,
                        "phase": phase.name,
                        "provider": provider_name,
                        "exit_code": 0,
                        "output_length": ro.output.len(),
                        "startup_ms": ro.startup_ms,
                        "inference_ms": ro.inference_ms,
                        "total_ms": ro.total_ms,
                        "cost_usd": ro.cost_usd,
                        "input_tokens": ro.input_tokens,
                        "output_tokens": ro.output_tokens,
                        "tool_call_count": ro.tool_call_count,
                        "message": format!("{} exit 0, {} chars ({:.1}s inference, {:.1}s total)",
                            provider_name, ro.output.len(), inference_s, total_s),
                    }),
                );
                let verdict = crate::phases::parse_phase_output(phase, &ro.output);
                emit_done_with_output(&start_instant, "success", &ro);
                let metrics = build_metrics_from_output(&ro, &verdict);
                (verdict, ro.output, metrics)
            }
            Ok(ro) => {
                let inference_s = ro.inference_ms as f64 / 1000.0;
                let total_s = ro.total_ms as f64 / 1000.0;
                self.telemetry.emit(
                    "boi.claude.exit",
                    LogLevel::Error,
                    &json!({
                        "spec_id": spec_id_hint,
                        "task_id": task_id,
                        "phase": phase.name,
                        "provider": provider_name,
                        "exit_code": 1,
                        "output_length": ro.output.len(),
                        "startup_ms": ro.startup_ms,
                        "inference_ms": ro.inference_ms,
                        "total_ms": ro.total_ms,
                        "cost_usd": ro.cost_usd,
                        "input_tokens": ro.input_tokens,
                        "output_tokens": ro.output_tokens,
                        "tool_call_count": ro.tool_call_count,
                        "message": format!("{} exit non-zero, {} chars ({:.1}s inference, {:.1}s total)",
                            provider_name, ro.output.len(), inference_s, total_s),
                    }),
                );
                let verdict = if phase.on_crash.as_deref() == Some("retry") {
                    Verdict::Done {
                        success: false,
                        reason: format!("Phase {} {} exited non-zero", phase.name, provider_name),
                    }
                } else {
                    Verdict::Done {
                        success: false,
                        reason: format!("Phase {} failed: {}", phase.name, ro.output),
                    }
                };
                emit_done_with_output(&start_instant, "nonzero", &ro);
                let metrics = build_metrics_from_output(&ro, &verdict);
                (verdict, ro.output, metrics)
            }
            Err(ProviderError::Timeout { secs }) => {
                let error_msg = format!("timeout after {}s", secs);
                self.telemetry.emit(
                    "boi.claude.error",
                    LogLevel::Error,
                    &json!({
                        "spec_id": spec_id_hint,
                        "task_id": task_id,
                        "phase": phase.name,
                        "provider": provider_name,
                        "message": &error_msg,
                    }),
                );
                emit_done(&start_instant, "timeout", Some(error_msg), None, None);
                let verdict = Verdict::Done { success: false, reason: "timeout".into() };
                let metrics = PhaseMetrics {
                    runtime: Some(provider_name.to_string()),
                    model: phase.model.clone(),
                    failure_mode: Some("timeout".to_string()),
                    ..Default::default()
                };
                (verdict, String::new(), metrics)
            }
            Err(e) => {
                let error_msg = e.to_string();
                self.telemetry.emit(
                    "boi.claude.error",
                    LogLevel::Error,
                    &json!({
                        "spec_id": spec_id_hint,
                        "task_id": task_id,
                        "phase": phase.name,
                        "provider": provider_name,
                        "message": format!("provider invoke error: {}", error_msg),
                    }),
                );
                emit_done(&start_instant, "crashed", Some(error_msg.clone()), None, None);
                let verdict = Verdict::Done {
                    success: false,
                    reason: format!("Phase {} spawn error: {}", phase.name, error_msg),
                };
                let metrics = PhaseMetrics {
                    runtime: Some(provider_name.to_string()),
                    model: phase.model.clone(),
                    failure_mode: Some("crash".to_string()),
                    ..Default::default()
                };
                (verdict, String::new(), metrics)
            }
        }
    }
}

impl PhaseRunner for ClaudePhaseRunner {
    fn run_phase(
        &self,
        phase: &PhaseConfig,
        spec_content: &str,
        task: Option<&BoiTask>,
        worktree_path: &str,
        timeout_secs: u64,
        spec_id: Option<&str>,
        vars: &std::collections::HashMap<String, String>,
    ) -> Verdict {
        self.run_phase_inner(phase, spec_content, task, worktree_path, timeout_secs, spec_id, vars).0
    }

    fn run_phase_full(
        &self,
        phase: &PhaseConfig,
        spec_content: &str,
        task: Option<&BoiTask>,
        worktree_path: &str,
        timeout_secs: u64,
        spec_id: Option<&str>,
        vars: &std::collections::HashMap<String, String>,
    ) -> (Verdict, String, PhaseMetrics) {
        self.run_phase_inner(phase, spec_content, task, worktree_path, timeout_secs, spec_id, vars)
    }
}

impl ClaudePhaseRunner {
    fn run_deterministic_phase(
        &self,
        phase: &PhaseConfig,
        task: Option<&BoiTask>,
        spec_id: Option<&str>,
    ) -> Verdict {
        let handler = match phase.completion_handler.as_deref() {
            Some(h) => h,
            None => {
                self.telemetry.emit(
                    "boi.builtin.error",
                    LogLevel::Error,
                    &json!({
                        "phase": phase.name,
                        "message": "deterministic phase has no completion_handler",
                    }),
                );
                return Verdict::Done {
                    success: false,
                    reason: format!("phase '{}' is deterministic but has no completion_handler", phase.name),
                };
            }
        };

        let sid = spec_id.unwrap_or("");
        let task_title = task.map(|t| t.title.as_str()).unwrap_or("");

        let ctx = BuiltinContext {
            spec_id: sid,
            task_title,
            repo_path: &self.repo_path,
        };

        self.telemetry.emit(
            "boi.builtin.run",
            LogLevel::Debug,
            &json!({
                "phase": phase.name,
                "handler": handler,
                "spec_id": sid,
                "message": format!("running builtin {}", handler),
            }),
        );

        let result = builtins::run_builtin(handler, &ctx);

        self.telemetry.emit(
            "boi.builtin.result",
            LogLevel::Debug,
            &json!({
                "phase": phase.name,
                "handler": handler,
                "spec_id": sid,
                "result": format!("{:?}", result),
            }),
        );

        result.to_verdict()
    }

    /// Returns (Verdict, verify_exit_code).
    fn run_verify_phase(
        &self,
        _phase: &PhaseConfig,
        task: Option<&BoiTask>,
        worktree_path: &str,
        timeout_secs: u64,
        spec_id: Option<&str>,
    ) -> (Verdict, Option<i64>) {
        let task = match task {
            Some(t) => t,
            None => return (Verdict::Proceed, None),
        };

        let has_verify = task.verify.as_deref().is_some_and(|c| !c.is_empty());
        let has_verify_prompt = task.verify_prompt.as_deref().is_some_and(|p| !p.is_empty());

        if !has_verify && !has_verify_prompt {
            return (Verdict::Proceed, None);
        }

        // Shell verify
        if has_verify {
            let verify_cmd = task
                .verify
                .as_deref()
                .expect("has_verify guard ensures verify is Some");

            self.telemetry.emit(
                "boi.verify.run",
                LogLevel::Debug,
                &json!({
                    "task_id": task.id,
                    "verify_cmd": verify_cmd,
                    "message": format!("cmd: {}", verify_cmd),
                }),
            );

            let start = Instant::now();
            let (passed, exit_code) = worker::run_verify_with_code(verify_cmd, worktree_path);
            let duration_ms = start.elapsed().as_millis() as u64;

            self.telemetry.emit("boi.verify.result", LogLevel::Debug, &json!({
                "task_id": task.id,
                "verify_cmd": verify_cmd,
                "passed": passed,
                "exit_code": exit_code,
                "duration_ms": duration_ms,
                "message": format!("exit {} ({}ms)", exit_code.map(|c| c.to_string()).unwrap_or_else(|| "?".to_string()), duration_ms),
            }));

            if !passed {
                return (Verdict::Redo { tasks: vec![] }, exit_code);
            }
        }

        // Claude verify_prompt (only reached if shell verify passed or wasn't set)
        if has_verify_prompt {
            let verify_prompt = task
                .verify_prompt
                .as_deref()
                .expect("has_verify_prompt guard ensures verify_prompt is Some");

            self.telemetry.emit("boi.verify_prompt.run", LogLevel::Debug, &json!({
                "task_id": task.id,
                "verify_prompt_length": verify_prompt.len(),
                "message": format!("verify_prompt: spawning claude ({} chars)", verify_prompt.len()),
            }));

            let result = worker::spawn_claude(
                verify_prompt,
                worktree_path,
                timeout_secs,
                None,
                spec_id,
                &self.claude_bin,
            );

            match result {
                Ok(ref cr) if cr.success => {
                    self.telemetry.emit(
                        "boi.verify_prompt.result",
                        LogLevel::Debug,
                        &json!({
                            "task_id": task.id,
                            "passed": true,
                            "output_length": cr.output.len(),
                            "startup_ms": cr.startup_ms,
                            "inference_ms": cr.inference_ms,
                            "total_ms": cr.total_ms,
                            "message": format!("verify_prompt passed ({}ms)", cr.total_ms),
                        }),
                    );
                }
                Ok(ref cr) => {
                    self.telemetry.emit(
                        "boi.verify_prompt.result",
                        LogLevel::Debug,
                        &json!({
                            "task_id": task.id,
                            "passed": false,
                            "output_length": cr.output.len(),
                            "startup_ms": cr.startup_ms,
                            "inference_ms": cr.inference_ms,
                            "total_ms": cr.total_ms,
                            "message": format!("verify_prompt failed ({}ms)", cr.total_ms),
                        }),
                    );
                    return (Verdict::Redo { tasks: vec![] }, None);
                }
                Err(e) => {
                    self.telemetry.emit(
                        "boi.verify_prompt.error",
                        LogLevel::Error,
                        &json!({
                            "task_id": task.id,
                            "message": format!("verify_prompt spawn error: {}", e),
                        }),
                    );
                    return (Verdict::Redo { tasks: vec![] }, None);
                }
            }
        }

        (Verdict::Proceed, Some(0))
    }
}

/// Mock phase runner for testing — returns configurable verdicts.
#[cfg(test)]
pub struct MockPhaseRunner {
    /// Verdicts to return, indexed by call order.
    pub verdicts: std::sync::Mutex<Vec<Verdict>>,
}

#[cfg(test)]
impl MockPhaseRunner {
    pub fn new(verdicts: Vec<Verdict>) -> Self {
        MockPhaseRunner {
            verdicts: std::sync::Mutex::new(verdicts),
        }
    }
}

#[cfg(test)]
impl PhaseRunner for MockPhaseRunner {
    fn run_phase(
        &self,
        _phase: &PhaseConfig,
        _spec_content: &str,
        _task: Option<&BoiTask>,
        _worktree_path: &str,
        _timeout_secs: u64,
        _spec_id: Option<&str>,
        _vars: &std::collections::HashMap<String, String>,
    ) -> Verdict {
        let mut verdicts = self.verdicts.lock().unwrap();
        if verdicts.is_empty() {
            Verdict::Proceed
        } else {
            verdicts.remove(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phases::{PhaseConfig, PhaseLevel, Verdict};
    use crate::spec::{BoiTask, TaskStatus};
    use crate::test_utils;

    fn test_telemetry() -> Telemetry {
        let db = test_utils::test_file("runner-tel", "db");
        let _ = std::fs::remove_file(&db);
        Telemetry::new(db)
    }

    fn make_phase(name: &str, requires_claude: bool) -> PhaseConfig {
        PhaseConfig {
            name: name.into(),
            level: PhaseLevel::Task,
            description: "test".into(),
            prompt_template: "Do the thing.".into(),
            timeout_minutes: Some(5),
            retry_count: None,
            can_add_tasks: false,
            can_fail_spec: false,
            requires_claude,
            runtime: None,
            completion_handler: None,
            approve_signal: Some("## Approved".into()),
            reject_signal: Some("[REJECT]".into()),
            on_approve: Some("next".into()),
            on_reject: Some("requeue:execute".into()),
            on_crash: None,
            min_lines_changed: None,
            model: None,
            code_model: None,
            effort: None,
            hooks_pre: vec![],
            hooks_post: vec![],
        }
    }

    fn make_task_with_verify(verify: &str) -> BoiTask {
        BoiTask {
            id: "t-1".into(),
            title: "Test task".into(),
            status: TaskStatus::Pending,
            depends: None,
            spec: Some("Do something".into()),
            verify: Some(verify.into()),
            verify_prompt: None,
            phases: None,
        }
    }

    #[test]
    fn test_claude_runner_verify_phase_success() {
        let runner = ClaudePhaseRunner::new(test_telemetry(), "claude".to_string());
        let phase = make_phase("task-verify", false);
        let task = make_task_with_verify("true");
        let outcome = runner.run_phase(
            &phase,
            "",
            Some(&task),
            "/tmp",
            10,
            None,
            &std::collections::HashMap::new(),
        );
        assert_eq!(outcome, Verdict::Proceed);
    }

    #[test]
    fn test_claude_runner_verify_phase_failure() {
        let runner = ClaudePhaseRunner::new(test_telemetry(), "claude".to_string());
        let phase = make_phase("task-verify", false);
        let task = make_task_with_verify("false");
        let outcome = runner.run_phase(
            &phase,
            "",
            Some(&task),
            "/tmp",
            10,
            None,
            &std::collections::HashMap::new(),
        );
        assert_eq!(outcome, Verdict::Redo { tasks: vec![] });
    }

    #[test]
    fn test_claude_runner_verify_phase_no_verify_cmd() {
        let runner = ClaudePhaseRunner::new(test_telemetry(), "claude".to_string());
        let phase = make_phase("task-verify", false);
        let task = BoiTask {
            id: "t-1".into(),
            title: "No verify".into(),
            status: TaskStatus::Pending,
            depends: None,
            spec: None,
            verify: None,
            verify_prompt: None,
            phases: None,
        };
        let outcome = runner.run_phase(
            &phase,
            "",
            Some(&task),
            "/tmp",
            10,
            None,
            &std::collections::HashMap::new(),
        );
        assert_eq!(outcome, Verdict::Proceed);
    }

    #[test]
    fn test_claude_runner_spec_level_no_claude_proceeds() {
        let runner = ClaudePhaseRunner::new(test_telemetry(), "claude".to_string());
        let phase = make_phase("no-op", false);
        // Spec-level phase with no task → proceed (skip is just proceed)
        let outcome = runner.run_phase(
            &phase,
            "",
            None,
            "/tmp",
            10,
            None,
            &std::collections::HashMap::new(),
        );
        assert_eq!(outcome, Verdict::Proceed);
    }

    #[test]
    fn test_mock_runner_returns_configured_verdicts() {
        let runner = MockPhaseRunner::new(vec![
            Verdict::Proceed,
            Verdict::Done {
                success: false,
                reason: "timeout".into(),
            },
            Verdict::Done {
                success: false,
                reason: "bad".into(),
            },
        ]);
        let phase = make_phase("test", true);

        assert_eq!(
            runner.run_phase(
                &phase,
                "",
                None,
                "/tmp",
                10,
                None,
                &std::collections::HashMap::new()
            ),
            Verdict::Proceed
        );
        assert_eq!(
            runner.run_phase(
                &phase,
                "",
                None,
                "/tmp",
                10,
                None,
                &std::collections::HashMap::new()
            ),
            Verdict::Done {
                success: false,
                reason: "timeout".into()
            }
        );
        assert_eq!(
            runner.run_phase(
                &phase,
                "",
                None,
                "/tmp",
                10,
                None,
                &std::collections::HashMap::new()
            ),
            Verdict::Done {
                success: false,
                reason: "bad".into()
            }
        );
        // Exhausted verdicts → default to Proceed
        assert_eq!(
            runner.run_phase(
                &phase,
                "",
                None,
                "/tmp",
                10,
                None,
                &std::collections::HashMap::new()
            ),
            Verdict::Proceed
        );
    }

    #[test]
    fn test_classify_failure_mode_timeout() {
        let v = Verdict::Done { success: false, reason: "timeout".into() };
        assert_eq!(classify_failure_mode(&v, "timeout"), Some("timeout".to_string()));
    }

    #[test]
    fn test_classify_failure_mode_crash() {
        let v = Verdict::Done { success: false, reason: "Phase X spawn error: os error".into() };
        assert_eq!(classify_failure_mode(&v, ""), Some("crash".to_string()));
    }

    #[test]
    fn test_classify_failure_mode_success_is_none() {
        let v = Verdict::Proceed;
        assert_eq!(classify_failure_mode(&v, ""), None);

        let v2 = Verdict::Done { success: true, reason: "done".into() };
        assert_eq!(classify_failure_mode(&v2, ""), None);
    }

    #[test]
    fn test_classify_failure_mode_redo_is_validation_fail() {
        let v = Verdict::Redo { tasks: vec![] };
        assert_eq!(classify_failure_mode(&v, ""), Some("validation_fail".to_string()));
    }

    #[test]
    fn test_phase_metrics_default() {
        let m = PhaseMetrics::default();
        assert_eq!(m.tool_call_count, 0);
        assert!(m.cost_usd.is_none());
        assert!(m.model.is_none());
    }

    #[test]
    fn test_verify_exit_code_populated_in_metrics_on_success() {
        let runner = ClaudePhaseRunner::new(test_telemetry(), "claude".to_string());
        let phase = make_phase("task-verify", false);
        let task = make_task_with_verify("true");
        let (verdict, _, metrics) = runner.run_phase_full(
            &phase, "", Some(&task), "/tmp", 10, None,
            &std::collections::HashMap::new(),
        );
        assert_eq!(verdict, Verdict::Proceed);
        assert_eq!(metrics.verify_exit_code, Some(0), "exit code 0 must be recorded for passing verify");
        assert_eq!(metrics.runtime.as_deref(), Some("verify"), "runtime must be 'verify' for non-claude phase");
    }

    #[test]
    fn test_verify_exit_code_populated_in_metrics_on_failure() {
        let runner = ClaudePhaseRunner::new(test_telemetry(), "claude".to_string());
        let phase = make_phase("task-verify", false);
        let task = make_task_with_verify("false");
        let (verdict, _, metrics) = runner.run_phase_full(
            &phase, "", Some(&task), "/tmp", 10, None,
            &std::collections::HashMap::new(),
        );
        assert_eq!(verdict, Verdict::Redo { tasks: vec![] });
        assert_eq!(metrics.verify_exit_code, Some(1), "exit code 1 must be recorded for failing verify");
    }

    #[test]
    fn test_verify_exit_code_none_when_no_verify_cmd() {
        let runner = ClaudePhaseRunner::new(test_telemetry(), "claude".to_string());
        let phase = make_phase("task-verify", false);
        let task = BoiTask {
            id: "t-1".into(), title: "No verify".into(), status: TaskStatus::Pending,
            depends: None, spec: None, verify: None, verify_prompt: None, phases: None,
        };
        let (verdict, _, metrics) = runner.run_phase_full(
            &phase, "", Some(&task), "/tmp", 10, None,
            &std::collections::HashMap::new(),
        );
        assert_eq!(verdict, Verdict::Proceed);
        assert_eq!(metrics.verify_exit_code, None, "no verify cmd → exit code must be None");
    }
}
