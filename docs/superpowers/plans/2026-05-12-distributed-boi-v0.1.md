# Distributed BOI v0.1 — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. This is a **master plan** — each Phase is decomposed into a dispatched BOI spec containing TDD-grained tasks. The master plan defines the DAG, files, acceptance criteria, and E2E requirements; the BOI specs implement.

**Goal:** Evolve BOI from a single-node Rust binary into a multi-machine, plugin-extensible task dispatcher with etcd-backed cluster state, gRPC-sidecar plugins, capability-based assignment, and on-demand provisioning. v0.1 ships in ~8–10 person-weeks of parallelizable work.

**Architecture:** etcd backbone (one stack everywhere, no embedded fallback). Plugins are language-agnostic gRPC sidecars that never touch the store directly — BOI core mediates. HRW over capability-matched membership snapshot, with CAS on `/boi/claims/{task_id}` as the actual correctness primitive (lease_id fencing token). Trusted cluster with mTLS + cluster CA. New nodes join via signed JWT tokens (CA fingerprint embedded — no TOFU). Lightweight degraded mode: in-flight tasks continue, new dispatches fail loud.

**Tech Stack:** Rust 2024 edition, tonic (gRPC), etcd-client crate, prost (protobuf), rcgen (TLS), JWT (jsonwebtoken crate), Docker Compose (E2E harness), buf (proto breaking-change CI).

**Source of truth:**
- Design doc: `docs/extensibility/distributed-architecture-design-2026-05-12.md` (9,036 words, 24 critique findings addressed, 6 expert decisions logged as §16)
- Locked decisions: §2 LD-1..LD-7
- Open questions: all closed in §16 Decisions Log
- Branch: `feat/distributed-architecture`

---

## Non-negotiable cross-cutting requirements

### E2E tests are first-class

**Every phase below ends with a containerized E2E acceptance test.** The harness is built in Phase 0a before any production code. No phase is "done" without its E2E test landing green on `make e2e` and in CI.

Containerized E2E means: Docker Compose with one or more `etcd` containers, N BOI node containers, M plugin sidecar containers (reference + mock), and a test-runner container that exercises the cluster through the CLI and gRPC. Tests must be:
- **Hermetic.** No host etcd, no host network surprises. `docker compose up` from a clean state.
- **Deterministic.** Flaky tests fail CI. Use real timeouts, not sleeps.
- **Diagnose-friendly.** Failures dump etcd state, node logs, and plugin transcripts to artifacts.

### Test pyramid

- Unit tests in every crate (`cargo test`).
- Plugin contract conformance: `boi plugin test <binary>` runs the full lifecycle + per-RPC checks against the binary in isolation (one container, no cluster).
- Cluster integration: 3-node etcd + 3 BOI nodes + reference plugins, scenarios at phase granularity.
- Provisioning E2E: includes a reference Docker provisioner plugin that boots new BOI-node containers.

### Commit discipline

Each phase = one or more BOI specs = one or more PR-shaped commits. No long-lived uncommitted branches. Every phase commit lands on `feat/distributed-architecture`.

---

## File structure

New crates / modules (in `boi/`):

