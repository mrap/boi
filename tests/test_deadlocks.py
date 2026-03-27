# test_deadlocks.py — Deadlock reproduction tests for BOI.
#
# Tests every cataloged deadlock/stuck scenario from q-008 t-1.
# Each test sets up the stuck state, runs self_heal (or the relevant
# function), and asserts the spec is recovered.
#
# Uses stdlib unittest only (no pytest dependency).
# All tests use mock data, no live Claude calls.

import json
import os
import sys
import tempfile
import textwrap
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path

# Add parent directory to path so we can import lib modules
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.daemon_ops import (
    CompletionContext,
    process_critic_completion,
    process_worker_completion,
    self_heal,
)
from lib.db import Database


class DeadlockTestCase(unittest.TestCase):
    """Base test case for deadlock reproduction tests."""

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.boi_state = self._tmpdir.name
        self.queue_dir = os.path.join(self.boi_state, "queue")
        self.events_dir = os.path.join(self.boi_state, "events")
        self.log_dir = os.path.join(self.boi_state, "logs")
        self.hooks_dir = os.path.join(self.boi_state, "hooks")
        self.specs_dir = os.path.join(self._tmpdir.name, "specs")
        self.script_dir = str(Path(__file__).resolve().parent.parent)
        os.makedirs(self.queue_dir)
        os.makedirs(self.events_dir)
        os.makedirs(self.log_dir)
        os.makedirs(self.hooks_dir)
        os.makedirs(self.specs_dir)

        # Disable critic by default
        critic_dir = os.path.join(self.boi_state, "critic")
        os.makedirs(critic_dir, exist_ok=True)
        os.makedirs(os.path.join(critic_dir, "custom"), exist_ok=True)
        config_path = os.path.join(critic_dir, "config.json")
        Path(config_path).write_text(json.dumps({"enabled": False}, indent=2) + "\n")

        # Create SQLite database
        db_path = os.path.join(self.boi_state, "boi.db")
        self.db = Database(db_path, self.queue_dir)
        self.ctx = CompletionContext(
            queue_dir=self.queue_dir,
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            log_dir=self.log_dir,
            script_dir=self.script_dir,
            db=self.db,
        )

    def tearDown(self):
        self.db.close()
        self._tmpdir.cleanup()

    def _make_spec(
        self,
        tasks_pending: int = 2,
        tasks_done: int = 0,
        filename: str = "spec.md",
        content: str | None = None,
    ) -> str:
        """Create a spec file. Returns absolute path."""
        spec_path = os.path.join(self.specs_dir, filename)
        if content is not None:
            Path(spec_path).write_text(content, encoding="utf-8")
            return spec_path

        lines = ["# Test Spec\n\n**Workspace:** in-place\n\n## Tasks\n"]
        tid = 1
        for _ in range(tasks_done):
            lines.append(
                f"\n### t-{tid}: Done task {tid}\n"
                "DONE\n\n"
                f"**Spec:** Completed task {tid}.\n\n"
                "**Verify:** true\n"
            )
            tid += 1
        for _ in range(tasks_pending):
            lines.append(
                f"\n### t-{tid}: Pending task {tid}\n"
                "PENDING\n\n"
                f"**Spec:** Do task {tid}.\n\n"
                "**Verify:** true\n"
            )
            tid += 1

        Path(spec_path).write_text("".join(lines), encoding="utf-8")
        return spec_path

    def _db_set_running(self, qid: str, worker_id: str, pid: int | None = None) -> None:
        """Set spec to running in db with a linked worker record."""
        now = datetime.now(timezone.utc).isoformat()
        self.db.conn.execute(
            "INSERT INTO workers (id, worktree_path) VALUES (?, '/tmp') "
            "ON CONFLICT(id) DO NOTHING",
            (worker_id,),
        )
        self.db.conn.execute(
            "UPDATE workers SET current_spec_id=?, current_pid=? WHERE id=?",
            (qid, pid, worker_id),
        )
        self.db.conn.execute(
            "UPDATE specs SET status='running', "
            "first_running_at=COALESCE(first_running_at, ?) WHERE id=?",
            (now, qid),
        )
        self.db.conn.commit()

    def _db_update_spec(self, qid: str, **fields) -> None:
        """Update spec columns directly via SQL (bypasses _UPDATABLE_COLUMNS)."""
        if not fields:
            return
        set_clause = ", ".join(f"{k}=?" for k in fields)
        self.db.conn.execute(
            f"UPDATE specs SET {set_clause} WHERE id=?",
            (*fields.values(), qid),
        )
        self.db.conn.commit()

    def _db_add_dependency(self, spec_id: str, blocks_on: str) -> None:
        """Insert a row into spec_dependencies. Temporarily disables FK if needed."""
        # Disable FK to allow referencing non-existent specs (missing-dep scenario)
        self.db.conn.execute("PRAGMA foreign_keys=OFF")
        self.db.conn.execute(
            "INSERT OR IGNORE INTO spec_dependencies (spec_id, blocks_on) VALUES (?, ?)",
            (spec_id, blocks_on),
        )
        self.db.conn.commit()
        self.db.conn.execute("PRAGMA foreign_keys=ON")

    def _db_get_blocked_by(self, spec_id: str) -> list:
        """Return list of blocks_on values for a spec."""
        cursor = self.db.conn.execute(
            "SELECT blocks_on FROM spec_dependencies WHERE spec_id=?", (spec_id,)
        )
        return [row["blocks_on"] for row in cursor]


