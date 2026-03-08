# queue.py — Spec queue operations for BOI.
#
# The queue lives at ~/.boi/queue/. Each queued spec is a JSON file
# named {queue-id}.json. Operations are atomic (write .tmp, then mv).
#
# All state-mutating operations are protected by an exclusive flock
# via lib.locking.queue_lock to prevent concurrent corruption.
#
# Queue entry schema:
#   {
#     "id": "q-001",
#     "spec_path": "/path/to/copied/spec.md",
#     "original_spec_path": "/path/to/user/spec.md",
#     "worktree": null,
#     "priority": 100,
#     "status": "queued",
#     "submitted_at": "ISO-8601",
#     "iteration": 0,
#     "max_iterations": 30,
#     "blocked_by": [],
#     "last_worker": null,
#     "last_iteration_at": null,
#     "consecutive_failures": 0,
#     "tasks_done": 0,
#     "tasks_total": 0,
#     "sync_back": true
#   }
#
# Status transitions: queued -> running -> (completed | requeued | failed | needs_review)
# needs_review: spec is paused waiting for human review (e.g., experiment proposals)
# needs_review -> queued (after review) or auto-rejected (after timeout)

import json
import os
import shutil
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

from lib.locking import queue_lock


DEFAULT_MAX_ITERATIONS = 30
DEFAULT_PRIORITY = 100
MAX_CONSECUTIVE_FAILURES = 5
COOLDOWN_SECONDS = 60  # Seconds to wait after a crash before retrying

# Default experiment budgets per mode
DEFAULT_EXPERIMENT_BUDGETS: dict[str, int] = {
    "execute": 0,
    "challenge": 2,
    "discover": 3,
    "generate": 5,
}


def _queue_filename(queue_id: str) -> str:
    """Return the filename for a queue entry."""
    return f"{queue_id}.json"


def _spec_name_from_entry(entry: dict[str, Any]) -> str:
    """Extract a human-readable spec name from a queue entry."""
    spec_path = entry.get("original_spec_path", entry.get("spec_path", ""))
    if spec_path:
        return os.path.splitext(os.path.basename(spec_path))[0]
    return ""


def _next_queue_id(queue_dir: str) -> str:
    """Generate the next queue ID (q-001, q-002, etc.).

    IMPORTANT: This must only be called while holding queue_lock
    to avoid TOCTOU races.
    """
    path = Path(queue_dir)
    if not path.is_dir():
        return "q-001"

    max_num = 0
    for entry in path.iterdir():
        name = entry.stem
        if name.startswith("q-") and not ("." in name):
            try:
                num = int(name[2:])
                if num > max_num:
                    max_num = num
            except ValueError:
                continue

    return f"q-{max_num + 1:03d}"


def _write_entry(queue_dir: str, entry: dict[str, Any]) -> None:
    """Write a queue entry atomically."""
    path = Path(queue_dir)
    path.mkdir(parents=True, exist_ok=True)

    filename = _queue_filename(entry["id"])
    target = path / filename
    tmp = path / f".{filename}.tmp"

    data = json.dumps(entry, indent=2, sort_keys=False) + "\n"
    tmp.write_text(data, encoding="utf-8")
    os.rename(str(tmp), str(target))


def _read_entry(queue_dir: str, queue_id: str) -> Optional[dict[str, Any]]:
    """Read a single queue entry. Returns None if not found."""
    path = Path(queue_dir) / _queue_filename(queue_id)
    if not path.is_file():
        return None
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError):
        return None


class DuplicateSpecError(Exception):
    """Raised when the same spec is already active in the queue."""

    def __init__(self, original_spec_path: str, existing_id: str, existing_status: str):
        self.original_spec_path = original_spec_path
        self.existing_id = existing_id
        self.existing_status = existing_status
        super().__init__(
            f"Spec '{original_spec_path}' is already in the queue as "
            f"{existing_id} (status: {existing_status}). "
            f"Cancel it first with 'boi cancel {existing_id}' or wait for it to finish."
        )


