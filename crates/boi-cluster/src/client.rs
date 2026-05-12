//! Typed `EtcdClient` wrapper.
//!
//! Wraps the `etcd-client` crate so the rest of `boi-cluster` (and
//! `boi-node`) never sees `Box<dyn Error>` or raw `etcd_client::Error`
//! at API boundaries. Lease keep-alive is owned by [`LeaseHandle`]; the
//! background task is cancelled on `revoke_lease` (or on handle drop).

use std::sync::Arc;
use std::time::Duration;

use etcd_client::{
    Client, Compare, DeleteOptions, GetOptions, PutOptions, Txn, TxnOp as EtcdTxnOp,
    TxnResponse, WatchOptions, Watcher, WatchStream,
};
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{debug, warn};

/// Typed error surface for `boi-cluster`.
#[derive(Debug, Error)]
pub enum ClusterError {
    #[error("etcd connect failed after {attempts} attempts: {source}")]
    ConnectExhausted {
        attempts: u32,
        #[source]
        source: etcd_client::Error,
    },

    #[error("etcd RPC error: {0}")]
    Rpc(#[from] etcd_client::Error),

    #[error("lease {lease_id} keep-alive task ended: {reason}")]
    KeepAliveExited { lease_id: i64, reason: String },

    #[error("invalid argument: {0}")]
    Invalid(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("membership snapshot is stale and resync failed")]
    StaleSnapshot,
}

pub type Result<T> = std::result::Result<T, ClusterError>;

/// Handle returned by [`EtcdClient::grant_lease`]. Drop = best-effort
/// cancel of the keep-alive background task. Use
/// [`EtcdClient::revoke_lease`] for an explicit revoke at the server.
pub struct LeaseHandle {
    pub lease_id: i64,
    pub ttl_secs: i64,
    keep_alive: Option<JoinHandle<()>>,
}

impl LeaseHandle {
    /// Returns whether the keep-alive background task is still alive.
    pub fn is_alive(&self) -> bool {
        self.keep_alive
            .as_ref()
            .map(|h| !h.is_finished())
            .unwrap_or(false)
    }
}

impl Drop for LeaseHandle {
    fn drop(&mut self) {
        if let Some(h) = self.keep_alive.take() {
            h.abort();
        }
    }
}

/// Convenience builder for the `Txn` operations that `boi-cluster`
/// modules use most. Re-exported here to avoid leaking `etcd-client`
/// types into every call site.
pub enum TxnOp {
    Put {
        key: Vec<u8>,
        value: Vec<u8>,
        lease: Option<i64>,
    },
    Get(Vec<u8>),
    Delete(Vec<u8>),
}

impl TxnOp {
    fn into_etcd(self) -> EtcdTxnOp {
        match self {
            TxnOp::Put { key, value, lease } => {
                let opts = lease.map(|id| PutOptions::new().with_lease(id));
                EtcdTxnOp::put(key, value, opts)
            }
            TxnOp::Get(key) => EtcdTxnOp::get(key, None),
            TxnOp::Delete(key) => EtcdTxnOp::delete(key, None),
        }
    }
}

/// Connect-with-retry config. Kept tiny on purpose; callers tune via
/// [`EtcdClient::connect_with`].
#[derive(Debug, Clone)]
pub struct ConnectConfig {
    pub attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for ConnectConfig {
    fn default() -> Self {
        Self {
            attempts: 6,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(2),
        }
    }
}

/// Thin wrapper around `etcd_client::Client`. Cloneable: the inner
/// `Client` is shared via `Arc<Mutex<_>>` because the underlying gRPC
/// channel is shared by reference but the typed RPC methods take
/// `&mut self`.
#[derive(Clone)]
pub struct EtcdClient {
    inner: Arc<Mutex<Client>>,
}

impl EtcdClient {
    /// Connect with default retry policy.
    pub async fn connect<E, S>(endpoints: E) -> Result<Self>
    where
        E: AsRef<[S]>,
        S: AsRef<str>,
    {
        Self::connect_with(endpoints, &ConnectConfig::default()).await
    }

