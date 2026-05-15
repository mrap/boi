//! `/boi/claims/{task_id}` + the `claim_lease_id` fencing sub-key.
//!
//! Per design §4 and Q2 (`q2-fencing-token.md`):
//!
//! - `/boi/claims/{task_id}` holds the claim envelope (`node_id`,
//!   `claimed_at`, `lease_id`, `attempt`), bound to the claim lease so
//!   it is auto-revoked on node failure.
//! - `/boi/claims/{task_id}/claim_lease_id` carries ONLY the i64 lease
//!   id (as decimal ASCII) so result-write Txns can predicate on a
//!   single field via `Compare(Value(...), "=", "<expected>")` without
//!   round-tripping the full envelope. (Q2 §5, "dedicated sub-key".)
//!
//! Claim acquisition is CAS: succeed iff `/boi/claims/{task_id}` is
//! absent (`Compare(Version(key) == 0)`). Release is unconditional
//! delete (the lease revocation is the durable kill-switch).

use serde::{Deserialize, Serialize};

use crate::client::{ClusterError, EtcdClient, Result, TxnOp};

pub const CLAIMS_PREFIX: &str = "/boi/claims/";

pub fn claim_key(task_id: &str) -> String {
    format!("{CLAIMS_PREFIX}{task_id}")
}

