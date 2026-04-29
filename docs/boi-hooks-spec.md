# BOI Hook Interface Specification

> Produced by q-917 iteration 3.
> Informed by: docs/boi-current-state.md (t-1 audit) and docs/boi-rust-architecture.md (t-2 design).
> Date: 2026-04-27

---

## Overview

The BOI hook interface is the primary extensibility surface of the Rust port. Hooks replace two separate mechanisms that exist in the Python codebase:

1. **Hardcoded hex-events calls** — `cli_ops._emit_dispatched_event()` and `daemon_ops.py` directly invoke `~/.hex-events/hex_emit.py` with no configuration.
2. **Shell hook scripts** — `~/.boi/hooks/on-complete.sh` and `on-fail.sh` are bare shell scripts invoked with `queue_id spec_path` as positional args.

In the Rust port, both are replaced by a single configurable hook system. BOI ships with a built-in default hook config (`hooks/default.yaml`, embedded at compile time) that emits hex-events for the four main lifecycle points. Users can override by creating `~/.boi/hooks.yaml`. BOI fires hooks at nine lifecycle points, sending structured JSON on stdin. Zero hex references in BOI source code.

---

## Configuration Format

Hooks are configured in `~/.boi/hooks.yaml`. BOI loads that file if it exists; otherwise it falls back to the built-in default (`hooks/default.yaml`, compiled into the binary). Each hook entry is optional; if absent, that lifecycle point fires silently (no-op).

```yaml
# ~/.boi/hooks.yaml

hooks:
  on_dispatch:
    command: "python3 ~/.hex-events/hex_emit.py boi.spec.dispatched"
    blocking: false
    timeout: 10

  on_worker_start:
    command: "python3 ~/.hex-events/hex_emit.py boi.worker.start"
    blocking: false
    timeout: 10

  on_task_start:
    command: "python3 ~/.hex-events/hex_emit.py boi.task.start"
    blocking: false
    timeout: 10

  on_task_complete:
    command: "python3 ~/.hex-events/hex_emit.py boi.task.completed"
    blocking: false
    timeout: 10

  on_task_fail:
    command: "python3 ~/.hex-events/hex_emit.py boi.task.failed"
    blocking: false
    timeout: 10

  on_complete:
    command: "python3 ~/.hex-events/hex_emit.py boi.spec.completed"
    blocking: false
    timeout: 10

  on_fail:
    command: "python3 ~/.hex-events/hex_emit.py boi.spec.failed"
    blocking: false
    timeout: 10

  on_cancel:
    command: "python3 ~/.hex-events/hex_emit.py boi.spec.cancelled"
    blocking: false
    timeout: 10

  on_stall:
    command: "python3 ~/.hex-events/hex_emit.py boi.spec.stalled"
    blocking: false
    timeout: 10
```

### Hook Config Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `command` | string | — | Shell command executed via `sh -c`. Required. Supports `~` expansion. |
| `blocking` | bool | `false` | If true, BOI waits for the command to exit (up to `timeout` seconds) before proceeding. If false, fire-and-forget. |
| `timeout` | int | `10` | Seconds to wait when `blocking: true`. Ignored for non-blocking hooks. |

### Shorthand Form

A hook may be specified as a bare string (equivalent to `blocking: false`, `timeout: 10`):

```yaml
hooks:
  on_dispatch: "python3 ~/.hex-events/hex_emit.py boi.spec.dispatched"
```

---

## Invocation Mechanics

### How BOI Fires a Hook

1. BOI serializes the lifecycle payload to a JSON string.
2. BOI spawns: `sh -c "<command>"` with stdin piped.
3. BOI writes the JSON bytes to the child's stdin, then closes the pipe.
4. If `blocking: true`: BOI waits up to `timeout` seconds for the child to exit.
5. If `blocking: false`: BOI detaches (does not wait for child exit).
6. Hook subprocess exit code is logged but **never blocks spec progression** (see Exit Codes section).

### Environment Variables

All hooks inherit the daemon's environment plus these additional variables:

