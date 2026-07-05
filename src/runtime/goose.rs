//! [`GooseRuntime`] — the worker-phase [`PhaseExecutor`] adapter.
//!
//! `GooseRuntime` is the runtime side of the worker branch: it builds a Goose
//! recipe, spawns `goose run --recipe <file>.yaml --output-format stream-json`,
//! drains the child's stdout through a `StreamMapper` (Task 7.2), and yields
//! the mapped [`BoiEvent`] stream.
//!
//! ## The terminal-`PhaseCompleted` guarantee (G21.5 — load-bearing)
//!
//! The orchestrator's drain treats a stream that ends with no `PhaseCompleted`
//! as a silently-stuck task. So [`GooseRuntime::run_phase`]'s stream yields
//! **exactly one** terminal `PhaseCompleted` on EVERY path: a clean `complete`
//! event, a Goose crash, retry exhaustion, a corrupt stream, a cancel. The
//! driver task that produces the stream cannot end without sending one.
//!
//! ## Subprocess lifecycle (Phase 6 preamble — review C2)
//!
//! Every `goose` child is built with `kill_on_drop(true)` as the panic-path
//! backstop. The intentional stop on a fired `cancel` is `start_kill()` + a
//! drained `wait()` under a 5 s grace (`CANCEL_GRACE`) — never a bare
//! `kill().await` with stdout undrained (a full pipe deadlocks the child).
//! The driver's `kill_and_reap` therefore **always keeps draining stdout
//! concurrently with `child.wait()`**: on the cancel path the line reader is
//! handed back to `kill_and_reap` rather than dropped, and on the retry /
//! crash path a still-live `goose` mid-generation would otherwise write-block
//! on a full stdout pipe and never die (review C-rt-1 / C-rt-S1). stderr is
//! drained in its own background task. Both drains are abortable so a grandchild
//! that inherited a pipe fd cannot wedge the grace window.
//!
//! ## Worst-case cancel latency (NIT)
//!
//! A cancel can cost up to `2 × CANCEL_GRACE` (10 s): `kill_and_reap` bounds
//! `child.wait()` at one `CANCEL_GRACE`, and the stderr-tail collection bounds
//! its own join at a second `CANCEL_GRACE`. Both timeouts are independent
//! backstops against a wedged grandchild; the common case returns in
//! milliseconds.
//!
//! ## The 2-retry loop owns the child lifecycle (review C3)
//!
//! A `VerdictParse`, a non-overflow `error` line, or a transient `AgentError`
//! → a retry, capped at 2 — **2 retries = 3 spawns** total. Each retry fully
//! terminates the prior child before spawning the next; the loop holds exactly
//! one live `Child`. A `ContextOverflow` and a `Transport` error do NOT retry
//! (Task 7.2's policy) — they end the phase immediately.
//!
//! ## 429 hardening (incident 2026-06-06)
//!
//! A throttled Claude Max token (HTTP 429) is swallowed by goose into either
//! an **empty completion** (a bare `complete`, exit 0) or a **held/stalled
//! connection** that produces nothing forever. Two defenses live here:
//!
//! - **D1 — per-attempt wall-clock timeout.** Every `goose run` attempt is
//!   bounded by `DEFAULT_ATTEMPT_TIMEOUT` (15 min; override:
//!   `BOI_GOOSE_ATTEMPT_TIMEOUT_SECS`, `0` disables). On expiry the goose
//!   process TREE is SIGKILLed (`killpg` — each child leads its own group,
//!   because goose's grandchildren hold the pipe fds and the stalled
//!   connection) and the attempt retries.
//! - **Loud rate-limit surfacing.** A retry exhaustion whose failures are
//!   rate-limit-shaped — every empty completion, or a verdict-parse/agent
//!   error carrying 429 markers — fails with `error: rate_limited` and a
//!   why/fix that SAY "RATE LIMITED", never a generic `verdict_parse` that
//!   blames the worker for a provider throttle (S6 — no quiet failures).

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use futures::stream::{BoxStream, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::config::PhaseDef;
use crate::runtime::recipe::{build_recipe, write_recipe};
use crate::runtime::stream::{StreamIdentity, StreamMapError, StreamMapper};
use crate::service::registry::PhaseExecutor;
use crate::types::context::PhaseContext;
use crate::types::event::BoiEvent;
use crate::types::verdict::{VerdictOutcome, WorkerVerdict};

/// How long a canceled `goose` child is given to die before its `Child` is
/// dropped (the `kill_on_drop` backstop then reaps it).
const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// The retry cap on a `VerdictParse` / non-overflow `error` — **2 retries = 3
/// spawns** total (review C3).
const MAX_RETRIES: u32 = 2;

/// The base delay for the between-retry exponential backoff (FIX-004). Retry
/// `n` (0-indexed) waits `DEFAULT_RETRY_BACKOFF_BASE * 3^n` — 5s then 15s for
/// the two retries — so a transient provider rate-limit window (the
/// empty-completion / agent-error cause) is not slammed 3× in ~90s.
const DEFAULT_RETRY_BACKOFF_BASE: Duration = Duration::from_secs(5);

/// The default per-attempt wall-clock timeout on one `goose run` spawn (D1,
/// 429 hardening) — 15 minutes. A rate-limited (HTTP 429) connection can
/// manifest as a held/stalled stream: goose blocks forever, the worker keeps
/// heartbeating, and the phase hangs invisibly (incident 2026-06-06). The
/// timeout cuts the attempt, kills the goose process TREE, and classifies the
/// attempt as a retryable failure. Override via [`ATTEMPT_TIMEOUT_ENV`];
/// `0` disables.
const DEFAULT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// The env var overriding [`DEFAULT_ATTEMPT_TIMEOUT`], in whole seconds.
/// `0` disables the per-attempt timeout entirely.
const ATTEMPT_TIMEOUT_ENV: &str = "BOI_GOOSE_ATTEMPT_TIMEOUT_SECS";

/// Resolve the per-attempt timeout from the raw [`ATTEMPT_TIMEOUT_ENV`] value.
///
/// `None` (unset) → [`DEFAULT_ATTEMPT_TIMEOUT`]; a parseable number → that
/// many seconds (`0` = disabled); garbage → the default, LOUDLY logged (a
/// typo'd override must never silently disable the hang protection — S6).
fn attempt_timeout_from(raw: Option<&str>) -> Duration {
    match raw {
        None => DEFAULT_ATTEMPT_TIMEOUT,
        Some(s) => match s.trim().parse::<u64>() {
            Ok(secs) => Duration::from_secs(secs),
            Err(e) => {
                tracing::error!(
                    value = %s,
                    error = %e,
                    default_secs = DEFAULT_ATTEMPT_TIMEOUT.as_secs(),
                    "invalid {ATTEMPT_TIMEOUT_ENV} — falling back to the default",
                );
                DEFAULT_ATTEMPT_TIMEOUT
            }
        },
    }
}

/// The hard line-length cap on a `goose` stdout line (~8 MB). A line past this
/// is a `Transport` error — a runaway line is never buffered unbounded
/// (review S1/S11).
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// The channel buffer between the driver task and the returned stream.
const EVENT_CHANNEL_CAP: usize = 256;

/// The `Fail` error tag for a rate-limit-shaped retry exhaustion (429
/// hardening). The incident class this names: a Claude Max HTTP 429 that
/// goose swallows into an empty completion / garbage verdict — the verdict
/// must SAY rate-limited, not blame the worker with a generic
/// `verdict_parse` (incident 2026-06-06, hex S6 — no quiet failures).
const TAG_RATE_LIMITED: &str = "rate_limited";

/// The `Fail` error tag for a per-attempt wall-clock timeout exhaustion (D1).
const TAG_ATTEMPT_TIMEOUT: &str = "attempt_timeout";

/// Whether a failure detail / stderr capture carries rate-limit markers —
/// the HTTP 429 status, Anthropic's `rate_limit_error` type, or prose
/// "rate limit" variants. Case-insensitive, deliberately broad: this only
/// re-labels an ALREADY-failed retry exhaustion, so a false positive costs a
/// more specific error message, never a behavior change.
fn rate_limit_shaped(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("429")
        || t.contains("rate_limit")
        || t.contains("rate limit")
        || t.contains("rate-limit")
}

/// The worker-phase [`PhaseExecutor`] adapter.
///
/// Holds the `goose` binary path, the recipe directory root, and the prompt-
/// template directory. One `GooseRuntime` is shared across all worker phase
/// runs; `run_phase` is called once per run.
///
/// ## `prompts_dir` — the G26.1 prompt-template resolution root
///
/// `PhaseDef::prompt_template` is a *filename* (`"execute.md"`). `GooseRuntime`
/// resolves it against `prompts_dir` (`<prompts_dir>/<prompt_template>`),
/// reads the file, and threads the **content** into
/// [`build_recipe`]. The phase TOMLs and
/// their prompt templates both live in `~/.boi/v2/phases/`, so `boot` passes
/// the phases directory. A worker phase whose template file is missing is a
/// loud terminal `Fail` — never a silent empty prompt (G26.1).
pub struct GooseRuntime {
    /// Path to the `goose` binary.
    goose_bin: PathBuf,
    /// Root directory under which per-phase-run recipe files are written.
    recipe_dir: PathBuf,
    /// Directory the `PhaseDef::prompt_template` filename resolves against
    /// (G26.1). Phase TOMLs and prompt templates co-locate in this dir.
    prompts_dir: PathBuf,
    /// Root under which task/integration worktrees live — the worker's `goose`
    /// runs in its worktree (RC1). `~/.boi/v2/worktrees` in production.
    worktree_root: PathBuf,
    /// The base delay for the between-retry exponential backoff (FIX-004). The
    /// nth retry waits `base * 3^n` before re-spawning, so a transient
    /// rate-limit window is not hit 3× back-to-back. Production default
    /// [`DEFAULT_RETRY_BACKOFF_BASE`]; tests set it to `Duration::ZERO`.
    retry_backoff_base: Duration,
    /// The per-attempt wall-clock timeout (D1). Resolved from
    /// [`ATTEMPT_TIMEOUT_ENV`] at construction (default
    /// [`DEFAULT_ATTEMPT_TIMEOUT`]); `Duration::ZERO` disables. Tests override
    /// via [`GooseRuntime::with_attempt_timeout`].
    attempt_timeout: Duration,
}

impl GooseRuntime {
    /// Construct the runtime with the production worktree root
    /// (`~/.boi/v2/worktrees`).
    ///
    /// G16.2 — `boot` needs a public constructor (the fields are private).
    /// `prompts_dir` is the directory a worker phase's `prompt_template`
    /// filename resolves against (G26.1).
    pub fn new(goose_bin: PathBuf, recipe_dir: PathBuf, prompts_dir: PathBuf) -> Self {
        Self::with_worktree_root(
            goose_bin,
            recipe_dir,
            prompts_dir,
            crate::runtime::worktree::default_worktree_root(),
        )
    }

    /// Construct the runtime with an explicit worktree root — the worker's
    /// `goose` runs in its task/integration worktree under this root (RC1).
    /// Tests pass a temp dir; [`GooseRuntime::new`] uses the production root.
    pub fn with_worktree_root(
        goose_bin: PathBuf,
        recipe_dir: PathBuf,
        prompts_dir: PathBuf,
        worktree_root: PathBuf,
    ) -> Self {
        Self {
            goose_bin,
            recipe_dir,
            prompts_dir,
            worktree_root,
            retry_backoff_base: DEFAULT_RETRY_BACKOFF_BASE,
            attempt_timeout: attempt_timeout_from(
                std::env::var(ATTEMPT_TIMEOUT_ENV).ok().as_deref(),
            ),
        }
    }

    /// Override the between-retry backoff base (FIX-004). Production uses
    /// `DEFAULT_RETRY_BACKOFF_BASE`; tests pass `Duration::ZERO` so the
    /// retry-loop tests do not sleep real wall-clock between fake-goose spawns.
    #[must_use]
    pub fn with_retry_backoff_base(mut self, base: Duration) -> Self {
        self.retry_backoff_base = base;
        self
    }

    /// Override the per-attempt wall-clock timeout (D1). Production resolves
    /// it from `BOI_GOOSE_ATTEMPT_TIMEOUT_SECS` (default 15 min);
    /// tests pass milliseconds so the stalled-goose tests stay fast.
    /// `Duration::ZERO` disables the timeout.
    #[must_use]
    pub fn with_attempt_timeout(mut self, timeout: Duration) -> Self {
        self.attempt_timeout = timeout;
        self
    }

    /// Run one worker phase: build+write the recipe, spawn `goose run …
    /// --output-format stream-json`, stream stdout through a `StreamMapper`
    /// (Task 7.2), and yield the mapped [`BoiEvent`]s.
    ///
    /// `--no-session` is NEVER passed — the terminal `complete` event sources
    /// its token counts from the session record (spike §Q4).
    ///
    /// The returned stream ALWAYS terminates in exactly one `PhaseCompleted`
    /// (G21.5) — including a Goose crash, retry exhaustion, a corrupt stream,
    /// or a cancel.
    pub fn run_phase(
        &self,
        phase: PhaseDef,
        ctx: PhaseContext,
        cancel: CancellationToken,
    ) -> BoxStream<'static, BoiEvent> {
        let goose_bin = self.goose_bin.clone();
        let recipe_dir = self.recipe_dir.clone();
        let prompts_dir = self.prompts_dir.clone();
        let worktree_root = self.worktree_root.clone();
        let retry_backoff_base = self.retry_backoff_base;
        let attempt_timeout = self.attempt_timeout;
        let (tx, rx) = mpsc::channel::<BoiEvent>(EVENT_CHANNEL_CAP);

        // The driver task: it owns the recipe build, the 2-retry loop, and the
        // child lifecycle. Whatever happens it sends exactly one terminal
        // `PhaseCompleted` before returning (G21.5).
        tokio::spawn(async move {
            let driver = PhaseDriver {
                goose_bin,
                recipe_dir,
                prompts_dir,
                worktree_root,
                retry_backoff_base,
                attempt_timeout,
                phase,
                ctx,
                cancel,
                tx,
            };
            driver.run().await;
        });

        ReceiverStream::new(rx).boxed()
    }
}

