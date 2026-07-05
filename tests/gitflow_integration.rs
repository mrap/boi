//! GitFlow branch-policy — the §7.1 hermetic integration battery (AC-4 /
//! AC-5 / AC-6, plus the M14 checkout-independence variant and the AC-16 /
//! M6 unmanaged-workspace-protection test leg).
//!
//! Unlike the in-crate Layer-3 battery (`src/runtime/worktree.rs`'s
//! `test_l3_branch_policy_*` tests), this is an EXTERNAL test crate: it can
//! only reach BOI through the crate's public surface — exactly what a direct
//! consumer (or a future non-CLI caller) sees. Every `StepCtx` here is
//! constructed by hand, never via `parse_spec`/dispatch: that is the AC-6
//! stale-snapshot / direct-socket bypass class the Layer-3 re-checks (R-B8)
//! exist for.
//!
//! "Hermetic" per §7.1: every repo mutated is a temp repo created by the
//! test; no daemon, no LLM, no network, no operator state. Markers land on
//! branches as COMMITTED trees via odb/ref plumbing — the D-13 read semantics
//! (`refs/heads/<base_branch>:.boi-policy.toml`) make an uncommitted
//! working-tree marker test nothing — and no helper here performs any
//! checkout (the AC-14 do-not-worsen posture: no forced-checkout machinery
//! anywhere near this battery).
//!
//! ## Lint posture
//!
//! `Cargo.toml`'s `unwrap_used` / `expect_used` / `panic` are `warn` lints;
//! `clippy --all-targets -D warnings` escalates them. `clippy.toml`'s
//! `allow-*-in-tests` keys exempt test-attribute bodies, but the fixture
//! helpers below are not test items — and a helper `.expect()`-ing on a
//! broken temp repo is the correct loud-fail behaviour — so the lints are
//! allowed crate-wide (this whole crate is test support).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use git2::{Repository, Signature};

use boi::config::{self, Delivery};
use boi::runtime::StepRun;
use boi::runtime::branch_policy::{self, PolicyVerdict};
use boi::runtime::preflight::branch_policy_gate;
use boi::runtime::worktree::{integration_branch, integration_worktree, merge_spec, prepare_spec};
use boi::types::context::SpecContract;
use boi::types::ids::{PhaseRunId, SpecId};
use boi::types::reasons::ErrorWhyFix;
use boi::types::step::{StepCtx, StepOutcome};

/// A gitflow marker body with the default protection (`protected = ["main"]`).
const GITFLOW_MARKER: &str = "model = \"gitflow\"\n";

/// A throwaway directory removed on drop — `std`-only (BOI takes no
/// `tempfile` dependency; the in-crate tests use the same pattern).
struct TempDir {
    /// The directory path.
    path: PathBuf,
}

impl TempDir {
    /// Create a uniquely-named throwaway directory under the system temp dir.
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("boi-gitflow-{}-{tag}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.path));
    }
}

// ---------------------------------------------------------------------------
// Temp-repo plumbing — no checkout anywhere. The repo SEED writes through
// the index + working tree so the attached checkout starts clean (see
// `init_repo_on_main`); everything after the seed is odb/ref writes only.
// The in-crate `branch_policy::testkit` is `pub(crate)` (deliberately not
// part of the public surface), so this external crate carries its own copy
// of the same fixture pattern.
// ---------------------------------------------------------------------------

/// The committer/author for fixture commits — fixed, so tests never depend
/// on the machine's git identity config.
fn sig() -> Signature<'static> {
    Signature::now("test", "test@localhost").expect("signature")
}

