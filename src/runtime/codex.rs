use super::{ProviderConfig, ProviderError, ProviderOutput, SpecProvider};
use crate::spawn::ClaudeResult;
use std::io::ErrorKind;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

macro_rules! boi_log {
    ($($arg:tt)*) => {
        eprintln!("[boi {}] {}", chrono::Utc::now().format("%H:%M:%S"), format!($($arg)*))
    };
}

/// Resolve the API key from config override or OPENAI_API_KEY environment variable.
/// An empty override string is treated as "not set".
pub fn resolve_openai_api_key(config_key: Option<&str>) -> Result<String, ProviderError> {
    match config_key {
        Some(k) if !k.is_empty() => Ok(k.to_string()),
        Some(_) => Err(ProviderError::MissingApiKey(
            "OPENAI_API_KEY override is empty".to_string(),
        )),
        None => std::env::var("OPENAI_API_KEY").map_err(|_| {
            ProviderError::MissingApiKey("OPENAI_API_KEY is not set".to_string())
        }),
    }
}

pub struct CodexProvider;

impl CodexProvider {
    pub fn new() -> Self {
        CodexProvider
    }
}

impl Default for CodexProvider {
    fn default() -> Self {
        CodexProvider
    }
}

impl SpecProvider for CodexProvider {
    /// Execute a prompt via `codex exec`.
    ///
    /// Uses `--output-last-message` to capture the final agent response to a temp file,
    /// which avoids having to parse JSONL streaming events. The prompt is written to
    /// stdin (codex exec reads from stdin when PROMPT is `-`).
    fn execute(&self, prompt: &str, config: &ProviderConfig) -> Result<ProviderOutput, ProviderError> {
        let api_key = resolve_openai_api_key(config.api_key.as_deref())?;

        let codex_bin = config.bin.as_deref().unwrap_or("codex");
        let model = config.model.as_deref().unwrap_or("codex-mini-latest");
        let timeout_secs = config.timeout_secs;

        // Unique temp file for this invocation's output.
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let tmp_output = format!("/tmp/boi-codex-out-{}.txt", ts);

        let args = vec![
            "exec".to_string(),
            "--dangerously-bypass-approvals-and-sandbox".to_string(),
            "--ephemeral".to_string(),
            "-m".to_string(),
            model.to_string(),
            "--output-last-message".to_string(),
            tmp_output.clone(),
            "-".to_string(), // read prompt from stdin
        ];
        boi_log!(
            "spawning codex\n  bin:    {}\n  args:   {}\n  prompt: {} chars",
            codex_bin,
            args.join(" "),
            prompt.len()
        );

        let mut cmd = Command::new(codex_bin);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("OPENAI_API_KEY", &api_key);
        // SAFETY: setsid() is async-signal-safe per POSIX, safe to call after fork.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(ProviderError::BinaryNotFound(format!(
                    "{}: {}",
                    codex_bin, e
                )));
            }
            Err(e) => return Err(ProviderError::ExecutionFailed(e.to_string())),
        };

        let pgid = child.id() as i32;
        let spawn_time = Instant::now();

        // Write prompt to stdin in a separate thread to avoid deadlock.
        let prompt_bytes = prompt.to_string();
        let stdin_pipe = child.stdin.take().expect("stdin was piped");
        let stdin_thread = std::thread::spawn(move || {
            use std::io::Write;
            let mut pipe = stdin_pipe;
            let _ = pipe.write_all(prompt_bytes.as_bytes());
            // pipe drops here, closing stdin and signalling EOF to codex
        });

        // Drain stdout/stderr in threads so the child doesn't block on full pipes.
        let stdout_pipe = child.stdout.take().expect("stdout was piped");
        let stderr_pipe = child.stderr.take().expect("stderr was piped");

        let stdout_thread = std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = String::new();
            let _ = std::io::BufReader::new(stdout_pipe).read_to_string(&mut buf);
            buf
        });
        let stderr_thread = std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = String::new();
            let _ = std::io::BufReader::new(stderr_pipe).read_to_string(&mut buf);
            buf
        });

        // Poll for process exit with timeout.
        let mut timed_out = false;
        let exit_status = loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    unsafe { libc::kill(-pgid, libc::SIGKILL) };
                    break Some(status);
                }
                Ok(None) => {
                    if spawn_time.elapsed().as_secs() >= timeout_secs {
                        let _ = child.kill();
                        let _ = child.wait();
                        unsafe { libc::kill(-pgid, libc::SIGKILL) };
                        timed_out = true;
                        break None;
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
                Err(_) => break None,
            }
        };

        let total_ms = spawn_time.elapsed().as_millis() as u64;
        let _ = stdin_thread.join();
        let stdout = stdout_thread.join().unwrap_or_default();
        let stderr = stderr_thread.join().unwrap_or_default();

        if !stderr.is_empty() {
            boi_log!("codex stderr:\n{}", stderr);
        }

        if timed_out {
            let _ = std::fs::remove_file(&tmp_output);
            return Err(ProviderError::Timeout);
        }

        // Read the output from the temp file written by --output-last-message.
        // Fall back to stdout if the file is missing or empty.
        let output = std::fs::read_to_string(&tmp_output)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| stdout.trim().to_string());
        let _ = std::fs::remove_file(&tmp_output);

        let success = exit_status.map(|s| s.success()).unwrap_or(false);
        boi_log!(
            "codex exit {} — {} chars output",
            if success { "0" } else { "non-zero" },
            output.len()
        );

        Ok(ProviderOutput {
            output,
            success,
            startup_ms: 0,
            inference_ms: total_ms,
            total_ms,
        })
    }
}

