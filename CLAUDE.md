# BOI (Beginning of Infinity)

A self-evolving autonomous agent fleet. Dispatches Claude Code workers to execute specs iteratively until all tasks are complete.

## Architecture

```
boi.sh (CLI)
  └─ daemon.py (poll loop)
       ├─ lib/db.py (SQLite state store, WAL mode)
       ├─ lib/daemon_ops.py (completion handling, spec picking)
       └─ worker.py (single iteration executor)
            ├─ lib/spec_parser.py (parse task statuses)
            ├─ lib/critic.py (quality validation)
            ├─ lib/evaluate.py (convergence detection)
            └─ lib/workspace_guard.py (worktree boundary checker)
```

### Flow

1. User dispatches a spec via `boi dispatch --spec spec.md`
2. Spec is validated (`lib/spec_validator.py`), copied to `~/.boi/queue/`, and enqueued in SQLite
3. Daemon polls every 5s: checks worker completions, dispatches queued specs to free workers
4. Worker reads spec, generates prompt from template + mode rules, launches `claude -p` in a tmux session
5. Claude executes one PENDING task, marks it DONE in the spec file, exits
6. Daemon detects completion, runs critic (if enabled), re-dispatches for next task
7. Repeats until all tasks are DONE or max iterations reached

### Key Components

| Component | File | Purpose |
|-----------|------|---------|
| CLI | `boi.sh` | Routes subcommands (dispatch, queue, status, log, cancel, stop, etc.) |
| Daemon | `daemon.py` | Poll loop: dispatch specs, monitor workers, self-heal, reconcile |
| Worker | `worker.py` | Execute one iteration: read spec, generate prompt, launch tmux, post-process |
| Database | `lib/db.py` | SQLite state store. Schema in `lib/schema.sql`. WAL for concurrent reads |
| Daemon Ops | `lib/daemon_ops.py` | Post-iteration logic, next-spec selection, critic orchestration |
| Spec Parser | `lib/spec_parser.py` | Parse `### t-N:` headings, status lines, task fields |
| Spec Validator | `lib/spec_validator.py` | Pre-dispatch validation (structure, required fields) |
| Critic | `lib/critic.py` | Quality gating: score >= 0.85 fast-approves, < 0.50 auto-rejects |
| Evaluate | `lib/evaluate.py` | Convergence detection for Generate-mode specs |
| Queue (legacy) | `lib/queue.py` | File-based JSON queue (being replaced by db.py) |
| Workspace Guard | `lib/workspace_guard.py` | Detects when workers write outside their worktree |

### State

All mutable state lives in `~/.boi/`:
- `boi.db` — SQLite database (specs, workers, iterations, events, messages)
- `config.json` — Worker definitions (worktree paths)
- `queue/` — Spec file copies, prompts, run scripts, iteration metadata
- `logs/` — Per-iteration log files (`{spec_id}-iter-{N}.log`)
- `projects/` — Per-project context and research notes

### Database Tables

- `specs` — One row per dispatched spec (status, iteration count, phase, priority)
- `workers` — One row per worker slot (worktree path, current assignment)
- `iterations` — Metadata per (spec, iteration, phase) execution
- `events` — Append-only event log
- `spec_dependencies` — DAG for `--after` ordering
- `messages` — Inter-process messaging audit trail
- `processes` — PID tracking for crash recovery

## Spec Format

A spec is a Markdown file with ordered tasks:

```markdown
# Feature Name

## Tasks

### t-1: First task
PENDING

**Spec:** What to do. Be explicit about files, functions, patterns.

**Verify:** `command to prove it worked`

### t-2: Second task
PENDING

**Blocked by:** t-1

**Spec:** What to do next.

**Verify:** How to verify.
```

### Task Statuses

| Status | Meaning |
|--------|---------|
| `PENDING` | Not started. Workers pick this up. |
| `DONE` | Completed and verified. |
| `SKIPPED` | Intentionally bypassed. |
| `FAILED` | Attempted but could not complete. |
| `EXPERIMENT_PROPOSED` | Alternative approach proposed (awaiting review). |
| `SUPERSEDED by t-N` | Replaced by a better task (Generate mode). |

### Task Fields

- `**Spec:**` — What to do (required)
- `**Verify:**` — How to prove it worked (required)
- `**Blocked by:** t-N` — Dependency (optional)
- `**Self-evolution:**` — What to do if unexpected work appears (optional)
- `**Model:** opus|sonnet|haiku` — Per-task model override (optional)

### Generate Mode

Goal-only format. A decomposition worker breaks it into tasks before execution:

```markdown
# [Generate] Feature Name

## Goal
What to build.

## Constraints
- Python 3.10+, stdlib only

## Success Criteria
- [ ] Criterion 1
- [ ] Criterion 2
```

## Modes

