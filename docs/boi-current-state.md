# BOI Current State — Python Architecture Audit

> Produced by q-917 iteration 1 for the BOI Rust Migration spec.
> Date: 2026-04-27

---

## Overview

BOI (Beginning of Infinity) is a Python/Bash agent task orchestration daemon. It dispatches iterative specs to Claude Code (or other) workers, tracks state via SQLite, and provides a full monitoring and management CLI. The codebase spans ~20K source lines across one Bash CLI (`boi.sh`, 4929 lines), one daemon (`daemon.py`, 3028 lines), one worker script (`worker.py`, 1782 lines), and 20+ Python library modules.

---

## 1. CLI Commands

All commands enter through `boi.sh`, which bootstraps Python and delegates to library functions.

| Command | Description | Python Target |
|---------|-------------|---------------|
| `boi dispatch --spec FILE` | Enqueue a spec; start daemon if not running | `lib.cli_ops.dispatch()` |
| `boi queue [--json]` | List all queued specs sorted by status | `lib.cli_ops.list_queue()` |
| `boi status [--watch] [--json]` | Real-time worker + queue status | `lib.status.build_queue_status()` |
| `boi log <id> [--full] [--failures]` | Show or tail iteration logs | `lib.cli_ops.show_log()` |
| `boi cancel <id>` | Mark spec as `canceled` | `lib.queue.cancel_spec()` |
| `boi resume <id>` | Reset failed/canceled → `queued` | `lib.queue.resume_spec()` |
| `boi workers [--json]` | List worktrees and availability | `lib.cli_ops.show_workers()` |
| `boi telemetry <id> [--json]` | Per-iteration breakdown (tasks, cost, duration) | `lib.telemetry.get_telemetry()` |
| `boi dashboard` | Interactive TUI (crossterm): running/queued specs, 2s refresh, keyboard-driven | `src/cli/dashboard.rs` |
| `boi purge [--all] [--dry-run]` | Remove completed/failed/canceled specs | `lib.cli_ops.purge()` |
| `boi stop` | Kill daemon and all workers | `lib.daemon_lock.stop_daemon()` |
| `boi install [--workers N]` | One-time setup: worktrees, config, DB | `install.sh` |
| `boi doctor` | Prerequisite check (Claude, git, tmux, Python 3.12) | `lib.cli_ops.doctor()` |
| `boi upgrade` | Pull latest BOI version | `install.sh upgrade` |
| `boi spec <id> [add\|skip\|next\|block\|edit]` | Live spec task management | `lib.spec_editor.*` |
| `boi project <create\|list\|status\|context\|delete>` | Project lifecycle management | `lib.project.*` |
| `boi do "..."` | Natural language → BOI CLI command | `lib.do.interpret()` |
| `boi config [get\|set]` | Show/change global config | `lib.cli_ops.config_ops()` |
| `boi critic [status\|run\|disable\|enable\|checks]` | Critic review system | `lib.critic.*` |
| `boi review <id>` | Review EXPERIMENT_PROPOSED tasks | `lib.review.*` |
| `boi prune-orphans [--dry-run\|--apply] [--max-idle-secs N] [--json]` | Identify (and optionally kill) orphaned worker processes | `src/cli/prune.rs` |

### Dispatch Flags

```
--spec FILE           Path to spec file (required)
--priority N          Queue priority; lower = higher priority (default: 100)
--max-iter N          Max iterations before marking failed (default: 30)
--mode MODE           execute|challenge|discover|generate (aliases e/c/d/g)
--worktree PATH       Pin to a specific worktree
--no-critic           Skip critic validation on completion
--timeout N           Worker timeout in seconds (default: 600)
--project NAME        Associate with a project (injects context.md)
--experiment-budget N Override default experiment budget for mode
--push                Push git changes after completion
--commit-scope SCOPE  Git commit scope string
```

---

## 2. Data Structures

### 2a. Spec File Format

