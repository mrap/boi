//! Consecutive-claim-failure cooldown (F-06).
//!
//! Per critique F-06: a node whose claim CAS keeps failing is a node
//! that is either flapping, overloaded, or wedged. After three failures
//! within a 5-minute window we flip its `caps.dynamic.health` to
//! `degraded` so the [`capability_filter`](crate::hrw::capability_filter)
//! in `hrw.rs` skips it. After the 5-minute window elapses without a
//! fresh failure the counter (and the degraded flag we set) clear.
//!
//! Storage layout — note the deviation from the spec wording:
//!
//! The spec text suggested `/boi/nodes/{id}/claim_failures`, but
//! `boi-cluster::nodes::NodeRecord::list` and
//! `MembershipSnapshot::refresh` both prefix-list `/boi/nodes/` and
//! decode every value as a `NodeRecord`. A sibling sub-key under
//! `/boi/nodes/` would break those decoders. We therefore namespace
//! cooldown state under `/boi/claim_failures/{id}` so the nodes prefix
//! stays homogeneous.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use boi_cluster::client::{EtcdClient, Result};
use boi_cluster::nodes::NodeCaps;

/// Per-node etcd prefix for cooldown state.
pub const CLAIM_FAILURES_PREFIX: &str = "/boi/claim_failures/";

/// Failures within `COOLDOWN_WINDOW_SECS` needed before a node is
/// flipped to `health=degraded`.
pub const FAILURE_THRESHOLD: u32 = 3;

/// Rolling window for the consecutive-failure counter. Once
/// `COOLDOWN_WINDOW_SECS` elapses without a fresh failure the counter
/// (and the degraded flag we set) clear on the next observation.
pub const COOLDOWN_WINDOW_SECS: i64 = 300; // 5 minutes

/// Dynamic-cap key used to take a node out of HRW rotation.
pub const HEALTH_KEY: &str = "health";
/// Value written to [`HEALTH_KEY`] when the cooldown trips.
pub const HEALTH_DEGRADED: &str = "degraded";

/// Cooldown record stored at `/boi/claim_failures/{id}`.
///
/// `first_failure_at` is the unix-seconds timestamp at which the
/// current window started; `last_failure_at` is the most recent failure.
/// `count` is the number of failures observed in the current window.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClaimFailures {
    pub count: u32,
    pub first_failure_at: i64,
    pub last_failure_at: i64,
}

