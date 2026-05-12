//! RED E2E #5 — plugin lifecycle, Handshake, and crash recovery.
//!
//! Asserts the contract from the distributed-architecture design doc:
//!   - §5 plugin contracts
//!   - §16 Q4 hybrid versioning + mandatory `Handshake` RPC
//!   - F-11 `BOI_READY\n` ready-signal + `plugin.ready_timeout_secs`
//!   - F-20 fixed restart budget: 3 restarts / 5 min → `unstable` →
//!     node `caps.dynamic.health=degraded`
//!
//! Every subtest is expected to FAIL today; Phase 2 wires the plugin
//! supervisor, Handshake RPC and crash bookkeeping. The red message
//! names the missing piece so a future implementor can grep for it.
//!
//! Wait semantics use `boi_test_harness::wait_for_etcd_key` only;
//! tests never invoke raw timer-based delays directly.

use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use boi_test_harness::{
    docker_available, docker_dir, dump_artifacts, etcdctl_get_prefix, start_cluster,
    wait_for_etcd_key,
};

/// Bounded wait used across subtests. Generous enough to absorb the
/// 10 s default `plugin.ready_timeout_secs` while keeping each test
/// well under the 90 s per-test budget.
const WAIT: Duration = Duration::from_secs(15);

/// Wrap a subtest body so a red failure dumps diagnostics before the
/// test process panics. Keeps every red informative.
fn run_subtest(name: &str, body: impl FnOnce() -> Result<()>) {
    if !docker_available() {
        eprintln!("SKIP {name}: docker not on PATH");
        return;
    }
    match body() {
        Ok(()) => panic!(
            "subtest `{name}` unexpectedly PASSED — Phase 2 (plugin \
             supervisor + Handshake RPC) is not implemented, so this \
             red test passing means the test itself is wrong"
        ),
        Err(e) => {
            let _ = dump_artifacts(name);
            panic!("RED [{name}] {e:#}");
        }
    }
}

/// Invoke `boi-node ...` inside a compose service. Today this exec'd
/// command will fail because `boi-node` exits 78 (EX_CONFIG stub from
/// Phase 0a) — that's the intended red signal.
fn boi_node_exec(service: &str, args: &[&str]) -> Result<std::process::Output> {
    let out = Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(docker_dir().join("docker-compose.yaml"))
        .arg("exec")
        .arg("-T")
        .arg(service)
        .arg("boi-node")
        .args(args)
        .output()
        .with_context(|| format!("invoke `docker compose exec {service} boi-node ...`"))?;
    Ok(out)
}

fn ensure_cluster() -> Result<boi_test_harness::Cluster> {
    start_cluster(1).context(
        "start_cluster(1) — Phase 0a stub binary is expected to make \
         the boi-node image build fail or the container exit 78 \
         (EX_CONFIG); Phase 0c gives the binary a real skeleton, \
         Phase 2 adds the plugin supervisor",
    )
}

// ---------------------------------------------------------------
// Subtest 1: plugin_ready_signal_required
// ---------------------------------------------------------------
//
// Per F-11: a plugin that never writes `BOI_READY\n` within
// `plugin.ready_timeout_secs` (default 10 s) must be killed and
// reported as `start_failed`. We point the supervisor at a binary
// that intentionally never emits the token.
#[test]
fn plugin_ready_signal_required() {
    if !docker_available() {
        eprintln!("SKIP plugin_ready_signal_required: docker not on PATH");
        return;
    }
    (|| -> Result<()> {
        let _cluster = ensure_cluster()?;
        let out = boi_node_exec(
            "node-a",
            &[
                "plugin",
                "start",
                "--name",
                "silent",
                "--bin",
                "/bin/sleep",
                "--args",
                "60",
                "--ready-timeout-secs",
                "10",
            ],
        )?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        // Success here = unexpected green. We expect a `start_failed`
        // status report with the ready-timeout reason.
        let reported = stdout.contains("start_failed")
            || stderr.contains("start_failed")
            || stdout.contains("ready_timeout")
            || stderr.contains("ready_timeout");
        if reported {
            return Ok(());
        }
        bail!(
            "expected `boi plugin start silent` to report `start_failed` \
             after plugin.ready_timeout_secs=10s elapsed without `BOI_READY\\n`; \
             got status={:?} stdout=`{}` stderr=`{}` — Phase 2 (plugin \
             supervisor + F-11 ready-signal enforcement) not yet implemented",
            out.status.code(),
            stdout.trim(),
            stderr.trim()
        );
    })()
    .unwrap();
}

