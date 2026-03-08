# BOI — Beginning of Infinity

A self-evolving autonomous agent fleet for Claude Code.

Workers iterate with fresh context per cycle. Specs carry state. The queue manages priority. Tasks evolve at runtime.

Named after David Deutsch's *The Beginning of Infinity*: knowledge grows through conjecture and criticism. BOI specs are conjectures. Each iteration is a round of criticism and refinement. The agent discovers what it couldn't foresee, adds new tasks, and keeps going.

## Why BOI Exists

AI coding agents degrade over long sessions. Context fills up, instructions get lost, and the agent starts hallucinating or repeating itself. This is called **context rot**.

Most tools deal with context rot by compressing old context (lossy), training longer-context models (expensive), or ignoring it (broken). BOI takes a different approach: **prevent it entirely**.

Every iteration starts with a fresh Claude session. Zero accumulated context. The spec file on disk is the single source of truth. The agent reads the spec, executes the next pending task, marks it done, and exits. The daemon detects remaining work and launches a new session. Each iteration gets the agent's full cognitive clarity.

## How It Works

```
You → boi dispatch --spec spec.md → Spec Queue (priority-sorted)
                                          |
                                     +---------+
                                     | Daemon  |
                                     | (picks  |
                                     |  next   |
                                     |  spec)  |
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

1. You write a **spec.md** with ordered tasks (or Claude helps you write one)
2. `boi dispatch --spec spec.md` adds it to the priority queue
3. The daemon assigns specs to available workers (isolated git worktrees)
4. Each worker gets a fresh `claude -p` session, reads the spec, executes the next PENDING task, marks it DONE, and exits
5. The daemon detects remaining PENDING tasks and requeues the spec
6. Workers can **add new PENDING tasks** to the spec during execution (self-evolution)
7. The spec completes when all tasks are DONE or SKIPPED

## What Makes BOI Different

| Feature | Vanilla Ralph Loop | Code Factory | BOI |
|---------|-------------------|-------------|-----|
| Fresh context per iteration | Yes | Per-step | Yes |
| Self-evolving task list | No | No | Yes (Discover/Generate modes) |
| Execution modes | No | No | 4 modes (Execute, Challenge, Discover, Generate) |
| Quality scoring | None | None | 18-signal scoring with quality gates |
| Experiments | No | No | Propose, review, adopt/reject |
| Goal-driven specs | No | Yes (PRD) | Generate mode (goal + criteria) |
| Overnight resilience | Basic | Single session | Consecutive-failure cooldown, max-iteration exit |
| Multi-spec queue | No | Single spec | Priority queue with DAG blocking |
| Parallel workers | No | No | Yes (N workers, N worktrees) |
| Telemetry | None | None | Per-iteration metrics + quality trends |
| External hooks | None | None | on-complete.sh, on-fail.sh, event JSON |

**Self-evolving specs** are the key differentiator. A worker implementing a feature might discover it needs a database migration. It adds a new PENDING task for the migration right there in the spec file. The daemon detects the new task and keeps iterating. The system discovers work it couldn't foresee at planning time.

## Quick Start

### 1. Install

```bash
curl -fsSL https://raw.githubusercontent.com/mrap/boi/main/install-public.sh | bash
```

Or clone and install manually:

```bash
git clone https://github.com/mrap/boi.git ~/boi
bash ~/boi/install.sh --workers 3
```

This creates `~/.boi/` state directories, sets up worker worktrees, writes config, and adds a `boi` alias to your shell.

### 2. Write a spec

```markdown
# My Feature Spec

## Tasks

### t-1: Set up the data model
PENDING

**Spec:** Create the database schema for UserPreferences with fields: user_id,
theme (enum: light/dark), language (string), notifications_enabled (bool).

**Verify:** Type checks pass. Tests cover schema validation.

### t-2: Build the API mutation
PENDING

**Spec:** Add a setUserPreferences mutation that validates input and writes
to the model from t-1. Follow existing mutation patterns in the codebase.

**Verify:** Type checks pass. Unit test covers valid and invalid input.

### t-3: Wire up the React UI
PENDING

**Spec:** Add a PreferencesPanel component to the settings page. Use the
API client to call the mutation from t-2. Follow existing form patterns.

