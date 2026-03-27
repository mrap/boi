# hooks.py — Integration hooks and lifecycle events for BOI.
#
# BOI exposes hooks that external systems can consume, without depending
# on them. This module provides:
#
#   1. Lifecycle event writing — standardized JSON events to ~/.boi/events/
#   2. Hook script execution — optional user scripts in ~/.boi/hooks/
#
# External systems (hex heartbeat, notification daemons, etc.) can poll
# the events directory. BOI itself does NOT send notifications or integrate
# with any specific system. It just writes events and runs hook scripts.
#
# Event schema (spec_completed example):
#   {
#     "type": "spec_completed",
#     "queue_id": "q-001",
#     "spec_path": "/path/to/spec.md",
#     "iterations": 3,
#     "tasks_done": 8,
#     "tasks_added": 2,
#     "timestamp": "2024-01-15T08:23:00+00:00"
#   }
#
# Hook scripts:
#   ~/.boi/hooks/on-complete.sh  — runs after spec completes (success or failure)
#   ~/.boi/hooks/on-fail.sh     — runs only on failure
#   Hook scripts receive: queue_id, spec_path as positional arguments.

import json
import os
import subprocess
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional


def write_lifecycle_event(
    events_dir: str,
    event_type: str,
    queue_id: str,
    spec_path: str = "",
    iterations: int = 0,
    tasks_done: int = 0,
    tasks_added: int = 0,
    tasks_total: int = 0,
    reason: str = "",
    worker_id: str = "",
    timestamp: Optional[str] = None,
    extra: Optional[dict[str, Any]] = None,
) -> str:
    """Write a lifecycle event to the events directory.

    Returns the path to the written event file.

    Events are written as individual JSON files with incrementing sequence
    numbers: event-00001.json, event-00002.json, ...
    """
    from lib.event_log import write_event

    if timestamp is None:
        timestamp = datetime.now(timezone.utc).isoformat()

    event: dict[str, Any] = {
        "type": event_type,
        "queue_id": queue_id,
        "timestamp": timestamp,
    }

    # Add optional fields only when they carry information
    if spec_path:
        event["spec_path"] = spec_path
    if iterations > 0:
        event["iterations"] = iterations
    if tasks_done > 0:
        event["tasks_done"] = tasks_done
    if tasks_added > 0:
        event["tasks_added"] = tasks_added
    if tasks_total > 0:
        event["tasks_total"] = tasks_total
    if reason:
        event["reason"] = reason
    if worker_id:
        event["worker_id"] = worker_id
    if extra:
        event.update(extra)

    seq = write_event(events_dir, event)
    return os.path.join(events_dir, f"event-{seq:05d}.json")


def write_spec_completed_event(
    events_dir: str,
    queue_id: str,
    spec_path: str,
    iterations: int,
    tasks_done: int,
    tasks_added: int = 0,
    tasks_total: int = 0,
    timestamp: Optional[str] = None,
) -> str:
    """Write a spec_completed lifecycle event. Returns event file path."""
    return write_lifecycle_event(
        events_dir=events_dir,
        event_type="spec_completed",
        queue_id=queue_id,
        spec_path=spec_path,
        iterations=iterations,
        tasks_done=tasks_done,
        tasks_added=tasks_added,
        tasks_total=tasks_total,
        timestamp=timestamp,
    )


def write_spec_failed_event(
    events_dir: str,
    queue_id: str,
    spec_path: str,
    iterations: int,
    tasks_done: int = 0,
    tasks_added: int = 0,
    reason: str = "",
    timestamp: Optional[str] = None,
) -> str:
    """Write a spec_failed lifecycle event. Returns event file path."""
    return write_lifecycle_event(
        events_dir=events_dir,
        event_type="spec_failed",
        queue_id=queue_id,
        spec_path=spec_path,
        iterations=iterations,
        tasks_done=tasks_done,
        tasks_added=tasks_added,
        reason=reason,
        timestamp=timestamp,
    )


def run_hook(
    hooks_dir: str,
    hook_name: str,
    queue_id: str,
    spec_path: str,
    timeout_seconds: int = 30,
) -> Optional[int]:
    """Run an optional hook script if it exists.

    Hook scripts live at {hooks_dir}/{hook_name}.sh and receive
    queue_id and spec_path as positional arguments.

    Returns the exit code if the hook ran, or None if no hook found.
    Does NOT raise on hook failure (hooks are best-effort).
    """
    hook_script = os.path.join(hooks_dir, f"{hook_name}.sh")

    if not os.path.isfile(hook_script):
        return None

    if not os.access(hook_script, os.X_OK):
        return None

    try:
        result = subprocess.run(
            ["bash", hook_script, queue_id, spec_path],
            capture_output=True,
            timeout=timeout_seconds,
            text=True,
        )
        return result.returncode
    except subprocess.TimeoutExpired:
        return -1
    except OSError:
        return -1


def run_completion_hooks(
    hooks_dir: str,
    queue_id: str,
    spec_path: str,
    is_failure: bool = False,
) -> dict[str, Optional[int]]:
    """Run all relevant hooks for a spec completion or failure.

    Always runs on-complete.sh (if present).
    Additionally runs on-fail.sh (if present) when is_failure is True.

    Returns a dict mapping hook name to exit code (None if hook not found).
    """
    results: dict[str, Optional[int]] = {}

    # on-complete fires for both success and failure
    results["on-complete"] = run_hook(hooks_dir, "on-complete", queue_id, spec_path)

    # on-fail fires only on failure
    if is_failure:
        results["on-fail"] = run_hook(hooks_dir, "on-fail", queue_id, spec_path)

    return results


def list_hooks(hooks_dir: str) -> list[str]:
    """List available hook scripts in the hooks directory.

    Returns a list of hook names (without .sh extension).
    """
    path = Path(hooks_dir)
    if not path.is_dir():
        return []

    hooks = []
    for entry in sorted(path.iterdir()):
        if entry.suffix == ".sh" and entry.is_file():
            hooks.append(entry.stem)

    return hooks


def get_tasks_added_from_telemetry(queue_dir: str, queue_id: str) -> int:
    """Extract total tasks_added from telemetry data.

    Reads the telemetry file and sums the tasks_added_per_iteration array.
    Returns 0 if telemetry is unavailable.
    """
    telemetry_path = Path(queue_dir) / f"{queue_id}.telemetry.json"
    if not telemetry_path.is_file():
        return 0

    try:
        data = json.loads(telemetry_path.read_text(encoding="utf-8"))
        added = data.get("tasks_added_per_iteration", [])
        return sum(a for a in added if isinstance(a, int))
    except (json.JSONDecodeError, OSError, TypeError):
        return 0
