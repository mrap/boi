# test_queue_compat.py — Characterization tests for lib/queue_compat.py
#
# FINDING: queue_compat.py is confirmed dead code.
#
# Investigation (2026-03-16):
#   grep -r "from lib.queue_compat\|import queue_compat" src/ → 0 matches
#   grep -r "queue_compat" *.py lib/*.py → 3 matches, all comments
#
# No Python file imports from lib.queue_compat. All production code
# imports directly from lib.queue or lib.db. The compat layer was
# created for an SQLite migration that completed without routing
# traffic through it.
#
# These tests lock in the compat layer's routing behavior so that
# it can be safely removed in the planned cleanup task (t-5c).
# They verify: _use_sqlite routing, get_queue, enqueue, and
# get_entry work correctly on both the SQLite and JSON paths.

import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import lib.queue_compat as qc


def _make_spec(tmpdir: str) -> str:
    """Create a minimal spec file in tmpdir and return its path."""
    spec = os.path.join(tmpdir, "test.spec.md")
    with open(spec, "w") as f:
        f.write("# Test spec\n")
    return spec


# ── Dead Code Documentation ────────────────────────────────────────────────

class TestDeadCodeStatus(unittest.TestCase):
    """Document that queue_compat.py has no callers in production code."""

    def test_no_production_callers(self) -> None:
        """queue_compat is not imported by any production module."""
        src = Path(__file__).resolve().parent.parent
        production_files = [
            src / "daemon.py",
            src / "worker.py",
            src / "lib" / "daemon_ops.py",
            src / "lib" / "cli_ops.py",
            src / "lib" / "conflict_detector.py",
            src / "lib" / "critic.py",
            src / "lib" / "review.py",
            src / "lib" / "status.py",
        ]
        for filepath in production_files:
            if not filepath.exists():
                continue
            content = filepath.read_text()
            self.assertNotIn(
                "queue_compat",
                content,
                f"{filepath.name} unexpectedly imports queue_compat",
            )


# ── Routing Logic Tests ────────────────────────────────────────────────────