**Verify:** Type checks pass. Component renders in storybook.
```

### 3. Dispatch

```bash
boi dispatch --spec ~/specs/user-prefs.md
```

Or with priority and iteration limit:

```bash
boi dispatch --spec ~/specs/user-prefs.md --priority 50 --max-iter 10
```

### 4. Watch it work

```bash
boi status           # snapshot of queue + workers
boi status --watch   # live auto-refresh every 2s
boi dashboard        # compact tmux-friendly view
```

### 5. Check results

```bash
boi log q-001              # tail latest iteration output
boi log q-001 --full       # full output
boi telemetry q-001        # per-iteration breakdown
```

## CLI Reference

| Command | Description |
|---------|-------------|
| `boi dispatch --spec <file>` | Submit a spec to the queue |
| `boi dispatch --tasks <file>` | Submit a tasks.md (auto-converts to spec format) |
| `boi queue` | Show all specs with status, iteration count, priority |
| `boi status` | Show workers and current assignments (with mode, quality, progress) |
| `boi status --watch` | Live auto-refresh dashboard |
| `boi log <queue-id>` | Tail latest iteration log |
| `boi log <queue-id> --full` | Full log output |
| `boi cancel <queue-id>` | Cancel a queued or running spec |
| `boi stop` | Stop daemon and all workers |
| `boi workers` | Show worktree/worker availability |
| `boi telemetry <queue-id>` | Per-iteration task, quality, and timing breakdown |
| `boi dashboard` | Compact tmux-friendly live view |
| `boi review <queue-id>` | Review experiment proposals (adopt/reject/defer) |
| `boi purge` | Remove completed/failed/canceled specs from queue |
| `boi purge --all` | Remove ALL specs (including queued/running) |
| `boi purge --dry-run` | Preview what would be removed |
| `boi install [--workers N]` | One-time setup |
| `boi install --worktree-paths P1,P2` | Install with existing worktrees (skips creation) |
| `boi critic status` | Show critic config and active checks |
| `boi critic run <queue-id>` | Manually trigger critic on a spec |
| `boi critic disable` | Disable the critic |
| `boi critic enable` | Enable the critic |
| `boi critic checks` | List all active checks (default + custom) |
| `boi spec <queue-id>` | Show tasks in a spec with status |
| `boi spec <queue-id> add "Title"` | Add a new task to a spec |
| `boi spec <queue-id> skip <task-id>` | Skip a pending task |
| `boi spec <queue-id> next <task-id>` | Reorder task to run next |
| `boi spec <queue-id> block <t-id> --on <dep>` | Mark task as blocked by another |
| `boi spec <queue-id> edit [<task-id>]` | Open spec/task in `$EDITOR` |
| `boi project create <name>` | Create a new project |
| `boi project list` | List all projects |
| `boi project status <name>` | Show project metadata + specs |
| `boi project context <name>` | Print project context.md |
| `boi project delete <name>` | Delete a project |
| `boi do "request"` | Translate natural language to BOI commands |
| `boi do --dry-run "request"` | Show generated commands without executing |
| `boi doctor` | Check prerequisites and environment health |

Options on `dispatch`:
- `--priority N` — Lower number = higher priority (default: 100)
- `--max-iter N` — Max iterations before marking failed (default: 30)
- `--worktree PATH` — Pin to a specific worktree
- `--no-critic` — Skip critic validation when this spec completes
- `--project NAME` — Associate this spec with a BOI project
- `--mode MODE` / `-m MODE` — Set execution mode: `execute` (default), `challenge`, `discover`, `generate` (aliases: `e`, `c`, `d`, `g`)
- `--experiment-budget N` — Override default experiment budget for the chosen mode

Options on `queue`, `status`, `telemetry`:
- `--json` — Machine-readable JSON output

### Status output

```
BOI

QUEUE                         MODE       WORKER  ITER   TASKS       QUALITY    PROGRESS   STATUS
q-001  ai-profile-v2          discover   w-1     7/30   5/8 done    B (0.78)   51%        running
q-002  topic-chats-backend    execute    ---     ---    0/9 done    ---        0%         queued
q-003  security-audit         challenge  w-2     2/30   1/12 done   A (0.91)   8%         running

Workers: 2/3 busy  |  Queue: 2 running, 1 queued
```

Generate mode specs show additional detail:
```
[q-004] Generate: devconfig-cli
  Phase: EXECUTE (2/3)
  Success Criteria: 4/6 met
  Experiment budget: 3/5 remaining
```

Quality alerts appear as warnings:
```
  ⚠ q-001: Quality declining (dropped 0.18 in last iteration)
