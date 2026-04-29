use std::path::PathBuf;
use std::process::Command;

fn worktrees_base() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".boi").join("worktrees")
}

fn worktree_path(spec_id: &str) -> PathBuf {
    worktrees_base().join(spec_id)
}

pub fn branch_name(spec_id: &str) -> String {
    format!("boi/{}", spec_id)
}

/// Create a git worktree for the given spec at ~/.boi/worktrees/{spec_id}.
/// Uses a named branch `boi/{spec_id}` so changes can be merged back.
/// Returns the path to the new worktree.
pub fn create(spec_id: &str, repo_path: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let dest = worktree_path(spec_id);

    if dest.exists() {
        return Ok(dest);
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let branch = branch_name(spec_id);

    // Delete stale branch from a prior run if it exists.
    let _ = Command::new("git") // intentional: best-effort stale branch cleanup from prior run
        .args(["branch", "-D", &branch])
        .current_dir(repo_path)
        .output();

    let output = Command::new("git")
        .args(["worktree", "add", "-b", &branch, dest.to_str().ok_or("worktree dest path is not valid UTF-8")?])
        .current_dir(repo_path)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git worktree add failed: {}", stderr).into());
    }

    Ok(dest)
}

/// Commit all changes in the worktree. Returns true if there were changes to commit.
pub fn commit_changes(spec_id: &str, message: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let dest = worktree_path(spec_id);
    if !dest.exists() {
        return Err("worktree does not exist".into());
    }

    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&dest)
        .output()?;
    let status_text = String::from_utf8_lossy(&status.stdout);

    if status_text.trim().is_empty() {
        return Ok(false);
    }

    let add = Command::new("git")
        .args(["add", "-A"])
        .current_dir(&dest)
        .output()?;
    if !add.status.success() {
        return Err(format!("git add failed: {}", String::from_utf8_lossy(&add.stderr)).into());
    }

    let commit = Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(&dest)
        .output()?;
    if !commit.status.success() {
        let stderr = String::from_utf8_lossy(&commit.stderr);
        if stderr.contains("nothing to commit") {
            return Ok(false);
        }
        return Err(format!("git commit failed: {}", stderr).into());
    }

    Ok(true)
}

/// Merge the worktree branch back into the source repo's current branch.
/// Returns the merge commit message on success.
pub fn merge_back(spec_id: &str, repo_path: &str) -> Result<String, Box<dyn std::error::Error>> {
    let branch = branch_name(spec_id);

    let output = Command::new("git")
        .args(["merge", &branch, "--no-edit"])
        .current_dir(repo_path)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git merge failed: {}", stderr).into());
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(stdout)
}

/// Remove the worktree, prune stale refs, and delete the branch.
/// Single call handles the full teardown sequence.
pub fn cleanup(spec_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    let dest = worktree_path(spec_id);

    if dest.exists() {
        let output = Command::new("git")
            .args(["worktree", "remove", "--force", dest.to_str().ok_or("worktree dest path is not valid UTF-8")?])
            .output()?;

        if !output.status.success() {
            let _ = std::fs::remove_dir_all(&dest); // intentional: fallback cleanup when git worktree remove fails
        }
    }

    Ok(())
}

/// Prune stale worktree refs and delete the branch.
/// Must be called from the repo directory after cleanup.
pub fn delete_branch(spec_id: &str, repo_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let branch = branch_name(spec_id);

    // Prune stale worktree references first so the branch is deletable.
    let prune = Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(repo_path)
        .output()?;
    if !prune.status.success() {
        eprintln!("[boi] worktree prune failed: {}", String::from_utf8_lossy(&prune.stderr));
    }

    let del = Command::new("git")
        .args(["branch", "-D", &branch])
        .current_dir(repo_path)
        .output()?;
    if !del.status.success() {
        eprintln!("[boi] branch delete failed for {}: {}",
            branch, String::from_utf8_lossy(&del.stderr));
    }
    Ok(())
}

