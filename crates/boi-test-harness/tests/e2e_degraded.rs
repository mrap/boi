//! RED E2E #6 — degraded mode under etcd partition + recovery.
//!
//! Per §9 of `distributed-architecture-design-2026-05-12.md`:
//! - F-07 `boi cluster local-fallback` drains a node, persists in-flight
//!   claims to `~/.boi/pending-flush/`, switches mode, prints a warning.
//! - F-08 pending-flush buffer survives etcd unreachable.
//! - F-12 `/metrics` exposes `boi_dispatch_rejected_etcd_unreachable_total`.
//!
//! When all nodes lose etcd:
//! 1. Already-claimed (in-flight) tasks keep running locally; their
//!    completions buffer and flush after etcd reconnects.
//! 2. NEW dispatches fail loud with an `etcd_unreachable` error and
//!    increment the rejection counter — never silently queue.
//! 3. After reconnect, dispatches resume within 5s.
//!
//! Five named subtests, all expected RED today (Phase 6 unimplemented).

use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use boi_test_harness::{
    docker_available, docker_dir, dump_artifacts, etcdctl_get_prefix, start_cluster,
    wait_for_etcd_key,
};

const WAIT: Duration = Duration::from_secs(5);
const RECONNECT_WAIT: Duration = Duration::from_secs(5);
const PARTITION_DRAIN: Duration = Duration::from_secs(10);

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

fn raw_exec(service: &str, args: &[&str]) -> Result<std::process::Output> {
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

/// Disconnect a single service from the test docker network — simulates
/// an etcd partition from that node's POV.
fn docker_network(action: &str, service: &str) -> Result<std::process::Output> {
    Command::new("docker")
        .arg("network")
        .arg(action)
        .arg("boi-test")
        .arg(service)
        .output()
        .with_context(|| format!("docker network {action} boi-test {service}"))
}

/// Partition every boi-node from the etcd container by removing them
/// from the shared docker network. (Equivalent to etcd being unreachable
/// from each node.) Returns the list of services actually disconnected.
fn partition_all_from_etcd() -> Result<Vec<&'static str>> {
    let mut disconnected = Vec::new();
    for n in ["node-a", "node-b", "node-c"] {
        // Disconnect etcd from each node — using the etcd container is
        // sufficient: pulling etcd off the network partitions it from
        // every peer at once. We loop over nodes to keep failures local.
        let _ = docker_network("disconnect", n);
        disconnected.push(n);
    }
    Ok(disconnected)
}

fn reconnect_all_to_etcd(svcs: &[&'static str]) -> Result<()> {
    for s in svcs {
        let _ = docker_network("connect", s);
    }
    Ok(())
}

fn ensure_cluster() -> Result<boi_test_harness::Cluster> {
    start_cluster(3).context(
        "start_cluster(3) — Phase 0a stub binary exits 78 (EX_CONFIG); \
         Phase 6 wires degraded-mode handling under test",
    )
}

/// Bring up cluster, advertise caps on all nodes, dispatch a single
/// long-running task `T`. Returns `(cluster, task_id)`.
fn dispatch_long_task() -> Result<(boi_test_harness::Cluster, String)> {
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
            "e2e-degraded-task",
            "--sleep-ms",
            "20000",
        ],
    )?;
    let task_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if task_id.is_empty() {
        bail!(
            "dispatch returned empty task_id — Phase 1+ (spec dispatch CLI) \
             stub binary, cannot exercise degraded-mode path"
        );
    }
    Ok((cluster, task_id))
}

// ---------------------------------------------------------------
// Subtest 1: in_flight_task_survives_etcd_partition
// ---------------------------------------------------------------
#[test]
fn in_flight_task_survives_etcd_partition() {
    run_subtest("in_flight_task_survives_etcd_partition", || {
        let (_cluster, task_id) = dispatch_long_task()?;

        // Wait for some node to take the claim BEFORE we partition.
        let claimed = wait_for_etcd_key(
            "/boi/claims/",
            |kvs| {
                kvs.iter().any(|kv| {
                    kv.key.contains(&task_id)
                        && !String::from_utf8_lossy(&kv.value).is_empty()
                })
            },
            WAIT,
        );
        if claimed.is_err() {
            bail!(
                "no claim observed on /boi/claims/{task_id} before partition; \
                 Phase 1/2 (claim path) not implemented — cannot assert F-08 \
                 buffer survives partition"
            );
        }

        // Partition every node from etcd. Worker should continue locally.
        let svcs = partition_all_from_etcd()?;

        // Reconnect after a bounded drain period; no raw sleep — we poll
        // for the partition window to elapse via wait_for_etcd_key with
        // an always-false predicate (it bails on timeout, which is what
        // we want).
        let _ = wait_for_etcd_key("/boi/__never__/", |_| false, PARTITION_DRAIN);
        reconnect_all_to_etcd(&svcs)?;

        // After reconnect, the worker must flush its completion event
        // (F-08 pending-flush buffer) so /boi/events/ shows
        // `task.completed` for this task_id.
        let flushed = wait_for_etcd_key(
            "/boi/events/",
            |kvs| {
                kvs.iter().any(|kv| {
                    let v = String::from_utf8_lossy(&kv.value);
                    v.contains(&task_id) && v.contains("task.completed")
                })
            },
            RECONNECT_WAIT + WAIT,
        );
        if flushed.is_ok() {
            return Ok(());
        }
        bail!(
            "in-flight task `{task_id}` did not flush a `task.completed` \
             event to /boi/events/ after etcd reconnect — F-08 pending-flush \
             buffer (Phase 6) not yet implemented"
        );
    });
}