BOI supports two spec formats: **Markdown** (primary) and **YAML** (alternative, auto-detected).

**Markdown format:**
```markdown
# Spec Title

**Initiative:** init-123
**Mode:** execute|challenge|discover|generate
**Runtime:** claude|codex|hermes|ollama
**Workspace:** /path/to/target/repo
**Push:** true|false
**Commit-Scope:** feat
**Max Iterations:** 30
**Timeout Seconds:** 600
**Emergency:** true   (bypass initiative requirement)

## Outcomes

- **Description:** REST API functional
  **Verify:** `curl http://localhost:3000/api/health`

## Tasks

### t-1: Task title
PENDING

**Spec:** What to do, precisely.
**Files:** api/schema.json, api/routes.py
**Verify:** python3 -m pytest tests/
**Blocked by:** (none)

### t-2: Follow-up task
DONE

**Spec:** ...
**Verify:** ...

## Dependencies

t-1: (none)
t-2: t-1

## Error Log

### [iter-1] Brief description
Details of what failed and what was tried.
```

**YAML format:**
```yaml
title: My Spec
initiative: init-123
mode: execute
runtime: claude
workspace: /path/to/repo

outcomes:
  - description: REST API functional
    verify: curl http://localhost:3000/api/health

tasks:
  - id: t-1
    title: Design API schema
    status: PENDING
    spec: |
      Design a JSON schema for users...
    verify: python3 schema_check.py
    depends: []
  - id: t-2
    title: Implement endpoints
    status: DONE
    spec: |
      Implement REST endpoints...
    verify: pytest tests/
    depends: [t-1]
