# Q1 — etcd revision pinning in HRW snapshots

**Status:** Decided (v0.1)
**Date:** 2026-05-12
**Owner:** boi-core
**Supersedes:** §14 Q1 in `distributed-architecture-design-2026-05-12.md`

## 1. Question (verbatim, §14)

> **Q1. etcd revision pinning in HRW snapshots.** Should `assign()` pin to the etcd `mod_revision` it read, and reject CAS attempts when the revision has advanced beyond a stale window? Trade-off: stricter determinism vs. higher CAS-retry rate under churn. Recommend an experiment in week 3 of v0.1 with two configs.

## 2. Why this matters

The `assign()` path reads a membership/capability snapshot, runs HRW, then issues a CAS to `/boi/claims/{task_id}`. If pinning is too strict, every membership change (a heartbeat lease renewal on `/boi/caps/` increments revisions roughly every 5–10 s per node — at 8 nodes that's ~1 rev/sec cluster-wide) invalidates in-flight assigns and inflates `boi_core_hrw_cas_retry_total`, harming throughput and tail latency under healthy churn. If pinning is absent, two dispatchers reading at very different revisions can collide on the same candidate even though one is reasoning about a node that is already saturated or `degraded` — the CAS still gives correctness (F-01), but the loser's retry cost is paid on every churn event, and observability loses the signal "this assignment was made against a stale view." Correctness is not at stake; assignment quality, retry rate, and the design's epistemic honesty are.

## 3. Options analyzed

### Option A — No pin (status quo prior to Q1)

*How:* `assign()` reads the snapshot at whatever revision the local watcher last observed; the claim CAS predicate is only `compare(version(/boi/claims/{tid}) == 0)`.
*Cost:* No way to attribute CAS failures to stale snapshots vs. genuine contention. A dispatcher whose watch is lagging by seconds (GC pause, slow network) happily assigns to a node that has since flipped to `health=degraded` (F-06) or hit `workers_max`; the claim succeeds, the worker is then immediately killed by §5.2 fencing, wasted RTT.
*Prevents:* CAS-retry storms during routine churn. Maximum dispatch throughput.

### Option B — Pin-and-reject (strict)

*How:* `assign()` records `R = current_revision` at snapshot read; the claim Txn predicate adds `compare(mod_revision(/boi/nodes/) == R AND mod_revision(/boi/caps/) == R)`. Any change since the read aborts the CAS.
*Cost:* On an 8-node cluster with 10 s caps-lease renewals, the expected churn rate is ~0.8 rev/s on `/boi/caps/` alone. A dispatcher's snapshot-to-CAS window is conservatively 5–20 ms; the abort probability per healthy assign is small but non-zero, and **grows linearly with cluster size**. At 32 nodes (the upper end §2 commits to), abort rates would dominate `boi_core_hrw_cas_retry_total`. Worse: every aborted assign re-reads, re-HRWs, re-CASes — amplification.
*Prevents:* All forms of stale-view assignment. Strongest determinism story for debugging.

### Option C — Pin with stale-window tolerance (**recommended**)

*How:* `assign()` records `R`. The claim Txn predicate is `compare(mod_revision(/boi/nodes/) <= R + W AND mod_revision(/boi/caps/) <= R + W)` for tolerance `W`. On Txn failure due to the revision predicate (not the claim-key predicate), increment `boi_core_hrw_snapshot_stale_total{reason=revision}`, re-read snapshot, re-HRW, re-CAS — bounded to 3 attempts before falling through to next-best HRW candidate.
*Cost:* One extra Txn comparator. A small (10–100 ms wall-clock equivalent) tolerance window absorbs routine heartbeat churn while still catching genuinely old reads (e.g., a partition-recovering node's catch-up).
*Prevents:* Stale-view assignments by orders of magnitude more than no-pin, without paying Option B's amplification cost. Gives an explicit, named metric for "my snapshot was too old," which is currently un-observable.

### Option D — Pin-and-warn (no reject)

*How:* Same predicate as Option C, but evaluated *advisorily*: failure increments a counter and logs, does not abort.
*Cost:* Adds observability without backpressure. Bad assignments still ship.
*Prevents:* Nothing operationally; useful only as a measurement phase.

## 4. Recommended decision

**Adopt Option C (pin with stale-window tolerance) in v0.1, with `W = 64 revisions`, and fall through to next-best HRW candidate after 3 snapshot-refresh attempts.** This is roughly 60–80 s of cluster-wide churn budget at v0.1's expected 8–16 node target, which dominates the realistic snapshot-to-CAS window by 3+ orders of magnitude while still detecting genuinely stale reads. Config keys: `cluster.assign.snapshot_revision_window = 64` (operator-tunable), `cluster.assign.snapshot_refresh_max = 3` (hardcoded; do not expose). Week-3 experiment runs Option D in parallel on a shadow dispatcher to confirm the window is large enough — promotion to C is conditional on `boi_core_hrw_snapshot_stale_total / boi_core_hrw_cas_retry_total < 0.05` in the shadow.

## 5. Implications on the design doc

- **§7 (Task assignment algorithm).** Replace the "Snapshot revision pinning" paragraph (lines ~402) with the Option C semantics; add `R` capture in pseudocode, add the dual `mod_revision` comparator to `etcd_cas_put`, add the 3-attempt refresh loop. Remove the "implementation plan picks via measurement" hedge — the decision is made; the measurement is now a *validation* of the chosen window, not a config selection.
- **§9 (Metrics catalog, F-12 table).** Add `boi_core_hrw_snapshot_stale_total{reason}` counter (reasons: `revision`, `node_degraded`, `node_gone`) and `boi_core_hrw_snapshot_refresh_total` counter.
- **§10 (Failure modes).** Add a row: *"Snapshot-vs-cluster skew during assignment — detected via revision comparator on claim Txn, recovered by snapshot refresh + retry; TTR ≤100 ms; worst case 3 refresh cycles then fall-through to next HRW candidate."*
- **§14 (Open questions).** Strike Q1; reference this file.
- **§11 (CLI / ships).** No surface change; `cluster.assign.snapshot_revision_window` is a `boi.toml` knob, not a CLI.

## 6. Confidence and what would change my mind

**Confidence: 7/10.**

What would flip me to **no-pin (Option A)**:
- Week-3 load test shows `boi_core_hrw_snapshot_stale_total` < 0.1% of assigns at 32 nodes with `W=64`, *and* shows no stale-snapshot pathology in the `boi_core_claim_lease_expired_total / boi_core_hrw_cas_retry_total` ratio. If the comparator never fires usefully, it's dead code with a Txn-size cost.
- A production incident where the refresh loop itself becomes the bottleneck under a thundering herd (e.g., 100+ tasks dispatched in <1 s after etcd recovery).

What would flip me to **strict (Option B)**:
- A real-world correctness-adjacent bug traced to stale-view assignment that the fencing layer (§5.2) caught only after wasted worker spawn. Specifically: any incident where a plugin's `Spawn` was issued and then immediately fenced because the assigned node was already `degraded` in the authoritative view at CAS time. If fencing-after-spawn is the actual cost driver, strict pinning earns its abort rate by preventing those spawns entirely.

What would flip the **window size**:
- Cluster sizes pushing past v0.1's 32-node target (revision rate scales linearly with `/boi/caps/` lease holders). At 64 nodes, `W=64` becomes one second of churn; widen to `W=256` or move the comparator to a coarser key (`/boi/cluster/epoch`, a single key bumped on membership change only — a v0.2 schema change, out of scope here).
