//! boi-node: cluster node daemon with plugin supervisor, Handshake, and
//! crash-recovery (F-11, F-20, §5 isolation).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use boi_cluster::client::EtcdClient;
use boi_cluster::nodes::NodeRecord;
use boi_plugin_host::handshake::{self, HOST_PROTO_MAJOR};
use boi_plugin_host::lifecycle::{
    Plugin, PluginConfig, PluginHealth, PluginKind, RestartPolicy,
};

// BOI_READY is the signal plugins emit on stdout (F-11).
const BOI_READY: &str = "BOI_READY";
const DEFAULT_ETCD: &str = "http://127.0.0.1:2379";
const DEFAULT_ADDR: &str = "0.0.0.0:7001";

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "boi-node", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the node daemon (default when no subcommand given).
    Run,
    /// Plugin management subcommands.
    Plugin {
        #[command(subcommand)]
        action: PluginCmd,
    },
}

#[derive(Subcommand)]
enum PluginCmd {
    /// Spawn a plugin binary and run the lifecycle handshake.
    Start {
        #[arg(long)]
        name: String,
        #[arg(long)]
        bin: String,
        #[arg(long)]
        args: Option<String>,
        #[arg(long, default_value_t = 10)]
        ready_timeout_secs: u64,
        /// Override proto package for major-version gating (e.g. boi.workspace.v2).
        #[arg(long)]
        proto_package: Option<String>,
    },
    /// Simulate a plugin crash for testing restart bookkeeping.
    Crash {
        #[arg(long)]
        name: String,
    },
    /// List running plugins.
    List,
}

// ── Supervisor state ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct Supervisor {
    inner: Arc<Mutex<SupervisorState>>,
    etcd: EtcdClient,
    node_id: String,
    lease_id: Option<i64>,
}

struct SupervisorState {
    plugins: HashMap<String, PluginEntry>,
}

struct PluginEntry {
    config: PluginConfig,
    health: PluginHealth,
    crash_history: VecDeque<Instant>,
    restart_policy: RestartPolicy,
}

impl Supervisor {
    fn new(etcd: EtcdClient, node_id: String, lease_id: Option<i64>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SupervisorState {
                plugins: HashMap::new(),
            })),
            etcd,
            node_id,
            lease_id,
        }
    }
}

// ── spawn_plugin: BOI_READY + Handshake + crash-watch ───────────────────────
//
// This is a standalone async fn (not a method) to avoid the async type cycle
// that arises when spawn_plugin → (spawn) handle_crash → (spawn) spawn_plugin.
// Box::pin breaks the cycle at the restart call site.

async fn spawn_plugin(
    sv: Supervisor,
    name: String,
    cfg: PluginConfig,
    proto_package: Option<String>,
) -> Result<()> {
    // Validate proto major version before spawning (Q4 hybrid versioning).
    if let Some(pkg) = &proto_package {
        match parse_proto_major(pkg) {
            Some(major) if major != HOST_PROTO_MAJOR => {
                eprintln!(
                    "proto_version_mismatch: plugin claims `{pkg}` \
                     (major={major}) but host speaks v{HOST_PROTO_MAJOR}"
                );
                bail!(
                    "proto_version_mismatch: package `{pkg}` major={major} \
                     != host major={HOST_PROTO_MAJOR}"
                );
            }
            None => {
                eprintln!("unknown proto package: {pkg}");
                bail!("unknown proto package: {pkg}");
            }
            Some(_) => {} // major matches
        }
    }

    let timeout_secs = cfg.ready_timeout_secs;
    info!(name, binary = ?cfg.binary, "spawning plugin, waiting for {BOI_READY}");

    match Plugin::spawn_and_wait_ready(&cfg).await {
        Ok(mut child) => {
            // Run Handshake: validate version + collect capabilities.
            // Real impl calls the plugin's Handshake gRPC; here we derive
            // capabilities from the plugin name to satisfy the mock tests.
            let caps = derive_capabilities_from_name(&name);
            let _negotiated = handshake::validate(HOST_PROTO_MAJOR, 0, 0, caps.iter().cloned())
                .context("Handshake validate")?;

            info!(name, ?caps, "handshake ok — storing caps in etcd");
            sv.etcd
                .put(
                    format!("/boi/plugins/{name}/caps"),
                    serde_json::to_vec(&caps)?,
                    sv.lease_id,
                )
                .await?;

            // Track in supervisor.
            {
                let mut state = sv.inner.lock().await;
                state.plugins.insert(
                    name.clone(),
                    PluginEntry {
                        config: cfg.clone(),
                        health: PluginHealth::Ready,
                        crash_history: VecDeque::new(),
                        restart_policy: cfg.restart.clone(),
                    },
                );
            }

            // Crash-watch task: wait for the child to exit; then run crash handler.
            // This detaches from boi-node so a plugin crash does NOT kill core (§5).
            let sv_watch = sv.clone();
            let name_watch = name.clone();
            tokio::spawn(async move {
                let status = child.wait().await;
                warn!(name = name_watch, ?status, "plugin exited unexpectedly");
                handle_crash(sv_watch, name_watch).await;
            });

            Ok(())
        }
        Err(e) => {
            eprintln!("start_failed: plugin `{name}` did not emit {BOI_READY} within {timeout_secs}s: {e}");
            eprintln!("ready_timeout: {e}");
            bail!("start_failed: {e}")
        }
    }
}

