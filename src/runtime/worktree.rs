//! The §5 worktree-per-task mechanic — the bodies of the seven worktree
//! deterministic phases.
//!
//! Each `pub fn` here is one deterministic-phase body; `deterministic.rs`
//! (Task 6.4) wires them into the `DETERMINISTIC_STEPS` table.
//!
//! ## The canonical deterministic-step shape (review disagreement (a))
//!
//! Every step body is a SYNCHRONOUS `fn` taking `ctx: Arc<StepCtx>` by value
//! and returning a boxed future:
//!
//! ```ignore
//! pub fn prepare_spec(ctx: Arc<StepCtx>) -> BoxFuture<'static, Result<StepRun, StepError>>
//! ```
//!
//! An `async fn` does NOT coerce to a `fn` pointer, so the `fn` *item* IS a
//! `DetStep` (Task 6.4) and populates the table directly. All seven worktree
//! bodies (and `validate` in Task 6.3) follow this one shape.
//!
//! ## Paths (§5)
//!
//! - integration branch `spec/<SpecId>/integration` (a deviation from §5's
//!   `spec/<SpecId>` — see [`integration_branch`]), worktree
//!   `<worktree_root>/<SpecId>/integration`
//! - task branch `spec/<SpecId>/<TaskId>`, worktree
//!   `<worktree_root>/<SpecId>/<TaskId>`
//!
//! `StepCtx.worktree_path` is the worktree the *current* step operates in (the
//! Task 6.5 executor sets it from `phase.level`); the worktree root is its
//! grandparent. `prepare_spec` / `verify_in` derive the branch + worktree they
//! must *create* from the spec/task ids.
//!
//! ## `git2` blocks (Phase 6 preamble)
//!
//! Every `git_ops` call blocks; each body wraps it in
//! [`tokio::task::spawn_blocking`]. A `JoinError` from a panicked closure
//! surfaces as a loud [`StepError`] — never a swallowed panic.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::BoxFuture;

use crate::runtime::branch_policy::{self, PolicyVerdict};
use crate::runtime::deterministic::StepRun;
use crate::runtime::git_ops::{self, MergeOutcome, RebaseOutcome};
use crate::types::ids::{SpecId, TaskId};
use crate::types::reasons::ErrorWhyFix;
use crate::types::step::{StepCtx, StepError, StepOutcome};
use crate::types::verdict::Evidence;

/// A worktree-step operation failed in a way that is a *harness* error, not a
/// task `Fail` — a panicked `spawn_blocking`, a missing task id where one was
/// required.
///
/// A `git2` failure is a [`git_ops::GitError`]; a step that *should* fail the
/// task (a merge conflict, a dirty tree) returns `Ok(StepRun { outcome:
/// StepOutcome::Fail … })`, NOT a `WorktreeError`. `WorktreeError` is reserved
/// for the genuinely-broken cases. Converted to `StepError::Worktree` /
/// `StepError::Git` at this module's boundary (G14.1 — `StepError`, a `types/`
/// type, cannot `#[from]` a `runtime/` error).
#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    /// A `git_ops` (libgit2) call failed.
    #[error("git: {0}")]
    Git(#[from] git_ops::GitError),
    /// A task-level worktree phase ran with `StepCtx.task_id == None`.
    #[error("worktree phase '{phase}' requires a task id but none was set")]
    MissingTaskId {
        /// The phase that ran without a task id.
        phase: String,
    },
    /// A `spawn_blocking` closure panicked.
    #[error("git op panicked: {0}")]
    Panicked(String),
}

impl From<WorktreeError> for StepError {
    fn from(e: WorktreeError) -> Self {
        match e {
            WorktreeError::Git(g) => StepError::Git(g.to_string()),
            other => StepError::Worktree(other.to_string()),
        }
    }
}

/// The integration branch name for a spec — `spec/<SpecId>/integration`.
///
/// ## Deviation from §5's `spec/<SpecId>` (a design defect)
///
/// §5 names the integration branch `spec/<SpecId>` and task branches
/// `spec/<SpecId>/<TaskId>`. That exact pair is **unrealizable in git**: with
/// loose-ref storage `refs/heads/spec/<SpecId>` cannot be both a *file* (the
/// integration ref) and a *directory* (the parent of the task refs) — git
/// rejects `git branch spec/S` then `git branch spec/S/T`. Naming the
/// integration branch `spec/<SpecId>/integration` keeps §5's hierarchy intact
/// (everything still nests under `spec/<SpecId>/`), removes the collision, and
/// matches the worktree path, which §5 *already* writes as `<SpecId>/integration`.
pub fn integration_branch(spec_id: &SpecId) -> String {
    format!("spec/{}/integration", spec_id.as_str())
}

/// The task branch name — `spec/<SpecId>/<TaskId>` (§5).
pub fn task_branch(spec_id: &SpecId, task_id: &TaskId) -> String {
    format!("spec/{}/{}", spec_id.as_str(), task_id.as_str())
}

/// The integration worktree path — `<worktree_root>/<SpecId>/integration`.
///
/// Public so the Task 6.5 `DeterministicExecutor` can build a spec-level
/// `StepCtx`'s `worktree_path` without re-deriving the §5 layout.
pub fn integration_worktree(worktree_root: &Path, spec_id: &SpecId) -> PathBuf {
    worktree_root.join(spec_id.as_str()).join("integration")
}

/// The task worktree path — `<worktree_root>/<SpecId>/<TaskId>`.
///
/// Public so the Task 6.5 `DeterministicExecutor` can build a task-level
/// `StepCtx`'s `worktree_path`.
pub fn task_worktree(worktree_root: &Path, spec_id: &SpecId, task_id: &TaskId) -> PathBuf {
    worktree_root.join(spec_id.as_str()).join(task_id.as_str())
}

/// The integration worktree registration NAME — distinct from the path. Git
/// stores worktree admin entries at `<repo>/.git/worktrees/<name>/`; for the
/// BOI integration layout `<root>/<SpecId>/integration` the path-basename is
/// always `"integration"`, which collides across specs (OBS-023). The name
/// is spec-scoped so every spec gets a distinct `.git/worktrees/<name>/`
/// entry. `-` is used instead of `/` because libgit2 rejects `/` in
/// worktree names.
pub fn integration_worktree_name(spec_id: &SpecId) -> String {
    format!("spec-{}-integration", spec_id.as_str())
}

/// The task worktree registration NAME — same rationale as
/// [`integration_worktree_name`]. The TaskId is unique per-spec so a
/// `spec-<SpecId>-task-<TaskId>` name is globally unique across all
/// concurrent BOI runs.
pub fn task_worktree_name(spec_id: &SpecId, task_id: &TaskId) -> String {
    format!("spec-{}-task-{}", spec_id.as_str(), task_id.as_str())
}

/// The default worktree root — `~/.boi/v2/worktrees` (§5 config).
///
/// `$HOME` is read once; if it is unset (a degenerate environment) the path
/// falls back to a relative `.boi/v2/worktrees`. The Task 6.5
/// `DeterministicExecutor::new` uses this; its test-only `with_worktree_root`
/// constructor overrides it with a tempdir.
pub fn default_worktree_root() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".boi").join("v2").join("worktrees")
}

/// The shared, persistent cargo target dir handed to every spawned worker —
/// `~/.boi/v2/cargo-target`.
///
/// Each worker runs `cargo` inside its own per-task worktree. Without a shared
/// `CARGO_TARGET_DIR`, every worktree builds cold (~1148 crates), and concurrent
/// builds deadlock on cargo's package-cache lock — a phase can hang 40+ minutes.
/// Pointing all worktrees at one warm artifact cache lets builds reuse prior
/// compilation. `$HOME` is read once; if unset it falls back to a relative
/// `.boi/v2/cargo-target`.
pub fn default_cargo_target_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".boi").join("v2").join("cargo-target")
}

/// Resolve the `CARGO_TARGET_DIR` value injected into every spawned build
/// child — the `goose` worker spawn path AND `validate`'s verification
/// commands ([`run_command`](crate::runtime::validate::run_command), OBS-032).
///
/// Every child runs `cargo` inside a per-task (or integration) git worktree.
/// Without a shared target dir each worktree builds cold and concurrent builds
/// deadlock on cargo's package-cache lock (a phase can hang 40+ minutes).
/// Pointing all worktrees at one warm artifact cache fixes that.
///
/// Respects a pre-existing `CARGO_TARGET_DIR` in the process environment (it is
/// returned verbatim, overriding the default); otherwise the shared
/// [`default_cargo_target_dir`] is used. The chosen directory is created if
/// missing — a failure to create it is logged loudly (S6) but is non-fatal: the
/// child can still create it on first build.
pub fn resolve_cargo_target_dir() -> PathBuf {
    let dir = cargo_target_dir_for(std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from));

    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            error = %e,
            path = %dir.display(),
            "could not pre-create the shared CARGO_TARGET_DIR",
        );
    }
    dir
}

/// Pure resolution of the shared `CARGO_TARGET_DIR`: an `override_dir` (a
/// pre-existing `CARGO_TARGET_DIR`) wins; otherwise the shared default
/// [`default_cargo_target_dir`]. Split out so the override-vs-default decision
/// is unit-testable without mutating the process environment (`set_var` is
/// unsound multi-threaded).
pub fn cargo_target_dir_for(override_dir: Option<PathBuf>) -> PathBuf {
    override_dir.unwrap_or_else(default_cargo_target_dir)
}

/// The worktree root — `StepCtx.worktree_path` is `<root>/<SpecId>/<leaf>`, so
/// the root is its grandparent. A `worktree_path` too shallow to have a
/// grandparent is a harness error.
fn worktree_root(ctx: &StepCtx) -> Result<PathBuf, WorktreeError> {
    ctx.worktree_path
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            WorktreeError::Panicked(format!(
                "worktree_path {} is too shallow to derive a worktree root",
                ctx.worktree_path.display()
            ))
        })
}

/// Run a blocking `git_ops` closure on the blocking pool; a panic in the
/// closure surfaces as [`WorktreeError::Panicked`], carrying the recovered
/// panic message (NIT — see [`join_error_detail`]).
async fn blocking<T, F>(label: &str, f: F) -> Result<T, WorktreeError>
where
    F: FnOnce() -> Result<T, git_ops::GitError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result.map_err(WorktreeError::Git),
        Err(join) => Err(WorktreeError::Panicked(format!(
            "{label}: {}",
            join_error_detail(join),
        ))),
    }
}

/// A human-readable detail string for a `JoinError` — recovering the panic
/// **payload** (the `panic!` message), not just the bare "task panicked" the
/// `JoinError` `Display` gives (NIT — a swallowed panic message is lost
/// diagnostic data).
///
/// `pub(crate)` — also used by [`crate::runtime::tool_host`]'s
/// `spawn_blocking` site.
pub(crate) fn join_error_detail(join: tokio::task::JoinError) -> String {
    if join.is_cancelled() {
        return "the blocking task was cancelled".to_owned();
    }
    // `into_panic` yields the `Box<dyn Any>` the closure panicked with — the
    // common payload types are `&str` and `String`.
    match join.try_into_panic() {
        Ok(payload) => {
            if let Some(s) = payload.downcast_ref::<&str>() {
                format!("the blocking task panicked: {s}")
            } else if let Some(s) = payload.downcast_ref::<String>() {
                format!("the blocking task panicked: {s}")
            } else {
                "the blocking task panicked (non-string payload)".to_owned()
            }
        }
        Err(join) => format!("the blocking task failed: {join}"),
    }
}