class TestStaleRunningSpecRecovered(DeadlockTestCase):
    """Scenario 1: Running spec with dead PID gets stuck forever.

    Reproduction: Set spec status to 'running', set a dead PID on the
    worker record. Without self-heal, the daemon never detects the failure
    and the spec sits in 'running' forever.

    Fix: self_heal detects dead PID and resets to 'requeued'.
    """

    def test_stale_running_spec_recovered(self):
        spec_path = self._make_spec(tasks_pending=2)
        entry = self.db.enqueue(spec_path)
        qid = entry["id"]
        self._db_set_running(qid, "w-1", pid=999999999)

        actions = self_heal(self.queue_dir, {"w-1": qid}, db=self.db)

        # Spec should be recovered
        stale = [a for a in actions if a["action"] == "stale_running_recovered"]
        self.assertEqual(len(stale), 1)

        updated = self.db.get_spec(qid)
        self.assertEqual(updated["status"], "requeued")

    def test_stale_running_no_pid_file(self):
        """Running spec with no PID (None) at all should also be recovered."""
        spec_path = self._make_spec(tasks_pending=2)
        entry = self.db.enqueue(spec_path)
        qid = entry["id"]
        self._db_set_running(qid, "w-1", pid=None)  # No PID = treated as dead

        actions = self_heal(self.queue_dir, {"w-1": qid}, db=self.db)

        stale = [a for a in actions if a["action"] == "stale_running_recovered"]
        self.assertEqual(len(stale), 1)

        updated = self.db.get_spec(qid)
        self.assertEqual(updated["status"], "requeued")


class TestOrphanedWorkerFreed(DeadlockTestCase):
    """Scenario 7: Worker assigned to a terminal-state spec.

    Reproduction: Worker w-1 is assigned to q-001 in daemon memory, but
    q-001 has already been marked 'completed' on disk. The worker slot
    appears busy, preventing new work from being assigned.

    Fix: self_heal detects the mismatch and reports the worker as orphaned
    so the daemon can free it.
    """

    def test_orphaned_worker_freed(self):
        spec_path = self._make_spec(tasks_pending=0, tasks_done=3)
        entry = self.db.enqueue(spec_path)
        qid = entry["id"]
        self._db_set_running(qid, "w-1")
        self.db.complete(qid, 3, 3)

        # Worker still thinks it owns this spec
        worker_specs = {"w-1": qid, "w-2": ""}
        actions = self_heal(self.queue_dir, worker_specs, db=self.db)

        orphan = [a for a in actions if a["action"] == "orphaned_worker"]
        self.assertEqual(len(orphan), 1)
        self.assertEqual(orphan[0]["worker_id"], "w-1")
        self.assertEqual(orphan[0]["queue_id"], qid)

    def test_orphaned_worker_failed_spec(self):
        """Worker assigned to a failed spec should also be freed."""
        spec_path = self._make_spec(tasks_pending=2)
        entry = self.db.enqueue(spec_path)
        qid = entry["id"]
        self._db_set_running(qid, "w-1")
        self.db.fail(qid, "test failure")

        worker_specs = {"w-1": qid}
        actions = self_heal(self.queue_dir, worker_specs, db=self.db)

        orphan = [a for a in actions if a["action"] == "orphaned_worker"]
        self.assertEqual(len(orphan), 1)


