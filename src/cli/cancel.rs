use crate::fmt::ensure_db_dir;
use crate::{hooks, queue, worker};
use serde_json::json;

pub fn cmd_cancel(spec_id: &str, db_str: &str, hook_cfg: &hooks::HookConfig) {
    ensure_db_dir(db_str);
    let q = match queue::Queue::open(db_str) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("error: cannot open queue: {}", e);
            std::process::exit(1);
        }
    };

    match q.status(spec_id) {
        Ok(None) => {
            eprintln!("error: spec '{}' not found", spec_id);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
        Ok(Some(_)) => {}
    }

    // Kill the Claude subprocess if it's running
    let pid_path = worker::pid_file_for(spec_id);
    if pid_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&pid_path) {
            if let Ok(pid) = content.trim().parse::<i32>() {
                eprintln!("[boi] killing claude subprocess (pid {})", pid);
                // SAFETY: `pid` was read from the spec's PID file written by `spawn_claude`.
                // Sending SIGTERM to request graceful shutdown of a known child process.
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
                // Wait briefly for graceful shutdown
                for _ in 0..10 {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    // SAFETY: `kill(pid, 0)` is a POSIX existence check -- signal 0 is
                    // never delivered; it only tests whether the process exists.
                    unsafe {
                        if libc::kill(pid, 0) != 0 {
                            break;
                        }
                    }
                }
                // Force kill if still alive
                // SAFETY: Same PID as above. SIGKILL is the last-resort escalation after
                // the 2s graceful shutdown window. The kill(pid, 0) check confirms the
                // process still exists before sending SIGKILL.
                unsafe {
                    if libc::kill(pid, 0) == 0 {
                        eprintln!("[boi] claude pid {} still alive — sending SIGKILL", pid);
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
        }
        let _ = std::fs::remove_file(&pid_path); // intentional: best-effort pid file cleanup
    }

    if let Err(e) = q.cancel(spec_id) {
        eprintln!("error: cancel failed: {}", e);
        std::process::exit(1);
    }

    // Do NOT clean up the worktree — preserve for inspection
    eprintln!("[boi] worktree preserved for inspection");

    let payload = json!({ "spec_id": spec_id });
    let _ = hooks::fire(hook_cfg, hooks::ON_CANCEL, &payload); // intentional: best-effort hook notification

    println!("cancelled {}", spec_id);
}
