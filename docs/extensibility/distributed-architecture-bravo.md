# BOI Distributed Architecture — Bravo Team

**Non-negotiable constraint:** All cluster state must be fully replicated to
every node with no single point of coordination. The system must make forward
progress (assign tasks, detect failures) even if any single node is
unreachable, including the most recent "coordinator."

---

## 1. Cluster State Model

Bravo uses epidemic (gossip) broadcast with state stored as CRDTs in each
node's local SQLite. There is no leader and no external coordination service.

**CRDT types used:**

| State | CRDT type | Convergence property |
|-------|-----------|---------------------|
| Node membership | OR-Set (Observed-Remove Set) | Add-wins; tombstones prevent zombie re-adds |
| Node capabilities | LWW-Register per cap field | Last-write-wins by Hybrid Logical Clock (HLC) |
| Task status | Multi-Value Register + causal history | Conflicts exposed to operator; resolved by timestamp |
| Task assignee | LWW-Register | Last-write-wins by HLC |
| Provisioner locks | OR-Set with TTL | Lease expires; node re-adds to claim |

**Hybrid Logical Clocks (HLC):** Every node maintains an HLC (physical time +
logical counter). All writes are tagged with the writer's HLC. Gossip messages
carry the writer's HLC; receivers advance their own HLC past the received
value. This gives a causal order that is consistent with wall-clock time and
tolerates ≤5 second clock skew without false conflicts.

**Gossip protocol:** Every node selects 3 random peers every 2 seconds and
pushes its full state digest (Bloom filter of key→HLC). Peers pull missing or
newer entries. Full convergence across N nodes takes O(log N) gossip rounds.

---

## 2. Node Lifecycle

### Discovery and join

Nodes discover each other via a configurable seed list (static IPs or DNS
service discovery). On startup a node contacts any seed, receives the current
gossip digest, pulls full state for unknown keys, and starts gossiping.

```
new-node ──GossipPull──► seed-node
seed responds with full state digest
new-node pulls deltas, populates local CRDT store
new-node starts gossiping with 3 random peers every 2 s
new-node announces itself by adding to membership OR-Set
```

No leader election needed. The new node is "in" as soon as it has gossiped its
membership add to a majority of nodes (typically 4–6 gossip rounds, ~10 s).

### Leave

Graceful: node removes itself from the membership OR-Set (observe-remove).
Ungraceful: failure detected via heartbeat decay (see below).

### Failure detection

Each node maintains a SWIM-style failure detector. Each node picks a random
peer and sends a direct ping every 1 second. If no response in 500 ms, it asks
3 other nodes to indirect-ping. If all fail, the node is marked `suspect`. If
still no response after 10 seconds, the node is marked `failed` and removed
from the OR-Set. This converges across the cluster within 2 gossip rounds.

---

## 3. Task Assignment Algorithm

Because there is no leader, every node runs an identical, deterministic
assignment function over its local CRDT state. The same function produces the
same assignment as long as all nodes have converged on the same CRDT values.

```
fn compute_assignment(task, cluster_state) -> Option<NodeID>:
    # All nodes run this identically
    candidates = [
        node for node in cluster_state.members
        if node.status != failed
        and node.caps.satisfies(task.requires)
        and node.workers_busy < node.workers_max
    ]
    if candidates.empty():
        return None
    # Rendezvous (HRW) hash: deterministic, load-balancing
    scored = [(hrw_score(node.id, task.id), node.id) for node in candidates]
    return max(scored).node_id

fn hrw_score(node_id, task_id) -> u64:
    return siphash(node_id || task_id)  # stable, no global state needed
```

**Determinism argument:** HRW hash depends only on `(node.id, task.id)`, both
stable identifiers. The candidate set is derived from fully-replicated CRDT
state. As long as all nodes have the same CRDT values (converged), they
produce the same assignment. During convergence, two nodes may temporarily
compute different candidates; this is handled via the coordination protocol
below.

### Preventing double-assignment

Pure gossip with no coordinator can produce double-assignment during
convergence. Bravo solves this with **optimistic locking via a Claim CRDT:**

```
1. Node A computes assignment → NodeX for task T.
2. Node A writes Claim { task_id: T, claimer: A, hlc: A.now() } to CRDT.
3. Node A gossips the claim. Other nodes merge it.
4. If Node B also computes a claim for T:
   - Both claims enter a Multi-Value Register (MVR).
   - Conflict resolution: lowest claimer-id wins (deterministic tiebreak).
   - Losing claimer backs off and re-runs assignment after next gossip round.
5. Winning claimer sends the actual work to NodeX via direct RPC.
6. NodeX accepts only if its local CRDT shows the same claim winner.
```

This produces at-most-one successful assignment per task even during
convergence splits. The window for double-work is bounded by one gossip round
(~2 seconds) and self-corrects.

---

## 4. Provisioning Flow

```
1. All nodes detect no capable node exists for task T (from CRDT state).
2. The node that "owns" provisioning for task T (determined by
   hrw_score(node.id, task.requires.hash)) writes a ProvisionerLease
   to the CRDT (TTL = 120 s).
3. That node calls Provisioner plugin gRPC: ProvisionNode { caps }
4. Provisioner starts new BOI node, which gossip-joins the cluster.
5. New node advertises caps via gossip.
6. All nodes now see new node as a candidate; assignment proceeds normally.
7. Lease holder writes ProvisionerDone to CRDT.
```

