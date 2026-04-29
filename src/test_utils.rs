use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Global mutex for tests that mutate the HOME env var.
/// Any test that calls std::env::set_var("HOME", ...) must hold this lock
/// to prevent races across modules running in parallel.
pub static HOME_LOCK: Mutex<()> = Mutex::new(());

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Returns a unique directory under the system temp dir for test isolation.
/// Each call produces a new path: `/tmp/boi-test-{label}-{pid}-{counter}/`
/// The directory is created automatically.
pub fn test_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "boi-test-{}-{}-{}",
        label,
        std::process::id(),
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("failed to create test dir");
    dir
}

/// Returns a unique file path under the system temp dir (file is NOT created).
pub fn test_file(label: &str, ext: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "boi-test-{}-{}-{}.{}",
        label,
        std::process::id(),
        n,
        ext
    ))
}

/// Creates a temporary git repo with a single initial commit.
pub fn test_git_repo(label: &str) -> PathBuf {
    use std::process::Command;
    let dir = test_dir(label);
    Command::new("git")
        .args(["init"])
        .current_dir(&dir)
        .output()
        .expect("git init failed");
    Command::new("git")
        .args(["config", "user.email", "test@boi.test"])
        .current_dir(&dir)
        .output()
        .expect("git config email failed");
    Command::new("git")
        .args(["config", "user.name", "BOI Test"])
        .current_dir(&dir)
        .output()
        .expect("git config name failed");
    std::fs::write(dir.join("README.md"), "test").expect("failed to write README");
    Command::new("git")
        .args(["add", "."])
        .current_dir(&dir)
        .output()
        .expect("git add failed");
    Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&dir)
        .output()
        .expect("git commit failed");
    dir
}

/// Creates a mock claude script that exits with the given code.
pub fn mock_claude_script(exit_code: u8, label: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = test_file(label, "sh");
    std::fs::write(&path, format!("#!/bin/sh\nexit {}\n", exit_code))
        .expect("failed to write mock script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("failed to chmod mock script");
    path
}

/// Creates a mock claude script that writes to stdout/stderr then exits.
pub fn mock_claude_script_with_output(
    exit_code: u8,
    stdout_msg: &str,
    stderr_msg: &str,
    label: &str,
) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = test_file(label, "sh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\necho '{}'\necho '{}' >&2\nexit {}\n",
            stdout_msg, stderr_msg, exit_code
        ),
    )
    .expect("failed to write mock script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("failed to chmod mock script");
    path
}
