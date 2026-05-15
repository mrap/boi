# BOI Distributed Architecture — Charlie Team

**Non-negotiable constraint:** An external etcd cluster is the single source
of truth for all coordination state. BOI nodes are stateless agents that read
from and write to etcd. No BOI-specific consensus protocol is implemented.

---

## 1. Cluster State Model

All coordination state lives in etcd. BOI nodes hold no durable state; they
are stateless workers that derive everything from etcd. Each node's local
SQLite is a write-through cache of etcd data for the node's own tasks only.

**etcd key schema:**

```
/boi/nodes/{node-id}/caps          → JSON: {os, arch, runtime, region, ...}
/boi/nodes/{node-id}/dynamic       → JSON: {workers_busy, workers_max, health}
/boi/nodes/{node-id}/heartbeat     → Lease-backed key; expires if node dies
/boi/tasks/{task-id}/status        → Enum: queued|assigned|running|done|failed
/boi/tasks/{task-id}/assignee      → node-id string
/boi/tasks/{task-id}/owner-lease   → Lease ID; held by the assigning node
/boi/provisioner/{caps-hash}/lock  → Lease-backed; held by provisioner caller
/boi/epoch                         → Monotonic counter; incremented on membership change
```

**Consistency:** etcd provides linearizable reads and writes. All BOI state
operations use etcd transactions (`txn`) with preconditions to implement
optimistic locking. No BOI node can produce a stale view when using linearizable
reads (`--consistency=l`).

**Epoch:** The `/boi/epoch` key is incremented by any node that detects a
membership change (node join, node failure). The epoch is used as a component
in the deterministic assignment hash. Because etcd guarantees linearizability,
all nodes that read the epoch at any given moment see the same value.

---

## 2. Node Lifecycle

### Discovery and join

A new node writes its caps to `/boi/nodes/{node-id}/caps` and creates an
etcd lease for `/boi/nodes/{node-id}/heartbeat` (TTL = 15 s, auto-renewed
every 5 s). The node watches `/boi/epoch` for cluster membership changes.

```
new-node writes /boi/nodes/{id}/caps to etcd
new-node creates lease L (TTL=15s)
new-node creates /boi/nodes/{id}/heartbeat with lease L
etcd auto-expires heartbeat if new-node dies (lease TTL)
new-node increments /boi/epoch via etcd txn
all watching nodes see epoch change, re-fetch membership
```

### Leave

Graceful: node deletes its heartbeat key and caps key, decrements the epoch.
Ungraceful: etcd lease expires (within 15 s); heartbeat key disappears;
watching nodes detect the epoch change and re-fetch membership.

### Failure detection

etcd leases handle it natively. When a node's lease expires, its heartbeat key
is atomically deleted by etcd. Any BOI node watching the `/boi/nodes/` prefix
receives a delete event and treats that node as failed. No BOI-level failure
detector is needed.

---

## 3. Task Assignment Algorithm

Any BOI node may attempt to assign a task. Conflicts are resolved by etcd
transactions. There is no elected leader.

```
fn try_assign_task(task_id):
    # 1. Read current epoch and membership (linearizable)
    epoch    = etcd.get("/boi/epoch")
    members  = etcd.get_prefix("/boi/nodes/", consistency=linearizable)
    
    # 2. Filter candidates
    candidates = [
        n for n in members
        if n.heartbeat.alive
        and n.caps.satisfies(task.requires)
        and n.dynamic.workers_busy < n.dynamic.workers_max
    ]
    if candidates.empty():
        trigger_provisioning(task)
        return
    
    # 3. Deterministic selection
    chosen = candidates[hash(task_id + epoch) % len(candidates)]
    
    # 4. Claim via etcd txn — only succeeds if task is still queued
    lease = etcd.grant_lease(ttl=300)  # assignment ownership lease
    success = etcd.txn(
        if:   [/boi/tasks/{task_id}/status == "queued"],
        then: [
            put /boi/tasks/{task_id}/status   = "assigned",
            put /boi/tasks/{task_id}/assignee = chosen.id,
            put /boi/tasks/{task_id}/owner-lease = lease.id,
        ]
    )
    if not success:
        # Another node won the race; task already assigned; nothing to do
        etcd.revoke_lease(lease)
        return

fn assign_loop():
    watch /boi/tasks/ for new queued tasks
    on new task: spawn try_assign_task(task_id)
```

