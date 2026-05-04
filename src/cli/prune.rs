use std::collections::HashSet;
use std::io::IsTerminal;

use crate::cli::daemon::read_daemon_pid;
use crate::fmt::{ensure_db_dir, is_pid_alive, truncate, BOLD, CYAN, DIM, GREEN, RED, RESET, YELLOW};
use crate::queue;

// ─── Data structures ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub cmdline: String,
    pub cwd: Option<String>,
    pub has_tty: bool,
    pub cpu_percent: f64,
    pub mem_rss_kb: u64,
    pub alive_secs: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PruneReason {
    NotInWorkerRegistry,
    DbMarkedEnded,
    InactiveWorktree { path: String },
    LongIdle { secs: u64 },
    OrphanedProcess { alive_secs: u64 },
}

impl PruneReason {
    pub fn label(&self) -> &'static str {
        match self {
            PruneReason::NotInWorkerRegistry => "not-in-registry",
            PruneReason::DbMarkedEnded => "db-marked-ended",
            PruneReason::InactiveWorktree { .. } => "inactive-worktree",
            PruneReason::LongIdle { .. } => "long-idle",
            PruneReason::OrphanedProcess { .. } => "orphaned",
        }
    }

    pub fn description(&self) -> String {
        match self {
            PruneReason::NotInWorkerRegistry => "alive but not in workers.current_pid".into(),
            PruneReason::DbMarkedEnded => "DB ended_at set but PID still alive".into(),
            PruneReason::InactiveWorktree { path } => {
                format!("CWD is inactive BOI worktree: {}", path)
            }
            PruneReason::LongIdle { secs } => format!("0% CPU for ≥{}s", secs),
            PruneReason::OrphanedProcess { alive_secs } => {
                format!("parent dead, alive {}s", alive_secs)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct OrphanCandidate {
    pub proc: ProcessInfo,
    pub reasons: Vec<PruneReason>,
}

#[derive(Debug, Default)]
pub struct DbState {
    pub worker_pids: HashSet<u32>,
    pub active_process_pids: HashSet<u32>,
    pub ended_process_pids: HashSet<u32>,
}

// ─── DB loading ───────────────────────────────────────────────────────────────

pub fn load_db_state(db_str: &str) -> Result<DbState, String> {
    ensure_db_dir(db_str);
    let q = queue::Queue::open(db_str).map_err(|e| format!("DB open failed: {e}"))?;
    let worker_pids = q
        .get_worker_pids()
        .map_err(|e| format!("worker query failed: {e}"))?;
    let (active_pids, ended_pids) = q
        .get_process_pids()
        .map_err(|e| format!("process query failed: {e}"))?;
    Ok(DbState {
        worker_pids: worker_pids.into_iter().collect(),
        active_process_pids: active_pids.into_iter().collect(),
        ended_process_pids: ended_pids.into_iter().collect(),
    })
}

// ─── Candidate filtering logic (pure, testable) ───────────────────────────────

const DAEMON_SAFELIST: &[&str] = &[
    "claude-mem",
    "claude/remote/server",
    "claude-compare/server.py",
];

/// Returns true if `cmdline` matches any entry in the long-running daemon safelist.
fn is_safelist_daemon(cmdline: &str) -> bool {
    DAEMON_SAFELIST.iter().any(|pat| cmdline.contains(pat))
}

/// Returns true if the process looks like an interactive Claude session that must not
/// be pruned: either it has a controlling TTY, or it started "claude" with the
/// dangerously-skip-permissions flag (which only interactive sessions use).
pub fn is_interactive_claude(proc: &ProcessInfo) -> bool {
    let is_claude_cmd = proc.cmdline.starts_with("claude")
        || proc.cmdline.contains("/claude ")
        || proc.cmdline.contains("/claude\0")  // null-sep variants
        || proc.cmdline.ends_with("/claude");
    if !is_claude_cmd {
        return false;
    }
    proc.has_tty || proc.cmdline.contains("--dangerously-skip-permissions")
}

/// Returns true if the process matches any exclude pattern (substring match).
pub fn matches_exclude_pattern(cmdline: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pat| cmdline.contains(pat.as_str()))
}

/// Returns true if the process's CWD is a BOI worktree pattern path.
pub fn is_boi_worktree_path(path: &str) -> bool {
    // Matches /private/tmp/.../boi-*-boi-rust/ or /tmp/.../boi-*-boi-rust/
    (path.contains("/tmp/") || path.contains("/private/tmp/"))
        && path.contains("boi-")
        && path.contains("-boi-rust")
}

/// Returns true if the process cmdline looks like a tail -f on BOI worktree paths.
fn is_tail_on_boi_path(cmdline: &str) -> bool {
    if !cmdline.starts_with("tail") {
        return false;
    }
    cmdline.contains("-f")
        && (cmdline.contains("/tmp/") || cmdline.contains("/private/tmp/"))
        && cmdline.contains("boi-")
}

/// Returns true if the cmdline targets "claude" processes or BOI worktree-related
/// processes that are candidates for orphan detection.
pub fn is_candidate_process(proc: &ProcessInfo) -> bool {
    let cmd = &proc.cmdline;
    let has_claude = cmd.contains("claude");
    let has_boi_cwd = proc
        .cwd
        .as_deref()
        .map(is_boi_worktree_path)
        .unwrap_or(false);
    let is_tail = is_tail_on_boi_path(cmd);
    has_claude || has_boi_cwd || is_tail
}

/// Evaluate the 5 strong heuristics. Returns the matching reasons (at least one
/// is required for the process to be flagged as an orphan).
pub fn check_heuristics(
    proc: &ProcessInfo,
    db_state: &DbState,
    alive_pids: &HashSet<u32>,
    max_idle_secs: u64,
) -> Vec<PruneReason> {
    let mut reasons = Vec::new();

    // Heuristic 1: alive but not in workers.current_pid
    if !db_state.worker_pids.is_empty() && !db_state.worker_pids.contains(&proc.pid) {
        reasons.push(PruneReason::NotInWorkerRegistry);
    }

    // Heuristic 2: DB thinks the process ended but it's still alive
    if db_state.ended_process_pids.contains(&proc.pid) {
        reasons.push(PruneReason::DbMarkedEnded);
    }

    // Heuristic 3: CWD is an inactive BOI worktree
    if let Some(ref cwd) = proc.cwd {
        if is_boi_worktree_path(cwd) && !std::path::Path::new(cwd).exists() {
            reasons.push(PruneReason::InactiveWorktree { path: cwd.clone() });
        }
    }

    // Heuristic 4: 0 CPU for >= N seconds
    // Approximated: if cpu% is 0 and the process has been alive >= max_idle_secs,
    // we treat it as idle for that duration (conservative approximation).
    if proc.cpu_percent < 0.01 && proc.alive_secs >= max_idle_secs {
        reasons.push(PruneReason::LongIdle { secs: proc.alive_secs });
    }

    // Heuristic 5: parent process is dead AND process has been alive >= N seconds
    let parent_dead = proc.ppid == 0 || !alive_pids.contains(&proc.ppid);
    if parent_dead && proc.alive_secs >= max_idle_secs {
        reasons.push(PruneReason::OrphanedProcess {
            alive_secs: proc.alive_secs,
        });
    }

    reasons
}

/// Core classification function. Returns Some(reasons) if the process is an orphan
/// candidate, or None if it should be protected/skipped.
pub fn classify_candidate(
    proc: &ProcessInfo,
    db_state: &DbState,
    alive_pids: &HashSet<u32>,
    max_idle_secs: u64,
    exclude_patterns: &[String],
) -> Option<Vec<PruneReason>> {
    // Never touch processes in workers.current_pid
    if db_state.worker_pids.contains(&proc.pid) {
        return None;
    }
    // Never touch processes with active (ended_at IS NULL) DB records
    if db_state.active_process_pids.contains(&proc.pid) {
        return None;
    }
    // Never touch safelist daemons
    if is_safelist_daemon(&proc.cmdline) {
        return None;
    }
    // Never touch interactive claude sessions
    if is_interactive_claude(proc) {
        return None;
    }
    // Never touch processes matching user exclusion patterns
    if matches_exclude_pattern(&proc.cmdline, exclude_patterns) {
        return None;
    }
    // Evaluate heuristics
    let reasons = check_heuristics(proc, db_state, alive_pids, max_idle_secs);
    if reasons.is_empty() {
        None
    } else {
        Some(reasons)
    }
}

/// Find all orphan candidates from a list of processes.
pub fn find_orphan_candidates(
    processes: &[ProcessInfo],
    db_state: &DbState,
    alive_pids: &HashSet<u32>,
    max_idle_secs: u64,
    exclude_patterns: &[String],
) -> Vec<OrphanCandidate> {
    processes
        .iter()
        .filter(|p| is_candidate_process(p))
        .filter_map(|p| {
            classify_candidate(p, db_state, alive_pids, max_idle_secs, exclude_patterns)
                .map(|reasons| OrphanCandidate {
                    proc: p.clone(),
                    reasons,
                })
        })
        .collect()
}

// ─── macOS process enumeration ────────────────────────────────────────────────

/// Parse `ps` etime field (`[[DD-]HH:]MM:SS`) to seconds.
fn parse_etime(s: &str) -> u64 {
    let s = s.trim();
    let (days, rest) = if let Some(idx) = s.find('-') {
        let d: u64 = s[..idx].parse().unwrap_or(0);
        (d, &s[idx + 1..])
    } else {
        (0, s)
    };
    let parts: Vec<&str> = rest.split(':').collect();
    match parts.len() {
        3 => {
            let h: u64 = parts[0].parse().unwrap_or(0);
            let m: u64 = parts[1].parse().unwrap_or(0);
            let sec: u64 = parts[2].parse().unwrap_or(0);
            days * 86400 + h * 3600 + m * 60 + sec
        }
        2 => {
            let m: u64 = parts[0].parse().unwrap_or(0);
            let sec: u64 = parts[1].parse().unwrap_or(0);
            days * 86400 + m * 60 + sec
        }
        _ => days * 86400,
    }
}

/// Try to get the CWD of a process via `lsof -p <pid> -a -d cwd -Fn`.
fn get_process_cwd(pid: u32) -> Option<String> {
    let out = std::process::Command::new("lsof")
        .args([
            "-p",
            &pid.to_string(),
            "-a",
            "-d",
            "cwd",
            "-Fn",
            "-w",
        ])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    // lsof -Fn outputs lines like:
    //   p<pid>
    //   n<path>
    for line in stdout.lines() {
        if let Some(path) = line.strip_prefix('n') {
            return Some(path.to_string());
        }
    }
    None
}

/// Collect running processes from `ps`. Only returns processes relevant to BOI
/// (cmdline contains "claude", cmdline contains "tail -f", or similar heuristics).
/// CWD is fetched lazily only for matching processes (expensive lsof call).
pub fn collect_system_processes() -> Vec<ProcessInfo> {
    let out = match std::process::Command::new("ps")
        .args(["-A", "-ww", "-o", "pid=,ppid=,pcpu=,rss=,etime=,tty=,args="])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut procs = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Fields: pid ppid pcpu rss etime tty args...
        let mut parts = line.splitn(7, ' ').map(str::trim);
        let pid: u32 = match parts.next().and_then(|s| s.parse().ok()) {
            Some(v) => v,
            None => continue,
        };
        let ppid: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let cpu: f64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let rss: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let etime_str = parts.next().unwrap_or("0:00");
        let tty_str = parts.next().unwrap_or("??");
        let cmdline = parts.next().unwrap_or("").to_string();

        if cmdline.is_empty() {
            continue;
        }

        let has_tty = tty_str != "??" && !tty_str.is_empty();
        let alive_secs = parse_etime(etime_str);

        // Only fetch CWD for processes that might be relevant (lsof is expensive)
        let cwd = if cmdline.contains("claude") || is_tail_on_boi_path(&cmdline) {
            get_process_cwd(pid)
        } else {
            None
        };

        procs.push(ProcessInfo {
            pid,
            ppid,
            cmdline,
            cwd,
            has_tty,
            cpu_percent: cpu,
            mem_rss_kb: rss,
            alive_secs,
        });
    }

    procs
}

// ─── Output formatting ────────────────────────────────────────────────────────

fn format_age(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86400, (secs % 86400) / 3600)
    }
}

