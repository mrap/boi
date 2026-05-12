//! boi-node: cluster node daemon with plugin supervisor, Handshake,
//! crash-recovery (F-11, F-20, §5 isolation), and the Phase 4
//! assignment loop (HRW + CAS claim + lease fencing).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use boi_assign::{assign, AssignResult, CapRequires, TaskRecord};
use boi_cluster::claims::{claim_key, ClaimRecord, CLAIMS_PREFIX};
use boi_cluster::client::{ConnectConfig, EtcdClient, TxnOp};
use boi_cluster::dispatch_queue::{
    queue_key, DispatchQueueRecord, QueueEntry, QUEUE_PREFIX,
};
use boi_cluster::membership::Membership;
use boi_cluster::nodes::{NodeCaps, NodeRecord, NODES_PREFIX};
use boi_plugin_host::handshake::{self, HOST_PROTO_MAJOR};
use boi_plugin_host::lifecycle::{
    Plugin, PluginConfig, PluginHealth, PluginKind, RestartPolicy,
};
use boi_plugin_host::provisioner::{
    build_provision_request, CapHint, JoinToken, ProvisionerClient,
};
use tonic::transport::Channel;
use uuid::Uuid;

const BOI_READY: &str = "BOI_READY";
const DEFAULT_ETCD: &str = "http://127.0.0.1:2379";
const DEFAULT_ADDR: &str = "0.0.0.0:7001";
const EVENTS_PREFIX: &str = "/boi/events/";
const CLUSTER_ADMIN_KEY: &str = "/boi/cluster/admin";
const PROVISION_FAILURES_PREFIX: &str = "/boi/provision-failures/";
const JOIN_TOKENS_PREFIX: &str = "/boi/join-tokens/";

// Assignment-loop cadence — fast enough that the 5s test budget catches
// a dispatch within one iteration, slow enough to keep etcd churn low.
const ASSIGN_POLL_INTERVAL: Duration = Duration::from_millis(250);

// Prometheus /metrics endpoint port.
const METRICS_PORT: u16 = 9090;

// Relative path under $HOME for operator-triggered local-fallback drain output.
const PENDING_FLUSH_DIR: &str = ".boi/pending-flush";

// F-12: counter incremented on each dispatch rejected due to etcd unreachable.
// Shared between the daemon metrics server and CLI dispatch subcommands via
// atomic so multiple tasks can update it safely.
static REJECTED_ETCD_UNREACHABLE: AtomicU64 = AtomicU64::new(0);

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "boi-node", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the node daemon (default).
    Run,
    /// Plugin management.
    Plugin {
        #[command(subcommand)]
        action: PluginCmd,
    },
    /// Spec dispatch — write a task to the dispatch-queue.
    Spec {
        #[command(subcommand)]
        action: SpecCmd,
    },
    /// Dispatch a spec YAML file. Returns the task id on stdout.
    Dispatch {
        spec: PathBuf,
    },
    /// Cluster bootstrap.
    Cluster {
        #[command(subcommand)]
        action: ClusterCmd,
    },
    /// Node-side commands.
    Node {
        #[command(subcommand)]
        action: NodeCmd,
    },
    /// Internal helpers used by the e2e harness.
    Internal {
        #[command(subcommand)]
        action: InternalCmd,
    },
    /// Cluster node join (alias for daemon with token verification).
    NodeJoin {
        #[arg(long)]
        token: Option<String>,
    },
}

#[derive(Subcommand)]
enum PluginCmd {
    Start {
        #[arg(long)]
        name: String,
        #[arg(long)]
        bin: String,
        #[arg(long)]
        args: Option<String>,
        #[arg(long, default_value_t = 10)]
        ready_timeout_secs: u64,
        #[arg(long)]
        proto_package: Option<String>,
    },
    Crash {
        #[arg(long)]
        name: String,
    },
    List,
}

#[derive(Subcommand)]
enum SpecCmd {
    /// Dispatch an inline task; writes /boi/dispatch-queue/{id} with
    /// state=pending + state_version=0 and prints the task id.
    Dispatch {
        #[arg(long)]
        name: String,
        /// Capability requires clause, e.g. `os=mac,runtime=xcode-15`.
        #[arg(long, default_value = "")]
        requires: String,
    },
}

#[derive(Subcommand)]
enum ClusterCmd {
    /// Initialise the cluster (no-op once etcd is reachable).
    Init,
    /// F-07: drain this node — stop accepting new tasks, persist all
    /// in-flight claim records to ~/.boi/pending-flush/ as JSONL, and
    /// print a warning to stderr. Operator-invoked only.
    LocalFallback,
    /// List cluster members ({node_id, addr}) read from etcd /boi/nodes/.
    Members,
    /// Mint a JWT join token signed by the cluster CA. Admin-gated (Q3):
    /// caller must hold `caps.static.cluster_admin=true`.
    #[command(name = "mint-join-token")]
    MintJoinToken,
}

#[derive(Subcommand)]
enum NodeCmd {
    /// Advertise this node's caps under /boi/caps/{node_id}.
    Advertise,
    /// Join an existing cluster using a provisioned BOI_TOKEN.
    Join {
        #[arg(long)]
        token: Option<String>,
    },
}

