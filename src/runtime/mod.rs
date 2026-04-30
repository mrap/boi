pub mod claude;
pub mod codex;
pub mod openrouter;

use crate::phases::PhaseConfig;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::time::Duration;

// ─── backward-compat types (used by codex.rs and existing call sites) ────────

/// Configuration for a spec provider invocation.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub model: Option<String>,
    pub timeout_secs: u64,
    /// Path to the provider binary. None → resolve via PATH.
    pub bin: Option<String>,
    /// API key override. None → read from environment.
    /// An empty string signals "no key" (useful for testing).
    pub api_key: Option<String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        ProviderConfig {
            model: None,
            timeout_secs: 300,
            bin: None,
            api_key: None,
        }
    }
}

/// Output from a successful provider execution (legacy — used by SpecProvider).
#[derive(Debug)]
pub struct ProviderOutput {
    pub output: String,
    pub success: bool,
    pub startup_ms: u64,
    pub inference_ms: u64,
    pub total_ms: u64,
}

/// Unified error enum for all provider invocations.
/// Each provider impl maps its native errors to this type at the boundary.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider {provider} not configured: {reason}")]
    NotConfigured { provider: String, reason: String },

    #[error("auth failed for {provider}: env var {env_var} missing or invalid")]
    AuthFailed { provider: String, env_var: String },

    #[error("rate limit on {provider}; retry after {retry_after_s:?}s")]
    RateLimit { provider: String, retry_after_s: Option<u32> },

    #[error("timeout after {secs}s")]
    Timeout { secs: u64 },

    #[error("bad response from {provider}: {body_excerpt}")]
    BadResponse { provider: String, body_excerpt: String },

    #[error("network error: {0}")]
    NetworkError(#[source] anyhow::Error),

    #[error("provider {provider} missing capability: {required}")]
    CapabilityMissing { provider: String, required: &'static str },

    #[error("budget exceeded for {provider} ({period})")]
    BudgetExceeded { provider: String, period: String },

    #[error("{0}")]
    Other(#[source] anyhow::Error),
}

/// Legacy trait for spec execution providers (kept for codex.rs backward compat).
pub trait SpecProvider: Send + Sync {
    fn execute(&self, prompt: &str, config: &ProviderConfig) -> Result<ProviderOutput, ProviderError>;
}

// ─── New unified Provider architecture ───────────────────────────────────────

/// Declares what a provider is capable of.
#[derive(Debug, Clone)]
pub struct Capabilities {
    pub tool_use: bool,
    pub streaming: bool,
    pub vision: bool,
    pub thinking: bool,
    pub max_tokens_in: u32,
    pub max_tokens_out: u32,
}

/// All context needed to invoke a provider for a single phase execution.
pub struct InvocationContext<'a> {
    pub phase: &'a PhaseConfig,
    pub prompt: &'a str,
    pub model: &'a str,
    pub timeout: Duration,
    pub spec_id: Option<&'a str>,
    pub task_id: Option<&'a str>,
    /// Working directory for the invocation (required by ClaudeCLI).
    pub worktree_path: &'a str,
}

/// Output from a successful Provider::invoke call.
#[derive(Debug)]
pub struct RuntimeOutput {
    pub output: String,
    pub success: bool,
    pub startup_ms: u64,
    pub inference_ms: u64,
    pub total_ms: u64,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub tool_call_count: i64,
}

/// First-class provider trait. All runtime dispatch goes through this.
/// Each provider impl owns its own config validation, invocation, and cost tracking.
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> Capabilities;
    fn validate_config(&self, phase: &PhaseConfig) -> Result<(), ProviderError>;
    fn invoke(&self, ctx: InvocationContext) -> Result<RuntimeOutput, ProviderError>;
    fn cost_estimate(&self, ctx: &InvocationContext) -> Option<Decimal>;
    fn actual_cost(&self, response: &RuntimeOutput) -> Option<Decimal>;
}

/// Provider that handles deterministic phases (commit, merge, cleanup).
/// These phases use `completion_handler` builtins — no LLM is invoked.
pub struct DeterministicProvider;