```

### 2b. Task Status Values

| Status | Meaning |
|--------|---------|
| `PENDING` | Awaiting execution |
| `DONE` | Successfully completed |
| `FAILED` | Error occurred |
| `SKIPPED` | Intentionally skipped by worker |
| `EXPERIMENT_PROPOSED` | Worker proposed alternative (needs human review) |
| `SUPERSEDED by t-N` | Replaced by another task; excluded from totals |

### 2c. Queue Entry (SQLite `specs` table)

```sql
CREATE TABLE specs (
    id TEXT PRIMARY KEY,                   -- q-001, q-002, ...
    spec_path TEXT NOT NULL,               -- ~/.boi/queue/q-001.spec.md (queue copy)
    original_spec_path TEXT,               -- User's original file path
    worktree TEXT,                         -- Optional pinned worktree
    priority INTEGER NOT NULL DEFAULT 100,
    status TEXT NOT NULL,                  -- queued|running|completed|failed|canceled|needs_review|requeued|assigning
    phase TEXT DEFAULT 'execute',          -- execute|task-verify|evaluate|decompose|review|plan-critique|code-review
    submitted_at TEXT NOT NULL,            -- ISO-8601
    first_running_at TEXT,
    last_iteration_at TEXT,
    last_worker TEXT,                      -- Worker ID (w-1, w-2, ...)
    iteration INTEGER NOT NULL DEFAULT 0,
    max_iterations INTEGER NOT NULL DEFAULT 30,
    consecutive_failures INTEGER DEFAULT 0,
    cooldown_until TEXT,                   -- Do not retry before this timestamp
    tasks_done INTEGER DEFAULT 0,
    tasks_total INTEGER DEFAULT 0,
    sync_back INTEGER DEFAULT 1,           -- Whether to sync worktree changes back
    project TEXT,                          -- Associated project name
    initial_task_ids TEXT,                 -- JSON array [t-1, t-2, ...]
    worker_timeout_seconds INTEGER,        -- Timeout per iteration
    failure_reason TEXT,
    needs_review_since TEXT,               -- When status became needs_review
    assigning_at TEXT,                     -- Lock timestamp preventing race assignment
    critic_passes INTEGER DEFAULT 0,       -- How many critic passes have run
    pre_iteration_tasks TEXT,              -- JSON object {t-1: PENDING, t-2: DONE}
    experiment_tasks TEXT,                 -- JSON array of EXPERIMENT_PROPOSED task IDs
    max_experiment_invocations INTEGER DEFAULT 0,
    experiment_invocations_used INTEGER DEFAULT 0,
    push TEXT DEFAULT 'false',
    commit_scope TEXT DEFAULT ''
);
```

### 2d. Worker State (SQLite `workers` table)

```sql
CREATE TABLE workers (
    id TEXT PRIMARY KEY,           -- w-1, w-2, w-3 (slot IDs)
    worktree_path TEXT NOT NULL,   -- ~/.boi/worktrees/w-1
    current_spec_id TEXT,          -- Active spec being executed
    current_pid INTEGER,           -- Process ID of tmux session or subprocess
    start_time TEXT,               -- When worker started current spec
    current_phase TEXT,            -- Active phase name
    current_task_id TEXT           -- For parallel task dispatch
);
```

### 2e. Iteration Metadata (per-file JSON)

Written to `~/.boi/queue/{spec_id}.iteration-N.json` after each iteration:

```json
{
  "queue_id": "q-001",
  "iteration": 2,
  "exit_code": 0,
  "duration_seconds": 87,
  "started_at": "2026-04-27T08:00:00Z",
  "pre_counts": {"pending": 3, "done": 1, "skipped": 0, "total": 4},
  "post_counts": {"pending": 1, "done": 3, "skipped": 0, "total": 4},
  "tasks_completed": 2,
  "tasks_added": 0,
  "tasks_skipped": 0,
  "model": "claude-sonnet-4-6",
  "estimated_input_tokens": 5200,
  "estimated_output_tokens": 2100,
  "estimated_cost_usd": 0.0473
}
```

### 2f. Python Task Dataclass (`lib/spec_parser.py`)

```python
@dataclass
class BoiTask:
    id: str                   # t-1, t-2, ...
    title: str
    status: str               # PENDING|DONE|FAILED|SKIPPED|EXPERIMENT_PROPOSED|SUPERSEDED
    body: str                 # Full body text after status line
    superseded_by: str        # For SUPERSEDED tasks: t-N reference
    experiment: str           # #### Experiment: section content
    discovery: str            # #### Discovery: section content
    blocked_by: list[str]     # Dependency IDs [t-1, t-2]

@dataclass
class Outcome:
    description: str
    verify: str               # Shell command string
    status: str               # PENDING|PASS|FAIL
