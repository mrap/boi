use crate::cli::daemon::{daemon_heartbeat_path, is_daemon_locked, read_daemon_pid};
use crate::config;
use crate::fmt::{ensure_db_dir, time_ago, BOLD, CYAN, GREEN, RED, RESET, YELLOW};
use crate::queue;

pub fn cmd_doctor(db_str: &str, cfg: &config::Config) {
    let mut issues = 0;

    println!("{}{}BOI Doctor{}\n", BOLD, CYAN, RESET);

    // 1. Daemon running?
    if is_daemon_locked() {
        if let Some(pid) = read_daemon_pid() {
            println!("  {}✓{} Daemon running (pid {})", GREEN, RESET, pid);
        } else {
            println!("  {}✓{} Daemon running (pid unknown)", GREEN, RESET);
        }
    } else {
        println!("  {}⊘{} Daemon not running", YELLOW, RESET);
    }

    // 2. Heartbeat fresh?
    let hb_path = daemon_heartbeat_path();
    if hb_path.exists() {
        if let Ok(ts_str) = std::fs::read_to_string(&hb_path) {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts_str.trim()) {
                let age = chrono::Utc::now()
                    .signed_duration_since(dt.with_timezone(&chrono::Utc))
                    .num_seconds();
                if age < 30 {
                    println!("  {}✓{} Heartbeat fresh ({}s ago)", GREEN, RESET, age);
                } else if age < 3600 {
                    println!("  {}⊘{} Heartbeat {}s ago", YELLOW, RESET, age);
                } else {
                    println!(
                        "  {}✗{} Heartbeat stale ({})",
                        RED,
                        RESET,
                        time_ago(ts_str.trim())
                    );
                    issues += 1;
                }
            }
        }
    } else {
        println!("  {}⊘{} No heartbeat file", YELLOW, RESET);
    }

    // 3. DB accessible?
    ensure_db_dir(db_str);
    match queue::Queue::open(db_str) {
        Ok(q) => {
            println!("  {}✓{} Database accessible ({})", GREEN, RESET, db_str);

            // Count specs
            match q.status_all() {
                Ok(specs) => {
                    let running = specs.iter().filter(|s| s.status == "running").count();
                    let queued = specs.iter().filter(|s| s.status == "queued").count();
                    let completed = specs.iter().filter(|s| s.status == "completed").count();
                    let failed = specs.iter().filter(|s| s.status == "failed").count();
                    println!(
                        "  {}✓{} Queue: {} running, {} queued, {} completed, {} failed",
                        GREEN, RESET, running, queued, completed, failed
                    );
                }
                Err(e) => {
                    println!("  {}✗{} Cannot query specs: {}", RED, RESET, e);
                    issues += 1;
                }
            }
        }
        Err(e) => {
            println!("  {}✗{} Database error: {}", RED, RESET, e);
            issues += 1;
        }
    }

    // 4. Worktrees healthy?
    let wt_dir = cfg.worktrees_dir();
    if wt_dir.exists() {
        match std::fs::read_dir(&wt_dir) {
            Ok(entries) => {
                let entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
                let stale = entries
                    .iter()
                    .filter(|e| !e.path().join(".git").exists())
                    .count();
                if stale == 0 {
                    println!(
                        "  {}✓{} Worktrees: {} active, 0 stale",
                        GREEN,
                        RESET,
                        entries.len()
                    );
                } else {
                    println!(
                        "  {}⊘{} Worktrees: {} active, {} stale (run cleanup)",
                        YELLOW,
                        RESET,
                        entries.len() - stale,
                        stale
                    );
                }
            }
            Err(e) => {
                println!("  {}✗{} Cannot read worktrees: {}", RED, RESET, e);
                issues += 1;
            }
        }
    } else {
        println!("  {}✓{} No worktrees directory (clean)", GREEN, RESET);
    }

    // 5. Config valid?
    let config_path = config::default_config_path();
    if config_path.exists() {
        match std::fs::read_to_string(&config_path) {
            Ok(content) => match serde_yml::from_str::<config::Config>(&content) {
                Ok(_) => println!("  {}✓{} Config valid ({})", GREEN, RESET, config_path.display()),
                Err(e) => {
                    println!(
                        "  {}✗{} Config parse error: {}",
                        RED, RESET, e
                    );
                    issues += 1;
                }
            },
            Err(e) => {
                println!("  {}✗{} Cannot read config: {}", RED, RESET, e);
                issues += 1;
            }
        }
    } else {
        println!("  {}✓{} Using default config (no config file)", GREEN, RESET);
    }

    println!();
    if issues == 0 {
        println!("{}All checks passed.{}", GREEN, RESET);
    } else {
        println!("{}{} issue(s) found.{}", RED, issues, RESET);
    }
}