class TestBlockedByCompletedSpecUnblocked(DeadlockTestCase):
    """Scenario 4: Spec blocked by a completed/missing spec.

    Reproduction: Spec B has blocked_by=[A]. Spec A completes, but B's
    blocked_by is never cleaned up, so B stays queued forever unable
    to be dequeued.

    Fix: self_heal removes completed specs from blocked_by lists.
    """

    def test_blocked_by_completed_spec_unblocked(self):
        blocker_path = self._make_spec(tasks_pending=0, tasks_done=1)
        blocker = self.db.enqueue(blocker_path)
        self._db_set_running(blocker["id"], "w-1")
        self.db.complete(blocker["id"], 1, 1)

        blocked_path = self._make_spec(tasks_pending=2, filename="blocked.md")
        blocked = self.db.enqueue(blocked_path, blocked_by=[blocker["id"]])

        # Verify it's blocked
        self.assertEqual(self._db_get_blocked_by(blocked["id"]), [blocker["id"]])

        actions = self_heal(self.queue_dir, {}, db=self.db)

        cleanup = [a for a in actions if a["action"] == "blocked_by_cleaned"]
        self.assertEqual(len(cleanup), 1)

        self.assertEqual(self._db_get_blocked_by(blocked["id"]), [])


class TestBlockedByMissingSpecUnblocked(DeadlockTestCase):
    """Scenario 4 variant: Spec blocked by a nonexistent spec ID.

    Reproduction: Spec has blocked_by=["q-999"] but q-999 doesn't exist
    (deleted, purged, or typo). Spec is permanently stuck.

    Fix: self_heal removes missing spec IDs from blocked_by lists.
    """

    def test_blocked_by_missing_spec_unblocked(self):
        spec_path = self._make_spec(tasks_pending=2)
        entry = self.db.enqueue(spec_path)
        qid = entry["id"]

        # Bypass FK to create a dep on a non-existent spec (simulates deleted blocker)
        self._db_add_dependency(qid, "q-999")

        actions = self_heal(self.queue_dir, {}, db=self.db)

        cleanup = [a for a in actions if a["action"] == "blocked_by_cleaned"]
        self.assertEqual(len(cleanup), 1)
        self.assertIn("missing", cleanup[0]["detail"])

        self.assertEqual(self._db_get_blocked_by(qid), [])


class TestCircularDependencyDetected(DeadlockTestCase):
    """Scenario 3: Circular blocked_by dependency.

    Reproduction: A blocked_by C, B blocked_by A, C blocked_by B.
    All three specs are permanently stuck; none can ever be dequeued
    because each waits for the next in the cycle.

    Fix: self_heal detects the cycle and cancels all specs in it.
    """

    def test_circular_dependency_detected(self):
        specs = []
        for i in range(3):
            path = self._make_spec(tasks_pending=1, filename=f"spec{i}.md")
            specs.append(self.db.enqueue(path))

        a_id, b_id, c_id = specs[0]["id"], specs[1]["id"], specs[2]["id"]

        # Create cycle: A->C, B->A, C->B
        for spec_id, dep in [(a_id, c_id), (b_id, a_id), (c_id, b_id)]:
            self._db_add_dependency(spec_id, dep)

        actions = self_heal(self.queue_dir, {}, db=self.db)

        cycle_actions = [
            a for a in actions if a["action"] == "circular_dependency_canceled"
        ]
        self.assertEqual(len(cycle_actions), 3)

        for spec in specs:
            updated = self.db.get_spec(spec["id"])
            self.assertEqual(updated["status"], "canceled")
            self.assertIn("Circular", updated.get("failure_reason", ""))

    def test_two_node_cycle(self):
        """Even a simple A->B->A cycle should be detected."""
        path_a = self._make_spec(tasks_pending=1, filename="a.md")
        path_b = self._make_spec(tasks_pending=1, filename="b.md")
        a = self.db.enqueue(path_a)
        b = self.db.enqueue(path_b)

        self._db_add_dependency(a["id"], b["id"])
        self._db_add_dependency(b["id"], a["id"])

        actions = self_heal(self.queue_dir, {}, db=self.db)

        cycle_actions = [
            a for a in actions if a["action"] == "circular_dependency_canceled"
        ]
        self.assertEqual(len(cycle_actions), 2)


