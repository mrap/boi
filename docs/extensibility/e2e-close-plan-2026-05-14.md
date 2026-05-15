# Distributed BOI v0.1 — E2E Close Plan (27 → 43 green)

## Current state

Branch `feat/distributed-architecture`, 27/43 E2E green. 16 remaining failures across 5 independent subsystems. Goal: 42/43+ green, PR to main.

## Approach

Five parallel BOI specs, one per subsystem. S1-S4 have zero shared-file conflicts and run simultaneously. S5 depends on S2 (extends mock plugin pattern).

```
  S1 (fencing)    ─┐
  S2 (mock plugin) ─┼─ parallel ─── merge ─── S5 (provisioner) ── merge ── PR
  S3 (tail RPC)   ─┤
  S4 (degraded)   ─┘
```

Shared file conflicts: `docker-compose.yaml` (S2 adds services, S4 adds env vars, S5 adds Docker socket mount), `boi-node.Dockerfile` (S2 adds boi-mock-plugin build, S4 adds curl), `Cargo.toml` workspace (S2 adds member). These merge sequentially via BOI's worktree isolation — S2 lands first since S5 depends on it.

---

## S1: Fencing test isolation (3 tests)

### Tests
- `e2e_fencing::stale_worker_completion_rejected` (passes individually, fails in suite)
- `e2e_fencing::no_double_dispatch_under_partition_recovery` (same)
- `e2e_fencing::audit_event_for_stale_writeback` (lease-dependent, needs unpause cleanup)

### Root cause
`compose_pause("node-a")` freezes the container. When a test fails (panic in `run_subtest`), `Cluster::drop` calls `docker compose down -v`. Docker Compose sends SIGTERM to paused containers, which is queued but undeliverable until unpaused. After the 10s stop timeout, Docker sends SIGKILL. This 10s delay per paused container accumulates across tests, and residual network state from the slow teardown bleeds into the next test's `docker compose up`.

### Fix
In `Cluster::down()`, run `docker compose unpause` before `docker compose down -v`. This unblocks SIGTERM delivery so teardown completes immediately. The unpause call is best-effort (ignores errors if nothing is paused).

Also: the `cluster init` lease-unbinding fix from this session already landed (preserves lease on node record writes). The `assign_if_winner` gate ensures claims land on the correct node's lease. With proper teardown, the fencing tests should pass in suite as they do individually.

### Files
- `crates/boi-test-harness/src/lib.rs` — `Cluster::down()`: add unpause before down

### Verify
```
cargo test -p boi-test-harness --features e2e --test e2e_fencing -- --test-threads=1
```

---

## S2: Mock plugin + hooks pipeline (4 tests)

### Tests
- `e2e_plugin_lifecycle::handshake_returns_capabilities`
- `e2e_plugin_lifecycle::crash_under_threshold_restarts`
- `e2e_hooks_audit::back_pressure_stalls_workflow`
- `e2e_hooks_audit::best_effort_tier_unchanged`

### What's needed

**A. `boi-mock-plugin` binary** — a small gRPC server implementing the Hooks service (`boi.hooks.v1.Hooks`):

1. On startup: write `BOI_READY\n` to stdout.
2. `Handshake` RPC: return `plugin_proto_minor=0`, `capabilities=["caps.x.foo", "caps.x.bar"]`.
3. `Emit` RPC: append a line to `/tmp/{plugin_id}.delivered` with the event JSON, then return `acked_sequence = request.sequence`. If `--ack-delay-ms N` is set, sleep N ms before responding (for back-pressure testing).
4. Signal handling: on SIGUSR1, call `std::process::abort()` (crash-on-demand for the supervisor test).

Build in `crates/boi-mock-plugin/` as a workspace member. Add to the Dockerfile: `RUN cargo build --release -p boi-mock-plugin` and `COPY --from=builder /src/target/release/boi-mock-plugin /usr/local/bin/boi-mock-plugin`.