def enqueue(
    queue_dir: str,
    spec_path: str,
    priority: int = DEFAULT_PRIORITY,
    max_iterations: int = DEFAULT_MAX_ITERATIONS,
    blocked_by: list[str] | None = None,
    checkout: str | None = None,  # alias: worktree
    queue_id: str | None = None,
    sync_back: bool = True,
    project: str | None = None,
) -> dict[str, Any]:
    """Add a spec to the queue. Returns the queue entry dict.

    The spec file is copied into the queue directory to prevent
    concurrent mutation if the same spec is dispatched twice or
    edited during a run.

    Raises DuplicateSpecError if the same original_spec_path is
    already active (queued/running/requeued) in the queue.

    ID generation is performed inside the lock to prevent TOCTOU races.
    """
    abs_spec_path = os.path.abspath(spec_path)

    with queue_lock(queue_dir):
        # Duplicate detection: reject if same spec is already active
        active_statuses = {"queued", "running", "requeued"}
        existing = get_queue(queue_dir)
        for e in existing:
            e_original = e.get("original_spec_path", e.get("spec_path", ""))
            if e_original == abs_spec_path and e.get("status") in active_statuses:
                raise DuplicateSpecError(abs_spec_path, e["id"], e["status"])

        if queue_id is None:
            queue_id = _next_queue_id(queue_dir)

        # Copy spec file into queue directory
        spec_copy_name = f"{queue_id}.spec.md"
        spec_copy_path = os.path.join(queue_dir, spec_copy_name)
        shutil.copy2(abs_spec_path, spec_copy_path)

        # Snapshot the initial task IDs from the spec at dispatch time.
        # Used later for computing evolution_ratio (Deutschian progress metric).
        initial_task_ids: list[str] = []
        try:
            from lib.spec_parser import parse_boi_spec

            spec_content = Path(spec_copy_path).read_text(encoding="utf-8")
            initial_tasks = parse_boi_spec(spec_content)
            initial_task_ids = [t.id for t in initial_tasks]
        except Exception:
            pass

        entry = {
            "id": queue_id,
            "spec_path": os.path.abspath(spec_copy_path),
            "original_spec_path": abs_spec_path,
            "worktree": checkout,
            "priority": priority,
            "status": "queued",
            "submitted_at": datetime.now(timezone.utc).isoformat(),
            "iteration": 0,
            "max_iterations": max_iterations,
            "blocked_by": list(blocked_by) if blocked_by else [],
            "last_worker": None,
            "last_iteration_at": None,
            "consecutive_failures": 0,
            "tasks_done": 0,
            "tasks_total": 0,
            "sync_back": sync_back,
            "project": project,
            "initial_task_ids": initial_task_ids,
        }

        _write_entry(queue_dir, entry)
        return entry


def dequeue(
    queue_dir: str, blocked_ids: set[str] | None = None
) -> Optional[dict[str, Any]]:
    """Pick the highest-priority unblocked spec with status 'queued' or 'requeued'.

    Returns the queue entry dict, or None if no eligible spec found.
    Atomically marks the spec as 'assigning' to prevent TOCTOU double-pickup.
    Caller should call set_running after launching the worker.

    blocked_ids: set of queue IDs that are blocked by incomplete dependencies.
    """
    with queue_lock(queue_dir):
        entries = get_queue(queue_dir)
        blocked = blocked_ids or set()

        now = datetime.now(timezone.utc).isoformat()

        for entry in entries:
            status = entry.get("status", "")
            if status not in ("queued", "requeued"):
                continue
            if entry["id"] in blocked:
                continue
            # Check cooldown (skip if still cooling down after a crash)
            cooldown_until = entry.get("cooldown_until")
            if cooldown_until and cooldown_until > now:
                continue
            # Check blocked_by field
            if entry.get("blocked_by"):
                all_deps_done = True
                for dep_id in entry["blocked_by"]:
                    dep = _read_entry(queue_dir, dep_id)
                    if dep is None or dep.get("status") != "completed":
                        all_deps_done = False
                        break
                if not all_deps_done:
                    continue
            # Atomically mark as 'assigning' to prevent double-pickup
            entry["status"] = "assigning"
            _write_entry(queue_dir, entry)
            return entry

        return None


