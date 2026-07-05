//! Verification-command execution — runs `[contract].verifications` commands
//! as subprocesses. Backs both the `validate` deterministic phase and
//! `WorkerToolHost::run_verification` (Task 6.6).
//!
//! ## Subprocess lifecycle (Phase 6 preamble — review C2 / C-rt-2)
//!
//! [`run_command`] builds the `tokio::process::Command` with
//! `kill_on_drop(true)` as the *panic-path backstop*. The *intentional-stop
//! mechanism* on a fired `cancel` is explicit `start_kill()` + a drained
//! `wait()` — never a bare `kill().await` while stdout is undrained (a full
//! pipe deadlocks the child). stdout AND stderr are drained concurrently.
//! Every child is reaped.
//!
//! The entire post-kill sequence — both drain joins AND `child.wait()` — runs
//! inside ONE `tokio::time::timeout(CANCEL_GRACE, …)`, and the drain
//! `JoinHandle`s are `abort()`ed on overrun (review C-rt-2). A killed command
//! whose grandchild inherited a stdout/stderr fd holds that pipe's write end
//! open, so a drain awaiting EOF would block forever; bounding the join inside
//! the timeout — not after it — keeps the cancel path genuinely bounded.
//!
//! ## `verify_spec` fallback (review S4)
//!
//! A contract with no `verifications` is not a free pass: [`validate`] calls
//! `config::verify_spec::detect_toolchain` and synthesizes three
//! `Verification::Command`s from the detected toolchain. A workspace with no
//! detectable toolchain → a loud `StepOutcome::Fail` ("no verifications, no
//! detectable toolchain"), never a trivial pass.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::config::verify_spec::{self, DetectedToolchain};
use crate::runtime::deterministic::StepRun;
use crate::types::context::Verification;
use crate::types::event::BoiEvent;
use crate::types::ids::{SpecId, TaskId};
use crate::types::reasons::ErrorWhyFix;
use crate::types::step::{StepCtx, StepError, StepOutcome};
use crate::types::verdict::{Evidence, VerificationEvidence, VerifyLevel};

/// How long a canceled command is given to die before its `Child` is dropped
/// (the `kill_on_drop` backstop then reaps it).
const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// The captured result of running one verification command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// The command's exit code. A command killed by a signal (no exit code) is
    /// reported as `-1` — a non-zero "failed" value.
    pub exit_code: i32,
    /// The command's captured stdout.
    pub stdout: String,
    /// The command's captured stderr.
    pub stderr: String,
}

/// A verification command could not be run.
///
/// A *non-zero exit* is NOT a `ValidateError` — it is a successful run with a
/// failing result, carried in [`CommandOutput::exit_code`]. `ValidateError` is
/// for the cases where the command never produced an exit code at all: a spawn
/// failure, or a cancel before completion.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidateError {
    /// The subprocess could not be spawned (bad worktree path, missing shell).
    #[error("spawn failed: {0}")]
    Spawn(String),
    /// The command was canceled before it produced an exit code.
    #[error("canceled before completion")]
    Canceled,
    /// Draining the child's stdout/stderr failed.
    #[error("output capture failed: {0}")]
    Capture(String),
}

impl From<ValidateError> for StepError {
    fn from(e: ValidateError) -> Self {
        StepError::Validate(e.to_string())
    }
}

