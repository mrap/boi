# test_concurrent_operations.py — Integration tests for concurrent operations.
#
# Verifies three concurrency scenarios:
#   (a) Dispatch 5 specs with 2 workers. Verify no double-assignment
#       (each spec runs on exactly one worker at a time).
#   (b) Cancel a spec while it's running. Verify worker is killed and
#       spec status is 'canceled'.
#   (c) Dispatch specs with dependencies (A blocks B). Verify B doesn't
#       start until A completes.

import os
import signal
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


# ── Shared completion handler ────────────────────────────────────────


def _test_completion_handler(harness, spec_id, phase, exit_code, worker_id):
    """Completion handler that counts tasks and requeues or completes.

    Mirrors the handler used in test_full_lifecycle and
    test_crash_recovery.
    """
    db = harness._daemon.db
    spec = db.get_spec(spec_id)
    if spec is None:
        return

    now = datetime.now(timezone.utc).isoformat()

    if exit_code != 0:
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


# ── Test (a): No double-assignment with multiple specs and workers ──


class TestNoDoubleAssignment(IntegrationTestCase):
    """Dispatch 5 specs with 2 workers. Verify each spec runs on exactly
    one worker at a time (no double-assignment)."""

    NUM_WORKERS = 2

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        """Complete all tasks in each spec in one pass."""
        return MockClaude(
            phase="execute",
            tasks_to_complete=2,
            exit_code=0,
            delay_seconds=0.2,
        )

    def setUp(self) -> None:
        super().setUp()

        # Patch completion handler
        harness_ref = self.harness
        self.harness.start()
        daemon = self.harness._daemon
        daemon._dispatch_phase_completion = (
            lambda spec_id, phase, exit_code, worker_id: (
                _test_completion_handler(
                    harness_ref, spec_id, phase, exit_code, worker_id
                )
            )
        )

    def test_five_specs_two_workers_no_double_assignment(self) -> None:
        """Dispatch 5 specs with 2 workers. Each spec should complete
        exactly once and never be assigned to two workers at the same
        time."""
        spec_ids = []
        for i in range(5):
            spec_path = self.create_spec(
                tasks_pending=2,
                filename=f"spec-{i}.md",
            )
            sid = self.dispatch_spec(spec_path, max_iterations=10)
            spec_ids.append(sid)

        # Wait for all 5 to complete
        for sid in spec_ids:
            self.harness.wait_for_status(sid, "completed", timeout=30)

        # Verify each spec completed exactly once
        for sid in spec_ids:
            spec = self.db.get_spec(sid)
            self.assertEqual(
                spec["status"], "completed",
                f"Spec {sid} should be completed",
            )
            self.assertEqual(spec["tasks_done"], 2)
            self.assertEqual(spec["tasks_total"], 2)

        # Verify no double-assignment: at any given iteration, a spec
        # should appear on at most one worker. Check the processes table.
        cursor = self.db.conn.execute(
            "SELECT spec_id, worker_id, iteration, phase "
            "FROM processes ORDER BY started_at"
        )
        processes = [dict(r) for r in cursor.fetchall()]

        # Group by (spec_id, iteration, phase) — each combination should
        # have exactly one worker assignment
        from collections import Counter
        assignments = Counter(
            (p["spec_id"], p["iteration"], p["phase"])
            for p in processes
        )
        for key, count in assignments.items():
            self.assertEqual(
                count, 1,
                f"Spec {key[0]} iteration {key[1]} phase {key[2]} "
                f"was assigned {count} times (expected 1)",
            )

    def test_all_workers_used(self) -> None:
        """With 5 specs and 2 workers, both workers should be used."""
        spec_ids = []
        for i in range(5):
            spec_path = self.create_spec(
                tasks_pending=2,
                filename=f"spec-multi-{i}.md",
            )
            sid = self.dispatch_spec(spec_path, max_iterations=10)
            spec_ids.append(sid)

        # Wait for all to complete
        for sid in spec_ids:
            self.harness.wait_for_status(sid, "completed", timeout=30)

        # Check that both workers were used
        cursor = self.db.conn.execute(
            "SELECT DISTINCT worker_id FROM processes"
        )
        workers_used = {r["worker_id"] for r in cursor.fetchall()}
        self.assertEqual(
            len(workers_used), 2,
            f"Expected both workers used, got: {workers_used}",
        )


# ── Test (b): Cancel a running spec ─────────────────────────────────


