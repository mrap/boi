# db.py — SQLite-backed queue and state management for BOI.
#
# Replaces the file-based JSON queue (lib/queue.py) with a single
# SQLite database. All mutable state lives in one place. WAL mode
# enables concurrent readers (boi status) without blocking the
# daemon's writes.
#
# Thread safety: all state-mutating methods acquire self.lock
# before touching the database. Read-only methods do not need
# the lock (WAL mode supports concurrent readers).
#
# All state-mutating methods call _log_event() to record changes.

import json
import os
import shutil
import signal
import sqlite3
import threading
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Optional


# Match existing queue.py constants
DEFAULT_MAX_ITERATIONS = 30
DEFAULT_PRIORITY = 100
MAX_CONSECUTIVE_FAILURES = 5
COOLDOWN_SECONDS = 60


class DuplicateSpecError(Exception):
    """Raised when the same spec is already active in the queue."""

    def __init__(
        self,
        original_spec_path: str,
        existing_id: str,
        existing_status: str,
    ):
        self.original_spec_path = original_spec_path
        self.existing_id = existing_id
        self.existing_status = existing_status
        super().__init__(
            f"Spec '{original_spec_path}' is already in the queue as "
            f"{existing_id} (status: {existing_status}). "
            f"Cancel it first with 'boi cancel {existing_id}' "
            "or wait for it to finish."
        )