**B. Supervisor Handshake wiring** — `spawn_plugin` in `main.rs` already starts the binary and waits for `BOI_READY\n`. After ready, it must:
1. Open a gRPC channel to the plugin's listen address (plugin publishes its gRPC port on stdout after `BOI_READY`, e.g. `GRPC_PORT=50051\n`).
2. Call `Handshake`, validate via `boi_plugin_host::handshake::validate`.
3. Store capabilities at `/boi/plugins/{name}/caps` in etcd.

**C. Supervisor crash bookkeeping** — `handle_crash` must:
1. Record the crash timestamp in the plugin's `crash_history` deque.
2. If 4+ crashes within 5 minutes: set plugin status to `unstable` at `/boi/plugins/{name}/status`.
3. Set node `caps.dynamic.health=degraded` at `/boi/caps/{node_id}` (updates the dynamic map).

**D. Hooks emit-burst back-pressure** — the `run_hooks_emit_burst` function currently advances HWM immediately (no real plugin involved). For back-pressure to work:
1. If the registered plugin has `ack_rate_cap` (e.g., `"1/s"`), parse it and enforce a sleep between HWM advances.
2. This causes `pending_acks` to grow naturally. When it hits `HOOKS_WAL_BACKPRESSURE_WINDOW` (100), the function prints `STALLED` and `hook.queue.saturated`.

**E. Best-effort delivery** — `dispatch_best_effort` currently logs and moves on. Wire it to:
1. If a plugin-sidecar address is configured (env `BOI_HOOKS_SIDECAR_ADDR`), send an HTTP POST with the event JSON.
2. The mock plugin (running as docker-compose `plugin-sidecar`) receives it and writes to `/tmp/{plugin_id}.delivered`.
3. If the sidecar is unreachable, log and continue (fire-and-forget semantics).

Alternatively (simpler): the `hooks-emit-burst` function for best_effort mode can write directly to `/tmp/{plugin_id}.delivered` inside the node container. The test checks the plugin-sidecar container's filesystem — so we need the mock plugin running there, OR the test needs to check the node container instead.

Looking at the test: it does `docker_exec_raw("plugin-sidecar", &["sh", "-c", &format!("cat /tmp/{BEST_EFFORT_PLUGIN}.delivered 2>/dev/null | wc -l")])`. This checks the plugin-sidecar container. So either:
- Run boi-mock-plugin in the plugin-sidecar container, receiving events via gRPC
- Or change the test to check the node container

The simplest path: update the docker-compose `plugin-sidecar` service to run `boi-mock-plugin --mode hooks-receiver --port 50051`. Then `dispatch_best_effort` sends events to the sidecar via gRPC or HTTP. The sidecar writes to `/tmp/`.

### Files
- `crates/boi-mock-plugin/` (new crate: Cargo.toml, src/main.rs)
- `Cargo.toml` workspace: add member
- `crates/boi-test-harness/docker/boi-node.Dockerfile`: build + copy boi-mock-plugin
- `crates/boi-test-harness/docker/docker-compose.yaml`: update plugin-sidecar to use boi-mock-plugin
- `crates/boi-node/src/main.rs`: wire spawn_plugin Handshake, handle_crash bookkeeping, emit-burst ack_rate_cap, dispatch_best_effort

### Verify
```
cargo test -p boi-test-harness --features e2e --test e2e_plugin_lifecycle -- --test-threads=1
cargo test -p boi-test-harness --features e2e --test e2e_hooks_audit -- --test-threads=1
```

---

## S3: Cross-node stdout tail RPC (3 tests)

### Tests
- `e2e_stdout_tail::tail_command_streams`
- `e2e_stdout_tail::disconnect_reattach_no_gap`
- `e2e_stdout_tail::stdout_tee_to_disk` (timing-sensitive, may need a claim-wait)

### What's needed

The `spec tail` CLI resolves the claimant via `/boi/claims/{task_id}` and reads the node's address from `/boi/nodes/{node_id}`. It then needs to fetch the log file from the claimant node over the network.

**A. Internal tail HTTP endpoint on the daemon** — extend the existing metrics TCP server (port 9090) with path routing:
- `GET /metrics` — existing Prometheus metrics
- `GET /internal/tail/{task_id}?since_bytes=N&max_bytes=M` — read `~/.boi/logs/{spec_id}/{task_id}.log` and return raw bytes

