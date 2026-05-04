# BOI (Beginning of Infinity)

BOI is a Rust binary that dispatches Claude Code workers to execute spec tasks in parallel. Workers run in isolated git worktrees, each with its own Claude session. To contribute: `cargo build && cargo test`. To run a spec: `boi dispatch <spec.yaml>` (daemon must be running first: `boi daemon`). See [README.md](README.md) for the full architecture and [SKILL.md](SKILL.md) for how BOI skills work.

**Related repos:** [mrap-hex](file:///Users/mrap/mrap-hex) (Mike's operating layer; configures BOI hooks), [hex-foundation](~/github.com/mrap/hex-foundation) (shared conventions + standing orders used by all agents).

## Architecture

```
boi (Rust binary)
  ├── dispatch    — parse spec YAML, enqueue to SQLite
  ├── daemon      — poll loop, spawn worker threads, monitor
  ├── worker      — create git worktree, spawn claude -p, verify, retry
  ├── status      — rich colored output, --watch, --json
  ├── spec        — add/skip/block tasks on running specs
  ├── hooks       — lifecycle events fired as subprocesses
  ├── telemetry   — append-only JSONL logging
  └── doctor      — health checks (daemon, DB, worktrees, config)
```

### Source modules

| Module | File | Purpose |
|--------|------|---------|
| CLI | `src/main.rs` | Routes subcommands, command handlers |
| Spec parser | `src/spec.rs` | YAML parsing, validation, dependency DAG |
| Queue | `src/queue.rs` | SQLite state store (specs, tasks, iterations, events, workers, processes) |
| Worker | `src/worker.rs` | Execute tasks: git worktree → claude -p → verify → retry |
| Spawn | `src/spawn.rs` | Spawn claude subprocess, PID tracking, timing, timeout |
| Prompt | `src/prompt.rs` | Build task prompt from spec content and task fields |
| Hooks | `src/hooks.rs` | Lifecycle hooks (9 events, JSON on stdin, configurable) |
| Config | `src/config.rs` | Load ~/.boi/config.yaml, defaults for everything |
| Telemetry | `src/telemetry.rs` | Append-only JSONL at ~/.boi/telemetry/boi.jsonl |
| Worktree | `src/worktree.rs` | Git worktree create/cleanup/prune |
| Workspace | `src/workspace/` | `WorkspaceBackend` trait — pluggable isolation (create, exec, merge, cleanup) |
| Pool | `src/pool/` | `WorkerPool` trait + `LocalThreadPool` impl; pluggable worker pool backend |
| Remote | `src/remote/` | Remote worker backends; `FlyDispatcher` implements `WorkerPool` via Fly Machines API |

### Flow

1. User dispatches: `boi dispatch spec.yaml [--mode discover] [--after q-NNN]`
2. Spec parsed and validated (YAML, task IDs, dependency DAG)
3. Enqueued to SQLite with atomic ID generation
4. Daemon polls every 5s, dequeues with atomic `assigning` state (prevents double-dispatch)
5. Worker thread: creates git worktree → builds prompt → spawns `claude -p` → monitors with timeout
6. On task complete: runs verify command, updates DB, fires `on_task_complete` hook
7. On task fail: retries up to N times, fires `on_task_fail` hook
8. On spec complete: fires `on_complete` hook, cleans up worktree
9. On daemon restart: recovers stuck specs (running/assigning → queued)

### State

All mutable state at `~/.boi/`:
- `boi-rust.db` — SQLite database (6 tables: specs, tasks, iterations, events, workers, processes)
- `config.yaml` — Hook configuration, worker count, timeouts
- `.env` — Auto-loaded at startup; put `OPENROUTER_API_KEY` and other secrets here (process env wins on conflict)
- `worktrees/` — Isolated git worktrees per spec
- `logs/` — Per-spec log files
- `telemetry/boi.jsonl` — Structured event log
- `daemon.pid` — Daemon process ID
- `daemon.heartbeat` — Last heartbeat timestamp

### Hooks

BOI fires lifecycle hooks as subprocesses. Hooks are configured in `~/.boi/config.yaml`. BOI has zero knowledge of what hooks do — hex configures them to emit events.

Hook points: `on_dispatch`, `on_worker_start`, `on_task_start`, `on_task_complete`, `on_task_fail`, `on_complete`, `on_fail`, `on_cancel`, `on_stall`

### CLI

```
boi dispatch <spec.yaml> [--mode e|c|d|g] [--after q-N] [--priority N] [--max-iter N] [--timeout N] [--project X] [--dry-run]
boi status [spec-id] [--all] [--watch] [--json]
boi dashboard                 # live height-aware dashboard (RUNNING + QUEUED prioritized)
boi log <spec-id> [--full]
boi cancel <spec-id>
boi outputs <spec-id>
boi daemon [--foreground]
boi config [key] [value]
boi workers
boi stop
boi telemetry <spec-id>
boi spec <spec-id> [add|skip|block]
boi providers list            # list registered runtime providers and availability
boi completions <shell>       # generate shell completions (bash|zsh|fish|elvish|powershell)
boi doctor
boi version
```

### Spec format (YAML)

```yaml
title: "Feature name"
mode: execute          # execute | challenge | discover | generate
workspace: /path/to   # optional, override workspace
tasks:
  - id: t-1
    title: "Task name"
    status: PENDING    # PENDING | DONE | FAILED | SKIPPED | RUNNING
    depends: [t-N]     # optional dependency list
    spec: |
      What to do.
    verify: "command that returns 0 on success"
```

### Python archive

The original Python implementation (daemon.py, worker.py, lib/, 80+ test files) is archived at `_archive/python/`. The Rust binary is the primary implementation.

## Commands

```bash
cargo build --release       # build binary
cargo test                  # run all tests
cargo install --path .      # install boi to PATH
boi daemon                  # start daemon (required before dispatch)
boi dispatch <spec.yaml>    # queue a spec
boi status                  # monitor queue
boi doctor                  # verify all health checks pass
```

## Gotchas

- **SQLite single-writer:** Never run two daemons against the same `boi-rust.db`. Second daemon will fail silently or corrupt state.
- **Worktree isolation:** Never edit files inside `~/.boi/worktrees/` directly — changes are lost on cleanup. Worktrees are ephemeral copies.
- **Daemon must be running:** `boi dispatch` enqueues but nothing executes until `boi daemon` is running. If PID file exists but daemon is dead, run `boi doctor` to reset.
- **Verify commands must be idempotent:** The worker may re-run verify on retry. Commands that fail the second time (e.g., CREATE TABLE) will break retries.
- **Hook config location:** Hook configuration lives in `~/.boi/config.yaml`, not in the BOI source. Don't hardcode hook paths in specs or code.
- **`cargo build` vs `cargo install`:** `cargo build` produces `target/release/boi`; `cargo install --path .` installs to `~/.cargo/bin/boi`. Use install for normal operation; build for debugging.

## How to Add a Feature

**New CLI subcommand:**
1. Add a new `Commands` variant in `src/main.rs`
2. Implement in a new or existing module (e.g., `src/myfeature.rs`)
3. Add `mod myfeature;` in `src/main.rs`

**New queue state / schema change:**
1. Add a numbered migration in `src/queue.rs` — migrations are append-only, never modify existing ones
2. Test with `cargo test` — queue tests run against a temp SQLite file

**New hook event:**
1. Add the event name to the `HookEvent` enum in `src/hooks.rs`
2. Call `hooks::fire(HookEvent::MyEvent, payload)` from the relevant lifecycle point in `src/worker.rs`
3. Update `~/.boi/config.yaml` with the new hook key (for local testing)

**Tests:**
- Tests live inline in `src/*.rs` under `#[cfg(test)]`
- Run: `cargo test`
- Integration smoke test: write a minimal BOI spec, dispatch it, verify task state with `boi status`

**Dispatching a smoke spec:**
```bash
# Write a minimal spec to /tmp/smoke.yaml, then:
boi daemon &         # if not already running
boi dispatch /tmp/smoke.yaml
boi status --watch   # confirm tasks move to DONE
```

## References

- [README.md](README.md) — Full architecture, design decisions, user guide
- [SKILL.md](SKILL.md) — How BOI skills work and how to invoke them
- [CHANGELOG.md](CHANGELOG.md) — Version history and release notes
- [CONTRIBUTING.md](CONTRIBUTING.md) — Contribution guidelines
- [hex-foundation AGENTS.md](~/github.com/mrap/hex-foundation/AGENTS.md) — Parent system conventions and standing orders
