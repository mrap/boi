//! `/boi/nodes/{id}` and `/boi/caps/{id}` schemas.
//!
//! Per design §4: each node owns exactly one `NodeRecord` (liveness +
//! identity) and one `NodeCaps` (capability advertisement). Both are
//! lease-bound by the owning node; CRUD here does not impose the lease
//! — callers attach the lease via the lower-level [`EtcdClient`] put.
//!
//! Capability key namespace (per §4 "Capability vocabulary"):
//! - *Reserved* (`os`, `arch`, `region`, `runtime`) — written by core only.
//! - *User-defined* — must be prefixed `x-<vendor>-<tag>`; opaque UTF-8 ≤256 B.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::client::{ClusterError, EtcdClient, Result};

pub const NODES_PREFIX: &str = "/boi/nodes/";
pub const CAPS_PREFIX: &str = "/boi/caps/";

/// Reserved static-cap keys (BOI core writes only).
pub const RESERVED_CAP_KEYS: &[&str] =
    &["os", "arch", "region", "runtime", "cluster_admin"];

/// User-defined cap key prefix.
pub const USER_CAP_PREFIX: &str = "x-";

/// Max length of a user-defined cap value (opaque UTF-8).
pub const MAX_CAP_VALUE_BYTES: usize = 256;

/// Liveness + identity record stored at `/boi/nodes/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRecord {
    pub node_id: String,
    pub addr: String,
    pub version: String,
    pub started_at: i64, // unix seconds
}

/// Capability advertisement stored at `/boi/caps/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NodeCaps {
    pub r#static: BTreeMap<String, String>,
    pub dynamic: BTreeMap<String, String>,
}

fn node_key(node_id: &str) -> String {
    format!("{NODES_PREFIX}{node_id}")
}

fn caps_key(node_id: &str) -> String {
    format!("{CAPS_PREFIX}{node_id}")
}

/// Validate a single static-cap key. Errors if the key is neither in
/// the reserved set nor prefixed `x-<vendor>-<tag>`, or if the value
/// exceeds `MAX_CAP_VALUE_BYTES`.
pub fn validate_static_cap(key: &str, value: &str) -> Result<()> {
    if value.len() > MAX_CAP_VALUE_BYTES {
        return Err(ClusterError::Invalid(format!(
            "cap value for `{key}` exceeds {MAX_CAP_VALUE_BYTES} bytes"
        )));
    }
    if RESERVED_CAP_KEYS.contains(&key) {
        return Ok(());
    }
    if let Some(rest) = key.strip_prefix(USER_CAP_PREFIX) {
        // Require at least `<vendor>-<tag>`: one '-' splitting two
        // non-empty segments. Cheap, catches the common "x-foo" mistake.
        let mut parts = rest.splitn(2, '-');
        let vendor = parts.next().unwrap_or("");
        let tag = parts.next().unwrap_or("");
        if vendor.is_empty() || tag.is_empty() {
            return Err(ClusterError::Invalid(format!(
                "user cap key `{key}` must be `x-<vendor>-<tag>`"
            )));
        }
        return Ok(());
    }
    Err(ClusterError::Invalid(format!(
        "cap key `{key}` is neither reserved nor `x-<vendor>-<tag>`"
    )))
}

/// Validate every key in a static-caps map.
pub fn validate_static_caps(caps: &BTreeMap<String, String>) -> Result<()> {
    for (k, v) in caps {
        validate_static_cap(k, v)?;
    }
    Ok(())
}

impl NodeRecord {
    /// Persist at `/boi/nodes/{id}` attached to `lease`.
    pub async fn put(&self, client: &EtcdClient, lease: Option<i64>) -> Result<()> {
        let body = serde_json::to_vec(self)
            .map_err(|e| ClusterError::Invalid(format!("encode NodeRecord: {e}")))?;
        client.put(node_key(&self.node_id), body, lease).await
    }

    pub async fn get(client: &EtcdClient, node_id: &str) -> Result<Option<Self>> {
        let raw = match client.get(node_key(node_id)).await? {
            Some(b) => b,
            None => return Ok(None),
        };
        serde_json::from_slice(&raw)
            .map(Some)
            .map_err(|e| ClusterError::Invalid(format!("decode NodeRecord: {e}")))
    }

    pub async fn delete(client: &EtcdClient, node_id: &str) -> Result<bool> {
        client.delete(node_key(node_id)).await
    }