// ---------------------------------------------------------------
// Subtest 2: handshake_returns_capabilities
// ---------------------------------------------------------------
//
// Per Q4: each plugin service has a mandatory in-proto `Handshake`
// RPC returning `plugin_proto_minor` + capability strings. Core stores
// them under `/boi/plugins/<name>/caps` so per-RPC gating can read
// them. We use the in-tree mock plugin that advertises caps
// `caps.x.foo` and `caps.x.bar`.
#[test]
fn handshake_returns_capabilities() {
    run_subtest("handshake_returns_capabilities", || {
        let _cluster = ensure_cluster()?;
        let _ = boi_node_exec(
            "node-a",
            &["plugin", "start", "--name", "mock-x", "--bin", "boi-mock-plugin"],
        );
        let kvs = wait_for_etcd_key(
            "/boi/plugins/mock-x/caps",
            |kvs| {
                let blob = kvs
                    .iter()
                    .map(|kv| String::from_utf8_lossy(&kv.value).into_owned())
                    .collect::<Vec<_>>()
                    .join("\n");
                blob.contains("caps.x.foo") && blob.contains("caps.x.bar")
            },
            WAIT,
        );
        match kvs {
            Ok(_) => Ok(()),
            Err(_) => bail!(
                "expected /boi/plugins/mock-x/caps to record \
                 [\"caps.x.foo\", \"caps.x.bar\"] after Handshake; got \
                 etcd-key-not-found — Phase 2 (Q4 mandatory Handshake \
                 RPC + capability storage) not yet implemented"
            ),
        }
    });
}

// ---------------------------------------------------------------
// Subtest 3: major_version_mismatch_rejected
// ---------------------------------------------------------------
//
// Per Q4 hybrid versioning: major bump = new proto package. A plugin
// claiming `boi.workspace.v2` (no such package exists today) must be
// rejected at Handshake before any RPC dispatch. The plugin should
// NOT be registered in etcd, and the CLI should surface the version
// error.
#[test]
fn major_version_mismatch_rejected() {
    if !docker_available() {
        eprintln!("SKIP major_version_mismatch_rejected: docker not on PATH");
        return;
    }
    (|| -> Result<()> {
        let _cluster = ensure_cluster()?;
        let out = boi_node_exec(
            "node-a",
            &[
                "plugin",
                "start",
                "--name",
                "wrong-major",
                "--bin",
                "boi-mock-plugin",
                "--proto-package",
                "boi.workspace.v2",
            ],
        )?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let rejected = !out.status.success()
            && (stderr.contains("proto_version_mismatch")
                || stderr.contains("unknown proto package")
                || stderr.contains("boi.workspace.v2")
                || stdout.contains("proto_version_mismatch"));
        let kvs = etcdctl_get_prefix("/boi/plugins/wrong-major/").unwrap_or_default();
        let registered = !kvs.is_empty();
        if rejected && !registered {
            return Ok(());
        }
        bail!(
            "expected Handshake to reject plugin claiming `boi.workspace.v2` \
             (major mismatch) and to NOT register it in etcd; got \
             registered={registered} status={:?} stdout=`{}` stderr=`{}` — \
             Phase 2 (Q4 major-version gating at Handshake) not yet implemented",
            out.status.code(),
            stdout.trim(),
            stderr.trim()
        );
    })()
    .unwrap();
}

