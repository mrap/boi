use chrono::Utc;
use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

macro_rules! boi_log {
    ($($arg:tt)*) => {
        eprintln!("[boi {}] {}", Utc::now().format("%H:%M:%S"), format!($($arg)*))
    };
}

pub struct ClaudeResult {
    pub success: bool,
    pub output: String,
    pub stderr: String,
    pub startup_ms: u64,
    pub inference_ms: u64,
    pub total_ms: u64,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub tool_call_count: i64,
    pub tool_calls_by_type: std::collections::HashMap<String, i64>,
}

pub fn pid_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(home).join(".boi").join("pids")
}

pub fn pid_file_for(spec_id: &str) -> std::path::PathBuf {
    pid_dir().join(format!("{}.pid", spec_id))
}

/// Spawn claude with the task prompt. Returns ClaudeResult with timing data.
/// startup_ms = time from spawn to first stdout byte.
/// inference_ms = time from first byte to process exit.
/// Respects timeout: kills the process and returns failure if exceeded.
/// The `claude_bin` parameter specifies the claude binary path.
/// If `spec_id` is provided, writes the child PID to ~/.boi/pids/{spec_id}.pid
/// so that `boi cancel` can kill it.
pub fn spawn_claude(
    prompt: &str,
    worktree_path: &str,
    timeout_secs: u64,
    model: Option<&str>,
    spec_id: Option<&str>,
    claude_bin: &str,
) -> Result<ClaudeResult, Box<dyn std::error::Error>> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;

    let mut args = vec![
        "-p".to_string(), prompt.to_string(),
        "--dangerously-skip-permissions".to_string(),
        "--no-session-persistence".to_string(),
        "--setting-sources".to_string(), "user".to_string(),
        "--output-format".to_string(), "stream-json".to_string(),
        "--verbose".to_string(),
        "--strict-mcp-config".to_string(),
    ];
    if let Some(m) = model {
        args.push("--model".to_string());
        args.push(m.to_string());
    }
    let args_display: Vec<&str> = args.iter().skip(2).map(|s| s.as_str()).collect();
    boi_log!("spawning claude\n  bin:    {}\n  args:   {}\n  cwd:    {}\n  prompt: {} chars",
        claude_bin, args_display.join(" "), worktree_path, prompt.len());

    let mut cmd = Command::new(claude_bin);
    cmd.args(&args)
        .current_dir(worktree_path)
        .env("AGENT_DIR", worktree_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: setsid() is async-signal-safe per POSIX, safe to call after fork before exec.
    // This puts the child in its own process group so we can kill all grandchildren via -pgid.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;

    // Store process group ID for killing grandchildren on exit/timeout
    let pgid = child.id() as i32;

    // Write PID file so `boi cancel` can kill this subprocess
    let pid_path = spec_id.map(|sid| {
        let p = pid_file_for(sid);
        if let Some(parent) = p.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("[boi] ERROR: failed to create pid dir {}: {}", parent.display(), e);
            }
        }
        let child_pid = child.id();
        if let Err(e) = std::fs::write(&p, child_pid.to_string()) {
            eprintln!("[boi] ERROR: failed to write pid file {}: {}", p.display(), e);
        }
        boi_log!(" wrote pid {} to {}", child_pid, p.display());
        p
    });

    let spawn_time = Instant::now();
    let stdout_pipe = child.stdout.take().expect("stdout was piped");
    let stderr_pipe = child.stderr.take().expect("stderr was piped");

    let stderr_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Err(e) = std::io::BufReader::new(stderr_pipe).read_to_string(&mut buf) {
            eprintln!("[boi] ERROR: failed to read claude stderr: {}", e);
        }
        buf
    });

    let reader_handle = std::thread::spawn(move || {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(stdout_pipe);
        let mut first_byte_time: Option<Instant> = None;
        let mut last_output = String::new();
        let mut raw_lines = Vec::new();
        let mut input_tokens: Option<i64> = None;
        let mut output_tokens: Option<i64> = None;
        let mut cache_read_tokens: Option<i64> = None;
        let mut cache_creation_tokens: Option<i64> = None;
        let mut cost_usd: Option<f64> = None;
        let mut tool_call_count: i64 = 0;
        let mut tool_calls_by_type = std::collections::HashMap::<String, i64>::new();

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if first_byte_time.is_none() {
                first_byte_time = Some(Instant::now());
            }
            if let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) {
                let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match event_type {
                    "assistant" => {
                        if let Some(content) = event.pointer("/message/content").and_then(|v| v.as_array()) {
                            for block in content {
                                let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                                if block_type == "text" {
                                    if let Some(msg) = block.get("text").and_then(|v| v.as_str()) {
                                        boi_log!("  claude: {}", msg);
                                    }
                                } else if block_type == "tool_use" {
                                    tool_call_count += 1;
                                    if let Some(tool_name) = block.get("name").and_then(|v| v.as_str()) {
                                        *tool_calls_by_type.entry(tool_name.to_string()).or_insert(0) += 1;
                                        let input_str = block.get("input")
                                            .map(|v| v.to_string())
                                            .unwrap_or_default();
                                        boi_log!("  tool: {} {}", tool_name, input_str);
                                    }
                                }
                            }
                        }
                        if let Some(usage) = event.pointer("/message/usage") {
                            if let Some(v) = usage.get("input_tokens").and_then(|v| v.as_i64()) {
                                input_tokens = Some(v);
                            }
                            if let Some(v) = usage.get("output_tokens").and_then(|v| v.as_i64()) {
                                output_tokens = Some(v);
                            }
                            if let Some(v) = usage.get("cache_read_input_tokens").and_then(|v| v.as_i64()) {
                                cache_read_tokens = Some(v);
                            }
                            if let Some(v) = usage.get("cache_creation_input_tokens").and_then(|v| v.as_i64()) {
                                cache_creation_tokens = Some(v);
                            }
                        }
                    }
                    "result" => {
                        if let Some(text) = event.get("result").and_then(|v| v.as_str()) {
                            last_output = text.to_string();
                        }
                        if let Some(v) = event.get("total_cost_usd").and_then(|v| v.as_f64())
                            .or_else(|| event.get("cost_usd").and_then(|v| v.as_f64()))
                        {
                            cost_usd = Some(v);
                        }
                        if let Some(usage) = event.get("usage") {
                            if let Some(v) = usage.get("input_tokens").and_then(|v| v.as_i64()) {
                                input_tokens = Some(v);
                            }
                            if let Some(v) = usage.get("output_tokens").and_then(|v| v.as_i64()) {
                                output_tokens = Some(v);
                            }
                            if let Some(v) = usage.get("cache_read_input_tokens").and_then(|v| v.as_i64()) {
                                cache_read_tokens = Some(v);
                            }
                            if let Some(v) = usage.get("cache_creation_input_tokens").and_then(|v| v.as_i64()) {
                                cache_creation_tokens = Some(v);
                            }
                        }
                    }
                    _ => {}
                }
            } else {
                raw_lines.push(line);
            }
        }
        if last_output.is_empty() && !raw_lines.is_empty() {
            last_output = raw_lines.join("\n");
        }
        (first_byte_time, last_output, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, cost_usd, tool_call_count, tool_calls_by_type)
    });

    let mut timed_out = false;
    let mut exit_status = None;
    loop {
        match child.try_wait()? {
            Some(status) => {
                exit_status = Some(status);
                // SAFETY: pgid is the child's PID (valid after setsid), negative value targets
                // the entire process group, killing any grandchildren that inherited the pipes.
                unsafe { libc::kill(-pgid, libc::SIGKILL); }
                break;
            }
            None => {
                if spawn_time.elapsed().as_secs() >= timeout_secs {
                    let _ = child.kill(); // intentional: best-effort cleanup on timeout
                    let _ = child.wait(); // intentional: reap zombie after kill
                    // SAFETY: same as above — kill the entire process group on timeout.
                    unsafe { libc::kill(-pgid, libc::SIGKILL); }
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }

    let total_ms = spawn_time.elapsed().as_millis() as u64;

    // Clean up PID file — child has exited (or been killed on timeout)
    if let Some(ref p) = pid_path {
        let _ = std::fs::remove_file(p); // intentional: best-effort pid file cleanup
    }

    if timed_out {
        let _ = reader_handle.join(); // intentional: best-effort thread join on timeout
        let stderr_output = stderr_handle.join().unwrap_or_default();
        if !stderr_output.is_empty() {
            boi_log!("claude stderr (timeout):\n{}", stderr_output);
        }
        return Ok(ClaudeResult {
            success: false,
            output: "timeout".to_string(),
            stderr: stderr_output,
            startup_ms: 0,
            inference_ms: 0,
            total_ms,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            cost_usd: None,
            tool_call_count: 0,
            tool_calls_by_type: std::collections::HashMap::new(),
        });
    }

    // With setsid + process group kill, grandchildren are dead and pipes unblock naturally.
    let (first_byte_instant, output, r_input_tokens, r_output_tokens, r_cache_read, r_cache_create, r_cost_usd, r_tool_count, r_tool_types) =
        reader_handle.join().unwrap_or((None, String::new(), None, None, None, None, None, 0, std::collections::HashMap::new()));
    let stderr_output = stderr_handle.join().unwrap_or_default();

    if !stderr_output.is_empty() {
        boi_log!("claude stderr:\n{}", stderr_output);
    }

    let startup_ms = first_byte_instant
        .map(|t| t.duration_since(spawn_time).as_millis() as u64)
        .unwrap_or(total_ms);
    let inference_ms = total_ms.saturating_sub(startup_ms);

    let success = exit_status.map(|s| s.success()).unwrap_or(false);
    Ok(ClaudeResult {
        success,
        output,
        stderr: stderr_output,
        startup_ms,
        inference_ms,
        total_ms,
        input_tokens: r_input_tokens,
        output_tokens: r_output_tokens,
        cache_read_tokens: r_cache_read,
        cache_creation_tokens: r_cache_create,
        cost_usd: r_cost_usd,
        tool_call_count: r_tool_count,
        tool_calls_by_type: r_tool_types,
    })
}
