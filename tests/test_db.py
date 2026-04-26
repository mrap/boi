# test_db.py — Unit tests for BOI SQLite database layer (lib/db.py).
#
# Tests cover: schema creation, table existence, CHECK constraints,
# indexes, WAL mode, ID generation, and event logging.
#
# Uses stdlib unittest only (no pytest dependency).

import json
import os
import sqlite3
import sys
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

# Add parent directory to path so we can import lib modules
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import shutil

from lib.db import Database, DuplicateSpecError


class DbTestCase(unittest.TestCase):
    """Base test case that creates a temp directory with a Database."""

    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.db_path = os.path.join(self._tmpdir.name, "boi.db")
        self.queue_dir = os.path.join(self._tmpdir.name, "queue")
        self.db = Database(self.db_path, self.queue_dir)

    def tearDown(self) -> None:
        self.db.close()
        self._tmpdir.cleanup()


# ─── Schema Creation Tests ──────────────────────────────────────────────


class TestSchemaCreation(DbTestCase):
    """Verify that init_schema() creates all expected tables and indexes."""

    def test_specs_table_exists(self) -> None:
        cursor = self.db.conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='specs'"
        )
        self.assertIsNotNone(cursor.fetchone())

    def test_spec_dependencies_table_exists(self) -> None:
        cursor = self.db.conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='spec_dependencies'"
        )
        self.assertIsNotNone(cursor.fetchone())

    def test_workers_table_exists(self) -> None:
        cursor = self.db.conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='workers'"
        )
        self.assertIsNotNone(cursor.fetchone())

    def test_processes_table_exists(self) -> None:
        cursor = self.db.conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='processes'"
        )
        self.assertIsNotNone(cursor.fetchone())

    def test_iterations_table_exists(self) -> None:
        cursor = self.db.conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='iterations'"
        )
        self.assertIsNotNone(cursor.fetchone())

    def test_events_table_exists(self) -> None:
        cursor = self.db.conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='events'"
        )
        self.assertIsNotNone(cursor.fetchone())

    def test_all_six_tables_exist(self) -> None:
        cursor = self.db.conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
        )
        tables = {row["name"] for row in cursor}
        expected = {
            "specs", "spec_dependencies", "workers",
            "processes", "iterations", "events",
        }
        self.assertTrue(expected.issubset(tables))

    def test_indexes_exist(self) -> None:
        cursor = self.db.conn.execute(
            "SELECT name FROM sqlite_master WHERE type='index'"
        )
        indexes = {row["name"] for row in cursor}
        expected = {
            "idx_specs_last_worker",
            "idx_events_spec_id",
            "idx_iterations_spec_id",
        }
        self.assertTrue(expected.issubset(indexes))


# ─── WAL Mode Tests ─────────────────────────────────────────────────────


class TestWalMode(DbTestCase):
    """Verify WAL mode is enabled."""

    def test_wal_mode_enabled(self) -> None:
        cursor = self.db.conn.execute("PRAGMA journal_mode")
        mode = cursor.fetchone()[0]
        self.assertEqual(mode, "wal")

    def test_foreign_keys_enabled(self) -> None:
        cursor = self.db.conn.execute("PRAGMA foreign_keys")
        enabled = cursor.fetchone()[0]
        self.assertEqual(enabled, 1)


# ─── Specs Table Column Tests ───────────────────────────────────────────


class TestSpecsTableSchema(DbTestCase):
    """Verify the specs table has all required columns."""

    def _get_columns(self) -> dict[str, str]:
        cursor = self.db.conn.execute("PRAGMA table_info(specs)")
        return {row["name"]: row["type"] for row in cursor}

    def test_has_id_column(self) -> None:
        cols = self._get_columns()
        self.assertIn("id", cols)

    def test_has_assigning_at_column(self) -> None:
        cols = self._get_columns()
        self.assertIn("assigning_at", cols)
        self.assertEqual(cols["assigning_at"], "TEXT")

    def test_has_critic_passes_column(self) -> None:
        cols = self._get_columns()
        self.assertIn("critic_passes", cols)
        self.assertEqual(cols["critic_passes"], "INTEGER")

    def test_has_pre_iteration_tasks_column(self) -> None:
        cols = self._get_columns()
        self.assertIn("pre_iteration_tasks", cols)

    def test_has_all_required_columns(self) -> None:
        cols = self._get_columns()
        required = [
            "id", "spec_path", "original_spec_path", "worktree",
            "priority", "status", "phase", "submitted_at",
            "first_running_at", "last_iteration_at", "last_worker",
            "iteration", "max_iterations", "consecutive_failures",
            "cooldown_until", "tasks_done", "tasks_total",
            "sync_back", "project", "initial_task_ids",
            "worker_timeout_seconds", "failure_reason",
            "needs_review_since", "assigning_at", "critic_passes",
            "pre_iteration_tasks",
        ]
        for col in required:
            self.assertIn(col, cols, f"Missing column: {col}")


# ─── Workers Table Column Tests ─────────────────────────────────────────


class TestWorkersTableSchema(DbTestCase):
    """Verify the workers table has current_phase column."""

    def test_has_current_phase_column(self) -> None:
        cursor = self.db.conn.execute("PRAGMA table_info(workers)")
        cols = {row["name"]: row["type"] for row in cursor}
        self.assertIn("current_phase", cols)
        self.assertEqual(cols["current_phase"], "TEXT")


# ─── CHECK Constraint Tests ─────────────────────────────────────────────


class TestCheckConstraints(DbTestCase):
    """Verify CHECK constraints on specs table."""

    def _insert_spec(self, **overrides: Any) -> None:
        defaults = {
            "id": "q-001",
            "spec_path": "/tmp/spec.md",
            "status": "queued",
            "submitted_at": "2026-01-01T00:00:00+00:00",
        }
        defaults.update(overrides)
        cols = ", ".join(defaults.keys())
        placeholders = ", ".join("?" for _ in defaults)
        self.db.conn.execute(
            f"INSERT INTO specs ({cols}) VALUES ({placeholders})",
            tuple(defaults.values()),
        )

    def test_valid_status_accepted(self) -> None:
        valid_statuses = [
            "queued", "assigning", "running", "completed",
            "failed", "canceled", "needs_review", "requeued",
        ]
        for i, status in enumerate(valid_statuses):
            self._insert_spec(id=f"q-{i+1:03d}", status=status)
        cursor = self.db.conn.execute("SELECT COUNT(*) FROM specs")
        self.assertEqual(cursor.fetchone()[0], len(valid_statuses))

    def test_invalid_status_rejected(self) -> None:
        with self.assertRaises(sqlite3.IntegrityError):
            self._insert_spec(status="bogus")

    def test_valid_phase_accepted(self) -> None:
        valid_phases = ["execute", "task-verify", "evaluate", "decompose"]
        for i, phase in enumerate(valid_phases):
            self._insert_spec(id=f"q-{i+1:03d}", phase=phase)
        cursor = self.db.conn.execute("SELECT COUNT(*) FROM specs")
        self.assertEqual(cursor.fetchone()[0], len(valid_phases))

    def test_invalid_phase_rejected(self) -> None:
        with self.assertRaises(sqlite3.IntegrityError):
            self._insert_spec(phase="bogus")


# ─── ID Generation Tests ────────────────────────────────────────────────


class TestNextQueueId(DbTestCase):
    """Verify _next_queue_id() generates sequential IDs."""

    def test_first_id_is_q001(self) -> None:
        self.assertEqual(self.db._next_queue_id(), "q-001")

    def test_increments_after_existing(self) -> None:
        self.db.conn.execute(
            "INSERT INTO specs (id, spec_path, status, submitted_at) "
            "VALUES (?, ?, ?, ?)",
            ("q-001", "/tmp/spec.md", "queued", "2026-01-01T00:00:00+00:00"),
        )
        self.assertEqual(self.db._next_queue_id(), "q-002")

    def test_handles_gaps(self) -> None:
        """IDs are based on the max, not count. Gaps don't matter."""
        for qid in ["q-001", "q-003", "q-005"]:
            self.db.conn.execute(
                "INSERT INTO specs (id, spec_path, status, submitted_at) "
                "VALUES (?, ?, ?, ?)",
                (qid, "/tmp/spec.md", "queued", "2026-01-01T00:00:00+00:00"),
            )
        self.assertEqual(self.db._next_queue_id(), "q-006")

    def test_handles_large_numbers(self) -> None:
        self.db.conn.execute(
            "INSERT INTO specs (id, spec_path, status, submitted_at) "
            "VALUES (?, ?, ?, ?)",
            ("q-099", "/tmp/spec.md", "queued", "2026-01-01T00:00:00+00:00"),
        )
        self.assertEqual(self.db._next_queue_id(), "q-100")

    def test_ignores_non_numeric_ids(self) -> None:
        """Non-standard IDs (e.g. q-abc) are skipped."""
        self.db.conn.execute(
            "INSERT INTO specs (id, spec_path, status, submitted_at) "
            "VALUES (?, ?, ?, ?)",
            ("q-abc", "/tmp/spec.md", "queued", "2026-01-01T00:00:00+00:00"),
        )
        self.assertEqual(self.db._next_queue_id(), "q-001")


# ─── Event Logging Tests ────────────────────────────────────────────────


class TestLogEvent(DbTestCase):
    """Verify _log_event() inserts into the events table."""

    def test_log_event_inserts_row(self) -> None:
        self.db._log_event("test_event", "hello world", spec_id="q-001")
        self.db.conn.commit()
        cursor = self.db.conn.execute("SELECT * FROM events")
        row = cursor.fetchone()
        self.assertIsNotNone(row)
        self.assertEqual(row["event_type"], "test_event")
        self.assertEqual(row["message"], "hello world")
        self.assertEqual(row["spec_id"], "q-001")
        self.assertEqual(row["level"], "info")

    def test_log_event_with_data(self) -> None:
        data = {"key": "value", "count": 42}
        self.db._log_event("data_event", "with data", data=data)
        self.db.conn.commit()
        cursor = self.db.conn.execute("SELECT data FROM events")
        row = cursor.fetchone()
        parsed = json.loads(row["data"])
        self.assertEqual(parsed["key"], "value")
        self.assertEqual(parsed["count"], 42)

    def test_log_event_without_spec_id(self) -> None:
        self.db._log_event("system_event", "no spec")
        self.db.conn.commit()
        cursor = self.db.conn.execute("SELECT spec_id FROM events")
        row = cursor.fetchone()
        self.assertIsNone(row["spec_id"])

    def test_log_event_autoincrement_seq(self) -> None:
        self.db._log_event("ev1", "first")
        self.db._log_event("ev2", "second")
        self.db.conn.commit()
        cursor = self.db.conn.execute(
            "SELECT seq FROM events ORDER BY seq"
        )
        seqs = [row["seq"] for row in cursor]
        self.assertEqual(len(seqs), 2)
        self.assertLess(seqs[0], seqs[1])

    def test_log_event_custom_level(self) -> None:
        self.db._log_event("warn_event", "warning", level="warn")
        self.db.conn.commit()
        cursor = self.db.conn.execute("SELECT level FROM events")
        row = cursor.fetchone()
        self.assertEqual(row["level"], "warn")


