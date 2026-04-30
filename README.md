# BOI — Beginning of Infinity

BOI is a self-evolving autonomous agent fleet for Claude Code. You write a spec file with ordered tasks; BOI assigns each iteration to a fresh Claude worker that executes the next pending task, marks it done, and exits. The daemon detects remaining work and requeues. Workers can add new tasks at runtime — the spec evolves as execution reveals what was unforeseen. Named after David Deutsch's *The Beginning of Infinity*: knowledge grows through conjecture and criticism.

## Quick Start

```bash
# 1. Install (run outside Claude Code, in a terminal)
boi install [--workers N]

# 2. Write a spec or let Claude write one for you
cat > my-feature.yaml << 'EOF'
title: My Feature
mode: execute

tasks:
  - id: t-1
    title: Implement the thing
    status: PENDING
    spec: |
      Add X to lib/foo.py following the existing pattern.
    verify: "python3 -m pytest tests/test_foo.py -x -q"
EOF

# 3. Dispatch
boi dispatch my-feature.yaml

# 4. Monitor
boi status              # shows hash IDs (e.g. SA7F3)
boi dashboard           # live interactive TUI
boi log SA7F3 -f        # live tail worker output
```

## Spec Format

Specs are YAML files. The daemon reads the `tasks:` array on each iteration to find the next `PENDING` task.

```yaml
title: Spec Title
mode: execute
context: |
  Optional context for why this work is needed.

tasks:
  - id: t-1
    title: First task
    status: PENDING
    spec: |
      What the worker must do. Be concrete: file paths, function names, patterns.
    verify: "test -f output.txt && python3 -m pytest tests/ -x -q"

  - id: t-2
    title: Second task
    status: PENDING
    depends: [t-1]
    spec: |
      Depends on t-1's output.
    verify: "grep 'expected' output.txt"
```

**Task status values:** `PENDING` → `DONE` | `SKIPPED` | `FAILED`