```
boi/
├── Cargo.toml                       (workspace adds new crates)
├── crates/
│   ├── boi-proto/                   ← NEW. All .proto files + generated bindings
│   │   ├── proto/
│   │   │   ├── boi/workspace/v1/workspace.proto
│   │   │   ├── boi/pool/v1/pool.proto
│   │   │   ├── boi/router/v1/router.proto
│   │   │   ├── boi/provisioner/v1/provisioner.proto
│   │   │   ├── boi/hooks/v1/hooks.proto
│   │   │   └── boi/cluster/v1/cluster.proto         (internal node-to-node)
│   │   └── src/lib.rs               (re-exports tonic-generated code)
│   ├── boi-cluster/                 ← NEW. etcd client + state model
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── client.rs            (etcd wrapper, lease mgmt)
│   │       ├── nodes.rs             (/boi/nodes + /boi/caps schema)
│   │       ├── dispatch_queue.rs    (state_version CAS)
│   │       ├── claims.rs            (lease_id fencing)
│   │       ├── hooks_hwm.rs         (/boi/hooks-hwm)
│   │       └── membership.rs        (watch + 30s TTL cache)
│   ├── boi-identity/                ← NEW. Cluster CA + mTLS + JWT join tokens
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── ca.rs
│   │       ├── mtls.rs
│   │       └── join_token.rs
│   ├── boi-plugin-host/             ← NEW. gRPC plugin lifecycle host
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── lifecycle.rs         (start, READY, restart, shutdown)
│   │       ├── handshake.rs         (Q4 versioning)
│   │       ├── workspace.rs         (Workspace plugin client)
│   │       ├── pool.rs              (Pool plugin client + WorkerEvent tee)
│   │       ├── router.rs
│   │       ├── provisioner.rs
│   │       └── hooks.rs             (best_effort + audit WAL)
│   ├── boi-assign/                  ← NEW. HRW + claim protocol
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── hrw.rs               (Q1 revision pinning)
│   │       ├── claim.rs             (Q2 lease_id fencing)
│   │       └── cooldown.rs          (F-06 consecutive_claim_failures)
│   ├── boi-node/                    ← NEW. The `boi node` daemon binary
│   │   └── src/main.rs
│   └── boi-test-harness/            ← NEW. E2E Docker Compose orchestration
│       ├── docker/
│       │   ├── docker-compose.yaml
│       │   ├── boi-node.Dockerfile
│       │   ├── reference-workspace-git.Dockerfile
│       │   └── reference-pool-localthread.Dockerfile
│       └── tests/                   (cargo test --features e2e)
├── crates/boi-cli/                  ← extends existing src/main.rs structure
│   └── src/
│       ├── cluster_cmd.rs           (boi cluster init/admin/...)
│       ├── node_cmd.rs              (boi node join/...)
│       ├── plugin_cmd.rs            (boi plugin test/install/...)
│       └── tail_cmd.rs              (boi spec tail)
└── reference-plugins/               ← NEW. Reference implementations
    ├── workspace-git/               (proves Workspace contract)
    ├── pool-localthread/            (proves Pool contract)
    ├── provisioner-docker/          (proves Provisioner via Docker)
    └── hooks-stdout/                (proves Hooks contract)
```

Existing `boi/src/` is preserved during transition — the new `crates/boi-node` is the future. v0.2 deprecates the old single-node entry point; v0.1 ships them side-by-side.

---

## Phase DAG

```
            ┌──────── Phase 0: Foundation ────────┐
            │  0a E2E harness (must be first)     │
            │  0b Proto contracts skeleton        │
            │  0c Workspace + module skeletons    │
            └──────────────────┬──────────────────┘
                               │
              ┌────────────────┴────────────────┐
              ▼                                 ▼
     ┌─── Phase 1 ────┐                ┌── Phase 2 ───┐
     │  Cluster state │                │  Plugin host │
     │  plane (etcd)  │                │  + 5 protos  │
     └────────┬───────┘                └──────┬───────┘
              │           ┌──────────────────┘
              ▼           ▼
     ┌─── Phase 3 ────────────┐
     │  Identity & bootstrap   │
     │  (CA, mTLS, JWT, admin)│
     └────────┬───────────────┘
              │
              ▼
     ┌─── Phase 4 ─────┐
     │  Assignment +   │
     │  routing (HRW)  │
     └────────┬────────┘
              │
              ▼
     ┌─── Phase 5 ────────────┐
     │  Provisioning flow      │
     └────────┬───────────────┘
              │
              ├──────────────────────┬──────────────┐
              ▼                      ▼              ▼
   ┌─ Phase 6 ─────────┐  ┌─ Phase 7 ────────┐  ┌─ Phase 8 ─────┐
   │  Degraded mode +  │  │  Worker stdout    │  │  Hooks audit  │
   │  observability    │  │  durability+tail  │  │  tier (WAL)   │
   └─────────┬─────────┘  └──────────┬───────┘  └───────┬───────┘
             │                       │                  │
             └───────────────────────┴──────────────────┘
                                 │
                                 ▼
                      ┌─ Phase 9 ────────────┐
                      │  Migration + docs    │
                      └──────────────────────┘
```

**Critical path:** 0 → 1 → 3 → 4 → 5 → 9. Phases 2, 6, 7, 8 parallelize once their deps clear.