    /// Connect with caller-supplied retry policy.
    pub async fn connect_with<E, S>(endpoints: E, cfg: &ConnectConfig) -> Result<Self>
    where
        E: AsRef<[S]>,
        S: AsRef<str>,
    {
        if cfg.attempts == 0 {
            return Err(ClusterError::Invalid("attempts must be >= 1".into()));
        }
        let endpoints: Vec<String> = endpoints
            .as_ref()
            .iter()
            .map(|s| s.as_ref().to_string())
            .collect();
        if endpoints.is_empty() {
            return Err(ClusterError::Invalid("no etcd endpoints provided".into()));
        }

        let mut backoff = cfg.initial_backoff;
        let mut last_err: Option<etcd_client::Error> = None;
        for attempt in 1..=cfg.attempts {
            match Client::connect(&endpoints, None).await {
                Ok(c) => {
                    debug!(attempt, "etcd connect ok");
                    return Ok(Self {
                        inner: Arc::new(Mutex::new(c)),
                    });
                }
                Err(e) => {
                    warn!(attempt, error = %e, "etcd connect failed; retrying");
                    last_err = Some(e);
                    if attempt < cfg.attempts {
                        sleep(backoff).await;
                        backoff = (backoff * 2).min(cfg.max_backoff);
                    }
                }
            }
        }
        Err(ClusterError::ConnectExhausted {
            attempts: cfg.attempts,
            source: last_err.expect("loop populates last_err on failure"),
        })
    }

    /// Grant a lease with the given TTL (seconds) and start a
    /// background keep-alive task. The keep-alive cadence is `ttl/3`,
    /// clamped to `[1s, 30s]`, matching common etcd guidance.
    pub async fn grant_lease(&self, ttl_secs: i64) -> Result<LeaseHandle> {
        if ttl_secs < 1 {
            return Err(ClusterError::Invalid("ttl_secs must be >= 1".into()));
        }
        let lease_id = {
            let mut c = self.inner.lock().await;
            c.lease_grant(ttl_secs, None).await?.id()
        };

        let cadence = Duration::from_secs(
            (ttl_secs / 3).clamp(1, 30) as u64,
        );
        let client = self.inner.clone();
        let task = tokio::spawn(async move {
            // Open a single keep-alive stream; re-establish on error so
            // a transient network blip does not nuke the lease.
            loop {
                let res = {
                    let mut c = client.lock().await;
                    c.lease_keep_alive(lease_id).await
                };
                let (mut keeper, mut stream) = match res {
                    Ok(pair) => pair,
                    Err(e) => {
                        warn!(lease_id, error = %e, "lease_keep_alive open failed");
                        sleep(cadence).await;
                        continue;
                    }
                };
                loop {
                    if let Err(e) = keeper.keep_alive().await {
                        warn!(lease_id, error = %e, "keep_alive send failed");
                        break;
                    }
                    match stream.message().await {
                        Ok(Some(_resp)) => { /* normal refresh */ }
                        Ok(None) => {
                            warn!(lease_id, "keep_alive stream closed");
                            break;
                        }
                        Err(e) => {
                            warn!(lease_id, error = %e, "keep_alive recv failed");
                            break;
                        }
                    }
                    sleep(cadence).await;
                }
            }
        });

        Ok(LeaseHandle {
            lease_id,
            ttl_secs,
            keep_alive: Some(task),
        })
    }

    /// Revoke `handle` at the server and stop its keep-alive task.
    pub async fn revoke_lease(&self, mut handle: LeaseHandle) -> Result<()> {
        if let Some(h) = handle.keep_alive.take() {
            h.abort();
        }
        let mut c = self.inner.lock().await;
        c.lease_revoke(handle.lease_id).await?;
        Ok(())
    }

    /// Put a key/value, optionally attached to a lease.
    pub async fn put(
        &self,
        key: impl Into<Vec<u8>>,
        value: impl Into<Vec<u8>>,
        lease: Option<i64>,
    ) -> Result<()> {
        let opts = lease.map(|id| PutOptions::new().with_lease(id));
        let mut c = self.inner.lock().await;
        c.put(key, value, opts).await?;
        Ok(())
    }

    /// Read a single key. `None` if the key is absent.
    pub async fn get(&self, key: impl Into<Vec<u8>>) -> Result<Option<Vec<u8>>> {
        let mut c = self.inner.lock().await;
        let resp = c.get(key, None).await?;
        Ok(resp.kvs().first().map(|kv| kv.value().to_vec()))
    }

