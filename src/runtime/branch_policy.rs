//! Workspace branch-policy — the GitFlow-awareness decision core + marker
//! loader (GitFlow program R-B4/R-B5; behavior matrix M1-M15).
//!
//! A workspace declares its branch model to the engine with a marker file at
//! the repo root: `.boi-policy.toml` (`model = "gitflow" | "trunk"`, optional
//! `protected = [..]`). The engine reads the marker from the **committed tree
//! of the spec's `base_branch`** (`refs/heads/<base_branch>:.boi-policy.toml`,
//! a libgit2 odb read — never any checkout's working tree; D-13). A
//! working-tree read would vary with whatever branch the operator checkout
//! happens to have checked out, silently disabling enforcement — the exact
//! quiet-failure class this module exists to close.
//!
//! ## The two halves
//!
//! - **Pure decision core** — [`evaluate`]: `(policy source, base_branch,
//!   ref existence) -> PolicyVerdict`. No I/O; the full behavior matrix is
//!   unit-testable without a git repo.
//! - **Loader** — [`load_policy_blocking`] / [`load_policy`]: opens the
//!   workspace repo, enumerates ref existence (`refs/heads/<base>`,
//!   `refs/heads/develop`, `refs/remotes/origin/<base>`), and reads + parses
//!   the marker blob from the base branch's tip tree.
//!
//! ## Verdict semantics (the matrix, condensed)
//!
//! - Marker absent (clean not-found) → **unmanaged**: existing behavior,
//!   any existing branch is a legal base (M6). "Missing" means exactly one
//!   thing — the blob is cleanly absent from the committed tree (R-B1).
//! - Marker present, `base_branch` in the protected set → hard
//!   [`PolicyVerdict::ProtectedBase`] (M2). The protected check runs BEFORE
//!   the existence check (D-3) and the `protected` list is honored under any
//!   model (M12). Defaults: `["main"]` for gitflow, `[]` for trunk.
//! - `base_branch` has no local head → [`PolicyVerdict::MissingBase`] —
//!   existence is `refs/heads/*` only; tags and remote-tracking refs do NOT
//!   count. When `origin/<base>` exists the hint is the tracking-branch
//!   command (M13), never the bootstrap command (which would fork history);
//!   `develop` with no remote gets the GitFlow bootstrap hint (M4).
//! - Marker present but unreadable — bad TOML, unknown field/model, non-UTF8,
//!   not a blob, repo/odb read error — → [`PolicyVerdict::PolicyInvalid`],
//!   never silently unmanaged (R-B2 error taxonomy: every failure other than
//!   clean not-found is loud).
//! - No marker but `refs/heads/develop` exists and the spec lands on `main`
//!   → allow with a one-line advisory (M8) — a migration aid, not a gate.
//!
//! ## Blocking calls
//!
//! Like `git_ops`, the loader is synchronous libgit2 FFI + disk I/O. Async
//! callers use [`load_policy`], which wraps the read in
//! [`tokio::task::spawn_blocking`] (the Phase 6 preamble rule).

use std::path::{Path, PathBuf};

use git2::Repository;
use serde::Deserialize;

/// The branch-policy marker file name, at the workspace repo root.
pub const POLICY_FILE_NAME: &str = ".boi-policy.toml";

/// The branch GitFlow protects by default.
const MAIN_BRANCH: &str = "main";

/// The GitFlow integration branch.
const DEVELOP_BRANCH: &str = "develop";

/// The branch model a workspace declares in its marker file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BranchModel {
    /// GitFlow: work integrates on `develop`; `main` moves only by release /
    /// hotfix merges performed by the release ceremony.
    Gitflow,
    /// Trunk-based: any branch is a legal delivery target unless explicitly
    /// listed in `protected`.
    Trunk,
}

/// The parsed `.boi-policy.toml` marker.
///
/// `deny_unknown_fields` makes a typo'd field a parse error — and the loader
/// maps every parse error to [`PolicySource::Invalid`], never to "no marker"
/// (R-B2: a typo must not silently disable enforcement).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchPolicy {
    /// The declared branch model.
    pub model: BranchModel,
    /// Branches the engine must never deliver to. `None` selects the model
    /// default: `["main"]` for gitflow, `[]` for trunk. An explicit list —
    /// including an explicit empty list — overrides the default under any
    /// model.
    protected: Option<Vec<String>>,
}

impl BranchPolicy {
    /// Build a policy programmatically. Production policies come from the
    /// loader's marker parse; this constructor serves tests and tooling.
    pub fn new(model: BranchModel, protected: Option<Vec<String>>) -> Self {
        BranchPolicy { model, protected }
    }

    /// Is `branch` in this policy's protected set (explicit list, or the
    /// model default when no list is declared)?
    pub fn is_protected(&self, branch: &str) -> bool {
        match &self.protected {
            Some(list) => list.iter().any(|b| b == branch),
            None => match self.model {
                BranchModel::Gitflow => branch == MAIN_BRANCH,
                BranchModel::Trunk => false,
            },
        }
    }

    /// The effective protected set, rendered for hint text.
    fn protected_display(&self) -> String {
        match &self.protected {
            Some(list) => list
                .iter()
                .map(|b| format!("\"{b}\""))
                .collect::<Vec<_>>()
                .join(", "),
            None => match self.model {
                BranchModel::Gitflow => format!("\"{MAIN_BRANCH}\""),
                BranchModel::Trunk => String::new(),
            },
        }
    }
}

/// Where the workspace's policy came from — the loader's three-way taxonomy
/// (R-B1/R-B2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicySource {
    /// No marker: the blob is cleanly absent from the committed tree of the
    /// base branch (the git equivalent of ENOENT), or the base branch itself
    /// has no local head to read from. Existing engine behavior applies.
    Unmanaged,
    /// A marker was read and parsed from the base branch's committed tree.
    Declared(BranchPolicy),
    /// The policy could not be determined: repo open failure, ref/odb read
    /// error, a present-but-unreadable marker (non-blob, non-UTF8, bad TOML,
    /// unknown field or model). NEVER mapped to [`PolicySource::Unmanaged`] —
    /// a read error that silently disabled enforcement would be a quiet
    /// failure.
    Invalid {
        /// What failed, with an actionable `Fix:` line.
        reason: String,
    },
}

