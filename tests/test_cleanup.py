# test_cleanup.py — TDD RED phase tests for orphaned process cleanup.
#
# Tests that `boi stop` kills all tracked PIDs and that `boi cleanup`
# finds and kills orphaned processes. All tests should FAIL until
# cleanup logic is implemented.
#
# Uses stdlib unittest only (no pytest dependency).

import os
import signal
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.db import Database


class CleanupTestCase(unittest.TestCase):
    """Base test case with DB and helpers for cleanup tests."""

    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.state_dir = self._tmpdir.name
        self.db_path = os.path.join(self.state_dir, "boi.db")
        self.queue_dir = os.path.join(self.state_dir, "queue")
        for d in (self.queue_dir,):
            os.makedirs(d, exist_ok=True)
        self.db = Database(self.db_path, self.queue_dir)

    def tearDown(self) -> None:
        self.db.close()
        self._tmpdir.cleanup()

    def _register_worker_with_pid(
        self,
        worker_id: str = "w-1",
        spec_id: str = "q-001",
        pid: int = 99999,
    ) -> None:
        """Register a worker with a tracked PID in the DB."""
        # Spec first (FK target)
        self.db.conn.execute(
            "INSERT OR IGNORE INTO specs (id, spec_path, status, submitted_at, priority, max_iterations) "
            "VALUES (?, '/tmp/spec.md', 'running', datetime('now'), 100, 30)",
            (spec_id,),
        )
        self.db.conn.execute(
            "INSERT OR REPLACE INTO workers (id, worktree_path, current_spec_id, current_pid) "
            "VALUES (?, '/tmp/worktree', ?, ?)",
            (worker_id, spec_id, pid),
        )
        self.db.conn.commit()


class TestStopKillsTrackedPids(CleanupTestCase):
    """Test that boi stop kills all PIDs tracked in the workers table."""

    def test_stop_kills_all_tracked_pids(self) -> None:
        """stop_all_workers should send SIGTERM to every tracked worker PID."""
        from lib.cli_ops import stop_all_workers

        self._register_worker_with_pid("w-1", "q-001", pid=11111)
        self._register_worker_with_pid("w-2", "q-002", pid=22222)

        with patch("os.kill") as mock_kill:
            stop_all_workers(self.queue_dir)
            killed_pids = {call.args[0] for call in mock_kill.call_args_list}
            self.assertIn(11111, killed_pids)
            self.assertIn(22222, killed_pids)

    def test_stop_force_sends_sigkill(self) -> None:
        """stop_all_workers(force=True) should send SIGKILL."""
        from lib.cli_ops import stop_all_workers

        self._register_worker_with_pid("w-1", "q-001", pid=11111)

        with patch("os.kill") as mock_kill:
            stop_all_workers(self.queue_dir, force=True)
            signals_sent = {call.args[1] for call in mock_kill.call_args_list}
            self.assertIn(signal.SIGKILL, signals_sent)


class TestCleanupOrphanedProcesses(CleanupTestCase):
    """Test that cleanup finds and kills orphaned worker processes."""

    def test_cleanup_finds_orphaned_processes(self) -> None:
        """cleanup_orphans should find processes not tracked in DB."""
        from lib.cli_ops import cleanup_orphans

        # Mock ps output: two claude processes, only one tracked
        self._register_worker_with_pid("w-1", "q-001", pid=11111)
        mock_ps_output = "11111 claude -p BOI Worker\n22222 claude -p BOI Worker\n"

        with patch("subprocess.check_output", return_value=mock_ps_output):
            with patch("os.kill") as mock_kill:
                orphans = cleanup_orphans(self.queue_dir)
                # Should have found PID 22222 as orphaned
                self.assertIn(22222, orphans)
                self.assertNotIn(11111, orphans)

    def test_cleanup_kills_only_untracked_processes(self) -> None:
        """cleanup_orphans should only kill PIDs not in the workers table."""
        from lib.cli_ops import cleanup_orphans

        self._register_worker_with_pid("w-1", "q-001", pid=11111)
        mock_ps_output = "11111 claude -p BOI Worker\n22222 claude -p BOI Worker\n"

        with patch("subprocess.check_output", return_value=mock_ps_output):
            with patch("os.kill") as mock_kill:
                cleanup_orphans(self.queue_dir)
                killed_pids = {call.args[0] for call in mock_kill.call_args_list}
                self.assertNotIn(11111, killed_pids, "Should not kill tracked workers")
                self.assertIn(22222, killed_pids, "Should kill orphaned processes")

    def test_cleanup_preserves_active_workers(self) -> None:
        """cleanup_orphans should never kill workers assigned to running specs."""
        from lib.cli_ops import cleanup_orphans

        self._register_worker_with_pid("w-1", "q-001", pid=11111)
        self._register_worker_with_pid("w-2", "q-002", pid=22222)
        mock_ps_output = "11111 claude -p BOI Worker\n22222 claude -p BOI Worker\n"

        with patch("subprocess.check_output", return_value=mock_ps_output):
            with patch("os.kill") as mock_kill:
                cleanup_orphans(self.queue_dir)
                killed_pids = {call.args[0] for call in mock_kill.call_args_list}
                self.assertNotIn(11111, killed_pids)
                self.assertNotIn(22222, killed_pids)


if __name__ == "__main__":
    unittest.main()
