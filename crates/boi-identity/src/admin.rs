//! Cluster-admin capability gate.
//!
//! A node is "cluster admin" iff its `/boi/caps/{node_id}` record carries
//! `static.cluster_admin = "true"`. Only admins may mint join tokens
//! (design §16 Q3). `init_cluster` bootstraps a fresh cluster by
//! generating the CA on disk and registering the seed node as admin in
//! one shot.

use std::path::Path;

use boi_cluster::{
    nodes::{NodeCaps, NodeRecord},
    ClusterError, EtcdClient,
};
use thiserror::Error;

use crate::ca::{CaError, ClusterCa};
use crate::join_token::{mint_join_token, TokenError};

#[derive(Debug, Error)]
pub enum AdminError {
    #[error("cluster error: {0}")]
    Cluster(#[from] ClusterError),
    #[error("ca error: {0}")]
    Ca(#[from] CaError),
    #[error("token error: {0}")]
    Token(#[from] TokenError),
    #[error("permission denied: node `{0}` is not cluster_admin")]
    PermissionDenied(String),
}

/// True iff `/boi/caps/{node_id}` has `static.cluster_admin == "true"`.
/// Missing record or missing key → false.
pub async fn is_cluster_admin(
    client: &EtcdClient,
    node_id: &str,
) -> Result<bool, AdminError> {
    let caps = match NodeCaps::get(client, node_id).await? {
        Some(c) => c,
        None => return Ok(false),
    };
    Ok(caps
        .r#static
        .get("cluster_admin")
        .map(|v| v == "true")
        .unwrap_or(false))
}

/// Gated wrapper around [`mint_join_token`]: rejects with
/// `PermissionDenied` if `caller_node_id` is not `cluster_admin`.
pub async fn mint_join_token_gated(
    client: &EtcdClient,
    caller_node_id: &str,
    ca_key_pem: &str,
    ca_cert_der: &[u8],
    cluster_id: &str,
    seed_addrs: Vec<String>,
    ttl_secs: i64,
) -> Result<String, AdminError> {
    if !is_cluster_admin(client, caller_node_id).await? {
        return Err(AdminError::PermissionDenied(caller_node_id.to_string()));
    }
    Ok(mint_join_token(
        ca_key_pem,
        ca_cert_der,
        cluster_id,
        seed_addrs,
        ttl_secs,
    )?)
}

/// `boi cluster init` library function.
///
/// Generates (or loads) the cluster CA at `ca_dir`, then writes the seed
/// node's `NodeRecord` + `NodeCaps` (with `cluster_admin=true`) to etcd.
/// Returns the loaded CA so the caller can mint the first join token.
pub async fn init_cluster(
    client: &EtcdClient,
    ca_dir: &Path,
    seed_node_id: &str,
    seed_addr: &str,
    version: &str,
) -> Result<ClusterCa, AdminError> {
    let ca = ClusterCa::load_or_generate(ca_dir)?;

    let rec = NodeRecord {
        node_id: seed_node_id.to_string(),
        addr: seed_addr.to_string(),
        version: version.to_string(),
        started_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    };
    rec.put(client, None).await?;

    let mut caps = NodeCaps::default();
    caps.r#static
        .insert("cluster_admin".to_string(), "true".to_string());
    caps.put(client, seed_node_id, None).await?;

    Ok(ca)
}

#[cfg(test)]
mod tests {
    use super::*;
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
            eprintln!("docker not available — skipping admin live-etcd test");
            return None;
        }
        let img = GenericImage::new("bitnami/etcd", "3.5")
            .with_exposed_port(2379.tcp())
            .with_wait_for(WaitFor::message_on_stderr(
                "ready to serve client requests",
            ))
            .with_env_var("ALLOW_NONE_AUTHENTICATION", "yes")
            .with_env_var("ETCD_ADVERTISE_CLIENT_URLS", "http://0.0.0.0:2379")
            .with_env_var("ETCD_LISTEN_CLIENT_URLS", "http://0.0.0.0:2379");
        let container = match img.start().await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("etcd container start failed; skipping: {e}");
                return None;
            }
        };
        let port = match container.get_host_port_ipv4(2379).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("mapped port read failed; skipping: {e}");
                return None;
            }
        };
        Some((container, format!("http://127.0.0.1:{port}")))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn admin_gate_init_mint_and_reject() {
        let Some((_c, ep)) = etcd_endpoint().await else {
            return;
        };
        let client = EtcdClient::connect([ep]).await.expect("connect");
        let dir = tempfile::tempdir().unwrap();

        // Bootstrap the cluster: CA on disk + seed admin in etcd.
        let ca = init_cluster(
            &client,
            dir.path(),
            "seed-1",
            "127.0.0.1:7001",
            "0.1.0",
        )
        .await
        .expect("init_cluster");

        // Sanity: admin flag is observable.
        assert!(is_cluster_admin(&client, "seed-1").await.unwrap());
        assert!(!is_cluster_admin(&client, "nobody").await.unwrap());

        // Admin can mint.
        let der = ca.cert_der().unwrap();
        let token = mint_join_token_gated(
            &client,
            "seed-1",
            ca.key_pem(),
            &der,
            "cluster-1",
            vec!["127.0.0.1:7001".into()],
            300,
        )
        .await
        .expect("admin mint must succeed");
        assert!(!token.is_empty());

        // Register a non-admin node, then watch mint get rejected.
        let mut caps = NodeCaps::default();
        caps.r#static
            .insert("cluster_admin".into(), "false".into());
        caps.put(&client, "worker-1", None).await.unwrap();

        let err = mint_join_token_gated(
            &client,
            "worker-1",
            ca.key_pem(),
            &der,
            "cluster-1",
            vec![],
            300,
        )
        .await;
        assert!(
            matches!(err, Err(AdminError::PermissionDenied(_))),
            "non-admin must be rejected, got {err:?}"
        );

        // Unknown node is also non-admin.
        let err2 = mint_join_token_gated(
            &client,
            "ghost",
            ca.key_pem(),
            &der,
            "cluster-1",
            vec![],
            300,
        )
        .await;
        assert!(matches!(err2, Err(AdminError::PermissionDenied(_))));
    }

    #[test]
    fn admin_error_permission_denied_renders() {
        let e = AdminError::PermissionDenied("n9".into());
        let s = format!("{e}");
        assert!(s.contains("permission denied"));
        assert!(s.contains("n9"));
    }
}
