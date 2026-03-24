# queue_compat.py -- Compatibility layer for BOI queue operations.
#
# Delegates to SQLite (lib.db.Database) when boi.db exists in the
# state directory, otherwise falls back to JSON (lib.queue).
# This allows a gradual migration: existing code can import from
# queue_compat and transparently use whichever backend is active.
#
# The JSON backend (queue.py) is preserved unchanged for rollback.

import os
from pathlib import Path
from typing import Any, Optional


def _use_sqlite(queue_dir: str) -> bool:
    """Check if the SQLite backend is available."""
    state_dir = str(Path(queue_dir).parent)
    db_path = os.path.join(state_dir, "boi.db")
    return os.path.isfile(db_path)


def _get_db(queue_dir: str) -> "Database":
    """Create a Database instance for the given queue_dir."""
    from lib.db import Database

    state_dir = str(Path(queue_dir).parent)
    db_path = os.path.join(state_dir, "boi.db")
    return Database(db_path, queue_dir)


# Re-export DuplicateSpecError from the active backend.
# Both queue.py and db.py define this class with the same interface.
def _get_duplicate_spec_error() -> type:
    """Return the DuplicateSpecError class from the appropriate module."""
    try:
        from lib.db import DuplicateSpecError
        return DuplicateSpecError
    except ImportError:
        from lib.queue import DuplicateSpecError
        return DuplicateSpecError


# Make DuplicateSpecError importable directly from this module.
from lib.queue import DuplicateSpecError  # noqa: E402


# -- Queue read operations -----------------------------------------------


def get_queue(queue_dir: str) -> list[dict[str, Any]]:
    """Get all queue entries sorted by priority."""
    if _use_sqlite(queue_dir):
        db = _get_db(queue_dir)
        try:
            return db.get_queue()
        finally:
            db.close()

    from lib.queue import get_queue as _json_get_queue
    return _json_get_queue(queue_dir)


def get_entry(queue_dir: str, queue_id: str) -> Optional[dict[str, Any]]:
    """Get a single queue entry by ID."""
    if _use_sqlite(queue_dir):
        db = _get_db(queue_dir)
        try:
            return db.get_spec(queue_id)
        finally:
            db.close()

    from lib.queue import get_entry as _json_get_entry
    return _json_get_entry(queue_dir, queue_id)


# -- Queue write operations -----------------------------------------------


def enqueue(
    queue_dir: str,
    spec_path: str,
    priority: int = 100,
    max_iterations: int = 30,
    blocked_by: Optional[list[str]] = None,
    checkout: Optional[str] = None,
    queue_id: Optional[str] = None,
    sync_back: bool = True,
    project: Optional[str] = None,
) -> dict[str, Any]:
    """Add a spec to the queue."""
    if _use_sqlite(queue_dir):
        db = _get_db(queue_dir)
        try:
            return db.enqueue(
                spec_path=spec_path,
                priority=priority,
                max_iterations=max_iterations,
                blocked_by=blocked_by,
                checkout=checkout,
                queue_id=queue_id,
                sync_back=sync_back,
                project=project,
            )
        finally:
            db.close()

    from lib.queue import enqueue as _json_enqueue
    return _json_enqueue(
        queue_dir,
        spec_path,
        priority=priority,
        max_iterations=max_iterations,
        blocked_by=blocked_by,
        checkout=checkout,
        queue_id=queue_id,
        sync_back=sync_back,
        project=project,
    )


def dequeue(
    queue_dir: str, blocked_ids: Optional[set[str]] = None
) -> Optional[dict[str, Any]]:
    """Pick the next eligible spec from the queue."""
    if _use_sqlite(queue_dir):
        db = _get_db(queue_dir)
        try:
            return db.pick_next_spec()
        finally:
            db.close()

    from lib.queue import dequeue as _json_dequeue
    return _json_dequeue(queue_dir, blocked_ids)


