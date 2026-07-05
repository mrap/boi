//! `boi dispatch <spec.toml>` — parse + validate a spec, persist its
//! structural rows in one transaction, and tell the daemon to start it.
//!
//! ## The flow (Task 9.4)
//!
//! 1. Read + parse + validate the spec TOML (`config::parse_spec`).
//! 2. Mint a [`SpecId`] and a [`TaskId`] per task; resolve each authored
//!    `blocked_by` ref to its minted task id.
//! 3. Build the v1 `spec_versions` snapshot in the **G21.3 shape**:
//!    `{ title, delivery, spec_contract: SpecContract, task_contracts: { <task_id>: TaskContract } }`.
//!    The orchestrator's `run_phase` re-hydrates `spec_contract`/`task_contracts`
//!    by key; `title` and `delivery` are stored as top-level snapshot extras.
//! 4. Persist `specs` + `spec_versions` + `spec_runtime` + `task_runtime` +
//!    `task_deps` in ONE transaction (`repo::insert_dispatch`).
//! 5. Submit `DaemonCommand::Dispatch` over the control socket — the daemon
//!    emits `SpecStarted` (`queued → running`), then runs preflight (a
//!    preflight failure is a legal `running → failed{PreflightFailed}`).
//!
//! `boi dispatch` is a control-socket client: **no daemon → fail loud**
//! ([`DispatchError::NoDaemon`]), non-zero exit. The structural rows are
//! persisted *before* the socket call; if the daemon is down the spec sits
//! `queued` and must be **manually re-dispatched** — `recover_after_crash` only
//! recovers `running` specs, never `queued` ones. `boi dispatch` exits non-zero
//! so the operator knows the spec did not start.
//!
//! ## Delivery (G22.3)
//!
//! v1.0 implements only the `merge` delivery end-to-end. The spec-level
//! `merge` deterministic phase (`runtime::worktree::merge_spec`) FF-merges the
//! integration branch into the base branch; the `pr` / `branch-only`
//! deliveries are NOT wired (`delivery` does not reach the `merge` step —
//! `SpecContract` carries no `delivery` field and the G21.3 snapshot shape is
//! fixed). Rather than dispatch a `branch-only` spec whose `merge` phase would
//! still FF-merge (wrong behaviour, silently), `boi dispatch` **rejects**
//! `pr` / `branch-only` loudly here. (Phase 9 report — a documented v1.0 gap.)

use std::path::Path;

use crate::cli::control::{self, ControlError};
use crate::cli::paths::{self, PathError};
use crate::config::verify_lint::{self, Finding};
use crate::config::{self, Delivery, Spec};
use crate::repo;
use crate::repo::db::RepoError;
use crate::runtime::branch_policy::{self, PolicyVerdict};
use crate::service::{DaemonCommand, DaemonResponse};
use crate::types::context::TaskContract;
use crate::types::ids::{SpecId, TaskId};

/// A `boi dispatch` failure.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    /// The `~/.boi/v2/` path layout could not be resolved.
    #[error(transparent)]
    Path(#[from] PathError),
    /// The spec file could not be read.
    #[error("cannot read spec file {path}: {source}")]
    Read {
        /// The unreadable path.
        path: String,
        /// The I/O error.
        source: std::io::Error,
    },
    /// The spec TOML failed to parse / validate.
    ///
    /// Carries the spec file path so the operator knows exactly which file
    /// failed and where to look. The inner [`config::ConfigError`] already
    /// includes the offending field and a Fix hint.
    #[error("invalid spec `{path}` — {source}")]
    InvalidSpec {
        /// The spec file that failed.
        path: String,
        /// The specific parse or validation failure (includes a Fix hint).
        #[source]
        source: config::ConfigError,
    },
    /// The spec uses a delivery mode not implemented at v1.0 (G22.3).
    #[error(
        "delivery `{0}` is not implemented at v1.0 — only `merge` is wired \
         end-to-end (the `pr` / `branch-only` deliveries are a known gap)"
    )]
    UnsupportedDelivery(&'static str),
    /// A minted id failed validation (an internal generator fault).
    #[error("internal: generated an invalid id: {0}")]
    BadId(String),
    /// An authored `blocked_by` ref names no task in the spec.
    #[error("task dependency `{dep}` (on task #{task}) names no task in the spec")]
    UnknownDep {
        /// The 1-based index of the dependent task.
        task: usize,
        /// The unresolved `blocked_by` ref.
        dep: String,
    },
    /// The structural insert failed.
    #[error("could not persist the spec: {0}")]
    Repo(#[from] RepoError),
    /// The snapshot JSON could not be built.
    #[error("could not serialize the spec snapshot: {0}")]
    Snapshot(#[from] serde_json::Error),
    /// No daemon is running — the spec was persisted `queued` but not started.
    #[error(
        "the spec was persisted but NO daemon is running to start it — \
         start `boi daemon`, the spec is queued"
    )]
    NoDaemon,
    /// The control socket faulted after a connection was made.
    #[error("control socket error: {0}")]
    Socket(String),
    /// The daemon rejected the dispatch command.
    #[error("the daemon rejected the dispatch: {0}")]
    DaemonRejected(String),
    /// The pre-dispatch verify-lint flagged at least one Tier A antipattern in
    /// a `Verification::Command`. The spec was NOT persisted — no orphan rows
    /// to garbage-collect. Catalogue + rationale: `config::verify_lint`.
    #[error("{}", format_verify_lint_findings(.0))]
    VerifyLintFailed(Vec<Finding>),
    /// The spec's `base_branch` is in the workspace's protected set (matrix
    /// M2/M12 — the GitFlow Layer-1 dispatch gate, R-B6). The spec was NOT
    /// persisted. `fix_hint` arrives fully rendered from
    /// `runtime::branch_policy` (a ready-to-print `Fix:` line).
    #[error(
        "branch-policy: base_branch `{branch}` is protected in this workspace \
         — spec rejected, nothing persisted\n  {fix_hint}"
    )]
    PolicyProtectedBase {
        /// The protected branch the spec named.
        branch: String,
        /// The rendered `Fix:` line for the spec author.
        fix_hint: String,
    },
    /// The spec's `base_branch` has no local head in the workspace (matrix
    /// M4/M5/M7/M13 — the universal dispatch-time existence check, D-4). The
    /// spec was NOT persisted.
    #[error(
        "branch-policy: base_branch `{branch}` does not exist in the \
         workspace — spec rejected, nothing persisted\n  {hint}"
    )]
    PolicyMissingBase {
        /// The branch that does not exist.
        branch: String,
        /// The rendered `Fix:` line — bootstrap, tracking-branch, or
        /// fix-the-spec, depending on what exists.
        hint: String,
    },
    /// The workspace branch policy could not be determined (matrix M11 — the
    /// R-B2 error taxonomy: a present-but-unreadable `.boi-policy.toml`, or
    /// any repo/ref/odb read failure, is NEVER treated as "no policy"). The
    /// spec was NOT persisted.
    #[error(
        "branch-policy: the workspace branch policy could not be read — spec \
         rejected, nothing persisted\n  {reason}"
    )]
    PolicyInvalid {
        /// What failed, with its rendered `Fix:` line.
        reason: String,
    },
}