/// Run one verification command in `worktree`.
///
/// The command is run through `sh -c` so shell syntax (pipes, `&&`, env) works.
/// Builds the `Command` with `kill_on_drop(true)`; drains stdout AND stderr
/// concurrently; on a fired `cancel` issues `start_kill()` then a drained
/// `wait()` under a 5 s grace timeout. Captures output; never panics on
/// a non-zero exit (that is a normal [`CommandOutput`]).
///
/// The child gets the shared, warm `CARGO_TARGET_DIR`
/// ([`resolve_cargo_target_dir`](crate::runtime::worktree::resolve_cargo_target_dir),
/// OBS-032) — the same injection as the `goose` worker spawn path — so a
/// `cargo build` verification in a freshly-created worktree reuses the warm
/// artifact cache instead of compiling ~1148 crates cold. A pre-existing
/// `CARGO_TARGET_DIR` in the daemon env wins (the resolver returns it
/// verbatim), and a command may still override it inside its own shell string.
pub async fn run_command(
    worktree: &Path,
    command: &str,
    cancel: &CancellationToken,
) -> Result<CommandOutput, ValidateError> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(worktree)
        // Perf (OBS-032): the shared, warm cargo target dir — the goose.rs
        // spawn path's twin. Without it the daemon env (no CARGO_TARGET_DIR)
        // is inherited and spec-level `cargo build --release` gates in fresh
        // integration worktrees build cold (35–200 min observed).
        .env(
            "CARGO_TARGET_DIR",
            crate::runtime::worktree::resolve_cargo_target_dir(),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Panic-path backstop: if this future is dropped (a panic unwinds past
        // it), the child is killed rather than orphaned.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| ValidateError::Spawn(format!("running `{command}` in {}: {e}", worktree.display())))?;

    // Take the pipe handles — both must be drained concurrently; an undrained
    // pipe fills its buffer and deadlocks the child (review C2).
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ValidateError::Capture("child stdout pipe missing".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ValidateError::Capture("child stderr pipe missing".to_owned()))?;
    let mut out_reader = drain(stdout);
    let mut err_reader = drain(stderr);

    tokio::select! {
        // The command finished on its own — join the two drain tasks.
        status = child.wait() => {
            let status = status.map_err(|e| ValidateError::Capture(format!("wait failed: {e}")))?;
            let stdout = (&mut out_reader).await.map_err(capture_join)??;
            let stderr = (&mut err_reader).await.map_err(capture_join)??;
            Ok(CommandOutput {
                exit_code: status.code().unwrap_or(-1),
                stdout,
                stderr,
            })
        }
        // The caller canceled — kill, then drain + reap, the WHOLE sequence
        // bounded by one grace window (review C-rt-2).
        () = cancel.cancelled() => {
            // Intentional-stop mechanism: signal, then a drained wait under a
            // grace timeout. NOT a bare `kill().await` with stdout undrained.
            // A `start_kill` error means the child already exited — benign,
            // but logged rather than silently dropped (no-quiet-failures).
            if let Err(e) = child.start_kill() {
                tracing::debug!(command, error = %e, "start_kill on an already-exited child");
            }
            // Bound the ENTIRE post-kill sequence — both stdout/stderr drains
            // AND `child.wait()` — in ONE grace window. The old code joined the
            // two drain `JoinHandle`s SEQUENTIALLY and UNBOUNDED *before* the
            // `timeout(child.wait())`: a killed command whose grandchild
            // inherited the stdout/stderr fd keeps that pipe's write end open,
            // so the drain never reaches EOF and the join blocked forever —
            // before the timeout could ever fire (review C-rt-2). Wrapping the
            // join inside the timeout, and aborting the drain tasks on overrun,
            // makes the cancel path genuinely bounded.
            let drained = tokio::time::timeout(CANCEL_GRACE, async {
                // The drained text is discarded — the run is being aborted —
                // but the `JoinHandle`s are consumed (`drop` is the must_use
                // consumer the lint requires).
                drop((&mut out_reader).await);
                drop((&mut err_reader).await);
                drop(child.wait().await);
            })
            .await;
            if drained.is_err() {
                // The grace window elapsed — a grandchild is still holding a
                // pipe (or the child has not died). Abort the drain tasks so
                // they cannot leak past this scope, and let `kill_on_drop`
                // reap the child as `child` drops at the end of this scope.
                out_reader.abort();
                err_reader.abort();
                tracing::warn!(
                    command,
                    "verification command did not drain + die within the cancel \
                     grace window — a grandchild likely still holds a pipe; \
                     drain tasks aborted",
                );
            }
            Err(ValidateError::Canceled)
        }
    }
}

/// Spawn a task that reads a child pipe to end-of-stream into a `String`.
fn drain<R>(pipe: R) -> tokio::task::JoinHandle<Result<String, ValidateError>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = BufReader::new(pipe);
        let mut buf = Vec::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = reader
                .read_until(b'\n', &mut line)
                .await
                .map_err(|e| ValidateError::Capture(format!("read failed: {e}")))?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&line);
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    })
}

