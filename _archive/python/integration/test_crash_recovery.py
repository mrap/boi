# test_crash_recovery.py — Integration tests for crash recovery.
#
# Verifies three crash/recovery scenarios:
#   (a) Daemon crash: daemon stops after assigning a spec but before
#       the worker completes. On restart, the spec is recovered to
#       'requeued'.
#   (b) Worker crash: worker is killed mid-iteration. The daemon
#       detects the dead PID and requeues the spec.
#   (c) PID reuse: a process record has a PID that now belongs to
#       a different OS process. The daemon detects the starttime
#       mismatch and does not treat the spec as still running.

import os
import signal
import subprocess
import sys
import time
import unittest
from datetime import datetime, timezone
from pathlib import Path

# Add project root to path
_PROJECT_ROOT = str(Path(__file__).resolve().parent.parent.parent)
sys.path.insert(0, _PROJECT_ROOT)

from lib.db import Database
from tests.integration.conftest import (
    IntegrationTestCase,
    MockClaude,
)


# ── Shared completion handler ───────────────────────────────────────


def _test_completion_handler(harness, spec_id, phase, exit_code, worker_id):
    """Completion handler that requeues on failure, otherwise checks tasks.

    Used by tests that need the daemon to process completions
    without importing the full daemon_ops module.
    """
    db = harness._daemon.db
    spec = db.get_spec(spec_id)
    if spec is None:
        return

    now = datetime.now(timezone.utc).isoformat()

    if exit_code != 0:
        # Non-zero exit: requeue (first failure won't hit max)
        db.insert_iteration(
            spec_id=spec_id,
            iteration=spec["iteration"],
            phase=phase,
            worker_id=worker_id,
            started_at=now,
            ended_at=now,
            duration_seconds=0,
            tasks_completed=0,
            exit_code=exit_code,
            pre_pending=(
                spec.get("tasks_total", 0) - spec.get("tasks_done", 0)
            ),
            post_pending=(
                spec.get("tasks_total", 0) - spec.get("tasks_done", 0)
            ),
        )
        db.requeue(
            spec_id,
            spec.get("tasks_done", 0),
            spec.get("tasks_total", 0),
        )
        return

    # Zero exit: check spec file for task progress
    spec_path = spec.get("spec_path", "")
    if not spec_path or not os.path.isfile(spec_path):
        db.requeue(spec_id, 0, 0)
        return

    from lib.spec_parser import parse_boi_spec

    content = Path(spec_path).read_text(encoding="utf-8")
    tasks = parse_boi_spec(content)
    done = sum(1 for t in tasks if t.status == "DONE")
    total = len(tasks)
    pending = sum(1 for t in tasks if t.status == "PENDING")

    pre_done = spec.get("tasks_done", 0)
    tasks_completed = max(0, done - pre_done)

    db.insert_iteration(
        spec_id=spec_id,
        iteration=spec["iteration"],
        phase=phase,
        worker_id=worker_id,
        started_at=now,
        ended_at=now,
        duration_seconds=0,
        tasks_completed=tasks_completed,
        exit_code=exit_code,
        pre_pending=total - pre_done,
        post_pending=pending,
    )

    if pending == 0 and total > 0:
        db.complete(spec_id, done, total)
    else:
        db.requeue(spec_id, done, total)


# ── Test (a): Daemon crash recovery ─────────────────────────────────