impl PhaseExecutor for GooseRuntime {
    /// The worker branch of [`PhaseExecutor`] — delegates to
    /// [`GooseRuntime::run_phase`].
    fn execute(
        &self,
        phase: PhaseDef,
        ctx: PhaseContext,
        cancel: CancellationToken,
    ) -> BoxStream<'static, BoiEvent> {
        self.run_phase(phase, ctx, cancel)
    }
}

/// The per-phase-run driver — owns the recipe, the retry loop, and the child.
struct PhaseDriver {
    goose_bin: PathBuf,
    recipe_dir: PathBuf,
    /// Directory the `phase.prompt_template` filename resolves against (G26.1).
    prompts_dir: PathBuf,
    /// Worktree root — the worker's `goose` runs in its worktree under it (RC1).
    worktree_root: PathBuf,
    /// Base delay for the between-retry exponential backoff (FIX-004).
    retry_backoff_base: Duration,
    /// Per-attempt wall-clock timeout (D1); `Duration::ZERO` disables.
    attempt_timeout: Duration,
    phase: PhaseDef,
    ctx: PhaseContext,
    cancel: CancellationToken,
    tx: mpsc::Sender<BoiEvent>,
}

/// The outcome of one `goose` attempt — what the retry loop decides on.
enum AttemptOutcome {
    /// The attempt produced a terminal `PhaseCompleted` (sent already) — done.
    Terminal,
    /// A retryable failure (`VerdictParse` / non-overflow error) — try again
    /// if the retry budget allows. Carries the failure detail for the
    /// eventual `Fail` verdict if the budget is exhausted.
    Retry(RetryReason),
    /// The phase was canceled mid-stream — the driver ends without retry.
    Canceled,
}

/// Why an attempt asked to be retried — carried so the final `Fail` verdict
/// after retry-exhaustion names the real cause.
#[derive(Debug, Clone)]
struct RetryReason {
    /// A short machine tag for the `Fail` verdict's `error`.
    tag: String,
    /// The human-readable detail.
    detail: String,
    /// The raw agent error string, set ONLY for a [`StreamMapError::AgentError`]
    /// retry. When retry-exhaustion is reached on an agent error the terminal
    /// path emits an `ErrorEncountered` alongside the `Fail` (the mapper no
    /// longer emits it — review C-cr-1); a `VerdictParse` exhaustion does not.
    agent_error: Option<String>,
}

/// The directory a worker's `goose` process runs in — the task worktree for a
/// task-level phase, the integration worktree for a spec-level phase. The Goose
/// `developer` extension's file/shell tools operate relative to this cwd;
/// without it a worker edits the daemon's cwd, not the worktree (RC1).
fn worker_cwd(ctx: &PhaseContext, worktree_root: &std::path::Path) -> PathBuf {
    match &ctx.task_id {
        Some(task_id) => {
            crate::runtime::worktree::task_worktree(worktree_root, &ctx.spec_id, task_id)
        }
        None => crate::runtime::worktree::integration_worktree(worktree_root, &ctx.spec_id),
    }
}

impl PhaseDriver {
    /// Run the phase to a terminal `PhaseCompleted` — the 2-retry loop.
    async fn run(self) {
        // G26.1 — resolve the prompt-template FILENAME to its content. A
        // worker phase that names a template the harness cannot read is a
        // hard, pre-spawn `Fail` (the worker would otherwise run with an empty
        // or filename-only prompt). A worker phase MUST declare a
        // `prompt_template` (config-layer `parse_phase` enforces it), so a
        // `None` here is itself a misconfiguration — also a loud `Fail`.
        let prompt_body = match &self.phase.prompt_template {
            Some(filename) => {
                let path = self.prompts_dir.join(filename);
                match std::fs::read_to_string(&path) {
                    Ok(body) => body,
                    Err(e) => {
                        tracing::error!(
                            path = %path.display(), error = %e,
                            "goose worker prompt-template file unreadable",
                        );
                        self.send_fail(
                            "prompt_template_unreadable",
                            &format!(
                                "the prompt-template file `{}` for phase `{}` could not be \
                                 read: {e}",
                                path.display(),
                                self.phase.name,
                            ),
                            "create the prompt-template file, or fix the phase TOML's \
                             `prompt_template` field",
                        )
                        .await;
                        return;
                    }
                }
            }
            None => {
                tracing::error!(
                    phase = %self.phase.name,
                    "a worker phase reached GooseRuntime with no prompt_template",
                );
                self.send_fail(
                    "prompt_template_missing",
                    &format!(
                        "worker phase `{}` declares no prompt_template — a worker phase \
                         MUST name one",
                        self.phase.name,
                    ),
                    "add a `prompt_template` to the phase TOML",
                )
                .await;
                return;
            }
        };

        // Build + write the recipe ONCE — every attempt re-uses it. The recipe
        // file name is keyed on `phase_run_id` so concurrent worker phases
        // sharing `recipe_dir` never clobber each other (review C-rt-S3). The
        // RESOLVED prompt body (not the filename) is threaded into the recipe
        // (G26.1).
        // G23.1 → G26.3 — the spec's `[[skill]]` blocks are re-hydrated onto
        // `PhaseContext::skills` at clock-in (see `rehydrate_contracts`). Thread
        // them into the recipe so worker phases actually load the declared Goose
        // extensions; passing `&[]` here is the silent-loss bug this fix closes.
        let recipe = build_recipe(
            &self.phase,
            &self.ctx,
            &self.ctx.skills,
            &self.ctx.phase_run_id,
            Some(&prompt_body),
        );
        let recipe_path = match write_recipe(&recipe, &self.recipe_dir, &self.ctx.phase_run_id) {
            Ok(p) => p,
            Err(e) => {
                // The recipe could not be written — a hard, pre-spawn failure.
                // Still terminate in PhaseCompleted (G21.5).
                tracing::error!(error = %e, "goose recipe write failed");
                self.send_fail(
                    "recipe_write_failed",
                    &e.to_string(),
                    "check the recipe directory is writable",
                )
                .await;
                return;
            }
        };

        // 2-retry loop — at most 3 attempts; each fully reaps its child before
        // the next spawns (review C3).
        let mut last_retry: Option<RetryReason> = None;
        for attempt in 0..=MAX_RETRIES {
            match self.run_attempt(&recipe_path, attempt).await {
                AttemptOutcome::Terminal => return,
                AttemptOutcome::Canceled => return,
                AttemptOutcome::Retry(reason) => {
                    tracing::warn!(
                        attempt,
                        tag = %reason.tag,
                        "goose attempt failed — retrying",
                    );
                    last_retry = Some(reason);
                    // FIX-004: back off before the next spawn so a transient
                    // provider rate-limit window (the empty-completion /
                    // agent-error cause) is not slammed 3× back-to-back. No
                    // sleep after the LAST attempt (it falls straight through to
                    // the terminal Fail). A cancel during the backoff ends the
                    // phase promptly rather than waiting out the delay.
                    if attempt < MAX_RETRIES && !self.retry_backoff_base.is_zero() {
                        let backoff = self.retry_backoff_base * 3u32.pow(attempt);
                        tracing::warn!(
                            attempt,
                            backoff_secs = backoff.as_secs(),
                            "goose backing off before retry",
                        );
                        tokio::select! {
                            _ = tokio::time::sleep(backoff) => {}
                            _ = self.cancel.cancelled() => return,
                        }
                    }
                    // Loop to the next attempt (a fresh `goose` spawn).
                }
            }
        }

        // The retry budget is exhausted — the synthesized terminal Fail, sent
        // AFTER the last child was reaped (run_attempt reaps before returning).
        let reason = last_retry.unwrap_or(RetryReason {
            tag: "verdict_parse".to_owned(),
            detail: "goose produced no parseable verdict across all attempts".to_owned(),
            agent_error: None,
        });
        // A transient-agent-error exhaustion emits a terminal `ErrorEncountered`
        // alongside the `Fail` — the mapper no longer emits it on the first
        // occurrence (review C-cr-1); it is synthesized here, once, only after
        // every retry failed. The why/fix are tag-aware (429 hardening): a
        // rate-limit-shaped or wall-clock-timeout exhaustion must SAY what
        // happened — "RATE LIMITED" / "timed out" — not a generic "inspect
        // the goose output" (S6 — no quiet failures).
        let why = match (reason.tag.as_str(), &reason.agent_error) {
            (TAG_RATE_LIMITED, _) => format!(
                "RATE LIMITED — the provider throttled every attempt: {}",
                reason.detail,
            ),
            (TAG_ATTEMPT_TIMEOUT, _) => format!(
                "every goose attempt exceeded its wall-clock timeout: {}",
                reason.detail,
            ),
            (_, Some(agent_error)) => {
                format!("goose returned an agent error on every attempt: {agent_error}")
            }
            (_, None) => reason.detail.clone(),
        };
        let fix = match reason.tag.as_str() {
            TAG_RATE_LIMITED => {
                "RATE LIMITED — wait for the provider's rate-limit window to reset before \
                 re-dispatching, and reduce concurrent BOI load; the token is throttled \
                 (HTTP 429), the worker is not at fault"
            }
            TAG_ATTEMPT_TIMEOUT => {
                "the goose attempts stalled past the per-attempt wall-clock cap — often a \
                 held rate-limited (HTTP 429) connection; check the provider's rate-limit \
                 window, or raise BOI_GOOSE_ATTEMPT_TIMEOUT_SECS for legitimately long phases"
            }
            _ if reason.agent_error.is_some() => {
                "inspect the goose error; the provider failed across all retries"
            }
            _ => "inspect the goose output; the worker did not emit a valid verdict",
        };
        if let Some(agent_error) = &reason.agent_error {
            self.send_error_encountered(agent_error).await;
        }
        self.send_fail(&reason.tag, &why, fix).await;
    }