pub fn claim_lease_key(task_id: &str) -> String {
    format!("{CLAIMS_PREFIX}{task_id}/claim_lease_id")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimRecord {
    pub task_id: String,
    pub node_id: String,
    pub lease_id: i64,
    pub claimed_at: i64, // unix seconds
    pub attempt: u32,
}

impl ClaimRecord {
    fn encode(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self)
            .map_err(|e| ClusterError::Invalid(format!("encode ClaimRecord: {e}")))
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes)
            .map_err(|e| ClusterError::Invalid(format!("decode ClaimRecord: {e}")))
    }

    /// Attempt to acquire the claim for `task_id`. Both the envelope
    /// key and the fencing sub-key are written atomically inside a
    /// single Txn gated on `Version(envelope_key) == 0` so a
    /// half-written claim cannot exist.
    pub async fn acquire(&self, client: &EtcdClient) -> Result<()> {
        let envelope_key = claim_key(&self.task_id).into_bytes();
        let lease_key = claim_lease_key(&self.task_id).into_bytes();
        let body = self.encode()?;
        let lease_value = self.lease_id.to_string().into_bytes();

        let resp = client
            .txn(
                vec![etcd_client::Compare::version(
                    envelope_key.clone(),
                    etcd_client::CompareOp::Equal,
                    0,
                )],
                vec![
                    TxnOp::Put {
                        key: envelope_key,
                        value: body,
                        lease: Some(self.lease_id),
                    },
                    TxnOp::Put {
                        key: lease_key,
                        value: lease_value,
                        lease: Some(self.lease_id),
                    },
                ],
                vec![],
            )
            .await?;
        if !resp.succeeded() {
            return Err(ClusterError::Conflict(format!(
                "claims/{} already held",
                self.task_id
            )));
        }
        Ok(())
    }

    pub async fn get(client: &EtcdClient, task_id: &str) -> Result<Option<Self>> {
        let raw = match client.get(claim_key(task_id)).await? {
            Some(b) => b,
            None => return Ok(None),
        };
        Self::decode(&raw).map(Some)
    }

    /// Read the bare fencing lease id from the sub-key. `None` if not
    /// claimed. The sub-key is the hot path for result-write Txns.
    pub async fn current_lease_id(client: &EtcdClient, task_id: &str) -> Result<Option<i64>> {
        let raw = match client.get(claim_lease_key(task_id)).await? {
            Some(b) => b,
            None => return Ok(None),
        };
        let s = std::str::from_utf8(&raw)
            .map_err(|e| ClusterError::Invalid(format!("claim_lease_id utf8: {e}")))?;
        s.parse::<i64>()
            .map(Some)
            .map_err(|e| ClusterError::Invalid(format!("claim_lease_id parse: {e}")))
    }

    /// Release the claim unconditionally (caller already holds it; the
    /// lease guarantees the keys disappear on caller crash either way).
    pub async fn release(client: &EtcdClient, task_id: &str) -> Result<()> {
        // Sub-key first so a partial revoke still leaves the envelope
        // as the visible "claimed but stale" signal for monitors.
        client.delete(claim_lease_key(task_id)).await?;
        client.delete(claim_key(task_id)).await?;
        Ok(())
    }

    /// Build the etcd `Compare` that result-write callers must include
    /// in their Txn to fence stale-claim writes (Q2 §5). The sub-key is
    /// compared by value as decimal ASCII of the i64 lease id.
    pub fn fence_compare(task_id: &str, expected_lease_id: i64) -> etcd_client::Compare {
        etcd_client::Compare::value(
            claim_lease_key(task_id).into_bytes(),
            etcd_client::CompareOp::Equal,
            expected_lease_id.to_string().into_bytes(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_helpers_use_expected_prefixes() {
        assert_eq!(claim_key("t1"), "/boi/claims/t1");
        assert_eq!(claim_lease_key("t1"), "/boi/claims/t1/claim_lease_id");
    }

    #[test]
    fn claim_record_round_trips() {
        let r = ClaimRecord {
            task_id: "t1".into(),
            node_id: "n1".into(),
            lease_id: 42,
            claimed_at: 1_700_000_000,
            attempt: 1,
        };
        let bytes = r.encode().unwrap();
        let back = ClaimRecord::decode(&bytes).unwrap();
        assert_eq!(r, back);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn first_acquire_wins_second_conflicts() {
        let Some((_c, ep)) = crate::testutil::etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");

        let lease = client.grant_lease(10).await.expect("lease");
        let rec = ClaimRecord {
            task_id: "t1".into(),
            node_id: "n1".into(),
            lease_id: lease.lease_id,
            claimed_at: 1_700_000_000,
            attempt: 1,
        };
        rec.acquire(&client).await.expect("first acquire");

        // Second acquire from another node sees Conflict.
        let lease2 = client.grant_lease(10).await.expect("lease2");
        let rec2 = ClaimRecord {
            task_id: "t1".into(),
            node_id: "n2".into(),
            lease_id: lease2.lease_id,
            claimed_at: 1_700_000_001,
            attempt: 1,
        };
        let err = rec2.acquire(&client).await;
        assert!(matches!(err, Err(ClusterError::Conflict(_))));

        // Sub-key carries the i64 lease id as decimal ASCII.
        let li = ClaimRecord::current_lease_id(&client, "t1")
            .await
            .expect("get sub-key")
            .expect("present");
        assert_eq!(li, lease.lease_id);

        ClaimRecord::release(&client, "t1").await.expect("release");
        assert!(ClaimRecord::get(&client, "t1").await.unwrap().is_none());
        assert!(ClaimRecord::current_lease_id(&client, "t1")
            .await
            .unwrap()
            .is_none());

        client.revoke_lease(lease).await.ok();
        client.revoke_lease(lease2).await.ok();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fence_compare_gates_result_write() {
        let Some((_c, ep)) = crate::testutil::etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");

        let lease = client.grant_lease(10).await.expect("lease");
        let rec = ClaimRecord {
            task_id: "t-fence".into(),
            node_id: "n1".into(),
            lease_id: lease.lease_id,
            claimed_at: 1_700_000_000,
            attempt: 1,
        };
        rec.acquire(&client).await.expect("acquire");

        // A result write fenced on the actual lease_id commits.
        let ok = client
            .txn(
                vec![ClaimRecord::fence_compare("t-fence", lease.lease_id)],
                vec![TxnOp::Put {
                    key: b"/boi/test/result-good".to_vec(),
                    value: b"ok".to_vec(),
                    lease: None,
                }],
                vec![],
            )
            .await
            .expect("txn-good");
        assert!(ok.succeeded());

        // A result write fenced on a wrong lease_id is rejected.
        let bad = client
            .txn(
                vec![ClaimRecord::fence_compare("t-fence", lease.lease_id + 999)],
                vec![TxnOp::Put {
                    key: b"/boi/test/result-bad".to_vec(),
                    value: b"nope".to_vec(),
                    lease: None,
                }],
                vec![],
            )
            .await
            .expect("txn-bad");
        assert!(!bad.succeeded());
        assert!(client
            .get("/boi/test/result-bad")
            .await
            .expect("get")
            .is_none());

        ClaimRecord::release(&client, "t-fence").await.ok();
        client.revoke_lease(lease).await.ok();
    }
}
