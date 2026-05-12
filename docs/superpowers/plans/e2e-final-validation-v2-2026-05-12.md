# E2E Final Validation v2 — 2026-05-12

Run against branch `feat/distributed-architecture`. Binary: `target/release/boi` (6.1 MB).
Log: `e2e-artifacts/final-validation-v2-2026-05-12.log`

---

## Summary

- **Previous (v1):** 2/42 green
- **Now (v2):** 2/42 green
- **Delta:** +0 green, -0 regressed
- **Hidden progress:** 6 tests now show "unexpectedly PASSED" — implementation works but `run_subtest` wrapper always panics regardless of body outcome. These tests need the wrapper removed to flip green.

---

## Newly green tests (wins)

None — cargo-reported green count unchanged at 2/42.

---

## Hidden wins: tests that "unexpectedly PASSED"

These tests have **working implementations** but fail because the `run_subtest` wrapper in the test harness panics even when the body returns `Ok(())`. Removing the wrapper on each of these would flip them green immediately.

| Test file | Subtest | Phase | What works |
|-----------|---------|-------|------------|
| e2e_plugin_lifecycle | `plugin_ready_signal_required` | 2 | F-11 `BOI_READY` ready-signal detection wired |
| e2e_plugin_lifecycle | `major_version_mismatch_rejected` | 2 | Protocol major-version rejection wired |
| e2e_plugin_lifecycle | `plugin_crash_does_not_kill_core` | 2 | §5 plugin isolation — crash doesn't kill daemon |
| e2e_assignment | `revision_pin_window_enforced` | 4 | Revision-pin window check passes |
| e2e_fencing | `new_claimant_completes_unaffected` | 4 | New claimant completes OK under stale-lease scenario |
| e2e_provisioning | `provision_token_is_admin_gated` | 5 | Admin-only token gating enforced |

**Action required:** For each of these 6 tests, replace `run_subtest(...)` with a normal `assert`-style body so cargo reports green.

---

## Still red (blocking)

These tests have genuine missing implementation (body returns `Err`, not just wrapper issue).

| Test | Subtest | Failure reason | Fix estimate |
|------|---------|----------------|:------------:|
| e2e_plugin_lifecycle | `handshake_returns_capabilities` | `/boi/plugins/mock-x/caps` absent after Handshake — caps not written to etcd | 1 spec (Phase 2b) |
| e2e_plugin_lifecycle | `crash_under_threshold_restarts` | `/boi/plugins/flaky/status` absent after 4 crashes — restart-budget bookkeeping not written | 1 spec (Phase 2b) |
| e2e_bootstrap | `cluster_init_creates_ca` | `/boi/cluster/ca.fingerprint` absent after `boi cluster init` — CA mint not wired | 1 spec (Phase 3) |
| e2e_bootstrap | `cluster_init_marks_seed_admin` | Node registers but `caps.static.cluster_admin=true` absent — seed-admin cap not set | same Phase 3 spec |
| e2e_bootstrap | `member_list_consistent` | `boi cluster members` returns node IDs but addresses empty | same Phase 3 spec |
| e2e_bootstrap | `valid_token_admits_node` | `MintJoinToken` exits 78 (stub) — token minting not wired | same Phase 3 spec |
| e2e_bootstrap | `non_admin_cannot_mint_token` | `unrecognized subcommand 'mint-join-token'` — CLI not wired | same Phase 3 spec |
| e2e_bootstrap | `tampered_token_rejected` | Tampered token admitted — signature verification absent | same Phase 3 spec |
| e2e_assignment | `task_lands_on_capable_node` | No `/boi/claims/<task_id>` within 2s — assignment loop not writing claim key | 1 spec (Phase 4b) |
| e2e_assignment | `non_capable_nodes_not_picked` | 14 claims vs expected 20 — HRW filter not distributing correctly | same Phase 4b spec |
| e2e_assignment | `claim_carries_lease_id` | Claim absent or missing `claim_lease_id` field | same Phase 4b spec |
| e2e_assignment | `lease_expiry_triggers_reassign_or_pending` | Claim persists after lease TTL — expiry/reassign path absent | same Phase 4b spec |
| e2e_fencing | `stale_worker_completion_rejected` | Stale-lease commit accepted (status 0) — Q2 fencing Txn not checking lease_id | 1 spec (Phase 4c) |
| e2e_fencing | `no_double_dispatch_under_partition_recovery` | Double claim observed during partition recovery — CAS not preventing race | same Phase 4c spec |
| e2e_fencing | `audit_event_for_stale_writeback` | No `/boi/events/` entry on fence rejection — F-15 canonical event not emitted | 1 spec (Phase 4c or 8) |
| e2e_provisioning | `no_capable_triggers_provision` | Docker isolation conflict + `ProvisionRequest` RPC not emitted — router not wired | 1 spec (Phase 5) |
| e2e_provisioning | `new_node_joins_and_claims` | No 4th node registers — Docker-provisioner plugin not implemented | same Phase 5 spec |
| e2e_provisioning | `provisioner_returned_success_but_no_join_triggers_cooldown` | F-06 counter absent — cooldown bookkeeping not written | same Phase 5 spec |