**Sizing recap** (from design §13):
- Phase 0: ~0.5 wk (harness + protos + skeletons)
- Phase 1: ~2 wk (cluster state plane)
- Phase 2: ~2 wk (plugin host + 5 protos + 2 reference plugins)
- Phase 3: ~1.5 wk (identity)
- Phase 4: ~1.5 wk (assignment)
- Phase 5: ~1 wk (provisioning)
- Phase 6: ~0.5 wk (degraded mode + obs)
- Phase 7: ~0.5 wk (stdout tail)
- Phase 8: ~0.5 wk (hooks audit)
- Phase 9: ~0.5 wk (migration + docs)
- **Total: ~9.5 person-weeks**, runnable in ~4 calendar weeks with two parallel tracks.

---

## Phase 0 — Foundation

**Spec name:** `phase-0-foundation`
**Depends on:** nothing
**Parallelizable internally:** 0a, 0b, 0c can be three tasks within one spec.
**Acceptance:** `make e2e` runs a no-op end-to-end scenario green (e.g., spin up 3 nodes, every node reports `health=ok` via cluster CLI). All 5 plugin protos compile + buf lint clean. New crate skeletons compile with `cargo build`.

### 0a. E2E harness

**Files:**
- Create: `crates/boi-test-harness/docker/docker-compose.yaml`
- Create: `crates/boi-test-harness/docker/boi-node.Dockerfile`
- Create: `crates/boi-test-harness/docker/etcd-init.sh`
- Create: `crates/boi-test-harness/tests/smoke.rs`
- Create: `crates/boi-test-harness/Makefile` (targets: `up`, `down`, `e2e`, `logs`, `clean`)
- Create: `.github/workflows/e2e.yaml` (or extend existing CI)

**What it produces:** `make e2e` from repo root spins up `etcd:v3.5` + 3 `boi-node` containers + a test-runner container that imports `boi-test-harness/tests/*`. The smoke test asserts: cluster has 3 nodes, each reports health=ok, `boi cluster members --json` returns 3 entries. Test artifacts (etcd state dump, node logs, plugin transcripts) are written to `./e2e-artifacts/` on failure.

### 0b. Proto contracts skeleton

**Files:**
- Create: `crates/boi-proto/proto/boi/{workspace,pool,router,provisioner,hooks,cluster}/v1/*.proto`
- Create: `crates/boi-proto/build.rs` (tonic_build)
- Create: `crates/boi-proto/src/lib.rs` (re-exports)
- Create: `buf.yaml`, `buf.gen.yaml`, `.github/workflows/buf.yaml`

Each proto declares package `boi.<name>.v1` (Q4 hybrid versioning). Each service includes a `Handshake(HandshakeRequest) returns (HandshakeResponse)` RPC with `plugin_proto_minor: uint32` and `capabilities: repeated string`. Buf breaking-change runs in CI.

### 0c. Workspace + skeletons

**Files:**
- Modify: `Cargo.toml` (root) — add workspace members: boi-proto, boi-cluster, boi-identity, boi-plugin-host, boi-assign, boi-node, boi-test-harness.
- Create: `crates/boi-cluster/src/lib.rs`, `crates/boi-identity/src/lib.rs`, etc. — each with a stub `pub fn placeholder() {}` and a passing unit test.

**Dispatch:** First spec to fire. `mode: execute`. Single spec, three internal tasks.

---

## Phase 1 — Cluster state plane

**Spec name:** `phase-1-cluster-state`
**Depends:** Phase 0 (specifically 0a harness must work for the acceptance E2E).
**Parallelizable:** with Phase 2.
**Acceptance:**
- Unit tests for each module in `boi-cluster`.
- E2E: 3 BOI nodes register themselves in etcd, each acquires a lease, advertises caps, sees other 2 via membership module. Kill one node container; within 2× lease TTL (15s default → 30s) the other 2 see it as gone. Restart it; it rejoins. Test runs in `make e2e`.

### Internal task breakdown (BOI spec tasks)

