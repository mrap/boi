---
name: boi
description: "Dispatch self-evolving specs to parallel Claude Code workers. Use when the user says 'boi', '/boi', 'dispatch this', 'dispatch spec', 'fire it', 'boi status', 'boi queue', 'boi log', 'boi stop', 'boi cancel', 'boi telemetry', 'boi dashboard', 'run this overnight', 'self-evolving loop', or wants to break a task into a spec and dispatch it to autonomous workers. Also use when the user has a complex task and wants it executed iteratively by fresh Claude sessions."
---

# BOI — Beginning of Infinity

Self-evolving autonomous agent fleet. Workers iterate with fresh context per cycle. Specs carry state. The queue manages priority.

## How BOI Works

1. User describes a task
2. Claude decomposes it into a **spec.yaml** (a YAML file with a `tasks:` array of objects with `status: PENDING`)
3. User confirms ("fire it", "dispatch", "go")
4. Claude runs `boi dispatch --spec <path>`
5. BOI daemon assigns specs to workers (isolated git worktrees)
6. Each worker gets a fresh session via the configured runtime CLI (default: `claude -p`; Codex: `codex exec`), reads the spec, executes the next PENDING task, marks it DONE, exits
7. Daemon detects remaining PENDING tasks and requeues the spec for the next iteration
8. Workers can ADD new PENDING tasks to the spec (self-evolution)
9. Spec completes when all tasks are DONE or SKIPPED

## Commands

### `/boi` or `/boi dispatch` — Plan and dispatch a spec

**Conversational planning flow:**

1. If the user provides a spec file path, validate it exists, then dispatch directly.
2. If the user describes a task without a spec file:
   a. Help decompose the task into discrete, ordered tasks (use `10x-engineer:brainstorming` if available)
   b. Write a spec.yaml file (see Spec Format below)
   c. Show the spec to the user for confirmation
   d. On confirmation ("fire it", "dispatch", "go", "yes"), dispatch it

**Dispatch command:**
```bash
boi dispatch --spec <path/to/spec.yaml> [--priority N] [--max-iter N] [--mode MODE]
```

Options:
- `--spec FILE` — Path to spec.yaml file (required)
- `--priority N` — Queue priority, lower = higher priority (default: 100)
- `--max-iter N` — Maximum iterations before marking failed (default: 30)
- `--worktree PATH` — Pin to a specific worktree
- `--no-critic` — Skip critic validation when this spec completes
- `--mode MODE` / `-m MODE` — Execution mode: `execute` (default), `challenge`, `discover`, `generate` (aliases: `e`, `c`, `d`, `g`)
- `--experiment-budget N` — Override default experiment budget for the chosen mode

Backward compatibility (converts tasks.md to spec format automatically):
```bash
boi dispatch --tasks <path/to/tasks.md>
```

After dispatch, run `boi status` to show initial state.

### `/boi status` — Show queue progress

```bash
boi status
```

Output format:
```
BOI

QUEUE                         MODE       WORKER  ITER   TASKS       QUALITY    PROGRESS   STATUS
q-001  add-dark-mode          discover   w-1     3/30   5/8 done    B (0.78)   51%        running
q-002  api-endpoints          execute    ---     ---    0/9 done    ---        0%         queued
q-003  polish-onboarding      execute    ---     ---    5/5 done    A (0.91)   100%       completed

Workers: 1/3 busy  |  Queue: 1 running, 1 queued, 1 completed
```

For live auto-refresh (every 2s):
```bash
boi status --watch
```

For machine-readable output:
```bash
boi status --json
```

### `/boi queue` — Show spec queue

```bash
boi queue [--json]
```

Shows all specs in the queue with their status, iteration count, and priority.

### `/boi log <queue-id>` — View worker output

```bash
boi log <queue-id>          # tail last 50 lines of latest iteration
boi log <queue-id> --full   # full output
```

Queue ID is required. If not provided, ask the user.

### `/boi telemetry <queue-id>` — Iteration breakdown

```bash
boi telemetry <queue-id> [--json]
```

Output:
```
Spec: add-dark-mode (q-001)
Iterations: 3 of 30
Total time: 47m 23s
Tasks: 5/8 done, 2 added (self-evolved), 1 skipped

Iteration breakdown:
  #1: 2 tasks done, 1 added, 0 skipped (12m 05s)
  #2: 2 tasks done, 1 added, 0 skipped (18m 41s)
  #3: 1 task done, 0 added, 1 skipped (16m 37s)
```

### `/boi cancel <queue-id>` — Cancel a spec

1. Queue ID required. If not provided, ask.
2. Run: `boi cancel <queue-id>`
3. Kills any active worker session for that spec.

### `/boi stop` — Stop everything

1. Confirm: "Stop all workers and the BOI daemon?"
2. Run: `boi stop`
3. Kills all worker tmux sessions and stops the daemon.

### `/boi workers` — Show worktree status

```bash
boi workers [--json]
```

Shows each worker's worktree path and health status (idle, busy, missing, unhealthy).

### `/boi dashboard` — Live dashboard

