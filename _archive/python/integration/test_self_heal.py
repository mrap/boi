# test_self_heal.py — Integration tests for daemon self-heal logic.
#
# Verifies four self-heal scenarios:
#   (a) Orphaned worker: worker marked busy in DB but its spec is
#       in a terminal state (completed). Self-heal frees the worker.
#   (b) Stuck assigning: spec stuck in 'assigning' for >60s.
#       Self-heal resets it to 'requeued'.
#   (c) needs_review timeout: experiment spec in needs_review for
#       >24 hours. Self-heal auto-rejects and requeues.
#   (d) Dead PID recovery: spec 'running' with dead PID. Self-heal
#       detects the dead PID and requeues the spec.

import os
import signal
import subprocess
import sys
import time
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path

# Add project root to path
_PROJECT_ROOT = str(Path(__file__).resolve().parent.parent.parent)
sys.path.insert(0, _PROJECT_ROOT)

from lib.db import Database
from tests.integration.conftest import (
    IntegrationTestCase,
    MockClaude,
    create_test_config,
)


# ── Test (a): Orphaned worker ────────────────────────────────────────


class TestOrphanedWorkerSelfHeal(IntegrationTestCase):
    """Worker marked busy with a spec that is already completed."""

    NUM_WORKERS = 1

    def test_orphaned_worker_freed_after_self_heal(self) -> None:
        """Mark worker busy in DB with a completed spec, run
        self-heal, verify worker is freed."""
        # Create and enqueue a spec
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path)

        # Register worker and assign it to the spec
        worker_id = "w-1"
        self.db.register_worker(
            worker_id, self.config["worktrees"][0]
        )

        # Transition spec through to completed
        self.db.pick_next_spec()
        self.db.set_running(spec_id, worker_id, "execute")
        self.db.assign_worker(worker_id, spec_id, pid=99999, phase="execute")
        self.db.complete(spec_id, tasks_done=3, tasks_total=3)
        # db.complete() now atomically frees the worker. Re-assign the worker
        # to the completed spec to simulate the orphaned state (e.g. after a
        # crash where the worker assignment was written but free was skipped).
        with self.db.lock:
            self.db.conn.execute(
                "UPDATE workers SET current_spec_id = ?, current_pid = 99999 "
                "WHERE id = ?",
                (spec_id, worker_id),
            )
            self.db.conn.commit()

        # Worker is now marked busy even though spec is completed (orphaned)
        worker = self.db.get_worker(worker_id)
        self.assertEqual(worker["current_spec_id"], spec_id)

        # Spec is completed
        spec = self.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "completed")

        # Run self-heal via daemon
        from daemon import Daemon

        daemon = Daemon(
            config_path=self.config["config_path"],
            db_path=self.config["db_path"],
            state_dir=self.config["state_dir"],
        )
        daemon.db = self.db
        daemon.self_heal()

        # Worker should now be freed
        worker = self.db.get_worker(worker_id)
        self.assertIsNone(
            worker["current_spec_id"],
            "Orphaned worker should be freed after self-heal",
        )
        self.assertIsNone(worker["current_pid"])

    def test_orphaned_worker_assigned_to_missing_spec(self) -> None:
        """Worker assigned to a spec ID that doesn't exist in the DB.
        Self-heal should free the worker."""
        worker_id = "w-1"
        self.db.register_worker(
            worker_id, self.config["worktrees"][0]
        )

        # Manually assign worker to a non-existent spec.
        # Disable FK enforcement temporarily to simulate corrupt state
        # (e.g. spec row was deleted while worker was still assigned).
        with self.db.lock:
            self.db.conn.execute("PRAGMA foreign_keys=OFF")
            self.db.conn.execute(
                "UPDATE workers SET "
                "current_spec_id = 'q-nonexistent', "
                "current_pid = 12345 "
                "WHERE id = ?",
                (worker_id,),
            )
            self.db.conn.commit()
            self.db.conn.execute("PRAGMA foreign_keys=ON")

        worker = self.db.get_worker(worker_id)
        self.assertEqual(worker["current_spec_id"], "q-nonexistent")

        # Run self-heal
        from daemon import Daemon

        daemon = Daemon(
            config_path=self.config["config_path"],
            db_path=self.config["db_path"],
            state_dir=self.config["state_dir"],
        )
        daemon.db = self.db
        daemon.self_heal()

        # Worker should be freed
        worker = self.db.get_worker(worker_id)
        self.assertIsNone(
            worker["current_spec_id"],
            "Worker assigned to missing spec should be freed",
        )

    def test_orphaned_worker_assigned_to_failed_spec(self) -> None:
        """Worker assigned to a spec that is 'failed'.
        Self-heal should free the worker."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path)

        worker_id = "w-1"
        self.db.register_worker(
            worker_id, self.config["worktrees"][0]
        )

        # Transition spec to failed
        self.db.pick_next_spec()
        self.db.set_running(spec_id, worker_id, "execute")
        self.db.assign_worker(worker_id, spec_id, pid=99999, phase="execute")
        self.db.fail(spec_id, "test failure reason")

        # Worker still busy
        worker = self.db.get_worker(worker_id)
        self.assertEqual(worker["current_spec_id"], spec_id)

        # Run self-heal
        from daemon import Daemon

        daemon = Daemon(
            config_path=self.config["config_path"],
            db_path=self.config["db_path"],
            state_dir=self.config["state_dir"],
        )
        daemon.db = self.db
        daemon.self_heal()

        # Worker should be freed
        worker = self.db.get_worker(worker_id)
        self.assertIsNone(
            worker["current_spec_id"],
            "Worker assigned to failed spec should be freed",
        )


# ── Test (b): Stuck assigning ────────────────────────────────────────


class TestStuckAssigningSelfHeal(IntegrationTestCase):
    """Spec stuck in 'assigning' for longer than 60 seconds."""

    NUM_WORKERS = 1

    def test_stuck_assigning_reset_to_requeued(self) -> None:
        """Set spec status to 'assigning' with assigning_at 120 seconds
        ago, run self-heal, verify reset to 'requeued'."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path)

        # Pick the spec (sets to 'assigning' with current timestamp)
        self.db.pick_next_spec()
        spec = self.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "assigning")

        # Backdate assigning_at by 120 seconds
        old_time = (
            datetime.now(timezone.utc) - timedelta(seconds=120)
        ).isoformat()
        with self.db.lock:
            self.db.conn.execute(
                "UPDATE specs SET assigning_at = ? WHERE id = ?",
                (old_time, spec_id),
            )
            self.db.conn.commit()

        # Run self-heal (calls recover_stale_assigning internally)
        from daemon import Daemon

        daemon = Daemon(
            config_path=self.config["config_path"],
            db_path=self.config["db_path"],
            state_dir=self.config["state_dir"],
        )
        daemon.db = self.db
        daemon.self_heal()

        # Spec should be requeued
        spec = self.db.get_spec(spec_id)
        self.assertEqual(
            spec["status"], "requeued",
            "Stuck-assigning spec should be reset to 'requeued'",
        )
        self.assertIsNone(
            spec["assigning_at"],
            "assigning_at should be cleared after recovery",
        )

    def test_recent_assigning_not_recovered(self) -> None:
        """Spec in 'assigning' for only 10 seconds should NOT be
        recovered by self-heal."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path)

        # Pick the spec (sets to 'assigning' with current timestamp)
        self.db.pick_next_spec()

        # assigning_at is recent (just set). Run self-heal.
        from daemon import Daemon

        daemon = Daemon(
            config_path=self.config["config_path"],
            db_path=self.config["db_path"],
            state_dir=self.config["state_dir"],
        )
        daemon.db = self.db
        daemon.self_heal()

        # Spec should still be in 'assigning'
        spec = self.db.get_spec(spec_id)
        self.assertEqual(
            spec["status"], "assigning",
            "Recently-assigning spec should NOT be recovered",
        )

    def test_stuck_assigning_event_logged(self) -> None:
        """Recovery of stuck-assigning should log an event."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path)

        self.db.pick_next_spec()

        # Backdate assigning_at
        old_time = (
            datetime.now(timezone.utc) - timedelta(seconds=120)
        ).isoformat()
        with self.db.lock:
            self.db.conn.execute(
                "UPDATE specs SET assigning_at = ? WHERE id = ?",
                (old_time, spec_id),
            )
            self.db.conn.commit()

        from daemon import Daemon

        daemon = Daemon(
            config_path=self.config["config_path"],
            db_path=self.config["db_path"],
            state_dir=self.config["state_dir"],
        )
        daemon.db = self.db
        daemon.self_heal()

        # Should have a recovery event
        events = self.db.get_events(spec_id=spec_id)
        event_types = [e["event_type"] for e in events]
        self.assertIn(
            "recover_stale_assigning", event_types,
            "Should log recover_stale_assigning event",
        )


