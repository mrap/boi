//! macOS backend: a per-user **gui-domain LaunchAgent**.
//!
//! The service runs in the user's Aqua login session (so it reaches the login
//! keychain). Install/start is hardened against the real footguns:
//! - **no `SessionCreate`** in the plist (it blocks the keychain);
//! - `bootout` is async, so we **wait until the label is gone** before
//!   re-`bootstrap`ing (otherwise the bootstrap races → EIO);
//! - **retry** the bootstrap, then fall back to `launchctl asuser` (both
//!   sudo-free);
//! - require an active GUI login (`gui/<uid>` exists); fail LOUD otherwise.
//! Works over SSH whenever a desktop login is active (a desktop Mac always has one).

use crate::render::launchd_plist;
use crate::{Error, Result, ServiceManager, ServiceSpec, ServiceStatus};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

pub struct LaunchdAgent;

impl LaunchdAgent {
    pub fn new() -> Self {
        LaunchdAgent
    }
}

extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}
fn uid() -> u32 {
    unsafe { libc_getuid() }
}
fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}
fn plist_path(label: &str) -> PathBuf {
    home()
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist"))
}
fn default_log(label: &str) -> PathBuf {
    home().join("Library/Logs").join(format!("{label}.log"))
}
fn gui_target(label: &str) -> String {
    format!("gui/{}/{label}", uid())
}

fn launchctl(args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("launchctl").args(args).output()
}

/// True if a GUI login session exists (so we can bootstrap into gui/<uid> and the
/// login keychain is unlocked).
fn gui_session_active() -> bool {
    launchctl(&["print", &format!("gui/{}", uid())])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Robust bootstrap (R8): clean bootout → wait until gone → retry bootstrap →
/// `asuser` fallback. Returns a loud error if all paths fail.
fn bootstrap_robust(label: &str) -> Result<()> {
    let u = uid();
    let domain = format!("gui/{u}");
    let plist = plist_path(label);
    let plist_s = plist.to_string_lossy().to_string();
    let target = gui_target(label);

    // 1. Clean any existing instance and WAIT for bootout to finish (it's async;
    //    a bootstrap that races it gets EIO).
    let _ = launchctl(&["bootout", &target]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let still = launchctl(&["print", &target])
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !still {
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    // 2. Self-heal a *disabled* label. macOS persists per-user disable state
    //    (see `launchctl print-disabled gui/<uid>`); a disabled label cannot be
    //    bootstrapped and surfaces as `Input/output error` (EIO-5) — and
    //    `bootout` does NOT clear the disabled bit. Mirror the systemd path's
    //    `enable --now` by best-effort `launchctl enable` before bootstrap.
    //    Idempotent and harmless if already enabled.
    let _ = launchctl(&["enable", &target]);

    // 3. Retry plain bootstrap.
    let mut last = String::new();
    for _ in 0..3 {
        match launchctl(&["bootstrap", &domain, &plist_s]) {
            Ok(o) if o.status.success() => return Ok(()),
            Ok(o) => last = String::from_utf8_lossy(&o.stderr).trim().to_string(),
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    // 3. asuser fallback (sudo-free; bridges into the user's Aqua bootstrap).
    if let Ok(o) = Command::new("launchctl")
        .args([
            "asuser",
            &u.to_string(),
            "launchctl",
            "bootstrap",
            &domain,
            &plist_s,
        ])
        .output()
    {
        if o.status.success() {
            return Ok(());
        }
        last = String::from_utf8_lossy(&o.stderr).trim().to_string();
    }

    Err(Error::Command(format!(
        "launchctl bootstrap {domain} {plist_s} failed after retries + asuser: {last}"
    )))
}

impl ServiceManager for LaunchdAgent {
    fn install(&self, spec: &ServiceSpec) -> Result<()> {
        // Default the log to ~/Library/Logs/<label>.log if the caller didn't set one,
        // so `logs` has somewhere to read.
        let mut spec = spec.clone();
        if spec.log_path.is_none() {
            spec.log_path = Some(default_log(&spec.label));
        }
        if let Some(lp) = &spec.log_path {
            if let Some(parent) = lp.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let p = plist_path(&spec.label);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, launchd_plist(&spec))?;
        Ok(())
    }

    fn start(&self, label: &str) -> Result<()> {
        if !plist_path(label).exists() {
            return Err(Error::Command(format!(
                "{label}: not installed (no plist at {}). Call install() first.",
                plist_path(label).display()
            )));
        }
        if !gui_session_active() {
            return Err(Error::Unsupported(format!(
                "no active GUI login (gui/{} missing). Log in at the console once \
                 (or enable auto-login) — the agent needs the login session for the keychain.",
                uid()
            )));
        }
        bootstrap_robust(label)?;
        // Kick it so RunAtLoad/KeepAlive engage immediately.
        let _ = launchctl(&["kickstart", "-k", &gui_target(label)]);
        Ok(())
    }

    fn stop(&self, label: &str) -> Result<()> {
        match launchctl(&["bootout", &gui_target(label)]) {
            Ok(o) if o.status.success() => Ok(()),
            // "not loaded" is not a failure for stop().
            Ok(_) => Ok(()),
            Err(e) => Err(Error::Command(format!("launchctl bootout: {e}"))),
        }
    }

    fn restart(&self, label: &str) -> Result<()> {
        match launchctl(&["kickstart", "-k", &gui_target(label)]) {
            Ok(o) if o.status.success() => Ok(()),
            // If it wasn't loaded, fall back to a full (re)bootstrap.
            _ => self.start(label),
        }
    }

    fn status(&self, label: &str) -> Result<ServiceStatus> {
        if !plist_path(label).exists() {
            return Ok(ServiceStatus::NotInstalled);
        }
        let out = match launchctl(&["print", &gui_target(label)]) {
            Ok(o) => o,
            Err(e) => return Err(Error::Command(format!("launchctl print: {e}"))),
        };
        if !out.status.success() {
            // plist exists but not loaded.
            return Ok(ServiceStatus::Stopped);
        }
        let text = String::from_utf8_lossy(&out.stdout);
        // `launchctl print` output is not API; parse loosely.
        let pid = text.lines().find_map(|l| {
            let l = l.trim();
            l.strip_prefix("pid = ")
                .and_then(|v| v.trim().parse::<u32>().ok())
        });
        match pid {
            Some(p) => Ok(ServiceStatus::Running { pid: Some(p) }),
            None => Ok(ServiceStatus::Stopped),
        }
    }

    fn logs(&self, label: &str, lines: usize) -> Result<String> {
        // Honor the plist's StandardOutPath if present; else the default.
        let log = launchctl_extract_stdout(label).unwrap_or_else(|| default_log(label));
        if !log.exists() {
            return Ok(String::new());
        }
        let content = std::fs::read_to_string(&log)?;
        let tail: Vec<&str> = content.lines().rev().take(lines).collect();
        Ok(tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
    }
}

/// Read StandardOutPath from the installed plist (so logs() honors a custom path).
fn launchctl_extract_stdout(label: &str) -> Option<PathBuf> {
    let p = plist_path(label);
    let out = Command::new("plutil")
        .args(["-extract", "StandardOutPath", "raw", "-o", "-"])
        .arg(&p)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}