def set_running(queue_dir: str, queue_id: str, worker_id: str) -> None:
    """Set a spec's status to 'running' and record the worker.

    Also snapshots the current task statuses from the spec file into
    'pre_iteration_tasks' for post-iteration regression detection.
    """
    from lib.spec_parser import parse_boi_spec

    with queue_lock(queue_dir):
        entry = _read_entry(queue_dir, queue_id)
        if entry is None:
            raise ValueError(f"Queue entry not found: {queue_id}")

        entry["status"] = "running"
        entry["last_worker"] = worker_id
        entry["iteration"] += 1
        entry["last_iteration_at"] = datetime.now(timezone.utc).isoformat()

        # Track when the spec first started running (for max duration timeout)
        if "first_running_at" not in entry:
            entry["first_running_at"] = datetime.now(timezone.utc).isoformat()

        # Snapshot task statuses before the iteration runs
        spec_path = entry.get("spec_path", "")
        if spec_path and Path(spec_path).is_file():
            try:
                content = Path(spec_path).read_text(encoding="utf-8")
                tasks = parse_boi_spec(content)
                entry["pre_iteration_tasks"] = {t.id: t.status for t in tasks}
            except Exception:
                entry["pre_iteration_tasks"] = {}
        else:
            entry["pre_iteration_tasks"] = {}

        _write_entry(queue_dir, entry)


def requeue(
    queue_dir: str, queue_id: str, tasks_done: int = 0, tasks_total: int = 0
) -> None:
    """Set a spec's status back to 'requeued' (still has PENDING tasks)."""
    with queue_lock(queue_dir):
        entry = _read_entry(queue_dir, queue_id)
        if entry is None:
            raise ValueError(f"Queue entry not found: {queue_id}")

        entry["status"] = "requeued"
        entry["tasks_done"] = tasks_done
        entry["tasks_total"] = tasks_total
        # Reset consecutive failures on successful iteration
        entry["consecutive_failures"] = 0
        # Clear any cooldown
        entry.pop("cooldown_until", None)

        _write_entry(queue_dir, entry)


def set_needs_review(
    queue_dir: str,
    queue_id: str,
    experiment_tasks: list[str],
    tasks_done: int = 0,
    tasks_total: int = 0,
) -> None:
    """Set a spec's status to 'needs_review' (has EXPERIMENT_PROPOSED tasks).

    The spec is paused until a human reviews the experiments via `boi review`.
    Records which tasks have experiments and the timestamp for timeout tracking.
    """
    with queue_lock(queue_dir):
        entry = _read_entry(queue_dir, queue_id)
        if entry is None:
            raise ValueError(f"Queue entry not found: {queue_id}")

        entry["status"] = "needs_review"
        entry["tasks_done"] = tasks_done
        entry["tasks_total"] = tasks_total
        entry["experiment_tasks"] = experiment_tasks
        entry["needs_review_since"] = datetime.now(timezone.utc).isoformat()
        # Reset consecutive failures on successful iteration
        entry["consecutive_failures"] = 0
        entry.pop("cooldown_until", None)

        _write_entry(queue_dir, entry)


def complete(
    queue_dir: str, queue_id: str, tasks_done: int = 0, tasks_total: int = 0
) -> None:
    """Set a spec's status to 'completed'.

    If sync_back is enabled, copies the final spec back to the original location.
    """
    with queue_lock(queue_dir):
        entry = _read_entry(queue_dir, queue_id)
        if entry is None:
            raise ValueError(f"Queue entry not found: {queue_id}")

        entry["status"] = "completed"
        entry["tasks_done"] = tasks_done
        entry["tasks_total"] = tasks_total

        _write_entry(queue_dir, entry)

    # Sync back outside the lock (file I/O that doesn't touch queue state)
    sync_back_spec(queue_dir, queue_id)


def sync_back_spec(queue_dir: str, queue_id: str) -> bool:
    """Copy the queue's spec copy back to the original location.

    Only runs if sync_back is True in the queue entry.
    Returns True if sync happened, False otherwise.
    """
    entry = _read_entry(queue_dir, queue_id)
    if entry is None:
        return False

    if not entry.get("sync_back", True):
        return False

    spec_path = entry.get("spec_path", "")
    original_path = entry.get("original_spec_path", "")

    if not spec_path or not original_path:
        return False

    if not os.path.isfile(spec_path):
        return False

    # Don't sync if spec_path and original_path are the same
    if os.path.abspath(spec_path) == os.path.abspath(original_path):
        return False

    try:
        shutil.copy2(spec_path, original_path)
        return True
    except OSError:
        return False


