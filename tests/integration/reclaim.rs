//! Audit C1 — worktree reclamation for failed/canceled specs.
//!
//! ## The finding these tests close
//!
//! `teardown` is reachable only as the terminal phase of the SUCCESS
//! pipeline; `SpecFailed` / `SpecCanceled` / crash-recovery never touched
//! disk, so failed/canceled specs leaked their worktrees forever (36 GB in 4
//! days, live), `boi clean` deleted the DB rows that map directories to
//! specs while leaving the gigabytes, and dead `.git/worktrees/` entries
//! polluted operator repos. The locked design promises "worktrees stay until
//! `boi clean`" and a `[worktree].auto_clean_canceled_after = "7 days"`
//! retention — neither was implemented.
//!
//! ## What is proven here, end-to-end (real git repos, real tempdir SQLite)
//!
//! 1. `boi clean <spec-id>` (the `cli::clean::clean_spec_with_reclaim` core
//!    the command calls) removes a failed spec's worktree directories from
//!    disk AND prunes the git worktree registrations in the operator
//!    workspace — and still deletes the rows (semantics unchanged).
//! 2. A DIRTY worktree (uncommitted change to a tracked file) is SKIPPED and
//!    loudly reported in the command's summary — never silently destroyed
//!    (audit A1's lesson) — while clean siblings are reclaimed and the row
//!    cascade still runs.
//! 3. The sweeper's auto-clean pass (design §5 `auto_clean_canceled_after`,
//!    default 7 days, applied to FAILED specs too by operator decision)
//!    reclaims a canceled spec older than the window and LEAVES a younger
//!    one — failed specs stay revivable (audit A2) for the whole window.
//!
//! Hermetic: tempdir workspaces + worktree roots + `sqlite::memory:` pools.
//! Never touches `~/.boi/v2`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use boi::cli::clean::clean_spec_with_reclaim;
use boi::repo;
use boi::repo::spec_runtime::TerminalReason;
use boi::repo::spec_versions::VersionTrigger;
use boi::runtime::git_ops;
use boi::runtime::reclaim::SpecWorktreeReclaimer;
use boi::runtime::worktree::{
    integration_branch, integration_worktree, integration_worktree_name, task_branch,
    task_worktree, task_worktree_name,
};
use boi::service::{EventBus, Sweeper};
use boi::types::context::SpecContract;
use boi::types::ids::{SpecId, TaskId};
use boi::types::reasons::{CancellationReason, FailureReason};
use boi::types::state::SpecStatus;
use chrono::{Duration as ChronoDuration, Utc};
use sqlx::SqlitePool;
use tokio::sync::mpsc;

/// A throwaway directory removed on drop — `std`-only (no `tempfile` dep).
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("boi-reclaim-it-{}-{tag}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.path));
    }
}