```

---

## 3. External Dependencies

### 3a. Critical Runtime Tools

| Tool | Invocation | Purpose | Fallback |
|------|-----------|---------|----------|
| `claude` | `claude -p "$(cat prompt.md)" --model M --effort E --dangerously-skip-permissions --output-format stream-json --verbose --strict-mcp-config` | Primary worker runtime | Fails hard; required for default runtime |
| `tmux` | `tmux -L boi new-session -d -s boi-q-001 bash run.sh` | Process isolation for workers | `BOI_NO_TMUX=1` → direct `subprocess.run()` |
| `git` | `git -C <worktree> diff`, `git status`, `git add`, `git commit` | Workspace boundary checking, output tracking | Graceful skip with warning |
| `python3` | `python3 daemon.py`, `python3 worker.py` | Core runtime | Hard failure if missing |

### 3b. Optional Runtime Tools

| Tool | Invocation | Purpose | Fallback |
|------|-----------|---------|----------|
| `codex` | `codex exec --model M --dangerously-bypass-approvals-and-sandbox < prompt.md` | Alternative worker runtime | Skip if not installed |
| `hermes` | `hermes chat -q "$(cat prompt.md)" --model M --quiet --yolo --max-turns 50` | Alternative worker runtime | Skip if not installed |
| `ollama` | Via `lib/ollama_react_worker.py` | Local inference runtime | Skip if not installed |
| `playwright` / `pytest` | `python3 -m pytest playwright` | E2E verification phase | Gracefully skipped |

### 3c. hex-events Integration (Optional, Fire-and-Forget)

```python
# lib/cli_ops._emit_dispatched_event()
subprocess.Popen(
    ['python3', '~/.hex-events/hex_emit.py', 'boi.spec.dispatched'],
    stdin=subprocess.PIPE,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL
)
```

Events emitted: `boi.spec.dispatched`, `boi.spec.completed`, `boi.spec.failed`

All hex-events calls are non-blocking (Popen, no wait). Silently ignored if `hex_emit.py` is missing. **This is the primary hex coupling to eliminate in the Rust port.**

---

## 4. Hardcoded Paths & Host Assumptions

| Path | Where Used | Purpose |
|------|-----------|---------|
| `~/.boi/` | All modules | State root |
| `~/.boi/boi.db` | `lib/db.py` | SQLite queue database |
| `~/.boi/config.json` | `boi.sh`, `lib/cli_ops.py` | Global configuration |
| `~/.boi/queue/` | `daemon.py`, `worker.py` | Spec copies, prompts, run scripts |
| `~/.boi/logs/` | `worker.py` | Per-iteration agent stdout/stderr |
| `~/.boi/outputs/` | `worker.py` | Spec output file collection |
| `~/.boi/events/` | `lib/event_log.py` | Append-only event sequence files |
| `~/.boi/worktrees/w-N/` | `lib/task_worktree.py` | Git worktrees for worker isolation |
| `~/.boi/projects/{name}/` | `lib/project.py` | Project context.md, research.md |
| `~/.boi/hooks/on-complete.sh` | `lib/hooks.py` | Shell hook: fires after any completion |
| `~/.boi/hooks/on-fail.sh` | `lib/hooks.py` | Shell hook: fires on failure |
| `~/.boi/src/` | `install.sh` | Source code location |
| `~/.hex-events/hex_emit.py` | `lib/cli_ops.py`, `daemon_ops.py` | Optional hex bridge (hardcoded) |
| `~/hex` | `worker.py` | Default AGENT_DIR for E2E guard |
| `/opt/homebrew/bin/python3.12` | `boi.sh` | Preferred Python binary (macOS) |

**Config overrides (partial):**
- `config.json` `context_root` → overrides default context directory for `--add-dir`
- `config.json` `runtime.default` → overrides default runtime (claude/codex/hermes)
- `config.json` `workers` → number of worker slots

**Platform assumptions:**
- macOS or Linux (fcntl-based locking; not Windows-compatible)
- `tmux` version ≥ 2.0 on PATH
- `python3.12` or `python3` available
- SQLite 3.x (bundled with Python stdlib)
- UTF-8 locale

---

## 5. File Formats

### 5a. Spec Files

Stored as `.md` (Markdown) or `.yaml` (YAML) in `~/.boi/queue/` after dispatch. The spec parser (`lib/spec_parser.py`) auto-detects format. Both formats support the same fields.

### 5b. Queue State

Stored in SQLite (`~/.boi/boi.db`) with WAL journal mode. No JSON queue files remain at runtime (deprecated). Schema maintained by `lib/db_migrate.py` with version-gated migrations.

### 5c. Log Files

- **Path**: `~/.boi/logs/{spec_id}-iter-{N}.log`
- **Format**: Raw unstructured text (agent stdout + stderr, interleaved)
- **Rotation**: Not automated; `boi purge` removes on cleanup

### 5d. Event Log

- **Directory**: `~/.boi/events/`
- **Files**: `event-00001.json`, `event-00002.json`, ... (incrementing sequence)
- **Format**: One JSON object per file

```json
{
  "seq": 42,
  "type": "spec_completed",
  "queue_id": "q-001",
  "timestamp": "2026-04-27T08:00:00+00:00",
  "spec_path": "~/.boi/queue/q-001.spec.md",
  "iteration": 4,
  "tasks_done": 5,
  "tasks_added": 1,
  "tasks_total": 5
}
```

Event types: `spec_queued`, `spec_completed`, `spec_failed`, `needs_review`, `requeued`

### 5e. Telemetry

- **Path**: `~/.boi/queue/{spec_id}.telemetry.json`
- **Format**: Aggregated stats across all iterations

```json
{
  "queue_id": "q-001",
  "total_iterations": 4,
  "total_duration_seconds": 340,
  "total_cost_usd": 0.187,
  "tasks_completed_per_iteration": [2, 1, 1, 1],
  "durations_per_iteration": [85, 90, 87, 78],
  "models_used": ["claude-sonnet-4-6"],
  "estimated_input_tokens_per_iteration": [5000, 5200, 5400, 4800],
  "estimated_output_tokens_per_iteration": [2000, 2100, 2200, 1900]
}
```

### 5f. Per-Iteration Queue Artifacts

For each spec `{spec_id}` and iteration `N`:

| File | Contents |
|------|---------|
| `{spec_id}.spec.md` | Copy of spec at dispatch time |
| `{spec_id}.prompt.md` | Generated worker prompt |
| `{spec_id}.run.sh` | Generated bash run script |
| `{spec_id}.exit` | Integer exit code written by run script |
| `{spec_id}.pid` | Worker PID (from tmux `pane_pid`) |
| `{spec_id}.iteration-N.json` | Metadata for iteration N |
| `{spec_id}.telemetry.json` | Aggregated telemetry |
| `{spec_id}.changed-files` | Manifest of modified files in target repo |
| `{spec_id}.critic-prompt.md` | Pre-generated critic phase prompt |

### 5g. Output Collection

- **Directory**: `~/.boi/outputs/{spec_id}/`
- **Contents**:
  - `spec.md` — Final spec file (all tasks DONE)
  - `files/` — Modified/new files copied from worktree
  - `manifest.json` — List of collected files with metadata

---

## 6. Worker Lifecycle

### Spawn Path

```
boi dispatch spec.md
  → lib.cli_ops.dispatch()
     → copy spec to ~/.boi/queue/q-NNN.spec.md
     → INSERT INTO specs (status=queued)
     → emit boi.spec.dispatched to hex-events (optional)
     → if daemon not running: spawn daemon (nohup python3 daemon.py &)