def fail(queue_dir: str, queue_id: str, reason: str = "") -> None:
    """Set a spec's status to 'failed'."""
    with queue_lock(queue_dir):
        entry = _read_entry(queue_dir, queue_id)
        if entry is None:
            raise ValueError(f"Queue entry not found: {queue_id}")

        entry["status"] = "failed"
        if reason:
            entry["failure_reason"] = reason

        _write_entry(queue_dir, entry)


def record_failure(queue_dir: str, queue_id: str) -> bool:
    """Increment consecutive failure count and apply cooldown.

    Returns True if max consecutive failures exceeded.
    Sets cooldown_until to prevent immediate retry after a crash.
    """
    with queue_lock(queue_dir):
        entry = _read_entry(queue_dir, queue_id)
        if entry is None:
            raise ValueError(f"Queue entry not found: {queue_id}")

        entry["consecutive_failures"] = entry.get("consecutive_failures", 0) + 1

        # Apply cooldown: wait COOLDOWN_SECONDS before retrying
        from datetime import timedelta

        cooldown_end = datetime.now(timezone.utc) + timedelta(seconds=COOLDOWN_SECONDS)
        entry["cooldown_until"] = cooldown_end.isoformat()

        _write_entry(queue_dir, entry)

        return entry["consecutive_failures"] >= MAX_CONSECUTIVE_FAILURES


def cancel(queue_dir: str, queue_id: str) -> None:
    """Set a spec's status to 'canceled'."""
    with queue_lock(queue_dir):
        entry = _read_entry(queue_dir, queue_id)
        if entry is None:
            raise ValueError(f"Queue entry not found: {queue_id}")

        entry["status"] = "canceled"
        _write_entry(queue_dir, entry)


def purge(
    queue_dir: str,
    log_dir: str,
    statuses: list[str] | None = None,
    dry_run: bool = False,
) -> list[dict[str, Any]]:
    """Remove queue artifacts for specs matching the given statuses.

    Args:
        queue_dir: Path to the queue directory.
        log_dir: Path to the log directory.
        statuses: List of statuses to purge. Defaults to completed/failed/canceled.
        dry_run: If True, return what would be removed without deleting.

    Returns:
        List of dicts describing each purged entry (id, status, files_removed).
    """
    if statuses is None:
        statuses = ["completed", "failed", "canceled"]

    with queue_lock(queue_dir):
        entries = get_queue(queue_dir)
        purged: list[dict[str, Any]] = []

        for entry in entries:
            if entry.get("status") not in statuses:
                continue

            queue_id = entry["id"]
            files_removed: list[str] = []
            path = Path(queue_dir)
            log_path = Path(log_dir) if log_dir else None

            # Queue entry file
            entry_file = path / _queue_filename(queue_id)
            if entry_file.is_file():
                files_removed.append(str(entry_file))

            # Spec copy file
            spec_copy = path / f"{queue_id}.spec.md"
            if spec_copy.is_file():
                files_removed.append(str(spec_copy))

            # Telemetry file
            telemetry_file = path / f"{queue_id}.telemetry.json"
            if telemetry_file.is_file():
                files_removed.append(str(telemetry_file))

            # Iteration files
            for f in sorted(path.iterdir()):
                if f.name.startswith(f"{queue_id}.iteration-") and f.name.endswith(
                    ".json"
                ):
                    files_removed.append(str(f))

            # PID and exit files
            for suffix in [".pid", ".exit"]:
                extra = path / f"{queue_id}{suffix}"
                if extra.is_file():
                    files_removed.append(str(extra))

            # Log files
            if log_path and log_path.is_dir():
                for f in sorted(log_path.iterdir()):
                    if f.name.startswith(f"{queue_id}-iter-") and f.name.endswith(
                        ".log"
                    ):
                        files_removed.append(str(f))

            if not dry_run:
                for fp in files_removed:
                    try:
                        os.remove(fp)
                    except OSError:
                        pass

            purged.append(
                {
                    "id": queue_id,
                    "status": entry.get("status", "unknown"),
                    "spec_name": _spec_name_from_entry(entry),
                    "tasks_total": entry.get("tasks_total", 0),
                    "iteration": entry.get("iteration", 0),
                    "files_removed": files_removed,
                }
            )

        return purged