The spec_id lookup: scan `~/.boi/logs/*/` for a file named `{task_id}.log`.

**B. Update `spec tail` CLI** — instead of reading local files, HTTP GET to `http://{claimant_addr}:9090/internal/tail/{task_id}?since_bytes=N&max_bytes=M`. Print the response body to stdout.

**C. Claim-wait for stdout_tee_to_disk** — the test dispatches and immediately checks for the log. Add a wait for the claim to appear (same pattern as the lease-expiry fix).

### Files
- `crates/boi-node/src/main.rs`: extend metrics server with path routing + tail handler; update `SpecCmd::Tail` to HTTP GET from claimant

### Verify
```
cargo test -p boi-test-harness --features e2e --test e2e_stdout_tail -- --test-threads=1
```

---

## S4: Degraded mode fixes (3 tests)

### Tests
- `e2e_degraded::new_dispatch_fails_loud_under_partition` (stack overflow)
- `e2e_degraded::metrics_counter_increments` (curl missing, counter sharing)
- `e2e_degraded::in_flight_task_survives_etcd_partition` (pending-flush buffer)

### What's needed

**A. Fix stack overflow on partition dispatch** — the gRPC client stack-overflows when the server is unreachable after network disconnect. Two options:
1. Set `RUST_MIN_STACK=8388608` (8MB) in docker-compose.yaml environment for all nodes.
2. Wrap the dispatch connect+insert in `tokio::task::spawn_blocking` (or a dedicated thread with a larger stack).

Option 1 is simplest and addresses any future deep-stack gRPC paths. The default Rust thread stack is 2MB; 8MB gives plenty of headroom.

**B. Install curl in the container** — add to the Dockerfile runtime stage: `RUN apt-get update && apt-get install -y --no-install-recommends curl && rm -rf /var/lib/apt/lists/*`. The test uses `curl -fsS http://127.0.0.1:9090/metrics` inside the node container.

**C. Pending-flush buffer** — when `commit_task_with_fence` fails with a network error during a partition, buffer the result:
1. Write `{"task_id": ..., "status": ..., "ts": ...}` to `~/.boi/pending-flush/{task_id}.json`.
2. On daemon startup (or after etcd reconnect): scan `~/.boi/pending-flush/`, replay each buffered result via `commit_task_with_fence`, and delete the file on success.
3. After flush, emit a `task.completed` event to `/boi/events/`.

The reconnect detection: the assignment loop already retries on `StaleSnapshot`. Add a similar check in `commit_task_with_fence`: on network error, buffer to disk. The lease_expiry_watcher already watches etcd — when the watch reconnects after a partition, trigger a flush of pending results.

### Files
- `crates/boi-test-harness/docker/boi-node.Dockerfile`: add curl
- `crates/boi-test-harness/docker/docker-compose.yaml`: add RUST_MIN_STACK=8388608
- `crates/boi-node/src/main.rs`: pending-flush write/replay in commit_task_with_fence + flush trigger

### Verify
```
cargo test -p boi-test-harness --features e2e --test e2e_degraded -- --test-threads=1
```

---

## S5: Provisioner plugin (3 tests)

### Tests
- `e2e_provisioning::no_capable_triggers_provision`
- `e2e_provisioning::provision_token_is_admin_gated`
- `e2e_provisioning::new_node_joins_and_claims`

### Dependencies
S2 must land first (provides boi-mock-plugin build pattern and Dockerfile changes).

### What's needed

**A. Provisioner mode in boi-mock-plugin** — extend the mock plugin with `--mode provisioner` that implements `boi.provisioner.v1.Provisioner`:
1. `Handshake` RPC: return minor=0, capabilities=["docker-provisioner"].
2. `Provision` RPC: receive `ProvisionRequest`, spawn a new boi-node container using Docker CLI (`docker run`), pass `BOI_TOKEN` from the request. Write the RPC to `/var/lib/boi-plugin/transcript.jsonl`. Return `machine_id` and `expected_node_id`.
3. `Deprovision` RPC: stop and remove the container.

The provisioner needs Docker CLI access. Mount the Docker socket in docker-compose: `volumes: ["/var/run/docker.sock:/var/run/docker.sock"]`. Install Docker CLI in the Dockerfile.

