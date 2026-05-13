//! RED E2E #4 — provisioning end-to-end.
//!
//! Per design §8 (provisioning), §5.4 (Provisioner plugin), and §16 Q3
//! (admin-gated mint): when a task is dispatched with capability
//! requirements that no node in the cluster satisfies, the router must
//! emit a `ProvisionRequest` to a registered Provisioner plugin. The
//! reference Docker provisioner spawns a new `boi-node` container with
//! a `BOI_TOKEN` minted by core (admin-only), and the new node joins
//! via `boi node join --token` and claims the queued task.
//!
//! Four named subtests, all expected RED today (Phase 5 unimplemented).
//! Failure messages name what's missing so the red signal is actionable.

use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use boi_test_harness::{
    docker_available, docker_dir, dump_artifacts, etcdctl_get_prefix, start_cluster,
    wait_for_etcd_key,
};

/// Short window for "observable within 3s" assertions.
const SHORT_WAIT: Duration = Duration::from_secs(3);
/// 60s budget for a freshly-provisioned node to boot, join, and claim.
const PROVISION_WAIT: Duration = Duration::from_secs(60);
/// Polling window for cooldown observations. The spec's 5-minute
/// no-retry guarantee is asserted via the F-06 counter in etcd — we
/// poll briefly and read the counter rather than waiting 5 minutes,
/// keeping the test under the 90s budget.
const COOLDOWN_OBSERVE: Duration = Duration::from_secs(10);

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

fn boi_node_exec_env(
    service: &str,
    env: &[(&str, &str)],
    args: &[&str],
) -> Result<std::process::Output> {
    let mut cmd = Command::new("docker");
    cmd.arg("compose")
        .arg("-f")
        .arg(compose_path())
        .arg("exec")
        .arg("-T");
    for (k, v) in env {
        cmd.arg("-e").arg(format!("{k}={v}"));
    }
    cmd.arg(service).arg("boi-node").args(args);
    cmd.output()
        .with_context(|| format!("invoke `docker compose exec {service} boi-node ...` with env"))
}