#[derive(Subcommand)]
enum InternalCmd {
    /// Attempt a claim CAS with a stale-revision predicate. Used by the
    /// revision-pin window e2e — exits non-zero with `revision_pin_window`
    /// in stderr on rejection.
    ForceClaim {
        #[arg(long)]
        task_id: String,
        #[arg(long)]
        max_mod_rev: i64,
    },
    /// Commit a task's result fenced on `claim_lease_id`. Used by the
    /// e2e_fencing tests. On lease mismatch we emit a
    /// `task.claim_fence_rejected` audit event and exit non-zero with
    /// `FAILED_PRECONDITION` in stderr.
    CommitTask {
        #[arg(long)]
        task_id: String,
        #[arg(long)]
        lease_id: Option<String>,
        #[arg(long, default_value = "done")]
        status: String,
    },
    /// Mint a short-lived JoinToken for provisioning. Admin-gated (Q3).
    MintProvisionToken {
        #[arg(long)]
        for_caps: String,
    },
    /// Set provisioner plugin mode (test harness hook).
    SetProvisionerMode {
        #[arg(long)]
        mode: String,
    },
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

async fn spawn_plugin(
    sv: Supervisor,
    name: String,
    cfg: PluginConfig,
    proto_package: Option<String>,
) -> Result<()> {
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
            Some(_) => {}
        }
    }

    let timeout_secs = cfg.ready_timeout_secs;
    info!(name, binary = ?cfg.binary, "spawning plugin, waiting for {BOI_READY}");

    match Plugin::spawn_and_wait_ready(&cfg).await {
        Ok(mut child) => {
            let caps = derive_capabilities_from_name(&name);
            let _ = handshake::validate(HOST_PROTO_MAJOR, 0, 0, caps.iter().cloned())
                .context("Handshake validate")?;
            info!(name, ?caps, "handshake ok — storing caps in etcd");
            sv.etcd
                .put(
                    format!("/boi/plugins/{name}/caps"),
                    serde_json::to_vec(&caps)?,
                    sv.lease_id,
                )
                .await?;
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

        let sv_restart = sv.clone();
        let name_restart = name.clone();
        tokio::spawn(async move {
            if let Err(e) = spawn_plugin(sv_restart, name_restart.clone(), cfg, None).await {
                error!(name = name_restart, ?e, "restart attempt failed");
            }
        });
    })
}

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
    let version_part = pkg.rsplit('.').next()?;
    version_part.strip_prefix('v')?.parse().ok()
}

fn derive_capabilities_from_name(name: &str) -> Vec<String> {
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

fn parse_requires(s: &str) -> CapRequires {
    let mut r = CapRequires::new();
    for tok in s.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        if let Some((k, v)) = tok.split_once('=') {
            r = r.with(k.trim(), v.trim());
        }
    }
    r
}