| Mode | Behavior |
|------|----------|
| `execute` | Complete the task exactly as specified. No new tasks. |
| `challenge` | Execute but question assumptions. Propose alternatives. |
| `discover` | Execute and append new tasks if unexpected work found. |
| `generate` | Full creative authority: add, modify, supersede tasks. |

## Phases

Each iteration runs one phase:

| Phase | Purpose | Model |
|-------|---------|-------|
| `decompose` | Break Generate-mode goals into tasks | Opus (high effort) |
| `execute` | Complete one PENDING task | Sonnet (medium effort) |
| `critic` | Validate quality after all tasks done | Sonnet (medium effort) |
| `evaluate` | Check convergence for Generate specs | Sonnet (medium effort) |

## Critic System

After all tasks are DONE, the critic validates quality:
- **Score >= 0.85:** Fast-approve (skip detailed checks)
- **Score 0.50-0.84:** Standard review (run all checks)
- **Score < 0.50:** Auto-reject (add new PENDING tasks)

Checks live in `templates/checks/`: code-quality, completeness, conjecture-criticism, fleet-readiness, goal-alignment, spec-integrity, verify-commands.

Disable per-spec with `--no-critic`.

## CLI Reference

```bash
# Dispatch
boi dispatch --spec spec.md [--priority N] [--max-iter N] [--mode MODE]
boi dispatch --spec spec.md --after q-001,q-002  # DAG dependencies

# Monitor
boi queue                    # Show spec queue with status
boi status [--watch]         # Workers + assignments + progress
boi log <queue-id> [--full]  # Show logs for a spec
boi telemetry <queue-id>     # Per-iteration breakdown
boi dashboard                # Live-updating queue progress

# Manage
boi cancel <queue-id>        # Cancel a spec
boi stop                     # Stop daemon and all workers
boi workers                  # Show worktree/worker availability
boi spec <id> add "title"    # Add task to running spec
boi spec <id> skip t-3       # Skip a task
boi spec <id> block t-3 --on t-2  # Add dependency

# Setup
boi install [--workers N]    # Create worktrees, write config
boi doctor                   # Check prerequisites and health
boi upgrade                  # Update to latest version
```

## Running Tests

```bash
# All tests
python3 -m pytest tests/ -v

# Specific test file
python3 -m pytest tests/test_spec_parser.py -v

# Skip slow integration tests
python3 -m pytest tests/ -v -k "not integration"

# With coverage
python3 -m pytest tests/ --cov=lib --cov-report=term-missing
```

Tests use mock data only, no live API calls. The test suite includes:
- Unit tests for every lib/ module
- Characterization tests (`test_characterization.py`)
- Eval suites (`eval_boi.py`, `eval_critic.py`)
- Integration tests (`tests/integration/`)

## Coding Conventions

- **Python 3.10+**, stdlib only. No pip dependencies.
- **Shell scripts:** `set -uo pipefail` (no `-e`).
- **Type hints** on all function signatures.
- **Logging** via `logging.getLogger("boi.<module>")`.
- **Database access** through `lib/db.py`. All mutations acquire `self.lock`. Reads are lock-free (WAL mode).
- **File writes:** Atomic via `.tmp` + `mv`. Never leave partially written files.
- **Spec file is source of truth.** Workers read and write the spec on disk. No in-memory-only state.
- **Templates** in `templates/`. Worker prompts use `{{PLACEHOLDER}}` substitution.
- **Process isolation:** Workers spawn in new sessions (`start_new_session=True`) so the daemon can kill entire process groups.
- **Tmux socket:** All BOI tmux sessions use `-L boi` socket.

## Project Layout

```
boi/
├── boi.sh              # CLI entry point
├── daemon.py           # Daemon poll loop
├── worker.py           # Single-iteration executor
├── lib/                # Core library modules
│   ├── db.py           # SQLite state store
│   ├── daemon_ops.py   # Completion handling, spec picking
│   ├── spec_parser.py  # Task parsing
│   ├── spec_validator.py  # Pre-dispatch validation
│   ├── critic.py       # Quality gating
│   ├── critic_config.py   # Critic configuration
│   ├── evaluate.py     # Convergence detection
│   ├── workspace_guard.py # Worktree boundary checker
│   ├── dag.py          # Spec dependency DAG
│   ├── hooks.py        # Completion hooks
│   ├── event_log.py    # Event logging
│   ├── schema.sql      # Database schema
│   └── ...
├── templates/
│   ├── worker-prompt.md       # Main worker prompt template
│   ├── critic-worker-prompt.md
│   ├── evaluate-prompt.md
│   ├── generate-decompose-prompt.md
│   ├── modes/          # Mode-specific rules (execute, challenge, discover, generate)
│   └── checks/         # Critic check templates
├── tests/              # Test suite (pytest)
├── examples/           # Example specs (hello-world, cli-tool, refactor)
├── docs/               # Documentation
├── scripts/            # Helper scripts
└── dashboard.sh        # Live queue dashboard
```