def get_queue(queue_dir: str) -> list[dict[str, Any]]:
    """Get all queue entries sorted by priority (lower = higher priority)."""
    path = Path(queue_dir)
    if not path.is_dir():
        return []

    entries = []
    for f in sorted(path.iterdir()):
        if not f.name.startswith("q-") or not f.name.endswith(".json"):
            continue
        # Skip telemetry and iteration files
        if ".telemetry" in f.name or ".iteration-" in f.name:
            continue
        try:
            data = json.loads(f.read_text(encoding="utf-8"))
            if "id" in data:
                entries.append(data)
        except (json.JSONDecodeError, OSError):
            continue

    entries.sort(key=lambda e: e.get("priority", DEFAULT_PRIORITY))
    return entries


def get_entry(queue_dir: str, queue_id: str) -> Optional[dict[str, Any]]:
    """Get a single queue entry by ID."""
    return _read_entry(queue_dir, queue_id)


def update_task_counts(
    queue_dir: str, queue_id: str, tasks_done: int, tasks_total: int
) -> None:
    """Update the task progress counts for a queue entry."""
    with queue_lock(queue_dir):
        entry = _read_entry(queue_dir, queue_id)
        if entry is None:
            raise ValueError(f"Queue entry not found: {queue_id}")

        entry["tasks_done"] = tasks_done
        entry["tasks_total"] = tasks_total
        _write_entry(queue_dir, entry)


def _is_pid_alive(pid: int) -> bool:
    """Check if a process with the given PID is alive."""
    try:
        os.kill(pid, 0)
        return True
    except (OSError, ProcessLookupError):
        return False


def recover_running_specs(queue_dir: str) -> int:
    """Recover specs stuck in 'running' status after a daemon crash.

    Scans all queue entries. For each with status 'running':
      - If the worker PID file exists and process is alive: skip (daemon monitors it)
      - If PID is dead or missing: reset to 'requeued', increment iteration, log warning

    Returns the count of recovered specs.
    """
    import sys

    recovered = 0
    with queue_lock(queue_dir):
        entries = get_queue(queue_dir)
        for entry in entries:
            if entry.get("status") != "running":
                continue

            queue_id = entry["id"]
            pid_file = os.path.join(queue_dir, f"{queue_id}.pid")
            pid_alive = False

            if os.path.isfile(pid_file):
                try:
                    pid_str = Path(pid_file).read_text(encoding="utf-8").strip()
                    pid = int(pid_str)
                    pid_alive = _is_pid_alive(pid)
                except (ValueError, OSError):
                    pid_alive = False

            if pid_alive:
                continue

            # PID dead or missing: recover this spec
            entry["status"] = "requeued"
            entry["iteration"] = entry.get("iteration", 0) + 1
            _write_entry(queue_dir, entry)
            recovered += 1
            print(
                f"Warning: Recovered stuck spec {queue_id} "
                f"(was 'running', reset to 'requeued', iteration {entry['iteration']})",
                file=sys.stderr,
            )

    return recovered


def get_experiment_budget(mode: str) -> int:
    """Return the default experiment budget for a mode."""
    return DEFAULT_EXPERIMENT_BUDGETS.get(mode, 0)


def set_experiment_budget(
    queue_dir: str,
    queue_id: str,
    max_invocations: int,
) -> None:
    """Set the experiment budget fields on a queue entry."""
    with queue_lock(queue_dir):
        entry = _read_entry(queue_dir, queue_id)
        if entry is None:
            raise ValueError(f"Queue entry not found: {queue_id}")

        entry["max_experiment_invocations"] = max_invocations
        entry["experiment_invocations_used"] = entry.get(
            "experiment_invocations_used", 0
        )
        _write_entry(queue_dir, entry)


def increment_experiment_usage(
    queue_dir: str,
    queue_id: str,
    count: int = 1,
) -> dict[str, int]:
    """Increment experiment_invocations_used for a queue entry.

    Returns dict with max_experiment_invocations, experiment_invocations_used,
    and remaining budget.
    """
    with queue_lock(queue_dir):
        entry = _read_entry(queue_dir, queue_id)
        if entry is None:
            raise ValueError(f"Queue entry not found: {queue_id}")

        used = entry.get("experiment_invocations_used", 0) + count
        entry["experiment_invocations_used"] = used
        max_budget = entry.get("max_experiment_invocations", 0)
        _write_entry(queue_dir, entry)

        return {
            "max_experiment_invocations": max_budget,
            "experiment_invocations_used": used,
            "remaining": max(0, max_budget - used),
        }
