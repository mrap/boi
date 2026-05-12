# boi-test-harness

Hermetic E2E harness for the distributed BOI v0.1 architecture. Drives a
Docker Compose topology (etcd + N `boi-node` containers + plugin
sidecars) from Rust tests, captures diagnostic artifacts on failure, and
ships a CI workflow that runs the suite on every PR.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│ docker compose network: boi-test                            │
│                                                             │
│  ┌─────┐    ┌─────────┐  ┌─────────┐  ┌─────────┐           │
│  │etcd │◄───┤ node-a  │  │ node-b  │  │ node-c  │           │
│  └──┬──┘    └─────────┘  └─────────┘  └─────────┘           │
│     │             ▲            ▲           ▲                │
│     │             └────────────┴───────────┘                │
│     │                  mTLS gRPC (Phase 1+)                 │
│     │                                                       │
│     │       ┌────────────────────────┐                      │
│     └──────►│   plugin-sidecar       │                      │
│             └────────────────────────┘                      │
└─────────────────────────────────────────────────────────────┘
        ▲
        │  cargo test --features e2e
        │
   ┌────┴─────────────┐
   │ tests/e2e_*.rs   │  driven by helpers in src/lib.rs
   └──────────────────┘
```

State lives in etcd only; there are no named volumes. `docker compose
down -v` between tests guarantees identical results when `make e2e` is
re-run.

## How to add a test

1. Create `tests/e2e_<topic>.rs`.
2. Inside, start the topology via `boi_test_harness::start_cluster(N)`
   and drive it through `etcdctl_get_prefix`, `wait_for_etcd_key`, and
   the soon-to-arrive gRPC clients.
3. On every assertion failure, call `dump_artifacts("<test_name>")`
   so the red run is diagnosable.
4. Never `sleep` — wait for state with `wait_for_etcd_key`'s bounded
   timeout. Tests that flake fail the spec.
5. Each test should take less than 90 seconds. Tear down with the
   `Cluster` `Drop` impl (idiomatic) or call `cluster.down()`
   explicitly.

## Running

```bash
# Full suite
make e2e

# One test
make e2e ARGS="--filter e2e_bootstrap"

# Interactive: bring topology up, poke around, tear down
make e2e-up
make e2e-down
```

Outside of `--features e2e`, `cargo check -p boi-test-harness` builds
the helpers without pulling in heavy deps (testcontainers, tonic) so
contributors can iterate fast.

## What `dump_artifacts` produces

`e2e-artifacts/<test_name>/`:

| File              | Contents                                            |
|-------------------|-----------------------------------------------------|
| `etcd-prefix.txt` | Full `etcdctl get --prefix /boi/` dump              |
| `etcd.log`        | `docker logs etcd`                                  |
| `node-a.log`      | `docker logs node-a` (and same for b, c)            |
| `plugin-sidecar.log` | `docker logs plugin-sidecar`                     |
| `trace.json`      | proto RPC trace (placeholder; Phase 1+ wires real)  |

CI uploads this directory as a workflow artifact when `make e2e` fails.

## Red baseline

Today every `tests/e2e_*.rs` test fails — by design — because Phases
1-9 are not implemented. `tests/smoke.rs` is the one test that passes,
and it asserts only that the harness scaffolding (compose file + etcd
image + readiness probe) works end-to-end.

The mapping from failing subtest → implementation phase lives in
`docs/superpowers/plans/e2e-red-baseline.md` once task T29A0 runs the
suite and produces the baseline log.