impl Provider for DeterministicProvider {
    fn name(&self) -> &str {
        "deterministic"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_use: false,
            streaming: false,
            vision: false,
            thinking: false,
            max_tokens_in: 0,
            max_tokens_out: 0,
        }
    }

    fn validate_config(&self, _phase: &PhaseConfig) -> Result<(), ProviderError> {
        Ok(())
    }

    fn invoke(&self, ctx: InvocationContext) -> Result<RuntimeOutput, ProviderError> {
        Err(ProviderError::NotConfigured {
            provider: "deterministic".into(),
            reason: format!(
                "phase '{}' is deterministic — invoke via builtins, not Provider::invoke",
                ctx.phase.name
            ),
        })
    }

    fn cost_estimate(&self, _ctx: &InvocationContext) -> Option<Decimal> {
        None
    }

    fn actual_cost(&self, _response: &RuntimeOutput) -> Option<Decimal> {
        None
    }
}

/// Whether a provider is active or disabled (with a reason).
#[derive(Debug, Clone)]
pub enum ProviderStatus {
    Active,
    Disabled(String),
}

/// Minimal PhaseConfig used when validating a provider at registration time.
/// All providers that need credentials check only `self.api_key` (not the phase),
/// so any well-formed phase works here.
fn probe_phase(provider_name: &str) -> PhaseConfig {
    PhaseConfig {
        name: provider_name.to_string(),
        level: crate::phases::PhaseLevel::Task,
        description: String::new(),
        prompt_template: String::new(),
        timeout_minutes: None,
        retry_count: None,
        can_add_tasks: false,
        can_fail_spec: false,
        requires_claude: false,
        runtime: Some(provider_name.to_string()),
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
}

/// Registry of all known providers.
///
/// Providers are keyed by their `name()`. Disabled entries (e.g. missing API key)
/// are tracked separately so callers can surface actionable messages.
pub struct ProviderRegistry {
    providers: HashMap<String, Box<dyn Provider>>,
    disabled: HashMap<String, String>,
}

impl ProviderRegistry {
    /// Build a registry with the three built-in providers.
    ///
    /// - `claude` — always registered (validate_config always passes)
    /// - `openrouter` — auto-disabled if OPENROUTER_API_KEY is absent (via registration-time validation)
    /// - `deterministic` — always registered
    pub fn new() -> Self {
        let mut registry = ProviderRegistry {
            providers: HashMap::new(),
            disabled: HashMap::new(),
        };

        registry.register(Box::new(claude::ClaudeCLIProvider::default()));

        let api_key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
        registry.register(Box::new(openrouter::OpenRouterProvider::new(
            api_key,
            "openai/gpt-4o",
        )));

        registry.register(Box::new(DeterministicProvider));

        registry
    }

    /// Register a provider.
    ///
    /// **Validation point 1 (registration time):** calls `validate_config` immediately.
    /// If validation fails the provider is inserted into the disabled map with the
    /// error message as the reason — it is NOT added to the active providers map.
    /// This is how OPENROUTER_API_KEY absence surfaces at startup rather than
    /// silently falling through to Claude.
    pub fn register(&mut self, p: Box<dyn Provider>) {
        let name = p.name().to_string();
        let probe = probe_phase(&name);
        match p.validate_config(&probe) {
            Ok(()) => {
                self.providers.insert(name, p);
            }
            Err(e) => {
                self.disabled.insert(name, e.to_string());
            }
        }
    }

    pub fn disable(&mut self, name: &str, reason: String) {
        self.providers.remove(name);
        self.disabled.insert(name.to_string(), reason);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Provider> {
        self.providers.get(name).map(|p| p.as_ref())
    }

    /// Returns all providers sorted by name, with their status.
    pub fn list(&self) -> Vec<(&str, ProviderStatus)> {
        let mut result: Vec<(&str, ProviderStatus)> = self
            .providers
            .iter()
            .map(|(k, _)| (k.as_str(), ProviderStatus::Active))
            .collect();
        for (k, reason) in &self.disabled {
            result.push((k.as_str(), ProviderStatus::Disabled(reason.clone())));
        }
        result.sort_by_key(|(name, _)| *name);
        result
    }

    /// Check whether a phase's named runtime is available and configured.
    pub fn validate_phase(&self, phase: &PhaseConfig) -> Result<(), ProviderError> {
        let runtime = match phase.runtime.as_deref() {
            Some(r) => r,
            None => return Ok(()),
        };

        if let Some(reason) = self.disabled.get(runtime) {
            return Err(ProviderError::NotConfigured {
                provider: runtime.to_string(),
                reason: reason.clone(),
            });
        }

        match self.providers.get(runtime) {
            Some(p) => p.validate_config(phase),
            None => Err(ProviderError::NotConfigured {
                provider: runtime.to_string(),
                reason: "not registered".to_string(),
            }),
        }
    }

    /// **Validation point 2 (phase TOML load time):** iterate all loaded phases and
    /// emit a LOUD warning for any phase whose `runtime` field names a provider that
    /// is disabled or missing.
    ///
    /// This is what surfaces the OpenRouter-runtime-drop bug at daemon startup
    /// instead of silently falling through to Claude.  Call this after
    /// `PhaseRegistry::new()` returns and the `ProviderRegistry` is built.
    pub fn validate_phases<'a>(&self, phases: impl Iterator<Item = &'a PhaseConfig>) {
        for phase in phases {
            let runtime = match phase.runtime.as_deref() {
                Some(r) => r,
                None => continue,
            };
            if let Err(e) = self.validate_phase(phase) {
                let env_hint = match runtime {
                    "openrouter" => " Add OPENROUTER_API_KEY to ~/.boi/.env.",
                    _ => "",
                };
                eprintln!(
                    "WARN: phase '{}' wants runtime='{}' but {}. \
                     Phases using this runtime will fail until configured.{}",
                    phase.name, runtime, e, env_hint
                );
            }
        }
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod provider_registry {
    use super::*;
    use crate::phases::{PhaseConfig, PhaseLevel};

    fn phase_with_runtime(runtime: Option<&str>) -> PhaseConfig {
        PhaseConfig {
            name: "test-phase".into(),
            level: PhaseLevel::Task,
            description: "test".into(),
            prompt_template: "Do it.".into(),
            timeout_minutes: Some(5),
            retry_count: None,
            can_add_tasks: false,
            can_fail_spec: false,
            requires_claude: true,
            runtime: runtime.map(|s| s.to_string()),
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
    }

    #[test]
    fn test_registry_new_has_claude() {
        let reg = ProviderRegistry::new();
        assert!(reg.get("claude").is_some());
    }

    #[test]
    fn test_registry_new_has_deterministic() {
        let reg = ProviderRegistry::new();
        assert!(reg.get("deterministic").is_some());
    }

    #[test]
    fn test_registry_openrouter_disabled_when_no_key() {
        // Safe to call in tests: if the key happens to be set in CI, provider is active.
        let reg = ProviderRegistry::new();
        let key_set = std::env::var("OPENROUTER_API_KEY").map(|k| !k.is_empty()).unwrap_or(false);
        if key_set {
            assert!(reg.get("openrouter").is_some());
        } else {
            assert!(reg.get("openrouter").is_none());
            assert!(reg.disabled.contains_key("openrouter"));
        }
    }

    #[test]
    fn test_registry_list_includes_all_names() {
        let reg = ProviderRegistry::new();
        let names: Vec<&str> = reg.list().iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"claude"), "claude missing: {:?}", names);
        assert!(names.contains(&"deterministic"), "deterministic missing: {:?}", names);
        assert!(names.contains(&"openrouter"), "openrouter missing: {:?}", names);
    }

    #[test]
    fn test_registry_list_sorted() {
        let reg = ProviderRegistry::new();
        let names: Vec<&str> = reg.list().iter().map(|(n, _)| *n).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "list() should be sorted alphabetically");
    }

    #[test]
    fn test_registry_register_custom() {
        struct FakeProvider;
        impl Provider for FakeProvider {
            fn name(&self) -> &str { "fake" }
            fn capabilities(&self) -> Capabilities {
                Capabilities { tool_use: false, streaming: false, vision: false, thinking: false, max_tokens_in: 0, max_tokens_out: 0 }
            }
            fn validate_config(&self, _: &PhaseConfig) -> Result<(), ProviderError> { Ok(()) }
            fn invoke(&self, _: InvocationContext) -> Result<RuntimeOutput, ProviderError> {
                Err(ProviderError::Other(anyhow::anyhow!("fake")))
            }
            fn cost_estimate(&self, _: &InvocationContext) -> Option<Decimal> { None }
            fn actual_cost(&self, _: &RuntimeOutput) -> Option<Decimal> { None }
        }

        let mut reg = ProviderRegistry::new();
        reg.register(Box::new(FakeProvider));
        assert!(reg.get("fake").is_some());
    }

    #[test]
    fn test_registry_disable() {
        let mut reg = ProviderRegistry::new();
        reg.disable("claude", "test disable".to_string());
        assert!(reg.get("claude").is_none());
        assert_eq!(reg.disabled.get("claude").map(|s| s.as_str()), Some("test disable"));
    }

    #[test]
    fn test_validate_phase_no_runtime() {
        let reg = ProviderRegistry::new();
        let phase = phase_with_runtime(None);
        assert!(reg.validate_phase(&phase).is_ok());
    }

    #[test]
    fn test_validate_phase_claude() {
        let reg = ProviderRegistry::new();
        let phase = phase_with_runtime(Some("claude"));
        assert!(reg.validate_phase(&phase).is_ok());
    }

    #[test]
    fn test_validate_phase_deterministic() {
        let reg = ProviderRegistry::new();
        let phase = phase_with_runtime(Some("deterministic"));
        assert!(reg.validate_phase(&phase).is_ok());
    }

    #[test]
    fn test_validate_phase_unknown_runtime() {
        let reg = ProviderRegistry::new();
        let phase = phase_with_runtime(Some("nonexistent-provider"));
        let err = reg.validate_phase(&phase).unwrap_err();
        assert!(matches!(err, ProviderError::NotConfigured { .. }));
    }

    #[test]
    fn test_validate_phase_disabled_provider() {
        let mut reg = ProviderRegistry::new();
        reg.disable("claude", "intentionally disabled".to_string());
        let phase = phase_with_runtime(Some("claude"));
        let err = reg.validate_phase(&phase).unwrap_err();
        assert!(matches!(err, ProviderError::NotConfigured { .. }));
    }

    #[test]
    fn test_deterministic_provider_name() {
        let p = DeterministicProvider;
        assert_eq!(p.name(), "deterministic");
    }

    #[test]
    fn test_deterministic_provider_validate_ok() {
        let p = DeterministicProvider;
        let phase = phase_with_runtime(None);
        assert!(p.validate_config(&phase).is_ok());
    }

    #[test]
    fn test_deterministic_provider_invoke_returns_error() {
        let p = DeterministicProvider;
        let phase = phase_with_runtime(None);
        let ctx = InvocationContext {
            phase: &phase,
            prompt: "x",
            model: "m",
            timeout: Duration::from_secs(10),
            spec_id: None,
            task_id: None,
            worktree_path: "/tmp",
        };
        let err = p.invoke(ctx).unwrap_err();
        assert!(matches!(err, ProviderError::NotConfigured { .. }));
    }
}

#[cfg(test)]
mod provider_trait {
    use super::*;
    use crate::phases::{PhaseConfig, PhaseLevel};

    fn mock_phase() -> PhaseConfig {
        PhaseConfig {
            name: "test-phase".into(),
            level: PhaseLevel::Task,
            description: "test".into(),
            prompt_template: "Do the thing.".into(),
            timeout_minutes: Some(5),
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
    }

    struct MockProvider {
        name: String,
        should_fail: bool,
    }

    impl Provider for MockProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                tool_use: true,
                streaming: false,
                vision: false,
                thinking: true,
                max_tokens_in: 200_000,
                max_tokens_out: 8_096,
            }
        }
        fn validate_config(&self, _phase: &PhaseConfig) -> Result<(), ProviderError> {
            if self.should_fail {
                Err(ProviderError::AuthFailed {
                    provider: self.name.clone(),
                    env_var: "TEST_KEY".into(),
                })
            } else {
                Ok(())
            }
        }
        fn invoke(&self, ctx: InvocationContext) -> Result<RuntimeOutput, ProviderError> {
            if self.should_fail {
                return Err(ProviderError::Other(anyhow::anyhow!("mock failure")));
            }
            Ok(RuntimeOutput {
                output: format!("mock output for: {}", ctx.prompt),
                success: true,
                startup_ms: 10,
                inference_ms: 100,
                total_ms: 110,
                input_tokens: Some(50),
                output_tokens: Some(20),
                cache_read_tokens: None,
                cache_creation_tokens: None,
                cost_usd: Some(0.001),
                tool_call_count: 0,
            })
        }
        fn cost_estimate(&self, _ctx: &InvocationContext) -> Option<Decimal> {
            None
        }
        fn actual_cost(&self, response: &RuntimeOutput) -> Option<Decimal> {
            response
                .cost_usd
                .map(|c| Decimal::try_from(c).unwrap_or(Decimal::ZERO))
        }
    }

    #[test]
    fn test_provider_trait_name() {
        let p = MockProvider { name: "test".into(), should_fail: false };
        assert_eq!(p.name(), "test");
    }

    #[test]
    fn test_provider_trait_capabilities() {
        let p = MockProvider { name: "test".into(), should_fail: false };
        let caps = p.capabilities();
        assert!(caps.tool_use);
        assert!(caps.thinking);
        assert!(!caps.streaming);
        assert!(!caps.vision);
        assert_eq!(caps.max_tokens_in, 200_000);
        assert_eq!(caps.max_tokens_out, 8_096);
    }

    #[test]
    fn test_provider_trait_validate_config_ok() {
        let p = MockProvider { name: "test".into(), should_fail: false };
        let phase = mock_phase();
        assert!(p.validate_config(&phase).is_ok());
    }

    #[test]
    fn test_provider_trait_validate_config_fail() {
        let p = MockProvider { name: "fail".into(), should_fail: true };
        let phase = mock_phase();
        let err = p.validate_config(&phase).unwrap_err();
        assert!(matches!(err, ProviderError::AuthFailed { .. }));
    }

    #[test]
    fn test_provider_trait_invoke_success() {
        let p = MockProvider { name: "test".into(), should_fail: false };
        let phase = mock_phase();
        let ctx = InvocationContext {
            phase: &phase,
            prompt: "hello world",
            model: "test-model",
            timeout: Duration::from_secs(30),
            spec_id: None,
            task_id: None,
            worktree_path: "/tmp",
        };
        let out = p.invoke(ctx).expect("invoke should succeed");
        assert!(out.success);
        assert!(out.output.contains("hello world"));
        assert_eq!(out.startup_ms, 10);
        assert_eq!(out.inference_ms, 100);
        assert_eq!(out.input_tokens, Some(50));
    }

    #[test]
    fn test_provider_trait_invoke_failure() {
        let p = MockProvider { name: "fail".into(), should_fail: true };
        let phase = mock_phase();
        let ctx = InvocationContext {
            phase: &phase,
            prompt: "hello",
            model: "test-model",
            timeout: Duration::from_secs(30),
            spec_id: None,
            task_id: None,
            worktree_path: "/tmp",
        };
        let err = p.invoke(ctx).unwrap_err();
        assert!(matches!(err, ProviderError::Other(_)));
    }

    #[test]
    fn test_provider_trait_actual_cost() {
        let p = MockProvider { name: "test".into(), should_fail: false };
        let phase = mock_phase();
        let ctx = InvocationContext {
            phase: &phase,
            prompt: "x",
            model: "m",
            timeout: Duration::from_secs(10),
            spec_id: None,
            task_id: None,
            worktree_path: "/tmp",
        };
        let out = p.invoke(ctx).unwrap();
        let cost = p.actual_cost(&out);
        assert!(cost.is_some());
        assert!(cost.unwrap() > Decimal::ZERO);
    }

    #[test]
    fn test_provider_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MockProvider>();
    }

    #[test]
    fn test_invocation_context_fields() {
        let phase = mock_phase();
        let ctx = InvocationContext {
            phase: &phase,
            prompt: "test prompt",
            model: "claude-3-5-sonnet",
            timeout: Duration::from_secs(300),
            spec_id: Some("spec-123"),
            task_id: Some("task-456"),
            worktree_path: "/workspace/my-repo",
        };
        assert_eq!(ctx.prompt, "test prompt");
        assert_eq!(ctx.model, "claude-3-5-sonnet");
        assert_eq!(ctx.spec_id, Some("spec-123"));
        assert_eq!(ctx.task_id, Some("task-456"));
        assert_eq!(ctx.worktree_path, "/workspace/my-repo");
        assert_eq!(ctx.timeout, Duration::from_secs(300));
    }

    #[test]
    fn test_provider_error_display() {
        let e = ProviderError::NotConfigured {
            provider: "openrouter".into(),
            reason: "OPENROUTER_API_KEY not set".into(),
        };
        let msg = e.to_string();
        assert!(msg.contains("openrouter"));
        assert!(msg.contains("OPENROUTER_API_KEY"));
    }

    #[test]
    fn test_runtime_output_fields() {
        let out = RuntimeOutput {
            output: "done".into(),
            success: true,
            startup_ms: 100,
            inference_ms: 500,
            total_ms: 600,
            input_tokens: Some(1000),
            output_tokens: Some(200),
            cache_read_tokens: Some(50),
            cache_creation_tokens: None,
            cost_usd: Some(0.005),
            tool_call_count: 3,
        };
        assert!(out.success);
        assert_eq!(out.tool_call_count, 3);
        assert_eq!(out.total_ms, 600);
    }

    #[test]
    fn test_capabilities_struct() {
        let caps = Capabilities {
            tool_use: true,
            streaming: false,
            vision: true,
            thinking: false,
            max_tokens_in: 128_000,
            max_tokens_out: 4_096,
        };
        assert!(caps.tool_use);
        assert!(!caps.streaming);
        assert!(caps.vision);
        assert!(!caps.thinking);
        assert_eq!(caps.max_tokens_in, 128_000);
    }
}

