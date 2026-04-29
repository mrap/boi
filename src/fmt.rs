// BOI formatting utilities
// ─── ANSI constants ───────────────────────────────────────────────────────────

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const RED: &str = "\x1b[31m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const CYAN: &str = "\x1b[36m";

// ─── formatting utilities ─────────────────────────────────────────────────────

pub fn progress_bar(completed: i64, total: i64, width: usize) -> String {
    if total == 0 {
        return format!("{}░{}", DIM, RESET).repeat(width);
    }
    let pct = (completed as f64 / total as f64).min(1.0);
    let filled = (pct * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);
    format!(
        "{}{}{}{}{}{}",
        GREEN,
        "█".repeat(filled),
        DIM,
        "░".repeat(empty),
        RESET,
        ""
    )
}

pub fn time_ago(ts: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        let now = chrono::Utc::now();
        let dur = now.signed_duration_since(dt.with_timezone(&chrono::Utc));
        let secs = dur.num_seconds();
        if secs < 60 {
            format!("{}s", secs)
        } else if secs < 3600 {
            format!("{}m ago", secs / 60)
        } else if secs < 86400 {
            format!("{}h ago", secs / 3600)
        } else {
            format!("{}d ago", secs / 86400)
        }
    } else {
        ts.to_string()
    }
}

pub fn elapsed_since(ts: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        let now = chrono::Utc::now();
        let dur = now.signed_duration_since(dt.with_timezone(&chrono::Utc));
        let secs = dur.num_seconds();
        if secs < 60 {
            format!("{}s", secs)
        } else if secs < 3600 {
            format!("{}m", secs / 60)
        } else {
            format!("{}h", secs / 3600)
        }
    } else {
        "?".to_string()
    }
}

pub fn term_width() -> usize {
    // SAFETY: `ws` is zeroed and passed by mutable reference to `ioctl`. File descriptor 1
    // (stdout) is always valid in a running process. `TIOCGWINSZ` only reads terminal
    // dimensions into the provided `winsize` struct; it cannot cause UB. On failure (e.g.
    // stdout is not a terminal), we fall back to 110 columns.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            ws.ws_col as usize
        } else {
            110
        }
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let truncated: String = chars[..max.saturating_sub(1)].iter().collect();
        format!("{}…", truncated)
    }
}

/// Count display width of a string (each char = 1 column, ignoring ANSI escapes).
/// This is a simple approximation that works for ASCII + common Unicode symbols.
pub fn display_width(s: &str) -> usize {
    s.chars().count()
}

pub fn is_pid_alive(pid: u32) -> bool {
    // SAFETY: `kill(pid, 0)` is a POSIX existence check. Signal 0 is never delivered;
    // it only tests whether the caller has permission to signal `pid` and the process
    // exists. The `pid as i32` cast is safe because valid PIDs fit in i32.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

pub fn ensure_db_dir(db_str: &str) {
    if let Some(parent) = std::path::Path::new(db_str).parent() {
        let _ = std::fs::create_dir_all(parent); // intentional: best-effort dir creation; Queue::open reports actual error
    }
}