    /// Range-read by prefix. Returns `(key, value)` pairs.
    pub async fn get_prefix(&self, prefix: impl Into<Vec<u8>>) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let opts = GetOptions::new().with_prefix();
        let mut c = self.inner.lock().await;
        let resp = c.get(prefix, Some(opts)).await?;
        Ok(resp
            .kvs()
            .iter()
            .map(|kv| (kv.key().to_vec(), kv.value().to_vec()))
            .collect())
    }

    /// Range-read by prefix, returning the kvs plus the cluster
    /// header revision at which the read was served. Used by
    /// `membership` to pin a snapshot's `mod_revision` (per Q1).
    pub async fn get_prefix_with_revision(
        &self,
        prefix: impl Into<Vec<u8>>,
    ) -> Result<(Vec<(Vec<u8>, Vec<u8>)>, i64)> {
        let opts = GetOptions::new().with_prefix();
        let mut c = self.inner.lock().await;
        let resp = c.get(prefix, Some(opts)).await?;
        let rev = resp.header().map(|h| h.revision()).unwrap_or(0);
        let kvs = resp
            .kvs()
            .iter()
            .map(|kv| (kv.key().to_vec(), kv.value().to_vec()))
            .collect();
        Ok((kvs, rev))
    }

    /// Open a watch on every key under `prefix`, starting from
    /// `start_revision` (inclusive). The caller owns the returned
    /// `(Watcher, WatchStream)` and is responsible for draining the
    /// stream. Used by `membership`.
    pub async fn watch_prefix(
        &self,
        prefix: impl Into<Vec<u8>>,
        start_revision: i64,
    ) -> Result<(Watcher, WatchStream)> {
        let opts = WatchOptions::new()
            .with_prefix()
            .with_start_revision(start_revision);
        let mut c = self.inner.lock().await;
        Ok(c.watch(prefix, Some(opts)).await?)
    }

    /// Delete a single key. Returns `true` if a key was removed.
    pub async fn delete(&self, key: impl Into<Vec<u8>>) -> Result<bool> {
        let mut c = self.inner.lock().await;
        let resp = c.delete(key, Some(DeleteOptions::new())).await?;
        Ok(resp.deleted() > 0)
    }

    /// Run an etcd Txn with caller-built compares + branches.
    pub async fn txn(
        &self,
        compares: Vec<Compare>,
        success: Vec<TxnOp>,
        failure: Vec<TxnOp>,
    ) -> Result<TxnResponse> {
        let txn = Txn::new()
            .when(compares)
            .and_then(success.into_iter().map(TxnOp::into_etcd).collect::<Vec<_>>())
            .or_else(failure.into_iter().map(TxnOp::into_etcd).collect::<Vec<_>>());
        let mut c = self.inner.lock().await;
        Ok(c.txn(txn).await?)
    }
}

// =====================================================================
// Tests
// =====================================================================
//
// Unit tests cover the pure-Rust surface (error display, validation,
// lease-handle drop semantics). Integration tests spin a real
// `bitnami/etcd:3.5` container via `testcontainers` and exercise
// connect/lease/put/get/delete/txn end-to-end. When Docker is not
// available the integration tests log a skip and return Ok, so
// `cargo test -p boi-cluster` is green on dev machines without
// engagement of a container runtime.

#[cfg(test)]
mod tests {
    use super::*;
    use etcd_client::Compare;

    // ---- Pure unit tests -------------------------------------------------

    #[test]
    fn cluster_error_display_includes_attempts() {
        // ConnectExhausted Display must surface the attempt count so
        // operators can tell "couldn't reach etcd at all" from "RPC
        // failed mid-flight".
        let inner = etcd_client::Error::InvalidArgs("boom".into());
        let e = ClusterError::ConnectExhausted {
            attempts: 7,
            source: inner,
        };
        let s = format!("{e}");
        assert!(s.contains("7"), "expected attempts in display, got: {s}");
        assert!(s.contains("connect failed"));
    }