---

## Still red (deferrable — stdout tail + degraded + hooks audit)

| Test | Subtest | Failure reason |
|------|---------|----------------|
| e2e_stdout_tail | `stdout_tee_to_disk` | `--stream-stdout` flag not recognized — Phase 7 not wired |
| e2e_stdout_tail | `tail_command_streams` | same root cause |
| e2e_stdout_tail | `tail_resolves_via_etcd` | same root cause |
| e2e_stdout_tail | `disconnect_reattach_no_gap` | same root cause |
| e2e_stdout_tail | `retention_7d_or_100mb_caps` | same root cause |
| e2e_degraded | `dispatches_resume_after_reconnect` | `boi dispatch` returns empty task_id — dispatch CLI not fully wired |
| e2e_degraded | `in_flight_task_survives_etcd_partition` | same root cause |
| e2e_degraded | `local_fallback_drains_and_persists` | same root cause |
| e2e_degraded | `metrics_counter_increments` | same root cause |
| e2e_degraded | `new_dispatch_fails_loud_under_partition` | same root cause |
| e2e_hooks_audit | `audit_events_wal_persisted` | WAL file `/root/.boi/hooks-wal/audit-shipper.jsonl` absent — Phase 8 not wired |
| e2e_hooks_audit | `back_pressure_stalls_workflow` | `hooks-emit-burst` subcommand absent — Phase 8 not wired |
| e2e_hooks_audit | `best_effort_tier_unchanged` | 0/10 events delivered to best-effort plugin — Phase 8 dispatcher absent |
| e2e_hooks_audit | `hwm_tracks_delivery_position` | HWM key absent — Phase 8 HWM advancing not wired |
| e2e_hooks_audit | `node_restart_replays_wal` | WAL missing before restart — Phase 8 persistence absent |
| e2e_hooks_audit | `plugin_crash_no_event_loss` | HWM not advancing after plugin crash/restart — Phase 8 redelivery absent |

---

## Regressions

None. All previously-green tests (`harness_smoke_etcd_only`, `fresh_install_walkthrough`) remain green.

---

## Verdict

- **Ready for PR? No**
- **Cargo-reported green: 2/42** — below the 29-test threshold from the spec
- **Implementation green (if test wrappers fixed): 8/42** — still below threshold

### Remaining specs needed (in priority order)

1. **Phase 2b — Flip 3 `run_subtest` wrappers + wire Handshake caps + restart bookkeeping**
   - Remove `run_subtest` from `plugin_ready_signal_required`, `major_version_mismatch_rejected`, `plugin_crash_does_not_kill_core`
   - Wire `/boi/plugins/{name}/caps` write in Handshake handler (code exists but not reaching etcd)
   - Wire `/boi/plugins/{name}/status=unstable` after restart budget exhausted
   - Estimated green gain: +5 (3 unwrapped + 2 newly wired)

2. **Phase 3 — Bootstrap: CA mint + seed-admin + token RBAC** (6 tests)
   - Wire `boi cluster init` to write `/boi/cluster/ca.fingerprint`
   - Set `caps.static.cluster_admin=true` on seed node record
   - Implement `boi-node cluster mint-join-token` subcommand
   - Implement token signature verification (fail-closed)
   - Estimated green gain: +6

