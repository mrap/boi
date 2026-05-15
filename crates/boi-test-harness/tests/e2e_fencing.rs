//! RED E2E #3 — claim CAS + lease fencing prevents double-execution.
//!
//! Per §10 rows 5 + 12 and Q2 lease_id fencing: a worker whose etcd
//! lease has expired must NOT be able to commit its completion write.
//! Core's etcd Txn predicate compares the worker's `claim_lease_id`
//! against the current claim row; a stale lease yields gRPC
//! FAILED_PRECONDITION and emits a `task.claim_fence_rejected` event.
//!
//! Four named subtests, all expected RED today (Phase 4 unimplemented).

use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use boi_test_harness::{
    compose_pause, compose_unpause, docker_available, docker_dir, dump_artifacts,
    etcdctl_get_prefix, network_connect, network_disconnect, start_cluster, wait_for_etcd_key,
};

const WAIT: Duration = Duration::from_secs(5);
const LEASE_TTL: Duration = Duration::from_secs(15);

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

fn partition_node(service: &str) -> Result<()> {
    compose_pause(service)
}

fn unpartition_node(service: &str) -> Result<()> {
    compose_unpause(service)
}

fn ensure_cluster() -> Result<boi_test_harness::Cluster> {
    start_cluster(3).context(
        "start_cluster(3) — Phase 0a stub binary exits 78 (EX_CONFIG); \
         Phase 4 wires the lease-fenced claim/commit path under test",
    )
}

/// Common setup: init cluster, advertise identical caps on a + b so the
/// task can be reassigned from a to b after partition, dispatch task T.
fn dispatch_fencing_task() -> Result<(boi_test_harness::Cluster, String)> {
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
            "e2e-fencing-task",
        ],
    )?;
    let task_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok((cluster, task_id))
}

// ---------------------------------------------------------------
// Subtest 1: stale_worker_completion_rejected
// ---------------------------------------------------------------
#[test]
fn stale_worker_completion_rejected() {
    run_subtest("stale_worker_completion_rejected", || {
        let (_cluster, task_id) = dispatch_fencing_task()?;

        // Wait for ANY node to claim the task.
        let _ = wait_for_etcd_key(
            "/boi/claims/",
            |kvs| {
                kvs.iter().any(|kv| {
                    kv.key.contains(&task_id) && !kv.key.contains("/claim_lease_id")
                })
            },
            WAIT,
        );

        // Detect which node claimed and capture its lease_id.
        let kvs_before = etcdctl_get_prefix("/boi/claims/").unwrap_or_default();
        let (claimant_node, stale_lease) = kvs_before
            .iter()
            .filter(|kv| !kv.key.contains("/claim_lease_id"))
            .find_map(|kv| {
                let v = String::from_utf8_lossy(&kv.value).to_string();
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&v) {
                    let node = parsed.get("node_id").and_then(|v| v.as_str()).map(String::from)?;
                    let lease = parsed.get("lease_id").and_then(|v| v.as_i64()).map(|n| n.to_string())?;
                    Some((node, lease))
                } else {
                    None
                }
            })
            .unwrap_or_else(|| ("node-a".to_string(), "0".to_string()));

        // Partition the claimant so its lease expires.
        partition_node(&claimant_node)?;
        let _ = wait_for_etcd_key(
            "/boi/claims/",
            |kvs| !kvs.iter().any(|kv| kv.key.contains(&task_id)),
            LEASE_TTL + WAIT,
        );

        // Reconnect. Stale claimant now tries to commit with its expired
        // lease_id. Core MUST reject via etcd Txn predicate.
        unpartition_node(&claimant_node)?;
        let out = boi_node_exec(
            &claimant_node,
            &[
                "internal",
                "commit-task",
                "--task-id",
                &task_id,
                "--lease-id",
                &stale_lease,
                "--status",
                "done",
            ],
        )?;

        let stderr = String::from_utf8_lossy(&out.stderr);
        let rejected = !out.status.success()
            && (stderr.contains("FAILED_PRECONDITION")
                || stderr.contains("stale_lease")
                || stderr.contains("claim_fence_rejected"));

        // Also verify dispatch-queue was NOT mutated by the rejected write.
        let q = etcdctl_get_prefix("/boi/dispatch-queue/").unwrap_or_default();
        let mutated_by_stale = q.iter().any(|kv| {
            kv.key.contains(&task_id)
                && String::from_utf8_lossy(&kv.value).contains(&stale_lease)
        });

        if rejected && !mutated_by_stale {
            return Ok(());
        }
        bail!(
            "expected stale-lease commit to be rejected with \
             FAILED_PRECONDITION and /boi/dispatch-queue/{task_id} to be \
             unchanged; got status={:?} stderr=`{}` mutated_by_stale={} — \
             Phase 4 (Q2 lease_id fencing in commit Txn) not yet implemented",
            out.status.code(),
            stderr.trim(),
            mutated_by_stale
        );
    });
}

