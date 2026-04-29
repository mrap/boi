use std::time::Duration;

use crate::runtime::{PhaseRuntime, RuntimeError, RuntimeOutput};

/// Map shorthand aliases to canonical OpenRouter model IDs.
pub(crate) fn resolve_model(model: &str) -> &str {
    match model {
        "gemini-flash" => "google/gemini-2.0-flash-001",
        "grok" => "x-ai/grok-beta",
        "qwen-coder" => "qwen/qwen-2.5-coder-32b-instruct",
        "haiku" => "anthropic/claude-haiku-4-5",
        other => other,
    }
}

/// Injectable HTTP layer so unit tests never hit the network.
pub(crate) trait HttpPost: Send + Sync {
    fn post_json(
        &self,
        url: &str,
        api_key: &str,
        body: &str,
        timeout: Duration,
    ) -> Result<String, RuntimeError>;
}

/// OpenRouter runtime that sends prompts as chat completion requests.
pub struct OpenRouterRuntime {
    /// Name of the environment variable that holds the API key.
    pub api_key_env: String,
    http: Box<dyn HttpPost>,
}

impl OpenRouterRuntime {
    pub fn new() -> Self {
        Self {
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            http: Box::new(ReqwestHttpPost),
        }
    }

    pub(crate) fn with_http(http: Box<dyn HttpPost>, api_key_env: &str) -> Self {
        Self { api_key_env: api_key_env.to_string(), http }
    }
}

impl Default for OpenRouterRuntime {
    fn default() -> Self {
        Self::new()
    }
}

struct ReqwestHttpPost;

impl HttpPost for ReqwestHttpPost {
    fn post_json(
        &self,
        url: &str,
        api_key: &str,
        body: &str,
        timeout: Duration,
    ) -> Result<String, RuntimeError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| RuntimeError::SpawnError(format!("reqwest client build failed: {e}")))?;

        let resp = client
            .post(url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .body(body.to_owned())
            .send()
            .map_err(|e| {
                if e.is_timeout() {
                    RuntimeError::Timeout
                } else {
                    RuntimeError::SpawnError(format!("HTTP request failed: {e}"))
                }
            })?;

        let status = resp.status();
        let text = resp
            .text()
            .map_err(|e| RuntimeError::SpawnError(format!("response body read failed: {e}")))?;

        if !status.is_success() {
            return Err(RuntimeError::NonZeroExit(format!(
                "OpenRouter returned HTTP {status}: {text}"
            )));
        }

        Ok(text)
    }
}

impl PhaseRuntime for OpenRouterRuntime {
    fn execute(
        &self,
        prompt: &str,
        model: &str,
        timeout: Duration,
    ) -> Result<RuntimeOutput, RuntimeError> {
        let api_key = std::env::var(&self.api_key_env).map_err(|_| {
            RuntimeError::SpawnError(format!(
                "env var {} is not set — cannot call OpenRouter",
                self.api_key_env
            ))
        })?;

        let resolved = resolve_model(model);

        let body = serde_json::json!({
            "model": resolved,
            "messages": [{"role": "user", "content": prompt}]
        })
        .to_string();

        let start = std::time::Instant::now();

        let raw = self.http.post_json(
            "https://openrouter.ai/api/v1/chat/completions",
            &api_key,
            &body,
            timeout,
        )?;

        let duration_ms = start.elapsed().as_millis() as u64;

        let response: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
            RuntimeError::SpawnError(format!("OpenRouter JSON parse failed: {e}: {raw}"))
        })?;

        let text = response["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| {
                RuntimeError::SpawnError(format!(
                    "OpenRouter response missing choices[0].message.content: {raw}"
                ))
            })?
            .to_owned();

        let usage = &response["usage"];
        let input_tokens = usage["prompt_tokens"].as_u64();
        let output_tokens = usage["completion_tokens"].as_u64();
        let cost_usd = usage["cost"].as_f64();

        Ok(RuntimeOutput { text, cost_usd, input_tokens, output_tokens, duration_ms })
    }
}

#[cfg(test)]
mod openrouter {
    use super::*;

    struct MockHttp {
        response: String,
    }

    impl HttpPost for MockHttp {
        fn post_json(
            &self,
            _url: &str,
            _api_key: &str,
            _body: &str,
            _timeout: Duration,
        ) -> Result<String, RuntimeError> {
            Ok(self.response.clone())
        }
    }

    struct ErrHttp(RuntimeError);