class TestDaemonCrashRecovery(IntegrationTestCase):
    """Kill daemon after assignment, restart, verify spec recovered."""

    NUM_WORKERS = 1

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        """Slow mock: worker delays 60s so it's still running when
        the daemon is stopped."""
        return MockClaude(
            phase="execute",
            tasks_to_complete=1,
            exit_code=0,
            delay_seconds=60,
        )

    def test_daemon_crash_spec_recovered_to_requeued(self) -> None:
        """Kill daemon after it assigns a spec to a worker but before
        the worker completes. Restart daemon. Verify spec is recovered
        to 'requeued'."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path)

        # Start daemon
        self.harness.start()

        # Wait for spec to reach 'running'
        self.harness.wait_for_status(spec_id, "running", timeout=10)

        # Verify a worker process is alive
        daemon = self.harness._daemon
        self.assertEqual(len(daemon.worker_procs), 1)
        worker_pid = list(daemon.worker_procs.values())[0].pid

        # Stop daemon. shutdown() kills workers but does NOT update
        # spec status in the DB. This simulates a crash where the
        # daemon exits without requeuing its running specs.
        self.harness.stop(timeout=5)

        # Give the OS a moment to reap the killed worker process
        time.sleep(0.5)

        # Spec should still be 'running' in DB
        spec = self.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "running")

        # Worker PID should be dead now
        try:
            os.kill(worker_pid, 0)
            self.fail("Worker PID should be dead after daemon shutdown")
        except ProcessLookupError:
            pass  # Expected

        # Simulate new daemon startup: create a fresh DB connection
        # and run startup recovery (same as Daemon.run() does).
        db2 = Database(self.config["db_path"], self.config["queue_dir"])
        try:
            recovered = db2.recover_running_specs()
            self.assertIn(spec_id, recovered)

            # Spec should now be 'requeued'
            spec = db2.get_spec(spec_id)
            self.assertEqual(spec["status"], "requeued")

            # Worker record should be freed (no longer assigned)
            cursor = db2.conn.execute(
                "SELECT * FROM workers "
                "WHERE current_spec_id = ?",
                (spec_id,),
            )
            busy_workers = cursor.fetchall()
            self.assertEqual(len(busy_workers), 0)

            # Events should record the recovery action
            events = db2.get_events(spec_id=spec_id)
            event_types = [e["event_type"] for e in events]
            self.assertIn("recover_dead_pid", event_types)
        finally:
            db2.close()

    def test_daemon_crash_process_record_marked_ended(self) -> None:
        """After daemon crash recovery, the process record should be
        marked as ended with exit_code -1 (unknown/crash)."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path)

        self.harness.start()
        self.harness.wait_for_status(spec_id, "running", timeout=10)

        self.harness.stop(timeout=5)
        time.sleep(0.5)

        db2 = Database(self.config["db_path"], self.config["queue_dir"])
        try:
            db2.recover_running_specs()

            # Process record should be ended with exit_code -1
            cursor = db2.conn.execute(
                "SELECT * FROM processes "
                "WHERE spec_id = ? AND ended_at IS NOT NULL",
                (spec_id,),
            )
            ended_procs = [dict(r) for r in cursor.fetchall()]
            self.assertTrue(
                len(ended_procs) > 0,
                "Recovery should end process records",
            )
            self.assertEqual(ended_procs[0]["exit_code"], -1)
        finally:
            db2.close()


# ── Test (b): Worker crash detection ────────────────────────────────