    /// Run one `goose` attempt: spawn, drain stdout through the mapper, drain
    /// stderr concurrently, reap the child. Returns the retry decision.
    async fn run_attempt(&self, recipe_path: &PathBuf, attempt: u32) -> AttemptOutcome {
        let mut child = match self.spawn_goose(recipe_path) {
            Ok(c) => c,
            Err(e) => {
                // Spawn failure — `goose` missing / unexecutable. Not a
                // retryable verdict-parse; end the phase loudly (G21.5).
                tracing::error!(error = %e, attempt, "goose spawn failed");
                self.send_fail(
                    "goose_spawn_failed",
                    &e.to_string(),
                    "verify `goose` is installed and on PATH (run preflight)",
                )
                .await;
                return AttemptOutcome::Terminal;
            }
        };

        // Take both pipes — BOTH must be drained concurrently or an undrained
        // pipe deadlocks the child (review C2).
        let Some(stdout) = child.stdout.take() else {
            // Should not happen — `Stdio::piped()` was set. There is no stdout
            // pipe to drain, so a bare `reap_child` (kill + bounded wait) is
            // correct here — `kill_and_reap`'s concurrent stdout drain has
            // nothing to drain.
            let _ = Self::reap_child(&mut child).await;
            self.send_fail(
                "goose_no_stdout",
                "goose child stdout pipe missing",
                "this is a harness bug — report it",
            )
            .await;
            return AttemptOutcome::Terminal;
        };
        let stderr = child.stderr.take();
        // Drain stderr in a background task so it never fills its pipe.
        let stderr_task = tokio::spawn(async move {
            match stderr {
                Some(pipe) => drain_to_string(pipe).await,
                None => String::new(),
            }
        });

        // Drain stdout line-by-line through the mapper, racing the cancel. The
        // line reader is handed BACK so `kill_and_reap` can keep draining it
        // concurrently with `child.wait()` — a `goose` killed mid-generation
        // write-blocks on a full stdout pipe and never dies if stdout is left
        // undrained (review C-rt-1 / C-rt-S1).
        let (outcome, reader) = self.drain_stdout(stdout).await;

        // D1 — a timed-out attempt kills the goose process TREE before the
        // regular reap: goose's grandchildren hold the pipe fds and the
        // stalled provider connection, and `kill_and_reap`'s `start_kill`
        // reaches only the direct child. LOUD — a silent timeout is the bug
        // class this fixes (S6).
        if matches!(outcome, DrainOutcome::TimedOut) {
            tracing::error!(
                attempt,
                timeout_secs = self.attempt_timeout.as_secs(),
                phase = %self.ctx.phase,
                "goose attempt exceeded its wall-clock timeout ({ATTEMPT_TIMEOUT_ENV}) — \
                 killing the goose process tree; the attempt is retryable (D1)",
            );
            Self::kill_process_tree(&child);
        }

        // Whatever happened, reap the child (review C3 — one live child) while
        // STILL draining its stdout — the undrained-pipe deadlock fix.
        let exit_status = self.kill_and_reap(&mut child, reader).await;

        // Collect the stderr tail — but BOUNDED. A SIGKILL'd `goose` whose
        // child grandprocesses inherited the stderr fd can hold the pipe's
        // write end open after `goose` itself is reaped (a `goose run` that
        // SIGKILLs mid-extension-spawn); the stderr drain would then block on
        // an EOF that never comes. The stderr tail is best-effort diagnostic
        // data — never worth a hang, so cap the wait and `abort` the drain
        // task if it overruns (review C2 — drains must be bounded).
        let abort = stderr_task.abort_handle();
        let stderr_tail = match tokio::time::timeout(CANCEL_GRACE, stderr_task).await {
            Ok(Ok(tail)) => tail,
            // Recover the panic payload, not the bare "task panicked" (NIT).
            Ok(Err(e)) => format!(
                "(stderr drain task failed: {})",
                crate::runtime::worktree::join_error_detail(e),
            ),
            Err(_) => {
                // The drain overran — abort it so the task does not leak past
                // the grandchild's lifetime.
                abort.abort();
                tracing::warn!(
                    "goose stderr drain did not finish within the grace window — \
                     a grandchild likely still holds the pipe; drain aborted",
                );
                "(stderr unavailable — drain timed out)".to_owned()
            }
        };

        self.resolve_attempt(outcome, &stderr_tail, exit_status)
            .await
    }

    /// Drain `goose` stdout line-by-line through a [`StreamMapper`], relaying
    /// every mapped event onto the channel, racing a fired `cancel`.
    ///
    /// Returns the [`DrainOutcome`] the caller turns into a retry decision AND
    /// the line reader itself — `kill_and_reap` keeps draining that reader
    /// concurrently with `child.wait()` so a killed-mid-write `goose` does not
    /// deadlock on a full stdout pipe (review C-rt-1 / C-rt-S1). The reader is
    /// NOT dropped on the cancel path — that was the deadlock.
    async fn drain_stdout<R>(&self, stdout: R) -> (DrainOutcome, BufReader<R>)
    where
        R: AsyncRead + Unpin,
    {
        let mut mapper = StreamMapper::new(StreamIdentity {
            spec_id: self.ctx.spec_id.clone(),
            task_id: self.ctx.task_id.clone(),
            phase_run_id: self.ctx.phase_run_id.clone(),
            phase: self.ctx.phase.clone(),
        });
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();

        // D1 — the per-attempt wall-clock deadline. One sleep armed for the
        // whole attempt (not per line): a stalled rate-limited connection
        // produces no lines at all, so a per-line idle timeout would be
        // equivalent here, but wall-clock is the documented contract
        // (BOI_GOOSE_ATTEMPT_TIMEOUT_SECS). `Duration::ZERO` disables.
        let timeout_enabled = !self.attempt_timeout.is_zero();
        let attempt_deadline = tokio::time::sleep(if timeout_enabled {
            self.attempt_timeout
        } else {
            // Disabled — park the sleep far in the future; the branch below
            // is also guarded off.
            Duration::from_secs(u32::MAX as u64)
        });
        tokio::pin!(attempt_deadline);

        let outcome = loop {
            line.clear();
            tokio::select! {
                // Bias the cancel branch so a fired token wins a ready line.
                biased;
                () = self.cancel.cancelled() => {
                    // Intentional stop — the kill AND the concurrent stdout
                    // drain happen in the caller's `kill_and_reap`, which is
                    // handed `reader` below. We must NOT drop `reader` here.
                    tracing::debug!("goose phase canceled mid-stream");
                    break DrainOutcome::Canceled;
                }
                () = &mut attempt_deadline, if timeout_enabled => {
                    // D1 — the attempt overran its wall-clock budget. The
                    // caller kills the process TREE and treats the attempt as
                    // a retryable failure. The reader is handed back so
                    // `kill_and_reap` keeps draining the dying pipe.
                    break DrainOutcome::TimedOut;
                }
                read = read_capped_line(&mut reader, &mut line) => {
                    match read {
                        Ok(0) => {
                            // EOF — the child closed stdout.
                            break DrainOutcome::StreamEnded {
                                saw_complete: mapper.seen_complete(),
                            };
                        }
                        Ok(_) => {
                            let text = String::from_utf8_lossy(&line);
                            match mapper.map(text.trim_end()) {
                                Ok(events) => {
                                    let mut gone = false;
                                    for ev in events {
                                        if self.tx.send(ev).await.is_err() {
                                            // The receiver was dropped — nobody
                                            // is draining. Stop; the caller
                                            // reaps the child.
                                            gone = true;
                                            break;
                                        }
                                    }
                                    if gone {
                                        break DrainOutcome::ReceiverGone;
                                    }
                                }
                                Err(e) => {
                                    // A mapper error ends this attempt — the
                                    // child is killed by the caller. Per-error
                                    // policy is resolved by `resolve_attempt`.
                                    break DrainOutcome::MapError(e);
                                }
                            }
                        }
                        Err(e) => {
                            // A read / line-cap error — a Transport failure.
                            break DrainOutcome::MapError(StreamMapError::Transport(
                                format!("reading goose stdout: {e}"),
                            ));
                        }
                    }
                }
            }
        };
        (outcome, reader)
    }

    /// Turn a [`DrainOutcome`] + stderr + exit status into an [`AttemptOutcome`].
    ///
    /// This is the per-error-kind policy gate (review S8): every outcome has a
    /// stated destination; the stream ALWAYS ends in a `PhaseCompleted`.
    ///
    /// When Goose exits non-zero, `resolve_attempt` and `resolve_map_error`
    /// produce `Fail{goose_exited_nonzero}` carrying the exit code and FULL
    /// stderr — not `stream_corrupt` / "this is a goose/transport bug". The
    /// `stream_corrupt` error is reserved for genuine mid-stream corruption on
    /// a Goose run that exited 0.
    async fn resolve_attempt(
        &self,
        outcome: DrainOutcome,
        stderr_full: &str,
        exit_status: Option<std::process::ExitStatus>,
    ) -> AttemptOutcome {
        match outcome {
            // The mapper emitted its terminal `PhaseCompleted` (a `complete` or
            // a non-overflow `error` event). Those events were already relayed
            // — the phase is done.
            DrainOutcome::StreamEnded { saw_complete: true } => AttemptOutcome::Terminal,

            // Stdout closed WITHOUT a `complete` event. If Goose exited
            // non-zero the real cause is the exit code + stderr (e.g. a missing
            // `prompt` field, a bad provider key). Report that honestly rather
            // than the generic "goose_crashed".
            DrainOutcome::StreamEnded {
                saw_complete: false,
            } => {
                if exit_status.is_some_and(|s| !s.success()) {
                    self.send_fail(
                        "goose_exited_nonzero",
                        &nonzero_exit_why(exit_status, stderr_full),
                        "inspect the goose stderr above — the recipe or environment \
                         rejected the run (e.g. missing `prompt` field, bad provider \
                         key, or unreadable recipe)",
                    )
                    .await;
                } else {
                    self.send_fail(
                        "goose_crashed",
                        &format!(
                            "goose exited without a terminal `complete` event; \
                             stderr tail: {}",
                            tail(stderr_full),
                        ),
                        "inspect the goose stderr; the worker process did not finish \
                         cleanly",
                    )
                    .await;
                }
                AttemptOutcome::Terminal
            }

            // The downstream receiver was dropped — the orchestrator's drain
            // ended. This is the ONE terminal path that emits no
            // `PhaseCompleted`, and that is correct, not a G21.5 violation: the
            // `rx` half lives in the Phase 5a `drain_phase` task and is dropped
            // ONLY when that drain has already ended (it stops `stream.next()`
            // and returns). The orchestrator has therefore already settled this
            // phase run from its own `DrainStatus`; a synthesized
            // `PhaseCompleted` would have no receiver and nothing to act on it.
            // G21.5 ("every path ends in `PhaseCompleted`") is a guarantee made
            // to the drain; once the drain is gone the guarantee has no
            // audience. `send_fail` is still called below for defence in depth
            // — it is a harmless no-op on the closed channel.
            DrainOutcome::ReceiverGone => {
                self.send_fail(
                    "receiver_gone",
                    "the orchestrator drain ended before the goose stream did",
                    "no action — the phase run was already settled by the drain",
                )
                .await;
                AttemptOutcome::Terminal
            }

            DrainOutcome::Canceled => AttemptOutcome::Canceled,

            // D1 — the attempt overran its wall-clock budget (the goose
            // process tree was already killed in `run_attempt`). A stalled
            // attempt is rate-limit-shaped (a held 429 connection — incident
            // 2026-06-06) and NOT the worker's fault → RETRY, with the detail
            // on the `agent_error` channel so exhaustion emits an
            // `ErrorEncountered` naming the stall.
            DrainOutcome::TimedOut => {
                let detail = format!(
                    "goose attempt exceeded the {}s per-attempt wall-clock timeout \
                     ({ATTEMPT_TIMEOUT_ENV}) — often a held rate-limited (HTTP 429) provider \
                     connection; the goose process tree was killed; goose stderr tail: {}",
                    self.attempt_timeout.as_secs(),
                    tail(stderr_full),
                );
                AttemptOutcome::Retry(RetryReason {
                    tag: TAG_ATTEMPT_TIMEOUT.to_owned(),
                    detail: detail.clone(),
                    agent_error: Some(detail),
                })
            }

            DrainOutcome::MapError(e) => self.resolve_map_error(e, stderr_full, exit_status).await,
        }
    }

