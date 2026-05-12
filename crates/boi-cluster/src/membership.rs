//! Cluster membership — etcd watch + cached snapshot.
//!
//! Per design §4 / Q1:
//! - On start, range-read `/boi/nodes/` and capture the etcd header
//!   revision. That `mod_revision` is the pin Phase 4's assignment loop
//!   will compare against (`cluster.assign.snapshot_revision_window`).
//! - A background task watches `/boi/nodes/` starting at `revision + 1`
//!   and applies PUT/DELETE events to the in-memory snapshot.
//! - Snapshots have a 30 s TTL. `snapshot()` returns the cached view if
//!   fresh; if the cache is older than TTL we attempt an inline resync,
//!   and if that fails we return [`ClusterError::StaleSnapshot`] — never
//!   silently hand back a stale view.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use etcd_client::EventType;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::client::{ClusterError, EtcdClient, Result};
use crate::nodes::{NodeRecord, NODES_PREFIX};

/// Default TTL after which a cached snapshot is considered stale.
pub const DEFAULT_SNAPSHOT_TTL: Duration = Duration::from_secs(30);

/// Immutable view of cluster membership at a specific etcd revision.
///
/// The `mod_revision` is the etcd header revision served alongside the
/// list read that produced this snapshot (per Q1).
#[derive(Debug, Clone)]
pub struct MembershipSnapshot {
    pub nodes: BTreeMap<String, NodeRecord>,
    pub mod_revision: i64,
    pub refreshed_at: Instant,
}

impl MembershipSnapshot {
    #[cfg(test)]
    fn empty(now: Instant) -> Self {
        Self {
            nodes: BTreeMap::new(),
            mod_revision: 0,
            refreshed_at: now,
        }
    }

    pub fn is_stale(&self, ttl: Duration, now: Instant) -> bool {
        now.saturating_duration_since(self.refreshed_at) > ttl
    }

    pub fn contains(&self, node_id: &str) -> bool {
        self.nodes.contains_key(node_id)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

fn node_id_from_key(key: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(key).ok()?;
    s.strip_prefix(NODES_PREFIX).map(|id| id.to_string())
}

/// Tracks membership via an etcd watch on `/boi/nodes/`.
///
/// Cloneable; clones share the underlying snapshot cache and watcher
/// task. The watcher task is aborted when the last clone drops.
#[derive(Clone)]
pub struct Membership {
    inner: Arc<Inner>,
}

struct Inner {
    client: EtcdClient,
    snapshot: RwLock<MembershipSnapshot>,
    ttl: Duration,
    watcher: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Ok(mut g) = self.watcher.try_lock() {
            if let Some(h) = g.take() {
                h.abort();
            }
        }
    }
}

impl Membership {
    /// Start a membership tracker with [`DEFAULT_SNAPSHOT_TTL`].
    pub async fn start(client: EtcdClient) -> Result<Self> {
        Self::start_with_ttl(client, DEFAULT_SNAPSHOT_TTL).await
    }

    /// Start a membership tracker with a caller-supplied TTL.
    /// Tests use a sub-second TTL to keep the suite fast.
    pub async fn start_with_ttl(client: EtcdClient, ttl: Duration) -> Result<Self> {
        if ttl.is_zero() {
            return Err(ClusterError::Invalid("ttl must be > 0".into()));
        }
        let snap = read_snapshot(&client).await?;
        let start_rev = snap.mod_revision + 1;
        let me = Self {
            inner: Arc::new(Inner {
                client: client.clone(),
                snapshot: RwLock::new(snap),
                ttl,
                watcher: tokio::sync::Mutex::new(None),
            }),
        };
        let task = tokio::spawn(watch_loop(me.inner.clone(), start_rev));
        *me.inner.watcher.lock().await = Some(task);
        Ok(me)
    }

    /// Returns the current snapshot.
    ///
    /// If the cached snapshot is older than the TTL we trigger an
    /// inline resync. If that resync fails we surface
    /// [`ClusterError::StaleSnapshot`]; we never return a known-stale
    /// view silently.
    pub async fn snapshot(&self) -> Result<MembershipSnapshot> {
        let ttl = self.inner.ttl;
        {
            let guard = self.inner.snapshot.read().await;
            if !guard.is_stale(ttl, Instant::now()) {
                return Ok(guard.clone());
            }
        }
        match read_snapshot(&self.inner.client).await {
            Ok(fresh) => {
                let mut guard = self.inner.snapshot.write().await;
                // Only overwrite if the resync moved forward in time.
                if fresh.mod_revision >= guard.mod_revision {
                    *guard = fresh.clone();
                }
                Ok(fresh)
            }
            Err(e) => {
                warn!(error = %e, "membership resync failed; returning StaleSnapshot");
                Err(ClusterError::StaleSnapshot)
            }
        }
    }

