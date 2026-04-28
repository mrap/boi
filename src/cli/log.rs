use crate::fmt::{ensure_db_dir, BOLD, CYAN, DIM, GREEN, RED, RESET, YELLOW};
use crate::telemetry::{LogLevel, Telemetry};

pub fn cmd_log(spec_id: &str, _full: bool, debug: bool, db_str: &str) {
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

        // In debug mode, show extra data for certain event types
        if debug {
            if let Some(ref data) = event.data {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(data) {
                    // Show relevant fields based on event type
                    match event.event_type.as_str() {
                        "boi.claude.exit" => {
                            if let Some(output_len) = parsed.get("output_length") {
                                let duration = parsed.get("duration_ms")
                                    .and_then(|v| v.as_u64())
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
                            if let Some(duration_ms) = parsed.get("duration_ms").and_then(|v| v.as_u64()) {
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
}

fn format_timestamp(ts: &str) -> String {
    // Parse RFC3339 and show HH:MM:SS
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

fn format_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{}ms", ms)
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{:.1}m", ms as f64 / 60_000.0)
    }
}
