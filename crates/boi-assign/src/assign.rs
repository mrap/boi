//! Revision-pinned claim assignment loop.
//!
//! Per design §7 (Task assignment algorithm) and §16 Q1/Q2:
//!
//! 1. `capability_filter` narrows membership to nodes whose advertised
//!    caps satisfy `task.requires` (and that aren't flagged degraded —
//!    see F-06 cooldown).
//! 2. `hrw_rank` orders the survivors by deterministic rendezvous hash.
//! 3. For each candidate in priority order we attempt the claim CAS via
//!    `boi_cluster::claims::ClaimRecord::acquire`. Before each CAS we
//!    check the *stale window* (Q1): if the snapshot we ranked on is
//!    more than `STALE_WINDOW` (W=64) etcd revisions behind the cluster,
//!    re-read the snapshot first so the candidate list is still trustworthy.
//! 4. On CAS conflict (another claimer beat us, or a stale claim is
//!    still present), refresh the working revision and retry up to
//!    `MAX_RETRIES` times for the *same* candidate.
//! 5. After `MAX_RETRIES` failures, fall through to the next HRW
//!    candidate.
//! 6. If every capable candidate is exhausted we return `NeedProvision`
//!    so the orchestrator can scale out (per F-01 / design §7).

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, warn};

use boi_cluster::claims::ClaimRecord;
use boi_cluster::client::{ClusterError, EtcdClient};
use boi_cluster::membership::MembershipSnapshot;
use boi_cluster::nodes::{NodeCaps, NODES_PREFIX};

use crate::hrw::{capability_filter, hrw_rank, AssignNode, CapRequires};

/// W=64. The maximum |snapshot.mod_revision - current_cluster_revision|
/// we accept before we *must* re-read membership before attempting CAS.
/// Per design §16 Q1, this bounds how stale a ranking decision can be
/// against the live cluster.
pub const STALE_WINDOW: i64 = 64;

/// Maximum CAS retries against a single candidate before falling
/// through to the next HRW pick.
pub const MAX_RETRIES: u32 = 3;

/// Minimal task view the assignment loop needs. The full task record
/// lives elsewhere (queue/store); here we only need identity + the
/// capability requires clause.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    #[serde(default)]
    pub requires: CapRequires,
}

/// Outcome of one `assign()` invocation.
#[derive(Debug)]
pub enum AssignResult {
    /// Claim acquired on this node. The envelope is already persisted
    /// to `/boi/claims/{task_id}` (lease-bound to `claim.lease_id`).
    Assigned(ClaimRecord),
    /// No capable candidate accepted the claim — orchestrator should
    /// provision more capacity (F-01).
    NeedProvision,
}

