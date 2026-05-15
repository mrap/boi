# Distributed BOI v0.1 — Adversarial Critique of Draft v1

**Subject:** `docs/extensibility/distributed-architecture-design-2026-05-12.md`
**Date:** 2026-05-12
**Stance:** Hostile. No diplomacy. Every section read as if it will ship.

---

## Critic A — Correctness Skeptic

The draft tells a clean story about determinism and exactly-once. The story has gaps.

**A1. The HRW "determinism argument" is rhetorical, not load-bearing.** §7 says two nodes compute the same assignment "iff (a) they enumerate the same candidate set." But the assignment that actually happens is the one whose **CAS write wins**, not the one some other node computes. Determinism of the *hash function* is irrelevant to correctness — what matters is that only one node ever holds a valid claim. The doc conflates "deterministic preference order" with "deterministic outcome" and never resolves it. Worse: in degraded mode (§9), the doc explicitly says determinism is "best-effort" because the cache is non-canonical. So the whole determinism argument is conditional on the happy path — but the failure modes table (§10) cites it as if it's invariant (row 11, "deterministic ordering picks the lex-smaller node_id"). The argument needs to be reframed: HRW gives *load-distribution stability*, not assignment determinism; assignment correctness rests entirely on the CAS.

**A2. Claim lease + state-machine has a dual-ownership window.** §4 says `CLAIMED → PENDING` re-queue is performed by "any monitor, only after observing `/boi/claims/{task_id}` lease expired." But etcd lease expiry is not synchronous with the watch event — there is a measurable window (etcd's heartbeat interval, ~1 s typical) where the lease key is deleted in storage but a particular client's watch has not yet received the DELETE event. During that window: monitor M1 sees lease gone and CAS-transitions `dispatch-queue` to PENDING; HRW reassigns to N4; N4 writes a fresh `/boi/claims/{tid}` with a *new* lease — and meanwhile, the original assignee N3, which suffered a 5-second GC pause (not a crash), wakes up, sees its own in-memory claim still cached, and continues writing worker state. Two nodes believe they hold valid claims, until N3's first etcd write returns `LeaseExpired`. The doc handwaves this in §10 row 12 ("fencing token") but never specifies the fencing token format (it's Q2 in open questions — meaning the protocol that prevents the dual-claim is *not designed yet*).

**A3. The Pool plugin idempotency requirement is asserted, not enforced.** §10 row 5 says "Pool plugin's `Spawn` is required to be idempotent on `task_id`." That is a contract assertion with no enforcement and no test. A non-reference Pool plugin author can ignore it; nothing in the proto, the host, or the failure-mode table catches a non-idempotent Pool. This is one of the load-bearing assumptions of zombie-worker correctness, and it lives in a *sentence*.

