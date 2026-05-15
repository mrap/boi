# E2E Final Validation Triage — 2026-05-12

Run against branch `feat/distributed-architecture`. Binary: `target/release/boi` (6.1 MB).

---

## Build status

- **cargo build --release**: PASS — binary compiles cleanly in ~3s (Docker in-container build; host build artifact at `target/release/boi`)
- **cargo test (unit)**: No unit tests exist in the workspace outside the E2E harness. The test harness lib reports `0 tests` (0 passed, 0 failed). All test coverage is in the E2E test files.
- Warning count: 0 warnings visible in captured output.

---

## E2E results summary

- **Total subtests**: 42
- **Green (passing)**: 2
- **Red (informative failure)**: 40
- **Errored (panic/compile)**: 0 — every red uses the structured `panic!("RED [...]")` harness, so all failures are informative assertions, not crashes.

---

## Per-test-file breakdown

| Test file | Subtests | Green | Red | Phase | Notes |
|-----------|----------|-------|-----|-------|-------|
| smoke | 1 | 1 | 0 | 0 | etcd-only smoke test; infra works |
| e2e_fresh_install | 1 | 1 | 0 | 1 | basic walkthrough passes |
| e2e_plugin_lifecycle | 5 | 0 | 5 | 2 | Handshake RPC + supervisor not wired |
| e2e_bootstrap | 6 | 0 | 6 | 3 | CA mint, token RBAC, member list not wired |
| e2e_assignment | 5 | 0 | 5 | 4 | Assignment loop, HRW, CAS claim not wired |
| e2e_fencing | 4 | 0 | 4 | 4/8 | Lease fencing + canonical events not wired |
| e2e_provisioning | 4 | 0 | 4 | 5 | Docker provisioner plugin not wired |
| e2e_stdout_tail | 5 | 0 | 5 | 7 | `boi dispatch` returns empty; Phase 7 stub |
| e2e_degraded | 5 | 0 | 5 | 1+ | Depends on dispatch CLI; same root cause as Phase 7 |
| e2e_hooks_audit | 6 | 0 | 6 | 8 | Audit WAL, HWM, back-pressure not wired |

---

## Green tests (implementation verified)

| Subtest | File | Notes |
|---------|------|-------|
| `harness_smoke_etcd_only` | smoke | Docker + etcd infra spins up and tears down cleanly |
| `fresh_install_walkthrough` | e2e_fresh_install | Single-node fresh install completes without error |

These confirm that the test harness infrastructure is sound and the binary at minimum starts up and exits cleanly in the simplest case.

---

## Red tests — triage

### e2e_assignment (Phase 4)

| Subtest | Expected phase | Failure reason | Actionable? | Fix estimate |
|---------|---------------|----------------|-------------|--------------|
| `task_lands_on_capable_node` | 4 | missing wiring — assignment loop + HRW + CAS claim not implemented | Yes | 1 spec |
| `non_capable_nodes_not_picked` | 4 | missing wiring — capability filter in assignment loop absent | Yes | same spec as above |
| `claim_carries_lease_id` | 4 | missing wiring — lease_id not embedded in claim key | Yes | same spec |
| `lease_expiry_triggers_reassign_or_pending` | 4 | missing wiring — no lease-expiry watcher or reassign path | Yes | same spec |
| `revision_pin_window_enforced` | 4 | stub binary — `service "node-a" is not running`; node exits before test can run | Yes | depends on Phase 4 assignment loop landing |

### e2e_bootstrap (Phase 3)

| Subtest | Expected phase | Failure reason | Actionable? | Fix estimate |
|---------|---------------|----------------|-------------|--------------|
| `cluster_init_creates_ca` | 3 | missing wiring — `boi cluster init` does not write `/boi/cluster/ca.fingerprint` | Yes | 1 spec |
| `cluster_init_marks_seed_admin` | 3 | missing wiring — seed-admin capability not set in etcd | Yes | same spec |
| `member_list_consistent` | 3 | missing wiring — `boi cluster members` CLI returns empty strings | Yes | same spec |
| `valid_token_admits_node` | 3 | stub binary — `MintJoinToken` exits with code 78 (stub) | Yes | same spec |
| `non_admin_cannot_mint_token` | 3 | stub binary — `service "node-b" is not running` | Yes | same spec |
| `tampered_token_rejected` | 3 | stub binary — cannot distinguish rejection from stub-not-running | Yes | same spec |