# ── Test (c): needs_review timeout ──────────────────────────────────


class TestNeedsReviewTimeoutSelfHeal(IntegrationTestCase):
    """Experiment spec in needs_review for >24 hours."""

    NUM_WORKERS = 1

    def _create_needs_review_spec(
        self, hours_ago: float = 25
    ) -> str:
        """Create a spec in needs_review status with experiment tasks."""
        # Create spec with an EXPERIMENT_PROPOSED task
        content = (
            "# Test Spec\n\n## Tasks\n\n"
            "### t-1: Done task 1\nDONE\n\n"
            "**Spec:** Completed task 1.\n\n"
            "**Verify:** true\n\n"
            "### t-2: Experiment task\n"
            "EXPERIMENT_PROPOSED\n\n"
            "**Spec:** Try something new.\n\n"
            "**Verify:** true\n\n"
            "#### Experiment: test approach\n\n"
            "This is an experiment.\n"
        )
        spec_path = self.create_spec(content=content)
        spec_id = self.dispatch_spec(spec_path)

        # Transition to needs_review
        worker_id = "w-1"
        self.db.register_worker(
            worker_id, self.config["worktrees"][0]
        )
        self.db.pick_next_spec()
        self.db.set_running(spec_id, worker_id, "execute")

        # Set status to needs_review with old timestamp
        review_time = (
            datetime.now(timezone.utc) - timedelta(hours=hours_ago)
        ).isoformat()
        with self.db.lock:
            self.db.conn.execute(
                "UPDATE specs SET "
                "status = 'needs_review', "
                "needs_review_since = ? "
                "WHERE id = ?",
                (review_time, spec_id),
            )
            self.db.conn.commit()

        # Free the worker so self-heal doesn't get confused
        self.db.free_worker(worker_id)

        return spec_id

    def test_needs_review_auto_rejected_after_timeout(self) -> None:
        """Spec in needs_review for 25 hours should be auto-rejected
        and requeued."""
        spec_id = self._create_needs_review_spec(hours_ago=25)

        # Verify state
        spec = self.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "needs_review")

        # Run self-heal
        from daemon import Daemon

        daemon = Daemon(
            config_path=self.config["config_path"],
            db_path=self.config["db_path"],
            state_dir=self.config["state_dir"],
        )
        daemon.db = self.db
        daemon.self_heal()

        # Spec should be requeued (auto-rejected)
        spec = self.db.get_spec(spec_id)
        self.assertIn(
            spec["status"],
            ["requeued", "queued"],
            "needs_review spec should be requeued after timeout",
        )

    def test_needs_review_not_rejected_before_timeout(self) -> None:
        """Spec in needs_review for only 1 hour should NOT be
        auto-rejected."""
        spec_id = self._create_needs_review_spec(hours_ago=1)

        spec = self.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "needs_review")

        from daemon import Daemon

        daemon = Daemon(
            config_path=self.config["config_path"],
            db_path=self.config["db_path"],
            state_dir=self.config["state_dir"],
        )
        daemon.db = self.db
        daemon.self_heal()

        # Spec should still be needs_review
        spec = self.db.get_spec(spec_id)
        self.assertEqual(
            spec["status"], "needs_review",
            "needs_review spec should NOT be rejected before timeout",
        )

    def test_needs_review_experiment_tasks_reverted(self) -> None:
        """After auto-rejection, EXPERIMENT_PROPOSED tasks in the spec
        file should be reverted to PENDING."""
        spec_id = self._create_needs_review_spec(hours_ago=25)
        spec = self.db.get_spec(spec_id)
        spec_path = spec["spec_path"]

        # Verify spec file has EXPERIMENT_PROPOSED
        content = Path(spec_path).read_text(encoding="utf-8")
        self.assertIn("EXPERIMENT_PROPOSED", content)

        from daemon import Daemon

        daemon = Daemon(
            config_path=self.config["config_path"],
            db_path=self.config["db_path"],
            state_dir=self.config["state_dir"],
        )
        daemon.db = self.db
        daemon.self_heal()

        # Spec file should have PENDING instead of EXPERIMENT_PROPOSED
        content = Path(spec_path).read_text(encoding="utf-8")
        self.assertNotIn(
            "EXPERIMENT_PROPOSED", content,
            "EXPERIMENT_PROPOSED should be reverted to PENDING",
        )
        self.assertIn("PENDING", content)