1. **etcd client wrapper + lease mgmt** (`crates/boi-cluster/src/client.rs`). Connect, retry, lease grant + keepalive.
2. **/boi/nodes + /boi/caps schemas** (`nodes.rs`). Per design §4 schema; reserved capability keys (`os`, `arch`, `region`, `runtime`), `x-<vendor>-<tag>` for user-defined (F-14).
3. **/boi/dispatch-queue with state_version CAS** (`dispatch_queue.rs`). Per F-03. Every state transition is a `Txn(compare(state_version == N); put state_version = N+1)`.
4. **/boi/claims with lease_id fencing** (`claims.rs`). Per Q2: `claim_lease_id` sub-key, single-field Txn compare.
5. **/boi/hooks-hwm prefix** (`hooks_hwm.rs`). Per Q6 audit tier; only HWM lives in etcd, bulk events on local-disk WAL.
6. **Membership module** (`membership.rs`). etcd watch + 30s TTL cached snapshot. Exposes `snapshot()` returning a `MembershipSnapshot` struct with the etcd `mod_revision` it was read at (Q1 enables revision pinning later in Phase 4).
7. **E2E test:** 3-node cluster, kill/restart, partition simulation via Docker network commands.

---

## Phase 2 — Plugin host + 5 protos + 2 reference plugins

**Spec name:** `phase-2-plugin-host`
**Depends:** Phase 0.
**Parallelizable:** with Phase 1.
**Acceptance:**
- Unit tests for every plugin client in `boi-plugin-host`.
- `boi plugin test <binary>` runs full conformance for each of the 5 contracts against a reference implementation.
- E2E: launch a BOI node, attach reference Git Workspace plugin + reference LocalThread Pool plugin, run a trivial spec end-to-end (still single-node mode at this point — no cluster needed).

### Internal task breakdown

1. **gRPC server scaffold + plugin lifecycle** (`lifecycle.rs`). Spawn child process, capture `BOI_READY\n` on stdout, restart-on-crash (F-20: fixed 3 restarts / 5 min → `unstable`, not exponential), graceful shutdown.
2. **Handshake RPC** (`handshake.rs`). Per Q4: validate `plugin_proto_minor`, collect capabilities, reject on major mismatch.
3. **Workspace plugin client + proto v1** (`workspace.rs`). Six-stage lifecycle: Provision, Fetch, Setup, Verify, Exec, Cleanup. Streams progress events.
4. **Pool plugin client + proto v1** (`pool.rs`). Spawn / Tail / Cancel / WorkerEvent stream. Pool **must** carry `boi-claim-lease` gRPC metadata; core enforces the etcd Txn predicate. Idempotency contract per F-05.
5. **Router plugin client + proto v1** (`router.rs`). Passthrough default in core; plugin slot reserved.
6. **Provisioner plugin client + proto v1** (`provisioner.rs`). Plugin calls back into core's `MintJoinToken` (Q3 gated).
7. **Hooks plugin client + proto v1** (`hooks.rs`). Two tiers (Q6): `delivery_tier: best_effort | audit` in manifest. Phase 2 only ships `best_effort`; `audit` WAL lands in Phase 8.
8. **`boi plugin test` conformance harness** (`crates/boi-cli/src/plugin_cmd.rs`). Per F-13. Drives every RPC with canned inputs against a binary in isolation (one container, no cluster).
9. **Reference Git Workspace plugin** (`reference-plugins/workspace-git/`). Implements the existing trait behavior over gRPC.
10. **Reference LocalThread Pool plugin** (`reference-plugins/pool-localthread/`). Runs `claude -p` workers; carries lease metadata.
11. **E2E test:** single BOI node + ref plugins + trivial spec.

---

## Phase 3 — Identity & bootstrap

**Spec name:** `phase-3-identity`
**Depends:** Phase 1.
**Acceptance:**
- Unit tests for CA mint, mTLS verify, JWT sign+verify with embedded fingerprint.
- E2E: `boi cluster init` on node A → A becomes admin with self-signed cluster CA → A mints join token for node B → B starts with `--token` env, completes mTLS handshake with pinned fingerprint → B appears in `boi cluster members`. Without the token, B's join attempt fails closed.
- E2E negative case: try to mint a token from a non-admin node — reject.

### Internal task breakdown