### e2e_degraded (Phase 1+)

| Subtest | Expected phase | Failure reason | Actionable? | Fix estimate |
|---------|---------------|----------------|-------------|--------------|
| `dispatches_resume_after_reconnect` | 1+ | stub binary — `boi dispatch` returns empty task_id | Yes | blocked on Phase 1+ dispatch CLI |
| `in_flight_task_survives_etcd_partition` | 1+ | stub binary — same root cause | Yes | blocked |
| `local_fallback_drains_and_persists` | 1+ | stub binary — same root cause | Yes | blocked |
| `metrics_counter_increments` | 1+ | stub binary — same root cause | Yes | blocked on Phase 4+8 |
| `new_dispatch_fails_loud_under_partition` | 1+ | stub binary — same root cause | Yes | blocked |

All 5 degraded tests fail at the same precondition: `boi dispatch` on the boi-node container returns an empty task_id. These are blocked on the dispatch CLI being wired in the binary, which is a Phase 4 dependency.

### e2e_fencing (Phase 4/8)

| Subtest | Expected phase | Failure reason | Actionable? | Fix estimate |
|---------|---------------|----------------|-------------|--------------|
| `stale_worker_completion_rejected` | 4 | stub binary — `service "node-a" is not running` | Yes | Phase 4 (lease_id fencing in commit Txn) |
| `new_claimant_completes_unaffected` | 4 | missing wiring — reassignment after lease expiry absent | Yes | Phase 4 spec |
| `no_double_dispatch_under_partition_recovery` | 4 | missing wiring — cannot assert invariant until assignment loop lands | Yes | Phase 4 spec |
| `audit_event_for_stale_writeback` | 4/8 | missing wiring — F-15 canonical event emission not wired | Yes | Phase 8 or 4b spec |

### e2e_hooks_audit (Phase 8)

| Subtest | Expected phase | Failure reason | Actionable? | Fix estimate |
|---------|---------------|----------------|-------------|--------------|
| `audit_events_wal_persisted` | 8 | stub binary — `service "node-a" is not running` | Yes | Phase 8 spec |
| `back_pressure_stalls_workflow` | 8 | stub binary — same | Yes | Phase 8 spec |
| `best_effort_tier_unchanged` | 8 | stub binary — in-process hooks dispatcher absent | Yes | Phase 8 spec |
| `hwm_tracks_delivery_position` | 8 | missing wiring — HWM at `/boi/hooks-hwm/{node}/{plugin}` not advancing | Yes | Phase 8 spec |
| `node_restart_replays_wal` | 8 | missing wiring — WAL file not created before restart | Yes | Phase 8 spec |
| `plugin_crash_no_event_loss` | 8 | missing wiring — HWM does not advance after plugin restart | Yes | Phase 8 spec |

### e2e_plugin_lifecycle (Phase 2)

| Subtest | Expected phase | Failure reason | Actionable? | Fix estimate |
|---------|---------------|----------------|-------------|--------------|
| `handshake_returns_capabilities` | 2 | missing wiring — Handshake RPC does not store caps in etcd | Yes | Phase 2 spec |
| `crash_under_threshold_restarts` | 2 | missing wiring — plugin supervisor restart-budget not written to etcd | Yes | Phase 2 spec |
| `plugin_crash_does_not_kill_core` | 2 | missing wiring — `/boi/nodes/node-a` absent (node registration not wired) | Yes | Phase 2 spec |
| `major_version_mismatch_rejected` | 2 | stub binary — container exits immediately, cannot run Handshake | Yes | Phase 2 spec |
| `plugin_ready_signal_required` | 2 | stub binary — container exits immediately | Yes | Phase 2 spec |

### e2e_provisioning (Phase 5)

| Subtest | Expected phase | Failure reason | Actionable? | Fix estimate |
|---------|---------------|----------------|-------------|--------------|
| `no_capable_triggers_provision` | 5 | missing wiring — router does not emit ProvisionRequest RPC | Yes | Phase 5 spec |
| `new_node_joins_and_claims` | 5 | missing wiring — Docker provisioner plugin not implemented | Yes | Phase 5 spec |
| `provisioner_returned_success_but_no_join_triggers_cooldown` | 5 | missing wiring — F-06 cooldown counter absent | Yes | Phase 5 spec |
| `provision_token_is_admin_gated` | 5 | stub binary — `service "node-b" is not running` | Yes | Phase 5 spec |

### e2e_stdout_tail (Phase 7)

