use crate::phases::{PhaseConfig, Verdict};
use crate::spec::BoiTask;
use crate::telemetry::{LogLevel, Telemetry};
use crate::worker;
use serde_json::json;
use std::time::Instant;

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

/// Production phase runner that spawns claude for requires_claude phases
/// and runs verify commands for non-claude phases.
pub struct ClaudePhaseRunner {
    pub telemetry: Telemetry,
    pub claude_bin: String,
}

impl ClaudePhaseRunner {
    pub fn new(telemetry: Telemetry, claude_bin: String) -> Self {
        ClaudePhaseRunner {
            telemetry,
            claude_bin,
        }
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

        let result = worker::spawn_claude(
            &prompt,
            worktree_path,
            timeout_secs,
            phase.model.as_deref(),
            spec_id,
            &self.claude_bin,
        );

        if let Ok(ref cr) = result {
            let startup_s = cr.startup_ms as f64 / 1000.0;
            self.telemetry.emit(
                "boi.claude.first_output",
                LogLevel::Debug,
                &json!({
                    "spec_id": spec_id_hint,
                    "task_id": task_id,
                    "startup_ms": cr.startup_ms,
                    "message": format!("first output after {:.1}s (startup)", startup_s),
                }),
            );
        }

        match result {
            Ok(ref cr) if cr.success => {
                let inference_s = cr.inference_ms as f64 / 1000.0;
                let total_s = cr.total_ms as f64 / 1000.0;
                self.telemetry.emit("boi.claude.exit", LogLevel::Debug, &json!({
                    "spec_id": spec_id_hint,
                    "task_id": task_id,
                    "phase": phase.name,
                    "exit_code": 0,
                    "output_length": cr.output.len(),
                    "stderr_length": cr.stderr.len(),
                    "stderr_preview": cr.stderr.chars().take(500).collect::<String>(),
                    "startup_ms": cr.startup_ms,
                    "inference_ms": cr.inference_ms,
                    "total_ms": cr.total_ms,
                    "message": format!("claude exit 0, {} chars ({:.1}s inference, {:.1}s total)",
                        cr.output.len(), inference_s, total_s),
                }));
                let verdict = crate::phases::parse_phase_output(phase, &cr.output);
                (verdict, cr.output.clone())
            }
            Ok(ref cr) => {
                let inference_s = cr.inference_ms as f64 / 1000.0;
                let total_s = cr.total_ms as f64 / 1000.0;
                self.telemetry.emit("boi.claude.exit", LogLevel::Error, &json!({
                    "spec_id": spec_id_hint,
                    "task_id": task_id,
                    "phase": phase.name,
                    "exit_code": 1,
                    "output_length": cr.output.len(),
                    "stderr_length": cr.stderr.len(),
                    "stderr_preview": cr.stderr.chars().take(500).collect::<String>(),
                    "startup_ms": cr.startup_ms,
                    "inference_ms": cr.inference_ms,
                    "total_ms": cr.total_ms,
                    "message": format!("claude exit non-zero, {} chars ({:.1}s inference, {:.1}s total){}",
                        cr.output.len(), inference_s, total_s,
                        if cr.stderr.is_empty() { String::new() } else {
                            format!("\n  stderr: {}", cr.stderr.chars().take(200).collect::<String>())
                        }),
                }));
                let verdict = if cr.output == "timeout" {
                    Verdict::Done { success: false, reason: "timeout".into() }
                } else if phase.on_crash.as_deref() == Some("retry") {
                    Verdict::Done { success: false, reason: format!("Phase {} claude exited non-zero", phase.name) }
                } else {
                    Verdict::Done { success: false, reason: format!("Phase {} failed: {}", phase.name, cr.output) }
                };
                (verdict, cr.output.clone())
            }
            Err(e) => {
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

            let result = worker::spawn_claude(
                verify_prompt,
                worktree_path,
                timeout_secs,
                None,
                spec_id,
                &self.claude_bin,
            );

            match result {
                Ok(ref cr) if cr.success => {
                    self.telemetry.emit(
                        "boi.verify_prompt.result",
                        LogLevel::Debug,
                        &json!({
                            "task_id": task.id,
                            "passed": true,
                            "output_length": cr.output.len(),
                            "startup_ms": cr.startup_ms,
                            "inference_ms": cr.inference_ms,
                            "total_ms": cr.total_ms,
                            "message": format!("verify_prompt passed ({}ms)", cr.total_ms),
                        }),
                    );
                }
                Ok(ref cr) => {
                    self.telemetry.emit(
                        "boi.verify_prompt.result",
                        LogLevel::Debug,
                        &json!({
                            "task_id": task.id,
                            "passed": false,
                            "output_length": cr.output.len(),
                            "startup_ms": cr.startup_ms,
                            "inference_ms": cr.inference_ms,
                            "total_ms": cr.total_ms,
                            "message": format!("verify_prompt failed ({}ms)", cr.total_ms),
                        }),
                    );
                    return Verdict::Redo { tasks: vec![] };
                }
                Err(e) => {
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
