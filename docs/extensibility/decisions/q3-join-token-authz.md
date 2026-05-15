# Q3 — Join-token issuance authorization

**Status:** Decided (v0.1 GA blocker)
**Date:** 2026-05-12
**Decider:** hex / Mike Rapadas
**Related:** §6 Join, §5.4 Provisioner, §8 Provisioning flow, §11 CLI, §13 v0.1 scope

---

## 1. Question (verbatim from §14)

> **Q3. Join-token issuance authorization.** Today any cluster member can mint join-tokens via `boi node` CLI. Should token-mint authority be restricted to a designated subset (e.g. nodes with capability `cluster.admin`)? Required answer before v0.1 GA.

---

## 2. Threat model

**Assumed posture (LD-7, §1):** v0.1 is a LAN/datacenter design with mTLS between nodes anchored at the cluster CA. The attacker model worth threat-modeling is therefore *not* a remote internet attacker — the cluster mTLS perimeter handles that — it is **a partially-compromised cluster member or insider with shell on one node**.

What such an adversary can already do *without* a join token:
- Read etcd state for the keys their node cert authorizes (cluster topology, capabilities, queued tasks).
- Disrupt in-flight work on that node.

What a mint-anywhere policy *adds* to their blast radius:
- **Lateral expansion.** Mint an arbitrary number of join tokens, hand them to attacker-controlled VMs in the same network, admit them as full cluster members. Each new node gets a CA-signed cert and full member privileges (read all caps, accept claims, run arbitrary specs via Pool plugins). The cluster grows under the attacker.
- **Capability spoofing at scale.** New nodes self-declare capabilities (§6 step 4, N7 not fixed in v0.1). Combined with mint-anywhere, one compromised node bootstraps an army of fake-`gpu` nodes that grab GPU tasks and exfil prompts.
- **Persistence.** Even if the original compromised node is detected and evicted, the lateral nodes it admitted remain — they have their own valid CA-signed certs.

What restriction does *not* prevent:
- Compromise of an actual `cluster.admin` node still gives full mint authority (no defense-in-depth past the chosen admin set).
- Theft of the cluster CA private key — total loss regardless of mint policy.
- Capability fraud on already-admitted nodes (N7, deferred).
- Provisioner-supplied infrastructure compromise (the Provisioner is implicitly trusted; §5.4 F-21).

The mint-anywhere default fails the **least-privilege** test: a Pool-plugin-only node has no operational reason to mint cluster members, yet today it can.

---

## 3. Options analyzed

### A. Anyone (status quo)

- **Attack surface:** any compromised member → unbounded lateral admission.
- **Ergonomics:** trivial; no day-2 ceremony.
- **Bootstrap day-1:** trivial — the seed node mints, hand the token to node 2.
- **Verdict:** unacceptable for GA. Violates least-privilege; turns one compromised node into cluster takeover.

### B. Capability-gated: only nodes advertising `cluster.admin` may mint

- **Attack surface:** compromised non-admin node cannot expand the cluster. Admin set is small, auditable, hardened separately.
- **Ergonomics:** one new capability tag; reuses existing capability machinery (§4, §5). `boi node` CLI on a non-admin node returns `PermissionDenied` with a clear message.
- **Bootstrap day-1:** the seed node from `boi cluster init` auto-advertises `cluster.admin=true` (it is the only node that can; it owns the CA). Operator promotes additional admin nodes via `boi cluster admin grant <node_id>` which CAS-writes `caps.static.cluster_admin=true` on that node's caps record (admin-only op, enforced same gate).
- **Day-2 workflow:** `boi cluster admin {grant|revoke|list} <node_id>`. Revoke is immediate (next mint call re-reads caps from etcd snapshot, ≤TTL stale; tighten with a direct etcd read on every mint).
- **Provisioner interaction:** the Provisioner plugin (§5.4) is *co-located with `boi-core`*. It calls core's local `MintJoinToken` RPC. Core checks whether **the local node** has `cluster.admin`. So Provisioner plugins only function on admin nodes — which is the right answer: the node that allocates new infrastructure *is* exercising admin authority.
- **Verdict:** strong fit. Reuses the capability primitive the design already has.

### C. Out-of-band root credential (CA private key access)

- **Attack surface:** smallest possible — only operator with CA key can mint.
- **Ergonomics:** painful. Every Provisioner-driven autoscale needs the CA key on the autoscaling node, defeating §5.4 F-21 isolation. Operators paste keys into CLIs.
- **Bootstrap day-1:** fine for the first node; terrible thereafter.
- **Verdict:** rejected. Breaks the Provisioner contract and pushes long-lived root creds onto operational paths. Use as a **break-glass** only.

### D. N-of-M quorum mint

- **Attack surface:** strongest (compromising one admin node insufficient).
- **Ergonomics:** quorum coordination for every join — incompatible with sub-second Provisioner-driven autoscale (§8). 5-minute token TTL (F-21) does not leave room for human-paced quorum.
- **Bootstrap day-1:** chicken-and-egg — first node has no peers to form quorum with.
- **Verdict:** rejected for v0.1. Revisit in v0.2 alongside capability-fraud quarantine if a stronger trust model is needed.

