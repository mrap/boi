# Q4 — Plugin protocol versioning

## 1. Question (verbatim)

> **Q4. Plugin protocol versioning.** Does each plugin proto carry a `version` field, with core refusing plugins reporting a major mismatch? Or do we rely on file naming (`workspace.v1.proto`)? Affects breaking-change cadence for plugin authors.

## 2. Why this matters

Plugins are third-party binaries shipped on their own cadence and linked to core only at runtime over a Unix-domain socket (§5). Without a versioning discipline in v0.1:

- **Silent contract drift.** Core adds a field to `PrepareRequest`, a plugin built against an older `.proto` ignores it (protobuf default behavior), and a load-bearing hint (`git_ref`) goes unhonored — the workspace is wrong, no error is raised. This violates Standing Order S6 (no quiet failures).
- **Unbounded compatibility matrix.** Every BOI release × every plugin release becomes a tested combination. With 5 contracts (Workspace, Pool, Router, Provisioner, Hooks) the matrix explodes within two minor releases.
- **F-19 trap.** `/boi/caps/` → `/boi/nodes/` collapse is already deferred as a breaking change. Plugins that read capability snapshots (Router in particular) become a second irreversible commitment. Without an advertised version, we cannot deprecate cleanly.
- **F-10 rolling upgrade depends on it.** §6 "Rolling upgrade" assumes a version-skew band. There is no band to enforce without a handshake.
- **Plugin DX.** Authors need a deterministic answer to "will my binary load against core ≥X.Y?" — file-name guesses are insufficient.

## 3. Options analyzed

### A. File-name versioning only (`workspace.v1.proto`, gRPC service path `boi.workspace.v1.Workspace`)

- **Handshake:** none beyond gRPC's "method not found" Unimplemented error. New major = new package, new generated stubs, new service path.
- **Compile vs runtime:** entirely compile-time. Runtime mismatch surfaces as `UNIMPLEMENTED` on the first RPC.
- **Ergonomics:** familiar (Google APIs, Envoy xDS). But: no way for a single binary to support `v1` and `v2` without dual-registering services; no way for core to *introspect* what minor features a plugin supports; deprecation of a field within v1 is invisible.

### B. In-proto `version` field with handshake

- **Mechanism:** add a mandatory `Handshake` RPC to every service that returns `proto_major`, `proto_minor`, `plugin_name`, `plugin_version`, `supported_capabilities: repeated string`. Core calls it immediately after `BOI_READY\n` (§5 lifecycle), before any other RPC.
- **Compile vs runtime:** runtime enforcement. Core rejects mismatched majors, warns on minor skew, gates feature use on the capability list.
- **Ergonomics:** one extra method per service. Plugin authors return a small constant. Capability strings (e.g. `workspace.git_ref_hint`, `pool.idempotent_spawn`) let core selectively use newer fields against older plugins.

### C. Buf-style breaking-change detection in CI + semver tags only

- **Mechanism:** `buf breaking` in the BOI repo blocks PRs that break wire compatibility; plugin authors pin a tag.
- **Compile vs runtime:** all compile-time / pre-release. Nothing enforced at handshake.
- **Ergonomics:** great for *core* discipline, useless for *operator* safety. Says nothing about which binary an operator actually installed. Necessary but not sufficient.

### D. Per-method capability advertisement (no file versioning)

- Plugin announces `capabilities: [...]` at handshake; no package versioning. Major changes are just new capability strings.
- Problem: irreducible field-shape changes (renaming `workdir_path` → `workdir`) have no expression mechanism. Eventually you need a package bump.

## 4. Recommended decision — Hybrid (A + B + C)

Adopt **all three**, each at the layer it belongs:

1. **File-name versioning is the source of truth for wire breaks.** Every proto lives in a versioned package: `package boi.workspace.v1;` with service path `boi.workspace.v1.Workspace`. A `v2` ships as a parallel package; a core that speaks both registers both clients. **Rule: major version = new package, no exceptions.** This is what F-19 will eventually pay (a `v2` proto), not an in-place mutation.

2. **In-proto handshake is mandatory and load-bearing.** Every plugin service grows one method:

   ```proto
   service Workspace {
     rpc Handshake(HandshakeRequest) returns (HandshakeResponse);
     rpc Prepare(...) returns (...);
     rpc Cleanup(...) returns (...);
     rpc Health(Ping) returns (Pong);
   }
   message HandshakeRequest {
     string core_version = 1;            // semver, informational
     uint32 core_proto_minor = 2;        // highest minor core speaks for this package
   }
   message HandshakeResponse {
     string plugin_name = 1;             // e.g. "git-worktree"
     string plugin_version = 2;          // semver, informational
     uint32 plugin_proto_minor = 3;      // highest minor the plugin implements within this package's major
     repeated string capabilities = 4;   // e.g. ["workspace.git_ref_hint","workspace.shallow_clone"]
   }
   ```

   Core calls `Handshake` immediately after `BOI_READY\n` (extends §5 lifecycle). Rules core enforces:
   - **Major mismatch is implicit** (different package → different gRPC service path → `UNIMPLEMENTED`; core walks its supported-major list newest-first and stops at the first one the plugin answers). If none match, core marks the plugin `unstable` and surfaces `plugin.unsupported_major` to Hooks.
   - **Minor skew:** if `plugin_proto_minor < core_proto_minor`, core MUST NOT send fields introduced after `plugin_proto_minor`; it logs `plugin.minor_skew` once and proceeds. If `plugin_proto_minor > core_proto_minor`, core proceeds — protobuf unknown-field tolerance handles it; core warns once.
   - **Capability gating:** core checks `capabilities` before using any feature whose semantics depend on the plugin opting in (e.g. only sends `hints.git_ref` if `workspace.git_ref_hint` is advertised; only relies on idempotent `Spawn` for retry semantics if `pool.idempotent_spawn` is present).
   - **Health-check piggyback:** `Health(Ping)` response gains `plugin_proto_minor` for cheap re-verification after plugin restart.