daemon.py main loop (polls every ~5s)
  → lib.daemon_ops.try_dequeue()
     → SELECT next eligible spec (status=queued, not in cooldown, not blocked)
     → UPDATE spec SET status=assigning, assigning_at=NOW
     → find available worker slot
     → UPDATE worker SET current_spec_id=...
     → UPDATE spec SET status=running
     → spawn subprocess: python3 worker.py {spec_id} {worktree} {spec_path} {iter} ...
```

### Worker Execution (`worker.py`)

```
worker.py {spec_id} {worktree} {spec_path} {iter} [--phase P] [--timeout T] [--mode M]
  1. Load spec, count pre-iteration task statuses
  2. Snapshot git status of target repo (workspace guard)
  3. Load phase config (.phase.toml)
  4. Generate prompt from templates/worker-prompt.md + mode fragment
  5. Write ~/.boi/queue/{spec_id}.prompt.md
  6. Generate bash run script → ~/.boi/queue/{spec_id}.run.sh
  7. Launch:
     - Default: tmux -L boi new-session -d -s boi-{spec_id} bash run.sh
     - BOI_NO_TMUX=1: subprocess.run(['bash', 'run.sh'], timeout=T)
  8. Monitor:
     - tmux: poll tmux has-session every 5s until session dies
     - direct: subprocess.run() blocks
  9. On timeout → SIGTERM to tmux session → raise TimeoutError
 10. Read exit code from {spec_id}.exit
 11. Count post-iteration task statuses
 12. Write {spec_id}.iteration-N.json
 13. collect_outputs() → copy modified files to ~/.boi/outputs/{spec_id}/files/
 14. Run outcome verify commands (shell, timeout 60s each)
     - If any FAIL → reset last DONE task to PENDING
 15. E2E phase (optional) → Playwright verify.py if web artifacts detected
     - If FAIL → reset last DONE task to PENDING
 16. Return exit code to daemon
