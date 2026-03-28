# BOI — Beginning of Infinity

BOI is a self-evolving autonomous agent fleet for Claude Code. You write a spec file with ordered tasks; BOI assigns each iteration to a fresh Claude worker that executes the next pending task, marks it done, and exits. The daemon detects remaining work and requeues. Workers can add new tasks at runtime — the spec evolves as execution reveals what was unforeseen. Named after David Deutsch's *The Beginning of Infinity*: knowledge grows through conjecture and criticism.

## Quick Start

```bash
# 1. Install (run outside Claude Code, in a terminal)
boi install [--workers N]

# 2. Write a spec or let Claude write one for you
cat > my-feature.spec.md << 'EOF'
# My Feature

## Tasks

### t-1: Implement the thing
PENDING

**Spec:** Add X to lib/foo.py following the existing pattern.

**Verify:**
```bash
python3 -m pytest tests/test_foo.py -x -q
```
EOF

# 3. Dispatch
boi dispatch --spec my-feature.spec.md

# 4. Monitor
boi status
boi log <queue-id>
```

## Spec Format

A spec is a Markdown file. The daemon reads it on each iteration to find the next PENDING task.

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

**Task status values:** `PENDING` → `DONE` | `SKIPPED` | `FAILED`

**Rules:**
- Headings must be `### t-N: Title` with status on the next line
- `**Spec:**` and `**Verify:**` are required
- `**Blocked by:** t-X` prevents a task from running until t-X is DONE
- Workers add new `### t-N: ... PENDING` tasks to self-evolve the spec
- One task per worker iteration; daemon requeues until all tasks are DONE

## Phases

A **phase** is a named worker role defined by a `.phase.toml` file. The daemon loads phases from `~/.boi/phases/` and hot-reloads them when files change.

### Built-in Phases

| Phase | Description | Model | Timeout |
|-------|-------------|-------|---------|
| `execute` | Execute tasks from the spec | claude-sonnet-4-6 | 600s |
| `review` | Code review: correctness, security, spec compliance | claude-sonnet-4-6 | 300s |
| `critic` | Quality gate: reviews completed work, adds [CRITIC] tasks on failure | claude-sonnet-4-6 | 300s |
| `decompose` | Decompose a high-level spec into actionable tasks | claude-opus-4-6 | 600s |
| `evaluate` | Evaluate spec completion and determine next steps | claude-sonnet-4-6 | 300s |

### Phase File Schema (`.phase.toml`)

```toml
# Top-level
name = "my-phase"                        # optional; derived from filename if omitted
description = "What this phase does"     # optional
completion_handler = "builtin:execute"   # optional — use built-in routing logic

# Worker configuration
[worker]
prompt_template = "templates/my-prompt.md"  # required — path to prompt template
model = "claude-sonnet-4-6"                  # default: claude-sonnet-4-6
effort = "medium"                            # low | medium | high
timeout = 300                                # seconds; must be > 0

# Completion routing
[completion]
approve_signal = "## Approved"    # string the worker must output to approve
reject_signal = "[REJECTED]"      # string that triggers rejection handling
on_approve = "next"               # next | complete | commit | phase:<name>
on_reject = "requeue:execute"     # fail | retry | requeue:<phase> | phase:<name>
on_crash = "retry"                # retry | fail

# Per-phase guardrail hooks
[hooks]
pre = ["verify-commands-pass"]    # gates to run before this phase starts
post = ["diff-is-non-empty"]      # gates to run after this phase completes
```

**`on_approve` values:**
- `next` — advance to the next phase in the pipeline
- `complete` — mark the spec done
- `commit` — commit changes, then advance
- `phase:<name>` — jump to a named phase

**`on_reject` values:**
- `requeue:<phase>` — send back to the named phase (e.g. `requeue:execute`)
- `phase:<name>` — jump to a named phase
- `retry` — re-run this phase
- `fail` — mark the spec failed

**`completion_handler`:** Set this top-level field to delegate routing to a built-in handler (e.g. `"builtin:execute"`). Use it when you want a phase to reuse the same routing logic as a built-in phase rather than defining your own `approve_signal`/`reject_signal` strings. When `completion_handler` is set, the daemon calls the named built-in handler and ignores the `[completion]` signals.

### Creating a Custom Phase

1. Write a prompt template at `~/.boi/phases/templates/security-scan-prompt.md`
2. Create the phase file:

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

3. Use it in a spec: `**Pipeline:** execute → security-scan → review`

The daemon hot-reloads phase files — no restart needed.

## Pipelines

A **pipeline** is an ordered list of phases a spec passes through. Phases run sequentially; the spec advances on approval.

### Default Pipeline

Configured in `~/.boi/guardrails.toml`:

```toml
[pipeline]
default = ["execute", "critic"]
```

### Per-Spec Override

Add a `**Pipeline:**` header to the spec:

```markdown
**Pipeline:** execute → review → critic
```

Arrows (`→` or `->`) and commas are all valid separators.

### Example Pipelines

```markdown
**Pipeline:** execute                          # execute only
**Pipeline:** decompose → execute → critic    # decompose first
**Pipeline:** execute → review → critic       # full review cycle
**Pipeline:** execute → security-scan         # custom phase
```

## Guardrails

Guardrails define quality gates that run at phase transitions. Configured globally in `~/.boi/guardrails.toml`, overridable per spec.

### `guardrails.toml` Format

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

