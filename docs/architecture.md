# Architecture

BOI is built on a simple principle: fresh context per iteration. Every worker session starts clean, reads the spec file from disk, executes one task, and exits. The spec file is the single source of truth. No state lives in memory between iterations.

## System Overview

```
You → boi dispatch --spec spec.md → Spec Queue (priority-sorted)
                                          |
                                     +---------+
                                     | Daemon  |
                                     | (polls  |
                                     |  every  |
                                     |  5s)    |
                                     +----+----+
                              +----------++-----------+
                              |           |           |
                          Worker 1    Worker 2    Worker 3
                          (fresh      (fresh      (fresh
                           claude)     claude)     claude)
                              |           |           |
                          Spec done?  Spec done?  Spec done?
                          Yes: done   No: requeue Yes: done
```

## Components

### CLI (`boi.sh`)

The entry point. Routes subcommands to their implementations. Handles argument parsing, validation, and output formatting. Calls into Python libraries in `lib/` for business logic.

### Daemon (`daemon.sh`)

The orchestration loop. Runs in the background (or foreground with `--foreground`).

Every 5 seconds, the daemon:
1. Scans `~/.boi/queue/` for specs with status `queued` or `requeued`
2. Sorts by priority (lower number = higher priority)
3. Filters out specs blocked by DAG dependencies
4. Assigns the highest-priority spec to a free worker
5. Monitors running workers by PID
6. Detects completion, requeues unfinished specs, handles crashes

The daemon writes events to `~/.boi/events/` for every state transition.

### Worker (`worker.sh`)

Executes one iteration of one spec. Launched by the daemon.

For each iteration:
1. Reads the spec file
2. Counts PENDING tasks (exits immediately if none)
3. Generates a prompt from the spec + worker prompt template + mode-specific instructions
4. Launches `claude -p` in a tmux session (`tmux -L boi`)
5. Claude reads the prompt, executes the next PENDING task, marks it DONE in the spec file
6. After Claude exits, writes iteration metadata (tasks done, duration, quality)
7. Writes exit code file for daemon monitoring

Workers are isolated. Each runs in its own git worktree with its own tmux session. Multiple workers can process different specs simultaneously.

### Queue (`lib/queue.py`)

The spec queue. Each spec in the queue is a JSON file at `~/.boi/queue/{queue-id}.json` containing:
- `spec_path`: Path to the spec.md file
- `status`: `queued`, `running`, `requeued`, `completed`, `failed`, `canceled`, `needs_review`
- `priority`: Lower = higher priority
- `iteration`: Current iteration count
- `max_iterations`: Hard stop
- `mode`: Execution mode (execute, challenge, discover, generate)
- `worker_id`: Currently assigned worker (if running)
- `project`: Associated project name (optional)

The queue supports:
- Priority ordering
- DAG-based blocking (one spec can block another)
- Automatic requeuing when PENDING tasks remain
- Consecutive failure tracking with cooldown

### Spec Parser (`lib/spec_parser.py`)

Parses spec.md files to extract task counts, statuses, and metadata. Used by the daemon to decide whether to requeue, and by the CLI for status display.

### Spec Validator (`lib/spec_validator.py`)

Validates spec format before dispatch. Checks for:
- Valid task headings (`### t-N: Title`)
- Status lines after each heading
- Required `**Spec:**` and `**Verify:**` sections
- Sequential task numbering
- Generate mode format (Goal, Constraints, Success Criteria)

### Critic (`lib/critic.py`, `lib/critic_config.py`)

Quality gate that reviews completed specs. See [critic.md](critic.md) for details.

### Telemetry (`lib/telemetry.py`)

Tracks per-iteration metrics: tasks completed, tasks added, tasks skipped, duration, quality scores, and Deutschian progress metrics (evolution ratio, productive failure rate, first-pass completion rate).

### Quality (`lib/quality.py`)

Computes quality scores across 18 signals in 4 categories (Code Quality, Test Quality, Documentation, Architecture). See the README for the full scoring breakdown.

## Directory Structure

