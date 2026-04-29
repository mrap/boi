use crate::phases::Verdict;
use crate::worktree;

pub struct BuiltinContext<'a> {
    pub spec_id: &'a str,
    pub task_title: &'a str,
    /// Source repo path for merge/cleanup. Empty string if not applicable.
    pub repo_path: &'a str,
}

#[derive(Debug, PartialEq)]
pub enum BuiltinResult {
    Success(String),
    NoOp(String),
    Error(String),
}

impl BuiltinResult {
    pub fn to_verdict(&self) -> Verdict {
        match self {
            BuiltinResult::Success(_) | BuiltinResult::NoOp(_) => Verdict::Proceed,
            BuiltinResult::Error(msg) => Verdict::Done { success: false, reason: msg.clone() },
        }
    }
}

/// Dispatch a deterministic builtin by handler name.
pub fn run_builtin(handler: &str, ctx: &BuiltinContext<'_>) -> BuiltinResult {
    match handler {
        "builtin:commit" => run_commit(ctx),
        "builtin:merge" => run_merge(ctx),
        "builtin:cleanup" => run_cleanup(ctx),
        other => BuiltinResult::Error(format!("unknown builtin: {}", other)),
    }
}

fn run_commit(ctx: &BuiltinContext<'_>) -> BuiltinResult {
    let msg = format!("boi({}): {}", ctx.spec_id, ctx.task_title);
    match worktree::commit_changes(ctx.spec_id, &msg) {
        Ok(true) => BuiltinResult::Success(format!("committed: {}", msg)),
        Ok(false) => BuiltinResult::NoOp("no changes to commit".into()),
        Err(e) => BuiltinResult::Error(format!("commit failed: {}", e)),
    }
}

fn run_merge(ctx: &BuiltinContext<'_>) -> BuiltinResult {
    if ctx.repo_path.is_empty() {
        return BuiltinResult::Error("builtin:merge requires repo_path".into());
    }
    match worktree::merge_back(ctx.spec_id, ctx.repo_path) {
        Ok(msg) => BuiltinResult::Success(format!("merged: {}", msg.trim())),
        Err(e) => BuiltinResult::Error(format!("merge failed: {}", e)),
    }
}

