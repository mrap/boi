# daemon-green

**Cross-platform per-user background service manager.** One trait, two native
backends — macOS `launchd` (gui-domain LaunchAgent) and Linux `systemd --user`.
Sudo-free, SSH-safe, and careful about the things that actually break in
production: the macOS login keychain, the launchd `bootout` race, and missing
DBus on a fresh SSH session.

## Why

Writing a background service that "just works" on a developer's laptop is
deceptively hard:

- **macOS:** the obvious choice (`launchctl load`, a `LaunchDaemon`, or a
  `SessionCreate` agent) silently breaks the **login keychain**. Your service
  starts fine but every `security find-generic-password` returns
  `errSecAuthFailed` — because the job is no longer in the user's login audit
  session.
- **Linux:** the obvious `systemctl --user` invocation works on the desktop and
  then fails over SSH because `XDG_RUNTIME_DIR` and `DBUS_SESSION_BUS_ADDRESS`
  are not set in the SSH environment.
- Both backends have their own crash-loop, idempotency, and async-teardown
  footguns that you discover the second time you try to redeploy.

`daemon-green` is the small library that gets all of those right, behind one
trait. No supervisor process, no daemon-of-a-daemon — the OS service managers
already do keepalive, backoff, and reaping correctly; this crate is the thin,
careful adapter on top.

## Platform behavior

|                      | macOS                                                   | Linux                                                                  |
| -------------------- | ------------------------------------------------------- | ---------------------------------------------------------------------- |
| Backend              | `launchd` per-user **gui-domain LaunchAgent**           | `systemd --user` unit                                                  |
| Unit location        | `~/Library/LaunchAgents/<label>.plist`                  | `~/.config/systemd/user/<label>.service`                               |
| Install / start verb | `launchctl bootstrap gui/<uid>` + `kickstart`           | `systemctl --user daemon-reload` + `enable --now`                      |
| Crash restart        | `KeepAlive` (on failure)                                | `Restart=always` + `StartLimit` crash-loop guard                       |
| Logs                 | `~/Library/Logs/<label>.log` (stdout+stderr)            | `journalctl --user -u <label>`                                         |
| Autostart            | `RunAtLoad` at login                                    | `WantedBy=default.target` + `loginctl enable-linger`                   |
| Sudo required        | No                                                      | No                                                                     |
| Works over SSH       | Yes (a desktop GUI login session must exist)            | Yes (`XDG_RUNTIME_DIR` + `DBUS_SESSION_BUS_ADDRESS` set on every call) |

## Install

```toml
[dependencies]
daemon-green = { git = "https://github.com/mrap/daemon-green" }
```

