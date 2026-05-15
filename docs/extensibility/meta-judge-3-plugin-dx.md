## Judge 3 — Plugin author experience

I'm a platform engineer at a hypothetical Meta-shaped corp. My job: ship a Workspace plugin that mounts our internal source-control snapshot (not git) into a working directory, and a Provisioner plugin that allocates from our internal bare-metal scheduler (not EC2). I need to write these in Python or Go, test them on my laptop, and not have my on-call paged when BOI core ships a new minor version. With that lens, the three designs vary wildly.

### Alpha — gossip mesh

**Concepts to learn before line 1 of code:** five protobuf services (Workspace, Pool, Router, Provisioner, Hooks), the `NodeRecord`/CapMap schema, the gossip wire format (because §7 lists it as a stable contract — meaning plugin behavior can leak into it), SWIM suspect/dead semantics, and the `TryClaim` CAS protocol (because if my Pool plugin lies about `workers_busy`, claims get rejected on the target node and I have to debug a distributed race). The capability map is the only contract I really *need*, but the doc forces me to understand membership to reason about why my plugin's Provisioner result "didn't take" — §4 step 4 says the dispatcher polls gossip for `status=Alive`, so my plugin's "done" doesn't mean done. **My provisioner is implicitly required to inject a `node_id`, seeds list, and capabilities into the booting node** (§4 step 2) — that is a real chunk of bootstrap code, undocumented as a contract, and it's the kind of thing that will silently break on a BOI core upgrade.

**Boundary failures:** Cap mismatch between what my Provisioner injects and what the node ends up advertising → task never schedules, no clear error. Indirect-ping false-positives across my corp NAT (Alpha's own self-review flags this) → my freshly provisioned node gets declared Dead, my pager fires.

**Isolation testability:** Workspace plugin — yes, it's a stateless RPC. Provisioner — **no.** I cannot meaningfully integration-test without standing up at least 2 BOI nodes plus a seed, because the contract is "node eventually appears in gossip as Alive."

**Hello world Workspace:**
```python
class Workspace(WorkspacePluginServicer):
    def Setup(self, req, ctx):
        path = f"/tmp/ws/{req.task_id}"
        os.makedirs(path); return SetupResponse(workdir=path)
    def Teardown(self, req, ctx):
        shutil.rmtree(f"/tmp/ws/{req.task_id}"); return TeardownResponse()
```
Plus a Unix socket, plus a `boi.toml` stanza. Maybe 40 lines. Provisioner hello world is 200+ lines because it has to plumb seeds.

**Lock-in:** Medium-low. Pure gRPC, no external store. But the gossip wire format being a stable contract means if I write tooling that taps into membership, I'm coupled to BOI's internal protocol.

### Bravo — single primary

**Concepts:** same five plugin protos *plus* a sixth (Seeder), the Primary lease, terms, the quorum journal, and the role-transfer pause window. Most of that is invisible to plugin authors — Bravo correctly hides cluster state behind the Primary. The Router plugin contract is the cleanest of the three (`Score(...)` — pure function, no state). The Provisioner contract is also the cleanest: I return `ProvisionAck(node_id_hint, deadline)`, the Primary handles join-watching. **I don't have to inject seeds — the new node uses `seed=Primary` (§4 diagram).** That's a much smaller bootstrap surface.

**Boundary failures:** During a Primary role transfer (100–500 ms, possibly seconds per their own self-review), my plugin RPCs that go through the Primary get stalled. If my Provisioner takes 25 s and the Primary fails at second 20, the pending-provision state may or may not survive the journal replay — Bravo's spec doesn't actually say.

**Isolation testability:** Best of the three. I can mock a single Primary endpoint and drive my plugin end-to-end. The 6-page lease protocol is BOI core's problem, not mine.

**Hello world Provisioner:**
```go
func (s *Prov) Provision(ctx, req) (*ProvisionAck, error) {
    nodeID, _ := s.scheduler.Allocate(req.RequiredCaps)
    return &ProvisionAck{NodeIdHint: nodeID, ExpectedJoinDeadline: 30}, nil
}
```
Maybe 30 lines. The booting node just needs the Primary address.

**Lock-in:** Low. The plugin contract is small and the Primary abstraction means I never touch cluster internals.

### Charlie — etcd-backed

**Concepts:** five protos, *plus* etcd. The doc claims plugins talk gRPC only — but read §4: the Provisioner plugin's contract is "Allocate returns once the node is reachable; node does its own etcd join" (line 277). So **my Provisioner plugin must ship code that knows how to write to etcd at first boot** (lease grant, `/boi/nodes/{id}/caps` put, keepalive loop). That is a giant leak. I now need etcd client libraries, cluster CA certs distributed to every provisioned node, and an understanding of etcd lease semantics. The "external store as backbone" choice has externalized half of BOI's bootstrap protocol into plugin authors' code.

**Boundary failures:** etcd cert rotation, lease TTL mismatch (my newly booted node takes 35 s to come up, default lease is 30 s — silent failure), etcd endpoint config drift, `assigning/{task_id}` lease-attached key semantics. The 30-second TTL trade-off is called out in Charlie's own self-review as deployment-dependent — meaning my plugin may need to know it.

**Isolation testability:** Workspace plugin — yes. Provisioner plugin — **no, I need a real etcd to integration-test**, because the contract bottoms out in "node appears in etcd." I cannot fake this with a BOI mock.

**Hello world Provisioner:** ~150 lines, of which 100 are etcd bootstrap on the provisioned node side. The fact that I have to write that code at all is the cliff.

**Lock-in:** **Highest of the three.** Switching BOI deployments means switching etcd clusters. My Provisioner has etcd hardcoded in its boot flow. If a future BOI moves to Consul or to a Bravo-style internal journal, my plugin is dead weight.

### Ranking (best DX → worst)

1. **Bravo.** Smallest plugin surface, clean Primary indirection, easiest isolation testing.
2. **Alpha.** Reasonable hello-world, but Provisioner authors must own seed-injection bootstrap and the gossip wire format is a stable contract (leak).
3. **Charlie.** Worst plugin DX. The Provisioner contract leaks etcd into plugin authors' code, integration tests require a real etcd, and lock-in is structural.

### Worst: Charlie

The single most painful onboarding cliff: **writing a Provisioner means writing an etcd client that runs on the freshly-booted node and registers it correctly under a lease tied to a CA you have to ship.** That's not a plugin — that's a distributed-systems homework assignment masquerading as a sidecar contract.
