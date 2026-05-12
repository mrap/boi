# Plugin Author Quickstart — Workspace Plugin in ~50 Lines

This quickstart walks you through building the smallest possible
v0.1 Workspace plugin. The same shape applies to Worker Pool and
Hooks plugins — only the proto service and RPC bodies differ.

By the end you will have a standalone binary that:
1. Speaks the `boi.workspace.v1` gRPC service.
2. Survives the host handshake.
3. Provisions a workspace as a fresh temp directory.
4. Executes commands inside it.
5. Cleans up on request.

It is not production-ready — there is no merge-back, no fetch, no
isolation enforcement — but it is enough to prove the contract end
to end and to copy-paste into a real implementation.

## Prerequisites

- Rust 1.78+ with `cargo`.
- The proto descriptors from `crates/boi-proto/proto/boi/workspace/`.
- A `boi-node` you can register the plugin against (see the
  operator guide).

## Step 1 — New Cargo Project

```
cargo new --bin tmpfs-workspace
cd tmpfs-workspace
```

Add to `Cargo.toml`:

```toml
[dependencies]
tonic = "0.11"
prost = "0.12"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "process"] }
tempfile = "3"

[build-dependencies]
tonic-build = "0.11"
```

## Step 2 — Generate the Service Stubs

Copy `workspace.proto` from `crates/boi-proto/proto/boi/workspace/v1/`
into `proto/`. Add `build.rs`:

```rust
fn main() {
    tonic_build::compile_protos("proto/workspace.proto").unwrap();
}
```

## Step 3 — Implement the Service

`src/main.rs`:

```rust
use std::collections::HashMap;
use std::process::Command;
use std::sync::Mutex;

use tonic::{transport::Server, Request, Response, Status};

pub mod pb {
    tonic::include_proto!("boi.workspace.v1");
}
use pb::workspace_server::{Workspace, WorkspaceServer};
use pb::*;

#[derive(Default)]
struct TmpfsWorkspace {
    paths: Mutex<HashMap<String, tempfile::TempDir>>,
}

#[tonic::async_trait]
impl Workspace for TmpfsWorkspace {
    async fn handshake(&self, _: Request<HandshakeRequest>)
        -> Result<Response<HandshakeResponse>, Status>
    {
        Ok(Response::new(HandshakeResponse {
            plugin_proto_minor: 0,
            capabilities: vec!["exec".into()],
        }))
    }

    async fn provision(&self, req: Request<ProvisionRequest>)
        -> Result<Response<ProvisionResponse>, Status>
    {
        let id = req.into_inner().spec_id;
        let dir = tempfile::tempdir().map_err(|e| Status::internal(e.to_string()))?;
        let path = dir.path().to_string_lossy().into_owned();
        self.paths.lock().unwrap().insert(id.clone(), dir);
        Ok(Response::new(ProvisionResponse { workspace_id: id, path }))
    }

    async fn exec(&self, req: Request<ExecRequest>)
        -> Result<Response<ExecResponse>, Status>
    {
        let r = req.into_inner();
        let paths = self.paths.lock().unwrap();
        let dir = paths.get(&r.workspace_id)
            .ok_or_else(|| Status::not_found("workspace"))?;
        let out = Command::new(&r.argv[0]).args(&r.argv[1..])
            .current_dir(dir.path()).envs(r.env).output()
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(ExecResponse {
            exit_code: out.status.code().unwrap_or(-1),
            stdout: out.stdout, stderr: out.stderr,
        }))
    }

    async fn cleanup(&self, req: Request<CleanupRequest>)
        -> Result<Response<CleanupResponse>, Status>
    {
        self.paths.lock().unwrap().remove(&req.into_inner().workspace_id);
        Ok(Response::new(CleanupResponse {}))
    }

    // Fetch/Setup/Verify are optional for a minimal plugin; return Ok.
    async fn fetch(&self, _: Request<FetchRequest>) -> Result<Response<FetchResponse>, Status> {
        Ok(Response::new(FetchResponse::default()))
    }
    async fn setup(&self, _: Request<SetupRequest>) -> Result<Response<SetupResponse>, Status> {
        Ok(Response::new(SetupResponse::default()))
    }
    async fn verify(&self, _: Request<VerifyRequest>) -> Result<Response<VerifyResponse>, Status> {
        Ok(Response::new(VerifyResponse { ok: true, detail: "".into() }))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "[::1]:50061".parse()?;
    Server::builder()
        .add_service(WorkspaceServer::new(TmpfsWorkspace::default()))
        .serve(addr).await?;
    Ok(())
}
```

That is roughly fifty lines of behavior — handshake, provision, exec,
cleanup, plus three stubs to satisfy the contract.

## Step 4 — Register With a Node

In `/etc/boi/node.toml`:

```toml
[[plugins.workspace]]
name = "tmpfs"
endpoint = "[::1]:50061"
# mTLS omitted for brevity; production deployments MUST set ca/cert/key here.
```

Restart the node and probe:

```
boi plugin ls
boi plugin test tmpfs
```

## Step 5 — Use It From a Spec

```yaml
workspace_backend: tmpfs
tasks:
  - id: hello
    run: ["sh", "-c", "echo hi from $(pwd)"]
```

Submit with `boi run spec.yaml --tail`. You should see `hi from
/tmp/...` in the streamed output.

## Versioning Notes

- The plugin advertises `plugin_proto_minor` in the handshake. Bump
  it whenever you adopt a new backwards-compatible field from a
  later v1 minor.
- Breaking changes require a `v2/` proto package and a new
  service name. The host will refuse to dial a plugin whose package
  major differs from its own.
- Capabilities are an open string set. Document any custom
  capabilities you advertise so spec authors can opt in.

## What This Quickstart Skips

- TLS: production plugins must terminate mTLS using the cluster CA.
- Crash recovery: the host expects `Provision` to be idempotent on
  retry within the same `claim_lease_id`.
- Streaming exec: long-running commands should chunk stdout and
  stderr; a future v1.x minor adds a streaming `Exec` RPC.
- Concurrency: the example uses a global mutex. Production
  implementations should use a per-workspace structure or a sharded
  map.

See the worker-pool and workspace-backends reference docs for the
full contract.