/// Map a `JoinError` from a drain task into a `ValidateError`.
fn capture_join(e: tokio::task::JoinError) -> ValidateError {
    ValidateError::Capture(format!("output-drain task panicked: {e}"))
}

/// `validate` — the deterministic verification phase (canonical `DetStep`
/// shape, Task 6.2). Runs every `Verification::Command` in the contract,
/// emitting one `VerifyChecked` per command, and returns a terminal
/// `StepOutcome` plus those events inside [`StepRun`].
///
/// - All-pass → `StepOutcome::Pass` with a `VerificationEvidence` list.
/// - Any non-zero exit → `StepOutcome::Fail` (failing command + stderr tail)
///   AND an `ErrorEncountered` event in `StepRun.events`.
/// - `Verification::Intent` entries need worker judgment — `validate` skips
///   them (worker `review` phases check intents).
/// - No `verifications` → a task-level validate runs the `verify_spec`
///   fallback (detect the workspace toolchain, synthesize three
///   `Verification::Command`s; an undetectable workspace is a loud `Fail`); a
///   spec-level validate passes — the per-task validates already gated the work.
pub fn validate(ctx: Arc<StepCtx>) -> BoxFuture<'static, Result<StepRun, StepError>> {
    Box::pin(async move { validate_inner(ctx, &CancellationToken::new()).await })
}

/// The cancellable body of [`validate`].
///
/// [`validate`] (the `DetStep`-shaped entry point) cannot take a
/// `CancellationToken` — the `DetStep` signature is fixed. The Task 6.5
/// executor that owns the token calls [`validate_inner`] directly so a fired
/// cancel reaches `run_command` between commands (review S10 — `validate`
/// checks `cancel` *between* commands; a `run_command` already in flight is
/// itself cancellable).
pub async fn validate_inner(
    ctx: Arc<StepCtx>,
    cancel: &CancellationToken,
) -> Result<StepRun, StepError> {
    // The verification set: the contract's, or the verify_spec fallback.
    let verifications = resolve_verifications(&ctx)?;

    let mut events = Vec::new();
    let mut evidence_list = Vec::new();

    for (command, level) in command_verifications(&verifications) {
        // Honor a cancel BETWEEN commands (review S10).
        if cancel.is_cancelled() {
            return Err(StepError::Validate("validate canceled".to_owned()));
        }
        let output = run_command(&ctx.worktree_path, &command, cancel)
            .await
            .map_err(StepError::from)?;

        // One VerifyChecked per command — spliced into the stream by Task 6.5.
        // `VerifyChecked` carries a non-optional TaskId; a spec-level validate
        // has no task, so the event is emitted only for a task-level run.
        if let Some(task_id) = &ctx.task_id {
            events.push(verify_checked_event(
                &ctx.spec_id,
                task_id,
                level,
                &command,
                &output,
            ));
        }
        evidence_list.push(VerificationEvidence {
            name: None,
            command: command.clone(),
            exit_code: output.exit_code,
            level,
        });

        if output.exit_code != 0 {
            // A non-zero exit fails the phase. Emit ErrorEncountered (a
            // fingerprint from the failure's first line — review item 31) and
            // return a Fail routed into the adjustment side-chain (5a.4).
            events.push(error_encountered_event(&ctx, &command, &output));
            return Ok(StepRun {
                outcome: StepOutcome::Fail {
                    error_why_fix: ErrorWhyFix {
                        error: format!("verification command failed (exit {})", output.exit_code),
                        why: format!(
                            "`{command}` exited non-zero; stderr: {}",
                            tail(&output.stderr)
                        ),
                        fix: "fix the cause of the failing verification, then re-run".to_owned(),
                    },
                },
                events,
            });
        }
    }

    Ok(StepRun {
        outcome: StepOutcome::Pass {
            evidence: Evidence {
                files_touched: vec![],
                verifications: evidence_list,
                // `validate` is not a merge step — no merge SHA (G25.2).
                merge_commit_sha: None,
                summary: format!(
                    "{} verification command(s) passed",
                    command_verifications(&verifications).count()
                ),
            },
        },
        events,
    })
}