/// Prune dangling worktree entries from git's internal list and remove
/// any directories under ~/.boi/worktrees/ that are no longer registered.
pub fn cleanup_stale() -> Result<(), Box<dyn std::error::Error>> {
    // git worktree prune removes stale administrative files.
    let _ = Command::new("git").args(["worktree", "prune"]).output(); // intentional: best-effort prune of stale refs

    let base = worktrees_base();
    if !base.exists() {
        return Ok(());
    }

    // Remove directories that are no longer tracked as git worktrees.
    for entry in std::fs::read_dir(&base)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            // A valid worktree has a .git file (not directory) inside it.
            if !path.join(".git").exists() {
                eprintln!("Removing stale worktree dir: {}", path.display());
                let _ = std::fs::remove_dir_all(&path); // intentional: best-effort stale worktree cleanup
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils;

    use std::sync::Mutex;
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_create_and_cleanup() {
        let _guard = TEST_LOCK.lock().unwrap();
        let repo_dir = test_utils::test_git_repo("wt-repo");

        let wt_base = test_utils::test_dir("wt-home");
        std::env::set_var("HOME", wt_base.to_str().unwrap());

        let spec_id = "test-spec-001";
        let dest = create(spec_id, repo_dir.to_str().unwrap()).unwrap();

        assert!(dest.exists(), "worktree directory should exist after create");
        assert!(dest.join(".git").exists(), "worktree should have .git pointer");

        cleanup(spec_id).unwrap();
    }

    #[test]
    fn test_create_idempotent() {
        let _guard = TEST_LOCK.lock().unwrap();
        let repo_dir = test_utils::test_git_repo("wt-repo2");

        let wt_base = test_utils::test_dir("wt-home2");
        std::env::set_var("HOME", wt_base.to_str().unwrap());

        let spec_id = "test-spec-idempotent";
        let dest1 = create(spec_id, repo_dir.to_str().unwrap()).unwrap();
        let dest2 = create(spec_id, repo_dir.to_str().unwrap()).unwrap();
        assert_eq!(dest1, dest2);
    }

    #[test]
    fn test_cleanup_nonexistent_is_ok() {
        let _guard = TEST_LOCK.lock().unwrap();
        assert!(cleanup("nonexistent-spec-xyz").is_ok());
    }

    #[test]
    fn test_cleanup_stale_empty_base() {
        let _guard = TEST_LOCK.lock().unwrap();
        let wt_base = test_utils::test_dir("wt-home3");
        std::env::set_var("HOME", wt_base.to_str().unwrap());
        assert!(cleanup_stale().is_ok());
    }

    #[test]
    fn test_commit_and_merge_back() {
        let _guard = TEST_LOCK.lock().unwrap();
        let repo_dir = test_utils::test_git_repo("wt-merge-repo");

        let wt_base = test_utils::test_dir("wt-merge-home");
        std::env::set_var("HOME", wt_base.to_str().unwrap());

        let spec_id = "test-merge-001";
        let repo = repo_dir.to_str().unwrap();
        let dest = create(spec_id, repo).unwrap();

        std::fs::write(dest.join("new-feature.txt"), "hello from boi").unwrap();

        assert!(!repo_dir.join("new-feature.txt").exists(),
            "file should NOT exist in source repo before merge");

        let committed = commit_changes(spec_id, "boi: add feature").unwrap();
        assert!(committed, "should report changes were committed");

        let result = merge_back(spec_id, repo);
        assert!(result.is_ok(), "merge should succeed: {:?}", result.err());

        assert!(repo_dir.join("new-feature.txt").exists(),
            "file should exist in source repo after merge");
        let content = std::fs::read_to_string(repo_dir.join("new-feature.txt")).unwrap();
        assert_eq!(content, "hello from boi");

        cleanup(spec_id).unwrap();
        delete_branch(spec_id, repo).unwrap();
    }

    #[test]
    fn test_commit_no_changes() {
        let _guard = TEST_LOCK.lock().unwrap();
        let repo_dir = test_utils::test_git_repo("wt-no-change-repo");

        let wt_base = test_utils::test_dir("wt-no-change-home");
        std::env::set_var("HOME", wt_base.to_str().unwrap());

        let spec_id = "test-no-change-001";
        let _dest = create(spec_id, repo_dir.to_str().unwrap()).unwrap();

        let committed = commit_changes(spec_id, "no changes").unwrap();
        assert!(!committed, "should report no changes to commit");

        cleanup(spec_id).unwrap();
    }

    #[test]
    fn test_branch_deleted_after_cleanup() {
        let _guard = TEST_LOCK.lock().unwrap();
        let repo_dir = test_utils::test_git_repo("wt-branch-del-repo");

        let wt_base = test_utils::test_dir("wt-branch-del-home");
        std::env::set_var("HOME", wt_base.to_str().unwrap());

        let spec_id = "test-branch-del-001";
        let repo = repo_dir.to_str().unwrap();
        let dest = create(spec_id, repo).unwrap();

        std::fs::write(dest.join("feature.txt"), "done").unwrap();
        commit_changes(spec_id, "add feature").unwrap();
        merge_back(spec_id, repo).unwrap();

        cleanup(spec_id).unwrap();
        delete_branch(spec_id, repo).unwrap();

        let output = std::process::Command::new("git")
            .args(["branch", "--list", &branch_name(spec_id)])
            .current_dir(&repo_dir)
            .output().unwrap();
        let branches = String::from_utf8_lossy(&output.stdout);
        assert!(branches.trim().is_empty(),
            "branch should be deleted, got: '{}'", branches.trim());
    }

    #[test]
    fn test_source_repo_clean_during_worktree_work() {
        let _guard = TEST_LOCK.lock().unwrap();
        let repo_dir = test_utils::test_git_repo("wt-isolation-repo");

        let wt_base = test_utils::test_dir("wt-isolation-home");
        std::env::set_var("HOME", wt_base.to_str().unwrap());

        let spec_id = "test-isolation-001";
        let repo = repo_dir.to_str().unwrap();
        let dest = create(spec_id, repo).unwrap();

        std::fs::write(dest.join("worktree-only.txt"), "isolated").unwrap();

        let status = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&repo_dir)
            .output().unwrap();
        let status_text = String::from_utf8_lossy(&status.stdout);
        assert!(!status_text.contains("worktree-only.txt"),
            "source repo should not show worktree files: {}", status_text);

        cleanup(spec_id).unwrap();
    }
}