class TestStaleLockRemoved(DeadlockTestCase):
    """Scenario: Lock file held by a dead process.

    Reproduction: queue/.lock exists with an flock held by a process
    that has since died. All queue operations hang waiting for the lock.

    Fix: self_heal detects the stale lock. (In practice, flock is released
    on process death, so the file is harmless. The test verifies the check
    runs without errors.)
    """

    def test_stale_lock_removed(self):
        """Lock file with no holder should produce no error."""
        lock_path = os.path.join(self.queue_dir, ".lock")
        Path(lock_path).write_text("")

        # self_heal should complete without error
        actions = self_heal(self.queue_dir, {}, db=self.db)

        # No crash = success. Lock file cleanup is handled by flock mechanics.
        # The important thing is self_heal doesn't hang or error.
        self.assertIsInstance(actions, list)

    def test_no_lock_file(self):
        """No lock file should produce no lock-related actions."""
        actions = self_heal(self.queue_dir, {}, db=self.db)
        lock_actions = [a for a in actions if "lock" in a.get("action", "")]
        self.assertEqual(len(lock_actions), 0)


class TestZeroPendingCompletion(DeadlockTestCase):
    """Scenario 2: Worker exits with 0 PENDING tasks but no exit file.

    Reproduction: Worker finds 0 PENDING tasks, exits immediately. No PID
    file is written, no exit file is written. Daemon can't detect that the
    worker even ran. Spec stays in 'running' forever.

    Fix: self_heal detects running spec with dead/missing PID and resets it.
    Additionally, process_worker_completion handles exit_code=None gracefully.
    """

    def test_zero_pending_no_pid(self):
        """Spec with 0 pending + running status + no PID should be recovered."""
        spec_path = self._make_spec(tasks_pending=0, tasks_done=3)
        entry = self.db.enqueue(spec_path)
        qid = entry["id"]
        self._db_set_running(qid, "w-1", pid=None)

        # Worker exited immediately, no PID recorded → dead
        actions = self_heal(self.queue_dir, {"w-1": qid}, db=self.db)

        stale = [a for a in actions if a["action"] == "stale_running_recovered"]
        self.assertEqual(len(stale), 1)

        updated = self.db.get_spec(qid)
        self.assertEqual(updated["status"], "requeued")

    def test_zero_pending_completion_via_process(self):
        """process_worker_completion with exit_code=0 and 0 pending should complete."""
        spec_path = self._make_spec(tasks_pending=0, tasks_done=3)
        entry = self.db.enqueue(spec_path)
        qid = entry["id"]
        self._db_set_running(qid, "w-1")

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=qid,
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "completed")
        updated = self.db.get_spec(qid)
        self.assertEqual(updated["status"], "completed")