/// Init a repo at `path` with one commit on `main` (a `README.md`) and HEAD
/// attached to `refs/heads/main`.
///
/// The seed commit is written THROUGH the index and working tree (the
/// `commit_task_output` pattern), not raw odb plumbing, so the attached
/// `main` checkout starts CLEAN — index == HEAD tree == working tree. That
/// is the real operator-checkout shape (R-B14 guard-independence: an
/// odb-only seed leaves every committed file reading as a staged deletion,
/// i.e. an accidentally-dirty checkout, and the audit-A1 dirty-checkout
/// guard then refuses any merge whose target is the checked-out branch —
/// the AC-16 leg). Later fixture commits (markers) stay odb-only on
/// purpose: D-13 reads committed trees, and none of their branches is the
/// checked-out merge target.
///
/// `core.hooksPath` is pinned to a repo-local (nonexistent) directory: the
/// machine running these tests may have a GLOBAL `core.hooksPath` (e.g. an
/// operator main-guard `reference-transaction` hook) that would otherwise
/// shadow repo hooks for any CLI git run against the fixture. libgit2 — the
/// only thing this battery uses — executes no hooks either way (§1.1 of the
/// requirements pins that), so the pin is belt-and-braces hermeticity, not a
/// behaviour the assertions rely on.
fn init_repo_on_main(path: &Path) {
    let repo = Repository::init(path).expect("init repo");
    repo.config()
        .expect("repo config")
        .set_str("core.hooksPath", "hooks-disabled")
        .expect("pin core.hooksPath");
    std::fs::write(path.join("README.md"), "hello\n").expect("write README");
    let mut index = repo.index().expect("index");
    index
        .add_path(Path::new("README.md"))
        .expect("stage README");
    index.write().expect("write index");
    let tree_id = index.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("tree");
    repo.commit(
        Some("refs/heads/main"),
        &sig(),
        &sig(),
        "initial",
        &tree,
        &[],
    )
    .expect("initial commit");
    repo.set_head("refs/heads/main").expect("set HEAD");
}

/// Commit `files` (name → content) onto `refs/heads/<branch>` — pure odb/ref
/// plumbing. Existing tree entries are preserved; the named entries are
/// added/replaced at the tree root. No checkout is touched.
fn commit_on_branch(path: &Path, branch: &str, files: &[(&str, &str)]) {
    let repo = Repository::open(path).expect("open repo");
    let full_ref = format!("refs/heads/{branch}");
    let parent = repo
        .find_reference(&full_ref)
        .expect("branch ref")
        .peel_to_commit()
        .expect("branch tip");
    let base_tree = parent.tree().expect("tip tree");
    let mut builder = repo.treebuilder(Some(&base_tree)).expect("treebuilder");
    for (name, content) in files {
        let blob = repo.blob(content.as_bytes()).expect("blob");
        builder.insert(*name, blob, 0o100644).expect("insert entry");
    }
    let tree = repo
        .find_tree(builder.write().expect("write tree"))
        .expect("tree");
    repo.commit(
        Some(&full_ref),
        &sig(),
        &sig(),
        "policy update",
        &tree,
        &[&parent],
    )
    .expect("commit on branch");
}

/// Create `refs/heads/<name>` at `main`'s current tip.
fn branch_from_main(path: &Path, name: &str) {
    let repo = Repository::open(path).expect("open repo");
    let main_tip = repo
        .find_reference("refs/heads/main")
        .expect("main")
        .peel_to_commit()
        .expect("main tip");
    repo.branch(name, &main_tip, false).expect("create branch");
}

/// The hex OID of `refs/heads/<name>`.
fn branch_oid(path: &Path, name: &str) -> String {
    let repo = Repository::open(path).expect("open repo");
    repo.find_reference(&format!("refs/heads/{name}"))
        .expect("branch ref")
        .peel_to_commit()
        .expect("tip")
        .id()
        .to_string()
}

/// Point HEAD at `refs/heads/<branch>` WITHOUT touching the working tree —
/// the M14 "checkout parked on a markerless throwaway branch" setup.
fn park_head_on(path: &Path, branch: &str) {
    let repo = Repository::open(path).expect("open repo");
    repo.set_head(&format!("refs/heads/{branch}"))
        .expect("park HEAD");
}

/// Detach HEAD at `main`'s tip — the other M14 parking position.
fn detach_head(path: &Path) {
    let repo = Repository::open(path).expect("open repo");
    let oid = repo
        .find_reference("refs/heads/main")
        .expect("main")
        .peel_to_commit()
        .expect("tip")
        .id();
    repo.set_head_detached(oid).expect("detach HEAD");
}