def set_running(queue_dir: str, queue_id: str, worker_id: str) -> None:
    """Set a spec's status to 'running' and record the worker."""
    if _use_sqlite(queue_dir):
        db = _get_db(queue_dir)
        try:
            db.set_running(queue_id, worker_id)
            return
        finally:
            db.close()

    from lib.queue import set_running as _json_set_running
    _json_set_running(queue_dir, queue_id, worker_id)


def requeue(
    queue_dir: str, queue_id: str,
    tasks_done: int = 0, tasks_total: int = 0,
) -> None:
    """Set a spec's status back to 'requeued'."""
    if _use_sqlite(queue_dir):
        db = _get_db(queue_dir)
        try:
            db.requeue(queue_id, tasks_done=tasks_done, tasks_total=tasks_total)
            return
        finally:
            db.close()

    from lib.queue import requeue as _json_requeue
    _json_requeue(queue_dir, queue_id, tasks_done=tasks_done, tasks_total=tasks_total)


def complete(
    queue_dir: str, queue_id: str,
    tasks_done: int = 0, tasks_total: int = 0,
) -> None:
    """Set a spec's status to 'completed'."""
    if _use_sqlite(queue_dir):
        db = _get_db(queue_dir)
        try:
            db.complete(queue_id, tasks_done=tasks_done, tasks_total=tasks_total)
            return
        finally:
            db.close()

    from lib.queue import complete as _json_complete
    _json_complete(queue_dir, queue_id, tasks_done=tasks_done, tasks_total=tasks_total)


def fail(queue_dir: str, queue_id: str, reason: str = "") -> None:
    """Set a spec's status to 'failed'."""
    if _use_sqlite(queue_dir):
        db = _get_db(queue_dir)
        try:
            db.fail(queue_id, reason=reason)
            return
        finally:
            db.close()

    from lib.queue import fail as _json_fail
    _json_fail(queue_dir, queue_id, reason=reason)


def record_failure(queue_dir: str, queue_id: str) -> bool:
    """Increment consecutive failure count and apply cooldown.

    Returns True if max consecutive failures exceeded.
    """
    if _use_sqlite(queue_dir):
        db = _get_db(queue_dir)
        try:
            return db.record_failure(queue_id)
        finally:
            db.close()

    from lib.queue import record_failure as _json_record_failure
    return _json_record_failure(queue_dir, queue_id)


def cancel(queue_dir: str, queue_id: str) -> None:
    """Set a spec's status to 'canceled'."""
    if _use_sqlite(queue_dir):
        db = _get_db(queue_dir)
        try:
            db.cancel(queue_id)
            return
        finally:
            db.close()

    from lib.queue import cancel as _json_cancel
    _json_cancel(queue_dir, queue_id)


def purge(
    queue_dir: str,
    log_dir: str,
    statuses: Optional[list[str]] = None,
    dry_run: bool = False,
) -> list[dict[str, Any]]:
    """Remove queue artifacts for specs matching statuses."""
    if _use_sqlite(queue_dir):
        db = _get_db(queue_dir)
        try:
            results = db.purge(statuses=statuses, dry_run=dry_run)
            # Clean up log files (db.purge handles queue dir files only)
            log_path = Path(log_dir) if log_dir else None
            if log_path and log_path.is_dir():
                for result in results:
                    sid = result["id"]
                    for f in sorted(log_path.iterdir()):
                        if (
                            f.name.startswith(f"{sid}-iter-")
                            and f.name.endswith(".log")
                        ):
                            result["files_removed"].append(str(f))
                            if not dry_run:
                                try:
                                    os.remove(str(f))
                                except OSError:
                                    pass
            return results
        finally:
            db.close()

    from lib.queue import purge as _json_purge
    return _json_purge(queue_dir, log_dir, statuses=statuses, dry_run=dry_run)


