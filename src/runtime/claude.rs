use super::{Capabilities, InvocationContext, Provider, ProviderError, RuntimeOutput};
use crate::phases::PhaseConfig;
use rust_decimal::Decimal;

/// Provider that invokes the Claude CLI binary.
/// Supports tool use and extended thinking; no direct HTTP API.
pub struct ClaudeCLIProvider {
    pub claude_bin: String,
}

impl ClaudeCLIProvider {
    pub fn new(claude_bin: impl Into<String>) -> Self {
        ClaudeCLIProvider { claude_bin: claude_bin.into() }
    }
}

impl Default for ClaudeCLIProvider {
    fn default() -> Self {
        ClaudeCLIProvider {
            claude_bin: std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string()),
        }
    }
}

impl Provider for ClaudeCLIProvider {
    fn name(&self) -> &str {
        "claude"
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
        // Auth is handled by the Claude CLI itself.
        // Binary presence will surface as an error at invoke time.
        Ok(())
    }

    fn invoke(&self, ctx: InvocationContext) -> Result<RuntimeOutput, ProviderError> {
        let model = if ctx.model.is_empty() { None } else { Some(ctx.model) };
        let result = crate::spawn::spawn_claude(
            ctx.prompt,
            ctx.worktree_path,
            ctx.timeout.as_secs(),
            model,
            ctx.spec_id,
            &self.claude_bin,
        )
        .map_err(|e| ProviderError::ExecutionFailed(e.to_string()))?;

        if result.output == "timeout" {
            return Err(ProviderError::Timeout);
        }

        Ok(RuntimeOutput {
            output: result.output,
            success: result.success,
            startup_ms: result.startup_ms,
            inference_ms: result.inference_ms,
            total_ms: result.total_ms,
            input_tokens: result.input_tokens,
            output_tokens: result.output_tokens,
            cache_read_tokens: result.cache_read_tokens,
            cache_creation_tokens: result.cache_creation_tokens,
            cost_usd: result.cost_usd,
            tool_call_count: result.tool_call_count,
        })
    }

    fn cost_estimate(&self, _ctx: &InvocationContext) -> Option<Decimal> {
        None
    }

    fn actual_cost(&self, response: &RuntimeOutput) -> Option<Decimal> {
        response.cost_usd.map(|c| Decimal::try_from(c).unwrap_or(Decimal::ZERO))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phases::{PhaseConfig, PhaseLevel};

    fn test_phase() -> PhaseConfig {
        PhaseConfig {
            name: "execute".into(),
            level: PhaseLevel::Task,
            description: "test".into(),
            prompt_template: "Do it.".into(),
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

    #[test]
    fn test_claude_provider_name() {
        let p = ClaudeCLIProvider::default();
        assert_eq!(p.name(), "claude");
    }

    #[test]
    fn test_claude_provider_capabilities() {
        let p = ClaudeCLIProvider::default();
        let caps = p.capabilities();
        assert!(caps.tool_use);
        assert!(caps.thinking);
        assert!(!caps.streaming);
        assert!(!caps.vision);
        assert_eq!(caps.max_tokens_in, 200_000);
    }

    #[test]
    fn test_claude_provider_validate_config_ok() {
        let p = ClaudeCLIProvider::new("claude");
        let phase = test_phase();
        assert!(p.validate_config(&phase).is_ok());
    }

    #[test]
    fn test_claude_provider_invoke_missing_binary() {
        let p = ClaudeCLIProvider::new("/nonexistent/claude-binary-xyz");
        let phase = test_phase();
        let ctx = super::InvocationContext {
            phase: &phase,
            prompt: "test",
            model: "",
            timeout: std::time::Duration::from_secs(5),
            spec_id: None,
            task_id: None,
            worktree_path: "/tmp",
        };
        let result = p.invoke(ctx);
        assert!(result.is_err(), "expected error for missing binary");
    }

    #[test]
    fn test_claude_provider_actual_cost_none_when_no_cost() {
        let p = ClaudeCLIProvider::default();
        let out = RuntimeOutput {
            output: "x".into(),
            success: true,
            startup_ms: 0,
            inference_ms: 0,
            total_ms: 0,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            cost_usd: None,
            tool_call_count: 0,
        };
        assert!(p.actual_cost(&out).is_none());
    }

    #[test]
    fn test_claude_provider_actual_cost_some() {
        let p = ClaudeCLIProvider::default();
        let out = RuntimeOutput {
            output: "x".into(),
            success: true,
            startup_ms: 0,
            inference_ms: 0,
            total_ms: 0,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            cost_usd: Some(0.025),
            tool_call_count: 0,
        };
        let cost = p.actual_cost(&out).expect("cost should be Some");
        assert!(cost > Decimal::ZERO);
    }
}
