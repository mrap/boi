# BOI — Agent Onboarding Reference

BOI (Beginning of Infinity) is a Rust binary (v2.0.0) that dispatches Claude Code workers to execute spec-defined tasks in isolated git worktrees. It manages a SQLite-backed queue, a phase-based execution pipeline, lifecycle hooks, and structured telemetry. BOI is used by [mrap-hex](file:///Users/mrap/mrap-hex) (operator layer that configures hooks) and emits events to [hex-events](~/.hex-events/). BOI itself has zero hex-specific code. Parent conventions: [hex-foundation AGENTS.md](~/github.com/mrap/hex-foundation/AGENTS.md).

## Repository Map

| Path | Purpose | Key files |
|------|---------|-----------|
| `src/` | **Primary binary** — all `boi` CLI logic | `main.rs`, `lib.rs`, `queue.rs`, `worker.rs`, `spec.rs` |
| `src/cli/` | One file per subcommand (21 files) | `daemon.rs`, `dispatch.rs`, `status.rs`, `doctor.rs` |
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
| `templates/` | Worker prompt template | `worker-prompt.md` |
| `docs/` | Architecture docs, design docs, specs | `architecture.md`, `boi-hooks-spec.md`, `pipelines.md` |
| `_archive/python/` | **DEPRECATED** — original Python implementation | Do not use |
| `scripts/` | Misc utility scripts | `autoresearch-propose.py` |

**Note:** `src/` is the primary implementation. `crates/` contains auxiliary binaries and libraries for distributed/cluster features. The main `boi` binary does NOT depend on `boi-cluster` or `boi-node`. `boi-assign` is not listed in workspace `members` — it's used only by `boi-node`.

**Note:** `CONTRIBUTING.md` is **stale** — it describes the Python implementation. Actual dev commands are `cargo build`, `cargo test`, `cargo clippy`.

## Core Concepts

| Concept | Definition | Where in code |
|---------|-----------|---------------|
| **Spec** | A YAML file defining a unit of work: title, mode, tasks, dependencies | `src/spec.rs` |
| **Task** | One item within a spec; has id, title, spec text, verify command, status | `src/spec.rs` (BoiTask struct) |
| **Iteration** | One attempt at executing a task (retry = new iteration) | `src/queue.rs` (iterations table) |
| **Status: queued** | Spec is waiting for daemon to pick it up | `src/queue.rs` |
| **Status: assigning** | Atomic transitional state — daemon has claimed spec, preventing double-dispatch | `src/queue.rs:437-439` |
| **Status: running** | Worker is actively executing the spec | `src/worker.rs` |
| **Worker** | An OS thread (LocalThreadPool) or remote machine (Fly) executing one spec | `src/pool/` |
| **Daemon** | Background process that polls queue, dispatches workers, monitors health | `src/cli/daemon.rs` |
| **Worktree** | Ephemeral git worktree at `~/.boi/worktrees/<spec-id>/` — destroyed after spec | `src/workspace/git.rs` |
| **Phase** | A named execution step (e.g., `execute`, `task-verify`, `critic`) configured via TOML | `src/phases.rs`, `phases/*.phase.toml` |
| **Pipeline** | Ordered sequence of phases for a mode (e.g., `generate` mode) | `phases/pipelines.toml` |
| **Hook** | Subprocess fired on lifecycle events; configured in `~/.boi/hooks.yaml` | `src/hooks.rs` |
| **Telemetry event** | Append-only JSONL entry at `~/.boi/telemetry/boi.jsonl` | `src/telemetry.rs` |

## Lifecycle / Flow

### Single-node (default)

1. User dispatches: `boi dispatch spec.yaml [--mode discover] [--after q-NNN]`
2. Spec parsed, validated (`validate_intake`), enqueued to SQLite with atomic ID generation
3. Daemon polls every 5s, dequeues with atomic `BEGIN IMMEDIATE` → sets status `assigning` (prevents double-dispatch)
4. Worker thread created via `LocalThreadPool`:
   a. Runs `spec_pre_phases` (e.g., `spec-critique`, `spec-improve`) — once per spec
   b. For each task: runs `task_phases` in order (e.g., `execute` → `task-verify`)
   c. Runs `spec_post_phases` (e.g., `critic`) — once after all tasks
5. Per task phase: creates git worktree → builds prompt → spawns `claude -p` → monitors with timeout
6. On task complete: runs verify command, updates DB, fires `on_task_complete` hook
7. On task fail: retries up to max_iter, fires `on_task_fail` hook
8. On spec complete: fires `on_complete` hook, cleans up worktree
9. On daemon restart: `recover_stuck_specs()` recovers stuck running/assigning → queued (`src/queue.rs:1530-1544`)

### Distributed (v0.1 — `boi-node`)

- Each machine runs `boi-node` daemon; cluster state in etcd
- HTTP fleet API at `0.0.0.0:7701`: `GET /v1/specs/next`, `POST /v1/specs/{id}/complete`
- Assignment via HRW rendezvous hash (`crates/boi-assign/src/assign.rs`)
- Claims via etcd CAS (`crates/boi-cluster/src/claims.rs`)
- Node membership: etcd-watch with 30s TTL (`crates/boi-cluster/src/membership.rs`)
- Prometheus metrics on port 9090
- Hooks WAL at `~/.boi/hooks-wal/` for audit-tier delivery

## Critical Invariants

If you violate these, the system breaks.

| # | Invariant | Enforcement |
|---|-----------|-------------|
| 1 | **Migrations are append-only.** Never modify `migrate_v1` or `migrate_v2`. New schema changes require `migrate_vN` + `SCHEMA_VERSION` bump. | `src/queue.rs:308-334` — version guard |
| 2 | **SQLite single-writer per DB path.** WAL allows concurrent reads; one writer only. `busy_timeout=5000ms`. | `src/queue.rs:182` — PRAGMA; violation = corruption or 5s stall then error |
| 3 | **`assigning` prevents double-dispatch.** `dequeue()` uses `BEGIN IMMEDIATE` to atomically claim a spec. | `src/queue.rs:437-439` (BEGIN IMMEDIATE), `src/queue.rs:546` (UPDATE) |
| 4 | **Worktrees are ephemeral.** Never edit files in `~/.boi/worktrees/` directly — destroyed on cleanup. | Convention (not enforced in code) |
| 5 | **Verify commands must be idempotent.** Worker may re-run verify on retry. `CREATE TABLE`-style verify breaks retries. | Convention (not enforced) |
| 6 | **Hook payloads are stable JSON contracts.** Breaking changes to payload structs break hex consumers. | Convention (not enforced) — no schema versioning in hook payloads |
| 7 | **Phase TOMLs must declare `level`, `can_add_tasks`, `can_fail_spec`.** | `src/phases.rs` — `PhaseConfig::from_toml` returns Err on missing fields; daemon exits 2 |
| 8 | **Pipelines use `spec_pre_phases`/`spec_post_phases`** (not legacy `spec_phases`). | `phases/pipelines.toml` — loud WARN at load time on legacy shape |
| 9 | **Spec intake validation.** Non-PENDING task statuses rejected before DB write. | `src/spec.rs:411` — `validate_intake()`; called from `src/queue.rs:428` |
| 10 | **Daemon restart recovery.** Specs stuck in running/assigning reset to queued. | `src/queue.rs:1530-1544` — `recover_stuck_specs()` |

## Code Conventions

| Concern | Pattern | Exemplary file |
|---------|---------|---------------|
| Error handling (main crate) | `anyhow::Result` | `src/config.rs` |
| Error handling (lib crates) | `thiserror` enums | `crates/boi-cluster/src/client.rs` |
| Async runtime | tokio `rt-multi-thread` | `src/cli/daemon.rs` |
| Logging (main crate) | `boi_log!` macro → `eprintln!` with timestamp | `src/worker.rs:17-21` |
| Logging (cluster crates) | `tracing::{info,warn,debug,error}` | `crates/boi-node/src/main.rs` |
| Tests | Inline `#[cfg(test)]` modules at bottom of file | `src/queue.rs:1839+` |
| SQLite test isolation | `serial_test::serial` attribute | `src/queue.rs` |
| Atomic file writes | Write to `.tmp` then `mv` | `src/telemetry.rs` |
| Config | YAML via `serde_yml` into typed structs | `src/config.rs` |
| CLI | `clap` derive with `Commands` enum | `src/main.rs:54` |
| Commit prefixes | `feat:`, `fix:`, `test:`, `release:`, `salvage:` | `git log --oneline` |
| Formatting | `cargo fmt` (default rustfmt) | — |

## How to Make Common Changes

### New CLI subcommand

1. Add variant to `Commands` enum in `src/main.rs:54`
2. Create handler in `src/cli/mycommand.rs`
3. Add `pub mod mycommand;` in `src/cli/mod.rs`
4. Wire the match arm in `main()` in `src/main.rs`

### New crate

1. Create `crates/my-crate/` with `Cargo.toml` and `src/lib.rs` (or `src/main.rs` for binary)
2. Add to workspace `members` in root `Cargo.toml:2`
3. Add path dependency from consumers: `my-crate = { path = "../my-crate" }`

### New hook event

1. Add `pub const ON_MY_EVENT: &str = "on_my_event";` in `src/hooks.rs`
2. Call `hooks::fire(ON_MY_EVENT, &payload)` from the lifecycle point (usually `src/worker.rs`)
3. Add entry in `hooks/default.yaml` if it should have a default handler
4. Document in `docs/boi-hooks-spec.md`

### New migration

1. Bump `SCHEMA_VERSION` in `src/queue.rs:174`
2. Add `fn migrate_vN(conn: &Connection) -> Result<()>` below existing migrations
3. Add call in the version match in `run_migrations()` (`src/queue.rs:308-334`)
4. **Never modify an existing `migrate_vN` function**
5. Test with `cargo test` — queue tests run against temp SQLite

### New workspace backend

1. Create `src/workspace/mybackend.rs` implementing `WorkspaceBackend` trait (`src/workspace/mod.rs:21`)
2. Satisfy the four invariants: isolation, idempotent create, best-effort cleanup, exec in-directory
3. Add `pub mod mybackend;` in `src/workspace/mod.rs`
4. Wire backend selection in config/daemon startup

### New worker pool backend

1. Create `src/pool/mypool.rs` (or `src/remote/mypool.rs`) implementing `WorkerPool` trait (`src/pool/mod.rs:52`)
2. Implement: `spawn`, `status`, `collect`, `cancel`, optionally `cleanup`, `max_workers`
3. Wire into `worker_pool.type` config option in `src/config.rs`

### New phase

1. Create `phases/my-phase.phase.toml` with required fields: `level`, `can_add_tasks`, `can_fail_spec`
2. Add the phase name to a pipeline in `phases/pipelines.toml`
3. If the phase needs a built-in implementation, add it in `src/builtins.rs`

## How to Debug a Failing Spec

1. **Check status:** `boi status <spec-id> -v` — shows current phase, task states, runtime info
2. **Check log:** `boi log <spec-id> --debug` — shows claude output, verify results
3. **Follow live:** `boi log <spec-id> -f` — tails daemon log filtered to this spec
4. **Check telemetry:** `~/.boi/telemetry/boi.jsonl` — structured lifecycle events with cost/token data
5. **Check per-spec logs:** `~/.boi/logs/<spec-id>/` — raw log files
6. **Check daemon health:** `boi doctor` — verifies daemon liveness, DB integrity, worktree state, config validity
7. **Check for stuck specs:** `boi status --all` — if a spec is stuck in `assigning`, daemon may have crashed mid-dispatch; restart daemon to trigger `recover_stuck_specs()`

### Common root causes

| Symptom | Cause | Fix |
|---------|-------|-----|
| Spec stuck as `running`/`assigning` | Daemon crashed mid-dispatch | `boi daemon restart` — auto-recovers via `recover_stuck_specs()` |
| Daemon refuses to start (exit 2) | Phase TOML missing required fields | Add `level`, `can_add_tasks`, `can_fail_spec` to the offending `.phase.toml` |
| Verify passes once then fails on retry | Verify command not idempotent (e.g., `CREATE TABLE`) | Rewrite verify to be idempotent |
| Worktree creation fails | Branch `boi/<spec-id>` already exists from a prior run | `boi prune-orphans --apply` or `git worktree remove` + `git branch -D` |
| Claude subprocess timeout | Task too large for timeout setting | Increase `--timeout` on dispatch |
| Hook subprocess hung | Missing `timeout` in hook config | Add `timeout: 10` to hook entry in `~/.boi/hooks.yaml` |
| Two daemons against same DB | `busy_timeout` causes 5s stall then error | Kill one daemon; check `~/.boi/daemon.pid` |

## Verification Before Declaring Done

Run these before claiming any task is complete:

```bash
cargo build --release       # must compile without errors
cargo test                  # must pass (1 #[ignore]'d test in src/worker.rs — see below)
cargo clippy                # check for warnings (not currently enforced as -D warnings)
boi doctor                  # if daemon-related changes
```

**Known ignored test** (`src/worker.rs:2786`): `test_redo_tasks_are_executed` — BUG M-5: task stays `in_progress` after Redo, blocks re-selection. Skipped by default; run with `--include-ignored` to surface.

## Things NOT to do

- **Don't edit files inside `~/.boi/worktrees/`** — they are ephemeral and destroyed on cleanup
- **Don't run two `boi daemon` instances against the same DB** — causes corruption or silent failures
- **Don't modify existing `migrate_vN` functions** — migrations are append-only; add a new `migrate_vN+1`
- **Don't break hook payload JSON schema** without updating hex consumers in mrap-hex
- **Don't bypass the migration system** to alter SQLite schema directly (no raw `ALTER TABLE` outside migrations)
- **Don't add a dependency without justification** in the commit message
- **Don't commit secrets** — `.env` is gitignored; verify `.gitleaksignore` covers any false positives
- **Don't remove or rename phase TOML required fields** (`level`, `can_add_tasks`, `can_fail_spec`) — daemon exits 2
- **Don't assume CONTRIBUTING.md is accurate** — it describes the Python era; use `cargo` commands
- **Don't dispatch specs without the daemon running** — they queue but never execute; use `boi doctor` to check

## CLI Quick Reference

```
boi dispatch <spec.yaml> [--after SPEC_ID] [--priority N] [--mode e|c|d|g]
             [--max-iter N] [--timeout N] [--no-critic] [--project X]
             [--dry-run] [--workspace PATH]
boi status [spec-id] [--all] [--watch] [--json] [--verbose/-v]
boi log <spec-id> [--full] [--debug] [--follow/-f]
boi cancel <spec-id>
boi outputs <spec-id>
boi daemon [start|stop [--destroy-running] [--yes] | restart [--destroy-running] [--yes] | foreground]
boi config [key] [value]
boi workers
boi stop
boi telemetry <spec-id>
boi spec <queue-id> [show|add|skip|block|tail]
boi phases [<name>] [--spec SPEC_ID] [--full]
boi providers [list]
boi doctor
boi version
boi bench [--phase P] [--spec FILE|--battery DIR] [--pipeline name:path] [--runs N] [--json]
boi dashboard
boi completions <bash|zsh|fish|elvish|powershell>
boi prune-orphans [--dry-run|--apply] [--yes] [--force] [--max-idle-secs N]
                  [--exclude-pattern PAT] [--json]
boi research <brief.md> [--threads N] [--project NAME]
```

**Not implemented** (referenced in SKILL.md or Python archive but absent from `Commands` enum): `boi resume`, `boi dep`, `boi project`, `boi critic`.

## Spec Format Reference

```yaml
title: "Feature name"
mode: execute              # execute | challenge | discover | generate
workspace: /path/to/repo   # optional — override workspace
tasks:
  - id: T1A2B
    title: "Task name"
    status: PENDING         # must be PENDING at intake (enforced by validate_intake)
    depends: ["T0X1Y"]     # optional — comma-separated also accepted
    spec: |
      What to implement. Be concrete.
    verify: "command that returns 0 on success"
```

Pipeline modes (`phases/pipelines.toml`):

| Mode | Pre-phases | Task phases | Post-phases |
|------|-----------|-------------|-------------|
| `execute` (default) | spec-critique, spec-improve | execute, task-verify | critic |
| `challenge` | plan-critique | execute, task-verify | critic |
| `discover` | — | execute, task-verify | critic, evaluate |
| `generate` | plan-critique | decompose, execute, code-review, task-verify | critic, evaluate |
| `v2` | spec-critique, spec-improve | execute, review, commit | doc-update, critic, merge, cleanup |

## PR / Commit Conventions

Commit prefixes (from git log): `feat:`, `fix:`, `test:`, `release:`, `salvage:`, `merge:`, `wip:`

Branch naming: `mrap/<feature-name>` or `fix/<description>` or `boi/<spec-id>` (auto-created for worktrees).

See `CONTRIBUTING.md` for PR guidelines (note: the dev setup section is stale — use Rust/cargo, not Python).

## State Files at `~/.boi/`

| Path | Purpose |
|------|---------|
| `boi-rust.db` | SQLite database (9 tables: specs, tasks, iterations, events, workers, processes, phase_runs, bench_results, runners) |
| `config.yaml` | Worker config, pool type, paths, Fly settings |
| `.env` | Secrets — auto-loaded at startup via dotenvy (process env wins on conflict) |
| `hooks.yaml` | Hook overrides (falls back to compiled-in `hooks/default.yaml`) |
| `worktrees/<id>/` | Ephemeral git worktrees per spec |
| `logs/` | Per-spec log files |
| `telemetry/boi.jsonl` | Structured lifecycle event log |
| `daemon.pid` | Daemon process ID |
| `daemon.heartbeat` | Last heartbeat timestamp |
| `env.sh` | Canonical env setup sourced by shells + daemon plist |
| `hooks-wal/` | Audit-tier hooks WAL (distributed mode) |

## References

- [README.md](README.md) — Architecture, design decisions, user guide
- [CHANGELOG.md](CHANGELOG.md) — Version history and release notes
- [SKILL.md](SKILL.md) — How BOI skills work and invocation
- [CONTRIBUTING.md](CONTRIBUTING.md) — PR guidelines (dev setup section is stale)
- [docs/architecture.md](docs/architecture.md) — High-level architecture
- [docs/boi-rust-architecture.md](docs/boi-rust-architecture.md) — Rust implementation details
- [docs/boi-hooks-spec.md](docs/boi-hooks-spec.md) — Hook system specification
- [docs/pipelines.md](docs/pipelines.md) — Pipeline and phase system
- [docs/phase-configurability-2026-05-12.md](docs/phase-configurability-2026-05-12.md) — Phase TOML breaking changes
- [docs/spec-format.md](docs/spec-format.md) — Spec YAML schema
- [hex-foundation AGENTS.md](~/github.com/mrap/hex-foundation/AGENTS.md) — Parent system conventions
