---
name: boi
description: "Dispatch self-evolving specs to parallel Claude Code workers. Use when the user says 'boi', '/boi', 'dispatch this', 'dispatch spec', 'fire it', 'boi status', 'boi queue', 'boi log', 'boi stop', 'boi cancel', 'boi telemetry', 'boi dashboard', 'boi resume', 'boi dep', 'boi cleanup', 'fleet dag', 'spec dependencies', 'run this overnight', 'self-evolving loop', or wants to break a task into a spec and dispatch it to autonomous workers. Also use when the user has a complex task and wants it executed iteratively by fresh Claude sessions."
---

# BOI — Beginning of Infinity

Self-evolving autonomous agent fleet. Workers iterate with fresh context per cycle. Specs carry state. The queue manages priority.

## How BOI Works

1. User describes a task
2. Claude decomposes it into a **spec.md** (a list of `### t-N:` tasks with `PENDING` status)
3. User confirms ("fire it", "dispatch", "go")
4. Claude runs `boi dispatch --spec <path>`
5. BOI daemon assigns specs to workers (isolated git worktrees)
6. Each worker gets a fresh `claude -p` session, reads the spec, executes the next PENDING task, marks it DONE, exits
7. Daemon detects remaining PENDING tasks and requeues the spec for the next iteration
8. Workers can ADD new PENDING tasks to the spec (self-evolution)
9. Spec completes when all tasks are DONE or SKIPPED

## Commands

### `/boi` or `/boi dispatch` — Plan and dispatch a spec

**Conversational planning flow:**

1. If the user provides a spec file path, validate it exists, then dispatch directly.
2. If the user describes a task without a spec file:
   a. Help decompose the task into discrete, ordered tasks
   b. Write a spec.md file (see Spec Format below)
   c. Show the spec to the user for confirmation
   d. On confirmation ("fire it", "dispatch", "go", "yes"), dispatch it

**Dispatch command:**
```bash
boi dispatch --spec <path/to/spec.md> [--priority N] [--max-iter N] [--mode MODE]
```

Options:
- `--spec FILE` — Path to spec.md file (required)
- `--priority N` — Queue priority, lower = higher priority (default: 100)
- `--max-iter N` — Maximum iterations before marking failed (default: 30)
- `--worktree PATH` — Pin to a specific worktree
- `--worktree-isolate` — Create a dedicated worktree and branch for this spec
- `--after q-A,q-B` — Wait for listed specs to complete before starting
- `--no-critic` — Skip critic validation when this spec completes
- `--mode MODE` / `-m MODE` — Execution mode: `execute` (default), `challenge`, `discover`, `generate` (aliases: `e`, `c`, `d`, `g`)
- `--project NAME` — Associate with a project (injects project context)

After dispatch, run `boi status` to show initial state.

### Other Commands

```bash
boi status [--watch] [--json]             Queue and worker status
boi log <queue-id> [--full]               Tail worker output
boi cancel <queue-id>                     Cancel a spec
boi stop                                  Stop daemon and all workers
boi install [--workers N]                 One-time setup (outside Claude Code)
boi resume <queue-id> | --all            Resume failed/canceled specs
boi cleanup                               Kill orphaned worker processes
boi workers [--json]                      Show worktree health
boi telemetry <queue-id> [--json]        Per-iteration metrics
boi critic status | run | enable | disable | checks
boi spec <queue-id> [add|skip|next|block|edit|deps]
boi dep add|remove|set|clear|show|viz|check
boi project create|list|status|context|delete
```

## Spec Format

```markdown
# Spec Title

**Pipeline:** execute → review        # optional — overrides default pipeline
**Gates:** strict, +lint-pass         # optional — overrides guardrails

## Tasks

### t-1: First task
PENDING

**Spec:** What the worker must do. Be concrete: file paths, function names, patterns.

**Verify:**
```bash
# Commands that prove the work is done. Non-zero exit = task incomplete.
test -f output.txt
python3 -m pytest tests/ -x -q
```

### t-2: Second task
PENDING
**Blocked by:** t-1

**Spec:** Depends on t-1's output.

**Verify:**
```bash
grep "expected" output.txt
```
```

**Rules:**
- Headings must be `### t-N: Title` with status on the next line
- `**Spec:**` and `**Verify:**` are required
- `**Blocked by:** t-X` prevents a task from running until t-X is DONE
- Workers add new `### t-N: ... PENDING` tasks to self-evolve the spec
- One task per worker iteration; daemon requeues until all tasks are DONE

**Task-sizing guidelines:**
- Each task: 10–30 min of Claude work
- 1–2 data sources per task; 1–2 file mutations per task
- Spec text < 2000 chars; > 50 chars

## Phases

A **phase** is a named worker role defined by a `.phase.toml` file. The daemon hot-reloads phase files from `~/.boi/phases/` without restart.

### Built-in Phases

| Phase | Description | Timeout |
|-------|-------------|---------|
| `execute` | Execute tasks from the spec | 600s |
| `review` | Code review: correctness, security, spec compliance | 300s |
| `critic` | Quality gate: adds `[CRITIC]` tasks on failure | 300s |
| `decompose` | Decompose a high-level spec into actionable tasks | 600s |
| `evaluate` | Evaluate spec completion and determine next steps | 300s |

