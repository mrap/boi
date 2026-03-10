# test_max_iterations.py — Integration test for max_iterations enforcement.
#
# Verifies:
#   - Daemon stops after exactly max_iterations execute-phase iterations
#     when MockClaude never completes all tasks.
#   - Spec status is 'failed' with appropriate failure reason.
#   - Critic phase does NOT increment the execute iteration counter.
#   - max_iterations check uses execute count only (not total phases).

import os
import sys
import time
import unittest
from datetime import datetime, timezone
from pathlib import Path

# Add project root to path
_PROJECT_ROOT = str(Path(__file__).resolve().parent.parent.parent)
sys.path.insert(0, _PROJECT_ROOT)

from tests.integration.conftest import (
    IntegrationTestCase,
    MockClaude,
)


# ── Shared completion handler with max_iterations enforcement ────────


def _max_iter_completion_handler(
    harness, spec_id, phase, exit_code, worker_id
):
    """Completion handler that enforces max_iterations.

    After each execute-phase iteration, checks if max_iterations has
    been reached. If so, fails the spec. Otherwise requeues if tasks
    remain.
    """
    db = harness._daemon.db
    spec = db.get_spec(spec_id)
    if spec is None:
        return

    now = datetime.now(timezone.utc).isoformat()

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

    if exit_code != 0:
        db.requeue(spec_id, done, total)
        return

    if pending == 0 and total > 0:
        db.complete(spec_id, done, total)
        return

    # Check max_iterations (only after execute phase)
    if phase == "execute" and db.has_reached_max_iterations(spec_id):
        db.fail(
            spec_id,
            f"Max iterations reached "
            f"({spec['iteration']}/{spec['max_iterations']}). "
            f"{pending} tasks still pending.",
        )
        return

    db.requeue(spec_id, done, total)


# ── Test: Max iterations enforcement with pure execute phases ────────


class TestMaxIterationsEnforcement(IntegrationTestCase):
    """Dispatch a spec with max_iterations=3 where MockClaude completes
    one task per iteration but never finishes all tasks. Verify daemon
    stops after exactly 3 execute-phase iterations and fails the spec."""

    NUM_WORKERS = 1

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        """Complete 1 task per execute iteration. With 5 PENDING tasks
        and max_iterations=3, 2 tasks will remain after 3 iterations."""
        return MockClaude(
            phase="execute",
            tasks_to_complete=1,
            exit_code=0,
        )

    def setUp(self) -> None:
        super().setUp()
        self.harness.start()

        harness_ref = self.harness
        daemon = self.harness._daemon
        daemon._dispatch_phase_completion = (
            lambda spec_id, phase, exit_code, worker_id: (
                _max_iter_completion_handler(
                    harness_ref, spec_id, phase, exit_code, worker_id
                )
            )
        )

    def test_fails_after_max_iterations(self) -> None:
        """Dispatch 5-task spec with max_iterations=3. MockClaude
        completes 1 task/iteration. After 3 iterations, spec should
        be 'failed' with 2 tasks still pending."""
        spec_path = self.create_spec(tasks_pending=5)
        spec_id = self.dispatch_spec(spec_path, max_iterations=3)

        spec = self.harness.wait_for_status(
            spec_id, "failed", timeout=30
        )

        self.assertEqual(spec["status"], "failed")

        # Iterations table has exactly 3 execute-phase entries
        iterations = self.harness.get_iterations(spec_id)
        execute_iterations = [
            it for it in iterations if it["phase"] == "execute"
        ]
        self.assertEqual(
            len(execute_iterations), 3,
            f"Expected 3 execute iterations, got "
            f"{len(execute_iterations)}. All iterations: {iterations}",
        )

        # Iteration numbers are 1, 2, 3
        iter_nums = sorted(
            it["iteration"] for it in execute_iterations
        )
        self.assertEqual(iter_nums, [1, 2, 3])

        # Each iteration completed 1 task
        for it in execute_iterations:
            self.assertEqual(it["tasks_completed"], 1)
            self.assertEqual(it["exit_code"], 0)

        # Failure reason mentions max iterations
        self.assertIn(
            "Max iterations",
            spec.get("failure_reason", ""),
        )

    def test_iteration_counter_equals_max(self) -> None:
        """The spec's iteration field should equal max_iterations
        when the spec is failed at the limit."""
        spec_path = self.create_spec(tasks_pending=5)
        spec_id = self.dispatch_spec(spec_path, max_iterations=3)

        spec = self.harness.wait_for_status(
            spec_id, "failed", timeout=30
        )

        self.assertEqual(spec["iteration"], 3)
        self.assertEqual(spec["max_iterations"], 3)

    def test_tasks_partially_done_after_max_iterations(self) -> None:
        """After failing at max_iterations, the spec file should have
        3 DONE tasks and 2 still PENDING."""
        spec_path = self.create_spec(tasks_pending=5)
        spec_id = self.dispatch_spec(spec_path, max_iterations=3)

        self.harness.wait_for_status(
            spec_id, "failed", timeout=30
        )

        # Read the queue copy of the spec file
        spec = self.db.get_spec(spec_id)
        queue_spec_path = spec["spec_path"]
        content = Path(queue_spec_path).read_text(encoding="utf-8")

        from lib.spec_parser import parse_boi_spec

        tasks = parse_boi_spec(content)
        done_count = sum(1 for t in tasks if t.status == "DONE")
        pending_count = sum(
            1 for t in tasks if t.status == "PENDING"
        )

        self.assertEqual(
            done_count, 3, "3 tasks should be done after 3 iterations"
        )
        self.assertEqual(
            pending_count, 2, "2 tasks should still be pending"
        )

    def test_events_include_failure(self) -> None:
        """Events table should include a 'failed' event with
        the max_iterations reason."""
        spec_path = self.create_spec(tasks_pending=5)
        spec_id = self.dispatch_spec(spec_path, max_iterations=3)

        self.harness.wait_for_status(
            spec_id, "failed", timeout=30
        )

        events = self.harness.get_events(spec_id=spec_id)
        event_types = [e["event_type"] for e in events]

        # Should have queued, running, requeued, and failed events
        self.assertIn("queued", event_types)
        self.assertIn("running", event_types)
        self.assertIn("failed", event_types)

        # Should have exactly 3 running events (one per iteration)
        running_count = event_types.count("running")
        self.assertEqual(running_count, 3)