/// The `refs/heads/spec/…` branch names present in the repo — the AC-5
/// "no `spec/*` branch created in the prepare variant" assertion surface.
fn spec_branches(path: &Path) -> Vec<String> {
    let repo = Repository::open(path).expect("open repo");
    repo.references_glob("refs/heads/spec/*")
        .expect("glob refs")
        .names()
        .map(|name| name.expect("ref name").to_owned())
        .collect()
}

/// Build the §7.1 GitFlow temp repo: one commit on `main`, the gitflow
/// marker COMMITTED on main (D-13 reads committed trees), and `develop`
/// branched at main's tip — so the marker sits on BOTH long-lived branches,
/// exactly as §7.1 step 1 prescribes.
fn init_gitflow_repo(path: &Path) {
    init_repo_on_main(path);
    commit_on_branch(path, "main", &[(".boi-policy.toml", GITFLOW_MARKER)]);
    branch_from_main(path, "develop");
}

// ---------------------------------------------------------------------------
// StepCtx + step-driving helpers — every ctx is constructed DIRECTLY (no
// parse_spec, no dispatch gate): the Layer-1/Layer-2 bypass Layer 3 exists
// for (AC-6).
// ---------------------------------------------------------------------------

/// A `SpecContract` rooted at `workspace`, delivering to `base`.
fn contract(workspace: &Path, base: &str) -> SpecContract {
    SpecContract {
        scope: "gitflow integration battery".into(),
        workspace: workspace.to_path_buf(),
        base_branch: base.into(),
        exclusions: vec![],
        verifications: vec![],
        must_emit: vec![],
    }
}

/// A spec-level `StepCtx` for `phase`, operating in `worktree_path`.
fn step_ctx(
    spec_id: &SpecId,
    phase: &str,
    worktree_path: PathBuf,
    spec_contract: SpecContract,
) -> Arc<StepCtx> {
    Arc::new(StepCtx {
        spec_id: spec_id.clone(),
        task_id: None,
        phase_run_id: PhaseRunId::new("P0000001a").unwrap(),
        phase: phase.into(),
        worktree_path,
        branch_ref: "n/a".into(),
        spec_contract,
        task_contract: None,
    })
}

/// Run `prepare_spec` for `spec_id` against `workspace` on `base`.
async fn run_prepare(spec_id: &SpecId, root: &Path, workspace: &Path, base: &str) -> StepRun {
    prepare_spec(step_ctx(
        spec_id,
        "workspace_prepare",
        integration_worktree(root, spec_id),
        contract(workspace, base),
    ))
    .await
    .expect("prepare_spec must not error")
}

/// Run `merge_spec` for `spec_id` against `workspace` on `base`.
async fn run_merge(spec_id: &SpecId, root: &Path, workspace: &Path, base: &str) -> StepRun {
    merge_spec(step_ctx(
        spec_id,
        "merge",
        integration_worktree(root, spec_id),
        contract(workspace, base),
    ))
    .await
    .expect("merge_spec must not error")
}

/// Commit a synthetic change in the integration worktree — §7.1 step 2's
/// "synthetic task commit", written and committed the way the engine's own
/// `commit` step does it (index add → tree → commit on the worktree's HEAD,
/// which is the integration branch).
fn commit_task_output(worktree: &Path) {
    std::fs::write(worktree.join("spec_work.txt"), "spec contribution\n")
        .expect("write task output");
    let repo = Repository::open(worktree).expect("open integration worktree");
    let mut index = repo.index().expect("index");
    index
        .add_path(Path::new("spec_work.txt"))
        .expect("stage task output");
    index.write().expect("write index");
    let tree_id = index.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("tree");
    let parent = repo
        .head()
        .expect("worktree HEAD")
        .peel_to_commit()
        .expect("integration tip");
    repo.commit(
        Some("HEAD"),
        &sig(),
        &sig(),
        "boi: synthetic task output",
        &tree,
        &[&parent],
    )
    .expect("commit task output");
}

/// Assert a `StepRun` passed.
fn assert_pass(run: &StepRun) {
    assert!(
        matches!(run.outcome, StepOutcome::Pass { .. }),
        "expected Pass, got {:?}",
        run.outcome
    );
}