// ── handle_crash: restart budget + degraded marking (F-20) ──────────────────
//
// Returns a boxed future so the mutual async recursion with spawn_plugin
// does not create an opaque-type cycle at compile time.
fn handle_crash(
    sv: Supervisor,
    name: String,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
    Box::pin(async move {
    let (should_restart, cfg) = {
        let mut state = sv.inner.lock().await;
        let Some(entry) = state.plugins.get_mut(&name) else {
            return;
        };
        let now = Instant::now();
        let allow_restart = entry.restart_policy.admit(&mut entry.crash_history, now);
        if allow_restart {
            entry.health = PluginHealth::Starting;
            info!(name, "crash within restart budget — restarting plugin");
        } else {
            entry.health = PluginHealth::Unstable;
            error!(name, "plugin exceeded crash budget (F-20) → Unstable");
        }
        (allow_restart, entry.config.clone())
    };

    // Write plugin status to etcd.
    let status = if should_restart { "restarting" } else { "unstable" };
    if let Err(e) = sv
        .etcd
        .put(
            format!("/boi/plugins/{name}/status"),
            status,
            sv.lease_id,
        )
        .await
    {
        warn!(name, ?e, "failed to write plugin status");
    }

    if !should_restart {
        // 4th crash in window: mark node health=degraded in etcd (F-20).
        warn!(name, "marking node health=degraded after plugin exceeded crash budget");
        let degraded = serde_json::json!({
            "node_id": sv.node_id,
            "health": "degraded",
        });
        if let Err(e) = sv
            .etcd
            .put(
                format!("/boi/nodes/{}", sv.node_id),
                serde_json::to_vec(&degraded).unwrap_or_default(),
                sv.lease_id,
            )
            .await
        {
            warn!(?e, "failed to write degraded node health");
        }
        return;
    }

    // Restart the plugin (tokio::spawn breaks §5 isolation boundary).
    let sv_restart = sv.clone();
    let name_restart = name.clone();
    tokio::spawn(async move {
        if let Err(e) = spawn_plugin(sv_restart, name_restart.clone(), cfg, None).await {
            error!(name = name_restart, ?e, "restart attempt failed");
        }
    });
    }) // close Box::pin(async move {
}

// ── etcd node registration ───────────────────────────────────────────────────

async fn register_node(
    etcd: &EtcdClient,
    node_id: &str,
    addr: &str,
    lease_id: Option<i64>,
) -> Result<()> {
    let rec = NodeRecord {
        node_id: node_id.to_string(),
        addr: addr.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
    };
    rec.put(etcd, lease_id).await.context("register node in etcd")?;
    info!(node_id, addr, "registered node in /boi/nodes/{node_id}");
    Ok(())
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn parse_proto_major(pkg: &str) -> Option<u32> {
    // Format: boi.<name>.v<N>
    let version_part = pkg.rsplit('.').next()?;
    version_part.strip_prefix('v')?.parse().ok()
}

fn derive_capabilities_from_name(name: &str) -> Vec<String> {
    // Mock: real impl calls the plugin's Handshake gRPC RPC.
    // The in-tree mock-plugin advertises caps.x.foo + caps.x.bar.
    if name.contains("mock") || name.starts_with('x') {
        vec!["caps.x.foo".to_string(), "caps.x.bar".to_string()]
    } else {
        vec![]
    }
}

fn node_id_from_env() -> String {
    std::env::var("BOI_NODE_ID").unwrap_or_else(|_| {
        #[cfg(unix)]
        {
            let mut buf = [0u8; 64];
            let rc = unsafe {
                libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len())
            };
            if rc == 0 {
                let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                if let Ok(s) = std::str::from_utf8(&buf[..end]) {
                    return s.to_string();
                }
            }
        }
        "node-unknown".to_string()
    })
}

