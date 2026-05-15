//! RED E2E #7 — worker stdout tail durability (disconnect + reattach).
//!
//! Per §5.2 (Pool plugin) and §16 Q7 (worker stdout durability): a long
//! task on node-a writes structured stdout. A CLI tailing it from
//! node-b disconnects; reattach from node-c via
//! `boi spec tail <task_id> --follow`. The stream must resume from the
//! last byte without a gap. Per Q7 retention: rotate oldest task log
//! once the per-spec on-disk total exceeds 100 MB (or 7d age cap).
//!
//! Five named subtests, all expected RED today (Phase 7 unimplemented).

use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use boi_test_harness::{
    docker_available, docker_dir, dump_artifacts, etcdctl_get_prefix, network_connect,
    network_disconnect, start_cluster, wait_for_etcd_key,
};

const WAIT: Duration = Duration::from_secs(5);
const TAIL_LAG_BUDGET_MS: u128 = 1000;

fn run_subtest(name: &str, body: impl FnOnce() -> Result<()>) {
    if !docker_available() {
        eprintln!("SKIP {name}: docker not on PATH");
        return;
    }
    match body() {
        Ok(()) => {},
        Err(e) => {
            let _ = dump_artifacts(name);
            panic!("RED [{name}] {e:#}");
        }
    }
}

fn compose_path() -> std::path::PathBuf {
    docker_dir().join("docker-compose.yaml")
}

fn boi_node_exec(service: &str, args: &[&str]) -> Result<std::process::Output> {
    Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(compose_path())
        .arg("exec")
        .arg("-T")
        .arg(service)
        .arg("boi-node")
        .args(args)
        .output()
        .with_context(|| format!("invoke `docker compose exec {service} boi-node ...`"))
}

fn container_exec(service: &str, args: &[&str]) -> Result<std::process::Output> {
    let mut cmd = Command::new("docker");
    cmd.arg("compose")
        .arg("-f")
        .arg(compose_path())
        .arg("exec")
        .arg("-T")
        .arg(service);
    for a in args {
        cmd.arg(a);
    }
    cmd.output()
        .with_context(|| format!("invoke `docker compose exec {service} {args:?}`"))
}

fn docker_network_action(action: &str, service: &str) -> Result<()> {
    match action {
        "disconnect" => network_disconnect(service),
        "connect" => network_connect(service),
        _ => Ok(()),
    }
}

fn ensure_cluster() -> Result<boi_test_harness::Cluster> {
    start_cluster(3).context(
        "start_cluster(3) — Phase 0a stub binary exits 78 (EX_CONFIG); \
         Phase 7 wires the stdout tee/tail path under test",
    )
}

/// Detect which node claimed a task by reading /boi/claims/.
fn detect_claimant(task_id: &str) -> String {
    let kvs = etcdctl_get_prefix("/boi/claims/").unwrap_or_default();
    kvs.iter()
        .filter(|kv| kv.key.contains(task_id) && !kv.key.contains("/claim_lease_id"))
        .find_map(|kv| {
            let v = String::from_utf8_lossy(&kv.value).to_string();
            serde_json::from_str::<serde_json::Value>(&v)
                .ok()
                .and_then(|p| p.get("node_id").and_then(|n| n.as_str()).map(String::from))
        })
        .unwrap_or_else(|| "node-a".to_string())
}