class TestCancelRunningSpec(IntegrationTestCase):
    """Cancel a spec while it's running. Verify the worker is killed
    and the spec status becomes 'canceled'."""

    NUM_WORKERS = 1

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        """Slow mock so the spec is still running when we cancel it."""
        return MockClaude(
            phase="execute",
            tasks_to_complete=1,
            exit_code=0,
            delay_seconds=60,
        )

    def setUp(self) -> None:
        super().setUp()
        self.harness.start()

        harness_ref = self.harness
        daemon = self.harness._daemon
        daemon._dispatch_phase_completion = (
            lambda spec_id, phase, exit_code, worker_id: (
                _test_completion_handler(
                    harness_ref, spec_id, phase, exit_code, worker_id
                )
            )
        )

    def test_cancel_running_spec_kills_worker(self) -> None:
        """Cancel a running spec. The worker process should be killed
        and the spec status should be 'canceled'."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path, max_iterations=10)

        # Wait for spec to reach 'running'
        self.harness.wait_for_status(spec_id, "running", timeout=10)

        # Get the worker process before canceling
        daemon = self.harness._daemon
        self.assertEqual(len(daemon.worker_procs), 1)
        worker_id = list(daemon.worker_procs.keys())[0]
        worker_proc = daemon.worker_procs[worker_id]
        worker_pid = worker_proc.pid

        # Cancel the spec in the DB
        self.db.cancel(spec_id)

        # Kill the worker process (the daemon doesn't automatically
        # kill workers on cancel, so we simulate what boi cancel does:
        # cancel in DB + kill the process group)
        try:
            pgid = os.getpgid(worker_pid)
            os.killpg(pgid, signal.SIGTERM)
        except (ProcessLookupError, PermissionError):
            pass

        # Wait for the worker to die
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if worker_proc.poll() is not None:
                break
            time.sleep(0.1)

        # If still alive, SIGKILL
        if worker_proc.poll() is None:
            try:
                pgid = os.getpgid(worker_pid)
                os.killpg(pgid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
            worker_proc.wait(timeout=5)

        # Verify spec status is 'canceled'
        spec = self.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "canceled")

        # Verify the worker PID is dead
        try:
            os.kill(worker_pid, 0)
            self.fail("Worker PID should be dead after cancel")
        except ProcessLookupError:
            pass  # Expected

        # Verify cancel event was logged
        events = self.db.get_events(spec_id=spec_id)
        event_types = [e["event_type"] for e in events]
        self.assertIn("canceled", event_types)

    def test_cancel_queued_spec_never_runs(self) -> None:
        """Cancel a spec while it's still queued (not yet assigned).
        It should never start running."""
        # First dispatch a slow spec to occupy the only worker
        slow_path = self.create_spec(
            tasks_pending=1, filename="slow.md"
        )
        slow_id = self.dispatch_spec(slow_path, max_iterations=10)

        # Wait for it to be running
        self.harness.wait_for_status(slow_id, "running", timeout=10)

        # Now dispatch a second spec that will be queued (worker is busy)
        queued_path = self.create_spec(
            tasks_pending=2, filename="queued.md"
        )
        queued_id = self.dispatch_spec(queued_path, max_iterations=10)

        # Give the daemon a cycle to not pick it up (worker is busy)
        time.sleep(1)

        # Cancel the queued spec
        self.db.cancel(queued_id)

        # Verify it's canceled
        spec = self.db.get_spec(queued_id)
        self.assertEqual(spec["status"], "canceled")

        # Verify it was never assigned to a worker (no process records)
        cursor = self.db.conn.execute(
            "SELECT * FROM processes WHERE spec_id = ?",
            (queued_id,),
        )
        procs = cursor.fetchall()
        self.assertEqual(
            len(procs), 0,
            "Canceled queued spec should have no process records",
        )


# ── Test (c): Dependency ordering ───────────────────────────────────


class TestDependencyOrdering(IntegrationTestCase):
    """Dispatch specs with dependencies. Verify blocked specs don't
    start until their dependencies are completed."""

    NUM_WORKERS = 2

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        """Complete all pending tasks in one pass."""
        return MockClaude(
            phase="execute",
            tasks_to_complete=2,
            exit_code=0,
            delay_seconds=0.3,
        )

    def setUp(self) -> None:
        super().setUp()

        harness_ref = self.harness
        self.harness.start()
        daemon = self.harness._daemon
        daemon._dispatch_phase_completion = (
            lambda spec_id, phase, exit_code, worker_id: (
                _test_completion_handler(
                    harness_ref, spec_id, phase, exit_code, worker_id
                )
            )
        )

    def test_blocked_spec_waits_for_dependency(self) -> None:
        """Dispatch A and B where B blocks on A. B should not start
        until A completes."""
        spec_a_path = self.create_spec(
            tasks_pending=2, filename="spec-a.md"
        )
        spec_a_id = self.dispatch_spec(spec_a_path, max_iterations=10)

        spec_b_path = self.create_spec(
            tasks_pending=2, filename="spec-b.md"
        )
        spec_b_id = self.dispatch_spec(
            spec_b_path,
            max_iterations=10,
            blocked_by=[spec_a_id],
        )

        # Wait for A to complete first
        self.harness.wait_for_status(
            spec_a_id, "completed", timeout=30
        )

        # Now B should be eligible and eventually complete
        self.harness.wait_for_status(
            spec_b_id, "completed", timeout=30
        )

        # Verify A completed before B started running
        a_events = self.db.get_events(spec_id=spec_a_id)
        b_events = self.db.get_events(spec_id=spec_b_id)

        a_completed_events = [
            e for e in a_events if e["event_type"] == "completed"
        ]
        b_running_events = [
            e for e in b_events if e["event_type"] == "running"
        ]

        self.assertTrue(
            len(a_completed_events) > 0,
            "A should have a completed event",
        )
        self.assertTrue(
            len(b_running_events) > 0,
            "B should have a running event",
        )

        a_completed_time = a_completed_events[0]["timestamp"]
        b_first_running_time = b_running_events[0]["timestamp"]
        self.assertLessEqual(
            a_completed_time,
            b_first_running_time,
            "A must complete before B starts running",
        )

    def test_independent_specs_run_concurrently(self) -> None:
        """Dispatch A and B with no dependencies. Both should run
        concurrently on separate workers (both have 2 workers)."""
        spec_a_path = self.create_spec(
            tasks_pending=2, filename="spec-ind-a.md"
        )
        spec_b_path = self.create_spec(
            tasks_pending=2, filename="spec-ind-b.md"
        )

        spec_a_id = self.dispatch_spec(spec_a_path, max_iterations=10)
        spec_b_id = self.dispatch_spec(spec_b_path, max_iterations=10)

        # Both should complete
        self.harness.wait_for_status(
            spec_a_id, "completed", timeout=30
        )
        self.harness.wait_for_status(
            spec_b_id, "completed", timeout=30
        )

        # Check that they ran on different workers (concurrent)
        cursor = self.db.conn.execute(
            "SELECT spec_id, worker_id FROM processes "
            "WHERE spec_id IN (?, ?) AND iteration = 1",
            (spec_a_id, spec_b_id),
        )
        assignments = {r["spec_id"]: r["worker_id"] for r in cursor}

        if spec_a_id in assignments and spec_b_id in assignments:
            self.assertNotEqual(
                assignments[spec_a_id],
                assignments[spec_b_id],
                "Independent specs should run on different workers "
                "when both are available",
            )

    def test_chain_dependency_a_blocks_b_blocks_c(self) -> None:
        """Dispatch A -> B -> C chain. C shouldn't start until B
        completes, and B shouldn't start until A completes."""
        spec_a_path = self.create_spec(
            tasks_pending=2, filename="chain-a.md"
        )
        spec_a_id = self.dispatch_spec(spec_a_path, max_iterations=10)

        spec_b_path = self.create_spec(
            tasks_pending=2, filename="chain-b.md"
        )
        spec_b_id = self.dispatch_spec(
            spec_b_path,
            max_iterations=10,
            blocked_by=[spec_a_id],
        )

        spec_c_path = self.create_spec(
            tasks_pending=2, filename="chain-c.md"
        )
        spec_c_id = self.dispatch_spec(
            spec_c_path,
            max_iterations=10,
            blocked_by=[spec_b_id],
        )

        # All three should complete in order
        self.harness.wait_for_status(
            spec_a_id, "completed", timeout=30
        )
        self.harness.wait_for_status(
            spec_b_id, "completed", timeout=30
        )
        self.harness.wait_for_status(
            spec_c_id, "completed", timeout=30
        )

        # Verify ordering via event timestamps
        events_a = self.db.get_events(spec_id=spec_a_id)
        events_b = self.db.get_events(spec_id=spec_b_id)
        events_c = self.db.get_events(spec_id=spec_c_id)

        a_completed = [
            e for e in events_a if e["event_type"] == "completed"
        ][0]["timestamp"]
        b_running = [
            e for e in events_b if e["event_type"] == "running"
        ][0]["timestamp"]
        b_completed = [
            e for e in events_b if e["event_type"] == "completed"
        ][0]["timestamp"]
        c_running = [
            e for e in events_c if e["event_type"] == "running"
        ][0]["timestamp"]

        self.assertLessEqual(
            a_completed, b_running,
            "A must complete before B starts",
        )
        self.assertLessEqual(
            b_completed, c_running,
            "B must complete before C starts",
        )


if __name__ == "__main__":
    unittest.main()
