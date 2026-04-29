use crate::cli::daemon::{daemon_heartbeat_path, is_daemon_locked};
use crate::config;
use crate::failure::{truncate_display, FailureReason};
use crate::fmt::{
    display_width, elapsed_since, ensure_db_dir, progress_bar, term_width, time_ago, truncate,
    BOLD, CYAN, DIM, GREEN, RED, RESET, YELLOW,
};
use crate::queue;
use serde_json::json;

/// Render a single error line for a failed spec.
/// In normal mode: one DIM RED line with short_summary, truncated to terminal width.
/// In verbose mode: multi-line DIM RED detail block.
fn render_error_line(error_text: &str, verbose: bool, width: usize) -> String {
    let reason = FailureReason::from_db(error_text);
    if verbose {
        let detail = reason.detail();
        let mut out = String::new();
        for line in detail.lines() {
            out.push_str(&format!("{}{}{}{}{}\n", DIM, RED, "    ", line, RESET));
        }
        out
    } else {
        let summary = reason.short_summary();
        let prefix = "    \u{2514}\u{2500} "; // "    └─ "
        let prefix_width: usize = 7; // 4 spaces + └ + ─ + space
        let budget = width.saturating_sub(prefix_width);
        let truncated = truncate_display(&summary, budget);
        format!("{}{}{}{}{}\n", DIM, RED, prefix, truncated, RESET)
    }
}

/// Returns an error line for a failed spec's error column, or empty string if no error.
fn maybe_render_error(error: Option<&str>, verbose: bool, width: usize) -> String {
    match error {
        Some(e) if !e.is_empty() => render_error_line(e, verbose, width),
        _ => String::new(),
    }
}

pub fn render_single_spec(q: &queue::Queue, id: &str) -> String {
    match q.status(id) {
        Ok(Some(st)) => {
            let mut out = String::new();
            let total = st.spec.total_tasks.unwrap_or(0);
            let (icon, color) = match st.spec.status.as_str() {
                "running" => ("▸", YELLOW),
                "completed" => ("✓", GREEN),
                "failed" => ("✗", RED),
                "queued" => ("◦", CYAN),
                _ => ("?", RESET),
            };

            out.push_str(&format!(
                "{}{}{} {}  {}  [{}/{}]\n",
                color, BOLD, icon, st.spec.id, st.spec.title, st.spec.completed_tasks, total
            ));
            out.push_str(&format!(
                "{}mode: {}  status: {}  iteration: {}/{}{}\n",
                RESET,
                st.spec.mode,
                st.spec.status,
                st.spec.iteration,
                st.spec.max_iterations,
                RESET
            ));
            if let Some(ref p) = st.spec.project {
                out.push_str(&format!("project: {}\n", p));
            }
            out.push('\n');

            for task in &st.tasks {
                let (ticon, tcolor) = match task.status.as_str() {
                    "DONE" => ("✓", GREEN),
                    "FAILED" => ("✗", RED),
                    "RUNNING" => ("▸", YELLOW),
                    "SKIPPED" => ("⊘", DIM),
                    _ => ("○", RESET),
                };
                out.push_str(&format!(
                    "  {}{} {:8}{} {} — {}{}\n",
                    tcolor,
                    ticon,
                    task.id,
                    RESET,
                    task.title,
                    if !task.depends.is_empty() && task.depends != "[]" {
                        format!(" {}(depends: {}){}", DIM, task.depends, RESET)
                    } else {
                        String::new()
                    },
                    ""
                ));
            }

            out
        }
        Ok(None) => format!("error: spec '{}' not found", id),
        Err(e) => format!("error: {}", e),
    }
}