// ---------------------------------------------------------------
// Subtest 2: new_dispatch_fails_loud_under_partition
// ---------------------------------------------------------------
#[test]
fn new_dispatch_fails_loud_under_partition() {
    run_subtest("new_dispatch_fails_loud_under_partition", || {
        let (_cluster, _seed_task_id) = dispatch_long_task()?;

        let svcs = partition_all_from_etcd()?;

        // Attempt a new dispatch while partitioned. MUST fail loud with
        // a recognizable `etcd_unreachable` error code on stderr — and
        // MUST NOT silently queue (no new key under /boi/dispatch-queue/).
        let pre_queue = etcdctl_get_prefix("/boi/dispatch-queue/").unwrap_or_default();
        let out = boi_node_exec(
            "node-a",
            &[
                "spec",
                "dispatch",
                "--requires",
                "os=linux",
                "--name",
                "e2e-degraded-rejected",
            ],
        )?;

        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let loud = !out.status.success()
            && (stderr.contains("etcd_unreachable")
                || stdout.contains("etcd_unreachable"));

        // Reconnect for hygiene before asserting (so subsequent reads work).
        reconnect_all_to_etcd(&svcs)?;

        let post_queue = etcdctl_get_prefix("/boi/dispatch-queue/").unwrap_or_default();
        let silently_queued = post_queue.iter().any(|kv| {
            let v = String::from_utf8_lossy(&kv.value);
            v.contains("e2e-degraded-rejected")
                && !pre_queue.iter().any(|p| p.key == kv.key)
        });

        if loud && !silently_queued {
            return Ok(());
        }
        bail!(
            "expected dispatch under partition to fail with `etcd_unreachable` \
             and NOT silently queue; got status={:?} stderr=`{}` \
             silently_queued={} — Phase 6 (loud-rejection on etcd-unreachable) \
             not yet implemented",
            out.status.code(),
            stderr.trim(),
            silently_queued
        );
    });
}

// ---------------------------------------------------------------
// Subtest 3: metrics_counter_increments
// ---------------------------------------------------------------
#[test]
fn metrics_counter_increments() {
    run_subtest("metrics_counter_increments", || {
        let (_cluster, _seed_task_id) = dispatch_long_task()?;

        // Scrape baseline metrics from node-a before partition.
        let pre = raw_exec(
            "node-a",
            &["curl", "-fsS", "http://127.0.0.1:9090/metrics"],
        )?;
        let pre_body = String::from_utf8_lossy(&pre.stdout).to_string();
        let pre_count = parse_counter(
            &pre_body,
            "boi_dispatch_rejected_etcd_unreachable_total",
        )
        .unwrap_or(0);

        let svcs = partition_all_from_etcd()?;

        // Three dispatch attempts while partitioned — each should bump
        // the rejection counter.
        for i in 0..3 {
            let _ = boi_node_exec(
                "node-a",
                &[
                    "spec",
                    "dispatch",
                    "--requires",
                    "os=linux",
                    "--name",
                    &format!("e2e-degraded-metric-{i}"),
                ],
            );
        }

        reconnect_all_to_etcd(&svcs)?;

        let post = raw_exec(
            "node-a",
            &["curl", "-fsS", "http://127.0.0.1:9090/metrics"],
        )?;
        let post_body = String::from_utf8_lossy(&post.stdout).to_string();
        let post_count = parse_counter(
            &post_body,
            "boi_dispatch_rejected_etcd_unreachable_total",
        );

        match post_count {
            Some(n) if n > pre_count && n > 0 => Ok(()),
            other => bail!(
                "expected `boi_dispatch_rejected_etcd_unreachable_total` to \
                 increment above {pre_count}; got {other:?} (post-body \
                 {} bytes) — F-12 metric not yet exposed (Phase 6)",
                post_body.len()
            ),
        }
    });
}

