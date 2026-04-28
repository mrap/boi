# db_to_json.py — Export SQLite specs back to JSON queue files.
#
# Inverse of Database.migrate_from_json(). Reads all specs from
# SQLite and writes each as a q-NNN.json file in the queue directory,
# using the same format as lib/queue.py. Enables rollback from the
# Python daemon back to the bash daemon.

import json
import os
from pathlib import Path
from typing import Any, Optional

from lib.db import Database


def export_queue_to_json(
    db: Database,
    queue_dir: Optional[str] = None,
) -> int:
    """Export all specs from SQLite to q-NNN.json files.

    Reads every row in the specs table, converts it to the JSON
    format used by lib/queue.py, and writes it to the queue
    directory as q-NNN.json. Also queries spec_dependencies to
    populate the blocked_by field.

    Args:
        db: An open Database instance.
        queue_dir: Directory to write JSON files. Defaults to
            db.queue_dir.

    Returns:
        Number of specs exported.
    """
    out_dir = Path(queue_dir or db.queue_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    specs = db.get_queue()
    exported = 0

    for spec in specs:
        entry = _spec_row_to_json(db, spec)
        _write_entry(out_dir, entry)
        exported += 1

    return exported


def _spec_row_to_json(db: Database, spec: dict[str, Any]) -> dict[str, Any]:
    """Convert a SQLite spec row dict to the JSON queue format.

    Maps SQLite column names and types back to the JSON schema
    used by lib/queue.py. Key differences handled:
      - sync_back: int (0/1) in SQLite -> bool in JSON
      - initial_task_ids: JSON string in SQLite -> list in JSON
      - pre_iteration_tasks: JSON string in SQLite -> dict in JSON
      - blocked_by: separate table in SQLite -> list in JSON
      - experiment_tasks: JSON string in SQLite -> list in JSON
    """
    sid = spec["id"]

    # Query dependencies from spec_dependencies table
    blocked_by: list[str] = []
    cursor = db.conn.execute(
        "SELECT blocks_on FROM spec_dependencies WHERE spec_id = ?",
        (sid,),
    )
    for row in cursor:
        blocked_by.append(row["blocks_on"])

    # Parse JSON-encoded fields
    initial_task_ids = _parse_json_field(
        spec.get("initial_task_ids"), default=[]
    )
    pre_iteration_tasks = _parse_json_field(
        spec.get("pre_iteration_tasks"), default=None
    )
    experiment_tasks = _parse_json_field(
        spec.get("experiment_tasks"), default=None
    )

    entry: dict[str, Any] = {
        "id": sid,
        "spec_path": spec.get("spec_path", ""),
        "original_spec_path": spec.get("original_spec_path"),
        "worktree": spec.get("worktree"),
        "priority": spec.get("priority", 100),
        "status": spec.get("status", "queued"),
        "phase": spec.get("phase", "execute"),
        "submitted_at": spec.get("submitted_at", ""),
        "iteration": spec.get("iteration", 0),
        "max_iterations": spec.get("max_iterations", 30),
        "blocked_by": blocked_by,
        "last_worker": spec.get("last_worker"),
        "last_iteration_at": spec.get("last_iteration_at"),
        "first_running_at": spec.get("first_running_at"),
        "consecutive_failures": spec.get("consecutive_failures", 0),
        "cooldown_until": spec.get("cooldown_until"),
        "tasks_done": spec.get("tasks_done", 0),
        "tasks_total": spec.get("tasks_total", 0),
        "sync_back": bool(spec.get("sync_back", 1)),
        "project": spec.get("project"),
        "initial_task_ids": initial_task_ids,
    }

    # Include optional fields only if they have values,
    # matching how queue.py writes them.
    if spec.get("worker_timeout_seconds") is not None:
        entry["worker_timeout_seconds"] = spec["worker_timeout_seconds"]
    if spec.get("failure_reason") is not None:
        entry["failure_reason"] = spec["failure_reason"]
    if spec.get("needs_review_since") is not None:
        entry["needs_review_since"] = spec["needs_review_since"]
    if spec.get("assigning_at") is not None:
        entry["assigning_at"] = spec["assigning_at"]
    if spec.get("critic_passes") is not None and spec["critic_passes"] != 0:
        entry["critic_passes"] = spec["critic_passes"]
    if pre_iteration_tasks is not None:
        entry["pre_iteration_tasks"] = pre_iteration_tasks
    if experiment_tasks is not None:
        entry["experiment_tasks"] = experiment_tasks
    if spec.get("max_experiment_invocations", 0) != 0:
        entry["max_experiment_invocations"] = spec[
            "max_experiment_invocations"
        ]
    if spec.get("experiment_invocations_used", 0) != 0:
        entry["experiment_invocations_used"] = spec[
            "experiment_invocations_used"
        ]
    if spec.get("decomposition_retries", 0) != 0:
        entry["decomposition_retries"] = spec["decomposition_retries"]

    return entry


def _parse_json_field(value: Any, default: Any = None) -> Any:
    """Parse a JSON-encoded string field, returning default on failure."""
    if value is None:
        return default
    if isinstance(value, (list, dict)):
        return value
    try:
        return json.loads(value)
    except (json.JSONDecodeError, TypeError):
        return default


def _write_entry(queue_dir: Path, entry: dict[str, Any]) -> None:
    """Write a queue entry as q-NNN.json atomically."""
    filename = f"{entry['id']}.json"
    target = queue_dir / filename
    tmp = queue_dir / f".{filename}.tmp"

    data = json.dumps(entry, indent=2, sort_keys=False) + "\n"
    tmp.write_text(data, encoding="utf-8")
    os.rename(str(tmp), str(target))
