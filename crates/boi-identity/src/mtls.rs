//! mTLS configuration helpers for tonic.
//!
//! Both server and client present a node cert (signed by the cluster CA)
//! and verify the peer's cert against the same cluster CA root. This
//! implements LD-7: only nodes whose certs chain to the cluster CA can
//! join the gRPC mesh.

use tonic::transport::{Certificate, ClientTlsConfig, Identity, ServerTlsConfig};

/// Build a tonic `ServerTlsConfig` that:
///   * presents `(cert_pem, key_pem)` as the server identity, and
///   * requires + verifies a client cert chaining to `ca_pem`.
pub fn build_server_tls(
    ca_pem: &str,
    cert_pem: &str,
    key_pem: &str,
) -> ServerTlsConfig {
    let identity = Identity::from_pem(cert_pem.as_bytes(), key_pem.as_bytes());
    let ca = Certificate::from_pem(ca_pem.as_bytes());
    ServerTlsConfig::new()
        .identity(identity)
        .client_ca_root(ca)
}

/// Build a tonic `ClientTlsConfig` that:
///   * presents `(cert_pem, key_pem)` as the client identity, and
///   * verifies the server cert against `ca_pem`.
///
/// The domain name defaults to `"localhost"` (matches the SAN that
/// `ClusterCa::mint_node_cert` writes). Callers that want a different
/// SNI value should call [`ClientTlsConfig::domain_name`] on the
/// returned config.
pub fn build_client_tls(
    ca_pem: &str,
    cert_pem: &str,
    key_pem: &str,
) -> ClientTlsConfig {
    let identity = Identity::from_pem(cert_pem.as_bytes(), key_pem.as_bytes());
    let ca = Certificate::from_pem(ca_pem.as_bytes());
    ClientTlsConfig::new()
        .ca_certificate(ca)
        .identity(identity)
        .domain_name("localhost")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ca::ClusterCa;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tonic::transport::{Channel, Endpoint, Server};
    use tonic_health::pb::health_client::HealthClient;
    use tonic_health::pb::HealthCheckRequest;

    async fn spawn_server(
        ca_pem: String,
        cert_pem: String,
        key_pem: String,
    ) -> (std::net::SocketAddr, oneshot::Sender<()>) {
        // Bind on an OS-chosen port so parallel tests don't collide.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

        let (_reporter, health_svc) = tonic_health::server::health_reporter();
        let tls = build_server_tls(&ca_pem, &cert_pem, &key_pem);

        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = Server::builder()
                .tls_config(tls)
                .expect("server tls config")
                .add_service(health_svc)
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = rx.await;
                })
                .await;
        });
        // Give the server a beat to start listening before clients dial.
        tokio::time::sleep(Duration::from_millis(50)).await;
        (addr, tx)
    }

    async fn connect_with(
        addr: std::net::SocketAddr,
        tls: ClientTlsConfig,
    ) -> Result<Channel, tonic::transport::Error> {
        let uri = format!("https://localhost:{}", addr.port());
        Endpoint::from_shared(uri)?
            .tls_config(tls)?
            .connect_timeout(Duration::from_secs(3))
            .connect()
            .await
    }

    #[tokio::test]
    async fn mtls_accepts_peer_signed_by_same_ca() {
        let ca = ClusterCa::generate_ca().unwrap();
        let server_bundle = ca.mint_node_cert("server-node").unwrap();
        let client_bundle = ca.mint_node_cert("client-node").unwrap();

        let (addr, shutdown) = spawn_server(
            ca.cert_pem().to_string(),
            server_bundle.cert_pem.clone(),
            server_bundle.key_pem.clone(),
        )
        .await;

        let client_tls = build_client_tls(
            ca.cert_pem(),
            &client_bundle.cert_pem,
            &client_bundle.key_pem,
        );
        let channel = connect_with(addr, client_tls)
            .await
            .expect("client should connect to server signed by same CA");

        let mut client = HealthClient::new(channel);
        let resp = client
            .check(HealthCheckRequest {
                service: String::new(),
            })
            .await
            .expect("health check should succeed over mTLS");
        // SERVING == 1; status >=0 is enough proof the RPC round-tripped.
        assert!(resp.into_inner().status >= 0);

        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn mtls_rejects_peer_signed_by_different_ca() {
        let ca_real = ClusterCa::generate_ca().unwrap();
        let ca_rogue = ClusterCa::generate_ca().unwrap();

        let server_bundle = ca_real.mint_node_cert("server-node").unwrap();
        // Client cert signed by rogue CA — server should reject.
        let rogue_client = ca_rogue.mint_node_cert("rogue-client").unwrap();

        let (addr, shutdown) = spawn_server(
            ca_real.cert_pem().to_string(),
            server_bundle.cert_pem.clone(),
            server_bundle.key_pem.clone(),
        )
        .await;

        // The client trusts the real CA for the server cert, but
        // presents a rogue-signed identity → server-side verification
        // fails. Tonic dials lazily, so the rejection may surface at
        // RPC time rather than connect() time. Either layer failing
        // is a pass.
        let bad_tls = build_client_tls(
            ca_real.cert_pem(),
            &rogue_client.cert_pem,
            &rogue_client.key_pem,
        );
        let rpc_failed = match connect_with(addr, bad_tls).await {
            Err(_) => true,
            Ok(channel) => {
                let mut client = HealthClient::new(channel);
                client
                    .check(HealthCheckRequest {
                        service: String::new(),
                    })
                    .await
                    .is_err()
            }
        };
        assert!(
            rpc_failed,
            "RPC must fail when client cert is signed by a different CA"
        );

        // Independently: a client that doesn't trust the server's CA at
        // all (rogue CA in the client root store) must also fail.
        let server_distrusted = build_client_tls(
            ca_rogue.cert_pem(),
            &rogue_client.cert_pem,
            &rogue_client.key_pem,
        );
        let rpc_failed2 = match connect_with(addr, server_distrusted).await {
            Err(_) => true,
            Ok(channel) => {
                let mut client = HealthClient::new(channel);
                client
                    .check(HealthCheckRequest {
                        service: String::new(),
                    })
                    .await
                    .is_err()
            }
        };
        assert!(
            rpc_failed2,
            "RPC must fail when client does not trust server CA"
        );

        let _ = shutdown.send(());
    }
}