```

### Telemetry output

```
Spec: ios-recording (q-001)
Mode: discover
Iterations: 3 of 30
Total time: 47m 23s
Tasks: 5/8 done, 2 added (self-evolved), 1 skipped
Quality: B (0.78)
Progress: 51%

Quality breakdown:
  Code quality:   0.82
  Test quality:   0.75
  Documentation:  0.90
  Architecture:   0.78

Progress metrics:
  Evolution ratio:         25% (self-evolved tasks / total completed)
  Productive failure rate: 50% (failed iterations that added new tasks)
  First-pass rate:         80% (tasks done without critic rejection)

Iteration breakdown:
  #1: 2 tasks done, 1 added, 0 skipped (12m 05s) quality: B (0.78)
  #2: 2 tasks done, 1 added, 0 skipped (18m 41s) quality: B (0.80)
  #3: 1 task done, 0 added, 1 skipped (16m 37s)  quality: A (0.85)
```

### Dashboard (compact, 60-char tmux pane)

```
=== BOI =============== 08:23 ==
 + q-001 ios-recording     5/8  3i disc B
 > q-002 topic-chats       2/9  1i exec   w-1
 . q-003 heartbeat         0/5  0i exec
Workers: 1/3 busy | Queue: 3
```

## How to Write a Good Spec

### Required format

Every task needs three things: a heading, a status line, and spec/verify sections.

```markdown
### t-1: Task title
PENDING

**Spec:** What the worker should do. Be concrete: file paths, function names,
patterns to follow. The worker has no memory of previous iterations.

**Verify:** How to prove the task is done. Commands to run, assertions to check.

**Self-evolution:** Optional. What to do if this task reveals additional work.
```

### Rules

- Task headings: `### t-N: Title` (three hashes, `t-` prefix, sequential numbers)
- Status line: `PENDING`, `DONE`, `SKIPPED`, `FAILED`, `EXPERIMENT_PROPOSED`, or `SUPERSEDED by t-N` on its own line immediately after the heading
- `**Spec:**` section required
- `**Verify:**` section required
- Tasks execute in ID order (lowest first)
- One task per iteration

### Tips

- **Scope each task to one Claude session** (10-30 minutes of work). If a task takes multiple sessions, it's too big. Split it.
- **Include file paths and concrete references.** The worker starts with zero context. It reads only the spec.
- **Reference earlier tasks** if later tasks depend on their output ("Build on the schema created in t-1").
- **Add verification commands** that prove the work is done (`python3 -m pytest tests/`, `lint check`, `python3 -m unittest`).
- **Think about self-evolution.** What might the worker discover? Add guidance: "If the API shape doesn't match, add a new task for the adapter layer."

### Spec validation

BOI validates specs before dispatch. Invalid specs are rejected with clear error messages:

```bash
$ boi dispatch --spec broken-spec.md
Error: Task t-3 missing **Spec:** section
Error: Task t-5 has no status line after heading
Spec validation failed: 2 errors
```

## Execution Modes

BOI has four execution modes that control what workers can do during each iteration. Modes are a graduated capability system, from strict task execution to full creative authority over the spec.

### Mode Overview

| Mode | Add Tasks | Skip Tasks | Write Challenges | Modify PENDING | Supersede | Experiments |
|------|-----------|------------|------------------|----------------|-----------|-------------|
| **Execute** | No | No | No | No | No | No |
| **Challenge** | No | Yes (with reason) | Yes | No | No | Yes (budget: 2) |
| **Discover** | Yes | Yes (with reason) | No | No | No | Yes (budget: 3) |
| **Generate** | Yes | Yes | Yes | Yes | Yes | Yes (budget: 5) |

### Setting the Mode

Three ways to set the mode, in order of precedence:

1. **Spec header** (highest): Add `**Mode:** discover` to the spec file header.
2. **CLI flag**: `boi dispatch --spec spec.md --mode discover` or `-m d`.
3. **Default**: `execute` if nothing else is specified.

### Execute Mode (default)

The strictest mode. Workers execute the current task exactly as specified and nothing else. No task additions, no skipping, no modifications. Use for well-defined, straightforward tasks.

```bash
boi dispatch --spec spec.md                     # execute is the default
boi dispatch --spec spec.md --mode execute
boi dispatch --spec spec.md -m e
```

### Challenge Mode

Execute the task, but also flag concerns. Workers can write observations to a `## Challenges` section and skip tasks with detailed reasoning, but cannot add new tasks or modify the spec structure. Use when you want a second pair of eyes on the approach.

```bash
boi dispatch --spec spec.md --mode challenge
```