    /// Route a [`StreamMapError`] per Task 7.2's policy (review S8).
    ///
    /// A non-zero Goose exit overrides the `Transport` classification:
    /// `stream_corrupt` is reserved for genuine mid-stream corruption on a run
    /// that exited 0. If the child exited non-zero, `Transport` is a symptom
    /// of the real cause (the exit code + stderr); we report that honestly.
    async fn resolve_map_error(
        &self,
        error: StreamMapError,
        stderr_full: &str,
        exit_status: Option<std::process::ExitStatus>,
    ) -> AttemptOutcome {
        match error {
            // A fully empty completion — no assistant text, no tool calls
            // (FIX-004). The provider returned nothing: the `claude-code`
            // rate-limit signature (a throttled token makes goose emit a bare
            // `complete` and exit 0 — incident 2026-06-06). RETRY (up to the
            // cap) — but UNLIKE `verdict_parse` this is NOT the worker's
            // fault: the reason is tagged `rate_limited` so the
            // post-exhaustion terminal Fail SAYS rate-limited, and the stderr
            // tail rides into the detail AND the `agent_error` channel so an
            // `ErrorEncountered` names the provider (S6 — no quiet failure).
            StreamMapError::EmptyCompletion => {
                let provider = format!(
                    "goose returned an empty completion (no assistant text, no tool calls) — \
                     the claude-code rate-limit signature (the throttled provider returns \
                     nothing and goose exits 0); goose stderr tail: {}",
                    tail(stderr_full),
                );
                tracing::warn!(
                    detail = %provider,
                    "goose empty completion — rate-limit-shaped, retrying with backoff (FIX-004)",
                );
                AttemptOutcome::Retry(RetryReason {
                    tag: TAG_RATE_LIMITED.to_owned(),
                    detail: provider.clone(),
                    agent_error: Some(provider),
                })
            }
            // A bad worker verdict — RETRY (up to the cap). The worker DID
            // respond (or act) but the payload was unparseable. Fold in the
            // stderr tail so a genuine parse failure still shows what goose
            // actually said (S6 — surface, don't swallow). When that stderr
            // carries 429 markers the parse failure is a SYMPTOM of the
            // provider throttle — tag it `rate_limited` (with the provider
            // evidence on the `agent_error` channel) so retry exhaustion does
            // not die under a generic `verdict_parse` that blames the worker.
            StreamMapError::VerdictParse(detail) => {
                let detail = format!("{detail}; goose stderr tail: {}", tail(stderr_full));
                if rate_limit_shaped(stderr_full) {
                    tracing::warn!(
                        detail = %detail,
                        "goose verdict parse failed with 429 markers in stderr — \
                         rate-limit-shaped, retrying with backoff",
                    );
                    AttemptOutcome::Retry(RetryReason {
                        tag: TAG_RATE_LIMITED.to_owned(),
                        detail: detail.clone(),
                        agent_error: Some(detail),
                    })
                } else {
                    AttemptOutcome::Retry(RetryReason {
                        tag: "verdict_parse".to_owned(),
                        detail,
                        agent_error: None,
                    })
                }
            }
            // A transient agent error (HTTP 503, rate-limit, …) — RETRY (up to
            // the cap), exactly like `VerdictParse`. The plan (Task 7.2/7.3) +
            // the Goose spike both say "any other `error` line → retry 2×"; a
            // transient provider error must NOT hard-fail the phase on its
            // first occurrence (review C-cr-1). The agent's error string is
            // carried so the post-exhaustion terminal `Fail` +
            // `ErrorEncountered` name the real cause — and an error string
            // carrying 429 markers is tagged `rate_limited` outright.
            StreamMapError::AgentError(detail) => {
                let tag = if rate_limit_shaped(&detail) {
                    TAG_RATE_LIMITED
                } else {
                    "goose_stream_error"
                };
                AttemptOutcome::Retry(RetryReason {
                    tag: tag.to_owned(),
                    detail: detail.clone(),
                    agent_error: Some(detail),
                })
            }
            // A context overflow — a retry just overflows again. Terminal Fail,
            // NO retry.
            StreamMapError::ContextOverflow => {
                self.send_fail(
                    "context_overflow",
                    "the worker's context window overflowed",
                    "reduce the phase's context size or split the task",
                )
                .await;
                AttemptOutcome::Terminal
            }
            // A non-JSON / structurally-unparseable line encountered during
            // streaming. If Goose exited non-zero, the transport error is a
            // symptom of the real failure — report the exit code + stderr
            // instead of blaming stream corruption. If Goose exited 0, this is
            // genuine mid-stream corruption → `stream_corrupt`.
            StreamMapError::Transport(detail) => {
                if exit_status.is_some_and(|s| !s.success()) {
                    tracing::error!(
                        detail = %detail,
                        "goose transport error on a non-zero exit — reporting exit cause",
                    );
                    self.send_fail(
                        "goose_exited_nonzero",
                        &nonzero_exit_why(exit_status, stderr_full),
                        "inspect the goose stderr above — the recipe or environment \
                         rejected the run (e.g. missing `prompt` field, bad provider \
                         key, or unreadable recipe)",
                    )
                    .await;
                } else {
                    tracing::error!(
                        detail = %detail,
                        "goose stream corrupt — unparseable line (exit 0)",
                    );
                    self.send_fail(
                        "stream_corrupt",
                        &format!("{detail}; stderr tail: {}", tail(stderr_full)),
                        "the goose stream-json output was malformed; check the goose \
                         version and configuration",
                    )
                    .await;
                }
                AttemptOutcome::Terminal
            }
        }
    }

    /// Build + spawn the `goose run` child.
    ///
    /// `goose run --recipe <path> --output-format stream-json` — never
    /// `--output` (the spike's corrected flag) and never `--no-session`
    /// (spike §Q4 — the `complete` token counts come from the session record).
    fn spawn_goose(&self, recipe_path: &PathBuf) -> std::io::Result<Child> {
        Command::new(&self.goose_bin)
            .arg("run")
            .arg("--recipe")
            .arg(recipe_path)
            .arg("--output-format")
            .arg("stream-json")
            // RC1 — the worker acts on the task worktree, not the daemon's cwd.
            .current_dir(worker_cwd(&self.ctx, &self.worktree_root))
            // Perf: hand every worker one shared, warm cargo target dir so each
            // worktree does not rebuild ~1148 crates cold (and concurrent builds
            // do not deadlock on cargo's package-cache lock). Inherited by the
            // worker's child `cargo` invocations. (Resolver lives in
            // `worktree.rs` — `validate::run_command` shares it, OBS-032.)
            .env(
                "CARGO_TARGET_DIR",
                crate::runtime::worktree::resolve_cargo_target_dir(),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // D1 — each goose child leads its OWN process group, so the
            // attempt-timeout path can `killpg` the whole tree: goose's
            // grandchildren (provider CLI subprocesses) hold the stdout/stderr
            // fds and the stalled provider connection; `start_kill` reaches
            // only the direct child.
            .process_group(0)
            // Panic-path backstop — a dropped child is killed, not orphaned.
            .kill_on_drop(true)
            .spawn()
    }

    /// SIGKILL a timed-out goose child's entire PROCESS GROUP (D1).
    ///
    /// [`spawn_goose`](Self::spawn_goose) puts every goose child in its own
    /// process group (`process_group(0)`), so the group id IS the child's pid
    /// and the kill takes out goose's grandchildren too — the provider CLI
    /// subprocesses that hold the pipe fds and the stalled (rate-limit-shaped)
    /// connection, which `start_kill` alone would leave running. A failure is
    /// logged loudly and the caller's `kill_and_reap` remains the backstop for
    /// the direct child.
    #[allow(unsafe_code)] // `libc::killpg` — no safe std/tokio surface kills a process group
    fn kill_process_tree(child: &Child) {
        let Some(pid) = child.id() else {
            // Already reaped — nothing to kill.
            return;
        };
        // SAFETY: `killpg` is async-signal-safe and takes no pointers; the
        // target is the child's OWN process group (created by
        // `process_group(0)` at spawn), never the daemon's.
        let rc = unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
        if rc != 0 {
            tracing::warn!(
                pid,
                error = %std::io::Error::last_os_error(),
                "killpg on the timed-out goose process group failed — \
                 falling back to the direct-child kill",
            );
        }
    }

    /// Kill + reap a `goose` child while draining stdout *concurrently*, all
    /// under [`CANCEL_GRACE`] (review C2 / C-rt-1 / C-rt-S1 — never `wait()`
    /// with stdout undrained).
    ///
    /// `reader` is the line reader handed back from [`drain_stdout`]. A `goose`
    /// killed mid-generation can still hold buffered bytes (or be actively
    /// writing) on its stdout pipe; if that pipe fills, the child write-blocks
    /// and `wait()` deadlocks. So a background task keeps reading (and
    /// discarding) stdout to EOF while [`reap_child`](Self::reap_child) does the
    /// kill + bounded wait. The drained bytes are thrown away — the run is over
    /// — but the pipe never fills.
    ///
    /// Returns the exit status from [`reap_child`](Self::reap_child) so the
    /// caller can distinguish a non-zero Goose exit from other failure modes.
    async fn kill_and_reap<R>(
        &self,
        child: &mut Child,
        reader: BufReader<R>,
    ) -> Option<std::process::ExitStatus>
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        // Drain stdout to EOF in the background so a killed-mid-write `goose`
        // cannot wedge `wait()` on a full pipe. Abortable: a grandchild that
        // inherited the stdout fd can hold the pipe open past `goose`'s own
        // death, so the drain is bounded by the same grace window as `wait()`.
        let drain_task = tokio::spawn(async move { discard_to_eof(reader).await });
        let drain_abort = drain_task.abort_handle();

        let status = Self::reap_child(child).await;

        // The child is reaped (or being dropped). The stdout drain has done its
        // job — abort it rather than awaiting an EOF a grandchild may withhold.
        drain_abort.abort();
        drop(drain_task);
        status
    }

    /// `start_kill()` then a bounded `wait()` under [`CANCEL_GRACE`].
    ///
    /// Safe to call on an already-exited child — `start_kill` then errors
    /// benignly and `wait()` returns the status. This is the bare reap; a
    /// caller with a live stdout pipe MUST use [`kill_and_reap`](Self::kill_and_reap)
    /// so the pipe is drained concurrently (an undrained pipe deadlocks the
    /// dying child — review C-rt-1).
    ///
    /// Returns the exit status so callers can distinguish a non-zero Goose exit
    /// (a recipe error, a missing `prompt`, a provider failure) from a crash or
    /// clean exit.
    async fn reap_child(child: &mut Child) -> Option<std::process::ExitStatus> {
        if let Err(e) = child.start_kill() {
            // The child already exited — benign, but logged (no quiet failure).
            tracing::debug!(error = %e, "start_kill on an already-exited goose child");
        }
        match tokio::time::timeout(CANCEL_GRACE, child.wait()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "waiting on the goose child failed");
                None
            }
            Err(_) => {
                // Still alive after the grace — `kill_on_drop` reaps it when
                // the `Child` drops.
                tracing::warn!("goose child did not die within the cancel grace window");
                None
            }
        }
    }

    /// Send an `ErrorEncountered` event for an agent error that exhausted the
    /// retry budget (review C-cr-1).
    ///
    /// This is NOT a terminal event — the terminal `PhaseCompleted{Fail}`
    /// follows it (via [`send_fail`](Self::send_fail)). It exists so a failed
    /// phase still surfaces a `boi.error` span event / fingerprint; the mapper
    /// no longer emits one on the first occurrence of a (now retryable) agent
    /// error.
    async fn send_error_encountered(&self, agent_error: &str) {
        let first_line = agent_error.lines().next().unwrap_or(agent_error).trim();
        let event = BoiEvent::ErrorEncountered {
            spec_id: self.ctx.spec_id.clone(),
            task_id: self.ctx.task_id.clone(),
            // G24.1 — the phase the error occurred in.
            phase: self.ctx.phase.clone(),
            error: "goose stream error".to_owned(),
            why: agent_error.to_owned(),
            fix_proposed: None,
            fingerprint: first_line.to_owned(),
        };
        if self.tx.send(event).await.is_err() {
            tracing::warn!("goose runtime could not send ErrorEncountered — channel closed");
        }
    }

    /// Send a terminal `PhaseCompleted` carrying a `Fail` verdict.
    ///
    /// Every non-clean path funnels through here so the stream ALWAYS ends in
    /// exactly one `PhaseCompleted` (G21.5).
    async fn send_fail(&self, error: &str, why: &str, fix: &str) {
        let event = BoiEvent::PhaseCompleted {
            phase_run_id: self.ctx.phase_run_id.clone(),
            spec_id: self.ctx.spec_id.clone(),
            task_id: self.ctx.task_id.clone(),
            phase: self.ctx.phase.clone(),
            verdict: WorkerVerdict {
                synopsis: format!("goose phase `{}` failed", self.ctx.phase),
                outcome: VerdictOutcome::Fail {
                    error: error.to_owned(),
                    why: why.to_owned(),
                    fix: fix.to_owned(),
                },
            },
            tokens_in: 0,
            tokens_out: 0,
            duration_ms: 0,
        };
        // A closed channel here means the orchestrator's drain already ended —
        // log and move on (no one is left to receive the terminal event).
        if self.tx.send(event).await.is_err() {
            tracing::warn!("goose runtime could not send the terminal Fail — channel closed");
        }
    }
}

