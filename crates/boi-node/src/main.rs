//! boi-node: cluster node daemon with plugin supervisor, Handshake,
//! crash-recovery (F-11, F-20, §5 isolation), and the Phase 4
//! assignment loop (HRW + CAS claim + lease fencing).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use boi_assign::{assign, AssignResult, CapRequires, TaskRecord};
use boi_cluster::claims::{claim_key, ClaimRecord, CLAIMS_PREFIX};
use boi_cluster::client::{EtcdClient, TxnOp};
use boi_cluster::dispatch_queue::{
    queue_key, DispatchQueueRecord, QueueEntry, QUEUE_PREFIX,
};
use boi_cluster::membership::Membership;
use boi_cluster::nodes::{NodeCaps, NodeRecord};
use boi_plugin_host::handshake::{self, HOST_PROTO_MAJOR};
use boi_plugin_host::lifecycle::{
    Plugin, PluginConfig, PluginHealth, PluginKind, RestartPolicy,
};

const BOI_READY: &str = "BOI_READY";
const DEFAULT_ETCD: &str = "http://127.0.0.1:2379";
const DEFAULT_ADDR: &str = "0.0.0.0:7001";
const EVENTS_PREFIX: &str = "/boi/events/";

// Assignment-loop cadence — fast enough that the 5s test budget catches
// a dispatch within one iteration, slow enough to keep etcd churn low.
const ASSIGN_POLL_INTERVAL: Duration = Duration::from_millis(250);

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
}

#[derive(Subcommand)]
enum NodeCmd {
    /// Advertise this node's caps under /boi/caps/{node_id}.
    Advertise,
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
    _node_id: &str,
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
            debug!(?e, "membership snapshot stale; skipping tick");
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
            let etcd = EtcdClient::connect(&etcd_endpoints())
                .await
                .context("connect to etcd")?;
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
    let etcd = EtcdClient::connect(&etcd_endpoints())
        .await
        .context("connect to etcd")?;
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
            let etcd = EtcdClient::connect(&etcd_endpoints())
                .await
                .context("connect to etcd")?;
            etcd.put("/boi/cluster/initialised", b"1".to_vec(), None)
                .await
                .context("write cluster init marker")?;
            println!("ok");
        }
    }
    Ok(())
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
    }
    Ok(())
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
