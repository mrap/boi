use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FailureReason {
    ModelResolution { model: String, provider: String },
    ProviderRateLimit { provider: String, retry_after_s: Option<u32> },
    ProviderHttp { provider: String, status: u16, body_excerpt: String },
    ProviderAuth { provider: String, env_var: String },
    Timeout { phase: String, secs: u64 },
    ToolError { phase: String, message: String },
    VerifyFailed { task: String, exit_code: i32, stderr_excerpt: String },
    WorkerCrash { phase: String, signal: Option<i32>, message: String },
    Other { message: String },
}

impl FailureReason {
    pub fn short_summary(&self) -> String {
        match self {
            FailureReason::ModelResolution { model, provider } => {
                format!("model '{}' not found via {}", model, provider)
            }
            FailureReason::ProviderRateLimit { provider, retry_after_s } => {
                match retry_after_s {
                    Some(s) => format!("rate limited by {} (retry in {}s)", provider, s),
                    None => format!("rate limited by {}", provider),
                }
            }
            FailureReason::ProviderHttp { provider, status, .. } => {
                format!("HTTP {} from {}", status, provider)
            }
            FailureReason::ProviderAuth { provider, env_var } => {
                format!("auth failed for {} (check {})", provider, env_var)
            }
            FailureReason::Timeout { phase, secs } => {
                format!("timed out in {} after {}s", phase, secs)
            }
            FailureReason::ToolError { phase, message } => {
                let msg = truncate(message, 60);
                format!("tool error in {}: {}", phase, msg)
            }
            FailureReason::VerifyFailed { task, exit_code, .. } => {
                format!("verify failed for '{}' (exit {})", task, exit_code)
            }
            FailureReason::WorkerCrash { phase, signal, .. } => match signal {
                Some(sig) => format!("worker crashed in {} (signal {})", phase, sig),
                None => format!("worker crashed in {}", phase),
            },
            FailureReason::Other { message } => truncate(message, 80).to_string(),
        }
    }

    pub fn detail(&self) -> String {
        match self {
            FailureReason::ModelResolution { model, provider } => {
                format!(
                    "ModelResolution\n  model:    {}\n  provider: {}",
                    model, provider
                )
            }
            FailureReason::ProviderRateLimit { provider, retry_after_s } => {
                let retry = match retry_after_s {
                    Some(s) => format!("{}s", s),
                    None => "unknown".to_string(),
                };
                format!(
                    "ProviderRateLimit\n  provider:    {}\n  retry_after: {}",
                    provider, retry
                )
            }
            FailureReason::ProviderHttp { provider, status, body_excerpt } => {
                format!(
                    "ProviderHttp\n  provider: {}\n  status:   {}\n  body:     {}",
                    provider, status, body_excerpt
                )
            }
            FailureReason::ProviderAuth { provider, env_var } => {
                format!(
                    "ProviderAuth\n  provider: {}\n  env_var:  {}",
                    provider, env_var
                )
            }
            FailureReason::Timeout { phase, secs } => {
                format!("Timeout\n  phase: {}\n  secs:  {}", phase, secs)
            }
            FailureReason::ToolError { phase, message } => {
                format!("ToolError\n  phase:   {}\n  message: {}", phase, message)
            }
            FailureReason::VerifyFailed { task, exit_code, stderr_excerpt } => {
                format!(
                    "VerifyFailed\n  task:      {}\n  exit_code: {}\n  stderr:    {}",
                    task, exit_code, stderr_excerpt
                )
            }
            FailureReason::WorkerCrash { phase, signal, message } => {
                let sig = match signal {
                    Some(s) => s.to_string(),
                    None => "none".to_string(),
                };
                format!(
                    "WorkerCrash\n  phase:   {}\n  signal:  {}\n  message: {}",
                    phase, sig, message
                )
            }
            FailureReason::Other { message } => {
                format!("Other\n  message: {}", message)
            }
        }
    }

