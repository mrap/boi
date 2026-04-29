use crate::fmt::{ensure_db_dir, is_pid_alive};
use crate::spawn::pid_dir;
use crate::telemetry::Telemetry;
use crate::{config, hooks, queue, worker};
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

    // SIGHUP hot-reload flag: set to true by signal_hook when SIGHUP arrives.
    let reload_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Err(e) = signal_hook::flag::register(
        signal_hook::consts::SIGHUP,
        std::sync::Arc::clone(&reload_flag),
    ) {
        eprintln!("[boi daemon] WARNING: failed to install SIGHUP handler: {}", e);
    }

    let mut wc = worker::WorkerConfig {
        max_workers: cfg.max_workers(),
        spawns_per_tick: cfg.spawns_per_tick(),
        task_timeout_secs: cfg.task_timeout_secs(),
        retry_count: cfg.retry_count(),
        cleanup_on_failure: cfg.cleanup_on_failure(),
        claude_bin: cfg.claude_bin(),
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

    let active: std::sync::Arc<std::sync::Mutex<Vec<std::thread::JoinHandle<()>>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    while running.load(std::sync::atomic::Ordering::SeqCst) {
        // Write heartbeat
        let heartbeat_path = daemon_heartbeat_path();
        if let Err(e) = std::fs::write(&heartbeat_path, chrono::Utc::now().to_rfc3339()) {
            eprintln!("[boi daemon] ERROR: failed to write heartbeat: {}", e);
        }

        // SIGHUP hot-reload: only max_workers, spawns_per_tick, claude_bin are live-updated.
        // All other settings remain frozen at startup. In-flight workers keep their original config.
        if reload_flag.swap(false, std::sync::atomic::Ordering::SeqCst) {
            match config::try_load() {
                Ok(new_cfg) => {
                    apply_reload(&mut wc, &new_cfg);
                    eprintln!(
                        "[boi daemon] reloaded config: max_workers={}, spawns_per_tick={}, claude_bin={}",
                        wc.max_workers, wc.spawns_per_tick, wc.claude_bin
                    );
                }
                Err(e) => eprintln!("[boi daemon] reload FAILED: {}; keeping current config", e),
            }
        }

        {
            let mut workers = active.lock().unwrap_or_else(|e| {
                eprintln!("[boi daemon] worker mutex poisoned, recovering: {}", e);
                e.into_inner()
            });
            workers.retain(|h| !h.is_finished());

            let to_spawn = compute_to_spawn(workers.len(), wc.max_workers, wc.spawns_per_tick);

            for slot in 0..to_spawn {
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
                                    continue; // skip to next batch slot
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
                            eprintln!(
                                "[boi daemon] starting worker for {} (batch slot {}/{})",
                                spec_id,
                                slot + 1,
                                to_spawn
                            );
                            let handle = std::thread::spawn(move || {
                                let wc = worker::WorkerConfig {
                                    max_workers: 1,
                                    spawns_per_tick: 1,
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

                            // Micro-jitter between successive spawns to smooth cold-start burst
                            if slot + 1 < to_spawn {
                                let jitter_ns = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.subsec_nanos() as u64)
                                    .unwrap_or(0);
                                let jitter_ms = 50 + (jitter_ns % 101);
                                std::thread::sleep(std::time::Duration::from_millis(jitter_ms));
                            }
                        }
                        Ok(None) => break, // queue drained
                        Err(e) => {
                            eprintln!("[boi daemon] dequeue error: {}", e);
                            break;
                        }
                    },
                    Err(e) => {
                        eprintln!("[boi daemon] queue open error: {}", e);
                        break;
                    }
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

    // Kill all setsid'd Claude subprocesses before stopping the daemon (F-04)
    cleanup_pid_files(&pid_dir());

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

/// How many workers to spawn this tick: capped by capacity and per-tick limit.
pub(crate) fn compute_to_spawn(workers_len: usize, max_workers: u32, spawns_per_tick: u32) -> u32 {
    let cap_remaining = max_workers.saturating_sub(workers_len as u32);
    cap_remaining.min(spawns_per_tick)
}

/// Hot-reload the three live-mutable fields from a freshly parsed config.
/// All other WorkerConfig fields remain at their startup values.
pub(crate) fn apply_reload(wc: &mut worker::WorkerConfig, new_cfg: &config::Config) {
    wc.max_workers = new_cfg.max_workers();
    wc.spawns_per_tick = new_cfg.spawns_per_tick();
    wc.claude_bin = new_cfg.claude_bin();
}

/// Send SIGHUP to the running daemon so it picks up config changes.
pub fn cmd_reload() {
    let pid = match read_daemon_pid() {
        Some(p) => p,
        None => {
            eprintln!("no daemon running (PID file not found)");
            std::process::exit(1);
        }
    };

    if !crate::fmt::is_pid_alive(pid) {
        eprintln!("daemon process {} is not running", pid);
        std::process::exit(1);
    }

    // SAFETY: `pid` was read from the daemon lock file and verified alive above.
    // SIGHUP to a known-live PID is a standard POSIX config-reload signal.
    unsafe { libc::kill(pid as i32, libc::SIGHUP) };
    println!("sent SIGHUP to daemon (pid {}); config will reload within one tick", pid);
}

#[cfg(test)]
mod daemon_batch {
    use super::*;
    use crate::{queue, spec, test_utils};

    const SIMPLE_SPEC: &str = "title: \"Batch Test\"\ntasks:\n  - id: t-1\n    title: \"Step\"\n    status: PENDING\n    spec: \"Do it\"\n";

    fn open_queue(label: &str) -> (queue::Queue, String) {
        let db_file = test_utils::test_file(label, "db");
        let _ = std::fs::remove_file(&db_file);
        let db_path = db_file.to_str().unwrap().to_string();
        let q = queue::Queue::open(&db_path).unwrap();
        (q, db_path)
    }

    fn enqueue_n(q: &queue::Queue, n: usize) {
        let boi_spec = spec::parse(SIMPLE_SPEC).unwrap();
        for _ in 0..n {
            q.enqueue(&boi_spec, None).unwrap();
        }
    }

    fn drain_n(q: &queue::Queue, to_spawn: u32) -> usize {
        let mut count = 0;
        for _ in 0..to_spawn {
            match q.dequeue() {
                Ok(Some(_)) => count += 1,
                Ok(None) => break,
                Err(_) => break,
            }
        }
        count
    }

    #[test]
    fn test_compute_to_spawn_at_capacity() {
        // workers_len == max_workers → 0 slots remaining
        assert_eq!(compute_to_spawn(4, 4, 4), 0);
    }

    #[test]
    fn test_compute_to_spawn_limited_by_spawns_per_tick() {
        // cap_remaining=8 but spawns_per_tick=4 → 4
        assert_eq!(compute_to_spawn(0, 8, 4), 4);
    }

    #[test]
    fn test_compute_to_spawn_limited_by_cap_remaining() {
        // cap_remaining=2, spawns_per_tick=4 → 2
        assert_eq!(compute_to_spawn(6, 8, 4), 2);
    }

    #[test]
    fn test_empty_queue_zero_spawns() {
        let (q, _db) = open_queue("batch-empty");
        let to_spawn = compute_to_spawn(0, 4, 4);
        let spawned = drain_n(&q, to_spawn);
        assert_eq!(spawned, 0);
    }

    #[test]
    fn test_one_eligible_cap4_tick4_spawns_one() {
        let (q, _db) = open_queue("batch-one");
        enqueue_n(&q, 1);
        let to_spawn = compute_to_spawn(0, 4, 4); // = 4
        let spawned = drain_n(&q, to_spawn);
        assert_eq!(spawned, 1, "only 1 item in queue, expect 1 spawn");
    }

    #[test]
    fn test_six_eligible_cap4_tick4_spawns_four_then_two() {
        let (q, _db) = open_queue("batch-six-cap4");
        enqueue_n(&q, 6);
        let to_spawn = compute_to_spawn(0, 4, 4); // = 4
        let first_tick = drain_n(&q, to_spawn);
        assert_eq!(first_tick, 4, "first tick: 4 spawned");

        // Second tick: 2 remain
        let to_spawn2 = compute_to_spawn(4, 8, 4); // simulate 4 workers running, max=8
        let second_tick = drain_n(&q, to_spawn2);
        assert_eq!(second_tick, 2, "second tick: remaining 2 spawned");
    }

    #[test]
    fn test_six_eligible_cap8_tick4_spawns_four() {
        let (q, _db) = open_queue("batch-six-cap8");
        enqueue_n(&q, 6);
        let to_spawn = compute_to_spawn(0, 8, 4); // = 4 (tick limit)
        let spawned = drain_n(&q, to_spawn);
        assert_eq!(spawned, 4);
    }

    #[test]
    fn test_four_eligible_cap2_tick4_spawns_two() {
        let (q, _db) = open_queue("batch-four-cap2");
        enqueue_n(&q, 4);
        let to_spawn = compute_to_spawn(6, 8, 4); // cap_remaining=2, tick=4 → 2
        let spawned = drain_n(&q, to_spawn);
        assert_eq!(spawned, 2);
    }
}

#[cfg(test)]
mod daemon_hotreload {
    use super::*;
    use crate::{config, test_utils, worker};

    fn make_wc(max_workers: u32, spawns_per_tick: u32, claude_bin: &str) -> worker::WorkerConfig {
        worker::WorkerConfig {
            max_workers,
            spawns_per_tick,
            task_timeout_secs: 1800,
            retry_count: 3,
            cleanup_on_failure: false,
            claude_bin: claude_bin.to_string(),
        }
    }

    #[test]
    fn test_apply_reload_updates_hot_fields() {
        let mut wc = make_wc(4, 2, "claude");
        let new_cfg = config::Config {
            max_workers: Some(8),
            spawns_per_tick: Some(6),
            claude_bin: Some("/usr/bin/claude".to_string()),
            ..Default::default()
        };
        apply_reload(&mut wc, &new_cfg);
        assert_eq!(wc.max_workers, 8);
        assert_eq!(wc.spawns_per_tick, 6);
        assert_eq!(wc.claude_bin, "/usr/bin/claude");
    }

    #[test]
    fn test_apply_reload_leaves_other_fields_unchanged() {
        let mut wc = make_wc(4, 2, "claude");
        wc.task_timeout_secs = 7200;
        wc.retry_count = 5;
        let new_cfg = config::Config {
            max_workers: Some(8),
            ..Default::default()
        };
        apply_reload(&mut wc, &new_cfg);
        assert_eq!(wc.task_timeout_secs, 7200, "task_timeout_secs must not change on reload");
        assert_eq!(wc.retry_count, 5, "retry_count must not change on reload");
    }

    #[test]
    fn test_bad_config_returns_err() {
        use std::io::Write;
        let path = test_utils::test_file("hotreload-bad-config", "yaml");
        let mut f = std::fs::File::create(&path).unwrap();
        // Deliberately invalid YAML
        f.write_all(b"max_workers: [this is: not: valid yaml\n").unwrap();
        let result = config::try_load_from(&path);
        assert!(result.is_err(), "invalid YAML should return Err, got: {:?}", result);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_missing_config_returns_defaults() {
        let path = test_utils::test_file("hotreload-missing", "yaml");
        let _ = std::fs::remove_file(&path);
        let cfg = config::try_load_from(&path)
            .expect("missing config file should return Ok with defaults");
        assert_eq!(cfg.max_workers(), 5);
        assert_eq!(cfg.spawns_per_tick(), 4);
    }

    #[test]
    fn test_noop_reload_same_values() {
        // Default config → default wc values; apply_reload is a no-op
        let mut wc = make_wc(5, 4, "claude");
        let same_cfg = config::Config::default();
        apply_reload(&mut wc, &same_cfg);
        assert_eq!(wc.max_workers, 5);
        assert_eq!(wc.spawns_per_tick, 4);
        assert_eq!(wc.claude_bin, "claude");
    }

    #[test]
    fn test_bad_config_keeps_original_wc() {
        use std::io::Write;
        let mut wc = make_wc(8, 3, "my-claude");
        let path = test_utils::test_file("hotreload-bad-keep", "yaml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"max_workers: [broken\n").unwrap();
        // Simulate what the daemon does: if load fails, don't call apply_reload
        if let Ok(new_cfg) = config::try_load_from(&path) {
            apply_reload(&mut wc, &new_cfg);
        }
        // Values must be unchanged
        assert_eq!(wc.max_workers, 8, "max_workers must be retained on bad config");
        assert_eq!(wc.spawns_per_tick, 3, "spawns_per_tick must be retained on bad config");
        assert_eq!(wc.claude_bin, "my-claude", "claude_bin must be retained on bad config");
        let _ = std::fs::remove_file(&path);
    }
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
