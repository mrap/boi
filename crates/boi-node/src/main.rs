//! `boi-node` — node daemon + cluster bootstrap CLI.
//!
//! Subcommands wired here (Phase 3):
//!   * `cluster init`              — generate cluster CA, persist locally,
//!                                   publish CA + fingerprint to etcd, register
//!                                   seed node with `cluster_admin=true`.
//!   * `cluster mint-join-token`   — gated by `cluster_admin`; returns
//!                                   `PermissionDenied` for non-admin callers.
//!   * `cluster members`           — list `/boi/nodes/` as JSON.
//!   * `node join --token <JWT>`   — validate token against the cluster CA
//!                                   (signature + pinned fingerprint), then
//!                                   register `/boi/nodes/{id}`.
//!
//! With no subcommand the binary runs as a long-lived daemon (sleeps until
//! SIGTERM); this lets `docker compose up` keep the container alive so
//! the e2e harness can `docker compose exec node-x boi-node <cmd>`.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

use boi_cluster::EtcdClient;
use boi_identity::ca::{ClusterCa, default_ca_dir};
use boi_identity::join_token::{ca_fingerprint, mint_join_token, validate_token, DEFAULT_TTL_SECS};

const CLUSTER_PREFIX: &str = "/boi/cluster/";
const NODES_PREFIX: &str = "/boi/nodes/";
const DEFAULT_CLUSTER_ID: &str = "boi-cluster";

/// JSON envelope written to `/boi/nodes/{id}`. The e2e harness greps the
/// raw value for `"cluster_admin":true`, so the field is serialized
/// at the top level (not nested under caps).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct NodeEntry {
    node_id: String,
    addr: String,
    version: String,
    started_at: i64,
    cluster_admin: bool,
}

#[derive(Parser, Debug)]
#[command(name = "boi-node", version, about = "BOI node daemon")]
struct Cli {
    /// Override BOI_NODE_ID for this invocation.
    #[arg(long = "node-id", global = true)]
    node_id: Option<String>,

    /// Override BOI_ETCD_ENDPOINTS (comma-separated).
    #[arg(long = "etcd", global = true)]
    etcd: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Cluster-scoped operations: init / mint-join-token / members.
    Cluster {
        #[command(subcommand)]
        sub: ClusterCmd,
    },
    /// Node-scoped operations: join.
    Node {
        #[command(subcommand)]
        sub: NodeCmd,
    },
}

#[derive(Subcommand, Debug)]
enum ClusterCmd {
    /// `cluster init` — generate CA, register seed with cluster_admin=true.
    Init,
    /// `cluster mint-join-token` — RBAC-gated MintJoinToken (cluster_admin only).
    MintJoinToken,
    /// `cluster members` — list registered nodes as JSON.
    Members,
}

#[derive(Subcommand, Debug)]
enum NodeCmd {
    /// `node join --token <JWT>` — validate + register.
    Join {
        #[arg(long)]
        token: String,
    },
}

fn node_id_from(cli: &Cli) -> Result<String> {
    cli.node_id
        .clone()
        .or_else(|| std::env::var("BOI_NODE_ID").ok())
        .ok_or_else(|| anyhow!("BOI_NODE_ID not set and --node-id not provided"))
}