    /// Serialize to JSON string for storage in the error column.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            format!("{{\"Other\":{{\"message\":\"serialization failed\"}}}}")
        })
    }

    /// Parse from a DB error column value.
    /// Tries JSON first; falls back to Other { message } for legacy plain strings and NULLs.
    pub fn from_db(text: &str) -> Self {
        let trimmed = text.trim();
        if trimmed.starts_with('{') {
            if let Ok(r) = serde_json::from_str(trimmed) {
                return r;
            }
        }
        FailureReason::Other { message: text.to_string() }
    }
}

/// Infer a typed FailureReason from phase name + free-text reason string.
/// Used at catch sites in the worker where the error is only available as a string.
pub fn infer_failure_reason(phase_name: &str, reason: &str) -> FailureReason {
    let lower = reason.to_lowercase();

    // Timeout
    if lower.contains("timeout") {
        return FailureReason::Timeout {
            phase: phase_name.to_string(),
            secs: 0,
        };
    }

    // HTTP 429 / rate limit
    if lower.contains("429") || lower.contains("rate limit") || lower.contains("too many requests") {
        let provider = if lower.contains("openrouter") { "openrouter" } else { "anthropic" };
        return FailureReason::ProviderRateLimit {
            provider: provider.to_string(),
            retry_after_s: None,
        };
    }

    // HTTP 401 / auth errors
    if lower.contains("401") || lower.contains("unauthorized") || lower.contains("api key") || lower.contains("invalid key") {
        let provider = if lower.contains("openrouter") { "openrouter" } else { "anthropic" };
        let env_var = if lower.contains("openrouter") { "OPENROUTER_API_KEY" } else { "ANTHROPIC_API_KEY" };
        return FailureReason::ProviderAuth {
            provider: provider.to_string(),
            env_var: env_var.to_string(),
        };
    }

    // Other HTTP 4xx/5xx
    if let Some(status) = extract_http_status(reason) {
        let provider = if lower.contains("openrouter") { "openrouter" } else { "anthropic" };
        let excerpt: String = reason.chars().take(300).collect();
        return FailureReason::ProviderHttp {
            provider: provider.to_string(),
            status,
            body_excerpt: excerpt,
        };
    }

    // Worktree / subprocess signal
    if lower.contains("worktree") || lower.contains("sigkill") || lower.contains("sigsegv") {
        return FailureReason::WorkerCrash {
            phase: phase_name.to_string(),
            signal: None,
            message: reason.chars().take(300).collect(),
        };
    }

    // Verify phase
    if phase_name.contains("verify") {
        return FailureReason::VerifyFailed {
            task: phase_name.to_string(),
            exit_code: 1,
            stderr_excerpt: reason.chars().take(300).collect(),
        };
    }

    // Default
    FailureReason::ToolError {
        phase: phase_name.to_string(),
        message: reason.chars().take(300).collect(),
    }
}