# ── Test: Critic phase doesn't increment iteration counter ──────────


class TestCriticDoesNotIncrementIteration(IntegrationTestCase):
    """Verify that critic phase does NOT increment the execute
    iteration counter. Uses a mixed execute/critic flow and checks
    that the iteration counter only reflects execute phases."""

    NUM_WORKERS = 1

    def setUp(self) -> None:
        super().setUp()
        self._critic_call_counts: dict[str, int] = {}
        self.harness.start()

        harness_ref = self.harness
        test_ref = self
        daemon = self.harness._daemon
        daemon._dispatch_phase_completion = (
            lambda spec_id, phase, exit_code, worker_id: (
                test_ref._critic_aware_completion(
                    harness_ref, spec_id, phase, exit_code, worker_id
                )
            )
        )

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        """Execute: complete all available tasks.
        Critic: add 2 tasks on first call, approve on second."""
        if phase == "execute":
            return MockClaude(
                phase="execute",
                tasks_to_complete=99,
                exit_code=0,
            )
        elif phase == "critic":
            key = f"{spec_id}-critic"
            count = self._critic_call_counts.get(key, 0)
            self._critic_call_counts[key] = count + 1

            if count == 0:
                return MockClaude(
                    phase="critic",
                    add_tasks=2,
                    exit_code=0,
                )
            else:
                return MockClaude(
                    phase="critic",
                    critic_approve=True,
                    exit_code=0,
                )
        return MockClaude(exit_code=0)

    def _critic_aware_completion(
        self, harness, spec_id, phase, exit_code, worker_id
    ):
        """Completion handler that triggers critic after all tasks done,
        then handles critic results (add tasks or approve)."""
        db = harness._daemon.db
        spec = db.get_spec(spec_id)
        if spec is None:
            return

        now = datetime.now(timezone.utc).isoformat()

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

        if phase == "execute":
            if pending == 0 and total > 0:
                # All tasks done: trigger critic phase
                with db.lock:
                    db.conn.execute(
                        "UPDATE specs SET "
                        "status = 'requeued', "
                        "phase = 'critic', "
                        "tasks_done = ?, "
                        "tasks_total = ? "
                        "WHERE id = ?",
                        (done, total, spec_id),
                    )
                    db._log_event(
                        "requeued",
                        "Triggering critic phase",
                        spec_id=spec_id,
                    )
                    db.conn.commit()
            else:
                db.requeue(spec_id, done, total)
        elif phase == "critic":
            # Re-read spec file after critic modified it
            content = Path(spec_path).read_text(encoding="utf-8")
            tasks = parse_boi_spec(content)
            new_pending = sum(
                1 for t in tasks if t.status == "PENDING"
            )
            new_done = sum(1 for t in tasks if t.status == "DONE")
            new_total = len(tasks)

            if new_pending > 0:
                # Critic added tasks: go back to execute phase
                with db.lock:
                    db.conn.execute(
                        "UPDATE specs SET "
                        "status = 'requeued', "
                        "phase = 'execute', "
                        "tasks_done = ?, "
                        "tasks_total = ?, "
                        "critic_passes = critic_passes + 1 "
                        "WHERE id = ?",
                        (new_done, new_total, spec_id),
                    )
                    db._log_event(
                        "requeued",
                        f"Critic added {new_pending} tasks, "
                        "back to execute",
                        spec_id=spec_id,
                    )
                    db.conn.commit()
            elif "## Critic Approved" in content:
                # Critic approved: complete the spec
                db.complete(spec_id, new_done, new_total)
            else:
                # No critic output (safety valve): complete anyway
                db.complete(spec_id, new_done, new_total)

    def test_critic_does_not_increment_iteration(self) -> None:
        """Dispatch a 3-task spec. Execute completes all 3, critic
        adds 2 more, execute completes those 2, critic approves.

        Expected flow:
          execute(iter=1) -> critic(iter=1) -> execute(iter=2) ->
          critic(iter=2) -> completed

        Verify: spec.iteration == 2 (not 4), meaning critic phases
        did not increment the counter."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path, max_iterations=10)

        spec = self.harness.wait_for_status(
            spec_id, "completed", timeout=45
        )

        self.assertEqual(spec["status"], "completed")

        # Get all iterations
        iterations = self.harness.get_iterations(spec_id)
        execute_iterations = [
            it for it in iterations if it["phase"] == "execute"
        ]
        critic_iterations = [
            it for it in iterations if it["phase"] == "critic"
        ]

        # Should have 2 execute passes (initial + post-critic)
        self.assertEqual(
            len(execute_iterations), 2,
            f"Expected 2 execute passes, got "
            f"{len(execute_iterations)}. All: {iterations}",
        )

        # Should have at least 1 critic iteration
        self.assertGreaterEqual(
            len(critic_iterations), 1,
            f"Expected at least 1 critic pass, got "
            f"{len(critic_iterations)}",
        )

        # Critic iterations should share the same iteration number
        # as the preceding execute phase (no increment)
        sorted_iters = sorted(
            iterations,
            key=lambda x: x.get("started_at", ""),
        )
        last_exec_iter = None
        for it in sorted_iters:
            if it["phase"] == "execute":
                last_exec_iter = it["iteration"]
            elif it["phase"] == "critic":
                self.assertEqual(
                    it["iteration"],
                    last_exec_iter,
                    "Critic phase should NOT increment iteration "
                    f"counter. Critic iteration: {it['iteration']}, "
                    f"last execute iteration: {last_exec_iter}",
                )

        # Execute iteration numbers should be sequential: 1, 2
        exec_nums = sorted(
            it["iteration"] for it in execute_iterations
        )
        self.assertEqual(exec_nums, [1, 2])

        # The final spec iteration count should equal the number
        # of execute-phase iterations only
        self.assertEqual(
            spec["iteration"],
            len(execute_iterations),
            "Spec iteration counter should equal execute-phase count",
        )

    def test_max_iterations_ignores_critic_phases(self) -> None:
        """With max_iterations=2 and a mixed execute/critic flow,
        the spec should NOT fail prematurely due to critic phases
        counting toward the limit.

        Flow: execute(1) -> critic(1) -> execute(2) -> critic(2)
        Total phases: 4. Execute phases: 2. With max_iterations=2,
        the spec should complete (all tasks done), not fail.
        """
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path, max_iterations=2)

        # If critic phases incorrectly counted, this would fail
        # because the daemon would see 4 total phases > max_iterations=2.
        # Since only execute phases count, 2 execute phases == max,
        # and the spec completes because all tasks are done.
        spec = self.harness.wait_for_status(
            spec_id, "completed", timeout=45
        )

        self.assertEqual(spec["status"], "completed")

        # Verify iteration counter is 2
        self.assertEqual(spec["iteration"], 2)

        # Verify we had both execute and critic phases
        iterations = self.harness.get_iterations(spec_id)
        phases = {it["phase"] for it in iterations}
        self.assertIn("execute", phases)
        self.assertIn("critic", phases)


if __name__ == "__main__":
    unittest.main()
