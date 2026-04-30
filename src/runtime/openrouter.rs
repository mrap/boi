use super::{Capabilities, InvocationContext, Provider, ProviderError, RuntimeOutput};
use crate::phases::PhaseConfig;
use rust_decimal::Decimal;

/// Provider that invokes models via the OpenRouter HTTP API.
/// Stub implementation — full HTTP client wiring is tracked separately.
pub struct OpenRouterProvider {
    pub api_key: String,
    pub model: String,
}

impl OpenRouterProvider {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        OpenRouterProvider {
            api_key: api_key.into(),
            model: model.into(),
        }
    }
}

impl Provider for OpenRouterProvider {
    fn name(&self) -> &str {
        "openrouter"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_use: false,
            streaming: false,
            vision: false,
            thinking: false,
            max_tokens_in: 128_000,
            max_tokens_out: 8_096,
        }
    }

    fn validate_config(&self, _phase: &PhaseConfig) -> Result<(), ProviderError> {
        if self.api_key.is_empty() {
            return Err(ProviderError::NotConfigured {
                provider: "openrouter".into(),
                reason: "OPENROUTER_API_KEY not set".into(),
            });
        }
        Ok(())
    }

    fn invoke(&self, _ctx: InvocationContext) -> Result<RuntimeOutput, ProviderError> {
        Err(ProviderError::NotConfigured {
            provider: "openrouter".into(),
            reason: "OpenRouter HTTP client not yet implemented".into(),
        })
    }

    fn cost_estimate(&self, _ctx: &InvocationContext) -> Option<Decimal> {
        None
    }

    fn actual_cost(&self, _response: &RuntimeOutput) -> Option<Decimal> {
        None
    }
}