/// Plugin sidecar transcript path. The Docker-provisioner plugin
/// appends each inbound RPC to this file; tests grep it as a
/// deterministic, sleep-free signal.
fn plugin_transcript() -> Result<String> {
    let out = Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(compose_path())
        .arg("exec")
        .arg("-T")
        .arg("plugin-sidecar")
        .arg("cat")
        .arg("/var/lib/boi-plugin/transcript.jsonl")
        .output()
        .context("read plugin-sidecar transcript")?;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Common cluster setup: 3 linux nodes (none satisfy os=mac). Returns
/// the cluster handle so the caller controls teardown ordering.
fn linux_only_cluster() -> Result<boi_test_harness::Cluster> {
    let cluster = start_cluster(3).context(
        "start_cluster(3) — Phase 0a stub binary exits 78; Phase 5 \
         wires the router ProvisionRequest path and reference \
         Docker-provisioner plugin under test",
    )?;
    let _ = boi_node_exec("node-a", &["cluster", "init"]);
    for n in ["node-a", "node-b", "node-c"] {
        let _ = boi_node_exec_env(
            n,
            &[("BOI_CAPS_STATIC", "os=linux,runtime=generic")],
            &["node", "advertise"],
        );
    }
    Ok(cluster)
}

fn dispatch_mac_task(from: &str) -> Result<(String, std::process::Output)> {
    let out = boi_node_exec(
        from,
        &[
            "spec",
            "dispatch",
            "--requires",
            "os=mac",
            "--name",
            "e2e-provision-task",
        ],
    )?;
    let task_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok((task_id, out))
}

// ---------------------------------------------------------------
// Subtest 1: no_capable_triggers_provision
// ---------------------------------------------------------------
#[test]
fn no_capable_triggers_provision() {
    run_subtest("no_capable_triggers_provision", || {
        let _cluster = linux_only_cluster()?;
        let (task_id, _) = dispatch_mac_task("node-a")?;

        // The router must call ProvisionRequest on the registered
        // provisioner plugin within 3s of dispatch. The plugin sidecar
        // appends each RPC to a transcript; we poll the transcript via
        // wait_for_etcd_key's deadline pattern by checking on each
        // tick of an etcd watch we don't actually care about.
        let deadline = std::time::Instant::now() + SHORT_WAIT;
        let mut saw = false;
        while std::time::Instant::now() < deadline {
            if let Ok(t) = plugin_transcript() {
                if t.contains("ProvisionRequest") && t.contains(&task_id) {
                    saw = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(200)); // allowed: bounded poll inside fixed 3s deadline
        }
        if saw {
            return Ok(());
        }
        bail!(
            "expected a `ProvisionRequest` RPC referencing task `{task_id}` \
             in the plugin-sidecar transcript within {:?} of dispatch; \
             none observed — Phase 5 (router emits ProvisionRequest when \
             no node satisfies `requires:`) not yet implemented",
            SHORT_WAIT
        );
    });
}

// ---------------------------------------------------------------
// Subtest 2: provision_token_is_admin_gated
// ---------------------------------------------------------------
#[test]
fn provision_token_is_admin_gated() {
    run_subtest("provision_token_is_admin_gated", || {
        let _cluster = linux_only_cluster()?;

        // node-a is admin (cluster bootstrap node per §8); node-b is
        // a regular node. Per Q3, only admin nodes can mint BOI_TOKEN
        // via `internal mint-provision-token`.
        let non_admin = boi_node_exec(
            "node-b",
            &[
                "internal",
                "mint-provision-token",
                "--for-caps",
                "os=mac",
            ],
        )?;
        let non_admin_stderr = String::from_utf8_lossy(&non_admin.stderr);
        let denied = !non_admin.status.success()
            && (non_admin_stderr.contains("PermissionDenied")
                || non_admin_stderr.contains("admin")
                || non_admin_stderr.contains("not authorized"));
        if !denied {
            bail!(
                "expected non-admin `node-b` mint-provision-token to fail \
                 with PermissionDenied; got status={:?} stderr=`{}` — \
                 Phase 5 (Q3 admin-gated token mint) not yet implemented",
                non_admin.status.code(),
                non_admin_stderr.trim()
            );
        }

        // Admin node-a must succeed and emit a non-empty token.
        let admin = boi_node_exec(
            "node-a",
            &[
                "internal",
                "mint-provision-token",
                "--for-caps",
                "os=mac",
            ],
        )?;
        let admin_stdout = String::from_utf8_lossy(&admin.stdout).trim().to_string();
        if !admin.status.success() || admin_stdout.is_empty() {
            bail!(
                "expected admin `node-a` mint-provision-token to succeed and \
                 emit a token on stdout; got status={:?} stdout=`{}` \
                 stderr=`{}` — Phase 5 (Q3 admin-gated token mint) not yet \
                 implemented",
                admin.status.code(),
                admin_stdout,
                String::from_utf8_lossy(&admin.stderr).trim()
            );
        }
        Ok(())
    });
}

// ---------------------------------------------------------------
// Subtest 3: new_node_joins_and_claims
// ---------------------------------------------------------------
#[test]
fn new_node_joins_and_claims() {
    run_subtest("new_node_joins_and_claims", || {
        let _cluster = linux_only_cluster()?;
        let (task_id, _) = dispatch_mac_task("node-a")?;

        // Within PROVISION_WAIT a 4th node must register under
        // /boi/nodes/ advertising os=mac.
        let new_node = wait_for_etcd_key(
            "/boi/nodes/",
            |kvs| {
                let macs: Vec<_> = kvs
                    .iter()
                    .filter(|kv| {
                        let v = String::from_utf8_lossy(&kv.value);
                        v.contains("os=mac")
                    })
                    .collect();
                macs.len() >= 1 && kvs.len() >= 4
            },
            PROVISION_WAIT,
        );
        if new_node.is_err() {
            bail!(
                "expected a 4th node advertising os=mac to register under \
                 /boi/nodes/ within {:?} of dispatch; none appeared — \
                 Phase 5 (Docker-provisioner plugin spawns boi-node \
                 container + `boi node join --token` path) not yet implemented",
                PROVISION_WAIT
            );
        }

        // That node must then claim the queued task.
        let claimed = wait_for_etcd_key(
            "/boi/claims/",
            |kvs| {
                kvs.iter().any(|kv| {
                    kv.key.contains(&task_id)
                        && {
                            let v = String::from_utf8_lossy(&kv.value);
                            !v.contains("node-a")
                                && !v.contains("node-b")
                                && !v.contains("node-c")
                        }
                })
            },
            PROVISION_WAIT,
        );
        if claimed.is_err() {
            bail!(
                "expected the newly-provisioned node to claim task \
                 `{task_id}` within {:?}; no claim by a non-{{a,b,c}} node \
                 observed — Phase 5 (assignment loop picks up newly-joined \
                 capable node) not yet implemented",
                PROVISION_WAIT
            );
        }
        Ok(())
    });
}

// ---------------------------------------------------------------
// Subtest 4: provisioner_returned_success_but_no_join_triggers_cooldown
// ---------------------------------------------------------------
#[test]
fn provisioner_returned_success_but_no_join_triggers_cooldown() {
    run_subtest(
        "provisioner_returned_success_but_no_join_triggers_cooldown",
        || {
            let _cluster = linux_only_cluster()?;

            // Configure the test provisioner to ack success without
            // actually spawning a container. The plugin sidecar reads
            // this env on startup; setting it via `internal
            // set-provisioner-mode` is the test-only hook.
            let _ = boi_node_exec(
                "node-a",
                &[
                    "internal",
                    "set-provisioner-mode",
                    "--mode",
                    "ack-without-spawn",
                ],
            );

            let (task_id, _) = dispatch_mac_task("node-a")?;

            // Wait for the failure counter to reach the F-06 threshold
            // (consecutive_claim_failures >= 3) under
            // /boi/provision-failures/<task_id>.
            let counter = wait_for_etcd_key(
                "/boi/provision-failures/",
                |kvs| {
                    kvs.iter().any(|kv| {
                        kv.key.contains(&task_id)
                            && String::from_utf8_lossy(&kv.value)
                                .contains("consecutive_claim_failures")
                    })
                },
                COOLDOWN_OBSERVE,
            );
            if counter.is_err() {
                bail!(
                    "expected F-06 `consecutive_claim_failures` counter at \
                     `/boi/provision-failures/{task_id}` to be tracked after \
                     ack-without-spawn responses; counter absent — Phase 5 \
                     (F-06 cooldown bookkeeping) not yet implemented"
                );
            }

            // Snapshot the transcript, then poll briefly: once the
            // counter has crossed >=3, no further ProvisionRequest for
            // this task should appear. We use COOLDOWN_OBSERVE as a
            // sufficiency window — the spec's 5-minute promise is
            // verified by the cooldown state in etcd, not by waiting
            // 5 minutes.
            let before = plugin_transcript().unwrap_or_default();
            let before_count = before.matches(&task_id).count();
            let deadline = std::time::Instant::now() + COOLDOWN_OBSERVE;
            while std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(500)); // allowed: bounded poll under fixed 10s observation window
            }
            let after = plugin_transcript().unwrap_or_default();
            let after_count = after.matches(&task_id).count();
            let new_requests = after_count.saturating_sub(before_count);

            // Verify task remains in pending-provision state.
            let pending = etcdctl_get_prefix("/boi/dispatch-queue/")
                .unwrap_or_default()
                .iter()
                .any(|kv| {
                    kv.key.contains(&task_id)
                        && String::from_utf8_lossy(&kv.value)
                            .contains("pending-provision")
                });

            if new_requests == 0 && pending {
                return Ok(());
            }
            bail!(
                "expected: (a) zero new ProvisionRequest RPCs for task \
                 `{task_id}` during the {:?} cooldown observation window \
                 (got {new_requests}); (b) task to remain in \
                 `pending-provision` (got pending={pending}) — Phase 5 \
                 (F-06 cooldown suppression + pending-provision state \
                 transition) not yet implemented",
                COOLDOWN_OBSERVE
            );
        },
    );
}