/// The outcome of draining one `goose` child's stdout.
enum DrainOutcome {
    /// The stream ended (stdout closed). `saw_complete` distinguishes a clean
    /// `complete`-terminated stream from a `goose` crash.
    StreamEnded {
        /// Whether a `complete` event was mapped before stdout closed.
        saw_complete: bool,
    },
    /// A `StreamMapper` error — routed by the per-error-kind policy.
    MapError(StreamMapError),
    /// The `cancel` token fired mid-stream.
    Canceled,
    /// The attempt exceeded its per-attempt wall-clock timeout (D1) — the
    /// goose process tree is killed and the attempt retried.
    TimedOut,
    /// The downstream receiver was dropped — nobody is draining.
    ReceiverGone,
}

/// Read one `\n`-terminated line into `buf`, capping at [`MAX_LINE_BYTES`].
///
/// Returns the number of bytes read (`0` = EOF). A line that exceeds the cap is
/// an `Err` — a runaway line is never buffered unbounded (review S1/S11).
async fn read_capped_line<R>(reader: &mut BufReader<R>, buf: &mut Vec<u8>) -> std::io::Result<usize>
where
    R: AsyncRead + Unpin,
{
    let mut total = 0;
    loop {
        let mut chunk = Vec::new();
        let n = reader.read_until(b'\n', &mut chunk).await?;
        if n == 0 {
            // EOF — return what we have (0 if the line was empty).
            return Ok(total);
        }
        buf.extend_from_slice(&chunk);
        total += n;
        if total > MAX_LINE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("goose stdout line exceeded the {MAX_LINE_BYTES}-byte cap"),
            ));
        }
        // A complete line ends in `\n`; a partial read (no `\n`) loops.
        if chunk.last() == Some(&b'\n') {
            return Ok(total);
        }
    }
}