    /// Force a full list-resync, regardless of the cache's age.
    pub async fn refresh(&self) -> Result<MembershipSnapshot> {
        let fresh = read_snapshot(&self.inner.client).await?;
        let mut guard = self.inner.snapshot.write().await;
        if fresh.mod_revision >= guard.mod_revision {
            *guard = fresh.clone();
        }
        Ok(fresh)
    }

    /// Snapshot age. Exposed for tests.
    pub async fn age(&self) -> Duration {
        Instant::now().saturating_duration_since(self.inner.snapshot.read().await.refreshed_at)
    }
}

async fn read_snapshot(client: &EtcdClient) -> Result<MembershipSnapshot> {
    let (kvs, rev) = client.get_prefix_with_revision(NODES_PREFIX).await?;
    let mut nodes = BTreeMap::new();
    for (k, v) in kvs {
        let Some(id) = node_id_from_key(&k) else { continue };
        match serde_json::from_slice::<NodeRecord>(&v) {
            Ok(rec) => {
                nodes.insert(id, rec);
            }
            Err(e) => {
                warn!(node_id = %id, error = %e, "skip undecodable NodeRecord");
            }
        }
    }
    Ok(MembershipSnapshot {
        nodes,
        mod_revision: rev,
        refreshed_at: Instant::now(),
    })
}

async fn watch_loop(inner: Arc<Inner>, mut start_rev: i64) {
    loop {
        let opened = inner.client.watch_prefix(NODES_PREFIX, start_rev).await;
        let (_watcher, mut stream) = match opened {
            Ok(pair) => pair,
            Err(e) => {
                warn!(error = %e, "membership watch open failed; resyncing");
                if let Ok(snap) = read_snapshot(&inner.client).await {
                    start_rev = snap.mod_revision + 1;
                    *inner.snapshot.write().await = snap;
                } else {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                continue;
            }
        };

        loop {
            match stream.message().await {
                Ok(Some(resp)) => {
                    if resp.canceled() {
                        debug!("membership watch canceled by server; reopening");
                        break;
                    }
                    for ev in resp.events() {
                        let Some(kv) = ev.kv() else { continue };
                        let Some(id) = node_id_from_key(kv.key()) else { continue };
                        let mut guard = inner.snapshot.write().await;
                        match ev.event_type() {
                            EventType::Put => {
                                if let Ok(rec) =
                                    serde_json::from_slice::<NodeRecord>(kv.value())
                                {
                                    guard.nodes.insert(id, rec);
                                }
                                guard.mod_revision = guard.mod_revision.max(kv.mod_revision());
                            }
                            EventType::Delete => {
                                guard.nodes.remove(&id);
                                guard.mod_revision = guard.mod_revision.max(kv.mod_revision());
                            }
                        }
                        guard.refreshed_at = Instant::now();
                    }
                    if let Some(h) = resp.header() {
                        let rev = h.revision();
                        if rev > 0 {
                            start_rev = rev + 1;
                        }
                    }
                }
                Ok(None) => {
                    debug!("membership watch stream closed; reopening");
                    break;
                }
                Err(e) => {
                    warn!(error = %e, "membership watch recv failed; reopening");
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::EtcdClient;

    fn rec(id: &str) -> NodeRecord {
        NodeRecord {
            node_id: id.into(),
            addr: format!("127.0.0.1:7{:03}", id.len()),
            version: "0.1.0".into(),
            started_at: 1_700_000_000,
        }
    }

    // ---- Pure unit ------------------------------------------------------

    #[test]
    fn snapshot_staleness_uses_refreshed_at() {
        let now = Instant::now();
        let snap = MembershipSnapshot::empty(now);
        let ttl = Duration::from_secs(30);
        assert!(!snap.is_stale(ttl, now));
        assert!(!snap.is_stale(ttl, now + Duration::from_secs(29)));
        assert!(snap.is_stale(ttl, now + Duration::from_secs(31)));
    }

    #[test]
    fn node_id_from_key_strips_prefix() {
        assert_eq!(node_id_from_key(b"/boi/nodes/abc"), Some("abc".to_string()));
        assert_eq!(node_id_from_key(b"/other/abc"), None);
        // Non-utf8 keys are ignored, not panicked on.
        assert_eq!(node_id_from_key(&[0xff, 0xff]), None);
    }

    // ---- Live etcd ------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_captures_existing_nodes_and_revision() {
        let Some((_c, ep)) = crate::testutil::etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");
        rec("a").put(&client, None).await.expect("put a");
        rec("b").put(&client, None).await.expect("put b");

        let m = Membership::start_with_ttl(client.clone(), Duration::from_secs(30))
            .await
            .expect("start");
        let snap = m.snapshot().await.expect("snapshot");
        assert_eq!(snap.len(), 2);
        assert!(snap.contains("a"));
        assert!(snap.contains("b"));
        assert!(snap.mod_revision > 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watch_propagates_put_and_delete() {
        let Some((_c, ep)) = crate::testutil::etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");

        let m = Membership::start_with_ttl(client.clone(), Duration::from_secs(30))
            .await
            .expect("start");
        assert_eq!(m.snapshot().await.expect("s0").len(), 0);

        rec("n1").put(&client, None).await.expect("put n1");
        // Wait for watcher to observe — bounded poll.
        let mut seen = false;
        for _ in 0..40 {
            if m.snapshot().await.expect("s").contains("n1") {
                seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(seen, "expected watcher to surface n1 via PUT");

        assert!(NodeRecord::delete(&client, "n1").await.expect("del"));
        let mut gone = false;
        for _ in 0..40 {
            if !m.snapshot().await.expect("s").contains("n1") {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(gone, "expected watcher to surface n1 via DELETE");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn three_nodes_register_then_lease_revoke_drops_member() {
        // Mirrors design §4: "3 BOI nodes register, kill one, the
        // others detect within 2× lease TTL". We model a node death
        // by revoking its lease (etcd then garbage-collects the
        // lease-bound NodeRecord — same observable effect as a node
        // process exit).
        let Some((_c, ep)) = crate::testutil::etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");

        let ttl_secs = 2_i64;
        let l1 = client.grant_lease(ttl_secs).await.expect("lease 1");
        let l2 = client.grant_lease(ttl_secs).await.expect("lease 2");
        let l3 = client.grant_lease(ttl_secs).await.expect("lease 3");

        rec("n1")
            .put(&client, Some(l1.lease_id))
            .await
            .expect("put n1");
        rec("n2")
            .put(&client, Some(l2.lease_id))
            .await
            .expect("put n2");
        rec("n3")
            .put(&client, Some(l3.lease_id))
            .await
            .expect("put n3");

        let m = Membership::start_with_ttl(client.clone(), Duration::from_secs(30))
            .await
            .expect("start");

        // All 3 visible.
        let mut all_seen = false;
        for _ in 0..40 {
            let s = m.snapshot().await.expect("s");
            if s.len() == 3 && s.contains("n1") && s.contains("n2") && s.contains("n3") {
                all_seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(all_seen, "expected all 3 nodes in initial snapshot");

        // "Kill" node 2 by revoking its lease.
        client.revoke_lease(l2).await.expect("revoke n2");

        // Watcher must surface the loss within 2× lease TTL.
        let deadline = Instant::now() + Duration::from_secs((ttl_secs * 2) as u64 + 1);
        let mut detected = false;
        while Instant::now() < deadline {
            let s = m.snapshot().await.expect("s");
            if !s.contains("n2") && s.contains("n1") && s.contains("n3") {
                detected = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            detected,
            "expected n2 to disappear from membership within 2× lease TTL"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_cache_triggers_inline_resync() {
        let Some((_c, ep)) = crate::testutil::etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");
        rec("x").put(&client, None).await.expect("put x");

        let m = Membership::start_with_ttl(client.clone(), Duration::from_millis(50))
            .await
            .expect("start");

        // Wait past TTL so the cache is stale.
        tokio::time::sleep(Duration::from_millis(120)).await;
        // snapshot() must succeed (etcd is reachable, resync works) and
        // must reflect a fresh refreshed_at (age < TTL after the call).
        let s = m.snapshot().await.expect("snapshot after stale");
        assert!(s.contains("x"));
        let age = Instant::now().saturating_duration_since(s.refreshed_at);
        assert!(
            age < Duration::from_millis(50),
            "resync should produce a fresh refreshed_at, got age = {:?}",
            age
        );
    }
}
