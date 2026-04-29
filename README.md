# BOI — Beginning of Infinity

Rust-based autonomous task execution system. Write a spec with ordered tasks; BOI runs each task in a fresh Claude worker, verifies completion, and continues until done. Named after David Deutsch's *The Beginning of Infinity*: knowledge grows through conjecture and criticism.

## Quick Start

```bash
# Dispatch a spec
boi dispatch my-feature.yaml

# Monitor progress
boi status
boi log <spec-id>        # e.g. boi log sa7f3
```

## Spec Format

```yaml
title: My Feature
mode: execute

tasks:
  - id: t-1
    title: Implement the thing
    status: PENDING
    spec: |
      Add X to lib/foo.py following the existing pattern.
    verify: "python3 -m pytest tests/test_foo.py -x -q"

  - id: t-2
    title: Follow-up step
    status: PENDING
    depends: [t-1]
    spec: |
      Update docs after t-1.
    verify: "test -f docs/foo.md"
```

**Fields:** `id`, `title`, `status` (PENDING/DONE/SKIPPED/FAILED), `spec`, `verify`, `depends`

## IDs

IDs are short 5-character hashes:
- Specs: `s` + 4 hex digits — e.g. `sa7f3`, `sb2e1`
- Tasks: `t` + 4 hex digits — e.g. `t4a9c`, `t2b4e`

## Architecture

```
boi dispatch → SQLite queue (~/.boi/boi-rust.db)
                    |
             Daemon (Rust binary)
             polls queue, assigns workers
                    |
        +-----------+-----------+
        |           |           |
     Worker 1    Worker 2    Worker 3
     git worktree git worktree git worktree
        |
     Reads spec → executes next PENDING task → verify → exits
        |
     Daemon routes to next phase (doc-update → task-verify → critic)
```

- **Daemon**: Rust binary, SQLite-backed, polls every few seconds
- **Workers**: Each runs in an isolated git worktree under `~/.boi/worktrees/`
- **State**: `~/.boi/boi-rust.db` (SQLite WAL mode)
- **Logs**: `~/.boi/logs/<spec-id>/`

## Phase System

Phases are named worker roles. Each spec passes through spec-level phases once, then task phases run per task.

**Default mode (`execute`):**
```
Spec phases:  spec-review → critic
Task phases:  execute → doc-update → task-verify
```

**Other modes:**

| Mode | Spec phases | Task phases |
|------|------------|-------------|
| `execute` (default) | spec-review, critic | execute, doc-update, task-verify |
| `challenge` | spec-review, plan-critique, critic | execute, doc-update, task-verify |
| `discover` | spec-review, critic, evaluate | execute, doc-update, task-verify |
| `generate` | spec-review, plan-critique, critic, evaluate | decompose, execute, doc-update, code-review, task-verify |

Phase files live at `~/.boi/phases/*.phase.toml`. The daemon hot-reloads them without restart.

## Config

**`~/.boi/config.yaml`** — global defaults:
```yaml
max_workers: 5
claude_bin: /path/to/claude
```

**`~/.boi/pipelines.toml`** — pipeline customization:
```toml
[mode.default]
spec_phases = ["spec-review", "critic"]
task_phases = ["execute", "doc-update", "task-verify"]
```

Per-spec overrides: add `mode: challenge` (or `c/d/g`) to the spec YAML.

## CLI Reference

```
boi dispatch <file.yaml> [options]   Submit a spec to the queue
boi status [<id>] [--watch] [--json] Show queue and spec status
boi log <id> [--full] [--debug]      Tail worker output
boi cancel <id>                      Cancel a queued or running spec
boi stop                             Stop daemon and all workers
boi daemon [start|stop|restart]      Manage the daemon process
boi spec <id> [show|add|skip|block]  Manage tasks within a spec
boi workers                          Show worktree health
boi telemetry <id>                   Per-iteration cost and timing
boi outputs <id>                     Files produced by a completed spec
boi phases [<name>]                  List or inspect phase definitions
boi config [<key> [<value>]]         Show or set config values
boi doctor                           Health check (runtime, paths, db)
boi version                          Print version
```

**`dispatch` options:**

| Flag | Default | Description |
|------|---------|-------------|
| `--mode` / `-m` | execute | Mode: execute/challenge/discover/generate (or e/c/d/g) |
| `--priority N` | 100 | Lower = higher priority |
| `--max-iter N` | 30 | Max iterations before failing |
| `--timeout N` | 30 | Task timeout in minutes |
| `--after <id>` | — | Wait for another spec to complete first |
| `--project NAME` | — | Associate with a named project |
| `--no-critic` | — | Skip the critic phase |
| `--dry-run` | — | Validate spec without enqueuing |