# ─── Database Lifecycle Tests ────────────────────────────────────────────


class TestDatabaseLifecycle(DbTestCase):
    """Verify Database init and close behavior."""

    def test_creates_queue_dir(self) -> None:
        self.assertTrue(os.path.isdir(self.queue_dir))

    def test_creates_db_file(self) -> None:
        self.assertTrue(os.path.isfile(self.db_path))

    def test_idempotent_init_schema(self) -> None:
        """Calling init_schema() twice doesn't fail."""
        self.db.init_schema()
        cursor = self.db.conn.execute(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table'"
        )
        count = cursor.fetchone()[0]
        self.assertGreaterEqual(count, 6)

    def test_close_and_reopen(self) -> None:
        """Can close and reopen the database."""
        self.db.conn.execute(
            "INSERT INTO specs (id, spec_path, status, submitted_at) "
            "VALUES (?, ?, ?, ?)",
            ("q-001", "/tmp/spec.md", "queued", "2026-01-01T00:00:00+00:00"),
        )
        self.db.conn.commit()
        self.db.close()

        db2 = Database(self.db_path, self.queue_dir)
        cursor = db2.conn.execute("SELECT id FROM specs WHERE id='q-001'")
        row = cursor.fetchone()
        self.assertIsNotNone(row)
        self.assertEqual(row["id"], "q-001")
        db2.close()


# ─── Helper: create a temp spec file ──────────────────────────────────


class CrudTestCase(DbTestCase):
    """Base test case with helper to create temp spec files."""

    def _make_spec_file(
        self, content: str = "# Test Spec\n\n## Tasks\n\n1. PENDING: Do something\n"
    ) -> str:
        """Write a temp spec file and return its absolute path."""
        spec_path = os.path.join(self._tmpdir.name, "test-spec.md")
        Path(spec_path).write_text(content, encoding="utf-8")
        return spec_path

    def _make_spec_file_named(self, name: str, content: str = "# Spec\n") -> str:
        """Write a named temp spec file and return its absolute path."""
        spec_path = os.path.join(self._tmpdir.name, name)
        Path(spec_path).write_text(content, encoding="utf-8")
        return spec_path


# ─── Enqueue Tests ─────────────────────────────────────────────────────


class TestEnqueue(CrudTestCase):
    """Verify enqueue() inserts spec and copies file."""

    def test_enqueue_returns_dict_with_id(self) -> None:
        spec = self._make_spec_file()
        result = self.db.enqueue(spec)
        self.assertEqual(result["id"], "q-001")
        self.assertEqual(result["status"], "queued")
        self.assertEqual(result["phase"], "execute")
        self.assertEqual(result["iteration"], 0)

    def test_enqueue_copies_spec_file(self) -> None:
        spec = self._make_spec_file("# My spec\n")
        result = self.db.enqueue(spec)
        copy_path = os.path.join(self.queue_dir, "q-001.spec.md")
        self.assertTrue(os.path.isfile(copy_path))
        copied_content = Path(copy_path).read_text(encoding="utf-8")
        self.assertEqual(copied_content, "# My spec\n")

    def test_enqueue_stores_original_path(self) -> None:
        spec = self._make_spec_file()
        result = self.db.enqueue(spec)
        self.assertEqual(result["original_spec_path"], os.path.abspath(spec))

    def test_enqueue_respects_priority(self) -> None:
        spec = self._make_spec_file()
        result = self.db.enqueue(spec, priority=50)
        self.assertEqual(result["priority"], 50)

    def test_enqueue_respects_max_iterations(self) -> None:
        spec = self._make_spec_file()
        result = self.db.enqueue(spec, max_iterations=10)
        self.assertEqual(result["max_iterations"], 10)

    def test_enqueue_with_custom_queue_id(self) -> None:
        spec = self._make_spec_file()
        result = self.db.enqueue(spec, queue_id="q-042")
        self.assertEqual(result["id"], "q-042")

    def test_enqueue_with_worktree(self) -> None:
        spec = self._make_spec_file()
        result = self.db.enqueue(spec, checkout="/tmp/worktree")
        self.assertEqual(result["worktree"], "/tmp/worktree")

    def test_enqueue_with_project(self) -> None:
        spec = self._make_spec_file()
        result = self.db.enqueue(spec, project="my-project")
        self.assertEqual(result["project"], "my-project")

    def test_enqueue_sync_back_true_by_default(self) -> None:
        spec = self._make_spec_file()
        result = self.db.enqueue(spec)
        self.assertEqual(result["sync_back"], 1)

    def test_enqueue_sync_back_false(self) -> None:
        spec = self._make_spec_file()
        result = self.db.enqueue(spec, sync_back=False)
        self.assertEqual(result["sync_back"], 0)

    def test_enqueue_sequential_ids(self) -> None:
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        r1 = self.db.enqueue(s1)
        r2 = self.db.enqueue(s2)
        self.assertEqual(r1["id"], "q-001")
        self.assertEqual(r2["id"], "q-002")

    def test_enqueue_logs_event(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        cursor = self.db.conn.execute(
            "SELECT * FROM events WHERE event_type = 'queued'"
        )
        row = cursor.fetchone()
        self.assertIsNotNone(row)
        self.assertEqual(row["spec_id"], "q-001")

    def test_enqueue_with_dependencies(self) -> None:
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        r1 = self.db.enqueue(s1)
        r2 = self.db.enqueue(s2, blocked_by=[r1["id"]])
        # Verify dependency row exists
        cursor = self.db.conn.execute(
            "SELECT * FROM spec_dependencies WHERE spec_id = ?",
            (r2["id"],),
        )
        dep = cursor.fetchone()
        self.assertIsNotNone(dep)
        self.assertEqual(dep["blocks_on"], r1["id"])

    def test_enqueue_duplicate_active_spec_rejected(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        with self.assertRaises(DuplicateSpecError):
            self.db.enqueue(spec)

    def test_enqueue_allows_same_spec_after_cancel(self) -> None:
        spec = self._make_spec_file()
        r1 = self.db.enqueue(spec)
        self.db.cancel(r1["id"])
        r2 = self.db.enqueue(spec)
        self.assertEqual(r2["status"], "queued")

    def test_enqueue_allows_same_spec_after_complete(self) -> None:
        spec = self._make_spec_file()
        r1 = self.db.enqueue(spec)
        self.db.complete(r1["id"])
        r2 = self.db.enqueue(spec)
        self.assertEqual(r2["status"], "queued")


# ─── GetSpec Tests ─────────────────────────────────────────────────────


class TestGetSpec(CrudTestCase):
    """Verify get_spec() retrieves a single spec."""

    def test_get_existing_spec(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        result = self.db.get_spec("q-001")
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], "q-001")
        self.assertEqual(result["status"], "queued")

    def test_get_nonexistent_spec_returns_none(self) -> None:
        result = self.db.get_spec("q-999")
        self.assertIsNone(result)

    def test_get_spec_returns_all_fields(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec, priority=42, max_iterations=5)
        result = self.db.get_spec("q-001")
        self.assertEqual(result["priority"], 42)
        self.assertEqual(result["max_iterations"], 5)
        self.assertEqual(result["iteration"], 0)
        self.assertEqual(result["phase"], "execute")


# ─── GetQueue Tests ────────────────────────────────────────────────────


class TestGetQueue(CrudTestCase):
    """Verify get_queue() returns specs sorted by priority."""

    def test_empty_queue(self) -> None:
        result = self.db.get_queue()
        self.assertEqual(result, [])

    def test_single_spec(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        result = self.db.get_queue()
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]["id"], "q-001")

    def test_priority_ordering(self) -> None:
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        s3 = self._make_spec_file_named("s3.md")
        self.db.enqueue(s1, priority=200)
        self.db.enqueue(s2, priority=50)
        self.db.enqueue(s3, priority=100)
        result = self.db.get_queue()
        self.assertEqual(len(result), 3)
        self.assertEqual(result[0]["priority"], 50)
        self.assertEqual(result[1]["priority"], 100)
        self.assertEqual(result[2]["priority"], 200)

    def test_includes_all_statuses(self) -> None:
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1)
        r2 = self.db.enqueue(s2)
        self.db.cancel(r2["id"])
        result = self.db.get_queue()
        self.assertEqual(len(result), 2)
        statuses = {r["status"] for r in result}
        self.assertEqual(statuses, {"queued", "canceled"})


# ─── Cancel Tests ──────────────────────────────────────────────────────