    #[test]
    fn invalid_endpoints_rejected_before_dial() {
        let cfg = ConnectConfig {
            attempts: 1,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(1),
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let res = rt.block_on(EtcdClient::connect_with::<[&str; 0], &str>([], &cfg));
        match res {
            Err(ClusterError::Invalid(_)) => {}
            other => panic!("expected Invalid, got {:?}", other.err()),
        }

        let res = rt.block_on(EtcdClient::connect_with(
            ["http://1.2.3.4:1"],
            &ConnectConfig {
                attempts: 0,
                ..cfg
            },
        ));
        match res {
            Err(ClusterError::Invalid(_)) => {}
            other => panic!("expected Invalid, got {:?}", other.err()),
        }
    }

    #[test]
    fn txn_op_into_etcd_smoke() {
        // Compile-time check that every variant lowers; if a future
        // edit removes `EtcdTxnOp::put`/`get`/`delete`, this fails to
        // build instead of at first runtime use.
        let _ops: Vec<EtcdTxnOp> = vec![
            TxnOp::Put {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
                lease: Some(1),
            },
            TxnOp::Get(b"k".to_vec()),
            TxnOp::Delete(b"k".to_vec()),
        ]
        .into_iter()
        .map(TxnOp::into_etcd)
        .collect();
    }

    // ---- Live-etcd integration tests ------------------------------------

    use testcontainers::{
        core::{IntoContainerPort, WaitFor},
        runners::AsyncRunner,
        GenericImage, ImageExt,
    };

    /// Detect whether a usable docker daemon is reachable. Used to
    /// skip live-etcd tests cleanly on machines without docker, so
    /// `cargo test -p boi-cluster` is green for everyone.
    fn docker_available() -> bool {
        std::process::Command::new("docker")
            .arg("info")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Bring up a single bitnami/etcd:3.5 container and return its
    /// `http://host:port` endpoint. Returns `None` if Docker isn't
    /// available (caller should `return Ok(())` in that case).
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_connect_put_get_delete_against_real_etcd() {
        let Some((_c, ep)) = etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");
        client.put("/boi/test/k1", "v1", None).await.expect("put");
        let got = client.get("/boi/test/k1").await.expect("get");
        assert_eq!(got.as_deref(), Some(b"v1".as_ref()));
        let removed = client.delete("/boi/test/k1").await.expect("delete");
        assert!(removed);
        let got = client.get("/boi/test/k1").await.expect("get-after-delete");
        assert!(got.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_lease_keepalive_holds_key_past_ttl() {
        let Some((_c, ep)) = etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");
        let lease = client.grant_lease(2).await.expect("lease");
        assert!(lease.is_alive());
        client
            .put("/boi/test/lease-key", "alive", Some(lease.lease_id))
            .await
            .expect("put-with-lease");

        // 2× ttl: if keep-alive is wired the key survives.
        tokio::time::sleep(Duration::from_secs(4)).await;
        let got = client.get("/boi/test/lease-key").await.expect("get");
        assert_eq!(got.as_deref(), Some(b"alive".as_ref()));

        client.revoke_lease(lease).await.expect("revoke");
        // After revoke the lease-bound key is gone.
        // etcd may take a tick to propagate the delete.
        let mut found_gone = false;
        for _ in 0..20 {
            if client
                .get("/boi/test/lease-key")
                .await
                .expect("get-after-revoke")
                .is_none()
            {
                found_gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(found_gone, "expected lease-bound key to be removed after revoke");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_txn_cas_round_trip() {
        let Some((_c, ep)) = etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");

        // CAS pattern that boi-cluster::dispatch_queue will lean on:
        // succeed iff key is absent (version == 0), then put.
        let key = b"/boi/test/cas".to_vec();
        let cmp = vec![Compare::version(key.clone(), etcd_client::CompareOp::Equal, 0)];
        let resp = client
            .txn(
                cmp,
                vec![TxnOp::Put {
                    key: key.clone(),
                    value: b"first".to_vec(),
                    lease: None,
                }],
                vec![],
            )
            .await
            .expect("txn-1");
        assert!(resp.succeeded(), "first CAS should succeed on a fresh key");

        // Second CAS with same precondition must fail (key now exists).
        let cmp2 = vec![Compare::version(key.clone(), etcd_client::CompareOp::Equal, 0)];
        let resp2 = client
            .txn(
                cmp2,
                vec![TxnOp::Put {
                    key: key.clone(),
                    value: b"second".to_vec(),
                    lease: None,
                }],
                vec![],
            )
            .await
            .expect("txn-2");
        assert!(!resp2.succeeded(), "second CAS must fail (version mismatch)");

        // Value must still be "first".
        let got = client.get(key).await.expect("get").expect("present");
        assert_eq!(&got, b"first");
    }
}