fn render_status(spec_id: Option<&str>, all: bool, verbose: bool, db_str: &str) -> String {
    ensure_db_dir(db_str);

    let daemon_running = is_daemon_locked();

    let mut out = String::new();
    if !daemon_running {
        out.push_str(&format!(
            "{}{}⚠  DAEMON NOT RUNNING — queued specs will not execute.{}\n",
            BOLD, RED, RESET
        ));
        out.push_str(&format!(
            "{}   Start with: boi daemon --foreground{}\n\n",
            DIM, RESET
        ));
    }

    let q = match queue::Queue::open(db_str) {
        Ok(q) => q,
        Err(e) => return format!("error: cannot open queue: {}", e),
    };

    if let Some(id) = spec_id {
        return render_single_spec(&q, id);
    }

    let specs = match q.status_all() {
        Ok(s) => s,
        Err(e) => return format!("error: {}", e),
    };

    if specs.is_empty() {
        out.push_str("queue is empty");
        return out;
    }

    let width = term_width();
    let mut out = String::new();

    // Partition by status
    let running: Vec<&queue::SpecRecord> = specs.iter().filter(|s| s.status == "running").collect();
    let queued: Vec<&queue::SpecRecord> = specs.iter().filter(|s| s.status == "queued").collect();

    let six_hours_ago = chrono::Utc::now() - chrono::Duration::hours(6);
    let mut finished: Vec<&queue::SpecRecord> = specs
        .iter()
        .filter(|s| {
            (s.status == "completed" || s.status == "failed" || s.status == "cancelled")
                && (all
                    || s.completed_at.as_ref().is_some_and(|ts| {
                        chrono::DateTime::parse_from_rfc3339(ts)
                            .map(|dt| dt.with_timezone(&chrono::Utc) > six_hours_ago)
                            .unwrap_or(false)
                    }))
        })
        .collect();
    // Sort recently-finished by completed_at DESC (most recent first).
    // Specs without completed_at sink to the bottom.
    finished.sort_by(|a, b| b.completed_at.cmp(&a.completed_at));

    // Layout constants (display column widths, not byte widths)
    // "▸ sa7f3  " = icon(1) + space(1) + id(5) + gap(2) = 9 display cols before title
    let prefix_dcols: usize = 9; // icon(1) + space(1) + id-padded(5) + gap(2)

    // Running section
    if !running.is_empty() {
        out.push_str(&format!("{}{}RUNNING{}\n", BOLD, YELLOW, RESET));
        for s in &running {
            let total = s.total_tasks.unwrap_or(0);
            let outcomes = q.outcome_count(&s.id);
            let elapsed = s
                .started_at
                .as_deref()
                .map(elapsed_since)
                .unwrap_or_else(|| "?".to_string());

            // Build right-side stats string
            let right = if outcomes > 0 {
                format!(
                    "{}/{} \u{b7} {} outcomes  {}",
                    s.completed_tasks, total, outcomes, elapsed
                )
            } else {
                format!("{}/{}  {}", s.completed_tasks, total, elapsed)
            };
            let right_dcols = display_width(&right);

            // Title fills the space between prefix+gap and right stats
            let title_budget = width.saturating_sub(prefix_dcols + right_dcols);
            let title_str = truncate(&s.title, title_budget);
            let title_dcols = display_width(&title_str);
            let spaces = width.saturating_sub(prefix_dcols + title_dcols + right_dcols);

            out.push_str(&format!(
                "{}\u{25b8} {:<5}{}  {}{}{}\n",
                YELLOW,
                s.id,
                RESET,
                title_str,
                " ".repeat(spaces),
                right,
            ));

            // Show current task
            if let Ok(Some(st)) = q.status(&s.id) {
                if let Some(running_task) = st.tasks.iter().find(|t| t.status == "RUNNING") {
                    out.push_str(&format!(
                        "         {}\u{2192} {}: {}{}\n",
                        DIM, running_task.id, running_task.title, RESET
                    ));
                }
            }

            // Progress bar — spans from column 9 to terminal width
            let bar_width = width.saturating_sub(9);
            out.push_str(&format!(
                "         {}\n",
                progress_bar(s.completed_tasks, total, bar_width),
            ));
        }
        out.push('\n');
    }

    // Queued section
    if !queued.is_empty() {
        out.push_str(&format!("{}{}QUEUED{}\n", BOLD, CYAN, RESET));
        for s in &queued {
            let total = s.total_tasks.unwrap_or(0);
            let dep_str = s
                .depends_on
                .as_deref()
                .map(|d| format!("(after {})", d))
                .unwrap_or_default();

            let right = if dep_str.is_empty() {
                format!("0/{}", total)
            } else {
                format!("0/{}  {}", total, dep_str)
            };
            let right_dcols = display_width(&right);

            let title_budget = width.saturating_sub(prefix_dcols + right_dcols);
            let title_str = truncate(&s.title, title_budget);
            let title_dcols = display_width(&title_str);
            let spaces = width.saturating_sub(prefix_dcols + title_dcols + right_dcols);

            out.push_str(&format!(
                "{}\u{25e6} {:<5}{}  {}{}{}\n",
                CYAN,
                s.id,
                RESET,
                title_str,
                " ".repeat(spaces),
                right,
            ));
        }
        out.push('\n');
    }

    // Recently finished section
    if !finished.is_empty() {
        out.push_str(&format!("{}RECENTLY FINISHED{}\n", BOLD, RESET));
        for s in &finished {
            let total = s.total_tasks.unwrap_or(0);
            let ago = s
                .completed_at
                .as_deref()
                .map(time_ago)
                .unwrap_or_else(|| "?".to_string());

            let (icon, color) = match s.status.as_str() {
                "completed" => ("\u{2713}", GREEN),
                "failed" => ("\u{2717}", RED),
                "cancelled" => ("\u{2298}", DIM),
                _ => ("?", RESET),
            };

            let outcomes = q.outcome_count(&s.id);
            let right = if outcomes > 0 {
                format!(
                    "{}/{} \u{b7} {} outcomes  {}",
                    s.completed_tasks, total, outcomes, ago
                )
            } else {
                format!("{}/{}  {}", s.completed_tasks, total, ago)
            };
            let right_dcols = display_width(&right);

            let title_budget = width.saturating_sub(prefix_dcols + right_dcols);
            let title_str = truncate(&s.title, title_budget);
            let title_dcols = display_width(&title_str);
            let spaces = width.saturating_sub(prefix_dcols + title_dcols + right_dcols);

            out.push_str(&format!(
                "{}{} {:<5}{}  {}{}{}\n",
                color,
                icon,
                s.id,
                RESET,
                title_str,
                " ".repeat(spaces),
                right,
            ));

            if s.status == "failed" {
                out.push_str(&maybe_render_error(s.error.as_deref(), verbose, width));
            }
        }
        out.push('\n');
    }

    // Summary line — lifetime totals from DB, worker slots from config
    let busy = running.len();
    let cfg = config::load();
    let max_workers = cfg.max_workers() as usize;
    let (lifetime_failed, lifetime_completed) = q.lifetime_counts().unwrap_or((0, 0));
    let total_shown = running.len() + queued.len() + finished.len();

    out.push_str(&format!(
        "{}{}/{} busy{} | {}{}\u{2717}{}  {}{}\u{2713}{}\n",
        BOLD,
        busy,
        max_workers,
        RESET,
        RED,
        lifetime_failed,
        RESET,
        GREEN,
        lifetime_completed,
        RESET,
    ));

    if !all {
        out.push_str(&format!(
            "{}Showing {} of {} specs (running + last 6h). Use --all for full history.{}\n",
            DIM,
            total_shown,
            specs.len(),
            RESET,
        ));
    }

    // Daemon heartbeat — check pidfile-based heartbeat first, then spec updates
    let heartbeat_path = daemon_heartbeat_path();
    let mut heartbeat_warned = false;
    if heartbeat_path.exists() {
        if let Ok(ts_str) = std::fs::read_to_string(&heartbeat_path) {
            let ts_trimmed = ts_str.trim();
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts_trimmed) {
                let age = chrono::Utc::now()
                    .signed_duration_since(dt.with_timezone(&chrono::Utc))
                    .num_seconds();
                if age > 3600 {
                    out.push_str(&format!(
                        "\n{}{}Warning: Daemon may be stuck. Heartbeat is stale (last: {}).{}\n",
                        BOLD, YELLOW, ts_trimmed, RESET,
                    ));
                    heartbeat_warned = true;
                }
            }
        }
    } else if !running.is_empty() && daemon_running {
        out.push_str(&format!(
            "\n{}{}⚠  No heartbeat file — daemon may be stuck.{}\n",
            BOLD, YELLOW, RESET,
        ));
        heartbeat_warned = true;
    }

    if !heartbeat_warned {
        if let Ok(Some(ts)) = q.last_spec_update() {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&ts) {
                let age = chrono::Utc::now()
                    .signed_duration_since(dt.with_timezone(&chrono::Utc))
                    .num_seconds();
                if age > 3600 {
                    out.push_str(&format!(
                        "\n{}{}Warning: Daemon may be stuck. No spec updated in {}.{}\n",
                        BOLD,
                        YELLOW,
                        time_ago(&ts),
                        RESET,
                    ));
                }
            }
        }
    }

    out
}