/// Ref-existence facts the loader gathers for the decision core.
///
/// Existence means a **local head** (`refs/heads/<name>`) exists with that
/// **byte-exact** name — checked by membership in the enumerated ref names,
/// not via DWIM revparse (which a like-named tag could shadow) and not via
/// `find_reference` (whose loose-ref filesystem lookup case-aliases on
/// case-insensitive filesystems — see `ref_exists`). Remote-tracking refs
/// and tags do not count: the engine's branch creation needs a local head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceRefs {
    /// `refs/heads/<base_branch>` resolves.
    pub base_exists: bool,
    /// `refs/heads/develop` resolves (drives the M8 advisory).
    pub develop_exists: bool,
    /// `refs/remotes/origin/<base_branch>` resolves (selects the
    /// tracking-branch hint over the bootstrap hint when the local head is
    /// absent).
    pub base_remote_tracking: bool,
}

/// The decision core's output — one verdict per (policy, base_branch,
/// refs) input, per the behavior matrix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyVerdict {
    /// The spec may target `base_branch` (M1/M3/M6/M8). `advisory` carries
    /// the one non-fatal M8 migration line ("develop exists but no marker;
    /// landing on main") when it applies — print it, don't gate on it.
    Allow {
        /// The optional M8 advisory line.
        advisory: Option<String>,
    },
    /// Policy is not evaluable because there is no workspace context (a
    /// `workspace_rationale` spec — M10). A defensive dead path: no
    /// production caller reaches policy evaluation without a workspace.
    Skip {
        /// Why evaluation was skipped.
        reason: String,
    },
    /// `base_branch` is in the workspace's protected set — the engine must
    /// never deliver to it (M2/M12). Hard reject.
    ProtectedBase {
        /// The protected branch the spec named.
        branch: String,
        /// Actionable `Fix:` line for the spec author.
        fix_hint: String,
    },
    /// `base_branch` has no local head in the workspace (M4/M5/M7/M13).
    /// Hard reject.
    MissingBase {
        /// The branch that does not exist.
        branch: String,
        /// Actionable `Fix:` line — bootstrap, tracking-branch, or
        /// fix-the-spec, depending on what exists.
        hint: String,
    },
    /// The policy could not be determined (M11 / the R-B2 error taxonomy).
    /// Hard reject — never treated as unmanaged.
    PolicyInvalid {
        /// What failed, with an actionable `Fix:` line.
        reason: String,
    },
}

impl PolicyVerdict {
    /// Is this verdict an allow (with or without advisory)?
    pub fn is_allow(&self) -> bool {
        matches!(self, PolicyVerdict::Allow { .. })
    }
}

/// The pure decision core — the behavior matrix as a function. No I/O.
///
/// `refs: None` means "no workspace context" (a `workspace_rationale` spec)
/// and short-circuits to [`PolicyVerdict::Skip`] (M10 — defensive dead path).
/// Check order is binding: invalid-policy first (M11), then protected BEFORE
/// existence (D-3 — a protected base must never be reported as merely
/// missing, and no hint may suggest a protected branch as fallback), then
/// existence (M4/M5/M7/M13), then allow (+ the M8 advisory).
pub fn evaluate(
    source: &PolicySource,
    base_branch: &str,
    refs: Option<&WorkspaceRefs>,
) -> PolicyVerdict {
    let Some(refs) = refs else {
        return PolicyVerdict::Skip {
            reason: "no workspace context (workspace_rationale spec) — \
                     branch policy is not evaluable without a workspace repo"
                .to_string(),
        };
    };

    if let PolicySource::Invalid { reason } = source {
        return PolicyVerdict::PolicyInvalid {
            reason: reason.clone(),
        };
    }

    // D-3: protected before existence.
    if let PolicySource::Declared(policy) = source {
        if policy.is_protected(base_branch) {
            return PolicyVerdict::ProtectedBase {
                branch: base_branch.to_string(),
                fix_hint: protected_fix_hint(policy, base_branch),
            };
        }
    }

    if !refs.base_exists {
        return PolicyVerdict::MissingBase {
            branch: base_branch.to_string(),
            hint: missing_base_hint(base_branch, refs),
        };
    }

    // M8 advisory: an unmanaged workspace that LOOKS like GitFlow (develop
    // exists) receiving a main-targeted spec. Non-fatal migration aid only.
    let advisory = (matches!(source, PolicySource::Unmanaged)
        && base_branch == MAIN_BRANCH
        && refs.develop_exists)
        .then(|| {
            format!(
                "workspace has a develop branch but no {POLICY_FILE_NAME}; \
                 landing on main — add a policy marker if this repo uses GitFlow"
            )
        });

    PolicyVerdict::Allow { advisory }
}

/// The `Fix:` line for a protected-base rejection.
///
/// On a gitflow workspace where `develop` itself is not protected, teach the
/// correct value outright; otherwise (trunk with an explicit list, or a
/// policy that protects develop too) name the protected set.
fn protected_fix_hint(policy: &BranchPolicy, branch: &str) -> String {
    if policy.model == BranchModel::Gitflow && !policy.is_protected(DEVELOP_BRANCH) {
        format!(
            "Fix: use base_branch = \"develop\"; `{branch}` moves only via \
             the project's release ceremony"
        )
    } else {
        format!(
            "Fix: choose a base_branch outside the protected list [{}] \
             declared in {POLICY_FILE_NAME}",
            policy.protected_display()
        )
    }
}

/// The `Fix:` line for a missing-base rejection.
///
/// Order matters: a remote-tracking ref means the right fix is a local
/// tracking branch (suggesting the bootstrap command would create a
/// divergent local branch and fork history — M13); a missing `develop` with
/// no remote gets the GitFlow bootstrap command (M4); anything else is a
/// typo'd or never-created branch (M5/M7).
fn missing_base_hint(base_branch: &str, refs: &WorkspaceRefs) -> String {
    if refs.base_remote_tracking {
        format!(
            "Fix: the workspace tracks `origin/{base_branch}` but has no local \
             branch — create it with `git branch {base_branch} origin/{base_branch}`"
        )
    } else if base_branch == DEVELOP_BRANCH {
        "Fix: bootstrap GitFlow first — \
         `git branch develop main && git push origin develop`"
            .to_string()
    } else {
        format!(
            "Fix: create branch `{base_branch}` in the workspace or correct \
             [contract].base_branch"
        )
    }
}