| Variable | Value | Description |
|----------|-------|-------------|
| `BOI_HOOK` | `on_dispatch` (etc.) | Which lifecycle point fired |
| `BOI_SPEC_ID` | `q-001` | Spec queue ID |
| `BOI_SPEC_PATH` | `/Users/mrap/.boi/queue/q-001.spec.md` | Absolute path to spec file |
| `BOI_ITERATION` | `3` | Current iteration number (1-based) |
| `BOI_VERSION` | `1.0.0` | BOI binary version |
| `HOME` | `/Users/mrap` | Inherited; used for `~` expansion in `command` |

Task-scoped hooks (`on_task_start`, `on_task_complete`, `on_task_fail`) additionally set:

| Variable | Value | Description |
|----------|-------|-------------|
| `BOI_TASK_ID` | `t-3` | Task ID within the spec |
| `BOI_TASK_TITLE` | `Specify the hook interface` | Task title |

### Exit Codes

| Exit Code | Meaning |
|-----------|---------|
| `0` | Success. Hook ran cleanly. No action taken. |
| Non-zero | Hook failed. **BOI logs a warning and continues regardless.** Hooks never block spec state transitions. |

The rationale: hooks are observability/notification integrations. A broken hex-events bridge should never stall a spec.

**Exception for blocking hooks:** If `blocking: true` and the hook times out (process still running after `timeout` seconds), BOI sends SIGTERM to the hook process, logs a timeout warning, and continues. This is the only hard kill; non-blocking hooks are never killed.

---

## Lifecycle Points

### `on_dispatch`

**Trigger:** A spec is accepted into the queue (i.e., `boi dispatch --spec FILE` succeeds). Fires once per spec, before any worker is assigned.

**Fired by:** CLI (`boi dispatch`), in `src/cli/dispatch.rs`.

**Payload:**
```json
{
  "hook": "on_dispatch",
  "spec_id": "q-001",
  "spec_path": "/Users/mrap/.boi/queue/q-001.spec.md",
  "spec_title": "BOI Rust Migration — Research + Architecture",
  "initiative": "boi-system",
  "mode": "generate",
  "priority": 100,
  "max_iterations": 30,
  "tasks_total": 4,
  "submitted_at": "2026-04-27T08:00:00Z",
  "iteration": 0,
  "timestamp": "2026-04-27T08:00:00Z"
}
```

**Default behavior:** Non-blocking, timeout 10s.

**Python equivalent:** `lib/cli_ops._emit_dispatched_event()` → `boi.spec.dispatched`

---

### `on_worker_start`

**Trigger:** A worker is assigned to a spec and begins executing its first action (immediately after the worker process starts and parses the spec, before spawning the agent runtime). Fires once per iteration.

**Fired by:** Worker process (`boi worker`), in `src/worker/mod.rs`, before prompt generation.

**Payload:**
```json
{
  "hook": "on_worker_start",
  "spec_id": "q-001",
  "spec_path": "/Users/mrap/.boi/queue/q-001.spec.md",
  "spec_title": "BOI Rust Migration — Research + Architecture",
  "worker_id": "w-1",
  "worktree": "/Users/mrap/.boi/worktrees/boi-worker-1",
  "iteration": 3,
  "tasks_done": 2,
  "tasks_pending": 2,
  "tasks_total": 4,
  "timestamp": "2026-04-27T08:00:00Z"
}
```

**Default behavior:** Non-blocking, timeout 10s.

**Python equivalent:** None (new in Rust port).

---

### `on_task_start`

**Trigger:** The worker identifies the next PENDING task and is about to include it in the agent prompt. Fires once per task per iteration.

**Fired by:** Worker process, in `src/worker/mod.rs`, after task selection, before prompt write.

**Note:** BOI fires this for the single task the worker is about to execute. If a worker executes multiple tasks in one session (unlikely but possible in generate mode), this fires once per task started.

**Payload:**
```json
{
  "hook": "on_task_start",
  "spec_id": "q-001",
  "spec_path": "/Users/mrap/.boi/queue/q-001.spec.md",
  "spec_title": "BOI Rust Migration — Research + Architecture",
  "worker_id": "w-1",
  "iteration": 3,
  "task_id": "t-3",
  "task_title": "Specify the hook interface",
  "tasks_done": 2,
  "tasks_pending": 2,
  "tasks_total": 4,
  "timestamp": "2026-04-27T08:00:00Z"
}
```

**Default behavior:** Non-blocking, timeout 10s.

**Python equivalent:** None (new in Rust port).

