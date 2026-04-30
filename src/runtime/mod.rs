pub mod claude;
pub mod codex;
pub mod openrouter;

use crate::phases::PhaseConfig;
use rust_decimal::Decimal;
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

/// Errors that a provider execution can return.
/// T9935 will expand this into the full unified ProviderError enum.
#[derive(Debug)]
pub enum ProviderError {
    // Legacy variants (used by codex.rs / SpecProvider impls)
    BinaryNotFound(String),
    MissingApiKey(String),
    ExecutionFailed(String),
    Timeout,
    // New variants for the Provider trait
    NotConfigured { provider: String, reason: String },
    NetworkError(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::BinaryNotFound(msg) => write!(f, "binary not found: {}", msg),
            ProviderError::MissingApiKey(msg) => write!(f, "missing API key: {}", msg),
            ProviderError::ExecutionFailed(msg) => write!(f, "execution failed: {}", msg),
            ProviderError::Timeout => write!(f, "execution timed out"),
            ProviderError::NotConfigured { provider, reason } => {
                write!(f, "provider {} not configured: {}", provider, reason)
            }
            ProviderError::NetworkError(msg) => write!(f, "network error: {}", msg),
        }
    }
}

impl std::error::Error for ProviderError {}

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
                Err(ProviderError::MissingApiKey("TEST_KEY not set".into()))
            } else {
                Ok(())
            }
        }
        fn invoke(&self, ctx: InvocationContext) -> Result<RuntimeOutput, ProviderError> {
            if self.should_fail {
                return Err(ProviderError::ExecutionFailed("mock failure".into()));
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
        assert!(matches!(err, ProviderError::MissingApiKey(_)));
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
        assert!(matches!(err, ProviderError::ExecutionFailed(_)));
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