/// A loaded policy plus the ref-existence facts — everything [`evaluate`]
/// needs for one spec's `base_branch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyContext {
    /// The policy as read from the base branch's committed tree.
    pub source: PolicySource,
    /// Ref existence at load time.
    pub refs: WorkspaceRefs,
}

impl PolicyContext {
    /// Evaluate this context for `base_branch` (the branch it was loaded
    /// for).
    pub fn verdict(&self, base_branch: &str) -> PolicyVerdict {
        evaluate(&self.source, base_branch, Some(&self.refs))
    }
}

/// Load the branch policy for `base_branch` from the workspace repo —
/// synchronous (blocking libgit2 I/O; async callers use [`load_policy`]).
///
/// The marker is read from the committed tree of `refs/heads/<base_branch>`
/// — never any working tree — so the result is independent of what any
/// checkout currently has checked out. Every failure other than a clean
/// not-found maps to [`PolicySource::Invalid`] (R-B2); this function does
/// not error.
pub fn load_policy_blocking(workspace: &Path, base_branch: &str) -> PolicyContext {
    match try_load(workspace, base_branch) {
        Ok(ctx) => ctx,
        Err(reason) => PolicyContext {
            source: PolicySource::Invalid { reason },
            refs: WorkspaceRefs {
                base_exists: false,
                develop_exists: false,
                base_remote_tracking: false,
            },
        },
    }
}

/// Async wrapper over [`load_policy_blocking`] — runs the blocking libgit2
/// read on the blocking pool. A join failure (panicked task) maps to
/// [`PolicySource::Invalid`], never to unmanaged.
pub async fn load_policy(workspace: PathBuf, base_branch: String) -> PolicyContext {
    let task = tokio::task::spawn_blocking(move || load_policy_blocking(&workspace, &base_branch));
    match task.await {
        Ok(ctx) => ctx,
        Err(e) => PolicyContext {
            source: PolicySource::Invalid {
                reason: format!(
                    "branch-policy load task failed: {e}\n  \
                     Fix: re-run the dispatch; if this repeats, check the \
                     daemon log for the panic"
                ),
            },
            refs: WorkspaceRefs {
                base_exists: false,
                develop_exists: false,
                base_remote_tracking: false,
            },
        },
    }
}

/// The fallible loader body. `Err` carries a fully-rendered reason (with its
/// `Fix:` line) and becomes [`PolicySource::Invalid`] in
/// [`load_policy_blocking`].
fn try_load(workspace: &Path, base_branch: &str) -> Result<PolicyContext, String> {
    let repo = Repository::open(workspace).map_err(|e| {
        format!(
            "cannot open workspace repository at `{}`: {e}\n  \
             Fix: check that [contract].workspace points at a git repository",
            workspace.display()
        )
    })?;

    let base_ref = format!("refs/heads/{base_branch}");
    let base_exists = ref_exists(&repo, &base_ref)?;
    let develop_exists = ref_exists(&repo, &format!("refs/heads/{DEVELOP_BRANCH}"))?;
    let base_remote_tracking = ref_exists(&repo, &format!("refs/remotes/origin/{base_branch}"))?;

    // No local head — nothing to read a policy from; the verdict will be
    // MissingBase regardless of any marker elsewhere (D-13).
    let source = if base_exists {
        read_marker(&repo, base_branch, &base_ref)?
    } else {
        PolicySource::Unmanaged
    };

    Ok(PolicyContext {
        source,
        refs: WorkspaceRefs {
            base_exists,
            develop_exists,
            base_remote_tracking,
        },
    })
}

