# Migration Guide — Single-Node BOI → Distributed v0.1

This guide walks an existing single-node BOI deployment through the
move to the distributed v0.1 control plane. It assumes you are
running the pre-distributed (v0.0) single-binary daemon with a local
SQLite queue and want to end up on a `boi-node` cluster backed by
etcd, with gRPC plugins for Workspace and Worker Pool providers.

If you are starting from a clean machine, skip this guide and read
`docs/operator/v0.1.md` instead — bootstrap is simpler when there is
no in-flight state to preserve.

## Audience

Operators who already run BOI in production (or on a long-lived dev
host) and have:

- A populated `~/.boi/queue.sqlite` with active specs.
- Custom in-process Workspace or Worker Pool implementations
  registered against the v0.0 Rust traits.
- Local logs under `~/.boi/logs/` referenced by automation.

## What Changes in v0.1

| Area              | v0.0 (single-node)                       | v0.1 (distributed)                                    |
|-------------------|------------------------------------------|-------------------------------------------------------|
| Coordinator       | Single `boi` daemon                      | One-or-more `boi-node` processes electing a leader    |
| Queue store       | Local SQLite                             | etcd-backed dispatch queue (lease-fenced claims)      |
| Worker pool       | Rust trait, in-process `std::thread`     | `boi.pool.v1` gRPC plugin (host-managed subprocess)   |
| Workspaces        | Rust trait, in-process git worktree      | `boi.workspace.v1` gRPC plugin                        |
| Plugin packaging  | Linked into the binary at compile time   | Standalone executable, declared in node config        |
| Hooks             | In-process callbacks                     | `boi.hooks.v1` gRPC stream with HWM checkpoints       |
| Logs              | Direct file write from worker thread     | Plugin streams via `Tail` RPC, host tees to disk      |
| Auth between procs| (none, in-process)                       | mTLS with a per-cluster CA, rotated every 90 days     |

The single-binary `boi` CLI continues to work as a thin client — it
dials a node via gRPC instead of touching SQLite directly.

## Compatibility Matrix

- **Spec YAML.** Unchanged. v0.0 spec files run as-is on v0.1.
- **Hooks scripts.** Unchanged shell contract. The hook *bus* moved
  to gRPC, but the script invocation is identical.
- **Workspace plugins.** Old in-process traits are now legacy v0.0.
  You must port to the `boi.workspace.v1` gRPC contract (see
  `docs/plugins/getting-started.md`).
- **Worker pool plugins.** Same — port to `boi.pool.v1`.
- **CLI.** Most subcommands are unchanged; new flags `--node` and
  `--cluster` select a remote target (see `docs/cli/v0.1.md`).

## Pre-Migration Checklist

1. **Drain in-flight specs.** Wait for the queue to clear or use
   `boi cancel --all` for non-critical work. Active claims do not
   migrate cleanly across the SQLite-to-etcd cutover.
2. **Snapshot SQLite.** `cp ~/.boi/queue.sqlite ~/.boi/queue.v0.bak`.
   If migration fails you can roll back by reinstalling the v0.0
   binary and copying this file into place.
3. **Inventory custom plugins.** List every binary or library
   compiled against the old in-process traits. Each one needs a
   port.
4. **Reserve a control-plane host.** v0.1 expects at least one
   long-lived `boi-node` per region. For a single-machine migration
   the same host is fine.
5. **Plan a CA.** v0.1 mTLS uses a per-cluster CA. Generate it
   before installing nodes — see `docs/operator/v0.1.md`.

## Migration Steps

### Step 1 — Install etcd

A single-node etcd is sufficient for a one-host cluster. The
operator guide covers HA topology.

```
sudo apt-get install -y etcd-server etcd-client
sudo systemctl enable --now etcd
```

Verify: `etcdctl endpoint status --write-out=table`.

### Step 2 — Generate the cluster CA

```
boi ca init --out ~/.boi/pki
boi ca issue --role node --out ~/.boi/pki/node.pem
```

The CA private key MUST be backed up off the cluster host. Loss of
the CA forces a full re-enrollment of every node and plugin.

### Step 3 — Install `boi-node`

```
cargo install --path crates/boi-node
mkdir -p /etc/boi
cp examples/node.toml /etc/boi/node.toml
sudo systemctl enable --now boi-node
```

### Step 4 — Port custom plugins

For each in-process plugin you maintain:

1. Re-implement the gRPC service from `crates/boi-proto/proto/`.
2. Wrap your existing core logic in a `tonic` server.
3. Add a stanza under `[plugins.workspace]` or `[plugins.pool]` in
   `/etc/boi/node.toml`.

The plugin author quickstart in `docs/plugins/getting-started.md`
shows the minimum viable Workspace plugin in roughly fifty lines.

### Step 5 — Replay any drained work

Re-submit specs that were cancelled in step 1. Because spec files
are unchanged, this is a normal `boi run` against the new node.

### Step 6 — Decommission v0.0

Once the cluster has been stable for at least one rolling restart
cycle, remove the old binary, archive `~/.boi/queue.sqlite`, and
delete `~/.boi/logs/` (logs now live next to the host process and
are streamed through the pool plugin).

## Rollback

If migration fails before step 5:

1. Stop `boi-node`.
2. Restore `~/.boi/queue.v0.bak` to `~/.boi/queue.sqlite`.
3. Reinstall the v0.0 binary and start the legacy daemon.

After step 5 (work has been submitted against etcd) a rollback
forfeits in-flight v0.1 specs. Drain first.

## Known Gotchas

- **Lease TTLs.** etcd lease TTL defaults to 15 s. Slow disks can
  cause expired-claim churn — tune via `node.toml` `lease_ttl_secs`.
- **Hooks at-least-once.** The hooks bus is at-least-once. Idempotency
  must live in your hook script — duplicates are routine after a
  leader election.
- **Workspace path semantics.** Remote workspace backends may return
  a path that is meaningless on the host. Tools that expect a local
  filesystem path on the controller (older CI shims, custom hooks)
  must be updated to call into the plugin's `Exec` RPC.

See the operator guide for ongoing maintenance after the cutover.