fn print_table(candidates: &[OrphanCandidate]) {
    if candidates.is_empty() {
        println!("{}No orphan candidates found.{}", GREEN, RESET);
        return;
    }
    println!(
        "{}{}{:<7} {:<8} {:<6} {:<8} {:<20} {}{}",
        BOLD, CYAN, "PID", "AGE", "CPU%", "MEM(KB)", "REASON", "CMD", RESET
    );
    for c in candidates {
        let reason = c.reasons.first().map(|r| r.label()).unwrap_or("-");
        let cmd = truncate(&c.proc.cmdline, 50);
        println!(
            "{:<7} {:<8} {:<6.1} {:<8} {:<20} {}{}{}",
            c.proc.pid,
            format_age(c.proc.alive_secs),
            c.proc.cpu_percent,
            c.proc.mem_rss_kb,
            reason,
            DIM,
            cmd,
            RESET,
        );
    }
}

fn print_json(candidates: &[OrphanCandidate]) {
    let items: Vec<serde_json::Value> = candidates
        .iter()
        .map(|c| {
            serde_json::json!({
                "pid": c.proc.pid,
                "ppid": c.proc.ppid,
                "cmdline": c.proc.cmdline,
                "cwd": c.proc.cwd,
                "has_tty": c.proc.has_tty,
                "cpu_percent": c.proc.cpu_percent,
                "mem_rss_kb": c.proc.mem_rss_kb,
                "alive_secs": c.proc.alive_secs,
                "reasons": c.reasons.iter().map(|r| {
                    serde_json::json!({
                        "label": r.label(),
                        "description": r.description(),
                    })
                }).collect::<Vec<_>>(),
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&items).unwrap_or_default());
}

// ─── Apply (signal sending) ───────────────────────────────────────────────────

fn send_signal(pid: u32, sig: libc::c_int) -> bool {
    // SAFETY: kill(2) with a valid positive PID and a standard signal number is safe.
    unsafe { libc::kill(pid as libc::pid_t, sig) == 0 }
}

fn apply_prune(candidates: &[OrphanCandidate], db_str: &str) {
    let q = match queue::Queue::open(db_str) {
        Ok(q) => Some(q),
        Err(e) => {
            eprintln!("{}warning: could not open DB to update ended_at: {}{}", YELLOW, e, RESET);
            None
        }
    };

    let mut survivors: Vec<u32> = Vec::new();

    for c in candidates {
        let pid = c.proc.pid;
        if send_signal(pid, libc::SIGTERM) {
            println!("  {}→{} SIGTERM sent to pid {}", YELLOW, RESET, pid);
        } else {
            println!("  {}→{} SIGTERM failed for pid {} (already gone?)", DIM, RESET, pid);
        }
        survivors.push(pid);
    }

    if survivors.is_empty() {
        return;
    }

    println!("Waiting 10s for processes to exit…");
    std::thread::sleep(std::time::Duration::from_secs(10));

    for pid in &survivors {
        if is_pid_alive(*pid) {
            println!(
                "  {}→{} SIGKILL sent to pid {} (did not exit after SIGTERM)",
                RED, RESET, pid
            );
            send_signal(*pid, libc::SIGKILL);
        } else if let Some(ref q) = q {
            let _ = q.mark_process_ended_by_pid(*pid);
        }
    }
}

// ─── Main command entry point ─────────────────────────────────────────────────

pub struct PruneConfig {
    pub dry_run: bool,
    pub apply: bool,
    pub yes: bool,
    pub force: bool,
    pub max_idle_secs: u64,
    pub exclude_patterns: Vec<String>,
    pub json: bool,
}

pub fn cmd_prune_orphans(cfg: &PruneConfig, db_str: &str) {
    // Build DB state
    let db_state = match load_db_state(db_str) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}error: {}{}", RED, e, RESET);
            if !cfg.force {
                eprintln!(
                    "Cannot determine protected PIDs. Use --force to override this safety check."
                );
                std::process::exit(1);
            }
            DbState::default()
        }
    };

    // Safety: refuse to prune if the protected set is empty and --force not given
    if db_state.worker_pids.is_empty()
        && db_state.active_process_pids.is_empty()
        && !cfg.force
    {
        eprintln!(
            "{}warning: protected set is empty (no active workers or processes in DB).{}",
            YELLOW, RESET
        );
        eprintln!("This is unusual — use --force to override the safety check.");
        std::process::exit(1);
    }

    // Add daemon PID to protected set
    let daemon_pid = read_daemon_pid();

    // Enumerate all system processes
    let all_procs = collect_system_processes();

    // Build alive PID set for parent-dead check
    let alive_pids: HashSet<u32> = all_procs.iter().map(|p| p.pid).collect();

    // Filter: exclude daemon and self
    let self_pid = std::process::id();
    let filtered: Vec<&ProcessInfo> = all_procs
        .iter()
        .filter(|p| {
            Some(p.pid) != daemon_pid && p.pid != self_pid
        })
        .collect();

    let owned: Vec<ProcessInfo> = filtered.into_iter().cloned().collect();

    // Find orphan candidates
    let candidates = find_orphan_candidates(
        &owned,
        &db_state,
        &alive_pids,
        cfg.max_idle_secs,
        &cfg.exclude_patterns,
    );

    // Output
    if cfg.json {
        print_json(&candidates);
    } else {
        let mode = if cfg.dry_run {
            format!("{}dry-run{}", YELLOW, RESET)
        } else {
            format!("{}apply{}", RED, RESET)
        };
        println!(
            "{}{}BOI prune-orphans{} [{}]  max-idle={}s\n",
            BOLD,
            CYAN,
            RESET,
            mode,
            cfg.max_idle_secs,
        );
        print_table(&candidates);
        println!();
    }

    if candidates.is_empty() {
        return;
    }

    if cfg.apply {
        // Require --yes in non-TTY contexts
        let stdout_is_tty = std::io::stdout().is_terminal();
        if !stdout_is_tty && !cfg.yes {
            eprintln!(
                "{}error: --apply in non-TTY context requires --yes{}", RED, RESET
            );
            std::process::exit(1);
        }
        if !cfg.yes && stdout_is_tty {
            eprint!(
                "Kill {} candidate(s)? [y/N] ",
                candidates.len()
            );
            let mut buf = String::new();
            std::io::stdin().read_line(&mut buf).ok();
            if !buf.trim().eq_ignore_ascii_case("y") {
                println!("Aborted.");
                return;
            }
        }
        apply_prune(&candidates, db_str);
    } else if !cfg.json {
        println!(
            "{}(dry-run — pass --apply to kill){}", DIM, RESET
        );
    }
}