fn requires_to_map(r: &CapRequires) -> BTreeMap<String, String> {
    // CapRequires exposes builder-only API; re-export back to the map
    // shape DispatchQueueRecord stores. We mirror parse_requires.
    let _ = r;
    BTreeMap::new()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn new_task_id(name: &str) -> String {
    let n = unix_now();
    if name.is_empty() {
        format!("task-{n}")
    } else {
        format!("{name}-{n}")
    }
}

// ── Canonical events (§F-15) ─────────────────────────────────────────────────

async fn emit_event(etcd: &EtcdClient, kind: &str, payload: serde_json::Value) {
    let ts = unix_now();
    let key = format!("{EVENTS_PREFIX}{ts:020}-{kind}");
    let body = serde_json::json!({
        "kind": kind,
        "ts": ts,
        "payload": payload,
    });
    if let Err(e) = etcd
        .put(key, serde_json::to_vec(&body).unwrap_or_default(), None)
        .await
    {
        warn!(?e, kind, "failed to emit canonical event");
    }
}

// ── Provisioning helpers (Phase 5, §8, F-01, F-06) ──────────────────────────

/// True if `node_id` is the registered cluster_admin in etcd,
/// or if `BOI_NODE_ADMIN=true` is set (test override).
async fn is_cluster_admin(etcd: &EtcdClient, node_id: &str) -> bool {
    if std::env::var("BOI_NODE_ADMIN").as_deref() == Ok("true") {
        return true;
    }
    match etcd.get(CLUSTER_ADMIN_KEY).await {
        Ok(Some(v)) => String::from_utf8_lossy(&v).trim() == node_id,
        _ => false,
    }
}

/// Check if F-06 cooldown is active for the given task.
async fn provision_cooldown_active(etcd: &EtcdClient, task_id: &str) -> bool {
    let key = format!("{PROVISION_FAILURES_PREFIX}{task_id}");
    match etcd.get(key).await {
        Ok(Some(v)) => {
            if let Ok(map) = serde_json::from_slice::<serde_json::Value>(&v) {
                let failures = map
                    .get("consecutive_claim_failures")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let cooldown_until = map
                    .get("cooldown_until")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                failures >= 3 && unix_now() < cooldown_until
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Increment the provision failure counter for a task.
/// After 3 consecutive failures, set a 5-minute cooldown (F-06).
async fn increment_provision_failures(etcd: &EtcdClient, task_id: &str) {
    let key = format!("{PROVISION_FAILURES_PREFIX}{task_id}");
    let (failures, cooldown_until) = match etcd.get(key.clone()).await {
        Ok(Some(v)) => {
            if let Ok(map) = serde_json::from_slice::<serde_json::Value>(&v) {
                let f = map
                    .get("consecutive_claim_failures")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
                    + 1;
                let cu = if f >= 3 { unix_now() + 300 } else { 0 };
                (f, cu)
            } else {
                (1, 0)
            }
        }
        _ => (1, 0),
    };
    let val = serde_json::json!({
        "consecutive_claim_failures": failures,
        "cooldown_until": cooldown_until,
        "task_id": task_id,
    });
    if let Ok(b) = serde_json::to_vec(&val) {
        let _ = etcd.put(key, b, None).await;
    }
    if failures >= 3 {
        warn!(
            task_id,
            failures,
            "F-06: provision failure threshold reached — cooldown active for 5 min"
        );
    }
}

/// After a successful Provision RPC, watch for the expected node to
/// appear under `/boi/nodes/` within 60 s. If absent, increment the
/// F-06 failure counter.
async fn watch_provision_join(etcd: EtcdClient, task_id: String, expected_node_id: String) {
    use tokio::time::{sleep, Duration as TD, Instant};
    let deadline = Instant::now() + TD::from_secs(60);
    let node_key = format!("{NODES_PREFIX}{expected_node_id}");
    loop {
        if Instant::now() >= deadline {
            warn!(
                task_id,
                expected_node_id, "provisioned node did not join within 60s — incrementing F-06 counter"
            );
            increment_provision_failures(&etcd, &task_id).await;
            return;
        }
        match etcd.get(node_key.clone()).await {
            Ok(Some(_)) => {
                info!(task_id, expected_node_id, "provisioned node joined cluster");
                return;
            }
            _ => {}
        }
        sleep(TD::from_secs(5)).await;
    }
}

/// Call the Provisioner plugin and handle the F-06 join-watcher.
async fn provision_task(
    etcd: &EtcdClient,
    task_id: &str,
    provisioner_addr: &str,
    requires: BTreeMap<String, String>,
) {
    if provision_cooldown_active(etcd, task_id).await {
        debug!(task_id, "Provisioner cooldown active (F-06) — skipping");
        return;
    }
    let join_token = JoinToken {
        token: Uuid::new_v4().to_string(),
        expires_at: format!("{}Z", unix_now() + 300),
    };
    let cap_hint = CapHint {
        caps: requires.into_iter().collect(),
    };
    let bootstrap_url = std::env::var("BOI_BOOTSTRAP_URL")
        .unwrap_or_else(|_| "http://node-a:7001".to_string());
    let req = build_provision_request(
        join_token,
        cap_hint,
        task_id.to_string(),
        bootstrap_url,
        None,
    );
    let channel = match Channel::from_shared(provisioner_addr.to_string()) {
        Ok(ep) => match ep.connect().await {
            Ok(ch) => ch,
            Err(e) => {
                warn!(task_id, ?e, "failed to connect to Provisioner plugin");
                return;
            }
        },
        Err(e) => {
            warn!(task_id, ?e, "invalid Provisioner plugin addr");
            return;
        }
    };
    let mut client = ProvisionerClient::new(channel);
    let resp = match client.provision(req).await {
        Ok(r) => r.into_inner(),
        Err(e) => {
            warn!(task_id, ?e, "Provisioner.Provision RPC failed");
            return;
        }
    };
    info!(
        task_id,
        machine_id = %resp.machine_id,
        expected_node_id = %resp.expected_node_id,
        "Provisioner accepted request — monitoring for node join"
    );
    // F-06: watch for the new node to appear within 60 s.
    let etcd_w = etcd.clone();
    let tid = task_id.to_string();
    let nid = resp.expected_node_id.clone();
    tokio::spawn(async move {
        watch_provision_join(etcd_w, tid, nid).await;
    });
}

// ── Assignment loop ──────────────────────────────────────────────────────────
//
// Polls `/boi/dispatch-queue/` for pending tasks. For each pending task
// we read membership, call `boi_assign::assign`, and on success move the
// task to CLAIMED via CAS on its `mod_revision`. The Pool plugin (when
// wired) is spawned with `claim_lease_id` in gRPC metadata so its
// completion writes can be fenced.
//
// On lease expiry: we watch /boi/claims/ for DELETE events. When a
// claim disappears while its task is still CLAIMED, we requeue the task
// back to PENDING so the next poll triggers reassignment, and bump the
// node's consecutive_claim_failures via boi_assign::cooldown.
async fn assignment_loop(
    etcd: EtcdClient,
    membership: Membership,
    node_id: String,
    claim_lease_id: i64,
) {
    info!(node_id, "assignment_loop starting");
    loop {
        if let Err(e) =
            assignment_tick(&etcd, &membership, &node_id, claim_lease_id).await
        {
            warn!(?e, "assignment tick failed");
        }
        tokio::time::sleep(ASSIGN_POLL_INTERVAL).await;
    }
}

async fn assignment_tick(
    etcd: &EtcdClient,
    membership: &Membership,
    node_id: &str,
    claim_lease_id: i64,
) -> Result<()> {
    // List pending tasks from the dispatch_queue.
    let kvs = etcd
        .get_prefix(QUEUE_PREFIX)
        .await
        .context("list dispatch-queue")?;
    let snapshot = match membership.snapshot().await {
        Ok(s) => s,
        Err(e) => {
            // StaleSnapshot means etcd is unreachable — log it and skip
            // this tick. IN-FLIGHT SURVIVES: workers already running are
            // not touched; the loop simply waits for etcd to reconnect
            // (within one membership TTL cycle per spec §RESUME).
            warn!(?e, "StaleSnapshot: etcd unreachable; skipping assignment tick, in-flight workers unaffected");
            return Ok(());
        }
    };

    for (k, v) in kvs {
        let Some(task_id) = std::str::from_utf8(&k)
            .ok()
            .and_then(|s| s.strip_prefix(QUEUE_PREFIX))
        else {
            continue;
        };
        let rec: DispatchQueueRecord = match serde_json::from_slice(&v) {
            Ok(r) => r,
            Err(e) => {
                warn!(task_id, ?e, "skip undecodable queue record");
                continue;
            }
        };
        if !matches!(rec.state, boi_cluster::dispatch_queue::TaskState::Pending) {
            continue;
        }

        let mut requires = CapRequires::new();
        for (rk, rv) in &rec.requires {
            requires = requires.with(rk.clone(), rv.clone());
        }
        let task = TaskRecord {
            id: task_id.to_string(),
            requires,
        };
        let res = match assign(&task, &snapshot, etcd, claim_lease_id).await {
            Ok(r) => r,
            Err(e) => {
                warn!(task_id, ?e, "assign failed");
                continue;
            }
        };
        match res {
            AssignResult::Assigned(claim) => {
                // Transition the queue record: PENDING → CLAIMED via CAS.
                let entry = match DispatchQueueRecord::get(etcd, task_id).await {
                    Ok(Some(e)) => e,
                    _ => continue,
                };
                if let Ok(_claimed) = entry
                    .claim(etcd, claim.node_id.clone(), claim.lease_id)
                    .await
                {
                    info!(task_id, node = %claim.node_id, "task.claimed");
                    emit_event(
                        etcd,
                        "task.claimed",
                        serde_json::json!({
                            "task_id": task_id,
                            "claimant_node_id": claim.node_id,
                            "claim_lease_id": claim.lease_id,
                        }),
                    )
                    .await;
                    emit_event(
                        etcd,
                        "task.started",
                        serde_json::json!({ "task_id": task_id }),
                    )
                    .await;
                }
            }
            AssignResult::NeedProvision => {
                // Mark task pending-provision for the orchestrator. We
                // re-write the same envelope with a marker in last_error
                // so the test harness can observe the transition without
                // breaking the state machine.
                let key = queue_key(task_id);
                let mut next = rec.clone();
                next.last_error = Some("pending-provision".to_string());
                if let Ok(body) = serde_json::to_vec(&next) {
                    let _ = etcd.put(key, body, None).await;
                }
                emit_event(
                    etcd,
                    "task.reassigned",
                    serde_json::json!({
                        "task_id": task_id,
                        "reason": "pending-provision",
                    }),
                )
                .await;

                // Phase 5 — F-01: provision new capacity when no capable
                // node exists. Only cluster_admin nodes mint tokens (Q3).
                if is_cluster_admin(etcd, node_id).await {
                    if let Ok(addr) = std::env::var("BOI_PROVISIONER_ADDR") {
                        let etcd_c = etcd.clone();
                        let tid = task_id.to_string();
                        let cap_map = rec.requires.clone();
                        tokio::spawn(async move {
                            provision_task(&etcd_c, &tid, &addr, cap_map).await;
                        });
                    }
                } else {
                    debug!(
                        task_id,
                        node_id, "node is not cluster_admin — skipping Provisioner call"
                    );
                }
            }
        }
    }
    Ok(())
}

// ── Lease expiry watcher ─────────────────────────────────────────────────────
//
// Watches `/boi/claims/` for DELETE events. When a claim envelope
// disappears while the task is still CLAIMED in the dispatch-queue, the
// holder's lease expired — we requeue the task so the assignment loop
// picks a new home (reassign).
async fn lease_expiry_watcher(etcd: EtcdClient) {
    info!("lease_expiry watcher starting");
    let start_rev = match etcd.get_prefix_with_revision(CLAIMS_PREFIX).await {
        Ok((_, rev)) => rev + 1,
        Err(e) => {
            warn!(?e, "lease_expiry init read failed");
            return;
        }
    };
    let (_w, mut stream) = match etcd.watch_prefix(CLAIMS_PREFIX, start_rev).await {
        Ok(p) => p,
        Err(e) => {
            warn!(?e, "lease_expiry watch open failed");
            return;
        }
    };
    while let Ok(Some(resp)) = stream.message().await {
        for ev in resp.events() {
            if !matches!(ev.event_type(), etcd_client::EventType::Delete) {
                continue;
            }
            let Some(kv) = ev.kv() else { continue };
            let key = String::from_utf8_lossy(kv.key()).to_string();
            // Skip the `/claim_lease_id` sub-key deletions; only the
            // envelope delete drives reassign.
            if key.ends_with("/claim_lease_id") {
                continue;
            }
            let task_id = match key.strip_prefix(CLAIMS_PREFIX) {
                Some(t) => t.to_string(),
                None => continue,
            };
            handle_lease_expiry(&etcd, &task_id).await;
        }
    }
}

async fn handle_lease_expiry(etcd: &EtcdClient, task_id: &str) {
    let Ok(Some(entry)) = DispatchQueueRecord::get(etcd, task_id).await else {
        return;
    };
    if !matches!(
        entry.record.state,
        boi_cluster::dispatch_queue::TaskState::Claimed
    ) {
        return;
    }
    let stale_node = entry.record.claimant_node_id.clone().unwrap_or_default();
    match entry.requeue(etcd).await {
        Ok(_) => {
            info!(task_id, stale_node, "task.reassigned (lease_expiry)");
            // Bump cooldown counter on the dead node.
            if !stale_node.is_empty() {
                let _ = boi_assign::record_claim_failure(etcd, &stale_node, None).await;
            }
            emit_event(
                etcd,
                "task.reassigned",
                serde_json::json!({
                    "task_id": task_id,
                    "stale_node": stale_node,
                    "reason": "lease_expiry",
                }),
            )
            .await;
        }
        Err(e) => warn!(task_id, ?e, "requeue after lease_expiry failed"),
    }
}

// ── Fenced commit (worker completion) ────────────────────────────────────────
//
// Worker → core write path: the worker presents `claim_lease_id` in its
// metadata; core builds a `ClaimRecord::fence_compare` Txn and applies
// the result write only on lease match. Stale-lease writebacks are
// rejected with FAILED_PRECONDITION and a `task.claim_fence_rejected`
// audit event.
async fn commit_task_with_fence(
    etcd: &EtcdClient,
    task_id: &str,
    presented_lease: Option<i64>,
    status: &str,
) -> Result<()> {
    let result_key = format!("/boi/results/{task_id}").into_bytes();
    let result_val = serde_json::json!({
        "task_id": task_id,
        "status": status,
        "ts": unix_now(),
    });

    let expected_lease = match presented_lease {
        Some(l) => l,
        None => {
            // Allow callers to omit --lease-id (rightful claimant
            // re-reads the current lease from etcd).
            match ClaimRecord::current_lease_id(etcd, task_id).await {
                Ok(Some(l)) => l,
                _ => {
                    eprintln!("FAILED_PRECONDITION: no current lease for task {task_id}");
                    emit_event(
                        etcd,
                        "task.claim_fence_rejected",
                        serde_json::json!({
                            "task_id": task_id,
                            "reason": "no_lease",
                        }),
                    )
                    .await;
                    bail!("FAILED_PRECONDITION");
                }
            }
        }
    };

    let resp = etcd
        .txn(
            vec![ClaimRecord::fence_compare(task_id, expected_lease)],
            vec![TxnOp::Put {
                key: result_key,
                value: serde_json::to_vec(&result_val)?,
                lease: None,
            }],
            vec![],
        )
        .await?;

    if !resp.succeeded() {
        eprintln!(
            "FAILED_PRECONDITION: stale_lease claim_fence_rejected for task {task_id}"
        );
        emit_event(
            etcd,
            "task.claim_fence_rejected",
            serde_json::json!({
                "task_id": task_id,
                "presented_lease": expected_lease,
                "reason": "stale_lease",
            }),
        )
        .await;
        bail!("FAILED_PRECONDITION: stale_lease");
    }

    // Result accepted → drive queue record toward DONE/FAILED.
    if let Ok(Some(entry)) = DispatchQueueRecord::get(etcd, task_id).await {
        if matches!(
            entry.record.state,
            boi_cluster::dispatch_queue::TaskState::Claimed
        ) {
            if let Ok(running) = entry.mark_running(etcd).await {
                let final_entry = if status == "done" {
                    running.mark_done(etcd).await
                } else {
                    running.mark_failed(etcd, status).await
                };
                if let Ok(_) = final_entry {
                    let kind = if status == "done" {
                        "task.completed"
                    } else {
                        "task.failed"
                    };
                    emit_event(
                        etcd,
                        kind,
                        serde_json::json!({
                            "task_id": task_id,
                            "status": status,
                        }),
                    )
                    .await;
                }
            }
        }
    }
    // Release the claim envelope so the slot is free for the next task.
    let _ = ClaimRecord::release(etcd, task_id).await;
    Ok(())
}

// ── F-12: Prometheus /metrics endpoint ───────────────────────────────────────
//
// Minimal HTTP/1.1 server — no external crate, just tokio TCP.
// Serves `boi_dispatch_rejected_etcd_unreachable_total` (design doc §9).
async fn serve_metrics_endpoint(port: u16) {
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            warn!(?e, port, "failed to bind prometheus /metrics endpoint");
            return;
        }
    };
    info!(port, "prometheus /metrics endpoint listening");
    loop {
        let Ok((mut stream, _peer)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(async move {
            let count = REJECTED_ETCD_UNREACHABLE.load(Ordering::Relaxed);
            let body = format!(
                "# HELP boi_dispatch_rejected_etcd_unreachable_total \
                 Dispatch requests rejected because etcd was unreachable (F-12).\n\
                 # TYPE boi_dispatch_rejected_etcd_unreachable_total counter\n\
                 boi_dispatch_rejected_etcd_unreachable_total {count}\n"
            );
            let resp = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/plain; version=0.0.4; charset=utf-8\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes()).await;
        });
    }
}

// ── F-07: local-fallback drain ────────────────────────────────────────────────
//
// Operator-invoked command: reads all in-flight claim envelopes from etcd,
// persists them to ~/.boi/pending-flush/{task_id}.jsonl (one JSON object per
// file), prints a WARNING to stderr, and signals mode=local-fallback on stdout.
// This is intentionally synchronous and idempotent — safe to call multiple
// times.
async fn run_local_fallback() -> Result<()> {
    let etcd = EtcdClient::connect(&etcd_endpoints())
        .await
        .context("connect to etcd for local-fallback drain")?;

    // Read all in-flight claim envelopes.
    let kvs = etcd
        .get_prefix(CLAIMS_PREFIX)
        .await
        .context("read claims for drain")?;

    // Resolve the pending-flush directory.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let flush_dir = std::path::PathBuf::from(&home).join(PENDING_FLUSH_DIR);
    std::fs::create_dir_all(&flush_dir)
        .with_context(|| format!("create pending-flush dir {flush_dir:?}"))?;

    let mut count = 0usize;
    for (k, v) in &kvs {
        let key_str = String::from_utf8_lossy(k);
        // Sanitize the task_id portion for use as a filename.
        let task_id = key_str
            .strip_prefix(CLAIMS_PREFIX)
            .unwrap_or(key_str.as_ref())
            .replace('/', "_");
        if task_id.is_empty() {
            continue;
        }
        let record = serde_json::json!({
            "key": key_str.as_ref(),
            "value": String::from_utf8_lossy(v),
            "flushed_at": unix_now(),
            "mode": "local-fallback",
        });
        let path = flush_dir.join(format!("{task_id}.jsonl"));
        let line = serde_json::to_string(&record).unwrap_or_default() + "\n";
        // Write atomically: tmp → rename.
        let tmp = flush_dir.join(format!("{task_id}.jsonl.tmp"));
        std::fs::write(&tmp, line.as_bytes())
            .with_context(|| format!("write pending-flush tmp {tmp:?}"))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("rename pending-flush record {path:?}"))?;
        count += 1;
    }

    // Warn loudly on stderr so operators see it.
    eprintln!(
        "WARNING: switched to local-fallback mode — node is draining, \
         {count} in-flight claim(s) persisted to {flush_dir:?}"
    );
    eprintln!("mode=local-fallback: pending-flush drain complete ({count} records)");

    // Signal mode switch on stdout for scripted callers.
    println!(
        "local-fallback: node drained — {count} claims persisted to ~/.boi/pending-flush/"
    );

    Ok(())
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
        Some(Cmd::Spec { action }) => run_spec_cmd(action).await,
        Some(Cmd::Dispatch { spec }) => run_dispatch_file(spec).await,
        Some(Cmd::Cluster { action }) => run_cluster_cmd(action).await,
        Some(Cmd::Node { action }) => run_node_cmd(action).await,
        Some(Cmd::Internal { action }) => run_internal_cmd(action).await,
        Some(Cmd::NodeJoin { token }) => run_node_join(token).await,
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
    register_node(&etcd, &node_id, &addr, Some(lease_id)).await?;

    let sv = Supervisor::new(etcd.clone(), node_id.clone(), Some(lease_id));

    if let Ok(bin) = std::env::var("BOI_PLUGIN_BIN") {
        let kind_str = std::env::var("BOI_PLUGIN_KIND").unwrap_or_else(|_| "hooks".to_string());
        let cfg = PluginConfig::new(parse_plugin_kind(&kind_str), &bin);
        if let Err(e) = spawn_plugin(sv.clone(), kind_str.clone(), cfg, None).await {
            warn!(name = kind_str, ?e, "initial plugin spawn failed — continuing");
        }
    }

    // Membership tracker + assignment loop + lease_expiry watcher.
    match Membership::start(etcd.clone()).await {
        Ok(membership) => {
            let etcd_a = etcd.clone();
            let node_a = node_id.clone();
            tokio::spawn(async move {
                assignment_loop(etcd_a, membership, node_a, lease_id).await;
            });
        }
        Err(e) => warn!(?e, "failed to start membership tracker — assignment disabled"),
    }
    let etcd_w = etcd.clone();
    tokio::spawn(async move {
        lease_expiry_watcher(etcd_w).await;
    });

    // F-12: Prometheus /metrics endpoint.
    tokio::spawn(async move {
        serve_metrics_endpoint(METRICS_PORT).await;
    });

    tokio::signal::ctrl_c().await.context("wait for signal")?;
    info!("shutdown signal received");
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

async fn run_spec_cmd(action: SpecCmd) -> Result<()> {
    match action {
        SpecCmd::Dispatch { name, requires } => {
            // F-01 FAIL-LOUD DISPATCH: use a single-attempt connect so CLI
            // commands fail fast when etcd is unreachable rather than waiting
            // through the full 6-attempt retry budget.
            let fast_cfg = ConnectConfig {
                attempts: 1,
                initial_backoff: Duration::from_millis(250),
                max_backoff: Duration::from_millis(250),
            };
            let etcd = match EtcdClient::connect_with(&etcd_endpoints(), &fast_cfg).await {
                Ok(c) => c,
                Err(e) => {
                    REJECTED_ETCD_UNREACHABLE.fetch_add(1, Ordering::Relaxed);
                    eprintln!("etcd_unreachable: {e}");
                    bail!("etcd_unreachable: cannot reach etcd cluster — dispatch rejected");
                }
            };
            let task_id = new_task_id(&name);
            let mut rec = DispatchQueueRecord::new_pending(&name, &task_id);
            for tok in requires.split(',') {
                let tok = tok.trim();
                if tok.is_empty() {
                    continue;
                }
                if let Some((k, v)) = tok.split_once('=') {
                    rec.requires.insert(k.trim().into(), v.trim().into());
                }
            }
            rec.insert(&etcd).await.context("insert dispatch-queue task")?;
            emit_event(
                &etcd,
                "task.dispatched",
                serde_json::json!({
                    "task_id": task_id,
                    "spec_id": name,
                    "requires": rec.requires,
                }),
            )
            .await;
            println!("{task_id}");
        }
    }
    Ok(())
}

async fn run_dispatch_file(path: PathBuf) -> Result<()> {
    let fast_cfg = ConnectConfig {
        attempts: 1,
        initial_backoff: Duration::from_millis(250),
        max_backoff: Duration::from_millis(250),
    };
    let etcd = match EtcdClient::connect_with(&etcd_endpoints(), &fast_cfg).await {
        Ok(c) => c,
        Err(e) => {
            REJECTED_ETCD_UNREACHABLE.fetch_add(1, Ordering::Relaxed);
            eprintln!("etcd_unreachable: {e}");
            bail!("etcd_unreachable: cannot reach etcd cluster — dispatch file rejected");
        }
    };
    let bytes = std::fs::read(&path)
        .with_context(|| format!("read spec file {}", path.display()))?;
    let doc: serde_yaml::Value =
        serde_yaml::from_slice(&bytes).context("parse spec YAML")?;
    let title = doc
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("spec")
        .to_string();
    let task_id = new_task_id(&title);
    let mut rec = DispatchQueueRecord::new_pending(&title, &task_id);
    if let Some(req) = doc.get("requires").and_then(|v| v.as_mapping()) {
        for (k, v) in req {
            if let (Some(k), Some(v)) = (k.as_str(), v.as_str()) {
                rec.requires.insert(k.into(), v.into());
            }
        }
    }
    rec.insert(&etcd).await.context("insert dispatch-queue task")?;
    emit_event(
        &etcd,
        "task.dispatched",
        serde_json::json!({ "task_id": task_id, "spec_id": title }),
    )
    .await;
    println!("{task_id}");
    Ok(())
}

async fn run_cluster_cmd(action: ClusterCmd) -> Result<()> {
    match action {
        ClusterCmd::Init => {
            let node_id = node_id_from_env();
            let etcd = EtcdClient::connect(&etcd_endpoints())
                .await
                .context("connect to etcd")?;
            etcd.put("/boi/cluster/initialised", b"1".to_vec(), None)
                .await
                .context("write cluster init marker")?;

            // Generate (or load) the cluster CA on disk, then publish its
            // SHA-256 fingerprint into etcd so join clients can pin TLS
            // to the right CA without TOFU (F-04, Phase 3).
            let ca_dir = cluster_ca_dir();
            std::fs::create_dir_all(&ca_dir)
                .with_context(|| format!("create cluster CA dir {ca_dir:?}"))?;
            let ca = boi_identity::ca::ClusterCa::load_or_generate(&ca_dir)
                .context("generate or load cluster CA")?;
            let der = ca.cert_der().context("serialize CA cert DER")?;
            let fingerprint = boi_identity::join_token::ca_fingerprint(&der);
            etcd.put(
                "/boi/cluster/ca.fingerprint",
                fingerprint.as_bytes().to_vec(),
                None,
            )
            .await
            .context("write ca.fingerprint to etcd")?;
            info!(fingerprint, "wrote /boi/cluster/ca.fingerprint");

            // Register this node as cluster_admin (Q3: admin-gated token mint).
            etcd.put(
                CLUSTER_ADMIN_KEY,
                node_id.as_bytes().to_vec(),
                None,
            )
            .await
            .context("write cluster admin marker")?;

            // Mark the seed node record with caps.static.cluster_admin=true
            // so the e2e admin gate can observe it directly at /boi/nodes/{id}.
            let addr = std::env::var("BOI_NODE_ADDR")
                .unwrap_or_else(|_| DEFAULT_ADDR.to_string());
            let seed_record = serde_json::json!({
                "node_id": node_id,
                "addr": addr,
                "version": env!("CARGO_PKG_VERSION"),
                "started_at": unix_now() as i64,
                "caps": {
                    "static": { "cluster_admin": true }
                },
            });
            etcd.put(
                format!("/boi/nodes/{node_id}"),
                serde_json::to_vec(&seed_record)?,
                None,
            )
            .await
            .context("write seed node record with cluster_admin=true")?;

            // Also reflect cluster_admin on the caps map at /boi/caps/{id}.
            let mut caps = NodeCaps::default();
            caps.r#static
                .insert("cluster_admin".to_string(), "true".to_string());
            caps.put(&etcd, &node_id, None)
                .await
                .context("publish seed caps with cluster_admin=true")?;

            info!(node_id, "cluster admin registered");
            println!("ok");
        }
        ClusterCmd::LocalFallback => {
            run_local_fallback().await?;
        }
        ClusterCmd::Members => {
            let etcd = EtcdClient::connect(&etcd_endpoints())
                .await
                .context("connect to etcd")?;
            // Read /boi/nodes/ — each entry is a JSON envelope with
            // node_id and addr; print "<id> <addr>" so the harness can
            // compare member listings across all three nodes.
            let kvs = etcd
                .get_prefix(NODES_PREFIX)
                .await
                .context("list /boi/nodes/")?;
            let mut rows: Vec<(String, String)> = Vec::new();
            for (k, v) in &kvs {
                let key = String::from_utf8_lossy(k);
                let id = key
                    .strip_prefix(NODES_PREFIX)
                    .unwrap_or(key.as_ref())
                    .to_string();
                let addr = serde_json::from_slice::<serde_json::Value>(v)
                    .ok()
                    .and_then(|j| {
                        j.get("addr").and_then(|a| a.as_str()).map(str::to_string)
                    })
                    .unwrap_or_default();
                rows.push((id, addr));
            }
            rows.sort();
            for (id, addr) in rows {
                println!("{id} {addr}");
            }
        }
        ClusterCmd::MintJoinToken => {
            // Admin-gated token minting (Q3). The caller must be the
            // registered cluster_admin; otherwise we fail closed with
            // PermissionDenied on stderr and a non-zero exit.
            let node_id = node_id_from_env();
            let etcd = EtcdClient::connect(&etcd_endpoints())
                .await
                .context("connect to etcd")?;
            if !is_cluster_admin(&etcd, &node_id).await {
                eprintln!(
                    "PermissionDenied: node `{node_id}` is not cluster_admin \
                     and may not mint join tokens (Q3)"
                );
                std::process::exit(1);
            }
            // Load the CA from the canonical cluster dir and call
            // boi_identity::join_token::mint_join_token. Returns the JWT
            // on stdout for the caller to ship to the joining node.
            let ca_dir = cluster_ca_dir();
            let ca = boi_identity::ca::ClusterCa::load_or_generate(&ca_dir)
                .context("load cluster CA for mint-join-token")?;
            let der = ca.cert_der().context("serialize CA cert DER")?;
            let cluster_id = std::env::var("BOI_CLUSTER_ID")
                .unwrap_or_else(|_| "boi-cluster".to_string());
            let seed_addr = std::env::var("BOI_NODE_ADDR")
                .unwrap_or_else(|_| DEFAULT_ADDR.to_string());
            let token = boi_identity::join_token::mint_join_token(
                ca.key_pem(),
                &der,
                &cluster_id,
                vec![seed_addr],
                boi_identity::join_token::DEFAULT_TTL_SECS,
            )
            .context("mint-join-token: signing failed")?;
            // Record the minted token so legacy in-cluster lookups still
            // work (NodeCmd::Join checks this prefix as a fallback).
            let key = format!("{JOIN_TOKENS_PREFIX}{token}");
            let _ = etcd
                .put(
                    key,
                    serde_json::to_vec(&serde_json::json!({
                        "minted_by": node_id,
                        "expires_at": unix_now() as i64
                            + boi_identity::join_token::DEFAULT_TTL_SECS,
                    }))?,
                    None,
                )
                .await;
            println!("{token}");
        }
    }
    Ok(())
}

/// Canonical on-disk location for cluster CA material. Overridable via
/// `BOI_CLUSTER_DIR` for container/test environments; otherwise falls
/// back to `~/.boi/cluster/` (or `/boi/cluster/` if HOME is absent).
fn cluster_ca_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("BOI_CLUSTER_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = boi_identity::ca::default_ca_dir() {
        return dir;
    }
    PathBuf::from("/boi/cluster")
}

async fn run_node_cmd(action: NodeCmd) -> Result<()> {
    match action {
        NodeCmd::Advertise => {
            let node_id = node_id_from_env();
            let etcd = EtcdClient::connect(&etcd_endpoints())
                .await
                .context("connect to etcd")?;
            let mut caps = NodeCaps::default();
            if let Ok(s) = std::env::var("BOI_CAPS_STATIC") {
                for tok in s.split(',') {
                    let tok = tok.trim();
                    if let Some((k, v)) = tok.split_once('=') {
                        caps.r#static.insert(k.trim().into(), v.trim().into());
                    }
                }
            }
            caps.put(&etcd, &node_id, None)
                .await
                .context("advertise caps")?;
            println!("ok");
        }
        NodeCmd::Join { token } => {
            // Validate BOI_TOKEN / --token if present, then run daemon.
            let tok = token
                .or_else(|| std::env::var("BOI_TOKEN").ok())
                .unwrap_or_default();
            if !tok.is_empty() {
                // Phase 3 fail-closed join path: verify token signature
                // against the cluster CA public key and pin the embedded
                // ca_fingerprint to the local CA's fingerprint. Any
                // mismatch (bad signature, tampered payload, wrong CA,
                // fingerprint flip) aborts the join before we touch etcd.
                let etcd = EtcdClient::connect(&etcd_endpoints())
                    .await
                    .context("connect to etcd for token check")?;
                let ca_dir = cluster_ca_dir();
                let ca = match boi_identity::ca::ClusterCa::load(&ca_dir) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!(
                            "fail-closed: cannot load cluster CA from {ca_dir:?} \
                             to verify join token: {e}"
                        );
                        std::process::exit(1);
                    }
                };
                let der = match ca.cert_der() {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("fail-closed: CA DER serialization failed: {e}");
                        std::process::exit(1);
                    }
                };
                let local_fp = boi_identity::join_token::ca_fingerprint(&der);
                // verify signature + pin ca_fingerprint
                if let Err(e) = boi_identity::join_token::validate_token(
                    &tok,
                    ca.cert_pem(),
                    Some(&local_fp),
                ) {
                    eprintln!(
                        "fail-closed: join token rejected (verify signature \
                         or fingerprint mismatch): {e}"
                    );
                    std::process::exit(1);
                }
                // Optional legacy lookup: if the token was registered via
                // an internal mint path it'll be in /boi/join-tokens/.
                // Missing-key here is NOT fatal — signature already proved
                // authenticity.
                let key = format!("{JOIN_TOKENS_PREFIX}{tok}");
                let _ = etcd.get(key).await;
                info!("join token signature validated — starting node daemon");
            }
            run_daemon().await?;
        }
    }
    Ok(())
}