#[cfg(test)]
mod provider_error {
    use super::*;

    #[test]
    fn test_not_configured_display() {
        let e = ProviderError::NotConfigured {
            provider: "openrouter".into(),
            reason: "OPENROUTER_API_KEY not set".into(),
        };
        let msg = e.to_string();
        assert!(msg.contains("openrouter"), "msg: {msg}");
        assert!(msg.contains("OPENROUTER_API_KEY"), "msg: {msg}");
        assert!(msg.contains("not configured"), "msg: {msg}");
    }

    #[test]
    fn test_auth_failed_display() {
        let e = ProviderError::AuthFailed {
            provider: "codex".into(),
            env_var: "OPENAI_API_KEY".into(),
        };
        let msg = e.to_string();
        assert!(msg.contains("codex"), "msg: {msg}");
        assert!(msg.contains("OPENAI_API_KEY"), "msg: {msg}");
        assert!(msg.contains("auth failed"), "msg: {msg}");
    }

    #[test]
    fn test_rate_limit_display_with_retry() {
        let e = ProviderError::RateLimit {
            provider: "openrouter".into(),
            retry_after_s: Some(60),
        };
        let msg = e.to_string();
        assert!(msg.contains("openrouter"), "msg: {msg}");
        assert!(msg.contains("rate limit"), "msg: {msg}");
        assert!(msg.contains("60"), "msg: {msg}");
    }