```bash
boi dashboard
```

Compact 60-char tmux-friendly view with color-coded status. Auto-refreshes every 2s. Same as `boi status --watch`.

### `/boi install` — One-time setup

```bash
boi install [--workers N]
```

Creates `~/.boi/` state directory, sets up worker worktrees, writes config. Must be run outside Claude Code.

## Spec Format

A BOI spec is a YAML file. Each task has an `id`, `title`, `status`, `spec`, and `verify` field.

```yaml
title: "My Project Spec"
mode: execute             # optional — execute (default), challenge, discover, generate
pipeline: execute → critic  # optional — overrides default pipeline

tasks:
  - id: t-1
    title: "First task title"
    status: PENDING
    spec: |
      What the worker should do. Be specific: which files to read,
      what to create, what patterns to follow.
    verify: |
      # Commands that prove the work is done. Non-zero exit = task incomplete.
      test -f output.txt
      python3 -m pytest tests/ -x -q

  - id: t-2
    title: "Second task title"
    status: PENDING
    depends: [t-1]
    spec: |
      Description of second task...
    verify: |
      grep "expected" output.txt
```

**Rules:**
- `id` must be `t-N` (sequential numbers)
- `status` must be `PENDING`, `DONE`, `SKIPPED`, or `FAILED`
- `spec` and `verify` are required
- `depends: [t-X]` prevents a task from running until t-X is DONE
- Tasks are executed in ID order (lowest first)
- Workers execute one task per iteration, then exit

**Writing good specs:**
- Each task should be completable in a single Claude session (10-30 min)
- Include file paths, function names, and concrete references
- Reference earlier tasks if later tasks depend on their output
- Add verification commands that prove the work is done (test runs, lint checks, file existence)

**What makes BOI specs special (self-evolution):**
- During an iteration, a worker can ADD new PENDING tasks to the `tasks:` array
- This lets the system discover work it couldn't foresee at planning time
- The daemon detects new PENDING tasks and requeues the spec automatically
- Example: a worker implementing a feature discovers it needs a migration, so it adds a new task for the migration

## Error Handling

- If `boi` command not found: "BOI is not installed. Run `bash ~/.boi/src/install.sh` from a tmux pane (outside Claude Code) first."
- If daemon not running on dispatch: `boi dispatch` starts it automatically.
- If queue ID invalid: relay the CLI error message.
- If spec validation fails: show the validation errors and help the user fix the spec format.

### `/boi spec <queue-id>` — Live spec management

View and modify tasks in a running or queued spec without editing the raw file.

```bash
boi spec <queue-id>                                # Show tasks with status
boi spec <queue-id> --json                         # Machine-readable output
boi spec <queue-id> add "Title" [--spec "..."] [--verify "..."]  # Add a new task
boi spec <queue-id> skip <task-id> [--reason "..."]              # Skip a task
boi spec <queue-id> next <task-id>                 # Reorder: make this task run next
boi spec <queue-id> block <task-id> --on <dep-id>  # Mark task as blocked by another
boi spec <queue-id> edit [<task-id>]               # Open in $EDITOR
```

Examples:
```bash
boi spec q-001                          # See all tasks and which is next
boi spec q-001 add "Fix flaky test" --spec "Stabilize the race condition in t-3's test" --verify "python3 -m pytest passes 5x"
boi spec q-001 skip t-4 --reason "No longer needed after API change"
boi spec q-001 next t-6                 # Move t-6 to run next
boi spec q-001 block t-5 --on t-3       # t-5 can't run until t-3 is DONE
boi spec q-001 edit t-2                 # Edit just t-2 in your editor
```

### `/boi project` — Organize specs into projects

Projects group related specs and provide shared context that gets injected into every worker prompt.

```bash
boi project create <name> [--description "..."]   # Create a project
boi project list [--json]                          # List all projects
boi project status <name> [--json]                 # Project metadata + associated specs
boi project context <name>                         # Print project context.md
boi project delete <name>                          # Delete a project (confirms first)
```

Dispatch a spec into a project:
```bash
boi dispatch --spec spec.yaml --project my-project
```

When a spec belongs to a project, workers automatically receive the project's `context.md` and `research.md` in their prompt. Workers can also append discoveries to `research.md` for future iterations.

Examples:
```bash
boi project create ios-app --description "iOS app rewrite"
boi dispatch --spec feature.md --project ios-app
boi project status ios-app              # Shows project info + all its specs
boi project list                        # Overview of all projects
```

### `/boi do` — Natural language interface

Translate natural language into BOI CLI commands. Claude interprets your request, generates the right commands, and (optionally) executes them.

```bash
boi do "show me what's running"                    # Generates: boi status
boi do "cancel the ios spec"                       # Generates: boi cancel q-001
boi do "add a task to q-002 for database migration"
boi do --dry-run "stop everything"                 # Show commands without executing
boi do --yes "skip t-4 in q-001"                   # Execute without confirmation
```

Options:
- `--dry-run` — Show generated commands without executing
- `--yes` / `-y` — Skip confirmation for destructive commands