/// Spawn codex for use from the runner. Wraps CodexProvider and returns the
/// same ClaudeResult type used by spawn_claude so runner.rs needs minimal changes.
pub fn spawn_codex(
    prompt: &str,
    _worktree_path: &str,
    timeout_secs: u64,
    model: Option<&str>,
    _spec_id: Option<&str>,
    codex_bin: &str,
) -> Result<ClaudeResult, Box<dyn std::error::Error>> {
    let config = ProviderConfig {
        model: model.map(String::from),
        timeout_secs,
        bin: Some(codex_bin.to_string()),
        api_key: None, // read OPENAI_API_KEY from env
    };

    let provider = CodexProvider::new();
    match provider.execute(prompt, &config) {
        Ok(out) => Ok(ClaudeResult {
            success: out.success,
            output: out.output,
            stderr: String::new(),
            startup_ms: out.startup_ms,
            inference_ms: out.inference_ms,
            total_ms: out.total_ms,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            cost_usd: None,
            tool_call_count: 0,
            tool_calls_by_type: std::collections::HashMap::new(),
        }),
        Err(e) => Err(Box::new(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn make_fake_codex(script: &str) -> String {
        let path = format!("/tmp/boi-test-fake-codex-{}", std::process::id());
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn test_codex_cli_not_found() {
        let provider = CodexProvider::new();
        let config = ProviderConfig {
            model: None,
            timeout_secs: 5,
            bin: Some("/nonexistent/codex-binary-that-does-not-exist".to_string()),
            api_key: Some("sk-test-key".to_string()),
        };
        let result = provider.execute("test prompt", &config);
        assert!(
            matches!(result, Err(ProviderError::BinaryNotFound(_))),
            "expected BinaryNotFound, got {:?}",
            result
        );
    }

    #[test]
    fn test_codex_missing_api_key() {
        let provider = CodexProvider::new();
        let config = ProviderConfig {
            model: None,
            timeout_secs: 5,
            bin: Some("codex".to_string()),
            // Empty string signals "no key" per ProviderConfig contract.
            api_key: Some(String::new()),
        };
        let result = provider.execute("test prompt", &config);
        assert!(
            matches!(result, Err(ProviderError::MissingApiKey(_))),
            "expected MissingApiKey, got {:?}",
            result
        );
    }

    #[test]
    fn test_codex_parses_successful_output() {
        // Fake codex binary: reads --output-last-message path and writes known text.
        let script = r#"#!/bin/sh
prev=""
for arg in "$@"; do
  if [ "$prev" = "--output-last-message" ]; then
    printf "Task completed successfully." > "$arg"
    break
  fi
  prev="$arg"
done
exit 0
"#;
        let fake_bin = make_fake_codex(script);

        let provider = CodexProvider::new();
        let config = ProviderConfig {
            model: Some("codex-mini-latest".to_string()),
            timeout_secs: 10,
            bin: Some(fake_bin.clone()),
            api_key: Some("sk-test-key".to_string()),
        };

        let result = provider.execute("do something useful", &config);
        let _ = std::fs::remove_file(&fake_bin);

        let out = result.expect("should succeed");
        assert!(out.success, "expected success=true");
        assert!(
            out.output.contains("Task completed successfully."),
            "unexpected output: {:?}",
            out.output
        );
    }

    #[test]
    fn test_resolve_openai_api_key_from_config() {
        let result = resolve_openai_api_key(Some("sk-explicit-key"));
        assert_eq!(result.unwrap(), "sk-explicit-key");
    }

    #[test]
    fn test_resolve_openai_api_key_empty_is_missing() {
        let result = resolve_openai_api_key(Some(""));
        assert!(matches!(result, Err(ProviderError::MissingApiKey(_))));
    }
}