---

## 4. Recommended decision

**Adopt Option B: token-mint authority is restricted to nodes whose `/boi/caps/{node_id}` record carries `caps.static.cluster_admin=true`; the mint RPC enforces this on the local node before calling `boi-bootstrap`, and `boi cluster init` auto-grants the seed node admin.**

**Exact mechanism:**
1. New static capability `cluster_admin: bool` in the caps schema (§4 row `/boi/caps/{node_id}`).
2. `boi-bootstrap` mint path (`MintJoinToken` RPC + `boi node token mint` CLI) first reads `/boi/caps/{self_node_id}` and rejects with `PermissionDenied` if `cluster_admin != true`. Read is a direct etcd `Get`, not the TTL-cached snapshot, so revocations take effect on the next call.
3. The `Provisioner.Allocate` flow (§8) calls `MintJoinToken` through the same gate — Provisioner plugins only function on admin nodes. Documented in §5.4.
4. **Bootstrap path:** `boi cluster init` writes the seed node's caps record with `cluster_admin=true` atomically with `/boi/cluster/ca` creation. There is always exactly one admin at t=0.
5. **Day-2 workflow:** `boi cluster admin grant <node_id>` / `revoke <node_id>` / `list`. These commands are themselves gated by `cluster_admin` on the invoking node (so only an admin can mint admins; resolves the chicken-and-egg post-bootstrap).
6. **Break-glass:** `boi cluster admin grant --ca-key <path> <node_id>` accepts a direct CA-key signature as an alternative to the cluster_admin gate, for the case where every admin node is dead. Documented, audited via Hooks `cluster.admin_break_glass`.

---

## 5. Implications on the design

**Sections to update in `distributed-architecture-design-2026-05-12.md`:**
- §4 caps schema: add `caps.static.cluster_admin: bool` with the writer being "issuing admin node via `boi cluster admin grant`."
- §5.4 Provisioner: add a sentence that `MintJoinToken` is admin-gated; Provisioner plugins are functional only on admin nodes; surface this in plugin-author docs (Judge 3 onboarding).
- §6 Bootstrap (first node): step 4 also writes `cluster_admin=true` for the seed node.
- §8 Provisioning flow: arrow from `core` to `/boi/join-tokens` annotated with "(admin-gated)."
- §10 Failure modes: add row — "Non-admin node attempts mint → `PermissionDenied`, surfaced via `cluster.mint_denied` Hooks event."
- §11 CLI: add `boi cluster admin grant | revoke | list [--ca-key <path>]`. Add `boi node token mint` (replaces the implicit any-node mint via `boi node`); its help text states the admin requirement.
- §13 v0.1 scope cut: add "Admin-gated join-token mint (Q3 resolution)" to the In-v0.1 list.
- §14: mark Q3 resolved with pointer to this decision.

**Wire-protocol change:** `bootstrap.proto` gains a no-arg `MintJoinToken` RPC whose authorization is server-side (the *local* core's identity); no client-side proof needed because it's a local Unix-socket RPC. The CLI invokes it the same way.

**Provisioner contract change (§5.4):** none to the proto — Provisioner still receives an opaque `join_token`. The change is purely on the *core* side: cores on non-admin nodes refuse to mint, which means Provisioner plugins simply error out there. Document this; the v0.1 expectation is "run Provisioner plugins on admin nodes."

**Migration impact (§12):** the single-node→cluster migration auto-grants admin to the existing node (it runs `boi cluster init`). No user-visible change for solo users.

---

## 6. Confidence: 8/10

**Why 8 and not 10:** the design assumes capability records are trustworthy enough to gate mint authority, but §6 step 4 lets nodes self-declare capabilities and N7 (capability-fraud quarantine) is explicitly deferred. The mitigation is that `cluster_admin` is a **`caps.static`** field written only via the `boi cluster admin grant` path (which itself enforces the gate), *not* something a joining node can self-advertise. As long as v0.1 enforces "static caps are write-once at join, mutated only via admin RPC," this holds. If that invariant slips and joining nodes can stuff `static.cluster_admin=true` into their initial caps payload, the entire scheme collapses to Option A. The mint RPC and the cap-write code paths must enforce this together; a conformance test belongs in the integration suite.

**What would change my mind:**
1. **Discovery that the v0.1 implementation cannot cheaply separate static-caps-from-admin-path vs static-caps-from-join-payload.** Then I'd push for Option C as a fallback (with documented break-glass UX cost) rather than ship a gate that doesn't actually gate.
2. **A concrete production deployment story where Provisioner plugins must run on every node** (e.g., per-node burst autoscale). Then Option B's "Provisioners only on admin nodes" becomes operationally noisy, and Option B+capability-delegation (a narrower `mint_join_token` capability separate from `cluster_admin`) becomes preferable. Unlikely for v0.1 workloads but worth re-examining for v0.2.
3. **Threat model shift to malicious operators** (not in scope today). Then Option D's quorum becomes worth its complexity.

---

**Decision owner sign-off:** required before §13 v0.1 list is finalized.