def set_needs_review(
    queue_dir: str,
    queue_id: str,
    experiment_tasks: list[str],
    tasks_done: int = 0,
    tasks_total: int = 0,
) -> None:
    """Set a spec's status to 'needs_review'."""
    if _use_sqlite(queue_dir):
        db = _get_db(queue_dir)
        try:
            db.set_needs_review(
                queue_id,
                experiment_tasks=experiment_tasks,
                tasks_done=tasks_done,
                tasks_total=tasks_total,
            )
            return
        finally:
            db.close()

    from lib.queue import set_needs_review as _json_set_needs_review
    _json_set_needs_review(
        queue_dir, queue_id,
        experiment_tasks=experiment_tasks,
        tasks_done=tasks_done,
        tasks_total=tasks_total,
    )


def update_task_counts(
    queue_dir: str, queue_id: str,
    tasks_done: int, tasks_total: int,
) -> None:
    """Update the task progress counts for a queue entry."""
    if _use_sqlite(queue_dir):
        db = _get_db(queue_dir)
        try:
            db.update_spec_fields(
                queue_id,
                tasks_done=tasks_done,
                tasks_total=tasks_total,
            )
            return
        finally:
            db.close()

    from lib.queue import update_task_counts as _json_update
    _json_update(queue_dir, queue_id, tasks_done, tasks_total)


def recover_running_specs(queue_dir: str) -> int:
    """Recover specs stuck in 'running' after a daemon crash.

    Returns the count of recovered specs.
    """
    if _use_sqlite(queue_dir):
        db = _get_db(queue_dir)
        try:
            recovered = db.recover_running_specs()
            return len(recovered)
        finally:
            db.close()

    from lib.queue import recover_running_specs as _json_recover
    return _json_recover(queue_dir)


def sync_back_spec(queue_dir: str, queue_id: str) -> bool:
    """Copy the queue's spec copy back to the original location."""
    from lib.queue import sync_back_spec as _json_sync
    return _json_sync(queue_dir, queue_id)


# -- Experiment budget operations -----------------------------------------


def get_experiment_budget(mode: str) -> int:
    """Return the default experiment budget for a mode."""
    from lib.queue import get_experiment_budget as _json_get_budget
    return _json_get_budget(mode)


def set_experiment_budget(
    queue_dir: str, queue_id: str, max_invocations: int,
) -> None:
    """Set the experiment budget fields on a queue entry."""
    if _use_sqlite(queue_dir):
        db = _get_db(queue_dir)
        try:
            db.update_spec_fields(
                queue_id,
                max_experiment_invocations=max_invocations,
            )
            return
        finally:
            db.close()

    from lib.queue import set_experiment_budget as _json_set_budget
    _json_set_budget(queue_dir, queue_id, max_invocations)


def increment_experiment_usage(
    queue_dir: str, queue_id: str, count: int = 1,
) -> dict[str, int]:
    """Increment experiment_invocations_used for a queue entry."""
    if _use_sqlite(queue_dir):
        db = _get_db(queue_dir)
        try:
            return db.increment_experiment_usage(queue_id, count=count)
        finally:
            db.close()

    from lib.queue import increment_experiment_usage as _json_incr
    return _json_incr(queue_dir, queue_id, count)


# -- Private helpers (re-exported for backward compatibility) ---------------
# These are used by some tests and lib modules that poke at internals.
# When SQLite is active, these still operate on JSON files.
# Callers should migrate away from these.


def _read_entry(queue_dir: str, queue_id: str) -> Optional[dict[str, Any]]:
    """Read a single JSON queue entry."""
    from lib.queue import _read_entry as _json_read
    return _json_read(queue_dir, queue_id)


def _write_entry(queue_dir: str, entry: dict[str, Any]) -> None:
    """Write a JSON queue entry atomically."""
    from lib.queue import _write_entry as _json_write
    _json_write(queue_dir, entry)


def _is_pid_alive(pid: int) -> bool:
    """Check if a process with the given PID is alive."""
    from lib.queue import _is_pid_alive as _json_is_pid_alive
    return _json_is_pid_alive(pid)