#[derive(Debug, Error)]
pub enum AssignError {
    #[error("cluster error: {0}")]
    Cluster(#[from] ClusterError),
}

pub type Result<T> = std::result::Result<T, AssignError>;

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Read the current etcd header revision via a prefix list on
/// `/boi/nodes/` (the membership prefix is the natural revision pin
/// per Q1). Returns just the revision; we discard the KVs here.
async fn current_cluster_revision(etcd: &EtcdClient) -> Result<i64> {
    let (_, rev) = etcd.get_prefix_with_revision(NODES_PREFIX).await?;
    Ok(rev)
}

/// Join a membership snapshot with per-node caps so the candidate set
/// the assignment loop ranks over carries cap info. Missing caps are
/// treated as empty (the node simply won't satisfy a non-empty
/// `requires`, but it remains visible — matches `NodeCaps::default()`).
pub async fn join_caps_pub(
    etcd: &EtcdClient,
    snapshot: &MembershipSnapshot,
) -> Result<Vec<AssignNode>> {
    let mut out = Vec::with_capacity(snapshot.nodes.len());
    for (id, rec) in &snapshot.nodes {
        let caps = NodeCaps::get(etcd, id).await?.unwrap_or_default();
        out.push(AssignNode::new(rec.clone(), caps));
    }
    Ok(out)
}

/// Attempt to assign `task` to a capable node.
///
/// `snapshot` is the membership view that ranking starts from. If the
/// cluster has moved past it by more than [`STALE_WINDOW`] revisions
/// we re-read membership before issuing the claim CAS.
///
/// `claim_lease_id` is the lease that will fence the claim envelope.
/// In production this is the assigner's (orchestrator's) lease; the
/// claim disappears automatically if the assigner crashes mid-flight.
pub async fn assign(
    task: &TaskRecord,
    snapshot: &MembershipSnapshot,
    etcd: &EtcdClient,
    claim_lease_id: i64,
) -> Result<AssignResult> {
    // Step 1 — join membership with caps so we can filter.
    let mut joined = join_caps_pub(etcd, snapshot).await?;

    // Step 2 — capability filter (also drops degraded nodes per F-06).
    let mut candidates = capability_filter(&joined, &task.requires);
    if candidates.is_empty() {
        debug!(task = %task.id, "no capable candidates — need provision");
        return Ok(AssignResult::NeedProvision);
    }

    // Step 3 — rank.
    let mut ranked = hrw_rank(&task.id, &candidates);
    let mut working_rev = snapshot.mod_revision;

    // Step 4–6 — walk the HRW order trying CAS on each candidate.
    let mut idx = 0;
    while idx < ranked.len() {
        let node_id = ranked[idx].clone();
        let mut decided: Option<AssignResult> = None;

        for attempt in 1..=MAX_RETRIES {
            // Stale-window check before every attempt. If we're more
            // than W=64 revisions behind, we cannot trust the ranking
            // we just computed — refresh and re-rank.
            let current = current_cluster_revision(etcd).await?;
            if (working_rev - current).abs() > STALE_WINDOW {
                debug!(
                    task = %task.id,
                    working_rev,
                    current,
                    "snapshot beyond stale window — refreshing"
                );
                let (kvs, rev) = etcd.get_prefix_with_revision(NODES_PREFIX).await?;
                working_rev = rev;
                // Rebuild the joined candidate list from the fresh
                // membership view. We don't reach into MembershipSnapshot
                // here — we just re-read /boi/nodes/ directly so the
                // refresh is self-contained.
                joined = rebuild_candidates(etcd, &kvs).await?;
                candidates = capability_filter(&joined, &task.requires);
                if candidates.is_empty() {
                    return Ok(AssignResult::NeedProvision);
                }
                ranked = hrw_rank(&task.id, &candidates);
                // Restart the walk against the refreshed ranking. If the
                // previous candidate is no longer present we want the
                // new top pick to get first shot, not the carry-over.
                idx = 0;
                decided = None;
                break;
            }

            let claim = ClaimRecord {
                task_id: task.id.clone(),
                node_id: node_id.clone(),
                lease_id: claim_lease_id,
                claimed_at: now_unix(),
                attempt,
            };

            match claim.acquire(etcd).await {
                Ok(()) => {
                    debug!(
                        task = %task.id,
                        node = %node_id,
                        attempt,
                        "claim acquired"
                    );
                    decided = Some(AssignResult::Assigned(claim));
                    break;
                }
                Err(ClusterError::Conflict(msg)) => {
                    warn!(
                        task = %task.id,
                        node = %node_id,
                        attempt,
                        %msg,
                        "claim CAS conflict — refreshing revision and retrying"
                    );
                    working_rev = current_cluster_revision(etcd).await?;
                    // Loop: retry against the same node up to MAX_RETRIES.
                }
                Err(e) => return Err(AssignError::Cluster(e)),
            }
        }

        if let Some(result) = decided {
            return Ok(result);
        }
        // Either we exhausted MAX_RETRIES on this candidate, or the
        // stale-window refresh restarted the loop (idx reset to 0). In
        // the exhaustion case, advance to the next candidate.
        if idx < ranked.len() && ranked[idx] == node_id {
            idx += 1;
        }
    }

    Ok(AssignResult::NeedProvision)
}

async fn rebuild_candidates(
    etcd: &EtcdClient,
    kvs: &[(Vec<u8>, Vec<u8>)],
) -> Result<Vec<AssignNode>> {
    let mut out = Vec::with_capacity(kvs.len());
    for (k, v) in kvs {
        let id = match std::str::from_utf8(k)
            .ok()
            .and_then(|s| s.strip_prefix(NODES_PREFIX))
        {
            Some(id) => id.to_string(),
            None => continue,
        };
        let rec: boi_cluster::nodes::NodeRecord = match serde_json::from_slice(v) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let caps = NodeCaps::get(etcd, &id).await?.unwrap_or_default();
        out.push(AssignNode::new(rec, caps));
    }
    Ok(out)
}

// =====================================================================
// Tests
// =====================================================================
//
// Tests run against a real `bitnami/etcd:3.5` container via
// `testcontainers`. If Docker is not available the test logs a skip
// and returns Ok so `cargo test -p boi-assign` is green on machines
// without a container runtime — same pattern as boi-cluster.

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::time::Instant;

    use boi_cluster::client::EtcdClient;
    use boi_cluster::nodes::{NodeCaps, NodeRecord};