    /// List every node currently registered. Order is etcd's key order.
    pub async fn list(client: &EtcdClient) -> Result<Vec<Self>> {
        let kvs = client.get_prefix(NODES_PREFIX).await?;
        let mut out = Vec::with_capacity(kvs.len());
        for (_, v) in kvs {
            let r: NodeRecord = serde_json::from_slice(&v)
                .map_err(|e| ClusterError::Invalid(format!("decode NodeRecord: {e}")))?;
            out.push(r);
        }
        Ok(out)
    }
}

impl NodeCaps {
    /// Persist at `/boi/caps/{id}` attached to `lease`. Validates the
    /// static-cap key namespace before writing.
    pub async fn put(
        &self,
        client: &EtcdClient,
        node_id: &str,
        lease: Option<i64>,
    ) -> Result<()> {
        validate_static_caps(&self.r#static)?;
        let body = serde_json::to_vec(self)
            .map_err(|e| ClusterError::Invalid(format!("encode NodeCaps: {e}")))?;
        client.put(caps_key(node_id), body, lease).await
    }

    pub async fn get(client: &EtcdClient, node_id: &str) -> Result<Option<Self>> {
        let raw = match client.get(caps_key(node_id)).await? {
            Some(b) => b,
            None => return Ok(None),
        };
        serde_json::from_slice(&raw)
            .map(Some)
            .map_err(|e| ClusterError::Invalid(format!("decode NodeCaps: {e}")))
    }

    pub async fn delete(client: &EtcdClient, node_id: &str) -> Result<bool> {
        client.delete(caps_key(node_id)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_keys_pass_validation() {
        for k in RESERVED_CAP_KEYS {
            validate_static_cap(k, "ok").expect("reserved key should pass");
        }
    }

    #[test]
    fn user_keys_require_vendor_and_tag() {
        validate_static_cap("x-acme-region", "v").expect("well-formed user key");
        assert!(matches!(
            validate_static_cap("x-acme", "v"),
            Err(ClusterError::Invalid(_))
        ));
        assert!(matches!(
            validate_static_cap("x-", "v"),
            Err(ClusterError::Invalid(_))
        ));
        assert!(matches!(
            validate_static_cap("x--tag", "v"),
            Err(ClusterError::Invalid(_))
        ));
    }

    #[test]
    fn unknown_unprefixed_key_rejected() {
        assert!(matches!(
            validate_static_cap("rogue", "v"),
            Err(ClusterError::Invalid(_))
        ));
    }

    #[test]
    fn oversize_cap_value_rejected() {
        let big = "x".repeat(MAX_CAP_VALUE_BYTES + 1);
        assert!(matches!(
            validate_static_cap("os", &big),
            Err(ClusterError::Invalid(_))
        ));
    }

    #[test]
    fn key_helpers_use_expected_prefixes() {
        assert_eq!(node_key("n1"), "/boi/nodes/n1");
        assert_eq!(caps_key("n1"), "/boi/caps/n1");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_record_crud_round_trip() {
        let Some((_c, ep)) = crate::testutil::etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");
        let rec = NodeRecord {
            node_id: "n1".into(),
            addr: "127.0.0.1:7001".into(),
            version: "0.1.0".into(),
            started_at: 1_700_000_000,
        };
        rec.put(&client, None).await.expect("put");

        let got = NodeRecord::get(&client, "n1")
            .await
            .expect("get")
            .expect("present");
        assert_eq!(got, rec);

        let listed = NodeRecord::list(&client).await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0], rec);

        assert!(NodeRecord::delete(&client, "n1").await.expect("delete"));
        assert!(NodeRecord::get(&client, "n1")
            .await
            .expect("get-after-delete")
            .is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_caps_validates_then_persists() {
        let Some((_c, ep)) = crate::testutil::etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");

        let mut caps = NodeCaps::default();
        caps.r#static.insert("os".into(), "linux".into());
        caps.r#static.insert("arch".into(), "arm64".into());
        caps.r#static.insert("x-acme-region".into(), "us-east".into());
        caps.dynamic.insert("workers_busy".into(), "0".into());
        caps.dynamic.insert("workers_max".into(), "4".into());
        caps.put(&client, "n1", None).await.expect("put-valid");

        let got = NodeCaps::get(&client, "n1")
            .await
            .expect("get")
            .expect("present");
        assert_eq!(got, caps);

        // Invalid key must be rejected before the write.
        let mut bad = NodeCaps::default();
        bad.r#static.insert("rogue".into(), "v".into());
        let err = bad.put(&client, "n2", None).await;
        assert!(matches!(err, Err(ClusterError::Invalid(_))));
        assert!(NodeCaps::get(&client, "n2")
            .await
            .expect("get-after-rejection")
            .is_none());
    }
}
