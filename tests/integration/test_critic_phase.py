# test_critic_phase.py — Integration test for the critic phase lifecycle.
#
# Verifies:
#   (a) Full critic lifecycle: execute completes all tasks, daemon
#       triggers critic, critic adds tasks, daemon returns to execute,
#       execute completes new tasks, critic approves, spec completed.
#   (b) Multiple critic passes: critic_passes counter increments each
#       time the critic runs and adds tasks.
#   (c) Critic crash (no output): spec is completed anyway via the
#       safety valve.

import os
import sys
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


# ── Shared critic-aware completion handler ──────────────────────────


def _critic_completion_handler(
    harness,
    spec_id,
    phase,
    exit_code,
    worker_id,
    max_critic_passes=None,
):
    """Completion handler that drives the execute->critic->execute cycle.

    After execute completes all tasks, triggers critic phase.
    After critic runs:
      - If critic added PENDING tasks, transitions back to execute.
      - If critic wrote '## Critic Approved', marks spec completed.
      - If critic produced no output (crash), marks spec completed
        (safety valve).
    Tracks critic_passes in the specs table.

    Args:
        harness: The DaemonTestHarness.
        spec_id: The spec being processed.
        phase: Phase that just completed.
        exit_code: Worker exit code.
        worker_id: Worker that ran the phase.
        max_critic_passes: Optional limit on critic passes.
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
            # Critic approved: increment critic_passes then complete
            with db.lock:
                db.conn.execute(
                    "UPDATE specs SET "
                    "critic_passes = critic_passes + 1 "
                    "WHERE id = ?",
                    (spec_id,),
                )
                db.conn.commit()
            db.complete(spec_id, new_done, new_total)
        else:
            # No critic output (crash/silent): safety valve, complete
            db.complete(spec_id, new_done, new_total)


# ── Test (a): Full critic lifecycle ──────────────────────────────────


class TestCriticLifecycle(IntegrationTestCase):
    """Full execute->critic->execute->critic->completed lifecycle.

    Flow:
      1. Execute completes all 3 initial tasks.
      2. Daemon triggers critic phase.
      3. Critic adds 2 new PENDING tasks.
      4. Daemon transitions back to execute.
      5. Execute completes the 2 new tasks.
      6. Daemon triggers critic again.
      7. Critic approves (writes '## Critic Approved').
      8. Spec marked completed.
    """

    NUM_WORKERS = 1

    def setUp(self) -> None:
        super().setUp()
        self._critic_call_counts: dict[str, int] = {}
        self.harness.start()

        harness_ref = self.harness
        daemon = self.harness._daemon
        daemon._dispatch_phase_completion = (
            lambda spec_id, phase, exit_code, worker_id: (
                _critic_completion_handler(
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

    def test_full_critic_lifecycle(self) -> None:
        """Dispatch 3-task spec. Execute completes all, critic adds 2,
        execute completes those, critic approves. Spec completed."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path, max_iterations=10)

        spec = self.harness.wait_for_status(
            spec_id, "completed", timeout=45
        )

        self.assertEqual(spec["status"], "completed")

        # All 5 tasks should be done (3 original + 2 critic-added)
        self.assertEqual(spec["tasks_done"], 5)
        self.assertEqual(spec["tasks_total"], 5)

    def test_iterations_include_both_phases(self) -> None:
        """Verify iterations table has both execute and critic entries."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path, max_iterations=10)

        self.harness.wait_for_status(
            spec_id, "completed", timeout=45
        )

        iterations = self.harness.get_iterations(spec_id)
        execute_iters = [
            it for it in iterations if it["phase"] == "execute"
        ]
        critic_iters = [
            it for it in iterations if it["phase"] == "critic"
        ]

        # 2 execute passes (initial + post-critic)
        self.assertEqual(
            len(execute_iters), 2,
            f"Expected 2 execute iterations, got "
            f"{len(execute_iters)}. All: {iterations}",
        )

        # 2 critic passes (one adds tasks, one approves)
        self.assertEqual(
            len(critic_iters), 2,
            f"Expected 2 critic iterations, got "
            f"{len(critic_iters)}. All: {iterations}",
        )

    def test_spec_file_has_all_tasks_done(self) -> None:
        """After completion, the spec file should have all 5 tasks
        marked DONE and contain the critic approval marker."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path, max_iterations=10)

        self.harness.wait_for_status(
            spec_id, "completed", timeout=45
        )

        spec = self.db.get_spec(spec_id)
        queue_spec_path = spec["spec_path"]
        content = Path(queue_spec_path).read_text(encoding="utf-8")

        # No PENDING tasks remain
        self.assertNotIn("\nPENDING\n", content)

        # Critic approval marker is present
        self.assertIn("## Critic Approved", content)

        from lib.spec_parser import parse_boi_spec

        tasks = parse_boi_spec(content)
        self.assertEqual(len(tasks), 5)
        for task in tasks:
            self.assertEqual(task.status, "DONE")

    def test_events_show_critic_transitions(self) -> None:
        """Events table should record the critic phase transitions."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path, max_iterations=10)

        self.harness.wait_for_status(
            spec_id, "completed", timeout=45
        )

        events = self.harness.get_events(spec_id=spec_id)
        event_types = [e["event_type"] for e in events]

        self.assertIn("queued", event_types)
        self.assertIn("running", event_types)
        self.assertIn("requeued", event_types)
        self.assertIn("completed", event_types)

        # Check that some requeue events mention critic
        requeue_messages = [
            e.get("message", "")
            for e in events
            if e["event_type"] == "requeued"
        ]
        critic_msgs = [
            m for m in requeue_messages if "critic" in m.lower()
        ]
        self.assertGreater(
            len(critic_msgs), 0,
            "Expected at least one requeue event mentioning critic. "
            f"Got requeue messages: {requeue_messages}",
        )


# ── Test (b): Critic passes counter ─────────────────────────────────


class TestCriticPassesCounter(IntegrationTestCase):
    """Verify critic_passes counter increments each time the critic
    runs. Critic adds tasks on passes 1-3, then approves on pass 4."""

    NUM_WORKERS = 1

    def setUp(self) -> None:
        super().setUp()
        self._critic_call_counts: dict[str, int] = {}
        self.harness.start()

        harness_ref = self.harness
        daemon = self.harness._daemon
        daemon._dispatch_phase_completion = (
            lambda spec_id, phase, exit_code, worker_id: (
                _critic_completion_handler(
                    harness_ref, spec_id, phase, exit_code, worker_id
                )
            )
        )

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        """Execute: complete all tasks.
        Critic: add 1 task on passes 1-3, approve on pass 4."""
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

            if count < 3:
                # Passes 1-3: add 1 task each
                return MockClaude(
                    phase="critic",
                    add_tasks=1,
                    exit_code=0,
                )
            else:
                # Pass 4: approve
                return MockClaude(
                    phase="critic",
                    critic_approve=True,
                    exit_code=0,
                )
        return MockClaude(exit_code=0)

    def test_critic_passes_increments(self) -> None:
        """Dispatch a 2-task spec. Critic runs 3 passes adding tasks,
        then approves on pass 4. Verify critic_passes == 4."""
        spec_path = self.create_spec(tasks_pending=2)
        spec_id = self.dispatch_spec(spec_path, max_iterations=20)

        spec = self.harness.wait_for_status(
            spec_id, "completed", timeout=60
        )

        self.assertEqual(spec["status"], "completed")

        # critic_passes should be 4 (3 add-task passes + 1 approval)
        self.assertEqual(
            spec["critic_passes"], 4,
            f"Expected 4 critic passes, got {spec['critic_passes']}",
        )

    def test_all_critic_added_tasks_completed(self) -> None:
        """Verify all tasks (original + critic-added) are DONE."""
        spec_path = self.create_spec(tasks_pending=2)
        spec_id = self.dispatch_spec(spec_path, max_iterations=20)

        self.harness.wait_for_status(
            spec_id, "completed", timeout=60
        )

        spec = self.db.get_spec(spec_id)
        queue_spec_path = spec["spec_path"]
        content = Path(queue_spec_path).read_text(encoding="utf-8")

        from lib.spec_parser import parse_boi_spec

        tasks = parse_boi_spec(content)
        # 2 original + 3 critic-added = 5 total
        self.assertEqual(
            len(tasks), 5,
            f"Expected 5 tasks (2 original + 3 critic-added), "
            f"got {len(tasks)}",
        )
        for task in tasks:
            self.assertEqual(task.status, "DONE")

    def test_iterations_track_all_phases(self) -> None:
        """Verify iterations table records all execute and critic phases."""
        spec_path = self.create_spec(tasks_pending=2)
        spec_id = self.dispatch_spec(spec_path, max_iterations=20)

        self.harness.wait_for_status(
            spec_id, "completed", timeout=60
        )

        iterations = self.harness.get_iterations(spec_id)
        execute_iters = [
            it for it in iterations if it["phase"] == "execute"
        ]
        critic_iters = [
            it for it in iterations if it["phase"] == "critic"
        ]

        # 4 execute passes: initial + 3 post-critic
        self.assertEqual(
            len(execute_iters), 4,
            f"Expected 4 execute iterations, got "
            f"{len(execute_iters)}. All: {iterations}",
        )

        # 4 critic passes
        self.assertEqual(
            len(critic_iters), 4,
            f"Expected 4 critic iterations, got "
            f"{len(critic_iters)}. All: {iterations}",
        )


# ── Test (c): Critic crash (no output) safety valve ──────────────────


class TestCriticCrashSafetyValve(IntegrationTestCase):
    """When critic produces no output (simulating a crash), the spec
    should be completed anyway via the safety valve. The daemon
    treats no critic output as implicit approval."""

    NUM_WORKERS = 1

    def setUp(self) -> None:
        super().setUp()
        self.harness.start()

        harness_ref = self.harness
        daemon = self.harness._daemon
        daemon._dispatch_phase_completion = (
            lambda spec_id, phase, exit_code, worker_id: (
                _critic_completion_handler(
                    harness_ref, spec_id, phase, exit_code, worker_id
                )
            )
        )

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        """Execute: complete all tasks.
        Critic: no output (fail_silently), simulating a crash."""
        if phase == "execute":
            return MockClaude(
                phase="execute",
                tasks_to_complete=99,
                exit_code=0,
            )
        elif phase == "critic":
            # Critic crashes: produces no output, exits 0
            return MockClaude(
                phase="critic",
                fail_silently=True,
                exit_code=0,
            )
        return MockClaude(exit_code=0)

    def test_spec_completes_on_critic_crash(self) -> None:
        """Dispatch 3-task spec. Execute completes all, critic crashes
        (no output). Spec should be completed via safety valve."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path, max_iterations=10)

        spec = self.harness.wait_for_status(
            spec_id, "completed", timeout=30
        )

        self.assertEqual(spec["status"], "completed")
        self.assertEqual(spec["tasks_done"], 3)
        self.assertEqual(spec["tasks_total"], 3)

    def test_no_critic_approval_marker(self) -> None:
        """After safety-valve completion, the spec file should NOT
        contain '## Critic Approved' since critic crashed."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path, max_iterations=10)

        self.harness.wait_for_status(
            spec_id, "completed", timeout=30
        )

        spec = self.db.get_spec(spec_id)
        queue_spec_path = spec["spec_path"]
        content = Path(queue_spec_path).read_text(encoding="utf-8")

        self.assertNotIn("## Critic Approved", content)

    def test_all_tasks_still_done(self) -> None:
        """Even with critic crash, all original tasks should be DONE."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path, max_iterations=10)

        self.harness.wait_for_status(
            spec_id, "completed", timeout=30
        )

        spec = self.db.get_spec(spec_id)
        queue_spec_path = spec["spec_path"]
        content = Path(queue_spec_path).read_text(encoding="utf-8")

        self.assertNotIn("\nPENDING\n", content)

        from lib.spec_parser import parse_boi_spec

        tasks = parse_boi_spec(content)
        for task in tasks:
            self.assertEqual(task.status, "DONE")

    def test_critic_crash_events_recorded(self) -> None:
        """Events should show the full lifecycle even with critic crash."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path, max_iterations=10)

        self.harness.wait_for_status(
            spec_id, "completed", timeout=30
        )

        events = self.harness.get_events(spec_id=spec_id)
        event_types = [e["event_type"] for e in events]

        self.assertIn("queued", event_types)
        self.assertIn("running", event_types)
        self.assertIn("completed", event_types)

        # Should have at least 2 running events (execute + critic)
        running_count = event_types.count("running")
        self.assertGreaterEqual(
            running_count, 2,
            f"Expected at least 2 running events, got {running_count}",
        )


if __name__ == "__main__":
    unittest.main()
