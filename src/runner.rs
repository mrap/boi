use crate::phases::{PhaseConfig, PhaseOutcome};
use crate::spec::BoiTask;
use crate::worker;

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
    ) -> PhaseOutcome;
}

/// Production phase runner that spawns claude for requires_claude phases
/// and runs verify commands for non-claude phases.
pub struct ClaudePhaseRunner;

impl PhaseRunner for ClaudePhaseRunner {
    fn run_phase(
        &self,
        phase: &PhaseConfig,
        spec_content: &str,
        task: Option<&BoiTask>,
        worktree_path: &str,
        timeout_secs: u64,
    ) -> PhaseOutcome {
        if !phase.requires_claude {
            // Non-claude phase: run verify command directly
            return self.run_verify_phase(phase, task, worktree_path);
        }

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

        // Spawn claude
        match worker::spawn_claude(&prompt, worktree_path, timeout_secs) {
            Ok((true, output)) => {
                crate::phases::parse_phase_output(phase, &output)
            }
            Ok((false, output)) => {
                if output == "timeout" {
                    PhaseOutcome::Timeout
                } else {
                    // Claude exited non-zero — check if on_crash says retry
                    if phase.on_crash.as_deref() == Some("retry") {
                        PhaseOutcome::Failed {
                            reason: format!("Phase {} claude exited non-zero", phase.name),
                        }
                    } else {
                        PhaseOutcome::Failed {
                            reason: format!("Phase {} failed: {}", phase.name, output),
                        }
                    }
                }
            }
            Err(e) => PhaseOutcome::Failed {
                reason: format!("Phase {} spawn error: {}", phase.name, e),
            },
        }
    }
}

impl ClaudePhaseRunner {
    /// Run a non-claude phase (e.g., task-verify). Executes the task's verify command.
    fn run_verify_phase(
        &self,
        _phase: &PhaseConfig,
        task: Option<&BoiTask>,
        worktree_path: &str,
    ) -> PhaseOutcome {
        let task = match task {
            Some(t) => t,
            None => return PhaseOutcome::Skipped, // Spec-level non-claude phase: nothing to run
        };

        let verify_cmd = match task.verify.as_deref() {
            Some(cmd) if !cmd.is_empty() => cmd,
            _ => return PhaseOutcome::Approved, // No verify command = pass
        };

        if worker::run_verify(verify_cmd, worktree_path) {
            PhaseOutcome::Approved
        } else {
            PhaseOutcome::Requeue {
                phase: "execute".to_string(),
            }
        }
    }
}

/// Mock phase runner for testing — returns configurable outcomes.
#[cfg(test)]
pub struct MockPhaseRunner {
    /// Outcomes to return, indexed by call order.
    pub outcomes: std::sync::Mutex<Vec<PhaseOutcome>>,
}

#[cfg(test)]
impl MockPhaseRunner {
    pub fn new(outcomes: Vec<PhaseOutcome>) -> Self {
        MockPhaseRunner {
            outcomes: std::sync::Mutex::new(outcomes),
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
    ) -> PhaseOutcome {
        let mut outcomes = self.outcomes.lock().unwrap();
        if outcomes.is_empty() {
            PhaseOutcome::Approved
        } else {
            outcomes.remove(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phases::{PhaseConfig, PhaseLevel, PhaseOutcome};
    use crate::spec::{BoiTask, TaskStatus};

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
        let runner = ClaudePhaseRunner;
        let phase = make_phase("task-verify", false);
        let task = make_task_with_verify("true");
        let outcome = runner.run_phase(&phase, "", Some(&task), "/tmp", 10);
        assert_eq!(outcome, PhaseOutcome::Approved);
    }

    #[test]
    fn test_claude_runner_verify_phase_failure() {
        let runner = ClaudePhaseRunner;
        let phase = make_phase("task-verify", false);
        let task = make_task_with_verify("false");
        let outcome = runner.run_phase(&phase, "", Some(&task), "/tmp", 10);
        assert_eq!(
            outcome,
            PhaseOutcome::Requeue {
                phase: "execute".into()
            }
        );
    }

    #[test]
    fn test_claude_runner_verify_phase_no_verify_cmd() {
        let runner = ClaudePhaseRunner;
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
        assert_eq!(outcome, PhaseOutcome::Approved);
    }

    #[test]
    fn test_claude_runner_spec_level_no_claude_skips() {
        let runner = ClaudePhaseRunner;
        let phase = make_phase("no-op", false);
        // Spec-level phase with no task → skipped
        let outcome = runner.run_phase(&phase, "", None, "/tmp", 10);
        assert_eq!(outcome, PhaseOutcome::Skipped);
    }

    #[test]
    fn test_mock_runner_returns_configured_outcomes() {
        let runner = MockPhaseRunner::new(vec![
            PhaseOutcome::Approved,
            PhaseOutcome::Timeout,
            PhaseOutcome::Failed { reason: "bad".into() },
        ]);
        let phase = make_phase("test", true);

        assert_eq!(
            runner.run_phase(&phase, "", None, "/tmp", 10),
            PhaseOutcome::Approved
        );
        assert_eq!(
            runner.run_phase(&phase, "", None, "/tmp", 10),
            PhaseOutcome::Timeout
        );
        assert_eq!(
            runner.run_phase(&phase, "", None, "/tmp", 10),
            PhaseOutcome::Failed { reason: "bad".into() }
        );
        // Exhausted outcomes → default to Approved
        assert_eq!(
            runner.run_phase(&phase, "", None, "/tmp", 10),
            PhaseOutcome::Approved
        );
    }
}
