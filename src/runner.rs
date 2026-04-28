use crate::phases::{PhaseConfig, Verdict};
use crate::spec::BoiTask;
use crate::telemetry::{LogLevel, Telemetry};
use crate::worker;
use serde_json::json;
use std::time::Instant;

/// Trait for running a single phase. Allows mocking in tests.
pub trait PhaseRunner: Send + Sync {
    /// Execute a phase and return the outcome.
    ///
    /// - `phase`: The phase configuration
    /// - `spec_content`: Full spec YAML
    /// - `task`: The task being processed (None for spec-level phases)
    /// - `worktree_path`: Working directory for execution
    /// - `timeout_secs`: Max seconds before timeout
    fn run_phase(
        &self,
        phase: &PhaseConfig,
        spec_content: &str,
        task: Option<&BoiTask>,
        worktree_path: &str,
        timeout_secs: u64,
    ) -> Verdict;
}

/// Production phase runner that spawns claude for requires_claude phases
/// and runs verify commands for non-claude phases.
pub struct ClaudePhaseRunner {
    pub telemetry: Telemetry,
}

impl ClaudePhaseRunner {
    pub fn new(telemetry: Telemetry) -> Self {
        ClaudePhaseRunner { telemetry }
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
    ) -> Verdict {
        if !phase.requires_claude {
            // Non-claude phase: run verify command directly
            return self.run_verify_phase(phase, task, worktree_path);
        }

        let task_id = task.map(|t| t.id.as_str());
        let spec_id_hint = "";

        // Build the prompt
        let task_context = task.map(|t| {
            format!(
                "Task: {} — {}\nSpec: {}\nVerify: {}",
                t.id,
                t.title,
                t.spec.as_deref().unwrap_or("(none)"),
                t.verify.as_deref().unwrap_or("(none)")
            )
        });
        let prompt = crate::phases::build_phase_prompt(
            phase,
            spec_content,
            task_context.as_deref(),
        );

        self.telemetry.emit("boi.claude.spawn", LogLevel::Debug, &json!({
            "spec_id": spec_id_hint,
            "task_id": task_id,
            "phase": phase.name,
            "prompt_length": prompt.len(),
            "message": format!("spawning claude for phase '{}'", phase.name),
        }));

        let start = Instant::now();
        let result = worker::spawn_claude(&prompt, worktree_path, timeout_secs);
        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok((true, ref output)) => {
                self.telemetry.emit("boi.claude.exit", LogLevel::Debug, &json!({
                    "spec_id": spec_id_hint,
                    "task_id": task_id,
                    "phase": phase.name,
                    "exit_code": 0,
                    "output_length": output.len(),
                    "duration_ms": duration_ms,
                    "message": format!("claude exit 0, output: {} chars ({}ms)", output.len(), duration_ms),
                }));
                crate::phases::parse_phase_output(phase, output)
            }
            Ok((false, ref output)) => {
                self.telemetry.emit("boi.claude.exit", LogLevel::Debug, &json!({
                    "spec_id": spec_id_hint,
                    "task_id": task_id,
                    "phase": phase.name,
                    "exit_code": 1,
                    "output_length": output.len(),
                    "duration_ms": duration_ms,
                    "message": format!("claude exit non-zero, output: {} chars ({}ms)", output.len(), duration_ms),
                }));
                if output == "timeout" {
                    Verdict::Done {
                        success: false,
                        reason: "timeout".into(),
                    }
                } else if phase.on_crash.as_deref() == Some("retry") {
                    Verdict::Done {
                        success: false,
                        reason: format!("Phase {} claude exited non-zero", phase.name),
                    }
                } else {
                    Verdict::Done {
                        success: false,
                        reason: format!("Phase {} failed: {}", phase.name, output),
                    }
                }
            }
            Err(e) => {
                self.telemetry.emit("boi.claude.error", LogLevel::Error, &json!({
                    "spec_id": spec_id_hint,
                    "task_id": task_id,
                    "phase": phase.name,
                    "duration_ms": duration_ms,
                    "message": format!("claude spawn error: {}", e),
                }));
                Verdict::Done {
                    success: false,
                    reason: format!("Phase {} spawn error: {}", phase.name, e),
                }
            }
        }
    }
}