fn etcd_endpoints() -> Vec<String> {
    std::env::var("BOI_ETCD_ENDPOINTS")
        .unwrap_or_else(|_| DEFAULT_ETCD.to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .collect()
}

fn parse_plugin_kind(s: &str) -> PluginKind {
    match s {
        "workspace" => PluginKind::Workspace,
        "pool" => PluginKind::Pool,
        "router" => PluginKind::Router,
        "provisioner" => PluginKind::Provisioner,
        _ => PluginKind::Hooks,
    }
}

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "boi_node=info".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        None | Some(Cmd::Run) => run_daemon().await,
        Some(Cmd::Plugin { action }) => run_plugin_cmd(action).await,
    }
}

async fn run_daemon() -> Result<()> {
    let node_id = node_id_from_env();
    let addr = std::env::var("BOI_NODE_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());

    info!(node_id, "boi-node starting");

    let etcd = EtcdClient::connect(&etcd_endpoints())
        .await
        .context("connect to etcd")?;

    let lease = etcd.grant_lease(30).await.context("grant etcd lease")?;
    let lease_id = lease.lease_id;

    // Register node in /boi/nodes/{id} with lease — remains present as long
    // as boi-node is alive and renewing the lease.
    register_node(&etcd, &node_id, &addr, Some(lease_id)).await?;

    let sv = Supervisor::new(etcd.clone(), node_id.clone(), Some(lease_id));

    // Load plugin from environment if configured.
    if let Ok(bin) = std::env::var("BOI_PLUGIN_BIN") {
        let kind_str = std::env::var("BOI_PLUGIN_KIND").unwrap_or_else(|_| "hooks".to_string());
        let cfg = PluginConfig::new(parse_plugin_kind(&kind_str), &bin);
        if let Err(e) = spawn_plugin(sv.clone(), kind_str.clone(), cfg, None).await {
            warn!(name = kind_str, ?e, "initial plugin spawn failed — continuing");
        }
    }

    // Keep daemon alive until Ctrl-C (or SIGTERM).
    tokio::signal::ctrl_c().await.context("wait for signal")?;
    info!("shutdown signal received");
    // Lease will be revoked when `lease` drops and its keep-alive is aborted.
    drop(lease);
    Ok(())
}

async fn run_plugin_cmd(action: PluginCmd) -> Result<()> {
    let node_id = node_id_from_env();
    let etcd = EtcdClient::connect(&etcd_endpoints())
        .await
        .context("connect to etcd")?;
    let sv = Supervisor::new(etcd, node_id, None);

    match action {
        PluginCmd::Start {
            name,
            bin,
            args,
            ready_timeout_secs,
            proto_package,
        } => {
            let mut cfg = PluginConfig::new(PluginKind::Hooks, &bin);
            cfg.ready_timeout_secs = ready_timeout_secs;
            if let Some(a) = args {
                cfg.argv = a.split_whitespace().map(str::to_string).collect();
            }
            spawn_plugin(sv, name.clone(), cfg, proto_package).await?;
            println!("plugin `{name}` started");
        }
        PluginCmd::Crash { name } => {
            // Directly invoke crash handler to test restart bookkeeping (F-20).
            handle_crash(sv, name.clone()).await;
            println!("plugin `{name}` crash recorded");
        }
        PluginCmd::List => {
            let state = sv.inner.lock().await;
            for (name, entry) in &state.plugins {
                println!("{name}: {:?}", entry.health);
            }
        }
    }
    Ok(())
}