3. **CI enforces wire stability within a major.** `buf breaking --against '.git#branch=main,subdir=proto'` runs on every BOI PR. Adding fields is allowed; renaming/renumbering/removing is rejected mechanically. A major bump requires a new `vN+1` package and a 1-minor-release deprecation window where core speaks both.

**Deprecation path for a field (worked example).**
- `vN.M`: field marked `[deprecated = true]` in proto; core still emits it; `Handshake` reports `core_proto_minor = M`; release notes call it out.
- `vN.M+1`: core continues to emit; `boi plugin test` warns plugin authors who consume it.
- `vN+1.0`: new package `boi.workspace.v2` ships without the field; core speaks both `v1` and `v2` during the deprecation window.
- `vN+2.0`: `v1` removed; `Handshake` against `v1` package fails with `UNIMPLEMENTED`; operator sees `plugin.unsupported_major` and is told to upgrade the plugin.

**Capability advertisement answers the §14 prompt directly.** A plugin can say "I implement `boi.workspace.v1` at `plugin_proto_minor=3` with capabilities `[git_ref_hint, shallow_clone]`" and core decides per-RPC how to call it. Yes — exactly the model.

**F-19 interaction.** The `/boi/caps/` → `/boi/nodes/` collapse touches the `ClusterSnapshot` shape that Router consumes (§5.3). Under this discipline it becomes a `boi.router.v2` ship, not an in-place mutation; v0.1 Routers continue to work against the `v1` package during the v0.2 deprecation window. F-19 stops being scary — it is a normal major bump.

## 5. Implications on the design

Sections to update in `distributed-architecture-design-2026-05-12.md`:

- **§5 Plugin contracts — lifecycle.** Insert a new bullet between `Start` and `Health-check`: "**Handshake:** immediately after `BOI_READY\n`, core calls `Handshake` on each service the plugin declares. Mismatched majors mark the plugin `unstable` (no retries); minor skew is logged once and tolerated; advertised capabilities gate optional fields. Handshake timeout reuses `plugin.ready_timeout_secs`."
- **§5.1–§5.5.** Add `rpc Handshake(...) returns (...);` to every service. Add the `HandshakeRequest`/`HandshakeResponse` shapes once in a `proto/common.v1.proto` and import.
- **§5.2 Pool — idempotency contract.** Predicate the *requirement* of idempotent `Spawn` on `capabilities` containing `pool.idempotent_spawn`. v0.1 ships with that capability mandatory (plugin-host harness fails plugins without it); v0.2+ may relax for plugins that opt out of retry semantics.
- **§5.3 Router — snapshot shape.** Add a note that `ClusterSnapshot` evolution follows the major/minor rules above; the F-19 collapse is now an explicit `boi.router.v2` candidate.
- **§6 Rolling upgrade.** Define the "version-skew band" concretely: core supports `current_major` and `current_major - 1` simultaneously; a node refuses to join a cluster running a different major from its own.
- **§11 What ships — `boi plugin test`.** The conformance harness grows three checks: (a) `Handshake` is implemented and returns a parseable response; (b) advertised capabilities match the methods/fields the plugin actually honors (harness sends each capability-gated field and asserts non-default behavior); (c) `buf breaking` is run against the plugin's own published `.proto` (for plugins that vendor proto changes).
- **§13 v0.1 scope cut.** Add to "In v0.1": "Plugin handshake protocol + buf-breaking CI + `v1` package convention." Remove "version-handshake + protocol-versioning" from F-10's deferred justification — we are doing it now because it is cheap (one RPC per service) and unblocks rolling upgrade.
- **§14.** Mark Q4 resolved; reference this file.

## 6. Confidence: 8/10

This is the standard play (HashiCorp plugin-system pattern, Envoy xDS package versioning, gRPC's own guidance) adapted to BOI's lifecycle. The one nontrivial bet is **capability strings as first-class API**: it works beautifully when capabilities map cleanly to optional fields/methods, and degrades into namespace soup if abused. Discipline required: every capability needs a written semantic in `proto/common.v1.proto` comments.

**What would change my mind:**

1. **Plugin authors universally tooling on grpcurl / reflection only.** If most plugin authors are scripting against gRPC reflection rather than generating stubs, the `Handshake` method becomes friction they will skip. Mitigation: ship a 30-line reference `Handshake` impl in every language.
2. **Discovery that BOI core is the only realistic plugin author** (i.e., third-party plugins don't materialize). Then this is overkill; collapse to option A.
3. **A capability-explosion in practice** — if v0.2 already needs 20 capability strings per service, the model is wrong and we should bite the bullet on more frequent major bumps.
4. **etcd-backed plugin registry arriving in v0.2** (deferred N8). A registry could carry version metadata out-of-band, reducing the value of in-proto `Handshake`. Even then, runtime handshake remains correct as defense-in-depth.
