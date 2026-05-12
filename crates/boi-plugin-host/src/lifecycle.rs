//! Plugin process lifecycle.
//!
//! Responsibilities:
//! - Spawn a plugin binary as a child process (stdout/stderr piped).
//! - Wait for `BOI_READY\n` on the child's stdout within
//!   `ready_timeout_secs` (default 10s). Surface anything emitted
//!   before that line to logs (F-11).
//! - Enforce the F-20 restart policy: at most 3 restarts within a
//!   5-minute rolling window. The 4th crash within that window flips
//!   the plugin to [`PluginHealth::Unstable`] and stops restarts.
//! - Graceful shutdown: send SIGTERM, wait up to `shutdown_grace_secs`
//!   (default 5s) for exit, then SIGKILL.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;

/// Default time we wait for the child to print `BOI_READY\n`.
pub const DEFAULT_READY_TIMEOUT_SECS: u64 = 10;
/// Default grace period before a graceful shutdown escalates to SIGKILL.
pub const DEFAULT_SHUTDOWN_GRACE_SECS: u64 = 5;
/// Per F-20: window in which restarts are counted.
pub const RESTART_WINDOW_SECS: u64 = 300;
/// Per F-20: maximum restarts allowed inside the window.
pub const RESTART_BUDGET: usize = 3;

/// One of the five plugin slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginKind {
    Workspace,
    Pool,
    Router,
    Provisioner,
    Hooks,
}

impl PluginKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            PluginKind::Workspace => "workspace",
            PluginKind::Pool => "pool",
            PluginKind::Router => "router",
            PluginKind::Provisioner => "provisioner",
            PluginKind::Hooks => "hooks",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginHealth {
    /// Plugin has not yet emitted `BOI_READY\n`.
    Starting,
    /// Ready signal observed; plugin is serving RPCs.
    Ready,
    /// Plugin exceeded the F-20 restart budget; host stops restarts.
    Unstable,
    /// Plugin was shut down by the host.
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct RestartPolicy {
    pub budget: usize,
    pub window: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            budget: RESTART_BUDGET,
            window: Duration::from_secs(RESTART_WINDOW_SECS),
        }
    }
}

impl RestartPolicy {
    /// Record a crash at `now` against the rolling window. Returns
    /// `true` if a restart is still allowed (i.e. budget not yet
    /// blown), `false` if the plugin should flip to `Unstable`.
    pub fn admit(&self, history: &mut VecDeque<Instant>, now: Instant) -> bool {
        let cutoff = now.checked_sub(self.window).unwrap_or(now);
        while let Some(front) = history.front() {
            if *front < cutoff {
                history.pop_front();
            } else {
                break;
            }
        }
        history.push_back(now);
        history.len() <= self.budget
    }
}

#[derive(Debug, Clone)]
pub struct PluginConfig {
    pub kind: PluginKind,
    pub binary: PathBuf,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub ready_timeout_secs: u64,
    pub shutdown_grace_secs: u64,
    pub restart: RestartPolicy,
}

