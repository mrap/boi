use crate::config;
use crate::fmt::{ensure_db_dir, is_pid_alive, BOLD, CYAN, DIM, GREEN, RED, RESET, YELLOW};
use crate::queue;

pub fn cmd_workers(db_str: &str, cfg: &config::Config) {
    let worktrees_dir = cfg.worktrees_dir();

    // Try to get worker records from DB
    ensure_db_dir(db_str);
    if let Ok(q) = queue::Queue::open(db_str) {
        if let Ok(workers) = q.get_workers() {
            if !workers.is_empty() {
                println!("{}{}WORKERS{}", BOLD, CYAN, RESET);
                for w in &workers {
                    let status = if let Some(ref sid) = w.current_spec_id {
                        let pid_alive = w
                            .current_pid
                            .map(|p| is_pid_alive(p as u32))
                            .unwrap_or(false);
                        if pid_alive {
                            format!(
                                "{}active{} — {} (pid {})",
                                GREEN,
                                RESET,
                                sid,
                                w.current_pid.unwrap_or(0)
                            )
                        } else {
                            format!("{}stale{} — {} (process gone)", YELLOW, RESET, sid)
                        }
                    } else {
                        format!("{}idle{}", DIM, RESET)
                    };
                    let wt = w
                        .worktree_path
                        .as_deref()
                        .unwrap_or("(no worktree)");
                    println!("  {:12}  {}  {}{}{}", w.id, status, DIM, wt, RESET);
                }
                return;
            }
        }
    }

    // Fallback: scan worktrees directory
    if !worktrees_dir.exists() {
        println!("no worktrees directory found at {}", worktrees_dir.display());
        return;
    }

    let mut entries: Vec<_> = match std::fs::read_dir(&worktrees_dir) {
        Ok(e) => e.filter_map(|e| e.ok()).collect(),
        Err(e) => {
            eprintln!("error reading worktrees dir: {}", e);
            return;
        }
    };
    entries.sort_by_key(|e| e.file_name());

    if entries.is_empty() {
        println!("no active worktrees");
        return;
    }

    println!("{}{}WORKTREES{}", BOLD, CYAN, RESET);
    for entry in &entries {
        let name = entry.file_name();
        let path = entry.path();
        let has_git = path.join(".git").exists();
        let status = if has_git {
            format!("{}active{}", GREEN, RESET)
        } else {
            format!("{}stale{}", RED, RESET)
        };
        println!(
            "  {:<20}  {}  {}",
            name.to_string_lossy(),
            status,
            path.display()
        );
    }
}
