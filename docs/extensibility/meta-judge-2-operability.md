# Meta Review — Judge 2 (Operability Lens)

## Judge 2 — Operability

I read every design assuming it's 3 a.m., I'm on PagerDuty, and a task didn't run. The question isn't "is this elegant?" — it's "can I figure out what happened and fix it before sunrise?" Under that lens, the three designs are very far apart.

### Scorecard

| Dimension | Alpha (Gossip) | Bravo (Single-Primary) | Charlie (etcd) |
|---|---|---|---|
| Reconstruct an assignment decision | Hard — every node has its own view; "the" snapshot doesn't exist | Medium — Primary's journal is authoritative if you can find which term was current | **Easy** — etcd revision is a global timestamp; replay state at revision N |
| Where state lives | Distributed across N nodes, eventually consistent | Primary in-memory + 3-node quorum journal | Single external etcd cluster |
| Day-2 dependencies before first dispatch | mTLS CA, seed addresses, NTP | mTLS CA, cluster secret (HMAC), seed list, quorum journal config | **etcd cluster fully operational**, mTLS CA, lease TTL tuning |
| Rolling upgrade safety | Risky — gossip wire format and SWIM constants must match across versions; no version field on `NodeRecord` | Moderate — Primary lease holder must be drained; term/journal format is a wire contract | Cleanest — nodes are stateless from etcd's view; drain by revoking lease, restart, rejoin |
| Cert rotation | Painful — every node talks to every other node; rotation window must cover full mesh | Painful — Primary validates joins against cluster CA; rotating CA mid-flight risks fencing live nodes | **Cleanest** — rotation flows through etcd PKI; BOI nodes pull from `/boi/...` |
| 3 a.m. observability | Worst — "what did node B think at t=X?" requires SSHing to B and hoping logs survived | Mediocre — must locate the Primary at the moment of failure (which is exactly when it failed) | **Best** — `etcdctl get --prefix /boi/ --rev=N` reconstructs the universe |

### Per-design specifics

**Alpha (gossip).** This is the operational worst. The "cluster view" is whatever node you happened to query. The doc admits eventual consistency converges in "2–3 gossip rounds (typically < 1 s for clusters ≤ 50 nodes)" — fine until you're past 50 nodes or on a degraded link. The TryClaim claim map is per-node in-memory (§3, "biggest risk" self-review concedes this); if a node crashes between TryClaim and dispatch, the 5 s expiry papers over it, but you cannot tell from logs whether a task ran 0, 1, or briefly 2 times. The failure-modes table item #3 ("network partition") cheerfully says "each partition assigns tasks independently to nodes in its view" — meaning: under partition, you will dispatch duplicate work and only catch it via task-level idempotency. Debugging an assignment requires reconstructing a Lamport-clocked merge of N node states. There is no "show me the cluster at 02:47:13" command — and the doc proposes none.

**Bravo (single Primary).** Better than Alpha but it inherits a unique pager risk: the Primary's quorum-write protocol is a hand-rolled simplified Raft (the self-review concedes this — "two concurrent term-acquisition attempts can both achieve quorum if quorum membership changes between Phase 1 and Phase 2"). When that bug bites, you will be debugging split-brain by reading HMAC-signed leases out of an append-only file. Version skew is dangerous: the Primary serializes assignment decisions, so a v1.1 Primary processing a v1.0 follower's heartbeat is a wire-format minefield. Rolling upgrade requires explicit leadership transfer — not documented. The 100–500 ms decision pause is also "not bounded by the protocol itself" (self-review). On the upside, the journal at least gives you a tape to replay.

**Charlie (etcd).** Most external operational dependency, smallest operational surface inside BOI. The trade is real: you must run a 3-node etcd cluster, monitor its disk (failure mode #10), tune lease TTL, manage etcd certs separately from BOI certs. But that's well-trodden ground — etcd is the most-operated KV store on the planet. Once you have it, every other operational question gets boring: assignment history is a key range, "what did the cluster look like at revision 42891" is one command, rolling upgrade is "drain lease, restart, rejoin," cert rotation flows through standard etcd tooling. Failure mode #5 (etcd majority partition → BOI fences) is explicit and safe.

### The 3 a.m. pages

| Design | Page I'd dread |
|---|---|
| Alpha | "Task X ran twice in production. Audit log shows two different nodes claim ownership, both with valid TryClaim acks." Reconstructing which node had which view at which Lamport step is borderline impossible without per-node gossip traces (which aren't speced). |
| Bravo | "Primary lease flapping every 30 s; decision pause cascading; nothing assigns." Root-causing requires reading the quorum-journal tape across N nodes while terms increment. The HMAC signatures help you verify but not diagnose. |
| Charlie | "etcd is down." That's a known runbook. |

### On-call cost

Alpha pages you for: ghost nodes (gossip GC failed), false-death from NAT/indirect-ping, divergent views, partition double-dispatch, claim-map races. Most of these require correlated logs from 3+ nodes.

Bravo pages you for: Primary flapping, journal write stalls, term contention, split-brain edge cases, decision-pause tail latency, lost queued dispatch RPCs during transfer.

Charlie pages you for: etcd health (disk, leader election, latency). One system, one runbook.

### Ranking (best to worst, operability only)

1. **Charlie** — externalized state means standard tooling, point-in-time reconstruction, clean upgrade/rotation paths. The etcd dependency is a real cost but it's a *known* cost.
2. **Bravo** — at least there's a journal to read, but hand-rolled quorum + Primary transfer is a debugging hazard the doc doesn't fully own.
3. **Alpha** — *worst to operate.* No global view, no claim durability, partition tolerance achieved by accepting duplicates, no story for cert rotation or rolling upgrade, debugging requires N-node log forensics. Verdict: do not put this on call without a step-function increase in observability tooling that is not in the spec.

### Bottom line

If your operability budget is "one engineer, modest tooling," Charlie is the only viable choice. Alpha will burn nights.