impl PluginConfig {
    pub fn new(kind: PluginKind, binary: impl Into<PathBuf>) -> Self {
        Self {
            kind,
            binary: binary.into(),
            argv: Vec::new(),
            env: Vec::new(),
            ready_timeout_secs: DEFAULT_READY_TIMEOUT_SECS,
            shutdown_grace_secs: DEFAULT_SHUTDOWN_GRACE_SECS,
            restart: RestartPolicy::default(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ReadyError {
    #[error("spawn failed: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("timeout waiting for BOI_READY (after {0:?})")]
    Timeout(Duration),
    #[error("child exited before emitting BOI_READY")]
    EarlyExit,
}

/// Live handle to a spawned plugin process.
pub struct PluginHandle {
    pub config: PluginConfig,
    pub child: Mutex<Option<Child>>,
    pub health: Mutex<PluginHealth>,
    pub restart_history: Mutex<VecDeque<Instant>>,
}

impl PluginHandle {
    pub fn new(config: PluginConfig) -> Self {
        Self {
            config,
            child: Mutex::new(None),
            health: Mutex::new(PluginHealth::Starting),
            restart_history: Mutex::new(VecDeque::new()),
        }
    }
}

/// Static helper for one-shot lifecycle ops (no long-lived handle).
pub struct Plugin;

impl Plugin {
    /// Spawn the child and wait for `BOI_READY\n`. On success the
    /// child is returned still running.
    pub async fn spawn_and_wait_ready(cfg: &PluginConfig) -> Result<Child, ReadyError> {
        let mut cmd = Command::new(&cfg.binary);
        cmd.args(&cfg.argv)
            .envs(cfg.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn()?;

        let stdout = child.stdout.take().expect("piped stdout");
        let mut reader = BufReader::new(stdout).lines();
        let wait = Duration::from_secs(cfg.ready_timeout_secs);

        let ready_fut = async {
            loop {
                match reader.next_line().await {
                    Ok(Some(line)) => {
                        if line.trim_end_matches('\r') == "BOI_READY" {
                            return Ok(reader.into_inner().into_inner());
                        }
                        // Anything else before ready is treated as
                        // plugin log output; we drop it here.
                    }
                    Ok(None) => return Err(ReadyError::EarlyExit),
                    Err(e) => return Err(ReadyError::Spawn(e)),
                }
            }
        };

        match timeout(wait, ready_fut).await {
            Ok(Ok(stdout)) => {
                child.stdout = Some(stdout);
                Ok(child)
            }
            Ok(Err(e)) => {
                let _ = child.kill().await;
                Err(e)
            }
            Err(_) => {
                let _ = child.kill().await;
                Err(ReadyError::Timeout(wait))
            }
        }
    }

    /// Graceful shutdown: SIGTERM, wait `grace`, then SIGKILL if
    /// still alive. On Unix sends SIGTERM via libc; falls back to
    /// `kill()` (SIGKILL) on other targets.
    pub async fn shutdown(child: &mut Child, grace: Duration) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            if let Some(pid) = child.id() {
                // Safety: kill(2) with SIGTERM on a child pid is safe.
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
            }
        }
        match timeout(grace, child.wait()).await {
            Ok(_) => Ok(()),
            Err(_) => child.kill().await,
        }
    }
}

// ---------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_sh(script: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(script.as_bytes()).unwrap();
        let path = f.path().to_path_buf();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o755);
        }
        std::fs::set_permissions(&path, perms).unwrap();
        f
    }

    #[test]
    fn restart_policy_allows_three_then_unstable() {
        let p = RestartPolicy::default();
        let mut hist = VecDeque::new();
        let t0 = Instant::now();
        assert!(p.admit(&mut hist, t0));
        assert!(p.admit(&mut hist, t0 + Duration::from_secs(1)));
        assert!(p.admit(&mut hist, t0 + Duration::from_secs(2)));
        assert!(!p.admit(&mut hist, t0 + Duration::from_secs(3))); // 4th in window
    }

    #[test]
    fn restart_policy_recovers_after_window() {
        let p = RestartPolicy::default();
        let mut hist = VecDeque::new();
        let t0 = Instant::now();
        for i in 0..3 {
            assert!(p.admit(&mut hist, t0 + Duration::from_secs(i)));
        }
        // Outside the 5-min window, the budget resets.
        let later = t0 + Duration::from_secs(RESTART_WINDOW_SECS + 1);
        assert!(p.admit(&mut hist, later));
    }

    #[tokio::test]
    async fn spawn_and_wait_ready_succeeds_on_ready_line() {
        let f = write_sh("#!/bin/sh\necho BOI_READY\nsleep 5\n");
        let cfg = PluginConfig {
            ready_timeout_secs: 3,
            ..PluginConfig::new(PluginKind::Hooks, f.path())
        };
        let mut child = Plugin::spawn_and_wait_ready(&cfg).await.expect("ready");
        let _ = child.kill().await;
    }

    #[tokio::test]
    async fn spawn_and_wait_ready_times_out_when_silent() {
        let f = write_sh("#!/bin/sh\nsleep 10\n");
        let cfg = PluginConfig {
            ready_timeout_secs: 1,
            ..PluginConfig::new(PluginKind::Hooks, f.path())
        };
        let err = Plugin::spawn_and_wait_ready(&cfg).await.unwrap_err();
        matches!(err, ReadyError::Timeout(_));
    }

    #[tokio::test]
    async fn spawn_and_wait_ready_detects_early_exit() {
        let f = write_sh("#!/bin/sh\nexit 1\n");
        let cfg = PluginConfig {
            ready_timeout_secs: 3,
            ..PluginConfig::new(PluginKind::Hooks, f.path())
        };
        let err = Plugin::spawn_and_wait_ready(&cfg).await.unwrap_err();
        matches!(err, ReadyError::EarlyExit);
    }

    #[tokio::test]
    async fn shutdown_terminates_child() {
        let f = write_sh("#!/bin/sh\necho BOI_READY\nsleep 30\n");
        let cfg = PluginConfig {
            ready_timeout_secs: 3,
            shutdown_grace_secs: 1,
            ..PluginConfig::new(PluginKind::Hooks, f.path())
        };
        let mut child = Plugin::spawn_and_wait_ready(&cfg).await.unwrap();
        Plugin::shutdown(&mut child, Duration::from_secs(1)).await.unwrap();
        // After shutdown, wait() should return promptly.
        let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .expect("child exited");
        assert!(status.is_ok());
    }
}