---

### `on_task_complete`

**Trigger:** A task's status changes from `PENDING` to `DONE` in the spec file. Detected by the worker's post-iteration task count diff.

**Fired by:** Worker process, in `src/worker/mod.rs`, after the agent session exits and the spec file is re-parsed. Fires once per task that transitioned to DONE in this iteration.

**Payload:**
```json
{
  "hook": "on_task_complete",
  "spec_id": "q-001",
  "spec_path": "/Users/mrap/.boi/queue/q-001.spec.md",
  "spec_title": "BOI Rust Migration — Research + Architecture",
  "worker_id": "w-1",
  "iteration": 3,
  "task_id": "t-3",
  "task_title": "Specify the hook interface",
  "tasks_done": 3,
  "tasks_pending": 1,
  "tasks_total": 4,
  "duration_seconds": 312,
  "timestamp": "2026-04-27T08:00:00Z"
}
```

**Default behavior:** Non-blocking, timeout 10s.

**Python equivalent:** None (new in Rust port). The Python system tracked task counts but never fired a per-task hook.

---

### `on_task_fail`

**Trigger:** A task's status changes from `PENDING` to `FAILED` in the spec file, OR the worker exits non-zero without marking any task DONE (implying task-level failure). Also fires when outcome verification resets the last DONE task back to PENDING (the reset task is treated as failed).

**Fired by:** Worker process, in `src/worker/mod.rs`, post-iteration.

**Payload:**
```json
{
  "hook": "on_task_fail",
  "spec_id": "q-001",
  "spec_path": "/Users/mrap/.boi/queue/q-001.spec.md",
  "spec_title": "BOI Rust Migration — Research + Architecture",
  "worker_id": "w-1",
  "iteration": 3,
  "task_id": "t-3",
  "task_title": "Specify the hook interface",
  "tasks_done": 2,
  "tasks_pending": 2,
  "tasks_total": 4,
  "worker_exit_code": 1,
  "failure_reason": "outcome_verify_failed",
  "timestamp": "2026-04-27T08:00:00Z"
}
```

**`failure_reason` values:**

| Value | Meaning |
|-------|---------|
| `worker_exit_nonzero` | Agent subprocess exited non-zero |
| `outcome_verify_failed` | A spec outcome verify command failed; last DONE task was reset |
| `task_marked_failed` | Worker explicitly marked the task FAILED |
| `timeout` | Worker timed out (exit code 124) |

**Default behavior:** Non-blocking, timeout 10s.

**Python equivalent:** None (new in Rust port).

---

### `on_complete`

**Trigger:** All tasks in the spec reach a terminal status (all DONE or SKIPPED, no PENDING or FAILED) AND all spec-level outcome verify commands pass. This is the "spec succeeded" event.

**Fired by:** Daemon, in `src/daemon/monitor.rs`, when transitioning spec status to `completed`.

**Payload:**
```json
{
  "hook": "on_complete",
  "spec_id": "q-001",
  "spec_path": "/Users/mrap/.boi/queue/q-001.spec.md",
  "spec_title": "BOI Rust Migration — Research + Architecture",
  "iteration": 4,
  "tasks_done": 4,
  "tasks_skipped": 0,
  "tasks_total": 4,
  "tasks_added": 1,
  "total_duration_seconds": 1240,
  "total_cost_usd": 0.187,
  "submitted_at": "2026-04-27T06:00:00Z",
  "completed_at": "2026-04-27T08:00:00Z",
  "timestamp": "2026-04-27T08:00:00Z"
}
```

**Default behavior:** Non-blocking, timeout 10s.

**Python equivalent:** `daemon_ops.py` → `boi.spec.completed` (hardcoded hex-events call).

---

### `on_fail`

**Trigger:** The spec has exhausted retries (`consecutive_failures >= 5`) or reached `max_iterations` with PENDING tasks remaining. The daemon transitions the spec to `failed` status.

**Fired by:** Daemon, in `src/daemon/monitor.rs`, when transitioning spec status to `failed`.