```

### Run Script (Embedded in `boi.sh`, generated per-iteration)

```bash
#!/bin/bash
set -uo pipefail
_START_TIME=$(date +%s)
cd "$_WORKTREE_PATH"

# Execute runtime
claude -p "$(cat q-001.prompt.md)" \
  --model claude-sonnet-4-6 \
  --effort medium \
  --dangerously-skip-permissions \
  --add-dir /path/to/context \
  --output-format stream-json \
  --verbose \
  --strict-mcp-config \
  > ~/.boi/logs/q-001-iter-2.log 2>&1

_AGENT_EXIT=$?
_END_TIME=$(date +%s)

# Count tasks via Python
python3 - <<'PYEOF'
import sys; sys.path.insert(0, '~/.boi/src')
from lib.spec_parser import count_boi_tasks
counts = count_boi_tasks('q-001.spec.md')
# Write iteration metadata JSON
PYEOF

echo "$_AGENT_EXIT" > ~/.boi/queue/q-001.exit
```

### Post-Iteration: Daemon State Update

```
daemon reads exit code, iteration JSON
  → UPDATE spec SET:
       iteration = N+1,
       tasks_done, tasks_total,
       last_iteration_at = NOW,
       consecutive_failures = 0 (or +1),
       status = completed|requeued|failed|needs_review
  → emit boi.spec.completed / boi.spec.failed to hex-events (optional)
  → fire ~/.boi/hooks/on-complete.sh or on-fail.sh
  → clear worker slot (UPDATE worker SET current_spec_id=NULL)
```

### State Transitions

```
queued ──→ assigning ──→ running ──→ requeued (PENDING tasks remain)
                                  ──→ completed (all DONE + outcomes pass)
                                  ──→ needs_review (EXPERIMENT_PROPOSED present)
                                  ──→ failed (max iterations or max failures)
                                  ──→ canceled (user action)
