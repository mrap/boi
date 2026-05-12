# Q6. Hooks Delivery Semantics

## 1. Question (verbatim)

> **Q6. Hooks delivery semantics.** §5.5 says fire-and-forget with one retry. For audit-grade hooks (e.g. SOC2 log shipping), is at-least-once delivery required? If so, do Hooks plugins move into the etcd-backed state plane (likely yes for that subset) and how is "audit hook" declared?

## 2. Why this matters

Two user populations consume Hook events and they have incompatible needs:

- **Observability/automation user** (Slack notifier, Grafana annotator, "ping me when a task fails"). Cares about latency, not loss. A dropped event during an etcd partition or plugin crash is annoying but not a violation. Fire-and-forget is correct; adding durability is a tax.
- **Compliance/audit user** (SOC2 / ISO27001 log shipping, billing meter, tamper-evident audit trail). A single dropped `task.completed` is a control failure — auditors will demand evidence of completeness. They need at-least-once with provable delivery and a way to detect gaps.

The §5.5 default ("fire-and-forget + one retry") is correct for population 1 and wrong for population 2. We cannot pick one. We also cannot make everything at-least-once: it bloats etcd, adds back-pressure surface to every hook, and punishes the 90% case for the 10% case.

## 3. Options analyzed

### Option A — Fire-and-forget only (current default; defer audit to v0.2)

- **Durability:** none beyond core's in-process retry-once.
- **Ordering:** best-effort per plugin; no guarantee across nodes.
- **Back-pressure:** none — slow plugins simply miss events after `OnEvent` deadline.
- **Plugin DX:** trivial. Implement `OnEvent`, return ack, done.
- **Verdict:** ships fastest but tells SOC2 users "come back in 6 months." Given the architecture explicitly cites audit shipping as a motivating use case for the plugin system (§5.5 hello-world is a notifier, but extensibility section sells observability as a first-class concern), deferring leaves a credibility gap. Reject.

### Option B — All hooks at-least-once via etcd-backed queue

- **Durability:** every emitted event written to `/boi/hooks-queue/{plugin_id}/{seq}` before the originating workflow proceeds (or async with bounded buffer).
- **Ordering:** per-(plugin, kind) FIFO via monotonic `seq`.
- **Back-pressure:** slow plugin → queue grows → core blocks emit → workflow latency spikes.
- **Plugin DX:** every plugin author now reasons about idempotency, even the Slack notifier.
- **Verdict:** writes thousands of low-value events to etcd. Etcd is not Kafka — it will fall over on a 100 task/s cluster. Reject.

### Option C — Two tiers: `best_effort` (default) + `audit` (declared)

- **Durability:** `best_effort` stays §5.5 as written. `audit` hooks get a per-plugin, per-node durable queue **on local disk** (`~/.boi/hooks-queue/{plugin_id}.db`, embedded BoltDB or SQLite WAL), plus an etcd-replicated **high-water mark** at `/boi/hooks-hwm/{plugin_id}/{node_id}` so cluster-wide gap detection is cheap.
- **Ordering:** per-plugin-per-node FIFO. No cluster-wide ordering (events emitted on different nodes may interleave). Each event carries `(emitter_node_id, monotonic_seq)` so consumers can detect gaps per emitter.
- **Back-pressure:** local disk queue has a soft cap (default 100k events / 1 GB). On breach: emit `hook.queue.saturated` event, then **drop oldest non-audit kinds first**; if still saturated, **stall the emitting workflow**. Audit guarantee is preserved over availability — this is the SOC2 user's stated preference.
- **Plugin DX:** declared in plugin manifest (`boi-plugin.yaml`): `kind: hooks` + `delivery: audit` + `subscribed_kinds: [...]`. Audit plugins MUST implement `Ack(seq)` RPC; core deletes from local queue only on ack. Plugins receive `dedup_key = sha256(emitter_node_id || seq || event.kind || event.ts)` and are responsible for idempotency on their sink (standard SOC2 shipper pattern — Datadog, Splunk forwarders all do this).
- **Verdict:** matches the bimodal user need; keeps etcd lean; localizes failure.

### Option D — All hooks at-least-once via Kafka/NATS sidecar

- Punts durability to an external broker. Real answer for a mature platform. Adds a hard dependency v0.1 doesn't have budget for and conflicts with §13's "ship one cluster well first." Defer to v0.3.

## 4. Recommended decision

**Adopt Option C in v0.1.** Two tiers, declared per plugin:

| Tier | Default? | Durability | Ordering | Back-pressure | Dedup |
|---|---|---|---|---|---|
| `best_effort` | yes | in-process retry-once (§5.5 unchanged) | none | drop | none |
| `audit` | opt-in | local-disk WAL queue + etcd HWM | per-(node, plugin) FIFO | stall workflow on saturation | `dedup_key` from `(node_id, seq, kind, ts)` |

**Queue location: local disk on the emitting node, NOT etcd.** Etcd holds only the per-(plugin, node) HWM so any core node can answer "has plugin X consumed everything up to seq N from node Y?" in O(nodes) reads. The bulk queue is on local disk because (a) etcd is not a queue, (b) audit events are tied to the node that emitted them and don't need replication — if the node dies before delivery, the audit event is reported as a gap (`hook.gap.detected`) and operator alarms fire. This is the same semantic as Kubernetes audit log local buffering.

**Declaration: in plugin manifest, not at runtime.** `boi-plugin.yaml`:

```yaml
kind: hooks
plugin_id: soc2-shipper
delivery: audit          # or "best_effort" (default)
subscribed_kinds: ["task.dispatched", "task.completed", "task.failed", "node.*"]
ack_deadline_s: 30
queue_max_events: 100000
```

**Dedup discipline (plugin side):** plugins MUST treat `dedup_key` as an idempotency token on their downstream sink (e.g. as Splunk HEC's `idempotency-key` header, or as the unique key in an S3 audit prefix). `boi plugin test` ships a conformance test that replays the same event 3x and asserts the plugin emits one downstream side effect.

**Ordering caveat documented up front:** there is no cluster-wide ordering. Auditors who require total order across the cluster must sort by `(event.ts, emitter_node_id, seq)` at ingest time. We document this; we do not paper over it.

## 5. Implications on the design

Sections to update:

- **§4 Cluster state model.** Add one new key prefix:
  ```
  /boi/hooks-hwm/{plugin_id}/{node_id}  →  {last_acked_seq, last_ack_ts}
  Reader: monitors, gap-detector. Writer: emitting node on plugin ack. TTL: none.
  ```
  Bulk queue stays off etcd; only HWM lives there.
- **§5.5 Hooks.** Add `delivery` field semantics; add `Ack(AckRequest) returns (AckResponse)` RPC; document `dedup_key` derivation; document the two failure modes (`hook.queue.saturated`, `hook.gap.detected`) as new canonical `kind` strings — these become events 16 and 17 in the enum table.
- **§10 Failure modes.** Add row: "Audit-hook plugin crash with unacked events" → recovery: queue replays on plugin restart from last HWM; gap detector runs every 60 s on the emitting node.
- **§11 CLI surface.** Add `boi plugin queue {inspect|drain|fast-forward} <plugin_id>` for operator surgery when an audit plugin is hopelessly behind.
- **§13 v0.1 scope cut.** Move "audit-tier hooks" from implicit-deferred to explicit in-scope; add ~0.5 person-week for local-WAL queue + HWM logic + conformance test.
- **`boi plugin test`.** New conformance suite for `delivery: audit` plugins: replay-idempotency test, ack-or-redeliver test, gap-detection test.

## 6. Confidence and what would change my mind

**Confidence: 7/10.**

Strongest part: the two-tier split and the decision to keep bulk queues off etcd. Both are standard practice (Kubernetes audit policy, Vector's two-tier sinks) and the failure modes are well-understood.

Weakest part: the local-disk WAL choice means an emitting node that dies before plugin ack creates a real audit gap — recoverable as a *detected* gap, but not as delivered data. For true SOC2 evidence-of-completeness, the user will eventually want cross-node replication of the audit queue. I'm accepting that gap because (a) gap-detection + alerting is itself a valid SOC2 control, (b) replicating the queue belongs in v0.2 once we know the workload, and (c) Option B's "everything through etcd" would be operationally worse.

**What would change my mind:**
1. If a design partner has a hard SOC2 requirement that mandates synchronous replicated durability before the originating workflow proceeds — then Option B (or a hybrid: audit events synchronously replicated to N-of-M peer nodes' queues via a small Raft group) becomes necessary, and the design-doc rough-sizing grows by ~1 person-week.
2. If realistic v0.1 workloads exceed ~50 events/sec sustained (e.g. high-frequency `worker.stdout` streaming as audit), local BoltDB may be insufficient and we'd switch the queue backend to a small embedded log (e.g. `parca`-style WAL or directly Kafka).
3. If plugin authors strongly push back on implementing `Ack` + `dedup_key` (DX cost) — but this is table stakes for any audit sink and I'd hold the line.