**Payload:**
```json
{
  "hook": "on_fail",
  "spec_id": "q-001",
  "spec_path": "/Users/mrap/.boi/queue/q-001.spec.md",
  "spec_title": "BOI Rust Migration — Research + Architecture",
  "iteration": 30,
  "tasks_done": 2,
  "tasks_pending": 2,
  "tasks_total": 4,
  "consecutive_failures": 5,
  "failure_reason": "max_consecutive_failures",
  "total_duration_seconds": 18000,
  "submitted_at": "2026-04-27T06:00:00Z",
  "failed_at": "2026-04-27T11:00:00Z",
  "timestamp": "2026-04-27T11:00:00Z"
}
```

**`failure_reason` values:**

| Value | Meaning |
|-------|---------|
| `max_consecutive_failures` | 5 consecutive worker failures |
| `max_iterations` | `max_iterations` reached with PENDING tasks |

**Default behavior:** Non-blocking, timeout 10s.

**Python equivalent:** `daemon_ops.py` → `boi.spec.failed` (hardcoded hex-events call) + `~/.boi/hooks/on-fail.sh` (old shell hook).

---

### `on_cancel`

**Trigger:** A user runs `boi cancel <spec_id>` and the spec transitions to `canceled` status. Also fires if a spec is canceled programmatically (e.g., via `boi stop`).

**Fired by:** CLI (`boi cancel`), in `src/cli/cancel.rs`, after status update.

**Payload:**
```json
{
  "hook": "on_cancel",
  "spec_id": "q-001",
  "spec_path": "/Users/mrap/.boi/queue/q-001.spec.md",
  "spec_title": "BOI Rust Migration — Research + Architecture",
  "iteration": 2,
  "tasks_done": 1,
  "tasks_pending": 3,
  "tasks_total": 4,
  "cancelled_by": "user",
  "timestamp": "2026-04-27T09:00:00Z"
}
```

**`cancelled_by` values:**

| Value | Meaning |
|-------|---------|
| `user` | `boi cancel <id>` or `boi stop` |
| `system` | Daemon canceled due to system condition (reserved for future use) |

**Default behavior:** Non-blocking, timeout 10s.

**Python equivalent:** None (new in Rust port). Python had no cancel hook.

---

### `on_stall`

**Trigger:** A worker is assigned and `status=running`, but no task has transitioned from PENDING to DONE for longer than the stall threshold. The stall threshold is configurable (default: 30 minutes). Detected by the daemon's monitor loop comparing `last_iteration_at` against `now`.

**Fired by:** Daemon, in `src/daemon/monitor.rs`, on each poll cycle when stall is detected.

**Fires repeatedly** at each poll interval while the stall persists (every 5 seconds by default). Implementors should deduplicate by `spec_id` and `iteration` if idempotency matters.

**Payload:**
```json
{
  "hook": "on_stall",
  "spec_id": "q-001",
  "spec_path": "/Users/mrap/.boi/queue/q-001.spec.md",
  "spec_title": "BOI Rust Migration — Research + Architecture",
  "worker_id": "w-1",
  "iteration": 3,
  "tasks_done": 2,
  "tasks_pending": 2,
  "tasks_total": 4,
  "stall_seconds": 1800,
  "stall_threshold_seconds": 1800,
  "last_progress_at": "2026-04-27T07:30:00Z",
  "timestamp": "2026-04-27T08:00:00Z"
}
```

**Stall threshold configuration:**
```yaml
stall_threshold_minutes: 30   # default: 30 minutes
```

**Default behavior:** Non-blocking, timeout 10s.

**Python equivalent:** None (new in Rust port).

---

## Common Payload Fields

All hook payloads include these fields:

| Field | Type | Description |
|-------|------|-------------|
| `hook` | string | Lifecycle point name (e.g., `"on_complete"`) |
| `spec_id` | string | Queue ID (e.g., `"q-001"` or future `"S0000001"`) |
| `spec_path` | string | Absolute path to the spec file in `~/.boi/queue/` |
| `spec_title` | string | Title field from the spec (or empty string if absent) |
| `iteration` | int | Current iteration number (0 = not yet started) |
| `timestamp` | string | ISO-8601 UTC timestamp when the hook fired |

Task-scoped hooks (`on_task_start`, `on_task_complete`, `on_task_fail`) additionally include:

| Field | Type | Description |
|-------|------|-------------|
| `task_id` | string | Task ID (e.g., `"t-3"`) |
| `task_title` | string | Task title from the spec |

---

## Rust Implementation Reference