/// Render every verify-lint finding as a single multi-line error string. The
/// Display impl on [`DispatchError::VerifyLintFailed`] delegates here so the
/// operator sees: a header line, then one indented line per finding with the
/// rule id, the location (`task=<ref or 'contract'>`), the offending
/// verification name, the truncated snippet, and the rule's fix hint.
///
/// Kept as a free function so unit tests can call it directly without
/// constructing a full `DispatchError`.
fn format_verify_lint_findings(findings: &[Finding]) -> String {
    let mut buf = format!(
        "verify-lint: {} finding(s) — spec rejected, nothing persisted",
        findings.len()
    );
    for f in findings {
        let task = f.task_ref.as_deref().unwrap_or("contract");
        let name = f.verification_name.as_deref().unwrap_or("(unnamed)");
        buf.push_str(&format!(
            "\n  [{rule}] task={task} verify={name} snippet={snippet} :: {fix}",
            rule = f.rule_id,
            snippet = f.command_snippet,
            fix = f.fix_hint,
        ));
    }
    buf
}

/// Run `boi dispatch <spec>`.
pub async fn run(spec_path: &Path) -> Result<(), DispatchError> {
    let db_url = paths::boi_db_url()?;
    let socket = paths::control_socket()?;
    run_with_opts(spec_path, &db_url, &socket).await
}

/// The testable body of `run` — accepts explicit db URL + socket path so tests
/// can inject a tempdir DB and a temp socket without touching `$HOME`.
async fn run_with_opts(
    spec_path: &Path,
    db_url: &str,
    socket: &std::path::Path,
) -> Result<(), DispatchError> {
    // (1) — read + parse + validate.
    let text = std::fs::read_to_string(spec_path).map_err(|source| DispatchError::Read {
        path: spec_path.display().to_string(),
        source,
    })?;
    let spec = config::parse_spec(&text).map_err(|source| DispatchError::InvalidSpec {
        path: spec_path.display().to_string(),
        source,
    })?;

    // G22.3 — reject a delivery v1.0 does not implement, loudly.
    match spec.delivery {
        Delivery::Merge => {}
        Delivery::Pr => return Err(DispatchError::UnsupportedDelivery("pr")),
        Delivery::BranchOnly => return Err(DispatchError::UnsupportedDelivery("branch-only")),
    }

    // (1b) — pre-dispatch verify-lint. Runs BEFORE the structural insert so a
    // flagged spec leaves no rows behind (no orphan to clean up). Hard fail,
    // no bypass — per S6 in CLAUDE.md, the rule is fix-the-spec, not skip the
    // gate. Catalogue lives in `config::verify_lint::RULES`.
    let findings = verify_lint::lint(&spec);
    if !findings.is_empty() {
        return Err(DispatchError::VerifyLintFailed(findings));
    }

    // (1c) — the GitFlow branch-policy dispatch gate (Layer 1, R-B6). Runs
    // after verify-lint (the pure spec lints report first) and BEFORE the
    // structural insert, so a rejected spec leaves zero rows — same
    // fix-the-spec-not-skip-the-gate doctrine as (1b). The committed-tree
    // read semantics (D-13) and the full behavior matrix live in
    // `runtime::branch_policy`. An M8 advisory (the workspace has a develop
    // branch but no marker, and the spec lands on main) is non-fatal:
    // print one line and proceed.
    if let Some(advisory) = check_branch_policy(&spec).await? {
        eprintln!("warning: {advisory}");
    }

    // (2)+(3) — mint ids, resolve deps, build the G21.3 snapshot.
    let plan = build_dispatch(&spec)?;

    // (4) — persist all five tables in one transaction.
    let pool = repo::connect(db_url).await?;
    repo::insert_dispatch(&pool, &plan.rows, chrono::Utc::now()).await?;
    println!(
        "Persisted spec {} ({} task(s)) — queued.",
        plan.rows.spec_id,
        plan.rows.tasks.len(),
    );

    // (5) — tell the daemon to start it.
    let command = DaemonCommand::Dispatch {
        spec_id: plan.rows.spec_id.as_str().to_owned(),
        skills: plan.skills,
        spec_file: Some(spec_path.to_string_lossy().into_owned()),
    };
    match control::send_command(socket, &command).await {
        Ok(DaemonResponse::Ok { detail }) => {
            println!("{detail}");
            Ok(())
        }
        Ok(DaemonResponse::Err { detail }) => Err(DispatchError::DaemonRejected(detail)),
        Err(ControlError::NoDaemon { .. }) => Err(DispatchError::NoDaemon),
        Err(other) => Err(DispatchError::Socket(other.to_string())),
    }
}