/// Does `full_ref` (a fully-qualified refname) exist — **byte-exact**?
///
/// Deliberately NOT `find_reference`: a loose-ref lookup is a filesystem
/// path read, which is case-insensitive (and unicode-normalizing) on
/// e.g. macOS APFS — the platform the deployed daemon runs on — so
/// `refs/heads/Main` would alias to `refs/heads/main`, sail past the
/// byte-exact protected check, and let the engine move a protected ref
/// (an R-B10 violation that CI on a case-sensitive filesystem never sees).
/// Post-hoc verification is impossible too: after `find_reference`,
/// `Reference::name()` echoes the queried name, not the on-disk name.
/// Enumerating the ref names (loose readdir + packed-refs) returns the
/// true stored names, so membership here is exact on every filesystem;
/// a case-aliased base lands in `MissingBase` (the M7 typo check) before
/// any git_ops call. Iteration errors are a loud `Err` (→ `PolicyInvalid`).
fn ref_exists(repo: &Repository, full_ref: &str) -> Result<bool, String> {
    let mut refs = repo.references().map_err(|e| {
        format!(
            "cannot enumerate references in the workspace repository: {e}\n  \
             Fix: check [contract].workspace points at a healthy git repository"
        )
    })?;
    for name in refs.names() {
        // A non-UTF8 ref name is yielded as `Err` — it cannot equal
        // `full_ref` (always valid UTF-8), so it is a definitional
        // non-match, not a swallowed error.
        if name.is_ok_and(|n| n == full_ref) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Read + parse the marker blob from the committed tree at the tip of
/// `refs/heads/<base_branch>`.
///
/// Returns `Unmanaged` for exactly one case: the tree has no entry named
/// `.boi-policy.toml` (clean not-found, R-B1). Every other failure — peel /
/// tree / odb errors, a non-blob entry, non-UTF8 content, TOML parse errors
/// (including unknown fields and unknown `model` values) — is `Err` (R-B2).
fn read_marker(
    repo: &Repository,
    base_branch: &str,
    base_ref: &str,
) -> Result<PolicySource, String> {
    let schema_hint = format!(
        "Fix: correct {POLICY_FILE_NAME} on branch `{base_branch}` — schema: \
         model = \"gitflow\" | \"trunk\", optional protected = [\"branch\", ...]"
    );

    let reference = repo
        .find_reference(base_ref)
        .map_err(|e| format!("cannot re-read `{base_ref}`: {e}\n  {schema_hint}"))?;
    let commit = reference
        .peel_to_commit()
        .map_err(|e| format!("cannot resolve `{base_ref}` to a commit: {e}\n  {schema_hint}"))?;
    let tree = commit
        .tree()
        .map_err(|e| format!("cannot read the tree of `{base_ref}`: {e}\n  {schema_hint}"))?;

    // Clean not-found: the ONLY path to Unmanaged with the base present.
    let Some(entry) = tree.get_name(POLICY_FILE_NAME) else {
        return Ok(PolicySource::Unmanaged);
    };

    let object = entry.to_object(repo).map_err(|e| {
        format!("cannot read `{POLICY_FILE_NAME}` blob from `{base_ref}`: {e}\n  {schema_hint}")
    })?;
    let blob = object.as_blob().ok_or_else(|| {
        format!("`{POLICY_FILE_NAME}` on `{base_ref}` is not a regular file\n  {schema_hint}")
    })?;
    let text = std::str::from_utf8(blob.content()).map_err(|_| {
        format!("`{POLICY_FILE_NAME}` on `{base_ref}` is not valid UTF-8\n  {schema_hint}")
    })?;
    let policy: BranchPolicy = toml::from_str(text).map_err(|e| {
        format!("`{POLICY_FILE_NAME}` on `{base_ref}` does not parse: {e}\n  {schema_hint}")
    })?;

    Ok(PolicySource::Declared(policy))
}

#[cfg(test)]
pub(crate) mod testkit {
    //! Shared temp-git-repo plumbing for the branch-policy ENFORCEMENT tests
    //! (the dispatch gate R-B6, the preflight backstop R-B7, the worktree
    //! re-checks R-B8) — in-crate test modules reach it as
    //! `crate::runtime::branch_policy::testkit`.
    //!
    //! Everything here writes refs/trees through the odb — no index state and
    //! no checkout machinery (the AC-14 do-not-worsen gate greps `src/`,
    //! test code INCLUDED, for forced-checkout call sites outside
    //! `git_ops.rs`) — so committing a marker to a branch never touches any
    //! working tree. That is also exactly the D-13 read model the loader
    //! implements: policy comes from committed trees, never from a checkout.

    use std::path::Path;

    use git2::{Repository, Signature};

    /// A gitflow marker body with default protection (`protected = ["main"]`).
    pub(crate) const GITFLOW_MARKER: &str = "model = \"gitflow\"\n";

    fn sig() -> Signature<'static> {
        Signature::now("test", "test@localhost").expect("signature")
    }

    /// Init a repo at `path` with one commit on `main` (a `README.md`) and
    /// HEAD attached to `refs/heads/main`.
    pub(crate) fn init_repo_on_main(path: &Path) {
        let repo = Repository::init(path).expect("init repo");
        let blob = repo.blob(b"hello\n").expect("blob");
        let mut builder = repo.treebuilder(None).expect("treebuilder");
        builder
            .insert("README.md", blob, 0o100644)
            .expect("insert README");
        let tree = repo
            .find_tree(builder.write().expect("write tree"))
            .expect("tree");
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

    /// Commit `files` (name → content) onto `refs/heads/<branch>` — pure
    /// odb/ref plumbing. Existing tree entries are preserved; the named
    /// entries are added/replaced at the tree root.
    pub(crate) fn commit_on_branch(path: &Path, branch: &str, files: &[(&str, &str)]) {
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

    /// Create `refs/heads/<name>` at `main`'s tip.
    pub(crate) fn branch_from_main(path: &Path, name: &str) {
        let repo = Repository::open(path).expect("open repo");
        let main_tip = repo
            .find_reference("refs/heads/main")
            .expect("main")
            .peel_to_commit()
            .expect("main tip");
        repo.branch(name, &main_tip, false).expect("create branch");
    }

    /// The hex OID of `refs/heads/<name>`.
    pub(crate) fn branch_oid(path: &Path, name: &str) -> String {
        let repo = Repository::open(path).expect("open repo");
        repo.find_reference(&format!("refs/heads/{name}"))
            .expect("branch ref")
            .peel_to_commit()
            .expect("tip")
            .id()
            .to_string()
    }

    /// Detach HEAD at `main`'s tip — parks the checkout off-branch WITHOUT
    /// touching the working tree (the M14 checkout-independence setup).
    pub(crate) fn detach_head(path: &Path) {
        let repo = Repository::open(path).expect("open repo");
        let oid = repo
            .find_reference("refs/heads/main")
            .expect("main")
            .peel_to_commit()
            .expect("tip")
            .id();
        repo.set_head_detached(oid).expect("detach HEAD");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Signature;
    use std::sync::atomic::{AtomicU64, Ordering};

    // -----------------------------------------------------------------
    // Temp-repo helpers (the git_ops.rs test pattern — std-only TempDir).
    // -----------------------------------------------------------------

    /// A throwaway directory under the system temp dir, removed on drop.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "boi-branch-policy-{}-{tag}-{n}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.path));
        }
    }

    fn sig() -> Signature<'static> {
        Signature::now("test", "test@localhost").unwrap()
    }

    /// Stage `files` (name → content bytes) and commit on the current branch.
    fn commit_files(repo: &Repository, files: &[(&str, &[u8])], message: &str) -> git2::Oid {
        let workdir = repo.workdir().expect("non-bare repo");
        let mut index = repo.index().expect("index");
        for (name, content) in files {
            let path = workdir.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create parent dirs");
            }
            std::fs::write(&path, content).expect("write file");
            index.add_path(Path::new(name)).expect("add path");
        }
        index.write().expect("write index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        let parents: Vec<git2::Commit<'_>> = match repo.head() {
            Ok(head) => vec![head.peel_to_commit().expect("head commit")],
            Err(_) => vec![], // first commit — unborn HEAD
        };
        let parent_refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig(), &sig(), message, &tree, &parent_refs)
            .expect("commit")
    }

    /// Initialise a repo with one commit on `main` containing README.md.
    fn init_repo(path: &Path) -> Repository {
        let repo = Repository::init(path).expect("init repo");
        commit_files(&repo, &[("README.md", b"hello\n")], "initial");
        if repo.find_branch("main", git2::BranchType::Local).is_err() {
            let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
            repo.branch("main", &head_commit, true).unwrap();
            repo.set_head("refs/heads/main").unwrap();
        }
        repo
    }

    /// A gitflow marker body with default protection.
    const GITFLOW_MARKER: &[u8] = b"model = \"gitflow\"\n";

    fn gitflow() -> PolicySource {
        PolicySource::Declared(BranchPolicy::new(BranchModel::Gitflow, None))
    }

    fn trunk(protected: Option<Vec<String>>) -> PolicySource {
        PolicySource::Declared(BranchPolicy::new(BranchModel::Trunk, protected))
    }

    fn refs(base: bool, develop: bool, tracking: bool) -> WorkspaceRefs {
        WorkspaceRefs {
            base_exists: base,
            develop_exists: develop,
            base_remote_tracking: tracking,
        }
    }

    // -----------------------------------------------------------------
    // The behavior matrix, one named test per row (AC-3: m01..m15).
    // Pure-core rows are L1; loader-backed rows (M14/M15) are L2.
    // -----------------------------------------------------------------

    /// M1: gitflow workspace, `base_branch = "develop"`, develop exists →
    /// allow, no advisory.
    #[test]
    fn test_l1_m01_gitflow_develop_allows() {
        let verdict = evaluate(&gitflow(), "develop", Some(&refs(true, true, false)));
        assert_eq!(verdict, PolicyVerdict::Allow { advisory: None });
    }

    /// M2: gitflow workspace, `base_branch = "main"` → ProtectedBase, and
    /// the protected check runs BEFORE the existence check (D-3): the
    /// verdict is identical whether or not the branch exists, and the hint
    /// teaches develop + the release ceremony.
    #[test]
    fn test_l1_m02_gitflow_main_protected_rejects_before_existence() {
        for base_exists in [true, false] {
            let verdict = evaluate(&gitflow(), "main", Some(&refs(base_exists, true, false)));
            match &verdict {
                PolicyVerdict::ProtectedBase { branch, fix_hint } => {
                    assert_eq!(branch, "main");
                    assert!(fix_hint.contains("base_branch = \"develop\""), "{fix_hint}");
                    assert!(fix_hint.contains("release ceremony"), "{fix_hint}");
                }
                other => panic!("expected ProtectedBase, got {other:?}"),
            }
        }
    }

    /// M3: gitflow legitimately stacks work on feature/hotfix branches —
    /// only the protected set is fenced.
    #[test]
    fn test_l1_m03_gitflow_feature_branch_allows() {
        for base in ["feature/x", "hotfix/3.2.6"] {
            let verdict = evaluate(&gitflow(), base, Some(&refs(true, true, false)));
            assert_eq!(verdict, PolicyVerdict::Allow { advisory: None }, "{base}");
        }
    }

    /// M4: `base_branch = "develop"` with no local head AND no
    /// `origin/develop` → MissingBase with the GitFlow bootstrap hint.
    /// (When the base has no local head the loader cannot read a marker
    /// from it, so the source is Unmanaged; a Declared policy gives the
    /// same verdict — develop is not protected.)
    #[test]
    fn test_l1_m04_gitflow_develop_missing_gets_bootstrap_hint() {
        for source in [PolicySource::Unmanaged, gitflow()] {
            let verdict = evaluate(&source, "develop", Some(&refs(false, false, false)));
            match &verdict {
                PolicyVerdict::MissingBase { branch, hint } => {
                    assert_eq!(branch, "develop");
                    assert!(
                        hint.contains("git branch develop main && git push origin develop"),
                        "{hint}"
                    );
                }
                other => panic!("expected MissingBase, got {other:?}"),
            }
        }
    }

    /// M5: gitflow workspace, nonexistent base branch → MissingBase.
    #[test]
    fn test_l1_m05_gitflow_nonexistent_branch_rejects() {
        let verdict = evaluate(
            &PolicySource::Unmanaged,
            "feature/nope",
            Some(&refs(false, true, false)),
        );
        match &verdict {
            PolicyVerdict::MissingBase { branch, hint } => {
                assert_eq!(branch, "feature/nope");
                assert!(hint.contains("base_branch"), "{hint}");
            }
            other => panic!("expected MissingBase, got {other:?}"),
        }
    }

    /// M6: trunk marker or no marker, any existing branch (including main)
    /// → allow — today's behavior, unchanged (the unmanaged-workspace path).
    #[test]
    fn test_l1_m06_unmanaged_main_allows() {
        // No marker, no develop branch — plain single-branch repo.
        let verdict = evaluate(
            &PolicySource::Unmanaged,
            "main",
            Some(&refs(true, false, false)),
        );
        assert_eq!(verdict, PolicyVerdict::Allow { advisory: None });
        // Explicit trunk marker with the default (empty) protected set.
        let verdict = evaluate(&trunk(None), "main", Some(&refs(true, false, false)));
        assert_eq!(verdict, PolicyVerdict::Allow { advisory: None });
    }

    /// M7: no marker, nonexistent branch → MissingBase — the NEW universal
    /// existence check (D-4) that converts the late workspace_prepare
    /// failure into an instant typed error.
    #[test]
    fn test_l1_m07_unmanaged_nonexistent_branch_rejects() {
        let verdict = evaluate(
            &PolicySource::Unmanaged,
            "tpyo",
            Some(&refs(false, false, false)),
        );
        match &verdict {
            PolicyVerdict::MissingBase { branch, .. } => assert_eq!(branch, "tpyo"),
            other => panic!("expected MissingBase, got {other:?}"),
        }
    }

    /// M8: no marker but `refs/heads/develop` exists and the spec targets
    /// main → allow WITH the one-line advisory (migration aid, non-fatal).
    #[test]
    fn test_l1_m08_unmanaged_with_develop_advises_on_main() {
        let verdict = evaluate(
            &PolicySource::Unmanaged,
            "main",
            Some(&refs(true, true, false)),
        );
        match &verdict {
            PolicyVerdict::Allow {
                advisory: Some(line),
            } => {
                assert!(line.contains(POLICY_FILE_NAME), "{line}");
                assert!(line.contains("develop"), "{line}");
            }
            other => panic!("expected Allow with advisory, got {other:?}"),
        }
        // The advisory is main-specific: another existing branch stays quiet.
        let verdict = evaluate(
            &PolicySource::Unmanaged,
            "topic",
            Some(&refs(true, true, false)),
        );
        assert_eq!(verdict, PolicyVerdict::Allow { advisory: None });
    }

    /// M9: `base_branch` stays required at parse time (D-2) — a spec without
    /// it never reaches policy evaluation. Pins the unchanged config-layer
    /// behavior this module builds on.
    #[test]
    fn test_l1_m09_base_branch_required_at_parse() {
        let spec = r#"
            title = "no base branch"
            [contract]
            scope = "s"
            workspace = "/tmp/x"
            [[tasks]]
            ref = "t"
            behavior = "b"
            verifications = [ { intent = "i" } ]
        "#;
        let err = crate::config::parse_spec(spec).expect_err("must reject");
        assert!(err.to_string().contains("base_branch"), "{err}");
    }

    /// M10: no workspace context (a workspace_rationale spec) → Skip — the
    /// pure-core defensive dead path. Production callers always have a
    /// workspace (the parse layer rejects rationale-only specs today).
    #[test]
    fn test_l1_m10_no_workspace_context_skips() {
        let verdict = evaluate(&gitflow(), "develop", None);
        match &verdict {
            PolicyVerdict::Skip { reason } => {
                assert!(reason.contains("workspace"), "{reason}");
            }
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    /// M11: an invalid marker is a hard PolicyInvalid — the reason (and its
    /// Fix line) survives into the verdict, and it is never an allow.
    #[test]
    fn test_l1_m11_invalid_marker_rejects() {
        let source = PolicySource::Invalid {
            reason: "bad marker\n  Fix: correct it".to_string(),
        };
        let verdict = evaluate(&source, "develop", Some(&refs(true, true, false)));
        match &verdict {
            PolicyVerdict::PolicyInvalid { reason } => {
                assert!(reason.contains("bad marker"), "{reason}");
                assert!(reason.contains("Fix:"), "{reason}");
            }
            other => panic!("expected PolicyInvalid, got {other:?}"),
        }
    }

    /// M12: the `protected` list is honored under ANY model — a trunk marker
    /// with an explicit `protected = ["main"]` fences main exactly like
    /// gitflow does.
    #[test]
    fn test_l1_m12_trunk_explicit_protected_rejects() {
        let source = trunk(Some(vec!["main".to_string()]));
        let verdict = evaluate(&source, "main", Some(&refs(true, false, false)));
        match &verdict {
            PolicyVerdict::ProtectedBase { branch, fix_hint } => {
                assert_eq!(branch, "main");
                assert!(fix_hint.contains("Fix:"), "{fix_hint}");
            }
            other => panic!("expected ProtectedBase, got {other:?}"),
        }
    }

    /// M13: no local develop but `origin/develop` exists → MissingBase with
    /// the tracking-branch hint, NOT the bootstrap command (which would
    /// create a divergent local develop and fork history).
    #[test]
    fn test_l1_m13_remote_tracking_develop_gets_tracking_hint() {
        let verdict = evaluate(
            &PolicySource::Unmanaged,
            "develop",
            Some(&refs(false, false, true)),
        );
        match &verdict {
            PolicyVerdict::MissingBase { branch, hint } => {
                assert_eq!(branch, "develop");
                assert!(hint.contains("git branch develop origin/develop"), "{hint}");
                assert!(!hint.contains("git push"), "bootstrap hint leaked: {hint}");
            }
            other => panic!("expected MissingBase, got {other:?}"),
        }
    }

    /// M14 (loader-level): the marker is read from the COMMITTED TREE of the
    /// base branch, never the working tree — with the checkout deliberately
    /// parked on a detached commit whose tree LACKS the marker, develop is
    /// still allowed and main is still protected.
    #[test]
    fn test_l2_m14_committed_marker_enforced_with_checkout_parked_off_branch() {
        let dir = TempDir::new("m14");
        let repo = init_repo(&dir.path);
        let pre_marker = repo.head().unwrap().target().unwrap();
        commit_files(&repo, &[(POLICY_FILE_NAME, GITFLOW_MARKER)], "add marker");
        let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("develop", &head_commit, false).unwrap();

        // Park the checkout DETACHED at the pre-marker commit and strip the
        // marker from the working tree by hand (no checkout machinery — the
        // engine's do-not-worsen rule bans new force-checkout sites): the
        // working tree now has no .boi-policy.toml at all and HEAD points at
        // no branch.
        repo.set_head_detached(pre_marker).unwrap();
        std::fs::remove_file(dir.path.join(POLICY_FILE_NAME)).unwrap();
        assert!(
            !dir.path.join(POLICY_FILE_NAME).exists(),
            "working tree must lack the marker for this test to mean anything"
        );

        // develop: enforced identically to M1 — allow.
        let ctx = load_policy_blocking(&dir.path, "develop");
        assert_eq!(
            ctx.source,
            PolicySource::Declared(BranchPolicy::new(BranchModel::Gitflow, None))
        );
        assert_eq!(
            ctx.verdict("develop"),
            PolicyVerdict::Allow { advisory: None }
        );

        // main: enforced identically to M2 — protected.
        let ctx = load_policy_blocking(&dir.path, "main");
        assert!(
            matches!(ctx.verdict("main"), PolicyVerdict::ProtectedBase { .. }),
            "{:?}",
            ctx.verdict("main")
        );
    }

    /// M15 (loader-level): the honest residual window — marker committed on
    /// develop only, not yet on main. A main-targeted spec evaluates
    /// unmanaged (allow) but NOT silently: the M8 advisory fires because
    /// develop exists. A develop-targeted spec is fully enforced.
    #[test]
    fn test_l2_m15_marker_on_develop_only_is_unmanaged_for_main_with_advisory() {
        let dir = TempDir::new("m15");
        let repo = init_repo(&dir.path);
        // Branch develop off main, check it out, commit the marker THERE only.
        let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("develop", &head_commit, false).unwrap();
        repo.set_head("refs/heads/develop").unwrap();
        commit_files(
            &repo,
            &[(POLICY_FILE_NAME, GITFLOW_MARKER)],
            "marker on develop",
        );

        // main's tree has no marker → unmanaged + the M8 advisory.
        let ctx = load_policy_blocking(&dir.path, "main");
        assert_eq!(ctx.source, PolicySource::Unmanaged);
        match ctx.verdict("main") {
            PolicyVerdict::Allow {
                advisory: Some(line),
            } => {
                assert!(line.contains(POLICY_FILE_NAME), "{line}");
            }
            other => panic!("expected Allow with advisory, got {other:?}"),
        }

        // develop's tree HAS the marker → fully enforced (allow on develop).
        let ctx = load_policy_blocking(&dir.path, "develop");
        assert_eq!(
            ctx.source,
            PolicySource::Declared(BranchPolicy::new(BranchModel::Gitflow, None))
        );
        assert_eq!(
            ctx.verdict("develop"),
            PolicyVerdict::Allow { advisory: None }
        );
    }

    // -----------------------------------------------------------------
    // AC-17 — the loader error taxonomy: clean not-found is the ONLY path
    // to unmanaged; every other failure is PolicyInvalid, never Allow.
    // -----------------------------------------------------------------

    /// AC-17a: a clean not-found (no marker blob on the base tree) is
    /// unmanaged — existing behavior, no advisory on a develop-less repo.
    #[test]
    fn test_l2_ac17_clean_absent_marker_is_unmanaged() {
        let dir = TempDir::new("ac17-absent");
        init_repo(&dir.path);
        let ctx = load_policy_blocking(&dir.path, "main");
        assert_eq!(ctx.source, PolicySource::Unmanaged);
        assert_eq!(ctx.verdict("main"), PolicyVerdict::Allow { advisory: None });
    }

    /// AC-17b: a present-but-corrupt marker (bad TOML) → PolicyInvalid,
    /// never Allow.
    #[test]
    fn test_l2_ac17_corrupt_marker_is_policy_invalid() {
        let dir = TempDir::new("ac17-corrupt");
        let repo = init_repo(&dir.path);
        commit_files(
            &repo,
            &[(POLICY_FILE_NAME, b"model = [unclosed")],
            "corrupt marker",
        );
        let ctx = load_policy_blocking(&dir.path, "main");
        let verdict = ctx.verdict("main");
        assert!(
            matches!(verdict, PolicyVerdict::PolicyInvalid { .. }),
            "corrupt marker must be PolicyInvalid, got {verdict:?}"
        );
        assert!(!verdict.is_allow(), "corrupt marker must never allow");
    }

    /// AC-17c: non-UTF8 marker content → PolicyInvalid, never unmanaged.
    #[test]
    fn test_l2_ac17_non_utf8_marker_is_policy_invalid() {
        let dir = TempDir::new("ac17-utf8");
        let repo = init_repo(&dir.path);
        commit_files(
            &repo,
            &[(POLICY_FILE_NAME, &[0xff, 0xfe, 0x00, 0x9f])],
            "binary marker",
        );
        let ctx = load_policy_blocking(&dir.path, "main");
        match &ctx.source {
            PolicySource::Invalid { reason } => {
                assert!(reason.contains("UTF-8"), "{reason}");
                assert!(reason.contains("Fix:"), "{reason}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    /// AC-17d: an unknown field (deny_unknown_fields) → PolicyInvalid.
    #[test]
    fn test_l2_ac17_unknown_field_is_policy_invalid() {
        let dir = TempDir::new("ac17-field");
        let repo = init_repo(&dir.path);
        commit_files(
            &repo,
            &[(
                POLICY_FILE_NAME,
                b"model = \"gitflow\"\nprotectd = [\"main\"]\n",
            )],
            "typo'd field",
        );
        let ctx = load_policy_blocking(&dir.path, "main");
        assert!(
            matches!(ctx.source, PolicySource::Invalid { .. }),
            "unknown field must be Invalid, got {:?}",
            ctx.source
        );
    }

    /// AC-17e: an unknown `model` value → PolicyInvalid.
    #[test]
    fn test_l2_ac17_unknown_model_is_policy_invalid() {
        let dir = TempDir::new("ac17-model");
        let repo = init_repo(&dir.path);
        commit_files(
            &repo,
            &[(POLICY_FILE_NAME, b"model = \"flow\"\n")],
            "bad model",
        );
        let ctx = load_policy_blocking(&dir.path, "main");
        assert!(
            matches!(ctx.source, PolicySource::Invalid { .. }),
            "unknown model must be Invalid, got {:?}",
            ctx.source
        );
    }

    /// AC-17f: a marker that is not a regular file (a directory entry named
    /// `.boi-policy.toml`) → PolicyInvalid.
    #[test]
    fn test_l2_ac17_marker_directory_is_policy_invalid() {
        let dir = TempDir::new("ac17-dir");
        let repo = init_repo(&dir.path);
        let nested = format!("{POLICY_FILE_NAME}/x");
        commit_files(&repo, &[(nested.as_str(), b"nope")], "marker as directory");
        let ctx = load_policy_blocking(&dir.path, "main");
        assert!(
            matches!(ctx.source, PolicySource::Invalid { .. }),
            "non-blob marker must be Invalid, got {:?}",
            ctx.source
        );
    }

    /// AC-17g: a repo-open failure (workspace is not a git repo) →
    /// PolicyInvalid, never unmanaged.
    #[test]
    fn test_l2_ac17_repo_open_failure_is_policy_invalid() {
        let dir = TempDir::new("ac17-norepo");
        let ctx = load_policy_blocking(&dir.path, "main");
        match &ctx.source {
            PolicySource::Invalid { reason } => {
                assert!(reason.contains("Fix:"), "{reason}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
        assert!(!ctx.verdict("main").is_allow());
    }

    // -----------------------------------------------------------------
    // Existence semantics + loader plumbing.
    // -----------------------------------------------------------------

    /// Existence is `refs/heads/*` ONLY: a tag with the requested name does
    /// not count (DWIM revparse would have been shadowed by it).
    #[test]
    fn test_l2_tag_does_not_count_as_branch_existence() {
        let dir = TempDir::new("tag-shadow");
        let repo = init_repo(&dir.path);
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.tag_lightweight("ghost", head.as_object(), false)
            .unwrap();
        let ctx = load_policy_blocking(&dir.path, "ghost");
        assert!(!ctx.refs.base_exists, "a tag must not satisfy existence");
        assert!(
            matches!(ctx.verdict("ghost"), PolicyVerdict::MissingBase { .. }),
            "{:?}",
            ctx.verdict("ghost")
        );
    }

    /// Existence is **byte-exact**: a case-aliased base name must not count.
    /// `find_reference`'s loose-ref lookup is a filesystem path read —
    /// case-insensitive on e.g. macOS APFS, the deployed daemon's platform —
    /// so `"Main"` would resolve to `refs/heads/main`, skate past the exact
    /// protected-set check as `Allow`, and let the engine fast-forward the
    /// real `refs/heads/main` (R-B10 violation). With exact-match enumeration
    /// the alias is `MissingBase` (M7's typo check) at every layer, before
    /// any git_ops call. NOTE: the aliasing this guards against only occurs
    /// on case-insensitive filesystems, but the exact-match implementation
    /// makes this test pass on every platform.
    #[test]
    fn test_l2_case_aliased_base_does_not_count_as_branch_existence() {
        let dir = TempDir::new("case-alias");
        let repo = init_repo(&dir.path);
        commit_files(&repo, &[(POLICY_FILE_NAME, GITFLOW_MARKER)], "add marker");
        // A remote-tracking ref for the true-cased name: the alias must not
        // pick up the M13 tracking hint either.
        let head = repo.head().unwrap().target().unwrap();
        repo.reference("refs/remotes/origin/main", head, false, "test")
            .unwrap();

        let ctx = load_policy_blocking(&dir.path, "Main");
        assert!(
            !ctx.refs.base_exists,
            "a case-aliased name must not satisfy existence"
        );
        assert!(
            !ctx.refs.base_remote_tracking,
            "a case-aliased name must not satisfy remote-tracking existence"
        );
        // No exactly-named local head → nothing to read a marker from.
        assert_eq!(ctx.source, PolicySource::Unmanaged);
        let verdict = ctx.verdict("Main");
        assert!(
            !verdict.is_allow(),
            "a case-aliased protected branch must never be allowed: {verdict:?}"
        );
        match verdict {
            PolicyVerdict::MissingBase { branch, hint } => {
                assert_eq!(branch, "Main");
                // The typo hint (M7), not the tracking or bootstrap hint.
                assert!(hint.contains("correct"), "{hint}");
            }
            other => panic!("expected MissingBase, got {other:?}"),
        }
    }

    /// The loader detects a remote-tracking ref for the base branch and the
    /// verdict carries the M13 tracking hint (the loader leg of M13).
    #[test]
    fn test_l2_remote_tracking_ref_detected_by_loader() {
        let dir = TempDir::new("tracking");
        let repo = init_repo(&dir.path);
        let head = repo.head().unwrap().target().unwrap();
        repo.reference("refs/remotes/origin/develop", head, false, "test")
            .unwrap();
        let ctx = load_policy_blocking(&dir.path, "develop");
        assert!(!ctx.refs.base_exists);
        assert!(ctx.refs.base_remote_tracking);
        match ctx.verdict("develop") {
            PolicyVerdict::MissingBase { hint, .. } => {
                assert!(hint.contains("git branch develop origin/develop"), "{hint}");
            }
            other => panic!("expected MissingBase, got {other:?}"),
        }
    }

    /// The async wrapper returns the same context as the blocking loader.
    #[tokio::test]
    async fn test_l2_async_loader_matches_blocking() {
        let dir = TempDir::new("async");
        let repo = init_repo(&dir.path);
        commit_files(&repo, &[(POLICY_FILE_NAME, GITFLOW_MARKER)], "add marker");
        let blocking = load_policy_blocking(&dir.path, "main");
        let asynced = load_policy(dir.path.clone(), "main".to_string()).await;
        assert_eq!(blocking, asynced);
        assert!(matches!(
            asynced.verdict("main"),
            PolicyVerdict::ProtectedBase { .. }
        ));
    }

    /// An explicit empty `protected = []` under gitflow overrides the
    /// default — the model controls defaults, not whether an explicit list
    /// is respected (the M12 rule's mirror image).
    #[test]
    fn test_l1_explicit_empty_protected_overrides_gitflow_default() {
        let source = PolicySource::Declared(BranchPolicy::new(BranchModel::Gitflow, Some(vec![])));
        let verdict = evaluate(&source, "main", Some(&refs(true, true, false)));
        assert_eq!(verdict, PolicyVerdict::Allow { advisory: None });
    }

    /// A gitflow policy that protects develop too falls back to the
    /// protected-list hint rather than teaching a protected branch.
    #[test]
    fn test_l1_protected_develop_hint_never_suggests_develop() {
        let source = PolicySource::Declared(BranchPolicy::new(
            BranchModel::Gitflow,
            Some(vec!["main".to_string(), "develop".to_string()]),
        ));
        let verdict = evaluate(&source, "develop", Some(&refs(true, true, false)));
        match &verdict {
            PolicyVerdict::ProtectedBase { fix_hint, .. } => {
                assert!(
                    !fix_hint.contains("use base_branch = \"develop\""),
                    "must not suggest a protected branch: {fix_hint}"
                );
            }
            other => panic!("expected ProtectedBase, got {other:?}"),
        }
    }
}
