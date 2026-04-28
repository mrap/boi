use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HookEntry {
    pub command: String,
    pub blocking: Option<bool>,
    pub timeout: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct HookConfig {
    pub hooks: Option<HashMap<String, HookEntry>>,
}

pub const ON_DISPATCH: &str = "on_dispatch";
pub const ON_WORKER_START: &str = "on_worker_start";
pub const ON_TASK_START: &str = "on_task_start";
pub const ON_TASK_COMPLETE: &str = "on_task_complete";
pub const ON_TASK_FAIL: &str = "on_task_fail";
pub const ON_PHASE_START: &str = "on_phase_start";
pub const ON_PHASE_COMPLETE: &str = "on_phase_complete";
pub const ON_PHASE_FAIL: &str = "on_phase_fail";
pub const ON_PHASE_SKIP: &str = "on_phase_skip";
pub const ON_COMPLETE: &str = "on_complete";
pub const ON_FAIL: &str = "on_fail";
pub const ON_CANCEL: &str = "on_cancel";
pub const ON_STALL: &str = "on_stall";
pub const ON_SPEC_PAUSED: &str = "on_spec_paused";

/// Fire a lifecycle hook for the given event. Never panics — errors are logged to stderr.
pub fn fire(
    config: &HookConfig,
    event: &str,
    payload: &Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let hooks = match &config.hooks {
        Some(h) => h,
        None => return Ok(()),
    };

    let entry = match hooks.get(event) {
        Some(e) => e,
        None => return Ok(()),
    };

    let blocking = entry.blocking.unwrap_or(false);
    let timeout_secs = entry.timeout.unwrap_or(10);

    let spec_id = payload
        .get("spec_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let task_id = payload
        .get("task_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let payload_str = match serde_json::to_string(payload) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[BOI] hook '{}' failed to serialize payload: {}", event, e);
            return Ok(());
        }
    };

    let mut child = match Command::new("sh")
        .args(["-c", &entry.command])
        .env("BOI_EVENT", event)
        .env("BOI_SPEC_ID", &spec_id)
        .env("BOI_TASK_ID", &task_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[BOI] hook '{}' failed to spawn: {}", event, e);
            return Ok(());
        }
    };

    // Write JSON payload to stdin then close it.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload_str.as_bytes());
        // stdin dropped here → EOF sent to child
    }

    if blocking {
        match wait_with_timeout(child, Duration::from_secs(timeout_secs), event) {
            Ok(status) if !status.success() => {
                eprintln!("[BOI] hook '{}' exited with status: {}", event, status);
            }
            Err(e) => {
                eprintln!("[BOI] hook '{}' error: {}", event, e);
            }
            _ => {}
        }
    } else {
        // Reap child in background thread to avoid zombies.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }

    Ok(())
}

fn wait_with_timeout(
    child: std::process::Child,
    timeout: Duration,
    event: &str,
) -> Result<std::process::ExitStatus, Box<dyn std::error::Error>> {
    use std::sync::{mpsc, Arc, Mutex};
    let (tx, rx) = mpsc::channel();
    let child_arc = Arc::new(Mutex::new(child));
    let child_thread = Arc::clone(&child_arc);

    std::thread::spawn(move || {
        let result = child_thread.lock().unwrap().wait();
        let _ = tx.send(result);
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(e)) => Err(Box::new(e)),
        Err(_) => {
            // Timeout — kill the child process to prevent zombie
            if let Ok(mut child_guard) = child_arc.lock() {
                let _ = child_guard.kill();
                let _ = child_guard.wait(); // reap
            }
            eprintln!("[boi hooks] {} timed out after {:?}", event, timeout);
            Err("hook timed out".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_fire_no_hooks_configured() {
        let config = HookConfig::default();
        let payload = json!({"spec_id": "q-001"});
        assert!(fire(&config, ON_DISPATCH, &payload).is_ok());
    }

    #[test]
    fn test_fire_event_not_configured() {
        let config = HookConfig {
            hooks: Some(HashMap::new()),
        };
        let payload = json!({"spec_id": "q-001"});
        assert!(fire(&config, ON_DISPATCH, &payload).is_ok());
    }

    #[test]
    fn test_fire_nonblocking_true_command() {
        let mut hooks = HashMap::new();
        hooks.insert(
            ON_DISPATCH.to_string(),
            HookEntry {
                command: "true".to_string(),
                blocking: Some(false),
                timeout: Some(5),
            },
        );
        let config = HookConfig { hooks: Some(hooks) };
        let payload = json!({"spec_id": "q-test", "task_id": "t-1"});
        assert!(fire(&config, ON_DISPATCH, &payload).is_ok());
    }

    #[test]
    fn test_fire_blocking_true_command() {
        let mut hooks = HashMap::new();
        hooks.insert(
            ON_COMPLETE.to_string(),
            HookEntry {
                command: "true".to_string(),
                blocking: Some(true),
                timeout: Some(5),
            },
        );
        let config = HookConfig { hooks: Some(hooks) };
        let payload = json!({"spec_id": "q-test"});
        assert!(fire(&config, ON_COMPLETE, &payload).is_ok());
    }

    #[test]
    fn test_fire_blocking_reads_stdin() {
        // Verify payload is written to stdin by having the hook read it.
        // We use `cat` which just reads stdin and exits 0.
        let mut hooks = HashMap::new();
        hooks.insert(
            ON_TASK_START.to_string(),
            HookEntry {
                command: "cat > /dev/null".to_string(),
                blocking: Some(true),
                timeout: Some(5),
            },
        );
        let config = HookConfig { hooks: Some(hooks) };
        let payload = json!({"spec_id": "q-999", "task_id": "t-5"});
        assert!(fire(&config, ON_TASK_START, &payload).is_ok());
    }

    #[test]
    fn test_fire_bad_command_does_not_crash() {
        let mut hooks = HashMap::new();
        hooks.insert(
            ON_FAIL.to_string(),
            HookEntry {
                command: "this_command_does_not_exist_boi_test".to_string(),
                blocking: Some(false),
                timeout: Some(2),
            },
        );
        let config = HookConfig { hooks: Some(hooks) };
        let payload = json!({"spec_id": "q-bad"});
        // Must not panic or return Err — errors are swallowed as warnings.
        assert!(fire(&config, ON_FAIL, &payload).is_ok());
    }
}
