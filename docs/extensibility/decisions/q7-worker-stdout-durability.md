# Q7 — Worker stdout streaming durability

## 1. Question (verbatim)

> Worker stdout streaming durability. Pool's `WorkerEvent` stream is in-memory between Pool plugin and core. If the dispatching CLI disconnects, do we tee stdout to etcd, to a local file, or drop it? Affects long-running interactive sessions.

## 2. Why this matters

The dispatching CLI is a fragile attachment: laptops sleep, SSH sessions drop, `boi dispatch` gets `Ctrl-C`'d. The worker, by contrast, lives on the assigned node under a claim lease (§4) and may run for hours. Today's behavior — stdout flowing only through the live gRPC `WorkerEvent` stream (§5.2) into the CLI — means a disconnect silently loses the *only* observable trace of an in-flight 8-hour task. The task may still succeed (exit code, `stdout_ref`, and Hooks events are durable per §4 + §5.5), but the user cannot:

- reattach to a running task to watch progress,
- post-mortem a hung task without `kill -QUIT` heroics,
- diff partial output against expectations,
- recover the model's chain-of-thought from a session that already burned $X in tokens.

For a system whose entire value prop is "fire a spec, walk away," dropping stdout on disconnect is a correctness bug in the user's mental model even if the state machine is technically fine.

## 3. Options analyzed

| Option | Durability location | Retention | Reattach | Cost | Notes |
|---|---|---|---|---|---|
| **A. Drop on disconnect (status quo)** | none | n/a | impossible | $0 | Unacceptable for any task >5 min. |
| **B. Tee to etcd** | etcd `/boi/stdout/{task_id}/` keyed by seq | bounded; pruned on `DONE` | core re-reads keys, streams to client | very high — etcd's 1.5 MB value cap, 8 MB total-tx cap, Raft cost per write; an 8h task at 5 KB/s = 144 MB | Wrong tool. etcd is a coordination store, not a log shipper. Rejected. |
| **C. Tee to local file on executing node** | `~/.boi/logs/{spec_id}/{task_id}.log` on the worker's node | retained on disk; default 7-day TTL via `boi-degraded` reaper; size-capped at 100 MB/file with head-truncation | `boi spec tail <task_id>` → core looks up `claimant_node_id` from `/boi/dispatch-queue/{task_id}`, opens a gRPC `Tail(task_id, from_offset)` against that node, streams from file + live tail | cheap — sequential append, no consensus | Survives CLI disconnect. Lost only if the node itself dies (which already loses the worker — bounded blast radius). |
| **D. Configurable sink (S3, Loki, syslog)** | plugin-provided | plugin-defined | plugin-defined | high design cost (new plugin kind: `LogSink`) | Right answer for v0.2+. Out of scope for v0.1's 8–10 wk budget. |
| **E. Per-task `durable: true\|false` in spec** | varies | varies | varies | medium design cost | Premature; nobody knows the right default yet. Defer. |

## 4. Recommended decision

**Adopt Option C for v0.1.**

**Sink.** Pool plugin host (the side of the proto core controls, not the plugin) tees every `WorkerEvent.Stdout`/`Stderr` chunk to `~/.boi/logs/{spec_id}/{task_id}.log` on the executing node as it forwards the chunk to any subscriber. This is host-side, not plugin-side — every Pool plugin gets durability for free; plugin authors do not implement it.

**Format.** Length-prefixed framed records (`u32 seq | u8 stream | u32 len | bytes`) so `Tail` can resume from an offset without re-parsing.

**Retention.** 7 days after task `DONE`/`FAILED`, OR 100 MB per file (whichever first), enforced by the existing `boi-degraded` reaper loop. Operator-tunable via `boi.toml [logs] retain_days, max_bytes`.

**Reattach CLI.** Add `boi spec tail <task_id> [--from-start] [--follow]`. Core resolves `claimant_node_id` from etcd, opens an internal `Tail` RPC to that node, streams bytes. If the task is `DONE`, returns the full file and exits. If the node is unreachable, returns `degraded: log unavailable, task state=<X>` — task state remains authoritative.

**Node-death behavior.** Logs are NOT replicated. If the node dies, logs die with it. This is acceptable because: (a) the worker itself died, (b) etcd-durable state (exit not recorded, claim lease will expire, task gets reassigned per §4) is the authoritative record, (c) replicating logs is Option D and out of scope. Document this loudly.

## 5. Implications on the design

Sections to update in `distributed-architecture-design-2026-05-12.md`:

- **§5.2 Pool.** Add a **Host-side stdout durability** subsection right after Idempotency contract. Note: `WorkerEvent` proto **does not change** — the tee happens in core's plugin-host as bytes flow through. This is critical: Pool plugin authors are unaffected.
- **§5.2.** Add a new RPC `Tail(TailRequest) returns (stream WorkerEvent)` on a *core-internal* service (`boi-node` RPC, NOT the Pool plugin contract) — separate proto file `proto/node_tail.proto`. Pool plugins do not implement this.
- **§11 CLI surface.** Add `boi spec tail <task_id> [--from-start] [--follow]` to the list. Also add `boi spec logs <task_id>` (non-follow alias) for symmetry with `boi plugin logs`.
- **§11 New crates/modules.** Add `boi-stdout-tee` (small) or fold into `boi-plugin` host.
- **§11 `boi.toml`.** Document new `[logs]` section: `retain_days = 7`, `max_bytes = 100_000_000`, `dir = "~/.boi/logs"`.
- **§13 In v0.1 list.** Add a bullet: "Host-side stdout tee to local file + `boi spec tail` reattach (Q7)."
- **§13 Deferred to v0.2+.** Add: "Replicated / configurable log sinks (`LogSink` plugin kind). Rationale: Q7 v0.1 covers reattach against the executing node; replication / centralization is its own design."
- **§14 Q7.** Mark resolved; link to this file.

`WorkerEvent` proto stays untouched. CLI gains two commands. One config section. No new plugin kind. Roughly 0.5 wk of the §13 budget — comfortably within the 1 wk allocated to CLI surface.

## 6. Confidence and what would change my mind

**Confidence: 8/10.**

What would move me:

- **Down to 5** if a user produces a workload where the 8h log is also state the *next* task depends on, and that next task may run on a different node — then we need centralized storage and Option D becomes v0.1-blocking.
- **Down to 6** if profiling shows the tee adds material latency to `WorkerEvent` forwarding under chunky stdout (megabyte-per-second LLM streams). Mitigation is already known (async append + bounded ring buffer), but it shifts complexity into v0.1.
- **Up to 9** after a one-day prototype confirming `Tail` reattach against a real local-claude Pool plugin works without surprise around partial UTF-8 boundaries at the resume offset.

Option D (configurable sink) is clearly correct for v0.2 once we know what shape "centralized" should take. Shipping C first generates the requirements doc for D.