### Phase File Schema (`~/.boi/phases/*.phase.toml`)

```toml
name = "my-phase"                        # optional; derived from filename if omitted
description = "What this phase does"
completion_handler = "builtin:execute"   # optional — use built-in routing logic

[worker]
prompt_template = "path/to/prompt.md"   # required
model = "claude-sonnet-4-6"
effort = "medium"                        # low | medium | high
timeout = 300                            # seconds

[completion]
approve_signal = "## Approved"
reject_signal = "[REJECTED]"
on_approve = "next"                      # next | complete | commit | phase:<name>
on_reject = "requeue:execute"            # fail | retry | requeue:<phase> | phase:<name>
on_crash = "retry"                       # retry | fail

[hooks]
pre = ["verify-commands-pass"]           # gates before phase starts
post = ["diff-is-non-empty"]             # gates after phase completes
```

### Custom Phase Example

```toml
# ~/.boi/phases/security-scan.phase.toml
name = "security-scan"
description = "Run SAST scan and block on high-severity findings"

[worker]
prompt_template = "~/.boi/phases/templates/security-scan-prompt.md"
model = "claude-sonnet-4-6"
effort = "high"
timeout = 300

[completion]
approve_signal = "## Security Approved"
reject_signal = "[SECURITY-FAIL]"
on_approve = "next"
on_reject = "requeue:execute"
on_crash = "retry"

[hooks]
post = ["no-secrets"]
```

Use it: `**Pipeline:** execute → security-scan → review`

## Pipelines

An ordered list of phases a spec passes through. Configured globally in `~/.boi/guardrails.toml`:

```toml
[pipeline]
default = ["execute", "critic"]
```

Override per spec with a `**Pipeline:**` header:

```markdown
**Pipeline:** execute → review → critic
**Pipeline:** decompose → execute → critic
**Pipeline:** execute → security-scan
```

Arrows (`→` or `->`), commas, and spaces are all valid separators.

## Guardrails

Quality gates that run at phase transitions. Configured in `~/.boi/guardrails.toml`.

```toml
[global]
strictness = "advisory"   # strict | advisory | permissive

[pipeline]
default = ["execute", "critic"]

[hooks]
post-execute = ["verify-commands-pass", "diff-is-non-empty"]
pre-commit   = ["no-secrets"]

[gates.lint-pass]
command = "python3 -m flake8 ."
timeout = 60
```

**Strictness levels:**

| Level | Behavior on gate failure |
|-------|--------------------------|
| `strict` | Blocks phase transition; appends a `[GATE-FAIL]` PENDING task |
| `advisory` | Logs a warning; execution continues |
| `permissive` | Silently skips; execution continues |

**Per-spec gate overrides** (`**Gates:**` header):

```markdown
**Gates:** strict, +lint-pass, -no-secrets
```

- `strict` / `advisory` / `permissive` — override strictness
- `+gate-name` — add a gate to all hook points
- `-gate-name` — remove a gate from all hook points

## Gates

Checks that run at hook points. Return passed or failed.

### Built-in Gates

| Gate | What it checks |
|------|---------------|
| `verify-commands-pass` | Parses the `**Verify:**` block and runs each command; fails on non-zero exit |
| `diff-is-non-empty` | Fails if `git diff HEAD` and `git diff --cached` show no changes |
| `tests-pass` | Runs the configured test command (default: `python3 -m pytest tests/ -x -q`) |
| `lint-pass` | Runs the configured lint command (default: `python3 -m flake8 .`) |
| `no-secrets` | Scans `git diff HEAD` for API keys, tokens, and secret patterns |

### Custom Gates

Drop a shell script at `~/.boi/gates/<name>.sh`. The gate name is the filename without `.sh`.

```bash
#!/bin/sh
set -uo pipefail
# SPEC_PATH and SPEC_ID are available as env vars
some-check || { echo "Reason" >&2; exit 1; }
exit 0
```

Enable in `guardrails.toml`:
```toml
[hooks]
post-execute = ["my-check"]
```

Exit 0 = passed. Any non-zero = failed.

## Execution Modes

| Mode | Workers can... | Use when... |
|------|---------------|-------------|
| `execute` | Execute tasks only (default) | Tasks are well-defined |
| `challenge` | Execute + flag concerns | You want observations alongside execution |
| `discover` | Execute + add new tasks | Most real-world work (recommended) |
| `generate` | Full creative authority over spec | Goal is clear but path is unclear |

## Error Handling

- `boi` command not found: "BOI is not installed. Run `bash ~/.boi/src/install.sh` from a tmux pane (outside Claude Code) first."
- Daemon not running on dispatch: `boi dispatch` starts it automatically.
- Queue ID invalid: relay the CLI error message.
- Spec validation fails: show validation errors and help fix the spec format.

## Constraints

- `boi install` runs **outside Claude Code** in a terminal.
- Workers are headless `claude -p` sessions — not interactive.
- Daemon polls every 5 seconds. Status may lag slightly.
- Default 3 workers, max 5. Set during install.
- Workers get fresh context each iteration. No memory of previous iterations.
- State lives in `~/.boi/` (queue database, logs, phases, guardrails, gates).
- Python: stdlib only. Shell: `set -uo pipefail`.