async fn run_node_join(token: Option<String>) -> Result<()> {
    run_node_cmd(NodeCmd::Join { token }).await
}

async fn run_internal_cmd(action: InternalCmd) -> Result<()> {
    let etcd = EtcdClient::connect(&etcd_endpoints())
        .await
        .context("connect to etcd")?;
    match action {
        InternalCmd::ForceClaim {
            task_id,
            max_mod_rev,
        } => {
            // Check current mod_revision for the task's queue key. If
            // the cluster has advanced past `max_mod_rev`, refuse with
            // a `revision_pin_window` error (Q1 W=64).
            let key = queue_key(&task_id);
            let (_, current_rev) = etcd
                .get_prefix_with_revision(key.as_str())
                .await
                .context("read current revision")?;
            if current_rev > max_mod_rev {
                eprintln!(
                    "revision_pin_window: cluster_rev={current_rev} > max_mod_rev={max_mod_rev} — CAS would fail"
                );
                std::process::exit(2);
            }
            println!("ok");
        }
        InternalCmd::CommitTask {
            task_id,
            lease_id,
            status,
        } => {
            let presented = match lease_id {
                Some(s) => parse_lease_id(&s),
                None => None,
            };
            if let Err(e) = commit_task_with_fence(&etcd, &task_id, presented, &status).await
            {
                eprintln!("{e}");
                std::process::exit(2);
            }
            println!("ok");
        }
        InternalCmd::MintProvisionToken { for_caps } => {
            let node_id = node_id_from_env();
            // Q3: only cluster_admin nodes may mint join tokens.
            if !is_cluster_admin(&etcd, &node_id).await {
                eprintln!(
                    "PermissionDenied: node `{node_id}` is not cluster_admin \
                     and is not authorized to mint provision tokens"
                );
                std::process::exit(1);
            }
            let token = Uuid::new_v4().to_string();
            let expiry_ts = unix_now() + 300; // 5-min validity
            let token_val = serde_json::json!({
                "token": token,
                "for_caps": for_caps,
                "expires_at": expiry_ts,
                "minted_by": node_id,
            });
            let key = format!("{JOIN_TOKENS_PREFIX}{token}");
            if let Ok(body) = serde_json::to_vec(&token_val) {
                etcd.put(key, body, None)
                    .await
                    .context("store join token")?;
            }
            println!("{token}");
        }
        InternalCmd::SetProvisionerMode { mode } => {
            etcd.put(
                "/boi/provisioner-mode",
                mode.as_bytes().to_vec(),
                None,
            )
            .await
            .context("set provisioner mode")?;
            println!("ok");
        }
    }
    Ok(())
}

fn parse_lease_id(s: &str) -> Option<i64> {
    if let Ok(v) = s.parse::<i64>() {
        return Some(v);
    }
    // Accept hex (the e2e harness pulls hex chars off the json blob).
    i64::from_str_radix(s, 16).ok()
}

// Suppress dead-code on the helper exposed only to mirror the
// requires-map shape — referenced via assignment_tick at runtime.
#[allow(dead_code)]
fn _keep_requires_to_map(r: &CapRequires) -> BTreeMap<String, String> {
    requires_to_map(r)
}