    #[test]
    fn test_rate_limit_display_no_retry() {
        let e = ProviderError::RateLimit {
            provider: "openrouter".into(),
            retry_after_s: None,
        };
        let msg = e.to_string();
        assert!(msg.contains("rate limit"), "msg: {msg}");
    }

    #[test]
    fn test_timeout_display() {
        let e = ProviderError::Timeout { secs: 300 };
        let msg = e.to_string();
        assert!(msg.contains("300"), "msg: {msg}");
        assert!(msg.contains("timeout"), "msg: {msg}");
    }

    #[test]
    fn test_bad_response_display() {
        let e = ProviderError::BadResponse {
            provider: "openrouter".into(),
            body_excerpt: r#"{"error":"invalid"}"#.into(),
        };
        let msg = e.to_string();
        assert!(msg.contains("openrouter"), "msg: {msg}");
        assert!(msg.contains("bad response"), "msg: {msg}");
        assert!(msg.contains("invalid"), "msg: {msg}");
    }

    #[test]
    fn test_network_error_display() {
        let e = ProviderError::NetworkError(anyhow::anyhow!("connection refused"));
        let msg = e.to_string();
        assert!(msg.contains("network error"), "msg: {msg}");
        assert!(msg.contains("connection refused"), "msg: {msg}");
    }

