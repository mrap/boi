# BOI Distributed Architecture — Alpha Team

**Non-negotiable constraint:** All task assignment decisions must be made by a
single elected leader node using Raft consensus. No node may assign a task
without authorization from the current leader.

---

## 1. Cluster State Model

All cluster state is stored in a Raft log replicated across all BOI nodes.
The Raft leader is the only node that may write state; followers serve reads
from their locally applied log.

**State in the log:**

| Key | Value | Who writes |
|-----|-------|-----------|
| `node/{id}/caps` | Capability advertisement (static + dynamic) | Leader (forwarded from follower) |
| `node/{id}/heartbeat` | Timestamp + health | Leader (forwarded from follower) |
| `task/{id}/status` | `queued → assigned → running → done/failed` | Leader |
| `task/{id}/assignee` | Node ID | Leader |
| `provisioner/inflight` | Provisioner call state | Leader |

No state lives outside the Raft log. SQLite on each node is a materialized
read cache of the applied log. Writes to SQLite happen inside the log-apply
callback, so the read cache is always at-most one log-index behind.

**Consistency:** Linearizable writes (Raft), read-your-writes from leader.
Followers may serve stale reads by up to one apply-cycle. Task assignment reads
always go through the leader to avoid stale capability data.

---

## 2. Node Lifecycle

### Discovery and join

A new node starts with a `--join <seed-addr>` flag. It sends a `JoinRequest`
gRPC call to the seed, which forwards it to the current leader. The leader
appends a `NodeJoin` entry to the log. Once that entry is applied across a
quorum, the new node is part of the Raft group and begins receiving log
replication.

```
new-node ──JoinRequest──► seed-node ──forward──► leader
leader appends NodeJoin to Raft log
quorum applies → new-node receives future log entries
new-node advertises caps via CapabilityHeartbeat RPC (every 5s)
```

### Leave

A node sends a `LeaveRequest` (graceful drain). The leader appends `NodeLeave`.
Any tasks assigned to that node that are not yet `running` are returned to
`queued` and re-assigned.

### Failure detection

Each node sends a heartbeat to the leader every 5 seconds. If the leader has
not received a heartbeat for 15 seconds, it appends `NodeSuspect`. At 30
seconds without recovery, it appends `NodeFailed` and reschedules any tasks
whose assignee is the failed node.

If the **leader** fails, Raft elects a new leader. During the election window
(typically <500 ms), no new assignments are made. Queued tasks wait; running
tasks continue running and self-report completion.

---

## 3. Task Assignment Algorithm

Assignment happens entirely on the leader in a single-threaded dispatcher loop.

```
fn assign_next_task():
    tasks = read_from_log_cache(status = queued, order_by = queued_at ASC)
    for task in tasks:
        candidates = [
            node for node in cluster_nodes
            if node.status == healthy
            and node.caps.satisfies(task.requires)
            and node.workers_busy < node.workers_max
        ]
        if candidates.empty():
            maybe_provision(task)
            continue
        # Deterministic selection: consistent hash of (task.id, cluster_epoch)
        chosen = candidates[hash(task.id + cluster_epoch) % len(candidates)]
        leader_append_log(TaskAssigned { task_id, node_id: chosen.id, epoch })
        break  # one assignment per loop tick to keep log writes serialized
```

**Determinism argument:** The leader is the only node that runs this loop.
`cluster_epoch` increments every time membership changes (NodeJoin or
NodeFailed log entries). For any fixed `(task.id, cluster_epoch)` the
candidate list is deterministic (Raft log is total order), and the hash
function is stable. Therefore the same task + same cluster view → same target.
No race is possible because a second leader cannot exist in the same term.

### Assignment log entry

```toml
[TaskAssigned]
task_id   = "T-abc123"
node_id   = "node-7"
term      = 4
epoch     = 22
timestamp = 1747065600
```

A task is considered assigned only after this entry is committed (quorum
acknowledgment). The assignee node polls the log for entries where
`node_id == self.id` and picks up its work.

---

## 4. Provisioning Flow

```
1. assign_next_task() finds no capable node → calls maybe_provision(task)
2. Leader checks provisioner_inflight for this capability set.
   If already provisioning → wait (don't double-provision).
3. Leader appends ProvisionerStarted to log.
4. Leader calls Provisioner plugin gRPC: ProvisionNode { caps: task.requires }
5. Provisioner allocates infra, starts new BOI process with --join <leader-addr>
6. New node sends JoinRequest → leader appends NodeJoin → quorum applies.
7. New node sends first CapabilityHeartbeat.
8. Leader's assign loop now sees the node as a candidate and assigns the task.
9. Leader appends ProvisionerCompleted.
```

Timeout: if new node does not join within 90 seconds, leader appends
`ProvisionerFailed` and the task returns to `queued` for a retry (with
exponential back-off on the Provisioner call).

Double-provisioning is prevented by the `provisioner_inflight` log check: the
leader holds a per-capability-set lock inside the Raft log itself, not in
in-process memory, so a leader failover does not lose the lock.

---

## 5. Failure Modes