/// Extract a 3-digit HTTP status code in range 400–599 from a string.
fn extract_http_status(text: &str) -> Option<u16> {
    let bytes = text.as_bytes();
    for i in 0..bytes.len().saturating_sub(2) {
        if bytes[i].is_ascii_digit() && bytes[i + 1].is_ascii_digit() && bytes[i + 2].is_ascii_digit() {
            let before_ok = i == 0 || !bytes[i - 1].is_ascii_digit();
            let after_ok = i + 3 >= bytes.len() || !bytes[i + 3].is_ascii_digit();
            if before_ok && after_ok {
                let n = (bytes[i] - b'0') as u16 * 100
                    + (bytes[i + 1] - b'0') as u16 * 10
                    + (bytes[i + 2] - b'0') as u16;
                if (400..=599).contains(&n) {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn truncate(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars {
        s
    } else {
        // find a char boundary
        let mut end = max_chars;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

/// Truncate with ellipsis for display (returns owned String).
pub fn truncate_display(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut result: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        result.push('…');
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- failure_reason tests (matched by `cargo test --lib failure_reason`) ---

    #[test]
    fn failure_reason_model_resolution_short_summary() {
        let r = FailureReason::ModelResolution {
            model: "claude-opus-5".to_string(),
            provider: "anthropic".to_string(),
        };
        assert_eq!(r.short_summary(), "model 'claude-opus-5' not found via anthropic");
    }

    #[test]
    fn failure_reason_model_resolution_detail() {
        let r = FailureReason::ModelResolution {
            model: "m".to_string(),
            provider: "p".to_string(),
        };
        let d = r.detail();
        assert!(d.contains("ModelResolution"), "got: {}", d);
        assert!(d.contains("model:"), "got: {}", d);
        assert!(d.contains("provider:"), "got: {}", d);
    }

    #[test]
    fn failure_reason_provider_rate_limit_with_retry() {
        let r = FailureReason::ProviderRateLimit {
            provider: "openai".to_string(),
            retry_after_s: Some(30),
        };
        assert_eq!(r.short_summary(), "rate limited by openai (retry in 30s)");
    }

    #[test]
    fn failure_reason_provider_rate_limit_no_retry() {
        let r = FailureReason::ProviderRateLimit {
            provider: "openai".to_string(),
            retry_after_s: None,
        };
        assert_eq!(r.short_summary(), "rate limited by openai");
    }

    #[test]
    fn failure_reason_provider_http_short_summary() {
        let r = FailureReason::ProviderHttp {
            provider: "anthropic".to_string(),
            status: 500,
            body_excerpt: "internal server error".to_string(),
        };
        assert_eq!(r.short_summary(), "HTTP 500 from anthropic");
    }

    #[test]
    fn failure_reason_provider_http_detail_includes_body() {
        let r = FailureReason::ProviderHttp {
            provider: "anthropic".to_string(),
            status: 429,
            body_excerpt: "quota exceeded".to_string(),
        };
        let d = r.detail();
        assert!(d.contains("quota exceeded"), "got: {}", d);
        assert!(d.contains("429"), "got: {}", d);
    }

    #[test]
    fn failure_reason_provider_auth_short_summary() {
        let r = FailureReason::ProviderAuth {
            provider: "openrouter".to_string(),
            env_var: "OPENROUTER_API_KEY".to_string(),
        };
        assert_eq!(r.short_summary(), "auth failed for openrouter (check OPENROUTER_API_KEY)");
    }

    #[test]
    fn failure_reason_timeout_short_summary() {
        let r = FailureReason::Timeout {
            phase: "execute".to_string(),
            secs: 3600,
        };
        assert_eq!(r.short_summary(), "timed out in execute after 3600s");
    }

    #[test]
    fn failure_reason_tool_error_short_summary() {
        let r = FailureReason::ToolError {
            phase: "verify".to_string(),
            message: "cargo build failed".to_string(),
        };
        let s = r.short_summary();
        assert!(s.contains("tool error in verify"), "got: {}", s);
        assert!(s.contains("cargo build failed"), "got: {}", s);
    }

    #[test]
    fn failure_reason_tool_error_long_message_truncated() {
        let long_msg = "a".repeat(200);
        let r = FailureReason::ToolError {
            phase: "p".to_string(),
            message: long_msg,
        };
        let s = r.short_summary();
        assert!(s.len() < 200, "should be truncated, got len={}", s.len());
    }

    #[test]
    fn failure_reason_verify_failed_short_summary() {
        let r = FailureReason::VerifyFailed {
            task: "T1234".to_string(),
            exit_code: 1,
            stderr_excerpt: "assertion failed".to_string(),
        };
        assert_eq!(r.short_summary(), "verify failed for 'T1234' (exit 1)");
    }

    #[test]
    fn failure_reason_verify_failed_detail_includes_stderr() {
        let r = FailureReason::VerifyFailed {
            task: "T1234".to_string(),
            exit_code: 2,
            stderr_excerpt: "no such file".to_string(),
        };
        let d = r.detail();
        assert!(d.contains("no such file"), "got: {}", d);
        assert!(d.contains("exit_code"), "got: {}", d);
    }

    #[test]
    fn failure_reason_worker_crash_with_signal() {
        let r = FailureReason::WorkerCrash {
            phase: "execute".to_string(),
            signal: Some(9),
            message: "OOM killed".to_string(),
        };
        assert_eq!(r.short_summary(), "worker crashed in execute (signal 9)");
    }

    #[test]
    fn failure_reason_worker_crash_no_signal() {
        let r = FailureReason::WorkerCrash {
            phase: "verify".to_string(),
            signal: None,
            message: "panic".to_string(),
        };
        assert_eq!(r.short_summary(), "worker crashed in verify");
    }

    #[test]
    fn failure_reason_other_short_summary() {
        let r = FailureReason::Other { message: "something went wrong".to_string() };
        assert_eq!(r.short_summary(), "something went wrong");
    }

    #[test]
    fn failure_reason_other_long_message_truncated() {
        let long_msg = "x".repeat(200);
        let r = FailureReason::Other { message: long_msg };
        let s = r.short_summary();
        assert!(s.len() <= 80, "should be truncated to 80 chars, got {}", s.len());
    }

    #[test]
    fn failure_reason_json_roundtrip() {
        let original = FailureReason::ProviderHttp {
            provider: "anthropic".to_string(),
            status: 503,
            body_excerpt: "service unavailable".to_string(),
        };
        let json = original.to_json();
        let parsed = FailureReason::from_db(&json);
        // Verify round-trip via JSON equality
        assert_eq!(original.to_json(), parsed.to_json());
    }

    #[test]
    fn failure_reason_from_db_plain_string_fallback() {
        let r = FailureReason::from_db("some legacy plain error text");
        match r {
            FailureReason::Other { message } => {
                assert_eq!(message, "some legacy plain error text");
            }
            other => panic!("expected Other, got {:?}", other),
        }
    }

    #[test]
    fn failure_reason_from_db_null_like_empty_fallback() {
        let r = FailureReason::from_db("");
        match r {
            FailureReason::Other { .. } => {}
            other => panic!("expected Other, got {:?}", other),
        }
    }

    #[test]
    fn failure_reason_from_db_invalid_json_fallback() {
        let r = FailureReason::from_db("{not valid json}");
        match r {
            FailureReason::Other { .. } => {}
            other => panic!("expected Other, got {:?}", other),
        }
    }

    #[test]
    fn failure_reason_all_variants_roundtrip() {
        let variants: Vec<FailureReason> = vec![
            FailureReason::ModelResolution { model: "m".to_string(), provider: "p".to_string() },
            FailureReason::ProviderRateLimit { provider: "p".to_string(), retry_after_s: Some(60) },
            FailureReason::ProviderRateLimit { provider: "p".to_string(), retry_after_s: None },
            FailureReason::ProviderHttp { provider: "p".to_string(), status: 429, body_excerpt: "b".to_string() },
            FailureReason::ProviderAuth { provider: "p".to_string(), env_var: "K".to_string() },
            FailureReason::Timeout { phase: "execute".to_string(), secs: 600 },
            FailureReason::ToolError { phase: "verify".to_string(), message: "err".to_string() },
            FailureReason::VerifyFailed { task: "T123".to_string(), exit_code: 1, stderr_excerpt: "e".to_string() },
            FailureReason::WorkerCrash { phase: "execute".to_string(), signal: Some(11), message: "segfault".to_string() },
            FailureReason::Other { message: "oops".to_string() },
        ];

        for v in &variants {
            let json = v.to_json();
            let parsed = FailureReason::from_db(&json);
            assert_eq!(json, parsed.to_json(), "round-trip failed for {:?}", v);
        }
    }

    #[test]
    fn failure_reason_truncate_display_short() {
        assert_eq!(truncate_display("hello", 10), "hello");
    }

    #[test]
    fn failure_reason_truncate_display_long() {
        let result = truncate_display("hello world this is long", 10);
        assert!(result.ends_with('…'), "should end with ellipsis: {}", result);
        assert!(result.chars().count() <= 10, "should be at most 10 chars: {}", result);
    }

    // --- failure_capture tests: infer_failure_reason ---

    #[test]
    fn failure_capture_infer_timeout() {
        let r = infer_failure_reason("execute", "timeout");
        assert!(matches!(r, FailureReason::Timeout { .. }), "expected Timeout, got {:?}", r);
    }

    #[test]
    fn failure_capture_infer_openrouter_timeout() {
        let r = infer_failure_reason("execute", "openrouter timeout");
        assert!(matches!(r, FailureReason::Timeout { .. }), "expected Timeout, got {:?}", r);
    }

    #[test]
    fn failure_capture_infer_rate_limit_429() {
        let r = infer_failure_reason("execute", "Phase execute failed: HTTP 429 Too Many Requests");
        assert!(matches!(r, FailureReason::ProviderRateLimit { .. }), "expected ProviderRateLimit, got {:?}", r);
    }

    #[test]
    fn failure_capture_infer_rate_limit_openrouter() {
        let r = infer_failure_reason("execute", "openrouter phase execute failed: 429 rate limit");
        match &r {
            FailureReason::ProviderRateLimit { provider, .. } => {
                assert_eq!(provider, "openrouter");
            }
            _ => panic!("expected ProviderRateLimit, got {:?}", r),
        }
    }

    #[test]
    fn failure_capture_infer_auth_401() {
        let r = infer_failure_reason("execute", "Phase execute failed: HTTP 401 Unauthorized");
        assert!(matches!(r, FailureReason::ProviderAuth { .. }), "expected ProviderAuth, got {:?}", r);
    }

    #[test]
    fn failure_capture_infer_auth_api_key() {
        let r = infer_failure_reason("execute", "invalid api key provided");
        assert!(matches!(r, FailureReason::ProviderAuth { .. }), "expected ProviderAuth, got {:?}", r);
    }

    #[test]
    fn failure_capture_infer_http_500() {
        let r = infer_failure_reason("execute", "Phase execute failed: HTTP 500 Internal Server Error");
        assert!(matches!(r, FailureReason::ProviderHttp { .. }), "expected ProviderHttp, got {:?}", r);
        if let FailureReason::ProviderHttp { status, .. } = r {
            assert_eq!(status, 500);
        }
    }

    #[test]
    fn failure_capture_infer_verify_failed_by_phase_name() {
        let r = infer_failure_reason("task-verify", "requeue limit exceeded");
        assert!(matches!(r, FailureReason::VerifyFailed { .. }), "expected VerifyFailed, got {:?}", r);
    }

    #[test]
    fn failure_capture_infer_verify_failed_phase_verify() {
        let r = infer_failure_reason("verify", "cargo test failed");
        assert!(matches!(r, FailureReason::VerifyFailed { .. }), "expected VerifyFailed, got {:?}", r);
    }

    #[test]
    fn failure_capture_infer_worker_crash_worktree() {
        let r = infer_failure_reason("init", "worktree /tmp/boi-S123 no longer exists");
        assert!(matches!(r, FailureReason::WorkerCrash { .. }), "expected WorkerCrash, got {:?}", r);
    }

    #[test]
    fn failure_capture_infer_default_tool_error() {
        let r = infer_failure_reason("execute", "Phase execute failed: some unknown error");
        assert!(matches!(r, FailureReason::ToolError { .. }), "expected ToolError, got {:?}", r);
    }

    #[test]
    fn failure_capture_infer_tool_error_carries_phase_name() {
        let r = infer_failure_reason("plan-critique", "something broke");
        if let FailureReason::ToolError { phase, .. } = r {
            assert_eq!(phase, "plan-critique");
        } else {
            panic!("expected ToolError, got {:?}", r);
        }
    }

    #[test]
    fn failure_capture_extract_http_status_finds_4xx() {
        assert_eq!(extract_http_status("error: HTTP 503 Service Unavailable"), Some(503));
    }

    #[test]
    fn failure_capture_extract_http_status_ignores_non_http() {
        assert_eq!(extract_http_status("exit code 1 after 200ms"), None);
    }

    #[test]
    fn failure_capture_extract_http_status_rejects_1xx_3xx() {
        assert_eq!(extract_http_status("redirected with 301"), None);
        assert_eq!(extract_http_status("ok 200"), None);
    }
}
