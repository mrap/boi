//! BOI distributed E2E test harness.
//!
//! Provides shared helpers used by the `tests/e2e_*.rs` suite that drives
//! a hermetic Docker Compose topology (etcd + N `boi-node` containers +
//! plugin sidecars). The helpers themselves are infrastructure: tests
//! call into them rather than re-implement docker/etcd glue.
//!
//! All helpers below return `anyhow::Result` so tests can `?` freely and
//! still produce informative red messages via `dump_artifacts` on failure.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

/// Path to the harness crate's `docker/` directory, relative to the
/// workspace root. Used to locate `docker-compose.yaml`.
pub fn docker_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docker")
}

/// Path where test artifacts (etcd dumps, container logs, RPC traces) are
/// written. Created if missing.
pub fn artifacts_root() -> PathBuf {
    // Walk up to the workspace root: CARGO_MANIFEST_DIR is the crate dir.
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    crate_dir
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("e2e-artifacts"))
        .unwrap_or_else(|| PathBuf::from("e2e-artifacts"))
}

/// A single key/value pair from an etcd prefix dump.
#[derive(Debug, Clone)]
pub struct KV {
    pub key: String,
    pub value: Vec<u8>,
}

/// Bring up a Docker Compose cluster with `n` `boi-node` services in
/// addition to etcd. Returns a handle that tears the cluster down on
/// drop unless `forget()` is called.
///
/// In red-baseline state this typically fails at the `boi-node` image
/// build step because `cargo build -p boi-node` produces the stub
/// binary (exit 78). That failure mode is intentional: tests assert
/// "binary stub" as their red signal.
pub fn start_cluster(n: usize) -> Result<Cluster> {
    if n == 0 || n > 3 {
        bail!("start_cluster: n must be in 1..=3 (only 3 node services defined in compose), got {n}");
    }
    let compose = docker_dir().join("docker-compose.yaml");
    if !compose.exists() {
        bail!("docker-compose.yaml missing at {}", compose.display());
    }
    let profiles: Vec<&str> = match n {
        1 => vec!["node-a"],
        2 => vec!["node-a", "node-b"],
        _ => vec!["node-a", "node-b", "node-c"],
    };
    let mut cmd = Command::new("docker");
    cmd.arg("compose")
        .arg("-f")
        .arg(&compose)
        .arg("up")
        .arg("-d")
        .arg("--build")
        .arg("etcd");
    for p in &profiles {
        cmd.arg(p);
    }
    let status = cmd
        .status()
        .context("failed to invoke `docker compose up`")?;
    if !status.success() {
        bail!(
            "docker compose up failed (exit {:?}); is docker running?",
            status.code()
        );
    }
    Ok(Cluster {
        compose,
        torn_down: false,
    })
}

/// Live cluster handle. Drop = teardown.
pub struct Cluster {
    compose: PathBuf,
    torn_down: bool,
}

impl Cluster {
    /// Tear the cluster down explicitly. Idempotent.
    pub fn down(&mut self) -> Result<()> {
        if self.torn_down {
            return Ok(());
        }
        // Unpause any paused containers so docker compose down doesn't
        // wait 10s per container for SIGTERM delivery.
        let _ = Command::new("docker")
            .arg("compose")
            .arg("-f")
            .arg(&self.compose)
            .arg("unpause")
            .status();
        let status = Command::new("docker")
            .arg("compose")
            .arg("-f")
            .arg(&self.compose)
            .arg("down")
            .arg("-v")
            .status()
            .context("failed to invoke `docker compose down`")?;
        self.torn_down = true;
        if !status.success() {
            bail!("docker compose down failed");
        }
        Ok(())
    }

    /// Leave the cluster running (e.g. for `make e2e-up` interactive use).
    pub fn forget(mut self) {
        self.torn_down = true;
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        let _ = self.down();
    }
}