```

---

## 7. Error Handling

### Timeout

- **Config**: `worker_timeout_seconds` per spec (default 600s)
- **Enforcement**: Worker polls elapsed time; sends SIGTERM to tmux
- **Result**: Exit code 124, status → `requeued`, cooldown applied (60s default)

### Consecutive Failures

- **Tracking**: `consecutive_failures` column in SQLite
- **Threshold**: 5 (`MAX_CONSECUTIVE_FAILURES`)
- **On exceed**: Status → `failed`, `failure_reason` written
- **Reset**: Reset to 0 on any successful iteration

### Crash Recovery

- **Scenario**: Daemon dies while a spec is `status=running`
- **Detection**: On daemon startup, `recover_running_specs()` queries for specs stuck in `running` state
- **Action**: Reset to `requeued` with `cooldown_until = NOW + 60s`

### Outcome Verification Failure

- **Trigger**: Any spec-level outcome's verify command exits non-zero
- **Action**: Find last DONE task, reset to PENDING; worker re-dispatched

### E2E Verification Failure

- **Trigger**: Playwright verify.py exits non-zero
- **Action**: Same as outcome failure: reset last DONE task to PENDING

### Hex-events Errors

- All hex-events subprocess calls are fire-and-forget (`Popen`, no wait)
- Failure is silently ignored; never propagates to spec state

### Worker-Level Error Codes

| Exit Code | Meaning |
|-----------|---------|
| 0 | Success |
| 2 | Spec file not found or parse error |
| 124 | Timeout |
| Other non-zero | Agent runtime failure |

---

## 8. Hooks System

### Shell Hooks (Current Implementation)

Located at `~/.boi/hooks/` (optional shell scripts):

- **`on-complete.sh <queue_id> <spec_path>`** — Fires after any spec completion (success or failure)
- **`on-fail.sh <queue_id> <spec_path>`** — Fires only on failure

**Invocation** (`lib/hooks.py`):
```python
subprocess.run(
    [str(hook_path), queue_id, spec_path],
    timeout=30,
    capture_output=True
)
```
- Best-effort: failure logged but never blocks
- Timeout: 30 seconds hard limit
- Blocking: Yes (subprocess.run, not Popen)

### Hex-events Bridge (Current)

Hardcoded in `lib/cli_ops._emit_dispatched_event()` and `daemon_ops.py`:
```python
# Non-blocking fire-and-forget
proc = subprocess.Popen(
    ['python3', os.path.expanduser('~/.hex-events/hex_emit.py'), event_name],
    stdin=subprocess.PIPE, stdout=DEVNULL, stderr=DEVNULL
)
proc.stdin.write(json.dumps(payload).encode())
proc.stdin.close()
```

**Payload structure:**
```json
{
  "spec_id": "q-001",
  "source": "cli",
  "spec_file": "~/.boi/queue/q-001.spec.md",
  "emergency": false,
  "iteration": 0,
  "tasks_done": 0,
  "tasks_total": 3
}
```

**Hook lifecycle points currently covered:**
- `boi.spec.dispatched` (in `cli_ops.py`)
- `boi.spec.completed` (in `daemon_ops.py`)
- `boi.spec.failed` (in `daemon_ops.py`)

**Hook lifecycle points NOT currently covered (gaps for Rust port to fill):**
- `on_worker_start` (worker begins a spec)
- `on_task_start` / `on_task_complete` / `on_task_fail` (per-task granularity)
- `on_cancel` (manual cancel)
- `on_stall` (no progress for N minutes)

---

## 9. Runtime Abstraction (`lib/runtime.py`)

BOI has a pluggable runtime system for worker execution. The `Runtime` abstract base class defines `build_exec_cmd()`. Resolution order:

1. Spec header `**Runtime:**`
2. `config.json` `runtime.default`
3. Default: `"claude"`

### ClaudeRuntime

```
claude -p "$(cat prompt.md)"
  --model claude-sonnet-4-6
  --effort medium
  --dangerously-skip-permissions
  --add-dir /path/to/context
  --output-format stream-json
  --verbose
  --strict-mcp-config