(This copy is vendored into `boi`'s `vendor/daemon-green/` and consumed there
via a local path dependency — see that repo's `Cargo.toml`.)

The crate is `std`-only — backends shell out to `launchctl` and `systemctl`,
no external Rust dependencies.

## Usage

```rust
use daemon_green::{native, ServiceManager, ServiceSpec, ServiceStatus};

fn main() -> Result<(), daemon_green::Error> {
    let spec = ServiceSpec::new("com.example.myd", "/usr/local/bin/myd")
        .arg("serve")
        .env("MY_VAR", "1")
        .keep_alive(true)
        .run_at_load(true);

    let mgr = native();

    mgr.install(&spec)?;              // render unit + register (idempotent)
    mgr.start(spec.label())?;         // bootstrap / enable --now (idempotent)

    match mgr.status(spec.label())? {
        ServiceStatus::Running { pid }   => println!("running, pid={pid:?}"),
        ServiceStatus::Stopped           => println!("installed but stopped"),
        ServiceStatus::Failed { reason } => println!("failed: {reason}"),
        ServiceStatus::NotInstalled      => println!("not installed"),
    }

    let tail = mgr.logs(spec.label(), 50)?;
    println!("--- last 50 log lines ---\n{tail}");

    mgr.stop(spec.label())?;
    Ok(())
}
```

`install` and `start` are idempotent — safe to call on every app launch.

## API

One trait, one builder, one status enum, one constructor.

```rust
pub trait ServiceManager {
    fn install(&self, spec: &ServiceSpec) -> Result<()>;
    fn start(&self, label: &str)          -> Result<()>;
    fn stop(&self, label: &str)           -> Result<()>;
    fn restart(&self, label: &str)        -> Result<()>;
    fn status(&self, label: &str)         -> Result<ServiceStatus>;
    fn logs(&self, label: &str, lines: usize) -> Result<String>;
}

pub fn native() -> Box<dyn ServiceManager>;  // compile-time #[cfg] dispatch
```

`ServiceSpec` (builder; defaults: `keep_alive = true`, `run_at_load = true`):

| Field         | Type                       | Notes                                                                                                  |
| ------------- | -------------------------- | ------------------------------------------------------------------------------------------------------ |
| `label`       | `String`                   | Reverse-DNS, e.g. `com.example.myd`. Used as the launchd `Label` and as `<label>.service` on Linux.    |
| `program`     | `PathBuf`                  | **Absolute** path. On macOS this becomes `ProgramArguments[0]` directly — no `bash -c` wrapper.        |
| `args`        | `Vec<String>`              | Arguments after the program.                                                                           |
| `env`         | `BTreeMap<String, String>` | Ordered → deterministic rendered units.                                                                |
| `working_dir` | `Option<PathBuf>`          | Service working directory.                                                                             |
| `keep_alive`  | `bool`                     | Restart on crash (launchd `KeepAlive` on failure / systemd `Restart=always`).                          |
| `run_at_load` | `bool`                     | Start at login / boot.                                                                                 |
| `log_path`    | `Option<PathBuf>`          | Combined stdout+stderr. Default: `~/Library/Logs/<label>.log` (macOS) / the journal (Linux).           |

`ServiceStatus`:

```rust
pub enum ServiceStatus {
    Running { pid: Option<u32> },
    Stopped,
    NotInstalled,
    Failed { reason: String },
}
```

Errors are surfaced via `daemon_green::Error` — every variant carries the
command, exit code, and stderr/stdout tail where relevant. Never a silent
swallow.

## macOS & the login keychain

This is the whole reason the crate exists, so the design choices are explicit:

- **gui-domain agent (`gui/<uid>`), not a `LaunchDaemon`.** A daemon runs as
  `root` outside any user session and cannot reach the login keychain at all.
  A gui-domain LaunchAgent runs inside the user's GUI login session and
  inherits the unlocked login keychain.
- **No `SessionCreate`.** That key spawns the job into a brand-new audit
  session, which empirically blocks the login keychain: a gui agent **without**
  `SessionCreate` reads the keychain with `rc=0`; the same agent **with**
  `SessionCreate` returns `rc=36` (`errSecAuthFailed`). We deliberately omit
  it, and the plist renderer is unit-tested to keep it out.
- **No `bash -c` wrapper.** The program path is `ProgramArguments[0]`
  directly. Wrapping it in a shell changes the keychain-ACL identity (the ACL
  is keyed by the code signature of the executing binary, and `/bin/bash` is
  not your app), which also breaks keychain reads silently.
- **GUI login required.** The agent runs in `gui/<uid>`, so the user must
  have logged in at least once on the desktop (or have auto-login enabled).
  Over SSH after a desktop login is fine; a pure headless mac with no console
  login is not — that is what `LaunchDaemon` is for, and `LaunchDaemon` will
  not see the keychain.
- **`bootout` race handled.** `launchctl bootout` is asynchronous; an
  immediate re-`bootstrap` races it and returns `EIO`. The backend waits the
  teardown out, retries, and on persistent failure falls back to
  `launchctl asuser <uid> launchctl load`. All sudo-free.

## Testing

Two layers, both runnable on a normal dev box:

```bash
# 1. Pure renderer unit tests (plist + systemd unit generation).
#    No launchctl / systemctl required — runs on macOS, Linux, anywhere.
cargo test --lib

# 2. End-to-end systemd --user lifecycle inside a real, privileged container
#    (boots actual systemd in Docker with --privileged --cgroupns=host and
#    drives install / start / status / logs / restart / stop).
bash tests/e2e/run.sh
```

The renderers are pure functions over `ServiceSpec`, so they are exhaustively
unit-testable on any host. The systemd lifecycle test boots real PID-1
`systemd` in a container instead of mocking it — `systemctl --user` behavior
is the contract being tested, and only a real systemd will catch regressions
in it. See [`tests/e2e/README.md`](tests/e2e/README.md) for the harness
details.

The macOS backend's launchd interactions are covered by manual smoke tests on
real hardware; there is no honest way to run `launchctl bootstrap gui/<uid>`
inside CI.

## Non-goals

- **Windows.** Deliberately deferred behind the `ServiceManager` trait. A
  Windows Service Control Manager backend can be added without touching the
  public API.
- **Non-systemd Linux** (OpenRC, runit, sysvinit, plain SSH boxes without a
  user instance). The Linux backend detects systemd and degrades with a
  clear `Error::Unsupported` instead of silently rendering a unit nothing
  will read.
- **A userland supervisor.** Explicitly rejected — `launchd` and `systemd`
  already do keepalive, exponential backoff, and zombie reaping correctly.
  Reimplementing them in process would be strictly worse.
- **Per-system daemons running as root.** Use a `LaunchDaemon` / system-level
  systemd unit directly if that is what you want; this crate is per-user
  only, on purpose.

## License

MIT. See `Cargo.toml`.
