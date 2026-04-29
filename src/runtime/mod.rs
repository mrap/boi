pub mod openrouter;

use std::time::Duration;

#[derive(Debug)]
pub enum RuntimeError {
    Timeout,
    /// Process exited non-zero; contains stdout output (may be empty).
    NonZeroExit(String),
    SpawnError(String),
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeError::Timeout => write!(f, "timeout"),
            RuntimeError::NonZeroExit(s) => write!(f, "non-zero exit: {}", s),
            RuntimeError::SpawnError(s) => write!(f, "spawn error: {}", s),
        }
    }
}

impl std::error::Error for RuntimeError {}

#[derive(Debug)]
pub struct RuntimeOutput {
    pub text: String,
    pub cost_usd: Option<f64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub duration_ms: u64,
}

pub trait PhaseRuntime: Send + Sync {
    fn execute(
        &self,
        prompt: &str,
        model: &str,
        timeout: Duration,
    ) -> Result<RuntimeOutput, RuntimeError>;
}

/// Wraps the Claude CLI spawning logic as a PhaseRuntime.
/// `model` is passed to `--model`; empty string means use Claude's default.
pub struct ClaudeCLI {
    pub claude_bin: String,
    pub worktree_path: String,
    pub spec_id: Option<String>,
    pub bare: bool,
}

impl PhaseRuntime for ClaudeCLI {
    fn execute(
        &self,
        prompt: &str,
        model: &str,
        timeout: Duration,
    ) -> Result<RuntimeOutput, RuntimeError> {
        let timeout_secs = timeout.as_secs().max(1);
        let model_opt = if model.is_empty() { None } else { Some(model) };

        let cr = crate::spawn::spawn_claude(
            prompt,
            &self.worktree_path,
            timeout_secs,
            model_opt,
            self.spec_id.as_deref(),
            &self.claude_bin,
            self.bare,
        )
        .map_err(|e| RuntimeError::SpawnError(e.to_string()))?;

        if !cr.success {
            if cr.output == "timeout" {
                return Err(RuntimeError::Timeout);
            }
            return Err(RuntimeError::NonZeroExit(cr.output));
        }

        Ok(RuntimeOutput {
            text: cr.output,
            cost_usd: None,
            input_tokens: None,
            output_tokens: None,
            duration_ms: cr.total_ms,
        })
    }
}

#[cfg(test)]
mod runtime_trait {
    use super::*;

    struct EchoRuntime;

    impl PhaseRuntime for EchoRuntime {
        fn execute(
            &self,
            prompt: &str,
            _model: &str,
            _timeout: Duration,
        ) -> Result<RuntimeOutput, RuntimeError> {
            Ok(RuntimeOutput {
                text: prompt.to_string(),
                cost_usd: None,
                input_tokens: None,
                output_tokens: None,
                duration_ms: 0,
            })
        }
    }

    struct FailRuntime {
        error: fn() -> RuntimeError,
    }

    impl PhaseRuntime for FailRuntime {
        fn execute(
            &self,
            _prompt: &str,
            _model: &str,
            _timeout: Duration,
        ) -> Result<RuntimeOutput, RuntimeError> {
            Err((self.error)())
        }
    }

    #[test]
    fn trait_object_dispatch() {
        let rt: Box<dyn PhaseRuntime> = Box::new(EchoRuntime);
        let out = rt.execute("hello world", "model-x", Duration::from_secs(10)).unwrap();
        assert_eq!(out.text, "hello world");
        assert_eq!(out.duration_ms, 0);
        assert!(out.cost_usd.is_none());
        assert!(out.input_tokens.is_none());
        assert!(out.output_tokens.is_none());
    }

    #[test]
    fn output_fields_accessible() {
        let out = RuntimeOutput {
            text: "response text".to_string(),
            cost_usd: Some(0.001),
            input_tokens: Some(100),
            output_tokens: Some(50),
            duration_ms: 1234,
        };
        assert_eq!(out.text, "response text");
        assert_eq!(out.cost_usd, Some(0.001));
        assert_eq!(out.input_tokens, Some(100));
        assert_eq!(out.output_tokens, Some(50));
        assert_eq!(out.duration_ms, 1234);
    }

    #[test]
    fn timeout_error_display() {
        let err = RuntimeError::Timeout;
        assert_eq!(err.to_string(), "timeout");
    }

    #[test]
    fn non_zero_exit_error_display() {
        let err = RuntimeError::NonZeroExit("bad output".to_string());
        assert!(err.to_string().contains("non-zero exit"));
        assert!(err.to_string().contains("bad output"));
    }

    #[test]
    fn spawn_error_display() {
        let err = RuntimeError::SpawnError("no such binary".to_string());
        assert!(err.to_string().contains("spawn error"));
    }

    #[test]
    fn fail_runtime_returns_timeout() {
        let rt: Box<dyn PhaseRuntime> = Box::new(FailRuntime { error: || RuntimeError::Timeout });
        let err = rt.execute("prompt", "model", Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, RuntimeError::Timeout));
    }

    #[test]
    fn fail_runtime_returns_non_zero_exit() {
        let rt: Box<dyn PhaseRuntime> =
            Box::new(FailRuntime { error: || RuntimeError::NonZeroExit("crash".to_string()) });
        let err = rt.execute("prompt", "model", Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, RuntimeError::NonZeroExit(_)));
    }
}