[gates.tests-pass]
command = "python3 -m pytest tests/ -x -q"
timeout = 120
```

### Strictness Levels

| Level | Behavior on gate failure |
|-------|--------------------------|
| `strict` | Blocks phase transition; appends a `[GATE-FAIL]` PENDING task to the spec |
| `advisory` | Logs a warning; execution continues |
| `permissive` | Silently skips; execution continues |

### Per-Spec Gate Overrides

Add a `**Gates:**` header to the spec:

```markdown
**Gates:** strict, +lint-pass, -no-secrets
```

Tokens:
- `strict` / `advisory` / `permissive` — override strictness
- `+gate-name` — add a gate to all hook points
- `-gate-name` — remove a gate from all hook points

## Gates

A **gate** is a check that runs at a hook point (e.g. `post-execute`, `pre-commit`). Gates return `passed=True` or `passed=False`.

### Built-in Gates

| Gate | What it checks |
|------|---------------|
| `verify-commands-pass` | Parses the `**Verify:**` block from the spec and runs the commands; fails if any exit non-zero |
| `diff-is-non-empty` | Runs `git diff HEAD` and `git diff --cached`; fails if no changes are detected |
| `tests-pass` | Runs the configured test command (default: `python3 -m pytest tests/ -x --tb=short -q`) |
| `lint-pass` | Runs the configured lint command (default: `python3 -m flake8 .`) |
| `no-secrets` | Scans `git diff HEAD` for API keys, tokens, private keys, and common secret patterns |

### Custom Gates

Drop a shell script at `~/.boi/gates/<name>.sh`. The daemon resolves gate names by checking the built-in registry first, then falling back to shell scripts.

```bash
# ~/.boi/gates/my-check.sh
#!/bin/sh
set -uo pipefail

# Environment variables available:
#   SPEC_PATH — path to the spec file
#   SPEC_ID   — queue ID (e.g. q-042)

if some-command fails; then
  echo "Reason for failure" >&2
  exit 1
fi

exit 0
```

Enable it in `guardrails.toml`:

```toml
[hooks]
post-execute = ["my-check"]
```

Exit 0 = passed. Any non-zero exit = failed. Stdout/stderr are captured as the failure message.

## CLI Reference

```
boi dispatch --spec <file.md> [options]   Submit a spec to the queue
boi status [--watch] [--json]             Show queue and worker status
boi log <queue-id> [--full]              Tail worker output for a spec
boi cancel <queue-id>                     Cancel a running or queued spec
boi stop                                  Stop daemon and all workers
boi install [--workers N]                 One-time setup (run outside Claude Code)
boi resume <queue-id> | --all            Resume failed or canceled specs
boi cleanup                               Kill orphaned worker processes
boi workers [--json]                      Show worktree health
boi telemetry <queue-id> [--json]        Per-iteration metrics
boi critic status | run | enable | disable | checks
boi spec <queue-id> [add|skip|next|block|edit|deps]
boi dep add|remove|set|clear|show|viz|check
boi project create|list|status|context|delete
```

**`dispatch` options:**

| Flag | Description |
|------|-------------|
| `--spec FILE` | Spec file path (required) |
| `--priority N` | Lower = higher priority (default: 100) |
| `--max-iter N` | Max iterations before marking failed (default: 30) |
| `--mode MODE` | `execute` \| `challenge` \| `discover` \| `generate` (aliases: e/c/d/g) |
| `--worktree-isolate` | Dedicated git worktree and branch for this spec |
| `--after q-A,q-B` | Wait for listed specs to complete before starting |
| `--no-critic` | Skip critic phase for this spec |
| `--project NAME` | Associate with a project (injects project context) |

## Architecture

```
boi dispatch → SQLite queue (~/.boi/queue.db)
                     |
              Daemon (daemon.py)
              polls every 5s
                     |
         +-----------+-----------+
         |           |           |
      Worker 1    Worker 2    Worker 3
      (claude -p) (claude -p) (claude -p)
      worktree    worktree    worktree
         |           |           |
      Reads spec, executes next PENDING task, marks DONE, exits
         |
      Daemon detects completion
         |
      _dispatch_phase_completion()
         |
      Phase routing:
        approve → _advance_pipeline() → next phase or complete
        reject  → requeue to target phase
        crash   → retry or fail
```

**Key behaviors:**

- **Hot-reload:** The daemon calls `_reload_phases_if_changed()` each poll cycle. Edit a `.phase.toml` file and the daemon picks it up without restart.
- **Pipeline advancement:** `_advance_pipeline()` reads `guardrails.toml` to find the configured pipeline, finds the current phase's index, and requeues the spec for `pipeline[index+1]`. If current phase is not in the pipeline or is the last entry, the spec is marked complete.
- **Phase routing:** `_dispatch_phase_completion()` checks the `PhaseConfig` for the current phase. If `completion_handler` is set to a builtin (e.g. `builtin:execute`), the corresponding builtin handler runs. Otherwise, the phase's `approve_signal` / `reject_signal` strings are matched against the worker's output to determine routing.
- **State directory:** `~/.boi/` holds the queue database, logs, phase files, guardrails config, and gates.
- **Worker isolation:** Each worker runs in a git worktree under `~/.boi/worktrees/`. Isolated specs (`--worktree-isolate`) get their own branch; shared specs use a rotating pool of worker worktrees.
- **Consecutive failure protection:** SIGTERM (exit 143) and SIGKILL (exit 137) do not count as consecutive failures. Workers killed externally are requeued, not failed.
