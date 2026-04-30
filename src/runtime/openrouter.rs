use super::{Capabilities, InvocationContext, Provider, ProviderError, RuntimeOutput};
use crate::phases::PhaseConfig;
use rust_decimal::Decimal;
use serde_json::json;
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
        let timeout_secs = ctx.timeout.as_secs();

        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .map_err(|e| ProviderError::NetworkError(anyhow::anyhow!("{}", e)))?;

        let body = json!({
            "model": model,
            "messages": [{"role": "user", "content": ctx.prompt}],
            "max_tokens": 8096
        });

        let t0 = Instant::now();

        let resp = client
            .post(OPENROUTER_API_URL)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .header("HTTP-Referer", "https://github.com/mrap/boi")
            .header("X-Title", "boi-spec-runner")
            .json(&body)
            .send()
            .map_err(|e| {
                if e.is_timeout() {
                    ProviderError::Timeout { secs: timeout_secs }
                } else {
                    ProviderError::NetworkError(anyhow::anyhow!("{}", e))
                }
            })?;

        let startup_ms = t0.elapsed().as_millis() as u64;
        let status = resp.status();

        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ProviderError::AuthFailed {
                provider: "openrouter".into(),
                env_var: "OPENROUTER_API_KEY".into(),
            });
        }

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u32>().ok());
            return Err(ProviderError::RateLimit {
                provider: "openrouter".into(),
                retry_after_s: retry,
            });
        }

        let body_text = resp.text().map_err(|e| ProviderError::NetworkError(anyhow::anyhow!("{}", e)))?;
        let total_ms = t0.elapsed().as_millis() as u64;
        let inference_ms = total_ms.saturating_sub(startup_ms);

        if !status.is_success() {
            let excerpt = body_text.chars().take(200).collect::<String>();
            return Err(ProviderError::BadResponse {
                provider: "openrouter".into(),
                body_excerpt: format!("HTTP {}: {}", status.as_u16(), excerpt),
            });
        }

        let parsed: serde_json::Value = serde_json::from_str(&body_text).map_err(|e| {
            ProviderError::BadResponse {
                provider: "openrouter".into(),
                body_excerpt: format!("JSON parse error: {} — body: {}", e, &body_text.chars().take(200).collect::<String>()),
            }
        })?;

        let content = parsed
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ProviderError::BadResponse {
                provider: "openrouter".into(),
                body_excerpt: format!("no choices[0].message.content — body: {}", &body_text.chars().take(200).collect::<String>()),
            })?;

        let input_tokens = parsed.pointer("/usage/prompt_tokens").and_then(|v| v.as_i64());
        let output_tokens = parsed.pointer("/usage/completion_tokens").and_then(|v| v.as_i64());
        let cost_usd = parsed.pointer("/usage/cost").and_then(|v| v.as_f64());

        Ok(RuntimeOutput {
            output: content.to_string(),
            success: true,
            startup_ms,
            inference_ms,
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

#[cfg(test)]
mod openrouter_provider {
    use super::*;
    use crate::phases::{PhaseConfig, PhaseLevel};

    fn stub_phase() -> PhaseConfig {
        PhaseConfig {
            name: "test".into(),
            level: PhaseLevel::Task,
            description: String::new(),
            prompt_template: String::new(),
            timeout_minutes: None,
            retry_count: None,
            can_add_tasks: false,
            can_fail_spec: false,
            requires_claude: false,
            runtime: Some("openrouter".into()),
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
    fn test_validate_config_fails_with_empty_key() {
        let p = OpenRouterProvider::new("", "openai/gpt-4o");
        assert!(p.validate_config(&stub_phase()).is_err());
    }

    #[test]
    fn test_validate_config_ok_with_key() {
        let p = OpenRouterProvider::new("sk-test-key", "openai/gpt-4o");
        assert!(p.validate_config(&stub_phase()).is_ok());
    }

    #[test]
    fn test_name() {
        let p = OpenRouterProvider::new("key", "model");
        assert_eq!(p.name(), "openrouter");
    }

    #[test]
    fn test_capabilities() {
        let p = OpenRouterProvider::new("key", "model");
        let caps = p.capabilities();
        assert!(!caps.tool_use);
        assert!(!caps.streaming);
        assert_eq!(caps.max_tokens_out, 8_096);
    }

    #[test]
    fn test_actual_cost_with_cost_usd() {
        let p = OpenRouterProvider::new("key", "model");
        let ro = RuntimeOutput {
            output: String::new(),
            success: true,
            startup_ms: 0,
            inference_ms: 0,
            total_ms: 0,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            cost_usd: Some(0.001234),
            tool_call_count: 0,
        };
        assert!(p.actual_cost(&ro).is_some());
    }
}