```

Model aliases: `opus` → `claude-opus-4-6`, `sonnet` → `claude-sonnet-4-6`, `haiku` → `claude-haiku-4-5-20251001`

### CodexRuntime

```
codex exec --model o4-mini --dangerously-bypass-approvals-and-sandbox < prompt.md
```

### HermesRuntime

```
hermes chat -q "$(cat prompt.md)" --model anthropic/claude-sonnet-4-6 --quiet --yolo --max-turns 50
```

### OllamaRuntime

Local inference via `lib/ollama_react_worker.py` (Python ReAct loop talking to local Ollama server).

---

## 10. Multi-Phase Pipeline

Specs can be routed through multiple execution phases. Phases are TOML-configured in `lib/phases/`:

| Phase | Description |
|-------|-------------|
| `execute` | Standard task execution |
| `task-verify` | Critic review (detect EXPERIMENT_PROPOSED) |
| `plan-critique` | Code review before execution |
| `code-review` | Post-execution code review |
| `evaluate` | Assess outcomes against spec |
| `decompose` | Break down generate-mode specs into tasks |
| `review` | Human review of changes |

Each phase defines: model, effort, timeout, approve/reject signals, next-phase transitions.

---

## 11. Dependency DAG

`lib/dag.py` provides pure-function DAG operations for task ordering:

- `topological_sort(tasks)` — Kahn's algorithm
- `find_assignable_tasks(tasks, in_progress)` — Runnable tasks (no pending deps)
- `critical_path(tasks)` — Longest chain to completion
- `downstream_count(task_id)` — Count of transitively-dependent tasks

Spec-level (cross-spec) dependencies stored in SQLite table `spec_dependencies(spec_id, blocks_on)`. Circular detection at dispatch time.

---

## 12. Locking & Concurrency

### File Lock (`lib/locking.py`)

```python
# fcntl.flock on ~/.boi/queue/.lock
# All queue mutations acquire this exclusive lock
```

### SQLite WAL Mode

```python
conn.execute("PRAGMA journal_mode=WAL")
conn.execute("PRAGMA wal_autocheckpoint=10000")
conn.execute("PRAGMA foreign_keys=ON")
```

Concurrent readers (e.g. `boi status`) can read while daemon writes without blocking.

### Threading Lock

`lib/db.py` wraps all mutations in `threading.Lock()` for thread-safety within the daemon process.

---

## 13. Configuration File (`~/.boi/config.json`)

```json
{
  "workers": 3,
  "runtime": {
    "default": "claude"
  },
  "context_root": "/path/to/shared/context",
  "workspace_header_enabled": true,
  "critic": {
    "enabled": true,
    "max_passes": 2
  },
  "experiment_budgets": {
    "execute": 0,
    "challenge": 2,
    "discover": 3,
    "generate": 5
  }
}
```

---

## 14. Workspace Guard

`lib/workspace_guard.py` prevents workers from corrupting the main repo:

1. **Before run**: Snapshot `git status` of target repo
2. **After run**: Re-snapshot, compute diff
3. **Leak detection**: Files modified outside target repo boundary are flagged
4. **Spec header injection** (optional): Prepends guard header to specs:

```
> **WORKSPACE GUARD** — Format: `### t-N: Title` then `PENDING`/`DONE` on its own line.
> Tasks: 2/5 done. Next PENDING: t-3.
> Do NOT alter DONE tasks. Do NOT add prose between headings and status lines. Do NOT duplicate task sections.
```

---

## 15. Key Gaps & Migration Notes

The following aspects are important for the Rust port:

1. **hex-events coupling is NOT via the hooks system** — it is hardcoded in `lib/cli_ops.py` and `daemon_ops.py`. The hook scripts at `~/.boi/hooks/` are a separate, older mechanism covering only `on-complete` and `on-fail`. The Rust port must unify these into the single configurable hook interface.

2. **Only 3 of 9 planned hook lifecycle points exist** — `on_dispatch`, `on_complete`, `on_fail` fire today. `on_worker_start`, `on_task_start`, `on_task_complete`, `on_task_fail`, `on_cancel`, `on_stall` do not.

3. **No per-task telemetry** — The current system tracks tasks-done counts but not per-task timing or cost. _(Resolved in Rust port: `PhaseInvocation` struct + `phase_runs` table now captures runtime, model, tokens, cost, duration, and exit status for every phase invocation. Events emitted to `~/.hex/audit/boi-phase-runs.jsonl` and daemon stderr.)_

4. **Queue IDs are sequential strings** (`q-NNN`) — The Rust port will redesign to `SNNNNNN` (specs) and `TNNNNNN` (tasks).

5. **SQLite is the right persistence layer** — It handles concurrency well with WAL mode. The Rust port should continue with SQLite via `rusqlite`.

6. **tmux is the process model** — Workers run in tmux sessions for process isolation and observability. The Rust port should keep this model but the daemon can use `tokio::process` for spawning.

7. **The spec parser is heavily Markdown-centric** — The YAML format exists but is secondary. The Rust port's `serde_yaml` approach should support both.

8. **Python 3.12 on macOS is path-assumed** — `/opt/homebrew/bin/python3.12` hardcoded in `boi.sh`. Rust binary eliminates this assumption entirely.
