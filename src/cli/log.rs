use crate::config::Config;
use crate::fmt::{ensure_db_dir, format_duration_ms, BOLD, CYAN, DIM, GREEN, RED, RESET, YELLOW};
use crate::telemetry::{LogLevel, Telemetry};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

fn render_phase_runs_table(telemetry: &Telemetry, spec_id: &str, full: bool) {
    let runs = telemetry.phase_runs_by_spec(spec_id);
    if runs.is_empty() {
        return;
    }

    println!("\n{}{}Phase invocations{}\n", BOLD, CYAN, RESET);

    if !full {
        println!(
            "  {:<20} {:<12} {:<28} {:<9} {:<10}",
            "PHASE", "RUNTIME", "MODEL", "DURATION", "COST"
        );
        println!("  {}", "-".repeat(82));
        for r in &runs {
            let phase = &r.phase_name;
            let runtime = r.runtime.as_deref().unwrap_or("?");
            let model = r.model.as_deref().unwrap_or("?");
            let duration = r.duration_ms
                .map(format_duration_ms)
                .unwrap_or_else(|| "—".to_string());
            let cost = r.cost_usd
                .map(|c| format!("${:.4}", c))
                .unwrap_or_else(|| "—".to_string());
            let exit_color = match r.exit_status.as_deref() {
                Some("success") => GREEN,
                Some("timeout") | Some("nonzero") | Some("crashed") => RED,
                _ => DIM,
            };
            println!(
                "  {}{:<20}{} {:<12} {:<28} {:<9} {:<10}",
                exit_color, phase, RESET, runtime, model, duration, cost
            );
        }
    } else {
        for (i, r) in runs.iter().enumerate() {
            if i > 0 { println!("  {}", "-".repeat(60)); }
            let phase_color = match r.exit_status.as_deref() {
                Some("success") => GREEN,
                Some(_) => RED,
                None => YELLOW,
            };
            println!("  {}phase:         {}{}{}", BOLD, phase_color, r.phase_name, RESET);
            println!("  inv_id:        {}{}{}", DIM, r.invocation_id, RESET);
            if let Some(ref v) = r.task_id        { println!("  task_id:       {}", v); }
            if let Some(ref v) = r.phase_level    { println!("  level:         {}", v); }
            if let Some(ref v) = r.mode           { println!("  mode:          {}", v); }
            if let Some(ref v) = r.runtime        { println!("  runtime:       {}", v); }
            if let Some(ref v) = r.model          { println!("  model:         {}", v); }
            if let Some(ref v) = r.effort         { println!("  effort:        {}", v); }
            if let Some(v) = r.thinking_enabled   { println!("  thinking:      {}", v); }
            if let Some(v) = r.thinking_budget_tokens { println!("  think_tokens:  {}", v); }
            if let Some(v) = r.extended_thinking  { println!("  ext_thinking:  {}", v); }
            if let Some(ref v) = r.prompt_template_path { println!("  prompt_tmpl:   {}", v); }
            if let Some(v) = r.prompt_length_chars  { println!("  prompt_chars:  {}", v); }
            if let Some(v) = r.prompt_length_tokens { println!("  prompt_tokens: {}", v); }
            if let Some(v) = r.timeout_secs       { println!("  timeout:       {}s", v); }
            println!("  bare_flag:     {}", r.bare_flag);
            if let Some(ref v) = r.brain_dir      { println!("  brain_dir:     {}", v); }
            if let Some(ref v) = r.api_key_env_used { println!("  api_key_env:   {}", v); }
            if let Some(ref args) = r.cli_args    { println!("  cli_args:      {:?}", args); }
            if let Some(ref v) = r.http_endpoint  { println!("  http_endpoint: {}", v); }
            if let Some(ref v) = r.started_at     { println!("  started_at:    {}", v); }
            if let Some(ref v) = r.completed_at   { println!("  completed_at:  {}", v); }
            if let Some(v) = r.duration_ms  { println!("  duration:      {}", format_duration_ms(v)); }
            if let Some(v) = r.startup_ms   { println!("  startup_ms:    {}", v); }
            if let Some(v) = r.inference_ms { println!("  inference_ms:  {}", v); }
            if let Some(v) = r.input_tokens         { println!("  in_tokens:     {}", v); }
            if let Some(v) = r.output_tokens        { println!("  out_tokens:    {}", v); }
            if let Some(v) = r.cache_read_tokens    { println!("  cache_read:    {}", v); }
            if let Some(v) = r.cache_creation_tokens{ println!("  cache_create:  {}", v); }
            if let Some(v) = r.cost_usd { println!("  cost_usd:      ${:.6}", v); }
            if let Some(ref v) = r.exit_status  { println!("  exit_status:   {}", v); }
            if let Some(ref v) = r.exit_reason  { println!("  exit_reason:   {}", v); }
            if let Some(v) = r.retry_index      { println!("  retry_index:   {}", v); }
            if let Some(ref v) = r.branch_sha   { println!("  branch_sha:    {}", v); }
            if let Some(ref v) = r.host_os      { println!("  host_os:       {}", v); }
            if let Some(ref v) = r.host_arch    { println!("  host_arch:     {}", v); }
            if let Some(ref v) = r.daemon_version { println!("  daemon_ver:    {}", v); }
        }
    }
    println!();
}