class TestWorkerCrashDetection(IntegrationTestCase):
    """Kill worker mid-iteration, verify daemon detects and requeues."""

    NUM_WORKERS = 1

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        """Slow mock so worker is running when we kill it."""
        return MockClaude(
            phase="execute",
            tasks_to_complete=1,
            exit_code=0,
            delay_seconds=60,
        )

    def setUp(self) -> None:
        super().setUp()
        self.harness.start()

        # Patch completion handler
        daemon = self.harness._daemon
        harness_ref = self.harness
        daemon._dispatch_phase_completion = (
            lambda spec_id, phase, exit_code, worker_id: (
                _test_completion_handler(
                    harness_ref, spec_id, phase, exit_code, worker_id
                )
            )
        )

    def _wait_for_requeue_event(
        self, spec_id: str, timeout: float = 15
    ) -> list[dict]:
        """Poll events until a 'requeued' event appears for the spec.

        The daemon re-dispatches immediately after requeue, so the
        spec's status is transient. Checking events is more reliable
        than checking status.
        """
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            events = self.db.get_events(spec_id=spec_id)
            requeue_events = [
                e for e in events if e["event_type"] == "requeued"
            ]
            if requeue_events:
                return requeue_events
            time.sleep(0.3)
        raise TimeoutError(
            f"No 'requeued' event for spec {spec_id} "
            f"within {timeout}s"
        )

    def test_worker_killed_daemon_requeues(self) -> None:
        """Kill a worker mid-iteration. Verify daemon detects the
        dead PID and requeues the spec."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path)

        # Wait for spec to be running
        self.harness.wait_for_status(spec_id, "running", timeout=10)

        # Get the worker process
        daemon = self.harness._daemon
        self.assertEqual(len(daemon.worker_procs), 1)
        worker_id = list(daemon.worker_procs.keys())[0]
        worker_proc = daemon.worker_procs[worker_id]

        # Kill the worker process with SIGKILL
        os.kill(worker_proc.pid, signal.SIGKILL)

        # Wait for requeue event (the daemon re-dispatches immediately
        # after requeue, so the status is transient; checking events
        # is more reliable)
        requeue_events = self._wait_for_requeue_event(spec_id)
        self.assertTrue(
            len(requeue_events) > 0,
            "Should have at least one requeue event after worker crash",
        )

    def test_worker_killed_process_record_updated(self) -> None:
        """When a worker is killed, the process record should show
        the signal exit code."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path)

        self.harness.wait_for_status(spec_id, "running", timeout=10)

        daemon = self.harness._daemon
        worker_proc = list(daemon.worker_procs.values())[0]

        # Kill the worker
        os.kill(worker_proc.pid, signal.SIGKILL)

        # Wait for daemon to process the crash
        self._wait_for_requeue_event(spec_id)

        # Process record should have a non-zero exit code
        cursor = self.db.conn.execute(
            "SELECT * FROM processes "
            "WHERE spec_id = ? AND ended_at IS NOT NULL",
            (spec_id,),
        )
        ended = [dict(r) for r in cursor.fetchall()]
        self.assertTrue(len(ended) > 0, "Should have ended process records")
        self.assertNotEqual(
            ended[0]["exit_code"], 0,
            "Exit code should be non-zero after SIGKILL",
        )

    def test_worker_killed_iteration_recorded(self) -> None:
        """When a worker is killed, an iteration record should be
        created with the crash exit code."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path)

        self.harness.wait_for_status(spec_id, "running", timeout=10)

        daemon = self.harness._daemon
        worker_proc = list(daemon.worker_procs.values())[0]

        os.kill(worker_proc.pid, signal.SIGKILL)

        # Wait for daemon to process the crash
        self._wait_for_requeue_event(spec_id)

        # Iteration record should exist with non-zero exit code
        iterations = self.db.get_iterations(spec_id)
        self.assertTrue(len(iterations) > 0, "Should have iteration records")
        # Find the crash iteration (iteration 1, first attempt)
        crash_iter = [
            it for it in iterations if it["exit_code"] != 0
        ]
        self.assertTrue(
            len(crash_iter) > 0,
            "Should have an iteration with non-zero exit code",
        )
        self.assertEqual(crash_iter[0]["tasks_completed"], 0)


# ── Test (c): PID reuse detection ───────────────────────────────────


class TestPidReuseDetection(IntegrationTestCase):
    """Verify PID reuse detection via /proc starttime comparison."""

    NUM_WORKERS = 1

    def test_pid_alive_correct_starttime_returns_true(self) -> None:
        """A live process with correct starttime should be detected
        as alive."""
        proc = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(999)"],
            start_new_session=True,
        )
        try:
            # Get real started_at with ticks
            started_at = self.db.make_started_at(proc.pid)

            self.assertTrue(
                Database._is_pid_alive_and_ours(proc.pid, started_at),
                "Process should be alive with correct starttime",
            )
        finally:
            os.kill(proc.pid, signal.SIGKILL)
            proc.wait()

    def test_pid_alive_wrong_starttime_returns_false(self) -> None:
        """A live process with wrong starttime (PID reuse scenario)
        should be detected as NOT ours."""
        proc = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(999)"],
            start_new_session=True,
        )
        try:
            # Create a fake started_at with wrong ticks (simulates
            # a different process that previously had this PID)
            fake_started_at = (
                datetime.now(timezone.utc).isoformat() + "|99999999999"
            )

            # On Linux with /proc, this should detect the mismatch
            stat_path = f"/proc/{proc.pid}/stat"
            if os.path.exists(stat_path):
                self.assertFalse(
                    Database._is_pid_alive_and_ours(
                        proc.pid, fake_started_at
                    ),
                    "Should detect PID reuse via starttime mismatch",
                )
            else:
                # Non-Linux: skip (can't test /proc-based detection)
                self.skipTest("/proc not available on this platform")
        finally:
            os.kill(proc.pid, signal.SIGKILL)
            proc.wait()

    def test_dead_pid_returns_false(self) -> None:
        """A dead process should not be detected as alive regardless
        of starttime."""
        proc = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(999)"],
            start_new_session=True,
        )
        pid = proc.pid
        started_at = self.db.make_started_at(pid)

        # Kill and wait for it to die
        os.kill(pid, signal.SIGKILL)
        proc.wait()

        self.assertFalse(
            Database._is_pid_alive_and_ours(pid, started_at),
            "Dead process should not be alive",
        )

    def test_pid_reuse_recovery_through_db(self) -> None:
        """Set up a spec as 'running' with a process whose PID belongs
        to a different OS process (simulated via wrong starttime ticks).
        Verify recover_running_specs detects the mismatch and requeues."""
        # Start a long-running process (simulates the "new" process
        # that has reused our PID)
        proc = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(999)"],
            start_new_session=True,
        )
        try:
            pid = proc.pid
            stat_path = f"/proc/{pid}/stat"
            if not os.path.exists(stat_path):
                self.skipTest("/proc not available on this platform")

            # Set up DB state: spec in 'running' with this PID
            spec_path = self.create_spec(tasks_pending=3)
            spec_id = self.dispatch_spec(spec_path)

            worker_id = "w-1"
            self.db.register_worker(
                worker_id, self.config["worktrees"][0]
            )

            # Transition: queued → assigning → running
            self.db.pick_next_spec()
            self.db.set_running(spec_id, worker_id, "execute")

            # Assign worker with the PID
            self.db.assign_worker(
                worker_id, spec_id, pid=pid, phase="execute"
            )

            # Register process with WRONG starttime ticks.
            # This simulates PID reuse: the PID is alive but belongs
            # to a completely different process than we originally
            # launched.
            fake_started_at = (
                datetime.now(timezone.utc).isoformat() + "|99999999999"
            )
            with self.db.lock:
                self.db.conn.execute(
                    "INSERT OR REPLACE INTO processes "
                    "(pid, spec_id, worker_id, iteration, "
                    "phase, started_at) "
                    "VALUES (?, ?, ?, ?, ?, ?)",
                    (
                        pid, spec_id, worker_id, 1,
                        "execute", fake_started_at,
                    ),
                )
                self.db.conn.commit()

            # Recovery should detect PID reuse (ticks mismatch)
            recovered = self.db.recover_running_specs()
            self.assertIn(spec_id, recovered)

            # Spec should be requeued
            spec = self.db.get_spec(spec_id)
            self.assertEqual(spec["status"], "requeued")

            # Recovery event should be logged
            events = self.db.get_events(spec_id=spec_id)
            event_types = [e["event_type"] for e in events]
            self.assertIn("recover_dead_pid", event_types)
        finally:
            try:
                os.kill(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            proc.wait()

    def test_pid_reuse_not_confused_with_real_process(self) -> None:
        """Verify that a running spec whose worker PID is genuinely
        alive (correct starttime) is NOT recovered/requeued."""
        proc = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(999)"],
            start_new_session=True,
        )
        try:
            pid = proc.pid
            stat_path = f"/proc/{pid}/stat"
            if not os.path.exists(stat_path):
                self.skipTest("/proc not available on this platform")

            # Set up spec in 'running' with this PID
            spec_path = self.create_spec(tasks_pending=3)
            spec_id = self.dispatch_spec(spec_path)

            worker_id = "w-1"
            self.db.register_worker(
                worker_id, self.config["worktrees"][0]
            )

            self.db.pick_next_spec()
            self.db.set_running(spec_id, worker_id, "execute")
            self.db.assign_worker(
                worker_id, spec_id, pid=pid, phase="execute"
            )

            # Register process with CORRECT starttime
            real_started_at = self.db.make_started_at(pid)
            with self.db.lock:
                self.db.conn.execute(
                    "INSERT OR REPLACE INTO processes "
                    "(pid, spec_id, worker_id, iteration, "
                    "phase, started_at) "
                    "VALUES (?, ?, ?, ?, ?, ?)",
                    (
                        pid, spec_id, worker_id, 1,
                        "execute", real_started_at,
                    ),
                )
                self.db.conn.commit()

            # Recovery should NOT requeue this spec (PID is alive
            # and starttime matches)
            recovered = self.db.recover_running_specs()
            self.assertNotIn(spec_id, recovered)

            # Spec should still be running
            spec = self.db.get_spec(spec_id)
            self.assertEqual(spec["status"], "running")
        finally:
            try:
                os.kill(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            proc.wait()


if __name__ == "__main__":
    unittest.main()
