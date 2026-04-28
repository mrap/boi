use crate::{
    hooks::{
        self, HookConfig, ON_COMPLETE, ON_FAIL, ON_TASK_COMPLETE, ON_TASK_FAIL, ON_TASK_START,
        ON_WORKER_START,
    },
    queue::Queue,
    spec,
};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub struct WorkerConfig {
    pub max_workers: u32,
    pub task_timeout_secs: u64,
    pub retry_count: u32,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        WorkerConfig {
            max_workers: 5,
            task_timeout_secs: 1800,
            retry_count: 3,
        }
    }
}

pub fn build_prompt(spec_content: &str, task: &spec::BoiTask) -> String {
    let task_spec = task.spec.as_deref().unwrap_or("(no spec provided)");
    let task_verify = task.verify.as_deref().unwrap_or("(no verify command)");
    format!(
        "You are a BOI worker. Execute exactly one task from this spec.\n\n\
        FULL SPEC:\n{}\n\n\
        YOUR TASK: {} — {}\n\n\
        SPEC:\n{}\n\n\
        VERIFY:\n{}\n\n\
        Execute the task completely. Mark it status: DONE in the spec file when done.",
        spec_content, task.id, task.title, task_spec, task_verify
    )
}

pub fn run_verify(verify_cmd: &str, dir: &str) -> bool {
    Command::new("sh")
        .args(["-c", verify_cmd])
        .current_dir(dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Spawn claude with the task prompt. Returns (success, stdout).
/// Respects timeout: kills the process and returns (false, "timeout") if exceeded.
/// Override the claude binary via CLAUDE_BIN env var (useful for tests).
pub fn spawn_claude(
    prompt: &str,
    worktree_path: &str,
    timeout_secs: u64,
) -> Result<(bool, String), Box<dyn std::error::Error>> {
    let claude_bin = std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());
    let mut child = Command::new(&claude_bin)
        .args(["-p", prompt, "--output-format", "json"])
        .env("AGENT_DIR", worktree_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let start = Instant::now();
    loop {
        match child.try_wait()? {
            Some(_) => break,
            None => {
                if start.elapsed().as_secs() >= timeout_secs {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok((false, "timeout".to_string()));
                }
                // Claude sessions run for minutes; 2s poll is responsive enough
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Ok((output.status.success(), stdout))
}

/// Execute all pending tasks for a queued spec.
///
/// Reads the spec YAML at `spec_path`, processes tasks in topological order,
/// spawning `claude -p` for each PENDING task. Updates `queue_path` (SQLite)
/// after each task and when the spec completes or fails.
pub fn run_worker(
    spec_id: &str,
    spec_path: &str,
    queue_path: &str,
    hook_config: &HookConfig,
    config: &WorkerConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let queue = Queue::open(queue_path)?;
    queue.update_spec(spec_id, "running")?;
    let _ = hooks::fire(hook_config, ON_WORKER_START, &json!({ "spec_id": spec_id }));

    let repo_path = std::env::var("BOI_REPO").unwrap_or_else(|_| {
        std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string())
    });
    let worktree_dir = crate::worktree::create(spec_id, &repo_path)?;
    let worktree_path = worktree_dir.to_str().unwrap_or("/tmp").to_string();

    let spec_content = std::fs::read_to_string(spec_path)?;
    let boi_spec = spec::parse_unchecked(&spec_content)?;

    let order = match spec::topological_sort(&boi_spec) {
        Ok(o) => o,
        Err(e) => {
            queue.update_spec(spec_id, "failed")?;
            return Err(Box::new(e));
        }
    };

    let task_map: HashMap<&str, &spec::BoiTask> =
        boi_spec.tasks.iter().map(|t| (t.id.as_str(), t)).collect();

    // Track which tasks we've completed this run (supplements spec YAML state)
    let mut done_ids: HashSet<String> = boi_spec
        .tasks
        .iter()
        .filter(|t| t.status == spec::TaskStatus::Done)
        .map(|t| t.id.clone())
        .collect();

    let mut overall_success = true;

    'tasks: for task_id in &order {
        let task = match task_map.get(task_id.as_str()) {
            Some(t) => t,
            None => continue,
        };

        if task.status != spec::TaskStatus::Pending {
            continue;
        }

        if let Some(deps) = &task.depends {
            if deps.iter().any(|d| !done_ids.contains(d)) {
                continue;
            }
        }

        let task_payload = json!({
            "spec_id": spec_id,
            "task_id": task.id,
            "task_title": task.title,
        });

        queue.update_task(spec_id, &task.id, "RUNNING")?;
        let _ = hooks::fire(hook_config, ON_TASK_START, &task_payload);

        let prompt = build_prompt(&spec_content, task);
        let mut task_success = false;

        'retry: for attempt in 0..=config.retry_count {
            match spawn_claude(&prompt, &worktree_path, config.task_timeout_secs) {
                Ok((exited_ok, _output)) => {
                    let verify_ok = task
                        .verify
                        .as_deref()
                        .map(|cmd| run_verify(cmd, &worktree_path))
                        .unwrap_or(exited_ok);

                    if verify_ok {
                        task_success = true;
                        break 'retry;
                    }
                    if attempt < config.retry_count {
                        eprintln!(
                            "[boi] task {} failed verify (attempt {}/{}), retrying",
                            task.id,
                            attempt + 1,
                            config.retry_count + 1
                        );
                    }
                }
                Err(e) => {
                    eprintln!("[boi] task {} spawn error: {}", task.id, e);
                    break 'retry;
                }
            }
        }

        if task_success {
            queue.update_task(spec_id, &task.id, "DONE")?;
            done_ids.insert(task.id.clone());
            let _ = hooks::fire(hook_config, ON_TASK_COMPLETE, &task_payload);
        } else {
            queue.update_task(spec_id, &task.id, "FAILED")?;
            let _ = hooks::fire(hook_config, ON_TASK_FAIL, &task_payload);
            overall_success = false;
            break 'tasks;
        }
    }

    if overall_success {
        queue.update_spec(spec_id, "completed")?;
        let _ = hooks::fire(hook_config, ON_COMPLETE, &json!({ "spec_id": spec_id }));
    } else {
        queue.update_spec(spec_id, "failed")?;
        let _ = hooks::fire(hook_config, ON_FAIL, &json!({ "spec_id": spec_id }));
    }

    let _ = crate::worktree::cleanup(spec_id);

    Ok(())
}

/// Poll the queue every 5 seconds and spawn workers up to `config.max_workers`.
/// Runs until the process is killed.
pub fn run_daemon(queue_path: &str, hook_config: HookConfig, config: WorkerConfig) {
    use std::sync::{Arc, Mutex};

    let active: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>> =
        Arc::new(Mutex::new(Vec::new()));

    eprintln!("[boi daemon] started, max_workers={}", config.max_workers);

    loop {
        {
            let mut workers = active.lock().unwrap();
            workers.retain(|h| !h.is_finished());

            if workers.len() < config.max_workers as usize {
                match Queue::open(queue_path) {
                    Ok(queue) => match queue.dequeue() {
                        Ok(Some(rec)) => {
                            let spec_id = rec.id.clone();
                            let spec_path = rec.spec_path.clone().unwrap_or_default();
                            let qpath = queue_path.to_string();
                            let hc = hook_config.clone();
                            let timeout = config.task_timeout_secs;
                            let retries = config.retry_count;

                            eprintln!("[boi daemon] starting worker for {}", spec_id);
                            let handle = std::thread::spawn(move || {
                                let wc = WorkerConfig {
                                    max_workers: 1,
                                    task_timeout_secs: timeout,
                                    retry_count: retries,
                                };
                                if let Err(e) =
                                    run_worker(&spec_id, &spec_path, &qpath, &hc, &wc)
                                {
                                    eprintln!(
                                        "[boi daemon] worker error for {}: {}",
                                        spec_id, e
                                    );
                                }
                            });
                            workers.push(handle);
                        }
                        Ok(None) => {}
                        Err(e) => eprintln!("[boi daemon] dequeue error: {}", e),
                    },
                    Err(e) => eprintln!("[boi daemon] queue open error: {}", e),
                }
            }
        }

        std::thread::sleep(Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hooks::HookConfig, queue::Queue, spec};
    use std::sync::Mutex;

    // Serializes tests that mutate env vars to avoid races.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Run `f` with CLAUDE_BIN set to `bin_path`, holding ENV_LOCK.
    fn with_claude_bin<F: FnOnce()>(bin_path: &str, f: F) {
        let _lock = ENV_LOCK.lock().unwrap();
        let old = std::env::var("CLAUDE_BIN").ok();
        unsafe { std::env::set_var("CLAUDE_BIN", bin_path) };
        f();
        unsafe {
            match old {
                Some(v) => std::env::set_var("CLAUDE_BIN", v),
                None => std::env::remove_var("CLAUDE_BIN"),
            }
        }
    }

    /// Run `f` with CLAUDE_BIN and BOI_REPO set, holding ENV_LOCK.
    fn with_test_env<F: FnOnce()>(bin_path: &str, repo_path: &str, f: F) {
        let _lock = ENV_LOCK.lock().unwrap();
        let old_bin = std::env::var("CLAUDE_BIN").ok();
        let old_repo = std::env::var("BOI_REPO").ok();
        unsafe {
            std::env::set_var("CLAUDE_BIN", bin_path);
            std::env::set_var("BOI_REPO", repo_path);
        }
        f();
        unsafe {
            match old_bin {
                Some(v) => std::env::set_var("CLAUDE_BIN", v),
                None => std::env::remove_var("CLAUDE_BIN"),
            }
            match old_repo {
                Some(v) => std::env::set_var("BOI_REPO", v),
                None => std::env::remove_var("BOI_REPO"),
            }
        }
    }

    /// Create a temporary git repo for worktree testing.
    fn setup_test_repo(suffix: &str) -> std::path::PathBuf {
        use std::process::Command;
        let repo_dir = std::env::temp_dir().join(format!("boi_test_repo_{}", suffix));
        let _ = std::fs::remove_dir_all(&repo_dir);
        std::fs::create_dir_all(&repo_dir).unwrap();
        Command::new("git").args(["init"]).current_dir(&repo_dir).output().unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@boi.test"])
            .current_dir(&repo_dir).output().unwrap();
        Command::new("git")
            .args(["config", "user.name", "BOI Test"])
            .current_dir(&repo_dir).output().unwrap();
        std::fs::write(repo_dir.join("README.md"), "test").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&repo_dir).output().unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&repo_dir).output().unwrap();
        repo_dir
    }

    fn mock_claude(exit_code: u8, suffix: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("boi_mock_claude_{}", suffix));
        std::fs::write(&path, format!("#!/bin/sh\nexit {}\n", exit_code)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn test_default_config() {
        let cfg = WorkerConfig::default();
        assert_eq!(cfg.max_workers, 5);
        assert_eq!(cfg.retry_count, 3);
        assert_eq!(cfg.task_timeout_secs, 1800);
    }

    #[test]
    fn test_build_prompt_contains_task_fields() {
        let task = spec::BoiTask {
            id: "t-1".to_string(),
            title: "Setup Cargo".to_string(),
            status: spec::TaskStatus::Pending,
            depends: None,
            spec: Some("Run cargo init".to_string()),
            verify: Some("test -f Cargo.toml".to_string()),
        };
        let prompt = build_prompt("title: Test\ntasks: []", &task);
        assert!(prompt.contains("t-1"));
        assert!(prompt.contains("Setup Cargo"));
        assert!(prompt.contains("Run cargo init"));
        assert!(prompt.contains("test -f Cargo.toml"));
    }

    #[test]
    fn test_run_verify_success() {
        assert!(run_verify("true", "/tmp"));
    }

    #[test]
    fn test_run_verify_failure() {
        assert!(!run_verify("false", "/tmp"));
    }

    #[test]
    fn test_run_verify_missing_command() {
        assert!(!run_verify("exit 1", "/tmp"));
    }

    #[test]
    fn test_spawn_claude_exit_0() {
        let script = mock_claude(0, "exit0");
        with_claude_bin(script.to_str().unwrap(), || {
            let (ok, _) = spawn_claude("prompt", "/tmp", 10).unwrap();
            assert!(ok);
        });
    }

    #[test]
    fn test_spawn_claude_exit_1() {
        let script = mock_claude(1, "exit1");
        with_claude_bin(script.to_str().unwrap(), || {
            let (ok, _) = spawn_claude("prompt", "/tmp", 10).unwrap();
            assert!(!ok);
        });
    }

    fn setup_test_db(suffix: &str, spec_yaml: &str) -> (Queue, String, String) {
        let spec_file = std::env::temp_dir().join(format!("boi_test_spec_{}.yaml", suffix));
        std::fs::write(&spec_file, spec_yaml).unwrap();

        let db_file = std::env::temp_dir().join(format!("boi_test_db_{}.db", suffix));
        let _ = std::fs::remove_file(&db_file);
        let queue = Queue::open(db_file.to_str().unwrap()).unwrap();
        let boi_spec = spec::parse(spec_yaml).unwrap();
        let spec_id = queue.enqueue(&boi_spec, spec_file.to_str()).unwrap();

        (queue, spec_id, db_file.to_str().unwrap().to_string())
    }

    #[test]
    fn test_run_worker_completes_on_success() {
        let script = mock_claude(0, "worker_ok");
        let repo = setup_test_repo("worker_ok");
        let spec_yaml =
            "title: \"Worker Test\"\ntasks:\n  - id: t-1\n    title: \"Step\"\n    status: PENDING\n    spec: \"Do it\"\n";
        let (queue, spec_id, db_path) = setup_test_db("worker_ok", spec_yaml);
        let spec_file = std::env::temp_dir().join("boi_test_spec_worker_ok.yaml");
        let config = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: 10,
            retry_count: 0,
        };

        with_test_env(script.to_str().unwrap(), repo.to_str().unwrap(), || {
            run_worker(
                &spec_id,
                spec_file.to_str().unwrap(),
                &db_path,
                &HookConfig::default(),
                &config,
            )
            .unwrap();
        });

        let st = queue.status(&spec_id).unwrap().unwrap();
        assert_eq!(st.spec.status, "completed");
        assert_eq!(st.tasks[0].status, "DONE");
    }

    #[test]
    fn test_run_worker_fails_on_task_failure() {
        let script = mock_claude(1, "worker_fail");
        let repo = setup_test_repo("worker_fail");
        let spec_yaml =
            "title: \"Fail Test\"\ntasks:\n  - id: t-1\n    title: \"Will Fail\"\n    status: PENDING\n";
        let (queue, spec_id, db_path) = setup_test_db("worker_fail", spec_yaml);
        let spec_file = std::env::temp_dir().join("boi_test_spec_worker_fail.yaml");
        let config = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: 10,
            retry_count: 0,
        };

        with_test_env(script.to_str().unwrap(), repo.to_str().unwrap(), || {
            let _ = run_worker(
                &spec_id,
                spec_file.to_str().unwrap(),
                &db_path,
                &HookConfig::default(),
                &config,
            );
        });

        let st = queue.status(&spec_id).unwrap().unwrap();
        assert_eq!(st.spec.status, "failed");
        assert_eq!(st.tasks[0].status, "FAILED");
    }

    #[test]
    fn test_run_worker_skips_done_tasks() {
        let script = mock_claude(0, "worker_skip");
        let repo = setup_test_repo("worker_skip");
        // t-1 is already DONE in YAML; only t-2 should be executed
        let spec_yaml = "title: \"Skip Test\"\ntasks:\n  - id: t-1\n    title: \"Done\"\n    status: DONE\n  - id: t-2\n    title: \"Pending\"\n    status: PENDING\n    depends: [t-1]\n";
        let (queue, spec_id, db_path) = setup_test_db("worker_skip", spec_yaml);
        let spec_file = std::env::temp_dir().join("boi_test_spec_worker_skip.yaml");
        let config = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: 10,
            retry_count: 0,
        };

        with_test_env(script.to_str().unwrap(), repo.to_str().unwrap(), || {
            run_worker(
                &spec_id,
                spec_file.to_str().unwrap(),
                &db_path,
                &HookConfig::default(),
                &config,
            )
            .unwrap();
        });

        let st = queue.status(&spec_id).unwrap().unwrap();
        assert_eq!(st.spec.status, "completed");
        let t2 = st.tasks.iter().find(|t| t.id == "t-2").unwrap();
        assert_eq!(t2.status, "DONE");
    }
}