impl ClaudePhaseRunner {
    fn run_verify_phase(
        &self,
        _phase: &PhaseConfig,
        task: Option<&BoiTask>,
        worktree_path: &str,
    ) -> Verdict {
        let task = match task {
            Some(t) => t,
            None => return Verdict::Proceed,
        };

        let verify_cmd = match task.verify.as_deref() {
            Some(cmd) if !cmd.is_empty() => cmd,
            _ => return Verdict::Proceed,
        };

        self.telemetry.emit("boi.verify.run", LogLevel::Debug, &json!({
            "task_id": task.id,
            "verify_cmd": verify_cmd,
            "message": format!("cmd: {}", verify_cmd),
        }));

        let start = Instant::now();
        let passed = worker::run_verify(verify_cmd, worktree_path);
        let duration_ms = start.elapsed().as_millis() as u64;

        self.telemetry.emit("boi.verify.result", LogLevel::Debug, &json!({
            "task_id": task.id,
            "verify_cmd": verify_cmd,
            "passed": passed,
            "duration_ms": duration_ms,
            "message": format!("exit {} ({})", if passed { "0 (passed)" } else { "non-zero (failed)" }, duration_ms),
        }));

        if passed {
            Verdict::Proceed
        } else {
            Verdict::Redo { tasks: vec![] }
        }
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
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_telemetry() -> Telemetry {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let db = std::path::PathBuf::from(format!(
            "/tmp/boi-test-runner-{}-{}.db",
            std::process::id(), n
        ));
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
            phases: None,
        }
    }

    #[test]
    fn test_claude_runner_verify_phase_success() {
        let runner = ClaudePhaseRunner::new(test_telemetry());
        let phase = make_phase("task-verify", false);
        let task = make_task_with_verify("true");
        let outcome = runner.run_phase(&phase, "", Some(&task), "/tmp", 10);
        assert_eq!(outcome, Verdict::Proceed);
    }

    #[test]
    fn test_claude_runner_verify_phase_failure() {
        let runner = ClaudePhaseRunner::new(test_telemetry());
        let phase = make_phase("task-verify", false);
        let task = make_task_with_verify("false");
        let outcome = runner.run_phase(&phase, "", Some(&task), "/tmp", 10);
        assert_eq!(
            outcome,
            Verdict::Redo { tasks: vec![] }
        );
    }

    #[test]
    fn test_claude_runner_verify_phase_no_verify_cmd() {
        let runner = ClaudePhaseRunner::new(test_telemetry());
        let phase = make_phase("task-verify", false);
        let task = BoiTask {
            id: "t-1".into(),
            title: "No verify".into(),
            status: TaskStatus::Pending,
            depends: None,
            spec: None,
            verify: None,
            phases: None,
        };
        let outcome = runner.run_phase(&phase, "", Some(&task), "/tmp", 10);
        assert_eq!(outcome, Verdict::Proceed);
    }

    #[test]
    fn test_claude_runner_spec_level_no_claude_proceeds() {
        let runner = ClaudePhaseRunner::new(test_telemetry());
        let phase = make_phase("no-op", false);
        // Spec-level phase with no task → proceed (skip is just proceed)
        let outcome = runner.run_phase(&phase, "", None, "/tmp", 10);
        assert_eq!(outcome, Verdict::Proceed);
    }

    #[test]
    fn test_mock_runner_returns_configured_verdicts() {
        let runner = MockPhaseRunner::new(vec![
            Verdict::Proceed,
            Verdict::Done { success: false, reason: "timeout".into() },
            Verdict::Done { success: false, reason: "bad".into() },
        ]);
        let phase = make_phase("test", true);

        assert_eq!(
            runner.run_phase(&phase, "", None, "/tmp", 10),
            Verdict::Proceed
        );
        assert_eq!(
            runner.run_phase(&phase, "", None, "/tmp", 10),
            Verdict::Done { success: false, reason: "timeout".into() }
        );
        assert_eq!(
            runner.run_phase(&phase, "", None, "/tmp", 10),
            Verdict::Done { success: false, reason: "bad".into() }
        );
        // Exhausted verdicts → default to Proceed
        assert_eq!(
            runner.run_phase(&phase, "", None, "/tmp", 10),
            Verdict::Proceed
        );
    }
}
