# BOI (Beginning of Infinity)

A self-evolving autonomous agent fleet. Single Rust binary dispatches Claude Code workers to execute specs iteratively until all tasks are complete.

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
| Runtime | `src/runtime/` | Provider trait, ProviderRegistry, built-in providers (claude, openrouter, codex, deterministic) |

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
boi log <spec-id> [--full]
boi cancel <spec-id>
boi outputs <spec-id>
boi daemon [--foreground]
boi config [key] [value]
boi workers
boi stop
boi telemetry <spec-id>
boi spec <spec-id> [add|skip|block]
boi doctor
boi version
boi providers list
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

### Provider Architecture

Every LLM phase is dispatched through the `Provider` trait in `src/runtime/`. The runner does a single `registry.get(provider_name)` lookup — no if/else chains on runtime strings.

Built-in providers (registered at startup):
- `claude` — ClaudeCLIProvider, always active, wraps `claude -p`
- `openrouter` — requires `OPENROUTER_API_KEY`; auto-disabled at startup if absent
- `codex` — requires `OPENAI_API_KEY`; auto-disabled at startup if absent
- `deterministic` — builtin handler executor for commit/merge/cleanup phases; no LLM

Validation fires at three points: registration time, phase TOML load time (loud WARN to stderr if a phase's `runtime` is disabled), and pre-invocation. This makes misconfigured providers surface loudly at startup rather than silently falling through.

`boi providers list` shows which providers are active vs. disabled and why. See [docs/providers.md](docs/providers.md) for the full architecture, how to add a new provider, and the ProviderError taxonomy.

### Python archive

The original Python implementation (daemon.py, worker.py, lib/, 80+ test files) is archived at `_archive/python/`. The Rust binary is the primary implementation.
