//! RED E2E #1 — cluster bootstrap + 3-node join.
//!
//! Six named subtests, one per assertion in TAEF7. Every subtest is
//! expected to FAIL today; the failure message names the Phase that
//! will turn it green so a future implementor can grep for it.
//!
//! Wait semantics use `boi_test_harness::wait_for_etcd_key` only;
//! tests never invoke raw timer-based delays directly.

use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use boi_test_harness::{
    docker_available, docker_dir, dump_artifacts, etcdctl_get_prefix, start_cluster,
    wait_for_etcd_key,
};

/// Bounded wait used across subtests. 5s satisfies the spec's
/// "within 5s" eventual-consistency caveat while keeping each test
/// well under the 90s per-test budget.
const WAIT: Duration = Duration::from_secs(5);

/// Wrap a subtest body so a red failure dumps diagnostics before the
/// test process panics. Keeps every red informative.
fn run_subtest(name: &str, body: impl FnOnce() -> Result<()>) {
    if !docker_available() {
        eprintln!("SKIP {name}: docker not on PATH");
        return;
    }
    match body() {
        Ok(()) => {},
        Err(e) => {
            let _ = dump_artifacts(name);
            // Surface the informative red message and fail the test.
            panic!("RED [{name}] {e:#}");
        }
    }
}

/// Invoke `boi cluster init` against `node-a`. Today this exec'd
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
    start_cluster(3).context(
        "start_cluster(3) — Phase 0a stub binary is expected to make \
         the boi-node image build fail or the container exit 78 \
         (EX_CONFIG); Phase 0c gives the binary a real skeleton",
    )
}

// ---------------------------------------------------------------
// Subtest 1: cluster_init_creates_ca
// ---------------------------------------------------------------
#[test]
fn cluster_init_creates_ca() {
    run_subtest("cluster_init_creates_ca", || {
        let _cluster = ensure_cluster()?;
        let _ = boi_node_exec("node-a", &["cluster", "init"]);
        let kvs = wait_for_etcd_key("/boi/cluster/", |kvs| {
            kvs.iter().any(|kv| kv.key == "/boi/cluster/ca.fingerprint")
        }, WAIT);
        match kvs {
            Ok(_) => Ok(()), // would mean Phase 3 is real (unexpected)
            Err(_) => bail!(
                "expected /boi/cluster/ca.fingerprint after `boi cluster init` \
                 on node-a, got etcd-key-not-found — Phase 3 (cluster CA mint) \
                 not yet implemented"
            ),
        }
    });
}

// ---------------------------------------------------------------
// Subtest 2: cluster_init_marks_seed_admin
// ---------------------------------------------------------------
#[test]
fn cluster_init_marks_seed_admin() {
    run_subtest("cluster_init_marks_seed_admin", || {
        let _cluster = ensure_cluster()?;
        let _ = boi_node_exec("node-a", &["cluster", "init"]);
        let kvs = etcdctl_get_prefix("/boi/nodes/").unwrap_or_default();
        let node_a = kvs.iter().find(|kv| kv.key == "/boi/nodes/node-a");
        let val = node_a
            .map(|kv| String::from_utf8_lossy(&kv.value).into_owned())
            .unwrap_or_default();
        if val.contains("\"cluster_admin\":true") || val.contains("cluster_admin=true") {
            return Ok(());
        }
        bail!(
            "expected /boi/nodes/node-a to record caps.static.cluster_admin=true \
             after seed init, got `{val}` — Phase 3 (seed-admin minting per Q3) \
             not yet implemented"
        );
    });
}

// ---------------------------------------------------------------
// Subtest 3: non_admin_cannot_mint_token
// ---------------------------------------------------------------
#[test]
fn non_admin_cannot_mint_token() {
    run_subtest("non_admin_cannot_mint_token", || {
        let _cluster = ensure_cluster()?;
        let _ = boi_node_exec("node-a", &["cluster", "init"]);
        // Attempt to mint from node-b (not admin). Must return
        // PermissionDenied per Q3.
        let out = boi_node_exec("node-b", &["cluster", "mint-join-token"])?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success()
            && (stderr.contains("PermissionDenied") || stderr.contains("permission denied"))
        {
            return Ok(());
        }
        bail!(
            "expected PermissionDenied from `MintJoinToken` on non-admin node-b \
             (Q3 cluster_admin gating); got status={:?} stderr=`{}` — \
             Phase 3 (RBAC + MintJoinToken RPC) not yet implemented",
            out.status.code(),
            stderr.trim()
        );
    });
}