Workers write challenges in this format:
```markdown
## Challenges

### c-1: [task t-3] Missing error handling
**Observed:** The API endpoint has no retry logic for transient failures.
**Risk:** HIGH
**Suggestion:** Add exponential backoff with 3 retries.
```

### Discover Mode

Execute the task AND handle what you find. Workers can add new PENDING tasks when they discover necessary work that wasn't foreseeable at planning time. The key differentiator of BOI. Use for most real-world work.

```bash
boi dispatch --spec spec.md --mode discover
```

Workers document discoveries:
```markdown
## Discovery

### Iteration 5
- **Found:** The database schema needs a new index for the user lookup query.
- **Added:** t-8 (add database index).
- **Rationale:** Without the index, getUserByEmail does a full table scan.
```

### Generate Mode

Full creative authority. Workers can add tasks, modify PENDING tasks, supersede tasks with better alternatives, and restructure the plan. Use for exploratory work where the path to the goal is unclear.

Generate mode uses a **goal-only spec format** (no pre-defined tasks) and a three-phase lifecycle:

1. **Decompose**: A decomposition worker breaks the goal into 5-15 concrete tasks.
2. **Execute**: Workers execute tasks iteratively (same as other modes).
3. **Evaluate**: An evaluation worker checks each Success Criterion against the implementation. Unmet criteria generate new tasks. The loop continues until all criteria are met or convergence is reached.

```bash
boi dispatch --spec goal-spec.md --mode generate
```

#### Generate Spec Format

```markdown
# [Generate] Config Management CLI

## Goal
Build a CLI tool that reads, validates, and applies YAML configuration files
with schema validation, environment variable interpolation, and dry-run mode.

## Constraints
- Python 3.10+, stdlib only (no pip dependencies)
- Must work on Linux and macOS
- Config files must be valid YAML

## Success Criteria
- [ ] CLI reads and parses YAML config files
- [ ] Schema validation catches malformed configs
- [ ] Environment variables are interpolated in config values
- [ ] Dry-run mode shows what would change without applying
- [ ] Help text is complete and accurate
- [ ] Unit tests cover all core functions
```

No `### t-N:` tasks are required. The decomposition worker creates them.

#### Convergence

Generate mode stops when:
- All Success Criteria are met and the critic approves (ideal).
- Max iterations reached (default 50 for Generate).
- No progress for 5 consecutive iterations (stalled).
- Diminishing returns: last 3 iterations improved criteria by less than 1 each, and more than 80% of criteria are met (good enough).

## Self-Evolving Specs

This is what makes BOI fundamentally different from a task runner.

During an iteration, a worker can modify the spec file:
- **Mark a task DONE** with notes about what was completed
- **Mark a task SKIPPED** if it's no longer relevant
- **Add new PENDING tasks** when it discovers work that wasn't foreseeable at planning time

Example: a worker implementing an API endpoint discovers the database schema needs a new index. It adds:

```markdown
### t-7: Add database index for user lookup
PENDING

**Spec:** The getUserByEmail query in t-3 does a full table scan. Add an index
on the email column in the User model.

**Verify:** Tests pass. Query plan shows index usage.
```

The daemon detects the new PENDING task and requeues the spec. The next iteration picks up t-7. The system adapts to reality as it discovers it.

This is why BOI works overnight. You dispatch a spec at 6 PM. By 8 AM, it has completed the original 5 tasks, discovered 3 more, completed those too, and logged the whole journey in telemetry.

## Integration Hooks

BOI writes lifecycle events to `~/.boi/events/` as JSON:

```json
{
  "type": "spec_completed",
  "queue_id": "q-001",
  "spec_path": "/home/user/specs/feature.md",
  "iterations": 5,
  "tasks_done": 8,
  "tasks_added": 3,
  "timestamp": "2026-03-06T08:23:45Z"
}
```

Optional hook scripts in `~/.boi/hooks/`:
- `on-complete.sh <queue-id> <spec-path>` runs after a spec completes
- `on-fail.sh <queue-id> <spec-path>` runs after a spec fails (max iterations or consecutive crashes)

Wire up whatever you want: GChat notifications, email alerts, dashboard updates. BOI doesn't care. It just writes events and calls hooks.

## Telemetry

BOI tracks per-iteration metrics for every spec:

- Tasks completed, added, and skipped per iteration
- Duration per iteration
- Consecutive failure count
- Total time spent across all iterations
- Execution mode per iteration
- Quality scores per iteration (when critic is enabled)

