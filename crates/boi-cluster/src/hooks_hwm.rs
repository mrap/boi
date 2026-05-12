//! `/boi/hooks-hwm/{node_id}/{plugin_id}` — audit-hook high-water mark.
//!
//! Per design §4 + Q6 (`q6-hooks-delivery.md`). The bulk audit queue
//! lives on local disk on each emitting node; only the high-water mark
//! (last acked seq + ts) replicates through etcd so gap-detection is
//! cheap cluster-wide.
//!
//! Path note: this spec calls for `/boi/hooks-hwm/{node}/{plugin}`
//! (the Phase 1 task's own ordering). The design doc shows it the
//! other way around (plugin first, then node); we follow the spec
//! because that is what callers in this phase rely on.

use serde::{Deserialize, Serialize};

use crate::client::{ClusterError, EtcdClient, Result};

pub const HOOKS_HWM_PREFIX: &str = "/boi/hooks-hwm/";

pub fn hwm_key(node_id: &str, plugin_id: &str) -> String {
    format!("{HOOKS_HWM_PREFIX}{node_id}/{plugin_id}")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HooksHwm {
    pub last_acked_seq: u64,
    pub last_ack_ts: i64, // unix seconds
}

impl HooksHwm {
    /// Persist the HWM scalar at `/boi/hooks-hwm/{node}/{plugin}`.
    /// HWMs are monotonic by contract; the caller is responsible for
    /// only advancing forward. This method intentionally exposes a
    /// last-writer-wins write — gap detection runs against the value,
    /// not against compare-and-set predicates.
    pub async fn put(&self, client: &EtcdClient, node_id: &str, plugin_id: &str) -> Result<()> {
        let body = serde_json::to_vec(self)
            .map_err(|e| ClusterError::Invalid(format!("encode HooksHwm: {e}")))?;
        client.put(hwm_key(node_id, plugin_id), body, None).await
    }

    pub async fn get(
        client: &EtcdClient,
        node_id: &str,
        plugin_id: &str,
    ) -> Result<Option<Self>> {
        let raw = match client.get(hwm_key(node_id, plugin_id)).await? {
            Some(b) => b,
            None => return Ok(None),
        };
        serde_json::from_slice(&raw)
            .map(Some)
            .map_err(|e| ClusterError::Invalid(format!("decode HooksHwm: {e}")))
    }

    /// List every HWM in the cluster. Returns `(node_id, plugin_id, hwm)`.
    pub async fn list_all(client: &EtcdClient) -> Result<Vec<(String, String, Self)>> {
        let kvs = client.get_prefix(HOOKS_HWM_PREFIX).await?;
        let mut out = Vec::with_capacity(kvs.len());
        for (k, v) in kvs {
            let key_str = std::str::from_utf8(&k)
                .map_err(|e| ClusterError::Invalid(format!("hwm key utf8: {e}")))?;
            let rest = key_str
                .strip_prefix(HOOKS_HWM_PREFIX)
                .ok_or_else(|| ClusterError::Invalid(format!("unexpected hwm key: {key_str}")))?;
            let (node_id, plugin_id) = rest.split_once('/').ok_or_else(|| {
                ClusterError::Invalid(format!("malformed hwm key: {key_str}"))
            })?;
            let hwm: HooksHwm = serde_json::from_slice(&v)
                .map_err(|e| ClusterError::Invalid(format!("decode HooksHwm: {e}")))?;
            out.push((node_id.to_string(), plugin_id.to_string(), hwm));
        }
        Ok(out)
    }

    pub async fn delete(client: &EtcdClient, node_id: &str, plugin_id: &str) -> Result<bool> {
        client.delete(hwm_key(node_id, plugin_id)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_helper_uses_expected_prefix() {
        assert_eq!(hwm_key("n1", "audit"), "/boi/hooks-hwm/n1/audit");
    }

    #[test]
    fn hwm_round_trips_through_json() {
        let h = HooksHwm {
            last_acked_seq: 42,
            last_ack_ts: 1_700_000_000,
        };
        let bytes = serde_json::to_vec(&h).unwrap();
        let back: HooksHwm = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(h, back);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hwm_crud_and_list_against_real_etcd() {
        let Some((_c, ep)) = crate::testutil::etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");

        let h1 = HooksHwm {
            last_acked_seq: 10,
            last_ack_ts: 1_700_000_000,
        };
        h1.put(&client, "nA", "audit").await.expect("put-1");
        let got = HooksHwm::get(&client, "nA", "audit")
            .await
            .expect("get")
            .expect("present");
        assert_eq!(got, h1);

        // Advancing the HWM overwrites (last-writer-wins by design).
        let h2 = HooksHwm {
            last_acked_seq: 25,
            last_ack_ts: 1_700_000_100,
        };
        h2.put(&client, "nA", "audit").await.expect("put-2");
        let got = HooksHwm::get(&client, "nA", "audit").await.unwrap().unwrap();
        assert_eq!(got.last_acked_seq, 25);

        // Another node/plugin pair sits alongside.
        let h3 = HooksHwm {
            last_acked_seq: 7,
            last_ack_ts: 1_700_000_050,
        };
        h3.put(&client, "nB", "telemetry").await.expect("put-3");

        let mut all = HooksHwm::list_all(&client).await.expect("list");
        all.sort_by(|a, b| (a.0.clone(), a.1.clone()).cmp(&(b.0.clone(), b.1.clone())));
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0, "nA");
        assert_eq!(all[0].1, "audit");
        assert_eq!(all[0].2.last_acked_seq, 25);
        assert_eq!(all[1].0, "nB");
        assert_eq!(all[1].1, "telemetry");

        assert!(HooksHwm::delete(&client, "nA", "audit").await.unwrap());
        assert!(HooksHwm::get(&client, "nA", "audit")
            .await
            .unwrap()
            .is_none());
    }
}
