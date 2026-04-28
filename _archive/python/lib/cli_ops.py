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
from lib.runtime import get_all_runtimes


def _get_db(queue_dir: str) -> Database:
    """Create a Database instance from queue_dir.

    The DB file lives at <state_dir>/boi.db where state_dir
    is the parent of queue_dir.
    """
    state_dir = str(Path(queue_dir).parent)
    db_path = os.path.join(state_dir, "boi.db")
    return Database(db_path, queue_dir)


def _inject_workspace_header_if_missing(spec_path: str, queue_dir: str) -> None:
    """Inject workspace header into queue copy when absent.

    YAML specs receive a YAML-format key (workspace: /path) so that
    content_is_yaml() continues to detect them correctly after injection.
    Markdown specs receive the legacy **Workspace:** bold header.
    """
    from lib.runtime import load_context_root
    from lib.spec_parser import content_is_yaml

    state_dir = str(Path(queue_dir).parent)
    context_root = load_context_root(state_dir)
    if not context_root:
        return
    p = Path(spec_path)
    content = p.read_text(encoding="utf-8")
    if "**Workspace:**" in content or content.startswith("workspace:") or "\nworkspace:" in content:
        return
    if content_is_yaml(content):
        p.write_text(f"workspace: {context_root}\n{content}", encoding="utf-8")
    else:
        lines = content.splitlines(keepends=True)
        insert_at = next((i + 1 for i, l in enumerate(lines) if l.startswith("#")), 0)
        lines.insert(insert_at, f"\n**Workspace:** {context_root}\n")
        p.write_text("".join(lines), encoding="utf-8")


def _check_initiative_linkage(spec_path: str) -> tuple[bool, bool, str]:
    """Check whether a spec links to an initiative or experiment.

    Returns (allowed, is_emergency, reason).
    - allowed=True means dispatch should proceed.
    - is_emergency=True means bypass was used (for audit logging).
    - reason is a human-readable explanation.

    Supported fields (markdown):
        **Initiative:** init-<id>
        **Experiment:** exp-NNN
        **Emergency:** true

    Supported fields (YAML top-level keys):
        initiative: init-<id>
        experiment: exp-NNN
        emergency: true
    """
    import re

    try:
        content = Path(spec_path).read_text(encoding="utf-8")
    except OSError:
        return True, False, "spec file unreadable — skipping linkage check"

    # Case-insensitive patterns for both markdown bold-field and YAML key formats.
    emergency_md = re.search(r"^\*\*Emergency:\*\*\s*true\b", content, re.MULTILINE | re.IGNORECASE)
    emergency_yaml = re.search(r"^emergency:\s*true\b", content, re.MULTILINE | re.IGNORECASE)
    if emergency_md or emergency_yaml:
        return True, True, "emergency bypass"

    initiative_md = re.search(r"^\*\*Initiative:\*\*\s*\S+", content, re.MULTILINE | re.IGNORECASE)
    initiative_yaml = re.search(r"^initiative:\s*\S+", content, re.MULTILINE | re.IGNORECASE)
    experiment_md = re.search(r"^\*\*Experiment:\*\*\s*\S+", content, re.MULTILINE | re.IGNORECASE)
    experiment_yaml = re.search(r"^experiment:\s*\S+", content, re.MULTILINE | re.IGNORECASE)

    if initiative_md or initiative_yaml or experiment_md or experiment_yaml:
        return True, False, "linked"

    return False, False, "no initiative or experiment linkage found"


def _emit_dispatched_event(spec_id: str, source: str, spec_path: str = "", is_emergency: bool = False) -> None:
    """Fire-and-forget: emit boi.spec.dispatched to hex-events.

    Silently ignores all errors — BOI must never fail because hex-events
    is down or missing.
    """
    try:
        import json as _json

        hex_emit = os.path.expanduser("~/.hex-events/hex_emit.py")
        if not os.path.exists(hex_emit):
            return

        payload = {"spec_id": spec_id, "source": source, "spec_file": spec_path, "emergency": is_emergency}
        subprocess.Popen(
            ["python3", hex_emit, "boi.spec.dispatched", _json.dumps(payload), source],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    except Exception:
        pass


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
    source: str = "cli",
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

    # Enforce initiative/experiment linkage before touching the DB.
    allowed, is_emergency, reason = _check_initiative_linkage(spec_path)
    if not allowed:
        raise ValueError(
            "Spec must link to an initiative or experiment.\n\n"
            "Add one of these to your spec header:\n"
            "  **Initiative:** init-<id>      (markdown)\n"
            "  **Experiment:** exp-NNN        (markdown)\n"
            "  initiative: init-<id>          (YAML)\n"
            "  experiment: exp-NNN            (YAML)\n\n"
            "To bypass for emergency fixes, add:\n"
            "  **Emergency:** true            (markdown)\n"
            "  emergency: true                (YAML)\n\n"
            "Emergency bypasses are audited. The work must be retroactively\n"
            "linked to an initiative within 48h or flagged as an orphan.\n\n"
            "Active initiatives: python3 $AGENT_DIR/.hex/scripts/hex-initiative.py list\n"
            "Active experiments: python3 $AGENT_DIR/.hex/scripts/hex-experiment.py list"
        )

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
        _inject_workspace_header_if_missing(entry["spec_path"], queue_dir)

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

        _emit_dispatched_event(spec_id, source, spec_path=entry["spec_path"], is_emergency=is_emergency)

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
            is_worker = any(rt.detect_worker_process(line) for rt in get_all_runtimes())
            if is_worker and "BOI Worker" in line:
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


def unblock_spec(queue_dir: str, spec_id: str, reason: Optional[str] = None) -> str:
    """Unblock a spec and return it to queued status.

    Calls db.unblock_spec() to reset status from 'blocked' to 'queued'.
    Returns the spec_id on success.
    Raises ValueError if spec not found.
    """
    db = _get_db(queue_dir)
    try:
        db.unblock_spec(spec_id, reason)
        return spec_id
    finally:
        db.close()


def get_blocked_specs(queue_dir: str) -> list[dict[str, Any]]:
    """Get all specs with status 'blocked'.

    Returns list of spec dicts including blocked_reason and blocked_at.
    """
    db = _get_db(queue_dir)
    try:
        all_specs = db.get_queue()
        return [s for s in all_specs if s.get("status") == "blocked"]
    finally:
        db.close()


def get_blocked_spec_details(queue_dir: str, spec_id: str) -> dict[str, Any]:
    """Get detailed information about a blocked spec.

    Returns dict with spec info including:
    - blocked_reason
    - blocked_at
    - last_progress_iteration
    - consecutive zero-progress count (iteration - last_progress_iteration)

    Raises ValueError if spec not found.
    """
    db = _get_db(queue_dir)
    try:
        spec = db.get_spec(spec_id)
        if spec is None:
            raise ValueError(f"Spec not found: {spec_id}")

        iteration = spec.get("iteration", 0)
        last_progress = spec.get("last_progress_iteration", 0)

        return {
            "spec_id": spec_id,
            "status": spec.get("status"),
            "blocked_reason": spec.get("blocked_reason"),
            "blocked_at": spec.get("blocked_at"),
            "iteration": iteration,
            "last_progress_iteration": last_progress,
            "zero_progress_count": iteration - last_progress,
            "tasks_done": spec.get("tasks_done", 0),
            "tasks_total": spec.get("tasks_total", 0),
        }
    finally:
        db.close()