/// Init a workspace repo with one commit on `main`.
fn init_workspace_repo(path: &Path) {
    use git2::{Repository, Signature};
    std::fs::create_dir_all(path).expect("mkdir workspace");
    let repo = Repository::init(path).expect("git init");
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

/// Seed the DB rows `boi clean` / the sweeper read for `spec_id`: the
/// identity row, a v1 snapshot whose `spec_contract.workspace` points at
/// `workspace`, the runtime row, and a terminal status stamped at
/// `terminal_at` (the auto-clean window keys off `completed_at`).
async fn seed_terminal_spec(
    pool: &SqlitePool,
    spec_id: &SpecId,
    workspace: &Path,
    status: SpecStatus,
    reason: TerminalReason,
    terminal_at: chrono::DateTime<Utc>,
) {
    repo::specs::insert_spec(pool, spec_id, Utc::now())
        .await
        .expect("insert_spec");
    let contract = SpecContract {
        scope: "audit C1 reclaim test".into(),
        workspace: workspace.to_path_buf(),
        base_branch: "main".into(),
        exclusions: vec![],
        verifications: vec![],
        must_emit: vec![],
    };
    let snapshot = serde_json::json!({
        "title": "reclaim-fixture",
        "spec_contract": serde_json::to_value(&contract).expect("contract to json"),
    });
    repo::spec_versions::append_version(
        pool,
        spec_id,
        1,
        &snapshot,
        VersionTrigger::Dispatch,
        None,
        Utc::now(),
    )
    .await
    .expect("append_version");
    repo::spec_runtime::initialize(pool, spec_id, 1)
        .await
        .expect("initialize spec_runtime");
    repo::spec_runtime::update_status(pool, spec_id, status, Some(reason), terminal_at)
        .await
        .expect("update_status to terminal");
}

/// Stand up the §5 disk layout for `spec_id`: an integration worktree and one
/// task worktree, registered in `workspace`. Returns (integration, task).
fn build_spec_worktrees(
    workspace: &Path,
    root: &Path,
    spec_id: &SpecId,
    task_id: &TaskId,
) -> (PathBuf, PathBuf) {
    git_ops::create_branch(workspace, &integration_branch(spec_id), "main").expect("int branch");
    git_ops::create_branch(workspace, &task_branch(spec_id, task_id), "main").expect("task branch");
    let integration = integration_worktree(root, spec_id);
    let task = task_worktree(root, spec_id, task_id);
    git_ops::add_worktree(
        workspace,
        &integration_branch(spec_id),
        &integration_worktree_name(spec_id),
        &integration,
    )
    .expect("add integration worktree");
    git_ops::add_worktree(
        workspace,
        &task_branch(spec_id, task_id),
        &task_worktree_name(spec_id, task_id),
        &task,
    )
    .expect("add task worktree");
    (integration, task)
}

/// The registered worktree names in `workspace`.
fn registration_names(workspace: &Path) -> Vec<String> {
    let repo = git2::Repository::open(workspace).expect("open workspace");
    repo.worktrees()
        .expect("list worktrees")
        .iter()
        .flatten()
        .map(str::to_owned)
        .collect()
}

fn spec_a() -> SpecId {
    SpecId::new("S0000001a").unwrap()
}
fn spec_b() -> SpecId {
    SpecId::new("S0000002b").unwrap()
}
fn spec_c() -> SpecId {
    SpecId::new("S0000003c").unwrap()
}
fn task_a() -> TaskId {
    TaskId::new("T0000001a").unwrap()
}

/// (1) `boi clean` on a FAILED spec removes its worktree directories from
/// disk, prunes the workspace's git registrations, and still runs the row
/// cascade (row semantics unchanged — audit C1 is disk + registrations only).
#[tokio::test]
async fn test_l3_reclaim_clean_removes_failed_spec_worktrees_and_prunes_registrations() {
    let dir = TempDir::new("clean-failed");
    let workspace = dir.path.join("workspace");
    init_workspace_repo(&workspace);
    let root = dir.path.join("worktrees");

    let pool = repo::connect("sqlite::memory:").await.expect("pool");
    seed_terminal_spec(
        &pool,
        &spec_a(),
        &workspace,
        SpecStatus::Failed,
        TerminalReason::Failure(FailureReason::DaemonCrash),
        Utc::now(),
    )
    .await;
    let (integration, task) = build_spec_worktrees(&workspace, &root, &spec_a(), &task_a());

    let summary = clean_spec_with_reclaim(&pool, &spec_a(), false, &root)
        .await
        .expect("clean succeeds");

    // Disk: both worktree dirs AND the emptied spec dir are gone.
    assert!(!integration.exists(), "integration dir reclaimed");
    assert!(!task.exists(), "task dir reclaimed");
    assert!(
        !root.join(spec_a().as_str()).exists(),
        "emptied <root>/<SpecId>/ removed",
    );
    // Registrations: the operator repo carries no spec-… entries any more.
    assert!(
        registration_names(&workspace).is_empty(),
        "git worktree registrations pruned, got {:?}",
        registration_names(&workspace),
    );
    // The summary carries the evidence.
    let reclaim = summary.reclaim.as_ref().expect("reclaim ran");
    assert_eq!(reclaim.removed.len(), 2, "both dirs reported removed");
    assert!(reclaim.skipped_dirty.is_empty());
    assert!(reclaim.failed.is_empty(), "faults: {:?}", reclaim.failed);
    assert!(summary.workspace_unresolved.is_none());
    // Rows: unchanged cascade semantics — the spec is gone.
    let err = repo::spec_runtime::fetch(&pool, &spec_a())
        .await
        .expect_err("rows deleted");
    assert!(
        err.to_string().contains("not found") || format!("{err:?}").contains("NotFound"),
        "spec_runtime row gone, got {err:?}",
    );
}

/// (2) A DIRTY worktree is skipped — reported in the summary, directory and
/// registration intact — while the clean sibling is reclaimed and the row
/// cascade still runs (never silently destroy work; audit A1's lesson).
#[tokio::test]
async fn test_l3_reclaim_clean_skips_dirty_worktree_and_reports_it_loudly() {
    let dir = TempDir::new("clean-dirty");
    let workspace = dir.path.join("workspace");
    init_workspace_repo(&workspace);
    let root = dir.path.join("worktrees");

    let pool = repo::connect("sqlite::memory:").await.expect("pool");
    seed_terminal_spec(
        &pool,
        &spec_a(),
        &workspace,
        SpecStatus::Failed,
        TerminalReason::Failure(FailureReason::DaemonCrash),
        Utc::now(),
    )
    .await;
    let (integration, task) = build_spec_worktrees(&workspace, &root, &spec_a(), &task_a());

    // Uncommitted edit to a TRACKED file — the work the dirty-check protects.
    std::fs::write(task.join("README.md"), "uncommitted operator work\n").unwrap();

    let summary = clean_spec_with_reclaim(&pool, &spec_a(), false, &root)
        .await
        .expect("clean succeeds");

    let reclaim = summary.reclaim.as_ref().expect("reclaim ran");
    assert_eq!(
        reclaim.skipped_dirty,
        vec![task.clone()],
        "the dirty worktree is reported, not destroyed",
    );
    assert!(
        task.join("README.md").is_file(),
        "dirty worktree dir survives"
    );
    let contents = std::fs::read_to_string(task.join("README.md")).unwrap();
    assert_eq!(
        contents, "uncommitted operator work\n",
        "the uncommitted work is byte-for-byte intact",
    );
    assert_eq!(
        reclaim.removed,
        vec![integration.clone()],
        "the clean sibling is still reclaimed",
    );
    assert!(!integration.exists());
    // The dirty worktree keeps its (valid) registration.
    assert_eq!(
        registration_names(&workspace),
        vec![task_worktree_name(&spec_a(), &task_a())],
    );
    // Rows: cascade ran regardless (row semantics unchanged).
    assert!(
        repo::spec_runtime::fetch(&pool, &spec_a()).await.is_err(),
        "rows deleted even with a dirty skip — the skip is loud, not blocking",
    );
}

/// (3) The sweeper's auto-clean pass reclaims a CANCELED spec older than the
/// retention window AND an old FAILED spec (operator decision: same window),
/// but LEAVES a younger canceled spec — failed/canceled specs stay revivable
/// (audit A2) for the whole window.
#[tokio::test]
async fn test_l3_reclaim_auto_clean_reclaims_only_specs_older_than_the_window() {
    let dir = TempDir::new("auto-clean");
    let workspace = dir.path.join("workspace");
    init_workspace_repo(&workspace);
    let root = dir.path.join("worktrees");
    let now = Utc::now();
    let window = StdDuration::from_secs(7 * 24 * 60 * 60);

    let pool = repo::connect("sqlite::memory:").await.expect("pool");
    // Spec A — canceled 8 days ago: PAST the window, must be reclaimed.
    seed_terminal_spec(
        &pool,
        &spec_a(),
        &workspace,
        SpecStatus::Canceled,
        TerminalReason::Cancellation(CancellationReason::Operator { note: None }),
        now - ChronoDuration::days(8),
    )
    .await;
    // Spec B — failed 9 days ago: PAST the window; failed specs use the same
    // window (audit C1 operator decision).
    seed_terminal_spec(
        &pool,
        &spec_b(),
        &workspace,
        SpecStatus::Failed,
        TerminalReason::Failure(FailureReason::DaemonCrash),
        now - ChronoDuration::days(9),
    )
    .await;
    // Spec C — canceled 1 day ago: INSIDE the window, must be left alone.
    seed_terminal_spec(
        &pool,
        &spec_c(),
        &workspace,
        SpecStatus::Canceled,
        TerminalReason::Cancellation(CancellationReason::Operator { note: None }),
        now - ChronoDuration::days(1),
    )
    .await;

    let task = task_a();
    let (a_int, a_task) = build_spec_worktrees(&workspace, &root, &spec_a(), &task);
    let (b_int, _b_task) = {
        // Spec B gets only an integration worktree — branch names differ per
        // spec so a second task branch isn't needed for the proof.
        git_ops::create_branch(&workspace, &integration_branch(&spec_b()), "main").unwrap();
        let b_int = integration_worktree(&root, &spec_b());
        git_ops::add_worktree(
            &workspace,
            &integration_branch(&spec_b()),
            &integration_worktree_name(&spec_b()),
            &b_int,
        )
        .unwrap();
        (b_int, ())
    };
    let c_int = {
        git_ops::create_branch(&workspace, &integration_branch(&spec_c()), "main").unwrap();
        let c_int = integration_worktree(&root, &spec_c());
        git_ops::add_worktree(
            &workspace,
            &integration_branch(&spec_c()),
            &integration_worktree_name(&spec_c()),
            &c_int,
        )
        .unwrap();
        c_int
    };

    // A sweeper wired exactly like boot wires it (real reclaimer, real pool).
    let bus = Arc::new(EventBus::new(pool.clone(), vec![]));
    let (daemon_tx, _daemon_rx) = mpsc::channel(16);
    let sweeper = Sweeper {
        bus,
        daemon_tx,
        pool: pool.clone(),
        threshold: StdDuration::from_secs(86_400),
        wall_clock_budget: StdDuration::from_secs(86_400),
        reclaimer: Some(Arc::new(SpecWorktreeReclaimer {
            worktree_root: root.clone(),
        })),
        auto_clean_after: window,
        auto_clean_pass_interval: StdDuration::ZERO,
        last_auto_clean_pass: Mutex::new(None),
        worktree_root: None,
    };

    sweeper.tick(now).await.expect("tick runs");

    // Specs A + B (past the window) — reclaimed: dirs gone, registrations gone.
    assert!(!a_int.exists(), "old canceled spec's integration reclaimed");
    assert!(!a_task.exists(), "old canceled spec's task reclaimed");
    assert!(
        !b_int.exists(),
        "old FAILED spec reclaimed too (same window)"
    );
    // Spec C (inside the window) — untouched: still revivable with its state.
    assert!(
        c_int.join("README.md").is_file(),
        "young canceled spec's worktree is LEFT for the whole window",
    );
    let names = registration_names(&workspace);
    assert_eq!(
        names,
        vec![integration_worktree_name(&spec_c())],
        "only the young spec keeps a registration, got {names:?}",
    );
    // Rows are NOT touched by auto-clean (disk + registrations only).
    assert!(repo::spec_runtime::fetch(&pool, &spec_a()).await.is_ok());
    assert!(repo::spec_runtime::fetch(&pool, &spec_b()).await.is_ok());
    assert!(repo::spec_runtime::fetch(&pool, &spec_c()).await.is_ok());
}