/// A `StepRun` carrying a `StepOutcome::Pass` and no intermediate events.
fn pass(summary: impl Into<String>) -> StepRun {
    StepRun {
        outcome: StepOutcome::Pass {
            evidence: Evidence {
                files_touched: vec![],
                verifications: vec![],
                summary: summary.into(),
                merge_commit_sha: None,
            },
        },
        events: vec![],
    }
}

/// A `StepRun` for a fast-forward merge step — a `StepOutcome::Pass` whose
/// `Evidence` records the merged commit SHA (G25.2).
///
/// The `merge` / `merge_to_integration` steps thread the `Oid` from
/// `git_ops::ff_merge` to here, recording it as `Evidence.merge_commit_sha`
/// on the persisted `merge` phase run (which was always `null` before).
fn pass_with_merge_sha(summary: impl Into<String>, merge_oid: git2::Oid) -> StepRun {
    StepRun {
        outcome: StepOutcome::Pass {
            evidence: Evidence {
                files_touched: vec![],
                verifications: vec![],
                summary: summary.into(),
                merge_commit_sha: Some(merge_oid.to_string()),
            },
        },
        events: vec![],
    }
}

/// A `StepRun` carrying a `StepOutcome::Fail` and no intermediate events.
fn fail(error: impl Into<String>, why: impl Into<String>, fix: impl Into<String>) -> StepRun {
    StepRun {
        outcome: StepOutcome::Fail {
            error_why_fix: ErrorWhyFix {
                error: error.into(),
                why: why.into(),
                fix: fix.into(),
            },
        },
        events: vec![],
    }
}

/// The `task_id` of a task-level step, or [`WorktreeError::MissingTaskId`].
fn require_task(ctx: &StepCtx) -> Result<TaskId, WorktreeError> {
    ctx.task_id
        .clone()
        .ok_or_else(|| WorktreeError::MissingTaskId {
            phase: ctx.phase.clone(),
        })
}

/// The branch-policy hints arrive fully rendered with a `Fix: ` prefix
/// (ready to print at the CLI); `ErrorWhyFix.fix` is its own field, so strip
/// the prefix here rather than render a stuttering "fix: Fix: …".
fn strip_fix_prefix(hint: &str) -> String {
    hint.strip_prefix("Fix: ").unwrap_or(hint).to_owned()
}

/// GitFlow Layer 3 (R-B8) — the runtime branch-policy re-check, the layer
/// that actually carries the R-B10 guarantee ("no engine code path moves a
/// protected ref"). The dispatch gate (Layer 1) and the daemon preflight
/// (Layer 2) are bypassable: direct socket clients skip the CLI, and runtime
/// phases rehydrate `spec_contract` from immutable snapshots a pre-gate
/// binary may have persisted. So [`prepare_spec`] re-checks before any
/// branch is created, and [`merge_spec`] re-checks immediately before EACH
/// `ff_merge` call — a fresh committed-tree read every time
/// (`refs/heads/<base_branch>:.boi-policy.toml`, D-13); TOCTOU is handled by
/// re-reading, never by trusting an earlier verdict.
///
/// The refusal lives HERE, not inside `git_ops::ff_merge` (R-B9): `ff_merge`
/// also serves the task→integration merge path where policy is irrelevant,
/// and staying out of it keeps this program off the dirty-checkout guard's
/// conflict surface.
///
/// `Some(StepRun)` is the refusal — a typed `StepOutcome::Fail` that routes
/// to `SpecFailed` (loud, nothing mutated). `None` means the policy allows
/// the step. The M8 advisory is a dispatch-output (Layer 1) surface; runtime
/// has no operator warning channel, so an advisory-carrying Allow is an
/// allow.
async fn branch_policy_refusal(workspace: &Path, base_branch: &str, at: &str) -> Option<StepRun> {
    let ctx = branch_policy::load_policy(workspace.to_path_buf(), base_branch.to_owned()).await;
    match ctx.verdict(base_branch) {
        PolicyVerdict::Allow { .. } | PolicyVerdict::Skip { .. } => None,
        PolicyVerdict::ProtectedBase { branch, fix_hint } => Some(fail(
            format!("branch policy refuses base branch `{branch}` at {at}"),
            format!(
                ".boi-policy.toml on `{branch}` marks it protected — \
                 the engine never delivers to a protected branch"
            ),
            strip_fix_prefix(&fix_hint),
        )),
        PolicyVerdict::MissingBase { branch, hint } => Some(fail(
            format!("base branch `{branch}` does not exist in the workspace ({at})"),
            "the spec's base_branch has no local head in the workspace repository",
            strip_fix_prefix(&hint),
        )),
        PolicyVerdict::PolicyInvalid { reason } => Some(fail(
            format!("the workspace branch policy could not be read ({at})"),
            reason,
            "correct (or remove) .boi-policy.toml on the spec's base_branch — \
             a present-but-unreadable policy is never ignored (R-B2)",
        )),
    }
}

// ---------------------------------------------------------------------------
// The seven worktree deterministic-phase bodies.
// ---------------------------------------------------------------------------

/// `workspace_prepare` — create the integration branch + worktree off
/// `[contract].base_branch` (§5 step 1). Spec-level.
pub fn prepare_spec(ctx: Arc<StepCtx>) -> BoxFuture<'static, Result<StepRun, StepError>> {
    Box::pin(async move {
        let repo = ctx.spec_contract.workspace.clone();
        // T7 defense-in-depth: `config::spec::parse_spec` already expands `~`
        // and rejects a non-absolute `[contract].workspace`, but a `StepCtx`
        // constructed directly (e.g. by a future caller that skips parse_spec)
        // could still slip a relative path through. Fail loudly here with a
        // typed StepError that names the absolute-path contract — never let
        // git_ops surface a generic "could not find repo" instead.
        if !repo.is_absolute() {
            return Err(StepError::Worktree(format!(
                "spec_contract.workspace `{}` is not an absolute path — \
                 workspace must be absolute (use parse_spec to expand `~` and \
                 validate the path before constructing a StepCtx)",
                repo.display()
            )));
        }
        let base = ctx.spec_contract.base_branch.clone();
        let branch = integration_branch(&ctx.spec_id);
        let root = worktree_root(&ctx).map_err(StepError::from)?;
        let integration = integration_worktree(&root, &ctx.spec_id);

        // GitFlow Layer 3 (R-B8): re-check the workspace branch policy
        // BEFORE any branch is created — beside the T7 re-check above, and
        // for the same reason: a StepCtx can reach this step without ever
        // crossing the dispatch gate.
        if let Some(refusal) = branch_policy_refusal(&repo, &base, "workspace_prepare").await {
            return Ok(refusal);
        }

        let branch_for_create = branch.clone();
        let repo_for_create = repo.clone();
        blocking("create integration branch", move || {
            git_ops::create_branch(&repo_for_create, &branch_for_create, &base)
        })
        .await
        .map_err(StepError::from)?;

        let integration_for_add = integration.clone();
        let integration_name = integration_worktree_name(&ctx.spec_id);
        blocking("add integration worktree", move || {
            git_ops::add_worktree(&repo, &branch, &integration_name, &integration_for_add)
        })
        .await
        .map_err(StepError::from)?;

        Ok(pass(format!(
            "integration branch {} + worktree {} created",
            integration_branch(&ctx.spec_id),
            integration.display()
        )))
    })
}

/// `workspace_verify_in` — create the task branch off integration + the task
/// worktree (§5 step 2), then assert the worktree is clean. Task-level.
///
/// ## Re-entry adoption (audit A2 — design §6 recovery)
///
/// `boi unblock` resumes a verdict-routed block by RESTARTING the task at this
/// phase (`on_task_unblocked`'s no-open-row arm) — with the task branch and
/// worktree SURVIVING from the blocked run. The old unconditional non-force
/// `create_branch` collided with the surviving branch (libgit2 EXISTS), failed
/// the re-entry, and re-blocked the task with a MISLEADING `WorkspaceUnclean`
/// — the documented recovery loop could never succeed. Re-entry now ADOPTS the
/// surviving §5 state instead:
///
/// - branch absent → fresh entry: create branch + worktree (the original path);
/// - branch present + worktree present on the task branch → adopt it (the
///   adoption is recorded in the phase synopsis / log);
/// - branch present + worktree dir MISSING → prune any stale registration and
///   re-add the worktree onto the surviving branch (a crash between
///   `create_branch` and `add_worktree`, or a manual cleanup);
/// - branch present + worktree on the WRONG branch, or dirty → `Fail` with a
///   TRUTHFUL reason naming the real re-entry state and the operator fix —
///   never "a fresh worktree must be clean" for a worktree that is not fresh.
pub fn verify_in(ctx: Arc<StepCtx>) -> BoxFuture<'static, Result<StepRun, StepError>> {
    Box::pin(async move {
        let task_id = require_task(&ctx).map_err(StepError::from)?;
        let repo = ctx.spec_contract.workspace.clone();
        let integration = integration_branch(&ctx.spec_id);
        let branch = task_branch(&ctx.spec_id, &task_id);
        let root = worktree_root(&ctx).map_err(StepError::from)?;
        let worktree = task_worktree(&root, &ctx.spec_id, &task_id);
        let task_name = task_worktree_name(&ctx.spec_id, &task_id);

        // The re-entry probe (audit A2): a surviving task branch means this is
        // an unblock restart, not a fresh entry.
        let branch_for_probe = branch.clone();
        let repo_for_probe = repo.clone();
        let branch_exists = blocking("probe task branch", move || {
            git_ops::branch_exists(&repo_for_probe, &branch_for_probe)
        })
        .await
        .map_err(StepError::from)?;

        let adopted = if branch_exists {
            if worktree.is_dir() {
                // The surviving worktree must actually be the task branch's
                // checkout — anything else is corrupt re-entry state, named
                // truthfully (never WorkspaceUnclean-when-it-isn't).
                let worktree_for_head = worktree.clone();
                let head = blocking("probe surviving worktree HEAD", move || {
                    git_ops::head_branch(&worktree_for_head)
                })
                .await
                .map_err(StepError::from)?;
                if head != branch {
                    return Ok(fail(
                        "surviving task worktree is on the wrong branch at re-entry",
                        format!(
                            "re-entry for task branch `{branch}`: the surviving worktree {} \
                             has `{head}` checked out",
                            worktree.display()
                        ),
                        "check out the task branch in the worktree (or remove the \
                         directory) and run `boi unblock` again",
                    ));
                }
            } else {
                // The branch survived but the worktree directory did not (a
                // crash between `create_branch` and `add_worktree`, or a
                // manual cleanup): prune any stale admin registration, then
                // re-add the worktree onto the surviving branch.
                let repo_for_readd = repo.clone();
                let branch_for_readd = branch.clone();
                let name_for_readd = task_name.clone();
                let worktree_for_readd = worktree.clone();
                blocking("re-add surviving task worktree", move || {
                    // `remove_worktree` is idempotent on a missing directory —
                    // here it only prunes a stale `.git/worktrees/<name>` entry.
                    git_ops::remove_worktree(
                        &repo_for_readd,
                        &name_for_readd,
                        &worktree_for_readd,
                    )?;
                    git_ops::add_worktree(
                        &repo_for_readd,
                        &branch_for_readd,
                        &name_for_readd,
                        &worktree_for_readd,
                    )
                })
                .await
                .map_err(StepError::from)?;
            }
            true
        } else {
            // Fresh entry — the original §5 step-2 path.
            let branch_for_create = branch.clone();
            let repo_for_create = repo.clone();
            let integration_for_create = integration.clone();
            blocking("create task branch", move || {
                git_ops::create_branch(
                    &repo_for_create,
                    &branch_for_create,
                    &integration_for_create,
                )
            })
            .await
            .map_err(StepError::from)?;

            let worktree_for_add = worktree.clone();
            let branch_for_add = branch.clone();
            let name_for_add = task_name.clone();
            let repo_for_add = repo.clone();
            blocking("add task worktree", move || {
                git_ops::add_worktree(
                    &repo_for_add,
                    &branch_for_add,
                    &name_for_add,
                    &worktree_for_add,
                )
            })
            .await
            .map_err(StepError::from)?;
            false
        };

        // Clean-state precondition — fresh or adopted, the worktree must be
        // clean before a worker clocks in.
        let worktree_for_clean = worktree.clone();
        let clean = blocking("verify-in clean check", move || {
            git_ops::is_clean(&worktree_for_clean)
        })
        .await
        .map_err(StepError::from)?;
        if !clean {
            // → routes to TaskBlocked{WorkspaceUnclean} (5a.4 / review C4) —
            // truthful in BOTH framings: the worktree IS unclean; the why/fix
            // name the actual state (audit A2).
            return Ok(if adopted {
                fail(
                    "surviving task worktree has uncommitted changes at re-entry",
                    format!(
                        "re-entry adopted the surviving worktree {} from the blocked \
                         run, but it holds uncommitted changes",
                        worktree.display()
                    ),
                    "commit, stash, or discard the changes in the worktree, then run \
                     `boi unblock` again",
                )
            } else {
                fail(
                    "workspace not clean at verify-in",
                    format!(
                        "the task worktree {} has uncommitted changes",
                        worktree.display()
                    ),
                    "investigate the worktree state; a fresh worktree must be clean",
                )
            });
        }
        Ok(pass(if adopted {
            // The adoption is recorded here — the synopsis IS the phase log
            // entry persisted on the `phase_runs` row (audit A2).
            format!(
                "re-entry: adopted surviving task branch {branch} + worktree {} (clean)",
                worktree.display()
            )
        } else {
            format!(
                "task branch {branch} + worktree {} created and clean",
                worktree.display()
            )
        }))
    })
}