fn failures_key(node_id: &str) -> String {
    format!("{CLAIM_FAILURES_PREFIX}{node_id}")
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl ClaimFailures {
    pub async fn get(client: &EtcdClient, node_id: &str) -> Result<Option<Self>> {
        let raw = match client.get(failures_key(node_id)).await? {
            Some(b) => b,
            None => return Ok(None),
        };
        serde_json::from_slice(&raw).map(Some).map_err(|e| {
            boi_cluster::client::ClusterError::Invalid(format!("decode ClaimFailures: {e}"))
        })
    }

    pub async fn put(&self, client: &EtcdClient, node_id: &str) -> Result<()> {
        let body = serde_json::to_vec(self).map_err(|e| {
            boi_cluster::client::ClusterError::Invalid(format!("encode ClaimFailures: {e}"))
        })?;
        client.put(failures_key(node_id), body, None).await
    }

    pub async fn delete(client: &EtcdClient, node_id: &str) -> Result<bool> {
        client.delete(failures_key(node_id)).await
    }
}

/// Flip `caps.dynamic.health = degraded` for `node_id`. No-op if the
/// node has no caps record yet (degradation only matters once a node
/// is advertising itself).
async fn mark_degraded(client: &EtcdClient, node_id: &str) -> Result<()> {
    let mut caps = match NodeCaps::get(client, node_id).await? {
        Some(c) => c,
        None => {
            warn!(node = %node_id, "cooldown: no caps record to flip degraded");
            return Ok(());
        }
    };
    caps.dynamic
        .insert(HEALTH_KEY.into(), HEALTH_DEGRADED.into());
    caps.put(client, node_id, None).await
}

/// Clear `caps.dynamic.health` iff it is currently `degraded`. Leaves
/// any other operator-set health value alone — we only undo what the
/// cooldown itself set.
async fn clear_degraded(client: &EtcdClient, node_id: &str) -> Result<()> {
    let mut caps = match NodeCaps::get(client, node_id).await? {
        Some(c) => c,
        None => return Ok(()),
    };
    if caps.dynamic.get(HEALTH_KEY).map(String::as_str) == Some(HEALTH_DEGRADED) {
        caps.dynamic.remove(HEALTH_KEY);
        caps.put(client, node_id, None).await?;
    }
    Ok(())
}

/// Record a single claim-CAS failure against `node_id`. Returns the
/// updated failure record. When the count reaches [`FAILURE_THRESHOLD`]
/// the node's `caps.dynamic.health` is flipped to `degraded`.
///
/// `now` is the unix-seconds timestamp the caller wants the failure
/// stamped with — pass `None` for "real now". Tests pass a fixed value
/// so they don't depend on wall clock.
pub async fn record_claim_failure(
    client: &EtcdClient,
    node_id: &str,
    now: Option<i64>,
) -> Result<ClaimFailures> {
    let now = now.unwrap_or_else(now_unix);
    let existing = ClaimFailures::get(client, node_id).await?;

    let mut state = match existing {
        Some(s) if now - s.last_failure_at <= COOLDOWN_WINDOW_SECS => s,
        // First failure, or the prior window has fully elapsed.
        _ => ClaimFailures {
            count: 0,
            first_failure_at: now,
            last_failure_at: now,
        },
    };
    state.count = state.count.saturating_add(1);
    state.last_failure_at = now;

    state.put(client, node_id).await?;

    if state.count >= FAILURE_THRESHOLD {
        debug!(
            node = %node_id,
            count = state.count,
            "cooldown threshold reached — marking node degraded"
        );
        mark_degraded(client, node_id).await?;
    }

    Ok(state)
}

/// Reset the consecutive-failure counter for `node_id` after a
/// successful claim. Clears the degraded flag if (and only if) it was
/// set by the cooldown.
pub async fn record_claim_success(client: &EtcdClient, node_id: &str) -> Result<()> {
    ClaimFailures::delete(client, node_id).await?;
    clear_degraded(client, node_id).await?;
    Ok(())
}

/// Sweep the cooldown record for `node_id`. If the last failure is
/// older than [`COOLDOWN_WINDOW_SECS`] the counter is dropped and any
/// cooldown-set `degraded` flag is cleared. Returns `true` if state
/// was changed.
///
/// Typical use: a periodic janitor task walks every known node and
/// calls this so stale degradations don't keep a recovered node out
/// of rotation forever.
pub async fn clear_expired_cooldown(
    client: &EtcdClient,
    node_id: &str,
    now: Option<i64>,
) -> Result<bool> {
    let now = now.unwrap_or_else(now_unix);
    let state = match ClaimFailures::get(client, node_id).await? {
        Some(s) => s,
        None => return Ok(false),
    };
    if now - state.last_failure_at <= COOLDOWN_WINDOW_SECS {
        return Ok(false);
    }
    ClaimFailures::delete(client, node_id).await?;
    clear_degraded(client, node_id).await?;
    debug!(node = %node_id, "cooldown expired — node returned to rotation");
    Ok(true)
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

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

    async fn register_node(client: &EtcdClient, id: &str, static_caps: &[(&str, &str)]) {
        let rec = NodeRecord {
            node_id: id.into(),
            addr: format!("127.0.0.1:{}", 7000 + id.len()),
            version: "0.1.0".into(),
            started_at: 1_700_000_000,
        };
        rec.put(client, None).await.expect("put node");
        let mut caps = NodeCaps::default();
        for (k, v) in static_caps {
            caps.r#static.insert((*k).into(), (*v).into());
        }
        caps.put(client, id, None).await.expect("put caps");
    }

    // ---- Pure unit tests ------------------------------------------------

    #[test]
    fn cooldown_constants_match_design() {
        // F-06: three consecutive failures within a 5-minute window.
        assert_eq!(FAILURE_THRESHOLD, 3);
        assert_eq!(COOLDOWN_WINDOW_SECS, 300);
    }

    #[test]
    fn failures_key_namespaces_under_claim_failures_prefix() {
        assert_eq!(failures_key("node-a"), "/boi/claim_failures/node-a");
    }

    // ---- Live-etcd tests -----------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cooldown_three_failures_mark_node_degraded() {
        let Some((_c, ep)) = etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");

        register_node(&client, "node-a", &[("os", "mac")]).await;

        // T+0, T+10, T+20 — three failures inside the 5-minute window.
        let s1 = record_claim_failure(&client, "node-a", Some(1_000))
            .await
            .expect("rec 1");
        assert_eq!(s1.count, 1);
        let caps = NodeCaps::get(&client, "node-a")
            .await
            .expect("get caps")
            .expect("present");
        assert!(
            caps.dynamic.get(HEALTH_KEY).is_none(),
            "node not yet degraded after 1 failure",
        );

        let s2 = record_claim_failure(&client, "node-a", Some(1_010))
            .await
            .expect("rec 2");
        assert_eq!(s2.count, 2);

        let s3 = record_claim_failure(&client, "node-a", Some(1_020))
            .await
            .expect("rec 3");
        assert_eq!(s3.count, 3);

        let caps = NodeCaps::get(&client, "node-a")
            .await
            .expect("get caps")
            .expect("present");
        assert_eq!(
            caps.dynamic.get(HEALTH_KEY).map(String::as_str),
            Some(HEALTH_DEGRADED),
            "after threshold the node must be flipped to degraded",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cooldown_degraded_node_is_skipped_by_capability_filter() {
        // End-to-end through hrw::capability_filter: a degraded node
        // is dropped from the candidate set. The filter already enforces
        // this (see hrw.rs); here we prove the cooldown writes the right
        // shape for the filter to act on.
        let Some((_c, ep)) = etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");

        register_node(&client, "node-a", &[("os", "mac")]).await;
        register_node(&client, "node-b", &[("os", "mac")]).await;

        for t in [1_000, 1_010, 1_020] {
            record_claim_failure(&client, "node-a", Some(t))
                .await
                .expect("rec");
        }

        let caps_a = NodeCaps::get(&client, "node-a")
            .await
            .expect("get")
            .expect("present");
        let caps_b = NodeCaps::get(&client, "node-b")
            .await
            .expect("get")
            .expect("present");

        let nodes = vec![
            crate::hrw::AssignNode::new(
                NodeRecord {
                    node_id: "node-a".into(),
                    addr: "127.0.0.1:7006".into(),
                    version: "0.1.0".into(),
                    started_at: 1_700_000_000,
                },
                caps_a,
            ),
            crate::hrw::AssignNode::new(
                NodeRecord {
                    node_id: "node-b".into(),
                    addr: "127.0.0.1:7006".into(),
                    version: "0.1.0".into(),
                    started_at: 1_700_000_000,
                },
                caps_b,
            ),
        ];
        let req = crate::hrw::CapRequires::new().with("os", "mac");
        let filtered: Vec<String> = crate::hrw::capability_filter(&nodes, &req)
            .into_iter()
            .map(|n| n.id().to_string())
            .collect();
        assert_eq!(
            filtered,
            vec!["node-b".to_string()],
            "degraded node-a must be filtered out",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cooldown_clears_after_window_elapses() {
        let Some((_c, ep)) = etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");

        register_node(&client, "node-a", &[("os", "mac")]).await;
        for t in [1_000, 1_010, 1_020] {
            record_claim_failure(&client, "node-a", Some(t))
                .await
                .expect("rec");
        }
        let caps = NodeCaps::get(&client, "node-a")
            .await
            .expect("get")
            .expect("present");
        assert_eq!(
            caps.dynamic.get(HEALTH_KEY).map(String::as_str),
            Some(HEALTH_DEGRADED),
        );

        // Inside window: clear is a no-op.
        let cleared = clear_expired_cooldown(&client, "node-a", Some(1_100))
            .await
            .expect("clear inside window");
        assert!(!cleared);
        let caps = NodeCaps::get(&client, "node-a")
            .await
            .expect("get")
            .expect("present");
        assert_eq!(
            caps.dynamic.get(HEALTH_KEY).map(String::as_str),
            Some(HEALTH_DEGRADED),
            "must still be degraded inside the window",
        );

        // Past window: clear must drop the record and the degraded flag.
        let now = 1_020 + COOLDOWN_WINDOW_SECS + 1;
        let cleared = clear_expired_cooldown(&client, "node-a", Some(now))
            .await
            .expect("clear past window");
        assert!(cleared);

        let caps = NodeCaps::get(&client, "node-a")
            .await
            .expect("get")
            .expect("present");
        assert!(
            caps.dynamic.get(HEALTH_KEY).is_none(),
            "degraded flag must be cleared after cooldown",
        );
        assert!(
            ClaimFailures::get(&client, "node-a")
                .await
                .expect("get")
                .is_none(),
            "failure record must be gone after cooldown clear",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cooldown_success_resets_counter_and_clears_degraded() {
        let Some((_c, ep)) = etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");

        register_node(&client, "node-a", &[("os", "mac")]).await;
        for t in [1_000, 1_010, 1_020] {
            record_claim_failure(&client, "node-a", Some(t))
                .await
                .expect("rec");
        }

        record_claim_success(&client, "node-a")
            .await
            .expect("success");

        assert!(
            ClaimFailures::get(&client, "node-a")
                .await
                .expect("get")
                .is_none(),
            "success must drop the failure record",
        );
        let caps = NodeCaps::get(&client, "node-a")
            .await
            .expect("get")
            .expect("present");
        assert!(
            caps.dynamic.get(HEALTH_KEY).is_none(),
            "success must clear the degraded flag",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cooldown_failure_outside_window_starts_fresh_count() {
        // A single failure, then a long gap, then a second failure
        // should NOT push count to 2 — the window resets.
        let Some((_c, ep)) = etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");

        register_node(&client, "node-a", &[("os", "mac")]).await;

        let s1 = record_claim_failure(&client, "node-a", Some(1_000))
            .await
            .expect("rec 1");
        assert_eq!(s1.count, 1);

        let s2 = record_claim_failure(
            &client,
            "node-a",
            Some(1_000 + COOLDOWN_WINDOW_SECS + 1),
        )
        .await
        .expect("rec 2");
        assert_eq!(s2.count, 1, "window elapsed — counter must restart");
        assert_eq!(s2.first_failure_at, 1_000 + COOLDOWN_WINDOW_SECS + 1);
    }
}
