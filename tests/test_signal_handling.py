# test_signal_handling.py — TDD RED phase tests for signal-aware failure handling.
#
# Tests that SIGTERM (exit 143) and SIGKILL (exit 137) are NOT counted
# as consecutive failures. Only real failures (exit 1-127) should count.
# All tests should FAIL until the signal-aware logic is implemented
# in lib/daemon_ops.py.
#
# Uses stdlib unittest only (no pytest dependency).

import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.db import Database


class SignalHandlingTestCase(unittest.TestCase):
    """Base test case with DB and helpers for signal handling tests."""

    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.state_dir = self._tmpdir.name
        self.db_path = os.path.join(self.state_dir, "boi.db")
        self.queue_dir = os.path.join(self.state_dir, "queue")
        self.logs_dir = os.path.join(self.state_dir, "logs")
        self.events_dir = os.path.join(self.state_dir, "events")
        self.hooks_dir = os.path.join(self.state_dir, "hooks")
        for d in (self.queue_dir, self.logs_dir, self.events_dir, self.hooks_dir):
            os.makedirs(d, exist_ok=True)
        self.db = Database(self.db_path, self.queue_dir)
        from lib.daemon_ops import CompletionContext
        self.ctx = CompletionContext(
            queue_dir=self.queue_dir,
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            log_dir=self.logs_dir,
            script_dir=self.state_dir,
            db=self.db,
        )

    def tearDown(self) -> None:
        self.db.close()
        self._tmpdir.cleanup()

    def _create_running_spec(
        self,
        spec_id: str = "q-001",
        iteration: int = 1,
        consecutive_failures: int = 0,
    ) -> str:
        """Insert a running spec into the DB."""
        spec_path = os.path.join(self.queue_dir, f"{spec_id}.spec.md")
        Path(spec_path).write_text(
            "# Test\n\n## Tasks\n\n### t-1: Task\nPENDING\n\n"
            "**Spec:** Do it.\n\n**Verify:** true\n"
        )
        self.db.conn.execute(
            "INSERT INTO specs (id, spec_path, status, iteration, "
            "consecutive_failures, submitted_at, priority, max_iterations, "
            "tasks_done, tasks_total) "
            "VALUES (?, ?, 'running', ?, ?, datetime('now'), 100, 30, 0, 1)",
            (spec_id, spec_path, iteration, consecutive_failures),
        )
        self.db.conn.commit()
        return spec_path


class TestSignalExitCodes(SignalHandlingTestCase):
    """Test that signal deaths (exit 128+) don't count as real failures."""

    def test_exit_143_sigterm_does_not_increment_failures(self) -> None:
        """SIGTERM (143) should requeue without incrementing failures."""
        from lib.daemon_ops import process_worker_completion

        self._create_running_spec(consecutive_failures=0)
        result = process_worker_completion(
            ctx=self.ctx,
            queue_id="q-001",
            exit_code="143",
        )
        row = self.db.conn.execute(
            "SELECT consecutive_failures, status FROM specs WHERE id = 'q-001'"
        ).fetchone()
        self.assertEqual(
            row["consecutive_failures"],
            0,
            "SIGTERM should not increment consecutive_failures",
        )
        self.assertIn(
            row["status"], ("queued", "requeued"), "SIGTERM should requeue the spec"
        )

    def test_exit_137_sigkill_does_not_increment_failures(self) -> None:
        """SIGKILL (137) should requeue without incrementing failures."""
        from lib.daemon_ops import process_worker_completion

        self._create_running_spec(consecutive_failures=0)
        result = process_worker_completion(
            ctx=self.ctx,
            queue_id="q-001",
            exit_code="137",
        )
        row = self.db.conn.execute(
            "SELECT consecutive_failures, status FROM specs WHERE id = 'q-001'"
        ).fetchone()
        self.assertEqual(
            row["consecutive_failures"],
            0,
            "SIGKILL should not increment consecutive_failures",
        )
        self.assertIn(
            row["status"], ("queued", "requeued"), "SIGKILL should requeue the spec"
        )

    def test_exit_1_real_failure_increments_failures(self) -> None:
        """Exit code 1 is a real failure and SHOULD increment failures."""
        from lib.daemon_ops import process_worker_completion

        self._create_running_spec(consecutive_failures=0)
        result = process_worker_completion(
            ctx=self.ctx,
            queue_id="q-001",
            exit_code="1",
        )
        row = self.db.conn.execute(
            "SELECT consecutive_failures FROM specs WHERE id = 'q-001'"
        ).fetchone()
        self.assertGreater(
            row["consecutive_failures"],
            0,
            "Real failure should increment consecutive_failures",
        )

    def test_exit_0_success_resets_failures(self) -> None:
        """Exit code 0 (success) should reset consecutive_failures to 0."""
        from lib.daemon_ops import process_worker_completion

        self._create_running_spec(consecutive_failures=3)
        result = process_worker_completion(
            ctx=self.ctx,
            queue_id="q-001",
            exit_code="0",
        )
        row = self.db.conn.execute(
            "SELECT consecutive_failures FROM specs WHERE id = 'q-001'"
        ).fetchone()
        self.assertEqual(
            row["consecutive_failures"], 0, "Success should reset consecutive_failures"
        )

    def test_signal_death_requeues_spec(self) -> None:
        """Signal death should set status to requeued, not failed."""
        from lib.daemon_ops import process_worker_completion

        self._create_running_spec(consecutive_failures=4)
        result = process_worker_completion(
            ctx=self.ctx,
            queue_id="q-001",
            exit_code="143",
        )
        row = self.db.conn.execute(
            "SELECT status FROM specs WHERE id = 'q-001'"
        ).fetchone()
        self.assertNotEqual(
            row["status"], "failed", "Signal death should never permanently fail a spec"
        )

    def test_5_signal_deaths_do_not_fail_spec(self) -> None:
        """5 SIGTERM kills should NOT fail the spec (they aren't real failures)."""
        from lib.daemon_ops import process_worker_completion

        # Simulate 5 consecutive SIGTERM kills
        self._create_running_spec(consecutive_failures=4)
        result = process_worker_completion(
            ctx=self.ctx,
            queue_id="q-001",
            exit_code="143",
        )
        row = self.db.conn.execute(
            "SELECT status, consecutive_failures FROM specs WHERE id = 'q-001'"
        ).fetchone()
        self.assertNotEqual(
            row["status"], "failed", "5 signal deaths should not fail the spec"
        )
        self.assertLess(
            row["consecutive_failures"],
            5,
            "Signal deaths should not count toward failure threshold",
        )


if __name__ == "__main__":
    unittest.main()
