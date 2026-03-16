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
                from lib.spec_parser import parse_boi_spec

                initial_tasks = parse_boi_spec(content)
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
                    from lib.spec_parser import parse_boi_spec

                    content = Path(spec_path).read_text(encoding="utf-8")
                    tasks = parse_boi_spec(content)
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
        """Find the first worker not currently assigned to a spec.

        Returns the worker dict or None if all workers are busy.
        """
        cursor = self.conn.execute(
            "SELECT * FROM workers WHERE current_spec_id IS NULL ORDER BY id ASC"
        )
        row = cursor.fetchone()
        return self._row_to_dict(row) if row else None

    def assign_worker(
        self,
        worker_id: str,
        spec_id: str,
        pid: int,
        phase: str = "execute",
    ) -> None:
        """Mark a worker as busy with the given spec and PID.

        Records current_phase for crash recovery dispatch.
        """
        with self.lock:
            now = self._now_iso()
            self.conn.execute(
                "UPDATE workers SET "
                "current_spec_id = ?, "
                "current_pid = ?, "
                "start_time = ?, "
                "current_phase = ? "
                "WHERE id = ?",
                (spec_id, pid, now, phase, worker_id),
            )
            self._log_event(
                "worker_assigned",
                f"Worker {worker_id} assigned to {spec_id} (pid={pid}, phase={phase})",
                spec_id=spec_id,
                data={
                    "worker_id": worker_id,
                    "pid": pid,
                    "phase": phase,
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
                "current_phase = NULL "
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