fn etcd_endpoints_from(cli: &Cli) -> Vec<String> {
    let raw = cli
        .etcd
        .clone()
        .or_else(|| std::env::var("BOI_ETCD_ENDPOINTS").ok())
        .unwrap_or_else(|| "http://localhost:2379".into());
    raw.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

fn ca_dir() -> PathBuf {
    default_ca_dir().unwrap_or_else(|| PathBuf::from("/var/lib/boi/cluster"))
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn connect(cli: &Cli) -> Result<EtcdClient> {
    let eps = etcd_endpoints_from(cli);
    EtcdClient::connect(&eps)
        .await
        .with_context(|| format!("connect etcd at {eps:?}"))
}

// -------------------- cluster init --------------------

async fn cluster_init(cli: &Cli) -> Result<()> {
    let node_id = node_id_from(cli)?;
    let dir = ca_dir();
    let ca = ClusterCa::load_or_generate(&dir)
        .with_context(|| format!("CA load_or_generate({dir:?})"))?;
    let der = ca.cert_der().context("CA DER")?;
    let fp = ca_fingerprint(&der);

    let client = connect(cli).await?;
    client
        .put(
            format!("{CLUSTER_PREFIX}ca.fingerprint"),
            fp.as_bytes().to_vec(),
            None,
        )
        .await?;
    client
        .put(
            format!("{CLUSTER_PREFIX}ca.crt"),
            ca.cert_pem().as_bytes().to_vec(),
            None,
        )
        .await?;
    client
        .put(
            format!("{CLUSTER_PREFIX}cluster_id"),
            DEFAULT_CLUSTER_ID.as_bytes().to_vec(),
            None,
        )
        .await?;

    let entry = NodeEntry {
        node_id: node_id.clone(),
        addr: format!("{node_id}:7000"),
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at: now_unix(),
        cluster_admin: true,
    };
    let body = serde_json::to_vec(&entry)?;
    client
        .put(format!("{NODES_PREFIX}{node_id}"), body, None)
        .await?;

    eprintln!("cluster init ok (node_id={node_id}, ca_fingerprint={fp})");
    Ok(())
}

// -------------------- cluster mint-join-token (RBAC) --------------------

/// Mint a join token. Returns `PermissionDenied` if the caller node is
/// not flagged `cluster_admin=true` in `/boi/nodes/{caller}`.
async fn mint_join_token_cmd(cli: &Cli) -> Result<()> {
    let caller = node_id_from(cli)?;
    let client = connect(cli).await?;

    // RBAC gate: caller must be cluster_admin.
    let raw = client
        .get(format!("{NODES_PREFIX}{caller}"))
        .await?
        .ok_or_else(|| anyhow!("PermissionDenied: caller `{caller}` not registered"))?;
    let entry: NodeEntry = serde_json::from_slice(&raw)
        .with_context(|| format!("decode /boi/nodes/{caller}"))?;
    if !entry.cluster_admin {
        // Stderr signal the harness greps for.
        eprintln!("PermissionDenied: node `{caller}` lacks cluster_admin");
        bail!("PermissionDenied");
    }

    // Load CA from etcd-published cert (single source of truth) so any
    // admin node can mint, not just the original seed.
    let ca_pem = client
        .get(format!("{CLUSTER_PREFIX}ca.crt"))
        .await?
        .ok_or_else(|| anyhow!("cluster CA not initialized; run `cluster init`"))?;
    let ca_pem = String::from_utf8(ca_pem).context("ca.crt utf8")?;

    // We need the CA *private key* to sign; that lives only on the seed's
    // local disk. Fall back to local CA dir for the key material.
    let local = ClusterCa::load(&ca_dir()).with_context(|| {
        "local CA key not present; mint-join-token must run on a seed node"
    })?;
    let der = local.cert_der()?;
    // Sanity: local CA must match cluster CA in etcd.
    if local.cert_pem().trim() != ca_pem.trim() {
        bail!("local CA does not match /boi/cluster/ca.crt — corrupted state");
    }

    let cluster_id = client
        .get(format!("{CLUSTER_PREFIX}cluster_id"))
        .await?
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_else(|| DEFAULT_CLUSTER_ID.to_string());

    let token = mint_join_token(
        local.key_pem(),
        &der,
        &cluster_id,
        vec![format!("{caller}:7000")],
        DEFAULT_TTL_SECS,
    )?;
    // Token to stdout, single line — the test reads stdout.trim().
    println!("{token}");
    Ok(())
}

// -------------------- cluster members --------------------

async fn cluster_members(cli: &Cli) -> Result<()> {
    let client = connect(cli).await?;
    let kvs = client.get_prefix(NODES_PREFIX).await?;
    let mut entries: Vec<NodeEntry> = Vec::with_capacity(kvs.len());
    for (_, v) in kvs {
        match serde_json::from_slice::<NodeEntry>(&v) {
            Ok(e) => entries.push(e),
            // Be permissive: legacy schemas (e.g. NodeRecord without
            // cluster_admin) shouldn't crash the listing.
            Err(_) => {}
        }
    }
    entries.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    let json = serde_json::to_string(&entries)?;
    println!("{json}");
    Ok(())
}

// -------------------- node join --token --------------------

async fn node_join(cli: &Cli, token: &str) -> Result<()> {
    let node_id = node_id_from(cli)?;
    let client = connect(cli).await?;

    // Pin: read the cluster's CA cert + fingerprint from etcd, validate
    // the token's signature against the CA public key and require its
    // embedded fingerprint to match. A tampered token (flipped fingerprint
    // or flipped signature bits) fails here.
    let ca_pem = client
        .get(format!("{CLUSTER_PREFIX}ca.crt"))
        .await?
        .ok_or_else(|| anyhow!("cluster CA not initialized"))?;
    let ca_pem = String::from_utf8(ca_pem).context("ca.crt utf8")?;
    let expected_fp = client
        .get(format!("{CLUSTER_PREFIX}ca.fingerprint"))
        .await?
        .ok_or_else(|| anyhow!("cluster CA fingerprint not initialized"))?;
    let expected_fp = String::from_utf8(expected_fp).context("ca.fingerprint utf8")?;

    let _claims = validate_token(token, &ca_pem, Some(expected_fp.trim()))
        .context("join token rejected (signature/fingerprint/expiry)")?;

    // Token is good — register self. We don't have a TLS plane yet to
    // request a signed leaf from the seed; record liveness so members()
    // sees us. Real cert provisioning lands once the gRPC plane is up.
    let entry = NodeEntry {
        node_id: node_id.clone(),
        addr: format!("{node_id}:7000"),
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at: now_unix(),
        cluster_admin: false,
    };
    let body = serde_json::to_vec(&entry)?;
    client
        .put(format!("{NODES_PREFIX}{node_id}"), body, None)
        .await?;
    eprintln!("node join ok (node_id={node_id})");
    Ok(())
}

// -------------------- daemon mode --------------------

/// No-op long-running daemon so the docker container stays up; the e2e
/// harness drives behavior via `docker compose exec node-x boi-node <cmd>`.
async fn run_daemon(cli: &Cli) -> Result<()> {
    let node_id = node_id_from(cli).unwrap_or_else(|_| "<unknown>".into());
    eprintln!("boi-node daemon up (node_id={node_id}); awaiting SIGTERM");
    tokio::signal::ctrl_c().await.ok();
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let res: Result<()> = match &cli.command {
        None => run_daemon(&cli).await,
        Some(Command::Cluster { sub }) => match sub {
            ClusterCmd::Init => cluster_init(&cli).await,
            ClusterCmd::MintJoinToken => mint_join_token_cmd(&cli).await,
            ClusterCmd::Members => cluster_members(&cli).await,
        },
        Some(Command::Node { sub }) => match sub {
            NodeCmd::Join { token } => node_join(&cli, token).await,
        },
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("boi-node: {e:#}");
            ExitCode::from(1)
        }
    }
}
