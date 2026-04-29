use crate::builtins::{self, BuiltinContext};
use crate::phases::{PhaseConfig, Verdict};
use crate::runtime::{ClaudeCLI, PhaseRuntime, RuntimeError};
use crate::runtime::openrouter::OpenRouterRuntime;
use crate::spec::BoiTask;
use crate::telemetry::{LogLevel, Telemetry};
use crate::worker;
use serde_json::json;
use std::time::{Duration, Instant};

/// Trait for running a single phase. Allows mocking in tests.
#[allow(clippy::too_many_arguments)]
pub trait PhaseRunner: Send + Sync {
    /// Execute a phase and return the outcome.
    fn run_phase(
        &self,
        phase: &PhaseConfig,
        spec_content: &str,
        task: Option<&BoiTask>,
        worktree_path: &str,
        timeout_secs: u64,
        spec_id: Option<&str>,
        vars: &std::collections::HashMap<String, String>,
    ) -> Verdict;

    /// Execute a phase and return both the verdict and the raw output text.
    /// Default delegates to `run_phase` with empty output; override for full output access.
    fn run_phase_full(
        &self,
        phase: &PhaseConfig,
        spec_content: &str,
        task: Option<&BoiTask>,
        worktree_path: &str,
        timeout_secs: u64,
        spec_id: Option<&str>,
        vars: &std::collections::HashMap<String, String>,
    ) -> (Verdict, String) {
        (self.run_phase(phase, spec_content, task, worktree_path, timeout_secs, spec_id, vars), String::new())
    }
}

/// Production phase runner that spawns claude for requires_claude phases,
/// runs verify commands for non-claude phases, and dispatches deterministic
/// builtin handlers without any Claude cold-start.
pub struct ClaudePhaseRunner {
    pub telemetry: Telemetry,
    pub claude_bin: String,
    /// Source repo path for deterministic builtins that need to merge/cleanup.
    /// Empty string disables merge/cleanup builtins.
    pub repo_path: String,
}

impl ClaudePhaseRunner {
    pub fn new(telemetry: Telemetry, claude_bin: String) -> Self {
        ClaudePhaseRunner {
            telemetry,
            claude_bin,
            repo_path: String::new(),
        }
    }

    pub fn with_repo_path(mut self, repo_path: impl Into<String>) -> Self {
        self.repo_path = repo_path.into();
        self
    }
}

