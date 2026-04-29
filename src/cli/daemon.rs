use crate::fmt::{ensure_db_dir, is_pid_alive};
use crate::telemetry::Telemetry;
use crate::{config, hooks, queue, worker};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

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

pub fn cmd_restart() {
    if is_daemon_locked() {
        cmd_stop();
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    cmd_start();
}

pub fn daemon_pid_path() -> PathBuf {
    daemon_lock_path()
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
    };

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

    let active: std::sync::Arc<std::sync::Mutex<Vec<std::thread::JoinHandle<()>>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    while running.load(std::sync::atomic::Ordering::SeqCst) {
        // Write heartbeat
        let heartbeat_path = daemon_heartbeat_path();
        if let Err(e) = std::fs::write(&heartbeat_path, chrono::Utc::now().to_rfc3339()) {
            eprintln!("[boi daemon] ERROR: failed to write heartbeat: {}", e);
        }

        {
            let mut workers = active.lock().unwrap_or_else(|e| {
                eprintln!("[boi daemon] worker mutex poisoned, recovering: {}", e);
                e.into_inner()
            });
            workers.retain(|h| !h.is_finished());

            if workers.len() < wc.max_workers as usize {
                match queue::Queue::open(db_str) {
                    Ok(queue) => match queue.dequeue() {
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
                            let qpath = db_str.to_string();
                            let hc = hook_cfg.clone();
                            let timeout = wc.task_timeout_secs;
                            let retries = wc.retry_count;
                            let cleanup_fail = wc.cleanup_on_failure;
                            let cbin = wc.claude_bin.clone();

                            // Use per-spec timeout if set, otherwise default
                            let spec_timeout = rec
                                .worker_timeout_seconds
                                .map(|t| t as u64)
                                .unwrap_or(timeout);

                            let tel = Telemetry::new(PathBuf::from(&qpath));
                            eprintln!("[boi daemon] starting worker for {}", spec_id);
                            let handle = std::thread::spawn(move || {
                                let wc = worker::WorkerConfig {
                                    max_workers: 1,
                                    task_timeout_secs: spec_timeout,
                                    retry_count: retries,
                                    cleanup_on_failure: cleanup_fail,
                                    claude_bin: cbin,
                                };
                                if let Err(e) =
                                    worker::run_worker(&spec_id, &spec_path, &qpath, &hc, &wc, &tel)
                                {
                                    eprintln!("[boi daemon] worker error for {}: {}", spec_id, e);
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

        // Sleep in small increments so SIGTERM is responsive
        for _ in 0..10 {
            if !running.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    eprintln!("[boi daemon] shutting down...");

    // Wait for workers to finish (with timeout)
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    {
        let mut workers = active.lock().unwrap_or_else(|e| {
            eprintln!("[boi daemon] worker mutex poisoned during shutdown: {}", e);
            e.into_inner()
        });
        while !workers.is_empty() && std::time::Instant::now() < deadline {
            workers.retain(|h| !h.is_finished());
            if !workers.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }

    // Clean up heartbeat (lock file releases automatically when _lock_file drops)
    let _ = std::fs::remove_file(daemon_heartbeat_path()); // intentional: best-effort heartbeat cleanup

    eprintln!("[boi daemon] stopped");
}

pub fn cmd_stop() {
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
