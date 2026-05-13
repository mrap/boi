//! RED E2E #8 — audit-tier hooks durability (Q6).
//!
//! Per §5.5 + Q6: a hooks plugin declaring `delivery_tier: audit` in its
//! manifest must receive at-least-once delivery backed by a local-disk
//! WAL on the emitting node. Events are written to the WAL BEFORE any
//! delivery attempt, survive plugin crashes and node restarts, advance a
//! monotonic high-water-mark stored under `/boi/hooks-hwm/{node}/{plugin}`,
//! exert back-pressure on the emitting workflow when the plugin stalls,
//! and dedup downstream via the `(node_id, seq, kind, ts)` key. A
//! `best_effort` plugin keeps the §5.5 fire-and-forget semantics (no WAL,
//! no HWM).
//!
//! Six named subtests, all expected RED today (Phase 8 unimplemented).

use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use boi_test_harness::{
    docker_available, docker_dir, dump_artifacts, etcdctl_get_prefix, start_cluster,
    wait_for_etcd_key,
};

const WAIT: Duration = Duration::from_secs(10);
const AUDIT_PLUGIN: &str = "audit-shipper";
const BEST_EFFORT_PLUGIN: &str = "notify-slack";

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

fn docker_exec_raw(service: &str, args: &[&str]) -> Result<std::process::Output> {
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

fn ensure_cluster() -> Result<boi_test_harness::Cluster> {
    start_cluster(2).context(
        "start_cluster(2) — Phase 0a stub binary exits 78 (EX_CONFIG); \
         Phase 8 wires the audit-tier hooks WAL + HWM path under test",
    )
}

/// Register an `audit` tier plugin manifest and emit `count` synthetic
/// events of kind `task.completed`. Returns the cluster handle so the
/// caller can keep it alive across the subtest.
fn dispatch_audit_plugin(count: usize) -> Result<boi_test_harness::Cluster> {
    let cluster = ensure_cluster()?;
    let _ = boi_node_exec("node-a", &["cluster", "init"]);
    let _ = boi_node_exec(
        "node-a",
        &[
            "plugin",
            "register",
            "--id",
            AUDIT_PLUGIN,
            "--kind",
            "hooks",
            "--delivery-tier",
            "audit",
            "--subscribed-kinds",
            "task.completed",
        ],
    )?;
    let _ = boi_node_exec(
        "node-a",
        &[
            "internal",
            "hooks-emit-burst",
            "--plugin",
            AUDIT_PLUGIN,
            "--kind",
            "task.completed",
            "--count",
            &count.to_string(),
        ],
    )?;
    Ok(cluster)
}

// ---------------------------------------------------------------
// Subtest 1: audit_events_wal_persisted
// ---------------------------------------------------------------
#[test]
fn audit_events_wal_persisted() {
    run_subtest("audit_events_wal_persisted", || {
        let _cluster = dispatch_audit_plugin(100)?;

        // The WAL file must exist on the emitting node container and
        // contain exactly 100 lines after the emit burst settles.
        let wal_path = format!("/root/.boi/hooks-wal/{AUDIT_PLUGIN}.jsonl");
        let out = docker_exec_raw("node-a", &["wc", "-l", &wal_path])?;
        if !out.status.success() {
            bail!(
                "expected WAL file at `{wal_path}` on node-a after emitting \
                 100 audit events; `wc -l` failed: stderr=`{}` — Phase 8 \
                 (audit-tier WAL on emitting node, written BEFORE delivery) \
                 not yet implemented",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let lines: usize = stdout
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if lines != 100 {
            bail!(
                "WAL at `{wal_path}` has {lines} lines; expected exactly 100 \
                 (one per emitted event, written BEFORE delivery attempt) — \
                 Phase 8 (Q6 audit WAL persistence) not yet implemented"
            );
        }
        Ok(())
    });
}

// ---------------------------------------------------------------
// Subtest 2: plugin_crash_no_event_loss
// ---------------------------------------------------------------
#[test]
fn plugin_crash_no_event_loss() {
    run_subtest("plugin_crash_no_event_loss", || {
        let _cluster = dispatch_audit_plugin(100)?;

        // Wait for the plugin sidecar to ack the first 50 events. After
        // 50 acks, the HWM under /boi/hooks-hwm/node-a/<plugin> should
        // be at last_acked_seq=50.
        let hwm_prefix = format!("/boi/hooks-hwm/node-a/{AUDIT_PLUGIN}");
        let _ = wait_for_etcd_key(
            &hwm_prefix,
            |kvs| {
                kvs.iter().any(|kv| {
                    let v = String::from_utf8_lossy(&kv.value);
                    v.contains("last_acked_seq") && v.contains("50")
                })
            },
            WAIT,
        );

        // Crash the plugin sidecar mid-delivery.
        let killed = Command::new("docker")
            .arg("compose")
            .arg("-f")
            .arg(compose_path())
            .arg("kill")
            .arg("plugin-sidecar")
            .status();
        if killed.map(|s| !s.success()).unwrap_or(true) {
            bail!(
                "could not `docker compose kill plugin-sidecar` — Phase 8 \
                 sidecar service is not yet defined in the compose topology"
            );
        }

        // Restart the sidecar with a fresh process. It must resume from
        // the persisted HWM and consume the remaining 50 events.
        let _ = Command::new("docker")
            .arg("compose")
            .arg("-f")
            .arg(compose_path())
            .arg("up")
            .arg("-d")
            .arg("plugin-sidecar")
            .status();

        let saw_full = wait_for_etcd_key(
            &hwm_prefix,
            |kvs| {
                kvs.iter().any(|kv| {
                    let v = String::from_utf8_lossy(&kv.value);
                    v.contains("last_acked_seq") && v.contains("100")
                })
            },
            WAIT,
        );
        if saw_full.is_ok() {
            return Ok(());
        }
        bail!(
            "after plugin crash at seq=50, expected HWM at `{hwm_prefix}` \
             to advance to 100 once the sidecar restarts and consumes the \
             remaining WAL entries; HWM did not advance — Phase 8 (audit \
             redelivery from WAL after plugin crash) not yet implemented"
        );
    });
}

// ---------------------------------------------------------------
// Subtest 3: node_restart_replays_wal
// ---------------------------------------------------------------
#[test]
fn node_restart_replays_wal() {
    run_subtest("node_restart_replays_wal", || {
        let _cluster = dispatch_audit_plugin(100)?;

        // Sanity: WAL exists on node-a.
        let wal_path = format!("/root/.boi/hooks-wal/{AUDIT_PLUGIN}.jsonl");
        let pre = docker_exec_raw("node-a", &["test", "-f", &wal_path])?;
        if !pre.status.success() {
            bail!(
                "precondition failed: WAL at `{wal_path}` missing before \
                 node restart — Phase 8 (audit WAL persistence) not yet \
                 implemented"
            );
        }

        // Kill node-a hard (SIGKILL), then bring it back up. The compose
        // bind-mount of ~/.boi/ on the host preserves the WAL across
        // container lifetimes.
        let _ = Command::new("docker")
            .arg("compose")
            .arg("-f")
            .arg(compose_path())
            .arg("kill")
            .arg("-s")
            .arg("KILL")
            .arg("node-a")
            .status();
        let up = Command::new("docker")
            .arg("compose")
            .arg("-f")
            .arg(compose_path())
            .arg("up")
            .arg("-d")
            .arg("node-a")
            .status()
            .context("`docker compose up -d node-a` after kill")?;
        if !up.success() {
            bail!(
                "could not restart node-a after kill — Phase 8 (audit WAL \
                 mount survives container restart) precondition unmet"
            );
        }

        // Post-restart: WAL must still be on disk, replay logic must
        // re-deliver entries past the persisted HWM. The simplest
        // observable: the WAL file still exists and the HWM eventually
        // reaches 100.
        let post = docker_exec_raw("node-a", &["test", "-f", &wal_path])?;
        if !post.status.success() {
            bail!(
                "WAL at `{wal_path}` did NOT survive node-a restart — Phase 8 \
                 (Q6: local-disk WAL on emitting node mounted from host) not \
                 yet implemented"
            );
        }
        let hwm_prefix = format!("/boi/hooks-hwm/node-a/{AUDIT_PLUGIN}");
        let replayed = wait_for_etcd_key(
            &hwm_prefix,
            |kvs| {
                kvs.iter().any(|kv| {
                    let v = String::from_utf8_lossy(&kv.value);
                    v.contains("last_acked_seq") && v.contains("100")
                })
            },
            WAIT,
        );
        if replayed.is_ok() {
            return Ok(());
        }
        bail!(
            "expected HWM at `{hwm_prefix}` to reach 100 after node-a \
             restart replays the WAL; never did — Phase 8 (WAL replay on \
             node bringup) not yet implemented"
        );
    });
}

// ---------------------------------------------------------------
// Subtest 4: hwm_tracks_delivery_position
// ---------------------------------------------------------------
#[test]
fn hwm_tracks_delivery_position() {
    run_subtest("hwm_tracks_delivery_position", || {
        let _cluster = dispatch_audit_plugin(50)?;
        let hwm_prefix = format!("/boi/hooks-hwm/node-a/{AUDIT_PLUGIN}");

        // Sample the HWM repeatedly during delivery; values must never
        // regress. We piggy-back on `wait_for_etcd_key`'s backoff loop
        // by recording each observation it sees.
        let observed = std::cell::RefCell::new(Vec::<u64>::new());
        let _ = wait_for_etcd_key(
            &hwm_prefix,
            |kvs| {
                for kv in kvs {
                    let v = String::from_utf8_lossy(&kv.value);
                    if let Some(idx) = v.find("last_acked_seq") {
                        let tail = &v[idx..];
                        let n: u64 = tail
                            .chars()
                            .skip_while(|c| !c.is_ascii_digit())
                            .take_while(|c| c.is_ascii_digit())
                            .collect::<String>()
                            .parse()
                            .unwrap_or(0);
                        observed.borrow_mut().push(n);
                    }
                }
                observed.borrow().last().copied() == Some(50)
            },
            WAIT,
        );

        let samples = observed.into_inner();
        if samples.is_empty() {
            bail!(
                "no HWM observations under `{hwm_prefix}` during delivery — \
                 Phase 8 (Q6 HWM at /boi/hooks-hwm/{{node}}/{{plugin}} \
                 advancing on ack) not yet implemented"
            );
        }
        // Monotonicity: each sample >= previous.
        for w in samples.windows(2) {
            if w[1] < w[0] {
                bail!(
                    "HWM regressed: saw seq={} then seq={}; sequence={:?} — \
                     Q6 violates monotonic advancement guarantee",
                    w[0], w[1], samples
                );
            }
        }
        if samples.last().copied() != Some(50) {
            bail!(
                "HWM never reached 50 (final observation={:?}); samples={:?} \
                 — Phase 8 ack-on-delivery path not yet implemented",
                samples.last(), samples
            );
        }
        Ok(())
    });
}

// ---------------------------------------------------------------
// Subtest 5: back_pressure_stalls_workflow
// ---------------------------------------------------------------
#[test]
fn back_pressure_stalls_workflow() {
    run_subtest("back_pressure_stalls_workflow", || {
        let _cluster = ensure_cluster()?;
        let _ = boi_node_exec("node-a", &["cluster", "init"]);
        let _ = boi_node_exec(
            "node-a",
            &[
                "plugin",
                "register",
                "--id",
                AUDIT_PLUGIN,
                "--kind",
                "hooks",
                "--delivery-tier",
                "audit",
                "--ack-rate-cap",
                "1/s",
                "--subscribed-kinds",
                "task.completed",
            ],
        )?;

        // Issue a workflow that emits 200 audit events as fast as it
        // can. With the plugin throttled to 1 ack/s and a soft WAL cap
        // of ~100, the emitting workflow MUST stall (not buffer the
        // backlog in unbounded memory).
        let out = boi_node_exec(
            "node-a",
            &[
                "internal",
                "hooks-emit-burst",
                "--plugin",
                AUDIT_PLUGIN,
                "--kind",
                "task.completed",
                "--count",
                "200",
                "--observe-stall",
            ],
        )?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stalled = stderr.contains("hook.queue.saturated")
            || stderr.contains("workflow_stalled_on_hooks")
            || stdout.contains("STALLED")
            || stdout.contains("hook.queue.saturated");
        if stalled {
            return Ok(());
        }
        bail!(
            "expected the emitting workflow to STALL once the audit WAL \
             saturated under a throttled plugin (and to surface either a \
             `hook.queue.saturated` event or a `workflow_stalled_on_hooks` \
             signal); saw stdout=`{}` stderr=`{}` — Phase 8 (Q6 back-pressure \
             from local WAL to emitting workflow) not yet implemented",
            stdout.trim(),
            stderr.trim()
        );
    });
}

// ---------------------------------------------------------------
// Subtest 6: best_effort_tier_unchanged
// ---------------------------------------------------------------
#[test]
fn best_effort_tier_unchanged() {
    run_subtest("best_effort_tier_unchanged", || {
        let _cluster = ensure_cluster()?;
        let _ = boi_node_exec("node-a", &["cluster", "init"]);
        let _ = boi_node_exec(
            "node-a",
            &[
                "plugin",
                "register",
                "--id",
                BEST_EFFORT_PLUGIN,
                "--kind",
                "hooks",
                "--delivery-tier",
                "best_effort",
                "--subscribed-kinds",
                "task.completed",
            ],
        )?;
        let _ = boi_node_exec(
            "node-a",
            &[
                "internal",
                "hooks-emit-burst",
                "--plugin",
                BEST_EFFORT_PLUGIN,
                "--kind",
                "task.completed",
                "--count",
                "10",
            ],
        )?;

        // A best_effort plugin MUST NOT create a WAL file or an HWM key.
        let wal_path = format!("/root/.boi/hooks-wal/{BEST_EFFORT_PLUGIN}.jsonl");
        let wal_check = docker_exec_raw("node-a", &["test", "-e", &wal_path])?;
        if wal_check.status.success() {
            bail!(
                "best_effort plugin `{BEST_EFFORT_PLUGIN}` unexpectedly has a \
                 WAL file at `{wal_path}` — Q6 says only `audit` tier writes \
                 a local-disk WAL; best_effort is §5.5 fire-and-forget"
            );
        }
        let hwm_prefix = format!("/boi/hooks-hwm/node-a/{BEST_EFFORT_PLUGIN}");
        let hwm = etcdctl_get_prefix(&hwm_prefix).unwrap_or_default();
        if !hwm.is_empty() {
            bail!(
                "best_effort plugin `{BEST_EFFORT_PLUGIN}` unexpectedly has \
                 etcd HWM keys under `{hwm_prefix}` ({} keys) — Q6 reserves \
                 HWM tracking for `audit` tier only",
                hwm.len()
            );
        }

        // The positive assertion — that the best_effort plugin actually
        // received the 10 events via the §5.5 in-process path — cannot
        // be verified until Phase 8 wires the dispatcher. Keep the test
        // RED until then by failing on the missing dispatcher signal.
        let trace = docker_exec_raw(
            "plugin-sidecar",
            &["sh", "-c", &format!("cat /tmp/{BEST_EFFORT_PLUGIN}.delivered 2>/dev/null | wc -l")],
        )?;
        let delivered: usize = String::from_utf8_lossy(&trace.stdout)
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if delivered == 10 {
            return Ok(());
        }
        bail!(
            "best_effort plugin `{BEST_EFFORT_PLUGIN}` did not receive the \
             10 emitted events fire-and-forget (saw {delivered}); Phase 8 \
             (§5.5 in-process hooks dispatcher) not yet implemented"
        );
    });
}