// ---------------------------------------------------------------
// Subtest 4: crash_under_threshold_restarts
// ---------------------------------------------------------------
//
// Per F-20: 3 restarts within a 5-minute window. The 4th crash inside
// the window flips the plugin to `unstable` and the node to
// `caps.dynamic.health=degraded`. We crash the plugin four times in
// rapid succession (well inside 5 min) and assert the final state.
#[test]
fn crash_under_threshold_restarts() {
    run_subtest("crash_under_threshold_restarts", || {
        let _cluster = ensure_cluster()?;
        let _ = boi_node_exec(
            "node-a",
            &["plugin", "start", "--name", "flaky", "--bin", "boi-mock-plugin"],
        );
        for _ in 0..4 {
            // Trigger an in-plugin panic via the mock plugin's
            // debug-only `crash` RPC. Today the CLI does not exist;
            // status is ignored on purpose so the supervisor's
            // bookkeeping (not ours) drives the assertion.
            let _ = boi_node_exec("node-a", &["plugin", "crash", "--name", "flaky"]);
        }
        let kvs = wait_for_etcd_key(
            "/boi/plugins/flaky/",
            |kvs| {
                kvs.iter().any(|kv| {
                    let v = String::from_utf8_lossy(&kv.value);
                    kv.key.ends_with("/status") && v.contains("unstable")
                })
            },
            WAIT,
        );
        if kvs.is_err() {
            bail!(
                "expected /boi/plugins/flaky/status=unstable after 4 crashes \
                 inside the 5-min window (F-20); got etcd-key-not-found — \
                 Phase 2 (plugin supervisor + restart-budget bookkeeping) not \
                 yet implemented"
            );
        }
        let node_kvs = wait_for_etcd_key(
            "/boi/nodes/node-a",
            |kvs| {
                kvs.iter().any(|kv| {
                    let v = String::from_utf8_lossy(&kv.value);
                    v.contains("\"health\":\"degraded\"") || v.contains("health=degraded")
                })
            },
            WAIT,
        );
        match node_kvs {
            Ok(_) => Ok(()),
            Err(_) => bail!(
                "expected node-a `caps.dynamic.health=degraded` after plugin \
                 `flaky` flipped to unstable (F-11/F-20); got non-degraded — \
                 Phase 2 (health propagation into node-cap document) not yet \
                 implemented"
            ),
        }
    });
}

// ---------------------------------------------------------------
// Subtest 5: plugin_crash_does_not_kill_core
// ---------------------------------------------------------------
//
// Per §5 isolation: a plugin SIGSEGV must NOT kill `boi-node`. After
// the crash the node still owns its etcd lease and the cluster sees
// it present under `/boi/nodes/`.
#[test]
fn plugin_crash_does_not_kill_core() {
    if !docker_available() {
        eprintln!("SKIP plugin_crash_does_not_kill_core: docker not on PATH");
        return;
    }
    (|| -> Result<()> {
        let _cluster = ensure_cluster()?;
        let _ = boi_node_exec(
            "node-a",
            &["plugin", "start", "--name", "crasher", "--bin", "boi-mock-plugin"],
        );
        let _ = boi_node_exec("node-a", &["plugin", "crash", "--name", "crasher"]);
        // After the plugin dies, boi-node should still be live and
        // renewing its etcd lease, so /boi/nodes/node-a stays
        // present. Today the boi-node stub exits 78, so the key was
        // never written in the first place — that's also the red.
        let kvs = wait_for_etcd_key(
            "/boi/nodes/",
            |kvs| kvs.iter().any(|kv| kv.key == "/boi/nodes/node-a"),
            WAIT,
        );
        match kvs {
            Ok(_) => Ok(()),
            Err(_) => bail!(
                "expected /boi/nodes/node-a to remain present after plugin \
                 `crasher` died, proving plugin isolation per §5; got \
                 etcd-key-not-found — Phase 2 (plugin supervisor isolating \
                 plugin failures from boi-node) not yet implemented"
            ),
        }
    })()
    .unwrap();
}
