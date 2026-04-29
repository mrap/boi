use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static COUNTER: AtomicU64 = AtomicU64::new(0);
static LOCK: Mutex<()> = Mutex::new(());

fn unique_id() -> u64 {
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

fn test_dir(label: &str) -> PathBuf {
    let n = unique_id();
    let dir = std::env::temp_dir().join(format!(
        "boi-v2smoke-{}-{}-{}",
        label,
        std::process::id(),
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("failed to create test dir");
    dir
}

fn test_file(label: &str, ext: &str) -> PathBuf {
    let n = unique_id();
    std::env::temp_dir().join(format!(
        "boi-v2smoke-{}-{}-{}.{}",
        label,
        std::process::id(),
        n,
        ext
    ))
}

fn test_git_repo(label: &str) -> PathBuf {
    use std::process::Command;
    let dir = test_dir(label);
    Command::new("git").args(["init"]).current_dir(&dir).output().expect("git init");
    Command::new("git")
        .args(["config", "user.email", "test@boi.test"])
        .current_dir(&dir)
        .output()
        .expect("git config email");
    Command::new("git")
        .args(["config", "user.name", "BOI Test"])
        .current_dir(&dir)
        .output()
        .expect("git config name");
    std::fs::write(dir.join("README.md"), "test").expect("write README");
    Command::new("git").args(["add", "."]).current_dir(&dir).output().expect("git add");
    Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&dir)
        .output()
        .expect("git commit");
    dir
}

fn write_mock_claude(home: &PathBuf) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = test_file("mock-claude", "sh");
    // Creates the target file and outputs all approval signals used across v2 phases.
    std::fs::write(
        &path,
        r###"#!/bin/sh
touch smoke-output.txt
echo "## Spec Approved"
echo "## Spec Improved"
echo "## Review Approved"
echo "## Docs Updated"
echo "## Critic Approved"
exit 0
"###,
    )
    .expect("write mock claude");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod mock claude");
    let _ = home; // unused; parameter kept for clarity
    path
}

#[test]
fn v2_smoke() {
    let _guard = LOCK.lock().unwrap();

    // Isolated HOME so worktrees land in a temp dir, not ~/.boi/worktrees/
    let fake_home = test_dir("home");
    std::env::set_var("HOME", fake_home.to_str().unwrap());

    // Point to the repo's pipelines.toml so mode.v2 is available.
    let pipelines_path = format!("{}/phases/pipelines.toml", env!("CARGO_MANIFEST_DIR"));
    std::env::set_var("BOI_PIPELINES_FILE", &pipelines_path);

    let repo = test_git_repo("repo");
    let mock_bin = write_mock_claude(&fake_home);
    let db_path = test_file("queue", "db");

    // Parse spec and inject workspace dynamically.
    let spec_yaml = format!("{}/tests/fixtures/v2-smoke.yaml", env!("CARGO_MANIFEST_DIR"));
    let content = std::fs::read_to_string(&spec_yaml).expect("read v2-smoke.yaml");
    let mut spec = boi::spec::parse(&content).expect("parse v2-smoke.yaml");
    spec.workspace = Some(repo.to_str().unwrap().to_string());

    let queue = boi::queue::Queue::open(db_path.to_str().unwrap()).expect("open queue");
    let spec_id = queue.enqueue(&spec, Some(&spec_yaml)).expect("enqueue");

    let telemetry = boi::telemetry::Telemetry::new(db_path.clone());
    let runner = boi::runner::ClaudePhaseRunner::new(
        telemetry.clone(),
        mock_bin.to_str().unwrap().to_string(),
    )
    .with_repo_path(repo.to_str().unwrap());

    let registry = boi::phases::PhaseRegistry::new();
    let hook_cfg = boi::hooks::HookConfig::default();
    let config = boi::worker::WorkerConfig {
        task_timeout_secs: 10,
        ..Default::default()
    };

    boi::worker::run_worker_with_phases(
        &spec_id,
        &spec_yaml,
        db_path.to_str().unwrap(),
        &hook_cfg,
        &config,
        &registry,
        &runner,
        &telemetry,
    )
    .expect("pipeline should complete without error");

    // --- Phase assertions ---
    let summaries = queue.phase_cost_summary(&spec_id).expect("phase_cost_summary");
    let phases: std::collections::HashSet<String> =
        summaries.iter().map(|s| s.phase.clone()).collect();

    assert!(phases.contains("spec-critique"), "spec-critique should have run; got: {:?}", phases);
    assert!(phases.contains("execute"),       "execute should have run; got: {:?}", phases);
    assert!(phases.contains("review"),        "review should have run; got: {:?}", phases);
    assert!(phases.contains("commit"),        "commit should have run; got: {:?}", phases);
    assert!(phases.contains("critic"),        "critic should have run; got: {:?}", phases);
    assert!(phases.contains("merge"),         "merge should have run; got: {:?}", phases);
    assert!(phases.contains("cleanup"),       "cleanup should have run; got: {:?}", phases);

    // --- commit ran: git log should contain a BOI commit ---
    let git_log = std::process::Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(&repo)
        .output()
        .expect("git log");
    let log_str = String::from_utf8_lossy(&git_log.stdout);
    assert!(
        log_str.contains("boi("),
        "expected BOI commit in git log, got: {}",
        log_str
    );

    // --- cleanup ran: worktree directory should be gone ---
    let worktree_dir = fake_home.join(".boi").join("worktrees").join(&spec_id);
    assert!(
        !worktree_dir.exists(),
        "worktree should be gone after cleanup, but {} still exists",
        worktree_dir.display()
    );

    // --- file exists in target branch after merge ---
    assert!(
        repo.join("smoke-output.txt").exists(),
        "smoke-output.txt should exist in repo after merge"
    );

    // Restore env
    std::env::remove_var("BOI_PIPELINES_FILE");
}
