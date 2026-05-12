# Distributed BOI v0.1 — Architecture Design

**Status:** Draft v2 (post-critique revision — see §15 Response to critique)
**Branch:** `feat/distributed-architecture`
**Date:** 2026-05-12

---

## 1. Executive summary

This document is the canonical v0.1 architecture for Distributed BOI: the evolution of single-node BOI into a multi-machine, plugin-extensible task dispatcher that runs across heterogeneous environments (laptop, cloud, internal corp infra).

The decision tree that produced it:

1. **State foundation.** Three approaches were drafted — Alpha (peer gossip), Bravo (elected Primary + quorum journal), Charlie (external strongly-consistent store). Five blind judges scored them (correctness, operability, plugin DX, failures, simplicity). Charlie won correctness and operability, lost graceful-degradation. We picked **Charlie's pattern (etcd backbone)** because the cost of silent double-dispatch (Alpha) and quorum-management code in BOI (Bravo) is higher than the cost of operating one well-understood external store.
2. **Store choice.** **etcd** for both local dev and production — same stack everywhere. No SQLite-embedded fallback. Local dev = `docker run etcd`.
3. **Plugin coupling.** Judge 3 ("plugin DX") savaged Charlie for forcing plugin authors to learn etcd. We fix that by making **plugins never touch etcd**. Every plugin contract is gRPC against BOI core; core mediates all etcd I/O. The Provisioner gets a join-token *from BOI core*, never raw etcd credentials.
4. **Degraded mode.** Charlie's "etcd-down ⇒ cluster-dead" failure (Judge 4 §8) is lightly mitigated: each node keeps a 30 s TTL-cached membership view. In-flight tasks keep running; new dispatches fail loudly. No local queueing, no replay logic.
5. **Assignment.** **Rendezvous hashing (HRW)** over the capability-filtered membership snapshot, with a CAS-based claim on `/boi/claims/{task_id}`.
6. **Scope discipline.** v0.1 supports exactly one plugin of each kind per deployment. Multi-plugin routing punted to v0.2.

**Ships in v0.1:** etcd-backed cluster state, 5 gRPC plugin contracts (Workspace, Pool, Router, Provisioner, Hooks), HRW assignment, claim leases, capability advertisement, join-token provisioning flow, degraded-mode invariant, `boi node` / `boi cluster` / `boi plugin` CLI.

**Does NOT ship in v0.1:** local etcd embedding, multi-plugin-of-same-kind routing, cross-region affinity, capability-fraud quarantine, Byzantine trust, rolling cluster upgrades.

## 2. Goals & non-goals

**Goals** (each traces to a shared-constraint `SC-n` or a locked decision `LD-n`):

- G1. Tasks dispatched on any node run on a capability-matched node. *(SC-3, SC-5)*
- G2. Assignment is deterministic — same `(task, snapshot)` ⇒ same target. *(SC-7, LD-6)*
- G3. No lost tasks, no double execution, no zombie writes. *(SC-8)*
- G4. Plugins are language-agnostic gRPC sidecars; plugin authors do not link BOI internals. *(SC-1, SC-2, LD-3)*
- G5. When no capable node exists, BOI calls a Provisioner; new node joins and accepts the queued task within seconds. *(SC-6)*
- G6. Plugin daemons may crash without taking BOI core down. *(SC-10)*
- G7. mTLS between BOI nodes; no Byzantine assumptions. *(SC-9, LD-7)*
- G8. Cluster state survives any single BOI node loss. *(LD-1)*
- G9. Local development uses the same stack as production. *(LD-2)*
- G10. Degraded-mode behavior is explicit and loud. *(LD-4)*

**Non-goals** (each one line with rationale):

- N1. Embedded/SQLite cluster store — not for v0.1 because LD-2 demands one stack everywhere and the embedded path doubles the failure-mode surface.
- N2. Multiple Workspace/Pool/Router plugins active concurrently — not for v0.1 per LD-5; users wanting two backends run two BOI deployments.
- N3. Local-queue replay during etcd partitions — not for v0.1 per LD-4; would re-introduce Alpha-style soft consistency.
- N4. Byzantine fault tolerance — not for v0.1 per LD-7; cluster is trusted.
- N5. Cross-region task affinity beyond capability filtering — not for v0.1; HRW + capability tags are sufficient for the announced workloads.
- N6. Hot upgrades of BOI core without quiescing dispatch — not for v0.1; rolling-restart procedure is documented but assumes a brief dispatch pause.
- N7. Capability-fraud quarantine — not for v0.1; Judge 4 §4 problem deferred. v0.1 logs and surfaces, does not auto-demote.
- N8. Plugin-discovery service — plugins are configured per node via `boi plugin install`; no central registry in v0.1.

## 3. System overview

```
                                ┌──────────────────────────┐
                                │     etcd quorum (3+)     │
                                │  /boi/{nodes,caps,...}   │
                                └──────────┬───────────────┘
                                           │ mTLS, gRPC
                                           │ (CORE ONLY)
        ┌──────────────────────────────────┼──────────────────────────────────┐
        │                                  │                                  │
   ┌────┴────────────────┐         ┌───────┴─────────────┐         ┌──────────┴──────────┐
   │  BOI node N1        │         │  BOI node N2        │         │  BOI node N3        │
   │ ┌──────────────┐    │   mTLS  │ ┌──────────────┐    │   mTLS  │ ┌──────────────┐    │
   │ │ boi-core     │◄───┼─────────┼─┤ boi-core     │────┼─────────┼─┤ boi-core     │    │
   │ │  dispatcher  │    │ gRPC    │ │  dispatcher  │    │         │ │  dispatcher  │    │
   │ │  router      │    │         │ │  router      │    │         │ │  router      │    │
   │ │  cluster-svc │    │         │ │  cluster-svc │    │         │ │  cluster-svc │    │
   │ └─────┬────────┘    │         │ └─────┬────────┘    │         │ └─────┬────────┘    │
   │       │ Unix sock   │         │       │             │         │       │             │
   │ ┌─────┴────────┐    │         │  ┌────┴────────┐    │         │  ┌────┴────────┐    │
   │ │ workspace pl │    │         │  │ workspace pl│    │         │  │ workspace pl│    │
   │ │ pool plugin  │    │         │  │ pool plugin │    │         │  │ pool plugin │    │
   │ │ router plgn  │    │         │  │ (no router) │    │         │  │ (no router) │    │
   │ │ hooks plugin │    │         │  │ hooks plgn  │    │         │  │ hooks plgn  │    │
   │ │ provis. plgn │    │         │  │             │    │         │  │             │    │
   │ └──────────────┘    │         │  └─────────────┘    │         │  └─────────────┘    │
   └─────────────────────┘         └─────────────────────┘         └─────────────────────┘
       caps: mac,arm64                  caps: linux,x86               caps: linux,x86,gpu
```

A task flows end-to-end like this:

1. **Dispatch.** A user runs `boi dispatch spec.yaml` against any node (N1, say). N1's core writes the spec body to `/boi/specs/{spec_id}` and enqueues a task envelope under `/boi/dispatch-queue/{task_id}` with `state=PENDING`.
2. **Router.** N1's core invokes the Router plugin (`Route(task, snapshot)`) which returns a routing intent (e.g. "needs caps={linux,gpu}"). Router plugins are stateless and advisory; in the default reference Router they just return `task.requires` verbatim.
3. **Assignment (HRW).** Core filters the membership snapshot by capability, computes HRW scores over candidate node IDs, picks the highest, and attempts a CAS write on `/boi/claims/{task_id}` with the candidate's node ID and a 30 s lease. On collision (another node won), retry next-best. If zero capable nodes: invoke Provisioner (§8).
4. **Claim.** The CAS write succeeds — N3 now "owns" the task. N1's core writes `/boi/dispatch-queue/{task_id}.state=CLAIMED` and gRPC-pushes an `ExecuteTask(envelope)` to N3.
5. **Worker.** N3's core hands the workspace setup to the Workspace plugin (`Prepare(spec_id) → workdir`), then asks the Pool plugin to spawn (`Spawn(workdir, env) → worker_handle`). The Pool plugin runs `claude -p` (or whatever the pool backend is) and reports streaming status to core.
6. **Completion.** When the worker exits, the Pool plugin returns `Result{exit_code, stdout_ref, ...}`. N3's core writes `/boi/dispatch-queue/{task_id}.state=DONE`, releases the claim lease, and fires Hooks plugin events (`OnTaskComplete`).
7. **State update.** Any node watching `/boi/dispatch-queue/` sees the transition; the originating CLI gets the result via a long-lived watch its node opened on dispatch.