| Scenario | Detection | Recovery | TTR | Worst case |
|----------|-----------|----------|-----|-----------|
| Leader crashes mid-assignment | Raft election (≤500 ms) | New leader reads log; uncommitted TaskAssigned is rolled back; task stays queued | ≤1 s | Task delayed by election window |
| Network partition splits cluster | Leader in minority loses quorum; stops writing | Majority partition elects new leader; tasks re-assigned | ≤30 s | Tasks in minority partition stall |
| Provisioner returns success, node never joins | 90 s join timeout | ProvisionerFailed logged; task re-queued; Provisioner called again | 90 s | Task delayed by 90 s per attempt |
| Node advertises capability it can't run | Task assigned; node returns RunError | RunError logged; task re-queued; node's cap entry patched via CapUpdate RPC | Depends on task timeout | Task fails once, then re-assignment |
| Long-running task outlives its node | Node heartbeat timeout (30 s) → NodeFailed | Task is in `running` state; leader appends TaskOrphaned; task re-queued | 30 s + task restart | Duplicate execution if node survives partition |
| Clock skew > 5 s | Heartbeat timestamp drift | mTLS cert validation requires clocks within 60 s; flag and alert only | N/A | False-positive suspect if >30 s skew causes missed heartbeats |
| Pool plugin daemon crashes | Worker returns error; node marks slot free | Plugin daemon restarted by BOI core supervisor (systemd/launchd); slot freed | Seconds | In-flight worker is orphaned |
| Raft log store (SQLite) corrupted on follower | Snapshot replay fails | Node wipes state, re-joins, receives leader snapshot | Minutes | Node temporarily absent from pool |

---

## 6. Plugin Integration Points

Plugins are gRPC sidecars (HashiCorp go-plugin style), started by BOI core
and communicated with over a local Unix socket. mTLS is used between BOI nodes;
plugin–core communication is local-socket only (no mTLS needed).

**Plugin types and gRPC services:**

```
WorkspacePlugin   — SetupWorkspace(task) → WorkspaceHandle
                    TeardownWorkspace(handle)

PoolPlugin        — StartWorker(task, workspace) → WorkerHandle
                    StopWorker(handle)
                    WorkerStatus(handle) → Status

RouterPlugin      — (optional override) SelectNode(task, candidates) → NodeID

ProvisionerPlugin — ProvisionNode(caps) → ProvisionHandle
                    DeprovisionNode(handle)

HooksPlugin       — OnTaskQueued(task)
                    OnTaskAssigned(task, node)
                    OnTaskCompleted(task, result)
```

BOI core provides a `PluginHost` module that manages plugin daemon lifecycle,
reconnects on crash, and enforces the gRPC contract (version handshake on
startup). If a plugin daemon crashes, `PluginHost` restarts it with
exponential back-off and notifies the relevant subsystem.

---

## 7. BOI Core Modules

**New modules required:**

| Module | Responsibility |
|--------|---------------|
| `raft/` | Raft consensus (uses `openraft` crate), log, snapshot |
| `cluster/` | Node registry, heartbeat sender/receiver, epoch tracking |
| `scheduler/` | Leader-only assign loop, provisioner gating |
| `plugin_host/` | Plugin lifecycle, gRPC client factory, crash recovery |
| `capability/` | Cap advertisement types, satisfaction predicate |
| `provisioner/` | ProvisionerPlugin client, inflight tracking |

**Retained from existing BOI:**

- `phases/` — phase execution unchanged
- `sqlite/` — now used as read cache for Raft-applied state
- `workspace/` — now delegated to WorkspacePlugin
- `worker_pool/` — now delegated to PoolPlugin

**CLI surface additions:**

```
boi cluster status          # show all nodes, their caps, health
boi cluster join <addr>     # join an existing cluster
boi scheduler pause/resume  # operator: pause assignment (e.g. maintenance)
boi plugin list             # show registered plugins and their status
```

---

## Self-Review

**Weakest assumption:** The Raft leader will be available and responsive.
In practice, leader elections under heavy load or flaky networking can take
1–3 seconds and during that window the entire assignment pipeline stalls.
This is acceptable for BOI's use case (task latency in seconds is fine) but
becomes a problem if the cluster is large and elections happen frequently.
There is no "read from followers" escape hatch for the critical assignment
path.

**Biggest risk:** The single-threaded assign loop is a bottleneck. At high
task throughput (hundreds of tasks per second across a large cluster), the
leader serializes every assignment. We have not benchmarked this; we believe
BOI's actual workload is tens of tasks per minute, which makes this
irrelevant — but the assumption could be wrong.

**Simpler alternative considered:** Use a gossip protocol (no elected leader)
with CRDT-based task state. Rejected because CRDT semantics make
"no double-execution" very hard to guarantee: merging concurrent
`TaskAssigned` writes from two nodes in a network partition requires
careful tombstoning and the correctness argument is subtle. Raft's total
order gives us the correctness proof for free.

**With 2× budget:** Replace the single-threaded assign loop with a
multi-leader sharding scheme: shard tasks by `task.id % num_leaders`,
each shard gets its own Raft group. This removes the throughput bottleneck.

**With ½ budget:** Drop on-demand provisioning entirely. Require cluster to be
pre-configured with enough nodes. The assign loop becomes a simple
"first capable node" scan. The system is still distributed and correct;
it just can't grow itself.
