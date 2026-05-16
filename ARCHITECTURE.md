# BOI Architecture

System map for BOI (Beginning of Infinity) — a Rust binary that dispatches Claude Code workers to execute spec-defined tasks in isolated git worktrees.

## Repository Map

| Path | Purpose | Key files |
|------|---------|-----------|
| `src/` | **Primary binary** — all `boi` CLI logic | `main.rs`, `lib.rs`, `queue.rs`, `worker.rs`, `spec.rs` |
| `src/cli/` | One file per subcommand (22 files) | `daemon.rs`, `dispatch.rs`, `status.rs`, `doctor.rs` |
| `src/workspace/` | `WorkspaceBackend` trait + `GitWorkspace` | `mod.rs`, `git.rs` |
| `src/pool/` | `WorkerPool` trait + `LocalThreadPool` | `mod.rs`, `local.rs` |
| `src/remote/` | Remote worker backends | `fly.rs` (Fly Machines API) |
| `src/runtime/` | LLM runtime backends | `claude.rs`, `codex.rs`, `openrouter.rs` |
| `crates/boi-cluster/` | etcd-backed cluster state (claims, dispatch, membership) | `src/client.rs`, `src/claims.rs`, `src/membership.rs` |
| `crates/boi-node/` | Cluster node daemon binary | `src/main.rs` |
| `crates/boi-identity/` | TLS cert generation for node identity | `src/lib.rs` |
| `crates/boi-assign/` | HRW rendezvous-hash assignment | `src/assign.rs` |
| `crates/boi-plugin-host/` | Plugin lifecycle: spawn, handshake, restart | `src/lib.rs` |
| `crates/boi-proto/` | gRPC contracts (tonic/prost) | `src/lib.rs` |
| `crates/boi-mock-plugin/` | Mock plugin for e2e tests | `src/main.rs` |
| `crates/boi-test-harness/` | Shared test helpers (`e2e` feature gate) | `src/lib.rs` |
| `phases/` | Phase TOML configs + `pipelines.toml` | `*.phase.toml`, `pipelines.toml` |
| `hooks/` | `default.yaml` — compiled into binary via `include_str!` | `default.yaml` |
| `templates/` | Worker prompt templates | `worker-prompt.md` |
| `docs/` | Design docs, historical docs, agent-facing topic docs | `docs/agents/*.md` |
| `_archive/python/` | **DEPRECATED** — original Python implementation | Do not use |

The main `boi` binary does NOT depend on `boi-cluster` or `boi-node`. `boi-assign` is used only by `boi-node` and is not listed in workspace `members`.

Workspace members (`Cargo.toml:2`): `"."`, `boi-test-harness`, `boi-node`, `boi-cluster`, `boi-identity`, `boi-proto`, `boi-plugin-host`, `boi-mock-plugin`.

## Core Concepts

BOI operates on **specs** — YAML files that define tasks with verify commands. A **daemon** polls a SQLite queue, dispatches **workers** into isolated git **worktrees**, and runs tasks through a configurable **pipeline** of **phases**. **Hooks** fire on lifecycle events for external integration.

For full definitions of each term, see [docs/agents/glossary.md](docs/agents/glossary.md).

## Lifecycle — Single-node (default)

```
User                    Daemon                    Worker
  |                       |                         |
  |-- dispatch spec.yaml->|                         |
  |                       |-- enqueue (SQLite) ---->|
  |                       |   poll every 5s         |
  |                       |-- dequeue (BEGIN IMMEDIATE, status=assigning)
  |                       |-- spawn worker -------->|
  |                       |                         |-- spec_pre_phases
  |                       |                         |   (spec-critique, spec-improve)
  |                       |                         |-- for each task:
  |                       |                         |     create worktree
  |                       |                         |     task_phases (execute, task-verify)
  |                       |                         |     update DB, fire hooks
  |                       |                         |-- spec_post_phases (critic)
  |                       |                         |-- cleanup worktree
  |                       |<-- complete ------------|
```

Step by step:

1. User dispatches: `boi dispatch spec.yaml [--mode discover] [--after q-NNN]`
2. Spec parsed, validated (`validate_intake` at `src/spec.rs:411`), enqueued to SQLite with atomic ID generation
3. Daemon polls every 5s, dequeues with a transaction (`unchecked_transaction()`) → atomically sets status `assigning` (prevents double-dispatch) (`src/queue.rs:514`, status update at `src/queue.rs:545`)
4. Worker thread created via `LocalThreadPool` (`src/pool/local.rs`):
   a. Runs `spec_pre_phases` (e.g., `spec-critique`, `spec-improve`) — once per spec
   b. For each task: runs `task_phases` in order (e.g., `execute` → `task-verify`)
   c. Runs `spec_post_phases` (e.g., `critic`) — once after all tasks