### Config Structs (`src/hooks/config.rs`)

```rust
#[derive(Debug, Deserialize, Default)]
pub struct HookConfig {
    pub command: String,
    #[serde(default)]
    pub blocking: bool,
    #[serde(default = "default_hook_timeout")]
    pub timeout: u64,  // seconds
}

fn default_hook_timeout() -> u64 { 10 }

#[derive(Debug, Deserialize, Default)]
pub struct HooksConfig {
    pub on_dispatch:      Option<HookConfig>,
    pub on_worker_start:  Option<HookConfig>,
    pub on_task_start:    Option<HookConfig>,
    pub on_task_complete: Option<HookConfig>,
    pub on_task_fail:     Option<HookConfig>,
    pub on_complete:      Option<HookConfig>,
    pub on_fail:          Option<HookConfig>,
    pub on_cancel:        Option<HookConfig>,
    pub on_stall:         Option<HookConfig>,
}
```

### Hook Enum (`src/hooks/mod.rs`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookPoint {
    OnDispatch,
    OnWorkerStart,
    OnTaskStart,
    OnTaskComplete,
    OnTaskFail,
    OnComplete,
    OnFail,
    OnCancel,
    OnStall,
}

impl HookPoint {
    pub fn config_key(&self) -> &'static str {
        match self {
            Self::OnDispatch     => "on_dispatch",
            Self::OnWorkerStart  => "on_worker_start",
            Self::OnTaskStart    => "on_task_start",
            Self::OnTaskComplete => "on_task_complete",
            Self::OnTaskFail     => "on_task_fail",
            Self::OnComplete     => "on_complete",
            Self::OnFail         => "on_fail",
            Self::OnCancel       => "on_cancel",
            Self::OnStall        => "on_stall",
        }
    }
}
```

### Hook Runner (`src/hooks/mod.rs`)

```rust
pub async fn fire(point: HookPoint, payload: &serde_json::Value) -> anyhow::Result<()> {
    let config = load_config()?;
    let hook = match config.hooks.get(point) {
        Some(h) => h,
        None => return Ok(()),  // no hook configured, silent no-op
    };

    let json = serde_json::to_string(payload)?;

    // Set BOI_HOOK and BOI_* env vars
    let mut child = tokio::process::Command::new("sh")
        .args(["-c", &hook.command])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("BOI_HOOK", point.config_key())
        .env("BOI_SPEC_ID", payload["spec_id"].as_str().unwrap_or(""))
        .env("BOI_SPEC_PATH", payload["spec_path"].as_str().unwrap_or(""))
        .env("BOI_ITERATION", payload["iteration"].as_u64().unwrap_or(0).to_string())
        .env("BOI_VERSION", env!("CARGO_PKG_VERSION"))
        .spawn()?;

    // Write JSON payload to stdin
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(json.as_bytes()).await;
        // stdin closed on drop, signaling EOF to child
    }

    if hook.blocking {
        match tokio::time::timeout(
            Duration::from_secs(hook.timeout),
            child.wait(),
        ).await {
            Ok(Ok(status)) => {
                if !status.success() {
                    tracing::warn!(
                        hook = point.config_key(),
                        exit_code = status.code(),
                        "hook exited non-zero (continuing)"
                    );
                }
            }
            Ok(Err(e)) => tracing::warn!(hook = point.config_key(), error = %e, "hook wait error"),
            Err(_) => {
                tracing::warn!(hook = point.config_key(), timeout = hook.timeout, "hook timed out, killing");
                let _ = child.kill().await;
            }
        }
    }
    // Non-blocking: child runs independently; we return immediately
    Ok(())
}
```

---

## Where Each Hook Is Fired (Call Sites)

| Hook Point | File | Trigger Condition |
|------------|------|-------------------|
| `on_dispatch` | `src/cli/dispatch.rs` | After spec inserted into SQLite with `status=queued` |
| `on_worker_start` | `src/worker/mod.rs` | Start of `run_worker()`, before prompt generation |
| `on_task_start` | `src/worker/mod.rs` | After selecting next PENDING task, before `atomic_write(prompt_path)` |
| `on_task_complete` | `src/worker/mod.rs` | For each task that transitioned PENDING→DONE in post-iteration diff |
| `on_task_fail` | `src/worker/mod.rs` | For each task that transitioned to FAILED, or on outcome reset |
| `on_complete` | `src/daemon/monitor.rs` | When transitioning spec to `completed` |
| `on_fail` | `src/daemon/monitor.rs` | When transitioning spec to `failed` |
| `on_cancel` | `src/cli/cancel.rs` | After `UPDATE specs SET status='canceled'` |
| `on_stall` | `src/daemon/monitor.rs` | Each poll cycle where stall threshold exceeded |

---

## Migration from Python Hooks

### Old Shell Hooks → New YAML Hooks

The old `~/.boi/hooks/on-complete.sh` received args `queue_id spec_path`. To migrate:

**Old hook (`~/.boi/hooks/on-complete.sh`):**
```bash
#!/bin/bash
QUEUE_ID=$1
SPEC_PATH=$2
echo "Spec $QUEUE_ID completed" | notify-send "BOI"
```

**New config (`~/.boi/hooks.yaml`):**
```yaml
hooks:
  on_complete:
    command: "bash ~/.boi/hooks/on-complete-new.sh"
    blocking: false
    timeout: 10
