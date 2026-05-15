use crate::fmt::{ensure_db_dir, is_pid_alive};
use crate::spawn::pid_dir;
use crate::{config, hooks, phases, pool, queue, runtime, worker};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

pub fn daemon_lock_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".boi").join("daemon.lock")
}

/// Try to acquire an exclusive flock on the daemon lock file.
/// Returns the held File on success (lock auto-releases when File drops).
/// Returns None if another daemon holds the lock.
pub fn try_acquire_daemon_lock() -> Option<std::fs::File> {
    let lock_path = daemon_lock_path();
    if let Some(parent) = lock_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("[boi daemon] ERROR: failed to create lock dir {}: {}", parent.display(), e);
        }
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .ok()?;
    // SAFETY: `file` is a valid open file descriptor obtained from `File::open` above.
    // `flock` with LOCK_NB is a standard POSIX call that cannot cause UB with a valid fd.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        use std::io::Write;
        let mut f = file;
        if let Err(e) = f.set_len(0) {
            eprintln!("[boi daemon] ERROR: failed to truncate lock file: {}", e);
        }
        if let Err(e) = write!(f, "{}", std::process::id()) {
            eprintln!("[boi daemon] ERROR: failed to write PID to lock file: {}", e);
        }
        Some(f)
    } else {
        None
    }
}

/// Check if a daemon is running by trying the lock.
pub fn is_daemon_locked() -> bool {
    try_acquire_daemon_lock().is_none()
}

/// Read the PID from the lock file (informational — the lock is the real guard).
pub fn read_daemon_pid() -> Option<u32> {
    std::fs::read_to_string(daemon_lock_path())
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

pub fn cmd_start() {
    if is_daemon_locked() {
        if let Some(pid) = read_daemon_pid() {
            eprintln!("daemon already running (pid {})", pid);
        } else {
            eprintln!("daemon already running");
        }
        std::process::exit(1);
    }

    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let log_dir = PathBuf::from(&home).join(".boi").join("logs");
    std::fs::create_dir_all(&log_dir).ok();

    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let log_path = log_dir.join(format!("daemon-{}.log", timestamp));
    let log_file = match std::fs::File::create(&log_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error creating log file: {}", e);
            std::process::exit(1);
        }
    };

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error finding current executable: {}", e);
            std::process::exit(1);
        }
    };

    let stderr_file = match log_file.try_clone() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error cloning log file handle: {}", e);
            std::process::exit(1);
        }
    };

    let child = match std::process::Command::new(&exe)
        .args(["daemon", "foreground"])
        .stdout(log_file)
        .stderr(stderr_file)
        .stdin(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error spawning daemon: {}", e);
            std::process::exit(1);
        }
    };

    println!("daemon started (pid {})", child.id());
    println!("log: {}", log_path.display());
}

pub fn cmd_restart(destroy_running: bool, yes: bool) {
    if is_daemon_locked() {
        cmd_stop(destroy_running, yes);
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    cmd_start();
}

pub fn daemon_pid_path() -> PathBuf {
    daemon_lock_path()
}

/// Kill every process group listed in PID files under `dir`, then remove the files.
/// Handles non-existent processes gracefully (ESRCH is ignored).
pub fn cleanup_pid_files(dir: &Path) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(pid) = content.trim().parse::<i32>() {
                // SAFETY: kill(-pid, SIGKILL) sends SIGKILL to the entire process group whose
                // PGID equals `pid`. This is safe: `pid` was read from a file we wrote, and
                // ESRCH is returned (and ignored) if the process group no longer exists.
                let rc = unsafe { libc::kill(-pid, libc::SIGKILL) };
                if rc == 0 {
                    eprintln!("[boi daemon] killed process group {} ({})", pid, path.display());
                }
            }
        }
        let _ = std::fs::remove_file(&path);
    }
}

pub fn daemon_heartbeat_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".boi").join("daemon.heartbeat")
}

