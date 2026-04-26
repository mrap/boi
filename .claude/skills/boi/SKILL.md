# BOI — Beginning of Infinity

Self-evolving autonomous agent fleet. Specs carry state. Workers iterate with fresh context. Agents rewrite their own task lists.

## Quick Setup

```bash
# 1. Clone the repo
git clone https://github.com/mrap/boi.git ~/boi

# 2. Install (run from tmux, NOT from Claude Code)
cd ~/boi && bash install.sh --workers 3

# 3. Start using it from Claude Code
/boi dispatch
```

That's it. BOI creates `~/.boi/` with worker worktrees, config, and a daemon that manages everything.

## How It Works

1. You describe a task
2. Claude decomposes it into a **spec.yaml** (a `tasks:` array with `status: PENDING`)
3. You confirm ("fire it", "dispatch", "go")
4. `boi dispatch --spec spec.yaml` adds it to the queue
5. BOI daemon assigns specs to workers (isolated git worktrees)
6. Each worker gets a fresh session via the configured runtime CLI (default: `claude -p`; Codex: `codex exec`), reads the spec, executes the next PENDING task, marks it DONE, exits
7. Daemon detects remaining PENDING tasks and requeues for the next iteration
8. Workers can ADD new PENDING tasks to the `tasks:` array (self-evolution)
9. Spec completes when all tasks are DONE or SKIPPED

## Commands

### `/boi` or `/boi dispatch` — Plan and dispatch a spec

Conversational flow: describe a task, Claude decomposes it into a spec, you confirm, it dispatches.

```bash
boi dispatch spec.yaml                    # shorthand (positional arg)
boi dispatch --spec <path/to/spec.yaml>   # explicit flag form
```

| Option | Description | Default |
|--------|-------------|---------|
| `--spec FILE` | Path to spec.yaml file (required) | - |
| `--priority N` | Queue priority (lower = higher priority) | 100 |
| `--max-iter N` | Max iterations before marking failed | 30 |
| `--worktree PATH` | Pin to a specific worktree | auto |
| `--no-critic` | Skip critic validation on completion | - |
| `--mode MODE` | Execution mode: `execute`, `challenge`, `discover`, `generate` | execute |
| `--after QUEUE_ID` | Wait for another spec to complete first (comma-separated for multiple) | - |
| `--timeout SECS` | Per-iteration timeout in seconds | 1800 |
| `--dry-run` | Validate and show what would be dispatched without enqueueing | - |

#### Spec Dependencies (`--after`)

Chain specs so one starts only after another completes:

```bash
boi dispatch --spec frontend.yaml                          # q-001
boi dispatch --spec backend.yaml                           # q-002
boi dispatch --spec integration-tests.yaml --after q-001,q-002  # q-003 waits for both
```

Multiple dependencies (AND logic): all must complete before the dependent spec starts. If a dependency fails or is cancelled, dependent specs are automatically failed with a clear message.

### `/boi status` — Show queue progress

```bash
boi status           # snapshot
boi status --watch   # live auto-refresh (every 2s)
boi status --json    # machine-readable
```

### `/boi log <queue-id>` — View worker output

```bash
boi log                # tail most recent spec
boi log q-001          # tail last 50 lines of specific spec
boi log q-001 --full   # full output
boi log q-001 --failures  # show only failed iterations
```

### `/boi spec <queue-id>` — View and modify tasks

```bash
boi spec q-001                                    # show tasks with status
boi spec q-001 add "Fix bug" --spec "..." --verify "..."  # add a task
boi spec q-001 skip t-4 --reason "Not needed"     # skip a task
boi spec q-001 next t-6                           # reorder: run t-6 next
boi spec q-001 block t-5 --on t-3                 # t-5 waits for t-3
```

### `/boi cancel [queue-id]` — Cancel a spec

Without a queue-id, cancels the most recent spec.

### `/boi stop` — Stop all workers and daemon

### `/boi workers` — Show worker status

### `/boi telemetry <queue-id>` — Iteration breakdown

### `/boi dashboard` — Live compact dashboard

### `/boi doctor` — Check prerequisites and environment health

### `/boi purge` — Remove completed/failed/canceled specs from queue

### `/boi upgrade` — Update BOI to the latest version

### `/boi do "..."` — Natural language interface

```bash
boi do "show me what's running"
boi do "cancel the ios spec"
boi do --dry-run "stop everything"
```

### `/boi project` — Organize specs into projects

```bash
boi project create my-app --description "App rewrite"
boi dispatch --spec feature.yaml --project my-app
boi project status my-app
```

Projects provide shared context (`context.md` + `research.md`) injected into every worker prompt.

### `/boi critic` — Quality gate

The critic reviews completed specs before marking them done. It checks for spec integrity, weak verification, code quality, and incomplete work. If issues found, it adds `[CRITIC]` tasks and requeues.

```bash
boi critic status    # show config
boi critic checks    # list active checks
boi critic disable   # turn off globally
```

Custom checks: drop `.md` files into `~/.boi/critic/custom/`.

### `/boi review <queue-id>` — Review experiments

For specs in `challenge`, `discover`, or `generate` mode, workers can propose experiments. Review them:
- `[a]` Adopt, `[r]` Reject, `[d]` Defer, `[v]` View

## Spec Format

A BOI spec is a YAML file. Each task has an `id`, `title`, `status`, `spec`, and `verify` field.

```yaml
title: "My Project Spec"
mode: execute             # optional — execute (default), challenge, discover, generate

tasks:
  - id: t-1
    title: "First task title"
    status: PENDING
    spec: |
      What the worker should do. Be specific.
    verify: |
      # Commands that prove the work is done. Non-zero exit = task incomplete.
      test -f output.txt

  - id: t-2
    title: "Second task title"
    status: PENDING
    depends: [t-1]
    spec: |
      Description...
    verify: |
      grep "expected" output.txt
```

**Rules:**
- `id` must be `t-N` (sequential numbers)
- `status` must be `PENDING`, `DONE`, `SKIPPED`, or `FAILED`
- `spec` and `verify` are required
- `depends: [t-X]` prevents a task from running until t-X is DONE
- One task per iteration. Keep tasks completable in 10-30 min.
- Workers add new PENDING tasks to the `tasks:` array (self-evolution)

## Execution Modes

| Mode | Workers can... | Use when... |
|------|---------------|-------------|
| `execute` | Execute tasks only | Tasks are well-defined |
| `challenge` | Execute + flag concerns | Want observations alongside execution |
| `discover` | Execute + add new tasks | Most real-world work (recommended) |
| `generate` | Full creative authority | Goal clear but path unclear |

## Constraints

- `boi install` runs outside Claude Code (in tmux or terminal)
- Workers are headless, non-interactive CLI agent sessions. Default runtime: `claude -p`. Codex runtime: `codex exec`. Configured globally in `~/.boi/config.json` or per-spec via `runtime: codex` field.
- Daemon polls every 5 seconds
- Default 3 workers, max 5
- Python stdlib only, no pip dependencies
- State lives in `~/.boi/`