    impl HttpPost for ErrHttp {
        fn post_json(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: Duration,
        ) -> Result<String, RuntimeError> {
            // Reconstruct a matching error from the stored discriminant
            match &self.0 {
                RuntimeError::Timeout => Err(RuntimeError::Timeout),
                RuntimeError::NonZeroExit(s) => Err(RuntimeError::NonZeroExit(s.clone())),
                RuntimeError::SpawnError(s) => Err(RuntimeError::SpawnError(s.clone())),
            }
        }
    }

    fn ok_response(content: &str, prompt_tokens: u64, completion_tokens: u64, cost: f64) -> String {
        serde_json::json!({
            "id": "test-id",
            "model": "google/gemini-2.0-flash-001",
            "choices": [{"message": {"role": "assistant", "content": content}}],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens,
                "cost": cost
            }
        })
        .to_string()
    }

    fn rt(http: Box<dyn HttpPost>, env_var: &str, key_val: &str) -> OpenRouterRuntime {
        // SAFETY: tests run single-threaded within this module
        unsafe { std::env::set_var(env_var, key_val) };
        OpenRouterRuntime::with_http(http, env_var)
    }

    #[test]
    fn parses_text_and_usage() {
        let runtime =
            rt(Box::new(MockHttp { response: ok_response("hello", 10, 5, 0.0001) }), "OR_KEY_1", "k");
        let out = runtime.execute("say hi", "gemini-flash", Duration::from_secs(30)).unwrap();
        assert_eq!(out.text, "hello");
        assert_eq!(out.input_tokens, Some(10));
        assert_eq!(out.output_tokens, Some(5));
        assert_eq!(out.cost_usd, Some(0.0001));
        assert!(out.duration_ms < 1000);
    }

    #[test]
    fn missing_api_key_errors_loud() {
        unsafe { std::env::remove_var("OR_KEY_ABSENT_12345") };
        let runtime = OpenRouterRuntime::with_http(
            Box::new(MockHttp { response: "{}".to_string() }),
            "OR_KEY_ABSENT_12345",
        );
        let err = runtime.execute("hi", "gemini-flash", Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, RuntimeError::SpawnError(_)));
        assert!(err.to_string().contains("OR_KEY_ABSENT_12345"));
    }

    #[test]
    fn malformed_json_errors_loud() {
        let runtime = rt(Box::new(MockHttp { response: "not json at all".to_string() }), "OR_KEY_2", "k");
        let err = runtime.execute("hi", "gemini-flash", Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, RuntimeError::SpawnError(_)));
    }

    #[test]
    fn missing_content_field_errors_loud() {
        let bad = serde_json::json!({"choices": [{"message": {}}]}).to_string();
        let runtime = rt(Box::new(MockHttp { response: bad }), "OR_KEY_3", "k");
        let err = runtime.execute("hi", "gemini-flash", Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, RuntimeError::SpawnError(_)));
    }

    #[test]
    fn http_error_propagates() {
        let runtime = rt(
            Box::new(ErrHttp(RuntimeError::NonZeroExit("HTTP 429".to_string()))),
            "OR_KEY_4",
            "k",
        );
        let err = runtime.execute("hi", "gemini-flash", Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, RuntimeError::NonZeroExit(_)));
    }

    #[test]
    fn timeout_propagates() {
        let runtime = rt(Box::new(ErrHttp(RuntimeError::Timeout)), "OR_KEY_5", "k");
        let err = runtime.execute("hi", "gemini-flash", Duration::from_secs(1)).unwrap_err();
        assert!(matches!(err, RuntimeError::Timeout));
    }

    #[test]
    fn usage_fields_optional_when_absent() {
        let body = serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "ok"}}],
            "usage": {}
        })
        .to_string();
        let runtime = rt(Box::new(MockHttp { response: body }), "OR_KEY_6", "k");
        let out = runtime.execute("hi", "gemini-flash", Duration::from_secs(5)).unwrap();
        assert_eq!(out.text, "ok");
        assert!(out.input_tokens.is_none());
        assert!(out.output_tokens.is_none());
        assert!(out.cost_usd.is_none());
    }

    #[test]
    fn model_alias_gemini_flash() {
        assert_eq!(resolve_model("gemini-flash"), "google/gemini-2.0-flash-001");
    }

    #[test]
    fn model_alias_grok() {
        assert_eq!(resolve_model("grok"), "x-ai/grok-beta");
    }

    #[test]
    fn model_alias_qwen_coder() {
        assert_eq!(resolve_model("qwen-coder"), "qwen/qwen-2.5-coder-32b-instruct");
    }

    #[test]
    fn model_alias_haiku() {
        assert_eq!(resolve_model("haiku"), "anthropic/claude-haiku-4-5");
    }

    #[test]
    fn unknown_model_passes_through() {
        assert_eq!(resolve_model("some/custom-model"), "some/custom-model");
    }
}
