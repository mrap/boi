use crate::{config, queue, spec};
use serde_json::json;
use std::io::Write as _;
use std::time::{Duration, Instant};

/// Base64 decode — mirrors the simple encoder in bench.rs.
fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    const TABLE: [i8; 256] = {
        let mut t = [-1i8; 256];
        let chars = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut i = 0usize;
        while i < 64 {
            t[chars[i] as usize] = i as i8;
            i += 1;
        }
        t
    };

    let input = input.trim();
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let bytes = input.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        let a = TABLE[bytes[i] as usize];
        let b = TABLE[bytes[i + 1] as usize];
        let c = TABLE[bytes[i + 2] as usize];
        let d = TABLE[bytes[i + 3] as usize];
        if a < 0 || b < 0 {
            return Err(format!("invalid base64 char at pos {i}"));
        }
        out.push(((a << 2) | (b >> 4)) as u8);
        if bytes[i + 2] != b'=' {
            if c < 0 {
                return Err(format!("invalid base64 char at pos {}", i + 2));
            }
            out.push(((b << 4) | (c >> 2)) as u8);
        }
        if bytes[i + 3] != b'=' {
            if d < 0 {
                return Err(format!("invalid base64 char at pos {}", i + 3));
            }
            out.push(((c << 2) | d) as u8);
        }
        i += 4;
    }
    Ok(out)
}

/// Run a single spec from BOI_SPEC_B64 env, wait for completion, emit JSON result.
/// This is the container entrypoint for `boi bench --remote=fly` dispatch.
pub fn cmd_run_spec() {
    let spec_b64 = match std::env::var("BOI_SPEC_B64") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("error: BOI_SPEC_B64 environment variable is not set");
            std::process::exit(1);
        }
    };

    let spec_bytes = match decode_base64(&spec_b64) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: failed to decode BOI_SPEC_B64: {e}");
            std::process::exit(1);
        }
    };

    let spec_content = match String::from_utf8(spec_bytes) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: BOI_SPEC_B64 is not valid UTF-8: {e}");
            std::process::exit(1);
        }
    };

    let boi_spec = match spec::parse(&spec_content) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: spec validation failed: {e}");
            std::process::exit(1);
        }
    };

    // Write spec to a temp file so dispatch can reference it
    let temp_path = format!("/tmp/boi-run-spec-{}.yaml", std::process::id());
    if let Err(e) = std::fs::write(&temp_path, &spec_content) {
        eprintln!("error: cannot write temp spec to {temp_path}: {e}");
        std::process::exit(1);
    }

    eprintln!("[run-spec] dispatching: {}", boi_spec.title);
    let _ = std::io::stderr().flush();

    let db_path = config::load().db_path();
    let db_str = db_path.to_str().unwrap_or("/tmp/boi-run-spec.db");

    crate::fmt::ensure_db_dir(db_str);

    let q = match queue::Queue::open(db_str) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("error: cannot open queue at {db_str}: {e}");
            std::process::exit(1);
        }
    };

    let spec_id = match q.enqueue_with_context(&boi_spec, Some(&temp_path), None) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("error: enqueue failed: {e}");
            std::process::exit(1);
        }
    };

    eprintln!("[run-spec] spec_id={spec_id} — waiting for daemon to process...");
    let _ = std::io::stderr().flush();

    let start = Instant::now();
    let timeout = Duration::from_secs(7200);
    let poll_interval = Duration::from_secs(5);

    loop {
        if start.elapsed() > timeout {
            eprintln!("[run-spec] timeout: spec did not complete within {}s", timeout.as_secs());
            emit_result("timeout", 0, 0, 0, 0, None, None, None);
            std::process::exit(1);
        }

        let status = match q.status(&spec_id) {
            Ok(Some(s)) => s,
            Ok(None) => {
                std::thread::sleep(poll_interval);
                continue;
            }
            Err(e) => {
                eprintln!("[run-spec] status query error: {e}");
                std::thread::sleep(poll_interval);
                continue;
            }
        };

        let spec_status = status.spec.status.as_str();
        match spec_status {
            "completed" | "failed" | "cancelled" | "timeout" => {
                let tasks_total = status.tasks.len() as i64;
                let tasks_done = status.tasks.iter().filter(|t| t.status == "DONE").count() as i64;
                let tasks_failed =
                    status.tasks.iter().filter(|t| t.status == "FAILED").count() as i64;
                let tasks_skipped =
                    status.tasks.iter().filter(|t| t.status == "SKIPPED").count() as i64;

                let (cost, input_tokens, output_tokens) =
                    q.aggregate_spec_cost(&spec_id).unwrap_or((None, None, None));

                eprintln!("[run-spec] done: status={spec_status} tasks={tasks_done}/{tasks_total}");
                let _ = std::io::stderr().flush();

                emit_result(
                    spec_status,
                    tasks_total,
                    tasks_done,
                    tasks_failed,
                    tasks_skipped,
                    cost,
                    input_tokens,
                    output_tokens,
                );

                if spec_status == "completed" {
                    std::process::exit(0);
                } else {
                    std::process::exit(1);
                }
            }
            _ => {
                std::thread::sleep(poll_interval);
            }
        }
    }
}

fn emit_result(
    status: &str,
    tasks_total: i64,
    tasks_done: i64,
    tasks_failed: i64,
    tasks_skipped: i64,
    cost: Option<f64>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
) {
    let result = json!({
        "status": status,
        "tasks_total": tasks_total,
        "tasks_done": tasks_done,
        "tasks_failed": tasks_failed,
        "tasks_skipped": tasks_skipped,
        "total_cost_usd": cost,
        "total_input_tokens": input_tokens,
        "total_output_tokens": output_tokens,
    });
    println!("{result}");
    let _ = std::io::stdout().flush();
}
