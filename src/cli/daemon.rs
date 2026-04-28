use crate::fmt::{ensure_db_dir, is_pid_alive};
use crate::{config, hooks, queue, worker};
use std::path::PathBuf;

pub fn daemon_pid_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".boi").join("daemon.pid")
}

pub fn daemon_heartbeat_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".boi").join("daemon.heartbeat")
}

pub fn check_existing_daemon(pid_path: &std::path::Path) -> bool {
    if let Ok(content) = std::fs::read_to_string(pid_path) {
        if let Ok(pid) = content.trim().parse::<i32>() {
            unsafe { libc::kill(pid, 0) == 0 }
        } else {
            false
        }
    } else {
        false
    }
}

pub fn cmd_daemon(db_str: &str, hook_cfg: hooks::HookConfig, cfg: &config::Config) {
    ensure_db_dir(db_str);

    // Singleton guard: check if a daemon is already running
    let pid_path = daemon_pid_path();
    if check_existing_daemon(&pid_path) {
        if let Ok(content) = std::fs::read_to_string(&pid_path) {
            eprintln!(
                "[boi daemon] error: daemon already running (pid {})",
                content.trim()
            );
        } else {
            eprintln!("[boi daemon] error: daemon already running");
        }
        std::process::exit(1);
    }

    // Write PID file
    let pid = std::process::id();
    if let Some(parent) = pid_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&pid_path, pid.to_string()).unwrap_or_else(|e| {
        eprintln!("warning: could not write PID file: {}", e);
    });

    // Register SIGTERM handler via atomic flag
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let r = running.clone();

    // Install SIGTERM + SIGINT handler
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

    eprintln!("[boi daemon] started (pid {}), max_workers={}", pid, wc.max_workers);

    let active: std::sync::Arc<std::sync::Mutex<Vec<std::thread::JoinHandle<()>>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    while running.load(std::sync::atomic::Ordering::SeqCst) {
        // Write heartbeat
        let heartbeat_path = daemon_heartbeat_path();
        let _ = std::fs::write(&heartbeat_path, chrono::Utc::now().to_rfc3339());

        {
            let mut workers = active.lock().unwrap();
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
                                        let _ = q2.update_spec(&spec_id, "failed");
                                    }
                                    continue;
                                }
                            };
                            let qpath = db_str.to_string();
                            let hc = hook_cfg.clone();
                            let timeout = wc.task_timeout_secs;
                            let retries = wc.retry_count;

                            // Use per-spec timeout if set, otherwise default
                            let spec_timeout = rec
                                .worker_timeout_seconds
                                .map(|t| t as u64)
                                .unwrap_or(timeout);

                            eprintln!("[boi daemon] starting worker for {}", spec_id);
                            let handle = std::thread::spawn(move || {
                                let wc = worker::WorkerConfig {
                                    max_workers: 1,
                                    task_timeout_secs: spec_timeout,
                                    retry_count: retries,
                                };
                                if let Err(e) =
                                    worker::run_worker(&spec_id, &spec_path, &qpath, &hc, &wc)
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
        let mut workers = active.lock().unwrap();
        while !workers.is_empty() && std::time::Instant::now() < deadline {
            workers.retain(|h| !h.is_finished());
            if !workers.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }

    // Clean up pidfile and heartbeat
    let _ = std::fs::remove_file(&pid_path);
    let _ = std::fs::remove_file(daemon_heartbeat_path());

    eprintln!("[boi daemon] stopped");
}

pub fn cmd_stop() {
    let pid_path = daemon_pid_path();

    if !pid_path.exists() {
        eprintln!("no daemon PID file found at {}", pid_path.display());
        std::process::exit(1);
    }

    let pid_str = match std::fs::read_to_string(&pid_path) {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            eprintln!("error reading PID file: {}", e);
            std::process::exit(1);
        }
    };

    let pid: u32 = match pid_str.parse() {
        Ok(p) => p,
        Err(_) => {
            eprintln!("invalid PID in file: {}", pid_str);
            std::process::exit(1);
        }
    };

    if !is_pid_alive(pid) {
        eprintln!("daemon process {} is not running (stale PID file)", pid);
        let _ = std::fs::remove_file(&pid_path);
        let _ = std::fs::remove_file(daemon_heartbeat_path());
        return;
    }

    // Send SIGTERM
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }

    println!("sent SIGTERM to daemon (pid {})", pid);

    // Wait briefly for graceful shutdown
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if !is_pid_alive(pid) {
            println!("daemon stopped");
            return;
        }
    }

    println!("daemon still running after 10s — sending SIGKILL");
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    let _ = std::fs::remove_file(&pid_path);
    let _ = std::fs::remove_file(daemon_heartbeat_path());
}