class TestUseSqliteRouting(unittest.TestCase):
    """_use_sqlite() should return True iff boi.db exists in parent of queue_dir."""

    def test_returns_false_when_no_db_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            queue_dir = os.path.join(tmpdir, "queue")
            os.makedirs(queue_dir)
            self.assertFalse(qc._use_sqlite(queue_dir))

    def test_returns_true_when_db_file_exists(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            queue_dir = os.path.join(tmpdir, "queue")
            os.makedirs(queue_dir)
            db_path = os.path.join(tmpdir, "boi.db")
            open(db_path, "w").close()
            self.assertTrue(qc._use_sqlite(queue_dir))

    def test_db_must_be_in_parent_not_queue_dir(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            queue_dir = os.path.join(tmpdir, "queue")
            os.makedirs(queue_dir)
            # boi.db inside queue_dir doesn't count
            wrong_path = os.path.join(queue_dir, "boi.db")
            open(wrong_path, "w").close()
            self.assertFalse(qc._use_sqlite(queue_dir))


# ── SQLite Path Tests ──────────────────────────────────────────────────────

class TestSqlitePath(unittest.TestCase):
    """Verify compat functions route to SQLite when boi.db is present."""

    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.tmpdir = self._tmpdir.name
        self.queue_dir = os.path.join(self.tmpdir, "queue")
        os.makedirs(self.queue_dir)
        # Create boi.db to activate SQLite routing
        self.db_path = os.path.join(self.tmpdir, "boi.db")
        # Initialise the database so the schema exists
        from lib.db import Database
        db = Database(self.db_path, self.queue_dir)
        db.close()

    def tearDown(self) -> None:
        self._tmpdir.cleanup()

    def test_get_queue_empty_returns_list(self) -> None:
        result = qc.get_queue(self.queue_dir)
        self.assertIsInstance(result, list)
        self.assertEqual(len(result), 0)

    def test_enqueue_and_get_queue(self) -> None:
        spec = _make_spec(self.tmpdir)
        entry = qc.enqueue(self.queue_dir, spec_path=spec)
        self.assertIn("id", entry)
        queue = qc.get_queue(self.queue_dir)
        self.assertEqual(len(queue), 1)
        self.assertEqual(queue[0]["id"], entry["id"])

    def test_get_entry_returns_entry(self) -> None:
        spec = _make_spec(self.tmpdir)
        entry = qc.enqueue(self.queue_dir, spec_path=spec)
        fetched = qc.get_entry(self.queue_dir, entry["id"])
        self.assertIsNotNone(fetched)
        self.assertEqual(fetched["id"], entry["id"])

    def test_get_entry_missing_returns_none(self) -> None:
        result = qc.get_entry(self.queue_dir, "q-nonexistent")
        self.assertIsNone(result)

    def test_set_running_updates_status(self) -> None:
        spec = _make_spec(self.tmpdir)
        entry = qc.enqueue(self.queue_dir, spec_path=spec)
        qc.set_running(self.queue_dir, entry["id"], "w-01")
        fetched = qc.get_entry(self.queue_dir, entry["id"])
        self.assertEqual(fetched["status"], "running")

    def test_complete_updates_status(self) -> None:
        spec = _make_spec(self.tmpdir)
        entry = qc.enqueue(self.queue_dir, spec_path=spec)
        qc.complete(self.queue_dir, entry["id"], tasks_done=1, tasks_total=1)
        fetched = qc.get_entry(self.queue_dir, entry["id"])
        self.assertEqual(fetched["status"], "completed")

    def test_fail_updates_status(self) -> None:
        spec = _make_spec(self.tmpdir)
        entry = qc.enqueue(self.queue_dir, spec_path=spec)
        qc.fail(self.queue_dir, entry["id"], reason="test failure")
        fetched = qc.get_entry(self.queue_dir, entry["id"])
        self.assertEqual(fetched["status"], "failed")

    def test_cancel_updates_status(self) -> None:
        spec = _make_spec(self.tmpdir)
        entry = qc.enqueue(self.queue_dir, spec_path=spec)
        qc.cancel(self.queue_dir, entry["id"])
        fetched = qc.get_entry(self.queue_dir, entry["id"])
        self.assertEqual(fetched["status"], "canceled")

    def test_requeue_updates_status(self) -> None:
        spec = _make_spec(self.tmpdir)
        entry = qc.enqueue(self.queue_dir, spec_path=spec)
        qc.set_running(self.queue_dir, entry["id"], "w-01")
        qc.requeue(self.queue_dir, entry["id"], tasks_done=1, tasks_total=2)
        fetched = qc.get_entry(self.queue_dir, entry["id"])
        self.assertEqual(fetched["status"], "requeued")

    def test_recover_running_specs_returns_int(self) -> None:
        result = qc.recover_running_specs(self.queue_dir)
        self.assertIsInstance(result, int)


# ── JSON Fallback Path Tests ───────────────────────────────────────────────

class TestJsonPath(unittest.TestCase):
    """Verify compat functions route to JSON when no boi.db is present."""

    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.tmpdir = self._tmpdir.name
        self.queue_dir = os.path.join(self.tmpdir, "queue")
        os.makedirs(self.queue_dir)
        # No boi.db → JSON path

    def tearDown(self) -> None:
        self._tmpdir.cleanup()

    def test_use_sqlite_is_false(self) -> None:
        self.assertFalse(qc._use_sqlite(self.queue_dir))

    def test_get_queue_empty_returns_list(self) -> None:
        result = qc.get_queue(self.queue_dir)
        self.assertIsInstance(result, list)

    def test_enqueue_and_get_queue(self) -> None:
        spec = _make_spec(self.tmpdir)
        entry = qc.enqueue(self.queue_dir, spec_path=spec)
        self.assertIn("id", entry)
        queue = qc.get_queue(self.queue_dir)
        self.assertEqual(len(queue), 1)

    def test_get_entry_returns_entry(self) -> None:
        spec = _make_spec(self.tmpdir)
        entry = qc.enqueue(self.queue_dir, spec_path=spec)
        fetched = qc.get_entry(self.queue_dir, entry["id"])
        self.assertIsNotNone(fetched)


# ── Shared Interface Tests ─────────────────────────────────────────────────

class TestSharedInterface(unittest.TestCase):
    """Functions that don't depend on SQLite/JSON routing."""

    def test_get_experiment_budget_execute(self) -> None:
        budget = qc.get_experiment_budget("execute")
        self.assertIsInstance(budget, int)
        self.assertEqual(budget, 0)

    def test_get_experiment_budget_discover(self) -> None:
        budget = qc.get_experiment_budget("discover")
        self.assertIsInstance(budget, int)
        self.assertGreater(budget, 0)

    def test_duplicate_spec_error_importable(self) -> None:
        from lib.queue_compat import DuplicateSpecError
        self.assertTrue(issubclass(DuplicateSpecError, Exception))

    def test_get_duplicate_spec_error_returns_class(self) -> None:
        cls = qc._get_duplicate_spec_error()
        self.assertTrue(issubclass(cls, Exception))

    def test_is_pid_alive_current_process(self) -> None:
        import os as _os
        result = qc._is_pid_alive(_os.getpid())
        self.assertTrue(result)

    def test_is_pid_alive_nonexistent_pid(self) -> None:
        # PID 0 is not a real user process
        result = qc._is_pid_alive(99999999)
        self.assertFalse(result)


if __name__ == "__main__":
    unittest.main()
