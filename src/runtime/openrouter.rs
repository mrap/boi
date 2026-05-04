use super::{Capabilities, InvocationContext, Provider, ProviderError, RuntimeOutput};
use crate::phases::PhaseConfig;
use rust_decimal::Decimal;
use std::time::Instant;

const OPENROUTER_API_URL: &str = "https://openrouter.ai/api/v1/chat/completions";

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

    fn invoke(&self, ctx: InvocationContext) -> Result<RuntimeOutput, ProviderError> {
        let model = if ctx.model.is_empty() { &self.model } else { ctx.model };
        let start = Instant::now();

        let body = serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": ctx.prompt}],
            "max_tokens": 4096,
        });

        let client = reqwest::blocking::Client::builder()
            .timeout(ctx.timeout)
            .build()
            .map_err(|e| ProviderError::NetworkError(anyhow::anyhow!("{}", e)))?;

        let resp = client
            .post(OPENROUTER_API_URL)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", "https://github.com/mrap/boi")
            .header("X-Title", "boi-spec-runner")
            .json(&body)
            .send()
            .map_err(|e| ProviderError::NetworkError(anyhow::anyhow!("{}", e)))?;

        let status = resp.status();
        if status.as_u16() == 429 {
            return Err(ProviderError::RateLimit {
                provider: "openrouter".into(),
                retry_after_s: None,
            });
        }

        let total_ms = start.elapsed().as_millis() as u64;

        let json: serde_json::Value = resp.json().map_err(|e| ProviderError::BadResponse {
            provider: "openrouter".into(),
            body_excerpt: format!("json parse error: {}", e),
        })?;

        if !status.is_success() {
            let excerpt = json.to_string();
            let excerpt = if excerpt.len() > 300 { &excerpt[..300] } else { &excerpt };
            return Err(ProviderError::BadResponse {
                provider: "openrouter".into(),
                body_excerpt: excerpt.to_string(),
            });
        }

        let content = json
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let input_tokens = json.pointer("/usage/prompt_tokens").and_then(|v| v.as_i64());
        let output_tokens = json.pointer("/usage/completion_tokens").and_then(|v| v.as_i64());
        let cost_usd = json.pointer("/usage/cost").and_then(|v| v.as_f64());

        Ok(RuntimeOutput {
            output: content,
            success: true,
            startup_ms: 0,
            inference_ms: total_ms,
            total_ms,
            input_tokens,
            output_tokens,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            cost_usd,
            tool_call_count: 0,
        })
    }

    fn cost_estimate(&self, _ctx: &InvocationContext) -> Option<Decimal> {
        None
    }

    fn actual_cost(&self, response: &RuntimeOutput) -> Option<Decimal> {
        response.cost_usd.and_then(|c| Decimal::try_from(c).ok())
    }
}
