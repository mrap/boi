## Judge 1 — Correctness & consistency

I evaluated each design against four correctness axes: double-dispatch under race, lost tasks, zombie tasks, and partition behavior — plus whether the stated "deterministic assignment" claim is actually delivered by the consistency model. Verdicts are blunt and cite specific sections/lines.

### Ranking (most → least correct)

1. **Charlie** (etcd-backed) — strongest correctness, fewest hand-waves.
2. **Bravo** (single-primary lease) — correct in the happy path, brittle at the seams.
3. **Alpha** (gossip mesh) — the most dangerous design on this axis.

### Alpha — gossip mesh

- **Double-dispatch**: Alpha's §3 admits the TryClaim claim map is **in-memory on the target node**. The Self-Review (§8 "Biggest risk", lines 365–372) concedes that if the target node crashes between receiving TryClaim and persisting it, "a race exists where two dispatchers both believe they own the task." The mitigation is "tasks should be designed to be idempotent." That is not a correctness guarantee; it is a request that the user not notice the bug. The hard constraint §8 says "No double-execution." Alpha violates it by construction.
- **Lost tasks**: §3 "Lost-task prevention" (lines 183–187) hand-waves that "the spec's retry/watchdog logic handles re-queuing." There is no described watchdog component, no owner for the pending-task set, and no replicated task queue. If the dispatcher crashes after TryClaim expiry but before re-queue, the task is gone — no node owns it.
- **Zombies**: TryClaim has a 5 s expiry (§3, lines 170–172). The Pool plugin can keep a worker running well past 5 s. So another dispatcher can legitimately reclaim the task, get `Claimed`, and run a second worker while the first is still alive. The claim map TTL is not coupled to actual worker liveness. Classic zombie.
- **Partition**: §5 row 3 says "Each partition assigns tasks independently to nodes in its view; duplicate tasks prevented by TryClaim CAS on target node." Wrong. If a task is dispatched in *both* partitions and the target node is in only one of them, both partitions independently CAS-succeed against *different* target nodes. The CAS is local to a target — it cannot prevent two different targets from each accepting the same task_id.
- **Determinism claim**: §3 says "same task + same cluster view → same target." But the cluster view is eventually consistent (§1, "no linearizability is claimed"). The determinism is conditional on a property the system explicitly disclaims. The argument is circular.

### Bravo — single-primary lease

- **Double-dispatch**: The Primary is the single writer for `leases`. Per §3 and §6, leases are written to the quorum journal before AssignAck is returned. This is genuinely safe in the steady state. The split-brain story (§5 row 4, §6 "Split-brain prevention") relies on quorum journal writes — defensible.
- **Lost tasks**: §6 step 2 ("Uncommitted assignments"): assignments the old Primary held in memory but had not replicated are "considered lost. The task returns to UNASSIGNED state." But §5 row 1 also says dispatch nodes queue AssignTask RPCs *locally* during the pause. The Self-Review (lines 437–438) admits: "If a dispatch node crashes during the pause, those queued tasks are lost. This is a real gap." Confirmed loss path. Constraint §8 violated, by author admission.
- **Zombies**: A worker on an executing node continues even after the Primary evicts it (§5 row 2 only releases the lease). There is no described kill path from new-Primary to orphaned worker. If the old worker is on a healthy node with a flaky link to the Primary, the new Primary reassigns and now two workers run.
- **Partition**: §5 row 3 is acceptable — minority partitions cannot quorum-write and therefore stall. Good. But the Self-Review (lines 449–453) explicitly flags a real bug in the lease acquisition: "two concurrent term-acquisition attempts can both achieve quorum if the quorum membership changes between Phase 1 and Phase 2." The author wrote "Full Raft eliminates this" — meaning the as-designed protocol has a known split-brain hole. This is the most damning specific admission in the bundle.
- **Determinism claim**: §3 mixes `idle_fraction` (live dynamic state) with `hash(task.id + node.id + term)`. The Primary aggregates `workers_busy` via heartbeats (500 ms stale, §1). Two assignments arriving on opposite sides of a heartbeat refresh will pick different nodes for the same task. Determinism holds only within one heartbeat tick — weaker than claimed.

### Charlie — etcd backbone

- **Double-dispatch**: §3 CAS on `/boi/tasks/assigning/{task_id}` is a real linearizable transaction in etcd. The losing node observes failure cleanly (line 184). This is the textbook correct primitive.
- **Lost tasks**: Monitor (§7 `scheduler::monitor`) watches stale `assigning/` / `assigned/` / `running/` keys and re-queues. The pending task is always in etcd until the atomic delete in the multi-key txn (lines 188–194). There is no window where the task exists outside etcd.
- **Zombies**: §5 row 2 — `running` heartbeat stops, lease expires, monitor re-queues. The orphaned worker process is "cleaned up by OS." This is the one soft spot: nothing in BOI core actively kills the old worker on the original host if that host comes back. But the design at least notices the case.
- **Partition**: §5 rows 4–5 — minority etcd partition keeps quorum; majority loss = read-only degraded mode, no new tasks, running tasks complete. Correct safety bias. A BOI node partitioned from etcd self-fences (§2 line 121). Clean.
- **Determinism claim**: §3 ranking reads at a specific etcd revision (line 158), so all nodes deterministically agree. The CAS makes determinism unnecessary for correctness; it is only a tie-break optimization. This is the only design where determinism is honestly delivered.

### Worst on this axis: **Alpha**

The most damning single flaw is **Alpha's reliance on a non-persistent, in-memory claim map on the target node as the sole guard against double-execution**, combined with the author's admission in §8 that this can fail and the mitigation is "make tasks idempotent." The shared constraint §8 ("No lost tasks. No double-execution. No zombies.") is non-negotiable, and Alpha violates all three: zombies via claim-TTL/worker-lifetime decoupling, double-execution under target-node crash and under cross-partition dispatch, lost tasks under dispatcher crash with no described owner.

Bravo has known gaps but the author flags them honestly. Charlie has the only assignment primitive (linearizable CAS on a replicated store) that actually implements the stated guarantees.
