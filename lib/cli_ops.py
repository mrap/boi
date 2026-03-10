# cli_ops.py — Thin CLI operations layer for boi.sh.
#
# Provides dispatch, cancel, and purge operations backed by SQLite.
# Called from boi.sh via inline Python heredocs.
#
# Each function creates its own Database instance, performs the
# operation, and closes the connection. This is appropriate for
# short-lived CLI calls (not long-running daemon use).

import json
import os
from pathlib import Path
from typing import Any, Optional

from lib.db import Database, DuplicateSpecError
from lib.db_to_json import export_queue_to_json


def _get_db(queue_dir: str) -> Database:
    """Create a Database instance from queue_dir.

    The DB file lives at <state_dir>/boi.db where state_dir
    is the parent of queue_dir.
    """
    state_dir = str(Path(queue_dir).parent)
    db_path = os.path.join(state_dir, "boi.db")
    return Database(db_path, queue_dir)


def dispatch(
    queue_dir: str,
    spec_path: str,
    priority: int = 100,
    max_iterations: int = 30,
    checkout: Optional[str] = None,
    timeout: Optional[int] = None,
    mode: str = "execute",
    project: Optional[str] = None,
    experiment_budget: Optional[int] = None,
) -> dict[str, Any]:
    """Enqueue a spec into the SQLite database.

    Handles the full dispatch flow: enqueue, set phase based on
    spec type, update task counts, set experiment budget and timeout.

    Returns a dict with: id, tasks, pending, mode, phase.
    Raises DuplicateSpecError if the same spec is already active.
    """
    from lib.queue import get_experiment_budget
    from lib.spec_parser import count_boi_tasks
    from lib.spec_validator import is_generate_spec

    db = _get_db(queue_dir)
    try:
        counts = count_boi_tasks(spec_path)

        entry = db.enqueue(
            spec_path=spec_path,
            priority=priority,
            max_iterations=max_iterations,
            checkout=checkout,
            project=project,
        )

        spec_id = entry["id"]

        # Determine phase from spec type
        spec_content = Path(entry["spec_path"]).read_text(encoding="utf-8")
        phase = "decompose" if is_generate_spec(spec_content) else "execute"

        # Build update fields for post-enqueue configuration
        updates: dict[str, Any] = {
            "phase": phase,
            "tasks_done": counts["done"],
            "tasks_total": counts["total"],
        }

        if timeout is not None:
            updates["worker_timeout_seconds"] = timeout

        if experiment_budget is not None:
            updates["max_experiment_invocations"] = experiment_budget
        else:
            updates["max_experiment_invocations"] = get_experiment_budget(mode)
        updates["experiment_invocations_used"] = 0

        db.update_spec_fields(spec_id, **updates)

        return {
            "id": spec_id,
            "tasks": counts["total"],
            "pending": counts["pending"],
            "mode": mode,
            "phase": phase,
        }
    finally:
        db.close()


def cancel_spec(queue_dir: str, queue_id: str) -> str:
    """Cancel a spec in the SQLite database.

    Returns the queue_id on success.
    Raises ValueError if spec not found.
    """
    db = _get_db(queue_dir)
    try:
        db.cancel(queue_id)
        return queue_id
    finally:
        db.close()


def purge_specs(
    queue_dir: str,
    log_dir: str,
    all_mode: bool = False,
    dry_run: bool = False,
) -> list[dict[str, Any]]:
    """Purge specs from the SQLite database.

    Removes spec rows and associated files (queue dir artifacts
    and log files). Returns list of purged spec descriptions.
    """
    if all_mode:
        statuses = [
            "queued", "running", "requeued",
            "completed", "failed", "canceled",
        ]
    else:
        statuses = ["completed", "failed", "canceled"]

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


def export_db(queue_dir: str) -> int:
    """Export all specs from SQLite to q-NNN.json files.

    Returns the number of specs exported.
    """
    db = _get_db(queue_dir)
    try:
        return export_queue_to_json(db, queue_dir)
    finally:
        db.close()


def migrate_db(
    queue_dir: str,
    events_dir: Optional[str] = None,
) -> dict[str, int]:
    """Migrate JSON queue and event files to SQLite.

    Reads q-*.json from queue_dir, event-*.json from events_dir,
    imports into SQLite, and archives the originals.

    Returns dict with counts: specs, events.
    """
    from lib.db_migrate import migrate_queue_to_db

    db = _get_db(queue_dir)
    try:
        return migrate_queue_to_db(db, queue_dir, events_dir)
    finally:
        db.close()