**A4. Provisioner reassignment hole.** Scenario: new node N5 is provisioned, joins, advertises caps, HRW picks it, CAS-claim succeeds, ExecuteTask is pushed — and then N5 dies before the worker spawns (so before any RUNNING-state write). The claim lease (30 s) eventually expires, monitor re-queues PENDING, HRW runs again. The *new* HRW might re-pick N5 if N5's lease hasn't yet expired (lease TTL 15 s on `/boi/nodes/`, but HRW reads `/boi/caps/` which has its own 15 s TTL — and there's no guarantee these two leases expire in lockstep). So a flapping N5 could oscillate: get assigned, die, get reassigned to itself. The doc doesn't discuss attempt counters on `/boi/nodes/{id}` health or a per-node "consecutive claim-failure" demotion. §10 row 4 only handles "capability-fraud" (plugin returns error), not "node never responds after claim."

**A5. Snapshot revision is not actually pinned anywhere.** §7 says "treat the snapshot's `cluster_revision` as the canonical version" but the pseudocode never reads `mod_revision`, never passes it to CAS, never threads it through. Q1 admits this is unresolved. So the doc states an invariant it does not implement. A reader believing the determinism argument will write CAS code that does not actually enforce it.

**A6. "PENDING → CLAIMED" is described as a CAS but the schema doesn't show a version field.** §4 row `/boi/dispatch-queue/` lists "Watch + CAS" but the value schema is `{spec_id, task_id, state, requires, attempts, last_error}` — no version, no expected_state, no claimant. The actual etcd transaction (compare `value.state == PENDING` then put `state=CLAIMED`) requires either an etcd Txn-on-value or an embedded epoch — unspecified.

**A7. Bootstrap CA trust is recursively broken.** §6 bootstrap step 2: "the cluster CA's fingerprint, which is itself published over a separate channel — see Migration §12 for the bootstrap-of-the-bootstrap question." §12 does not answer it; Q5 in open questions admits it. So the *first* join after bootstrap has a TOFU window where a MITM with the etcd-bootstrap-URL and a fake join token (the token is opaque, the new node can't tell a real CA from a fake one) can swap CAs. mTLS only protects after the CA is trusted; the first trust step is unspecified.

---

## Critic B — Operability Adversary

It's 3 a.m. The page says `boi_core_etcd_unreachable_seconds > 300`. What now?

**B1. There is no runbook.** The doc specifies metrics and gauges but never names the procedure. Where do operators go? `boi cluster status` is listed in the CLI section but its output schema is not. What does `boi cluster status` print when etcd is down? Per §9, status queries refuse with `EtcdUnreachable` once cache is stale — so the diagnostic CLI is *useless during the outage*. There is no "tell me what I know locally" command.

**B2. The "pending-flush" buffer at `~/.boi/pending-flush/` is a footgun.** §9 says result writes that fail during partition "buffer locally in `~/.boi/pending-flush/` and surface a loud 'result unflushed' warning." Loud where? In what log? With what retention? With what flush-policy when etcd returns (replay all? skip stale?)? What's the disk-fill behavior if a node runs 50 workers/hour and etcd is down for 8 hours? What happens to those buffered results if the node is then drained — do they migrate? Are they lost? This is unspecified and is a *correctness* hole, not just operability.

**B3. No certificate rotation procedure.** §10 row 9 names `boi cluster ca rotate` and a "24 h dual-CA trust window." That sentence is the entire rotation procedure. There is no specification of: how dual-CA trust is configured, how etcd's own certs (cluster ↔ etcd) rotate, what the operator runs on each node in what order, and how to abort a rotation midway.

**B4. No rolling-upgrade procedure.** N6 admits "rolling-restart procedure is documented" — except it isn't. The §11 CLI surface lists no upgrade verb. Q4 admits plugin protocol versioning is unresolved. So an operator upgrading BOI must… stop the cluster? The doc doesn't say.

**B5. Backward compatibility across BOI versions is not addressed.** What happens when N1 is v0.1.0 and N2 is v0.1.1 and they read each other's `/boi/nodes/{id}.version`? Does either refuse? Is there a min-version field? Q4 lives in open questions but is operationally a blocker for any second release.

**B6. Escape valve missing.** "etcd is wedged, get me out" — what does the operator do? `boi cluster bypass`? Single-node downgrade? Force-claim-release? None of these exist in the CLI surface. The §9 invariant ("no silent queueing") is honest, but combined with no escape valve it means: during a multi-hour etcd outage with no live human operator, BOI is hard-down.

**B7. Observability gaps.** §9 names two metrics (`boi_core_etcd_health`, `boi_core_etcd_unreachable_seconds`). The rest of the doc names zero. What's the metric for: claim lease expiry rate, HRW re-CAS retry rate, provision-request fulfillment latency, plugin restart counter? §10 rows reference detection mechanisms ("Plugin returns error", "Restart-backoff counter") but never specify the *metric name* an operator alerts on.

**B8. Hooks plugin observability is hostile to debug.** §5.5 says Hooks is fire-and-forget with one retry. If a Hooks plugin silently fails to deliver `task.completed`, the only signal is in the local plugin log — which lives where? The plugin host (§11 `boi-plugin` crate) is implied to capture stdout/stderr but the storage path, rotation, and `boi plugin logs` shape are unspecified.

**B9. Plugin "unhealthy" is silent to dispatchers.** §5 says "Three consecutive failures → plugin marked unhealthy" but never says whether that flips `/boi/caps/{node_id}.dynamic.health`. §10 row 7 implies yes for Pool, §6 implies yes generally, but the *contract* — "an unhealthy plugin demotes the node within X seconds" — is not in §4 (the schema) or §5 (the contract).

---

## Critic C — Plugin Author Hostile

I'm a Meta engineer. I want to write a Meta-SCM Workspace plugin. I read §5.1 and §5 lifecycle. Here's what I cannot do.

**C1. The `BOI_PLUGIN_SOCKET` env var, the correlation token, `plugin_id` — none of these are specified.** §5 says "core supplies each plugin a unique `plugin_id`, a `BOI_PLUGIN_SOCKET` env var, and a per-invocation correlation token." Where is the correlation token? In a request header (gRPC metadata)? A field on every proto message? How does my plugin propagate it to its logs? The "hello world" examples (§5.1, §5.2) don't show it. I cannot write structured logs that correlate to BOI-side logs without inventing a convention.

**C2. The `READY` signal on stdout is underspecified.** §5 says "expects `READY` on stdout within 10 s." Literal token `READY\n`? Some JSON envelope? Stderr okay? What if my plugin is a Java sidecar that takes 12 s to boot a JVM — is the 10 s tunable per-plugin? The reference implementations are not in the doc, so I cannot copy a known-good pattern.

**C3. Workspace `Prepare`: workdir lifetime, isolation, cleanup ordering.** §5.1 says `Prepare → workdir_path`. What guarantees does BOI offer about `workdir_path` lifetime? Is BOI going to call `Cleanup` after the worker exits, or do I get to decide? What if my workdir is on a shared filesystem and another task wants the same git ref — am I expected to be re-entrant? `hints` is `map<string,string>` — the entire user-extensibility surface — but there's no namespacing convention.

**C4. Pool `Spawn` idempotency contract is invisible.** A2/A3 above note that idempotency on `task_id` is asserted in §10 row 5 but absent from the §5.2 contract. As the Pool author, I read §5.2 and see no idempotency requirement. I happily build a Pool that re-runs `claude -p` on every `Spawn` call. My plugin passes integration tests. Then a re-claim happens in prod and the worker double-spawns.

**C5. I cannot test my plugin without mocking BOI core.** There is no `boi plugin test --as-if-core` harness mentioned. The plugin contracts are gRPC against core, and the gRPC services are not published as a public proto file with stubs the way Envoy's xDS is. I will have to reverse-engineer the request/response shape from the doc, build my own mock, and pray it matches.

**C6. "Provisioner never touches etcd" leaks via the bootstrap URL.** §5.4 hands the Provisioner `boi_bootstrap_url`. The promise is the Provisioner doesn't *speak etcd*. But the bootstrap URL is itself a privileged endpoint that the Provisioner injects into untrusted (newly allocated) infra. Concrete leak: a malicious or buggy Provisioner could log `boi_bootstrap_url` + `join_token` to a third-party log shipper. Now any attacker with read access to those logs has a one-shot key to the cluster. The doc treats the token as opaque, but its *security boundary* is the same as a short-lived etcd credential — the "no etcd in the plugin" claim is partially cosmetic.

A second leak: in §6 join step 3, the response from `/v1/join` contains `etcd_endpoints`. The *new node's core* learns etcd_endpoints. But if the new node is owned/observed by the Provisioner's infra layer (Fly machine envs, K8s pod envs), the Provisioner's operator can read them. So "the plugin doesn't touch etcd" is true for the plugin process, but the *infra the plugin owns* gets etcd creds.

**C7. Capability advertisement format is implicit.** §4 schema says `{static:{os,arch,region,...}, dynamic:{workers_busy,...}}` — the "..." is doing all the work. Where is the capability vocabulary documented? My plugin advertises `meta_scm`, BOI's HRW filter rejects it because the Router's `requires` parser doesn't know the tag, and nothing in the doc tells me what tag namespace is reserved vs. open.

**C8. Hooks event vocabulary is implicit.** §5.5 lists `task.dispatched, task.completed, node.joined` as examples. The full kind enum is not specified. A Hooks author writing a SOC2-grade audit log needs the complete list, with semantics, in the doc.

**C9. Plugin identity / signing.** §5 implies plugins are local processes core launches by binary path. There is no plugin signing, no checksum, no provenance. A "trusted cluster" (LD-7) is the cluster-of-nodes; the supply chain of *plugins* is unspecified.

---

## Critic D — Simplicity Hawk

This design is mostly tight, but several knobs and features are not earning their keep in v0.1.

**D1. The Hooks plugin is a second plane.** §5.5 introduces a fifth plugin type with its own protocol, lifecycle, retry semantics. It is fire-and-forget; everything it does could be a structured log line consumed by Fluentbit/Vector. Cut it. We lose: integrated Slack notifications in v0.1. We keep: every other observability story works without it (Prometheus + structured logs are already specified).

**D2. The Router plugin is a knob with no default story.** §5.3 says "in the default reference Router they just return `task.requires` verbatim." If the default is a passthrough, why is it a plugin at all? Cut the Router plugin and bake the passthrough behavior into core. We lose: bespoke routing logic that nobody has asked for. We keep: HRW + capability filter unchanged.

**D3. Per-deployment lease TTL knob.** §6 mentions `node.lease_ttl_secs` as operator-tunable. There is no rationale beyond "high-jitter WANs." If v0.1 doesn't ship to WAN deployments (Charlie's locked decision implies LAN/datacenter), this is speculative. Cut it; ship one TTL (15 s).

**D4. Dual capability planes (static / dynamic).** §4 schema separates `static` and `dynamic` caps. The only `dynamic` fields used are `workers_busy`, `workers_max`, `health`. These are filter inputs, not user-facing capabilities. Collapsing them into the node record (`/boi/nodes/{id}`) eliminates a separate key prefix and a redundant lease. We lose: nothing — same information, half the keys. We keep: filter logic.

**D5. Cargo-culted lexicographic tie-break.** §7 spends a paragraph on the probability of SipHash u64 collision (≈2⁻⁶⁴). At cluster sizes of 10–1000 nodes this is unobservable in the lifetime of the universe. The deterministic tie-break is defensible *only* because it is free; but it implies a "we considered this carefully" framing that invites readers to demand more. Either drop the discussion or fold it into a footnote.

**D6. `boi cluster ca rotate` with a 24 h dual-CA window is a v0.2 feature wearing v0.1 clothes.** §10 row 9 names it; §11 lists a `boi-ca` crate. The rotation flow itself is not specified (B3). Ship "CA rotation requires cluster downtime" in v0.1 and defer dual-CA to v0.2. We lose: zero-downtime CA rotation. We keep: a CA that exists, with a documented offline procedure.

**D7. Plugin restart with exponential backoff up to 60 s.** §5 specifies "1, 2, 4, …, capped 60 s." Why a 60-second cap? Why backoff at all if "Three consecutive failures" already marks the plugin unhealthy? Pick one mechanism. Cut the backoff schedule; on three failures, mark unhealthy and stop restarting; surface to the operator.

**Five proposed cuts (D1, D2, D3, D6, D7).**

---

## Synthesis: actionable findings

| F-ID | Severity   | Description                                                                                          | Section      | Suggested fix |
|------|------------|------------------------------------------------------------------------------------------------------|--------------|---------------|
| F-01 | Blocker    | HRW "determinism" argument conflates preference order with assignment outcome; correctness rests on CAS | §7           | Rewrite the determinism paragraph as "HRW provides load-distribution stability; assignment correctness rests entirely on CAS write to `/boi/claims/`." Remove "deterministic ordering picks the lex-smaller node_id" from §10 row 11 framing. |
| F-02 | Blocker    | Fencing token format unspecified — dual-claim window in lease-expiry race is unmitigated              | §10 row 5, §10 row 12, Q2 | Pull Q2 out of "open questions" into §7. Specify: each claim carries `lease_id`; every Pool→etcd write (via core) must include `If: claim.lease_id == <expected>` as an etcd Txn precondition. Reject and abort the worker on mismatch. |
| F-03 | Blocker    | `/boi/dispatch-queue/{task_id}` state transitions called "CAS" but schema has no version/epoch field   | §4           | Add `state_version: u64` to envelope schema; every state-machine transition uses `Txn(compare value.state_version == N; put value.state_version = N+1)`. |
| F-04 | Blocker    | Bootstrap CA trust is unresolved — first join has TOFU window with no defined procedure                | §6, §12, Q5  | Resolve Q5: bundle CA fingerprint into the join token's signed payload OR require operator to pre-distribute fingerprint to provisioned node via Provisioner-supplied env var `BOI_CA_FINGERPRINT`. Document chosen path; remove Q5. |
| F-05 | Blocker    | Pool idempotency requirement asserted in failure-mode table but absent from plugin contract           | §5.2, §10 row 5 | Add to §5.2: "Pool plugins MUST treat `Spawn(task_id=X)` as idempotent for the lifetime of a claim. Receiving a second `Spawn(X)` while the first is running MUST return the existing handle, not spawn a duplicate." Add a conformance test in plugin-host harness. |
| F-06 | Blocker    | Provisioner reassignment loop: provisioned-then-dead node can be re-picked by HRW                     | §6, §8       | Add per-node `consecutive_claim_failures` counter in `/boi/nodes/{id}`. After 3 failures, core flips `caps.dynamic.health=degraded` for 5 minutes (cooldown). Document in §6 failure-detection. |
| F-07 | Important  | "etcd is broken, get me out" escape valve missing                                                    | §9, §11      | Add `boi cluster local-fallback` CLI: drains the node, persists in-flight claims to disk, switches to single-node mode with a warning. Explicit operator-invoked, never automatic. |
| F-08 | Important  | Pending-flush buffer (`~/.boi/pending-flush/`) semantics unspecified: retention, flush-policy, drain interaction | §9       | Specify: buffer is per-node JSONL file, max size 100 MB (configurable), oldest-first eviction; on etcd recovery, flushed in order with at-least-once semantics into `/boi/dispatch-queue/` state writes; `boi node drain` refuses to proceed while buffer non-empty unless `--force-drop-buffer`. |
| F-09 | Important  | No certificate rotation procedure documented end-to-end                                              | §10 row 9, §11 | Add a `### Certificate rotation` subsection to §6 with step-by-step: `boi cluster ca rotate` mints new CA, dual-trust window, per-node `boi node cert renew`, abort path. Or descope to v0.2 and document offline-only rotation. |
| F-10 | Important  | No rolling-upgrade procedure                                                                          | N6, §11      | Add `### Rolling upgrade` subsection: quiesce dispatch via `boi cluster pause-dispatch`, upgrade nodes one at a time, resume. Or descope rolling upgrade explicitly and document cluster-wide restart procedure for v0.1. |
| F-11 | Important  | Plugin lifecycle: `READY` signal, correlation token propagation, plugin_id source — underspecified    | §5           | Specify: plugins must print exactly `BOI_READY\n` to stdout within an operator-configurable timeout (default 10 s). Correlation token rides in gRPC metadata key `boi-corr-id`. `plugin_id` is `<plugin-name>-<host-uuid>` generated by core. |
| F-12 | Important  | Observability surface: only 2 metrics named in the doc; per-row "detection" mechanisms in §10 are not tied to named metrics | §9, §10 | Add a §9 sub-section "Metrics catalog" listing every gauge/counter with name, labels, and what raises it. At minimum: claim_lease_expired_total, hrw_cas_retry_total, provision_req_latency_seconds, plugin_restart_total{plugin}, dispatch_queue_state_count{state}. |
| F-13 | Important  | Plugin host has no test harness for plugin authors                                                    | §5, §11      | Add `boi plugin test <binary>` CLI: launches plugin against a mock-core fixture, exercises lifecycle + each RPC with canned inputs; ships as part of `boi-plugin` crate. |
| F-14 | Important  | Capability tag vocabulary and namespacing are implicit                                                | §4, §5.3     | Add a §4 sub-section "Capability vocabulary": reserved keys (`os`, `arch`, `region`, `runtime`); user-defined keys must be `x-<vendor>-<tag>`; HRW filter is exact-match on key=value with set semantics. |
| F-15 | Important  | Hooks event kinds enumerated only by example; audit-grade hook authors cannot enumerate the set       | §5.5         | Add a `### Event kinds` table to §5.5 listing every `kind` string core emits, with semantics. At minimum: `task.{dispatched,claimed,started,completed,failed,reassigned}`, `node.{joined,drained,crashed,degraded}`, `provision.{requested,fulfilled,failed}`, `cluster.{ca_rotated,partition_detected,partition_healed}`. |
| F-16 | Suggestion | Hooks plugin is a second observability plane and can be replaced by structured-log consumption        | §5.5         | Defer Hooks plugin to v0.2; ship structured-log emission for the same event vocabulary in v0.1. (Deferral note: lose integrated Slack/PagerDuty; gain less protocol surface.) |
| F-17 | Suggestion | Router plugin's default is passthrough; in v0.1 nobody overrides it                                   | §5.3         | Defer Router plugin to v0.2; bake passthrough behavior into core. (Re-introduce when a real workload demands custom routing.) |
| F-18 | Suggestion | Per-deployment lease-TTL knob has no v0.1 justification                                               | §6           | Drop `node.lease_ttl_secs` config; hardcode 15 s. Re-introduce when a deployment provides a real WAN scenario. |
| F-19 | Suggestion | Static/dynamic capability split is two key prefixes for one logical record                            | §4           | Collapse `/boi/caps/{id}` into `/boi/nodes/{id}`; one lease, one watch, one record. |
| F-20 | Suggestion | Plugin restart exponential backoff overlaps with "unhealthy after 3 failures" — two mechanisms        | §5           | Pick one: either fixed retry-count-then-unhealthy or exponential-backoff-forever. Default to fixed (simpler, fewer states). |
| F-21 | Important  | Provisioner can log `join_token + boi_bootstrap_url` to third-party log shippers; "doesn't touch etcd" is a partial promise | §5.4, §8 | Add explicit security note in §5.4: join_token is a short-lived bearer credential; Provisioner plugins MUST NOT log it. Token TTL already 10 min; consider tightening to 5 min and adding `mint_for=<node_fingerprint>` binding. |
| F-22 | Important  | `boi cluster status` and other diagnostic CLIs refuse to serve when cache stale — diagnostics are useless during outage | §9, §11 | Specify: `boi cluster status --local` always serves from cache regardless of staleness, with stale-age stamped on output. Pair with `--stale-ok` flag on relevant read-only commands. |
| F-23 | Important  | BOI-version compatibility across nodes is not addressed                                               | §11, Q4      | Resolve Q4 in §11: every `/boi/nodes/{id}` carries `version:semver`; core refuses to elect itself as dispatcher if any other node's version differs in major.minor by more than ±1. Document the supported skew band. |
| F-24 | Suggestion | "Citations summary" at end of doc duplicates inline citations and adds no information                 | trailing paragraph | Cut the trailing citations block; keep inline citations only. |

**Total: 24 findings (4 Blockers, 14 Important, 6 Suggestion).**

Quality note: Blockers F-01 through F-06 are pre-implementation correctness gaps and must be resolved before the v0.1 implementation plan is written. Important findings are operability/DX gaps that, if shipped unresolved, will produce predictable 3 a.m. pages and plugin-author churn. Suggestion findings are simplification opportunities; reject with reasoning is acceptable.