pub fn cmd_daemon(db_str: &str, hook_cfg: hooks::HookConfig, cfg: &config::Config) {
    ensure_db_dir(db_str);

    // Singleton guard via flock — atomic, no stale PID files possible.
    // _lock_file must live for the entire daemon lifetime; dropping it releases the lock.
    let _lock_file = match try_acquire_daemon_lock() {
        Some(f) => f,
        None => {
            if let Some(pid) = read_daemon_pid() {
                eprintln!("[boi daemon] error: daemon already running (pid {})", pid);
            } else {
                eprintln!("[boi daemon] error: daemon already running");
            }
            std::process::exit(1);
        }
    };

    let pid = std::process::id();

    // Register SIGTERM handler via atomic flag
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let r = running.clone();

    // Install SIGTERM + SIGINT handler
    // SAFETY: The registered closures only perform a single atomic store, which is
    // async-signal-safe. No heap allocation, locking, or non-reentrant calls occur
    // inside the signal handler. The AtomicBool outlives the handlers (Arc-held).
    unsafe {
        let r2 = running.clone();
        signal_hook::low_level::register(signal_hook::consts::SIGTERM, move || {
            r2.store(false, std::sync::atomic::Ordering::SeqCst);
        })
        .ok();

        let r3 = r.clone();
        signal_hook::low_level::register(signal_hook::consts::SIGINT, move || {
            r3.store(false, std::sync::atomic::Ordering::SeqCst);
        })
        .ok();
    }

    let wc = worker::WorkerConfig {
        max_workers: cfg.max_workers(),
        task_timeout_secs: cfg.task_timeout_secs(),
        retry_count: cfg.retry_count(),
        cleanup_on_failure: cfg.cleanup_on_failure(),
        claude_bin: cfg.claude_bin(),
        models: cfg.models.clone(),
        convergence_threshold: cfg.convergence_threshold(),
    };

    // Orphan cleanup: kill any setsid'd Claude processes from a previous crash (F-03)
    cleanup_pid_files(&pid_dir());

    // Crash recovery: reset any specs stuck in 'running' or 'assigning' back to 'queued'
    if let Ok(q) = queue::Queue::open(db_str) {
        match q.recover_stuck_specs() {
            Ok(0) => {}
            Ok(n) => eprintln!("[boi daemon] recovered {} stuck spec(s) back to queued", n),
            Err(e) => eprintln!("[boi daemon] warning: crash recovery failed: {}", e),
        }
        match q.prune_events(30) {
            Ok(0) => {}
            Ok(n) => eprintln!("[boi daemon] pruned {} event(s) older than 30 days", n),
            Err(e) => eprintln!("[boi daemon] warning: event prune failed: {}", e),
        }
        match q.prune_phase_runs(90) {
            Ok(0) => {}
            Ok(n) => eprintln!("[boi daemon] pruned {} phase_run(s) older than 90 days", n),
            Err(e) => eprintln!("[boi daemon] warning: phase_run prune failed: {}", e),
        }
    }

    eprintln!(
        "[boi daemon] started (pid {}), max_workers={}",
        pid, wc.max_workers
    );

    // Validation point 2: warn loudly at startup for any phase whose `runtime`
    // field names a provider that is disabled or missing (e.g. no API key).
    // This surfaces the OpenRouter-runtime-drop class of bugs at daemon start
    // instead of silently falling through to Claude at invocation time.
    {
        let provider_registry = runtime::ProviderRegistry::new();
        let phase_registry = phases::PhaseRegistry::new();
        provider_registry.validate_phases(phase_registry.list().into_iter());
    }

    let registry = match cfg.build_pool_registry(db_str, hook_cfg) {
        Ok(r) => {
            eprintln!("[boi daemon] pool registry: default={}", r.default_name());
            r
        }
        Err(e) => {
            eprintln!("[boi daemon] ERROR: failed to build pool registry: {}", e);
            std::process::exit(1);
        }
    };

    // Per-pool active job tracking: pool_name → running JobIds
    let mut active_jobs: std::collections::HashMap<String, Vec<pool::JobId>> =
        std::collections::HashMap::new();

    while running.load(std::sync::atomic::Ordering::SeqCst) {
        // Write heartbeat
        let heartbeat_path = daemon_heartbeat_path();
        if let Err(e) = std::fs::write(&heartbeat_path, chrono::Utc::now().to_rfc3339()) {
            eprintln!("[boi daemon] ERROR: failed to write heartbeat: {}", e);
        }

        // Prune finished jobs across all pools
        for pool_name in registry.pool_names() {
            if let Some(pool) = registry.get(pool_name) {
                let jobs = active_jobs.entry(pool_name.to_string()).or_default();
                let mut still_running = Vec::new();
                for job_id in jobs.drain(..) {
                    match pool.status(&job_id) {
                        Ok(pool::JobStatus::Running) => still_running.push(job_id),
                        _ => {
                            let _ = pool.cleanup(&job_id);
                        }
                    }
                }
                *jobs = still_running;
            }
        }

        // Compute which pools have free capacity
        let available_pools: Vec<&str> = registry
            .pool_names()
            .into_iter()
            .filter(|name| {
                if let Some(pool) = registry.get(name) {
                    let active = active_jobs.get(*name).map_or(0, |j| j.len());
                    active < pool.max_workers() as usize
                } else {
                    false
                }
            })
            .collect();

        if !available_pools.is_empty() {
            match queue::Queue::open(db_str) {
                Ok(queue) => match queue.dequeue_for_pools(&available_pools, registry.default_name()) {
                    Ok(Some(rec)) => {
                        let spec_id = rec.id.clone();
                        let spec_path = match rec.spec_path.as_deref() {
                            Some(p) if !p.is_empty() => p.to_string(),
                            _ => {
                                eprintln!(
                                    "[boi daemon] spec {} has no spec_path — marking failed",
                                    spec_id
                                );
                                if let Ok(q2) = queue::Queue::open(db_str) {
                                    if let Err(e) = q2.update_spec(&spec_id, "failed") {
                                        eprintln!("[boi daemon] ERROR: failed to mark spec {} as failed: {}", spec_id, e);
                                    }
                                }
                                continue;
                            }
                        };

                        let pool_name = rec.worker_pool.as_deref()
                            .unwrap_or(registry.default_name())
                            .to_string();
                        let pool = match registry.resolve(rec.worker_pool.as_deref()) {
                            Some(p) => p,
                            None => {
                                eprintln!(
                                    "[boi daemon] unknown pool '{}' for spec {} — requeueing",
                                    pool_name, spec_id
                                );
                                if let Ok(q2) = queue::Queue::open(db_str) {
                                    let _ = q2.requeue(&spec_id);
                                }
                                continue;
                            }
                        };

                        // Use per-spec timeout if set, otherwise default
                        let spec_timeout = rec
                            .worker_timeout_seconds
                            .map(|t| t as u64)
                            .unwrap_or(wc.task_timeout_secs);

                        let spec_wc = worker::WorkerConfig {
                            max_workers: 1,
                            task_timeout_secs: spec_timeout,
                            retry_count: wc.retry_count,
                            cleanup_on_failure: wc.cleanup_on_failure,
                            claude_bin: wc.claude_bin.clone(),
                            models: wc.models.clone(),
                            convergence_threshold: wc.convergence_threshold,
                        };

                        eprintln!("[boi daemon] starting worker for {} on pool '{}'", spec_id, pool_name);
                        match pool.spawn(&spec_id, &spec_path, db_str, &spec_wc) {
                            Ok(job_id) => {
                                active_jobs.entry(pool_name).or_default().push(job_id);
                            }
                            Err(e) => eprintln!("[boi daemon] spawn error for {}: {}", spec_id, e),
                        }
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("[boi daemon] dequeue error: {}", e),
                },
                Err(e) => eprintln!("[boi daemon] queue open error: {}", e),
            }
        }

        // Sleep in small increments so SIGTERM is responsive
        for _ in 0..10 {
            if !running.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    eprintln!("[boi daemon] shutting down...");

    // Wait for active jobs to finish (with timeout)
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let total_active: usize = active_jobs.values().map(|j| j.len()).sum();
        if total_active == 0 || std::time::Instant::now() >= deadline {
            break;
        }
        for pool_name in registry.pool_names() {
            if let Some(pool) = registry.get(pool_name) {
                let jobs = active_jobs.entry(pool_name.to_string()).or_default();
                let mut still_running = Vec::new();
                for job_id in jobs.drain(..) {
                    match pool.status(&job_id) {
                        Ok(pool::JobStatus::Running) => still_running.push(job_id),
                        _ => {
                            let _ = pool.cleanup(&job_id);
                        }
                    }
                }
                *jobs = still_running;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    // Clean up heartbeat (lock file releases automatically when _lock_file drops)
    let _ = std::fs::remove_file(daemon_heartbeat_path()); // intentional: best-effort heartbeat cleanup

    eprintln!("[boi daemon] stopped");
}

pub fn confirm_and_destroy_running_specs(db_str: &str, yes: bool) -> bool {
    let queue = match queue::Queue::open(db_str) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("error opening queue: {}", e);
            return false;
        }
    };
    let running = match queue.list_running_specs() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error listing running specs: {}", e);
            return false;
        }
    };
    if running.is_empty() {
        return true;
    }
    println!("WARNING: The following SPECS (and ALL their tasks) will be permanently cancelled:");
    for (spec_id, title, task_id) in &running {
        let task_info = if task_id.is_empty() { String::new() } else { format!(" (running task: {})", task_id) };
        println!("  {} — {}{}", spec_id, title, task_info);
    }
    // SAFETY: isatty(0) is a standard POSIX call checking if stdin is a TTY.
    let is_tty = unsafe { libc::isatty(0) } == 1;
    if !is_tty && !yes {
        eprintln!("ERROR: --destroy-running requires --yes in non-TTY environments.");
        eprintln!("       Re-run with: boi daemon stop --destroy-running --yes");
        std::process::exit(1);
    }
    if !yes {
        print!("Cancel {} spec(s)? This cannot be undone. [y/N] ", running.len());
        use std::io::Write;
        std::io::stdout().flush().ok();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        if input.trim().to_lowercase() != "y" {
            println!("Aborted.");
            return false;
        }
    }
    for (spec_id, _, _) in &running {
        match queue.cancel(spec_id) {
            Ok(_) => eprintln!("[boi] cancelled spec {}", spec_id),
            Err(e) => eprintln!("[boi] error cancelling {}: {}", spec_id, e),
        }
    }
    true
}

pub fn cmd_stop(destroy_running: bool, yes: bool) {
    let pid = match read_daemon_pid() {
        Some(p) => p,
        None => {
            if is_daemon_locked() {
                eprintln!("daemon is running but PID unknown");
                std::process::exit(1);
            }
            eprintln!("no daemon running");
            return;
        }
    };

    if !is_pid_alive(pid) {
        eprintln!("daemon process {} is not running", pid);
        let _ = std::fs::remove_file(daemon_heartbeat_path()); // intentional: best-effort heartbeat cleanup
        return;
    }

    if destroy_running {
        let cfg = config::load();
        let db_path = cfg.db_path();
        let db_str_owned = db_path.to_str().unwrap_or("/tmp/boi.db").to_string();
        if !confirm_and_destroy_running_specs(&db_str_owned, yes) {
            return;
        }
        cleanup_pid_files(&pid_dir());
    }

    // SAFETY: `pid` was read from the daemon lock file and verified alive via
    // `is_pid_alive`. Sending SIGTERM to a valid PID is a standard POSIX operation.
    // Worst case (PID recycled): SIGTERM to an unrelated process, which is the
    // inherent race in PID-based signaling; the flock guard minimizes this window.
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    println!("sent SIGTERM to daemon (pid {})", pid);

    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if !is_pid_alive(pid) {
            println!("daemon stopped");
            return;
        }
    }

    println!("daemon still running after 10s — sending SIGKILL");
    // SAFETY: Same PID as the SIGTERM above. SIGKILL is the last-resort escalation
    // after the 10s graceful shutdown window expired.
    unsafe { libc::kill(pid as i32, libc::SIGKILL); }
    let _ = std::fs::remove_file(daemon_heartbeat_path()); // intentional: best-effort heartbeat cleanup
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::test_dir;

    #[test]
    fn test_pid_dir_returns_correct_path() {
        let dir = pid_dir();
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let expected = PathBuf::from(home).join(".boi").join("pids");
        assert_eq!(dir, expected);
    }

    #[test]
    fn test_cleanup_pid_files_removes_files() {
        let dir = test_dir("daemon-pid-cleanup");
        std::fs::write(dir.join("worker-1.pid"), "99999999").unwrap();
        std::fs::write(dir.join("worker-2.pid"), "99999998").unwrap();

        cleanup_pid_files(&dir);

        assert!(!dir.join("worker-1.pid").exists(), "worker-1.pid should be removed");
        assert!(!dir.join("worker-2.pid").exists(), "worker-2.pid should be removed");
    }
}