**Determinism argument:** `hash(task_id + epoch)` is a pure function of two
stable values. Because epoch is linearizable, all concurrent `try_assign_task`
calls for the same task compute the same `chosen` node. If two nodes
simultaneously attempt the etcd `txn`, exactly one succeeds (etcd serializes
conflicting transactions). The loser's txn fails the precondition check and
returns without side effects.

---

## 4. Provisioning Flow

```
1. try_assign_task finds no capable node.
2. Node checks /boi/provisioner/{caps-hash}/lock (etcd lease-backed).
   If lock exists → another node is provisioning; wait and retry.
3. Node claims lock via etcd txn:
   txn(if: lock not exists, then: put lock = self.id with lease TTL=120s)
4. Node calls Provisioner plugin gRPC: ProvisionNode { caps: task.requires }
5. Provisioner starts new BOI node, which writes to etcd and creates heartbeat.
6. New node increments /boi/epoch.
7. All watching nodes re-fetch membership; new node appears as a candidate.
8. Lock holder releases /boi/provisioner/{caps-hash}/lock.
9. Any node's assign loop picks up the queued task and assigns it to new node.
```

Timeout: if the provisioner call takes > 90 s without the new node appearing,
the lock holder revokes the lease (and thus the lock) and writes a
`ProvisionerFailed` event to etcd. Another node may retry.

---

## 5. Failure Modes

| Scenario | Detection | Recovery | TTR | Worst case |
|----------|-----------|----------|-----|-----------|
| Assigning node crashes mid-txn | etcd reverts uncommitted txn atomically | Task remains `queued`; next assign loop iteration picks it up | ≤15 s (owner-lease TTL) | Task delayed by lease TTL if partially assigned |
| etcd cluster loses quorum | etcd returns errors to all BOI nodes | All BOI assignment halts; running tasks continue; queue frozen | Until etcd quorum restored | Full cluster halt; no task assignment |
| Network partition (BOI nodes, not etcd) | Nodes on partitioned side can't reach etcd | Assignment stalls on partitioned side; etcd-connected side continues normally | Partition duration | Tasks on disconnected side can't be assigned |
| Provisioner returns success, node never joins | Provisioner lock TTL (120 s) expires | Lock released; another node retries provisioning | 120 s | Double-provisioning if first node eventually appears |
| Node advertises capability it can't run | RunError from worker; node updates /dynamic caps via etcd | Epoch unchanged; task re-queued; assignment retries with updated caps | 1 assign loop tick | One failed execution |
| Long-running task outlives its node | etcd heartbeat lease expires (15 s) | Heartbeat key deleted; epoch incremented; watching nodes detect; task owner-lease expires → task re-queued | 15–300 s (owner-lease TTL) | Duplicate execution if original node survives partition |
| Clock skew > 5 s | etcd lease TTL drift | etcd client library warns on large skew; lease TTLs may be inaccurate; operator alert | N/A | False-expire of heartbeat lease causing node to appear dead |
| Pool plugin daemon crashes | Worker RPC fails | PluginHost restarts plugin; node updates /dynamic caps in etcd | Seconds | In-flight worker orphaned |

---

## 6. Plugin Integration Points

Same gRPC sidecar model. Charlie's key architectural difference: plugin state
that must survive the plugin daemon crashing can be written to etcd (plugins
receive an etcd client via the `PluginContext` passed at startup).

**Plugin gRPC services:**