### Source code (`~/boi/`)

```
~/boi/
  boi.sh                        # CLI entry point
  daemon.sh                     # Queue-aware dispatch daemon
  worker.sh                     # Iterative worker (one claude -p per iteration)
  dashboard.sh                  # Live-updating compact display
  install.sh                    # Setup (git worktrees, config)
  install-public.sh             # Public install script (curl | bash)
  lib/
    queue.py                    # Spec queue operations
    spec_parser.py              # Parse spec.md for task statuses
    spec_validator.py           # Validate spec format
    spec_editor.py              # Add, skip, reorder, block tasks
    project.py                  # Project CRUD
    do.py                       # Natural language → CLI translation
    status.py                   # Status + dashboard formatting
    telemetry.py                # Per-iteration metrics
    quality.py                  # 18-signal quality scoring
    evaluate.py                 # Generate mode evaluation phase
    review.py                   # Experiment review
    event_log.py                # Event logging
    hooks.py                    # Lifecycle hooks
    critic_config.py            # Critic configuration
    critic.py                   # Critic execution
    daemon_ops.py               # Daemon helper operations
  templates/
    worker-prompt.md            # Worker prompt template
    do-prompt.md                # boi do system prompt
    critic-prompt.md            # Critic prompt template
    critic-worker-prompt.md     # Critic worker wrapper
    generate-decompose-prompt.md # Generate mode decomposition
    evaluate-prompt.md          # Generate mode evaluation
    modes/                      # Mode-specific prompt fragments
    checks/                     # Default check definitions
  tests/                        # Unit tests (mock data only)
```

### Runtime state (`~/.boi/`)

```
~/.boi/
  config.json                   # Worker/worktree mappings
  daemon.pid                    # Daemon process ID
  queue/                        # Spec queue
    q-001.json                  # Queue entry metadata
    q-001.spec.md               # Copy of spec file
    q-001.telemetry.json        # Per-iteration metrics
    q-001.iteration-1.json      # Iteration 1 metadata
    q-001.pid                   # Worker PID (while running)
    q-001.prompt.md             # Generated worker prompt
    q-001.run.sh                # Worker run script
    q-001.exit                  # Exit code after iteration
  logs/
    daemon.log                  # Daemon log
    q-001-iter-1.log            # Worker output per iteration
  events/                       # Lifecycle events (JSON)
  hooks/                        # Optional: on-complete.sh, on-fail.sh
  worktrees/                    # Git worktrees (one per worker)
    boi-worker-1/
    boi-worker-2/
    boi-worker-3/
  projects/                     # Project directories
    my-project/
      project.json
      context.md
      research.md
  critic/                       # Critic configuration
    config.json
    custom/                     # Custom check definitions
```

## Key Design Decisions

### Why fresh context per iteration?

AI coding agents degrade over long sessions. Context fills up, instructions get lost, and the agent starts repeating itself or hallucinating. BOI prevents this by giving each iteration a brand-new Claude session. The spec file carries state, not memory.

### Why one task per iteration?

Constraining workers to one task keeps the scope manageable. The worker can focus its full attention on one concrete piece of work. If it fails, only one task is affected. The daemon requeues and a fresh session tries again.

### Why tmux?

Workers need to run Claude headlessly (no interactive terminal). tmux provides isolated sessions that the daemon can monitor. Each worker gets its own tmux session under the `boi` server (`tmux -L boi`), keeping BOI sessions separate from the user's.

### Why git worktrees?

Each worker needs an isolated copy of the codebase. Git worktrees provide this without the cost of full clones. All worktrees share the same git objects, so they're fast to create and disk-efficient.

### Why Markdown specs?

Markdown is human-readable, version-controllable, and trivial to parse. Workers can edit specs with standard file I/O. Users can read and modify specs in any text editor. No databases, no APIs, no serialization formats.

### Why Python stdlib only?

Zero external dependencies means BOI works on any machine with Python 3.10+. No `pip install`, no virtual environments, no version conflicts. This is critical for a tool that runs on diverse machines.