class Database:
    """SQLite-backed state store for BOI specs, workers, and events.

    Args:
        db_path: Path to the SQLite database file.
        queue_dir: Path to the queue directory (for spec file copies).
    """

    def __init__(self, db_path: str, queue_dir: str) -> None:
        self.db_path = db_path
        self.queue_dir = queue_dir
        self.lock = threading.Lock()

        Path(queue_dir).mkdir(parents=True, exist_ok=True)

        self.conn = sqlite3.connect(
            db_path,
            check_same_thread=False,
            timeout=30,
        )
        self.conn.row_factory = sqlite3.Row
        self.conn.execute("PRAGMA journal_mode=WAL")
        self.conn.execute("PRAGMA wal_autocheckpoint=10000")
        self.conn.execute("PRAGMA foreign_keys=ON")

        self.init_schema()

    def init_schema(self) -> None:
        """Create tables and indexes from schema.sql if they don't exist."""
        schema_path = Path(__file__).parent / "schema.sql"
        schema_sql = schema_path.read_text(encoding="utf-8")
        self.conn.executescript(schema_sql)

    def close(self) -> None:
        """Close the database connection."""
        self.conn.close()

    # ── Internal helpers ─────────────────────────────────────────────

    def _now_iso(self) -> str:
        """Return current UTC time as ISO-8601 string."""
        return datetime.now(timezone.utc).isoformat()

    def _log_event(
        self,
        event_type: str,
        message: str,
        spec_id: Optional[str] = None,
        data: Optional[dict[str, Any]] = None,
        level: str = "info",
    ) -> None:
        """Insert an event into the events table.

        Must be called inside a transaction (caller holds self.lock).
        """
        self.conn.execute(
            "INSERT INTO events (timestamp, spec_id, event_type, message, data, level) "
            "VALUES (?, ?, ?, ?, ?, ?)",
            (
                self._now_iso(),
                spec_id,
                event_type,
                message,
                json.dumps(data) if data else None,
                level,
            ),
        )

    def _next_queue_id(self) -> str:
        """Generate the next queue ID (q-001, q-002, etc.).

        Scans the specs table for the highest existing numeric suffix
        and returns one higher. Must be called while holding self.lock.
        """
        cursor = self.conn.execute("SELECT id FROM specs ORDER BY id DESC")
        max_num = 0
        for row in cursor:
            spec_id = row["id"]
            if spec_id.startswith("q-"):
                try:
                    num = int(spec_id[2:])
                    if num > max_num:
                        max_num = num
                except ValueError:
                    continue
        return f"q-{max_num + 1:03d}"

    def _row_to_dict(self, row: sqlite3.Row) -> dict[str, Any]:
        """Convert a sqlite3.Row to a plain dict."""
        return dict(row)

    def _spec_name_from_path(self, spec_path: str) -> str:
        """Extract a human-readable name from a spec path."""
        if spec_path:
            return os.path.splitext(os.path.basename(spec_path))[0]
        return ""

    def _find_dependency_path(
        self, from_id: str, target_id: str
    ) -> Optional[list[str]]:
        """Walk the spec_dependencies chain from from_id looking for target_id.

        Checks if target_id is reachable from from_id by following
        blocks_on edges (i.e., does from_id transitively depend on
        target_id?). Used to detect circular dependencies before
        inserting new edges.

        Returns the path (list of spec IDs) from from_id to target_id
        if reachable, or None if no path exists. Must be called while
        holding self.lock.
        """
        from collections import deque

        queue: deque[list[str]] = deque([[from_id]])
        visited: set[str] = {from_id}

        while queue:
            path = queue.popleft()
            current = path[-1]

            rows = self.conn.execute(
                "SELECT blocks_on FROM spec_dependencies WHERE spec_id = ?",
                (current,),
            ).fetchall()

            for row in rows:
                next_id = row["blocks_on"]
                if next_id == target_id:
                    return path
                if next_id not in visited:
                    visited.add(next_id)
                    queue.append(path + [next_id])

        return None

    # ── Spec CRUD operations ────────────────────────────────────────

    def enqueue(
        self,
        spec_path: str,
        priority: int = DEFAULT_PRIORITY,
        max_iterations: int = DEFAULT_MAX_ITERATIONS,
        blocked_by: Optional[list[str]] = None,
        checkout: Optional[str] = None,
        queue_id: Optional[str] = None,
        sync_back: bool = True,
        project: Optional[str] = None,
    ) -> dict[str, Any]:
        """Add a spec to the queue.

        Copies the spec file into the queue directory (copy-on-dispatch).
        Inserts a row into specs and optionally into spec_dependencies.
        Raises DuplicateSpecError if the same original_spec_path is
        already active (queued/running/requeued).

        Returns the spec row as a dict.
        """
        abs_spec_path = os.path.abspath(spec_path)

        with self.lock:
            # Duplicate detection
            active_statuses = ("queued", "running", "requeued")
            cursor = self.conn.execute(
                "SELECT id, status FROM specs "
                "WHERE original_spec_path = ? AND status IN (?, ?, ?)",
                (abs_spec_path, *active_statuses),
            )
            existing = cursor.fetchone()
            if existing:
                raise DuplicateSpecError(
                    abs_spec_path, existing["id"], existing["status"]
                )

            if queue_id is None:
                queue_id = self._next_queue_id()

            # Copy spec file into queue directory
            spec_copy_name = f"{queue_id}.spec.md"
            spec_copy_path = os.path.join(self.queue_dir, spec_copy_name)
            shutil.copy2(abs_spec_path, spec_copy_path)

            # Snapshot initial task IDs
            initial_task_ids: list[str] = []
            content = Path(spec_copy_path).read_text(encoding="utf-8")
            try:
                from lib.spec_parser import parse_spec

                initial_tasks = parse_spec(content)
                initial_task_ids = [t.id for t in initial_tasks]
            except Exception:
                pass

            now = self._now_iso()
            abs_copy_path = os.path.abspath(spec_copy_path)

            self.conn.execute(
                "INSERT INTO specs ("
                "  id, spec_path, original_spec_path, worktree, priority,"
                "  status, phase, submitted_at, iteration, max_iterations,"
                "  sync_back, project, initial_task_ids"
                ") VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    queue_id,
                    abs_copy_path,
                    abs_spec_path,
                    checkout,
                    priority,
                    "queued",
                    "execute",
                    now,
                    0,
                    max_iterations,
                    1 if sync_back else 0,
                    project,
                    json.dumps(initial_task_ids),
                ),
            )

            # Validate and insert dependency edges
            if blocked_by:
                # Missing dependency validation
                missing = []
                warned = []
                for dep_id in blocked_by:
                    row = self.conn.execute(
                        "SELECT status FROM specs WHERE id = ?",
                        (dep_id,),
                    ).fetchone()
                    if row is None:
                        missing.append(dep_id)
                    elif row["status"] in ("canceled", "failed"):
                        warned.append((dep_id, row["status"]))
                if missing:
                    raise ValueError(f"Dependency not found: {', '.join(missing)}")
                if warned:
                    import sys

                    for wid, wstatus in warned:
                        print(
                            f"Warning: dependency {wid} has status '{wstatus}'"
                            " — dependent spec may fail",
                            file=sys.stderr,
                        )

                # Circular dependency detection
                for dep_id in blocked_by:
                    path = self._find_dependency_path(dep_id, queue_id)
                    if path is not None:
                        cycle = [queue_id] + path + [queue_id]
                        cycle_str = " \u2192 ".join(cycle)
                        raise ValueError(f"Circular dependency detected: {cycle_str}")

                for dep_id in blocked_by:
                    self.conn.execute(
                        "INSERT INTO spec_dependencies (spec_id, blocks_on) "
                        "VALUES (?, ?)",
                        (queue_id, dep_id),
                    )

            self._log_event(
                "queued",
                f"Spec queued: {self._spec_name_from_path(abs_spec_path)}",
                spec_id=queue_id,
                data={
                    "priority": priority,
                    "max_iterations": max_iterations,
                },
            )
            self.conn.commit()

            return self._row_to_dict(
                self.conn.execute(
                    "SELECT * FROM specs WHERE id = ?", (queue_id,)
                ).fetchone()
            )

    def get_spec(self, spec_id: str) -> Optional[dict[str, Any]]:
        """Get a single spec by ID. Returns None if not found."""
        cursor = self.conn.execute("SELECT * FROM specs WHERE id = ?", (spec_id,))
        row = cursor.fetchone()
        return self._row_to_dict(row) if row else None

    def get_queue(self) -> list[dict[str, Any]]:
        """Get all specs sorted by priority (lower = higher priority)."""
        cursor = self.conn.execute(
            "SELECT * FROM specs ORDER BY priority ASC, submitted_at ASC"
        )
        return [self._row_to_dict(row) for row in cursor]

    def cancel(self, spec_id: str) -> None:
        """Set a spec's status to 'canceled' and clean up resources."""
        with self.lock:
            row = self.conn.execute(
                "SELECT id FROM specs WHERE id = ?", (spec_id,)
            ).fetchone()
            if row is None:
                raise ValueError(f"Spec not found: {spec_id}")

            self.conn.execute(
                "UPDATE specs SET status = 'canceled' WHERE id = ?",
                (spec_id,),
            )

            # End any active process records for this spec
            now = self._now_iso()
            self.conn.execute(
                "UPDATE processes SET ended_at = ?, exit_code = -1 "
                "WHERE spec_id = ? AND ended_at IS NULL",
                (now, spec_id),
            )

            # Free any worker assigned to this spec
            self.conn.execute(
                "UPDATE workers SET "
                "current_spec_id = NULL, "
                "current_pid = NULL, "
                "start_time = NULL, "
                "current_phase = NULL "
                "WHERE current_spec_id = ?",
                (spec_id,),
            )

            self._log_event("canceled", "Spec canceled", spec_id=spec_id)
            self._cascade_dependency_failure(spec_id, "canceled")
            self.conn.commit()

    def purge(
        self,
        statuses: Optional[list[str]] = None,
        dry_run: bool = False,
    ) -> list[dict[str, Any]]:
        """Remove specs matching the given statuses.

        Also cleans up associated files in the queue directory
        (spec copies, telemetry, iteration files, pid/exit files).

        Returns list of dicts describing each purged entry.
        """
        if statuses is None:
            statuses = ["completed", "failed", "canceled"]

        with self.lock:
            placeholders = ", ".join("?" for _ in statuses)
            cursor = self.conn.execute(
                f"SELECT * FROM specs WHERE status IN ({placeholders})",
                statuses,
            )
            entries = [self._row_to_dict(row) for row in cursor]
            purged: list[dict[str, Any]] = []

            for entry in entries:
                sid = entry["id"]
                files_removed: list[str] = []
                qpath = Path(self.queue_dir)

                # Spec copy
                spec_copy = qpath / f"{sid}.spec.md"
                if spec_copy.is_file():
                    files_removed.append(str(spec_copy))

                # Telemetry
                telem = qpath / f"{sid}.telemetry.json"
                if telem.is_file():
                    files_removed.append(str(telem))

                # Iteration files
                for f in sorted(qpath.iterdir()):
                    if f.name.startswith(f"{sid}.iteration-") and f.name.endswith(
                        ".json"
                    ):
                        files_removed.append(str(f))

                # PID and exit files
                for suffix in [".pid", ".exit"]:
                    extra = qpath / f"{sid}{suffix}"
                    if extra.is_file():
                        files_removed.append(str(extra))

                # Prompt and run script files
                for suffix in [".prompt.md", ".run.sh"]:
                    extra = qpath / f"{sid}{suffix}"
                    if extra.is_file():
                        files_removed.append(str(extra))

                if not dry_run:
                    for fp in files_removed:
                        try:
                            os.remove(fp)
                        except OSError:
                            pass

                purged.append(
                    {
                        "id": sid,
                        "status": entry.get("status", "unknown"),
                        "spec_name": self._spec_name_from_path(
                            entry.get("original_spec_path", "")
                        ),
                        "tasks_total": entry.get("tasks_total", 0),
                        "iteration": entry.get("iteration", 0),
                        "files_removed": files_removed,
                    }
                )

            # Delete rows from DB
            if not dry_run and entries:
                self.conn.execute(
                    f"DELETE FROM specs WHERE status IN ({placeholders})",
                    statuses,
                )
                self._log_event(
                    "purged",
                    f"Purged {len(entries)} spec(s)",
                    data={"ids": [e["id"] for e in entries]},
                )
                self.conn.commit()

            return purged

    def complete(
        self,
        spec_id: str,
        tasks_done: int = 0,
        tasks_total: int = 0,
    ) -> None:
        """Set a spec's status to 'completed'.

        If sync_back is enabled, copies the queue's spec copy
        back to the original location.
        """
        with self.lock:
            row = self.conn.execute(
                "SELECT * FROM specs WHERE id = ?", (spec_id,)
            ).fetchone()
            if row is None:
                raise ValueError(f"Spec not found: {spec_id}")

            self.conn.execute(
                "UPDATE specs SET status = 'completed', "
                "tasks_done = ?, tasks_total = ? "
                "WHERE id = ?",
                (tasks_done, tasks_total, spec_id),
            )
            # Free any worker assigned to this spec atomically
            self.conn.execute(
                "UPDATE workers SET "
                "current_spec_id = NULL, "
                "current_pid = NULL, "
                "start_time = NULL, "
                "current_phase = NULL, "
                "current_task_id = NULL "
                "WHERE current_spec_id = ?",
                (spec_id,),
            )
            self._log_event(
                "completed",
                f"Spec completed ({tasks_done}/{tasks_total} tasks)",
                spec_id=spec_id,
                data={
                    "tasks_done": tasks_done,
                    "tasks_total": tasks_total,
                },
            )
            self.conn.commit()

            # Sync back outside the transaction
            spec_path = row["spec_path"]
            original_path = row["original_spec_path"]
            do_sync = row["sync_back"]

        # Sync back (file I/O outside the lock)
        if do_sync and spec_path and original_path:
            if os.path.isfile(spec_path):
                if os.path.abspath(spec_path) != os.path.abspath(original_path):
                    try:
                        shutil.copy2(spec_path, original_path)
                    except OSError:
                        pass

    # ── Spec state transitions ─────────────────────────────────────

    def pick_next_spec(
        self,
        blocked_ids: Optional[set[str]] = None,
    ) -> Optional[dict[str, Any]]:
        """Pick the highest-priority eligible spec and set it to 'assigning'.

        A spec is eligible if:
        - status is 'queued' or 'requeued'
        - not in blocked_ids
        - cooldown_until is None or in the past
        - all spec_dependencies are completed

        Sets assigning_at timestamp for stuck-assigning recovery.
        Returns the spec dict or None if nothing is eligible.
        """
        blocked = blocked_ids or set()

        with self.lock:
            now = self._now_iso()
            cursor = self.conn.execute(
                "SELECT * FROM specs "
                "WHERE status IN ('queued', 'requeued') "
                "ORDER BY priority ASC, submitted_at ASC"
            )
            for row in cursor:
                spec = self._row_to_dict(row)
                sid = spec["id"]

                if sid in blocked:
                    continue

                # Check cooldown
                cooldown_until = spec.get("cooldown_until")
                if cooldown_until and cooldown_until > now:
                    continue

                # Check dependencies (all must be completed)
                deps = self.conn.execute(
                    "SELECT blocks_on FROM spec_dependencies WHERE spec_id = ?",
                    (sid,),
                ).fetchall()
                if deps:
                    all_done = True
                    for dep in deps:
                        dep_row = self.conn.execute(
                            "SELECT status FROM specs WHERE id = ?",
                            (dep["blocks_on"],),
                        ).fetchone()
                        if dep_row is None or dep_row["status"] != "completed":
                            all_done = False
                            break
                    if not all_done:
                        continue

                # Atomically mark as 'assigning'
                self.conn.execute(
                    "UPDATE specs SET status = 'assigning', "
                    "assigning_at = ? WHERE id = ?",
                    (now, sid),
                )
                self._log_event(
                    "assigning",
                    "Spec picked for assignment",
                    spec_id=sid,
                )
                self.conn.commit()

                # Re-read the updated row
                updated = self.conn.execute(
                    "SELECT * FROM specs WHERE id = ?", (sid,)
                ).fetchone()
                return self._row_to_dict(updated)

            return None

    def set_running(
        self,
        spec_id: str,
        worker_id: str,
        phase: str = "execute",
    ) -> None:
        """Transition a spec from 'assigning' to 'running'.

        CRITICAL: Only increments the iteration counter when
        phase == 'execute'. Critic, evaluate, and decompose phases
        use the current iteration number without incrementing.

        Also snapshots pre_iteration_tasks from the spec file for
        post-iteration regression detection.
        """
        with self.lock:
            row = self.conn.execute(
                "SELECT * FROM specs WHERE id = ?", (spec_id,)
            ).fetchone()
            if row is None:
                raise ValueError(f"Spec not found: {spec_id}")

            now = self._now_iso()
            current_iteration = row["iteration"]

            # Only increment iteration for execute phase
            if phase == "execute":
                new_iteration = current_iteration + 1
            else:
                new_iteration = current_iteration

            # Set first_running_at only on the very first run
            first_running = row["first_running_at"] or now

            # Snapshot pre_iteration_tasks from spec file
            pre_tasks = {}
            spec_path = row["spec_path"]
            if spec_path and os.path.isfile(spec_path):
                try:
                    from lib.spec_parser import parse_spec

                    content = Path(spec_path).read_text(encoding="utf-8")
                    tasks = parse_spec(content)
                    pre_tasks = {t.id: t.status for t in tasks}
                except Exception:
                    pass

            self.conn.execute(
                "UPDATE specs SET "
                "status = 'running', "
                "phase = ?, "
                "iteration = ?, "
                "last_worker = ?, "
                "last_iteration_at = ?, "
                "first_running_at = ?, "
                "assigning_at = NULL, "
                "pre_iteration_tasks = ? "
                "WHERE id = ?",
                (
                    phase,
                    new_iteration,
                    worker_id,
                    now,
                    first_running,
                    json.dumps(pre_tasks),
                    spec_id,
                ),
            )
            self._log_event(
                "running",
                f"Spec running (iteration {new_iteration}, phase={phase})",
                spec_id=spec_id,
                data={
                    "iteration": new_iteration,
                    "phase": phase,
                    "worker_id": worker_id,
                },
            )
            self.conn.commit()

    def has_reached_max_iterations(self, spec_id: str) -> bool:
        """Check if a spec has reached its max_iterations limit.

        Only counts execute-phase iterations, not critic/evaluate/decompose.
        """
        row = self.conn.execute(
            "SELECT iteration, max_iterations FROM specs WHERE id = ?",
            (spec_id,),
        ).fetchone()
        if row is None:
            raise ValueError(f"Spec not found: {spec_id}")
        return row["iteration"] >= row["max_iterations"]

    def requeue(
        self,
        spec_id: str,
        tasks_done: int = 0,
        tasks_total: int = 0,
    ) -> None:
        """Set a spec's status to 'requeued' (still has PENDING tasks).

        Resets consecutive_failures and clears cooldown on success.
        """
        with self.lock:
            row = self.conn.execute(
                "SELECT id FROM specs WHERE id = ?", (spec_id,)
            ).fetchone()
            if row is None:
                raise ValueError(f"Spec not found: {spec_id}")

            self.conn.execute(
                "UPDATE specs SET "
                "status = 'requeued', "
                "tasks_done = ?, "
                "tasks_total = ?, "
                "consecutive_failures = 0, "
                "cooldown_until = NULL "
                "WHERE id = ?",
                (tasks_done, tasks_total, spec_id),
            )
            self._log_event(
                "requeued",
                f"Spec requeued ({tasks_done}/{tasks_total} tasks done)",
                spec_id=spec_id,
                data={
                    "tasks_done": tasks_done,
                    "tasks_total": tasks_total,
                },
            )
            self.conn.commit()

    def _cascade_dependency_failure(self, spec_id: str, terminal_status: str) -> None:
        """Fail all specs that depend on spec_id (transitively).

        Must be called inside a transaction (caller holds self.lock).
        When a spec reaches a terminal failed/canceled state, any spec
        that lists it in spec_dependencies should also be failed with
        a clear dependency_failed reason.
        """
        queue: list[tuple[str, str]] = [(spec_id, terminal_status)]
        while queue:
            failed_id, status = queue.pop(0)
            # Find specs that depend on failed_id
            dependents = self.conn.execute(
                "SELECT spec_id FROM spec_dependencies WHERE blocks_on = ?",
                (failed_id,),
            ).fetchall()
            for dep_row in dependents:
                dep_id = dep_row["spec_id"]
                # Only cascade to specs still waiting
                dep_spec = self.conn.execute(
                    "SELECT status FROM specs WHERE id = ?", (dep_id,)
                ).fetchone()
                if dep_spec and dep_spec["status"] in ("queued", "requeued"):
                    failure_reason = f"dependency_failed: {failed_id} ({status})"
                    self.conn.execute(
                        "UPDATE specs SET status = 'failed', "
                        "failure_reason = ? WHERE id = ?",
                        (failure_reason, dep_id),
                    )
                    self._log_event(
                        "dependency_failed",
                        failure_reason,
                        spec_id=dep_id,
                        data={
                            "blocked_on": failed_id,
                            "blocked_on_status": status,
                        },
                        level="warn",
                    )
                    # Transitively cascade
                    queue.append((dep_id, "failed"))

    def fail(self, spec_id: str, reason: str = "") -> None:
        """Set a spec's status to 'failed'."""
        with self.lock:
            row = self.conn.execute(
                "SELECT id FROM specs WHERE id = ?", (spec_id,)
            ).fetchone()
            if row is None:
                raise ValueError(f"Spec not found: {spec_id}")

            self.conn.execute(
                "UPDATE specs SET status = 'failed', failure_reason = ? WHERE id = ?",
                (reason or None, spec_id),
            )
            self._log_event(
                "failed",
                f"Spec failed: {reason or 'unknown'}",
                spec_id=spec_id,
                data={"reason": reason},
                level="warn",
            )
            self._cascade_dependency_failure(spec_id, "failed")
            self.conn.commit()

    def record_failure(self, spec_id: str) -> bool:
        """Increment consecutive failure count and apply cooldown.

        Returns True if max consecutive failures has been reached.
        Sets cooldown_until to prevent immediate retry after a crash.
        """
        with self.lock:
            row = self.conn.execute(
                "SELECT consecutive_failures FROM specs WHERE id = ?",
                (spec_id,),
            ).fetchone()
            if row is None:
                raise ValueError(f"Spec not found: {spec_id}")

            new_count = (row["consecutive_failures"] or 0) + 1
            cooldown_end = (
                datetime.now(timezone.utc) + timedelta(seconds=COOLDOWN_SECONDS)
            ).isoformat()

            self.conn.execute(
                "UPDATE specs SET "
                "consecutive_failures = ?, "
                "cooldown_until = ? "
                "WHERE id = ?",
                (new_count, cooldown_end, spec_id),
            )
            self._log_event(
                "failure_recorded",
                f"Failure #{new_count} recorded",
                spec_id=spec_id,
                data={
                    "consecutive_failures": new_count,
                    "cooldown_until": cooldown_end,
                },
                level="warn",
            )
            self.conn.commit()

            return new_count >= MAX_CONSECUTIVE_FAILURES

    def crash_requeue(self, spec_id: str) -> None:
        """Set spec status to 'requeued' after a crash.

        Unlike requeue(), does NOT reset consecutive_failures or
        clear cooldown. Those were already set by record_failure().
        """
        with self.lock:
            row = self.conn.execute(
                "SELECT id FROM specs WHERE id = ?", (spec_id,)
            ).fetchone()
            if row is None:
                raise ValueError(f"Spec not found: {spec_id}")

            self.conn.execute(
                "UPDATE specs SET status = 'requeued' WHERE id = ?",
                (spec_id,),
            )
            self._log_event(
                "crash_requeued",
                "Spec requeued after crash (with cooldown)",
                spec_id=spec_id,
            )
            self.conn.commit()

    def signal_requeue(self, spec_id: str) -> None:
        """Requeue a spec after a signal death (SIGTERM, SIGKILL, etc.).

        Unlike crash_requeue(), does NOT increment consecutive_failures
        or set cooldown. Signal deaths are external kills, not worker bugs.
        """
        with self.lock:
            row = self.conn.execute(
                "SELECT id FROM specs WHERE id = ?", (spec_id,)
            ).fetchone()
            if row is None:
                raise ValueError(f"Spec not found: {spec_id}")

            self.conn.execute(
                "UPDATE specs SET status = 'requeued', "
                "cooldown_until = NULL "
                "WHERE id = ?",
                (spec_id,),
            )
            self._log_event(
                "signal_requeued",
                "Spec requeued after signal death (no failure counted)",
                spec_id=spec_id,
            )
            self.conn.commit()

    def set_needs_review(
        self,
        spec_id: str,
        experiment_tasks: list[str],
        tasks_done: int = 0,
        tasks_total: int = 0,
    ) -> None:
        """Set a spec's status to 'needs_review' for experiment review.

        Records which tasks have experiments and the timestamp for
        timeout tracking.
        """
        with self.lock:
            row = self.conn.execute(
                "SELECT id FROM specs WHERE id = ?", (spec_id,)
            ).fetchone()
            if row is None:
                raise ValueError(f"Spec not found: {spec_id}")

            now = self._now_iso()
            self.conn.execute(
                "UPDATE specs SET "
                "status = 'needs_review', "
                "tasks_done = ?, "
                "tasks_total = ?, "
                "experiment_tasks = ?, "
                "needs_review_since = ?, "
                "consecutive_failures = 0, "
                "cooldown_until = NULL "
                "WHERE id = ?",
                (
                    tasks_done,
                    tasks_total,
                    json.dumps(experiment_tasks),
                    now,
                    spec_id,
                ),
            )
            self._log_event(
                "needs_review",
                "Spec paused for experiment review "
                f"({len(experiment_tasks)} experiments)",
                spec_id=spec_id,
                data={"experiment_tasks": experiment_tasks},
            )
            self.conn.commit()

    def increment_experiment_usage(
        self,
        spec_id: str,
        count: int = 1,
    ) -> dict[str, int]:
        """Increment experiment_invocations_used for a spec.

        Returns dict with max_experiment_invocations,
        experiment_invocations_used, and remaining budget.
        """
        with self.lock:
            row = self.conn.execute(
                "SELECT experiment_invocations_used, "
                "max_experiment_invocations FROM specs WHERE id = ?",
                (spec_id,),
            ).fetchone()
            if row is None:
                raise ValueError(f"Spec not found: {spec_id}")

            used = (row["experiment_invocations_used"] or 0) + count
            max_budget = row["max_experiment_invocations"] or 0

            self.conn.execute(
                "UPDATE specs SET experiment_invocations_used = ? WHERE id = ?",
                (used, spec_id),
            )
            self._log_event(
                "experiment_usage_incremented",
                f"Experiment usage: {used}/{max_budget}",
                spec_id=spec_id,
                data={"used": used, "max_budget": max_budget},
            )
            self.conn.commit()

            return {
                "max_experiment_invocations": max_budget,
                "experiment_invocations_used": used,
                "remaining": max(0, max_budget - used),
            }

    def recover_stale_assigning(self, timeout_seconds: int = 60) -> list[str]:
        """Reset specs stuck in 'assigning' to 'requeued'.

        If a spec has been in 'assigning' for longer than
        timeout_seconds, it means the daemon crashed after picking
        but before launching. Reset to 'requeued' so it can be
        re-dispatched.

        Returns list of spec IDs that were recovered.
        """
        cutoff = (
            datetime.now(timezone.utc) - timedelta(seconds=timeout_seconds)
        ).isoformat()

        recovered: list[str] = []
        with self.lock:
            cursor = self.conn.execute(
                "SELECT id FROM specs "
                "WHERE status = 'assigning' "
                "AND assigning_at IS NOT NULL "
                "AND assigning_at < ?",
                (cutoff,),
            )
            stale = cursor.fetchall()
            for row in stale:
                sid = row["id"]
                self.conn.execute(
                    "UPDATE specs SET "
                    "status = 'requeued', "
                    "assigning_at = NULL "
                    "WHERE id = ?",
                    (sid,),
                )
                self._log_event(
                    "recover_stale_assigning",
                    f"Recovered stuck-assigning spec after {timeout_seconds}s",
                    spec_id=sid,
                    level="warn",
                )
                recovered.append(sid)

            if recovered:
                self.conn.commit()

        return recovered

    # ── General field updates ─────────────────────────────────────

    # Columns that may be updated via update_spec_fields().
    _UPDATABLE_COLUMNS = frozenset(
        {
            "phase",
            "consecutive_failures",
            "critic_passes",
            "decomposition_retries",
            "tasks_done",
            "tasks_total",
            "worker_timeout_seconds",
            "failure_reason",
            "needs_review_since",
            "experiment_tasks",
            "status",
            "max_experiment_invocations",
            "experiment_invocations_used",
        }
    )

    def update_spec_fields(
        self,
        spec_id: str,
        **fields: Any,
    ) -> None:
        """Update one or more columns on a spec row.

        Only columns listed in _UPDATABLE_COLUMNS are allowed.
        Raises ValueError if the spec doesn't exist or if an
        invalid column is specified.
        """
        invalid = set(fields) - self._UPDATABLE_COLUMNS
        if invalid:
            raise ValueError(f"Cannot update columns: {', '.join(sorted(invalid))}")
        if not fields:
            return

        with self.lock:
            row = self.conn.execute(
                "SELECT id FROM specs WHERE id = ?", (spec_id,)
            ).fetchone()
            if row is None:
                raise ValueError(f"Spec not found: {spec_id}")

            set_clauses = ", ".join(f"{col} = ?" for col in fields)
            values = list(fields.values()) + [spec_id]
            self.conn.execute(
                f"UPDATE specs SET {set_clauses} WHERE id = ?",
                values,
            )
            self._log_event(
                "spec_updated",
                f"Updated fields: {', '.join(fields.keys())}",
                spec_id=spec_id,
                data=fields,
            )
            self.conn.commit()

    # ── Worker management ──────────────────────────────────────────

    def register_worker(
        self,
        worker_id: str,
        worktree_path: str,
    ) -> None:
        """Register a worker slot in the database.

        If the worker already exists, update its worktree_path.
        """
        with self.lock:
            self.conn.execute(
                "INSERT INTO workers (id, worktree_path) "
                "VALUES (?, ?) "
                "ON CONFLICT(id) DO UPDATE SET worktree_path = ?",
                (worker_id, worktree_path, worktree_path),
            )
            self._log_event(
                "worker_registered",
                f"Worker registered: {worker_id}",
                data={"worker_id": worker_id, "worktree_path": worktree_path},
            )
            self.conn.commit()

    def get_free_worker(self) -> Optional[dict[str, Any]]:
        """Find the next free worker using round-robin selection.

        Picks the free worker whose id comes after the last assigned
        worker, wrapping around. Prevents w-1 from handling 57%+ of
        all tasks (the old ORDER BY id ASC behaviour).

        Returns the worker dict or None if all workers are busy.
        """
        # Find the last assigned worker from the event log.
        # Events persist across assign/free cycles, unlike start_time.
        last_event = self.conn.execute(
            "SELECT data FROM events WHERE event_type = 'worker_assigned' "
            "ORDER BY seq DESC LIMIT 1"
        ).fetchone()

        if last_event:
            last_id = json.loads(last_event["data"]).get("worker_id", "")
        else:
            last_id = ""

        # Try workers after last_id first (round-robin)
        row = self.conn.execute(
            "SELECT * FROM workers WHERE current_spec_id IS NULL AND id > ? "
            "ORDER BY id ASC LIMIT 1",
            (last_id,),
        ).fetchone()

        if row is None:
            # Wrap around to beginning
            row = self.conn.execute(
                "SELECT * FROM workers WHERE current_spec_id IS NULL "
                "ORDER BY id ASC LIMIT 1"
            ).fetchone()

        return self._row_to_dict(row) if row else None

    def assign_worker(
        self,
        worker_id: str,
        spec_id: str,
        pid: int,
        phase: str = "execute",
        task_id: str | None = None,
    ) -> None:
        """Mark a worker as busy with the given spec and PID.

        Records current_phase for crash recovery dispatch.
        Optionally records current_task_id for parallel DAG execution.
        """
        with self.lock:
            now = self._now_iso()
            self.conn.execute(
                "UPDATE workers SET "
                "current_spec_id = ?, "
                "current_pid = ?, "
                "start_time = ?, "
                "current_phase = ?, "
                "current_task_id = ? "
                "WHERE id = ?",
                (spec_id, pid, now, phase, task_id, worker_id),
            )
            self._log_event(
                "worker_assigned",
                f"Worker {worker_id} assigned to {spec_id} (pid={pid}, phase={phase}, task={task_id})",
                spec_id=spec_id,
                data={
                    "worker_id": worker_id,
                    "pid": pid,
                    "phase": phase,
                    "task_id": task_id,
                },
            )
            self.conn.commit()

    def free_worker(self, worker_id: str) -> None:
        """Release a worker so it can accept new specs."""
        with self.lock:
            self.conn.execute(
                "UPDATE workers SET "
                "current_spec_id = NULL, "
                "current_pid = NULL, "
                "start_time = NULL, "
                "current_phase = NULL, "
                "current_task_id = NULL "
                "WHERE id = ?",
                (worker_id,),
            )
            self._log_event(
                "worker_freed",
                f"Worker {worker_id} freed",
                data={"worker_id": worker_id},
            )
            self.conn.commit()

    def get_worker(self, worker_id: str) -> Optional[dict[str, Any]]:
        """Get a single worker by ID. Returns None if not found."""
        cursor = self.conn.execute("SELECT * FROM workers WHERE id = ?", (worker_id,))
        row = cursor.fetchone()
        return self._row_to_dict(row) if row else None

    def get_worker_current_spec(self, worker_id: str) -> Optional[dict[str, Any]]:
        """Get the spec currently assigned to a worker.

        Returns the spec dict or None if the worker is idle or
        the spec no longer exists.
        """
        worker = self.get_worker(worker_id)
        if worker is None or worker["current_spec_id"] is None:
            return None
        return self.get_spec(worker["current_spec_id"])

    def get_workers_on_spec(self, spec_id: str) -> list[dict[str, Any]]:
        """Return all workers currently assigned to a spec.

        Used for parallel DAG execution to determine which tasks
        are already in_progress (assigned to a worker).
        """
        cursor = self.conn.execute(
            "SELECT * FROM workers WHERE current_spec_id = ?",
            (spec_id,),
        )
        return [self._row_to_dict(row) for row in cursor]

    def get_in_progress_task_ids(self, spec_id: str) -> set[str]:
        """Return the set of task IDs currently being executed for a spec.

        Reads current_task_id from all workers assigned to this spec.
        Used by the daemon to avoid assigning the same task twice.
        """
        workers = self.get_workers_on_spec(spec_id)
        return {w["current_task_id"] for w in workers if w.get("current_task_id")}

    def get_all_workers(self) -> list[dict[str, Any]]:
        """Return all registered workers."""
        cursor = self.conn.execute("SELECT * FROM workers")
        return [self._row_to_dict(row) for row in cursor]

    def get_free_workers(self, limit: int = 10) -> list[dict[str, Any]]:
        """Return up to N free workers using round-robin selection.

        Unlike get_free_worker() which returns one, this returns
        multiple for parallel task assignment.
        """
        workers: list[dict[str, Any]] = []
        for _ in range(limit):
            w = self.get_free_worker()
            if w is None:
                break
            workers.append(w)
            # Temporarily mark as busy to avoid returning the same worker
            # The caller is responsible for actually assigning or freeing.
            # We use a sentinel spec_id that will be overwritten.
            with self.lock:
                self.conn.execute(
                    "UPDATE workers SET current_spec_id = '__reserving' WHERE id = ?",
                    (w["id"],),
                )
                self.conn.commit()

        # Unreserve any workers we grabbed (caller will assign them)
        for w in workers:
            with self.lock:
                self.conn.execute(
                    "UPDATE workers SET current_spec_id = NULL "
                    "WHERE id = ? AND current_spec_id = '__reserving'",
                    (w["id"],),
                )
                self.conn.commit()

        return workers

    # ── Process tracking ───────────────────────────────────────────

    def register_process(
        self,
        pid: int,
        spec_id: str,
        worker_id: str,
        iteration: int,
        phase: str,
    ) -> None:
        """Record that a process has been spawned for a spec iteration."""
        with self.lock:
            now = self._now_iso()
            self.conn.execute(
                "INSERT INTO processes "
                "(pid, spec_id, worker_id, iteration, phase, started_at) "
                "VALUES (?, ?, ?, ?, ?, ?)",
                (pid, spec_id, worker_id, iteration, phase, now),
            )
            self._log_event(
                "process_started",
                f"Process {pid} started for {spec_id} "
                f"(iteration={iteration}, phase={phase})",
                spec_id=spec_id,
                data={
                    "pid": pid,
                    "worker_id": worker_id,
                    "iteration": iteration,
                    "phase": phase,
                },
            )
            self.conn.commit()

    def end_process(self, pid: int, spec_id: str, exit_code: int) -> None:
        """Record that a process has exited."""
        with self.lock:
            now = self._now_iso()
            self.conn.execute(
                "UPDATE processes SET ended_at = ?, exit_code = ? "
                "WHERE pid = ? AND spec_id = ? AND ended_at IS NULL",
                (now, exit_code, pid, spec_id),
            )
            self._log_event(
                "process_ended",
                f"Process {pid} exited with code {exit_code}",
                spec_id=spec_id,
                data={"pid": pid, "exit_code": exit_code},
            )
            self.conn.commit()

    def get_active_processes(self) -> list[dict[str, Any]]:
        """Get all processes that have not ended yet."""
        cursor = self.conn.execute("SELECT * FROM processes WHERE ended_at IS NULL")
        return [self._row_to_dict(row) for row in cursor]

    # ── PID validation ─────────────────────────────────────────────

    @staticmethod
    def _is_pid_alive_and_ours(pid: int, started_at: str) -> bool:
        """Check if a PID is alive AND belongs to the process we started.

        Uses /proc/{pid}/stat start time comparison on Linux to detect
        PID reuse. Falls back to bare kill-0 on non-Linux platforms.

        Args:
            pid: The process ID to check.
            started_at: ISO-8601 timestamp of when we launched the process.

        Returns:
            True if the PID is alive and appears to be the same process.
        """
        # Step 1: check if PID exists at all
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return False
        except PermissionError:
            # Process exists but we can't signal it. Treat as alive
            # but proceed to start-time check.
            pass

        # Step 2: on Linux, compare /proc start time to detect reuse
        stat_path = f"/proc/{pid}/stat"
        if os.path.exists(stat_path):
            try:
                with open(stat_path, "r") as f:
                    stat_line = f.read()
                # Format: pid (comm) state ... field[21] = starttime
                # The comm field can contain spaces and parens, so find
                # the last ')' to safely split.
                close_paren = stat_line.rfind(")")
                if close_paren == -1:
                    # Can't parse; assume alive (conservative)
                    return True
                fields_after_comm = stat_line[close_paren + 2 :].split()
                # starttime is field index 19 after the comm block
                # (fields_after_comm[0] = state, [1] = ppid, ... [19] = starttime)
                if len(fields_after_comm) > 19:
                    proc_starttime = int(fields_after_comm[19])
                    # Convert our started_at to a comparable value.
                    # We use a heuristic: if the process start time
                    # (in clock ticks since boot) is significantly
                    # different from what we'd expect, it's a reuse.
                    # Since we can't precisely convert ISO time to
                    # clock ticks, we store the starttime at registration
                    # and compare directly. We encode the starttime
                    # in the started_at string if it contains a pipe.
                    if "|" in started_at:
                        _, stored_ticks_str = started_at.rsplit("|", 1)
                        stored_ticks = int(stored_ticks_str)
                        return proc_starttime == stored_ticks
                    # If started_at is a plain ISO timestamp (legacy),
                    # we can't compare ticks directly. Fall through
                    # to "alive" since kill-0 passed.
                    return True
            except (OSError, ValueError, IndexError):
                # Can't read /proc; fall back to kill-0 result
                return True

        # Non-Linux or /proc not available: kill-0 passed, assume alive
        return True

    @staticmethod
    def _get_proc_starttime(pid: int) -> Optional[int]:
        """Read the starttime field from /proc/{pid}/stat.

        Returns the start time in clock ticks since boot, or None
        if the file can't be read (non-Linux, process gone, etc.).
        """
        stat_path = f"/proc/{pid}/stat"
        try:
            with open(stat_path, "r") as f:
                stat_line = f.read()
            close_paren = stat_line.rfind(")")
            if close_paren == -1:
                return None
            fields_after_comm = stat_line[close_paren + 2 :].split()
            if len(fields_after_comm) > 19:
                return int(fields_after_comm[19])
        except (OSError, ValueError, IndexError):
            pass
        return None

    def make_started_at(self, pid: int) -> str:
        """Create a started_at string that includes the proc starttime.

        Format: "ISO_TIMESTAMP|STARTTIME_TICKS"
        This allows _is_pid_alive_and_ours to detect PID reuse.
        """
        now = self._now_iso()
        ticks = self._get_proc_starttime(pid)
        if ticks is not None:
            return f"{now}|{ticks}"
        return now

    # ── Recovery ───────────────────────────────────────────────────

    # ── Event queries ───────────────────────────────────────────────

    def get_events(
        self,
        spec_id: Optional[str] = None,
        limit: Optional[int] = None,
    ) -> list[dict[str, Any]]:
        """Query events from the events table.

        Args:
            spec_id: If provided, filter events to this spec only.
            limit: If provided, return at most this many events
                   (most recent first when limited, but returned
                   in chronological order).

        Returns:
            List of event dicts in chronological order (oldest first).
        """
        if spec_id is not None and limit is not None:
            cursor = self.conn.execute(
                "SELECT * FROM ("
                "  SELECT * FROM events WHERE spec_id = ? "
                "  ORDER BY seq DESC LIMIT ?"
                ") sub ORDER BY seq ASC",
                (spec_id, limit),
            )
        elif spec_id is not None:
            cursor = self.conn.execute(
                "SELECT * FROM events WHERE spec_id = ? ORDER BY seq ASC",
                (spec_id,),
            )
        elif limit is not None:
            cursor = self.conn.execute(
                "SELECT * FROM ("
                "  SELECT * FROM events ORDER BY seq DESC LIMIT ?"
                ") sub ORDER BY seq ASC",
                (limit,),
            )
        else:
            cursor = self.conn.execute("SELECT * FROM events ORDER BY seq ASC")
        return [self._row_to_dict(row) for row in cursor]

    # ── Dependency queries ────────────────────────────────────────

    def get_dependencies(self, queue_id: str) -> list[dict[str, Any]]:
        """Get specs that queue_id must wait for (its blockers).

        Returns list of dicts with 'id' and 'status' for each
        spec listed in spec_dependencies.blocks_on for queue_id.
        """
        rows = self.conn.execute(
            "SELECT s.id, s.status "
            "FROM spec_dependencies sd "
            "JOIN specs s ON s.id = sd.blocks_on "
            "WHERE sd.spec_id = ?",
            (queue_id,),
        ).fetchall()
        return [dict(r) for r in rows]

    def get_dependents(self, queue_id: str) -> list[dict[str, Any]]:
        """Get specs that are waiting on queue_id to complete.

        Returns list of dicts with 'id' and 'status' for each
        spec that has queue_id in its spec_dependencies.blocks_on.
        """
        rows = self.conn.execute(
            "SELECT s.id, s.status "
            "FROM spec_dependencies sd "
            "JOIN specs s ON s.id = sd.spec_id "
            "WHERE sd.blocks_on = ?",
            (queue_id,),
        ).fetchall()
        return [dict(r) for r in rows]

    def add_dependency(self, spec_id: str, dep_id: str) -> None:
        """Add a dependency: spec_id must wait for dep_id to complete.

        Validates that both specs exist and performs DFS cycle detection
        before inserting. Silently ignores if the edge already exists.

        Raises ValueError if either spec is not found or if the new edge
        would create a circular dependency.
        """
        with self.lock:
            for sid in (spec_id, dep_id):
                if not self.conn.execute(
                    "SELECT 1 FROM specs WHERE id = ?", (sid,)
                ).fetchone():
                    raise ValueError(f"Spec not found: {sid}")

            # Cycle detection: would dep_id transitively depend on spec_id?
            path = self._find_dependency_path(dep_id, spec_id)
            if path is not None:
                cycle = [spec_id] + path + [spec_id]
                cycle_str = " \u2192 ".join(cycle)
                raise ValueError(f"Circular dependency detected: {cycle_str}")

            try:
                self.conn.execute(
                    "INSERT INTO spec_dependencies (spec_id, blocks_on) VALUES (?, ?)",
                    (spec_id, dep_id),
                )
            except sqlite3.IntegrityError:
                pass  # edge already exists

            self._log_event(
                "dependency_added",
                f"Dependency added: {spec_id} blocked by {dep_id}",
                spec_id=spec_id,
                data={"dep_id": dep_id},
            )
            self.conn.commit()

    def remove_dependency(self, spec_id: str, dep_id: str) -> None:
        """Remove a dependency: spec_id is no longer blocked by dep_id.

        Raises ValueError if spec_id does not exist.
        Silently succeeds if the dependency edge does not exist.
        """
        with self.lock:
            if not self.conn.execute(
                "SELECT 1 FROM specs WHERE id = ?", (spec_id,)
            ).fetchone():
                raise ValueError(f"Spec not found: {spec_id}")

            self.conn.execute(
                "DELETE FROM spec_dependencies WHERE spec_id = ? AND blocks_on = ?",
                (spec_id, dep_id),
            )
            self._log_event(
                "dependency_removed",
                f"Dependency removed: {spec_id} unblocked from {dep_id}",
                spec_id=spec_id,
                data={"dep_id": dep_id},
            )
            self.conn.commit()

    def replace_dependencies(self, spec_id: str, dep_ids: list[str]) -> None:
        """Replace all dependencies for spec_id with the given dep_ids.

        Performs cycle detection for each new dependency. If any would
        create a cycle, raises ValueError and makes no changes.
        """
        with self.lock:
            if not self.conn.execute(
                "SELECT 1 FROM specs WHERE id = ?", (spec_id,)
            ).fetchone():
                raise ValueError(f"Spec not found: {spec_id}")

            # Validate all new deps exist and check cycles
            for dep_id in dep_ids:
                if not self.conn.execute(
                    "SELECT 1 FROM specs WHERE id = ?", (dep_id,)
                ).fetchone():
                    raise ValueError(f"Spec not found: {dep_id}")
                path = self._find_dependency_path(dep_id, spec_id)
                if path is not None:
                    cycle = [spec_id] + path + [spec_id]
                    raise ValueError(
                        f"Circular dependency detected: {' -> '.join(cycle)}"
                    )

            # Atomic replace
            self.conn.execute(
                "DELETE FROM spec_dependencies WHERE spec_id = ?",
                (spec_id,),
            )
            for dep_id in dep_ids:
                self.conn.execute(
                    "INSERT INTO spec_dependencies (spec_id, blocks_on) VALUES (?, ?)",
                    (spec_id, dep_id),
                )
            self._log_event(
                "dependencies_replaced",
                f"Dependencies replaced for {spec_id}: "
                f"{', '.join(dep_ids) if dep_ids else '(none)'}",
                spec_id=spec_id,
                data={"dep_ids": dep_ids},
            )
            self.conn.commit()

    def clear_dependencies(self, spec_id: str) -> int:
        """Remove all dependencies from spec_id. Returns count removed."""
        with self.lock:
            if not self.conn.execute(
                "SELECT 1 FROM specs WHERE id = ?", (spec_id,)
            ).fetchone():
                raise ValueError(f"Spec not found: {spec_id}")

            cursor = self.conn.execute(
                "DELETE FROM spec_dependencies WHERE spec_id = ?",
                (spec_id,),
            )
            count = cursor.rowcount
            self._log_event(
                "dependencies_cleared",
                f"All dependencies cleared for {spec_id} ({count} removed)",
                spec_id=spec_id,
                data={"count": count},
            )
            self.conn.commit()
            return count

    def get_fleet_dag(self) -> dict[str, Any]:
        """Return the full fleet dependency DAG.

        Returns dict with:
          - specs: list of {id, status, deps: [dep_ids], dependents: [dep_ids]}
          - edges: list of {from: spec_id, to: blocks_on}
        """
        specs = self.conn.execute("SELECT id, status FROM specs").fetchall()

        edges = self.conn.execute(
            "SELECT spec_id, blocks_on FROM spec_dependencies"
        ).fetchall()

        edge_list = [{"from": e["spec_id"], "to": e["blocks_on"]} for e in edges]

        # Build adjacency info per spec
        spec_deps: dict[str, list[str]] = {}
        spec_dependents: dict[str, list[str]] = {}
        for e in edges:
            spec_deps.setdefault(e["spec_id"], []).append(e["blocks_on"])
            spec_dependents.setdefault(e["blocks_on"], []).append(e["spec_id"])

        spec_list = []
        for s in specs:
            spec_list.append(
                {
                    "id": s["id"],
                    "status": s["status"],
                    "deps": spec_deps.get(s["id"], []),
                    "dependents": spec_dependents.get(s["id"], []),
                }
            )

        return {"specs": spec_list, "edges": edge_list}

    def check_fleet_dag(self) -> list[dict[str, str]]:
        """Validate the fleet DAG. Returns list of issues found.

        Checks for:
        - Self-loops
        - Deps on failed/canceled specs that block queued specs
        """
        issues: list[dict[str, str]] = []

        edges = self.conn.execute(
            "SELECT spec_id, blocks_on FROM spec_dependencies"
        ).fetchall()

        for e in edges:
            sid, dep = e["spec_id"], e["blocks_on"]

            # Self-loop
            if sid == dep:
                issues.append({"type": "self_loop", "spec": sid})
                continue

            # Check if dep is in a terminal non-completed state
            dep_row = self.conn.execute(
                "SELECT status FROM specs WHERE id = ?", (dep,)
            ).fetchone()
            if dep_row and dep_row["status"] in ("failed", "canceled"):
                spec_row = self.conn.execute(
                    "SELECT status FROM specs WHERE id = ?", (sid,)
                ).fetchone()
                if spec_row and spec_row["status"] in ("queued", "requeued"):
                    issues.append(
                        {
                            "type": "blocked_by_terminal",
                            "spec": sid,
                            "blocked_on": dep,
                            "blocked_on_status": dep_row["status"],
                        }
                    )

        return issues

    # ── Iteration metadata ────────────────────────────────────────

    def insert_iteration(
        self,
        spec_id: str,
        iteration: int,
        phase: str,
        worker_id: str,
        started_at: str,
        ended_at: str,
        duration_seconds: int,
        tasks_completed: int = 0,
        tasks_added: int = 0,
        tasks_skipped: int = 0,
        exit_code: Optional[int] = None,
        pre_pending: Optional[int] = None,
        post_pending: Optional[int] = None,
        quality_score: Optional[float] = None,
        quality_breakdown: Optional[str] = None,
    ) -> None:
        """Insert a row into the iterations table.

        Records metadata about a single (spec, iteration, phase)
        execution. Called by the daemon after a worker completes.
        """
        with self.lock:
            self.conn.execute(
                "INSERT INTO iterations ("
                "  spec_id, iteration, phase, worker_id,"
                "  started_at, ended_at, duration_seconds,"
                "  tasks_completed, tasks_added, tasks_skipped,"
                "  exit_code, pre_pending, post_pending,"
                "  quality_score, quality_breakdown"
                ") VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    spec_id,
                    iteration,
                    phase,
                    worker_id,
                    started_at,
                    ended_at,
                    duration_seconds,
                    tasks_completed,
                    tasks_added,
                    tasks_skipped,
                    exit_code,
                    pre_pending,
                    post_pending,
                    quality_score,
                    quality_breakdown,
                ),
            )
            self._log_event(
                "iteration_recorded",
                f"Iteration {iteration} ({phase}) recorded "
                f"({tasks_completed} completed, {tasks_added} added)",
                spec_id=spec_id,
                data={
                    "iteration": iteration,
                    "phase": phase,
                    "duration_seconds": duration_seconds,
                    "exit_code": exit_code,
                },
            )
            self.conn.commit()

    def get_iterations(
        self,
        spec_id: str,
    ) -> list[dict[str, Any]]:
        """Get all iteration records for a spec.

        Returns rows sorted by iteration number then phase,
        in insertion order within the same (iteration, phase).
        """
        cursor = self.conn.execute(
            "SELECT * FROM iterations WHERE spec_id = ? "
            "ORDER BY iteration ASC, phase ASC",
            (spec_id,),
        )
        return [self._row_to_dict(row) for row in cursor]

    # ── Messaging ─────────────────────────────────────────────────

    def record_message(self, msg: dict) -> None:
        """Record a message in the messages table for audit.

        Args:
            msg: Message dict from lib.messaging.create_message().
        """
        now = time.time()
        direction = msg.get("_direction", "to_worker")
        if msg.get("sender") == "worker":
            direction = "to_daemon"

        with self.lock:
            self.conn.execute(
                "INSERT OR IGNORE INTO messages "
                "(id, spec_id, task_id, msg_type, sender, direction, "
                "payload, created_at, delivered_at) "
                "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    msg["id"],
                    msg["spec_id"],
                    msg.get("task_id"),
                    msg["type"],
                    msg["sender"],
                    direction,
                    json.dumps(msg.get("payload", {})),
                    now,
                    now,
                ),
            )
            self.conn.commit()

    def ack_message_db(self, msg_id: str) -> None:
        """Mark a message as acknowledged in the database.

        Args:
            msg_id: The message ID to acknowledge.
        """
        now = time.time()
        with self.lock:
            self.conn.execute(
                "UPDATE messages SET acked_at = ? WHERE id = ?",
                (now, msg_id),
            )
            self.conn.commit()

    def get_unacked_messages(
        self,
        spec_id: Optional[str] = None,
        direction: Optional[str] = None,
    ) -> list[dict[str, Any]]:
        """Get unacknowledged messages, optionally filtered.

        Args:
            spec_id: Filter by spec ID.
            direction: Filter by direction ("to_worker" or "to_daemon").

        Returns:
            List of message rows as dicts.
        """
        query = "SELECT * FROM messages WHERE acked_at IS NULL"
        params: list = []

        if spec_id:
            query += " AND spec_id = ?"
            params.append(spec_id)
        if direction:
            query += " AND direction = ?"
            params.append(direction)

        query += " ORDER BY created_at ASC"

        cursor = self.conn.execute(query, params)
        return [self._row_to_dict(row) for row in cursor]

    def get_latest_progress(
        self,
        spec_id: str,
    ) -> Optional[dict[str, Any]]:
        """Get the most recent PROGRESS message for a spec.

        Returns:
            Message row as dict, or None.
        """
        cursor = self.conn.execute(
            "SELECT * FROM messages "
            "WHERE spec_id = ? AND msg_type = 'PROGRESS' AND direction = 'to_daemon' "
            "ORDER BY created_at DESC LIMIT 1",
            (spec_id,),
        )
        row = cursor.fetchone()
        return self._row_to_dict(row) if row else None

    def cleanup_old_messages(self, max_age_days: int = 7) -> int:
        """Delete acknowledged messages older than max_age_days.

        Returns:
            Number of rows deleted.
        """
        cutoff = time.time() - (max_age_days * 86400)
        with self.lock:
            cursor = self.conn.execute(
                "DELETE FROM messages WHERE acked_at IS NOT NULL AND acked_at < ?",
                (cutoff,),
            )
            self.conn.commit()
            return cursor.rowcount

    # ── JSON migration ────────────────────────────────────────────

    def migrate_from_json(self, queue_dir: Optional[str] = None) -> int:
        """Import existing q-*.json queue files into SQLite.

        Reads all q-NNN.json files from queue_dir (defaults to
        self.queue_dir), parses each one, and inserts a row into
        the specs table. Also migrates iteration-N.json files into
        the iterations table.

        Skips entries whose ID already exists in the database.

        Returns the number of specs imported.
        """
        src_dir = Path(queue_dir or self.queue_dir)
        if not src_dir.is_dir():
            return 0

        imported = 0

        with self.lock:
            for f in sorted(src_dir.iterdir()):
                # Match q-NNN.json but not q-NNN.spec.md,
                # q-NNN.telemetry.json, q-NNN.iteration-N.json, etc.
                if not f.name.endswith(".json"):
                    continue
                if not f.name.startswith("q-"):
                    continue
                if ".telemetry" in f.name or ".iteration-" in f.name:
                    continue
                # Must be exactly q-NNN.json
                stem = f.stem  # e.g. "q-001"
                if "." in stem:
                    continue

                try:
                    entry = json.loads(f.read_text(encoding="utf-8"))
                except (json.JSONDecodeError, OSError):
                    continue

                sid = entry.get("id", stem)

                # Skip if already exists
                existing = self.conn.execute(
                    "SELECT id FROM specs WHERE id = ?", (sid,)
                ).fetchone()
                if existing:
                    continue

                # Map JSON fields to specs table columns
                blocked_by = entry.get("blocked_by", [])
                sync_back_val = entry.get("sync_back", True)
                initial_ids = entry.get("initial_task_ids", [])

                self.conn.execute(
                    "INSERT INTO specs ("
                    "  id, spec_path, original_spec_path, worktree,"
                    "  priority, status, phase, submitted_at,"
                    "  first_running_at, last_iteration_at,"
                    "  last_worker, iteration, max_iterations,"
                    "  consecutive_failures, cooldown_until,"
                    "  tasks_done, tasks_total, sync_back,"
                    "  project, initial_task_ids,"
                    "  worker_timeout_seconds, failure_reason,"
                    "  needs_review_since, assigning_at,"
                    "  critic_passes, pre_iteration_tasks"
                    ") VALUES ("
                    "  ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,"
                    "  ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?"
                    ")",
                    (
                        sid,
                        entry.get("spec_path", ""),
                        entry.get("original_spec_path"),
                        entry.get("worktree"),
                        entry.get("priority", DEFAULT_PRIORITY),
                        entry.get("status", "queued"),
                        entry.get("phase", "execute"),
                        entry.get("submitted_at", self._now_iso()),
                        entry.get("first_running_at"),
                        entry.get("last_iteration_at"),
                        entry.get("last_worker"),
                        entry.get("iteration", 0),
                        entry.get("max_iterations", DEFAULT_MAX_ITERATIONS),
                        entry.get("consecutive_failures", 0),
                        entry.get("cooldown_until"),
                        entry.get("tasks_done", 0),
                        entry.get("tasks_total", 0),
                        1 if sync_back_val else 0,
                        entry.get("project"),
                        json.dumps(initial_ids)
                        if isinstance(initial_ids, list)
                        else initial_ids,
                        entry.get("worker_timeout_seconds"),
                        entry.get("failure_reason"),
                        entry.get("needs_review_since"),
                        entry.get("assigning_at"),
                        entry.get("critic_passes", 0),
                        json.dumps(entry.get("pre_iteration_tasks"))
                        if entry.get("pre_iteration_tasks")
                        else None,
                    ),
                )

                # Insert dependency edges
                if blocked_by:
                    for dep_id in blocked_by:
                        try:
                            self.conn.execute(
                                "INSERT INTO spec_dependencies "
                                "(spec_id, blocks_on) VALUES (?, ?)",
                                (sid, dep_id),
                            )
                        except sqlite3.IntegrityError:
                            pass  # dependency target may not exist

                # Migrate iteration files for this spec
                self._migrate_iteration_files(src_dir, sid)

                imported += 1

            if imported > 0:
                self._log_event(
                    "migrated",
                    f"Migrated {imported} spec(s) from JSON",
                    data={"count": imported},
                )
                self.conn.commit()

        return imported

    def _migrate_iteration_files(self, queue_dir: Path, spec_id: str) -> int:
        """Import iteration-N.json files for a spec into the iterations table.

        Called during migrate_from_json(). Must be called while
        holding self.lock and inside a transaction.

        Returns the number of iteration files imported.
        """
        prefix = f"{spec_id}.iteration-"
        count = 0

        for f in sorted(queue_dir.iterdir()):
            if not f.name.startswith(prefix):
                continue
            if not f.name.endswith(".json"):
                continue

            try:
                data = json.loads(f.read_text(encoding="utf-8"))
            except (json.JSONDecodeError, OSError):
                continue

            iteration = data.get("iteration", 0)
            phase = data.get("phase", "execute")
            worker_id = data.get("worker_id", "unknown")
            started_at = data.get("started_at", "")
            duration = data.get("duration_seconds", 0)

            # Compute ended_at from started_at + duration if missing
            ended_at = data.get("ended_at", "")
            if not ended_at and started_at and duration:
                try:
                    start_dt = datetime.fromisoformat(started_at)
                    end_dt = start_dt + timedelta(seconds=duration)
                    ended_at = end_dt.isoformat()
                except (ValueError, TypeError):
                    ended_at = started_at

            if not ended_at:
                ended_at = started_at or self._now_iso()

            if not started_at:
                started_at = self._now_iso()

            # Extract task counts
            tasks_completed = data.get("tasks_completed", 0)
            tasks_added = data.get("tasks_added", 0)
            tasks_skipped = data.get("tasks_skipped", 0)
            exit_code = data.get("exit_code")

            pre_pending = None
            post_pending = None
            pre_counts = data.get("pre_counts")
            post_counts = data.get("post_counts")
            if isinstance(pre_counts, dict):
                pre_pending = pre_counts.get("pending")
            if isinstance(post_counts, dict):
                post_pending = post_counts.get("pending")

            quality_score = data.get("quality_score")
            quality_breakdown = data.get("quality_breakdown")
            if isinstance(quality_breakdown, dict):
                quality_breakdown = json.dumps(quality_breakdown)

            try:
                self.conn.execute(
                    "INSERT OR IGNORE INTO iterations ("
                    "  spec_id, iteration, phase, worker_id,"
                    "  started_at, ended_at, duration_seconds,"
                    "  tasks_completed, tasks_added, tasks_skipped,"
                    "  exit_code, pre_pending, post_pending,"
                    "  quality_score, quality_breakdown"
                    ") VALUES ("
                    "  ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?"
                    ")",
                    (
                        spec_id,
                        iteration,
                        phase,
                        worker_id,
                        started_at,
                        ended_at,
                        duration,
                        tasks_completed,
                        tasks_added,
                        tasks_skipped,
                        exit_code,
                        pre_pending,
                        post_pending,
                        quality_score,
                        quality_breakdown,
                    ),
                )
                count += 1
            except sqlite3.IntegrityError:
                pass  # duplicate (spec_id, iteration, phase)

        return count

    # ── Recovery ───────────────────────────────────────────────────

    def recover_running_specs(self) -> list[str]:
        """Check all running specs for dead worker PIDs.

        If a spec is 'running' but its worker PID is dead (or reused),
        reset it to 'requeued'. Uses _is_pid_alive_and_ours for PID
        reuse detection.

        Returns list of spec IDs that were recovered.
        """
        recovered: list[str] = []
        with self.lock:
            cursor = self.conn.execute(
                "SELECT s.id, w.current_pid, w.id AS worker_id, "
                "p.started_at AS proc_started_at "
                "FROM specs s "
                "JOIN workers w ON w.current_spec_id = s.id "
                "LEFT JOIN processes p ON p.spec_id = s.id "
                "AND p.pid = w.current_pid AND p.ended_at IS NULL "
                "WHERE s.status = 'running'"
            )
            rows = cursor.fetchall()

            for row in rows:
                spec_id = row["id"]
                pid = row["current_pid"]
                worker_id = row["worker_id"]
                proc_started_at = row["proc_started_at"] or ""

                if pid is None:
                    # No PID recorded; treat as dead
                    alive = False
                else:
                    alive = self._is_pid_alive_and_ours(pid, proc_started_at)

                if not alive:
                    self.conn.execute(
                        "UPDATE specs SET status = 'requeued', "
                        "assigning_at = NULL WHERE id = ?",
                        (spec_id,),
                    )
                    # End the process record if it exists
                    if pid is not None:
                        self.conn.execute(
                            "UPDATE processes SET ended_at = ?, "
                            "exit_code = -1 "
                            "WHERE pid = ? AND spec_id = ? "
                            "AND ended_at IS NULL",
                            (self._now_iso(), pid, spec_id),
                        )
                    # Free the worker
                    self.conn.execute(
                        "UPDATE workers SET "
                        "current_spec_id = NULL, "
                        "current_pid = NULL, "
                        "start_time = NULL, "
                        "current_phase = NULL "
                        "WHERE id = ?",
                        (worker_id,),
                    )
                    self._log_event(
                        "recover_dead_pid",
                        f"Recovered spec with dead PID {pid}",
                        spec_id=spec_id,
                        data={
                            "pid": pid,
                            "worker_id": worker_id,
                        },
                        level="warn",
                    )
                    recovered.append(spec_id)

            if recovered:
                self.conn.commit()

        return recovered

    # ── Configuration methods ────────────────────────────────────────

    def get_config_value(self, key: str, default: Any = None) -> Any:
        """Read a configuration value from the config table.

        Args:
            key: The configuration key.
            default: Default value if key not found.

        Returns:
            The configuration value, or default if not found.
        """
        cursor = self.conn.execute(
            "SELECT value FROM config WHERE key = ?", (key,)
        )
        row = cursor.fetchone()
        if row is None:
            return default
        # Try to parse as JSON for complex types, otherwise return as string
        value = row["value"]
        if value.lower() == "true":
            return True
        if value.lower() == "false":
            return False
        try:
            return json.loads(value)
        except json.JSONDecodeError:
            return value

    def set_config_value(self, key: str, value: Any) -> None:
        """Set a configuration value in the config table.

        Args:
            key: The configuration key.
            value: The value to store (will be JSON-serialized if not a string).
        """
        with self.lock:
            # Convert value to string for storage
            if isinstance(value, bool):
                str_value = "true" if value else "false"
            elif isinstance(value, (dict, list)):
                str_value = json.dumps(value)
            else:
                str_value = str(value)

            self.conn.execute(
                "INSERT INTO config (key, value, updated_at) VALUES (?, ?, ?) "
                "ON CONFLICT(key) DO UPDATE SET value = excluded.value, "
                "updated_at = excluded.updated_at",
                (key, str_value, self._now_iso()),
            )
            self.conn.commit()

    # ── Notification tracking methods ─────────────────────────────────

    def record_notification(
        self, spec_id: str, status: str, channel: str
    ) -> bool:
        """Record that a notification was sent for a spec.

        Args:
            spec_id: The spec ID.
            status: The status (completed, failed, etc.).
            channel: The notification channel (slack, etc.).

        Returns:
            True if record was newly created, False if already exists.
        """
        with self.lock:
            cursor = self.conn.execute(
                "INSERT OR IGNORE INTO notifications "
                "(spec_id, status, channel, notified_at) "
                "VALUES (?, ?, ?, ?)",
                (spec_id, status, channel, self._now_iso()),
            )
            self.conn.commit()
            return cursor.rowcount > 0

    def has_notification_been_sent(self, spec_id: str, status: str) -> bool:
        """Check if a notification has already been sent for a spec.

        Args:
            spec_id: The spec ID.
            status: The status to check.

        Returns:
            True if notification has been sent, False otherwise.
        """
        cursor = self.conn.execute(
            "SELECT 1 FROM notifications WHERE spec_id = ? AND status = ?",
            (spec_id, status),
        )
        return cursor.fetchone() is not None

    # ── Parallel task dispatch ────────────────────────────────────────────────

    def populate_tasks_from_spec(
        self,
        spec_id: str,
        tasks: list,
    ) -> None:
        """Insert task rows from parsed BoiTask objects into the tasks table.

        Uses INSERT OR IGNORE so re-populating an already-populated spec
        is safe (won't overwrite in-progress task state).
        """
        with self.lock:
            for task in tasks:
                depends_on = json.dumps(list(task.blocked_by))
                self.conn.execute(
                    "INSERT OR IGNORE INTO tasks "
                    "(spec_id, task_id, title, status, depends_on) "
                    "VALUES (?, ?, ?, ?, ?)",
                    (
                        spec_id,
                        task.id,
                        task.title,
                        task.status if task.status in (
                            "PENDING", "DONE", "FAILED", "SKIPPED"
                        ) else "PENDING",
                        depends_on,
                    ),
                )
            self.conn.commit()

    def get_tasks_for_spec(self, spec_id: str) -> list[dict[str, Any]]:
        """Return all task rows for a spec from the tasks table."""
        cursor = self.conn.execute(
            "SELECT * FROM tasks WHERE spec_id = ? ORDER BY id",
            (spec_id,),
        )
        return [self._row_to_dict(row) for row in cursor]

    def get_eligible_task_ids(self, spec_id: str) -> list[str]:
        """Return PENDING task IDs whose dependencies are all DONE/SKIPPED.

        Excludes tasks that are already ASSIGNED/RUNNING (in-progress).
        """
        _RESOLVED = {"DONE", "SKIPPED"}
        _BLOCKED = {"ASSIGNED", "RUNNING"}

        tasks = self.get_tasks_for_spec(spec_id)
        if not tasks:
            return []

        status_by_id: dict[str, str] = {t["task_id"]: t["status"] for t in tasks}

        eligible: list[str] = []
        for task in tasks:
            if task["status"] != "PENDING":
                continue
            deps = json.loads(task.get("depends_on") or "[]")
            if all(status_by_id.get(dep) in _RESOLVED for dep in deps):
                eligible.append(task["task_id"])

        return eligible

    def assign_task_to_worker(
        self, spec_id: str, task_id: str, worker_id: str
    ) -> None:
        """Mark a task as ASSIGNED with the given worker."""
        with self.lock:
            now = self._now_iso()
            self.conn.execute(
                "UPDATE tasks SET status = 'ASSIGNED', worker_id = ?, started_at = ? "
                "WHERE spec_id = ? AND task_id = ? AND status = 'PENDING'",
                (worker_id, now, spec_id, task_id),
            )
            self.conn.commit()

    def update_task_worktree(
        self,
        spec_id: str,
        task_id: str,
        worktree_path: str,
        branch_name: str,
    ) -> None:
        """Store the task-specific worktree path and branch name."""
        with self.lock:
            self.conn.execute(
                "UPDATE tasks SET worktree_path = ?, branch_name = ? "
                "WHERE spec_id = ? AND task_id = ?",
                (worktree_path, branch_name, spec_id, task_id),
            )
            self.conn.commit()

    def complete_task(
        self,
        spec_id: str,
        task_id: str,
        status: str,
        output: str = "",
    ) -> None:
        """Mark a task DONE or FAILED with optional output text."""
        with self.lock:
            now = self._now_iso()
            self.conn.execute(
                "UPDATE tasks SET status = ?, completed_at = ?, output = ? "
                "WHERE spec_id = ? AND task_id = ?",
                (status, now, output, spec_id, task_id),
            )
            self.conn.commit()

    def all_tasks_terminal(self, spec_id: str) -> bool:
        """Return True if every task for the spec is in a terminal state.

        Terminal states: DONE, FAILED, SKIPPED.
        Returns True vacuously if there are no tasks.
        """
        _TERMINAL = ("DONE", "FAILED", "SKIPPED")
        tasks = self.get_tasks_for_spec(spec_id)
        if not tasks:
            return True
        return all(t["status"] in _TERMINAL for t in tasks)

    def any_task_failed(self, spec_id: str) -> bool:
        """Return True if any task for the spec has status FAILED."""
        cursor = self.conn.execute(
            "SELECT 1 FROM tasks WHERE spec_id = ? AND status = 'FAILED' LIMIT 1",
            (spec_id,),
        )
        return cursor.fetchone() is not None