/// The contract's verifications, or the verify_spec fallback.
///
/// A contract with no `verifications` falls back to `detect_toolchain`; a
/// workspace with no detectable toolchain is a loud error the caller turns
/// into a `Fail` (see [`detect_or_fail`]).
fn resolve_verifications(ctx: &StepCtx) -> Result<Vec<Verification>, StepError> {
    // Task-level validate uses the task contract's verifications if present;
    // otherwise the spec contract's; otherwise the verify_spec fallback.
    let authored: &[Verification] = match &ctx.task_contract {
        Some(tc) if !tc.verifications.is_empty() => &tc.verifications,
        _ if !ctx.spec_contract.verifications.is_empty() => &ctx.spec_contract.verifications,
        _ => &[],
    };
    if !authored.is_empty() {
        return Ok(authored.to_vec());
    }
    // No authored verifications. A spec-level validate (no task) has nothing
    // additional to check — every task already ran its own task-level
    // `validate`. Pass with an empty set rather than forcing the verify_spec
    // toolchain fallback, which a docs-only spec cannot satisfy (P3 — the
    // false-green smoke run's spec-level `step_error`).
    if ctx.task_id.is_none() {
        return Ok(vec![]);
    }
    // A task-level validate with no verifications — verify_spec fallback.
    detect_or_fail(&ctx.spec_contract.workspace)
}

/// The verify_spec fallback: detect the workspace toolchain and synthesize
/// three named `Verification::Command`s. `None` → a loud error.
fn detect_or_fail(workspace: &Path) -> Result<Vec<Verification>, StepError> {
    let DetectedToolchain {
        tests,
        static_,
        syntax,
    } = verify_spec::detect_toolchain(workspace).ok_or_else(|| {
        // Never a trivial pass — a spec with no verifications and no
        // detectable toolchain cannot define success (review S4).
        StepError::Validate(format!(
            "no verifications declared and no toolchain detected in {}",
            workspace.display()
        ))
    })?;
    Ok(vec![
        Verification::Command {
            name: Some("tests".to_owned()),
            command: tests,
        },
        Verification::Command {
            name: Some("static".to_owned()),
            command: static_,
        },
        Verification::Command {
            name: Some("syntax".to_owned()),
            command: syntax,
        },
    ])
}

/// Iterate the `Verification::Command`s of a list, each paired with its
/// inferred [`VerifyLevel`].
///
/// The detected-toolchain levels: a `tests` command is L2 (integration), a
/// `static`/`syntax` command is L1; an authored command with no name is L2 by
/// default. `Verification::Intent` entries are skipped — they need worker
/// judgment, not a deterministic command run.
fn command_verifications(
    verifications: &[Verification],
) -> impl Iterator<Item = (String, VerifyLevel)> + '_ {
    verifications.iter().filter_map(|v| match v {
        Verification::Command { name, command } => {
            let level = match name.as_deref() {
                Some("static") | Some("syntax") => VerifyLevel::L1,
                _ => VerifyLevel::L2,
            };
            Some((command.clone(), level))
        }
        Verification::Intent { .. } => None,
    })
}

/// Build a `VerifyChecked` event for one command's result.
fn verify_checked_event(
    spec_id: &SpecId,
    task_id: &TaskId,
    level: VerifyLevel,
    command: &str,
    output: &CommandOutput,
) -> BoiEvent {
    BoiEvent::VerifyChecked {
        spec_id: spec_id.clone(),
        task_id: task_id.clone(),
        level: verify_level_str(level).to_owned(),
        command: command.to_owned(),
        exit_code: output.exit_code,
        stdout_excerpt: excerpt(&output.stdout),
    }
}