/// GitFlow Layer 1 (R-B6): evaluate the workspace branch policy for the
/// spec's `base_branch` — pre-persist.
///
/// The policy is read from the committed tree of `refs/heads/<base_branch>`
/// in the spec's workspace (D-13 — never any checkout's working tree). On
/// allow, returns the optional one-line M8 advisory for the operator; every
/// rejection maps to a typed [`DispatchError`] whose message carries the
/// fully-rendered `Fix:` hint from `runtime::branch_policy`.
///
/// A workspace-less spec cannot reach here: the normalized
/// [`Spec`]'s contract is a `types::SpecContract` whose `workspace` is a
/// non-optional `PathBuf` (a rationale-only raw spec dies in `normalize` —
/// matrix M10's dead path lives in the pure core, not here).
async fn check_branch_policy(spec: &Spec) -> Result<Option<String>, DispatchError> {
    let workspace = spec.contract.workspace.clone();
    let base = spec.contract.base_branch.clone();
    let ctx = branch_policy::load_policy(workspace, base.clone()).await;
    match ctx.verdict(&base) {
        PolicyVerdict::Allow { advisory } => Ok(advisory),
        PolicyVerdict::Skip { .. } => Ok(None),
        PolicyVerdict::ProtectedBase { branch, fix_hint } => {
            Err(DispatchError::PolicyProtectedBase { branch, fix_hint })
        }
        PolicyVerdict::MissingBase { branch, hint } => {
            Err(DispatchError::PolicyMissingBase { branch, hint })
        }
        PolicyVerdict::PolicyInvalid { reason } => Err(DispatchError::PolicyInvalid { reason }),
    }
}

/// The fully-resolved dispatch payload — the structural rows plus the skill
/// names the daemon's preflight needs.
#[derive(Debug)]
struct DispatchPlan {
    rows: repo::DispatchRows,
    skills: Vec<String>,
}

