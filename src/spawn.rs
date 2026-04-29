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
                        if let Some(msg) = event.pointer("/message/content/0/text").and_then(|v| v.as_str()) {
                            boi_log!("  claude: {}", msg);
                        }
                        if let Some(tool) = event.pointer("/message/content/0/name").and_then(|v| v.as_str()) {
                            let input_str = event.pointer("/message/content/0/input")
                                .map(|v| v.to_string())
                                .unwrap_or_default();
                            boi_log!("  tool: {} {}", tool, input_str);
                        }
                    }
                    "result" => {
                        if let Some(text) = event.get("result").and_then(|v| v.as_str()) {
                            last_output = text.to_string();
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
        (first_byte_time, last_output)
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
        });
    }

    // With setsid + process group kill, grandchildren are dead and pipes unblock naturally.
    let (first_byte_instant, output) = reader_handle.join().unwrap_or((None, String::new()));
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
    })
}