/// Build an `ErrorEncountered` event for a failing command.
fn error_encountered_event(ctx: &StepCtx, command: &str, output: &CommandOutput) -> BoiEvent {
    BoiEvent::ErrorEncountered {
        spec_id: ctx.spec_id.clone(),
        task_id: ctx.task_id.clone(),
        // G24.1 — the phase whose verification failed.
        phase: ctx.phase.clone(),
        error: format!(
            "verification `{command}` failed (exit {})",
            output.exit_code
        ),
        why: format!("stderr: {}", tail(&output.stderr)),
        fix_proposed: Some("fix the cause of the failing verification".to_owned()),
        // Fingerprint from the failure's first line (review item 31) — groups
        // recurrences of the same failure.
        fingerprint: fingerprint(command, output),
    }
}

/// The lowercase `l1`/`l2`/`l3` string a `BoiEvent::VerifyChecked` carries.
fn verify_level_str(level: VerifyLevel) -> &'static str {
    match level {
        VerifyLevel::L1 => "l1",
        VerifyLevel::L2 => "l2",
        VerifyLevel::L3 => "l3",
    }
}

/// A stable fingerprint for a failing command — the command plus the first
/// non-empty stderr line.
fn fingerprint(command: &str, output: &CommandOutput) -> String {
    let first_line = output
        .stderr
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    format!("{command} :: {first_line}")
}

/// The last few lines of a stderr capture, for an error message.
fn tail(text: &str) -> String {
    const MAX_LINES: usize = 5;
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(MAX_LINES);
    lines[start..].join("\n")
}