```
WorkspacePlugin   — SetupWorkspace(task, etcd_prefix) → WorkspaceHandle
                    TeardownWorkspace(handle)

PoolPlugin        — StartWorker(task, workspace) → WorkerHandle
                    StopWorker(handle)
                    WorkerStatus(handle) → Status

RouterPlugin      — SelectNode(task, candidates) → NodeID
                    (Called after candidate filtering, before etcd txn)

ProvisionerPlugin — ProvisionNode(caps, join_addr) → ProvisionHandle
                    DeprovisionNode(handle)

HooksPlugin       — OnTaskQueued / OnTaskAssigned / OnTaskCompleted
```

**etcd_prefix for plugins:** Each plugin invocation receives a scoped etcd
prefix (`/boi/plugins/{plugin-type}/{node-id}/`) where it can store state
durably. This means plugin authors can crash and restart without losing their
state, as long as they wrote it to etcd.

**Plugin author experience:** Plugin authors need to understand etcd basics
(keys, leases, watches) if they want durable state. This is an extra
dependency on the plugin contract but gives powerful crash-safety guarantees.

---

## 7. BOI Core Modules

**New modules required:**

| Module | Responsibility |
|--------|---------------|
| `etcd_client/` | etcd gRPC client wrapper, lease management, watch streams |
| `cluster/` | Node registration, epoch tracking, membership watch |
| `scheduler/` | assign_loop, try_assign_task, etcd txn logic |
| `plugin_host/` | Plugin lifecycle, gRPC client factory, etcd context injection |
| `capability/` | Cap types, satisfaction predicate |
| `provisioner/` | ProvisionerPlugin client, etcd lock management |

**External dependencies added:**

- **etcd** — must be pre-provisioned and available before any BOI node starts
- `etcd-client` Rust crate (async gRPC to etcd v3 API)

**Retained from existing BOI:**

- `phases/` — unchanged
- `sqlite/` — local cache of this node's own task results only
- `workspace/` — delegated to WorkspacePlugin
- `worker_pool/` — delegated to PoolPlugin

**CLI surface additions:**

```
boi cluster status          # show all nodes via etcd membership
boi cluster epochs          # show recent epoch changes (debug)
boi etcd health             # check etcd cluster reachability
boi plugin list             # show registered plugins
```

---

## Self-Review

**Weakest assumption:** etcd is always available. This is a hard operational
dependency: if etcd loses quorum, the entire BOI cluster stops assigning tasks.
Running tasks continue but nothing new is dispatched. For an internal
corporate deployment, etcd must be a separate, highly-available service
(typically 3–5 node etcd cluster), which is significant infrastructure
overhead. Teams that don't already run etcd must stand it up.

**Biggest risk:** The `owner-lease` TTL (300 s by default) determines how
long a "assigned but not yet running" task sits stuck if the assigning node
dies. 300 seconds is a long time. If we shorten it, we risk expiring
legitimately slow starts. There is no good answer without observing actual
task start latencies in the environment.

**Simpler alternative considered:** Use Redis instead of etcd. Redis has leases
(via EXPIRE), atomic transactions (via MULTI/EXEC), and is operationally
simpler to run than a 3-node etcd cluster. Rejected because etcd's watch API
is strictly better for event-driven assignment loops, etcd's linearizability
guarantee is formally specified, and using a well-known distributed systems
primitive (etcd) reduces the "prove it's correct" burden versus Redis.

**With 2× budget:** Build a proper etcd operator for Kubernetes that manages
the etcd cluster, BOI node deployment, and auto-scaling of the worker pool.
The entire provisioning flow becomes a Kubernetes reconciliation loop. Much
simpler operationally for teams already on Kubernetes.

**With ½ budget:** Remove the RouterPlugin entirely. The assignment algorithm
is purely `hash(task_id + epoch) % candidates`. No plugin contract for
routing; custom routing requires forking BOI (which violates constraint 2).
This simplifies the implementation significantly but reduces flexibility.