/// `workspace_verify_out` — assert the task worktree is clean after the
/// worker's commit phase (no orphan files left). Task-level.
pub fn verify_out(ctx: Arc<StepCtx>) -> BoxFuture<'static, Result<StepRun, StepError>> {
    Box::pin(async move {
        require_task(&ctx).map_err(StepError::from)?;
        let worktree = ctx.worktree_path.clone();
        let worktree_for_clean = worktree.clone();
        let clean = blocking("verify-out clean check", move || {
            git_ops::is_clean(&worktree_for_clean)
        })
        .await
        .map_err(StepError::from)?;
        if !clean {
            // → routes to TaskBlocked{WorkspaceUnclean}.
            return Ok(fail(
                "workspace not clean at verify-out",
                format!(
                    "the task worktree {} has uncommitted or untracked changes after commit",
                    worktree.display()
                ),
                "commit or remove all changes — the clean-state invariant forbids orphans",
            ));
        }
        Ok(pass(format!(
            "task worktree {} clean at verify-out",
            worktree.display()
        )))
    })
}

/// `commit` — stage every change in the task worktree and commit it to the
/// task branch. Task-level.
///
/// A worktree with nothing to commit is a `Pass` (an idempotent no-op commit),
/// not a `Fail` — a phase that produced no file changes is legal.
pub fn commit(ctx: Arc<StepCtx>) -> BoxFuture<'static, Result<StepRun, StepError>> {
    Box::pin(async move {
        let task_id = require_task(&ctx).map_err(StepError::from)?;
        let worktree = ctx.worktree_path.clone();
        let message = format!(
            "boi: task {} work on spec {}",
            task_id.as_str(),
            ctx.spec_id.as_str()
        );

        let committed = blocking("commit task worktree", move || {
            commit_all(&worktree, &message)
        })
        .await
        .map_err(StepError::from)?;

        Ok(pass(match committed {
            Some(files) => format!("committed {files} file(s) to the task branch"),
            None => "nothing to commit — task worktree had no changes".to_owned(),
        }))
    })
}

/// `merge_to_integration` — FF-merge the task branch into integration; on a
/// non-FF, lazily rebase the task branch onto integration then retry FF (§5
/// step 4). Task-level. The implicit terminal task phase (C4) — `route_task`
/// names it directly; it is NOT in `standard.toml`'s `task_phases`.
pub fn merge_to_integration(ctx: Arc<StepCtx>) -> BoxFuture<'static, Result<StepRun, StepError>> {
    Box::pin(async move {
        let task_id = require_task(&ctx).map_err(StepError::from)?;
        let repo = ctx.spec_contract.workspace.clone();
        let integration = integration_branch(&ctx.spec_id);
        let branch = task_branch(&ctx.spec_id, &task_id);
        let worktree = ctx.worktree_path.clone();

        // 1. Try a straight FF merge.
        let ff = {
            let (repo, into, from) = (repo.clone(), integration.clone(), branch.clone());
            blocking("ff-merge task branch", move || {
                git_ops::ff_merge(&repo, &into, &from)
            })
            .await
            .map_err(StepError::from)?
        };
        if let MergeOutcome::FastForwarded(merged_oid) = ff {
            return Ok(pass_with_merge_sha(
                format!("task branch {branch} fast-forward merged into {integration}"),
                merged_oid,
            ));
        }

        // 2. Non-FF — lazy rebase the task branch onto integration (§5).
        let rebase = {
            let (worktree, onto) = (worktree.clone(), integration.clone());
            blocking("lazy rebase onto integration", move || {
                git_ops::rebase_onto(&worktree, &onto)
            })
            .await
            .map_err(StepError::from)?
        };
        if let RebaseOutcome::Conflicts(files) = rebase {
            // → orchestrator routes this Fail → TaskBlocked{MergeConflict}
            //   (5a.4 / review C4). Name the conflicted files.
            let listed = files
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Ok(fail(
                format!("merge conflict rebasing {branch} onto {integration}"),
                format!("conflicting files: {listed}"),
                "resolve the conflicts in the task worktree, then re-run the merge",
            ));
        }

        // 3. Rebase was clean — retry the FF (it must succeed now).
        let retry = {
            let (repo, into, from) = (repo, integration.clone(), branch.clone());
            blocking("ff-merge after rebase", move || {
                git_ops::ff_merge(&repo, &into, &from)
            })
            .await
            .map_err(StepError::from)?
        };
        match retry {
            MergeOutcome::FastForwarded(merged_oid) => Ok(pass_with_merge_sha(
                format!("task branch {branch} rebased then fast-forward merged into {integration}"),
                merged_oid,
            )),
            MergeOutcome::NotFastForwardable => Ok(fail(
                format!("merge of {branch} into {integration} failed after a clean rebase"),
                "a fast-forward was still not possible after rebasing — integration moved again"
                    .to_owned(),
                "re-run the merge so the task branch rebases onto the new integration HEAD",
            )),
        }
    })
}

/// Reduce a `merge`-phase [`git_ops::ff_merge`] error to the step contract:
/// the A1 dirty-operator-checkout refusal ([`git_ops::GitError::TargetCheckoutDirty`])
/// becomes a LOUD merge-phase `Fail` verdict — the spec halts with the merged
/// work intact on the integration branch. Every other error stays a harness
/// [`StepError`].
///
/// The verdict's `fix` names ONLY recovery that is actually possible (review
/// M1 findings 2+5): a spec-level merge `Fail` is TERMINAL — merge.toml's
/// `[on.fail]` has no `next`, so `route_spec`'s Fail arm returns Halt and the
/// orchestrator emits `SpecFailed`; `failed` has no exit edge (§6), `boi
/// unblock` is task-scoped (no task is blocked here — all are `passing` at
/// merge time) and the daemon's A2 validation refuses unblock under a
/// terminal spec. The earlier text promised exactly that dead loop
/// ("re-run the merge (boi unblock / retry)") — on this system operator
/// guidance is executed by agents, so an impossible printed recovery is a
/// bug, not a nit. The real recovery: land the surviving integration branch
/// manually, or re-dispatch the spec.
///
/// A `Fail` (not a `StepError`) because this is the "step that *should* fail
/// the task" case from [`WorktreeError`]'s contract: the workspace state is
/// an operator condition to act on, not a broken harness.
fn dirty_workspace_to_fail(error: WorktreeError, ctx: &StepCtx) -> Result<StepRun, StepError> {
    match error {
        WorktreeError::Git(e @ git_ops::GitError::TargetCheckoutDirty { .. }) => {
            let integration = integration_branch(&ctx.spec_id);
            Ok(fail(
                format!(
                    "merge into {} refused: the workspace checkout is dirty",
                    ctx.spec_contract.base_branch
                ),
                e.to_string(),
                format!(
                    "commit or stash the uncommitted changes in {workspace}, then \
                     land the merged work manually — fast-forward {base} to \
                     {integration} (`git merge --ff-only {integration}` with \
                     {base} checked out) — or re-dispatch the spec; a failed \
                     merge phase is spec-terminal, so no `boi` command re-runs \
                     it. The merged work is intact on {integration}",
                    workspace = ctx.spec_contract.workspace.display(),
                    base = ctx.spec_contract.base_branch,
                ),
            ))
        }
        other => Err(StepError::from(other)),
    }
}