/// Truncate text to a bounded excerpt for an event payload (char-boundary safe).
fn excerpt(text: &str) -> String {
    const MAX: usize = 2000;
    if text.len() <= MAX {
        return text.to_owned();
    }
    let cut = text
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= MAX)
        .last()
        .unwrap_or(0);
    format!("{}… [truncated]", &text[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::context::{SpecContract, TaskContract};
    use crate::types::ids::{PhaseRunId, SpecId, TaskId};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A throwaway directory removed on drop — `std`-only.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("boi-validate-{}-{tag}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.path));
        }
    }

    fn spec_id() -> SpecId {
        SpecId::new("S0000001a").unwrap()
    }
    fn task_id() -> TaskId {
        TaskId::new("T0000001a").unwrap()
    }

    /// A `StepCtx` for `validate` with the given task/spec verifications.
    fn validate_ctx(
        workspace: &Path,
        spec_verifications: Vec<Verification>,
        task_contract: Option<TaskContract>,
    ) -> Arc<StepCtx> {
        Arc::new(StepCtx {
            spec_id: spec_id(),
            task_id: Some(task_id()),
            phase_run_id: PhaseRunId::new("P0000001a").unwrap(),
            phase: "validate".into(),
            worktree_path: workspace.to_path_buf(),
            branch_ref: "n/a".into(),
            spec_contract: SpecContract {
                scope: "demo".into(),
                workspace: workspace.to_path_buf(),
                base_branch: "main".into(),
                exclusions: vec![],
                verifications: spec_verifications,
                must_emit: vec![],
            },
            task_contract,
        })
    }

    /// A spec-level `StepCtx` for `validate` — no task, the given spec
    /// verifications.
    fn spec_validate_ctx(workspace: &Path, spec_verifications: Vec<Verification>) -> Arc<StepCtx> {
        Arc::new(StepCtx {
            spec_id: spec_id(),
            task_id: None,
            phase_run_id: PhaseRunId::new("P0000001a").unwrap(),
            phase: "validate".into(),
            worktree_path: workspace.to_path_buf(),
            branch_ref: "n/a".into(),
            spec_contract: SpecContract {
                scope: "demo".into(),
                workspace: workspace.to_path_buf(),
                base_branch: "main".into(),
                exclusions: vec![],
                verifications: spec_verifications,
                must_emit: vec![],
            },
            task_contract: None,
        })
    }

    #[tokio::test]
    async fn test_l2_run_command_captures_a_passing_command() {
        let dir = TempDir::new("run-pass");
        let out = run_command(&dir.path, "echo hello", &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);
        assert!(
            out.stdout.contains("hello"),
            "stdout captured: {:?}",
            out.stdout
        );
    }

    #[tokio::test]
    async fn test_l2_run_command_captures_a_failing_command() {
        let dir = TempDir::new("run-fail");
        // Non-zero exit + stderr — a normal CommandOutput, not an error.
        let out = run_command(
            &dir.path,
            "echo oops 1>&2; exit 3",
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(out.exit_code, 3);
        assert!(
            out.stderr.contains("oops"),
            "stderr captured: {:?}",
            out.stderr
        );
    }

    /// OBS-032 — verification commands get the shared, warm `CARGO_TARGET_DIR`,
    /// exactly like the `goose` worker spawn path (goose.rs). The daemon's env
    /// has no `CARGO_TARGET_DIR`, so without the injection a spec-level
    /// `cargo build --release` gate in a fresh integration worktree compiles
    /// ~1148 crates cold (35–200 min observed). The child must see the same
    /// value [`resolve_cargo_target_dir`] resolves (the ambient override, or
    /// the shared default) — never an empty/unset variable.
    #[tokio::test]
    async fn test_l2_run_command_child_env_carries_the_shared_cargo_target_dir() {
        let dir = TempDir::new("run-cargo-target");
        let expected = crate::runtime::worktree::resolve_cargo_target_dir()
            .display()
            .to_string();
        let out = run_command(
            &dir.path,
            r#"printf "%s" "$CARGO_TARGET_DIR""#,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(out.exit_code, 0);
        assert!(
            !out.stdout.is_empty(),
            "the verification child must not see an unset CARGO_TARGET_DIR \
             (OBS-032: cold ~1148-crate builds in fresh worktrees)",
        );
        assert_eq!(
            out.stdout, expected,
            "the verification child sees the resolved shared CARGO_TARGET_DIR",
        );
    }

    /// A command that sets its own `CARGO_TARGET_DIR` inside the shell string
    /// still wins — a shell-level `export` overrides the injected parent env
    /// naturally, so per-command opt-outs keep working. (An `export`, not a
    /// `VAR=x cmd "$VAR"` prefix — POSIX expands the argument *before* the
    /// prefix assignment takes effect.)
    #[tokio::test]
    async fn test_l2_run_command_shell_set_cargo_target_dir_override_wins() {
        let dir = TempDir::new("run-cargo-target-override");
        let out = run_command(
            &dir.path,
            r#"export CARGO_TARGET_DIR=/custom/override; printf "%s" "$CARGO_TARGET_DIR""#,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(out.exit_code, 0);
        assert_eq!(
            out.stdout, "/custom/override",
            "a shell-level CARGO_TARGET_DIR set inside the command wins over \
             the injected value",
        );
    }

    #[tokio::test]
    async fn test_l2_run_command_cancel_kills_the_child_within_the_grace() {
        let dir = TempDir::new("run-cancel");
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        // Fire the cancel shortly after the command starts.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel_for_task.cancel();
        });
        let start = std::time::Instant::now();
        // A long sleep — without the kill it would run for 30 s.
        let result = run_command(&dir.path, "sleep 30", &cancel).await;
        assert!(
            matches!(result, Err(ValidateError::Canceled)),
            "a canceled command yields ValidateError::Canceled, got {result:?}",
        );
        assert!(
            start.elapsed() < CANCEL_GRACE + Duration::from_secs(2),
            "the child must die within the cancel grace, took {:?}",
            start.elapsed(),
        );
    }

    /// Regression test for C-rt-2 — the cancel-path drain deadlock.
    ///
    /// The command backgrounds a grandchild that (a) **inherits stdout** and
    /// (b) **writes continuously** — a `while :; do echo …; done` loop, NOT a
    /// `sleep` (a `sleep` produces no output and cannot exercise the undrained
    /// pipe — exactly why the original cancel test missed this bug). The `sh`
    /// process tokio sees then `wait`s on the grandchild, so it stays alive
    /// until killed.
    ///
    /// On cancel, `start_kill()` SIGKILLs `sh`, but the backgrounded grandchild
    /// **survives, still holding the stdout pipe's write end open**. The OLD
    /// cancel arm did `drop((&mut out_reader).await)` — awaiting a drain task
    /// that reads stdout to EOF — *before* the bounded `timeout(child.wait())`.
    /// That EOF never comes (the grandchild holds the write end), so the join
    /// blocked **forever**, before the timeout could fire. With the fix the
    /// whole post-kill sequence is inside one `CANCEL_GRACE` timeout and the
    /// drain tasks are aborted on overrun — so the cancel returns bounded.
    ///
    /// The whole test is wrapped in an outer timeout: revert the fix and this
    /// test hangs on the unbounded join until the outer timeout trips it as a
    /// failure (it does NOT pass) — a genuine fail-before / pass-after.
    #[tokio::test]
    async fn test_l2_run_command_cancel_is_bounded_despite_a_pipe_holding_grandchild() {
        let dir = TempDir::new("run-cancel-grandchild");
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            cancel_for_task.cancel();
        });

        // Background a continuously-writing grandchild that inherits stdout,
        // then `wait` so `sh` stays alive. `start_kill` kills `sh`; the
        // grandchild is orphaned and keeps the stdout write end open — an EOF
        // the drain would otherwise wait on forever.
        let start = std::time::Instant::now();
        let result = tokio::time::timeout(
            // Generous outer bound: the fix returns within ~CANCEL_GRACE; the
            // unbounded-join bug would blow far past this.
            CANCEL_GRACE + Duration::from_secs(20),
            run_command(
                &dir.path,
                "while : ; do echo pipe-filling-output ; done & echo started ; wait",
                &cancel,
            ),
        )
        .await;

        let inner = result.expect(
            "C-rt-2 regression: run_command did NOT return within the outer bound — \
             the cancel-path stdout drain join is unbounded (a grandchild holds the \
             pipe write end, so the drain's EOF never arrives)",
        );
        assert!(
            matches!(inner, Err(ValidateError::Canceled)),
            "a canceled command yields ValidateError::Canceled, got {inner:?}",
        );
        assert!(
            start.elapsed() < CANCEL_GRACE + Duration::from_secs(10),
            "the cancel must be bounded by the grace window even with a \
             pipe-holding grandchild, took {:?}",
            start.elapsed(),
        );
    }

    #[tokio::test]
    async fn test_l2_validate_passes_when_all_commands_exit_zero() {
        let dir = TempDir::new("validate-pass");
        let run = validate(validate_ctx(
            &dir.path,
            vec![
                Verification::Command {
                    name: Some("syntax".to_owned()),
                    command: "true".to_owned(),
                },
                Verification::Command {
                    name: None,
                    command: "echo ok".to_owned(),
                },
            ],
            None,
        ))
        .await
        .unwrap();
        assert!(
            matches!(run.outcome, StepOutcome::Pass { .. }),
            "all-zero commands → Pass, got {:?}",
            run.outcome,
        );
        // One VerifyChecked per command — two commands, two events.
        let verify_checked = run
            .events
            .iter()
            .filter(|e| matches!(e, BoiEvent::VerifyChecked { .. }))
            .count();
        assert_eq!(verify_checked, 2, "one VerifyChecked per command");
    }

    #[tokio::test]
    async fn test_l2_validate_fails_and_emits_error_encountered_on_a_nonzero_command() {
        let dir = TempDir::new("validate-fail");
        let run = validate(validate_ctx(
            &dir.path,
            vec![
                Verification::Command {
                    name: None,
                    command: "true".to_owned(),
                },
                Verification::Command {
                    name: None,
                    command: "echo boom 1>&2; exit 1".to_owned(),
                },
            ],
            None,
        ))
        .await
        .unwrap();
        assert!(
            matches!(run.outcome, StepOutcome::Fail { .. }),
            "a non-zero command → Fail, got {:?}",
            run.outcome,
        );
        // The stream carries VerifyChecked events AND one ErrorEncountered.
        assert!(
            run.events
                .iter()
                .any(|e| matches!(e, BoiEvent::ErrorEncountered { .. })),
            "a non-zero command must also emit ErrorEncountered",
        );
    }

    #[tokio::test]
    async fn test_l2_validate_intent_only_contract_passes_with_no_commands() {
        let dir = TempDir::new("validate-intent");
        // An Intent-only contract — validate skips intents, runs nothing, passes.
        let run = validate(validate_ctx(
            &dir.path,
            vec![Verification::Intent {
                name: Some("scoped".to_owned()),
                intent: "stays within the api crate".to_owned(),
            }],
            None,
        ))
        .await
        .unwrap();
        let StepOutcome::Pass { evidence } = &run.outcome else {
            unreachable!("an Intent-only contract → Pass, got {:?}", run.outcome);
        };
        assert!(evidence.verifications.is_empty(), "no commands were run");
        assert!(run.events.is_empty(), "no VerifyChecked for an Intent");
    }

    #[tokio::test]
    async fn test_l2_validate_empty_contract_with_no_toolchain_fails_loudly() {
        // An empty workspace dir — no Cargo.toml etc → detect_toolchain None.
        let dir = TempDir::new("validate-no-toolchain");
        let run = validate(validate_ctx(&dir.path, vec![], None)).await;
        let err = run.unwrap_err();
        assert!(
            matches!(&err, StepError::Validate(m) if m.contains("no toolchain")),
            "an empty contract with no detectable toolchain must fail loudly, got {err:?}",
        );
    }

    #[tokio::test]
    async fn test_l2_validate_falls_back_to_detected_toolchain() {
        // A workspace with a Cargo.toml → verify_spec synthesizes 3 commands.
        let dir = TempDir::new("validate-detect");
        std::fs::write(dir.path.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        // The synthesized commands (`cargo test`, ...) would actually run; use
        // a contract that resolves to the fallback but assert on resolution,
        // not execution — `resolve_verifications` is the unit under test.
        let ctx = validate_ctx(&dir.path, vec![], None);
        let resolved = resolve_verifications(&ctx).unwrap();
        assert_eq!(resolved.len(), 3, "verify_spec synthesizes 3 commands");
        let names: Vec<Option<&str>> = resolved
            .iter()
            .map(|v| match v {
                Verification::Command { name, .. } => name.as_deref(),
                Verification::Intent { .. } => None,
            })
            .collect();
        assert_eq!(names, vec![Some("tests"), Some("static"), Some("syntax")]);
    }

    /// P3 regression — a spec-level `validate` (no task) with no spec-level
    /// verifications passes: the per-task validates already gated the work. It
    /// must NOT hit the verify_spec toolchain fallback, which a docs-only spec
    /// cannot satisfy (the false-green smoke run's spec-level `step_error`).
    #[tokio::test]
    async fn test_l2_validate_spec_level_no_verifications_passes() {
        // A docs-only workspace — no Cargo.toml etc; the toolchain fallback
        // would error. A spec-level validate with no spec verifications passes.
        let dir = TempDir::new("validate-spec-empty");
        let run = validate(spec_validate_ctx(&dir.path, vec![]))
            .await
            .unwrap();
        let StepOutcome::Pass { evidence } = &run.outcome else {
            unreachable!(
                "a spec-level validate with no verifications → Pass, got {:?}",
                run.outcome,
            );
        };
        assert!(evidence.verifications.is_empty(), "no commands were run");
    }

    #[test]
    fn test_l1_command_verifications_skips_intents_and_levels_commands() {
        let list = vec![
            Verification::Intent {
                name: None,
                intent: "no panics".into(),
            },
            Verification::Command {
                name: Some("static".into()),
                command: "clippy".into(),
            },
            Verification::Command {
                name: Some("tests".into()),
                command: "test".into(),
            },
        ];
        let got: Vec<(String, VerifyLevel)> = command_verifications(&list).collect();
        assert_eq!(got.len(), 2, "the Intent is skipped");
        assert_eq!(got[0].1, VerifyLevel::L1, "a `static` command is L1");
        assert_eq!(got[1].1, VerifyLevel::L2, "a `tests` command is L2");
    }
}