class TestCancel(CrudTestCase):
    """Verify cancel() sets status to 'canceled'."""

    def test_cancel_sets_status(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.cancel("q-001")
        result = self.db.get_spec("q-001")
        self.assertEqual(result["status"], "canceled")

    def test_cancel_nonexistent_raises(self) -> None:
        with self.assertRaises(ValueError):
            self.db.cancel("q-999")

    def test_cancel_logs_event(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.cancel("q-001")
        cursor = self.db.conn.execute(
            "SELECT * FROM events WHERE event_type = 'canceled'"
        )
        row = cursor.fetchone()
        self.assertIsNotNone(row)
        self.assertEqual(row["spec_id"], "q-001")

    def test_cancel_ends_active_process(self) -> None:
        """Canceling a spec must end any active process record.

        Bug: cancel() only set status to 'canceled' but left the process
        record with ended_at=NULL. Ghost processes accumulated and caused
        worker starvation (workers appeared busy with dead specs).
        """
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.register_worker("w-1", "/tmp/wt-1")
        self.db.assign_worker("w-1", "q-001", pid=12345)
        # Start a process (iteration + phase are NOT NULL in schema)
        self.db.conn.execute(
            "INSERT INTO processes (spec_id, worker_id, pid, iteration, phase, started_at) "
            "VALUES (?, ?, ?, ?, ?, ?)",
            ("q-001", "w-1", 12345, 1, "execute", "2026-03-16T00:00:00Z"),
        )
        self.db.conn.commit()

        self.db.cancel("q-001")

        # Process should be ended
        active = self.db.conn.execute(
            "SELECT * FROM processes WHERE spec_id = 'q-001' AND ended_at IS NULL"
        ).fetchall()
        self.assertEqual(len(active), 0, "Cancel should end active processes")

    def test_cancel_frees_assigned_worker(self) -> None:
        """Canceling a spec must free the worker assigned to it.

        Bug: cancel() left the worker's current_spec_id pointing to the
        canceled spec. The worker appeared busy and couldn't be assigned
        new work.
        """
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.register_worker("w-1", "/tmp/wt-1")
        self.db.assign_worker("w-1", "q-001", pid=12345)

        self.db.cancel("q-001")

        worker = self.db.get_worker("w-1")
        self.assertIsNone(
            worker["current_spec_id"],
            "Cancel should free the worker assigned to the canceled spec",
        )


# ─── Purge Tests ───────────────────────────────────────────────────────


class TestPurge(CrudTestCase):
    """Verify purge() removes specs and files."""

    def test_purge_removes_completed_specs(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.complete("q-001")
        purged = self.db.purge()
        self.assertEqual(len(purged), 1)
        self.assertEqual(purged[0]["id"], "q-001")
        # Verify deleted from DB
        self.assertIsNone(self.db.get_spec("q-001"))

    def test_purge_removes_failed_specs(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        # Manually set to failed for this test
        with self.db.lock:
            self.db.conn.execute(
                "UPDATE specs SET status = 'failed' WHERE id = 'q-001'"
            )
            self.db.conn.commit()
        purged = self.db.purge()
        self.assertEqual(len(purged), 1)

    def test_purge_removes_canceled_specs(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.cancel("q-001")
        purged = self.db.purge()
        self.assertEqual(len(purged), 1)

    def test_purge_skips_queued_specs(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        purged = self.db.purge()
        self.assertEqual(len(purged), 0)
        # Still in DB
        self.assertIsNotNone(self.db.get_spec("q-001"))

    def test_purge_cleans_spec_copy_file(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.complete("q-001")
        copy_path = os.path.join(self.queue_dir, "q-001.spec.md")
        self.assertTrue(os.path.isfile(copy_path))
        self.db.purge()
        self.assertFalse(os.path.isfile(copy_path))

    def test_purge_dry_run_does_not_delete(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.complete("q-001")
        purged = self.db.purge(dry_run=True)
        self.assertEqual(len(purged), 1)
        # Still in DB
        self.assertIsNotNone(self.db.get_spec("q-001"))
        # File still exists
        copy_path = os.path.join(self.queue_dir, "q-001.spec.md")
        self.assertTrue(os.path.isfile(copy_path))

    def test_purge_custom_statuses(self) -> None:
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1)
        self.db.enqueue(s2)
        self.db.cancel("q-001")
        self.db.complete("q-002")
        # Only purge canceled
        purged = self.db.purge(statuses=["canceled"])
        self.assertEqual(len(purged), 1)
        self.assertEqual(purged[0]["id"], "q-001")
        # q-002 (completed) still exists
        self.assertIsNotNone(self.db.get_spec("q-002"))

    def test_purge_empty_queue(self) -> None:
        purged = self.db.purge()
        self.assertEqual(purged, [])


# ─── Complete Tests ────────────────────────────────────────────────────


class TestComplete(CrudTestCase):
    """Verify complete() sets status and handles sync_back."""

    def test_complete_sets_status(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.complete("q-001", tasks_done=3, tasks_total=3)
        result = self.db.get_spec("q-001")
        self.assertEqual(result["status"], "completed")
        self.assertEqual(result["tasks_done"], 3)
        self.assertEqual(result["tasks_total"], 3)

    def test_complete_nonexistent_raises(self) -> None:
        with self.assertRaises(ValueError):
            self.db.complete("q-999")

    def test_complete_logs_event(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.complete("q-001", tasks_done=2, tasks_total=5)
        cursor = self.db.conn.execute(
            "SELECT * FROM events WHERE event_type = 'completed'"
        )
        row = cursor.fetchone()
        self.assertIsNotNone(row)
        self.assertEqual(row["spec_id"], "q-001")
        data = json.loads(row["data"])
        self.assertEqual(data["tasks_done"], 2)
        self.assertEqual(data["tasks_total"], 5)

    def test_complete_sync_back_copies_spec(self) -> None:
        """When sync_back=True, complete() copies the queue spec
        back to the original location."""
        original_content = "# Original\n"
        spec = self._make_spec_file(original_content)
        self.db.enqueue(spec, sync_back=True)

        # Modify the queue copy to simulate worker edits
        copy_path = os.path.join(self.queue_dir, "q-001.spec.md")
        modified_content = "# Modified by worker\n"
        Path(copy_path).write_text(modified_content, encoding="utf-8")

        self.db.complete("q-001")

        # Original file should now have the modified content
        result_content = Path(spec).read_text(encoding="utf-8")
        self.assertEqual(result_content, modified_content)

    def test_complete_sync_back_false_does_not_copy(self) -> None:
        """When sync_back=False, complete() does NOT copy the spec back."""
        original_content = "# Original\n"
        spec = self._make_spec_file(original_content)
        self.db.enqueue(spec, sync_back=False)

        # Modify the queue copy
        copy_path = os.path.join(self.queue_dir, "q-001.spec.md")
        modified_content = "# Modified by worker\n"
        Path(copy_path).write_text(modified_content, encoding="utf-8")

        self.db.complete("q-001")

        # Original file should still have the original content
        result_content = Path(spec).read_text(encoding="utf-8")
        self.assertEqual(result_content, original_content)


# ─── PickNextSpec Tests ────────────────────────────────────────────────


class TestPickNextSpec(CrudTestCase):
    """Verify pick_next_spec() picks, respects cooldown and deps."""

    def test_picks_highest_priority_queued(self) -> None:
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1, priority=200)
        self.db.enqueue(s2, priority=50)
        result = self.db.pick_next_spec()
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], "q-002")  # lower priority = higher
        self.assertEqual(result["status"], "assigning")

    def test_picks_requeued_specs(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.requeue("q-001", tasks_done=1, tasks_total=3)
        result = self.db.pick_next_spec()
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], "q-001")
        self.assertEqual(result["status"], "assigning")

    def test_returns_none_on_empty_queue(self) -> None:
        result = self.db.pick_next_spec()
        self.assertIsNone(result)

    def test_returns_none_all_completed(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.complete("q-001")
        result = self.db.pick_next_spec()
        self.assertIsNone(result)

    def test_skips_blocked_ids(self) -> None:
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1, priority=50)
        self.db.enqueue(s2, priority=100)
        result = self.db.pick_next_spec(blocked_ids={"q-001"})
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], "q-002")

    def test_returns_none_all_blocked(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        result = self.db.pick_next_spec(blocked_ids={"q-001"})
        self.assertIsNone(result)

    def test_sets_assigning_at_timestamp(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        result = self.db.pick_next_spec()
        self.assertIsNotNone(result["assigning_at"])

    def test_skips_spec_in_cooldown(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        # Set cooldown 1 hour in the future
        future = (
            datetime.now(timezone.utc) + timedelta(hours=1)
        ).isoformat()
        with self.db.lock:
            self.db.conn.execute(
                "UPDATE specs SET cooldown_until = ? WHERE id = 'q-001'",
                (future,),
            )
            self.db.conn.commit()
        result = self.db.pick_next_spec()
        self.assertIsNone(result)

    def test_picks_spec_past_cooldown(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        # Set cooldown 1 hour in the past
        past = (
            datetime.now(timezone.utc) - timedelta(hours=1)
        ).isoformat()
        with self.db.lock:
            self.db.conn.execute(
                "UPDATE specs SET cooldown_until = ? WHERE id = 'q-001'",
                (past,),
            )
            self.db.conn.commit()
        result = self.db.pick_next_spec()
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], "q-001")

    def test_skips_spec_with_unfinished_dependency(self) -> None:
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        r1 = self.db.enqueue(s1)
        self.db.enqueue(s2, blocked_by=[r1["id"]])
        # q-001 is queued (not completed), so q-002 is blocked
        # Pick should return q-001 (the unblocked one)
        result = self.db.pick_next_spec()
        self.assertEqual(result["id"], "q-001")

    def test_picks_spec_after_dependency_completed(self) -> None:
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        r1 = self.db.enqueue(s1)
        self.db.enqueue(s2, blocked_by=[r1["id"]])
        self.db.complete(r1["id"])
        result = self.db.pick_next_spec()
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], "q-002")

    def test_logs_assigning_event(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.pick_next_spec()
        cursor = self.db.conn.execute(
            "SELECT * FROM events WHERE event_type = 'assigning'"
        )
        row = cursor.fetchone()
        self.assertIsNotNone(row)
        self.assertEqual(row["spec_id"], "q-001")


# ─── SetRunning Tests ──────────────────────────────────────────────────


class TestSetRunning(CrudTestCase):
    """Verify set_running() phase-aware iteration increment."""

    def _enqueue_and_assign(self, spec_name: str = "spec.md") -> str:
        """Helper: enqueue a spec and pick it (to 'assigning')."""
        spec = self._make_spec_file_named(spec_name)
        result = self.db.enqueue(spec)
        self.db.pick_next_spec()
        return result["id"]

    def test_execute_phase_increments_iteration(self) -> None:
        sid = self._enqueue_and_assign()
        self.db.set_running(sid, "w-1", phase="execute")
        spec = self.db.get_spec(sid)
        self.assertEqual(spec["iteration"], 1)
        self.assertEqual(spec["status"], "running")
        self.assertEqual(spec["phase"], "execute")

    def test_critic_phase_does_not_increment_iteration(self) -> None:
        sid = self._enqueue_and_assign()
        # First run as execute to set iteration to 1
        self.db.set_running(sid, "w-1", phase="execute")
        # Requeue and re-assign
        self.db.requeue(sid)
        self.db.pick_next_spec()
        # Run as critic
        self.db.set_running(sid, "w-1", phase="task-verify")
        spec = self.db.get_spec(sid)
        # iteration should still be 1 (critic doesn't increment)
        self.assertEqual(spec["iteration"], 1)
        self.assertEqual(spec["phase"], "task-verify")

    def test_evaluate_phase_does_not_increment_iteration(self) -> None:
        sid = self._enqueue_and_assign()
        self.db.set_running(sid, "w-1", phase="execute")
        self.db.requeue(sid)
        self.db.pick_next_spec()
        self.db.set_running(sid, "w-1", phase="evaluate")
        spec = self.db.get_spec(sid)
        self.assertEqual(spec["iteration"], 1)
        self.assertEqual(spec["phase"], "evaluate")

    def test_decompose_phase_does_not_increment_iteration(self) -> None:
        sid = self._enqueue_and_assign()
        self.db.set_running(sid, "w-1", phase="execute")
        self.db.requeue(sid)
        self.db.pick_next_spec()
        self.db.set_running(sid, "w-1", phase="decompose")
        spec = self.db.get_spec(sid)
        self.assertEqual(spec["iteration"], 1)
        self.assertEqual(spec["phase"], "decompose")

    def test_multiple_execute_phases_increment_sequentially(self) -> None:
        sid = self._enqueue_and_assign()
        for expected_iter in range(1, 4):
            self.db.set_running(sid, "w-1", phase="execute")
            spec = self.db.get_spec(sid)
            self.assertEqual(spec["iteration"], expected_iter)
            if expected_iter < 3:
                self.db.requeue(sid)
                self.db.pick_next_spec()

    def test_sets_first_running_at_on_first_run(self) -> None:
        sid = self._enqueue_and_assign()
        self.db.set_running(sid, "w-1")
        spec = self.db.get_spec(sid)
        self.assertIsNotNone(spec["first_running_at"])

    def test_preserves_first_running_at_on_subsequent_runs(self) -> None:
        sid = self._enqueue_and_assign()
        self.db.set_running(sid, "w-1")
        first_running = self.db.get_spec(sid)["first_running_at"]
        self.db.requeue(sid)
        self.db.pick_next_spec()
        self.db.set_running(sid, "w-1")
        spec = self.db.get_spec(sid)
        self.assertEqual(spec["first_running_at"], first_running)

    def test_sets_last_worker(self) -> None:
        sid = self._enqueue_and_assign()
        self.db.set_running(sid, "worker-alpha")
        spec = self.db.get_spec(sid)
        self.assertEqual(spec["last_worker"], "worker-alpha")

    def test_clears_assigning_at(self) -> None:
        sid = self._enqueue_and_assign()
        # Verify assigning_at was set by pick_next_spec
        spec = self.db.get_spec(sid)
        self.assertIsNotNone(spec["assigning_at"])
        # After set_running, assigning_at should be cleared
        self.db.set_running(sid, "w-1")
        spec = self.db.get_spec(sid)
        self.assertIsNone(spec["assigning_at"])

    def test_sets_last_iteration_at(self) -> None:
        sid = self._enqueue_and_assign()
        self.db.set_running(sid, "w-1")
        spec = self.db.get_spec(sid)
        self.assertIsNotNone(spec["last_iteration_at"])

    def test_nonexistent_spec_raises(self) -> None:
        with self.assertRaises(ValueError):
            self.db.set_running("q-999", "w-1")

    def test_logs_running_event(self) -> None:
        sid = self._enqueue_and_assign()
        self.db.set_running(sid, "w-1", phase="execute")
        cursor = self.db.conn.execute(
            "SELECT * FROM events WHERE event_type = 'running'"
        )
        row = cursor.fetchone()
        self.assertIsNotNone(row)
        self.assertEqual(row["spec_id"], sid)
        data = json.loads(row["data"])
        self.assertEqual(data["phase"], "execute")
        self.assertEqual(data["iteration"], 1)

    def test_snapshots_pre_iteration_tasks(self) -> None:
        content = (
            "# Spec\n\n## Tasks\n\n"
            "1. DONE: First task\n"
            "2. PENDING: Second task\n"
        )
        spec = self._make_spec_file(content)
        self.db.enqueue(spec)
        self.db.pick_next_spec()
        self.db.set_running("q-001", "w-1")
        spec_row = self.db.get_spec("q-001")
        pre_tasks = json.loads(spec_row["pre_iteration_tasks"])
        # Should have captured task statuses
        self.assertIsInstance(pre_tasks, dict)


# ─── MaxIterations Tests ──────────────────────────────────────────────


class TestMaxIterations(CrudTestCase):
    """Verify max_iterations checks count execute-phase only."""

    def test_max_iterations_counts_execute_only(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec, max_iterations=2)
        sid = "q-001"

        # Execute 1
        self.db.pick_next_spec()
        self.db.set_running(sid, "w-1", phase="execute")
        self.assertFalse(self.db.has_reached_max_iterations(sid))

        # Critic (doesn't count)
        self.db.requeue(sid)
        self.db.pick_next_spec()
        self.db.set_running(sid, "w-1", phase="task-verify")
        self.assertFalse(self.db.has_reached_max_iterations(sid))

        # Execute 2
        self.db.requeue(sid)
        self.db.pick_next_spec()
        self.db.set_running(sid, "w-1", phase="execute")
        self.assertTrue(self.db.has_reached_max_iterations(sid))

    def test_has_reached_max_nonexistent_raises(self) -> None:
        with self.assertRaises(ValueError):
            self.db.has_reached_max_iterations("q-999")


# ─── Requeue Tests ────────────────────────────────────────────────────


class TestRequeue(CrudTestCase):
    """Verify requeue() transitions and resets counters."""

    def test_requeue_sets_status(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.requeue("q-001", tasks_done=2, tasks_total=5)
        result = self.db.get_spec("q-001")
        self.assertEqual(result["status"], "requeued")
        self.assertEqual(result["tasks_done"], 2)
        self.assertEqual(result["tasks_total"], 5)

    def test_requeue_resets_consecutive_failures(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.record_failure("q-001")
        self.db.record_failure("q-001")
        # Verify failures were recorded
        s = self.db.get_spec("q-001")
        self.assertEqual(s["consecutive_failures"], 2)
        # Requeue should reset
        self.db.requeue("q-001")
        result = self.db.get_spec("q-001")
        self.assertEqual(result["consecutive_failures"], 0)

    def test_requeue_clears_cooldown(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.record_failure("q-001")
        s = self.db.get_spec("q-001")
        self.assertIsNotNone(s["cooldown_until"])
        self.db.requeue("q-001")
        result = self.db.get_spec("q-001")
        self.assertIsNone(result["cooldown_until"])

    def test_requeue_nonexistent_raises(self) -> None:
        with self.assertRaises(ValueError):
            self.db.requeue("q-999")

    def test_requeue_logs_event(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.requeue("q-001", tasks_done=1, tasks_total=3)
        cursor = self.db.conn.execute(
            "SELECT * FROM events WHERE event_type = 'requeued'"
        )
        row = cursor.fetchone()
        self.assertIsNotNone(row)
        self.assertEqual(row["spec_id"], "q-001")
        data = json.loads(row["data"])
        self.assertEqual(data["tasks_done"], 1)


# ─── Fail Tests ───────────────────────────────────────────────────────


class TestFail(CrudTestCase):
    """Verify fail() sets status and reason."""

    def test_fail_sets_status(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.fail("q-001", reason="max iterations exceeded")
        result = self.db.get_spec("q-001")
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["failure_reason"], "max iterations exceeded")

    def test_fail_without_reason(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.fail("q-001")
        result = self.db.get_spec("q-001")
        self.assertEqual(result["status"], "failed")
        self.assertIsNone(result["failure_reason"])

    def test_fail_nonexistent_raises(self) -> None:
        with self.assertRaises(ValueError):
            self.db.fail("q-999")

    def test_fail_logs_event(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.fail("q-001", reason="test failure")
        cursor = self.db.conn.execute(
            "SELECT * FROM events WHERE event_type = 'failed'"
        )
        row = cursor.fetchone()
        self.assertIsNotNone(row)
        self.assertEqual(row["level"], "warn")
        data = json.loads(row["data"])
        self.assertEqual(data["reason"], "test failure")


# ─── RecordFailure Tests ──────────────────────────────────────────────


class TestRecordFailure(CrudTestCase):
    """Verify record_failure() increments count and applies cooldown."""

    def test_increments_consecutive_failures(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.record_failure("q-001")
        result = self.db.get_spec("q-001")
        self.assertEqual(result["consecutive_failures"], 1)
        self.db.record_failure("q-001")
        result = self.db.get_spec("q-001")
        self.assertEqual(result["consecutive_failures"], 2)

    def test_sets_cooldown_until(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.record_failure("q-001")
        result = self.db.get_spec("q-001")
        self.assertIsNotNone(result["cooldown_until"])

    def test_returns_false_below_max(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        result = self.db.record_failure("q-001")
        self.assertFalse(result)

    def test_returns_true_at_max(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        from lib.db import MAX_CONSECUTIVE_FAILURES
        for i in range(MAX_CONSECUTIVE_FAILURES - 1):
            self.assertFalse(self.db.record_failure("q-001"))
        self.assertTrue(self.db.record_failure("q-001"))

    def test_nonexistent_spec_raises(self) -> None:
        with self.assertRaises(ValueError):
            self.db.record_failure("q-999")

    def test_logs_failure_event(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.record_failure("q-001")
        cursor = self.db.conn.execute(
            "SELECT * FROM events WHERE event_type = 'failure_recorded'"
        )
        row = cursor.fetchone()
        self.assertIsNotNone(row)
        self.assertEqual(row["level"], "warn")


# ─── RecoverStaleAssigning Tests ──────────────────────────────────────


class TestRecoverStaleAssigning(CrudTestCase):
    """Verify recover_stale_assigning() resets stuck specs."""

    def test_recovers_stuck_spec(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        # Manually set to assigning with old timestamp
        old_time = (
            datetime.now(timezone.utc) - timedelta(seconds=120)
        ).isoformat()
        with self.db.lock:
            self.db.conn.execute(
                "UPDATE specs SET status = 'assigning', "
                "assigning_at = ? WHERE id = 'q-001'",
                (old_time,),
            )
            self.db.conn.commit()

        recovered = self.db.recover_stale_assigning(timeout_seconds=60)
        self.assertEqual(recovered, ["q-001"])
        result = self.db.get_spec("q-001")
        self.assertEqual(result["status"], "requeued")
        self.assertIsNone(result["assigning_at"])

    def test_does_not_recover_recent_assigning(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        # Set to assigning with very recent timestamp
        now = datetime.now(timezone.utc).isoformat()
        with self.db.lock:
            self.db.conn.execute(
                "UPDATE specs SET status = 'assigning', "
                "assigning_at = ? WHERE id = 'q-001'",
                (now,),
            )
            self.db.conn.commit()

        recovered = self.db.recover_stale_assigning(timeout_seconds=60)
        self.assertEqual(recovered, [])
        result = self.db.get_spec("q-001")
        self.assertEqual(result["status"], "assigning")

    def test_recovers_multiple_stuck_specs(self) -> None:
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1)
        self.db.enqueue(s2)
        old_time = (
            datetime.now(timezone.utc) - timedelta(seconds=120)
        ).isoformat()
        with self.db.lock:
            for sid in ["q-001", "q-002"]:
                self.db.conn.execute(
                    "UPDATE specs SET status = 'assigning', "
                    "assigning_at = ? WHERE id = ?",
                    (old_time, sid),
                )
            self.db.conn.commit()

        recovered = self.db.recover_stale_assigning(timeout_seconds=60)
        self.assertEqual(sorted(recovered), ["q-001", "q-002"])

    def test_does_not_touch_non_assigning_specs(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        # spec is 'queued', not 'assigning'
        recovered = self.db.recover_stale_assigning(timeout_seconds=60)
        self.assertEqual(recovered, [])
        result = self.db.get_spec("q-001")
        self.assertEqual(result["status"], "queued")

    def test_logs_recovery_event(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        old_time = (
            datetime.now(timezone.utc) - timedelta(seconds=120)
        ).isoformat()
        with self.db.lock:
            self.db.conn.execute(
                "UPDATE specs SET status = 'assigning', "
                "assigning_at = ? WHERE id = 'q-001'",
                (old_time,),
            )
            self.db.conn.commit()

        self.db.recover_stale_assigning(timeout_seconds=60)
        cursor = self.db.conn.execute(
            "SELECT * FROM events "
            "WHERE event_type = 'recover_stale_assigning'"
        )
        row = cursor.fetchone()
        self.assertIsNotNone(row)
        self.assertEqual(row["spec_id"], "q-001")
        self.assertEqual(row["level"], "warn")


# ─── Full Lifecycle Tests ─────────────────────────────────────────────


class TestFullLifecycle(CrudTestCase):
    """End-to-end lifecycle: enqueue → pick → run → requeue → complete."""

    def test_queued_to_completed_lifecycle(self) -> None:
        spec = self._make_spec_file()
        # Enqueue
        self.db.enqueue(spec)
        s = self.db.get_spec("q-001")
        self.assertEqual(s["status"], "queued")
        self.assertEqual(s["iteration"], 0)

        # Pick
        picked = self.db.pick_next_spec()
        self.assertEqual(picked["status"], "assigning")

        # Run (iteration 1)
        self.db.set_running("q-001", "w-1", phase="execute")
        s = self.db.get_spec("q-001")
        self.assertEqual(s["status"], "running")
        self.assertEqual(s["iteration"], 1)

        # Requeue (still pending tasks)
        self.db.requeue("q-001", tasks_done=1, tasks_total=3)
        s = self.db.get_spec("q-001")
        self.assertEqual(s["status"], "requeued")

        # Pick again
        self.db.pick_next_spec()
        self.db.set_running("q-001", "w-1", phase="execute")
        s = self.db.get_spec("q-001")
        self.assertEqual(s["iteration"], 2)

        # Complete
        self.db.complete("q-001", tasks_done=3, tasks_total=3)
        s = self.db.get_spec("q-001")
        self.assertEqual(s["status"], "completed")

    def test_queued_to_failed_lifecycle(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.pick_next_spec()
        self.db.set_running("q-001", "w-1")
        self.db.fail("q-001", reason="max iterations")
        s = self.db.get_spec("q-001")
        self.assertEqual(s["status"], "failed")
        self.assertEqual(s["failure_reason"], "max iterations")

    def test_execute_then_critic_iteration_count(self) -> None:
        """Run 2 execute + 1 critic. Iteration should be 2, not 3."""
        spec = self._make_spec_file()
        self.db.enqueue(spec, max_iterations=3)
        sid = "q-001"

        # Execute 1
        self.db.pick_next_spec()
        self.db.set_running(sid, "w-1", phase="execute")
        self.assertEqual(self.db.get_spec(sid)["iteration"], 1)

        # Critic (doesn't increment)
        self.db.requeue(sid)
        self.db.pick_next_spec()
        self.db.set_running(sid, "w-1", phase="task-verify")
        self.assertEqual(self.db.get_spec(sid)["iteration"], 1)

        # Execute 2
        self.db.requeue(sid)
        self.db.pick_next_spec()
        self.db.set_running(sid, "w-1", phase="execute")
        self.assertEqual(self.db.get_spec(sid)["iteration"], 2)

        # Has NOT reached max (2 < 3)
        self.assertFalse(self.db.has_reached_max_iterations(sid))

        # Execute 3
        self.db.requeue(sid)
        self.db.pick_next_spec()
        self.db.set_running(sid, "w-1", phase="execute")
        self.assertEqual(self.db.get_spec(sid)["iteration"], 3)

        # NOW reached max (3 >= 3)
        self.assertTrue(self.db.has_reached_max_iterations(sid))


# ─── Worker Management Tests ──────────────────────────────────────────


class TestRegisterWorker(CrudTestCase):
    """Verify register_worker() inserts or updates workers."""

    def test_register_new_worker(self) -> None:
        self.db.register_worker("w-1", "/tmp/worktree-1")
        w = self.db.get_worker("w-1")
        self.assertIsNotNone(w)
        self.assertEqual(w["id"], "w-1")
        self.assertEqual(w["worktree_path"], "/tmp/worktree-1")
        self.assertIsNone(w["current_spec_id"])
        self.assertIsNone(w["current_pid"])
        self.assertIsNone(w["current_phase"])

    def test_register_worker_upsert(self) -> None:
        self.db.register_worker("w-1", "/tmp/old-path")
        self.db.register_worker("w-1", "/tmp/new-path")
        w = self.db.get_worker("w-1")
        self.assertEqual(w["worktree_path"], "/tmp/new-path")

    def test_register_multiple_workers(self) -> None:
        self.db.register_worker("w-1", "/tmp/wt-1")
        self.db.register_worker("w-2", "/tmp/wt-2")
        self.assertEqual(self.db.get_worker("w-1")["id"], "w-1")
        self.assertEqual(self.db.get_worker("w-2")["id"], "w-2")

    def test_get_worker_nonexistent(self) -> None:
        self.assertIsNone(self.db.get_worker("w-nonexistent"))


class TestGetFreeWorker(CrudTestCase):
    """Verify get_free_worker() returns idle workers."""

    def test_returns_first_free_worker(self) -> None:
        self.db.register_worker("w-1", "/tmp/wt-1")
        self.db.register_worker("w-2", "/tmp/wt-2")
        free = self.db.get_free_worker()
        self.assertIsNotNone(free)
        self.assertEqual(free["id"], "w-1")

    def test_returns_none_when_all_busy(self) -> None:
        self.db.register_worker("w-1", "/tmp/wt-1")
        spec = self._make_spec_file()
        entry = self.db.enqueue(spec)
        self.db.pick_next_spec()
        self.db.set_running(entry["id"], "w-1")
        self.db.assign_worker("w-1", entry["id"], pid=1234)
        free = self.db.get_free_worker()
        self.assertIsNone(free)

    def test_returns_none_when_no_workers(self) -> None:
        self.assertIsNone(self.db.get_free_worker())

    def test_skips_busy_returns_free(self) -> None:
        self.db.register_worker("w-1", "/tmp/wt-1")
        self.db.register_worker("w-2", "/tmp/wt-2")
        spec = self._make_spec_file()
        entry = self.db.enqueue(spec)
        self.db.pick_next_spec()
        self.db.set_running(entry["id"], "w-1")
        self.db.assign_worker("w-1", entry["id"], pid=1234)
        free = self.db.get_free_worker()
        self.assertIsNotNone(free)
        self.assertEqual(free["id"], "w-2")


class TestAssignWorker(CrudTestCase):
    """Verify assign_worker() marks a worker busy."""

    def test_assign_sets_fields(self) -> None:
        self.db.register_worker("w-1", "/tmp/wt-1")
        spec = self._make_spec_file()
        entry = self.db.enqueue(spec)
        sid = entry["id"]
        self.db.pick_next_spec()
        self.db.set_running(sid, "w-1")
        self.db.assign_worker("w-1", sid, pid=4567, phase="execute")
        w = self.db.get_worker("w-1")
        self.assertEqual(w["current_spec_id"], sid)
        self.assertEqual(w["current_pid"], 4567)
        self.assertEqual(w["current_phase"], "execute")
        self.assertIsNotNone(w["start_time"])

    def test_assign_records_phase(self) -> None:
        """Verify current_phase is stored for crash recovery."""
        self.db.register_worker("w-1", "/tmp/wt-1")
        spec = self._make_spec_file()
        entry = self.db.enqueue(spec)
        sid = entry["id"]
        self.db.pick_next_spec()
        self.db.set_running(sid, "w-1", phase="task-verify")
        self.db.assign_worker("w-1", sid, pid=4567, phase="task-verify")
        w = self.db.get_worker("w-1")
        self.assertEqual(w["current_phase"], "task-verify")


class TestFreeWorker(CrudTestCase):
    """Verify free_worker() releases a worker."""

    def test_free_clears_all_fields(self) -> None:
        self.db.register_worker("w-1", "/tmp/wt-1")
        spec = self._make_spec_file()
        entry = self.db.enqueue(spec)
        sid = entry["id"]
        self.db.pick_next_spec()
        self.db.set_running(sid, "w-1")
        self.db.assign_worker("w-1", sid, pid=4567)
        self.db.free_worker("w-1")
        w = self.db.get_worker("w-1")
        self.assertIsNone(w["current_spec_id"])
        self.assertIsNone(w["current_pid"])
        self.assertIsNone(w["start_time"])
        self.assertIsNone(w["current_phase"])

    def test_free_makes_worker_available(self) -> None:
        self.db.register_worker("w-1", "/tmp/wt-1")
        spec = self._make_spec_file()
        entry = self.db.enqueue(spec)
        sid = entry["id"]
        self.db.pick_next_spec()
        self.db.set_running(sid, "w-1")
        self.db.assign_worker("w-1", sid, pid=4567)
        # No free worker available
        self.assertIsNone(self.db.get_free_worker())
        # Free it
        self.db.free_worker("w-1")
        free = self.db.get_free_worker()
        self.assertIsNotNone(free)
        self.assertEqual(free["id"], "w-1")


class TestGetWorkerCurrentSpec(CrudTestCase):
    """Verify get_worker_current_spec() returns the assigned spec."""

    def test_returns_spec_when_assigned(self) -> None:
        self.db.register_worker("w-1", "/tmp/wt-1")
        spec = self._make_spec_file()
        entry = self.db.enqueue(spec)
        sid = entry["id"]
        self.db.pick_next_spec()
        self.db.set_running(sid, "w-1")
        self.db.assign_worker("w-1", sid, pid=4567)
        current = self.db.get_worker_current_spec("w-1")
        self.assertIsNotNone(current)
        self.assertEqual(current["id"], sid)

    def test_returns_none_when_idle(self) -> None:
        self.db.register_worker("w-1", "/tmp/wt-1")
        self.assertIsNone(self.db.get_worker_current_spec("w-1"))

    def test_returns_none_for_nonexistent_worker(self) -> None:
        self.assertIsNone(self.db.get_worker_current_spec("w-nope"))


# ─── Process Tracking Tests ──────────────────────────────────────────


class TestProcessTracking(CrudTestCase):
    """Verify register_process(), end_process(), get_active_processes()."""

    def test_register_process(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.register_worker("w-1", "/tmp/wt-1")
        self.db.register_process(
            pid=5555, spec_id="q-001", worker_id="w-1",
            iteration=1, phase="execute"
        )
        active = self.db.get_active_processes()
        self.assertEqual(len(active), 1)
        self.assertEqual(active[0]["pid"], 5555)
        self.assertEqual(active[0]["spec_id"], "q-001")
        self.assertEqual(active[0]["phase"], "execute")
        self.assertIsNone(active[0]["ended_at"])

    def test_end_process(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.register_worker("w-1", "/tmp/wt-1")
        self.db.register_process(
            pid=5555, spec_id="q-001", worker_id="w-1",
            iteration=1, phase="execute"
        )
        self.db.end_process(pid=5555, spec_id="q-001", exit_code=0)
        active = self.db.get_active_processes()
        self.assertEqual(len(active), 0)

        # Verify the row was updated, not deleted
        row = self.db.conn.execute(
            "SELECT * FROM processes WHERE pid = 5555 AND spec_id = 'q-001'"
        ).fetchone()
        self.assertIsNotNone(row)
        self.assertIsNotNone(row["ended_at"])
        self.assertEqual(row["exit_code"], 0)

    def test_multiple_active_processes(self) -> None:
        spec1 = self._make_spec_file()
        self.db.enqueue(spec1)
        spec2 = self._make_spec_file_named("spec2.md")
        self.db.enqueue(spec2)
        self.db.register_worker("w-1", "/tmp/wt-1")
        self.db.register_worker("w-2", "/tmp/wt-2")
        self.db.register_process(
            pid=111, spec_id="q-001", worker_id="w-1",
            iteration=1, phase="execute"
        )
        self.db.register_process(
            pid=222, spec_id="q-002", worker_id="w-2",
            iteration=1, phase="execute"
        )
        active = self.db.get_active_processes()
        self.assertEqual(len(active), 2)

        # End one
        self.db.end_process(pid=111, spec_id="q-001", exit_code=0)
        active = self.db.get_active_processes()
        self.assertEqual(len(active), 1)
        self.assertEqual(active[0]["pid"], 222)

    def test_end_process_only_ends_active_record(self) -> None:
        """If a process has multiple records, only the active one is ended."""
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.register_worker("w-1", "/tmp/wt-1")

        # Iteration 1 (already ended)
        self.db.register_process(
            pid=5555, spec_id="q-001", worker_id="w-1",
            iteration=1, phase="execute"
        )
        self.db.end_process(pid=5555, spec_id="q-001", exit_code=0)

        # Iteration 2 (still active with different PID)
        self.db.register_process(
            pid=6666, spec_id="q-001", worker_id="w-1",
            iteration=2, phase="execute"
        )
        active = self.db.get_active_processes()
        self.assertEqual(len(active), 1)
        self.assertEqual(active[0]["pid"], 6666)

    def test_events_logged_for_process_lifecycle(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.register_worker("w-1", "/tmp/wt-1")
        self.db.register_process(
            pid=5555, spec_id="q-001", worker_id="w-1",
            iteration=1, phase="execute"
        )
        self.db.end_process(pid=5555, spec_id="q-001", exit_code=0)
        events = self.db.conn.execute(
            "SELECT event_type FROM events WHERE spec_id = 'q-001' "
            "ORDER BY seq"
        ).fetchall()
        event_types = [e["event_type"] for e in events]
        self.assertIn("process_started", event_types)
        self.assertIn("process_ended", event_types)


# ─── PID Validation Tests ────────────────────────────────────────────


class TestIsPidAliveAndOurs(unittest.TestCase):
    """Verify _is_pid_alive_and_ours() detects dead and reused PIDs."""

    def test_dead_pid_returns_false(self) -> None:
        """A PID that doesn't exist should return False."""
        # Use a very high PID that almost certainly doesn't exist
        result = Database._is_pid_alive_and_ours(999999999, "2026-01-01T00:00:00+00:00")
        self.assertFalse(result)

    def test_own_pid_returns_true(self) -> None:
        """Our own PID should return True."""
        my_pid = os.getpid()
        started_at = Database._get_proc_starttime(my_pid)
        if started_at is not None:
            ts = f"2026-01-01T00:00:00+00:00|{started_at}"
        else:
            ts = "2026-01-01T00:00:00+00:00"
        result = Database._is_pid_alive_and_ours(my_pid, ts)
        self.assertTrue(result)

    def test_pid_reuse_detection(self) -> None:
        """If we store ticks and the current process has different ticks, detect reuse."""
        my_pid = os.getpid()
        # Store a fake starttime that doesn't match
        fake_started_at = "2026-01-01T00:00:00+00:00|0"
        actual_ticks = Database._get_proc_starttime(my_pid)
        if actual_ticks is not None and actual_ticks != 0:
            # On Linux, this should return False because ticks don't match
            result = Database._is_pid_alive_and_ours(my_pid, fake_started_at)
            self.assertFalse(result)
        else:
            # Non-Linux or can't read proc, skip
            self.skipTest("/proc not available or starttime is 0")

    def test_legacy_timestamp_no_ticks(self) -> None:
        """Without ticks in started_at, fall back to kill-0 (alive)."""
        my_pid = os.getpid()
        result = Database._is_pid_alive_and_ours(
            my_pid, "2026-01-01T00:00:00+00:00"
        )
        self.assertTrue(result)


class TestGetProcStarttime(unittest.TestCase):
    """Verify _get_proc_starttime() reads /proc correctly."""

    def test_own_pid(self) -> None:
        ticks = Database._get_proc_starttime(os.getpid())
        if os.path.exists(f"/proc/{os.getpid()}/stat"):
            self.assertIsNotNone(ticks)
            self.assertIsInstance(ticks, int)
            self.assertGreater(ticks, 0)
        else:
            self.assertIsNone(ticks)

    def test_dead_pid(self) -> None:
        ticks = Database._get_proc_starttime(999999999)
        self.assertIsNone(ticks)


class TestMakeStartedAt(CrudTestCase):
    """Verify make_started_at() produces the right format."""

    def test_includes_ticks_on_linux(self) -> None:
        pid = os.getpid()
        started_at = self.db.make_started_at(pid)
        if os.path.exists(f"/proc/{pid}/stat"):
            self.assertIn("|", started_at)
            iso_part, ticks_part = started_at.rsplit("|", 1)
            int(ticks_part)  # Should not raise
        else:
            self.assertNotIn("|", started_at)


# ─── Recovery Tests ──────────────────────────────────────────────────


class TestRecoverRunningSpecs(CrudTestCase):
    """Verify recover_running_specs() resets specs with dead PIDs."""

    def test_recover_spec_with_dead_pid(self) -> None:
        """Spec running with a dead PID should be requeued."""
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.register_worker("w-1", "/tmp/wt-1")
        self.db.pick_next_spec()
        self.db.set_running("q-001", "w-1")
        # Assign worker with a dead PID
        dead_pid = 999999999
        self.db.assign_worker("w-1", "q-001", pid=dead_pid)
        self.db.register_process(
            pid=dead_pid, spec_id="q-001", worker_id="w-1",
            iteration=1, phase="execute"
        )

        recovered = self.db.recover_running_specs()
        self.assertEqual(recovered, ["q-001"])
        s = self.db.get_spec("q-001")
        self.assertEqual(s["status"], "requeued")

        # Worker should be freed
        w = self.db.get_worker("w-1")
        self.assertIsNone(w["current_spec_id"])

        # Process should be ended
        active = self.db.get_active_processes()
        self.assertEqual(len(active), 0)

    def test_no_recovery_for_alive_pid(self) -> None:
        """Spec running with our own PID should NOT be recovered."""
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.register_worker("w-1", "/tmp/wt-1")
        self.db.pick_next_spec()
        self.db.set_running("q-001", "w-1")
        # Assign worker with our own PID (alive)
        my_pid = os.getpid()
        started_at = self.db.make_started_at(my_pid)
        self.db.assign_worker("w-1", "q-001", pid=my_pid)
        self.db.register_process(
            pid=my_pid, spec_id="q-001", worker_id="w-1",
            iteration=1, phase="execute"
        )
        # Manually set started_at with ticks for proper detection
        self.db.conn.execute(
            "UPDATE processes SET started_at = ? "
            "WHERE pid = ? AND spec_id = 'q-001'",
            (started_at, my_pid),
        )
        self.db.conn.commit()

        recovered = self.db.recover_running_specs()
        self.assertEqual(recovered, [])
        s = self.db.get_spec("q-001")
        self.assertEqual(s["status"], "running")

    def test_recovery_frees_worker(self) -> None:
        """Worker should be freed when its spec is recovered."""
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.register_worker("w-1", "/tmp/wt-1")
        self.db.pick_next_spec()
        self.db.set_running("q-001", "w-1")
        self.db.assign_worker("w-1", "q-001", pid=999999999)
        self.db.register_process(
            pid=999999999, spec_id="q-001", worker_id="w-1",
            iteration=1, phase="execute"
        )

        self.db.recover_running_specs()
        w = self.db.get_worker("w-1")
        self.assertIsNone(w["current_spec_id"])
        self.assertIsNone(w["current_pid"])

    def test_no_running_specs_is_noop(self) -> None:
        """No running specs should return empty list."""
        self.db.register_worker("w-1", "/tmp/wt-1")
        recovered = self.db.recover_running_specs()
        self.assertEqual(recovered, [])

    def test_recover_multiple_specs(self) -> None:
        """Multiple running specs with dead PIDs should all be recovered."""
        spec1 = self._make_spec_file()
        self.db.enqueue(spec1)
        spec2 = self._make_spec_file_named("spec2.md")
        self.db.enqueue(spec2)
        self.db.register_worker("w-1", "/tmp/wt-1")
        self.db.register_worker("w-2", "/tmp/wt-2")

        # Set up q-001 as running with dead PID
        self.db.pick_next_spec()
        self.db.set_running("q-001", "w-1")
        self.db.assign_worker("w-1", "q-001", pid=999999998)
        self.db.register_process(
            pid=999999998, spec_id="q-001", worker_id="w-1",
            iteration=1, phase="execute"
        )

        # Set up q-002 as running with dead PID
        self.db.pick_next_spec()
        self.db.set_running("q-002", "w-2")
        self.db.assign_worker("w-2", "q-002", pid=999999997)
        self.db.register_process(
            pid=999999997, spec_id="q-002", worker_id="w-2",
            iteration=1, phase="execute"
        )

        recovered = self.db.recover_running_specs()
        self.assertEqual(sorted(recovered), ["q-001", "q-002"])

    def test_pid_reuse_detected_during_recovery(self) -> None:
        """A reused PID (different starttime) should be recovered."""
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.register_worker("w-1", "/tmp/wt-1")
        self.db.pick_next_spec()
        self.db.set_running("q-001", "w-1")

        my_pid = os.getpid()
        # Store with fake ticks (0) that don't match the real process
        fake_started_at = "2026-01-01T00:00:00+00:00|0"
        self.db.assign_worker("w-1", "q-001", pid=my_pid)
        self.db.register_process(
            pid=my_pid, spec_id="q-001", worker_id="w-1",
            iteration=1, phase="execute"
        )
        # Override started_at with fake ticks
        self.db.conn.execute(
            "UPDATE processes SET started_at = ? "
            "WHERE pid = ? AND spec_id = 'q-001'",
            (fake_started_at, my_pid),
        )
        self.db.conn.commit()

        actual_ticks = Database._get_proc_starttime(my_pid)
        if actual_ticks is not None and actual_ticks != 0:
            recovered = self.db.recover_running_specs()
            self.assertEqual(recovered, ["q-001"])
            s = self.db.get_spec("q-001")
            self.assertEqual(s["status"], "requeued")
        else:
            self.skipTest("/proc not available for PID reuse test")


# ─── GetEvents Tests ──────────────────────────────────────────────────


class TestGetEvents(CrudTestCase):
    """Verify get_events() queries with optional spec_id and limit."""

    def test_get_all_events(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.cancel("q-001")
        events = self.db.get_events()
        self.assertGreater(len(events), 0)
        # Should include both queued and canceled events
        types = [e["event_type"] for e in events]
        self.assertIn("queued", types)
        self.assertIn("canceled", types)

    def test_get_events_empty_db(self) -> None:
        events = self.db.get_events()
        self.assertEqual(events, [])

    def test_get_events_filter_by_spec_id(self) -> None:
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1)
        self.db.enqueue(s2)
        events = self.db.get_events(spec_id="q-001")
        for e in events:
            self.assertEqual(e["spec_id"], "q-001")

    def test_get_events_filter_returns_empty_for_unknown_spec(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        events = self.db.get_events(spec_id="q-999")
        self.assertEqual(events, [])

    def test_get_events_with_limit(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.cancel("q-001")
        # Should have at least 2 events (queued + canceled)
        all_events = self.db.get_events()
        self.assertGreaterEqual(len(all_events), 2)
        limited = self.db.get_events(limit=1)
        self.assertEqual(len(limited), 1)
        # Should return the most recent event
        self.assertEqual(limited[0]["seq"], all_events[-1]["seq"])

    def test_get_events_with_limit_returns_chronological(self) -> None:
        """Limited results should still be in chronological order."""
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.requeue("q-001")
        self.db.cancel("q-001")
        limited = self.db.get_events(limit=2)
        self.assertEqual(len(limited), 2)
        # Chronological order: earlier seq first
        self.assertLess(limited[0]["seq"], limited[1]["seq"])

    def test_get_events_with_spec_id_and_limit(self) -> None:
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1)
        self.db.enqueue(s2)
        self.db.cancel("q-001")
        # q-001 has queued + canceled events
        events = self.db.get_events(spec_id="q-001", limit=1)
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["spec_id"], "q-001")
        # Should be the most recent event for q-001
        self.assertEqual(events[0]["event_type"], "canceled")

    def test_get_events_returns_data_field(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec, priority=42)
        events = self.db.get_events(spec_id="q-001")
        queued_event = [e for e in events if e["event_type"] == "queued"][0]
        data = json.loads(queued_event["data"])
        self.assertEqual(data["priority"], 42)

    def test_get_events_chronological_order(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.requeue("q-001")
        self.db.cancel("q-001")
        events = self.db.get_events(spec_id="q-001")
        seqs = [e["seq"] for e in events]
        self.assertEqual(seqs, sorted(seqs))


# ─── InsertIteration Tests ───────────────────────────────────────────


class TestInsertIteration(CrudTestCase):
    """Verify insert_iteration() writes to the iterations table."""

    def test_insert_iteration_basic(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.insert_iteration(
            spec_id="q-001",
            iteration=1,
            phase="execute",
            worker_id="w-1",
            started_at="2026-01-01T00:00:00+00:00",
            ended_at="2026-01-01T00:05:00+00:00",
            duration_seconds=300,
            tasks_completed=2,
            tasks_added=0,
            tasks_skipped=1,
            exit_code=0,
            pre_pending=3,
            post_pending=1,
        )
        iters = self.db.get_iterations("q-001")
        self.assertEqual(len(iters), 1)
        it = iters[0]
        self.assertEqual(it["spec_id"], "q-001")
        self.assertEqual(it["iteration"], 1)
        self.assertEqual(it["phase"], "execute")
        self.assertEqual(it["worker_id"], "w-1")
        self.assertEqual(it["duration_seconds"], 300)
        self.assertEqual(it["tasks_completed"], 2)
        self.assertEqual(it["tasks_added"], 0)
        self.assertEqual(it["tasks_skipped"], 1)
        self.assertEqual(it["exit_code"], 0)
        self.assertEqual(it["pre_pending"], 3)
        self.assertEqual(it["post_pending"], 1)

    def test_insert_multiple_iterations(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        for i in range(1, 4):
            self.db.insert_iteration(
                spec_id="q-001",
                iteration=i,
                phase="execute",
                worker_id="w-1",
                started_at=f"2026-01-01T0{i}:00:00+00:00",
                ended_at=f"2026-01-01T0{i}:05:00+00:00",
                duration_seconds=300,
                tasks_completed=1,
            )
        iters = self.db.get_iterations("q-001")
        self.assertEqual(len(iters), 3)
        self.assertEqual([it["iteration"] for it in iters], [1, 2, 3])

    def test_insert_iteration_with_quality_score(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.insert_iteration(
            spec_id="q-001",
            iteration=1,
            phase="execute",
            worker_id="w-1",
            started_at="2026-01-01T00:00:00+00:00",
            ended_at="2026-01-01T00:05:00+00:00",
            duration_seconds=300,
            quality_score=0.85,
            quality_breakdown='{"code": 0.9, "tests": 0.8}',
        )
        iters = self.db.get_iterations("q-001")
        self.assertEqual(iters[0]["quality_score"], 0.85)
        self.assertEqual(
            iters[0]["quality_breakdown"],
            '{"code": 0.9, "tests": 0.8}',
        )

    def test_insert_iteration_multiple_phases_same_iteration(self) -> None:
        """Multiple phases can share the same iteration number."""
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.insert_iteration(
            spec_id="q-001",
            iteration=1,
            phase="execute",
            worker_id="w-1",
            started_at="2026-01-01T00:00:00+00:00",
            ended_at="2026-01-01T00:05:00+00:00",
            duration_seconds=300,
        )
        self.db.insert_iteration(
            spec_id="q-001",
            iteration=1,
            phase="task-verify",
            worker_id="w-1",
            started_at="2026-01-01T00:06:00+00:00",
            ended_at="2026-01-01T00:08:00+00:00",
            duration_seconds=120,
        )
        iters = self.db.get_iterations("q-001")
        self.assertEqual(len(iters), 2)
        phases = [it["phase"] for it in iters]
        self.assertIn("execute", phases)
        self.assertIn("task-verify", phases)

    def test_insert_iteration_logs_event(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.insert_iteration(
            spec_id="q-001",
            iteration=1,
            phase="execute",
            worker_id="w-1",
            started_at="2026-01-01T00:00:00+00:00",
            ended_at="2026-01-01T00:05:00+00:00",
            duration_seconds=300,
            tasks_completed=2,
        )
        events = self.db.get_events(spec_id="q-001")
        iter_events = [
            e for e in events if e["event_type"] == "iteration_recorded"
        ]
        self.assertEqual(len(iter_events), 1)
        data = json.loads(iter_events[0]["data"])
        self.assertEqual(data["iteration"], 1)
        self.assertEqual(data["phase"], "execute")

    def test_insert_iteration_defaults(self) -> None:
        """Optional fields default to 0 or None."""
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.insert_iteration(
            spec_id="q-001",
            iteration=1,
            phase="execute",
            worker_id="w-1",
            started_at="2026-01-01T00:00:00+00:00",
            ended_at="2026-01-01T00:05:00+00:00",
            duration_seconds=300,
        )
        it = self.db.get_iterations("q-001")[0]
        self.assertEqual(it["tasks_completed"], 0)
        self.assertEqual(it["tasks_added"], 0)
        self.assertEqual(it["tasks_skipped"], 0)
        self.assertIsNone(it["exit_code"])
        self.assertIsNone(it["pre_pending"])
        self.assertIsNone(it["post_pending"])
        self.assertIsNone(it["quality_score"])
        self.assertIsNone(it["quality_breakdown"])


# ─── GetIterations Tests ─────────────────────────────────────────────


class TestGetIterations(CrudTestCase):
    """Verify get_iterations() queries for a spec."""

    def test_get_iterations_empty(self) -> None:
        iters = self.db.get_iterations("q-nonexistent")
        self.assertEqual(iters, [])

    def test_get_iterations_sorted_by_iteration(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        # Insert out of order
        for i in [3, 1, 2]:
            self.db.insert_iteration(
                spec_id="q-001",
                iteration=i,
                phase="execute",
                worker_id="w-1",
                started_at=f"2026-01-01T0{i}:00:00+00:00",
                ended_at=f"2026-01-01T0{i}:05:00+00:00",
                duration_seconds=300,
            )
        iters = self.db.get_iterations("q-001")
        self.assertEqual([it["iteration"] for it in iters], [1, 2, 3])

    def test_get_iterations_only_for_requested_spec(self) -> None:
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1)
        self.db.enqueue(s2)
        self.db.insert_iteration(
            spec_id="q-001",
            iteration=1,
            phase="execute",
            worker_id="w-1",
            started_at="2026-01-01T00:00:00+00:00",
            ended_at="2026-01-01T00:05:00+00:00",
            duration_seconds=300,
        )
        self.db.insert_iteration(
            spec_id="q-002",
            iteration=1,
            phase="execute",
            worker_id="w-2",
            started_at="2026-01-01T00:00:00+00:00",
            ended_at="2026-01-01T00:05:00+00:00",
            duration_seconds=300,
        )
        iters = self.db.get_iterations("q-001")
        self.assertEqual(len(iters), 1)
        self.assertEqual(iters[0]["spec_id"], "q-001")


# ─── MigrateFromJson Tests ───────────────────────────────────────────


class TestMigrateFromJson(CrudTestCase):
    """Verify migrate_from_json() imports q-*.json files."""

    def _write_queue_json(
        self, queue_id: str, entry: dict[str, Any]
    ) -> None:
        """Write a JSON queue entry file."""
        path = Path(self.queue_dir) / f"{queue_id}.json"
        path.write_text(
            json.dumps(entry, indent=2) + "\n", encoding="utf-8"
        )

    def _write_iteration_json(
        self, queue_id: str, iteration: int, data: dict[str, Any]
    ) -> None:
        """Write an iteration metadata JSON file."""
        path = (
            Path(self.queue_dir)
            / f"{queue_id}.iteration-{iteration}.json"
        )
        path.write_text(
            json.dumps(data, indent=2) + "\n", encoding="utf-8"
        )

    def _make_json_entry(self, queue_id: str, **overrides: Any) -> dict[str, Any]:
        """Create a standard JSON queue entry dict."""
        entry = {
            "id": queue_id,
            "spec_path": f"/tmp/queue/{queue_id}.spec.md",
            "original_spec_path": f"/tmp/specs/{queue_id}.md",
            "worktree": None,
            "priority": 100,
            "status": "queued",
            "submitted_at": "2026-01-01T00:00:00+00:00",
            "iteration": 0,
            "max_iterations": 30,
            "blocked_by": [],
            "last_worker": None,
            "last_iteration_at": None,
            "consecutive_failures": 0,
            "tasks_done": 0,
            "tasks_total": 0,
            "sync_back": True,
            "project": None,
            "initial_task_ids": [],
        }
        entry.update(overrides)
        return entry

    def test_migrate_empty_dir(self) -> None:
        count = self.db.migrate_from_json()
        self.assertEqual(count, 0)

    def test_migrate_single_entry(self) -> None:
        entry = self._make_json_entry("q-001", priority=50)
        self._write_queue_json("q-001", entry)
        count = self.db.migrate_from_json()
        self.assertEqual(count, 1)
        spec = self.db.get_spec("q-001")
        self.assertIsNotNone(spec)
        self.assertEqual(spec["priority"], 50)
        self.assertEqual(spec["status"], "queued")

    def test_migrate_multiple_entries(self) -> None:
        for i in range(1, 4):
            entry = self._make_json_entry(
                f"q-{i:03d}", priority=100 + i
            )
            self._write_queue_json(f"q-{i:03d}", entry)
        count = self.db.migrate_from_json()
        self.assertEqual(count, 3)
        queue = self.db.get_queue()
        self.assertEqual(len(queue), 3)

    def test_migrate_preserves_status(self) -> None:
        entry = self._make_json_entry("q-001", status="completed")
        self._write_queue_json("q-001", entry)
        self.db.migrate_from_json()
        spec = self.db.get_spec("q-001")
        self.assertEqual(spec["status"], "completed")

    def test_migrate_preserves_iteration_count(self) -> None:
        entry = self._make_json_entry("q-001", iteration=5)
        self._write_queue_json("q-001", entry)
        self.db.migrate_from_json()
        spec = self.db.get_spec("q-001")
        self.assertEqual(spec["iteration"], 5)

    def test_migrate_preserves_sync_back(self) -> None:
        entry = self._make_json_entry("q-001", sync_back=False)
        self._write_queue_json("q-001", entry)
        self.db.migrate_from_json()
        spec = self.db.get_spec("q-001")
        self.assertEqual(spec["sync_back"], 0)

    def test_migrate_preserves_sync_back_true(self) -> None:
        entry = self._make_json_entry("q-001", sync_back=True)
        self._write_queue_json("q-001", entry)
        self.db.migrate_from_json()
        spec = self.db.get_spec("q-001")
        self.assertEqual(spec["sync_back"], 1)

    def test_migrate_skips_existing_ids(self) -> None:
        """If a spec ID already exists in SQLite, skip it."""
        spec = self._make_spec_file()
        self.db.enqueue(spec)  # Creates q-001 in SQLite
        entry = self._make_json_entry("q-001", priority=999)
        self._write_queue_json("q-001", entry)
        count = self.db.migrate_from_json()
        self.assertEqual(count, 0)
        # Original priority should be preserved
        spec_row = self.db.get_spec("q-001")
        self.assertNotEqual(spec_row["priority"], 999)

    def test_migrate_skips_telemetry_files(self) -> None:
        """Should not try to import .telemetry.json files."""
        entry = self._make_json_entry("q-001")
        self._write_queue_json("q-001", entry)
        # Write a telemetry file that should be ignored
        telem = Path(self.queue_dir) / "q-001.telemetry.json"
        telem.write_text('{"bogus": true}', encoding="utf-8")
        count = self.db.migrate_from_json()
        self.assertEqual(count, 1)

    def test_migrate_skips_iteration_files(self) -> None:
        """Should not try to import .iteration-N.json as queue entries."""
        entry = self._make_json_entry("q-001")
        self._write_queue_json("q-001", entry)
        # Write an iteration file
        iter_file = Path(self.queue_dir) / "q-001.iteration-1.json"
        iter_file.write_text('{"iteration": 1}', encoding="utf-8")
        count = self.db.migrate_from_json()
        # Should import only the q-001.json, not the iteration file
        self.assertEqual(count, 1)

    def test_migrate_imports_iteration_files(self) -> None:
        """Iteration files should be imported into the iterations table."""
        entry = self._make_json_entry("q-001", iteration=2)
        self._write_queue_json("q-001", entry)
        self._write_iteration_json(
            "q-001",
            1,
            {
                "queue_id": "q-001",
                "iteration": 1,
                "exit_code": 0,
                "duration_seconds": 120,
                "started_at": "2026-01-01T00:00:00Z",
                "tasks_completed": 1,
                "tasks_added": 0,
                "tasks_skipped": 0,
                "pre_counts": {"pending": 3, "done": 0, "skipped": 0, "total": 3},
                "post_counts": {"pending": 2, "done": 1, "skipped": 0, "total": 3},
            },
        )
        self._write_iteration_json(
            "q-001",
            2,
            {
                "queue_id": "q-001",
                "iteration": 2,
                "exit_code": 0,
                "duration_seconds": 90,
                "started_at": "2026-01-01T00:05:00Z",
                "tasks_completed": 2,
                "tasks_added": 0,
                "tasks_skipped": 0,
                "pre_counts": {"pending": 2, "done": 1, "skipped": 0, "total": 3},
                "post_counts": {"pending": 0, "done": 3, "skipped": 0, "total": 3},
            },
        )
        self.db.migrate_from_json()
        iters = self.db.get_iterations("q-001")
        self.assertEqual(len(iters), 2)
        self.assertEqual(iters[0]["iteration"], 1)
        self.assertEqual(iters[0]["tasks_completed"], 1)
        self.assertEqual(iters[0]["pre_pending"], 3)
        self.assertEqual(iters[0]["post_pending"], 2)
        self.assertEqual(iters[1]["iteration"], 2)
        self.assertEqual(iters[1]["tasks_completed"], 2)

    def test_migrate_handles_blocked_by(self) -> None:
        """Dependencies (blocked_by) should create spec_dependencies rows."""
        e1 = self._make_json_entry("q-001")
        e2 = self._make_json_entry("q-002", blocked_by=["q-001"])
        self._write_queue_json("q-001", e1)
        self._write_queue_json("q-002", e2)
        self.db.migrate_from_json()
        deps = self.db.conn.execute(
            "SELECT * FROM spec_dependencies WHERE spec_id = 'q-002'"
        ).fetchall()
        self.assertEqual(len(deps), 1)
        self.assertEqual(deps[0]["blocks_on"], "q-001")

    def test_migrate_handles_malformed_json(self) -> None:
        """Malformed JSON files should be skipped."""
        (Path(self.queue_dir) / "q-001.json").write_text(
            "not valid json{{{", encoding="utf-8"
        )
        entry = self._make_json_entry("q-002")
        self._write_queue_json("q-002", entry)
        count = self.db.migrate_from_json()
        self.assertEqual(count, 1)
        self.assertIsNone(self.db.get_spec("q-001"))
        self.assertIsNotNone(self.db.get_spec("q-002"))

    def test_migrate_logs_event(self) -> None:
        entry = self._make_json_entry("q-001")
        self._write_queue_json("q-001", entry)
        self.db.migrate_from_json()
        events = self.db.get_events()
        migrated_events = [
            e for e in events if e["event_type"] == "migrated"
        ]
        self.assertEqual(len(migrated_events), 1)
        data = json.loads(migrated_events[0]["data"])
        self.assertEqual(data["count"], 1)

    def test_migrate_custom_queue_dir(self) -> None:
        """migrate_from_json() can read from a custom directory."""
        alt_dir = os.path.join(self._tmpdir.name, "alt-queue")
        os.makedirs(alt_dir)
        entry = self._make_json_entry("q-001")
        (Path(alt_dir) / "q-001.json").write_text(
            json.dumps(entry) + "\n", encoding="utf-8"
        )
        count = self.db.migrate_from_json(queue_dir=alt_dir)
        self.assertEqual(count, 1)
        self.assertIsNotNone(self.db.get_spec("q-001"))

    def test_migrate_nonexistent_dir_returns_zero(self) -> None:
        count = self.db.migrate_from_json(
            queue_dir="/tmp/nonexistent-boi-dir-xyz"
        )
        self.assertEqual(count, 0)

    def test_migrate_preserves_project(self) -> None:
        entry = self._make_json_entry(
            "q-001", project="my-project"
        )
        self._write_queue_json("q-001", entry)
        self.db.migrate_from_json()
        spec = self.db.get_spec("q-001")
        self.assertEqual(spec["project"], "my-project")

    def test_migrate_preserves_initial_task_ids(self) -> None:
        entry = self._make_json_entry(
            "q-001", initial_task_ids=["1", "2", "3"]
        )
        self._write_queue_json("q-001", entry)
        self.db.migrate_from_json()
        spec = self.db.get_spec("q-001")
        ids = json.loads(spec["initial_task_ids"])
        self.assertEqual(ids, ["1", "2", "3"])


# ─── Worker Load Balancing Tests ───────────────────────────────────────


class TestWorkerLoadBalancing(CrudTestCase):
    """Verify workers are selected with round-robin fairness."""

    def setUp(self) -> None:
        super().setUp()
        self.db.register_worker("w-1", "/tmp/wt-1")
        self.db.register_worker("w-2", "/tmp/wt-2")
        self.db.register_worker("w-3", "/tmp/wt-3")
        # Create a spec so foreign key constraints are satisfied
        spec = self._make_spec_file()
        entry = self.db.enqueue(spec)
        self.spec_id = entry["id"]
        self.db.pick_next_spec()
        self.db.set_running(self.spec_id, "w-1")

    def test_free_worker_rotates_not_always_w1(self) -> None:
        """get_free_worker should not always return the same worker."""
        # Assign w-1, then free it. Next pick should NOT be w-1.
        self.db.assign_worker("w-1", self.spec_id, pid=100)
        self.db.free_worker("w-1")

        worker = self.db.get_free_worker()
        self.assertIsNotNone(worker)
        self.assertNotEqual(
            worker["id"], "w-1",
            "get_free_worker always returns w-1. Should rotate.",
        )

    def test_round_robin_cycles_through_all_workers(self) -> None:
        """Workers should be selected in round-robin order."""
        picked = []
        for i in range(3):
            w = self.db.get_free_worker()
            self.assertIsNotNone(w)
            picked.append(w["id"])
            self.db.assign_worker(w["id"], self.spec_id, pid=100 + i)
            self.db.free_worker(w["id"])

        # All three workers should have been picked
        self.assertEqual(len(set(picked)), 3, f"Expected 3 unique workers, got {picked}")

    def test_wraps_around_after_last_worker(self) -> None:
        """After assigning the last worker, should wrap back to first."""
        # Assign w-3, free it
        self.db.assign_worker("w-3", self.spec_id, pid=100)
        self.db.free_worker("w-3")

        # Next should wrap around and NOT be w-3
        worker = self.db.get_free_worker()
        self.assertIsNotNone(worker)
        self.assertEqual(worker["id"], "w-1", "Should wrap around to w-1 after w-3")

    def test_all_workers_busy_returns_none(self) -> None:
        """When all workers are busy, get_free_worker returns None."""
        # Need more specs for each worker (foreign key constraint)
        spec2 = self._make_spec_file_named("spec2.md")
        entry2 = self.db.enqueue(spec2)
        self.db.pick_next_spec()
        self.db.set_running(entry2["id"], "w-2")

        spec3 = self._make_spec_file_named("spec3.md")
        entry3 = self.db.enqueue(spec3)
        self.db.pick_next_spec()
        self.db.set_running(entry3["id"], "w-3")

        self.db.assign_worker("w-1", self.spec_id, pid=100)
        self.db.assign_worker("w-2", entry2["id"], pid=101)
        self.db.assign_worker("w-3", entry3["id"], pid=102)

        self.assertIsNone(self.db.get_free_worker())


if __name__ == "__main__":
    unittest.main()