/// Execute `etcdctl get --prefix <prefix>` against the etcd container
/// and parse the result. Empty result means no keys match.
pub fn etcdctl_get_prefix(prefix: &str) -> Result<Vec<KV>> {
    let out = Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(docker_dir().join("docker-compose.yaml"))
        .arg("exec")
        .arg("-T")
        .arg("etcd")
        .arg("etcdctl")
        .arg("get")
        .arg("--prefix")
        .arg(prefix)
        .arg("--print-value-only=false")
        .output()
        .context("failed to invoke etcdctl")?;
    if !out.status.success() {
        bail!(
            "etcdctl get --prefix {prefix} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // etcdctl text output: alternating key / value lines.
    let mut kvs = Vec::new();
    let lines: Vec<&[u8]> = out.stdout.split(|&b| b == b'\n').collect();
    let mut i = 0;
    while i + 1 < lines.len() {
        let k = String::from_utf8_lossy(lines[i]).to_string();
        if k.is_empty() {
            i += 1;
            continue;
        }
        let v = lines[i + 1].to_vec();
        kvs.push(KV { key: k, value: v });
        i += 2;
    }
    Ok(kvs)
}

/// Poll etcd for keys under `prefix` until `predicate` returns true or
/// `timeout` elapses. Backs off 100ms..500ms; never sleeps unconditionally.
pub fn wait_for_etcd_key<F>(prefix: &str, predicate: F, timeout: Duration) -> Result<Vec<KV>>
where
    F: Fn(&[KV]) -> bool,
{
    let deadline = Instant::now() + timeout;
    let mut backoff = Duration::from_millis(100);
    loop {
        let kvs = etcdctl_get_prefix(prefix).unwrap_or_default();
        if predicate(&kvs) {
            return Ok(kvs);
        }
        if Instant::now() >= deadline {
            bail!(
                "wait_for_etcd_key timed out after {:?} on prefix {prefix} \
                 (last {} keys): predicate not satisfied",
                timeout,
                kvs.len()
            );
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_millis(500));
    }
}

/// Dump etcd state and per-container logs to
/// `e2e-artifacts/<test_name>/`. Called from test failure paths so red
/// runs are diagnosable.
///
/// Produces:
/// - `etcd-prefix.txt` — full `/boi/` etcd dump
/// - `<service>.log` — `docker logs` for each compose service
/// - `trace.json` — placeholder for proto RPC trace (Phase 1+ wires this)
pub fn dump_artifacts(test_name: &str) -> Result<PathBuf> {
    let dir = artifacts_root().join(test_name);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create_dir_all {}", dir.display()))?;

    let etcd = Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(docker_dir().join("docker-compose.yaml"))
        .arg("exec")
        .arg("-T")
        .arg("etcd")
        .arg("etcdctl")
        .arg("get")
        .arg("--prefix")
        .arg("/boi/")
        .output();
    if let Ok(out) = etcd {
        let _ = std::fs::write(dir.join("etcd-prefix.txt"), &out.stdout);
    }

    for svc in ["etcd", "node-a", "node-b", "node-c", "plugin-sidecar"] {
        if let Ok(out) = Command::new("docker")
            .arg("compose")
            .arg("-f")
            .arg(docker_dir().join("docker-compose.yaml"))
            .arg("logs")
            .arg("--no-color")
            .arg(svc)
            .output()
        {
            let _ = std::fs::write(dir.join(format!("{svc}.log")), &out.stdout);
        }
    }

    let _ = std::fs::write(
        dir.join("trace.json"),
        b"{\"note\":\"proto RPC trace placeholder - wired in Phase 1+\"}",
    );
    Ok(dir)
}

/// True if a `docker` binary is on PATH. Tests can early-skip with a
/// clear message rather than panicking when run outside CI.
pub fn docker_available() -> bool {
    Command::new("docker")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Resolve the Docker Compose container name for a service.
fn compose_container_name(service: &str) -> Result<String> {
    let out = Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(docker_dir().join("docker-compose.yaml"))
        .arg("ps")
        .arg("-q")
        .arg(service)
        .output()
        .with_context(|| format!("docker compose ps -q {service}"))?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() {
        bail!("no container found for service {service}");
    }
    Ok(name)
}

/// Resolve the actual Docker network name for the boi-test network.
fn compose_network_name() -> Result<String> {
    let out = Command::new("docker")
        .arg("network")
        .arg("ls")
        .arg("--filter")
        .arg("name=boi-test")
        .arg("--format")
        .arg("{{.Name}}")
        .output()
        .context("docker network ls")?;
    let names = String::from_utf8_lossy(&out.stdout);
    let name = names.lines().next().unwrap_or("").trim().to_string();
    if name.is_empty() {
        bail!("boi-test network not found");
    }
    Ok(name)
}

/// Disconnect a compose service from the boi-test network, using the
/// correct container ID and network name (handles Docker Compose project
/// name prefixing).
pub fn network_disconnect(service: &str) -> Result<()> {
    let container = compose_container_name(service)?;
    let network = compose_network_name()?;
    let out = Command::new("docker")
        .arg("network")
        .arg("disconnect")
        .arg(&network)
        .arg(&container)
        .output()
        .with_context(|| format!("docker network disconnect {network} {container}"))?;
    if !out.status.success() {
        bail!(
            "docker network disconnect failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// Reconnect a compose service to the boi-test network.
pub fn network_connect(service: &str) -> Result<()> {
    let container = compose_container_name(service)?;
    let network = compose_network_name()?;
    let out = Command::new("docker")
        .arg("network")
        .arg("connect")
        .arg(&network)
        .arg(&container)
        .output()
        .with_context(|| format!("docker network connect {network} {container}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.contains("already") {
            bail!("docker network connect failed: {stderr}");
        }
    }
    Ok(())
}

/// Pause a compose service (freezes all processes — reliable for simulating
/// node failure without container restart). The daemon's lease keepalive
/// stops, so etcd revokes the lease after the TTL.
pub fn compose_pause(service: &str) -> Result<()> {
    let out = Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(docker_dir().join("docker-compose.yaml"))
        .arg("pause")
        .arg(service)
        .output()
        .with_context(|| format!("docker compose pause {service}"))?;
    if !out.status.success() {
        bail!(
            "docker compose pause {service} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// Unpause a compose service (resumes frozen processes).
pub fn compose_unpause(service: &str) -> Result<()> {
    let out = Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(docker_dir().join("docker-compose.yaml"))
        .arg("unpause")
        .arg(service)
        .output()
        .with_context(|| format!("docker compose unpause {service}"))?;
    if !out.status.success() {
        bail!(
            "docker compose unpause {service} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// Convenience: assert the harness can locate a path inside the workspace.
pub fn must_exist(p: &Path) -> Result<()> {
    if !p.exists() {
        return Err(anyhow!("expected path missing: {}", p.display()));
    }
    Ok(())
}