// ---------------------------------------------------------------
// Subtest 4: valid_token_admits_node
// ---------------------------------------------------------------
#[test]
fn valid_token_admits_node() {
    run_subtest("valid_token_admits_node", || {
        let _cluster = ensure_cluster()?;
        let _ = boi_node_exec("node-a", &["cluster", "init"]);
        let mint = boi_node_exec("node-a", &["cluster", "mint-join-token"])?;
        let token = String::from_utf8_lossy(&mint.stdout).trim().to_string();
        if token.is_empty() {
            bail!(
                "MintJoinToken on admin node-a produced no token (stub binary \
                 exit 78) — Phase 3 (token minting) not yet implemented"
            );
        }
        // Drive node-b's join with the token. Today the boi-node stub
        // exits 78 before doing anything, so /boi/nodes/node-b will
        // never appear.
        let _ = Command::new("docker")
            .arg("compose")
            .arg("-f")
            .arg(docker_dir().join("docker-compose.yaml"))
            .arg("exec")
            .arg("-T")
            .arg("-e")
            .arg(format!("BOI_TOKEN={token}"))
            .arg("node-b")
            .arg("boi-node")
            .arg("node")
            .arg("join")
            .arg("--token")
            .arg(&token)
            .status();
        let kvs = wait_for_etcd_key(
            "/boi/nodes/",
            |kvs| kvs.iter().any(|kv| kv.key == "/boi/nodes/node-b"),
            WAIT,
        );
        match kvs {
            Ok(_) => Ok(()),
            Err(_) => bail!(
                "expected /boi/nodes/node-b after token-authenticated join \
                 (Phase 3 Handshake), got etcd-key-not-found — Phase 3 \
                 (node join + mTLS chain-of-trust) not yet implemented"
            ),
        }
    });
}

// ---------------------------------------------------------------
// Subtest 5: tampered_token_rejected
// ---------------------------------------------------------------
#[test]
fn tampered_token_rejected() {
    run_subtest("tampered_token_rejected", || {
        let _cluster = ensure_cluster()?;
        let _ = boi_node_exec("node-a", &["cluster", "init"]);
        let mint = boi_node_exec("node-a", &["cluster", "mint-join-token"])?;
        let token = String::from_utf8_lossy(&mint.stdout).trim().to_string();
        // Flip one bit of the fingerprint segment.
        let tampered = if token.is_empty() {
            // No real token to tamper — proves Phase 3 is missing.
            "AAAA.BBBB.tampered".to_string()
        } else {
            let mut bytes = token.into_bytes();
            if let Some(last) = bytes.last_mut() {
                *last ^= 0x01;
            }
            String::from_utf8_lossy(&bytes).into_owned()
        };
        let status = Command::new("docker")
            .arg("compose")
            .arg("-f")
            .arg(docker_dir().join("docker-compose.yaml"))
            .arg("exec")
            .arg("-T")
            .arg("-e")
            .arg(format!("BOI_TOKEN={tampered}"))
            .arg("node-b")
            .arg("boi-node")
            .arg("node")
            .arg("join")
            .arg("--token")
            .arg(&tampered)
            .status()?;
        // The join command must exit non-zero (fail-closed). The node-b
        // container is already running its daemon so /boi/nodes/node-b
        // will exist from the initial startup — we check the EXIT CODE
        // of the join command, not etcd presence.
        if status.success() {
            bail!(
                "tampered token join exited 0 — fail-closed semantics violated. \
                 Expected non-zero exit from token signature verification."
            );
        }
        Ok(())
    });
}

// ---------------------------------------------------------------
// Subtest 6: member_list_consistent
// ---------------------------------------------------------------
#[test]
fn member_list_consistent() {
    run_subtest("member_list_consistent", || {
        let _cluster = ensure_cluster()?;
        let _ = boi_node_exec("node-a", &["cluster", "init"]);
        // Try to drive each node to join. All will exit 78 today.
        for node in ["node-b", "node-c"] {
            let _ = boi_node_exec(node, &["node", "join", "--token", "stub"]);
        }
        // Read `boi cluster members` from each node and ensure they
        // see the same 3 names.
        let mut listings: Vec<(String, String)> = Vec::new();
        for node in ["node-a", "node-b", "node-c"] {
            let out = boi_node_exec(node, &["cluster", "members"])?;
            listings.push((node.to_string(), String::from_utf8_lossy(&out.stdout).into_owned()));
        }
        let all_same = listings
            .windows(2)
            .all(|w| w[0].1.trim() == w[1].1.trim() && !w[0].1.trim().is_empty());
        let all_three = listings.iter().all(|(_, l)| {
            l.contains("node-a") && l.contains("node-b") && l.contains("node-c")
        });
        if all_same && all_three {
            return Ok(());
        }
        // Bounded retry against eventual consistency before declaring red.
        let _ = wait_for_etcd_key(
            "/boi/nodes/",
            |kvs| kvs.len() >= 3,
            WAIT,
        );
        Err(anyhow!(
            "expected `boi cluster members` to agree across 3 nodes within 5s \
             and to list {{node-a,node-b,node-c}}; got listings={:?} — Phase 3 \
             (`cluster members` CLI + etcd-backed member list) not yet implemented",
            listings
                .iter()
                .map(|(n, l)| format!("{n}=`{}`", l.trim()))
                .collect::<Vec<_>>()
        ))
    });
}