**B. Fix `internal mint-provision-token`** — the command exists but needs to:
1. Load the cluster CA from the CA directory (generated by `cluster init`).
2. Sign a JWT with `ca_fingerprint` embedded (using `boi_identity::join_token`).
3. Check admin gate: read `/boi/cluster/admin` and verify it matches the caller's node_id.

**C. Wire the provision trigger** — the NeedProvision path in `assignment_tick` already has a provision_task call gated on `is_cluster_admin`. Ensure it:
1. Mints a JoinToken via the local CA.
2. Builds a `ProvisionRequest` with the token + cap hints.
3. Calls the provisioner plugin's gRPC Provision RPC.
4. The provisioner spawns the container. The new node runs `boi-node node join --token <token>`, registers in etcd, and the assignment loop claims the queued task.

**D. Docker-compose changes** — add the provisioner as a sidecar or run it on node-a:
- Option: run boi-mock-plugin in provisioner mode as the `plugin-sidecar` service (or a new `provisioner-sidecar` service).
- Mount Docker socket: `volumes: ["/var/run/docker.sock:/var/run/docker.sock"]`.
- The provisioned container must join the same `boi-test` network.

### Files
- `crates/boi-mock-plugin/src/main.rs`: add provisioner mode
- `crates/boi-node/src/main.rs`: fix mint-provision-token, wire provision_task to call provisioner gRPC
- `crates/boi-test-harness/docker/docker-compose.yaml`: provisioner sidecar + Docker socket mount
- `crates/boi-test-harness/docker/boi-node.Dockerfile`: install Docker CLI

### Verify
```
cargo test -p boi-test-harness --features e2e --test e2e_provisioning -- --test-threads=1
```

---

## Counter-review

**Critique 1: S2 is too large.** It covers 4 tests across 2 test files with 5 sub-deliverables (mock plugin, supervisor handshake, crash bookkeeping, back-pressure, best-effort delivery). Risk: a BOI worker might not finish in the time budget.

*Response:* The sub-deliverables are tightly coupled — the mock plugin is useless without the supervisor wiring, and vice versa. Splitting would create a circular dependency. The spec uses `mode: challenge` so the worker can reorder tasks.

**Critique 2: S5 provisioner needs Docker-in-Docker.** The provisioner sidecar calls `docker run` from inside a container. This requires the Docker socket mounted, which has security implications and may not work on all CI environments.

*Response:* This is an E2E test environment, not production. Docker socket mounting is standard for Docker-in-Docker test scenarios. The compose file already runs in a local dev environment. If CI doesn't support Docker socket, those tests would be skipped via `docker_available()`.

**Critique 3: S3 tail HTTP endpoint re-uses the metrics port.** Mixing metrics and data-plane traffic on the same port is fragile.

*Response:* For E2E purposes, a single-port approach with path routing is simpler and avoids adding yet another port binding. In production, these would be separated. The E2E test only needs to verify the data flow, not production hardening.

**Critique 4: S4 pending-flush is a new feature, not a bug fix.** The test expects F-08 (pending-flush buffer) which was designed but never built.

*Response:* Correct. The design doc specifies F-08. The implementation is bounded: write JSON to disk on network error, replay on reconnect. The daemon already has reconnect detection in the assignment loop.

**Critique 5: Fencing isolation fix (S1) might mask timing issues rather than fix them.** Adding unpause before down addresses the symptom (slow teardown) but doesn't explain why the tests pass individually.

*Response:* The unpause fix addresses the root cause: `docker compose down` on paused containers waits 10s for SIGTERM delivery before SIGKILL. This delay causes residual state. Unpausing first makes teardown instant. This IS the root cause, not a mask.

---

## Dispatch plan

```
S1 → boi dispatch (no --after, independent)
S2 → boi dispatch (no --after, independent)
S3 → boi dispatch (no --after, independent)
S4 → boi dispatch (no --after, independent)
S5 → boi dispatch --after S2
```

S1-S4 run in parallel. S5 waits for S2. Total wall time: max(S1,S2,S3,S4) + S5 = ~2h + 2h = ~4h.
