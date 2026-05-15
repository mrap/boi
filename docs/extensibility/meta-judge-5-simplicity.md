## Judge 5 — Simplicity & cost-to-ship

Lens: how cheap is this to build, ship, and trust. Lines of code, dependencies, and conceptual surface area that a single Rust contributor (i.e. the person who actually writes this) has to hold in their head.

### Ranking (smallest viable first)

**1. Charlie (external etcd). Cheapest path to v0.1.**
**2. Alpha (gossip mesh). Cheapest steady-state, expensive to write correctly.**
**3. Bravo (single-primary with quorum journal). The most bloated by far.**

---

### Charlie — external store

**Net-new modules in core (Charlie §7):** 12 (`store::etcd`, `cluster::registry`, `cluster::membership`, `scheduler::assign`, `scheduler::monitor`, `scheduler::provision`, `plugin::host`, `plugin::router`, `cmd::dispatch`, `cmd::node`, `config`, `tls`). But the heavy primitives (linearizable reads, CAS, leases, watches) are *outside* the binary. Effectively the contributor writes glue.

**External deps:** etcd cluster (operational), `etcd-client` Rust crate, `tonic`/`prost` for gRPC plugins, `rustls` for mTLS. One real new infra dependency (etcd).

**Conceptual surface for a new contributor:** etcd's key-value + watch + lease + txn model — well-documented, off-the-shelf. No SWIM, no Raft, no Lamport clocks. The assignment algorithm (Charlie §3) is ~30 lines: filter, sha256-sort, CAS, done. A new contributor can be productive in days.

**v0.1 estimate:** **3–4 weeks.** Most of that is wiring gRPC plugin scaffolding and the spec→pending→assigned→done state machine. The hard problems are delegated to etcd.

**Production-trust estimate:** **8–10 weeks.** etcd is the bottleneck — you trust it from day 1. BOI's own paths are simple enough to harden quickly.

**Cuttable without losing core value:** the `assigning/` intermediate key (Charlie itself notes this in §8 "Second Pass" — collapse into a single atomic txn). The Router plugin can ship as built-in only. The 5-min provisioning retry monitor is a hex-events policy, not core code.

---

### Alpha — gossip mesh

**Net-new modules in core (Alpha §7):** 11 modules, but two of them — `cluster::gossip` and `cluster::store` (CRDT-ish version-gated merge + SWIM indirect-ping) — are nontrivial. Plus `claim` (TryClaim CAS server with expiry GC).

**External deps:** Likely `tonic`, `prost`, a SWIM crate (or hand-rolled), UUID, plus claim-map persistence if you want crash safety (Alpha's own §8 flags this).

**Conceptual surface:** SWIM (suspect/dead/indirect-ping), CRDT-merge semantics, Lamport version vectors, deterministic ranking with optimistic CAS, claim TTLs, NAT-traversal corner cases (Alpha §8). A contributor needs to internalize gossip-cluster theory before touching anything. This is high cognitive load.

**v0.1 estimate:** **6–8 weeks.** SWIM + indirect ping + claim CAS + provisioning dedup all need careful implementation. Testing requires multi-node harnesses.

**Production-trust estimate:** **16–20 weeks.** The author admits the TryClaim window allows double-execution under target crash (Alpha §8 "Biggest risk"). Earning trust means adding a claim WAL, fixing NAT issues, and surviving partition tests. Each is real work.

**Cuttable:** SWIM indirect-ping (use plain heartbeats — Alpha §8 ½-budget says so). The Router plugin (Alpha §8 ½-budget concurs). The Provisioner plugin in v0.1.

---

### Bravo — single primary + quorum journal

**Net-new modules in core (Bravo §8):** 9 — but `boi::primary` (lease + decision loop + assignment + provisioning approval, single-threaded), `boi::journal` (quorum write/read), and `boi::cluster` (heartbeats + failure detection + lease-acquisition vote) collectively reinvent Raft minus the proof.

**External deps:** `tonic`, `prost`, `rustls`, plus whatever HMAC + quorum-write primitives. The journal is hand-rolled — no etcd, no `raft-rs`, no `openraft`. The team's own self-review (Bravo §8) admits: *"two concurrent term-acquisition attempts can both achieve quorum if the quorum membership changes between their Phase 1 and Phase 2 steps."* They are aware their custom protocol has a known correctness bug and defer the fix to "2× budget."

**Conceptual surface:** quorum journals, lease terms, Phase 1/Phase 2 vote protocol, primary role transfer, term fencing, split-brain reconciliation, decision pause semantics, in-flight committed vs uncommitted journal entries. This is "you must learn distributed consensus" territory.

**v0.1 estimate:** **10–14 weeks**, and the v0.1 will have known consensus bugs.

**Production-trust estimate:** **6–9 months,** or never without replacing the hand-rolled quorum with real Raft. Custom consensus is a graveyard.

**Cuttable:** The RouterPlugin (Bravo's own ½-budget answer). But the actual bloat is the quorum journal itself — the entire `boi::journal` module is solving a problem Charlie pays etcd to solve and Alpha solves with eventual-consistency + CAS-on-target.

---

### What is bloated worst

Bravo is bloated worst. It writes a quasi-Raft from scratch (`boi::journal` + lease acquisition + Phase 1/Phase 2 vote in §6) and ships with a known correctness gap (Bravo §8). It carries a `RouterPlugin` synchronous RPC on the hot path of every assignment (Bravo §3 dispatch flow), a SeederPlugin (§7) that adds another plugin contract for what could be a config file, and a sub-second decision pause that the protocol does not actually bound (Bravo §8 "Biggest risk"). The complexity is paying for strong consistency the workload does not require — BOI tasks are already designed to be retryable.

### Single biggest piece to cut

**Cut Bravo's `boi::journal` quorum-write subsystem entirely.** If strong consistency is the requirement, use etcd (Charlie's bet). If it isn't, accept eventual consistency and a CAS (Alpha's bet). Inventing a third option — a hand-rolled simplified Raft — is the worst of both: implementation cost of consensus without correctness guarantees of consensus. The team admits the bug exists. Delete the module, depend on etcd, and Bravo collapses into a worse Charlie.

For Charlie: cut the `assigning/` intermediate key (single atomic txn instead). For Alpha: cut SWIM indirect-ping and the Router plugin from v0.1.

---

*Final ranking by cost-to-ship: Charlie (3–4w / 8–10w) < Alpha (6–8w / 16–20w) < Bravo (10–14w / 6–9mo).*