/// `merge` — FF-merge the integration branch into `[contract].base_branch` (§5
/// step 5, the `delivery` field). Spec-level.
///
/// v1.0 implements the `merge` delivery (FF into the base branch). The `pr` /
/// `branch-only` deliveries are Phase 9 CLI concerns; this step does the merge.
///
/// `[contract].base_branch` is checked out in the OPERATOR's own workspace —
/// NOT a BOI-owned worktree — so both `ff_merge` call sites route their errors
/// through `dirty_workspace_to_fail`: a dirty operator checkout halts the
/// merge with an actionable `Fail` verdict instead of force-syncing over the
/// operator's uncommitted work (audit A1 / OBS-030).
pub fn merge_spec(ctx: Arc<StepCtx>) -> BoxFuture<'static, Result<StepRun, StepError>> {
    Box::pin(async move {
        let repo = ctx.spec_contract.workspace.clone();
        let base = ctx.spec_contract.base_branch.clone();
        let integration = integration_branch(&ctx.spec_id);
        let worktree = ctx.worktree_path.clone();

        // GitFlow Layer 3 (R-B8): fresh policy re-check immediately before
        // the ref-moving `ff_merge`. The refusal lives here, NOT inside
        // `git_ops::ff_merge` (R-B9 — ff_merge also serves the
        // task→integration path).
        if let Some(refusal) = branch_policy_refusal(&repo, &base, "merge").await {
            return Ok(refusal);
        }

        // 1. Try a straight FF merge.
        let ff = {
            let (repo, into, from) = (repo.clone(), base.clone(), integration.clone());
            let result = blocking("merge integration into base", move || {
                git_ops::ff_merge(&repo, &into, &from)
            })
            .await;
            match result {
                Ok(outcome) => outcome,
                // A1: a dirty operator checkout is a loud Fail, not a harness error.
                Err(e) => return dirty_workspace_to_fail(e, &ctx),
            }
        };
        if let MergeOutcome::FastForwarded(merged_oid) = ff {
            return Ok(pass_with_merge_sha(
                format!("integration branch {integration} fast-forward merged into {base}"),
                merged_oid,
            ));
        }

        // 2. Non-FF — `base` advanced concurrently while the spec ran (the
        //    Sgq7hdyfn race). Lazy-rebase the integration branch onto the
        //    updated base, mirroring `merge_to_integration`'s strategy: the
        //    operator-instruction the old failure message named ("rebase the
        //    integration branch onto the updated base branch and re-merge")
        //    is now performed automatically. Only an actual content conflict
        //    escalates.
        let rebase = {
            let (worktree, onto) = (worktree.clone(), base.clone());
            blocking("lazy rebase integration onto base", move || {
                git_ops::rebase_onto(&worktree, &onto)
            })
            .await
            .map_err(StepError::from)?
        };
        if let RebaseOutcome::Conflicts(files) = rebase {
            let listed = files
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            // Honest-recovery contract (review M1 findings 2+5): this Fail is
            // spec-terminal — never print a `boi`-retry that cannot run.
            return Ok(fail(
                format!("merge conflict rebasing {integration} onto {base}"),
                format!("conflicting files: {listed}"),
                format!(
                    "resolve manually: rebase {integration} onto {base} and \
                     fast-forward, or re-dispatch the spec — a failed merge \
                     phase is spec-terminal; the work is intact on {integration}"
                ),
            ));
        }

        // R-B8: the rebase above is a time window — re-read the policy
        // before the SECOND ff_merge call site too (a marker landing on the
        // base mid-step must refuse here, not ride the retry through).
        if let Some(refusal) =
            branch_policy_refusal(&repo, &base, "merge (post-rebase retry)").await
        {
            return Ok(refusal);
        }

        // 3. Rebase was clean — retry the FF; it must succeed now.
        let retry = {
            let (repo, into, from) = (repo, base.clone(), integration.clone());
            let result = blocking("merge integration into base after rebase", move || {
                git_ops::ff_merge(&repo, &into, &from)
            })
            .await;
            match result {
                Ok(outcome) => outcome,
                // A1 again — the guard covers the post-rebase retry too.
                Err(e) => return dirty_workspace_to_fail(e, &ctx),
            }
        };
        match retry {
            MergeOutcome::FastForwarded(merged_oid) => Ok(pass_with_merge_sha(
                format!(
                    "integration branch {integration} rebased then fast-forward merged into {base}"
                ),
                merged_oid,
            )),
            MergeOutcome::NotFastForwardable => Ok(fail(
                format!("merge of {integration} into {base} failed after a clean rebase"),
                format!(
                    "a fast-forward was still not possible after rebasing — {base} moved again"
                ),
                // Honest-recovery contract (review M1 findings 2+5): this
                // Fail is spec-terminal — manual landing or re-dispatch only.
                format!(
                    "{base} is advancing concurrently — land the work manually \
                     (`git merge --ff-only {integration}` with {base} checked \
                     out) or re-dispatch the spec; a failed merge phase is \
                     spec-terminal. The work is intact on {integration}"
                ),
            )),
        }
    })
}

/// `teardown` — remove the integration and task worktrees (§5 step 6).
/// Spec-level. Best-effort: a worktree already gone is not an error.
pub fn teardown(ctx: Arc<StepCtx>) -> BoxFuture<'static, Result<StepRun, StepError>> {
    Box::pin(async move {
        let repo = ctx.spec_contract.workspace.clone();
        let root = worktree_root(&ctx).map_err(StepError::from)?;
        let integration = integration_worktree(&root, &ctx.spec_id);
        let spec_root = root.join(ctx.spec_id.as_str());

        let repo_for_rm = repo.clone();
        let integration_for_rm = integration.clone();
        let integration_name = integration_worktree_name(&ctx.spec_id);
        blocking("remove integration worktree", move || {
            git_ops::remove_worktree(&repo_for_rm, &integration_name, &integration_for_rm)
        })
        .await
        .map_err(StepError::from)?;

        // Remove every task worktree directory under the spec root. The task
        // worktrees are siblings of `integration` under `<root>/<SpecId>/`.
        let spec_id_for_rm = ctx.spec_id.clone();
        let removed = blocking("remove task worktrees", move || {
            remove_task_worktrees(&repo, &spec_id_for_rm, &spec_root, &integration)
        })
        .await
        .map_err(StepError::from)?;

        Ok(pass(format!(
            "removed the integration worktree and {removed} task worktree(s)"
        )))
    })
}

// ---------------------------------------------------------------------------
// Blocking helpers — called only inside `spawn_blocking`.
// ---------------------------------------------------------------------------

/// Stage every change in `worktree` and commit it. Returns `Some(file_count)`
/// when a commit was made, `None` when the worktree had nothing to commit.
///
/// `pub(crate)` so the sweeper can WIP-commit a reaped task's worktree before
/// it blocks the task — a dirty worktree left by a reap would otherwise bounce
/// `boi unblock` on `workspace_unclean` (see `service::sweeper`). Call only
/// inside `spawn_blocking` (the git2 / `git2-calls-spawn-blocking` rule).
pub(crate) fn commit_all(
    worktree: &Path,
    message: &str,
) -> Result<Option<usize>, git_ops::GitError> {
    use git2::{IndexAddOption, Repository, Signature};

    let repo = Repository::open(worktree)?;
    let mut index = repo.index()?;
    // Stage all changes — modifications, additions, deletions.
    index.add_all(["*"].iter(), IndexAddOption::DEFAULT, None)?;
    index.write()?;

    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;
    // Resolve HEAD's commit as the parent. An UNBORN HEAD (no commits yet) is a
    // legitimate first-commit case → `None` parent → a root commit. But a
    // GENUINE error peeling HEAD (a corrupt ref, a broken object DB) must NOT
    // be collapsed to `None` — the old `.ok()` did exactly that and would then
    // silently write a parentless root commit *over real history* (review
    // C-rt-S2). `is_unborn_head` distinguishes the unborn-HEAD case (a fresh
    // repo's `head()` errors `UnbornBranch`; a missing ref/target errors
    // `NotFound`) from a broken repo: either of those is the legit `None`;
    // anything else propagates as a `GitError`.
    let parent = match repo.head().and_then(|h| h.peel_to_commit()) {
        Ok(commit) => Some(commit),
        Err(e) => {
            let git_err = git_ops::GitError::from(e);
            if git_ops::is_unborn_head(&git_err) {
                // Unborn / never-committed HEAD — this is the repo's first
                // commit; a `None` parent is correct.
                None
            } else {
                // A genuinely-broken HEAD — propagate, never overwrite history.
                return Err(git_err);
            }
        }
    };

    // Nothing to commit if the new tree equals the parent's tree.
    if let Some(parent) = &parent {
        if parent.tree_id() == tree_id {
            return Ok(None);
        }
    }
    // The CHANGED-file count — a real tree diff of the parent's tree against
    // the new tree (review C-cr-5). The old `tree.len()` reported the new
    // tree's top-level entry count (a directory counts as 1, a 50-file change
    // inside `src/` counts as 1) — a misleading number with no relation to
    // what the commit actually changed. `diff_tree_to_tree` against a `None`
    // old tree (the root-commit case) reports every file as an addition.
    let parent_tree = match &parent {
        Some(parent) => Some(parent.tree()?),
        None => None,
    };
    let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)?;
    let file_count = diff.deltas().len();
    let signature = match repo.signature() {
        Ok(s) => s,
        Err(_) => Signature::now("boi", "boi@localhost")?,
    };
    let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        message,
        &tree,
        &parents,
    )?;
    Ok(Some(file_count))
}