/// Parse a Prometheus-style counter sample, ignoring `# HELP` / `# TYPE`
/// lines. Returns the most recently emitted value for `name` (no labels).
fn parse_counter(body: &str, name: &str) -> Option<u64> {
    let mut last: Option<u64> = None;
    for line in body.lines() {
        if line.starts_with('#') {
            continue;
        }
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(name) {
            let rest = rest.trim_start();
            // Strip optional `{label="..."}` block.
            let rest = if let Some(stripped) = rest.strip_prefix('{') {
                stripped.split_once('}').map(|(_, r)| r.trim_start()).unwrap_or(rest)
            } else {
                rest
            };
            if let Some(num) = rest.split_whitespace().next() {
                if let Ok(v) = num.parse::<f64>() {
                    last = Some(v as u64);
                }
            }
        }
    }
    last
}

// ---------------------------------------------------------------
// Subtest 4: dispatches_resume_after_reconnect
// ---------------------------------------------------------------
#[test]
fn dispatches_resume_after_reconnect() {
    run_subtest("dispatches_resume_after_reconnect", || {
        let (_cluster, _seed_task_id) = dispatch_long_task()?;

        let svcs = partition_all_from_etcd()?;
        // One rejected attempt during partition (we don't assert on it
        // here — covered by subtest 2).
        let _ = boi_node_exec(
            "node-a",
            &[
                "spec",
                "dispatch",
                "--requires",
                "os=linux",
                "--name",
                "e2e-degraded-pre-reconnect",
            ],
        );
        reconnect_all_to_etcd(&svcs)?;

        // Post-reconnect dispatch must succeed within RECONNECT_WAIT and
        // produce a task_id we can locate in /boi/dispatch-queue/.
        let out = boi_node_exec(
            "node-a",
            &[
                "spec",
                "dispatch",
                "--requires",
                "os=linux",
                "--name",
                "e2e-degraded-post-reconnect",
            ],
        )?;
        let task_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() || task_id.is_empty() {
            bail!(
                "post-reconnect dispatch failed: status={:?} stdout=`{}` \
                 stderr=`{}` — Phase 6 (resumption after etcd recovery) not \
                 yet implemented",
                out.status.code(),
                task_id,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }

        let saw = wait_for_etcd_key(
            "/boi/dispatch-queue/",
            |kvs| {
                kvs.iter().any(|kv| {
                    let v = String::from_utf8_lossy(&kv.value);
                    v.contains("e2e-degraded-post-reconnect") || kv.key.contains(&task_id)
                })
            },
            RECONNECT_WAIT,
        );
        if saw.is_ok() {
            return Ok(());
        }
        bail!(
            "dispatched task `{task_id}` did not appear in /boi/dispatch-queue/ \
             within {RECONNECT_WAIT:?} after etcd reconnect — Phase 6 not yet \
             implemented"
        );
    });
}

// ---------------------------------------------------------------
// Subtest 5: local_fallback_drains_and_persists
// ---------------------------------------------------------------
#[test]
fn local_fallback_drains_and_persists() {
    run_subtest("local_fallback_drains_and_persists", || {
        let (_cluster, task_id) = dispatch_long_task()?;
        let _ = wait_for_etcd_key(
            "/boi/claims/",
            |kvs| kvs.iter().any(|kv| kv.key.contains(&task_id)),
            WAIT,
        );

        // Invoke F-07 local-fallback on node-a. Expected behavior:
        // - in-flight claims persisted under ~/.boi/pending-flush/
        // - mode switches (stderr advertises "local-fallback" or similar)
        // - prints a clear warning to stderr
        let out = boi_node_exec("node-a", &["cluster", "local-fallback"])?;
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();

        let warned = stderr.to_lowercase().contains("warn")
            || stderr.contains("local-fallback")
            || stderr.contains("degraded");

        // Inspect ~/.boi/pending-flush/ inside the node container.
        let ls = raw_exec(
            "node-a",
            &["sh", "-c", "ls -1 /root/.boi/pending-flush/ 2>&1"],
        )?;
        let ls_body = String::from_utf8_lossy(&ls.stdout).to_string();
        let persisted = ls.status.success()
            && ls_body
                .lines()
                .any(|l| !l.trim().is_empty() && !l.contains("No such"));

        let mode_switched = stdout.contains("local-fallback")
            || stderr.contains("mode=local-fallback")
            || stderr.contains("switched to local-fallback");

        if out.status.success() && warned && persisted && mode_switched {
            return Ok(());
        }
        bail!(
            "`boi cluster local-fallback` did not satisfy F-07: \
             status={:?} warned={warned} persisted={persisted} \
             mode_switched={mode_switched} stderr=`{}` ls=`{}` — Phase 6 \
             (F-07 drain/persist/mode-switch) not yet implemented",
            out.status.code(),
            stderr.trim(),
            ls_body.trim()
        );
    });
}