    use testcontainers::{
        core::{IntoContainerPort, WaitFor},
        runners::AsyncRunner,
        GenericImage, ImageExt,
    };

    fn docker_available() -> bool {
        std::process::Command::new("docker")
            .arg("info")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    async fn etcd_endpoint() -> Option<(
        testcontainers::ContainerAsync<GenericImage>,
        String,
    )> {
        if !docker_available() {
            eprintln!("docker not available — skipping live-etcd subtest");
            return None;
        }
        let img = GenericImage::new("bitnami/etcd", "3.5")
            .with_exposed_port(2379.tcp())
            .with_wait_for(WaitFor::message_on_stderr("ready to serve client requests"))
            .with_env_var("ALLOW_NONE_AUTHENTICATION", "yes")
            .with_env_var("ETCD_ADVERTISE_CLIENT_URLS", "http://0.0.0.0:2379")
            .with_env_var("ETCD_LISTEN_CLIENT_URLS", "http://0.0.0.0:2379");
        let container = match img.start().await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("failed to start etcd container; skipping: {e}");
                return None;
            }
        };
        let port = match container.get_host_port_ipv4(2379).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("failed to read mapped port; skipping: {e}");
                return None;
            }
        };
        Some((container, format!("http://127.0.0.1:{port}")))
    }

    async fn register_node(
        client: &EtcdClient,
        id: &str,
        static_caps: &[(&str, &str)],
        lease_id: Option<i64>,
    ) {
        let rec = NodeRecord {
            node_id: id.into(),
            addr: format!("127.0.0.1:{}", 7000 + id.len()),
            version: "0.1.0".into(),
            started_at: 1_700_000_000,
        };
        rec.put(client, lease_id).await.expect("put node");
        let mut caps = NodeCaps::default();
        for (k, v) in static_caps {
            caps.r#static.insert((*k).into(), (*v).into());
        }
        caps.put(client, id, lease_id).await.expect("put caps");
    }

    async fn snapshot_from_etcd(client: &EtcdClient) -> MembershipSnapshot {
        let (kvs, rev) = client
            .get_prefix_with_revision(NODES_PREFIX)
            .await
            .expect("list nodes");
        let mut nodes = BTreeMap::new();
        for (k, v) in kvs {
            let id = std::str::from_utf8(&k)
                .ok()
                .and_then(|s| s.strip_prefix(NODES_PREFIX))
                .map(|s| s.to_string());
            if let Some(id) = id {
                if let Ok(rec) = serde_json::from_slice::<NodeRecord>(&v) {
                    nodes.insert(id, rec);
                }
            }
        }
        MembershipSnapshot {
            nodes,
            mod_revision: rev,
            refreshed_at: Instant::now(),
        }
    }

    #[test]
    fn stale_window_constant_is_64() {
        // Smoke: the W=64 design knob in §16 Q1 must remain pinned here
        // so a typo doesn't silently widen the staleness budget.
        assert_eq!(STALE_WINDOW, 64);
    }

    #[test]
    fn max_retries_constant_is_3() {
        assert_eq!(MAX_RETRIES, 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn assign_picks_hrw_top_capable_node() {
        let Some((_c, ep)) = etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");

        // Three mac nodes — all are capable. HRW picks one deterministically.
        register_node(&client, "node-a", &[("os", "mac")], None).await;
        register_node(&client, "node-b", &[("os", "mac")], None).await;
        register_node(&client, "node-c", &[("os", "mac")], None).await;

        let snap = snapshot_from_etcd(&client).await;
        let task = TaskRecord {
            id: "t1".into(),
            requires: CapRequires::new().with("os", "mac"),
        };

        // Predict the HRW winner using the same primitives the loop uses.
        let joined = join_caps_pub(&client, &snap).await.expect("join");
        let filtered = capability_filter(&joined, &task.requires);
        let expected = hrw_rank(&task.id, &filtered)
            .into_iter()
            .next()
            .expect("at least one candidate");

        let lease = client.grant_lease(10).await.expect("lease");
        let res = assign(&task, &snap, &client, lease.lease_id)
            .await
            .expect("assign");
        match res {
            AssignResult::Assigned(claim) => {
                assert_eq!(claim.node_id, expected);
                assert_eq!(claim.task_id, "t1");
                assert_eq!(claim.lease_id, lease.lease_id);
            }
            other => panic!("expected Assigned, got {:?}", other),
        }

        // Side-effect: the claim envelope and fencing sub-key exist.
        let envelope = ClaimRecord::get(&client, "t1")
            .await
            .expect("get claim")
            .expect("claim present");
        assert_eq!(envelope.node_id, expected);
        let fence = ClaimRecord::current_lease_id(&client, "t1")
            .await
            .expect("get fence")
            .expect("fence present");
        assert_eq!(fence, lease.lease_id);

        ClaimRecord::release(&client, "t1").await.ok();
        client.revoke_lease(lease).await.ok();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn assign_filters_by_capability_excluding_non_matching_nodes() {
        let Some((_c, ep)) = etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");

        register_node(&client, "linux-1", &[("os", "linux")], None).await;
        register_node(&client, "linux-2", &[("os", "linux")], None).await;
        register_node(&client, "mac-1", &[("os", "mac")], None).await;

        let snap = snapshot_from_etcd(&client).await;
        let task = TaskRecord {
            id: "t-mac".into(),
            requires: CapRequires::new().with("os", "mac"),
        };

        let lease = client.grant_lease(10).await.expect("lease");
        let res = assign(&task, &snap, &client, lease.lease_id)
            .await
            .expect("assign");
        match res {
            AssignResult::Assigned(claim) => assert_eq!(claim.node_id, "mac-1"),
            other => panic!("expected Assigned to mac-1, got {:?}", other),
        }

        ClaimRecord::release(&client, "t-mac").await.ok();
        client.revoke_lease(lease).await.ok();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn assign_with_stale_snapshot_refreshes_then_succeeds() {
        // Stale window: pass a snapshot whose mod_revision is 100 ahead
        // of reality. The pre-CAS stale check trips, the loop re-reads
        // membership, and the CAS proceeds against the refreshed view.
        let Some((_c, ep)) = etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");

        register_node(&client, "node-x", &[("os", "mac")], None).await;
        let mut snap = snapshot_from_etcd(&client).await;
        // Force staleness: pretend we ranked on a revision far ahead of
        // the real cluster (|snap.rev - current| > 64).
        snap.mod_revision += 200;

        let task = TaskRecord {
            id: "t-stale".into(),
            requires: CapRequires::new().with("os", "mac"),
        };
        let lease = client.grant_lease(10).await.expect("lease");

        let res = assign(&task, &snap, &client, lease.lease_id)
            .await
            .expect("assign");
        match res {
            AssignResult::Assigned(claim) => assert_eq!(claim.node_id, "node-x"),
            other => panic!("expected Assigned after refresh, got {:?}", other),
        }

        ClaimRecord::release(&client, "t-stale").await.ok();
        client.revoke_lease(lease).await.ok();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn assign_returns_need_provision_when_all_candidates_busy() {
        let Some((_c, ep)) = etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");

        register_node(&client, "only-node", &[("os", "mac")], None).await;

        // Pre-claim t-busy under a *different* lease so the CAS will
        // see version != 0 and conflict.
        let pre_lease = client.grant_lease(60).await.expect("pre-lease");
        let pre = ClaimRecord {
            task_id: "t-busy".into(),
            node_id: "someone-else".into(),
            lease_id: pre_lease.lease_id,
            claimed_at: 1_700_000_000,
            attempt: 1,
        };
        pre.acquire(&client).await.expect("pre-claim");

        let snap = snapshot_from_etcd(&client).await;
        let task = TaskRecord {
            id: "t-busy".into(),
            requires: CapRequires::new().with("os", "mac"),
        };
        let lease = client.grant_lease(10).await.expect("lease");
        let res = assign(&task, &snap, &client, lease.lease_id)
            .await
            .expect("assign");
        assert!(
            matches!(res, AssignResult::NeedProvision),
            "expected NeedProvision when every capable candidate's claim conflicts, got {:?}",
            res
        );

        // The pre-existing claim is unchanged.
        let envelope = ClaimRecord::get(&client, "t-busy")
            .await
            .expect("get")
            .expect("present");
        assert_eq!(envelope.node_id, "someone-else");

        ClaimRecord::release(&client, "t-busy").await.ok();
        client.revoke_lease(pre_lease).await.ok();
        client.revoke_lease(lease).await.ok();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn assign_returns_need_provision_when_no_capable_node_exists() {
        let Some((_c, ep)) = etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");

        register_node(&client, "linux-only", &[("os", "linux")], None).await;
        let snap = snapshot_from_etcd(&client).await;
        let task = TaskRecord {
            id: "t-nomatch".into(),
            requires: CapRequires::new().with("os", "mac"),
        };
        let lease = client.grant_lease(10).await.expect("lease");
        let res = assign(&task, &snap, &client, lease.lease_id)
            .await
            .expect("assign");
        assert!(matches!(res, AssignResult::NeedProvision));
        client.revoke_lease(lease).await.ok();
    }
}