impl ClaudePhaseRunner {
    /// Inner implementation that returns both the verdict and the raw Claude output.
    #[allow(clippy::too_many_arguments)]
    fn run_phase_inner(
        &self,
        phase: &PhaseConfig,
        spec_content: &str,
        task: Option<&BoiTask>,
        worktree_path: &str,
        timeout_secs: u64,
        spec_id: Option<&str>,
        vars: &std::collections::HashMap<String, String>,
    ) -> (Verdict, String) {
        // Deterministic phases: skip Claude entirely, run a registered builtin handler.
        if phase.runtime.as_deref() == Some("deterministic") {
            return (self.run_deterministic_phase(phase, task, spec_id), String::new());
        }

        // OpenRouter phases: send prompt via HTTP unless BOI_FORCE_CLAUDE=1 is set.
        if phase.runtime.as_deref() == Some("openrouter")
            && std::env::var("BOI_FORCE_CLAUDE").as_deref() != Ok("1")
        {
            return self.run_openrouter_phase(phase, spec_content, task, timeout_secs, spec_id, vars);
        }

        if !phase.requires_claude {
            return (self.run_verify_phase(phase, task, worktree_path, timeout_secs, spec_id), String::new());
        }

        let task_id = task.map(|t| t.id.as_str());
        let spec_id_hint = spec_id.unwrap_or("");

        let task_context = task.map(|t| {
            format!(
                "Task: {} — {}\nSpec: {}\nVerify: {}",
                t.id,
                t.title,
                t.spec.as_deref().unwrap_or("(none)"),
                t.verify.as_deref().unwrap_or("(none)")
            )
        });
        let prompt =
            crate::phases::build_phase_prompt(phase, spec_content, task_context.as_deref(), vars);

        self.telemetry.emit(
            "boi.claude.spawn",
            LogLevel::Debug,
            &json!({
                "spec_id": spec_id_hint,
                "task_id": task_id,
                "phase": phase.name,
                "prompt_length": prompt.len(),
                "message": "spawning claude...",
            }),
        );

        let model_str = phase.model.as_deref().unwrap_or("");
        let rt = ClaudeCLI {
            claude_bin: self.claude_bin.clone(),
            worktree_path: worktree_path.to_string(),
            spec_id: spec_id.map(|s| s.to_string()),
            bare: phase.bare,
        };
        let result = rt.execute(&prompt, model_str, Duration::from_secs(timeout_secs));

        match result {
            Ok(ro) => {
                let total_s = ro.duration_ms as f64 / 1000.0;
                self.telemetry.emit("boi.claude.exit", LogLevel::Debug, &json!({
                    "spec_id": spec_id_hint,
                    "task_id": task_id,
                    "phase": phase.name,
                    "exit_code": 0,
                    "output_length": ro.text.len(),
                    "total_ms": ro.duration_ms,
                    "message": format!("claude exit 0, {} chars ({:.1}s total)", ro.text.len(), total_s),
                }));
                let verdict = crate::phases::parse_phase_output(phase, &ro.text);
                (verdict, ro.text)
            }
            Err(RuntimeError::Timeout) => {
                self.telemetry.emit("boi.claude.exit", LogLevel::Error, &json!({
                    "spec_id": spec_id_hint,
                    "task_id": task_id,
                    "phase": phase.name,
                    "exit_code": 1,
                    "message": "claude timeout",
                }));
                (Verdict::Done { success: false, reason: "timeout".into() }, "timeout".to_string())
            }
            Err(RuntimeError::NonZeroExit(output)) => {
                self.telemetry.emit("boi.claude.exit", LogLevel::Error, &json!({
                    "spec_id": spec_id_hint,
                    "task_id": task_id,
                    "phase": phase.name,
                    "exit_code": 1,
                    "output_length": output.len(),
                    "message": format!("claude exit non-zero, {} chars", output.len()),
                }));
                let verdict = if phase.on_crash.as_deref() == Some("retry") {
                    Verdict::Done { success: false, reason: format!("Phase {} claude exited non-zero", phase.name) }
                } else {
                    Verdict::Done { success: false, reason: format!("Phase {} failed: {}", phase.name, output) }
                };
                (verdict, output)
            }
            Err(RuntimeError::SpawnError(e)) => {
                self.telemetry.emit(
                    "boi.claude.error",
                    LogLevel::Error,
                    &json!({
                        "spec_id": spec_id_hint,
                        "task_id": task_id,
                        "phase": phase.name,
                        "message": format!("claude spawn error: {}", e),
                    }),
                );
                (Verdict::Done { success: false, reason: format!("Phase {} spawn error: {}", phase.name, e) }, String::new())
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_openrouter_phase(
        &self,
        phase: &PhaseConfig,
        spec_content: &str,
        task: Option<&BoiTask>,
        timeout_secs: u64,
        spec_id: Option<&str>,
        vars: &std::collections::HashMap<String, String>,
    ) -> (Verdict, String) {
        let task_context = task.map(|t| {
            format!(
                "Task: {} — {}\nSpec: {}\nVerify: {}",
                t.id,
                t.title,
                t.spec.as_deref().unwrap_or("(none)"),
                t.verify.as_deref().unwrap_or("(none)")
            )
        });
        let prompt =
            crate::phases::build_phase_prompt(phase, spec_content, task_context.as_deref(), vars);

        let model = phase.model.as_deref().unwrap_or("gemini-flash");
        let api_key_env = phase.api_key_env.as_deref().unwrap_or("OPENROUTER_API_KEY");
        let mut rt = OpenRouterRuntime::new();
        rt.api_key_env = api_key_env.to_string();

        let spec_id_hint = spec_id.unwrap_or("");
        let task_id = task.map(|t| t.id.as_str());

        self.telemetry.emit(
            "boi.openrouter.spawn",
            crate::telemetry::LogLevel::Debug,
            &json!({
                "spec_id": spec_id_hint,
                "task_id": task_id,
                "phase": phase.name,
                "model": model,
                "message": "sending prompt to openrouter...",
            }),
        );

        let result = rt.execute(&prompt, model, Duration::from_secs(timeout_secs));

        match result {
            Ok(ro) => {
                let total_s = ro.duration_ms as f64 / 1000.0;
                self.telemetry.emit("boi.openrouter.exit", crate::telemetry::LogLevel::Debug, &json!({
                    "spec_id": spec_id_hint,
                    "task_id": task_id,
                    "phase": phase.name,
                    "model": model,
                    "output_length": ro.text.len(),
                    "total_ms": ro.duration_ms,
                    "cost_usd": ro.cost_usd,
                    "message": format!("openrouter ok, {} chars ({:.1}s)", ro.text.len(), total_s),
                }));
                let verdict = crate::phases::parse_phase_output(phase, &ro.text);
                (verdict, ro.text)
            }
            Err(RuntimeError::Timeout) => {
                self.telemetry.emit("boi.openrouter.exit", crate::telemetry::LogLevel::Error, &json!({
                    "spec_id": spec_id_hint,
                    "task_id": task_id,
                    "phase": phase.name,
                    "message": "openrouter timeout",
                }));
                (Verdict::Done { success: false, reason: "openrouter timeout".into() }, "timeout".to_string())
            }
            Err(RuntimeError::NonZeroExit(output)) => {
                self.telemetry.emit("boi.openrouter.exit", crate::telemetry::LogLevel::Error, &json!({
                    "spec_id": spec_id_hint,
                    "task_id": task_id,
                    "phase": phase.name,
                    "message": format!("openrouter error: {}", output),
                }));
                (Verdict::Done { success: false, reason: format!("openrouter phase {} failed: {}", phase.name, output) }, output)
            }
            Err(RuntimeError::SpawnError(e)) => {
                self.telemetry.emit("boi.openrouter.error", crate::telemetry::LogLevel::Error, &json!({
                    "spec_id": spec_id_hint,
                    "task_id": task_id,
                    "phase": phase.name,
                    "message": format!("openrouter error: {}", e),
                }));
                (Verdict::Done { success: false, reason: format!("openrouter phase {} error: {}", phase.name, e) }, String::new())
            }
        }
    }
}

impl PhaseRunner for ClaudePhaseRunner {
    fn run_phase(
        &self,
        phase: &PhaseConfig,
        spec_content: &str,
        task: Option<&BoiTask>,
        worktree_path: &str,
        timeout_secs: u64,
        spec_id: Option<&str>,
        vars: &std::collections::HashMap<String, String>,
    ) -> Verdict {
        self.run_phase_inner(phase, spec_content, task, worktree_path, timeout_secs, spec_id, vars).0
    }

    fn run_phase_full(
        &self,
        phase: &PhaseConfig,
        spec_content: &str,
        task: Option<&BoiTask>,
        worktree_path: &str,
        timeout_secs: u64,
        spec_id: Option<&str>,
        vars: &std::collections::HashMap<String, String>,
    ) -> (Verdict, String) {
        self.run_phase_inner(phase, spec_content, task, worktree_path, timeout_secs, spec_id, vars)
    }
}

impl ClaudePhaseRunner {
    fn run_deterministic_phase(
        &self,
        phase: &PhaseConfig,
        task: Option<&BoiTask>,
        spec_id: Option<&str>,
    ) -> Verdict {
        let handler = match phase.completion_handler.as_deref() {
            Some(h) => h,
            None => {
                self.telemetry.emit(
                    "boi.builtin.error",
                    LogLevel::Error,
                    &json!({
                        "phase": phase.name,
                        "message": "deterministic phase has no completion_handler",
                    }),
                );
                return Verdict::Done {
                    success: false,
                    reason: format!("phase '{}' is deterministic but has no completion_handler", phase.name),
                };
            }
        };

        let sid = spec_id.unwrap_or("");
        let task_title = task.map(|t| t.title.as_str()).unwrap_or("");

        let ctx = BuiltinContext {
            spec_id: sid,
            task_title,
            repo_path: &self.repo_path,
        };

        self.telemetry.emit(
            "boi.builtin.run",
            LogLevel::Debug,
            &json!({
                "phase": phase.name,
                "handler": handler,
                "spec_id": sid,
                "message": format!("running builtin {}", handler),
            }),
        );

        let result = builtins::run_builtin(handler, &ctx);

        self.telemetry.emit(
            "boi.builtin.result",
            LogLevel::Debug,
            &json!({
                "phase": phase.name,
                "handler": handler,
                "spec_id": sid,
                "result": format!("{:?}", result),
            }),
        );

        result.to_verdict()
    }

    fn run_verify_phase(
        &self,
        _phase: &PhaseConfig,
        task: Option<&BoiTask>,
        worktree_path: &str,
        timeout_secs: u64,
        spec_id: Option<&str>,
    ) -> Verdict {
        let task = match task {
            Some(t) => t,
            None => return Verdict::Proceed,
        };

        let has_verify = task.verify.as_deref().is_some_and(|c| !c.is_empty());
        let has_verify_prompt = task.verify_prompt.as_deref().is_some_and(|p| !p.is_empty());

        if !has_verify && !has_verify_prompt {
            return Verdict::Proceed;
        }

        // Shell verify
        if has_verify {
            let verify_cmd = task
                .verify
                .as_deref()
                .expect("has_verify guard ensures verify is Some");

            self.telemetry.emit(
                "boi.verify.run",
                LogLevel::Debug,
                &json!({
                    "task_id": task.id,
                    "verify_cmd": verify_cmd,
                    "message": format!("cmd: {}", verify_cmd),
                }),
            );

            let start = Instant::now();
            let passed = worker::run_verify(verify_cmd, worktree_path);
            let duration_ms = start.elapsed().as_millis() as u64;

            self.telemetry.emit("boi.verify.result", LogLevel::Debug, &json!({
                "task_id": task.id,
                "verify_cmd": verify_cmd,
                "passed": passed,
                "duration_ms": duration_ms,
                "message": format!("exit {} ({}ms)", if passed { "0 (passed)" } else { "non-zero (failed)" }, duration_ms),
            }));

            if !passed {
                return Verdict::Redo { tasks: vec![] };
            }
        }

        // Claude verify_prompt (only reached if shell verify passed or wasn't set)
        if has_verify_prompt {
            let verify_prompt = task
                .verify_prompt
                .as_deref()
                .expect("has_verify_prompt guard ensures verify_prompt is Some");

            self.telemetry.emit("boi.verify_prompt.run", LogLevel::Debug, &json!({
                "task_id": task.id,
                "verify_prompt_length": verify_prompt.len(),
                "message": format!("verify_prompt: spawning claude ({} chars)", verify_prompt.len()),
            }));

            let rt = ClaudeCLI {
                claude_bin: self.claude_bin.clone(),
                worktree_path: worktree_path.to_string(),
                spec_id: spec_id.map(|s| s.to_string()),
                bare: false,
            };
            let result = rt.execute(verify_prompt, "", Duration::from_secs(timeout_secs));

            match result {
                Ok(ro) => {
                    self.telemetry.emit(
                        "boi.verify_prompt.result",
                        LogLevel::Debug,
                        &json!({
                            "task_id": task.id,
                            "passed": true,
                            "output_length": ro.text.len(),
                            "total_ms": ro.duration_ms,
                            "message": format!("verify_prompt passed ({}ms)", ro.duration_ms),
                        }),
                    );
                }
                Err(RuntimeError::Timeout) => {
                    self.telemetry.emit(
                        "boi.verify_prompt.result",
                        LogLevel::Debug,
                        &json!({
                            "task_id": task.id,
                            "passed": false,
                            "message": "verify_prompt timeout",
                        }),
                    );
                    return Verdict::Redo { tasks: vec![] };
                }
                Err(RuntimeError::NonZeroExit(output)) => {
                    self.telemetry.emit(
                        "boi.verify_prompt.result",
                        LogLevel::Debug,
                        &json!({
                            "task_id": task.id,
                            "passed": false,
                            "output_length": output.len(),
                            "message": format!("verify_prompt failed ({} chars)", output.len()),
                        }),
                    );
                    return Verdict::Redo { tasks: vec![] };
                }
                Err(RuntimeError::SpawnError(e)) => {
                    self.telemetry.emit(
                        "boi.verify_prompt.error",
                        LogLevel::Error,
                        &json!({
                            "task_id": task.id,
                            "message": format!("verify_prompt spawn error: {}", e),
                        }),
                    );
                    return Verdict::Redo { tasks: vec![] };
                }
            }
        }

        Verdict::Proceed
    }
}

/// Mock phase runner for testing — returns configurable verdicts.
#[cfg(test)]
pub struct MockPhaseRunner {
    /// Verdicts to return, indexed by call order.
    pub verdicts: std::sync::Mutex<Vec<Verdict>>,
}

#[cfg(test)]
impl MockPhaseRunner {
    pub fn new(verdicts: Vec<Verdict>) -> Self {
        MockPhaseRunner {
            verdicts: std::sync::Mutex::new(verdicts),
        }
    }
}

#[cfg(test)]
impl PhaseRunner for MockPhaseRunner {
    fn run_phase(
        &self,
        _phase: &PhaseConfig,
        _spec_content: &str,
        _task: Option<&BoiTask>,
        _worktree_path: &str,
        _timeout_secs: u64,
        _spec_id: Option<&str>,
        _vars: &std::collections::HashMap<String, String>,
    ) -> Verdict {
        let mut verdicts = self.verdicts.lock().unwrap();
        if verdicts.is_empty() {
            Verdict::Proceed
        } else {
            verdicts.remove(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phases::{PhaseConfig, PhaseLevel, Verdict};
    use crate::spec::{BoiTask, TaskStatus};
    use crate::test_utils;

    fn test_telemetry() -> Telemetry {
        let db = test_utils::test_file("runner-tel", "db");
        let _ = std::fs::remove_file(&db);
        Telemetry::new(db)
    }

    fn make_phase(name: &str, requires_claude: bool) -> PhaseConfig {
        PhaseConfig {
            name: name.into(),
            level: PhaseLevel::Task,
            description: "test".into(),
            prompt_template: "Do the thing.".into(),
            timeout_minutes: Some(5),
            retry_count: None,
            can_add_tasks: false,
            can_fail_spec: false,
            requires_claude,
            runtime: None,
            api_key_env: None,
            completion_handler: None,
            approve_signal: Some("## Approved".into()),
            reject_signal: Some("[REJECT]".into()),
            on_approve: Some("next".into()),
            on_reject: Some("requeue:execute".into()),
            on_crash: None,
            min_lines_changed: None,
            model: None,
            code_model: None,
            effort: None,
            hooks_pre: vec![],
            hooks_post: vec![],
            bare: false,
        }
    }

    fn make_task_with_verify(verify: &str) -> BoiTask {
        BoiTask {
            id: "t-1".into(),
            title: "Test task".into(),
            status: TaskStatus::Pending,
            depends: None,
            spec: Some("Do something".into()),
            verify: Some(verify.into()),
            verify_prompt: None,
            phases: None,
        }
    }

    #[test]
    fn test_claude_runner_verify_phase_success() {
        let runner = ClaudePhaseRunner::new(test_telemetry(), "claude".to_string());
        let phase = make_phase("task-verify", false);
        let task = make_task_with_verify("true");
        let outcome = runner.run_phase(
            &phase,
            "",
            Some(&task),
            "/tmp",
            10,
            None,
            &std::collections::HashMap::new(),
        );
        assert_eq!(outcome, Verdict::Proceed);
    }

    #[test]
    fn test_claude_runner_verify_phase_failure() {
        let runner = ClaudePhaseRunner::new(test_telemetry(), "claude".to_string());
        let phase = make_phase("task-verify", false);
        let task = make_task_with_verify("false");
        let outcome = runner.run_phase(
            &phase,
            "",
            Some(&task),
            "/tmp",
            10,
            None,
            &std::collections::HashMap::new(),
        );
        assert_eq!(outcome, Verdict::Redo { tasks: vec![] });
    }

    #[test]
    fn test_claude_runner_verify_phase_no_verify_cmd() {
        let runner = ClaudePhaseRunner::new(test_telemetry(), "claude".to_string());
        let phase = make_phase("task-verify", false);
        let task = BoiTask {
            id: "t-1".into(),
            title: "No verify".into(),
            status: TaskStatus::Pending,
            depends: None,
            spec: None,
            verify: None,
            verify_prompt: None,
            phases: None,
        };
        let outcome = runner.run_phase(
            &phase,
            "",
            Some(&task),
            "/tmp",
            10,
            None,
            &std::collections::HashMap::new(),
        );
        assert_eq!(outcome, Verdict::Proceed);
    }

    #[test]
    fn test_claude_runner_spec_level_no_claude_proceeds() {
        let runner = ClaudePhaseRunner::new(test_telemetry(), "claude".to_string());
        let phase = make_phase("no-op", false);
        // Spec-level phase with no task → proceed (skip is just proceed)
        let outcome = runner.run_phase(
            &phase,
            "",
            None,
            "/tmp",
            10,
            None,
            &std::collections::HashMap::new(),
        );
        assert_eq!(outcome, Verdict::Proceed);
    }

    #[test]
    fn test_mock_runner_returns_configured_verdicts() {
        let runner = MockPhaseRunner::new(vec![
            Verdict::Proceed,
            Verdict::Done {
                success: false,
                reason: "timeout".into(),
            },
            Verdict::Done {
                success: false,
                reason: "bad".into(),
            },
        ]);
        let phase = make_phase("test", true);

        assert_eq!(
            runner.run_phase(
                &phase,
                "",
                None,
                "/tmp",
                10,
                None,
                &std::collections::HashMap::new()
            ),
            Verdict::Proceed
        );
        assert_eq!(
            runner.run_phase(
                &phase,
                "",
                None,
                "/tmp",
                10,
                None,
                &std::collections::HashMap::new()
            ),
            Verdict::Done {
                success: false,
                reason: "timeout".into()
            }
        );
        assert_eq!(
            runner.run_phase(
                &phase,
                "",
                None,
                "/tmp",
                10,
                None,
                &std::collections::HashMap::new()
            ),
            Verdict::Done {
                success: false,
                reason: "bad".into()
            }
        );
        // Exhausted verdicts → default to Proceed
        assert_eq!(
            runner.run_phase(
                &phase,
                "",
                None,
                "/tmp",
                10,
                None,
                &std::collections::HashMap::new()
            ),
            Verdict::Proceed
        );
    }
}
