# Q2 — Worker fencing-token format

**Status:** Decided (2026-05-12)
**Scope:** Pool plugin contract (§5.2) + dispatch-queue Txn predicates (§4) + failure rows §10/5 and §10/12
**Decision owner:** distributed-locking review (Kleppmann-style fencing discipline)

---

## 1. Question (verbatim)

> **Q2. Worker fencing-token format.** §10 row 5 alludes to using `lease_id` as a fencing token for late writes. The exact mechanism — is it the etcd lease ID, or a separate monotonic per-task counter? — needs design before the Pool proto is frozen.

## 2. Why this matters

The dual-write hazard: a node N1 claims task T, spawns a worker, then suffers a long GC pause / network partition / clock skew. Its etcd lease on `/boi/claims/T` expires; a monitor (§10 row 5) re-queues T; N2 claims T with a fresh lease, spawns a second worker, and proceeds. Meanwhile N1's worker — unaware — finishes computing and tries to commit a `RUNNING → SUCCEEDED` write to `/boi/dispatch-queue/T`. Without fencing, that late write either (a) clobbers N2's in-flight state, or (b) corrupts attempt accounting. §10 row 12 is the same race in the still-running window. The fencing token is what lets core reject N1's write *deterministically* at the storage layer (etcd Txn), not just defensively at a service boundary.

Kleppmann's requirement: tokens must be **monotonically increasing per resource**, **issued by the lock service**, **carried on every protected write**, and **verified at the storage layer**.

## 3. Options analyzed

### Option A — Use the etcd `lease_id` (i64) directly

**Mechanism.** `claim_lease_id` field already exists in the dispatch-queue envelope (§4). On claim, core writes it; on write-back, core's Txn predicate is `compare(value.claim_lease_id == <expected>)`. Worker carries `lease_id` as gRPC metadata `boi-claim-lease` (already specified in §5.2).
**Prevents.** Late write from a node whose lease expired — the envelope's `claim_lease_id` was overwritten when N2 claimed; N1's Txn fails predicate.
**Still possible / concerns.**
- etcd `LeaseID` is a **64-bit value, not monotonic per resource**. A reassignment can in principle draw a numerically *smaller* ID than the previous claim. Equality-compare on `claim_lease_id` works; ordering compare does not. This is fine for our use (we never need "newer than"), but it forecloses any future use that wants `>`.
- Lease **renewal does not rotate the ID**. A worker keeping its lease alive keeps the same token across hours — good for stability, no rotation logic needed.

### Option B — Separate monotonic per-task counter (e.g. `claim_epoch: u64` at `/boi/dispatch-queue/{task_id}.claim_epoch`)

**Mechanism.** BOI core increments `claim_epoch` inside the same Txn that mints a new claim (`PENDING → CLAIMED`). Token is independent of etcd's lease machinery.
**Prevents.** Same race as A. Plus, it provides true monotonicity, so monitors can do `claim_epoch > N` comparisons.
**Still possible / concerns.**
- Adds a field that is **isomorphic to `state_version`** for the transitions that matter (every claim increments `state_version` already per §4). It is redundant.
- Two sources of truth (etcd lease lifetime vs. our counter) can drift if the increment logic and the lease-grant logic ever get separated.

### Option C — Reuse the existing `state_version: u64`

**Mechanism.** `state_version` already increments on every state transition (§4, line 105/111-114). Use the `state_version` *at the moment of claim* as the fencing token: store it as a separate snapshot field `claim_state_version`, and predicate result writes on `claim_state_version == <expected>`.
**Prevents.** Same as A/B. Cleanly monotonic, since `state_version` only goes up.
**Still possible / concerns.**
- `state_version` increments on **every** transition, not just claim transitions. A benign `CLAIMED → RUNNING` bumps it. So the token must be a *snapshot at claim time*, not live `state_version`. That means we still introduce a new field — at which point it's just option B with a different name.

### Option D — Composite `(lease_id, attempt)`

Rejected: `attempt` is already in the claim record; composite tokens complicate the etcd Txn predicate (etcd compares one field per `Compare`); no additional safety over A.

## 4. Recommended decision

**Use etcd `lease_id` directly. No new field. No rotation on lease renewal.**

Concrete:

