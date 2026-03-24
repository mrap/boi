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
import signal
import subprocess
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
    blocked_by: Optional[list[str]] = None,
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
            blocked_by=blocked_by or None,
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
            "queued",
            "running",
            "requeued",
            "completed",
            "failed",
            "canceled",
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
                    if f.name.startswith(f"{sid}-iter-") and f.name.endswith(".log"):
                        result["files_removed"].append(str(f))
                        if not dry_run:
                            try:
                                os.remove(str(f))
                            except OSError:
                                pass

        return results
    finally:
        db.close()


def add_dependency(
    queue_dir: str,
    spec_id: str,
    dep_ids: list[str],
) -> dict[str, Any]:
    """Add one or more post-dispatch dependencies to a spec.

    Returns dict with spec_id and list of successfully added dep IDs.
    Raises ValueError on missing specs or circular dependencies.
    """
    db = _get_db(queue_dir)
    try:
        added = []
        for dep_id in dep_ids:
            db.add_dependency(spec_id, dep_id)
            added.append(dep_id)
        return {"spec_id": spec_id, "added": added}
    finally:
        db.close()


def remove_dependency(
    queue_dir: str,
    spec_id: str,
    dep_ids: list[str],
) -> dict[str, Any]:
    """Remove one or more dependencies from a spec.

    Returns dict with spec_id and list of dep IDs passed for removal.
    Raises ValueError if spec_id does not exist.
    """
    db = _get_db(queue_dir)
    try:
        removed = []
        for dep_id in dep_ids:
            db.remove_dependency(spec_id, dep_id)
            removed.append(dep_id)
        return {"spec_id": spec_id, "removed": removed}
    finally:
        db.close()


def replace_dependencies(
    queue_dir: str,
    spec_id: str,
    dep_ids: list[str],
) -> dict[str, Any]:
    """Atomically replace all dependencies for a spec.

    Returns dict with spec_id and the new dep list.
    Raises ValueError on missing specs or circular dependencies.
    """
    db = _get_db(queue_dir)
    try:
        db.replace_dependencies(spec_id, dep_ids)
        return {"spec_id": spec_id, "deps": dep_ids}
    finally:
        db.close()


def clear_dependencies(
    queue_dir: str,
    spec_id: str,
) -> dict[str, Any]:
    """Remove all dependencies from a spec.

    Returns dict with spec_id and count of cleared deps.
    Raises ValueError if spec not found.
    """
    db = _get_db(queue_dir)
    try:
        count = db.clear_dependencies(spec_id)
        return {"spec_id": spec_id, "cleared": count}
    finally:
        db.close()


def get_fleet_dag(queue_dir: str) -> dict[str, Any]:
    """Return the full fleet dependency DAG.

    Returns dict with specs and edges.
    """
    db = _get_db(queue_dir)
    try:
        return db.get_fleet_dag()
    finally:
        db.close()


def check_fleet_dag(queue_dir: str) -> list[dict[str, str]]:
    """Validate the fleet DAG for issues.

    Returns list of issue dicts.
    """
    db = _get_db(queue_dir)
    try:
        return db.check_fleet_dag()
    finally:
        db.close()


def resume_spec(queue_dir: str, queue_id: str) -> list[str]:
    """Resume failed/canceled specs back to queued.

    Resets status to 'queued', clears consecutive_failures and
    failure_reason, but preserves iteration count and tasks_done.

    If queue_id is '--all', resumes ALL failed specs.
    Returns list of resumed spec IDs.
    Raises ValueError if spec not found or not in a resumable state.
    """
    RESUMABLE = {"failed", "canceled"}
    db = _get_db(queue_dir)
    try:
        if queue_id == "--all":
            specs = db.get_queue()
            failed = [s for s in specs if s["status"] in RESUMABLE]
            resumed = []
            for spec in failed:
                db.update_spec_fields(
                    spec["id"],
                    status="queued",
                    consecutive_failures=0,
                    failure_reason=None,
                )
                resumed.append(spec["id"])
            return resumed

        spec = db.get_spec(queue_id)
        if spec is None:
            raise ValueError(f"Spec not found: {queue_id}")
        if spec["status"] not in RESUMABLE:
            raise ValueError(
                f"Cannot resume spec '{queue_id}' with status "
                f"'{spec['status']}'. Only failed or canceled specs "
                "can be resumed."
            )
        db.update_spec_fields(
            queue_id,
            status="queued",
            consecutive_failures=0,
            failure_reason=None,
        )
        return [queue_id]
    finally:
        db.close()


def stop_all_workers(queue_dir: str, force: bool = False) -> list[int]:
    """Kill all worker processes tracked in the DB.

    Sends SIGTERM (or SIGKILL if force=True) to every worker
    with a current_pid. Returns list of PIDs that were signaled.
    """
    sig = signal.SIGKILL if force else signal.SIGTERM
    db = _get_db(queue_dir)
    try:
        workers = db.get_all_workers()
        killed: list[int] = []
        for w in workers:
            pid = w.get("current_pid")
            if pid is not None:
                try:
                    os.kill(pid, sig)
                    killed.append(pid)
                except ProcessLookupError:
                    pass
        return killed
    finally:
        db.close()


def cleanup_orphans(queue_dir: str) -> list[int]:
    """Find and kill orphaned BOI worker processes not tracked in the DB.

    Scans running processes for the BOI Worker pattern, cross-references
    against tracked PIDs in the workers table, and kills any that are
    untracked.

    Returns list of orphaned PIDs that were killed.
    """
    db = _get_db(queue_dir)
    try:
        workers = db.get_all_workers()
        tracked_pids = {
            w["current_pid"] for w in workers if w.get("current_pid") is not None
        }

        try:
            ps_output = subprocess.check_output(
                ["ps", "ax", "-o", "pid,args"],
                text=True,
            )
        except (subprocess.CalledProcessError, FileNotFoundError):
            return []

        orphans: list[int] = []
        for line in ps_output.strip().split("\n"):
            line = line.strip()
            if "claude" in line and "BOI Worker" in line:
                parts = line.split(None, 1)
                if parts:
                    try:
                        pid = int(parts[0])
                    except ValueError:
                        continue
                    if pid not in tracked_pids:
                        try:
                            os.kill(pid, signal.SIGTERM)
                            orphans.append(pid)
                        except ProcessLookupError:
                            pass
        return orphans
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