class TestCriticReviewHandled(DeadlockTestCase):
    """Scenario 1: critic_review outcome unhandled.

    Reproduction: process_worker_completion returns outcome='critic_review',
    daemon needs to launch a critic worker. If the daemon doesn't handle
    this outcome, spec stays in 'running' forever.

    Fix: process_worker_completion now requeues the spec when triggering
    critic review, and process_critic_completion handles the critic result.
    """

    def test_critic_review_triggers_requeue(self):
        """When critic is enabled and tasks are all done, spec should be
        requeued for critic review (not left in running)."""
        spec_path = self._make_spec(tasks_pending=0, tasks_done=3)
        entry = self.db.enqueue(spec_path)
        qid = entry["id"]
        self._db_set_running(qid, "w-1")

        # Enable critic
        critic_dir = os.path.join(self.boi_state, "critic")
        config_path = os.path.join(critic_dir, "config.json")
        Path(config_path).write_text(
            json.dumps({"enabled": True, "max_passes": 2}, indent=2) + "\n"
        )

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=qid,
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "critic_review")

        # Spec should be requeued, not stuck in running
        updated = self.db.get_spec(qid)
        self.assertEqual(updated["status"], "requeued")

    def test_critic_completion_approved(self):
        """process_critic_completion with approved result should complete spec."""
        # Create a spec where the critic approves (no CRITIC tasks added)
        spec_content = textwrap.dedent("""\
            # Test Spec

            ## Tasks

            ### t-1: Task one
            DONE

            **Spec:** Completed.

            **Verify:** true
        """)
        spec_path = self._make_spec(content=spec_content)
        entry = self.db.enqueue(spec_path)
        qid = entry["id"]
        self._db_set_running(qid, "w-1")

        # Simulate critic writing an approval to spec
        approved_content = spec_content + textwrap.dedent("""\

            ## Critic Review

            **Approved:** Yes
        """)
        # Write to the queue's copy of the spec
        queue_spec_path = entry["spec_path"]
        Path(queue_spec_path).write_text(approved_content, encoding="utf-8")

        result = process_critic_completion(
            queue_dir=self.queue_dir,
            queue_id=qid,
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=queue_spec_path,
            db=self.db,
        )

        self.assertEqual(result["outcome"], "critic_approved")
        updated = self.db.get_spec(qid)
        self.assertEqual(updated["status"], "completed")


class TestMaxRunningDurationExceeded(DeadlockTestCase):
    """Scenario: Spec running longer than max duration.

    Reproduction: PID check is broken (PID file missing, PID reused by
    another process), but the spec has been in 'running' for hours.
    Without the timeout, spec sits in running forever.

    Fix: self_heal checks first_running_at against max_running_duration_seconds
    and force-fails specs that exceed the limit.
    """

    def test_max_running_duration_exceeded(self):
        spec_path = self._make_spec(tasks_pending=2)
        entry = self.db.enqueue(spec_path)
        qid = entry["id"]
        # Use live PID so stale_running doesn't trigger
        self._db_set_running(qid, "w-1", pid=os.getpid())

        # Backdate first_running_at to exceed max duration (60*5=300s, elapsed=600s)
        old_time = (datetime.now(timezone.utc) - timedelta(seconds=600)).isoformat()
        self._db_update_spec(
            qid,
            worker_timeout_seconds=60,
            max_iterations=5,
            first_running_at=old_time,
        )

        # Write live PID file (matching worker.current_pid so stale_running skips it)
        pid_file = os.path.join(self.queue_dir, f"{qid}.pid")
        Path(pid_file).write_text(str(os.getpid()) + "\n")

        actions = self_heal(self.queue_dir, {"w-1": qid}, db=self.db)

        duration_actions = [
            a for a in actions if a["action"] == "max_running_duration_exceeded"
        ]
        self.assertEqual(len(duration_actions), 1)

        updated = self.db.get_spec(qid)
        self.assertEqual(updated["status"], "failed")
        self.assertEqual(updated["failure_reason"], "Maximum running duration exceeded")
        self.assertFalse(os.path.isfile(pid_file))


class TestDeletedSpecFile(DeadlockTestCase):
    """Scenario: Spec file deleted while spec is queued/running.

    Reproduction: The spec.md file is deleted from disk (user error,
    disk cleanup, etc.) while the queue entry still references it.
    Worker tries to read it and fails. Spec gets stuck.

    Fix: process_worker_completion handles missing spec file gracefully
    by treating it as a crash/failure.
    """

    def test_deleted_spec_file(self):
        spec_path = self._make_spec(tasks_pending=2)
        entry = self.db.enqueue(spec_path)
        qid = entry["id"]
        self._db_set_running(qid, "w-1")

        # Delete the spec file from the queue copy
        queue_spec = entry["spec_path"]
        if os.path.isfile(queue_spec):
            os.remove(queue_spec)

        # process_worker_completion with exit_code=0 should handle missing spec
        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=qid,
            exit_code="0",
        )

        # Should complete (0 pending since spec can't be read) or handle gracefully
        # The key assertion: spec is NOT stuck in 'running'
        updated = self.db.get_spec(qid)
        self.assertNotEqual(
            updated["status"],
            "running",
            "Spec should not remain stuck in 'running' when spec file is deleted",
        )