pub fn cmd_log(spec_id: &str, full: bool, debug: bool, follow: bool, db_str: &str, cfg: &Config) {
    if follow {
        cmd_log_follow(spec_id, cfg);
        return;
    }

    ensure_db_dir(db_str);
    let telemetry = Telemetry::new(std::path::PathBuf::from(db_str));

    let events = telemetry.by_spec(spec_id);
    if events.is_empty() {
        println!("no events found for {}", spec_id);
        return;
    }

    let min_level = if debug { LogLevel::Debug } else { LogLevel::Info };

    println!(
        "{}{}Events for {}{}\n",
        BOLD, CYAN, spec_id, RESET
    );

    for event in &events {
        let event_level = LogLevel::from_str(&event.level);
        if event_level < min_level {
            continue;
        }

        let ts = format_timestamp(&event.timestamp);
        let level_indicator = level_prefix(event_level);
        let msg = event.message.as_deref().unwrap_or(&event.event_type);

        println!("  {} {} {}", ts, level_indicator, msg);

        if debug {
            if let Some(ref data) = event.data {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(data) {
                    match event.event_type.as_str() {
                        "boi.claude.exit" => {
                            if let Some(output_len) = parsed.get("output_length") {
                                let duration = parsed.get("duration_ms")
                                    .and_then(|v| v.as_i64())
                                    .map(format_duration_ms)
                                    .unwrap_or_else(|| "?".to_string());
                                let exit_code = parsed.get("exit_code")
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(-1);
                                println!(
                                    "       {}exit={}, output={} chars, duration={}{}",
                                    DIM, exit_code, output_len, duration, RESET
                                );
                            }
                        }
                        "boi.verify.result" => {
                            let passed = parsed.get("passed")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            let cmd = parsed.get("verify_cmd")
                                .and_then(|v| v.as_str())
                                .unwrap_or("?");
                            let indicator = if passed {
                                format!("{}passed{}", GREEN, RESET)
                            } else {
                                format!("{}failed{}", RED, RESET)
                            };
                            println!(
                                "       {}cmd: {} — {}{}",
                                DIM, cmd, indicator, RESET
                            );
                        }
                        "boi.phase.outcome" => {
                            if let Some(duration_ms) = parsed.get("duration_ms").and_then(|v| v.as_i64()) {
                                let outcome = parsed.get("outcome")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                println!(
                                    "       {}outcome={}, duration={}{}",
                                    DIM, outcome, format_duration_ms(duration_ms), RESET
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    println!();

    render_phase_runs_table(&telemetry, spec_id, full);
}

fn cmd_log_follow(spec_id: &str, cfg: &Config) {
    let logs_dir = cfg.logs_dir();
    let path = match find_latest_daemon_log(&logs_dir) {
        Some(p) => p,
        None => {
            eprintln!("no daemon log file found in {}", logs_dir.display());
            return;
        }
    };

    println!("{}{}following {} (spec: {}){}",
        BOLD, CYAN, path.display(), spec_id, RESET);

    let mut file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error opening {}: {}", path.display(), e);
            return;
        }
    };

    let mut pos: u64 = 0;

    loop {
        let file_len = match file.seek(SeekFrom::End(0)) {
            Ok(n) => n,
            Err(_) => pos,
        };
        if file_len > pos && file.seek(SeekFrom::Start(pos)).is_ok() {
            let mut buf = Vec::new();
            if file.read_to_end(&mut buf).is_ok() {
                pos += buf.len() as u64;
                let content = String::from_utf8_lossy(&buf);
                for line in content.lines() {
                    if line.contains(spec_id) {
                        println!("{}", line);
                    }
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

pub fn find_latest_daemon_log(logs_dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(logs_dir).ok()?;
    let mut candidates: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("daemon-") && n.ends_with(".log"))
                .unwrap_or(false)
        })
        .collect();
    candidates.sort();
    candidates.into_iter().last()
}

fn format_timestamp(ts: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        format!("{}{}{}", DIM, dt.format("%H:%M:%S"), RESET)
    } else {
        format!("{}{}{}", DIM, &ts[..19.min(ts.len())], RESET)
    }
}

fn level_prefix(level: LogLevel) -> String {
    match level {
        LogLevel::Debug => format!("{}[dbg]{}", DIM, RESET),
        LogLevel::Info => format!("{}[inf]{}", GREEN, RESET),
        LogLevel::Warn => format!("{}[wrn]{}", YELLOW, RESET),
        LogLevel::Error => format!("{}[ERR]{}", RED, RESET),
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils;
    use std::fs;

    #[test]
    fn test_find_latest_daemon_log_empty_dir() {
        let dir = test_utils::test_dir("log_follow_empty");
        assert!(find_latest_daemon_log(&dir).is_none());
    }

    #[test]
    fn test_find_latest_daemon_log_ignores_non_daemon_files() {
        let dir = test_utils::test_dir("log_follow_nonmatching");
        fs::write(dir.join("other-file.txt"), b"x").unwrap();
        fs::write(dir.join("worker-20260429.log"), b"x").unwrap();
        assert!(find_latest_daemon_log(&dir).is_none());
    }

    #[test]
    fn test_find_latest_daemon_log_picks_most_recent() {
        let dir = test_utils::test_dir("log_follow_latest");
        let f1 = dir.join("daemon-20260101-100000.log");
        let f2 = dir.join("daemon-20260429-120000.log");
        fs::write(&f1, b"old content").unwrap();
        fs::write(&f2, b"new content").unwrap();

        let result = find_latest_daemon_log(&dir);
        assert_eq!(result, Some(f2));
    }

    #[test]
    fn test_find_latest_daemon_log_single_file() {
        let dir = test_utils::test_dir("log_follow_single");
        let f = dir.join("daemon-20260429-090000.log");
        fs::write(&f, b"data").unwrap();

        let result = find_latest_daemon_log(&dir);
        assert_eq!(result, Some(f));
    }
}