Destructive commands (cancel, stop, purge, delete, skip) always prompt for confirmation unless `--yes` is passed.

### `/boi review <queue-id>` — Review experiment proposals

When a worker proposes an experiment (Challenge, Discover, or Generate modes), the spec pauses with `needs_review` status. Use this command to review and act on experiments.

```bash
boi review q-001
```

For each experiment, choose:
- `[a]` Adopt: merge the experiment branch, mark the task DONE.
- `[r]` Reject: delete the experiment branch, reset the task to PENDING.
- `[d]` Defer: keep the spec paused.
- `[v]` View: see the full experiment details.

Experiments auto-reject after 24 hours if not reviewed.

### Execution Modes

BOI supports 4 execution modes that control what workers can do:

| Mode | Workers can... | Use when... |
|------|---------------|-------------|
| `execute` | Execute tasks only (default) | Tasks are well-defined and straightforward |
| `challenge` | Execute + flag concerns | You want observations alongside execution |
| `discover` | Execute + add new tasks | Most real-world work (recommended for complex specs) |
| `generate` | Full creative authority over spec | Goal is clear but path is unclear |

Set the mode via CLI flag, spec header, or default:
```bash
boi dispatch --spec spec.yaml --mode discover    # CLI flag
boi dispatch --spec spec.yaml -m g              # Single-letter alias
```

Or in the spec file:
```yaml
mode: discover
```

Generate mode accepts goal-only specs (no pre-defined tasks) with `goal:`, `constraints:`, and `success_criteria:` top-level fields.

### `/boi critic` — Manage the critic system

The critic is BOI's quality gate. It reviews completed specs before marking them done, checking for spec integrity issues, weak verification commands, code quality problems, incomplete work, and fleet-readiness gaps. If it finds issues, it adds `[CRITIC]` PENDING tasks and requeues the spec. If everything passes, the spec is approved.

```bash
boi critic status           # Show critic config, active checks, pass counts
boi critic run <queue-id>   # Manually trigger critic on a spec
boi critic disable          # Disable the critic globally
boi critic enable           # Enable the critic globally
boi critic checks           # List all active checks (default + custom)
```

### Customizing the Critic

The critic is configured via `~/.boi/critic/`:

- **Config:** Edit `~/.boi/critic/config.json` to change `enabled`, `max_passes`, or restrict which default checks run.
- **Custom checks:** Drop `.md` files into `~/.boi/critic/custom/` to add new checks. If a custom check has the same filename as a default check, the custom one replaces the default.
- **Custom prompt:** Create `~/.boi/critic/prompt.md` to completely replace the default critic prompt template. Use `{{SPEC_CONTENT}}`, `{{CHECKS}}`, `{{QUEUE_ID}}`, `{{ITERATION}}` variables.
- **Disable per-spec:** Use `boi dispatch --spec spec.yaml --no-critic` to skip critic validation for a single spec.

### Helping Users Create Custom Checks

When a user wants to create a custom critic check, guide them conversationally:

1. **Ask what they want to catch.** "What kind of issues should this check look for?" Common categories: security, performance, accessibility, API design, test coverage, documentation.

2. **Draft the check file.** A check file is a Markdown document with three sections:
   - **Title and description** (one paragraph explaining what it validates)
   - **Checklist** (concrete yes/no items the critic evaluates)
   - **Examples of violations** (code snippets showing what bad looks like, with severity tags)

3. **Write it to the right location.** Save to `~/.boi/critic/custom/<check-name>.md`.

4. **Verify it's active.** Run `boi critic checks` to confirm the new check appears in the list.

**Example conversation flow:**

User: "I want the critic to check for performance issues"

Claude writes `~/.boi/critic/custom/performance-review.md`:
```markdown
# Performance Review

Validates that code changes do not introduce performance regressions.

## Checklist

- [ ] No O(n^2) or worse algorithms on unbounded input
- [ ] Database queries use indexes (no full table scans on large tables)
- [ ] No synchronous I/O in hot paths (event loops, request handlers)
- [ ] Large collections are paginated, not loaded entirely into memory
- [ ] Cache invalidation is handled correctly (no stale reads, no cache stampedes)

## Examples of Violations

### Quadratic loop (HIGH severity)
for item in items:
    if item in other_list:  # O(n) lookup inside O(n) loop = O(n^2)
        results.append(item)

### Unbounded query (MEDIUM severity)
users = db.query("SELECT * FROM users")  # loads entire table
```

Then confirms: `boi critic checks` shows `performance-review (custom)`.

## Constraints

- `boi install` runs **outside Claude Code** in a terminal.
- Workers are headless, non-interactive CLI agent sessions. Default runtime: `claude -p`. Codex runtime: `codex exec`. Configured globally in `~/.boi/config.json` or per-spec via `runtime: codex` field.
- Daemon polls every 5 seconds. Status may lag slightly.
- Default 3 workers, max 5. Set during install.
- Workers get fresh context each iteration. No memory of previous iterations.
- State lives in `~/.boi/` (queue, logs, events, hooks, config).
- Python: stdlib only. Shell: `set -uo pipefail`.
