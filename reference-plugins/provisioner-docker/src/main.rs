//! Reference Docker provisioner plugin for BOI.
//!
//! Receives a ProvisionRequest, runs `docker run` to spawn a new
//! boi-node container with BOI_TOKEN env var, and returns the
//! container ID as machine_id.
//!
//! The container boots into `boi-node node join --token <BOI_TOKEN>`.
//!
//! Test harness hook: if `/boi/provisioner-mode` in etcd contains
//! `ack-without-spawn`, the plugin acknowledges the request without
//! spawning a container (used by the F-06 cooldown subtest).
//!
//! Observability: every inbound RPC is appended to
//! `/var/lib/boi-plugin/transcript.jsonl` so tests can grep for
//! specific RPCs without sleeping.

use std::io::Write as _;
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tonic::{transport::Server, Request, Response, Status};
use tracing::{info, warn};

use boi_proto::provisioner::v1 as pb;
use pb::provisioner_server::{Provisioner, ProvisionerServer};

const TRANSCRIPT: &str = "/var/lib/boi-plugin/transcript.jsonl";
const DEFAULT_LISTEN: &str = "0.0.0.0:7002";
// Docker Compose service image built from the boi-node Dockerfile.
const BOI_NODE_IMAGE: &str = "boi-test-harness_node-a";
// Fallback if the image name env var is not set.
const BOI_NODE_IMAGE_ENV: &str = "BOI_NODE_IMAGE";
// How many times we've provisioned — used to generate unique node IDs.
static PROVISION_COUNTER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

fn append_transcript(entry: serde_json::Value) {
    if let Some(parent) = std::path::Path::new(TRANSCRIPT).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(TRANSCRIPT)
    {
        let mut line = serde_json::to_string(&entry).unwrap_or_default();
        line.push('\n');
        let _ = f.write_all(line.as_bytes());
    }
}

fn provisioner_mode() -> String {
    // Check env var first (set by `boi-node internal set-provisioner-mode`
    // via etcd, but here we read it from the environment for simplicity).
    std::env::var("BOI_PROVISIONER_MODE").unwrap_or_default()
}

#[derive(Debug, Default)]
struct DockerProvisioner {
    // Shared mutable counter for in-flight requests (unused but kept for
    // future deprovisioning bookkeeping).
    _state: Arc<Mutex<()>>,
}

#[tonic::async_trait]
impl Provisioner for DockerProvisioner {
    async fn handshake(
        &self,
        req: Request<pb::HandshakeRequest>,
    ) -> Result<Response<pb::HandshakeResponse>, Status> {
        let minor = req.into_inner().host_proto_minor;
        info!(host_proto_minor = minor, "Handshake received");
        append_transcript(serde_json::json!({
            "rpc": "Handshake",
            "host_proto_minor": minor,
        }));
        Ok(Response::new(pb::HandshakeResponse {
            plugin_proto_minor: 0,
            capabilities: vec!["docker".to_string()],
        }))
    }

    async fn provision(
        &self,
        req: Request<pb::ProvisionRequest>,
    ) -> Result<Response<pb::ProvisionResponse>, Status> {
        let r = req.into_inner();
        let request_id = r.request_id.clone();
        let spec_id = r.spec_id.clone();
        let token = r
            .join_token
            .as_ref()
            .map(|t| t.token.clone())
            .unwrap_or_default();

        info!(request_id, spec_id, "ProvisionRequest received");
        append_transcript(serde_json::json!({
            "rpc": "ProvisionRequest",
            "request_id": request_id,
            "spec_id": spec_id,
        }));

        let mode = provisioner_mode();
        if mode == "ack-without-spawn" {
            // Test mode: ack success without spawning a container.
            info!(request_id, "ack-without-spawn mode — returning success without Docker");
            let n = PROVISION_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(Response::new(pb::ProvisionResponse {
                machine_id: format!("mock-machine-{n}"),
                expected_node_id: format!("provisioned-node-{n}"),
            }));
        }

        // Normal mode: spawn a boi-node container.
        let image = std::env::var(BOI_NODE_IMAGE_ENV)
            .unwrap_or_else(|_| BOI_NODE_IMAGE.to_string());
        let etcd = std::env::var("BOI_ETCD_ENDPOINTS")
            .unwrap_or_else(|_| "http://etcd:2379".to_string());
        let n = PROVISION_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let node_id = format!("provisioned-node-{n}");

        let output = Command::new("docker")
            .arg("run")
            .arg("-d")
            .arg("--network=boi-test")
            .arg(format!("-e=BOI_TOKEN={token}"))
            .arg(format!("-e=BOI_NODE_ID={node_id}"))
            .arg(format!("-e=BOI_ETCD_ENDPOINTS={etcd}"))
            .arg(&image)
            .arg("boi-node")
            .arg("node")
            .arg("join")
            .arg(format!("--token={token}"))
            .output();

        match output {
            Ok(out) if out.status.success() => {
                let machine_id = String::from_utf8_lossy(&out.stdout)
                    .trim()
                    .to_string();
                info!(machine_id, node_id, request_id, "container spawned");
                Ok(Response::new(pb::ProvisionResponse {
                    machine_id,
                    expected_node_id: node_id,
                }))
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                warn!(request_id, ?stderr, "docker run failed");
                Err(Status::internal(format!("docker run failed: {stderr}")))
            }
            Err(e) => {
                warn!(request_id, error = %e, "docker run error");
                Err(Status::internal(format!("docker exec error: {e}")))
            }
        }
    }

    async fn deprovision(
        &self,
        req: Request<pb::DeprovisionRequest>,
    ) -> Result<Response<pb::DeprovisionResponse>, Status> {
        let machine_id = req.into_inner().machine_id;
        info!(machine_id, "DeprovisionRequest received");
        append_transcript(serde_json::json!({
            "rpc": "DeprovisionRequest",
            "machine_id": machine_id,
        }));
        let out = Command::new("docker")
            .arg("rm")
            .arg("-f")
            .arg(&machine_id)
            .output();
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => warn!(machine_id, stderr = %String::from_utf8_lossy(&o.stderr), "docker rm failed"),
            Err(e) => warn!(machine_id, error = %e, "docker rm error"),
        }
        Ok(Response::new(pb::DeprovisionResponse {}))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "boi_provisioner_docker=info".parse().unwrap()),
        )
        .init();

    let addr = std::env::var("BOI_PROVISIONER_LISTEN")
        .unwrap_or_else(|_| DEFAULT_LISTEN.to_string());
    let addr = addr.parse().context("parse listen address")?;

    // Signal readiness to the plugin host (BOI_READY handshake, F-11).
    println!("BOI_READY");

    info!(%addr, "boi-provisioner-docker listening");
    Server::builder()
        .add_service(ProvisionerServer::new(DockerProvisioner::default()))
        .serve(addr)
        .await
        .context("gRPC server error")?;

    Ok(())
}
