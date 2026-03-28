# test_critic_requeue.py — Integration test: critic tasks prevent auto-completion.
#
# Regression test for the [CRITIC] PENDING parsing fix (t-1 in q-244).
#
# Before the fix: count_boi_tasks() returned 0 pending for [CRITIC] PENDING tasks,
# so the spec auto-completed despite having unfinished critic work.
# After the fix: count_boi_tasks() correctly counts [CRITIC] PENDING tasks,
# causing the daemon to requeue instead of complete.
#
# Uses real parse_boi_spec and count_boi_tasks — no mocks for the parser.

import os
import sys
import threading
import time
import unittest
from datetime import datetime, timezone
from pathlib import Path

_PROJECT_ROOT = str(Path(__file__).resolve().parent.parent.parent)
sys.path.insert(0, _PROJECT_ROOT)

from tests.integration.conftest import (
    IntegrationTestCase,
    MockClaude,
)


class TestCriticRequeue(IntegrationTestCase):
    """[CRITIC] PENDING tasks must prevent auto-completion.

    Flow:
      1. Spec has 2 PENDING tasks.
      2. Execute: MockClaude marks both DONE.
      3. Daemon detects 0 pending, triggers critic phase.
      4. Critic: MockClaude adds 1 [CRITIC] PENDING task.
      5. count_boi_tasks() must return 1 pending — NOT 0.
      6. Daemon must requeue (NOT complete) the spec.
    """

    NUM_WORKERS = 1

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        if phase == "execute":
            # Complete all tasks (up to 10) each iteration
            return MockClaude(
                phase="execute", tasks_to_complete=10, exit_code=0
            )
        elif phase == "critic":
            # Always add 1 [CRITIC] task — never approve
            return MockClaude(
                phase="critic",
                add_tasks=1,
                critic_approve=False,
                exit_code=0,
            )
        return MockClaude(exit_code=0)

    def setUp(self) -> None:
        super().setUp()
        # Event set when critic phase has completed and requeued the spec
        self._critic_ran = threading.Event()
        self._critic_spec_id: str = ""
        # Snapshot of spec content at the moment critic phase completed
        # (saved before requeuing to avoid race with execute worker)
        self._critic_spec_content: str = ""
        self.harness.start()
        daemon = self.harness._daemon
        daemon._dispatch_phase_completion = self._phase_completion

    def _phase_completion(
        self,
        spec_id: str,
        phase: str,
        exit_code: int,
        worker_id: str,
    ) -> None:
        """Completion handler using real count_boi_tasks — no mocks.

        Execute phase: if all tasks done, transition to critic.
        Critic phase: use count_boi_tasks on the real spec file to
          determine if the critic added pending tasks; requeue if so.
        """
        from lib.spec_parser import count_boi_tasks

        daemon = self.harness._daemon
        db = daemon.db
        spec = db.get_spec(spec_id)
        if spec is None:
            return

        spec_path = spec.get("spec_path", "")
        if not spec_path or not os.path.isfile(spec_path):
            db.requeue(spec_id, 0, 0)
            return

        # Use REAL count_boi_tasks on the real spec file
        counts = count_boi_tasks(spec_path)
        done = counts["done"]
        total = counts["total"]
        pending = counts["pending"]

        now = datetime.now(timezone.utc).isoformat()
        pre_done = spec.get("tasks_done", 0)
        db.insert_iteration(
            spec_id=spec_id,
            iteration=spec["iteration"],
            phase=phase,
            worker_id=worker_id,
            started_at=now,
            ended_at=now,
            duration_seconds=0,
            tasks_completed=max(0, done - pre_done),
            exit_code=exit_code,
            pre_pending=total - pre_done,
            post_pending=pending,
        )

        if exit_code != 0:
            db.requeue(spec_id, done, total)
            return

        if phase == "execute":
            if pending == 0 and total > 0:
                # All tasks done — trigger critic
                db.update_spec_fields(spec_id, phase="critic")
                db.requeue(spec_id, done, total)
            else:
                db.requeue(spec_id, done, total)

        elif phase == "critic":
            # Re-read counts AFTER critic modified spec
            counts_after = count_boi_tasks(spec_path)
            pending_after = counts_after["pending"]
            done_after = counts_after["done"]
            total_after = counts_after["total"]

            if pending_after > 0:
                # Critic added tasks — snapshot content BEFORE requeuing
                # so test assertions see the pre-execute state.
                self._critic_spec_content = Path(spec_path).read_text(
                    encoding="utf-8"
                )
                # Back to execute phase
                db.update_spec_fields(spec_id, phase="execute")
                db.requeue(spec_id, done_after, total_after)
                # Signal that critic phase is done and spec was requeued
                self._critic_spec_id = spec_id
                self._critic_ran.set()
            else:
                content = Path(spec_path).read_text(encoding="utf-8")
                if "## Critic Approved" in content:
                    db.complete(spec_id, done_after, total_after)
                else:
                    db.complete(spec_id, done_after, total_after)

    def test_critic_tasks_prevent_auto_completion(self) -> None:
        """[CRITIC] PENDING tasks must be counted as pending,
        preventing auto-completion after the critic phase.

        This is the primary regression test for the fix in t-1 (q-244):
        Before the fix, count_boi_tasks() silently dropped [CRITIC] PENDING
        tasks, causing the spec to complete prematurely.
        """
        spec_content = (
            "# Test Critic Requeue\n\n"
            "## Tasks\n\n"
            "### t-1: First task\n"
            "PENDING\n\n"
            "**Spec:** Do the first thing.\n\n"
            "**Verify:** true\n\n"
            "### t-2: Second task\n"
            "PENDING\n\n"
            "**Spec:** Do the second thing.\n\n"
            "**Verify:** true\n"
        )
        spec_path = self.create_spec(content=spec_content)
        spec_id = self.dispatch_spec(spec_path, max_iterations=20)

        # Wait for the critic phase to run and requeue the spec
        notified = self._critic_ran.wait(timeout=30)
        self.assertTrue(
            notified,
            "Timed out waiting for critic phase to complete. "
            "The daemon may not have triggered the critic phase.",
        )

        # Brief pause to let the DB commit settle (no sleep needed
        # after the event fires — the requeue already committed above)
        time.sleep(0.05)

        # 7. Spec must NOT be completed — critic added a pending task
        spec = self.db.get_spec(spec_id)
        self.assertIsNotNone(spec, "Spec must exist in database")
        self.assertNotEqual(
            spec["status"],
            "completed",
            f"Spec MUST NOT be completed after critic added [CRITIC] "
            f"PENDING task. Got status={spec['status']!r}. "
            "This indicates count_boi_tasks() is not counting "
            "[CRITIC] PENDING tasks correctly.",
        )

        # 8. Spec file must contain the [CRITIC] task with PENDING status.
        # Use the snapshot saved before requeuing to avoid a race where the
        # execute worker immediately completes the [CRITIC] task.
        queue_spec_path = spec["spec_path"]
        content = self._critic_spec_content
        self.assertTrue(
            content,
            "Critic spec content snapshot must be non-empty",
        )
        self.assertIn(
            "[CRITIC]",
            content,
            "Spec file must contain [CRITIC] task after critic ran",
        )
        self.assertIn(
            "PENDING",
            content,
            "Spec file must contain PENDING status after critic ran",
        )

        # 9. count_boi_tasks() must return >= 1 pending — real function,
        #    parsed from the snapshot, no mocks
        from lib.spec_parser import count_boi_tasks
        import tempfile, os as _os

        with tempfile.NamedTemporaryFile(
            mode="w", suffix=".md", delete=False, encoding="utf-8"
        ) as tmp:
            tmp.write(content)
            tmp_path = tmp.name
        try:
            counts = count_boi_tasks(tmp_path)
        finally:
            _os.unlink(tmp_path)
        self.assertGreaterEqual(
            counts["pending"],
            1,
            f"count_boi_tasks() must return >= 1 pending after critic "
            f"added [CRITIC] PENDING task, got pending={counts['pending']} "
            f"(full counts: {counts}). "
            "This is the regression check for the [CRITIC] PENDING parsing fix.",
        )


if __name__ == "__main__":
    unittest.main()
