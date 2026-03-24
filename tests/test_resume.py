# test_resume.py — TDD RED phase tests for `boi resume` command.
#
# Tests the resume_spec() function that resets failed/canceled specs
# back to queued while preserving progress. All tests should FAIL
# until resume_spec() is implemented in lib/cli_ops.py.
#
# Uses stdlib unittest only (no pytest dependency).

import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.db import Database


class ResumeTestCase(unittest.TestCase):
    """Base test case with a Database and helper to create specs."""

    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.db_path = os.path.join(self._tmpdir.name, "boi.db")
        self.queue_dir = os.path.join(self._tmpdir.name, "queue")
        self.db = Database(self.db_path, self.queue_dir)

    def tearDown(self) -> None:
        self.db.close()
        self._tmpdir.cleanup()

    def _create_spec(
        self,
        spec_id: str = "q-001",
        status: str = "failed",
        iteration: int = 3,
        tasks_done: int = 2,
        tasks_total: int = 5,
        consecutive_failures: int = 5,
        failure_reason: str = "Worker crashed",
    ) -> str:
        """Insert a spec directly into the DB for testing."""
        spec_path = os.path.join(self.queue_dir, f"{spec_id}.spec.md")
        Path(spec_path).parent.mkdir(parents=True, exist_ok=True)
        Path(spec_path).write_text("# Test Spec\n\n## Tasks\n\n### t-1: Task\nDONE\n")
        self.db.conn.execute(
            "INSERT INTO specs (id, spec_path, status, iteration, "
            "tasks_done, tasks_total, consecutive_failures, failure_reason, "
            "submitted_at, priority, max_iterations) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, datetime('now'), 100, 30)",
            (
                spec_id,
                spec_path,
                status,
                iteration,
                tasks_done,
                tasks_total,
                consecutive_failures,
                failure_reason,
            ),
        )
        self.db.conn.commit()
        return spec_path


class TestResumeFailedSpec(ResumeTestCase):
    """Test that resume_spec resets a failed spec to queued."""

    def test_resume_failed_spec_resets_to_queued(self) -> None:
        from lib.cli_ops import resume_spec

        self._create_spec(status="failed")
        resume_spec(self.queue_dir, "q-001")
        row = self.db.conn.execute(
            "SELECT status FROM specs WHERE id = 'q-001'"
        ).fetchone()
        self.assertEqual(row["status"], "queued")

    def test_resume_preserves_tasks_done(self) -> None:
        from lib.cli_ops import resume_spec

        self._create_spec(status="failed", tasks_done=3, tasks_total=5)
        resume_spec(self.queue_dir, "q-001")
        row = self.db.conn.execute(
            "SELECT tasks_done, tasks_total FROM specs WHERE id = 'q-001'"
        ).fetchone()
        self.assertEqual(row["tasks_done"], 3)
        self.assertEqual(row["tasks_total"], 5)

    def test_resume_preserves_iteration_count(self) -> None:
        from lib.cli_ops import resume_spec

        self._create_spec(status="failed", iteration=7)
        resume_spec(self.queue_dir, "q-001")
        row = self.db.conn.execute(
            "SELECT iteration FROM specs WHERE id = 'q-001'"
        ).fetchone()
        self.assertEqual(row["iteration"], 7)

    def test_resume_resets_consecutive_failures(self) -> None:
        from lib.cli_ops import resume_spec

        self._create_spec(status="failed", consecutive_failures=5)
        resume_spec(self.queue_dir, "q-001")
        row = self.db.conn.execute(
            "SELECT consecutive_failures FROM specs WHERE id = 'q-001'"
        ).fetchone()
        self.assertEqual(row["consecutive_failures"], 0)

    def test_resume_clears_failure_reason(self) -> None:
        from lib.cli_ops import resume_spec

        self._create_spec(status="failed", failure_reason="Worker crashed")
        resume_spec(self.queue_dir, "q-001")
        row = self.db.conn.execute(
            "SELECT failure_reason FROM specs WHERE id = 'q-001'"
        ).fetchone()
        self.assertIsNone(row["failure_reason"])


class TestResumeEdgeCases(ResumeTestCase):
    """Test error handling for resume_spec."""

    def test_resume_nonexistent_spec_errors(self) -> None:
        from lib.cli_ops import resume_spec

        with self.assertRaises(ValueError):
            resume_spec(self.queue_dir, "q-999")

    def test_resume_already_running_spec_errors(self) -> None:
        from lib.cli_ops import resume_spec

        self._create_spec(spec_id="q-002", status="running")
        with self.assertRaises(ValueError):
            resume_spec(self.queue_dir, "q-002")

    def test_resume_completed_spec_errors(self) -> None:
        from lib.cli_ops import resume_spec

        self._create_spec(spec_id="q-003", status="completed")
        with self.assertRaises(ValueError):
            resume_spec(self.queue_dir, "q-003")


class TestResumeAll(ResumeTestCase):
    """Test resume_all_failed that resumes all failed specs."""

    def test_resume_all_resumes_all_failed(self) -> None:
        from lib.cli_ops import resume_spec

        self._create_spec(spec_id="q-001", status="failed")
        self._create_spec(spec_id="q-002", status="failed")
        self._create_spec(spec_id="q-003", status="running")
        self._create_spec(spec_id="q-004", status="completed")

        # resume_spec with queue_id="--all" or a separate function
        resume_spec(self.queue_dir, "--all")

        for qid in ("q-001", "q-002"):
            row = self.db.conn.execute(
                "SELECT status FROM specs WHERE id = ?", (qid,)
            ).fetchone()
            self.assertEqual(row["status"], "queued", f"{qid} should be queued")

        # running and completed should be untouched
        for qid in ("q-003", "q-004"):
            row = self.db.conn.execute(
                "SELECT status FROM specs WHERE id = ?", (qid,)
            ).fetchone()
            self.assertNotEqual(row["status"], "queued", f"{qid} should not be queued")


if __name__ == "__main__":
    unittest.main()
