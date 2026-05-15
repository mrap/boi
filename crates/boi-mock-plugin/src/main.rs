use clap::Parser;
use tonic::{transport::Server, Request, Response, Status};

use boi_proto::hooks::v1::{
    hooks_server::{Hooks, HooksServer},
    EmitRequest, EmitResponse, HandshakeRequest, HandshakeResponse,
};

use boi_proto::provisioner::v1::{
    provisioner_server::{Provisioner, ProvisionerServer},
    DeprovisionRequest, DeprovisionResponse,
    HandshakeRequest as ProvHandshakeRequest, HandshakeResponse as ProvHandshakeResponse,
    ProvisionRequest, ProvisionResponse,
};

#[derive(Parser, Debug)]
#[command(name = "boi-mock-plugin")]
struct Args {
    #[arg(long, default_value_t = 50051)]
    port: u16,
    #[arg(long, default_value_t = 0)]
    ack_delay_ms: u64,
    #[arg(long, default_value = "mock")]
    plugin_id: String,
    /// Run as provisioner plugin instead of hooks plugin.
    #[arg(long)]
    provisioner: bool,
}

struct MockPlugin {
    ack_delay_ms: u64,
    plugin_id: String,
}

#[tonic::async_trait]
impl Hooks for MockPlugin {
    async fn handshake(
        &self,
        _request: Request<HandshakeRequest>,
    ) -> Result<Response<HandshakeResponse>, Status> {
        Ok(Response::new(HandshakeResponse {
            plugin_proto_minor: 0,
            capabilities: vec!["caps.x.foo".to_string(), "caps.x.bar".to_string()],
        }))
    }

    async fn emit(
        &self,
        request: Request<EmitRequest>,
    ) -> Result<Response<EmitResponse>, Status> {
        let req = request.into_inner();
        if self.ack_delay_ms > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(self.ack_delay_ms)).await;
        }
        let path = format!("/tmp/{}.delivered", self.plugin_id);
        let line = format!(
            "{}\n",
            serde_json::json!({
                "event_type": req.event_type,
                "sequence": req.sequence,
            })
        );
        use tokio::io::AsyncWriteExt;
        if let Ok(mut f) = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
        {
            let _ = f.write_all(line.as_bytes()).await;
        }
        Ok(Response::new(EmitResponse {
            acked_sequence: req.sequence,
        }))
    }
}

const TRANSCRIPT_PATH: &str = "/var/lib/boi-plugin/transcript.jsonl";

struct MockProvisioner;

#[tonic::async_trait]
impl Provisioner for MockProvisioner {
    async fn handshake(
        &self,
        _request: Request<ProvHandshakeRequest>,
    ) -> Result<Response<ProvHandshakeResponse>, Status> {
        Ok(Response::new(ProvHandshakeResponse {
            plugin_proto_minor: 0,
            capabilities: vec!["provisioner.docker".to_string()],
        }))
    }

    async fn provision(
        &self,
        request: Request<ProvisionRequest>,
    ) -> Result<Response<ProvisionResponse>, Status> {
        let req = request.into_inner();
        let line = format!(
            "{}\n",
            serde_json::json!({
                "rpc": "ProvisionRequest",
                "spec_id": req.spec_id,
                "request_id": req.request_id,
            })
        );
        use tokio::io::AsyncWriteExt;
        if let Ok(mut f) = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(TRANSCRIPT_PATH)
            .await
        {
            let _ = f.write_all(line.as_bytes()).await;
        }
        Ok(Response::new(ProvisionResponse {
            machine_id: format!("mock-machine-{}", req.request_id),
            expected_node_id: format!("mock-node-{}", req.request_id),
        }))
    }

    async fn deprovision(
        &self,
        request: Request<DeprovisionRequest>,
    ) -> Result<Response<DeprovisionResponse>, Status> {
        let req = request.into_inner();
        let line = format!(
            "{}\n",
            serde_json::json!({
                "rpc": "DeprovisionRequest",
                "machine_id": req.machine_id,
            })
        );
        use tokio::io::AsyncWriteExt;
        if let Ok(mut f) = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(TRANSCRIPT_PATH)
            .await
        {
            let _ = f.write_all(line.as_bytes()).await;
        }
        Ok(Response::new(DeprovisionResponse {}))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    println!("BOI_READY");
    println!("GRPC_PORT={}", args.port);

    #[cfg(unix)]
    tokio::spawn(async {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sig = signal(SignalKind::user_defined1()).expect("SIGUSR1 handler");
        sig.recv().await;
        std::process::abort();
    });

    let addr = format!("0.0.0.0:{}", args.port).parse()?;

    if args.provisioner {
        if let Some(parent) = std::path::Path::new(TRANSCRIPT_PATH).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Server::builder()
            .add_service(ProvisionerServer::new(MockProvisioner))
            .serve(addr)
            .await?;
    } else {
        Server::builder()
            .add_service(HooksServer::new(MockPlugin {
                ack_delay_ms: args.ack_delay_ms,
                plugin_id: args.plugin_id,
            }))
            .serve(addr)
            .await?;
    }

    Ok(())
}
