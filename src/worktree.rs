use std::path::PathBuf;
use std::process::Command;

fn worktrees_base() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".boi").join("worktrees")
}

fn worktree_path(spec_id: &str) -> PathBuf {
    worktrees_base().join(spec_id)
}

/// Create a git worktree for the given spec at ~/.boi/worktrees/{spec_id}.
/// Returns the path to the new worktree.
pub fn create(spec_id: &str, repo_path: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let dest = worktree_path(spec_id);

    if dest.exists() {
        return Ok(dest);
    }

    // Ensure the parent directory exists.
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let output = Command::new("git")
        .args(["worktree", "add", "--detach", dest.to_str().unwrap()])
        .current_dir(repo_path)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git worktree add failed: {}", stderr).into());
    }

    Ok(dest)
}

/// Remove the worktree for the given spec and delete its directory.
pub fn cleanup(spec_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    let dest = worktree_path(spec_id);

    if !dest.exists() {
        return Ok(());
    }

    // git worktree remove --force is safe even if the worktree has uncommitted changes.
    let output = Command::new("git")
        .args(["worktree", "remove", "--force", dest.to_str().unwrap()])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("git worktree remove warning: {}", stderr);
        // Best-effort: remove the directory even if git command failed.
        let _ = std::fs::remove_dir_all(&dest);
    }

    Ok(())
}

/// Prune dangling worktree entries from git's internal list and remove
/// any directories under ~/.boi/worktrees/ that are no longer registered.
pub fn cleanup_stale() -> Result<(), Box<dyn std::error::Error>> {
    // git worktree prune removes stale administrative files.
    let _ = Command::new("git").args(["worktree", "prune"]).output();

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
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(suffix: &str) -> Self {
            let p = std::env::temp_dir().join(format!("boi-test-{}-{}", suffix, std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn init_git_repo(dir: &std::path::Path) {
        Command::new("git").args(["init"]).current_dir(dir).output().unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@boi.test"])
            .current_dir(dir).output().unwrap();
        Command::new("git")
            .args(["config", "user.name", "BOI Test"])
            .current_dir(dir).output().unwrap();
        // Need at least one commit so HEAD exists for `git worktree add --detach`.
        std::fs::write(dir.join("README.md"), "test").unwrap();
        Command::new("git").args(["add", "."]).current_dir(dir).output().unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir).output().unwrap();
    }

    use std::sync::Mutex;
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_create_and_cleanup() {
        let _guard = TEST_LOCK.lock().unwrap();
        let repo_dir = TempDir::new("repo");
        init_git_repo(repo_dir.path());

        let wt_base = TempDir::new("home");
        std::env::set_var("HOME", wt_base.path().to_str().unwrap());

        let spec_id = "test-spec-001";
        let dest = create(spec_id, repo_dir.path().to_str().unwrap()).unwrap();

        assert!(dest.exists(), "worktree directory should exist after create");
        assert!(dest.join(".git").exists(), "worktree should have .git pointer");

        cleanup(spec_id).unwrap();
    }

    #[test]
    fn test_create_idempotent() {
        let _guard = TEST_LOCK.lock().unwrap();
        let repo_dir = TempDir::new("repo2");
        init_git_repo(repo_dir.path());

        let wt_base = TempDir::new("home2");
        std::env::set_var("HOME", wt_base.path().to_str().unwrap());

        let spec_id = "test-spec-idempotent";
        let dest1 = create(spec_id, repo_dir.path().to_str().unwrap()).unwrap();
        let dest2 = create(spec_id, repo_dir.path().to_str().unwrap()).unwrap();
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
        let wt_base = TempDir::new("home3");
        std::env::set_var("HOME", wt_base.path().to_str().unwrap());
        assert!(cleanup_stale().is_ok());
    }
}