class TestDeletedCheckout(DeadlockTestCase):
    """Scenario: Worker checkout directory deleted mid-iteration.

    Reproduction: The worker's checkout/worktree directory is deleted
    while a spec is running in it. The worker can't find its files.

    Fix: self_heal detects running specs with dead PIDs (the worker
    would crash), resets them to 'requeued' so they can be assigned
    to a different worker with a healthy checkout.
    """

    def test_deleted_checkout(self):
        spec_path = self._make_spec(tasks_pending=2)
        entry = self.db.enqueue(spec_path)
        qid = entry["id"]
        self._db_set_running(qid, "w-1", pid=999999999)

        # Record a checkout path that doesn't exist
        self._db_update_spec(qid, worktree="/tmp/nonexistent-checkout-" + str(os.getpid()))

        actions = self_heal(self.queue_dir, {"w-1": qid}, db=self.db)

        # Should recover via stale running detection (dead PID)
        stale = [a for a in actions if a["action"] == "stale_running_recovered"]
        self.assertEqual(len(stale), 1)

        updated = self.db.get_spec(qid)
        self.assertEqual(updated["status"], "requeued")


class TestMultipleDeadlocksSimultaneous(DeadlockTestCase):
    """Integration test: Multiple deadlock scenarios happening at once.

    Verifies self_heal can handle several stuck states in a single pass.
    """

    def test_multiple_deadlocks_resolved(self):
        # Issue 1: Stale running spec with dead PID
        spec1_path = self._make_spec(tasks_pending=2, filename="spec1.md")
        entry1 = self.db.enqueue(spec1_path)
        self._db_set_running(entry1["id"], "w-1", pid=999999999)

        # Issue 2: Blocked by missing spec (bypass FK to create orphan dep)
        spec2_path = self._make_spec(tasks_pending=1, filename="spec2.md")
        entry2 = self.db.enqueue(spec2_path)
        self._db_add_dependency(entry2["id"], "q-missing")

        # Issue 3: Orphaned worker (spec completed but worker still assigned)
        spec3_path = self._make_spec(tasks_pending=0, tasks_done=1, filename="spec3.md")
        entry3 = self.db.enqueue(spec3_path)
        self._db_set_running(entry3["id"], "w-2")
        self.db.complete(entry3["id"], 1, 1)

        # Issue 4: Circular dependency (A <-> B)
        spec4_path = self._make_spec(tasks_pending=1, filename="spec4.md")
        spec5_path = self._make_spec(tasks_pending=1, filename="spec5.md")
        entry4 = self.db.enqueue(spec4_path)
        entry5 = self.db.enqueue(spec5_path)
        self._db_add_dependency(entry4["id"], entry5["id"])
        self._db_add_dependency(entry5["id"], entry4["id"])

        worker_specs = {
            "w-1": entry1["id"],
            "w-2": entry3["id"],
            "w-3": "",
        }
        actions = self_heal(self.queue_dir, worker_specs, db=self.db)

        action_types = {a["action"] for a in actions}
        self.assertIn("stale_running_recovered", action_types)
        self.assertIn("blocked_by_cleaned", action_types)
        self.assertIn("orphaned_worker", action_types)
        self.assertIn("circular_dependency_canceled", action_types)

        # Verify all issues resolved
        self.assertEqual(self.db.get_spec(entry1["id"])["status"], "requeued")
        self.assertEqual(self._db_get_blocked_by(entry2["id"]), [])
        self.assertEqual(self.db.get_spec(entry4["id"])["status"], "canceled")
        self.assertEqual(self.db.get_spec(entry5["id"])["status"], "canceled")


if __name__ == "__main__":
    unittest.main()
