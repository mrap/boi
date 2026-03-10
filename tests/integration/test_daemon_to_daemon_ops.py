# test_daemon_to_daemon_ops.py — Integration test for daemon -> daemon_ops interface.
#
# These tests exercise the REAL _dispatch_phase_completion path
# (daemon.py -> lib/daemon_ops.py) without any patching. They verify
# that the daemon passes the correct kwargs to daemon_ops functions.
#
# This test file exists because the original rewrite introduced a
# spec_path vs script_dir kwarg mismatch that unit tests didn't catch
# (each side was tested in isolation with mocks). These integration
# tests call the real daemon with real daemon_ops to catch interface
# mismatches.

import os
import sys
import unittest
from pathlib import Path

# Add project root to path
_PROJECT_ROOT = str(Path(__file__).resolve().parent.parent.parent)
sys.path.insert(0, _PROJECT_ROOT)

from tests.integration.conftest import (
    IntegrationTestCase,
    MockClaude,
)


class TestDaemonToDaemonOpsExecutePhase(IntegrationTestCase):
    """Test that daemon correctly calls daemon_ops.process_worker_completion.

    This is the exact interface that broke: daemon.py passed spec_path=
    but daemon_ops expected script_dir=. Uses NO patching of
    _dispatch_phase_completion — exercises the real code path.
    """

    NUM_WORKERS = 1

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        """Complete 1 task per iteration, all phases."""
        if phase == "execute":
            return MockClaude(
                phase="execute", tasks_to_complete=1, exit_code=0
            )
        elif phase == "critic":
            return MockClaude(
                phase="critic", critic_approve=True, exit_code=0
            )
        return MockClaude(exit_code=0)

    def setUp(self) -> None:
        super().setUp()
        # Start the daemon WITHOUT patching _dispatch_phase_completion.
        # This exercises the real daemon -> daemon_ops path.
        self.harness.start()

    def test_single_task_completes_via_real_daemon_ops(self) -> None:
        """A 1-task spec completes through the real daemon_ops path."""
        spec_path = self.create_spec(tasks_pending=1)
        spec_id = self.dispatch_spec(spec_path)

        spec = self.harness.wait_for_status(
            spec_id, "completed", timeout=30
        )
        self.assertEqual(spec["status"], "completed")

    def test_multi_task_completes_via_real_daemon_ops(self) -> None:
        """A 3-task spec completes through real daemon_ops requeue loop."""
        spec_path = self.create_spec(tasks_pending=3)
        spec_id = self.dispatch_spec(spec_path, max_iterations=10)

        spec = self.harness.wait_for_status(
            spec_id, "completed", timeout=60
        )
        self.assertEqual(spec["status"], "completed")
        # All 3 tasks should be done
        self.assertEqual(spec["tasks_done"], 3)
        self.assertEqual(spec["tasks_total"], 3)

    def test_daemon_ops_records_events(self) -> None:
        """Real daemon_ops writes events to the events table."""
        spec_path = self.create_spec(tasks_pending=1)
        spec_id = self.dispatch_spec(spec_path)

        self.harness.wait_for_status(spec_id, "completed", timeout=30)

        events = self.harness.get_events(spec_id)
        event_types = [e["event_type"] for e in events]

        # Must have at least queued, running, and completed events
        self.assertIn("queued", event_types)
        self.assertIn("running", event_types)
        self.assertIn("completed", event_types)

    def test_worker_freed_after_completion(self) -> None:
        """Worker is freed after spec completes via real daemon_ops."""
        spec_path = self.create_spec(tasks_pending=1)
        spec_id = self.dispatch_spec(spec_path)

        self.harness.wait_for_status(spec_id, "completed", timeout=30)

        # Check worker is free
        worker = self.db.get_worker("w-1")
        self.assertIsNone(
            worker.get("current_spec_id"),
            "Worker should be freed after completion",
        )


class TestDaemonToDaemonOpsFailure(IntegrationTestCase):
    """Test that daemon handles worker failures through real daemon_ops."""

    NUM_WORKERS = 1

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        """Fail on execute with non-zero exit code."""
        return MockClaude(
            phase="execute", tasks_to_complete=0, exit_code=1
        )

    def setUp(self) -> None:
        super().setUp()
        self.harness.start()

    def test_failed_worker_requeues_via_real_daemon_ops(self) -> None:
        """Worker failure is handled by real daemon_ops without crash."""
        spec_path = self.create_spec(tasks_pending=2)
        spec_id = self.dispatch_spec(spec_path, max_iterations=2)

        # Should eventually reach failed or requeued, not crash
        spec = self.harness.wait_for_any_status(
            spec_id,
            ["failed", "requeued", "completed"],
            timeout=30,
        )
        self.assertIn(
            spec["status"],
            ["failed", "requeued", "completed"],
            "Spec should reach a terminal or requeued state, not crash",
        )


class TestDaemonToDaemonOpsConcurrent(IntegrationTestCase):
    """Test multiple specs through real daemon_ops concurrently."""

    NUM_WORKERS = 2

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        return MockClaude(
            phase="execute", tasks_to_complete=1, exit_code=0
        )

    def setUp(self) -> None:
        super().setUp()
        self.harness.start()

    def test_two_specs_complete_concurrently(self) -> None:
        """Two specs on two workers both complete through real daemon_ops."""
        spec1_path = self.create_spec(
            tasks_pending=1, filename="spec1.md"
        )
        spec2_path = self.create_spec(
            tasks_pending=1, filename="spec2.md"
        )

        spec1_id = self.dispatch_spec(spec1_path)
        spec2_id = self.dispatch_spec(spec2_path)

        spec1 = self.harness.wait_for_status(
            spec1_id, "completed", timeout=30
        )
        spec2 = self.harness.wait_for_status(
            spec2_id, "completed", timeout=30
        )

        self.assertEqual(spec1["status"], "completed")
        self.assertEqual(spec2["status"], "completed")


if __name__ == "__main__":
    unittest.main()