- **Field name:** `claim_lease_id` (already in §4 dispatch-queue envelope, type `i64`, etcd `LeaseID`).
- **Storage:** `/boi/dispatch-queue/{task_id}` envelope. Set inside the same etcd Txn that performs `PENDING → CLAIMED`. Cleared (set to `0`) when a monitor re-queues to `PENDING` after lease expiry (§4 line 114 already specifies this).
- **On the wire (Pool → core callbacks):** gRPC metadata key **`boi-claim-lease`**, ASCII-encoded i64. Plugin-host conformance harness (§11) rejects callbacks missing this header.
- **etcd Txn predicate on result writes:** core wraps every worker-result write in:
  ```
  Txn().If(
    Compare(Value("/boi/dispatch-queue/{tid}"), "=", <envelope-with-claim_lease_id==expected>)
  ).Then(Put(...)).Else(Abort)
  ```
  Implemented practically as `Compare(ModRevision, "=", <pinned>)` plus a value-decode assert on `claim_lease_id`; or — preferred — a dedicated sub-key `/boi/dispatch-queue/{tid}/claim_lease_id` (u64) carrying ONLY the lease id, enabling a single-field `Compare(Value(...), "=", "<expected>")`. **Recommend the dedicated sub-key** to avoid envelope round-trips on the hot path.
- **Lease renewal:** token does NOT rotate. The same `lease_id` is held for the life of the claim; renewals are heartbeats, not new grants. This is the Kleppmann invariant — the token represents *holding the lock*, not *the most recent heartbeat*.
- **Worker completes before its lease expires, but core didn't see the renewal (partition healed late):** if the lease was actually alive at the etcd cluster (quorum saw heartbeats), then `claim_lease_id` in etcd still matches, and the write commits. If the etcd cluster itself revoked the lease (the authoritative event), then by definition the claim record was overwritten and the worker's write fails the Txn — correctly. There is no third case. **The etcd cluster is the sole source of truth for liveness;** the worker's local belief about its lease is irrelevant. This is why we use etcd's own `lease_id` rather than a counter we maintain.

### Why not B/C

`state_version` and a separate counter both require BOI core to maintain monotonicity in lockstep with etcd's lease lifecycle. Any drift (lease-grant succeeds but counter increment fails, or vice versa) is a correctness bug. Using `lease_id` directly makes etcd the **sole** authority: granting the lease and minting the token are the same event. Fewer moving parts, fewer reconciliation paths.

### The one weakness, acknowledged

Equality-only comparison. We can never write a predicate like "any token strictly newer than X." If a future workflow needs that — say, "let the higher-epoch worker win even if both are still alive" — we will need to add a counter. v0.1 doesn't need it.

## 5. Implications on the design

- **§4 (state schema):** No change. `claim_lease_id: i64` is already specified. ADD a sentence: "`claim_lease_id` doubles as the fencing token; renewals do not rotate it." Recommend ADD sub-key `/boi/dispatch-queue/{task_id}/claim_lease_id` for hot-path single-field Txn compare.
- **§5.2 (Pool proto):** No new proto field. The `boi-claim-lease` gRPC metadata header is already normative. CLARIFY: the value is the i64 of the etcd `LeaseID` as decimal ASCII; conformance harness validates parseability and that it matches the active claim.
- **§5.2 Idempotency contract:** Unchanged. Already says "core only re-issues `Spawn(X)` when the claim has been re-acquired (new `lease_id`) after lease expiry" — this is now reinforced as the rotation point.
- **§10 row 5 and row 12:** Tighten language from "uses `lease_id` as a fencing token" to "etcd Txn `Compare(claim_lease_id == <expected>)` rejects stale-claim writes; the etcd cluster is sole authority for lease liveness."
- **§14:** Mark Q2 resolved. Remove from open-questions list.

## 6. Confidence — 8/10

What would change my mind:

- **Drops to 5/10** if profiling shows the value-decode-on-Txn overhead is material on the result-write hot path AND the sub-key alternative is rejected for operational reasons.
- **Drops to 4/10** if a v0.1 use case emerges requiring `token_new > token_old` ordering (e.g. "higher-epoch worker wins"). Then add `claim_epoch: u64` alongside `claim_lease_id`, keep both, predicate on epoch.
- **Drops to 3/10** if etcd ever changes `LeaseID` semantics such that the same numeric ID could be reissued to a different lease within the dispatch-queue retention window. Current etcd guarantees uniqueness for cluster lifetime; if that weakens, switch to option B.
- **Stays at 8/10** otherwise. This is the standard Kleppmann pattern; etcd was designed for exactly this.