3. **Phase 4b — Assignment loop: claim key write + HRW + lease_id field** (4 tests)
   - Fix claim key write to use `CLAIMS_PREFIX/<task_id>` (currently writes 14/20)
   - Embed `claim_lease_id` in claim value
   - Wire lease-expiry → reassign or `pending-provision` transition
   - Remove `run_subtest` from `revision_pin_window_enforced`
   - Estimated green gain: +5

4. **Phase 4c — Fencing: Q2 lease_id Txn + canonical events** (3 tests)
   - Add `lease_id` precondition to the commit Txn in stale-writeback path
   - Prevent double-claim during partition recovery via CAS
   - Emit F-15 `task.claim_fence_rejected` event
   - Remove `run_subtest` from `new_claimant_completes_unaffected`
   - Estimated green gain: +4

5. **Phase 5 — Provisioning: ProvisionRequest emission + Docker plugin + cooldown** (3 tests)
   - Wire router to emit `ProvisionRequest` RPC when no capable node found
   - Implement reference Docker-provisioner plugin
   - Write F-06 `consecutive_claim_failures` counter
   - Remove `run_subtest` from `provision_token_is_admin_gated`
   - Estimated green gain: +4

6. **Phase 7 — Stdout tail: `--stream-stdout` dispatch flag** (5 tests)
   - Wire `--stream-stdout` argument on `boi-node spec dispatch`
   - Estimated green gain: +5

7. **Phase 1+/degraded — dispatch CLI returns task_id** (5 tests)
   - `boi dispatch <spec>` and `boi-node spec dispatch` must return non-empty `<spec_id> <task_id>`
   - Estimated green gain: +5

8. **Phase 8 — Hooks audit: WAL + HWM + back-pressure + `hooks-emit-burst`** (6 tests)
   - Write audit-tier WAL to `/root/.boi/hooks-wal/`
   - Advance HWM key on ack
   - Implement `boi-node internal hooks-emit-burst` subcommand
   - Wire in-process best-effort dispatcher
   - Estimated green gain: +6

**Total potential gain if all 8 specs land: 40 additional green (42/42)**

### Critical path to 29+ green

Minimum work to reach the PR threshold (29 green from 2):
- Spec 1 (Phase 2b): +5 → 7 total
- Spec 2 (Phase 3): +6 → 13 total
- Spec 3 (Phase 4b): +5 → 18 total
- Spec 4 (Phase 4c): +4 → 22 total
- Spec 5 (Phase 5): +4 → 26 total
- Spec 6 (Phase 7): +5 → 31 total ← **threshold crossed here**

Six specs to reach 29+. Specs 3 and 4 (Phase 4b/4c) depend on each other and can likely be combined. Realistic path: **5 focused specs**.

---

## Key technical findings

1. **`run_subtest` is the wrong pattern for "done" tests.** Six features are implemented but cargo still reports failure. The test wrapper needs to be flipped to a normal assertion once implementation lands. This is a systemic issue — every future wiring spec must also update the test file.

2. **Assignment loop is partially working.** `non_capable_nodes_not_picked` shows 14/20 expected claims — the loop runs but HRW distribution is wrong. The claim key format or capability filter has a bug.

3. **Node registration is working.** `cluster_init_marks_seed_admin` saw a real node record `{"node_id":"node-a","addr":"0.0.0.0:7001","version":"0.1.0",...}` — the daemon registers successfully. Only the `cluster_admin` cap is missing.

4. **Handshake code exists but caps not reaching etcd.** `boi-node` source writes `/boi/plugins/{name}/caps` in the Handshake path, but the test sees `etcd-key-not-found`. Likely cause: Docker image is running an old cached binary or the plugin binary path in the test doesn't trigger the Handshake path.

5. **`--stream-stdout` is the sole blocker for all 5 stdout-tail tests.** Single CLI flag addition unblocks the entire Phase 7 suite.

6. **Degraded tests all fail on the same root cause.** `boi dispatch` returns empty — not multiple independent failures. One fix unblocks all 5.