# ── Test (d): Dead PID recovery ─────────────────────────────────────


class TestDeadPidRecoverySelfHeal(IntegrationTestCase):
    """Spec 'running' with a dead worker PID. Self-heal requeues."""

    NUM_WORKERS = 1

    def test_dead_pid_spec_requeued_by_self_heal(self) -> None:
        """Register process with dead PID, run self-heal, verify
        spec requeued."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path)

        worker_id = "w-1"
        self.db.register_worker(
            worker_id, self.config["worktrees"][0]
        )

        # Transition to running
        self.db.pick_next_spec()
        self.db.set_running(spec_id, worker_id, "execute")

        # Spawn a process and immediately kill it to get a dead PID
        proc = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(999)"],
            start_new_session=True,
        )
        dead_pid = proc.pid
        started_at = self.db.make_started_at(dead_pid)
        os.kill(dead_pid, signal.SIGKILL)
        proc.wait()

        # Assign the dead PID to the worker and register the process
        self.db.assign_worker(
            worker_id, spec_id, pid=dead_pid, phase="execute"
        )
        self.db.register_process(
            pid=dead_pid,
            spec_id=spec_id,
            worker_id=worker_id,
            iteration=1,
            phase="execute",
        )
        # Update the process started_at with the real value
        with self.db.lock:
            self.db.conn.execute(
                "UPDATE processes SET started_at = ? "
                "WHERE pid = ? AND spec_id = ?",
                (started_at, dead_pid, spec_id),
            )
            self.db.conn.commit()

        # Confirm spec is running with dead PID
        spec = self.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "running")
        worker = self.db.get_worker(worker_id)
        self.assertEqual(worker["current_pid"], dead_pid)

        # Run self-heal
        from daemon import Daemon

        daemon = Daemon(
            config_path=self.config["config_path"],
            db_path=self.config["db_path"],
            state_dir=self.config["state_dir"],
        )
        daemon.db = self.db
        daemon.self_heal()

        # Spec should be requeued
        spec = self.db.get_spec(spec_id)
        self.assertEqual(
            spec["status"], "requeued",
            "Spec with dead PID should be requeued by self-heal",
        )

    def test_dead_pid_worker_freed(self) -> None:
        """After dead PID recovery, the worker should be freed."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path)

        worker_id = "w-1"
        self.db.register_worker(
            worker_id, self.config["worktrees"][0]
        )

        self.db.pick_next_spec()
        self.db.set_running(spec_id, worker_id, "execute")

        # Create and kill a process
        proc = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(999)"],
            start_new_session=True,
        )
        dead_pid = proc.pid
        started_at = self.db.make_started_at(dead_pid)
        os.kill(dead_pid, signal.SIGKILL)
        proc.wait()

        self.db.assign_worker(
            worker_id, spec_id, pid=dead_pid, phase="execute"
        )
        self.db.register_process(
            pid=dead_pid,
            spec_id=spec_id,
            worker_id=worker_id,
            iteration=1,
            phase="execute",
        )
        with self.db.lock:
            self.db.conn.execute(
                "UPDATE processes SET started_at = ? "
                "WHERE pid = ? AND spec_id = ?",
                (started_at, dead_pid, spec_id),
            )
            self.db.conn.commit()

        # Run self-heal
        from daemon import Daemon

        daemon = Daemon(
            config_path=self.config["config_path"],
            db_path=self.config["db_path"],
            state_dir=self.config["state_dir"],
        )
        daemon.db = self.db
        daemon.self_heal()

        # Worker should be freed
        worker = self.db.get_worker(worker_id)
        self.assertIsNone(
            worker["current_spec_id"],
            "Worker should be freed after dead PID recovery",
        )
        self.assertIsNone(worker["current_pid"])

    def test_dead_pid_recovery_event_logged(self) -> None:
        """Dead PID recovery should log a recover_dead_pid event."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path)

        worker_id = "w-1"
        self.db.register_worker(
            worker_id, self.config["worktrees"][0]
        )

        self.db.pick_next_spec()
        self.db.set_running(spec_id, worker_id, "execute")

        # Create and kill a process
        proc = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(999)"],
            start_new_session=True,
        )
        dead_pid = proc.pid
        started_at = self.db.make_started_at(dead_pid)
        os.kill(dead_pid, signal.SIGKILL)
        proc.wait()

        self.db.assign_worker(
            worker_id, spec_id, pid=dead_pid, phase="execute"
        )
        self.db.register_process(
            pid=dead_pid,
            spec_id=spec_id,
            worker_id=worker_id,
            iteration=1,
            phase="execute",
        )
        with self.db.lock:
            self.db.conn.execute(
                "UPDATE processes SET started_at = ? "
                "WHERE pid = ? AND spec_id = ?",
                (started_at, dead_pid, spec_id),
            )
            self.db.conn.commit()

        from daemon import Daemon

        daemon = Daemon(
            config_path=self.config["config_path"],
            db_path=self.config["db_path"],
            state_dir=self.config["state_dir"],
        )
        daemon.db = self.db
        daemon.self_heal()

        # Check for recovery event
        events = self.db.get_events(spec_id=spec_id)
        event_types = [e["event_type"] for e in events]
        self.assertIn(
            "recover_dead_pid", event_types,
            "Should log recover_dead_pid event",
        )

    def test_live_pid_not_recovered(self) -> None:
        """Spec running with a genuinely alive PID should NOT be
        recovered by self-heal."""
        stat_path = "/proc/1/stat"
        if not os.path.exists(stat_path):
            self.skipTest("/proc not available on this platform")

        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path)

        worker_id = "w-1"
        self.db.register_worker(
            worker_id, self.config["worktrees"][0]
        )

        self.db.pick_next_spec()
        self.db.set_running(spec_id, worker_id, "execute")

        # Create a long-running process
        proc = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(999)"],
            start_new_session=True,
        )
        try:
            live_pid = proc.pid
            started_at = self.db.make_started_at(live_pid)

            self.db.assign_worker(
                worker_id, spec_id, pid=live_pid, phase="execute"
            )
            self.db.register_process(
                pid=live_pid,
                spec_id=spec_id,
                worker_id=worker_id,
                iteration=1,
                phase="execute",
            )
            with self.db.lock:
                self.db.conn.execute(
                    "UPDATE processes SET started_at = ? "
                    "WHERE pid = ? AND spec_id = ?",
                    (started_at, live_pid, spec_id),
                )
                self.db.conn.commit()

            # Run self-heal
            from daemon import Daemon

            daemon = Daemon(
                config_path=self.config["config_path"],
                db_path=self.config["db_path"],
                state_dir=self.config["state_dir"],
            )
            daemon.db = self.db
            daemon.self_heal()

            # Spec should still be running
            spec = self.db.get_spec(spec_id)
            self.assertEqual(
                spec["status"], "running",
                "Spec with live PID should NOT be recovered",
            )
        finally:
            try:
                os.kill(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            proc.wait()


if __name__ == "__main__":
    unittest.main()