/// Assert a `StepRun` failed; hand back the error/why/fix triple.
fn assert_fail(run: &StepRun) -> ErrorWhyFix {
    let StepOutcome::Fail { error_why_fix } = &run.outcome else {
        panic!("expected Fail, got {:?}", run.outcome);
    };
    error_why_fix.clone()
}

/// The spec id used by single-spec tests.
fn spec_id() -> SpecId {
    SpecId::new("S0000001a").unwrap()
}

/// A second spec id, for tests that drive two lifecycles in one repo.
fn second_spec_id() -> SpecId {
    SpecId::new("S0000002a").unwrap()
}

// ---------------------------------------------------------------------------
// The battery.
// ---------------------------------------------------------------------------

/// AC-4 (positive proof): on a GitFlow workspace, a `base_branch =
/// "develop"` spec driven `prepare_spec` → synthetic task commit →
/// `merge_spec` lands its delivery on `develop` — develop advances to the
/// integration tip and `refs/heads/main` is byte-identical before/after.
#[tokio::test]
async fn test_l3_branch_policy_develop_delivery_advances_develop_only() {
    let dir = TempDir::new("ac4-positive");
    let repo = dir.path.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_gitflow_repo(&repo);
    let root = dir.path.join("worktrees");
    let main_before = branch_oid(&repo, "main");
    let develop_before = branch_oid(&repo, "develop");

    let run = run_prepare(&spec_id(), &root, &repo, "develop").await;
    assert_pass(&run);
    commit_task_output(&integration_worktree(&root, &spec_id()));
    let run = run_merge(&spec_id(), &root, &repo, "develop").await;
    assert_pass(&run);

    let integration_tip = branch_oid(&repo, &integration_branch(&spec_id()));
    assert_eq!(
        branch_oid(&repo, "develop"),
        integration_tip,
        "develop must fast-forward to the integration tip",
    );
    assert_ne!(
        branch_oid(&repo, "develop"),
        develop_before,
        "the synthetic task commit must actually advance develop",
    );
    assert_eq!(
        branch_oid(&repo, "main"),
        main_before,
        "refs/heads/main must be byte-identical after a develop delivery (R-B10)",
    );
}

/// AC-5 (negative proof) + AC-6 (both-steps backstop): a `StepCtx`
/// constructed directly — bypassing the dispatch gate entirely — with
/// `base_branch = "main"` on a GitFlow workspace is refused by BOTH
/// `prepare_spec` and `merge_spec` with the typed protected reason;
/// `refs/heads/main` never moves; the prepare variant creates no `spec/*`
/// branch.
#[tokio::test]
async fn test_l3_branch_policy_protected_main_refused_at_both_steps() {
    let dir = TempDir::new("ac5-protected");
    let repo = dir.path.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_gitflow_repo(&repo);
    let root = dir.path.join("worktrees");
    let main_before = branch_oid(&repo, "main");

    // prepare_spec refuses, before any branch creation.
    let run = run_prepare(&spec_id(), &root, &repo, "main").await;
    let ewf = assert_fail(&run);
    assert!(
        ewf.error
            .contains("branch policy refuses base branch `main`"),
        "typed protected refusal at prepare, got: {}",
        ewf.error,
    );
    assert!(
        ewf.fix.contains("base_branch = \"develop\""),
        "the fix teaches develop, got: {}",
        ewf.fix,
    );
    assert_eq!(
        spec_branches(&repo),
        Vec::<String>::new(),
        "AC-5: the prepare refusal must precede any spec/* branch creation",
    );

    // merge_spec refuses too — Layer 3 holds at both consumption sites
    // without Layer 1 ever running (AC-6).
    let run = run_merge(&spec_id(), &root, &repo, "main").await;
    let ewf = assert_fail(&run);
    assert!(
        ewf.error
            .contains("branch policy refuses base branch `main`"),
        "typed protected refusal at merge, got: {}",
        ewf.error,
    );

    assert_eq!(
        branch_oid(&repo, "main"),
        main_before,
        "R-B10: refs/heads/main must be byte-identical after both refusals",
    );
}