| Subtest | Expected phase | Failure reason | Actionable? | Fix estimate |
|---------|---------------|----------------|-------------|--------------|
| `stdout_tee_to_disk` | 7 | stub binary — `boi dispatch` returns empty; `service "node-a" is not running` | Yes | Phase 7 spec |
| `tail_command_streams` | 7 | stub binary — same | Yes | Phase 7 spec |
| `tail_resolves_via_etcd` | 7 | stub binary — same | Yes | Phase 7 spec |
| `disconnect_reattach_no_gap` | 7 | stub binary — same | Yes | Phase 7 spec |
| `retention_7d_or_100mb_caps` | 7 | stub binary — same | Yes | Phase 7 spec |

---

## Failure category summary

| Category | Count | Description |
|----------|-------|-------------|
| stub binary | 21 | `boi-node` exits before test can interact with it (missing CLI subcommand handlers, exit 78/1) |
| missing wiring | 19 | Binary runs but etcd keys are absent or RPCs return empty/zero values |
| infra | 0 | No Docker/etcd-level failures; infrastructure is solid |
| proto mismatch | 0 | No shape mismatches; harness and binary agree on protocol |
| genuine bug | 0 | No cases where code is wrong vs. simply unimplemented |

---

## Recommendation

### Honest assessment

The system does **not** work end-to-end yet. The binary builds and the test harness infrastructure (Docker, etcd, compose teardown) works reliably, but `boi-node` is still a stub in virtually every dimension that the tests exercise. Of 42 subtests, only 2 pass — and those 2 test infrastructure, not boi-node behavior.

The root cause for ~half the failures is the same: `boi-node` exits or returns empty responses when asked to perform any substantive operation. The other half get further but find no etcd keys written, meaning the behavior is designed in the spec but not yet connected to etcd writes.

This is not a regression from a previously-working state — the tests were written as a red baseline and have never been green. The good news is that every failure is informative and actionable, with zero infra/flake noise.

### Specs required to reach full green

Estimate: **6–7 additional specs**, roughly 1 per phase:

| Spec | Phases covered | Tests that turn green |
|------|----------------|----------------------|
| Phase 2: Plugin supervisor + Handshake | 2 | 5 |
| Phase 3: Cluster init + token RBAC | 3 | 6 |
| Phase 4a: Assignment loop + HRW + CAS claim | 4 | 5 |
| Phase 4b: Lease fencing + reassignment + canonical events | 4/8 | 4 + 1 |
| Phase 5: Provisioning + Docker plugin | 5 | 4 |
| Phase 7: Dispatch CLI + stdout tail | 7 + 1+ degraded | 5 + 5 |
| Phase 8: Hooks WAL + HWM + back-pressure | 8 | 5 remaining |

Total: ~35 tests would turn green after these 7 specs. The remaining 3 degraded tests (`in_flight_task_survives_etcd_partition`, etc.) need Phases 4+7 both done before they become testable.

### Deferrable for v0.1 merge

The following can be deferred without breaking core correctness:

- **Phase 7 (stdout tail, 5 tests)** — streaming tail is a UX feature, not a correctness requirement for task dispatch
- **Phase 8 (hooks/audit, 6 tests)** — audit WAL and HWM delivery are important for durability guarantees but can ship in v0.2
- **`audit_event_for_stale_writeback`** (fencing) — event emission is secondary to the fencing itself working

That's 12 tests deferrable.

### Blockers for v0.1 merge

These must be green before v0.1 can ship:

- **Phase 2 (plugin lifecycle, 5 tests)** — plugin isolation is a safety property; a crashing plugin must not kill the node
- **Phase 3 (cluster bootstrap + security, 6 tests)** — token RBAC and CA fingerprint are security primitives; shipping without them would be irresponsible
- **Phase 4 (assignment + fencing, 9 tests)** — this is the entire point of the system; without correct assignment and lease fencing, the distributed scheduler does not exist
- **Phase 5 (provisioning, 4 tests)** — auto-provisioning when no capable node exists is a core design goal
- **e2e_degraded (5 tests)** — if dispatch doesn't work under partition, the system isn't fit for production

That's 29 blocking tests (9 test files worth of Phase 2–5 + degraded coverage).

**Bottom line:** 2 of 42 tests green. The implementation gap is broad but coherent — nothing is broken, it's just mostly unimplemented. Estimated 7 more specs to reach full green; 5–6 of those are v0.1 blockers.