1. **Cluster CA** (`ca.rs`). rcgen-based self-signed root, persistence at `~/.boi/cluster/ca.{crt,key}` on the seed node.
2. **mTLS between nodes** (`mtls.rs`). Tonic transport with rustls; both directions verify against cluster CA.
3. **JWT join tokens** (`join_token.rs`). Signed by cluster CA private key. Payload includes cluster ID, seed addr, token ID, expiry (5 min per F-21), CA fingerprint (F-04).
4. **`cluster_admin` capability gate** (`crates/boi-cluster/src/nodes.rs` extension). `cluster_admin` is write-only via admin path, not self-declarable. `MintJoinToken` RPC rejects unless caller's node has `caps.static.cluster_admin=true` (Q3).
5. **`boi cluster init`** (`cluster_cmd.rs`). Atomic: generate CA → store under `~/.boi/cluster/` → register seed node with `cluster_admin=true` → write `~/.boi/config.yaml` with cluster ID + seed addr. Idempotent: re-run is a no-op if state present.
6. **`boi cluster admin grant|revoke|list`** (`cluster_cmd.rs`). Modifies `caps.static.cluster_admin` on a named node via admin RPC.
7. **`boi node join --token`** (`node_cmd.rs`). Parse token → extract CA fingerprint → pin TLS handshake → request signed cert → write to `~/.boi/node/cert.{crt,key}` → start node loop.
8. **`--ca-key` break-glass** (cluster_cmd.rs). Operator-only path to mint a token offline with the CA private key.
9. **E2E test:** 2-node admit + reject paths.

---

## Phase 4 — Assignment & routing

**Spec name:** `phase-4-assignment`
**Depends:** Phase 1, Phase 2.
**Acceptance:**
- Unit tests for HRW math, capability filter, claim protocol, cooldown.
- E2E: 3-node cluster with `caps.os=mac` on node A only, `caps.os=linux` on B and C. Dispatch a spec with `requires: os=mac` — lands on A every time. Kill A — task reassigns to a provisioned node (but Phase 4 stubs the provisioner; full E2E for that is Phase 5). Stop adversary: kill B mid-task with a valid claim — claim lease expires, task reassigns to C.

### Internal task breakdown

1. **HRW core** (`crates/boi-assign/src/hrw.rs`). Pure function over `(task_id, [node])` → sorted preference list. Cite F-01: this is load distribution; correctness lives in CAS.
2. **Capability filter** (extends HRW). Returns only nodes whose advertised caps satisfy the task's `requires` clause.
3. **Revision-pinned assign() with W=64 stale window** (`hrw.rs`). Per Q1. assign() reads membership snapshot's `mod_revision`, passes it through the claim Txn as `compare(mod_revision <= snapshot_rev + 64)`. On `Txn` rejection due to stale window, refresh snapshot and retry up to 3 times before falling through to next-best HRW.
4. **Claim CAS protocol** (`claim.rs`). Atomic etcd Txn: compare claim absent + state_version == N + mod_revision in window; put `claim_lease_id`, set `claimant_node_id`, bump state_version. Per Q2 and F-02/F-03.
5. **Consecutive-failure cooldown** (`cooldown.rs`). Per F-06. Increment `consecutive_claim_failures` on each failed claim; at 3, flip `caps.dynamic.health=degraded` for 5 min. HRW skips degraded nodes.
6. **Default in-core Router** (`crates/boi-plugin-host/src/router.rs` passthrough impl). Calls assignment directly. Plugin slot reserved.
7. **E2E:** cap-match routing, claim-on-crash, cooldown observability.

---

## Phase 5 — Provisioning

**Spec name:** `phase-5-provisioning`
**Depends:** Phase 3, Phase 4.
**Acceptance:**
- Unit tests for no-capable-node detection, MintJoinToken authz.
- E2E: 1-node cluster (admin), dispatch task with `requires: os=mac` while no mac node exists. Reference Docker-provisioner plugin spawns a new container with `BOI_TOKEN` env, container boots into `boi node join --token $BOI_TOKEN`, advertises `os=mac`, claims the task, completes it. Then a second E2E: provisioner returns success but the new node never joins — F-06 cooldown kicks in.

### Internal task breakdown