```

**New hook (`~/.boi/hooks/on-complete-new.sh`):**
```bash
#!/bin/bash
# JSON payload on stdin; also available as env vars
PAYLOAD=$(cat)
QUEUE_ID=$(echo "$PAYLOAD" | python3 -c "import sys,json; print(json.load(sys.stdin)['spec_id'])")
# Or use $BOI_SPEC_ID env var directly
echo "Spec $BOI_SPEC_ID completed" | notify-send "BOI"
```

### Old Hardcoded hex-events Calls → New YAML Hooks

The Python codebase hardcodes three `hex_emit.py` calls. In the Rust port, these are covered by the built-in default (`hooks/default.yaml`). To customize, add them to `~/.boi/hooks.yaml`:

```yaml
hooks:
  on_dispatch:
    command: "python3 ~/.hex-events/hex_emit.py boi.spec.dispatched"
    blocking: false
    timeout: 10
  on_complete:
    command: "python3 ~/.hex-events/hex_emit.py boi.spec.completed"
    blocking: false
    timeout: 10
  on_fail:
    command: "python3 ~/.hex-events/hex_emit.py boi.spec.failed"
    blocking: false
    timeout: 10
```

The hex_emit.py script reads JSON from stdin unchanged — the payload format is a superset of what Python currently sends.

---

## Hex Integration Reference Payloads

For the hex-events integration specifically, the `hex_emit.py` script reads the full JSON payload from stdin and maps it to a hex event. The fields hex currently uses from BOI:

| Hex Field | Comes From |
|-----------|-----------|
| `spec_id` | `spec_id` |
| `source` | hardcoded `"boi"` in hex_emit.py |
| `spec_file` | `spec_path` |
| `iteration` | `iteration` |
| `tasks_done` | `tasks_done` |
| `tasks_total` | `tasks_total` |

All new payload fields (`spec_title`, `worker_id`, `duration_seconds`, etc.) are passed through as-is and available to hex-events consumers.

---

## Design Rationale

### Non-blocking Default

All hooks default to `blocking: false`. This ensures hooks (which are I/O-bound integrations with external services) never add latency to the spec lifecycle. A slow hex-events endpoint or a notification service timeout cannot stall a worker iteration.

### Fire-and-Forget on Failure

Hook failures are logged but never propagate to spec state. This matches the existing Python behavior for hex-events calls (`Popen`, no wait). External integrations are best-effort; spec execution is the critical path.

### JSON on stdin (not args)

Structured JSON on stdin is more extensible than positional CLI args. New fields can be added without breaking existing consumers. The old shell hooks used positional args (`queue_id spec_path`) which cannot evolve.

### `sh -c` invocation

Using `sh -c "<command>"` allows users to write any shell expression — pipes, redirections, environment variable expansion, compound commands — without BOI needing to understand shell syntax. This is the same pattern used by git hooks and many other extensibility systems.

### Stall Detection Fires Repeatedly

`on_stall` fires on every daemon poll cycle while the stall persists, rather than once. This lets consumers implement escalating alerts (e.g., page after 30 min, escalate after 60 min) in the hook command, using the `stall_seconds` field to decide severity.