All telemetry lives in `~/.boi/queue/{id}.telemetry.json`. Access it via `boi telemetry <queue-id>` or `--json` for programmatic consumption.

## Progress Measurement

BOI measures progress on two axes: **completion** (how many tasks are done) and **quality** (how well was the work done). These combine into a single progress score.

### The Formula

```
progress = completion * (0.5 + 0.5 * quality)
```

- At quality 0.0, progress is halved (you're doing the work but doing it poorly).
- At quality 1.0, progress equals completion (full credit for good work).
- This means a spec that's 100% complete with 0% quality scores 50%, not 100%.

### Quality Scoring

Quality is measured across 18 signals in 4 categories:

| Category | Weight | Signals |
|----------|--------|---------|
| Code Quality | 35% | Error handling, input validation, resource management, naming, complexity, edge cases |
| Test Quality | 25% | Coverage, assertion quality, edge case testing, test isolation, verify command rigor |
| Documentation | 15% | Inline comments, spec clarity, error messages |
| Architecture | 25% | Separation of concerns, dependency management, extensibility, consistency |

Each signal is scored as a ratio (e.g., "6 of 8 I/O operations have error handling = 0.75"). Signals are counted and classified, not subjectively assessed.

If a category doesn't apply (e.g., no source files were modified, skip Code Quality), its weight is redistributed proportionally.

### Grading Scale

| Grade | Score Range |
|-------|------------|
| A | 0.85 - 1.00 |
| B | 0.70 - 0.84 |
| C | 0.50 - 0.69 |
| D | 0.30 - 0.49 |
| F | 0.00 - 0.29 |

### Quality Gates

The critic uses quality scores to gate behavior:
- Score >= 0.85: **Fast-approve.** Skip detailed checks.
- Score 0.50-0.84: **Standard review.** Run all checks.
- Score < 0.50: **Auto-reject.** Add a `[CRITIC]` task for quality improvement.

### Deutschian Progress Metrics

Three metrics inspired by David Deutsch's epistemology, tracking how well the system creates knowledge:

- **Evolution ratio**: What fraction of completed tasks were self-evolved (not in the original spec)? Higher means the system discovered and adapted.
- **Productive failure rate**: Of iterations where no task was completed, how many added new tasks? Failure that produces new conjectures is productive.
- **First-pass completion rate**: What fraction of tasks were completed without critic rejection? Higher means higher initial quality.

These appear in `boi telemetry <queue-id>` output.

## Experiments

In Challenge, Discover, and Generate modes, workers can propose alternative approaches by creating experiments. Each mode has an experiment budget (default: 2/3/5 respectively). Override with `--experiment-budget N`.

### How Experiments Work

1. A worker finds evidence for a better approach during task execution.
2. It creates a branch: `git checkout -b experiment-{queue_id}-{task_id}`.
3. It implements the alternative on that branch.
4. It writes an `#### Experiment:` section under the task with thesis, evidence, and results.
5. It marks the task `EXPERIMENT_PROPOSED`.
6. The daemon pauses the spec (status: `needs_review`) and notifies you.

### Reviewing Experiments

```bash
boi review q-001
```

For each experiment, you see a summary and choose:
- `[a]` **Adopt**: Merge the experiment branch, mark the task DONE.
- `[r]` **Reject**: Delete the branch, reset the task to PENDING.
- `[d]` **Defer**: Keep the spec paused.
- `[v]` **View**: See the full experiment details.

After review, the spec resumes execution.

Experiments auto-reject after 24 hours if not reviewed (configurable via `experiment_timeout_hours` in `~/.boi/config.json`).

### Task Statuses

| Status | Meaning |
|--------|---------|
| `PENDING` | Not yet started |
| `DONE` | Completed |
| `SKIPPED` | Intentionally bypassed (with reason) |
| `FAILED` | Attempted but could not complete |
| `EXPERIMENT_PROPOSED` | Worker proposed an alternative (awaiting review) |
| `SUPERSEDED by t-N` | Replaced by a better task (Generate mode only) |

## Error Log

Workers (in all modes except Execute) can append to an `## Error Log` section when an attempt fails. Future workers read the Error Log before starting and avoid retrying documented failures.

```markdown
## Error Log

### [iter-5] Attempted regex-based parsing
Tried to parse the config file with regex. Failed because nested YAML
structures can't be reliably matched. Future workers should use the
yaml module from stdlib instead.
```

## Critic

The critic is BOI's built-in quality gate. When a spec finishes all its PENDING tasks, the critic reviews the completed work before marking the spec as done. It evaluates spec integrity, verification rigor, code quality, completeness, and fleet-readiness. It also computes a quality score (see Progress Measurement above). If it finds issues, it adds new `[CRITIC]` PENDING tasks to the spec and requeues it. If everything passes, it writes a `## Critic Approved` section and the spec is marked completed.

The critic is mode-aware:
- **Execute mode**: Flags if the worker added tasks (it shouldn't have).
- **Challenge mode**: Flags if the worker added tasks (it shouldn't have).
- **Discover mode**: Validates new tasks have proper format (Spec + Verify).
- **Generate mode**: Validates SUPERSEDED tasks reference replacements. Runs an additional **goal-alignment** check that verifies each Success Criterion is met by a completed task.

Generate mode specs get 3 critic passes (vs. 2 for other modes) to allow extra iteration on goal alignment.

### Configuration

The critic is configured via `~/.boi/critic/config.json`:

```json
{
  "enabled": true,
  "trigger": "on_complete",
  "max_passes": 2,
  "checks": ["spec-integrity", "verify-commands", "code-quality", "completeness", "fleet-readiness"],
  "custom_checks_dir": "custom",
  "timeout_seconds": 600
}
```

| Field | Description | Default |
|-------|-------------|---------|
| `enabled` | Whether the critic runs at all | `true` |
| `trigger` | When to run (`on_complete` = after all tasks done) | `"on_complete"` |
| `max_passes` | Maximum critic review passes before force-approving | `2` |
| `checks` | Which default checks to run (remove entries to skip them) | All 5 |
| `custom_checks_dir` | Subdirectory name for custom checks | `"custom"` |
| `timeout_seconds` | Maximum time for a critic pass | `600` |

### Custom Checks

Add `.md` files to `~/.boi/critic/custom/` to define additional review criteria. Each file should contain a title, description, and checklist that the critic evaluates against the spec.

If a custom check has the same filename as a default check (e.g., `code-quality.md`), the custom version replaces the default.

#### Example: Security-Focused Custom Check

Create `~/.boi/critic/custom/security-review.md`:

```markdown
# Security Review

Validates that code changes do not introduce security vulnerabilities.

## Checklist

- [ ] No secrets, tokens, or credentials hardcoded in source files
- [ ] All user input is validated and sanitized before use
- [ ] File paths are canonicalized to prevent path traversal attacks
- [ ] Subprocess calls use argument lists, not shell=True with string interpolation
- [ ] No use of eval(), exec(), or equivalent dynamic code execution with untrusted input
- [ ] HTTP endpoints validate authentication and authorization
- [ ] Sensitive data is not logged or written to world-readable files

## Examples of Violations

### Hardcoded secret (HIGH severity)
API_KEY = "sk-live-abc123def456"

### Unsanitized path (HIGH severity)
user_path = request.args["file"]
open(f"/data/{user_path}")  # path traversal: ../../etc/passwd

### Shell injection (HIGH severity)
subprocess.run(f"grep {user_input} log.txt", shell=True)
```

Once saved, this check is automatically included in the next critic pass. Verify with `boi critic checks`.

### Custom Prompt

Create `~/.boi/critic/prompt.md` to completely replace the default critic prompt template. The template supports these variables:

- `{{SPEC_CONTENT}}` — The full spec file contents
- `{{CHECKS}}` — All active check definitions (default + custom)
- `{{QUEUE_ID}}` — The spec's queue ID
- `{{ITERATION}}` — The current critic pass number
- `{{SPEC_PATH}}` — Absolute path to the spec file

### Disabling the Critic

Three ways to skip critic validation:

1. **Globally:** `boi critic disable` (sets `enabled: false` in config.json, re-enable with `boi critic enable`)
2. **Per-spec:** `boi dispatch --spec spec.md --no-critic` (skips the critic for this spec only)
3. **Edit config directly:** Set `"enabled": false` in `~/.boi/critic/config.json`

### Running the Critic Manually

Trigger a critic pass on any spec, regardless of its completion status:

```bash
boi critic run <queue-id>
```

This generates a critic prompt, launches a Claude worker to evaluate the spec, and applies the result (approve or add `[CRITIC]` tasks).

### CLI

```bash
boi critic status     # Show config, active checks, pass history
boi critic run q-001  # Manually trigger critic on a spec
boi critic disable    # Set enabled=false
boi critic enable     # Set enabled=true
boi critic checks     # List all active checks (default + custom)
```

`boi doctor` also reports critic status.

### Directory Structure

```
~/.boi/critic/
├── config.json          # Settings (enabled, trigger, max_passes)
├── prompt.md            # Optional: replaces the default critic prompt
├── custom/              # Optional: additional check definitions
│   ├── security-review.md
│   └── performance-check.md
```

## Live Spec Management

Modify running specs without touching the raw Markdown file. Add tasks, skip tasks, reorder, or set up blocking dependencies.

```bash
boi spec q-001                          # Show all tasks with status
boi spec q-001 --json                   # Machine-readable output
```

### Adding tasks

```bash
boi spec q-001 add "Fix flaky test" --spec "Stabilize the race condition" --verify "pytest passes 5x"
```

New tasks get the next available `t-N` ID and are appended as PENDING.

### Skipping tasks

```bash
boi spec q-001 skip t-4 --reason "Superseded by t-6"
```

Only PENDING tasks can be skipped. DONE tasks cannot be retroactively skipped.

### Reordering

```bash
boi spec q-001 next t-6
```

Moves t-6 to be the next task workers pick up. Physically reorders the task in the spec file so it appears right after the last DONE task.

### Blocking

```bash
boi spec q-001 block t-5 --on t-3
```

Workers will skip t-5 until t-3 is DONE. Multiple dependencies can be added by calling block multiple times.

### Editing

```bash
boi spec q-001 edit          # Open full spec in $EDITOR
boi spec q-001 edit t-2      # Edit just task t-2
```

## Projects

Projects group related specs and inject shared context into every worker prompt.

### Creating a project

```bash
boi project create ios-app --description "iOS app rewrite"
```

Creates `~/.boi/projects/ios-app/` with `project.json` and an empty `context.md`.

### Dispatching into a project

```bash
boi dispatch --spec feature.md --project ios-app
```

Workers on this spec automatically receive the project's `context.md` and `research.md` in their prompt. Workers can append discoveries to `research.md` for future iterations.

### Managing projects

```bash
boi project list                   # All projects with spec counts
boi project list --json            # Machine-readable
boi project status ios-app         # Metadata + associated specs
boi project status ios-app --json  # Machine-readable
boi project context ios-app        # Print context.md contents
boi project delete ios-app         # Delete (confirms first, does not cancel specs)
```

### Project directory structure

```
~/.boi/projects/ios-app/
├── project.json    # Name, description, defaults
├── context.md      # Shared context injected into worker prompts
└── research.md     # Auto-populated by workers with discoveries
```

## `boi do` — Natural Language Interface

Talk to BOI in plain English. Claude translates your request into CLI commands.

```bash
boi do "show me what's running"                    # → boi status
boi do "cancel the ios spec"                       # → boi cancel q-001
boi do "add a task to q-002 for database migration"
boi do "skip t-4 in q-001, no longer needed"
```

### Safety

Destructive commands (cancel, stop, purge, delete, skip) require confirmation:

```bash
boi do "cancel everything"
# → Will run: boi cancel q-001; boi cancel q-002
# → This is destructive. Proceed? [y/N]
```

Use `--yes` to skip confirmation, or `--dry-run` to see what would run without executing:

```bash
boi do --dry-run "stop everything"    # Shows commands only
boi do --yes "skip t-4 in q-001"      # Executes without asking
```

## Architecture

```
~/boi/                          # Source code (standalone, no external deps)
  boi.sh                        # CLI entry point
  daemon.sh                     # Queue-aware dispatch daemon
  worker.sh                     # Iterative worker (one claude -p per iteration)
  dashboard.sh                  # Live-updating compact display
  install.sh                    # One-time setup (git worktrees, config)
  lib/
    queue.py                    # Spec queue (enqueue, dequeue, requeue, priority, DAG)
    spec_parser.py              # Parse spec.md for task status counts
    spec_validator.py           # Validate spec format (standard + Generate specs)
    spec_editor.py              # Add, skip, reorder, block tasks in specs
    project.py                  # Project CRUD (create, list, get, delete)
    do.py                       # Natural language → CLI command translation
    status.py                   # Format status, dashboard, telemetry output
    telemetry.py                # Per-iteration metrics tracking (quality, modes, Deutschian metrics)
    quality.py                  # Quality score computation (18 signals, 4 categories)
    evaluate.py                 # Generate mode: evaluate phase, convergence algorithm
    review.py                   # Experiment review (adopt, reject, finalize)
    event_log.py                # Event logging
    hooks.py                    # Lifecycle hooks (on-complete, on-fail)
    critic_config.py            # Critic configuration management
    critic.py                   # Critic execution (prompt generation, quality gating)
    daemon_ops.py               # Daemon operations (worker completion, experiments, phases)
  templates/
    worker-prompt.md            # Prompt template injected into each worker session
    do-prompt.md                # System prompt for boi do (natural language → CLI)
    critic-prompt.md            # Default critic prompt template
    critic-worker-prompt.md     # Wrapper prompt for critic worker sessions
    generate-decompose-prompt.md # Decomposition prompt for Generate mode
    evaluate-prompt.md          # Evaluation prompt for Generate mode
    modes/                      # Mode-specific prompt fragments
      execute.md
      challenge.md
      discover.md
      generate.md
    checks/                     # Default check definitions
      spec-integrity.md
      verify-commands.md
      code-quality.md
      completeness.md
      fleet-readiness.md
      quality-scoring.md        # 18-signal quality scoring prompt
      goal-alignment.md         # Generate-mode goal alignment check
  tests/                        # Unit tests (mock data, no live API calls)
  SKILL.md                      # Claude Code skill (teaches Claude the CLI)

~/.boi/                         # Runtime state
  config.json                   # Worker/worktree mappings
  queue/                        # Spec queue (JSON per spec + telemetry)
  logs/                         # Per-iteration worker logs
  events/                       # Lifecycle event JSON files
  hooks/                        # Optional hook scripts
  projects/                     # Project directories (context, research, config)
  critic/                       # Critic config and custom checks
    config.json
    custom/                     # User's custom check definitions
    prompt.md                   # Optional: custom critic prompt override
```

## Comparison with Other Approaches

### vs. Vanilla Ralph Loops

Ralph Loops (Geoffrey Huntley) pioneered the pattern: fresh context per iteration, state in files not memory. BOI builds on this with a spec queue (multiple specs, priority ordering), parallel workers, self-evolving specs, and integrated telemetry. Ralph is a script. BOI is a system.

### vs. DAG-Based Orchestrators

Some systems decompose goals into a DAG of tasks and spawn separate sessions per task. They handle context exhaustion reactively with watchdogs that detect and respawn stuck agents. BOI prevents context exhaustion by design (fresh session every iteration) and adds self-evolution (DAG-based systems typically have a fixed task list set by the orchestrator). BOI uses plain Markdown specs, not external task trackers.

### vs. Code Factory

Code Factory takes a feature idea, generates a PRD, and runs autonomously through architecture, TDD, code review, and diff submission. It's opinionated and targets greenfield features. BOI is a generic execution engine for any task list: bug fixes, refactors, research, multi-platform features. Code Factory produces one diff. BOI can produce many, across multiple specs, overnight.

### vs. In-Session Iteration (ralph-wiggum, Pimang Loop)

These run the iteration loop inside a single Claude session. The agent stays in one conversation and tries to manage its own context. This degrades over time. As one author put it: "Long sessions degrade context. The agent grades its own homework." BOI's fresh-process-per-iteration approach guarantees each iteration starts clean.

## Requirements

- Claude Code CLI (`claude` command available)
- tmux (for worker sessions and install)
- Git worktrees (for isolated working directories per worker)
- Python 3.10+ (stdlib only, no pip dependencies)
- Bash/Zsh

## Testing

477 tests across unit, integration, and eval suites (447 unit, 18 integration, 12 eval):

```bash
cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py' -v
cd ~/boi && python3 -m unittest tests.integration_boi -v
cd ~/boi && python3 -m unittest tests.eval_boi -v
```

All tests use mock data. No live Claude calls. No real worktrees.

## Security

**BOI workers run with `--dangerously-skip-permissions`**, which disables all Claude Code permission prompts. Workers can execute any shell command, read/write any file, and make network requests without human confirmation. This is required for non-interactive operation but means:

- Only run specs you wrote or have reviewed. Spec content is injected directly into Claude's prompt.
- On shared machines, run BOI inside a Docker container for filesystem and network isolation.
- Review self-evolved tasks (tasks BOI adds during execution) before running follow-up iterations.

For detailed recommendations on sandboxing, Docker isolation, input validation, and running BOI on shared machines, see [docs/security.md](docs/security.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines on how to contribute.

## License

MIT. See [LICENSE](LICENSE).

---

*"The beginning of infinity is the beginning of explanation. Anything not forbidden by the laws of physics is achievable, given the right knowledge."* — David Deutsch