    #[test]
    fn test_capability_missing_display() {
        let e = ProviderError::CapabilityMissing {
            provider: "openrouter".into(),
            required: "tool_use",
        };
        let msg = e.to_string();
        assert!(msg.contains("openrouter"), "msg: {msg}");
        assert!(msg.contains("tool_use"), "msg: {msg}");
        assert!(msg.contains("missing capability"), "msg: {msg}");
    }

    #[test]
    fn test_budget_exceeded_display() {
        let e = ProviderError::BudgetExceeded {
            provider: "openrouter".into(),
            period: "daily".into(),
        };
        let msg = e.to_string();
        assert!(msg.contains("openrouter"), "msg: {msg}");
        assert!(msg.contains("daily"), "msg: {msg}");
        assert!(msg.contains("budget exceeded"), "msg: {msg}");
    }

    #[test]
    fn test_other_display() {
        let e = ProviderError::Other(anyhow::anyhow!("something unexpected"));
        let msg = e.to_string();
        assert!(msg.contains("something unexpected"), "msg: {msg}");
    }

    #[test]
    fn test_provider_error_is_std_error() {
        fn takes_std_error(_: &dyn std::error::Error) {}
        let e = ProviderError::Timeout { secs: 10 };
        takes_std_error(&e);
    }

    #[test]
    fn test_all_variants_match() {
        let variants: Vec<ProviderError> = vec![
            ProviderError::NotConfigured { provider: "p".into(), reason: "r".into() },
            ProviderError::AuthFailed { provider: "p".into(), env_var: "E".into() },
            ProviderError::RateLimit { provider: "p".into(), retry_after_s: None },
            ProviderError::Timeout { secs: 1 },
            ProviderError::BadResponse { provider: "p".into(), body_excerpt: "x".into() },
            ProviderError::NetworkError(anyhow::anyhow!("net")),
            ProviderError::CapabilityMissing { provider: "p".into(), required: "tool_use" },
            ProviderError::BudgetExceeded { provider: "p".into(), period: "daily".into() },
            ProviderError::Other(anyhow::anyhow!("other")),
        ];
        for v in &variants {
            assert!(!v.to_string().is_empty(), "Display should be non-empty for {:?}", v);
        }
    }
}