/// AC-6 (stale-snapshot simulation): a spec that legitimately STARTED on an
/// unmanaged workspace (`base_branch = "main"` allowed, M6) is refused at
/// `merge_spec` when the gitflow marker lands on main mid-spec — the Layer-3
/// re-check reads the committed tree fresh at consumption time and never
/// trusts the prepare-time verdict (R-B8 TOCTOU). A second, freshly-prepared
/// spec is refused at `prepare_spec` too: both re-check sites see the new
/// policy.
#[tokio::test]
async fn test_l3_branch_policy_stale_snapshot_refused_when_marker_lands_mid_spec() {
    let dir = TempDir::new("ac6-stale-snapshot");
    let repo = dir.path.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo_on_main(&repo); // unmanaged: no marker anywhere.
    let root = dir.path.join("worktrees");

    // The "stale snapshot": prepared while unmanaged, so main is allowed.
    let run = run_prepare(&spec_id(), &root, &repo, "main").await;
    assert_pass(&run);
    commit_task_output(&integration_worktree(&root, &spec_id()));

    // The workspace adopts GitFlow mid-spec — the marker lands on main.
    commit_on_branch(&repo, "main", &[(".boi-policy.toml", GITFLOW_MARKER)]);
    let main_after_marker = branch_oid(&repo, "main");

    // merge_spec must refuse with the typed protected reason — fresh read,
    // nothing mutated.
    let run = run_merge(&spec_id(), &root, &repo, "main").await;
    let ewf = assert_fail(&run);
    assert!(
        ewf.error
            .contains("branch policy refuses base branch `main`"),
        "the mid-spec marker must refuse the merge, got: {}",
        ewf.error,
    );
    assert_eq!(
        branch_oid(&repo, "main"),
        main_after_marker,
        "the engine must not move main after the policy landed",
    );

    // And a NEW spec prepared after the marker is refused up front.
    let run = run_prepare(&second_spec_id(), &root, &repo, "main").await;
    let ewf = assert_fail(&run);
    assert!(
        ewf.error
            .contains("branch policy refuses base branch `main`"),
        "prepare after the marker landed must refuse, got: {}",
        ewf.error,
    );
}

/// M14 / §7.1.6 (checkout-independence): with the operator checkout parked
/// on a markerless throwaway branch (positive leg) and then detached
/// (negative leg), outcomes are IDENTICAL to the attached-checkout tests —
/// the D-13 policy read comes from `refs/heads/<base_branch>`'s committed
/// tree, never from whatever the checkout happens to show.
#[tokio::test]
async fn test_l3_branch_policy_enforcement_is_checkout_independent() {
    let dir = TempDir::new("m14-checkout-independence");
    let repo = dir.path.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo_on_main(&repo);
    // `scratch` branches off BEFORE the marker commit: its tree has no
    // `.boi-policy.toml` — the markerless throwaway parking spot.
    branch_from_main(&repo, "scratch");
    commit_on_branch(&repo, "main", &[(".boi-policy.toml", GITFLOW_MARKER)]);
    branch_from_main(&repo, "develop");
    park_head_on(&repo, "scratch");
    let root = dir.path.join("worktrees");
    let main_before = branch_oid(&repo, "main");

    // Positive leg (mirrors AC-4): develop delivery passes, develop
    // advances, main untouched — even though the checkout's tree has no
    // marker at all.
    let run = run_prepare(&spec_id(), &root, &repo, "develop").await;
    assert_pass(&run);
    commit_task_output(&integration_worktree(&root, &spec_id()));
    let run = run_merge(&spec_id(), &root, &repo, "develop").await;
    assert_pass(&run);
    assert_eq!(
        branch_oid(&repo, "develop"),
        branch_oid(&repo, &integration_branch(&spec_id())),
        "M14 positive: develop must advance exactly as with an attached checkout",
    );
    assert_eq!(branch_oid(&repo, "main"), main_before);

    // Negative leg (mirrors AC-5), now with HEAD detached: a main-targeted
    // spec is refused at both steps and main never moves.
    detach_head(&repo);
    let run = run_prepare(&second_spec_id(), &root, &repo, "main").await;
    let ewf = assert_fail(&run);
    assert!(
        ewf.error
            .contains("branch policy refuses base branch `main`"),
        "M14 negative: typed protected refusal at prepare, got: {}",
        ewf.error,
    );
    let run = run_merge(&second_spec_id(), &root, &repo, "main").await;
    let ewf = assert_fail(&run);
    assert!(
        ewf.error
            .contains("branch policy refuses base branch `main`"),
        "M14 negative: typed protected refusal at merge, got: {}",
        ewf.error,
    );
    assert_eq!(
        branch_oid(&repo, "main"),
        main_before,
        "M14 negative: refs/heads/main must be byte-identical",
    );
}