**Fields:**
- `id` — unique task identifier (`t-1`, `t-2`, ...)
- `title` — short description
- `status` — `PENDING` until the worker marks it `DONE`
- `spec` — what the worker must do (multiline string)
- `verify` — shell command that proves the work is done; non-zero exit = task incomplete
- `depends` — optional list of task IDs that must be `DONE` first
- `context_files` — optional list of file paths injected into every worker prompt for this spec (see [Context Injection](#context-injection))

**Rules:**
- Workers update `status: DONE` in the YAML file on success
- Workers add new tasks to the `tasks:` array to self-evolve the spec
- One task per worker iteration; daemon requeues until all tasks are DONE

## Phases

A **phase** is a named worker role defined by a `.phase.toml` file. The daemon loads phases from `~/.boi/phases/` and hot-reloads them when files change.

### Built-in Phases

| Phase | Description | Model (alias) | Timeout |
|-------|-------------|---------------|---------|
| `execute` | Execute tasks from the spec | sonnet | 600s |
| `review` | Code review: correctness, security, spec compliance | sonnet | 300s |
| `critic` | Quality antagonist: challenge assumptions, surface edge cases, verify correctness | sonnet | 300s |
| `decompose` | Decompose a high-level spec into actionable tasks | opus | 600s |
| `evaluate` | Evaluate spec completion and determine next steps | sonnet | 300s |

Model aliases are resolved by the configured runtime. See [Runtime Configuration](#runtime-configuration) below.

### Phase File Schema (`.phase.toml`)

```toml
# Top-level
name = "my-phase"                        # optional; derived from filename if omitted
description = "What this phase does"     # optional
completion_handler = "builtin:execute"   # optional — use built-in routing logic

# Phase metadata
[phase]
level = "task"                           # "spec" | "task"; falls back to name-based derivation if omitted
timeout_minutes = 5                      # optional; overrides [worker].timeout
can_add_tasks = false                    # whether this phase may append tasks to the spec
can_fail_spec = false                    # whether a rejection from this phase marks the spec failed

# Worker configuration
[worker]
prompt_template = "templates/my-prompt.md"  # required for claude/default phases
model = "claude-sonnet-4-6"                  # default: claude-sonnet-4-6
effort = "medium"                            # low | medium | high
timeout = 300                                # seconds; must be > 0
runtime = "claude"                           # "claude" (default) | "deterministic"

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

**`completion_handler`:** Used in two contexts:
- **Claude phases** (default): delegates completion routing to a built-in handler (e.g. `"builtin:execute"`) instead of `approve_signal`/`reject_signal` strings.
- **Deterministic phases** (`[worker] runtime = "deterministic"`): names the builtin to *execute* directly — no Claude spawn. Built-ins: `builtin:commit`, `builtin:merge`, `builtin:cleanup`. The `[completion]` block is ignored for deterministic phases.

### Creating a Custom Phase

1. Write a prompt template at `~/.boi/phases/templates/security-scan-prompt.md`
2. Create the phase file:

```toml
# ~/.boi/phases/security-scan.phase.toml

name = "security-scan"
description = "Run SAST scan and block on high-severity findings"

[worker]
prompt_template = "~/.boi/phases/templates/security-scan-prompt.md"
model = "sonnet"   # alias; resolved by the configured runtime
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

The daemon hot-reloads phase files — no restart needed.

## Pipelines

A **pipeline** is an ordered list of phases a spec passes through. Phases run sequentially; the spec advances on approval.

### Default Pipeline

Configured in `~/.boi/guardrails.toml`:

```toml
[pipeline]
default = ["execute", "review", "critic"]
```

To use a shorter pipeline, edit `guardrails.toml`. For example, execute-only:

```toml
[pipeline]
default = ["execute"]
```

### Pipeline Config Files (for `boi bench`)

`boi bench` accepts named pipeline configs as TOML files:

```toml
[pipeline]
name = "v2"
spec_phases = ["spec-critique", "spec-improve"]   # phases run once on the spec
task_phases = ["execute", "task-verify"]           # phases run per task
post_phases = ["doc-update", "critic", "merge"]    # phases run after all tasks complete
```

Pass them with `--pipeline name:path/to/pipeline.toml` (repeatable for N-way comparisons).

### Pipeline v2 Mode (opt-in)

v2 is a redesigned pipeline with clean phase separation and deterministic steps that skip Claude cold-start. Set `mode: v2` in your spec:

```yaml
title: My Feature
mode: v2

tasks:
  - id: t-1
    title: Implement the thing
    status: PENDING
    spec: |
      Add X to lib/foo.py following the existing pattern.
    verify: "python3 -m pytest tests/test_foo.py -x -q"
```

v2 pipeline layout:

```
Spec-pre  (loop ≤3): spec-critique ↔ spec-improve
Per-task:            execute → review → commit     (commit is deterministic)
Spec-post:           doc-update → critic → merge → cleanup
                                           ^         ^       ^
                                           Claude    det.    det.
```

Deterministic phases (`commit`, `merge`, `cleanup`) run as plain shell operations — no Claude spawn, no cold-start latency. v1 is still the default; v2 is opt-in until A/B benchmarks confirm the speedup. See [docs/pipelines.md](docs/pipelines.md) for a full v1 vs v2 comparison and guidance on when to use each.

## Remote Bench Dispatch (`--remote=fly`)

`boi bench` can run containers on **Fly.io Machines** instead of local Docker:

```sh
boi bench --remote=fly \
  --pipeline smoke:pipelines/smoke.toml \
  --spec tests/bench_specs/simple.yaml \
  --runs 3
```

Each run creates a Fly.io machine, executes the bench inside it, and cleans up. Machines
scale to zero when idle — ~$14–23/month at 900 runs/month, per-second billing.

| Flag | Description |
|------|-------------|
| `--remote fly\|local` | `local` (default) or `fly` |
| `--concurrency N` | Max parallel Fly.io machines (default: 4) |
| `--max-cost N` | Refuse dispatch if estimated cost > $N (default: 10.0) |

Prerequisites: one-time setup in [docs/fly-io-setup.md](docs/fly-io-setup.md).
Architecture and cost model: [docs/remote-dispatch.md](docs/remote-dispatch.md).

## Guardrails

Guardrails define quality gates that run at phase transitions. Configured globally in `~/.boi/guardrails.toml`, overridable per spec.

### `guardrails.toml` Format

```toml
[global]
strictness = "advisory"   # strict | advisory | permissive

[pipeline]
default = ["execute", "review", "critic"]

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
#   SPEC_ID   — queue ID (e.g. SA7F3)

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

## Runtime Configuration

BOI dispatches every LLM phase through a unified `Provider` trait. The registry holds
all known providers; the runner does a single `registry.get(provider_name)` lookup —
no branching on name strings. Providers that fail credential validation at startup are
auto-disabled, causing a loud warning rather than a silent fallback.

Built-in providers: `claude` (always active), `openrouter` (requires `OPENROUTER_API_KEY`),
`codex` (requires `OPENAI_API_KEY`), `deterministic` (builtin handler, no LLM). Use
`boi providers list` to see which are active on your machine. See [docs/providers.md](docs/providers.md)
for the full architecture and how to add a new provider.

### Global Default

Set in `~/.boi/config.json`:

```json
{
  "runtime": { "default": "claude" }
}
```

### Per-Spec Override

Add a `runtime:` field to any spec:

```yaml
runtime: codex
```

Spec-level override takes precedence over the global default.

### Model Mappings

Phase config accepts either full model IDs or aliases (`opus`, `sonnet`, `haiku`). The runtime resolves them:

| Alias | Claude | Codex |
|-------|--------|-------|
| `opus` | claude-opus-4-6 | o3 |
| `sonnet` | claude-sonnet-4-6 | o4-mini |
| `haiku` | claude-haiku-4-5-20251001 | o4-mini |

### Per-Phase Model Overrides

Override the model for any phase globally via `~/.boi/config.yaml`:

```yaml
models:
  spec-review: claude-opus-4-7
  plan-critique: claude-opus-4-7
  execute: claude-sonnet-4-6
  task-verify: claude-haiku-4-5-20251001
```

Keys are phase names. Values are full model IDs or aliases. Config-level overrides take precedence over the `model` field in each phase's `.phase.toml` at runtime.

### Context Injection

Inject files into every worker prompt so workers have access to shared memory, project notes, or other context.

**Global (all specs):** add to `~/.boi/config.yaml`:

```yaml
context:
  always_include:
    - ~/.claude/shared-memory/SHARED.md
    - ~/notes.md
```

**Per-spec:** add `context_files` to any spec YAML:

```yaml
context_files:
  - ~/.claude/shared-memory/SHARED.md
  - docs/architecture.md
```

Both lists are merged at dispatch time. File contents are read once and stored in the DB. Missing files are silently skipped. Total context is capped at 50,000 characters to prevent prompt bloat. The combined content is available in prompt templates as `{{PROJECT_CONTEXT}}`.

### CLI Check

`boi doctor` validates the configured runtime's CLI is installed. If the global default is `codex`, it checks for `codex` in PATH instead of `claude`.

## CLI Reference

```
boi dispatch <file.yaml> [options]        Submit a spec to the queue             (alias: d, dis)
boi status [--watch] [--json]             Show queue and worker status            (alias: s, st)
boi dashboard                             Interactive TUI dashboard (keyboard-driven) (alias: dash)
boi log <queue-id> [--full] [-f|--follow] Tail worker output for a spec          (alias: l)
boi cancel <queue-id>                     Cancel a running or queued spec         (alias: can)
boi stop                                  Stop daemon and all workers
boi install [--workers N]                 One-time setup (run outside Claude Code)
boi resume <queue-id> | --all            Resume failed or canceled specs
boi cleanup                               Kill orphaned worker processes
boi workers [--json]                      Show worktree health                    (alias: w)
boi telemetry <queue-id> [--json]        Per-iteration metrics                   (alias: tel)
boi phases <queue-id> [--full]           Phase invocations table (runtime, model, duration, cost) (alias: ph)
boi outputs <queue-id>                    Show files produced by a completed spec  (alias: out)
boi outputs --recent                      Show last 10 completed specs with output counts
boi spec <queue-id> [add|skip|next|block|edit|deps]                               (alias: sp)
boi critic status | run | enable | disable | checks
boi dep add|remove|set|clear|show|viz|check
boi project create|list|status|context|delete
boi providers list                        List registered and disabled runtime providers (alias: prov)
boi doctor                                Health check                            (alias: doc)
boi config [key] [value]                  Show or set config values               (alias: cfg)
boi bench --pipeline name:path [--pipeline ...] --spec FILE | --battery DIR [--runs N] [--remote fly|local] [--concurrency N]  Benchmark N pipelines (alias: b)
boi bench --phase <name> --spec FILE [--runs N]  Benchmark a single phase in isolation
boi version                               Print version                           (alias: v, ver)
```

**`dispatch` options:**

| Flag | Description |
|------|-------------|
| `--priority N` | Lower = higher priority (default: 100) |
| `--max-iter N` | Max iterations before marking failed (default: 30) |
| `--mode MODE` | `execute` \| `challenge` \| `discover` \| `generate` (aliases: e/c/d/g) |
| `--worktree-isolate` | Dedicated git worktree and branch for this spec |
| `--after SA7F3,TB2E1` | Wait for listed specs to complete before starting |
| `--project NAME` | Associate with a project (injects project context) |

## Output Preservation

BOI automatically preserves the work product of every completed spec so outputs are never lost when the worktree is cleaned up.

### Where outputs go

```
~/.boi/outputs/<queue-id>/
  ├── spec.yaml            # final spec file with all tasks DONE
  ├── manifest.json        # list of all files created/modified, with paths and sizes
  ├── files/               # copies of created/modified files (relative paths preserved)
  │   └── path/to/file.py
  └── verify-outputs.log   # stdout/stderr from all verify commands
```

Files written **outside** the worktree (e.g. to `~/.hex/`, permanent config paths) are already in persistent locations — they are listed in `manifest.json` under `action: outside_worktree` but not copied.

### Viewing outputs

```bash
boi outputs SA7F3
```

```
Spec: pulse-message-persistence (SA7F3)
Completed: 2026-04-25T20:15:00Z
Mode: execute

Outputs (3 files):
  path/to/server.py          (modified, 41KB)
  ~/.hex/audit/events.jsonl  (outside_worktree, 0KB)
  docs/context-eval.md       (created, 2.1KB)

Files preserved at: ~/.boi/outputs/SA7F3/files/
```

```bash
boi outputs --recent    # last 10 completed specs
```

### Failure safety

If output collection fails (disk full, permission error), BOI logs the error and **does not delete the worktree**. The worktree is only removed after outputs have been successfully preserved.

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
      (runtime)   (runtime)   (runtime)
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

The **runtime** is the CLI agent backend. Default is `claude` (`claude -p`); `codex` (`codex exec`) is also supported.

**Key behaviors:**

- **Hot-reload:** The daemon calls `_reload_phases_if_changed()` each poll cycle. Edit a `.phase.toml` file and the daemon picks it up without restart.
- **Pipeline advancement:** `_advance_pipeline()` reads `guardrails.toml` to find the configured pipeline, finds the current phase's index, and requeues the spec for `pipeline[index+1]`. If current phase is not in the pipeline or is the last entry, the spec is marked complete.
- **Phase routing:** `_dispatch_phase_completion()` checks the `PhaseConfig` for the current phase. If `completion_handler` is set to a builtin (e.g. `builtin:execute`), the corresponding builtin handler runs. Otherwise, the phase's `approve_signal` / `reject_signal` strings are matched against the worker's output to determine routing.
- **State directory:** `~/.boi/` holds the queue database, logs, phase files, guardrails config, and gates.
- **Worker isolation:** Each worker runs in a git worktree under `~/.boi/worktrees/`. Isolated specs (`--worktree-isolate`) get their own branch; shared specs use a rotating pool of worker worktrees.
- **Consecutive failure protection:** SIGTERM (exit 143) and SIGKILL (exit 137) do not count as consecutive failures. Workers killed externally are requeued, not failed.