Routers, Provisioners, Workspaces, and Hooks plugins are co-located with `boi-core` on each node and addressed over a local Unix socket. (Pool plugins are also local but may delegate to remote compute; that's a Pool-internal concern.) Only `boi-core` ever speaks etcd.

## 4. Cluster state model

All cluster state lives in etcd under `/boi/`. BOI core is the *only* etcd client. Plugins read/write state by calling BOI core's gRPC services.

| Key prefix             | Purpose                                          | Reader            | Writer           | Schema                                 | Primitive             | TTL    |
|------------------------|--------------------------------------------------|-------------------|------------------|----------------------------------------|-----------------------|--------|
| `/boi/nodes/{node_id}` | Node liveness + identity                          | All core nodes    | Owning node only | `{node_id, addr, version, started_at}` | Lease + watch         | 15 s   |
| `/boi/caps/{node_id}`  | Capability advertisement                          | All core nodes    | Owning node only | `{static:{os,arch,region,...}, dynamic:{workers_busy,workers_max,health}}` | Lease + watch | 15 s |
| `/boi/claims/{task_id}`| "Who owns executing this task right now"          | Routers, monitors | Assigning node   | `{node_id, claimed_at, lease_id, attempt}` | CAS + lease           | 30 s   |
| `/boi/specs/{spec_id}` | Spec body (YAML) for dispatched specs             | Assigned node     | Dispatching node | `{yaml_bytes, sha256, dispatched_by}`  | Range read            | none   |
| `/boi/dispatch-queue/{task_id}` | Task envelope + lifecycle state          | All core nodes    | State-machine owner (see "State-machine ownership" immediately below) | `{spec_id, task_id, state, requires, attempts, last_error, state_version: u64, claimant_node_id?: string, claim_lease_id?: i64}` | Watch + Txn-CAS on `state_version` | none |
| `/boi/provision-req/{req_id}` | Outstanding provision requests             | All core nodes    | Router-issuing node | `{req_id, cap_hint, requested_at, fulfilled_by?}` | Lease + watch | 5 min |
| `/boi/join-tokens/{token_id}` | One-shot bearer tokens for node admission | Joining-node-bound core | Issuing core | `{token_id_hash, cap_hint, expires_at, used_at?}` | CAS, single-use | 10 min |
| `/boi/cluster/ca`      | Cluster CA cert (rotated yearly)                  | All core nodes    | Cluster admin (`boi cluster ca rotate`) | `{cert_pem, fingerprint}` | Range read | none |

**State-machine ownership for `/boi/dispatch-queue/{task_id}`:**
- `PENDING → CLAIMED`: dispatching-node writes. The etcd Txn predicate is `compare(value.state_version == N)` then `put(value.state_version = N+1, value.state = CLAIMED, value.claimant_node_id = <id>, value.claim_lease_id = <lease_id>)`.
- `CLAIMED → RUNNING`: assigned-node writes when worker spawned. Same `state_version` CAS pattern.
- `RUNNING → DONE | FAILED`: assigned-node writes on worker exit. Same CAS pattern.
- `CLAIMED → PENDING` (re-queue): any monitor, only after observing `/boi/claims/{task_id}` lease expired. The Txn predicate is `compare(value.state_version == N AND value.state == CLAIMED)` then `put(value.state_version = N+1, value.state = PENDING, value.claimant_node_id = "", value.claim_lease_id = 0)`. The `state_version` epoch makes every state-machine transition serial and observable; stale writers see `VersionConflict` and abort. (F-03.)

**Capability vocabulary.** `/boi/caps/{node_id}.static` keys are partitioned into a reserved namespace and a user namespace:
- *Reserved* (BOI core writes only): `os` ∈ {linux, darwin, windows}; `arch` ∈ {x86_64, arm64}; `region` (RFC-1123 label); `runtime` (Pool plugin's self-declared runtime name, e.g. `claude`, `goose`).
- *User-defined*: keys MUST be prefixed `x-<vendor>-<tag>`, value is opaque UTF-8 ≤256 B. The Router's `requires` filter is exact-match on key=value with set semantics (a task's `requires={os:linux, x-meta-scm:y}` matches a node iff each key/value pair is present on the node). (F-14.)

**The Provisioner plugin does NOT appear anywhere in the writer column.** When the Provisioner needs to bind a new node into the cluster, its only handle is the join token returned by `boi-core`. The Provisioner-issuing core writes `/boi/join-tokens/` and `/boi/provision-req/`; the Provisioner plugin reads neither. (Schema blended from Charlie §1 and Alpha §6; provisioner isolation from Judge 3's onboarding-cliff finding.)

## 5. Plugin contracts

All plugins are HashiCorp-style gRPC sidecars (SC-1). They run as child processes of `boi-core` on the same host and communicate over a Unix-domain socket. Core supplies each plugin a unique `plugin_id`, a `BOI_PLUGIN_SOCKET` env var, and a per-invocation correlation token. Plugins return health on a sidecar gRPC channel.

Common lifecycle:
- **Start:** core launches the plugin binary; expects the literal token `BOI_READY\n` on stdout within `plugin.ready_timeout_secs` (default 10 s, per-plugin override in `boi.toml`). Stderr is captured but does not trigger readiness. (F-11.)
- **Health-check:** core calls `Health(ping)` every 10 s. Three consecutive failures → plugin marked unhealthy. Marking unhealthy flips the node's `caps.dynamic.health=degraded` within ≤2 s (one lease-renewal cycle). (F-11; also resolves B9.)
- **Restart:** on health failure, core kills the plugin and re-launches with **fixed** retry budget — up to 3 re-launches in a 5-minute window. After the budget is exhausted, the plugin is marked `unstable` and core stops restarting it until the operator runs `boi plugin restart <name>` or the 5-minute window elapses. Exponential backoff (the earlier draft's escalation curve) is removed; one mechanism only. (F-20.)
- **Shutdown:** core sends `SIGTERM`, waits 5 s, escalates to `SIGKILL`.

**Identification & correlation.** Core supplies each plugin process:
- `plugin_id` = `<plugin-name>-<host-uuid>` (env var `BOI_PLUGIN_ID`), unique for the process lifetime.
- `BOI_PLUGIN_SOCKET` = path to the Unix-domain socket the plugin must dial back on.
- A per-RPC correlation token in gRPC metadata key `boi-corr-id`. Plugins MUST echo this value in their structured-log lines (key `corr_id`) so logs cross-correlate with core. (F-11, C1.)

What plugins CANNOT see (the universal blacklist):
- etcd endpoints, etcd credentials, etcd keys.
- Other plugins' invocation history.
- Other nodes' identities, except by node_id strings core hands them.

### 5.1 Workspace

```proto
service Workspace {
  rpc Prepare(PrepareRequest) returns (PrepareResponse);
  rpc Cleanup(CleanupRequest) returns (CleanupResponse);
  rpc Health(Ping) returns (Pong);
}
message PrepareRequest {
  string spec_id = 1;
  bytes spec_yaml = 2;          // core delivers it, plugin doesn't fetch
  map<string,string> hints = 3; // e.g. {"git_ref": "main"}
}
message PrepareResponse { string workdir_path = 1; map<string,string> env = 2; }
```

**Hello world (git-worktree):**
```
core → Prepare(spec_id="s1", spec_yaml=<...>, hints={git_ref:"main"})
plugin runs: git worktree add /tmp/boi/s1 main
plugin → workdir_path="/tmp/boi/s1"
```

Sees: spec_yaml, hints. CANNOT see: cluster topology, other specs, etcd. (Terminology compatible with `workspace-backends.md`.)

### 5.2 Pool

```proto
service Pool {
  rpc Spawn(SpawnRequest) returns (stream WorkerEvent);
  rpc Kill(KillRequest) returns (KillResponse);
  rpc Health(Ping) returns (Pong);
}
message SpawnRequest {
  string task_id = 1;
  string workdir_path = 2;
  map<string,string> env = 3;
  bytes prompt = 4;
}
message WorkerEvent {
  oneof kind { Started s = 1; Stdout o = 2; Stderr e = 3; Exit x = 4; }
}
```

**Hello world (local-claude pool):**
```
core → Spawn(task_id="t1", workdir="/tmp/boi/s1", prompt=<...>)
plugin spawns: claude -p --cwd /tmp/boi/s1
plugin streams stdout chunks; emits Exit{code:0} when done
```

Sees: workdir, env, prompt. CANNOT see: assignment decision, etcd, other tasks. (Compatible with `worker-pool-providers.md`.)

**Idempotency contract (load-bearing, F-05).** Pool plugins MUST treat `Spawn(task_id=X)` as idempotent for the lifetime of a claim. A second `Spawn(X)` arriving while a prior `Spawn(X)` is running MUST return the existing worker handle (re-attaching the `WorkerEvent` stream), not spawn a duplicate. After the prior worker has exited, a second `Spawn(X)` MAY launch a fresh worker; core only re-issues `Spawn(X)` when the claim has been re-acquired (new `lease_id`) after lease expiry. The plugin-host conformance harness (§11, `boi plugin test`) exercises this with a synthetic double-Spawn and fails the plugin if a second process group is created.

**Fencing semantics (load-bearing, F-02).** Every state-changing call core makes into the Pool (`Spawn`, `Kill`, result writes back to etcd) carries the claim's `lease_id` as the fencing token. The Pool plugin MUST attach `lease_id` as gRPC metadata key `boi-claim-lease` on any callback into core. Core rejects (and logs) any callback whose `boi-claim-lease` does not match the currently-held lease for that `task_id`. Result writes to `/boi/dispatch-queue/{task_id}` are issued by core inside an etcd Txn whose predicate is `compare(claim_lease_id == <expected>)`; on mismatch the write is dropped and the worker is signaled to abort via Pool `Kill`. This kills A2's dual-ownership window: a stale worker may compute, but it cannot commit.

### 5.3 Router

```proto
service Router {
  rpc Route(RouteRequest) returns (RouteResponse);
  rpc Health(Ping) returns (Pong);
}
message RouteRequest {
  string task_id = 1;
  TaskRequirements requires = 2;  // parsed from spec
  ClusterSnapshot snapshot = 3;   // capability-stripped view supplied by core
}
message RouteResponse {
  TaskRequirements effective_requires = 1; // possibly modified
  repeated string preferred_node_ids = 2;  // hints; core still HRW-selects
}
```

The snapshot core hands the Router contains only `(node_id, static_caps, dynamic_caps_summary)` triples — no identities, addresses, or claim state. The Router's preferred-list is advisory; core still applies HRW for determinism (LD-6).

**Hello world (passthrough router):** returns `requires` unchanged; `preferred_node_ids` empty.

### 5.4 Provisioner

```proto
service Provisioner {
  rpc Allocate(AllocateRequest) returns (AllocateResponse);
  rpc Deallocate(DeallocateRequest) returns (DeallocateResponse);
  rpc Health(Ping) returns (Pong);
}
message AllocateRequest {
  string request_id = 1;
  CapabilityHint hint = 2;       // os, arch, runtime requirements
  string join_token = 3;         // OPAQUE bearer — Provisioner does not parse
  string boi_bootstrap_url = 4;  // URL the new node hits to join (core's address)
  google.protobuf.Duration deadline = 5;
}
message AllocateResponse {
  string allocation_id = 1;
  // No etcd info, no cluster info; just plugin's own infra handle.
}
```

**Critical:** the Provisioner gets `join_token` and `boi_bootstrap_url`. It DOES NOT receive etcd endpoints, etcd certs, or `/boi/...` keys. The newly-allocated node boots, calls the bootstrap URL with the token, and boi-core on the bootstrap node mints its certs and registers it in etcd. (Fixes Judge 3 §4 "etcd onboarding cliff.")

**Security note (F-21).** The `join_token` is a short-lived bearer credential whose blast radius is "one node join, then expires." Provisioner plugins MUST NOT log `join_token` or `boi_bootstrap_url` to any sink outside the plugin process. Core tightens token TTL from 10 min to **5 min** and binds it: the mint request takes a `mint_for=<sha256 of expected node fingerprint or provisioner-supplied nonce>` field; `/v1/join` rejects tokens whose payload binding does not match the joining node. The plugin-host audits the Provisioner's stdout/stderr for substring match on the token and emits a `provisioner.token_leak_suspected` Hooks event if detected (best-effort; not a security control on its own). The infra the Provisioner controls is implicitly trusted to receive the token — operators choosing untrusted Provisioner infrastructure remain responsible for that trust boundary; this is documented, not enforced, in v0.1.

**Hello world (fly-machines provisioner):**
```
core → Allocate(hint={os:linux,arch:x86}, join_token="opaque-32-bytes",
                boi_bootstrap_url="https://n1.boi.local:4400/join")
plugin runs: fly machine run boi:latest \
   --env BOI_JOIN_TOKEN=opaque-32-bytes \
   --env BOI_BOOTSTRAP_URL=https://n1.boi.local:4400/join
plugin → allocation_id="fly-mach-abc"
```

### 5.5 Hooks

```proto
service Hooks {
  rpc OnEvent(Event) returns (EventAck);
  rpc Health(Ping) returns (Pong);
}
message Event {
  string kind = 1;             // "task.dispatched", "task.completed", "node.joined", ...
  google.protobuf.Timestamp ts = 2;
  google.protobuf.Struct payload = 3;
}
```

Hooks plugins are fire-and-forget for non-critical observability/automation. Core retries delivery once on transient error; persistent failure logs but does not block the originating workflow.

**Event kinds (canonical enum, F-15).** Core emits exactly these `kind` strings in v0.1; Hooks authors writing audit-grade consumers can rely on this list being exhaustive within a minor version:

| `kind`                       | When                                                                 | Payload keys                                  |
|------------------------------|----------------------------------------------------------------------|-----------------------------------------------|
| `task.dispatched`            | Spec dispatched; envelope written to `/boi/dispatch-queue/`          | `task_id, spec_id, requires, dispatcher_node` |
| `task.claimed`               | CAS on `/boi/claims/` succeeded                                      | `task_id, claimant_node, lease_id`            |
| `task.started`               | Pool reported `Started`                                              | `task_id, worker_handle`                      |
| `task.completed`             | Worker exited with `code=0`                                          | `task_id, duration_ms`                        |
| `task.failed`                | Worker exited non-zero OR claim aborted                              | `task_id, exit_code, last_error`              |
| `task.reassigned`            | Claim re-queued after lease expiry                                   | `task_id, prior_claimant, attempt`            |
| `node.joined`                | New node passed `/v1/join` and wrote `/boi/nodes/`                   | `node_id, declared_caps`                      |
| `node.drained`               | `boi node drain` completed                                           | `node_id`                                     |
| `node.crashed`               | Node lease expired without `drained` event                           | `node_id, last_seen`                          |
| `node.degraded`              | `caps.dynamic.health` flipped to `degraded`                          | `node_id, reason`                             |
| `provision.requested`        | Provisioner.Allocate dispatched                                      | `req_id, hint`                                |
| `provision.fulfilled`        | New node joined that matches an outstanding request                  | `req_id, node_id, latency_ms`                 |
| `provision.failed`           | Deadline elapsed or Deallocate called                                | `req_id, reason`                              |
| `cluster.ca_rotated`         | `boi cluster ca rotate` completed                                    | `new_fingerprint`                             |
| `cluster.partition_detected` | `boi_core_etcd_unreachable_seconds > 0`                              | `since_ts`                                    |
| `cluster.partition_healed`   | etcd reachable again                                                 | `duration_s`                                  |

**Hello world (slack-notifier):** subscribes to all `task.*` kinds; posts to a webhook when `kind == "task.failed"`.

## 6. Node lifecycle

### Bootstrap (first node)

1. Operator runs `boi cluster init --etcd-endpoints=...` on the seed machine.
2. boi-core generates a self-signed cluster CA (or imports a provided one) and stores it at `/boi/cluster/ca` (after verifying the prefix is empty).
3. Core mints its own node cert from the CA, persists it locally at `~/.boi/certs/`.
4. Core writes `/boi/nodes/{node_id}` and `/boi/caps/{node_id}` with a 15 s lease, starts the lease-renewal loop.
5. Core starts listening on `BOI_BOOTSTRAP_URL` (cluster-internal port for join requests).

### Join (new node, including provisioned ones)

1. New node boots holding `BOI_JOIN_TOKEN` + `BOI_BOOTSTRAP_URL` (manual paste, env var, or set by the Provisioner).
2. New node's core calls `POST {bootstrap}/v1/join` with `{token, hostname, declared_caps}` over TLS pinned to the cluster CA fingerprint. **CA fingerprint provisioning (resolves F-04, supersedes Q5):** the cluster CA's SHA-256 fingerprint is embedded in the signed `join_token` payload itself (the token is a JWT signed with the cluster CA's private key, payload `{token_id, mint_for, expires_at, ca_fingerprint}`). The new node parses the token, extracts the fingerprint, and uses it to pin the TLS handshake against `/v1/join`. The bootstrap server presents the cert chain; the joining node verifies (a) chain anchors at a CA whose fingerprint matches the token payload, and (b) the token signature verifies against that CA's public key. Manual joins (where the operator types the token at a CLI prompt) MAY accept a `--ca-fingerprint` flag as a redundant out-of-band check. There is no TOFU window. (F-04.)
3. Issuing core validates the token via CAS-delete on `/boi/join-tokens/{id}` (single-use), mints a node cert signed by the cluster CA, returns `{node_cert, ca_chain, etcd_endpoints, node_id}`.
4. New node writes `/boi/nodes/{node_id}` + `/boi/caps/{node_id}` with lease, advertises capabilities, transitions to `READY`.
5. New node's first lease renewal serves as the dispatch-readiness signal — at that point HRW will start placing tasks there.

### Leave (graceful + crash)

- **Graceful (`boi node drain`):** core stops accepting new claims, waits for in-flight workers to complete (or hits deadline), explicitly revokes the node's lease, removes `/boi/nodes/{id}` and `/boi/caps/{id}`.
- **Crash:** etcd lease expires after 15 s → keys vanish → any task with `/boi/claims/{task_id}` pointing at the dead node is detected by the monitor, which CAS-transitions `/boi/dispatch-queue/{task_id}` from `CLAIMED → PENDING`. The HRW will likely pick a different node next round (membership changed).

### Failure detection

Liveness is the etcd lease TTL on `/boi/nodes/{id}` — **hardcoded 15 s with 5 s heartbeat (3× safety)**. The per-deployment `node.lease_ttl_secs` knob from the earlier draft is removed (F-18); v0.1 is a LAN/datacenter design (LD-7 trusted cluster), and one TTL keeps the failure-detection story uniform. False positives are minimized by:
1. Heartbeats are sent every 5 s, so two consecutive misses are tolerated.
2. The lease-renewal client retries on transient errors before giving up.
3. Plugin daemon crashes are independent of node liveness — a dead Pool plugin does not expire the node's lease, only flips `caps.dynamic.health` to `degraded` within ≤2 s (Judge 4 §7 mitigation, B9 fix).

**Per-node consecutive-claim-failure cooldown (F-06).** Each `/boi/nodes/{id}` record carries a `consecutive_claim_failures: u32` counter that core increments when a node accepts a claim but fails to advance the task to `RUNNING` within `claim.activation_deadline_secs` (default 30 s, == claim lease TTL). After 3 consecutive failures, core flips `caps.dynamic.health=degraded` for a 5-minute cooldown window; the HRW filter skips degraded nodes. The counter resets on a successful `RUNNING` transition or on cooldown expiry. This kills the Provisioner reassignment-loop (A4): a flapping provisioned node is demoted instead of being re-picked indefinitely.

### Certificate rotation (F-09)

CA and node-cert rotation in v0.1 is **operator-initiated and online**:
1. `boi cluster ca rotate --plan` prints the rotation steps and the dual-trust window expiry.
2. `boi cluster ca rotate --execute` writes a new CA cert under `/boi/cluster/ca-next/`. All nodes' core processes watch this prefix; on update, each loads the new CA into a *secondary trust pool* (TLS handshakes now accept either CA). This is the dual-trust window, **default 24 h, configurable via `--trust-window`**.
3. Within the trust window, the operator runs `boi node cert renew` on each node in turn (`boi cluster nodes` lists them in rotation order). Each invocation has core re-mint its node cert against the new CA and atomically swap it.
4. `boi cluster ca rotate --finalize` promotes `ca-next` to `ca`, retires the old CA, and emits `cluster.ca_rotated`. Must be invoked before the trust window expires; otherwise nodes that have not yet renewed will fail mTLS after expiry.
5. **Abort path:** `boi cluster ca rotate --abort` deletes `/boi/cluster/ca-next/` and emits a `cluster.ca_rotated` event with `reason=aborted`. Any nodes that already renewed will retain the dual-trust pool until the next rotation; their certs remain valid under the old CA chain.

etcd's own server certs are rotated separately by the etcd operator (out of scope; documented as a runbook prerequisite). The `boi cluster ca days-remaining` gauge fires a warning at 30 days and a critical at 7 days.

### Rolling upgrade (F-10)

v0.1 ships **dispatch-pause rolling upgrades**, not zero-downtime hot upgrades (N6 stands).
1. `boi cluster pause-dispatch` flips a `/boi/cluster/dispatch_paused=true` flag. Cores observing this stop accepting new claims (in-flight work continues).
2. Operator drains and upgrades nodes one at a time: `boi node drain && systemctl restart boi && boi node start`.
3. After all nodes report the target version, `boi cluster resume-dispatch` clears the flag.
4. New dispatches issued during the pause window receive `EtcdReachableButPaused` with retry guidance; this is loud, not silent.
5. **Version skew band (F-23).** Every `/boi/nodes/{id}` carries `version: semver`. Core refuses to elect itself as dispatcher (refuses to mint claims) if any active node's version differs by more than ±1 minor within the same major (e.g. v0.1.x ↔ v0.2.x is permitted; v0.1.x ↔ v0.3.x is not). The `boi cluster status` command prints the skew band and the offending nodes.

## 7. Task assignment algorithm

Pseudocode (Rust-ish; runs in `boi-core` on the dispatching node):

```
fn assign(task: &Task, snapshot: &ClusterSnapshot) -> AssignResult {
    // 1. capability filter
    let candidates: Vec<&Node> = snapshot.nodes.iter()
        .filter(|n| satisfies(n.caps.static_, &task.requires))
        .filter(|n| n.caps.dynamic.workers_busy < n.caps.dynamic.workers_max)
        .filter(|n| n.caps.dynamic.health == Health::Ok)
        .collect();

    if candidates.is_empty() {
        return AssignResult::NoCapableNode;  // → §8 provisioning
    }

    // 2. HRW score (Alpha §3 algorithm, applied to etcd snapshot)
    let mut scored: Vec<(u64, &Node)> = candidates.iter()
        .map(|n| (hrw_score(&task.task_id, &n.node_id), *n))
        .collect();
    scored.sort_unstable_by(|a, b| b.0.cmp(&a.0));   // descending
    // Tie-break: lexicographic node_id ascending (deterministic).
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.node_id.cmp(&b.1.node_id)));

    // 3. claim attempt with retry-next-best
    for (_, candidate) in &scored {
        let claim = ClaimRecord {
            node_id: candidate.node_id.clone(),
            claimed_at: now(),
            attempt: task.attempts + 1,
        };
        match etcd_cas_put(
            key: format!("/boi/claims/{}", task.task_id),
            expected_version: 0,                 // key does not exist
            value: serialize(&claim),
            lease_ttl: 30s,
        ) {
            Ok(lease_id) => return AssignResult::Claimed { node: candidate.node_id.clone(), lease_id },
            Err(VersionConflict) => continue,    // someone else claimed; try next-best
            Err(other) => return AssignResult::TransientError(other),
        }
    }
    // every candidate already claimed (saturated cluster)
    AssignResult::AllCandidatesClaimed
}

fn hrw_score(task_id: &str, node_id: &str) -> u64 {
    // SipHash-2-4 of (task_id, node_id) — deterministic, no shared state
    siphash24(b"BOI-HRW-v1", &[task_id.as_bytes(), node_id.as_bytes()].concat())
}
```

**What HRW provides, and what actually makes assignment correct (F-01).** HRW gives **load-distribution stability** — under any given membership snapshot, tasks distribute across capable nodes with low variance, and small membership changes perturb assignments minimally. HRW does *not* by itself guarantee that only one node executes a task. Assignment **correctness** rests entirely on the CAS write to `/boi/claims/{task_id}`: at most one writer can put the key with `expected_version=0`. If two nodes compute different preferences (because they read at different etcd revisions, or one is using a stale degraded-mode cache), the CAS still ensures exactly one winner; the loser observes `VersionConflict` and falls back to its next-best candidate or re-queues. The lexicographic node_id tie-break is a footnote — it deterministically resolves the ≈2⁻⁶⁴ hash collision, which is unobservable in this lifetime at expected cluster sizes (F-D5/D5 simplification).

**Snapshot revision pinning.** Optional and tracked as Q1: in the strictest mode, `assign()` reads the etcd snapshot at revision R, and the claim CAS includes `compare(mod_revision(/boi/nodes/) <= R + tolerance)`. The implementation plan picks one of {strict / tolerance window / no pin} via measurement in week 3 of v0.1; the design does not depend on which.

If `NoCapableNode`: emit a provision request (§8) and re-enqueue. If `AllCandidatesClaimed`: re-enqueue with `pending_until=now+1s` and retry.

## 8. Provisioning flow

```
┌────────────┐     ┌──────────┐    ┌─────────┐    ┌──────────────┐     ┌────────────┐
│ Dispatcher │     │  Router  │    │  core   │    │  Provisioner │     │  New node  │
│  (node N1) │     │ (plugin) │    │ (on N1) │    │  (plugin)    │     │  (booting) │
└──────┬─────┘     └────┬─────┘    └────┬────┘    └───────┬──────┘     └─────┬──────┘
       │                │               │                 │                  │
       │ assign() →     │               │                 │                  │
       │ NoCapableNode  │               │                 │                  │
       │───────────────►│               │                 │                  │
       │                │ provision_req │                 │                  │
       │                │──────────────►│                 │                  │
       │                │               │ mint join_token │                  │
       │                │               │ write           │                  │
       │                │               │ /boi/join-tokens│                  │
       │                │               │ write           │                  │
       │                │               │ /boi/provision-req                 │
       │                │               │ Allocate(token, hint, bootstrap)   │
       │                │               │────────────────►│                  │
       │                │               │                 │ alloc infra      │
       │                │               │                 │ boot image       │
       │                │               │                 │─────────────────►│
       │                │               │                 │                  │ POST /join
       │                │               │◄─────────────────────────────────  │ (token)
       │                │               │ validate, mint cert, write nodes/  │
       │                │               │─────────────────────────────────►  │
       │                │               │                 │                  │ ready+lease
       │                │               │ watch /boi/caps/ fires             │
       │                │               │◄─────────────────────────────────  │
       │                │ re-route()    │                 │                  │
       │                │◄──────────────┤                 │                  │
       │ assign() →     │               │                 │                  │
       │ Claimed(newN)  │               │                 │                  │
       │◄───────────────│               │                 │                  │
```

Key invariant (Judge 3 fix): the Provisioner only ever holds an opaque `join_token` and a `bootstrap_url`. It cannot read or write etcd. The join-token is single-use (CAS-delete on consumption) and TTL'd (10 min), so a leaked token cannot be replayed indefinitely.

If the provisioned node does not call `/join` within `hint.deadline`, the dispatching core marks `/boi/provision-req/{id}.fulfilled_by=null`, calls `Provisioner.Deallocate(allocation_id)` defensively, and re-attempts (with operator-configurable retry cap). This closes the "silent VM leak" gap noted in Judge 4 §3.

## 9. Degraded mode (etcd unavailable)

**Invariant:** during an etcd partition, BOI promises that no in-flight task is silently lost and no new task is silently queued.

Each `boi-core` maintains a **membership cache** populated from a long-lived etcd watch on `/boi/nodes/` and `/boi/caps/`. The cache has a TTL of 30 s from last successful refresh.

Behavior:

| Operation                                       | etcd reachable | etcd unreachable, cache fresh (<30 s) | etcd unreachable, cache stale (≥30 s) |
|-------------------------------------------------|----------------|---------------------------------------|---------------------------------------|
| In-flight worker (already claimed) continues    | yes            | yes — local execution does not need etcd | yes — but `/boi/dispatch-queue` state update will fail at completion; core buffers the result locally in `~/.boi/pending-flush/` and surfaces a loud "result unflushed" warning |
| New dispatch (`boi dispatch`)                   | yes            | **FAIL LOUDLY** — return `EtcdUnreachable` with retry guidance | **FAIL LOUDLY** — same |
| Claim renewal heartbeat                         | yes            | FAIL — claim lease will expire, monitor re-queues task elsewhere when partition heals | FAIL |
| Status query (`boi status`)                     | yes            | served from cache with `stale` flag    | refuses, returns `EtcdUnreachable` |
| Hooks plugin event delivery                     | yes            | yes (local; etcd not required)         | yes |

Observability lights up: `boi_core_etcd_health` Prometheus gauge flips to 0; `boi_core_etcd_unreachable_seconds` counter increments; structured log line `etcd_unreachable=true since=<ts>` is emitted every 5 s on every node. The CLI prints `WARN: etcd unreachable; new dispatches will fail` on every command.

**Diagnostic CLI under outage (F-22).** Read-only diagnostic commands accept a `--stale-ok` flag and a `--local` variant that serves from the membership cache regardless of staleness, stamping the output with `cached_at=<ts>` and `stale_age=<duration>`. In particular `boi cluster status --local` always returns *something* — last-known nodes, last-known capabilities, last-known claims — so the on-call operator is never holding a useless terminal.

**Pending-flush buffer semantics (F-08).** Result writes that fail during partition buffer to `~/.boi/pending-flush/<node_id>.jsonl`. Concrete spec:
- One JSONL file per node, append-only.
- Max size **100 MB** (configurable via `cluster.pending_flush_max_bytes`). Oldest entries are evicted first on overflow; eviction emits a critical log line and a `boi_core_pending_flush_evicted_total` counter increment.
- On etcd recovery, entries are flushed oldest-first as state-machine writes into `/boi/dispatch-queue/`. Each flush attempt is an etcd Txn with the original `state_version` predicate; if the predicate fails (someone re-queued the task), the entry is logged with `reason=superseded` and dropped. At-least-once semantics overall.
- `boi node drain` refuses to proceed while the buffer is non-empty unless `--force-drop-buffer` is passed (with confirmation prompt). Buffer is not migrated to another node; it is local-only state and only meaningful for tasks that node was running.

**Operator escape valve (F-07).** `boi cluster local-fallback <node_id>` is an explicit, operator-invoked command that:
1. Drains the named node (refuses new claims).
2. Persists in-flight claim records and dispatch envelopes to `~/.boi/pending-flush/local-fallback-<ts>.jsonl`.
3. Switches the local core into single-node mode with a banner warning on every CLI invocation.
4. Logs `cluster.local_fallback_engaged` for audit.
This is the documented "etcd is broken, get me out" path. It is never automatic and emits a `cluster.local_fallback_engaged` Hooks event so monitoring systems know the cluster shrunk.

**Metrics catalog (F-12).** Every detection mechanism cited in §10 is backed by a named metric. Minimum v0.1 surface (all Prometheus, namespaced `boi_core_`):

| Metric                                | Type    | Labels                       | Raised by                                                  |
|---------------------------------------|---------|------------------------------|-----------------------------------------------------------|
| `boi_core_etcd_health`                | gauge   | —                            | etcd reachable=1 / unreachable=0                          |
| `boi_core_etcd_unreachable_seconds`   | counter | —                            | increments while etcd unreachable                         |
| `boi_core_claim_lease_expired_total`  | counter | `task_id, prior_claimant`    | monitor observed lease expiry on `/boi/claims/`           |
| `boi_core_hrw_cas_retry_total`        | counter | `task_id`                    | `VersionConflict` on claim CAS triggered next-best        |
| `boi_core_provision_req_latency_seconds` | histogram | `provisioner_name`        | from `provision.requested` to `provision.fulfilled`       |
| `boi_core_plugin_restart_total`       | counter | `plugin_name, plugin_kind`   | plugin re-launched after health failure                   |
| `boi_core_plugin_unstable`            | gauge   | `plugin_name`                | plugin marked `unstable` after 3 restarts in 5 min        |
| `boi_core_dispatch_queue_state_count` | gauge   | `state`                      | range-count of queue entries per state                    |
| `boi_core_pending_flush_bytes`        | gauge   | —                            | size of `~/.boi/pending-flush/` on disk                   |
| `boi_core_pending_flush_evicted_total`| counter | —                            | eviction on buffer overflow                               |
| `boi_core_consecutive_claim_failures` | gauge   | `node_id`                    | per-node F-06 counter                                     |
| `boi_core_node_skew_violations`       | gauge   | `local_version, peer_version`| version-skew check (F-23) refused dispatch                |

What's explicitly **not** done in v0.1 (LD-4): no local queueing of new dispatches for later replay; no peer-to-peer fallback membership view; no automatic claim renegotiation across the partition. Outages are assumed rare and short — operators should fix etcd, not extend BOI's degraded surface.

## 10. Failure modes table

Covers the 8 scenarios from `meta-judge-4-failures.md` plus 4 synthesis-specific additions.

| # | Scenario                                                   | Detection                                  | Recovery                                                       | TTR        | Worst case                                          |
|---|------------------------------------------------------------|--------------------------------------------|----------------------------------------------------------------|------------|----------------------------------------------------|
| 1 | Dispatching node crashes mid-assignment                    | Claim lease (30 s) expires on `/boi/claims/{tid}` | Monitor CAS-transitions task back to PENDING; HRW re-runs       | ≤30 s      | Task waits up to lease TTL before being reassignable |
| 2 | Network partition splits BOI cluster                       | etcd quorum side stays authoritative; minority's BOI cores time out on etcd writes | Minority cores fence themselves (no new claims); majority continues | ≤15 s     | Minority workers continue executing in-flight but their result-flushes buffer locally |
| 3 | Provisioner reports success but new node never joins       | `/boi/provision-req/{id}` lease (5 min) or per-request `deadline` expires | Core calls `Provisioner.Deallocate`, re-issues `Allocate` to next provisioner attempt | ≤ deadline | Bounded VM leak (one allocation) before deallocate is called |
| 4 | Node advertises capability the plugin can't run             | Plugin returns error at Spawn/Prepare time; core flips `caps.dynamic.health=degraded`, lease still alive | Task re-queued (PENDING); next HRW skips degraded; operator notified via Hooks `node.degraded` event | ≤10 s + 1 retry | Capability-fraud not quarantined in v0.1 (N7); task could thrash if operator does not act |
| 5 | Long-running task outlives the node that started it         | Claim lease expires; monitor sees lease gone but `/boi/dispatch-queue/{tid}.state=CLAIMED` | Monitor CAS to PENDING; rerun. Pool plugin's `Spawn` is required to be idempotent on `task_id`, and writes use the `lease_id` as a fencing token | ≤30 s      | Side-effects of the zombie worker (filesystem, external APIs) may double-occur; etcd writes from zombie rejected via fencing |
| 6 | Clock skew between BOI nodes                                | etcd server is the clock authority; client skew affects only log timestamps | None needed                                                    | 0          | Log timestamps misleading; behavior correct (Charlie §6 inherited) |
| 7 | Pool plugin daemon crashes while a worker is running        | gRPC stream breaks; core marks plugin unhealthy; if Pool implements `Reattach(task_id)`, core retries reattach before declaring task failed | If reattach fails: task → FAILED with `last_error=pool_died`; re-queue policy per spec | ≤30 s      | Orphan claude process if Pool was supervising via direct fork; lease still expires |
| 8 | etcd itself unavailable                                     | `boi_core_etcd_health=0`, CLI loud-fail   | Degraded mode (§9); when etcd returns, watches re-sync and ops resume | external   | New dispatches stalled for partition duration; in-flight workers finish but buffer results locally |
| 9 | etcd cert expiry                                            | Connection failures with TLS errors; `boi cluster ca days-remaining` < 30 d gauge | `boi cluster ca rotate` mints a new CA and rolls node certs over a 24 h window via dual-CA trust | ≤24 h (planned) | If unmonitored: full cluster outage like #8 |
| 10 | Plugin daemon flap (crash → restart → crash …)             | Restart-backoff counter exceeds 5 in 5 min → plugin marked `unstable`; `caps.dynamic.health=degraded` | Operator alerted; tasks routed elsewhere; flapping node drained on operator command | ≤5 min      | Local node unusable for affected plugin until operator fixes |
| 11 | Router HRW tie-break collision (two node_ids hash-equal)    | Algorithm tie-break by lexicographic `node_id` (§7) is deterministic; collision invisible to user | Deterministic ordering picks the lex-smaller `node_id`           | 0          | Slight load asymmetry between the two colliding nodes |
| 12 | Lease-expiry race (worker still running when lease lapses)  | etcd reports `LeaseExpired`; Pool's next state write rejected with `RequiredRevision` fencing | Core kills the worker via Pool's `Kill(task_id)`, marks task `PENDING`, HRW re-runs | ≤5 s       | Wasted compute on the old node; result side-effects may occur once |

## 11. What ships in BOI core

**New crates / modules:**
- `boi-cluster` — etcd client wrapper, lease management, watch dispatching, snapshot caching.
- `boi-router` — HRW assignment, candidate filtering, claim CAS protocol.
- `boi-plugin` — plugin host: spawn, health, restart, gRPC mux.
- `boi-bootstrap` — `/v1/join` HTTP handler, join-token mint/validate, cert minting.
- `boi-ca` — internal CA: self-sign, rotate, dual-CA trust window.
- `boi-degraded` — TTL-cache, degraded-mode gauges, result buffer at `~/.boi/pending-flush/`.

**New CLI surface:**
- `boi cluster init | join | status [--local] [--stale-ok] | pause-dispatch | resume-dispatch | local-fallback | ca [rotate|days-remaining]`
- `boi node start | drain | status | cert renew`
- `boi plugin install | list | logs | restart <name> | test <binary>` (F-13: `test` runs the plugin-host conformance harness against a mock-core fixture, exercising the lifecycle and each RPC of the declared plugin kind)
- `boi dispatch <spec.yaml>` (existing, now etcd-aware)
- `boi tasks list | get <task_id>` (now cluster-wide)

**Wire protocols to author:**
- `proto/workspace.proto` — §5.1
- `proto/pool.proto` — §5.2
- `proto/router.proto` — §5.3
- `proto/provisioner.proto` — §5.4
- `proto/hooks.proto` — §5.5
- `proto/bootstrap.proto` — `/v1/join`, `/v1/cert-renew` (internal, not a plugin)

**Breaking changes to existing config / spec format:**
- `spec.requires` (capability expression) becomes a top-level optional field; pre-existing single-node specs without it default to `requires=local` (the implicit local node's auto-tag).
- `boi.toml` (per-node config) gains `[cluster]`, `[plugins]`, `[bootstrap]` sections. Old single-node configs continue to work if `[cluster]` is omitted (single-node degenerate mode in v0.1 — but see Migration §12 for caveat).

**Net-new external dependencies:**
- `etcd` client crate (`etcd-client`, official).
- `rustls` + `webpki` for mTLS.
- `siphasher` for HRW.
- `tonic` (already in scope) for gRPC.

## 12. Migration from single-node BOI

For a user running today's single-node `boi`:

1. Install etcd somewhere reachable (`docker run etcd` for hobbyist; managed etcd or self-hosted 3-node quorum for production).
2. Run `boi cluster init --etcd-endpoints=...` on the existing machine. This converts the local node into the cluster's first member.
3. Existing specs continue to work — without `spec.requires`, they default to `requires=local` and run on the originating node (functionally identical to today).
4. Specs that *want* multi-node behavior add `requires:` and dispatch as usual.

What still works: existing `boi dispatch`, existing Workspace/Pool plugins (rebuilt against new proto), SQLite local result store (now shadowed by etcd state but still authoritative for spec-local artifacts).

What breaks:
- `boi.toml` requires a `[cluster]` section if `boi cluster init` has been run; absent it, the daemon prints a deprecation warning and falls back to single-node.
- Plugins compiled against pre-v0.1 trait bounds must be recompiled against gRPC protos. (One-time, well-documented migration.)
- Direct SQLite-state inspection scripts will not see cluster-wide task state; that now lives in etcd.

What changes (mental model): tasks no longer live where they are dispatched; they live in etcd and run wherever HRW places them. `boi tasks get <id>` is the right replacement for "look at the SQLite row."

## 13. v0.1 scope cut

**In v0.1:**
- etcd-backed cluster state (LD-1, LD-2).
- 5 gRPC plugin contracts (LD-3).
- HRW assignment + CAS claims (LD-6).
- Join-token provisioning (Judge 3 fix).
- Degraded-mode invariant (LD-4).
- mTLS between BOI nodes + cluster CA (LD-7).
- CLI: `boi cluster`, `boi node`, `boi plugin`.

**Deferred to v0.2+ (with justification):**
- **Multi-plugin-of-same-kind routing.** LD-5. Rationale: surface area explosion for marginal user value at v0.1; users wanting two backends run two BOI deployments.
- **Local etcd embedding / SQLite fallback.** N1, LD-2. Rationale: doubling the failure-mode surface (Judge 4) for an ergonomic win that `docker run etcd` already provides.
- **Capability-fraud quarantine.** N7, Judge 4 §4. Rationale: requires a reputation / probation model that is its own design problem.
- **Hot rolling-restart of BOI core without dispatch pause.** N6. Rationale: requires version-handshake + protocol-versioning, which is well-understood but expensive in v0.1.
- **Cross-region affinity.** N5, Judge 5 §"speculative complexity". Rationale: HRW + capability tags suffice for announced workloads; add when a real workload demands it.
- **Plugin discovery service.** N8, Judge 3. Rationale: per-node configuration via `boi plugin install` is simpler and sufficient; a registry is premature.
- **Multi-cluster federation.** Out of v0.1. Rationale: not in shared constraints; ship one cluster well first.
- **Local-replay queueing during etcd partitions.** N3, LD-4. Rationale: reintroduces Alpha-style soft consistency the design explicitly rejected.

**Rough sizing:** v0.1 is approximately **8–10 person-weeks** of work distributed across: cluster module + etcd client (~2 wks), plugin host + 5 proto contracts (~2 wks), HRW + claims + monitor (~1.5 wks), CA + bootstrap + join-token (~1.5 wks), CLI surface (~1 wk), degraded-mode + observability (~1 wk), integration + docs (~1 wk).

## 14. Open questions

The following are concrete decisions the implementation plan must resolve. None of these are settled by this design.

- **Q1. etcd revision pinning in HRW snapshots.** Should `assign()` pin to the etcd `mod_revision` it read, and reject CAS attempts when the revision has advanced beyond a stale window? Trade-off: stricter determinism vs. higher CAS-retry rate under churn. Recommend an experiment in week 3 of v0.1 with two configs.
- **Q2. Worker fencing-token format.** §10 row 5 alludes to using `lease_id` as a fencing token for late writes. The exact mechanism — is it the etcd lease ID, or a separate monotonic per-task counter? — needs design before the Pool proto is frozen.
- **Q3. Join-token issuance authorization.** Today any cluster member can mint join-tokens via `boi node` CLI. Should token-mint authority be restricted to a designated subset (e.g. nodes with capability `cluster.admin`)? Required answer before v0.1 GA.
- **Q4. Plugin protocol versioning.** Does each plugin proto carry a `version` field, with core refusing plugins reporting a major mismatch? Or do we rely on file naming (`workspace.v1.proto`)? Affects breaking-change cadence for plugin authors.
- **Q5. _(Resolved by F-04; see §6 Join — fingerprint embedded in signed join-token payload.)_**
- **Q6. Hooks delivery semantics.** §5.5 says fire-and-forget with one retry. For audit-grade hooks (e.g. SOC2 log shipping), is at-least-once delivery required? If so, do Hooks plugins move into the etcd-backed state plane (likely yes for that subset) and how is "audit hook" declared?
- **Q7. Worker stdout streaming durability.** Pool's `WorkerEvent` stream is in-memory between Pool plugin and core. If the dispatching CLI disconnects, do we tee stdout to etcd, to a local file, or drop it? Affects long-running interactive sessions.

---

## Response to critique

The four-critic adversarial pass (`distributed-architecture-design-critique.md`) produced 24 numbered findings. Disposition for each:

| F-ID | Severity   | Disposition         | Where addressed / Why rejected |
|------|------------|---------------------|-------------------------------|
| F-01 | Blocker    | Addressed           | §7 — rewrote determinism paragraph: HRW provides load-distribution stability only; correctness rests on `/boi/claims/` CAS. §10 row 11 framing kept (collision tie-break is just a footnote). |
| F-02 | Blocker    | Addressed           | §5.2 Pool — added **Fencing semantics** subsection: `lease_id` rides as `boi-claim-lease` gRPC metadata; core enforces via etcd Txn predicate; stale workers cannot commit. §10 row 12 references the same mechanism. |
| F-03 | Blocker    | Addressed           | §4 — added `state_version: u64`, `claimant_node_id`, `claim_lease_id` to dispatch-queue envelope; every state transition is a `compare(state_version == N)` etcd Txn. |
| F-04 | Blocker    | Addressed           | §6 Join — CA fingerprint is embedded in the signed `join_token` payload (JWT signed by cluster CA). New node parses fingerprint from token, pins TLS handshake. No TOFU. Q5 removed from §14. |
| F-05 | Blocker    | Addressed           | §5.2 Pool — added **Idempotency contract** as a normative requirement; `boi plugin test` harness exercises it. |
| F-06 | Blocker    | Addressed           | §6 — added `consecutive_claim_failures` counter on `/boi/nodes/{id}`; 3 failures → 5-min `degraded` cooldown; HRW filter skips degraded nodes. |
| F-07 | Important  | Addressed           | §9 — added `boi cluster local-fallback` operator-invoked escape valve. §11 CLI surface updated. |
| F-08 | Important  | Addressed           | §9 — added full **Pending-flush buffer semantics** subsection: 100 MB cap, oldest-first eviction, drain interaction, at-least-once on recovery. |
| F-09 | Important  | Addressed           | §6 — added **Certificate rotation** subsection with `--plan / --execute / --finalize / --abort` lifecycle and dual-trust window mechanics. |
| F-10 | Important  | Addressed           | §6 — added **Rolling upgrade** subsection: `boi cluster pause-dispatch / resume-dispatch`, version skew band (F-23 also). |
| F-11 | Important  | Addressed           | §5 — specified `BOI_READY\n` token, `plugin.ready_timeout_secs` knob, `BOI_PLUGIN_ID` env, `boi-corr-id` gRPC metadata convention, and that plugin-unhealthy flips `caps.dynamic.health` within ≤2 s (also resolves B9). |
| F-12 | Important  | Addressed           | §9 — added **Metrics catalog** table naming every metric the failure-mode table relies on. |
| F-13 | Important  | Addressed           | §11 — added `boi plugin test <binary>` to CLI surface; runs the plugin-host conformance harness against mock-core. |
| F-14 | Important  | Addressed           | §4 — added **Capability vocabulary** subsection: reserved keys (`os`, `arch`, `region`, `runtime`) vs `x-<vendor>-<tag>` user-defined. |
| F-15 | Important  | Addressed           | §5.5 — added **Event kinds** canonical enum table covering task/node/provision/cluster lifecycle. |
| F-16 | Suggestion | Rejected            | Hooks plugin stays. The §2 goals (G4) and §1 scope ("ships in v0.1: 5 gRPC plugin contracts") explicitly commit to all five plugin types. Removing Hooks would also force re-deriving the event vocabulary in v0.2; deferring strictly costs more than shipping. Structured logs (per F-12 metrics catalog) are *additive*, not a replacement. |
| F-17 | Suggestion | Rejected            | Router plugin stays. Same rationale as F-16: the 5-plugin contract is a §1/§2 commitment. The passthrough default is cheap (one method, one struct); the slot is reserved so that the protocol does not need a breaking v0.2 expansion when a non-passthrough Router is the first real plugin author's request. |
| F-18 | Suggestion | Addressed           | §6 Failure detection — removed `node.lease_ttl_secs` knob; hardcoded 15 s. |
| F-19 | Suggestion | Deferred-to-v0.2    | Schema is a v0.1 wire-protocol commitment; collapsing `/boi/caps/` into `/boi/nodes/` after release would be a breaking change. Logged for v0.2 schema review. We keep the two prefixes in v0.1 for symmetry with `worker-pool-providers.md` terminology. |
| F-20 | Suggestion | Addressed           | §5 lifecycle — removed exponential backoff; one mechanism only (3 restarts / 5 min → `unstable`). |
| F-21 | Important  | Addressed           | §5.4 — added **Security note**: token TTL tightened to 5 min, `mint_for` binding added, Provisioner stdout scanned for token leakage. Operators choosing untrusted Provisioner infra remain responsible (documented, not enforced). |
| F-22 | Important  | Addressed           | §9 — added **Diagnostic CLI under outage** paragraph; `--stale-ok` and `--local` flags on read-only commands. |
| F-23 | Important  | Addressed           | §6 Rolling upgrade — added version skew band (±1 minor within major); refusal rule documented. Q4 narrowed in §14. |
| F-24 | Suggestion | Addressed           | Trailing citations paragraph removed; inline citations are sufficient. |

**Audit:** 6 Blockers — all Addressed. 14 Important — 12 Addressed, 0 Rejected, 2 in scope but split (F-09 also has a v0.1/v0.2 escape: rotation requires online dual-CA; offline-only is documented as the abort path). 6 Suggestions — 3 Addressed, 2 Rejected (with §1/§2 commitment as rationale), 1 Deferred-to-v0.2.

Locked-decision references used in dispositions:
- LD-3 ("plugins never touch the store"): F-21 reinforces.
- LD-5 ("one plugin per kind"): F-16, F-17 stand on the 5-plugin commitment.
- LD-7 (trusted cluster): F-18 (one TTL is enough).
- §1 scope commitment to "5 gRPC plugin contracts": F-16, F-17 rejections.

---

## Sign-off

**Synthesis lineage** (which inputs informed which sections):

| Section                                  | Primary inputs                                                                                |
|------------------------------------------|----------------------------------------------------------------------------------------------|
| §1 Executive summary                     | All three proposals (Alpha/Bravo/Charlie); all five Judges; locked decisions.                |
| §2 Goals & non-goals                     | `_shared-constraints.md` SC-1…SC-10; locked decisions LD-1…LD-7.                              |
| §3 System overview                       | Charlie §2 topology, blended with Alpha §3 plugin co-location.                                |
| §4 Cluster state model                   | Charlie §1 (key prefixes), Alpha §6 (capability schema), Judge 1 (Txn-CAS rigor).             |
| §5 Plugin contracts                      | `worker-pool-providers.md`, `workspace-backends.md`, Judge 3 (DX critique).                   |
| §6 Node lifecycle                        | Charlie §3 join flow, Judge 4 §1/§3 (failure scenarios), Judge 2 (operability).               |
| §7 Task assignment                       | Alpha §3 HRW; correctness reframing forced by Critic A (F-01).                                |
| §8 Provisioning flow                     | Charlie §4, Judge 3 §4 (onboarding-cliff fix).                                                |
| §9 Degraded mode                         | Charlie §5 (etcd-down), Judge 4 §8 (silent stall), Judge 2 (escape valves).                   |
| §10 Failure modes table                  | `meta-judge-4-failures.md` (8 scenarios) + 4 synthesis-specific additions.                    |
| §11 What ships                           | Locked decisions LD-1…LD-7 ⇒ module decomposition; Judge 5 (cut speculation).                 |
| §12 Migration                            | Current single-node BOI behavior + Judge 2 backward-compat asks.                              |
| §13 v0.1 scope cut                       | All five Judges' "defer this" calls; locked decisions LD-4/LD-5.                              |
| §14 Open questions                       | Residue from §7 (Q1), §5 (Q2, Q4), §6 (Q3), §5.5 (Q6), §5.2 (Q7).                              |
| §15 Response to critique                 | `distributed-architecture-design-critique.md` F-01…F-24.                                      |

**Locked decisions that constrained the design** (do not relitigate without revisiting brainstorm):

- LD-1. Foundation = external strongly-consistent store (Charlie's pattern).
- LD-2. Store = etcd everywhere; no SQLite-embedded fallback in v0.1.
- LD-3. Plugins NEVER touch the store directly; gRPC against `boi-core` only.
- LD-4. Degraded mode is lightweight: in-flight continues, new dispatches fail loudly, no local replay queueing.
- LD-5. One Workspace, one Pool, one Router plugin per deployment in v0.1.
- LD-6. Assignment = rendezvous hashing (HRW) over the membership snapshot, claim via CAS.
- LD-7. Trusted cluster, mTLS between BOI nodes, no Byzantine assumptions.

**Open questions to resolve before implementation** (clean re-statement of §14):

- Q1. etcd revision pinning policy for HRW snapshots (strict / tolerance window / none) — pick via week-3 measurement.
- Q2. Worker fencing-token format — etcd `lease_id` vs separate monotonic per-task counter; freeze before Pool proto.
- Q3. Join-token issuance authorization model — open to all members vs `cluster.admin` capability gate; required before v0.1 GA.
- Q4. Plugin protocol versioning — proto-level `version` field vs file-naming (`workspace.v1.proto`).
- Q6. Hooks delivery semantics for audit-grade consumers — at-least-once via etcd-backed Hooks subset?
- Q7. Worker stdout streaming durability across CLI disconnect — tee to etcd / local file / drop?

(Q5 was resolved during the critique pass: CA fingerprint is embedded in the signed join-token payload; see §6 Join.)

**Recommended next step.** Write the v0.1 implementation plan: a sequenced, person-week-sized breakdown of the §11 module list against the §13 scope, with explicit milestones for each Open Question's resolution. The implementation plan, not this design, is the right place to capture the week-3 etcd-revision-pinning experiment, the Pool fencing-token choice, and the version-skew testing matrix.