// ---------------------------------------------------------------
// Subtest 2: new_claimant_completes_unaffected
// ---------------------------------------------------------------
#[test]
fn new_claimant_completes_unaffected() {
    run_subtest("new_claimant_completes_unaffected", || {
        let (_cluster, task_id) = dispatch_fencing_task()?;
        // Wait for any node to claim.
        let _ = wait_for_etcd_key(
            "/boi/claims/",
            |kvs| kvs.iter().any(|kv| kv.key.contains(&task_id) && !kv.key.contains("/claim_lease_id")),
            WAIT,
        );
        // Detect the initial claimant.
        let initial_claimant = etcdctl_get_prefix("/boi/claims/").unwrap_or_default()
            .iter()
            .find_map(|kv| {
                let v = String::from_utf8_lossy(&kv.value).to_string();
                serde_json::from_str::<serde_json::Value>(&v).ok()
                    .and_then(|p| p.get("node_id").and_then(|n| n.as_str()).map(String::from))
            })
            .unwrap_or_else(|| "node-a".to_string());

        // Partition the initial claimant so its lease expires.
        partition_node(&initial_claimant)?;
        let _ = wait_for_etcd_key(
            "/boi/claims/",
            |kvs| !kvs.iter().any(|kv| kv.key.contains(&task_id) && !kv.key.contains("/claim_lease_id")),
            LEASE_TTL + WAIT,
        );

        // A different node should re-claim. Wait for any new claim.
        let reclaimed = wait_for_etcd_key(
            "/boi/claims/",
            |kvs| {
                kvs.iter().any(|kv| {
                    let v = String::from_utf8_lossy(&kv.value);
                    kv.key.contains(&task_id)
                        && !kv.key.contains("/claim_lease_id")
                        && !v.contains(&format!("\"node_id\":\"{}\"", initial_claimant))
                })
            },
            LEASE_TTL + WAIT,
        );
        if reclaimed.is_err() {
            bail!(
                "expected a different node to re-claim task `{task_id}` after \
                 {initial_claimant}'s lease expiry; no new claim observed — \
                 Phase 4 (reassignment after lease expiry) not yet implemented"
            );
        }
        // Detect the new claimant.
        let new_claimant = etcdctl_get_prefix("/boi/claims/").unwrap_or_default()
            .iter()
            .find_map(|kv| {
                if !kv.key.contains(&task_id) || kv.key.contains("/claim_lease_id") { return None; }
                let v = String::from_utf8_lossy(&kv.value).to_string();
                serde_json::from_str::<serde_json::Value>(&v).ok()
                    .and_then(|p| p.get("node_id").and_then(|n| n.as_str()).map(String::from))
            })
            .unwrap_or_else(|| "node-b".to_string());

        // New claimant commits "done" — must succeed.
        let out = boi_node_exec(
            &new_claimant,
            &[
                "internal",
                "commit-task",
                "--task-id",
                &task_id,
                "--status",
                "done",
            ],
        )?;
        if !out.status.success() {
            bail!(
                "rightful new claimant node-b failed to commit completion: \
                 status={:?} stderr=`{}` — Phase 4 (post-reassign commit path) \
                 not yet implemented",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    });
}

// ---------------------------------------------------------------
// Subtest 3: audit_event_for_stale_writeback
// ---------------------------------------------------------------
#[test]
fn audit_event_for_stale_writeback() {
    run_subtest("audit_event_for_stale_writeback", || {
        let (_cluster, task_id) = dispatch_fencing_task()?;
        let _ = wait_for_etcd_key(
            "/boi/claims/",
            |kvs| kvs.iter().any(|kv| kv.key.contains(&task_id) && !kv.key.contains("/claim_lease_id")),
            WAIT,
        );
        let claimant = etcdctl_get_prefix("/boi/claims/").unwrap_or_default()
            .iter()
            .find_map(|kv| {
                let v = String::from_utf8_lossy(&kv.value).to_string();
                serde_json::from_str::<serde_json::Value>(&v).ok()
                    .and_then(|p| p.get("node_id").and_then(|n| n.as_str()).map(String::from))
            })
            .unwrap_or_else(|| "node-a".to_string());
        partition_node(&claimant)?;
        let _ = wait_for_etcd_key(
            "/boi/claims/",
            |kvs| !kvs.iter().any(|kv| kv.key.contains(&task_id) && !kv.key.contains("/claim_lease_id")),
            LEASE_TTL + WAIT,
        );
        unpartition_node(&claimant)?;
        let _ = boi_node_exec(
            &claimant,
            &[
                "internal",
                "commit-task",
                "--task-id",
                &task_id,
                "--lease-id",
                "12345",
                "--status",
                "done",
            ],
        );

        // The canonical event lives under /boi/events/ per F-15.
        let saw_event = wait_for_etcd_key(
            "/boi/events/",
            |kvs| {
                kvs.iter().any(|kv| {
                    String::from_utf8_lossy(&kv.value)
                        .contains("task.claim_fence_rejected")
                })
            },
            WAIT,
        );
        if saw_event.is_ok() {
            return Ok(());
        }
        bail!(
            "expected a `task.claim_fence_rejected` canonical event under \
             /boi/events/ after stale writeback; saw none — Phase 4/8 \
             (F-15 canonical event emission on fence rejection) not yet \
             implemented"
        );
    });
}

// ---------------------------------------------------------------
// Subtest 4: no_double_dispatch_under_partition_recovery
// ---------------------------------------------------------------
#[test]
fn no_double_dispatch_under_partition_recovery() {
    run_subtest("no_double_dispatch_under_partition_recovery", || {
        let (_cluster, task_id) = dispatch_fencing_task()?;
        // Wait for any node to claim.
        let _ = wait_for_etcd_key(
            "/boi/claims/",
            |kvs| kvs.iter().any(|kv| kv.key.contains(&task_id) && !kv.key.contains("/claim_lease_id")),
            WAIT,
        );
        let initial_claimant = etcdctl_get_prefix("/boi/claims/").unwrap_or_default()
            .iter()
            .find_map(|kv| {
                if kv.key.contains("/claim_lease_id") { return None; }
                let v = String::from_utf8_lossy(&kv.value).to_string();
                serde_json::from_str::<serde_json::Value>(&v).ok()
                    .and_then(|p| p.get("node_id").and_then(|n| n.as_str()).map(String::from))
            })
            .unwrap_or_else(|| "node-a".to_string());

        let mut violation: Option<String> = None;
        let check = |label: &str, out: &mut Option<String>| {
            let kvs = etcdctl_get_prefix("/boi/claims/").unwrap_or_default();
            let claimants: Vec<String> = kvs
                .iter()
                .filter(|kv| kv.key.contains(&task_id) && !kv.key.contains("/claim_lease_id"))
                .map(|kv| String::from_utf8_lossy(&kv.value).to_string())
                .collect();
            if claimants.len() > 1 {
                *out = Some(format!(
                    "double claim at `{label}`: {} entries — {:?}",
                    claimants.len(),
                    claimants
                ));
            }
        };

        check("steady-state", &mut violation);
        partition_node(&initial_claimant)?;
        check("post-disconnect", &mut violation);
        // Wait for a DIFFERENT node to reclaim.
        let _ = wait_for_etcd_key(
            "/boi/claims/",
            |kvs| {
                kvs.iter().any(|kv| {
                    kv.key.contains(&task_id)
                        && !kv.key.contains("/claim_lease_id")
                        && !String::from_utf8_lossy(&kv.value).contains(&format!("\"node_id\":\"{}\"", initial_claimant))
                })
            },
            LEASE_TTL + WAIT,
        );
        check("post-reassign", &mut violation);
        unpartition_node(&initial_claimant)?;
        check("post-reconnect", &mut violation);

        if let Some(v) = violation {
            bail!(
                "{v} — cluster permitted two simultaneous claimant_node_id \
                 values for task `{task_id}` during partition recovery; Q2 \
                 fencing must prevent this (Phase 4 not yet implemented)"
            );
        }

        // No double-claim observed through the full partition/recovery
        // cycle — the CAS invariant held. If reassignment to node-b
        // completed, the invariant is positively asserted.
        let reassigned = etcdctl_get_prefix("/boi/claims/").unwrap_or_default()
            .iter()
            .any(|kv| {
                kv.key.contains(&task_id)
                    && !kv.key.contains("/claim_lease_id")
                    && !String::from_utf8_lossy(&kv.value).contains(&format!("\"node_id\":\"{}\"", initial_claimant))
            });
        if reassigned {
            return Ok(());
        }
        bail!(
            "no double-claim observed, but reassignment to node-b did not \
             complete — cannot positively assert the invariant until \
             lease expiry + reassign is fully wired"
        );
    });
}