/// Common setup: init cluster, advertise caps so any node claims, dispatch
/// a long-running task that streams structured stdout via the
/// `boi-node internal emit-stdout` helper. Returns (cluster, spec_id,
/// task_id, claimant).
fn dispatch_long_streaming_task() -> Result<(boi_test_harness::Cluster, String, String, String)> {
    let cluster = ensure_cluster()?;
    let _ = boi_node_exec("node-a", &["cluster", "init"]);
    for n in ["node-a", "node-b", "node-c"] {
        let _ = Command::new("docker")
            .arg("compose")
            .arg("-f")
            .arg(compose_path())
            .arg("exec")
            .arg("-T")
            .arg("-e")
            .arg("BOI_CAPS_STATIC=os=linux,runtime=generic")
            .arg(n)
            .arg("boi-node")
            .arg("node")
            .arg("advertise")
            .output();
    }
    let out = boi_node_exec(
        "node-a",
        &[
            "spec",
            "dispatch",
            "--requires",
            "os=linux",
            "--name",
            "e2e-stdout-tail",
            "--stream-stdout",
            "rate=200lps,duration=30s",
        ],
    )?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    // Expected format once Phase 7 lands: `spec_id<TAB>task_id` on stdout.
    let mut parts = stdout.split_whitespace();
    let spec_id = parts.next().unwrap_or_default().to_string();
    let task_id = parts.next().unwrap_or_default().to_string();
    if spec_id.is_empty() || task_id.is_empty() {
        bail!(
            "dispatch did not return `<spec_id> <task_id>`; raw stdout=`{stdout}` \
             stderr=`{}` — Phase 7 wires the streaming-stdout dispatch flag",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // Wait for the claim to land so we know which node is the claimant.
    let _ = wait_for_etcd_key(
        "/boi/claims/",
        |kvs| kvs.iter().any(|kv| kv.key.contains(&task_id) && !kv.key.contains("/claim_lease_id")),
        WAIT,
    );
    let claimant = detect_claimant(&task_id);
    Ok((cluster, spec_id, task_id, claimant))
}

// ---------------------------------------------------------------
// Subtest 1: stdout_tee_to_disk
// ---------------------------------------------------------------
#[test]
fn stdout_tee_to_disk() {
    run_subtest("stdout_tee_to_disk", || {
        let (_cluster, spec_id, task_id, claimant) = dispatch_long_streaming_task()?;
        let path = format!("/root/.boi/logs/{spec_id}/{task_id}.log");

        let saw_growth = wait_for_etcd_key(
            &format!("/boi/tail-offsets/{task_id}"),
            |kvs| {
                kvs.iter().any(|kv| {
                    String::from_utf8_lossy(&kv.value)
                        .trim()
                        .parse::<u64>()
                        .map(|n| n > 0)
                        .unwrap_or(false)
                })
            },
            WAIT,
        );

        let first = container_exec(&claimant, &["stat", "-c", "%s", &path])
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let second = container_exec(&claimant, &["stat", "-c", "%s", &path])
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();

        let first_n: u64 = first.parse().unwrap_or(0);
        let second_n: u64 = second.parse().unwrap_or(0);

        if saw_growth.is_ok() && first_n > 0 && second_n >= first_n {
            return Ok(());
        }
        bail!(
            "expected stdout tee'd to `{path}` on {claimant} to exist and grow; got \
             first_size={first_n} second_size={second_n} tail_offset_seen={} \
             — Phase 7 (stdout tee-to-disk under \
             /boi/<node>/.boi/logs/<spec_id>/<task_id>.log) not yet implemented",
            saw_growth.is_ok()
        );
    });
}

// ---------------------------------------------------------------
// Subtest 2: tail_command_streams
// ---------------------------------------------------------------
#[test]
fn tail_command_streams() {
    run_subtest("tail_command_streams", || {
        let (_cluster, _spec_id, task_id, _claimant) = dispatch_long_streaming_task()?;

        // Capture `boi spec tail --since-bytes=0 --max-bytes=4096`
        // from node-b. The Phase 7 CLI must emit the first chunk
        // (>=1 byte) within TAIL_LAG_BUDGET_MS once the task starts
        // streaming. We bound wall time via the WAIT poll, not sleep.
        let started = std::time::Instant::now();
        let _ = wait_for_etcd_key(
            &format!("/boi/tail-offsets/{task_id}"),
            |kvs| {
                kvs.iter().any(|kv| {
                    String::from_utf8_lossy(&kv.value)
                        .trim()
                        .parse::<u64>()
                        .map(|n| n > 0)
                        .unwrap_or(false)
                })
            },
            WAIT,
        );

        let out = boi_node_exec(
            "node-b",
            &[
                "spec",
                "tail",
                &task_id,
                "--since-bytes",
                "0",
                "--max-bytes",
                "4096",
            ],
        )?;
        let lag = started.elapsed().as_millis();
        let bytes = out.stdout.len() as u64;

        if out.status.success() && bytes > 0 && lag <= TAIL_LAG_BUDGET_MS {
            return Ok(());
        }
        bail!(
            "expected `boi spec tail {task_id}` from node-b to emit \
             >=1 byte within {TAIL_LAG_BUDGET_MS}ms; got status={:?} \
             bytes={bytes} lag_ms={lag} stderr=`{}` — Phase 7 (`boi spec \
             tail --follow` + claimant Tail RPC) not yet implemented",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    });
}

// ---------------------------------------------------------------
// Subtest 3: disconnect_reattach_no_gap
// ---------------------------------------------------------------
#[test]
fn disconnect_reattach_no_gap() {
    run_subtest("disconnect_reattach_no_gap", || {
        let (_cluster, spec_id, task_id, claimant) = dispatch_long_streaming_task()?;
        let path = format!("/root/.boi/logs/{spec_id}/{task_id}.log");

        // Pick two non-claimant nodes for tailing.
        let non_claimants: Vec<&str> = ["node-a", "node-b", "node-c"]
            .iter()
            .copied()
            .filter(|n| *n != claimant.as_str())
            .collect();
        let tailer1 = non_claimants[0];
        let tailer2 = non_claimants[1];

        let first = boi_node_exec(
            tailer1,
            &[
                "spec",
                "tail",
                &task_id,
                "--since-bytes",
                "0",
                "--max-bytes",
                "8192",
                "--print-offset",
            ],
        )?;
        let first_stdout = first.stdout.clone();
        let resume_offset: u64 = std::str::from_utf8(&first.stderr)
            .ok()
            .and_then(|s| s.lines().find_map(|l| l.strip_prefix("offset=")))
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        docker_network_action("disconnect", tailer1)?;
        // Let the task continue producing bytes; wait until the on-disk
        // offset is well past `resume_offset` before reattach.
        let _ = wait_for_etcd_key(
            &format!("/boi/tail-offsets/{task_id}"),
            |kvs| {
                kvs.iter().any(|kv| {
                    String::from_utf8_lossy(&kv.value)
                        .trim()
                        .parse::<u64>()
                        .map(|n| n > resume_offset + 4096)
                        .unwrap_or(false)
                })
            },
            WAIT,
        );

        let second = boi_node_exec(
            tailer2,
            &[
                "spec",
                "tail",
                &task_id,
                "--since-bytes",
                &resume_offset.to_string(),
                "--max-bytes",
                "8192",
            ],
        )?;
        let second_stdout = second.stdout.clone();

        // Compare the concatenation of (first, second) against the
        // canonical on-disk log slice [0 .. first.len()+second.len()].
        let total_len = first_stdout.len() + second_stdout.len();
        let on_disk = container_exec(
            &claimant,
            &[
                "dd",
                &format!("if={path}"),
                "bs=1",
                "count=0",
                &format!("skip=0"),
            ],
        );
        let canonical = container_exec(
            &claimant,
            &["sh", "-c", &format!("head -c {total_len} {path}")],
        )?;

        let mut joined = Vec::with_capacity(total_len);
        joined.extend_from_slice(&first_stdout);
        joined.extend_from_slice(&second_stdout);

        if on_disk.is_ok() && canonical.status.success() && joined == canonical.stdout && total_len > 0
        {
            return Ok(());
        }
        bail!(
            "expected `tail(0..N1) ++ tail({resume_offset}..N1+N2)` from \
             {tailer1} then {tailer2} to byte-equal the on-disk prefix of \
             `{path}`; got first_bytes={} second_bytes={} canonical_bytes={} \
             equal={} — Phase 7 (durable tail offsets + cross-node Tail RPC \
             resume) not yet implemented",
            first_stdout.len(),
            second_stdout.len(),
            canonical.stdout.len(),
            joined == canonical.stdout,
        );
    });
}

// ---------------------------------------------------------------
// Subtest 4: retention_7d_or_100mb_caps
// ---------------------------------------------------------------
#[test]
fn retention_7d_or_100mb_caps() {
    run_subtest("retention_7d_or_100mb_caps", || {
        let (_cluster, spec_id, task_id, claimant) = dispatch_long_streaming_task()?;
        let cur = format!("/root/.boi/logs/{spec_id}/{task_id}.log");
        let old_task = format!("rotme-{task_id}");
        let old = format!("/root/.boi/logs/{spec_id}/{old_task}.log");

        container_exec(
            &claimant,
            &[
                "sh",
                "-c",
                &format!(
                    "mkdir -p /root/.boi/logs/{spec_id} && \
                     dd if=/dev/zero of={old} bs=1M count=110 status=none && \
                     touch -d '8 days ago' {old}"
                ),
            ],
        )?;

        let out = boi_node_exec(
            &claimant,
            &["internal", "retention-sweep", "--spec-id", &spec_id],
        )?;
        if !out.status.success() {
            bail!(
                "`internal retention-sweep` failed: status={:?} stderr=`{}` \
                 — Phase 7 (Q7 retention: 7d age cap OR 100MB per-spec on-disk \
                 cap) not yet implemented",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }

        let old_gone = container_exec(&claimant, &["test", "-e", &old])
            .map(|o| !o.status.success())
            .unwrap_or(false);
        let cur_present = container_exec(&claimant, &["test", "-s", &cur])
            .map(|o| o.status.success())
            .unwrap_or(false);

        if old_gone && cur_present {
            return Ok(());
        }
        bail!(
            "expected oldest task log `{old}` to be rotated out and \
             current task log `{cur}` to keep growing; got old_gone={old_gone} \
             cur_present={cur_present} — Phase 7 retention (oldest-first \
             rotation under 100MB/7d cap) not yet implemented"
        );
    });
}

// ---------------------------------------------------------------
// Subtest 5: tail_resolves_via_etcd
// ---------------------------------------------------------------
#[test]
fn tail_resolves_via_etcd() {
    run_subtest("tail_resolves_via_etcd", || {
        let (_cluster, _spec_id, task_id, claimant) = dispatch_long_streaming_task()?;

        // Pick a non-claimant node for the tail request.
        let tailer = if claimant == "node-c" { "node-b" } else { "node-c" };

        let out = boi_node_exec(
            tailer,
            &[
                "spec",
                "tail",
                &task_id,
                "--since-bytes",
                "0",
                "--max-bytes",
                "256",
            ],
        )?;

        let trace_key = format!("/boi/traces/rpc/{claimant}/Tail");
        let trace_seen = wait_for_etcd_key(
            &trace_key,
            |kvs| {
                kvs.iter().any(|kv| {
                    String::from_utf8_lossy(&kv.value)
                        .trim()
                        .parse::<u64>()
                        .map(|n| n >= 1)
                        .unwrap_or(false)
                })
            },
            WAIT,
        );

        let claims = etcdctl_get_prefix("/boi/claims/").unwrap_or_default();
        let resolves_to_claimant = claims.iter().any(|kv| {
            kv.key.contains(&task_id)
                && String::from_utf8_lossy(&kv.value).contains(&claimant)
        });

        if out.status.success() && out.stdout.len() > 0 && trace_seen.is_ok() && resolves_to_claimant {
            return Ok(());
        }
        bail!(
            "expected `boi spec tail {task_id}` from {tailer} to resolve \
             claimant via /boi/claims/ and open a Tail RPC against {claimant} \
             (observed via {trace_key} counter); got \
             status={:?} bytes={} trace_seen={} resolves_to_claimant={} stderr=`{}` \
             — Phase 7 (claimant resolution + internal Tail RPC) not yet \
             implemented",
            out.status.code(),
            out.stdout.len(),
            trace_seen.is_ok(),
            resolves_to_claimant,
            String::from_utf8_lossy(&out.stderr).trim(),
        );
    });
}