5. Per task phase: creates git worktree → builds prompt → spawns `claude -p` → monitors with timeout
6. On task complete: runs verify command, updates DB, fires `on_task_complete` hook
7. On task fail: retries up to max_iter, fires `on_task_fail` hook
8. On spec complete: fires `on_complete` hook, cleans up worktree
9. On daemon restart: `recover_stuck_specs()` recovers stuck running/assigning → queued (`src/queue.rs:1530`)

## Lifecycle — Distributed (v0.1, `boi-node`)

- Each machine runs `boi-node` daemon; cluster state in etcd
- gRPC API at `0.0.0.0:7001` (`DEFAULT_ADDR` in `crates/boi-node/src/main.rs:40`); services: cluster, pool, router, workspace, provisioner, hooks
- Assignment via HRW rendezvous hash (`crates/boi-assign/src/assign.rs`)
- Claims via etcd CAS (`crates/boi-cluster/src/claims.rs`)
- Node membership: etcd-watch with 30s TTL (`crates/boi-cluster/src/membership.rs`)
- Prometheus metrics on port 9090
- Hooks WAL at `~/.boi/hooks-wal/` for audit-tier delivery

## State Locations (`~/.boi/`)

| Path | Purpose |
|------|---------|
| `boi-rust.db` | SQLite database (9 tables: specs, tasks, iterations, events, workers, processes, phase_runs, bench_results, runners) |
| `config.yaml` | Worker config, pool type, paths, Fly settings (`src/config.rs:244`) |
| `.env` | Secrets — auto-loaded at startup via dotenvy (process env wins on conflict) |
| `hooks.yaml` | Hook overrides (falls back to compiled-in `hooks/default.yaml`) |
| `worktrees/<id>/` | Ephemeral git worktrees per spec |
| `logs/` | Per-spec log files |
| `telemetry/boi.jsonl` | Structured lifecycle event log |
| `daemon.pid` | Daemon process ID |
| `daemon.heartbeat` | Last heartbeat timestamp |
| `env.sh` | Canonical env setup sourced by shells + daemon plist |
| `hooks-wal/` | Audit-tier hooks WAL (distributed mode) |

## Hook Points

BOI fires hooks on lifecycle events (`src/hooks.rs:20-33`). Hook config lives in `~/.boi/hooks.yaml`; if absent, falls back to `hooks/default.yaml` (compiled into the binary via `include_str!`).

Available events: `on_dispatch`, `on_worker_start`, `on_task_start`, `on_task_complete`, `on_task_fail`, `on_phase_start`, `on_phase_complete`, `on_phase_fail`, `on_phase_skip`, `on_complete`, `on_fail`, `on_cancel`, `on_stall`, `on_spec_paused`.

Hook payloads are JSON. Breaking changes to payload structs break hex consumers — see [docs/agents/invariants.md](docs/agents/invariants.md) #6.

For the full hook system specification, see [docs/boi-hooks-spec.md](docs/boi-hooks-spec.md).

## Pipeline & Phase System

Phases are TOML-configured execution steps. Each phase config (`phases/*.phase.toml`) must declare `level`, `can_add_tasks`, and `can_fail_spec` (`src/phases.rs:198`).

Pipelines (`phases/pipelines.toml`) define ordered phase sequences per mode:

| Mode | Pre-phases | Task phases | Post-phases |
|------|-----------|-------------|-------------|
| `execute` | spec-critique, spec-improve | execute, task-verify | critic |
| `challenge` | plan-critique | execute, task-verify | critic |
| `discover` | — | execute, task-verify | critic, evaluate |
| `generate` | plan-critique | decompose, execute, code-review, task-verify | critic, evaluate |
| `v2` | spec-critique, spec-improve | execute, review, commit | doc-update, critic, merge, cleanup |

For the full spec YAML schema, see [docs/agents/spec-format.md](docs/agents/spec-format.md). For pipeline design rationale, see [docs/pipelines.md](docs/pipelines.md).

## Key Traits

| Trait | Location | Purpose |
|-------|----------|---------|
| `WorkspaceBackend` | `src/workspace/mod.rs:21` | Pluggable isolation: create, exec, merge, cleanup |
| `WorkerPool` | `src/pool/mod.rs:52` | Pluggable execution: spawn, status, collect, cancel |

Default implementations: `GitWorkspace` (git worktrees) and `LocalThreadPool` (OS threads). Remote: `Fly` backend in `src/remote/fly.rs`.

## Further Reading

- [docs/agents/invariants.md](docs/agents/invariants.md) — what NOT to break
- [docs/agents/adding-features.md](docs/agents/adding-features.md) — change recipes
- [docs/agents/conventions.md](docs/agents/conventions.md) — coding patterns
- [docs/boi-rust-architecture.md](docs/boi-rust-architecture.md) — Rust implementation details
- [docs/boi-hooks-spec.md](docs/boi-hooks-spec.md) — hook system specification
- [docs/pipelines.md](docs/pipelines.md) — pipeline and phase system design