/// Mint ids, resolve `blocked_by` refs, and build the G21.3 snapshot.
///
/// Pure — no I/O. Factored out of [`run`] so the snapshot-shape + dep-resolution
/// L2 tests drive it directly.
fn build_dispatch(spec: &Spec) -> Result<DispatchPlan, DispatchError> {
    // Mint a SpecId.
    let spec_id =
        SpecId::new(repo::random_id('S')).map_err(|e| DispatchError::BadId(e.to_string()))?;

    // Mint a TaskId per task; remember the author-ref → minted-id mapping so
    // `blocked_by` refs resolve. A task with no author ref cannot be a
    // dependency target, which is correct (you can only depend on a named ref).
    let mut tasks = Vec::with_capacity(spec.tasks.len());
    let mut ref_to_id: std::collections::HashMap<String, TaskId> = std::collections::HashMap::new();
    for task in &spec.tasks {
        let task_id =
            TaskId::new(repo::random_id('T')).map_err(|e| DispatchError::BadId(e.to_string()))?;
        if let Some(r) = &task.task_ref {
            ref_to_id.insert(r.clone(), task_id.clone());
        }
        tasks.push((task_id, task));
    }

    // The task_contracts map — keyed by minted task id (G21.3).
    let mut task_contracts = serde_json::Map::new();
    let mut dispatch_tasks = Vec::with_capacity(tasks.len());
    let mut deps = Vec::new();
    for (idx, (task_id, task_def)) in tasks.iter().enumerate() {
        let contract = TaskContract {
            behavior: task_def.behavior.clone(),
            verifications: task_def.verifications.clone(),
        };
        task_contracts.insert(
            task_id.as_str().to_owned(),
            serde_json::to_value(&contract)?,
        );
        dispatch_tasks.push(repo::DispatchTask {
            task_id: task_id.clone(),
            task_ref: task_def.task_ref.clone(),
        });
        // Resolve every `blocked_by` ref to a minted task id.
        for dep_ref in &task_def.blocked_by {
            let depends_on = ref_to_id
                .get(dep_ref)
                .ok_or_else(|| DispatchError::UnknownDep {
                    task: idx + 1,
                    dep: dep_ref.clone(),
                })?
                .clone();
            deps.push(repo::DispatchDep {
                task_id: task_id.clone(),
                depends_on,
            });
        }
    }

    // The G21.3-shaped snapshot — `{ title, delivery, spec_contract, task_contracts }`.
    // `title` and `delivery` are kept as top-level snapshot extras; the
    // orchestrator re-hydrates only `spec_contract`/`task_contracts` by key, so
    // the extra top-level keys are transparent to it.
    let delivery_str = match spec.delivery {
        Delivery::Merge => "merge",
        Delivery::Pr => "pr",
        Delivery::BranchOnly => "branch-only",
    };
    // `skills` lands at the snapshot top level alongside `spec_contract` /
    // `task_contracts` so `rehydrate_contracts` (G23.1 → G26.3) can re-attach
    // them to every `PhaseContext` without a parallel side-channel.
    let snapshot = serde_json::json!({
        "title": spec.title,
        "delivery": delivery_str,
        "spec_contract": serde_json::to_value(&spec.contract)?,
        "task_contracts": serde_json::Value::Object(task_contracts),
        "skills": serde_json::to_value(&spec.skills)?,
    });

    Ok(DispatchPlan {
        rows: repo::DispatchRows {
            spec_id,
            snapshot,
            tasks: dispatch_tasks,
            deps,
        },
        skills: spec.skills.iter().map(|s| s.name.clone()).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::control;
    use crate::cli::testtmp::TempDir;
    use crate::service::{DaemonCommand, DaemonResponse};
    use tokio_util::sync::CancellationToken;

    /// A minimal valid spec TOML with two tasks, the second depending on the
    /// first by ref.
    const TWO_TASK_SPEC: &str = r#"
title = "Add a feature"

[contract]
scope = "the feature"
workspace = "/repo"
base_branch = "main"

[[tasks]]
ref = "first"
behavior = "do the first thing"
verifications = [{ command = "true" }]

[[tasks]]
behavior = "do the second thing"
blocked_by = ["first"]
verifications = [{ intent = "the second thing works" }]
"#;

    /// `build_dispatch` produces a G21.3-shaped snapshot — a `spec_contract`
    /// object and a `task_contracts` map keyed by the minted task ids.
    #[test]
    fn test_l2_build_dispatch_snapshot_has_g21_3_shape() {
        let spec = config::parse_spec(TWO_TASK_SPEC).unwrap();
        let plan = build_dispatch(&spec).unwrap();
        let snap = &plan.rows.snapshot;

        assert!(
            snap.get("spec_contract").is_some(),
            "the snapshot has a `spec_contract` key (G21.3)",
        );
        let contracts = snap
            .get("task_contracts")
            .and_then(|v| v.as_object())
            .expect("a `task_contracts` object (G21.3)");
        assert_eq!(contracts.len(), 2, "one contract per task");
        // Every key is a minted task id present in `tasks`.
        for tid in plan.rows.tasks.iter().map(|t| t.task_id.as_str()) {
            assert!(
                contracts.contains_key(tid),
                "task_contracts is keyed by minted task id",
            );
        }
    }

    /// `build_dispatch` resolves an authored `blocked_by` ref to the minted
    /// task id — the resulting `DispatchDep` names the right two tasks.
    #[test]
    fn test_l2_build_dispatch_resolves_blocked_by_refs() {
        let spec = config::parse_spec(TWO_TASK_SPEC).unwrap();
        let plan = build_dispatch(&spec).unwrap();
        assert_eq!(plan.rows.deps.len(), 1, "one dependency edge");
        let dep = &plan.rows.deps[0];
        // The dependent is task 2, the dependency is task 1 (the "first" ref).
        assert_eq!(dep.task_id, plan.rows.tasks[1].task_id);
        assert_eq!(dep.depends_on, plan.rows.tasks[0].task_id);
    }

    /// An unknown `blocked_by` ref is rejected loudly — by `config::validate`'s
    /// `check_dependency_graph`, *before* `boi dispatch` ever builds a snapshot.
    ///
    /// `build_dispatch`'s own `UnknownDep` arm is therefore defensive (a
    /// belt-and-suspenders for a `Spec` constructed bypassing `validate`); the
    /// real, reachable rejection is here at parse time.
    #[test]
    fn test_l2_unknown_dep_is_rejected_at_parse() {
        let bad = r#"
title = "x"
[contract]
scope = "x"
workspace = "/r"
base_branch = "main"
[[tasks]]
behavior = "t"
blocked_by = ["nonexistent"]
verifications = [{ command = "true" }]
"#;
        let err = config::parse_spec(bad).unwrap_err();
        assert!(
            matches!(err, config::ConfigError::DanglingDep { .. }),
            "a dangling blocked_by ref is rejected at parse, got {err:?}",
        );
    }

    /// The minted snapshot re-hydrates into a `SpecContract` — proving the
    /// stored shape is exactly what the orchestrator's `run_phase` expects.
    #[test]
    fn test_l2_snapshot_rehydrates_into_contracts() {
        use crate::types::context::SpecContract;
        let spec = config::parse_spec(TWO_TASK_SPEC).unwrap();
        let plan = build_dispatch(&spec).unwrap();
        let snap = &plan.rows.snapshot;

        let spec_contract: SpecContract =
            serde_json::from_value(snap.get("spec_contract").unwrap().clone()).unwrap();
        assert_eq!(spec_contract.base_branch, "main");

        let first_id = plan.rows.tasks[0].task_id.as_str();
        let task_contract: TaskContract = serde_json::from_value(
            snap.get("task_contracts")
                .unwrap()
                .get(first_id)
                .unwrap()
                .clone(),
        )
        .unwrap();
        assert_eq!(task_contract.behavior, "do the first thing");
    }

    /// B1 regression: `build_dispatch` must include `title` and `delivery` as
    /// top-level snapshot keys.
    ///
    /// Before the fix, `build_dispatch` only stored `{spec_contract, task_contracts}`
    /// — no top-level `title` or `delivery` — so any consumer reading those keys
    /// fell back to `title: ""` and `delivery: "merge"` (hardcoded fallback).
    #[test]
    fn test_l2_regression_b1_build_dispatch_snapshot_includes_title_and_delivery() {
        let spec = config::parse_spec(TWO_TASK_SPEC).unwrap();
        let plan = build_dispatch(&spec).unwrap();
        let snap = &plan.rows.snapshot;

        assert_eq!(
            snap.get("title").and_then(|v| v.as_str()).unwrap_or(""),
            "Add a feature",
            "B1 regression: build_dispatch must include `title` in the snapshot at the \
             top level (consumers read it from there)",
        );
        assert_eq!(
            snap.get("delivery").and_then(|v| v.as_str()).unwrap_or(""),
            "merge",
            "B1 regression: build_dispatch must include `delivery` in the snapshot at the \
             top level (consumers read it from there)",
        );
        // The G21.3 keys must still be present — the orchestrator reads them.
        assert!(
            snap.get("spec_contract").is_some(),
            "spec_contract key preserved"
        );
        assert!(
            snap.get("task_contracts").is_some(),
            "task_contracts key preserved"
        );
    }

    // -----------------------------------------------------------------------
    // S3 — `dispatch::run()` integration paths (NoDaemon / successful / rejected).
    // -----------------------------------------------------------------------

    /// The `TWO_TASK_SPEC` body with `[contract].workspace` pointed at
    /// `workspace`. `run_with_opts` crosses the Layer-1 branch-policy gate
    /// (R-B6), which opens the workspace repository — the in-source `/repo`
    /// placeholder would be a `PolicyInvalid` rejection at the gate, so the
    /// integration-path tests need a real (temp) git repo. The const itself
    /// stays untouched for the pure parse/snapshot tests (R-C1.3).
    fn two_task_spec_in(workspace: &std::path::Path) -> String {
        TWO_TASK_SPEC.replace(
            "workspace = \"/repo\"",
            &format!("workspace = \"{}\"", workspace.display()),
        )
    }

    /// Init a real git workspace under `dir` (one commit on `main`) and write
    /// the two-task spec pointing at it. Returns the spec file path.
    fn write_spec_file(dir: &std::path::Path) -> std::path::PathBuf {
        use crate::runtime::branch_policy::testkit;
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).expect("create workspace dir");
        testkit::init_repo_on_main(&workspace);
        let spec_path = dir.join("spec.toml");
        std::fs::write(&spec_path, two_task_spec_in(&workspace)).expect("write test spec file");
        spec_path
    }

    /// A handler that returns `DaemonResponse::Ok` for every `Dispatch` command.
    struct OkHandler;

    #[async_trait::async_trait]
    impl control::CommandHandler for OkHandler {
        async fn handle(&self, _cmd: DaemonCommand) -> DaemonResponse {
            DaemonResponse::Ok {
                detail: "started".to_owned(),
            }
        }
    }

    /// A handler that returns `DaemonResponse::Err` for every `Dispatch` command
    /// (simulates a preflight failure).
    struct ErrHandler;

    #[async_trait::async_trait]
    impl control::CommandHandler for ErrHandler {
        async fn handle(&self, _cmd: DaemonCommand) -> DaemonResponse {
            DaemonResponse::Err {
                detail: "preflight failed: skills not found".to_owned(),
            }
        }
    }

    /// NoDaemon path: spec rows are persisted `queued` and `DispatchError::NoDaemon`
    /// is returned when no daemon is listening on the socket.
    #[tokio::test]
    async fn test_l2_dispatch_run_no_daemon_persists_queued_returns_no_daemon() {
        let dir = TempDir::new("dispatch-nodaemon");
        let db_path = dir.path().join("boi.db");
        let db_url = format!("sqlite://{}?mode=rwc", db_path.display());
        let socket = dir.path().join("absent.sock");
        let spec_path = write_spec_file(dir.path());

        let err = run_with_opts(&spec_path, &db_url, &socket)
            .await
            .expect_err("NoDaemon must be returned when no daemon is listening");
        assert!(
            matches!(err, DispatchError::NoDaemon),
            "expected NoDaemon, got {err:?}",
        );

        // The spec rows must have been persisted (the DB write precedes the socket call).
        let pool = repo::connect(&db_url).await.unwrap();
        let specs = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM specs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            specs, 1,
            "spec row must be persisted even when daemon is absent"
        );
    }

    /// Successful path: all five structural rows are inserted in one transaction
    /// and `DaemonResponse::Ok` is returned.
    #[tokio::test]
    async fn test_l2_dispatch_run_successful_path_returns_ok() {
        let dir = TempDir::new("dispatch-ok");
        let db_path = dir.path().join("boi.db");
        let db_url = format!("sqlite://{}?mode=rwc", db_path.display());
        let socket = dir.path().join("daemon.sock");
        let spec_path = write_spec_file(dir.path());

        let shutdown = CancellationToken::new();
        let server = tokio::spawn(control::serve(
            socket.clone(),
            std::sync::Arc::new(OkHandler),
            shutdown.clone(),
        ));
        // Wait for the socket to appear.
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        run_with_opts(&spec_path, &db_url, &socket)
            .await
            .expect("successful dispatch returns Ok");

        // All five tables are populated.
        let pool = repo::connect(&db_url).await.unwrap();
        let specs = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM specs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(specs, 1, "specs row inserted");
        let versions = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM spec_versions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(versions, 1, "spec_versions row inserted");
        let tasks = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM task_runtime")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(tasks, 2, "one task_runtime row per task");

        shutdown.cancel();
        drop(server);
    }

    /// Preflight-fail path: when the daemon responds `DaemonResponse::Err`,
    /// `dispatch::run` returns `DispatchError::DaemonRejected` (not a hang,
    /// not an `IllegalTransition` surfaced to the CLI).
    #[tokio::test]
    async fn test_l2_dispatch_run_daemon_rejected_returns_dispatch_rejected() {
        let dir = TempDir::new("dispatch-rejected");
        let db_path = dir.path().join("boi.db");
        let db_url = format!("sqlite://{}?mode=rwc", db_path.display());
        let socket = dir.path().join("daemon.sock");
        let spec_path = write_spec_file(dir.path());

        let shutdown = CancellationToken::new();
        let _server = tokio::spawn(control::serve(
            socket.clone(),
            std::sync::Arc::new(ErrHandler),
            shutdown.clone(),
        ));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let err = run_with_opts(&spec_path, &db_url, &socket)
            .await
            .expect_err("a daemon Err response is a DaemonRejected error");
        assert!(
            matches!(err, DispatchError::DaemonRejected(_)),
            "expected DaemonRejected, got {err:?}",
        );
        // The detail from the daemon response is surfaced.
        if let DispatchError::DaemonRejected(detail) = &err {
            assert!(
                detail.contains("preflight"),
                "detail carries the daemon's message"
            );
        }

        shutdown.cancel();
    }

    // -----------------------------------------------------------------------
    // TFF1B — descriptive, helpful errors on a malformed spec.
    //
    // Each test writes a malformed spec to a temp file, calls run_with_opts
    // with a dummy DB URL + absent socket (safe: parse failure returns before
    // any DB or socket access), and asserts the error message carries:
    //   (a) the spec file path
    //   (b) the offending field / TOML location
    //   (c) what is wrong
    //   (d) a Fix hint
    // -----------------------------------------------------------------------

    /// Bad TOML: the error names the spec file and describes the parse failure.
    #[tokio::test]
    async fn test_l2_invalid_spec_bad_toml_names_file_and_cause() {
        let dir = TempDir::new("dispatch-badtoml");
        let spec_path = dir.path().join("spec.toml");
        std::fs::write(&spec_path, "this is not [valid toml syntax {{{{").unwrap();
        let db_url = format!("sqlite://{}?mode=rwc", dir.path().join("boi.db").display());

        let err = run_with_opts(&spec_path, &db_url, &dir.path().join("absent.sock"))
            .await
            .expect_err("bad TOML must be rejected");

        let msg = err.to_string();
        // (a) file path present.
        assert!(
            msg.contains(spec_path.to_str().unwrap()),
            "error must name the spec file path, got: {msg}",
        );
        // (c) TOML issue described.
        assert!(
            msg.to_lowercase().contains("toml") || msg.to_lowercase().contains("invalid"),
            "error must describe the TOML parse failure, got: {msg}",
        );
        // (d) Fix hint present.
        assert!(
            msg.contains("Fix:"),
            "error must include a Fix hint, got: {msg}",
        );
        assert!(
            matches!(err, DispatchError::InvalidSpec { .. }),
            "error variant must be InvalidSpec, got: {err:?}",
        );
    }

    /// Missing required field: the error names the file and the missing field.
    #[tokio::test]
    async fn test_l2_invalid_spec_missing_title_names_file_and_field() {
        let dir = TempDir::new("dispatch-missing-title");
        let spec_path = dir.path().join("spec.toml");
        let bad = r#"
title = ""
[contract]
scope = "x"
workspace = "/r"
base_branch = "main"
[[tasks]]
behavior = "t"
verifications = [{ command = "true" }]
"#;
        std::fs::write(&spec_path, bad).unwrap();
        let db_url = format!("sqlite://{}?mode=rwc", dir.path().join("boi.db").display());

        let err = run_with_opts(&spec_path, &db_url, &dir.path().join("absent.sock"))
            .await
            .expect_err("empty title must be rejected");

        let msg = err.to_string();
        // (a) file path present.
        assert!(
            msg.contains(spec_path.to_str().unwrap()),
            "error must name the spec file path, got: {msg}",
        );
        // (b)+(c) missing field named.
        assert!(
            msg.contains("title"),
            "error must name the missing field, got: {msg}",
        );
        // (d) Fix hint present.
        assert!(
            msg.contains("Fix:"),
            "error must include a Fix hint, got: {msg}",
        );
    }

    /// Unknown field: the error names the file and the unrecognized field.
    #[tokio::test]
    async fn test_l2_invalid_spec_unknown_field_names_file_and_field() {
        let dir = TempDir::new("dispatch-unknown-field");
        let spec_path = dir.path().join("spec.toml");
        let bad = r#"
title = "x"
flavor = "exotic"
[contract]
scope = "x"
workspace = "/r"
base_branch = "main"
[[tasks]]
behavior = "t"
verifications = [{ command = "true" }]
"#;
        std::fs::write(&spec_path, bad).unwrap();
        let db_url = format!("sqlite://{}?mode=rwc", dir.path().join("boi.db").display());

        let err = run_with_opts(&spec_path, &db_url, &dir.path().join("absent.sock"))
            .await
            .expect_err("unknown field must be rejected");

        let msg = err.to_string();
        // (a) file path present.
        assert!(
            msg.contains(spec_path.to_str().unwrap()),
            "error must name the spec file path, got: {msg}",
        );
        // (b)+(c) unknown field named.
        assert!(
            msg.contains("flavor"),
            "error must name the unknown field, got: {msg}",
        );
        // (d) Fix hint present.
        assert!(
            msg.contains("Fix:"),
            "error must include a Fix hint, got: {msg}",
        );
    }

    /// Unresolvable task dep: the error names the file and the missing ref.
    #[tokio::test]
    async fn test_l2_invalid_spec_bad_dep_names_file_and_dep_ref() {
        let dir = TempDir::new("dispatch-bad-dep");
        let spec_path = dir.path().join("spec.toml");
        let bad = r#"
title = "x"
[contract]
scope = "x"
workspace = "/r"
base_branch = "main"
[[tasks]]
behavior = "t"
blocked_by = ["nonexistent-ref"]
verifications = [{ command = "true" }]
"#;
        std::fs::write(&spec_path, bad).unwrap();
        let db_url = format!("sqlite://{}?mode=rwc", dir.path().join("boi.db").display());

        let err = run_with_opts(&spec_path, &db_url, &dir.path().join("absent.sock"))
            .await
            .expect_err("dangling dep must be rejected");

        let msg = err.to_string();
        // (a) file path present.
        assert!(
            msg.contains(spec_path.to_str().unwrap()),
            "error must name the spec file path, got: {msg}",
        );
        // (b)+(c) missing dep ref named.
        assert!(
            msg.contains("nonexistent-ref"),
            "error must name the unresolvable dep ref, got: {msg}",
        );
        // (d) Fix hint present.
        assert!(
            msg.contains("Fix:"),
            "error must include a Fix hint, got: {msg}",
        );
    }

    // -----------------------------------------------------------------------
    // S4 — pre-dispatch verify-lint (the lint runs BEFORE persistence so a
    // flagged spec leaves no rows behind).
    // -----------------------------------------------------------------------

    /// A spec that violates R6 — `grep -c PATTERN file && ...` — the exact
    /// bug that motivated the lint (2026-05-29 incident).
    const R6_BAD_SPEC: &str = r#"
title = "Bad verify"

[contract]
scope = "trigger the lint"
workspace = "/repo"
base_branch = "main"

[[tasks]]
ref = "broken"
behavior = "do nothing"
verifications = [{ name = "bad", command = "grep -c PATTERN file && test \"$(cat /tmp/x)\" = \"0\"" }]
"#;

    /// Verify-lint path: a spec that violates R6 is rejected BEFORE any
    /// structural row is persisted — no orphan to clean up.
    #[tokio::test]
    async fn test_l2_dispatch_run_verify_lint_rejects_r6_bad_spec_and_persists_nothing() {
        let dir = TempDir::new("dispatch-lint-r6");
        let db_path = dir.path().join("boi.db");
        let db_url = format!("sqlite://{}?mode=rwc", db_path.display());
        let socket = dir.path().join("absent.sock");
        let spec_path = dir.path().join("spec.toml");
        std::fs::write(&spec_path, R6_BAD_SPEC).expect("write bad spec");

        let err = run_with_opts(&spec_path, &db_url, &socket)
            .await
            .expect_err("R6-bad spec must be rejected by the lint");

        let findings = match &err {
            DispatchError::VerifyLintFailed(f) => f.clone(),
            other => panic!("expected VerifyLintFailed, got {other:?}"),
        };
        assert!(
            findings.iter().any(|f| f.rule_id == "R6"),
            "at least one finding must name R6, got {findings:?}"
        );

        // The rendered error string contains the header and at least one
        // finding line — gives the operator everything they need to fix it.
        let msg = err.to_string();
        assert!(
            msg.contains("verify-lint:"),
            "Display impl must lead with the header, got: {msg}",
        );
        assert!(
            msg.contains("[R6]"),
            "Display impl must name the rule id, got: {msg}",
        );

        // NO specs row was persisted — the lint runs BEFORE the structural
        // insert. This is the design invariant: a flagged spec leaves no
        // orphan rows to garbage-collect.
        let pool = repo::connect(&db_url).await.unwrap();
        let specs = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM specs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            specs, 0,
            "verify-lint failure must NOT persist any spec rows"
        );
    }

    /// The fixture spec used by the existing dispatch tests must still
    /// dispatch cleanly through the lint (no R1..R7 violations) — guards
    /// against a future lint rule that accidentally rejects valid specs.
    #[tokio::test]
    async fn test_l2_dispatch_run_verify_lint_does_not_flag_two_task_fixture() {
        let dir = TempDir::new("dispatch-lint-clean");
        let db_path = dir.path().join("boi.db");
        let db_url = format!("sqlite://{}?mode=rwc", db_path.display());
        let socket = dir.path().join("absent.sock");
        let spec_path = write_spec_file(dir.path());

        let err = run_with_opts(&spec_path, &db_url, &socket)
            .await
            .expect_err("absent daemon → NoDaemon, not a lint failure");

        // The error is NoDaemon (the spec passes the lint and reaches
        // persistence; the socket call is what fails). Specifically NOT
        // VerifyLintFailed — that would be a regression.
        assert!(
            matches!(err, DispatchError::NoDaemon),
            "fixture spec must pass the lint and fail at the socket, got {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // GitFlow Layer 1 — the branch-policy dispatch gate (R-B6).
    //
    // Each test stands up a real temp git workspace (committed trees only —
    // the D-13 read model; no checkout machinery, AC-14) and drives
    // `run_with_opts` with a dummy DB + absent socket. Rejections must be
    // typed AND pre-persist: zero rows in `specs`.
    // -----------------------------------------------------------------------

    use crate::runtime::branch_policy::testkit;

    /// Count the `specs` rows in the test DB.
    async fn spec_rows(db_url: &str) -> i64 {
        let pool = repo::connect(db_url).await.unwrap();
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM specs")
            .fetch_one(&pool)
            .await
            .unwrap()
    }

    /// Write the two-task spec with `base_branch` swapped to `base`,
    /// targeting `workspace`.
    fn spec_with_base(
        dir: &std::path::Path,
        workspace: &std::path::Path,
        base: &str,
    ) -> std::path::PathBuf {
        let text = two_task_spec_in(workspace).replace(
            "base_branch = \"main\"",
            &format!("base_branch = \"{base}\""),
        );
        let spec_path = dir.join("spec.toml");
        std::fs::write(&spec_path, text).expect("write spec");
        spec_path
    }

    /// AC-7: a spec naming a nonexistent `base_branch` is rejected at
    /// dispatch with the typed `PolicyMissingBase` (matrix M7 — the universal
    /// existence check, D-4), pre-persist: no DB rows, no daemon contact.
    #[tokio::test]
    async fn test_l2_dispatch_policy_missing_base_rejected_pre_persist() {
        let dir = TempDir::new("dispatch-policy-missing-base");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        testkit::init_repo_on_main(&workspace);
        let db_url = format!("sqlite://{}?mode=rwc", dir.path().join("boi.db").display());
        let spec_path = spec_with_base(dir.path(), &workspace, "no-such-branch");

        let err = run_with_opts(&spec_path, &db_url, &dir.path().join("absent.sock"))
            .await
            .expect_err("a nonexistent base_branch must be rejected at dispatch");

        match &err {
            DispatchError::PolicyMissingBase { branch, hint } => {
                assert_eq!(branch, "no-such-branch");
                assert!(hint.contains("Fix:"), "hint carries a Fix line: {hint}");
            }
            other => panic!("expected PolicyMissingBase, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("nothing persisted"),
            "the rendering states the pre-persist guarantee: {msg}"
        );
        assert_eq!(
            spec_rows(&db_url).await,
            0,
            "AC-7: a MissingBase rejection must leave zero spec rows"
        );
    }

    /// M2: on a gitflow workspace (marker committed on main), a
    /// `base_branch = "main"` spec is hard-rejected with the typed
    /// `PolicyProtectedBase` whose hint teaches develop + the ceremony —
    /// pre-persist.
    #[tokio::test]
    async fn test_l2_dispatch_policy_protected_base_rejected_pre_persist() {
        let dir = TempDir::new("dispatch-policy-protected");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        testkit::init_repo_on_main(&workspace);
        testkit::commit_on_branch(
            &workspace,
            "main",
            &[(".boi-policy.toml", testkit::GITFLOW_MARKER)],
        );
        let db_url = format!("sqlite://{}?mode=rwc", dir.path().join("boi.db").display());
        let spec_path = spec_with_base(dir.path(), &workspace, "main");

        let err = run_with_opts(&spec_path, &db_url, &dir.path().join("absent.sock"))
            .await
            .expect_err("a protected base_branch must be rejected at dispatch");

        match &err {
            DispatchError::PolicyProtectedBase { branch, fix_hint } => {
                assert_eq!(branch, "main");
                assert!(
                    fix_hint.contains("base_branch = \"develop\""),
                    "the hint teaches the correct value: {fix_hint}"
                );
                assert!(
                    fix_hint.contains("release ceremony"),
                    "the hint names the ceremony: {fix_hint}"
                );
            }
            other => panic!("expected PolicyProtectedBase, got {other:?}"),
        }
        assert_eq!(
            spec_rows(&db_url).await,
            0,
            "a ProtectedBase rejection must leave zero spec rows"
        );
    }

    /// AC-15 (dispatch leg) / M11: a present-but-unreadable marker is a typed
    /// `PolicyInvalid` rejection — never silently treated as "no policy" —
    /// pre-persist.
    #[tokio::test]
    async fn test_l2_dispatch_policy_invalid_marker_rejected_pre_persist() {
        let dir = TempDir::new("dispatch-policy-invalid");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        testkit::init_repo_on_main(&workspace);
        testkit::commit_on_branch(
            &workspace,
            "main",
            &[(".boi-policy.toml", "model = \"flying-buttress\"\n")],
        );
        let db_url = format!("sqlite://{}?mode=rwc", dir.path().join("boi.db").display());
        let spec_path = spec_with_base(dir.path(), &workspace, "main");

        let err = run_with_opts(&spec_path, &db_url, &dir.path().join("absent.sock"))
            .await
            .expect_err("an unreadable marker must be rejected at dispatch");

        match &err {
            DispatchError::PolicyInvalid { reason } => {
                assert!(
                    reason.contains(".boi-policy.toml"),
                    "the reason names the marker: {reason}"
                );
                assert!(
                    reason.contains("Fix:"),
                    "the reason carries a Fix line: {reason}"
                );
            }
            other => panic!("expected PolicyInvalid, got {other:?}"),
        }
        assert_eq!(
            spec_rows(&db_url).await,
            0,
            "a PolicyInvalid rejection must leave zero spec rows"
        );
    }

    /// M8: an unmanaged workspace that LOOKS like GitFlow (develop exists, no
    /// marker) receiving a main-targeted spec dispatches normally — the gate
    /// returns the one-line advisory instead of rejecting, and the spec
    /// persists (the absent socket yields NoDaemon, proving the gate passed).
    #[tokio::test]
    async fn test_l2_dispatch_policy_m8_advisory_allows_and_persists() {
        let dir = TempDir::new("dispatch-policy-m8");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        testkit::init_repo_on_main(&workspace);
        testkit::branch_from_main(&workspace, "develop");
        let db_url = format!("sqlite://{}?mode=rwc", dir.path().join("boi.db").display());
        let spec_path = spec_with_base(dir.path(), &workspace, "main");

        // The advisory rendering, asserted directly on the gate function
        // (run_with_opts only prints it).
        let spec_text = std::fs::read_to_string(&spec_path).unwrap();
        let spec = config::parse_spec(&spec_text).unwrap();
        let advisory = check_branch_policy(&spec)
            .await
            .expect("M8 is an allow, not a rejection")
            .expect("the M8 advisory line is present");
        assert!(
            advisory.contains("develop branch but no .boi-policy.toml"),
            "the advisory explains itself: {advisory}"
        );
        assert!(
            advisory.contains("landing on main"),
            "the advisory names the consequence: {advisory}"
        );

        // And the full dispatch path proceeds to persistence.
        let err = run_with_opts(&spec_path, &db_url, &dir.path().join("absent.sock"))
            .await
            .expect_err("absent daemon → NoDaemon");
        assert!(
            matches!(err, DispatchError::NoDaemon),
            "M8 must allow the dispatch through the gate, got {err:?}"
        );
        assert_eq!(
            spec_rows(&db_url).await,
            1,
            "the M8 allow persists the spec"
        );
    }

    /// M6: a plain unmanaged workspace (no marker, no develop) with
    /// `base_branch = "main"` dispatches with NO advisory — today's behavior,
    /// byte-for-byte (the unmanaged-workspace path; AC-16 test leg).
    #[tokio::test]
    async fn test_l2_dispatch_policy_unmanaged_main_allows_without_advisory() {
        let dir = TempDir::new("dispatch-policy-m6");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        testkit::init_repo_on_main(&workspace);
        let spec_path = spec_with_base(dir.path(), &workspace, "main");

        let spec_text = std::fs::read_to_string(&spec_path).unwrap();
        let spec = config::parse_spec(&spec_text).unwrap();
        let advisory = check_branch_policy(&spec)
            .await
            .expect("an unmanaged workspace allows main");
        assert_eq!(advisory, None, "no advisory on a plain unmanaged workspace");
    }
}
