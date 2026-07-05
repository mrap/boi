//! Linux backend: `systemd --user` units.
//!
//! Units live in `~/.config/systemd/user/<label>.service` (no root). Every
//! `systemctl --user` / `journalctl --user` call sets `XDG_RUNTIME_DIR` +
//! `DBUS_SESSION_BUS_ADDRESS` so it works over SSH / from non-login contexts.
//! `loginctl enable-linger` keeps the user manager alive across logout + starts
//! the service at boot.

use crate::{Error, Result, ServiceManager, ServiceSpec, ServiceStatus};
use std::path::PathBuf;
use std::process::Command;

pub struct SystemdUser;

impl SystemdUser {
    pub fn new() -> Self {
        SystemdUser
    }
}

fn uid() -> u32 {
    // Safe: getuid never fails.
    unsafe { libc_getuid() }
}

// Avoid a libc dependency for one call.
extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"))
}

fn unit_dir() -> PathBuf {
    home().join(".config/systemd/user")
}

fn unit_path(label: &str) -> PathBuf {
    unit_dir().join(format!("{label}.service"))
}

/// Build a `systemctl --user` command with the runtime-dir/bus env set, so it
/// connects to the user manager over SSH / headless.
fn systemctl(args: &[&str]) -> Command {
    let u = uid();
    let xdg = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{u}"));
    let bus = std::env::var("DBUS_SESSION_BUS_ADDRESS")
        .unwrap_or_else(|_| format!("unix:path={xdg}/bus"));
    let mut c = Command::new("systemctl");
    c.arg("--user")
        .args(args)
        .env("XDG_RUNTIME_DIR", xdg)
        .env("DBUS_SESSION_BUS_ADDRESS", bus);
    c
}

/// Run a command, mapping a non-zero exit (or spawn failure) to a descriptive
/// `Error::Command` with the stderr/stdout tail (S6: loud, never silent).
fn run(mut cmd: Command, ctx: &str) -> Result<std::process::Output> {
    let out = cmd
        .output()
        .map_err(|e| Error::Command(format!("{ctx}: spawn failed: {e}")))?;
    if !out.status.success() {
        let code = out
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        let tail = String::from_utf8_lossy(&out.stderr);
        let tail = tail.trim();
        let tail = if tail.is_empty() {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            tail.to_string()
        };
        return Err(Error::Command(format!("{ctx}: exited {code}: {tail}")));
    }
    Ok(out)
}

use crate::render::systemd_unit;

impl SystemdUser {
    fn ensure_systemd() -> Result<()> {
        if std::path::Path::new("/run/systemd/system").is_dir() {
            Ok(())
        } else {
            Err(Error::Unsupported(
                "daemon-green: systemd not detected (/run/systemd/system missing) — \
                 user-service management unsupported on this host"
                    .into(),
            ))
        }
    }
}

impl ServiceManager for SystemdUser {
    fn install(&self, spec: &ServiceSpec) -> Result<()> {
        Self::ensure_systemd()?;
        std::fs::create_dir_all(unit_dir())?;
        std::fs::write(unit_path(&spec.label), systemd_unit(spec))?;
        run(
            systemctl(&["daemon-reload"]),
            "systemctl --user daemon-reload",
        )?;
        // Keep the user manager alive across logout + start at boot. Best-effort:
        // self-linger is allowed without sudo on most distros; if it fails (some
        // hardened setups), the service still works while logged in.
        let _ = Command::new("loginctl")
            .args(["enable-linger", &whoami()])
            .status();
        Ok(())
    }

    fn start(&self, label: &str) -> Result<()> {
        Self::ensure_systemd()?;
        run(
            systemctl(&["enable", "--now", &svc(label)]),
            "systemctl --user enable --now",
        )?;
        Ok(())
    }

    fn stop(&self, label: &str) -> Result<()> {
        Self::ensure_systemd()?;
        run(
            systemctl(&["disable", "--now", &svc(label)]),
            "systemctl --user disable --now",
        )?;
        Ok(())
    }

    fn restart(&self, label: &str) -> Result<()> {
        Self::ensure_systemd()?;
        // Clear any start-limit failure first, else restart is refused.
        let _ = run(systemctl(&["reset-failed", &svc(label)]), "reset-failed");
        run(
            systemctl(&["restart", &svc(label)]),
            "systemctl --user restart",
        )?;
        Ok(())
    }

    fn status(&self, label: &str) -> Result<ServiceStatus> {
        Self::ensure_systemd()?;
        // `show` is machine-readable and always exits 0 (even when not loaded).
        let out = systemctl(&[
            "show",
            &svc(label),
            "-p",
            "LoadState,ActiveState,SubState,ExecMainPID,Result",
        ])
        .output()
        .map_err(|e| Error::Command(format!("systemctl --user show: spawn failed: {e}")))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut kv = std::collections::HashMap::new();
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                kv.insert(k.trim(), v.trim().to_string());
            }
        }
        let load = kv.get("LoadState").map(String::as_str).unwrap_or("");
        let active = kv.get("ActiveState").map(String::as_str).unwrap_or("");
        if load == "not-found" || load.is_empty() {
            return Ok(ServiceStatus::NotInstalled);
        }
        match active {
            "active" | "activating" => {
                let pid = kv
                    .get("ExecMainPID")
                    .and_then(|p| p.parse::<u32>().ok())
                    .filter(|p| *p != 0);
                Ok(ServiceStatus::Running { pid })
            }
            "failed" => Ok(ServiceStatus::Failed {
                reason: kv.get("Result").cloned().unwrap_or_else(|| "failed".into()),
            }),
            _ => Ok(ServiceStatus::Stopped),
        }
    }

    fn logs(&self, label: &str, lines: usize) -> Result<String> {
        Self::ensure_systemd()?;
        let n = lines.to_string();
        let mut c = Command::new("journalctl");
        let u = uid();
        let xdg = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{u}"));
        c.args(["--user", "-u", &svc(label), "-n", &n, "--no-pager"])
            .env("XDG_RUNTIME_DIR", &xdg);
        let out = c
            .output()
            .map_err(|e| Error::Command(format!("journalctl --user: spawn failed: {e}")))?;
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }
}

fn svc(label: &str) -> String {
    if label.ends_with(".service") {
        label.to_string()
    } else {
        format!("{label}.service")
    }
}

fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| {
            Command::new("id")
                .arg("-un")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        })
}

#[cfg(test)]
mod tests {
    use super::svc;

    #[test]
    fn svc_appends_service_suffix_once() {
        assert_eq!(svc("com.x.y"), "com.x.y.service");
        assert_eq!(svc("com.x.y.service"), "com.x.y.service");
    }
}
