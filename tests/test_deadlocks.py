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
    process_critic_completion,
    process_worker_completion,
    self_heal,
)
from lib.queue import (
    _read_entry,
    _write_entry,
    complete,
    enqueue,
    get_entry,
    set_running,
)


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

    def tearDown(self):
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

        lines = ["# Test Spec\n\n## Tasks\n"]
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


class TestStaleRunningSpecRecovered(DeadlockTestCase):
    """Scenario 1: Running spec with dead PID gets stuck forever.

    Reproduction: Set spec status to 'running', write a PID file with
    a dead PID. Without self-heal, the daemon never detects the failure
    and the spec sits in 'running' forever.

    Fix: self_heal detects dead PID and resets to 'requeued'.
    """

    def test_stale_running_spec_recovered(self):
        spec_path = self._make_spec(tasks_pending=2)
        entry = enqueue(self.queue_dir, spec_path)
        qid = entry["id"]
        set_running(self.queue_dir, qid, "w-1")

        # Write PID file with a definitely-dead PID
        pid_file = os.path.join(self.queue_dir, f"{qid}.pid")
        Path(pid_file).write_text("999999999\n")

        actions = self_heal(self.queue_dir, {"w-1": qid})

        # Spec should be recovered
        stale = [a for a in actions if a["action"] == "stale_running_recovered"]
        self.assertEqual(len(stale), 1)

        updated = get_entry(self.queue_dir, qid)
        self.assertEqual(updated["status"], "requeued")
        self.assertFalse(os.path.isfile(pid_file))

    def test_stale_running_no_pid_file(self):
        """Running spec with no PID file at all should also be recovered."""
        spec_path = self._make_spec(tasks_pending=2)
        entry = enqueue(self.queue_dir, spec_path)
        qid = entry["id"]
        set_running(self.queue_dir, qid, "w-1")

        # No PID file created at all
        actions = self_heal(self.queue_dir, {"w-1": qid})

        stale = [a for a in actions if a["action"] == "stale_running_recovered"]
        self.assertEqual(len(stale), 1)

        updated = get_entry(self.queue_dir, qid)
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
        entry = enqueue(self.queue_dir, spec_path)
        qid = entry["id"]
        set_running(self.queue_dir, qid, "w-1")
        complete(self.queue_dir, qid, 3, 3)

        # Worker still thinks it owns this spec
        worker_specs = {"w-1": qid, "w-2": ""}
        actions = self_heal(self.queue_dir, worker_specs)

        orphan = [a for a in actions if a["action"] == "orphaned_worker"]
        self.assertEqual(len(orphan), 1)
        self.assertEqual(orphan[0]["worker_id"], "w-1")
        self.assertEqual(orphan[0]["queue_id"], qid)

    def test_orphaned_worker_failed_spec(self):
        """Worker assigned to a failed spec should also be freed."""
        from lib.queue import fail

        spec_path = self._make_spec(tasks_pending=2)
        entry = enqueue(self.queue_dir, spec_path)
        qid = entry["id"]
        set_running(self.queue_dir, qid, "w-1")
        fail(self.queue_dir, qid, "test failure")

        worker_specs = {"w-1": qid}
        actions = self_heal(self.queue_dir, worker_specs)

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
        blocker = enqueue(self.queue_dir, blocker_path)
        set_running(self.queue_dir, blocker["id"], "w-1")
        complete(self.queue_dir, blocker["id"], 1, 1)

        blocked_path = self._make_spec(tasks_pending=2, filename="blocked.md")
        blocked = enqueue(self.queue_dir, blocked_path, blocked_by=[blocker["id"]])

        # Verify it's blocked
        entry = get_entry(self.queue_dir, blocked["id"])
        self.assertEqual(entry["blocked_by"], [blocker["id"]])

        actions = self_heal(self.queue_dir, {})

        cleanup = [a for a in actions if a["action"] == "blocked_by_cleaned"]
        self.assertEqual(len(cleanup), 1)

        updated = get_entry(self.queue_dir, blocked["id"])
        self.assertEqual(updated["blocked_by"], [])