If the lease holder fails during provisioning, another node detects the failed
SWIM state, the TTL-expired lease is not renewed, and a new lease holder is
elected by the same HRW function. The provisioner call may be retried.

---

## 5. Failure Modes

| Scenario | Detection | Recovery | TTR | Worst case |
|----------|-----------|----------|-----|-----------|
| Any single node crashes mid-assignment | SWIM detection ~10 s | CRDT claim conflict resolved; task re-assigned by new winning claimer | ~10 s | Task delayed by SWIM timeout |
| Network partition | Nodes on each side continue independently; CRDT diverges | On heal, CRDTs merge; task claim conflicts resolved deterministically | Partition duration + 2 gossip rounds | Task may start on both sides of partition (claim conflict; losing side aborts) |
| Provisioner returns success, node never joins | TTL on ProvisionerLease expires (120 s) | New lease holder re-provisions | 120 s | Double-provisioning if node is slow |
| Node advertises capability it can't run | RunError returned from worker | Node updates its caps CRDT; gossip converges; task re-queued | 1 gossip round | One failed execution |
| Long-running task outlives its node | SWIM detection ~10 s | Task CRDT shows `running` on failed node; all nodes mark TaskOrphaned; task re-queued | 10 s + task restart | Duplicate run if partition heals after restart |
| Clock skew > 5 s | HLC detects physical time jump; logs warning | HLC compensates by advancing logical counter; conflicts flagged for review | Immediate | Incorrect LWW resolution for cap updates if skew > HLC tolerance |
| Pool plugin daemon crashes | Worker RPC fails; Pool plugin reconnect attempted | PluginHost restarts plugin daemon; slot freed | Seconds | Orphaned worker until restart |
| Gossip store corrupted | CRC check on SQLite CRDT table | Node wipes local store, re-gossips from scratch; full convergence in O(log N) rounds | Minutes | Node absent from pool during re-sync |

---

## 6. Plugin Integration Points

Same gRPC sidecar model as the shared constraints specify. Key difference:
the **Router plugin** in Bravo is optional — the default HRW assignment is
sufficient for most cases. Plugins plug in at:

- **PoolPlugin** — runs on every node independently; no central coordination
- **WorkspacePlugin** — invoked by the node that will run the task
- **RouterPlugin** — if present, overrides HRW score computation; must itself
  be deterministic (same inputs → same output) or all nodes must call it
  (adding a round-trip RPC to the critical path)
- **ProvisionerPlugin** — called by the CRDT lease holder only
- **HooksPlugin** — called by the node where the event occurs; ordering across
  nodes is gossip-order, not causal

**Warning for plugin authors:** Because there is no single coordinator, a Hook
event "OnTaskAssigned" may fire on multiple nodes before claim resolution
completes. Hooks must be idempotent.

---

## 7. BOI Core Modules

**New modules required:**

| Module | Responsibility |
|--------|---------------|
| `gossip/` | SWIM failure detector, epidemic broadcast, digest protocol |
| `crdt/` | OR-Set, LWW-Register, MVR, HLC implementation |
| `scheduler/` | HRW assignment, claim protocol, conflict resolution |
| `plugin_host/` | Plugin lifecycle, gRPC client factory |
| `capability/` | Cap advertisement, satisfaction predicate |
| `provisioner/` | ProvisionerPlugin client, CRDT lease management |

**Removed from existing BOI:**

- No single SQLite "master" — each node's SQLite becomes a CRDT replica

**CLI surface additions:**

```
boi cluster members         # show all known nodes and their CRDT state
boi cluster gossip-stats    # convergence metrics, message rate
boi cluster sync <peer>     # force full gossip sync with peer (debug)
boi plugin list             # registered plugins and status
```

---

## Self-Review

**Weakest assumption:** CRDT convergence is fast enough that the
double-assignment window is acceptable. In a LAN this is true (2–4 gossip
rounds, ~4–8 seconds). Over high-latency or partitioned networks, the
convergence window stretches and the claim conflict window grows proportionally.
The claim protocol prevents actual double-execution but not double-assignment
followed by one abort — which wastes resources if the task is expensive to start.

**Biggest risk:** The Multi-Value Register for task status introduces visible
complexity. When two nodes concurrently update a task's status (rare but
possible during partitions), the MVR surfaces a conflict to the operator
rather than resolving it silently. This is correct but operationally ugly.
Most teams expect task state to be unambiguous.

**Simpler alternative considered:** Use a single gossip-elected "coordinator"
per task (by HRW, the coordinator is the node with the highest HRW score for
that task). All assignment decisions go through the coordinator. Rejected
because this reintroduces a single point of failure per task (the coordinator
node) and complicates the "no single point of coordination" constraint.

**With 2× budget:** Replace gossip with a proper causal broadcast (HLC-ordered
reliable multicast). Eliminates the convergence window entirely; all nodes see
the same events in causal order. Operationally more complex (reliable delivery
requires buffering) but removes the claim conflict protocol.

**With ½ budget:** Drop the MVR for task status; use LWW everywhere. Accept
that rare concurrent updates will silently pick a winner. Lose the ability to
detect concurrent conflicts, but the system becomes simpler to reason about
for operators.