/// Remove every task worktree directory under `spec_root` except `integration`.
/// Returns the count removed.
///
/// OBS-023: the prune-name passed to `git_ops::remove_worktree` is derived
/// from `spec_id` + the per-task subdir name (`TaskId`), matching
/// [`task_worktree_name`] — the names installed by `verify_in`. A
/// path-basename derivation would never find the registration because the
/// registration name is spec-scoped, not path-basename-scoped.
fn remove_task_worktrees(
    repo: &Path,
    spec_id: &SpecId,
    spec_root: &Path,
    integration: &Path,
) -> Result<usize, git_ops::GitError> {
    if !spec_root.is_dir() {
        return Ok(0);
    }
    let mut removed = 0;
    let entries = std::fs::read_dir(spec_root)
        .map_err(|e| git_ops::GitError::BadPath(format!("reading {}: {e}", spec_root.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| {
            git_ops::GitError::BadPath(format!("dir entry in {}: {e}", spec_root.display()))
        })?;
        let path = entry.path();
        if path.as_path() == integration || !path.is_dir() {
            continue;
        }
        // Each task subdir is named `<TaskId>` by §5; reconstruct the
        // registration name from spec_id + that basename.
        let task_basename = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let name = format!("spec-{}-task-{}", spec_id.as_str(), task_basename);
        git_ops::remove_worktree(repo, &name, &path)?;
        removed += 1;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::context::SpecContract;
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
                std::env::temp_dir().join(format!("boi-worktree-{}-{tag}-{n}", std::process::id()));
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

    /// Init a source repo with one commit on `main`.
    fn init_source_repo(path: &Path) {
        use git2::{Repository, Signature};
        let repo = Repository::init(path).expect("init");
        let sig = Signature::now("test", "test@localhost").unwrap();
        std::fs::write(path.join("README.md"), "hello\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("README.md")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
            .unwrap();
        if repo.find_branch("main", git2::BranchType::Local).is_err() {
            let head = repo.head().unwrap().peel_to_commit().unwrap();
            repo.branch("main", &head, true).unwrap();
            repo.set_head("refs/heads/main").unwrap();
        }
    }

    /// A `SpecContract` rooted at `workspace`.
    fn spec_contract(workspace: &Path) -> SpecContract {
        SpecContract {
            scope: "demo".into(),
            workspace: workspace.to_path_buf(),
            base_branch: "main".into(),
            exclusions: vec![],
            verifications: vec![],
            must_emit: vec![],
        }
    }

    /// A `StepCtx` for a step operating in `worktree_path`.
    fn step_ctx(
        phase: &str,
        task: Option<TaskId>,
        worktree_path: PathBuf,
        workspace: &Path,
    ) -> Arc<StepCtx> {
        step_ctx_on(phase, task, worktree_path, workspace, "main")
    }

    /// [`step_ctx`] with an explicit `base_branch` — constructed DIRECTLY
    /// (no `parse_spec`, no dispatch gate): exactly the Layer-1/Layer-2
    /// bypass the Layer-3 re-checks exist for (AC-6).
    fn step_ctx_on(
        phase: &str,
        task: Option<TaskId>,
        worktree_path: PathBuf,
        workspace: &Path,
        base_branch: &str,
    ) -> Arc<StepCtx> {
        let mut contract = spec_contract(workspace);
        contract.base_branch = base_branch.into();
        Arc::new(StepCtx {
            spec_id: spec_id(),
            task_id: task,
            phase_run_id: PhaseRunId::new("P0000001a").unwrap(),
            phase: phase.into(),
            worktree_path,
            branch_ref: "n/a".into(),
            spec_contract: contract,
            task_contract: None,
        })
    }

    /// Assert a `StepRun` is a `Pass`.
    fn assert_pass(run: &StepRun) {
        assert!(
            matches!(run.outcome, StepOutcome::Pass { .. }),
            "expected Pass, got {:?}",
            run.outcome
        );
        assert!(run.events.is_empty(), "worktree steps emit no events");
    }

    /// Assert a `StepRun` is a `Fail`.
    fn assert_fail(run: &StepRun) -> ErrorWhyFix {
        let StepOutcome::Fail { error_why_fix } = &run.outcome else {
            unreachable!("expected Fail, got {:?}", run.outcome);
        };
        error_why_fix.clone()
    }

    #[tokio::test]
    async fn test_l2_prepare_spec_creates_the_integration_branch_and_worktree() {
        let dir = TempDir::new("prepare");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        let root = dir.path.join("worktrees");
        let integration = integration_worktree(&root, &spec_id());

        let run = prepare_spec(step_ctx(
            "workspace_prepare",
            None,
            integration.clone(),
            &repo,
        ))
        .await
        .unwrap();
        assert_pass(&run);
        assert!(
            integration.join("README.md").is_file(),
            "integration checked out"
        );
    }

    #[tokio::test]
    async fn test_l2_verify_in_creates_a_task_worktree_off_integration() {
        let dir = TempDir::new("verify-in");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        let root = dir.path.join("worktrees");

        // prepare_spec first so the integration branch exists.
        prepare_spec(step_ctx(
            "workspace_prepare",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
        ))
        .await
        .unwrap();

        let worktree = task_worktree(&root, &spec_id(), &task_id());
        let run = verify_in(step_ctx(
            "workspace_verify_in",
            Some(task_id()),
            worktree.clone(),
            &repo,
        ))
        .await
        .unwrap();
        assert_pass(&run);
        assert!(
            worktree.join("README.md").is_file(),
            "task worktree checked out"
        );
    }

    /// AUDIT A2 (mechanical half) — `boi unblock` re-enters `verify_in` after
    /// a verdict-routed block closed every phase run, with the task branch AND
    /// worktree surviving from the blocked run (including the worker's
    /// committed work). Re-entry must ADOPT the surviving state (§6 recovery),
    /// not collide on the non-force `create_branch` (the old libgit2 EXISTS →
    /// `Fail` → a misleading `WorkspaceUnclean` re-block: the loop could never
    /// succeed).
    #[tokio::test]
    async fn test_l2_verify_in_reentry_adopts_the_surviving_branch_and_worktree() {
        let dir = TempDir::new("verify-in-reentry");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        let root = dir.path.join("worktrees");

        prepare_spec(step_ctx(
            "workspace_prepare",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
        ))
        .await
        .unwrap();
        let worktree = task_worktree(&root, &spec_id(), &task_id());
        // First entry — creates branch + worktree.
        verify_in(step_ctx(
            "workspace_verify_in",
            Some(task_id()),
            worktree.clone(),
            &repo,
        ))
        .await
        .unwrap();
        // The worker did real work and committed it before the task blocked.
        std::fs::write(worktree.join("blocked_run_work.txt"), "work survives\n").unwrap();
        commit(step_ctx("commit", Some(task_id()), worktree.clone(), &repo))
            .await
            .unwrap();

        // RE-ENTRY (the unblock restart): must adopt, never a harness error.
        let run = verify_in(step_ctx(
            "workspace_verify_in",
            Some(task_id()),
            worktree.clone(),
            &repo,
        ))
        .await
        .expect("re-entry must not be a harness error (the EXISTS collision)");
        assert_pass(&run);
        // The adoption is recorded in the phase synopsis (the phase log).
        let StepOutcome::Pass { evidence } = &run.outcome else {
            unreachable!("assert_pass above");
        };
        assert!(
            evidence.summary.contains("adopt"),
            "the synopsis must record the adoption, got: {}",
            evidence.summary,
        );
        // The blocked run's committed work survives the adoption.
        assert!(
            worktree.join("blocked_run_work.txt").is_file(),
            "the blocked run's committed work must survive re-entry",
        );
    }

    /// AUDIT A2 — re-entry with a DIRTY surviving worktree (the blocked run
    /// left uncommitted changes) must `Fail` with a TRUTHFUL reason naming the
    /// re-entry state and the operator fix — not a harness error, and not the
    /// fresh-worktree message (the worktree is not fresh).
    #[tokio::test]
    async fn test_l2_verify_in_reentry_dirty_worktree_fails_with_a_truthful_reason() {
        let dir = TempDir::new("verify-in-reentry-dirty");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        let root = dir.path.join("worktrees");

        prepare_spec(step_ctx(
            "workspace_prepare",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
        ))
        .await
        .unwrap();
        let worktree = task_worktree(&root, &spec_id(), &task_id());
        verify_in(step_ctx(
            "workspace_verify_in",
            Some(task_id()),
            worktree.clone(),
            &repo,
        ))
        .await
        .unwrap();
        // The blocked run left UNCOMMITTED changes behind.
        std::fs::write(worktree.join("uncommitted.txt"), "left behind\n").unwrap();

        let run = verify_in(step_ctx(
            "workspace_verify_in",
            Some(task_id()),
            worktree.clone(),
            &repo,
        ))
        .await
        .expect("a dirty re-entry is a task Fail, not a harness error");
        let ewf = assert_fail(&run);
        assert!(
            ewf.why.contains("re-entry") || ewf.error.contains("re-entry"),
            "the reason must truthfully name the re-entry state, got: {ewf:?}",
        );
        assert!(
            ewf.why.contains("uncommitted") || ewf.error.contains("uncommitted"),
            "the reason must name the real problem (uncommitted changes), got: {ewf:?}",
        );
        // The dirty file is intact — re-entry never destroys operator-visible
        // state.
        assert!(
            worktree.join("uncommitted.txt").is_file(),
            "re-entry must not destroy the uncommitted changes",
        );
    }

    /// AUDIT A2 — re-entry where the task BRANCH survives but the worktree
    /// directory is gone (a crash between `create_branch` and `add_worktree`,
    /// or a manual cleanup): the worktree is re-added onto the surviving
    /// branch — never an EXISTS collision on the branch.
    #[tokio::test]
    async fn test_l2_verify_in_reentry_recreates_a_missing_worktree_dir() {
        let dir = TempDir::new("verify-in-reentry-gone");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        let root = dir.path.join("worktrees");

        prepare_spec(step_ctx(
            "workspace_prepare",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
        ))
        .await
        .unwrap();
        let worktree = task_worktree(&root, &spec_id(), &task_id());
        verify_in(step_ctx(
            "workspace_verify_in",
            Some(task_id()),
            worktree.clone(),
            &repo,
        ))
        .await
        .unwrap();
        // The worktree dir + registration are removed; the branch survives.
        git_ops::remove_worktree(
            &repo,
            &task_worktree_name(&spec_id(), &task_id()),
            &worktree,
        )
        .unwrap();
        assert!(!worktree.exists(), "the worktree dir is gone");

        let run = verify_in(step_ctx(
            "workspace_verify_in",
            Some(task_id()),
            worktree.clone(),
            &repo,
        ))
        .await
        .expect("re-entry onto a surviving branch must not be a harness error");
        assert_pass(&run);
        assert!(
            worktree.join("README.md").is_file(),
            "the worktree is re-created onto the surviving branch",
        );
    }

    #[tokio::test]
    async fn test_l2_commit_then_merge_to_integration_fast_forwards_a_clean_task() {
        let dir = TempDir::new("merge-clean");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        let root = dir.path.join("worktrees");

        prepare_spec(step_ctx(
            "workspace_prepare",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
        ))
        .await
        .unwrap();
        let worktree = task_worktree(&root, &spec_id(), &task_id());
        verify_in(step_ctx(
            "workspace_verify_in",
            Some(task_id()),
            worktree.clone(),
            &repo,
        ))
        .await
        .unwrap();

        // Worker writes a file into the task worktree.
        std::fs::write(worktree.join("feature.txt"), "task work\n").unwrap();
        let run = commit(step_ctx("commit", Some(task_id()), worktree.clone(), &repo))
            .await
            .unwrap();
        assert_pass(&run);

        // merge_to_integration FF-merges (integration has not moved).
        let run = merge_to_integration(step_ctx(
            "merge_to_integration",
            Some(task_id()),
            worktree.clone(),
            &repo,
        ))
        .await
        .unwrap();
        assert_pass(&run);

        // Bug #3 regression: the fast-forward must SYNC the integration
        // worktree, not just move its branch ref. A stale integration worktree
        // shows up to the spec-level `review` worker as a phantom staged
        // revert — the failure that blocked the host E2E.
        let integration = integration_worktree(&root, &spec_id());
        assert!(
            git_ops::is_clean(&integration).unwrap(),
            "the integration worktree must be clean after merge_to_integration",
        );
        assert!(
            integration.join("feature.txt").is_file(),
            "the merged file must be present in the integration worktree",
        );
    }

    /// REGRESSION — Sgq7hdyfn (2026-06-05) stale-base merge race.
    ///
    /// While a spec is running, a concurrent commit lands on `main` in a file
    /// the spec never touches (in Sgq7hdyfn: `env.sh` + v0.31.0 release work
    /// raced its session-less deletions). The spec's `merge` phase must still
    /// land the integration branch cleanly via a real 3-way merge — there is
    /// no content conflict to resolve, just a non-FF history.
    ///
    /// The current `ff_merge`-only implementation Fails this case with
    /// "integration branch … cannot fast-forward into main" — the verdict that
    /// killed Sgq7hdyfn after all 4 tasks passed, and the family covering
    /// ~73% of BOI's `preflight_failed` spec failures.
    ///
    /// RED gate: this test FAILS on the current code (NotFastForwardable →
    /// `Fail`), and will PASS once `merge_spec` does a real merge for non-FF
    /// histories without content conflict.
    #[tokio::test]
    async fn test_l3_worktree_spec_merge_handles_stale_base_without_real_conflict() {
        use git2::{Repository, Signature};

        let dir = TempDir::new("merge-stale-base");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        let root = dir.path.join("worktrees");

        // ── 1. Normal spec-lifecycle: prepare → verify_in → task commit →
        //      merge_to_integration. The integration branch now holds the
        //      spec's contribution. ──
        prepare_spec(step_ctx(
            "workspace_prepare",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
        ))
        .await
        .unwrap();
        let worktree = task_worktree(&root, &spec_id(), &task_id());
        verify_in(step_ctx(
            "workspace_verify_in",
            Some(task_id()),
            worktree.clone(),
            &repo,
        ))
        .await
        .unwrap();
        std::fs::write(worktree.join("spec_work.txt"), "spec contribution\n").unwrap();
        let run = commit(step_ctx("commit", Some(task_id()), worktree.clone(), &repo))
            .await
            .unwrap();
        assert_pass(&run);
        let run = merge_to_integration(step_ctx(
            "merge_to_integration",
            Some(task_id()),
            worktree,
            &repo,
        ))
        .await
        .unwrap();
        assert_pass(&run);

        // ── 2. Simulate the race: a concurrent agent lands an unrelated
        //      commit on `main` while the spec is still running. ──
        {
            let r = Repository::open(&repo).unwrap();
            let sig = Signature::now("racer", "racer@localhost").unwrap();
            std::fs::write(repo.join("unrelated.txt"), "concurrent commit on main\n").unwrap();
            let mut index = r.index().unwrap();
            index.add_path(Path::new("unrelated.txt")).unwrap();
            index.write().unwrap();
            let tree_oid = index.write_tree().unwrap();
            let tree = r.find_tree(tree_oid).unwrap();
            let parent = r
                .find_branch("main", git2::BranchType::Local)
                .unwrap()
                .into_reference()
                .peel_to_commit()
                .unwrap();
            r.commit(
                Some("refs/heads/main"),
                &sig,
                &sig,
                "concurrent: unrelated file",
                &tree,
                &[&parent],
            )
            .unwrap();
        }

        // ── 3. The spec-level merge phase. The spec's work and the concurrent
        //      commit touch DIFFERENT files; no real content conflict exists.
        //      DESIRED behaviour: real 3-way merge lands cleanly, `Pass`. ──
        let run = merge_spec(step_ctx(
            "merge",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
        ))
        .await
        .unwrap();
        assert_pass(&run);

        // ── 4. Post-merge invariants: `main` carries BOTH contributions. ──
        let r = Repository::open(&repo).unwrap();
        let main_tip = r
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .into_reference()
            .peel_to_commit()
            .unwrap();
        let tree = main_tip.tree().unwrap();
        assert!(
            tree.get_path(Path::new("spec_work.txt")).is_ok(),
            "main must contain the spec's contribution after a non-FF merge",
        );
        assert!(
            tree.get_path(Path::new("unrelated.txt")).is_ok(),
            "main must retain the concurrent unrelated commit after the merge",
        );
    }

    /// REGRESSION — audit A1 / OBS-030 (fired 2026-06-07 and again 2026-06-10,
    /// wiping uncommitted `todo.md` edits).
    ///
    /// The spec-level `merge` phase fast-forwards `[contract].base_branch` —
    /// which is checked out in the OPERATOR's own main checkout (the
    /// `workspace`). §5's clean-state invariants cover BOI-created worktrees
    /// only; nothing guarantees the operator's checkout is clean, and the
    /// post-FF forced sync silently reset every tracked modification there.
    ///
    /// DESIRED behaviour: the merge phase halts LOUDLY with a `Fail` verdict —
    /// the operator's uncommitted edit is byte-identical, `main` does not
    /// move, and the merged work stays intact on the integration branch for a
    /// retry after the operator commits or stashes.
    #[tokio::test]
    async fn test_l3_worktree_spec_merge_halts_loudly_when_operator_checkout_is_dirty() {
        use git2::Repository;

        let dir = TempDir::new("merge-dirty-operator");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        let root = dir.path.join("worktrees");

        // ── 1. Normal spec lifecycle: prepare → verify_in → task commit →
        //      merge_to_integration. The integration branch now holds the
        //      spec's contribution. ──
        prepare_spec(step_ctx(
            "workspace_prepare",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
        ))
        .await
        .unwrap();
        let worktree = task_worktree(&root, &spec_id(), &task_id());
        verify_in(step_ctx(
            "workspace_verify_in",
            Some(task_id()),
            worktree.clone(),
            &repo,
        ))
        .await
        .unwrap();
        std::fs::write(worktree.join("spec_work.txt"), "spec contribution\n").unwrap();
        let run = commit(step_ctx("commit", Some(task_id()), worktree.clone(), &repo))
            .await
            .unwrap();
        assert_pass(&run);
        let run = merge_to_integration(step_ctx(
            "merge_to_integration",
            Some(task_id()),
            worktree,
            &repo,
        ))
        .await
        .unwrap();
        assert_pass(&run);

        // ── 2. The OPERATOR edits a tracked file in the workspace's main
        //      checkout — uncommitted (the OBS-030 state). ──
        std::fs::write(repo.join("README.md"), "operator uncommitted edit\n").unwrap();

        // ── 3. The spec-level merge phase must REFUSE, loudly. ──
        let run = merge_spec(step_ctx(
            "merge",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
        ))
        .await
        .unwrap();
        let error_why_fix = assert_fail(&run);
        // The verdict is actionable: it names the dirty workspace and the
        // recovery ("commit or stash").
        assert!(
            error_why_fix.why.contains("uncommitted"),
            "the why must name the uncommitted changes: {error_why_fix:?}",
        );
        assert!(
            error_why_fix.fix.contains(&repo.display().to_string()),
            "the fix must name the workspace path: {error_why_fix:?}",
        );
        assert!(
            error_why_fix.fix.contains("commit or stash"),
            "the fix must name the recovery: {error_why_fix:?}",
        );
        // Review M1 findings 2+5: the printed recovery must be POSSIBLE. A
        // spec-level merge Fail is TERMINAL (merge.toml `[on.fail]` has no
        // `next` → `route_spec` Halt → `SpecFailed`; `failed` has no exit
        // edge), `boi unblock` is task-scoped and the daemon refuses it under
        // a terminal spec — so the verdict must name the REAL recovery: land
        // the surviving integration branch manually, or re-dispatch.
        assert!(
            error_why_fix.fix.contains(&integration_branch(&spec_id())),
            "the fix must name the surviving integration branch: {error_why_fix:?}",
        );
        assert!(
            error_why_fix.fix.contains("re-dispatch"),
            "the fix must offer re-dispatch: {error_why_fix:?}",
        );
        assert!(
            !error_why_fix.fix.contains("boi unblock"),
            "the fix must not promise `boi unblock` — the refusal terminally \
             fails the spec and the daemon refuses unblock under it: {error_why_fix:?}",
        );

        // ── 4. Post-refusal invariants: the operator's edit is UNTOUCHED,
        //      `main` did not move, and the integration branch still carries
        //      the merged work. ──
        assert_eq!(
            std::fs::read_to_string(repo.join("README.md")).unwrap(),
            "operator uncommitted edit\n",
            "the operator's uncommitted edit must survive the refusal",
        );
        let r = Repository::open(&repo).unwrap();
        let main_tree = r
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .into_reference()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap();
        assert!(
            main_tree.get_path(Path::new("spec_work.txt")).is_err(),
            "main must NOT advance on a refused merge",
        );
        let integration_tree = r
            .find_branch(&integration_branch(&spec_id()), git2::BranchType::Local)
            .unwrap()
            .into_reference()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap();
        assert!(
            integration_tree
                .get_path(Path::new("spec_work.txt"))
                .is_ok(),
            "the merged work must stay intact on the integration branch",
        );
    }

    /// The A1 guard must also cover `merge_spec`'s SECOND `ff_merge` call
    /// site — the retry after a stale-base lazy rebase (the Sgq7hdyfn path).
    /// A concurrent commit lands on `main` (so the first FF attempt is
    /// NotFastForwardable and the integration branch is rebased), AND the
    /// operator's checkout is dirty: the post-rebase FF retry must refuse
    /// just as loudly, with the dirty edit untouched and the (rebased)
    /// integration branch still carrying the work.
    #[tokio::test]
    async fn test_l3_worktree_spec_merge_halts_on_dirty_checkout_after_stale_base_rebase() {
        use git2::{Repository, Signature};

        let dir = TempDir::new("merge-dirty-stale-base");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        let root = dir.path.join("worktrees");

        // ── 1. Normal spec lifecycle up to a populated integration branch. ──
        prepare_spec(step_ctx(
            "workspace_prepare",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
        ))
        .await
        .unwrap();
        let worktree = task_worktree(&root, &spec_id(), &task_id());
        verify_in(step_ctx(
            "workspace_verify_in",
            Some(task_id()),
            worktree.clone(),
            &repo,
        ))
        .await
        .unwrap();
        std::fs::write(worktree.join("spec_work.txt"), "spec contribution\n").unwrap();
        let run = commit(step_ctx("commit", Some(task_id()), worktree.clone(), &repo))
            .await
            .unwrap();
        assert_pass(&run);
        let run = merge_to_integration(step_ctx(
            "merge_to_integration",
            Some(task_id()),
            worktree,
            &repo,
        ))
        .await
        .unwrap();
        assert_pass(&run);

        // ── 2. The stale-base race: a concurrent commit lands on `main`
        //      (committed — the checkout is clean after it)… ──
        {
            let r = Repository::open(&repo).unwrap();
            let sig = Signature::now("racer", "racer@localhost").unwrap();
            std::fs::write(repo.join("unrelated.txt"), "concurrent commit on main\n").unwrap();
            let mut index = r.index().unwrap();
            index.add_path(Path::new("unrelated.txt")).unwrap();
            index.write().unwrap();
            let tree_oid = index.write_tree().unwrap();
            let tree = r.find_tree(tree_oid).unwrap();
            let parent = r
                .find_branch("main", git2::BranchType::Local)
                .unwrap()
                .into_reference()
                .peel_to_commit()
                .unwrap();
            r.commit(
                Some("refs/heads/main"),
                &sig,
                &sig,
                "concurrent: unrelated file",
                &tree,
                &[&parent],
            )
            .unwrap();
        }
        // ── …AND the operator has an uncommitted edit on top of it. ──
        std::fs::write(repo.join("README.md"), "operator uncommitted edit\n").unwrap();

        // ── 3. merge phase: FF #1 is NotFastForwardable → lazy rebase of the
        //      integration branch (clean — different files) → FF #2 must hit
        //      the A1 guard and refuse. ──
        let run = merge_spec(step_ctx(
            "merge",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
        ))
        .await
        .unwrap();
        let error_why_fix = assert_fail(&run);
        assert!(
            error_why_fix.fix.contains("commit or stash"),
            "the post-rebase refusal must be the same actionable A1 verdict: {error_why_fix:?}",
        );
        // Review M1 findings 2+5 — same honest-recovery contract as the
        // first-FF refusal: manual landing or re-dispatch, never a command
        // the terminal spec refuses.
        assert!(
            error_why_fix.fix.contains("re-dispatch"),
            "the fix must offer re-dispatch: {error_why_fix:?}",
        );
        assert!(
            !error_why_fix.fix.contains("boi unblock"),
            "the fix must not promise `boi unblock` under a terminal spec: {error_why_fix:?}",
        );

        // ── 4. The dirty edit is untouched; `main` did not advance; the
        //      (rebased) integration branch still carries the work. ──
        assert_eq!(
            std::fs::read_to_string(repo.join("README.md")).unwrap(),
            "operator uncommitted edit\n",
            "the operator's uncommitted edit must survive the refusal",
        );
        let r = Repository::open(&repo).unwrap();
        let main_tree = r
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .into_reference()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap();
        assert!(
            main_tree.get_path(Path::new("spec_work.txt")).is_err(),
            "main must NOT advance on a refused merge",
        );
        let integration_tree = r
            .find_branch(&integration_branch(&spec_id()), git2::BranchType::Local)
            .unwrap()
            .into_reference()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap();
        assert!(
            integration_tree
                .get_path(Path::new("spec_work.txt"))
                .is_ok(),
            "the merged work must stay intact on the (rebased) integration branch",
        );
    }

    #[tokio::test]
    async fn test_l2_merge_to_integration_fails_with_conflict_files_on_a_diverged_task() {
        // §13.3 MergeConflict failure-path test.
        let dir = TempDir::new("merge-conflict");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        let root = dir.path.join("worktrees");

        prepare_spec(step_ctx(
            "workspace_prepare",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
        ))
        .await
        .unwrap();

        // Both task worktrees are created off the SAME (initial) integration
        // HEAD — `verify_in` runs for both BEFORE either merges. Task B branches
        // off old integration; Task A then merges and moves integration.
        let task_a = TaskId::new("T000000aa").unwrap();
        let wt_a = task_worktree(&root, &spec_id(), &task_a);
        verify_in(step_ctx(
            "workspace_verify_in",
            Some(task_a.clone()),
            wt_a.clone(),
            &repo,
        ))
        .await
        .unwrap();
        let task_b = TaskId::new("T000000bb").unwrap();
        let wt_b = task_worktree(&root, &spec_id(), &task_b);
        verify_in(step_ctx(
            "workspace_verify_in",
            Some(task_b.clone()),
            wt_b.clone(),
            &repo,
        ))
        .await
        .unwrap();

        // Task A edits README.md and merges — moving integration.
        std::fs::write(wt_a.join("README.md"), "task A version\n").unwrap();
        commit(step_ctx(
            "commit",
            Some(task_a.clone()),
            wt_a.clone(),
            &repo,
        ))
        .await
        .unwrap();
        merge_to_integration(step_ctx("merge_to_integration", Some(task_a), wt_a, &repo))
            .await
            .unwrap();

        // Task B (branched off the OLD integration) edits the SAME file — its
        // merge must rebase, hit a conflict, and Fail.
        std::fs::write(wt_b.join("README.md"), "task B version\n").unwrap();
        commit(step_ctx(
            "commit",
            Some(task_b.clone()),
            wt_b.clone(),
            &repo,
        ))
        .await
        .unwrap();
        let run = merge_to_integration(step_ctx("merge_to_integration", Some(task_b), wt_b, &repo))
            .await
            .unwrap();
        let ewf = assert_fail(&run);
        assert!(
            ewf.error.contains("merge conflict"),
            "the Fail names a merge conflict: {ewf:?}",
        );
        assert!(
            ewf.why.contains("README.md"),
            "the Fail names the conflicted file: {ewf:?}",
        );
    }

    #[tokio::test]
    async fn test_l2_verify_out_fails_on_a_dirty_worktree() {
        // §13.3 WorkspaceUnclean failure-path test.
        let dir = TempDir::new("verify-out-dirty");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        let root = dir.path.join("worktrees");

        prepare_spec(step_ctx(
            "workspace_prepare",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
        ))
        .await
        .unwrap();
        let worktree = task_worktree(&root, &spec_id(), &task_id());
        verify_in(step_ctx(
            "workspace_verify_in",
            Some(task_id()),
            worktree.clone(),
            &repo,
        ))
        .await
        .unwrap();

        // Leave an uncommitted file — verify_out must Fail.
        std::fs::write(worktree.join("orphan.txt"), "stray\n").unwrap();
        let run = verify_out(step_ctx(
            "workspace_verify_out",
            Some(task_id()),
            worktree,
            &repo,
        ))
        .await
        .unwrap();
        let ewf = assert_fail(&run);
        assert!(
            ewf.error.contains("not clean"),
            "verify_out Fail names the unclean state: {ewf:?}",
        );
    }

    #[tokio::test]
    async fn test_l2_teardown_removes_the_worktrees() {
        let dir = TempDir::new("teardown");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        let root = dir.path.join("worktrees");

        let integration = integration_worktree(&root, &spec_id());
        prepare_spec(step_ctx(
            "workspace_prepare",
            None,
            integration.clone(),
            &repo,
        ))
        .await
        .unwrap();
        let worktree = task_worktree(&root, &spec_id(), &task_id());
        verify_in(step_ctx(
            "workspace_verify_in",
            Some(task_id()),
            worktree.clone(),
            &repo,
        ))
        .await
        .unwrap();
        assert!(integration.exists() && worktree.exists());

        // teardown is spec-level — its worktree_path is the integration one.
        let run = teardown(step_ctx("teardown", None, integration.clone(), &repo))
            .await
            .unwrap();
        assert_pass(&run);
        assert!(!integration.exists(), "integration worktree removed");
        assert!(!worktree.exists(), "task worktree removed");
    }

    #[tokio::test]
    async fn test_l2_commit_with_no_changes_is_a_passing_noop() {
        let dir = TempDir::new("commit-noop");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        let root = dir.path.join("worktrees");

        prepare_spec(step_ctx(
            "workspace_prepare",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
        ))
        .await
        .unwrap();
        let worktree = task_worktree(&root, &spec_id(), &task_id());
        verify_in(step_ctx(
            "workspace_verify_in",
            Some(task_id()),
            worktree.clone(),
            &repo,
        ))
        .await
        .unwrap();

        // No file written — commit is a passing no-op.
        let run = commit(step_ctx("commit", Some(task_id()), worktree, &repo))
            .await
            .unwrap();
        assert_pass(&run);
        let StepOutcome::Pass { evidence } = &run.outcome else {
            unreachable!();
        };
        assert!(evidence.summary.contains("nothing to commit"));
    }

    /// Regression test for C-cr-5 — `commit` reports the real CHANGED-file
    /// count, not the new tree's top-level entry count.
    ///
    /// The OLD code reported `tree.len()` — the number of top-level entries in
    /// the *whole* commit tree. A worker that adds three files inside `src/`
    /// changes three files, but `tree.len()` of the resulting tree is 2
    /// (`README.md` + the `src` directory) — a number with no relation to what
    /// the commit changed. The fix computes a real tree diff. This test adds
    /// exactly three new files under a subdirectory and asserts the reported
    /// count is `3`, not `2`.
    #[tokio::test]
    async fn test_l2_commit_reports_the_real_changed_file_count() {
        let dir = TempDir::new("commit-count");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        let root = dir.path.join("worktrees");

        prepare_spec(step_ctx(
            "workspace_prepare",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
        ))
        .await
        .unwrap();
        let worktree = task_worktree(&root, &spec_id(), &task_id());
        verify_in(step_ctx(
            "workspace_verify_in",
            Some(task_id()),
            worktree.clone(),
            &repo,
        ))
        .await
        .unwrap();

        // The worker adds THREE files under `src/` — a single new top-level
        // entry (`src`), but three genuinely-changed files.
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(worktree.join("src/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(worktree.join("src/b.rs"), "fn b() {}\n").unwrap();
        std::fs::write(worktree.join("src/c.rs"), "fn c() {}\n").unwrap();

        let run = commit(step_ctx("commit", Some(task_id()), worktree, &repo))
            .await
            .unwrap();
        assert_pass(&run);
        let StepOutcome::Pass { evidence } = &run.outcome else {
            unreachable!();
        };
        // The summary names the real changed-file count — 3, not the tree's
        // top-level entry count (`README.md` + `src` = 2).
        assert!(
            evidence.summary.contains("committed 3 file(s)"),
            "C-cr-5 regression: commit must report the real changed-file count \
             (3), not the tree's top-level entry count (2) — got: {}",
            evidence.summary,
        );
    }

    /// Regression test for C-rt-S2 — a broken HEAD surfaces the REAL cause,
    /// not a misleading downstream error.
    ///
    /// `commit_all` resolves HEAD's commit as the new commit's parent. The OLD
    /// code did `repo.head().ok().and_then(|h| h.peel_to_commit().ok())` — the
    /// `.ok()` collapsed *every* failure to `None`. The intent was a
    /// history-detaching root commit; in practice libgit2's own
    /// `commit`-with-`update_ref` guard rejects that with the cryptic "current
    /// tip is not the first parent", so the OLD code fails — but with an error
    /// pointing at the wrong thing (a parent mismatch, not the broken HEAD).
    ///
    /// This test repoints the current branch ref at a BLOB's OID (a valid
    /// object that is not a commit), so `repo.head().peel_to_commit()` fails
    /// with a non-`NotFound`, non-`UnbornBranch` error. The fix's
    /// `is_unborn_head` check classifies this HEAD as broken and propagates
    /// **that** error — the failure names the real HEAD problem. The test
    /// asserts the surfaced error is NOT the misleading "first parent" message:
    /// under the OLD `.ok()` code it is exactly that — a genuine fail-before /
    /// pass-after on the error's content. (The legitimate unborn-HEAD
    /// first-commit case stays a `None` parent — `git_ops`'s `is_unborn_head`
    /// test covers the `UnbornBranch` classification.)
    #[tokio::test]
    async fn test_l2_commit_on_a_non_commit_head_surfaces_the_real_head_error() {
        let dir = TempDir::new("commit-broken-head");
        let repo_path = dir.path.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        init_source_repo(&repo_path);

        // Write a standalone blob, then repoint the current branch ref at it.
        // HEAD now resolves to a *blob*, so `peel_to_commit` fails — a HEAD
        // that is broken (not a commit) but neither unborn nor not-found.
        let repo = git2::Repository::open(&repo_path).unwrap();
        let blob_oid = repo.blob(b"i am a blob, not a commit").unwrap();
        let head_ref_name = repo
            .head()
            .unwrap()
            .name()
            .expect("HEAD resolves to a named branch")
            .to_owned();
        repo.reference(&head_ref_name, blob_oid, true, "point HEAD at a blob")
            .unwrap();
        drop(repo);

        // `commit` calls `commit_all`. The branch ref points at a blob, so
        // resolving the parent commit fails — the fix surfaces THAT error.
        let result = commit(step_ctx(
            "commit",
            Some(task_id()),
            repo_path.clone(),
            &repo_path,
        ))
        .await;
        let err = result.expect_err(
            "C-rt-S2 regression: commit on a HEAD that does not resolve to a \
             commit must be a loud error",
        );
        let StepError::Git(message) = &err else {
            unreachable!("a broken HEAD must propagate as StepError::Git, got {err:?}");
        };
        // The fix propagates the REAL parent-resolution error. The OLD `.ok()`
        // code instead reached `repo.commit` with a `None` parent and failed
        // with the misleading "current tip is not the first parent".
        assert!(
            !message.contains("first parent"),
            "C-rt-S2 regression: the error must name the real broken-HEAD \
             cause, not the misleading downstream `not the first parent` \
             message — got: {message}",
        );
    }

    #[tokio::test]
    async fn test_l1_task_level_step_without_task_id_is_a_loud_step_error() {
        let dir = TempDir::new("no-task");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        // verify_out is task-level — running it with task_id None is a loud
        // StepError, never a panic and never a silent pass.
        let err = verify_out(step_ctx(
            "workspace_verify_out",
            None,
            dir.path.join("wt"),
            &repo,
        ))
        .await
        .unwrap_err();
        assert!(matches!(err, StepError::Worktree(_)), "got {err:?}");
    }

    /// T7 defense-in-depth: even though `config::spec::parse_spec` rejects a
    /// relative `[contract].workspace` at parse time, `prepare_spec` MUST NOT
    /// hand a relative path to `git_ops` (which surfaces an inscrutable error
    /// like "could not find repo"). It must short-circuit with a typed
    /// `StepError` whose message clearly names the workspace-is-not-absolute
    /// contract violation.
    ///
    /// Constructs a `StepCtx` directly with a relative `workspace` (bypassing
    /// `parse_spec`'s expansion), invokes `prepare_spec`, and asserts the
    /// returned `StepError` is a typed worktree/validate variant whose
    /// message names the absolute-path requirement. The pre-fix code lacks
    /// the check, so the call either reaches `git_ops` and surfaces a generic
    /// git error, or worse, opens a repo in the test process's cwd — both
    /// of those error strings do NOT contain "absolute", so the assertion
    /// pinpoints the missing guard.
    #[tokio::test]
    async fn test_l2_prepare_spec_rejects_relative_workspace_path_with_typed_error() {
        let dir = TempDir::new("relative-workspace");
        let root = dir.path.join("worktrees");
        let integration = integration_worktree(&root, &spec_id());

        // A relative path — never absolute, so it can't be a valid workspace.
        let relative_workspace = PathBuf::from("relative/workspace/path");

        let err = prepare_spec(step_ctx(
            "workspace_prepare",
            None,
            integration,
            &relative_workspace,
        ))
        .await
        .expect_err(
            "T7 regression: prepare_spec must reject a relative workspace path \
             with a typed StepError, not pass it through to git_ops",
        );

        // Must be a typed StepError variant (not a panic — `await.expect_err`
        // already established no panic — and must name the absolute-path
        // contract so the operator sees the real cause).
        let msg = err.to_string();
        assert!(
            msg.contains("absolute"),
            "T7 regression: the error must name the workspace-is-not-absolute \
             cause — got: {msg:?}",
        );
    }

    #[test]
    fn test_l1_branch_and_path_derivation() {
        // Integration branch nests under spec/<SpecId>/ (deviation from §5 —
        // see `integration_branch`'s doc) so it does not collide with the
        // task-branch directory.
        assert_eq!(integration_branch(&spec_id()), "spec/S0000001a/integration");
        assert_eq!(
            task_branch(&spec_id(), &task_id()),
            "spec/S0000001a/T0000001a"
        );
        let root = PathBuf::from("/wt");
        assert_eq!(
            integration_worktree(&root, &spec_id()),
            PathBuf::from("/wt/S0000001a/integration")
        );
        assert_eq!(
            task_worktree(&root, &spec_id(), &task_id()),
            PathBuf::from("/wt/S0000001a/T0000001a")
        );
    }

    // -----------------------------------------------------------------------
    // GitFlow Layer 3 — the §7.1 hermetic integration battery (R-B8/R-B9/
    // R-B10; AC-4/AC-5/AC-6/AC-15). Every repo is a temp repo; every StepCtx
    // is constructed directly (the parse/dispatch bypass — AC-6's stale-
    // snapshot class). Markers land on branches via committed-tree plumbing
    // only (testkit — no checkout machinery, AC-14).
    // -----------------------------------------------------------------------

    use crate::runtime::branch_policy::testkit;

    /// Build the §7.1 GitFlow temp repo: one commit on `main` (via
    /// `init_source_repo`), the gitflow marker COMMITTED on main (D-13 reads
    /// committed trees — an uncommitted working-tree marker would test
    /// nothing), and `develop` branched at main's tip (so the marker sits on
    /// both long-lived branches).
    fn init_gitflow_repo(repo: &Path) {
        init_source_repo(repo);
        testkit::commit_on_branch(
            repo,
            "main",
            &[(".boi-policy.toml", testkit::GITFLOW_MARKER)],
        );
        testkit::branch_from_main(repo, "develop");
    }

    /// AC-4 (positive proof): on a GitFlow workspace a `base_branch =
    /// "develop"` spec runs the full §5 lifecycle and the delivery lands on
    /// `develop` — `main`'s OID is byte-identical before/after.
    #[tokio::test]
    async fn test_l3_branch_policy_gitflow_spec_lands_on_develop_main_untouched() {
        let dir = TempDir::new("policy-gitflow-positive");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_gitflow_repo(&repo);
        let root = dir.path.join("worktrees");
        let main_before = testkit::branch_oid(&repo, "main");

        // prepare → verify_in → task work → commit → merge_to_integration →
        // merge, all on base develop.
        let run = prepare_spec(step_ctx_on(
            "workspace_prepare",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
            "develop",
        ))
        .await
        .unwrap();
        assert_pass(&run);
        let worktree = task_worktree(&root, &spec_id(), &task_id());
        let run = verify_in(step_ctx_on(
            "workspace_verify_in",
            Some(task_id()),
            worktree.clone(),
            &repo,
            "develop",
        ))
        .await
        .unwrap();
        assert_pass(&run);
        std::fs::write(worktree.join("spec_work.txt"), "spec contribution\n").unwrap();
        let run = commit(step_ctx_on(
            "commit",
            Some(task_id()),
            worktree.clone(),
            &repo,
            "develop",
        ))
        .await
        .unwrap();
        assert_pass(&run);
        let run = merge_to_integration(step_ctx_on(
            "merge_to_integration",
            Some(task_id()),
            worktree,
            &repo,
            "develop",
        ))
        .await
        .unwrap();
        assert_pass(&run);
        let run = merge_spec(step_ctx_on(
            "merge",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
            "develop",
        ))
        .await
        .unwrap();
        assert_pass(&run);

        // develop advanced to the integration tip; main never moved (AC-4).
        assert_eq!(
            testkit::branch_oid(&repo, "develop"),
            testkit::branch_oid(&repo, &integration_branch(&spec_id())),
            "develop must fast-forward to the integration tip",
        );
        assert_eq!(
            testkit::branch_oid(&repo, "main"),
            main_before,
            "refs/heads/main must be byte-identical after a develop delivery",
        );
    }

    /// AC-5 + AC-6 (negative proof + stale-snapshot backstop): a `StepCtx`
    /// constructed directly with `base_branch = "main"` on a GitFlow
    /// workspace — `prepare_spec` AND `merge_spec` both refuse with the typed
    /// protected reason; `refs/heads/main` is byte-identical; no `spec/*`
    /// branch is created. The checkout is parked DETACHED first — the D-13
    /// read is checkout-independent (the §7.1.6 / M14 variant).
    #[tokio::test]
    async fn test_l3_branch_policy_protected_base_refused_main_oid_identical() {
        let dir = TempDir::new("policy-protected-negative");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_gitflow_repo(&repo);
        testkit::detach_head(&repo);
        let root = dir.path.join("worktrees");
        let main_before = testkit::branch_oid(&repo, "main");

        // prepare_spec refuses (Layer 3, pre-branch-creation).
        let run = prepare_spec(step_ctx_on(
            "workspace_prepare",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
            "main",
        ))
        .await
        .unwrap();
        let ewf = assert_fail(&run);
        assert!(
            ewf.error
                .contains("branch policy refuses base branch `main`"),
            "typed protected refusal, got: {}",
            ewf.error,
        );
        assert!(
            ewf.fix.contains("base_branch = \"develop\""),
            "the fix teaches develop, got: {}",
            ewf.fix,
        );
        // No spec/* branch was created off main (AC-5's prepare leg).
        {
            let r = git2::Repository::open(&repo).unwrap();
            assert!(
                r.find_reference(&format!("refs/heads/{}", integration_branch(&spec_id())))
                    .is_err(),
                "the refusal must precede integration-branch creation",
            );
        }

        // merge_spec refuses too — immediately, before any ff_merge (AC-6:
        // both steps hold without Layer 1 ever running).
        let run = merge_spec(step_ctx_on(
            "merge",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
            "main",
        ))
        .await
        .unwrap();
        let ewf = assert_fail(&run);
        assert!(
            ewf.error
                .contains("branch policy refuses base branch `main`"),
            "typed protected refusal at merge, got: {}",
            ewf.error,
        );

        // R-B10: the protected ref never moved.
        assert_eq!(
            testkit::branch_oid(&repo, "main"),
            main_before,
            "refs/heads/main must be byte-identical after both refusals",
        );
    }

    /// M7 at runtime: a nonexistent `base_branch` is a typed Layer-3 refusal
    /// at `prepare_spec` — not a generic libgit2 NotFound out of
    /// `create_branch`.
    #[tokio::test]
    async fn test_l3_branch_policy_missing_base_refused_at_prepare() {
        let dir = TempDir::new("policy-missing-base");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        let root = dir.path.join("worktrees");

        let run = prepare_spec(step_ctx_on(
            "workspace_prepare",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
            "no-such-branch",
        ))
        .await
        .unwrap();
        let ewf = assert_fail(&run);
        assert!(
            ewf.error
                .contains("base branch `no-such-branch` does not exist"),
            "typed missing-base refusal, got: {}",
            ewf.error,
        );
        assert!(
            ewf.fix.contains("create branch"),
            "the fix is actionable, got: {}",
            ewf.fix,
        );
    }

    /// AC-15 (runtime leg) / M11: a present-but-unreadable marker is a typed
    /// `StepOutcome::Fail` at both re-check sites — never silently treated
    /// as "no policy" (R-B2).
    #[tokio::test]
    async fn test_l3_branch_policy_invalid_marker_is_typed_step_fail() {
        let dir = TempDir::new("policy-invalid-marker");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        testkit::commit_on_branch(
            &repo,
            "main",
            &[(".boi-policy.toml", "mode = \"gitflow\"\n")],
        );
        let root = dir.path.join("worktrees");
        let main_before = testkit::branch_oid(&repo, "main");

        for step in ["workspace_prepare", "merge"] {
            let ctx = step_ctx_on(
                step,
                None,
                integration_worktree(&root, &spec_id()),
                &repo,
                "main",
            );
            let run = if step == "workspace_prepare" {
                prepare_spec(ctx).await.unwrap()
            } else {
                merge_spec(ctx).await.unwrap()
            };
            let ewf = assert_fail(&run);
            assert!(
                ewf.error.contains("branch policy could not be read"),
                "{step}: typed PolicyInvalid refusal, got: {}",
                ewf.error,
            );
            assert!(
                ewf.why.contains(".boi-policy.toml"),
                "{step}: the why names the marker, got: {}",
                ewf.why,
            );
        }
        assert_eq!(testkit::branch_oid(&repo, "main"), main_before);
    }

    /// R-B8's TOCTOU guarantee: a spec prepared while the workspace was
    /// unmanaged is refused at `merge_spec` when the gitflow marker lands on
    /// the base branch MID-SPEC — the re-check reads the committed tree
    /// fresh at each consumption site; nothing trusts the prepare-time
    /// verdict.
    #[tokio::test]
    async fn test_l3_branch_policy_marker_added_mid_spec_refuses_at_merge() {
        let dir = TempDir::new("policy-toctou");
        let repo = dir.path.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_source_repo(&repo);
        let root = dir.path.join("worktrees");

        // Unmanaged at prepare time: base main is allowed (M6).
        let run = prepare_spec(step_ctx(
            "workspace_prepare",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
        ))
        .await
        .unwrap();
        assert_pass(&run);
        let worktree = task_worktree(&root, &spec_id(), &task_id());
        verify_in(step_ctx(
            "workspace_verify_in",
            Some(task_id()),
            worktree.clone(),
            &repo,
        ))
        .await
        .unwrap();
        std::fs::write(worktree.join("spec_work.txt"), "spec contribution\n").unwrap();
        commit(step_ctx("commit", Some(task_id()), worktree.clone(), &repo))
            .await
            .unwrap();
        let run = merge_to_integration(step_ctx(
            "merge_to_integration",
            Some(task_id()),
            worktree,
            &repo,
        ))
        .await
        .unwrap();
        assert_pass(&run);

        // The workspace adopts GitFlow mid-spec: the marker lands on main.
        testkit::commit_on_branch(
            &repo,
            "main",
            &[(".boi-policy.toml", testkit::GITFLOW_MARKER)],
        );
        let main_after_marker = testkit::branch_oid(&repo, "main");

        // The merge step must refuse — fresh read, typed reason, main unmoved.
        let run = merge_spec(step_ctx(
            "merge",
            None,
            integration_worktree(&root, &spec_id()),
            &repo,
        ))
        .await
        .unwrap();
        let ewf = assert_fail(&run);
        assert!(
            ewf.error
                .contains("branch policy refuses base branch `main`"),
            "the mid-spec marker must refuse the merge, got: {}",
            ewf.error,
        );
        assert_eq!(
            testkit::branch_oid(&repo, "main"),
            main_after_marker,
            "the engine must not move main after the policy landed",
        );
    }

    /// With no `CARGO_TARGET_DIR` override, build children get the shared,
    /// persistent `~/.boi/v2/cargo-target` so every worktree reuses one warm
    /// artifact cache instead of building ~1148 crates cold (OBS-032).
    #[test]
    fn test_l1_cargo_target_dir_defaults_to_shared_warm_dir() {
        let resolved = cargo_target_dir_for(None);
        assert_eq!(
            resolved,
            default_cargo_target_dir(),
            "no override → the shared warm cargo target dir",
        );
        assert!(
            resolved.ends_with("cargo-target"),
            "the shared dir is the dedicated cargo-target path, got {}",
            resolved.display(),
        );
    }

    /// A pre-existing `CARGO_TARGET_DIR` wins — the injection is overridable.
    #[test]
    fn test_l1_cargo_target_dir_respects_an_override() {
        let custom = PathBuf::from("/tmp/some/operator/chosen/target");
        let resolved = cargo_target_dir_for(Some(custom.clone()));
        assert_eq!(
            resolved, custom,
            "an explicit override is returned verbatim"
        );
    }
}
