# test_daemon_lock.py — TDD RED phase tests for daemon single-instance locking.
#
# Tests that only one daemon can run at a time using fcntl.flock,
# and that stale locks from dead processes are reclaimed. All tests
# should FAIL until the daemon lock logic is implemented.
#
# Uses stdlib unittest only (no pytest dependency).

import fcntl
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))


class DaemonLockTestCase(unittest.TestCase):
    """Base test case with a temp state directory."""

    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.state_dir = self._tmpdir.name
        self.lock_path = os.path.join(self.state_dir, "daemon.lock")
        self.pid_path = os.path.join(self.state_dir, "daemon.pid")

    def tearDown(self) -> None:
        self._tmpdir.cleanup()


class TestDaemonLockAcquisition(DaemonLockTestCase):
    """Test that the daemon acquires and releases locks properly."""

    def test_daemon_acquires_lock_on_start(self) -> None:
        """DaemonLock.acquire() should create a lock file and hold it."""
        from lib.daemon_lock import DaemonLock

        lock = DaemonLock(self.state_dir)
        lock.acquire()
        try:
            self.assertTrue(os.path.exists(self.lock_path))
            # Verify the lock is actually held (non-blocking acquire should fail)
            fd = open(self.lock_path, "w")
            try:
                with self.assertRaises((BlockingIOError, OSError)):
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            finally:
                fd.close()
        finally:
            lock.release()

    def test_second_daemon_exits_if_locked(self) -> None:
        """A second DaemonLock.acquire() should raise if lock is held."""
        from lib.daemon_lock import DaemonLock

        lock1 = DaemonLock(self.state_dir)
        lock1.acquire()
        try:
            lock2 = DaemonLock(self.state_dir)
            with self.assertRaises(SystemExit):
                lock2.acquire()
        finally:
            lock1.release()

    def test_lock_released_on_daemon_stop(self) -> None:
        """DaemonLock.release() should free the lock so another can acquire."""
        from lib.daemon_lock import DaemonLock

        lock1 = DaemonLock(self.state_dir)
        lock1.acquire()
        lock1.release()

        # A second lock should now succeed
        lock2 = DaemonLock(self.state_dir)
        lock2.acquire()
        lock2.release()


class TestStaleLockRecovery(DaemonLockTestCase):
    """Test that stale locks from dead processes are reclaimed."""

    def test_stale_lock_from_dead_process_is_reclaimed(self) -> None:
        """If the PID in the lock file is dead, acquire should succeed."""
        from lib.daemon_lock import DaemonLock

        # Write a PID file with a definitely-dead PID
        Path(self.pid_path).write_text("999999999\n")
        # Create a lock file (but don't hold the flock)
        Path(self.lock_path).write_text("")

        lock = DaemonLock(self.state_dir)
        # Should succeed because PID 999999999 is not alive
        lock.acquire()
        lock.release()


class TestDaemonStatus(DaemonLockTestCase):
    """Test daemon status reporting."""

    def test_daemon_status_shows_running(self) -> None:
        """daemon_status() should report running when lock is held."""
        from lib.daemon_lock import daemon_status, DaemonLock

        lock = DaemonLock(self.state_dir)
        lock.acquire()
        try:
            status = daemon_status(self.state_dir)
            self.assertEqual(status["running"], True)
            self.assertIn("pid", status)
        finally:
            lock.release()

    def test_daemon_status_shows_stopped(self) -> None:
        """daemon_status() should report stopped when no lock is held."""
        from lib.daemon_lock import daemon_status

        status = daemon_status(self.state_dir)
        self.assertEqual(status["running"], False)


if __name__ == "__main__":
    unittest.main()