/// AC-16 (test leg) / M6 — unmanaged-workspace protection: on an unmanaged workspace
/// (no marker, no develop) a `base_branch = "main"` spec is allowed
/// end-to-end through the dispatch-validation surface — `parse_spec`, the
/// delivery gate's accepted value, a clean verify-lint, the Layer-1 policy
/// verdict (Allow, NO advisory), the Layer-2 preflight gate — and the
/// runtime lifecycle delivers to main exactly as today: main fast-forwards
/// to the integration tip.
#[tokio::test]
async fn test_l3_branch_policy_unmanaged_main_spec_allowed_end_to_end() {
    let dir = TempDir::new("ac16-unmanaged");
    let repo = dir.path.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_repo_on_main(&repo); // no marker, no develop — the unmanaged-workspace shape.
    let root = dir.path.join("worktrees");
    let main_before = branch_oid(&repo, "main");

    // A real spec file, through the real parse/validate layer.
    let spec_text = format!(
        r#"title = "GitFlow AC-16 regression — unmanaged main delivery"

[contract]
scope = "Prove an unmanaged workspace keeps today's main-based delivery"
base_branch = "main"
workspace = "{}"

[[tasks]]
ref = "hello"
behavior = "Create spec_work.txt"
verifications = [
  {{ intent = "spec_work.txt exists on the delivered branch" }},
]
"#,
        repo.display()
    );
    let spec = config::parse_spec(&spec_text).expect("the M6 spec must parse");
    assert_eq!(spec.delivery, Delivery::Merge, "the shipped delivery");
    assert!(
        config::lint(&spec).is_empty(),
        "the M6 spec must pass verify-lint",
    );

    // Layer 1's verdict surface: Allow with NO advisory (no develop branch
    // exists, so even the M8 migration aid stays silent).
    let policy = branch_policy::load_policy_blocking(&spec.contract.workspace, "main");
    assert_eq!(
        policy.verdict("main"),
        PolicyVerdict::Allow { advisory: None },
        "M6: an unmanaged workspace allows main with no advisory",
    );

    // Layer 2's gate: the daemon-side preflight passes.
    branch_policy_gate(spec.contract.workspace.clone(), "main".into())
        .await
        .expect("M6: the preflight backstop must allow an unmanaged main spec");

    // Layer 3 + delivery: the full lifecycle runs on the PARSED contract and
    // lands on main — today's behavior, byte-for-byte (N-1).
    let prepare_ctx = step_ctx(
        &spec_id(),
        "workspace_prepare",
        integration_worktree(&root, &spec_id()),
        spec.contract.clone(),
    );
    assert_pass(&prepare_spec(prepare_ctx).await.expect("prepare runs"));
    commit_task_output(&integration_worktree(&root, &spec_id()));
    let merge_ctx = step_ctx(
        &spec_id(),
        "merge",
        integration_worktree(&root, &spec_id()),
        spec.contract.clone(),
    );
    assert_pass(&merge_spec(merge_ctx).await.expect("merge runs"));

    assert_eq!(
        branch_oid(&repo, "main"),
        branch_oid(&repo, &integration_branch(&spec_id())),
        "AC-16: the unmanaged delivery must land on main, as today",
    );
    assert_ne!(
        branch_oid(&repo, "main"),
        main_before,
        "AC-16: main must actually advance on an unmanaged workspace",
    );
}