1. **No-capable-node detection** in assignment loop. When HRW filter returns empty set AND cluster has spare capacity in caps schema, emit ProvisionRequest.
2. **MintJoinToken RPC in core** (admin-gated per Q3). Internal RPC; only callable by admin nodes, callable by Provisioner plugin running on those nodes.
3. **Provisioner plugin invocation** (`crates/boi-plugin-host/src/provisioner.rs`). Core mints token, passes `(token, capability_hint, expires_at)` to plugin's `Provision` RPC.
4. **Reference Docker provisioner** (`reference-plugins/provisioner-docker/`). Receives request, spawns BOI-node container with token in env, returns success when container's `boi-node` process is up (not when it has joined — joining is async).
5. **Provision-then-dead cooldown wire** (uses Phase 4's `consecutive_claim_failures` for the new node).
6. **E2E:** provision happy path + provision-then-no-join.

---

## Phase 6 — Degraded mode + observability

**Spec name:** `phase-6-degraded`
**Depends:** Phase 1.
**Parallelizable:** with Phase 5, 7, 8.
**Acceptance:**
- Unit tests for cached membership TTL behavior, fail-loud on stale etcd.
- E2E: stop etcd container mid-cluster, observe in-flight task continues to completion. Try to dispatch a new task during outage — fails with "etcd unreachable, retry" error and a metric counter increments. Restore etcd, dispatch succeeds. Also: `boi cluster local-fallback` drains node, persists in-flight claims to `~/.boi/pending-flush/`, switches to single-node, prints warning.

### Internal task breakdown

1. **30s TTL cached membership view** (`crates/boi-cluster/src/membership.rs` extension). Already partially in Phase 1; this phase adds the stale-tolerance semantics.
2. **Fail-loud dispatch when etcd unreachable** (assignment loop). New dispatches return an explicit `etcd_unreachable` error; no silent queueing.
3. **`boi cluster local-fallback`** (`cluster_cmd.rs`). Per F-07. Drains, persists claims, switches mode.
4. **Pending-flush buffer semantics** (`crates/boi-cluster/src/`). Per F-08. 100 MB cap, oldest-first eviction, JSONL on disk, at-least-once flush on recovery.
5. **Metrics catalog** (`crates/boi-node/src/main.rs` Prometheus exporter). Per F-12. Named gauges/counters: `claim_lease_expired_total`, `hrw_cas_retry_total`, `provision_req_latency_seconds`, `plugin_restart_total{plugin}`, `dispatch_queue_state_count{state}`, etc.
6. **Structured event log** (canonical event kinds per F-15). `task.{dispatched,claimed,started,completed,failed,reassigned}`, `node.{joined,drained,crashed,degraded}`, `provision.{requested,fulfilled,failed}`, `cluster.{ca_rotated,partition_detected,partition_healed}`.
7. **`--stale-ok` and `--local` flags** on read-only CLI commands (per F-22).
8. **E2E:** etcd partition, escape valve, metrics scrape.

---

## Phase 7 — Worker stdout durability + tail

**Spec name:** `phase-7-stdout-tail`
**Depends:** Phase 2, Phase 4.
**Parallelizable:** with Phase 6, 8.
**Acceptance:**
- Unit tests for log rotation (7d / 100MB) and Tail RPC.
- E2E: long-running task (90+ second sleep), CLI disconnects mid-stream, reattach via `boi spec tail <task_id> --follow` from a different node, see the stream resume. Disk fills past 100 MB → oldest task logs rotated out, current task continues writing.

### Internal task breakdown

1. **Host-side stdout tee** (`crates/boi-plugin-host/src/pool.rs` extension). Pool plugin's `WorkerEvent` stream chunks are tee'd to `~/.boi/logs/{spec_id}/{task_id}.log` on the executing node.
2. **Retention rotation** (`crates/boi-plugin-host/src/pool.rs`). Background sweeper: 7 days OR 100 MB total, operator-tunable.
3. **Internal `Tail` RPC** (in `crates/boi-proto/proto/boi/cluster/v1/cluster.proto`). Node-to-node only; not a plugin RPC.
4. **`boi spec tail <task_id> [--follow]`** (`tail_cmd.rs`). Core resolves `claimant_node_id` from etcd, opens internal Tail RPC to that node, streams to stdout.
5. **E2E:** disconnect + reattach + rotation.

---

## Phase 8 — Hooks audit tier

**Spec name:** `phase-8-hooks-audit`
**Depends:** Phase 2 (best_effort already there), Phase 6 (uses pending-flush patterns).
**Acceptance:**
- Unit tests for WAL append/dedup, HWM tracking, FIFO ordering.
- E2E: dispatch an audit-tier hook plugin. Crash the plugin mid-delivery — events resume from HWM on restart, no duplicates downstream (dedup key `(node_id, seq, kind, ts)`). Crash the BOI node — on restart, WAL is replayed.

### Internal task breakdown

1. **Local-disk WAL on emitting node** (`crates/boi-plugin-host/src/hooks.rs` audit path). JSONL append, fsync per batch.
2. **`/boi/hooks-hwm/` HWM tracking** (already in Phase 1 schema; this phase wires the writer/reader).
3. **Per-(node, plugin) FIFO + back-pressure** (`hooks.rs`). Stall the workflow emitting if HWM is too far behind.
4. **Plugin-side dedup contract** documented in `hooks.proto` v1.
5. **`boi plugin test` covers both tiers**.
6. **E2E:** crash-and-recover scenarios.

---

## Phase 9 — Migration + docs

**Spec name:** `phase-9-migration-docs`
**Depends:** all prior phases.
**Acceptance:**
- Migration guide proves out: take a current single-node BOI install, follow doc step by step, end up with a working 1-node distributed cluster running the same specs.
- CLI reference, plugin author guide, operator guide.
- E2E: a "fresh install" container starts from zero, follows the docs, lands a working cluster.

### Internal tasks

1. Migration guide at `docs/migration/single-node-to-distributed-v0.1.md`.
2. Update `docs/extensibility/worker-pool-providers.md` and `workspace-backends.md` to reference gRPC plugin contracts.
3. CLI reference at `docs/cli/v0.1.md`.
4. Plugin author quickstart at `docs/plugins/getting-started.md` — minimal Workspace plugin in ~50 lines.
5. Operator guide at `docs/operator/v0.1.md` — bootstrap, CA rotation, rolling restart procedure.
6. E2E: fresh-machine install walkthrough container.

---

## Dispatch sequencing

Each Phase becomes a BOI spec on `feat/distributed-architecture` branch. Specs use `phase_overrides` with `claude-opus-4-7` + `effort: high` on `execute`, `task-verify`, `plan-critique`, `critic`. `mode: challenge` to keep `code-review` out of the loop until Phase 9 lands the code-review fixes (Phase 9 may itself dispatch the fixes from `S1C7D` if they haven't merged by then).

**Dispatch order:**

1. `phase-0-foundation` — first, blocks everything.
2. Once Phase 0 lands: `phase-1-cluster-state` AND `phase-2-plugin-host` in parallel.
3. After Phase 1: `phase-3-identity`.
4. After Phases 1+2: `phase-4-assignment`.
5. After Phases 3+4: `phase-5-provisioning` AND (in parallel) `phase-6-degraded`, `phase-7-stdout-tail`, `phase-8-hooks-audit`.
6. Finally: `phase-9-migration-docs`.

`boi dispatch --after <comma-separated-spec-ids>` handles the DAG.

---

## Acceptance gate (every phase)

A phase is "done" when:
- ✅ All internal tasks land.
- ✅ Unit tests green (`cargo test`).
- ✅ E2E test green (`make e2e -- --filter phase-N`).
- ✅ Branch `feat/distributed-architecture` is updated with a merge from `boi/<spec_id>`.
- ✅ The phase's acceptance criteria in this plan are demonstrably met (the BOI spec's `verify:` block enforces).

No phase ships without its containerized E2E test green.

---

## Self-review notes

- **Spec coverage:** every locked decision LD-1..LD-7 maps to phases:
  - LD-1 (external store) → Phase 1
  - LD-2 (etcd everywhere) → Phase 0 harness + Phase 1
  - LD-3 (plugins never touch store) → Phase 2 host design + Phase 5 provisioner contract
  - LD-4 (lightweight degraded mode) → Phase 6
  - LD-5 (one plugin per kind) → enforced in Phase 2 plugin-host
  - LD-6 (HRW + CAS) → Phase 4
  - LD-7 (mTLS + trust) → Phase 3
- All 6 §16 decisions map to phases: Q1→4, Q2→1+2, Q3→3, Q4→0+2, Q6→1+2+8, Q7→7.
- E2E coverage per phase: explicit acceptance gates.
- No "TODO" / "TBD" / "later" markers in this plan.
- Cross-phase type consistency: schemas (`state_version`, `claim_lease_id`, `consecutive_claim_failures`) defined in Phase 1 are used by Phase 4 (assign), Phase 5 (provision), Phase 6 (degraded). Plugin proto package names (`boi.workspace.v1` etc.) consistent in Phase 0 and Phase 2.
