# Getting Started

This guide walks you through installing BOI, writing your first spec, dispatching it, and watching it work.

## Prerequisites

- **Python 3.10+** (check with `python3 --version`)
- **Git** (check with `git --version`)
- **tmux** (check with `tmux -V`)
- **Claude Code CLI** (check with `claude --version`)

## Install

### Quick install (curl)

```bash
curl -fsSL https://raw.githubusercontent.com/boi-dev/boi/main/install-public.sh | bash
```

### Manual install (clone)

```bash
git clone https://github.com/boi-dev/boi.git ~/boi
bash ~/boi/install.sh --workers 3
```

Both methods create:
- `~/.boi/` state directory (queue, logs, events, config)
- 3 git worktrees at `~/.boi/worktrees/boi-worker-{1..3}/`
- A `boi` alias in your shell

### Verify the install

```bash
boi --version
boi doctor
```

`boi doctor` checks all prerequisites and reports any issues.

## Write Your First Spec

A spec is a Markdown file with ordered tasks. Each task has a title, status, spec section, and verify section.

Create a file called `hello-boi.md`:

```markdown
# Hello BOI

## Tasks

### t-1: Create the greeting script
PENDING

**Spec:** Create a file called `hello.py` that prints "Hello from BOI!" when run.

**Verify:** `python3 hello.py` outputs "Hello from BOI!".

### t-2: Add a unit test
PENDING

**Spec:** Create `test_hello.py` that imports the greeting function from `hello.py`
and asserts the output is correct. Use `unittest` from stdlib.

**Verify:** `python3 -m unittest test_hello -v` passes.
```

Key rules:
- Task headings: `### t-N: Title` (three hashes, sequential IDs)
- Status on its own line: `PENDING`, `DONE`, `SKIPPED`, or `FAILED`
- `**Spec:**` tells the worker what to do
- `**Verify:**` tells the worker how to prove it worked

See [spec-format.md](spec-format.md) for the full spec format guide.

## Dispatch

Submit your spec to the queue:

```bash
boi dispatch --spec hello-boi.md
```

BOI validates the spec, assigns it a queue ID (e.g., `q-001`), and starts the daemon. The daemon assigns the spec to a free worker, which launches a fresh Claude session to execute the first PENDING task.

### Dispatch options

```bash
# Set priority (lower = higher priority, default: 100)
boi dispatch --spec hello-boi.md --priority 50

# Limit iterations (default: 30)
boi dispatch --spec hello-boi.md --max-iter 10

# Use discover mode (workers can add new tasks)
boi dispatch --spec hello-boi.md --mode discover

# Pin to a specific worktree
boi dispatch --spec hello-boi.md --worktree ~/.boi/worktrees/boi-worker-1

# Skip the critic quality check
boi dispatch --spec hello-boi.md --no-critic
```

## Watch Progress

### Status snapshot

```bash
boi status
```

Output:
```
BOI

QUEUE                         MODE       WORKER  ITER   TASKS       QUALITY    PROGRESS   STATUS
q-001  hello-boi              execute    w-1     1/30   0/2 done    ---        0%         running

Workers: 1/3 busy  |  Queue: 1 running
```

### Live auto-refresh

```bash
boi status --watch
```

Updates every 2 seconds. Press `Ctrl+C` to stop.

### Compact dashboard

```bash
boi dashboard
```

A tmux-friendly compact view, good for small panes.

## Check Results

### View worker output

```bash
boi log q-001              # tail latest iteration
boi log q-001 --full       # full output
```

### Iteration breakdown

```bash
boi telemetry q-001
```

Shows tasks completed, time spent, and quality scores per iteration.

### Read the spec

Open your `hello-boi.md` file. Tasks marked `DONE` include notes about what the worker accomplished. This is the source of truth.

## What Happens Next

After the worker completes t-1, the daemon detects remaining PENDING tasks and requeues the spec. A new worker picks up t-2 with a fresh Claude session. When all tasks are DONE, the critic (if enabled) reviews the work. If it passes, the spec is marked completed.

If a worker discovers additional work is needed (in Discover or Generate mode), it adds new PENDING tasks to the spec. The daemon keeps iterating until everything is done.

## Managing Specs

```bash
boi queue                  # Show all specs with status
boi cancel q-001           # Cancel a spec
boi stop                   # Stop daemon and all workers
boi workers                # Show worker availability
boi purge                  # Remove completed/failed specs from queue
```

## Next Steps

- [Spec Format](spec-format.md) for writing effective specs
- [Modes](modes.md) for understanding Execute, Challenge, Discover, and Generate
- [Critic](critic.md) for quality gates and custom checks
- [Projects](projects.md) for grouping related specs with shared context
- [Architecture](architecture.md) for how BOI works under the hood