fn run_cleanup(ctx: &BuiltinContext<'_>) -> BuiltinResult {
    if ctx.repo_path.is_empty() {
        return BuiltinResult::Error("builtin:cleanup requires repo_path".into());
    }
    if let Err(e) = worktree::cleanup(ctx.spec_id) {
        return BuiltinResult::Error(format!("worktree cleanup failed: {}", e));
    }
    if let Err(e) = worktree::delete_branch(ctx.spec_id, ctx.repo_path) {
        return BuiltinResult::Error(format!("branch delete failed: {}", e));
    }
    BuiltinResult::Success("worktree and branch cleaned up".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils;

    fn make_ctx<'a>(spec_id: &'a str, task_title: &'a str, repo_path: &'a str) -> BuiltinContext<'a> {
        BuiltinContext { spec_id, task_title, repo_path }
    }

    // --- runtime parsing ---

    #[test]
    fn test_deterministic_runtime_in_phase_config() {
        use crate::phases::{PhaseConfig, PhaseLevel};
        let phase = PhaseConfig {
            name: "commit".into(),
            level: PhaseLevel::Task,
            description: "".into(),
            prompt_template: String::new(),
            timeout_minutes: Some(1),
            retry_count: None,
            can_add_tasks: false,
            can_fail_spec: false,
            requires_claude: false,
            runtime: Some("deterministic".into()),
            completion_handler: Some("builtin:commit".into()),
            approve_signal: None,
            reject_signal: None,
            on_approve: None,
            on_reject: None,
            on_crash: None,
            min_lines_changed: None,
            model: None,
            code_model: None,
            effort: None,
            hooks_pre: vec![],
            hooks_post: vec![],
        };
        assert_eq!(phase.runtime.as_deref(), Some("deterministic"));
        assert!(!phase.requires_claude);
        assert_eq!(phase.completion_handler.as_deref(), Some("builtin:commit"));
    }

    // --- registry lookup ---

    #[test]
    fn test_deterministic_unknown_builtin_returns_error() {
        let ctx = make_ctx("s001", "My Task", "/tmp");
        let result = run_builtin("builtin:unknown", &ctx);
        assert!(matches!(result, BuiltinResult::Error(_)));
        if let BuiltinResult::Error(msg) = result {
            assert!(msg.contains("unknown builtin"), "msg was: {}", msg);
        }
    }

    #[test]
    fn test_deterministic_unknown_builtin_verdict_is_done_failure() {
        let ctx = make_ctx("s001", "My Task", "/tmp");
        let verdict = run_builtin("builtin:unknown", &ctx).to_verdict();
        assert!(matches!(verdict, Verdict::Done { success: false, .. }));
    }

    // --- builtin:commit ---

    #[test]
    fn test_deterministic_commit_no_changes_is_noop() {
        let _guard = test_utils::HOME_LOCK.lock().unwrap();
        let repo = test_utils::test_git_repo("builtin-commit-noop");
        let home = test_utils::test_dir("builtin-commit-noop-home");
        std::env::set_var("HOME", home.to_str().unwrap());

        let spec_id = "det-commit-noop-001";
        worktree::create(spec_id, repo.to_str().unwrap()).unwrap();

        let result = run_builtin("builtin:commit", &make_ctx(spec_id, "Test Task", repo.to_str().unwrap()));
        assert!(matches!(result, BuiltinResult::NoOp(_)), "expected NoOp, got {:?}", result);

        worktree::cleanup(spec_id).unwrap();
    }

    #[test]
    fn test_deterministic_commit_with_changes_succeeds() {
        let _guard = test_utils::HOME_LOCK.lock().unwrap();
        let repo = test_utils::test_git_repo("builtin-commit-changes");
        let home = test_utils::test_dir("builtin-commit-changes-home");
        std::env::set_var("HOME", home.to_str().unwrap());

        let spec_id = "det-commit-changes-001";
        let dest = worktree::create(spec_id, repo.to_str().unwrap()).unwrap();
        std::fs::write(dest.join("new.txt"), "hello").unwrap();

        let result = run_builtin("builtin:commit", &make_ctx(spec_id, "Add File", repo.to_str().unwrap()));
        assert!(matches!(result, BuiltinResult::Success(_)), "expected Success, got {:?}", result);

        // Commit message should contain spec_id
        let log = std::process::Command::new("git")
            .args(["log", "--format=%s", "-1"])
            .current_dir(&dest)
            .output()
            .unwrap();
        let subject = String::from_utf8_lossy(&log.stdout);
        assert!(subject.contains(spec_id), "commit subject: {}", subject.trim());
        assert!(subject.contains("Add File"), "commit subject: {}", subject.trim());

        worktree::cleanup(spec_id).unwrap();
    }

    // --- builtin:merge ---

    #[test]
    fn test_deterministic_merge_brings_file_into_repo() {
        let _guard = test_utils::HOME_LOCK.lock().unwrap();
        let repo = test_utils::test_git_repo("builtin-merge-repo");
        let home = test_utils::test_dir("builtin-merge-home");
        std::env::set_var("HOME", home.to_str().unwrap());

        let spec_id = "det-merge-001";
        let dest = worktree::create(spec_id, repo.to_str().unwrap()).unwrap();
        std::fs::write(dest.join("merged.txt"), "from worktree").unwrap();
        worktree::commit_changes(spec_id, "add merged.txt").unwrap();

        let result = run_builtin("builtin:merge", &make_ctx(spec_id, "Merge", repo.to_str().unwrap()));
        assert!(matches!(result, BuiltinResult::Success(_)), "merge failed: {:?}", result);
        assert!(repo.join("merged.txt").exists(), "merged.txt should appear in repo after merge");

        worktree::cleanup(spec_id).unwrap();
    }

    #[test]
    fn test_deterministic_merge_without_repo_path_returns_error() {
        let ctx = make_ctx("s001", "Merge", "");
        let result = run_builtin("builtin:merge", &ctx);
        assert!(matches!(result, BuiltinResult::Error(_)));
    }

    // --- builtin:cleanup ---

    #[test]
    fn test_deterministic_cleanup_removes_worktree() {
        let _guard = test_utils::HOME_LOCK.lock().unwrap();
        let repo = test_utils::test_git_repo("builtin-cleanup-repo");
        let home = test_utils::test_dir("builtin-cleanup-home");
        std::env::set_var("HOME", home.to_str().unwrap());

        let spec_id = "det-cleanup-001";
        let dest = worktree::create(spec_id, repo.to_str().unwrap()).unwrap();
        assert!(dest.exists(), "worktree should exist before cleanup");

        let result = run_builtin("builtin:cleanup", &make_ctx(spec_id, "Cleanup", repo.to_str().unwrap()));
        assert!(matches!(result, BuiltinResult::Success(_)), "cleanup failed: {:?}", result);
        assert!(!dest.exists(), "worktree dir should be gone after cleanup");
    }

    #[test]
    fn test_deterministic_cleanup_without_repo_path_returns_error() {
        let ctx = make_ctx("s001", "Cleanup", "");
        let result = run_builtin("builtin:cleanup", &ctx);
        assert!(matches!(result, BuiltinResult::Error(_)));
    }

    // --- verdict mapping ---

    #[test]
    fn test_deterministic_success_maps_to_proceed() {
        assert_eq!(BuiltinResult::Success("ok".into()).to_verdict(), Verdict::Proceed);
    }

    #[test]
    fn test_deterministic_noop_maps_to_proceed() {
        assert_eq!(BuiltinResult::NoOp("nothing".into()).to_verdict(), Verdict::Proceed);
    }

    #[test]
    fn test_deterministic_error_maps_to_done_failure() {
        let v = BuiltinResult::Error("oops".into()).to_verdict();
        assert!(matches!(v, Verdict::Done { success: false, .. }));
    }
}