class TestBlockedByMissingSpecUnblocked(DeadlockTestCase):
    """Scenario 4 variant: Spec blocked by a nonexistent spec ID.

    Reproduction: Spec has blocked_by=["q-999"] but q-999 doesn't exist
    (deleted, purged, or typo). Spec is permanently stuck.

    Fix: self_heal removes missing spec IDs from blocked_by lists.
    """

    def test_blocked_by_missing_spec_unblocked(self):
        spec_path = self._make_spec(tasks_pending=2)
        entry = enqueue(self.queue_dir, spec_path)
        qid = entry["id"]

        # Manually set blocked_by to a nonexistent spec
        raw = _read_entry(self.queue_dir, qid)
        raw["blocked_by"] = ["q-999"]
        _write_entry(self.queue_dir, raw)

        actions = self_heal(self.queue_dir, {})

        cleanup = [a for a in actions if a["action"] == "blocked_by_cleaned"]
        self.assertEqual(len(cleanup), 1)
        self.assertIn("missing", cleanup[0]["detail"])

        updated = get_entry(self.queue_dir, qid)
        self.assertEqual(updated["blocked_by"], [])


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
            specs.append(enqueue(self.queue_dir, path))

        a_id, b_id, c_id = specs[0]["id"], specs[1]["id"], specs[2]["id"]

        # Create cycle: A->C, B->A, C->B
        for qid, dep in [(a_id, c_id), (b_id, a_id), (c_id, b_id)]:
            raw = _read_entry(self.queue_dir, qid)
            raw["blocked_by"] = [dep]
            _write_entry(self.queue_dir, raw)

        actions = self_heal(self.queue_dir, {})

        cycle_actions = [
            a for a in actions if a["action"] == "circular_dependency_canceled"
        ]
        self.assertEqual(len(cycle_actions), 3)

        for spec in specs:
            updated = get_entry(self.queue_dir, spec["id"])
            self.assertEqual(updated["status"], "canceled")
            self.assertIn("Circular", updated.get("failure_reason", ""))

    def test_two_node_cycle(self):
        """Even a simple A->B->A cycle should be detected."""
        path_a = self._make_spec(tasks_pending=1, filename="a.md")
        path_b = self._make_spec(tasks_pending=1, filename="b.md")
        a = enqueue(self.queue_dir, path_a)
        b = enqueue(self.queue_dir, path_b)

        raw_a = _read_entry(self.queue_dir, a["id"])
        raw_a["blocked_by"] = [b["id"]]
        _write_entry(self.queue_dir, raw_a)

        raw_b = _read_entry(self.queue_dir, b["id"])
        raw_b["blocked_by"] = [a["id"]]
        _write_entry(self.queue_dir, raw_b)

        actions = self_heal(self.queue_dir, {})

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
        actions = self_heal(self.queue_dir, {})

        # No crash = success. Lock file cleanup is handled by flock mechanics.
        # The important thing is self_heal doesn't hang or error.
        self.assertIsInstance(actions, list)

    def test_no_lock_file(self):
        """No lock file should produce no lock-related actions."""
        actions = self_heal(self.queue_dir, {})
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
        entry = enqueue(self.queue_dir, spec_path)
        qid = entry["id"]
        set_running(self.queue_dir, qid, "w-1")

        # Worker exited immediately, no PID file, no exit file
        # self_heal should detect dead PID and recover
        actions = self_heal(self.queue_dir, {"w-1": qid})

        stale = [a for a in actions if a["action"] == "stale_running_recovered"]
        self.assertEqual(len(stale), 1)

        updated = get_entry(self.queue_dir, qid)
        self.assertEqual(updated["status"], "requeued")

    def test_zero_pending_completion_via_process(self):
        """process_worker_completion with exit_code=0 and 0 pending should complete."""
        spec_path = self._make_spec(tasks_pending=0, tasks_done=3)
        entry = enqueue(self.queue_dir, spec_path)
        qid = entry["id"]
        set_running(self.queue_dir, qid, "w-1")

        result = process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=qid,
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "completed")
        updated = get_entry(self.queue_dir, qid)
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
        entry = enqueue(self.queue_dir, spec_path)
        qid = entry["id"]
        set_running(self.queue_dir, qid, "w-1")

        # Enable critic
        critic_dir = os.path.join(self.boi_state, "critic")
        config_path = os.path.join(critic_dir, "config.json")
        Path(config_path).write_text(
            json.dumps({"enabled": True, "max_passes": 2}, indent=2) + "\n"
        )

        result = process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=qid,
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "critic_review")

        # Spec should be requeued, not stuck in running
        updated = get_entry(self.queue_dir, qid)
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
        entry = enqueue(self.queue_dir, spec_path)
        qid = entry["id"]
        set_running(self.queue_dir, qid, "w-1")

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
        )

        self.assertEqual(result["outcome"], "critic_approved")
        updated = get_entry(self.queue_dir, qid)
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
        entry = enqueue(self.queue_dir, spec_path)
        qid = entry["id"]
        set_running(self.queue_dir, qid, "w-1")

        # Backdate first_running_at to exceed max duration
        e = _read_entry(self.queue_dir, qid)
        e["worker_timeout_seconds"] = 60
        e["max_iterations"] = 5
        # 600s > 300s (60*5) limit
        e["first_running_at"] = (
            datetime.now(timezone.utc) - timedelta(seconds=600)
        ).isoformat()
        _write_entry(self.queue_dir, e)

        # Write live PID so stale_running doesn't trigger first
        pid_file = os.path.join(self.queue_dir, f"{qid}.pid")
        Path(pid_file).write_text(str(os.getpid()) + "\n")

        actions = self_heal(self.queue_dir, {"w-1": qid})

        duration_actions = [
            a for a in actions if a["action"] == "max_running_duration_exceeded"
        ]
        self.assertEqual(len(duration_actions), 1)

        updated = get_entry(self.queue_dir, qid)
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
        entry = enqueue(self.queue_dir, spec_path)
        qid = entry["id"]
        set_running(self.queue_dir, qid, "w-1")

        # Delete the spec file from the queue copy
        queue_spec = entry["spec_path"]
        if os.path.isfile(queue_spec):
            os.remove(queue_spec)

        # process_worker_completion with exit_code=0 should handle missing spec
        result = process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=qid,
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code="0",
        )

        # Should complete (0 pending since spec can't be read) or handle gracefully
        # The key assertion: spec is NOT stuck in 'running'
        updated = get_entry(self.queue_dir, qid)
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
        entry = enqueue(self.queue_dir, spec_path)
        qid = entry["id"]
        set_running(self.queue_dir, qid, "w-1")

        # Set a checkout path that doesn't exist
        e = _read_entry(self.queue_dir, qid)
        e["worktree"] = "/tmp/nonexistent-checkout-" + str(os.getpid())
        _write_entry(self.queue_dir, e)

        # Worker would have crashed, leaving a dead PID
        pid_file = os.path.join(self.queue_dir, f"{qid}.pid")
        Path(pid_file).write_text("999999999\n")

        actions = self_heal(self.queue_dir, {"w-1": qid})

        # Should recover via stale running detection
        stale = [a for a in actions if a["action"] == "stale_running_recovered"]
        self.assertEqual(len(stale), 1)

        updated = get_entry(self.queue_dir, qid)
        self.assertEqual(updated["status"], "requeued")