pub fn cmd_status(spec_id: Option<&str>, all: bool, verbose: bool, db_str: &str) {
    println!("{}", render_status(spec_id, all, verbose, db_str));
}

pub fn cmd_status_watch(spec_id: Option<&str>, all: bool, verbose: bool, db_str: &str) {
    loop {
        // Clear screen
        print!("\x1b[2J\x1b[H");
        print!("{}", render_status(spec_id, all, verbose, db_str));
        let now = chrono::Utc::now().format("%H:%M:%S");
        println!("\n{}Updated at {} — Ctrl+C to exit{}", DIM, now, RESET);
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

pub fn cmd_status_json(spec_id: Option<&str>, all: bool, db_str: &str) {
    ensure_db_dir(db_str);
    let q = match queue::Queue::open(db_str) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("error: cannot open queue: {}", e);
            std::process::exit(1);
        }
    };

    if let Some(id) = spec_id {
        match q.status(id) {
            Ok(Some(st)) => {
                let tasks: Vec<serde_json::Value> = st
                    .tasks
                    .iter()
                    .map(|t| {
                        json!({
                            "id": t.id,
                            "title": t.title,
                            "status": t.status,
                            "depends": t.depends,
                        })
                    })
                    .collect();
                let out = json!({
                    "id": st.spec.id,
                    "title": st.spec.title,
                    "mode": st.spec.mode,
                    "status": st.spec.status,
                    "completed_tasks": st.spec.completed_tasks,
                    "total_tasks": st.spec.total_tasks,
                    "iteration": st.spec.iteration,
                    "max_iterations": st.spec.max_iterations,
                    "project": st.spec.project,
                    "tasks": tasks,
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&out)
                        .expect("json! macro output is always serializable")
                );
            }
            Ok(None) => {
                eprintln!("error: spec '{}' not found", id);
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
        }
        return;
    }

    let specs = match q.status_all() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };

    let six_hours_ago = chrono::Utc::now() - chrono::Duration::hours(6);
    let filtered: Vec<&queue::SpecRecord> = if all {
        specs.iter().collect()
    } else {
        specs
            .iter()
            .filter(|s| {
                s.status == "running"
                    || s.status == "queued"
                    || s.completed_at.as_ref().is_some_and(|ts| {
                        chrono::DateTime::parse_from_rfc3339(ts)
                            .map(|dt| dt.with_timezone(&chrono::Utc) > six_hours_ago)
                            .unwrap_or(false)
                    })
            })
            .collect()
    };

    let items: Vec<serde_json::Value> = filtered
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "title": s.title,
                "mode": s.mode,
                "status": s.status,
                "completed_tasks": s.completed_tasks,
                "total_tasks": s.total_tasks,
                "iteration": s.iteration,
                "max_iterations": s.max_iterations,
                "project": s.project,
                "queued_at": s.queued_at,
                "started_at": s.started_at,
                "completed_at": s.completed_at,
            })
        })
        .collect();

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({ "specs": items }))
            .expect("json! macro output is always serializable")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for ch in chars.by_ref() {
                    if ch == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn status_render_error_no_error_returns_empty() {
        let out = maybe_render_error(None, false, 80);
        assert!(out.is_empty(), "None error should produce empty output, got: {:?}", out);
    }

    #[test]
    fn status_render_error_empty_string_returns_empty() {
        let out = maybe_render_error(Some(""), false, 80);
        assert!(out.is_empty(), "empty error should produce empty output, got: {:?}", out);
    }

    #[test]
    fn status_render_error_typed_error_shows_short_summary() {
        let err = r#"{"ProviderRateLimit":{"provider":"anthropic","retry_after_s":null}}"#;
        let out = render_error_line(err, false, 80);
        let plain = strip_ansi(&out);
        assert!(plain.contains("\u{2514}\u{2500}"), "should contain └─: {:?}", plain);
        assert!(plain.contains("rate limited by anthropic"), "should show short summary: {:?}", plain);
    }

    #[test]
    fn status_render_error_long_error_truncated_with_ellipsis() {
        let long_msg = "x".repeat(200);
        let err = format!(r#"{{"Other":{{"message":"{}"}}}}"#, long_msg);
        // Narrow terminal of 30 cols → prefix(7) + 23 cols for summary
        let out = render_error_line(&err, false, 30);
        let plain = strip_ansi(&out);
        assert!(
            plain.contains('\u{2026}'),
            "should be truncated with ellipsis (…): {:?}",
            plain
        );
    }

    #[test]
    fn status_render_error_verbose_shows_detail() {
        let err = r#"{"ProviderHttp":{"provider":"anthropic","status":500,"body_excerpt":"internal server error"}}"#;
        let out = render_error_line(err, true, 80);
        let plain = strip_ansi(&out);
        assert!(plain.contains("ProviderHttp"), "verbose should contain ProviderHttp: {:?}", plain);
        assert!(
            plain.contains("internal server error"),
            "verbose should show body excerpt: {:?}",
            plain
        );
    }
}