#[cfg(test)]
mod provider_validation_startup {
    use super::*;
    use crate::phases::{PhaseConfig, PhaseLevel};

    fn phase_with_runtime(name: &str, runtime: &str) -> PhaseConfig {
        PhaseConfig {
            name: name.into(),
            level: PhaseLevel::Task,
            description: String::new(),
            prompt_template: String::new(),
            timeout_minutes: None,
            retry_count: None,
            can_add_tasks: false,
            can_fail_spec: false,
            requires_claude: false,
            runtime: Some(runtime.into()),
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
    }

    fn phase_no_runtime(name: &str) -> PhaseConfig {
        let mut p = phase_with_runtime(name, "claude");
        p.runtime = None;
        p
    }

    struct AlwaysFailProvider {
        pname: String,
    }

    impl Provider for AlwaysFailProvider {
        fn name(&self) -> &str { &self.pname }
        fn capabilities(&self) -> Capabilities {
            Capabilities { tool_use: false, streaming: false, vision: false, thinking: false, max_tokens_in: 0, max_tokens_out: 0 }
        }
        fn validate_config(&self, _: &PhaseConfig) -> Result<(), ProviderError> {
            Err(ProviderError::AuthFailed {
                provider: self.pname.clone(),
                env_var: "FAKE_API_KEY".into(),
            })
        }
        fn invoke(&self, _: InvocationContext) -> Result<RuntimeOutput, ProviderError> {
            Err(ProviderError::Other(anyhow::anyhow!("not implemented")))
        }
        fn cost_estimate(&self, _: &InvocationContext) -> Option<Decimal> { None }
        fn actual_cost(&self, _: &RuntimeOutput) -> Option<Decimal> { None }
    }

    // ── Validation point 1: registration time ────────────────────────────────

    #[test]
    fn test_register_auto_disables_failing_provider() {
        let mut reg = ProviderRegistry { providers: HashMap::new(), disabled: HashMap::new() };
        reg.register(Box::new(AlwaysFailProvider { pname: "bad-prov".into() }));
        assert!(reg.get("bad-prov").is_none(), "failing provider must not be in active map");
        assert!(reg.disabled.contains_key("bad-prov"), "failing provider must be in disabled map");
    }

    #[test]
    fn test_register_keeps_passing_provider_active() {
        let mut reg = ProviderRegistry { providers: HashMap::new(), disabled: HashMap::new() };
        reg.register(Box::new(claude::ClaudeCLIProvider::default()));
        assert!(reg.get("claude").is_some(), "claude must be active after registration");
        assert!(!reg.disabled.contains_key("claude"), "claude must not be in disabled map");
    }

    #[test]
    fn test_disabled_map_has_non_empty_reason_after_auto_disable() {
        let mut reg = ProviderRegistry { providers: HashMap::new(), disabled: HashMap::new() };
        reg.register(Box::new(AlwaysFailProvider { pname: "bad-prov".into() }));
        let reason = reg.disabled.get("bad-prov").cloned().unwrap_or_default();
        assert!(!reason.is_empty(), "disabled reason must not be empty; got: {:?}", reason);
    }

    #[test]
    fn test_openrouter_auto_disabled_when_key_absent() {
        let key_set = std::env::var("OPENROUTER_API_KEY")
            .map(|k| !k.is_empty())
            .unwrap_or(false);
        if !key_set {
            let reg = ProviderRegistry::new();
            assert!(reg.get("openrouter").is_none(), "openrouter should be auto-disabled without API key");
            assert!(reg.disabled.contains_key("openrouter"), "openrouter must appear in disabled map");
        }
    }

    #[test]
    fn test_registration_time_disable_prevents_lookup() {
        let mut reg = ProviderRegistry { providers: HashMap::new(), disabled: HashMap::new() };
        reg.register(Box::new(AlwaysFailProvider { pname: "no-creds".into() }));
        // get() must return None so runner can't accidentally invoke the provider
        assert!(reg.get("no-creds").is_none());
    }

    // ── Validation point 2: phase TOML load time ─────────────────────────────

    #[test]
    fn test_validate_phases_active_providers_no_warning() {
        let reg = ProviderRegistry::new();
        let phases = vec![
            phase_with_runtime("phase-a", "claude"),
            phase_with_runtime("phase-b", "deterministic"),
        ];
        // Must not panic; all named providers are active
        reg.validate_phases(phases.iter());
    }

    #[test]
    fn test_validate_phases_skips_phases_without_runtime() {
        let reg = ProviderRegistry::new();
        let phases = vec![phase_no_runtime("no-runtime-phase")];
        // No runtime field → no validation, no panic
        reg.validate_phases(phases.iter());
    }

    #[test]
    fn test_validate_phases_disabled_provider_emits_no_panic() {
        let mut reg = ProviderRegistry::new();
        reg.disable("claude", "test-disabled".to_string());
        let phases = vec![phase_with_runtime("needs-claude", "claude")];
        // Emits WARN to stderr but must not panic or propagate an error
        reg.validate_phases(phases.iter());
    }

    #[test]
    fn test_validate_phases_unknown_runtime_emits_no_panic() {
        let reg = ProviderRegistry::new();
        let phases = vec![phase_with_runtime("mystery-phase", "nonexistent-llm")];
        // Emits WARN to stderr but must not panic
        reg.validate_phases(phases.iter());
    }

    #[test]
    fn test_validate_phases_error_via_validate_phase_for_disabled() {
        let mut reg = ProviderRegistry::new();
        reg.disable("claude", "intentionally disabled for test".to_string());
        let phase = phase_with_runtime("probe-phase", "claude");
        // The underlying validate_phase must return Err (drives the WARN path)
        assert!(
            reg.validate_phase(&phase).is_err(),
            "validate_phase must return Err for disabled provider"
        );
    }

    // ── Validation point 3: pre-invocation ───────────────────────────────────

    #[test]
    fn test_pre_invocation_validate_catches_disabled_provider() {
        let mut reg = ProviderRegistry::new();
        reg.disable("claude", "simulating dynamic env change".to_string());
        let phase = phase_with_runtime("task-phase", "claude");
        // Pre-invocation path: validate before invoke()
        let result = reg.validate_phase(&phase);
        assert!(result.is_err(), "pre-invoke validation must fail for disabled provider");
        assert!(
            matches!(result.unwrap_err(), ProviderError::NotConfigured { .. }),
            "error must be NotConfigured"
        );
    }

    #[test]
    fn test_pre_invocation_validate_passes_for_active_provider() {
        let reg = ProviderRegistry::new();
        let phase = phase_with_runtime("task-phase", "claude");
        assert!(
            reg.validate_phase(&phase).is_ok(),
            "pre-invoke validation must pass for active claude provider"
        );
    }

    #[test]
    fn test_pre_invocation_validate_catches_unknown_runtime() {
        let reg = ProviderRegistry::new();
        let phase = phase_with_runtime("task-phase", "unknown-runtime");
        let result = reg.validate_phase(&phase);
        assert!(result.is_err(), "pre-invoke validation must fail for unknown runtime");
        assert!(
            matches!(result.unwrap_err(), ProviderError::NotConfigured { .. }),
            "error must be NotConfigured"
        );
    }
}