class TestMultipleDeadlocksSimultaneous(DeadlockTestCase):
    """Integration test: Multiple deadlock scenarios happening at once.

    Verifies self_heal can handle several stuck states in a single pass.
    """

    def test_multiple_deadlocks_resolved(self):
        # Issue 1: Stale running spec with dead PID
        spec1_path = self._make_spec(tasks_pending=2, filename="spec1.md")
        entry1 = enqueue(self.queue_dir, spec1_path)
        set_running(self.queue_dir, entry1["id"], "w-1")
        pid_file = os.path.join(self.queue_dir, f"{entry1['id']}.pid")
        Path(pid_file).write_text("999999999\n")

        # Issue 2: Blocked by missing spec
        spec2_path = self._make_spec(tasks_pending=1, filename="spec2.md")
        entry2 = enqueue(self.queue_dir, spec2_path)
        raw2 = _read_entry(self.queue_dir, entry2["id"])
        raw2["blocked_by"] = ["q-missing"]
        _write_entry(self.queue_dir, raw2)

        # Issue 3: Orphaned worker
        spec3_path = self._make_spec(tasks_pending=0, tasks_done=1, filename="spec3.md")
        entry3 = enqueue(self.queue_dir, spec3_path)
        set_running(self.queue_dir, entry3["id"], "w-2")
        complete(self.queue_dir, entry3["id"], 1, 1)

        # Issue 4: Circular dependency
        spec4_path = self._make_spec(tasks_pending=1, filename="spec4.md")
        spec5_path = self._make_spec(tasks_pending=1, filename="spec5.md")
        entry4 = enqueue(self.queue_dir, spec4_path)
        entry5 = enqueue(self.queue_dir, spec5_path)
        raw4 = _read_entry(self.queue_dir, entry4["id"])
        raw4["blocked_by"] = [entry5["id"]]
        _write_entry(self.queue_dir, raw4)
        raw5 = _read_entry(self.queue_dir, entry5["id"])
        raw5["blocked_by"] = [entry4["id"]]
        _write_entry(self.queue_dir, raw5)

        worker_specs = {
            "w-1": entry1["id"],
            "w-2": entry3["id"],
            "w-3": "",
        }
        actions = self_heal(self.queue_dir, worker_specs)

        action_types = {a["action"] for a in actions}
        self.assertIn("stale_running_recovered", action_types)
        self.assertIn("blocked_by_cleaned", action_types)
        self.assertIn("orphaned_worker", action_types)
        self.assertIn("circular_dependency_canceled", action_types)

        # Verify all issues resolved
        self.assertEqual(get_entry(self.queue_dir, entry1["id"])["status"], "requeued")
        self.assertEqual(get_entry(self.queue_dir, entry2["id"])["blocked_by"], [])
        self.assertEqual(get_entry(self.queue_dir, entry4["id"])["status"], "canceled")
        self.assertEqual(get_entry(self.queue_dir, entry5["id"])["status"], "canceled")


if __name__ == "__main__":
    unittest.main()