/// Read a stdout reader to EOF, discarding every byte.
///
/// Used by [`PhaseDriver::kill_and_reap`] to keep a killed `goose`'s stdout
/// pipe drained while `child.wait()` runs — an undrained full pipe deadlocks
/// the dying child (review C-rt-1 / C-rt-S1). The bytes are discarded: the run
/// is over, only the pipe-not-filling matters. A read error just ends the
/// drain — the caller's grace-window timeout is the real backstop.
async fn discard_to_eof<R>(mut reader: BufReader<R>)
where
    R: AsyncRead + Unpin,
{
    let mut scratch = [0u8; 8192];
    loop {
        match tokio::io::AsyncReadExt::read(&mut reader, &mut scratch).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

/// Drain a child pipe to end-of-stream into a `String` (for the stderr tail).
async fn drain_to_string<R>(pipe: R) -> String
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(pipe);
    let mut buf = Vec::new();
    let mut chunk = Vec::new();
    loop {
        chunk.clear();
        match reader.read_until(b'\n', &mut chunk).await {
            Ok(0) => break,
            Ok(_) => buf.extend_from_slice(&chunk),
            Err(e) => {
                buf.extend_from_slice(format!("(stderr read error: {e})").as_bytes());
                break;
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Build the `why` string for a `Fail{goose_exited_nonzero}` verdict.
///
/// Carries the exit code and the FULL captured stderr so the operator can see
/// the real Goose error (e.g. "Error: no text provided for prompt in headless
/// mode") without any truncation.
fn nonzero_exit_why(exit_status: Option<std::process::ExitStatus>, stderr: &str) -> String {
    let code = match exit_status.and_then(|s| s.code()) {
        Some(c) => format!("{c}"),
        None => "(killed by signal)".to_owned(),
    };
    if stderr.trim().is_empty() {
        format!("goose exited with code {code} (no stderr output)")
    } else {
        format!(
            "goose exited with code {code}; stderr:\n{}",
            stderr.trim_end()
        )
    }
}

/// The last few lines of a captured stderr, for an error message.
fn tail(text: &str) -> String {
    const MAX_LINES: usize = 8;
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(MAX_LINES);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::context::{SpecContract, TaskContract, Verification};
    use crate::types::ids::{PhaseRunId, SpecId, TaskId};
    use std::path::Path;
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
                std::env::temp_dir().join(format!("boi-goose-{}-{tag}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.path));
        }
    }

    fn phase_run() -> PhaseRunId {
        PhaseRunId::new("P0000001a").unwrap()
    }

    fn execute_phase() -> PhaseDef {
        let toml = std::fs::read_to_string(format!(
            "{}/tests/fixtures/phases/execute.toml",
            env!("CARGO_MANIFEST_DIR"),
        ))
        .unwrap();
        crate::config::parse_phase(&toml).unwrap()
    }

    /// Construct a [`GooseRuntime`] for a test, writing the `execute` phase's
    /// prompt-template file (`execute.md`) into `dir` so the G26.1 template
    /// resolution finds it. `dir` doubles as the recipe dir AND the prompts
    /// dir — a single tempdir per test (the production layout co-locates the
    /// phase TOMLs + templates in `~/.boi/v2/phases/`).
    fn goose_runtime(bin: PathBuf, dir: &Path) -> GooseRuntime {
        std::fs::write(
            dir.join("execute.md"),
            "Implement the task described in <phase_context>.",
        )
        .unwrap();
        // The worker's `goose` runs in its task worktree (RC1) — pre-create the
        // worktree `phase_ctx()` resolves to, rooted in this test's dir.
        std::fs::create_dir_all(crate::runtime::worktree::task_worktree(
            dir,
            &SpecId::new("S0000001a").unwrap(),
            &TaskId::new("T0000001a").unwrap(),
        ))
        .unwrap();
        GooseRuntime::with_worktree_root(
            bin,
            dir.to_path_buf(),
            dir.to_path_buf(),
            dir.to_path_buf(),
        )
        // FIX-004: zero backoff so the retry-loop tests don't sleep real
        // wall-clock between fake-goose spawns.
        .with_retry_backoff_base(Duration::ZERO)
    }

    /// An assistant `message` whose text is present but is NOT a parseable
    /// `WorkerVerdict` — exercises the `verdict_parse` path (the worker DID
    /// respond, just malformed) as distinct from an empty completion.
    fn malformed_verdict_message() -> String {
        serde_json::json!({
            "type": "message",
            "message": {
                "role": "assistant",
                "content": [{ "type": "text", "text": "I think the plan looks fine, shipping." }]
            }
        })
        .to_string()
    }

    fn phase_ctx() -> PhaseContext {
        PhaseContext {
            spec_id: SpecId::new("S0000001a").unwrap(),
            task_id: Some(TaskId::new("T0000001a").unwrap()),
            phase: "execute".into(),
            phase_run_id: phase_run(),
            iteration: 0,
            spec_contract: SpecContract {
                scope: "demo".into(),
                workspace: PathBuf::from("/repo"),
                base_branch: "main".into(),
                exclusions: vec![],
                verifications: vec![],
                must_emit: vec![],
            },
            task_contract: Some(TaskContract {
                behavior: "do it".into(),
                verifications: vec![Verification::Command {
                    name: None,
                    command: "true".into(),
                }],
            }),
            tasks: vec![],
            skills: vec![],
            decisions: vec![],
            prior_phase_runs: vec![],
        }
    }

    /// `worker_cwd` for a task-level phase is the task worktree — the Goose
    /// `developer` tools must operate there, not in the daemon's cwd (RC1).
    #[test]
    fn test_l2_worker_cwd_is_the_task_worktree() {
        let root = PathBuf::from("/tmp/wt-root");
        let ctx = phase_ctx();
        let expected = crate::runtime::worktree::task_worktree(
            &root,
            &ctx.spec_id,
            ctx.task_id.as_ref().unwrap(),
        );
        assert_eq!(worker_cwd(&ctx, &root), expected);
    }

    /// `worker_cwd` for a spec-level phase (no task) is the integration worktree.
    #[test]
    fn test_l2_worker_cwd_spec_level_is_the_integration_worktree() {
        let root = PathBuf::from("/tmp/wt-root");
        let mut ctx = phase_ctx();
        ctx.task_id = None;
        let expected = crate::runtime::worktree::integration_worktree(&root, &ctx.spec_id);
        assert_eq!(worker_cwd(&ctx, &root), expected);
    }

    /// A valid worker-verdict assistant `message` line — a `complete`-
    /// terminating stream's final assistant message carries this.
    fn passing_verdict_message() -> String {
        serde_json::json!({
            "type": "message",
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "text",
                    "text": "{\"synopsis\":\"did it\",\"outcome\":{\"type\":\"passing\",\"evidence\":{\"files_touched\":[],\"verifications\":[],\"summary\":\"ok\"}}}"
                }]
            }
        })
        .to_string()
    }

    /// A `complete` event line — the terminal stream event.
    fn complete_line() -> String {
        serde_json::json!({ "type": "complete" }).to_string()
    }

    /// A `complete` event line carrying real token counts — used to exercise
    /// the OBS-024 cost-injection path. `total_tokens` is what Goose typically
    /// emits when the session record has the figure; `input_tokens` /
    /// `output_tokens` are the split (spike §Q4 — all three optional).
    fn complete_line_with_tokens(input: i64, output: i64) -> String {
        serde_json::json!({
            "type": "complete",
            "total_tokens": input + output,
            "input_tokens": input,
            "output_tokens": output,
        })
        .to_string()
    }

    /// A `notification` event line — a non-terminal MCP log.
    fn notification_line() -> String {
        serde_json::json!({ "type": "notification", "extension_id": "boi" }).to_string()
    }

    /// A stateful fake-`goose` stand-in for the real binary.
    ///
    /// The real `goose` is NOT installed (real-`goose` E2E is Phase 10's
    /// Docker+Ollama harness). This builds a stateful shell stand-in: each
    /// attempt's JSONL output is written to a *file* via `std::fs::write` (so
    /// arbitrary JSON — apostrophes, quotes, anything — never passes through
    /// shell quoting), and the script `cat`s the file for the current
    /// invocation, optionally `exit`-ing or `sleep`-ing per the script.
    struct FakeGoose {
        /// The script path — pass this as `goose_bin`.
        bin: PathBuf,
        /// The invocation-counter file.
        counter: PathBuf,
    }

    impl FakeGoose {
        /// Build a fake-`goose` in `dir`. `attempts` is one JSONL-line list per
        /// invocation: invocation N `cat`s `attempts[min(N-1, len-1)]` (the
        /// last entry is reused for any further invocations). `trailer` is an
        /// optional shell snippet appended after the `cat` (e.g. `sleep 30`,
        /// `exit 0`) — empty for a clean exit-0.
        fn new(dir: &Path, attempts: &[Vec<String>], trailer: &str) -> Self {
            Self::with_preamble(dir, attempts, "", trailer)
        }

        /// [`FakeGoose::new`] with a shell `preamble` that runs BEFORE the
        /// stdout `cat`. Needed when a test must guarantee stderr content is
        /// written before the harness reacts to stdout (the harness kills the
        /// child the moment the mapped stream errors, so a post-`cat` stderr
        /// write races the SIGKILL) — mirroring a real `goose`, which logs
        /// provider errors to stderr while it runs.
        fn with_preamble(
            dir: &Path,
            attempts: &[Vec<String>],
            preamble: &str,
            trailer: &str,
        ) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let counter = dir.join("invocations");
            // Write each attempt's JSONL to its own file — no shell quoting.
            for (i, lines) in attempts.iter().enumerate() {
                let body = if lines.is_empty() {
                    String::new()
                } else {
                    format!("{}\n", lines.join("\n"))
                };
                std::fs::write(dir.join(format!("attempt-{i}.jsonl")), body).unwrap();
            }
            let last = attempts.len().saturating_sub(1);
            let script = format!(
                r#"#!/bin/sh
n=$(cat '{counter}' 2>/dev/null || echo 0)
n=$((n + 1))
echo "$n" > '{counter}'
idx=$((n - 1))
if [ "$idx" -gt {last} ]; then idx={last}; fi
{preamble}
cat '{dir}/attempt-'"$idx"'.jsonl'
{trailer}
"#,
                counter = counter.display(),
                dir = dir.display(),
                last = last,
                preamble = preamble,
                trailer = trailer,
            );
            let bin = dir.join("fake-goose.sh");
            std::fs::write(&bin, script).unwrap();
            let mut perms = std::fs::metadata(&bin).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&bin, perms).unwrap();
            Self { bin, counter }
        }

        /// How many times the fake was invoked.
        fn invocations(&self) -> u32 {
            std::fs::read_to_string(&self.counter)
                .map(|s| s.trim().parse().unwrap_or(0))
                .unwrap_or(0)
        }
    }

    /// Drain a `BoxStream<BoiEvent>` into a `Vec`.
    async fn collect(mut stream: BoxStream<'static, BoiEvent>) -> Vec<BoiEvent> {
        let mut out = Vec::new();
        while let Some(e) = stream.next().await {
            out.push(e);
        }
        out
    }

    /// The terminal event of a `GooseRuntime` stream MUST be a `PhaseCompleted`
    /// (G21.5) — assert and return its verdict.
    fn terminal_verdict(events: &[BoiEvent]) -> WorkerVerdict {
        let last = events.last().expect("the stream is never empty (G21.5)");
        let BoiEvent::PhaseCompleted { verdict, .. } = last else {
            unreachable!("the stream must end in PhaseCompleted, got {last:?}");
        };
        verdict.clone()
    }

    /// A fake-`goose` that emits a tool call, the verdict, then a `complete`
    /// → `run_phase` yields the mapped stream ending in
    /// `PhaseCompleted{Passing}`.
    #[tokio::test]
    async fn test_l2_run_phase_yields_mapped_stream_ending_in_phase_completed() {
        let dir = TempDir::new("happy");
        let tool_msg = serde_json::json!({
            "type": "message",
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "toolRequest",
                    "toolCall": { "name": "verify_run", "arguments": { "command": "true" } }
                }]
            }
        })
        .to_string();
        let fake = FakeGoose::new(
            &dir.path,
            &[vec![tool_msg, passing_verdict_message(), complete_line()]],
            "",
        );

        let runtime = goose_runtime(fake.bin.clone(), &dir.path);
        let events =
            collect(runtime.run_phase(execute_phase(), phase_ctx(), CancellationToken::new()))
                .await;

        // A ToolInvoked was mapped from the tool message.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, BoiEvent::ToolInvoked { .. })),
            "the tool message must map to a ToolInvoked, got {events:?}",
        );
        // The stream ends in PhaseCompleted{Passing}.
        let verdict = terminal_verdict(&events);
        assert!(
            matches!(verdict.outcome, VerdictOutcome::Passing { .. }),
            "a complete-terminated stream lifts to Passing, got {verdict:?}",
        );
    }

    /// OBS-024 regression: the goose runtime threads the mapper's
    /// `(tokens_in, tokens_out)` pair through to `PhaseCompleted` honestly.
    ///
    /// The original OBS-024 fix also computed a per-phase USD figure from
    /// the resolved model — that machinery was removed per the
    /// 2026-06-01 "strip $ everywhere, keep tokens everywhere" directive
    /// (the pricing module is deleted; the per-run dollar column is dropped
    /// by migration 0003). What remains worth pinning is the token
    /// pass-through: with 1825 input and 0 output (the OBS-024 live shape
    /// from spec Scf0qrv75, a Goose session-record limitation — spike §Q4)
    /// the mapper's figures must reach `PhaseCompleted` unmolested.
    #[tokio::test]
    async fn test_l2_phase_completed_carries_real_tokens_obs_024() {
        let dir = TempDir::new("obs-024-tokens");
        let fake = FakeGoose::new(
            &dir.path,
            &[vec![
                passing_verdict_message(),
                // The OBS-024 live shape: 1825 input, 0 output (a Goose
                // session-record limitation — spike §Q4 — `output_tokens`
                // can be null/0 even on a clean completion).
                complete_line_with_tokens(1_825, 0),
            ]],
            "",
        );

        let runtime = goose_runtime(fake.bin.clone(), &dir.path);
        let events =
            collect(runtime.run_phase(execute_phase(), phase_ctx(), CancellationToken::new()))
                .await;

        let last = events.last().expect("G21.5 — stream is never empty");
        let BoiEvent::PhaseCompleted {
            tokens_in,
            tokens_out,
            ..
        } = last
        else {
            unreachable!("the stream must end in PhaseCompleted, got {last:?}");
        };

        assert_eq!(*tokens_in, 1_825, "tokens_in carried through unchanged");
        assert_eq!(
            *tokens_out, 0,
            "tokens_out carried through honestly — Goose reported 0",
        );
    }

    /// The retry-recovery path: a fake-`goose` that emits an empty completion
    /// (a bare `complete`, no text/tools) on attempts 1-2 and a valid verdict
    /// on attempt 3 → `Passing`, after exactly 3 spawns. (FIX-004: the empty
    /// completions retry just like the old verdict-parse path did.)
    #[tokio::test]
    async fn test_l2_retry_recovers_on_third_attempt() {
        let dir = TempDir::new("retry-recover");
        // Attempts 1-2: a bare `complete` (no text/tools → EmptyCompletion →
        // retry). Attempt 3: the verdict message then `complete`.
        let fake = FakeGoose::new(
            &dir.path,
            &[
                vec![complete_line()],
                vec![complete_line()],
                vec![passing_verdict_message(), complete_line()],
            ],
            "",
        );

        let runtime = goose_runtime(fake.bin.clone(), &dir.path);
        let events =
            collect(runtime.run_phase(execute_phase(), phase_ctx(), CancellationToken::new()))
                .await;

        let verdict = terminal_verdict(&events);
        assert!(
            matches!(verdict.outcome, VerdictOutcome::Passing { .. }),
            "the 3rd attempt's valid verdict must win, got {verdict:?}",
        );
        assert_eq!(
            fake.invocations(),
            3,
            "2 retries means exactly 3 goose spawns",
        );
    }

    /// FIX-004 → 429-hardening: 3× empty completion (bare `complete`, no text,
    /// no tools) → `Fail{rate_limited}` after exactly 3 spawns. The empty
    /// completion IS the claude-code 429 signature — the throttled provider
    /// returns nothing and goose exits 0 (incident 2026-06-06) — so the
    /// terminal verdict must SAY rate-limited in error AND why AND fix, not
    /// die under a label that blames the worker.
    #[tokio::test]
    async fn test_l2_retry_exhaustion_on_empty_completion_yields_fail_rate_limited() {
        let dir = TempDir::new("retry-exhaust-empty");
        // Every attempt emits a bare `complete` (no text, no tools →
        // EmptyCompletion). The last entry is reused for any further attempts.
        let fake = FakeGoose::new(&dir.path, &[vec![complete_line()]], "");

        let runtime = goose_runtime(fake.bin.clone(), &dir.path);
        let events =
            collect(runtime.run_phase(execute_phase(), phase_ctx(), CancellationToken::new()))
                .await;

        let verdict = terminal_verdict(&events);
        let VerdictOutcome::Fail { error, why, fix } = &verdict.outcome else {
            unreachable!("retry exhaustion must yield a Fail verdict, got {verdict:?}");
        };
        assert_eq!(
            error, "rate_limited",
            "an exhausted empty-completion run is rate-limit-shaped — the \
             error tag must say so, got `{error}`",
        );
        assert!(
            why.contains("RATE LIMITED"),
            "the why must shout RATE LIMITED, got: {why}",
        );
        assert!(
            fix.to_lowercase().contains("rate-limit"),
            "the fix must name the rate-limit window, got: {fix}",
        );
        assert_eq!(
            fake.invocations(),
            3,
            "retry exhaustion is exactly 3 spawns — no extra attempts",
        );
    }

    /// 429-hardening: 3× verdict-parse failure WITH 429 markers in the goose
    /// stderr → `Fail{rate_limited}`, not the generic `verdict_parse`. The
    /// worker "responded" with unusable text while the provider was loudly
    /// throttling on stderr — the throttle is the real cause and the verdict
    /// must say so.
    #[tokio::test]
    async fn test_l2_verdict_parse_exhaustion_with_429_stderr_yields_rate_limited() {
        let dir = TempDir::new("retry-exhaust-429-stderr");
        // The 429 marker is written to stderr BEFORE the stdout lines — like a
        // real goose, which logs the provider error while it runs (a
        // post-stdout stderr write would race the harness's kill).
        let fake = FakeGoose::with_preamble(
            &dir.path,
            &[vec![malformed_verdict_message(), complete_line()]],
            "echo 'HTTP 429 rate_limit_error: usage limit reached' >&2",
            "",
        );

        let runtime = goose_runtime(fake.bin.clone(), &dir.path);
        let events =
            collect(runtime.run_phase(execute_phase(), phase_ctx(), CancellationToken::new()))
                .await;

        let verdict = terminal_verdict(&events);
        let VerdictOutcome::Fail { error, why, .. } = &verdict.outcome else {
            unreachable!("retry exhaustion must yield a Fail verdict, got {verdict:?}");
        };
        assert_eq!(
            error, "rate_limited",
            "a verdict-parse exhaustion with 429 markers in stderr is \
             rate-limit-shaped — got `{error}`",
        );
        assert!(
            why.contains("429"),
            "the why must carry the 429 evidence from stderr, got: {why}",
        );
        assert_eq!(fake.invocations(), 3, "exactly 3 spawns");
    }

    /// D1: a goose attempt that stalls (emits one notification then blocks
    /// forever — the held-429-connection shape from incident 2026-06-06) is
    /// cut off by the per-attempt wall-clock timeout, classified as a
    /// RETRYABLE failure, and after the retry budget the phase fails
    /// `attempt_timeout` with a why/fix that name the timeout — all bounded,
    /// never an indefinite hang.
    #[tokio::test]
    async fn test_l2_attempt_timeout_on_stalled_goose_retries_then_fails_attempt_timeout() {
        let dir = TempDir::new("attempt-timeout");
        // `exec sleep 30` — the goose child IS the stalled process; without
        // the attempt timeout this phase would sit for 30s per attempt.
        let fake = FakeGoose::new(&dir.path, &[vec![notification_line()]], "exec sleep 30");

        let runtime =
            goose_runtime(fake.bin.clone(), &dir.path).with_attempt_timeout(Duration::from_secs(2));
        let start = std::time::Instant::now();
        let events =
            collect(runtime.run_phase(execute_phase(), phase_ctx(), CancellationToken::new()))
                .await;
        let elapsed = start.elapsed();

        // Bounded: 3 attempts × 2s + kill/reap overhead — nowhere near the
        // 30s sleeps. (2s, not lower: under parallel-suite load a fresh
        // /bin/sh can take >1s to get scheduled; a child cut down before it
        // runs records no invocation and the 3-spawn assert below flakes.)
        assert!(
            elapsed < Duration::from_secs(20),
            "the attempt timeout must bound a stalled goose, took {elapsed:?}",
        );
        let verdict = terminal_verdict(&events);
        let VerdictOutcome::Fail { error, why, fix } = &verdict.outcome else {
            unreachable!("timeout exhaustion must yield a Fail verdict, got {verdict:?}");
        };
        assert_eq!(error, "attempt_timeout", "why: {why}");
        assert_eq!(
            fake.invocations(),
            3,
            "a timed-out attempt is RETRYABLE — the full 3-spawn budget runs; why: {why}",
        );
        assert!(
            why.contains("wall-clock"),
            "the why must name the wall-clock timeout, got: {why}",
        );
        assert!(
            fix.contains("BOI_GOOSE_ATTEMPT_TIMEOUT_SECS"),
            "the fix must name the override env var, got: {fix}",
        );
        // A timeout exhaustion is loud: an ErrorEncountered rides alongside
        // the terminal Fail (S6 — the failed phase surfaces a boi.error).
        assert!(
            events
                .iter()
                .any(|e| matches!(e, BoiEvent::ErrorEncountered { .. })),
            "timeout exhaustion must emit ErrorEncountered, got {events:?}",
        );
    }

    /// D1: the timeout kill takes out the goose child's whole PROCESS TREE,
    /// not just the direct child. goose's grandchildren (provider CLI
    /// subprocesses) hold the stdout/stderr fds — killing only the leader
    /// leaves a stalled grandchild running (and holding the rate-limited
    /// connection) past the attempt. Each attempt here spawns a `sleep 60`
    /// grandchild and records its pid; after the phase settles every recorded
    /// grandchild must be dead.
    #[tokio::test]
    #[allow(unsafe_code)] // `libc::kill(pid, 0)` — the no-op-signal liveness probe
    async fn test_l2_attempt_timeout_kills_the_goose_process_tree() {
        let dir = TempDir::new("timeout-tree-kill");
        let gc_file = dir.path.join("grandchildren");
        // `sleep 60 &` is a GRANDchild (no `exec` — the shell stays the
        // child); `wait` keeps the shell alive so the attempt stalls until
        // the timeout fires.
        let trailer = format!("sleep 60 &\necho $! >> '{}'\nwait", gc_file.display());
        let fake = FakeGoose::new(&dir.path, &[vec![notification_line()]], &trailer);

        let runtime =
            goose_runtime(fake.bin.clone(), &dir.path).with_attempt_timeout(Duration::from_secs(2));
        let events =
            collect(runtime.run_phase(execute_phase(), phase_ctx(), CancellationToken::new()))
                .await;

        let verdict = terminal_verdict(&events);
        let VerdictOutcome::Fail { error, .. } = &verdict.outcome else {
            unreachable!("timeout exhaustion must yield a Fail verdict, got {verdict:?}");
        };
        assert_eq!(error, "attempt_timeout");

        let pids: Vec<i32> = std::fs::read_to_string(&gc_file)
            .expect("each attempt recorded its grandchild pid")
            .lines()
            .map(|l| l.trim().parse().expect("a pid per line"))
            .collect();
        assert_eq!(pids.len(), 3, "one grandchild per attempt, got {pids:?}");

        // Every grandchild must die — poll briefly (the SIGKILL is delivered
        // at timeout; reparenting + reaping can lag a few ms).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let alive: Vec<i32> = pids
                .iter()
                .copied()
                // SAFETY: signal 0 probes existence only — no signal is sent.
                .filter(|&pid| unsafe { libc::kill(pid, 0) } == 0)
                .collect();
            if alive.is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "grandchildren survived the process-tree kill: {alive:?}",
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// `attempt_timeout_from` — the `BOI_GOOSE_ATTEMPT_TIMEOUT_SECS` parse:
    /// unset → the 15-min default; a number → that many seconds; `0` →
    /// disabled; garbage → the default (loudly logged, never a silent hang).
    #[test]
    fn test_l1_attempt_timeout_from_env_values() {
        assert_eq!(attempt_timeout_from(None), DEFAULT_ATTEMPT_TIMEOUT);
        assert_eq!(DEFAULT_ATTEMPT_TIMEOUT, Duration::from_secs(15 * 60));
        assert_eq!(attempt_timeout_from(Some("300")), Duration::from_secs(300));
        assert_eq!(attempt_timeout_from(Some(" 60 ")), Duration::from_secs(60));
        assert_eq!(attempt_timeout_from(Some("0")), Duration::ZERO);
        assert_eq!(
            attempt_timeout_from(Some("not-a-number")),
            DEFAULT_ATTEMPT_TIMEOUT,
        );
    }

    /// `rate_limit_shaped` — the 429-marker heuristic the loud-surfacing
    /// paths key on.
    #[test]
    fn test_l1_rate_limit_shaped_detects_429_markers() {
        assert!(rate_limit_shaped("HTTP 429 rate_limit_error"));
        assert!(rate_limit_shaped("provider says Rate Limit exceeded"));
        assert!(rate_limit_shaped("rate-limited by upstream"));
        assert!(!rate_limit_shaped("HTTP 503 Service Unavailable"));
        assert!(!rate_limit_shaped(""));
    }

    /// 3× malformed-but-present verdict → `Fail{verdict_parse}` after exactly 3
    /// spawns. The worker DID respond (assistant text present) but the payload
    /// never parsed — that IS the worker's fault, so `verdict_parse` stands.
    #[tokio::test]
    async fn test_l2_retry_exhaustion_on_malformed_verdict_yields_fail_verdict_parse() {
        let dir = TempDir::new("retry-exhaust-malformed");
        let fake = FakeGoose::new(
            &dir.path,
            &[vec![malformed_verdict_message(), complete_line()]],
            "",
        );

        let runtime = goose_runtime(fake.bin.clone(), &dir.path);
        let events =
            collect(runtime.run_phase(execute_phase(), phase_ctx(), CancellationToken::new()))
                .await;

        let verdict = terminal_verdict(&events);
        let VerdictOutcome::Fail { error, .. } = &verdict.outcome else {
            unreachable!("retry exhaustion must yield a Fail verdict, got {verdict:?}");
        };
        assert_eq!(error, "verdict_parse");
        assert_eq!(fake.invocations(), 3, "exactly 3 spawns");
    }

    /// A fake-`goose` that emits a line then blocks (`exec sleep 30`) + a fired
    /// `cancel` → the cancel returns within `CANCEL_GRACE` and the stream
    /// terminates (does not hang).
    #[tokio::test]
    async fn test_l2_cancel_returns_within_grace_and_stream_terminates() {
        let dir = TempDir::new("cancel");
        // The fake emits one notification then `exec`s a 30s sleep — `exec`
        // replaces the shell with `sleep`, so the `goose`-child PID *is* the
        // sleeping process and `start_kill` reaps the whole thing (mirrors a
        // real `goose` run, which `tokio::process` `exec`s directly). Without
        // `exec`, the `sleep` would be an orphan grandchild holding the stderr
        // pipe — that case is the bounded-stderr-drain fallback in `run_attempt`.
        let fake = FakeGoose::new(&dir.path, &[vec![notification_line()]], "exec sleep 30");

        let runtime = goose_runtime(fake.bin.clone(), &dir.path);
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel_for_task.cancel();
        });

        let start = std::time::Instant::now();
        let events = collect(runtime.run_phase(execute_phase(), phase_ctx(), cancel)).await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < CANCEL_GRACE + Duration::from_secs(3),
            "a canceled phase must return within the grace window, took {elapsed:?}",
        );
        // A canceled phase need not emit a terminal PhaseCompleted (the drain
        // treats a Canceled stream as terminal — registry.rs); the contract is
        // that it does not hang and the stream ends. `collect` returned, so it
        // ended — any events relayed before the cancel are fine.
        let _ = events;
    }

    /// Regression test for C-rt-1 / C-rt-S1 — the undrained-stdout cancel path.
    ///
    /// A `goose` mid-generation writes continuously to stdout. Once the OS pipe
    /// buffer (~64 KB) fills, the child **write-blocks** in `write()`. The OLD
    /// cancel path: `drain_stdout` returned on cancel and **dropped the stdout
    /// reader**, then `kill_and_reap` did `start_kill()` + `wait()` with stdout
    /// fully undrained — the shape the Phase 6 preamble forbids verbatim ("never
    /// `wait()` with stdout undrained"). The fix keeps `discard_to_eof`
    /// draining stdout *concurrently* with `child.wait()` so the dying child's
    /// pipe is never left full while it is being reaped.
    ///
    /// The fake-`goose` here drives a **real, continuous, pipe-filling stdout
    /// stream** — a `while` loop that writes 64-byte lines without end (NOT a
    /// `sleep`; a `sleep` produces no output and structurally cannot fill the
    /// pipe or exercise the undrained-stdout path — exactly why the original
    /// `exec sleep 30` cancel test missed this). The volume is unbounded, so
    /// the pipe is genuinely full and `goose` is genuinely write-blocked when
    /// the cancel fires. The test then asserts the cancel still returns
    /// bounded — `kill_and_reap` drains the full pipe while it reaps, so the
    /// write-blocked child is killed and reaped cleanly rather than left
    /// wedged on a pipe nobody is reading.
    #[tokio::test]
    async fn test_l2_cancel_of_a_write_blocked_goose_drains_stdout_and_is_bounded() {
        let dir = TempDir::new("cancel-writeblocked");
        // `exec` a shell that writes 64-byte lines forever — the goose-child
        // PID *is* this writer, so it is genuinely the process that
        // write-blocks once the ~64 KB pipe fills. Real continuous output, not
        // a `sleep`.
        let fake = FakeGoose::new(
            &dir.path,
            &[vec![notification_line()]],
            "exec sh -c 'while : ; do printf \"%064d\\n\" 0 ; done'",
        );

        let runtime = goose_runtime(fake.bin.clone(), &dir.path);
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        // Fire the cancel after the pipe has had time to fill and the writer
        // has had time to write-block.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            cancel_for_task.cancel();
        });

        let start = std::time::Instant::now();
        let events = collect(runtime.run_phase(execute_phase(), phase_ctx(), cancel)).await;
        let elapsed = start.elapsed();
        let _ = events;

        // The cancel of a write-blocked `goose` must still be bounded — the
        // concurrent stdout drain lets `wait()` reap the killed child rather
        // than the reap racing a full, unread pipe (review C-rt-1 / C-rt-S1).
        assert!(
            elapsed < CANCEL_GRACE + Duration::from_secs(5),
            "the cancel of a write-blocked goose must return within the grace \
             window — kill_and_reap must drain stdout while it reaps, took {elapsed:?}",
        );
    }

    /// A fake-`goose` that exits WITHOUT a `complete` event → the stream still
    /// ends in `PhaseCompleted{Fail{goose_crashed}}` (G21.5).
    #[tokio::test]
    async fn test_l2_stream_end_without_complete_yields_goose_crashed() {
        let dir = TempDir::new("crashed");
        // The fake emits a notification then exits — no `complete` event.
        let fake = FakeGoose::new(&dir.path, &[vec![notification_line()]], "exit 0");

        let runtime = goose_runtime(fake.bin.clone(), &dir.path);
        let events =
            collect(runtime.run_phase(execute_phase(), phase_ctx(), CancellationToken::new()))
                .await;

        let verdict = terminal_verdict(&events);
        let VerdictOutcome::Fail { error, .. } = &verdict.outcome else {
            unreachable!("a complete-less stream must yield Fail, got {verdict:?}");
        };
        assert_eq!(error, "goose_crashed");
    }

    /// A missing `goose` binary → the stream ends in `PhaseCompleted{Fail}`
    /// (a spawn failure is loud and terminal — G21.5).
    #[tokio::test]
    async fn test_l2_missing_goose_binary_yields_terminal_fail() {
        let dir = TempDir::new("no-bin");
        // `goose_runtime` writes the prompt template so the G26.1 resolution
        // succeeds — the test then exercises the *spawn* failure, not a
        // missing-template failure.
        let runtime = goose_runtime(PathBuf::from("/nonexistent/path/to/goose"), &dir.path);
        let events =
            collect(runtime.run_phase(execute_phase(), phase_ctx(), CancellationToken::new()))
                .await;

        let verdict = terminal_verdict(&events);
        let VerdictOutcome::Fail { error, .. } = &verdict.outcome else {
            unreachable!("a missing goose bin must yield Fail, got {verdict:?}");
        };
        assert_eq!(error, "goose_spawn_failed");
    }

    /// G26.1 — a worker phase whose `prompt_template` file is missing yields a
    /// terminal `Fail{prompt_template_unreadable}` BEFORE any `goose` spawn.
    /// Before G26.1 the filename was appended verbatim and the worker ran with
    /// `execute.md` (a filename) as its entire prompt — a silent corruption.
    #[tokio::test]
    async fn test_l2_missing_prompt_template_yields_terminal_fail() {
        let dir = TempDir::new("no-template");
        // A FakeGoose that would pass IF it ran — but the missing template
        // must fail the phase before the spawn. Note: we do NOT call
        // `goose_runtime` (which writes the template); we construct the
        // runtime directly with an empty prompts dir.
        let fake = FakeGoose::new(
            &dir.path,
            &[vec![passing_verdict_message(), complete_line()]],
            "",
        );
        let runtime = GooseRuntime::new(fake.bin.clone(), dir.path.clone(), dir.path.clone());
        // `execute.md` is NOT written into `dir` — the resolution must fail.
        std::fs::remove_file(dir.path.join("execute.md")).ok();

        let events =
            collect(runtime.run_phase(execute_phase(), phase_ctx(), CancellationToken::new()))
                .await;

        let verdict = terminal_verdict(&events);
        let VerdictOutcome::Fail { error, .. } = &verdict.outcome else {
            unreachable!("a missing prompt template must yield Fail, got {verdict:?}");
        };
        assert_eq!(error, "prompt_template_unreadable");
        // The `goose` fake was NEVER invoked — the template gate is pre-spawn.
        assert_eq!(
            fake.invocations(),
            0,
            "a missing prompt template must fail BEFORE the goose spawn",
        );
    }

    /// A Goose run that exits non-zero (e.g. missing `prompt` field, bad
    /// provider key) must yield `Fail{goose_exited_nonzero}` carrying the exit
    /// code and FULL stderr — NOT `stream_corrupt` / "goose/transport bug".
    ///
    /// This is the regression guard for the root-cause misattribution: before
    /// this fix, a non-zero Goose exit with no stream-json output would produce
    /// `Fail{goose_crashed}`, and a Transport error on a non-zero exit would
    /// produce `Fail{stream_corrupt}` blaming "this is a goose/transport bug" —
    /// both hiding the real cause (e.g. "Error: no text provided for prompt in
    /// headless mode").
    #[tokio::test]
    async fn test_l2_nonzero_exit_yields_goose_exited_nonzero_with_full_stderr() {
        let dir = TempDir::new("nonzero-exit");
        // A fake goose that writes nothing to stdout, prints the Goose
        // headless-mode error to stderr, and exits 1 — exactly what happens
        // when the recipe has no `prompt` field.
        let fake = FakeGoose::new(
            &dir.path,
            &[vec![]],
            "echo 'Error: no text provided for prompt in headless mode' >&2\nexit 1",
        );

        let runtime = goose_runtime(fake.bin.clone(), &dir.path);
        let events =
            collect(runtime.run_phase(execute_phase(), phase_ctx(), CancellationToken::new()))
                .await;

        // Exactly one invocation — a non-zero exit is terminal, no retry.
        assert_eq!(
            fake.invocations(),
            1,
            "a non-zero goose exit must not be retried",
        );

        let verdict = terminal_verdict(&events);
        let VerdictOutcome::Fail { error, why, fix } = &verdict.outcome else {
            unreachable!("a non-zero goose exit must yield Fail, got {verdict:?}");
        };

        // Must name the situation honestly with `goose_exited_nonzero`.
        assert_eq!(
            error, "goose_exited_nonzero",
            "a non-zero exit must use `goose_exited_nonzero`, not `{error}`",
        );

        // Must carry the FULL stderr — the operator must see the real Goose error.
        assert!(
            why.contains("Error: no text provided"),
            "the `why` must carry the full stderr, got: {why}",
        );

        // Must include the exit code (1).
        assert!(
            why.contains("code 1") || why.contains("exited with code 1"),
            "the `why` must name the exit code, got: {why}",
        );

        // Must NOT misattribute the failure to stream corruption.
        assert!(
            !why.contains("stream_corrupt"),
            "a non-zero exit must NOT mention stream_corrupt in `why`, got: {why}",
        );
        assert!(
            !fix.contains("goose/transport bug"),
            "a non-zero exit must NOT blame goose/transport, got fix: {fix}",
        );
    }

    /// G26.1 — a worker phase's RESOLVED prompt-template content reaches the
    /// recipe `instructions` (not the bare filename). Drives a real recipe
    /// build + write and asserts the written recipe carries the template body.
    #[tokio::test]
    async fn test_l2_prompt_template_content_reaches_the_recipe() {
        let dir = TempDir::new("template-content");
        let recipe_dir = dir.path.join("recipes");
        std::fs::create_dir_all(&recipe_dir).unwrap();
        let prompts_dir = dir.path.join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        // A distinctive prompt body — easy to find in the written recipe.
        std::fs::write(
            prompts_dir.join("execute.md"),
            "UNIQUE_PROMPT_MARKER: implement the behavior.",
        )
        .unwrap();
        let fake = FakeGoose::new(
            &dir.path,
            &[vec![passing_verdict_message(), complete_line()]],
            "",
        );
        // The worker runs in its task worktree (RC1) — pre-create it.
        std::fs::create_dir_all(crate::runtime::worktree::task_worktree(
            &dir.path,
            &SpecId::new("S0000001a").unwrap(),
            &TaskId::new("T0000001a").unwrap(),
        ))
        .unwrap();
        let runtime = GooseRuntime::with_worktree_root(
            fake.bin.clone(),
            recipe_dir.clone(),
            prompts_dir,
            dir.path.clone(),
        );
        let events =
            collect(runtime.run_phase(execute_phase(), phase_ctx(), CancellationToken::new()))
                .await;

        // The phase passed (the recipe built + the fake goose ran).
        let verdict = terminal_verdict(&events);
        assert!(
            matches!(verdict.outcome, VerdictOutcome::Passing { .. }),
            "the phase must pass, got {verdict:?}",
        );
        // The written recipe carries the RESOLVED prompt body — not `execute.md`.
        let recipe_yaml = std::fs::read_to_string(
            recipe_dir.join(format!("recipe-{}.yaml", phase_run().as_str())),
        )
        .unwrap();
        assert!(
            recipe_yaml.contains("UNIQUE_PROMPT_MARKER"),
            "the resolved prompt-template content must reach the recipe (G26.1)",
        );
    }

    /// A context-overflow `error` line → `Fail{context_overflow}` with NO
    /// retry (the fake runs exactly once).
    #[tokio::test]
    async fn test_l2_context_overflow_does_not_retry() {
        let dir = TempDir::new("overflow");
        // The error string carries an apostrophe ("model's") — the file-based
        // FakeGoose makes that safe (no shell quoting of the JSON).
        let err_line = serde_json::json!({
            "type": "error",
            "error": "This model's maximum context length is 200000 tokens"
        })
        .to_string();
        let fake = FakeGoose::new(&dir.path, &[vec![err_line]], "");

        let runtime = goose_runtime(fake.bin.clone(), &dir.path);
        let events =
            collect(runtime.run_phase(execute_phase(), phase_ctx(), CancellationToken::new()))
                .await;

        let verdict = terminal_verdict(&events);
        let VerdictOutcome::Fail { error, .. } = &verdict.outcome else {
            unreachable!("a context overflow must yield Fail, got {verdict:?}");
        };
        assert_eq!(error, "context_overflow");
        assert_eq!(fake.invocations(), 1, "a context overflow must not retry",);
    }

    /// Regression test for C-cr-1 — a non-overflow `error` line is RETRYABLE.
    ///
    /// The OLD `map_error` returned a terminal `Fail` immediately for any
    /// non-overflow `error`, and the retry arm was reachable only via
    /// `VerdictParse` — so a transient provider error (HTTP 503, a rate-limit)
    /// hard-failed the phase on its FIRST occurrence, contradicting the plan
    /// (Task 7.2/7.3) and the Goose spike ("any other `error` line → retry
    /// 2×").
    ///
    /// This fake-`goose` emits a transient agent `error` on attempts 1-2 and a
    /// valid `complete`-terminated verdict on attempt 3. With the fix the agent
    /// error is a retryable `StreamMapError::AgentError`, so the phase recovers
    /// to `Passing` after exactly 3 spawns. The OLD code would have hard-failed
    /// on attempt 1 (one spawn, a `Fail` verdict).
    #[tokio::test]
    async fn test_l2_transient_agent_error_retries_and_recovers() {
        let dir = TempDir::new("agent-error-recover");
        let err_line = serde_json::json!({
            "type": "error",
            "error": "provider returned HTTP 503 Service Unavailable"
        })
        .to_string();
        let fake = FakeGoose::new(
            &dir.path,
            &[
                vec![err_line.clone()],
                vec![err_line],
                vec![passing_verdict_message(), complete_line()],
            ],
            "",
        );

        let runtime = goose_runtime(fake.bin.clone(), &dir.path);
        let events =
            collect(runtime.run_phase(execute_phase(), phase_ctx(), CancellationToken::new()))
                .await;

        let verdict = terminal_verdict(&events);
        assert!(
            matches!(verdict.outcome, VerdictOutcome::Passing { .. }),
            "a transient agent error must be RETRIED — the 3rd attempt's valid \
             verdict must win, got {verdict:?}",
        );
        assert_eq!(
            fake.invocations(),
            3,
            "a transient agent error retries 2× — exactly 3 spawns (the old \
             code hard-failed on attempt 1)",
        );
    }

    /// Regression test for C-cr-1 — an agent error that persists across every
    /// attempt yields a terminal `Fail` AND an `ErrorEncountered`, after the
    /// full 3-spawn retry budget.
    ///
    /// The terminal `Fail` + `ErrorEncountered` are synthesized by the goose
    /// runtime only AFTER retry exhaustion (the mapper no longer emits them on
    /// the first occurrence). The stream still ends in exactly one
    /// `PhaseCompleted` (G21.5), and a `boi.error` span event still has its
    /// source `ErrorEncountered`.
    #[tokio::test]
    async fn test_l2_persistent_agent_error_fails_after_retry_budget_with_error_encountered() {
        let dir = TempDir::new("agent-error-exhaust");
        // Every attempt emits the same transient-looking agent error — the last
        // entry is reused, so every retry gets it.
        let err_line = serde_json::json!({
            "type": "error",
            "error": "provider returned HTTP 503 Service Unavailable"
        })
        .to_string();
        let fake = FakeGoose::new(&dir.path, &[vec![err_line]], "");

        let runtime = goose_runtime(fake.bin.clone(), &dir.path);
        let events =
            collect(runtime.run_phase(execute_phase(), phase_ctx(), CancellationToken::new()))
                .await;

        // The retry budget was fully spent — exactly 3 spawns.
        assert_eq!(
            fake.invocations(),
            3,
            "a persistent agent error exhausts the 2-retry budget — 3 spawns",
        );
        // An `ErrorEncountered` is emitted alongside the terminal `Fail`.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, BoiEvent::ErrorEncountered { .. })),
            "retry-exhausted agent error must emit ErrorEncountered, got {events:?}",
        );
        // The stream still ends in exactly one terminal PhaseCompleted{Fail}.
        let verdict = terminal_verdict(&events);
        let VerdictOutcome::Fail { error, why, .. } = &verdict.outcome else {
            unreachable!("a retry-exhausted agent error must yield Fail, got {verdict:?}");
        };
        assert_eq!(error, "goose_stream_error");
        assert!(
            why.contains("503"),
            "the Fail verdict must name the agent error, got {why}",
        );
        let phase_completed = events
            .iter()
            .filter(|e| matches!(e, BoiEvent::PhaseCompleted { .. }))
            .count();
        assert_eq!(
            phase_completed, 1,
            "exactly one terminal PhaseCompleted (G21.5)"
        );
    }

    /// The live spawn path injects `CARGO_TARGET_DIR` into the worker's
    /// environment, and `create_dir_all` makes the shared dir exist. This is the
    /// real seam: a spawned `goose` child (and its child `cargo`) inherits it.
    /// (The resolver's pure override-vs-default tests live with it in
    /// `worktree.rs`.)
    #[test]
    fn test_l2_spawned_worker_command_carries_cargo_target_dir() {
        let target = crate::runtime::worktree::resolve_cargo_target_dir();
        assert!(
            target.is_dir(),
            "resolve_cargo_target_dir must ensure the dir exists, got {}",
            target.display(),
        );

        let mut cmd = Command::new("/bin/true");
        cmd.env("CARGO_TARGET_DIR", &target);
        let injected = cmd
            .as_std()
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new("CARGO_TARGET_DIR"))
            .and_then(|(_, v)| v)
            .map(PathBuf::from);
        assert_eq!(
            injected.as_deref(),
            Some(target.as_path()),
            "the spawned worker command's env carries CARGO_TARGET_DIR",
        );
    }
}
