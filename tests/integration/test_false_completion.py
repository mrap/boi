# test_false_completion.py — Integration test: false completion with no-diff detection.
#
# Documents the "marked DONE but no files changed" pattern.
# Serves as anchor for the P1 fix (daemon-enforced verify).
#
# A worker that marks all tasks DONE but creates no files in the worktree is a
# "false completion" — the spec appears done, but no real work was performed.
# This test verifies that this scenario IS detectable at test level, and documents
# that the daemon currently has NO mechanism to detect or warn about it.

import os
import subprocess
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


class TestFalseCompletion(IntegrationTestCase):
    """Document the 'marked DONE but no files changed' false completion pattern.

    Flow:
      1. Spec has 1 task that requires creating a file.
      2. Execute: MockClaude marks the task DONE but does NOT create the file.
      3. Daemon completes the spec (it has no mechanism to detect the empty diff).
      4. Assert: spec is completed, task is DONE.
      5. Assert: git status in the worktree is clean (no files were created).
      6. The discrepancy (DONE task, empty diff) is the false completion pattern.

    The daemon currently has NO mechanism to detect this discrepancy.
    This test anchors the P1 fix (daemon-enforced verify).
    """

    NUM_WORKERS = 1

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        if phase == "execute":
            # Mark all tasks DONE — but create no files in the worktree.
            # MockClaude only modifies the spec file (in queue dir), never
            # touches the worktree directory where the worker runs.
            return MockClaude(
                phase="execute", tasks_to_complete=10, exit_code=0
            )
        return MockClaude(exit_code=0)

    def setUp(self) -> None:
        super().setUp()

        # Initialize git repos in all worktrees so we can check diffs.
        for wt in self.config["worktrees"]:
            subprocess.run(
                ["git", "init", wt],
                check=True,
                capture_output=True,
            )
            subprocess.run(
                ["git", "-C", wt, "config", "user.email", "test@boi.local"],
                check=True,
                capture_output=True,
            )
            subprocess.run(
                ["git", "-C", wt, "config", "user.name", "BOI Test"],
                check=True,
                capture_output=True,
            )
            # Create initial commit so HEAD exists and we can diff later.
            init_file = os.path.join(wt, ".gitkeep")
            Path(init_file).write_text("", encoding="utf-8")
            subprocess.run(
                ["git", "-C", wt, "add", "."],
                check=True,
                capture_output=True,
            )
            subprocess.run(
                ["git", "-C", wt, "commit", "-m", "Initial commit"],
                check=True,
                capture_output=True,
            )

        self._execute_done = threading.Event()
        self._done_spec_id: str = ""

        self.harness.start()
        self.harness._daemon._dispatch_phase_completion = (
            self._on_phase_completion
        )

    def _on_phase_completion(
        self,
        spec_id: str,
        phase: str,
        exit_code: int,
        worker_id: str,
    ) -> None:
        """Minimal completion handler: complete spec after successful execute."""
        from lib.spec_parser import count_boi_tasks
        from lib.daemon_ops import warn_if_empty_diff

        daemon = self.harness._daemon
        db = daemon.db
        spec = db.get_spec(spec_id)
        if spec is None:
            return

        spec_path = spec.get("spec_path", "")
        counts: dict = {"pending": 0, "done": 0, "total": 0}
        if spec_path and os.path.isfile(spec_path):
            counts = count_boi_tasks(spec_path)

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
            tasks_completed=max(0, counts["done"] - pre_done),
            exit_code=exit_code,
            pre_pending=counts["total"] - pre_done,
            post_pending=counts["pending"],
        )

        if exit_code != 0:
            db.requeue(spec_id, counts["done"], counts["total"])
            return

        if phase == "execute":
            if counts["pending"] == 0 and counts["total"] > 0:
                # All tasks DONE — complete the spec.
                # NOTE: the daemon does NOT check git diff here.
                #
                # Emit the empty-diff warning if the worktree has no changes
                # (the P1 fix that this test anchors).
                newly_done = counts["done"] - pre_done
                worker_row = db.get_worker(worker_id)
                worktree_path = (worker_row or {}).get("worktree_path", "")
                warn_if_empty_diff(spec_id, worktree_path, newly_done)
                db.complete(spec_id, counts["done"], counts["total"])
                self._done_spec_id = spec_id
                self._execute_done.set()
            else:
                db.requeue(spec_id, counts["done"], counts["total"])

    def test_false_completion_no_diff(self) -> None:
        """Task marked DONE with no file changes in worktree = false completion.

        This test documents that the daemon currently allows a worker to mark
        a task DONE without creating any files or making any changes. The git
        worktree remains clean, proving that no real work was performed.

        The daemon DOES now log a WARNING when tasks are marked DONE but the
        git diff is empty (the P1 detection implemented in t-8).
        """
        import logging

        spec_content = (
            "# Test False Completion\n\n"
            "## Tasks\n\n"
            "### t-1: Create output file\n"
            "PENDING\n\n"
            "**Spec:** Create the file output.txt with content 'done'.\n\n"
            "**Verify:** test -f output.txt && grep -q done output.txt\n"
        )
        spec_path = self.create_spec(content=spec_content)

        with self.assertLogs("boi.daemon_ops", level=logging.WARNING) as log_ctx:
            spec_id = self.dispatch_spec(spec_path, max_iterations=5)

            # Wait for execute phase to complete and spec to be marked done.
            notified = self._execute_done.wait(timeout=30)
            self.assertTrue(
                notified,
                "Timed out waiting for execute phase to complete. "
                "The daemon may not have launched a worker.",
            )

            time.sleep(0.05)

        # --- Assertion 0: empty-diff warning was logged ---
        warning_msgs = " ".join(log_ctx.output)
        self.assertIn(
            "tasks marked DONE but git diff is empty",
            warning_msgs,
            "Expected empty-diff WARNING to be logged by boi.daemon_ops",
        )

        # --- Assertion 1: spec is blocked by ship verify (needs_review) ---
        # The daemon now runs **Verify:** commands in the ship phase. Because
        # `test -f output.txt` fails (MockClaude never created the file), the
        # daemon sets status='needs_review' instead of completing the spec.
        spec = self.db.get_spec(spec_id)
        self.assertIsNotNone(spec, "Spec must exist in database")
        self.assertEqual(
            spec["status"],
            "needs_review",
            f"Daemon ship verify should have blocked the false completion. "
            f"Got status={spec['status']!r}. "
            "Expected 'needs_review' because `test -f output.txt` fails.",
        )

        # --- Assertion 2: task in spec file is DONE ---
        queue_spec_path = spec["spec_path"]
        self.assertTrue(
            os.path.isfile(queue_spec_path),
            f"Spec file not found at {queue_spec_path}",
        )
        from lib.spec_parser import count_boi_tasks

        counts = count_boi_tasks(queue_spec_path)
        self.assertEqual(
            counts["pending"],
            0,
            "All tasks in spec are marked DONE (false completion confirmed in spec file).",
        )
        self.assertGreater(
            counts["done"],
            0,
            "At least one task should be marked DONE.",
        )

        # --- Assertion 3: git diff is empty in ALL worktrees (no real work) ---
        #
        # The worker ran in one of the worktrees but only modified the spec file
        # (which lives in the queue dir, not the worktree). No actual output files
        # were created in the worktree. This is the false completion pattern:
        # the spec says DONE, but the worktree has no new changes.
        any_worktree_dirty = False
        for wt in self.config["worktrees"]:
            result = subprocess.run(
                ["git", "-C", wt, "status", "--porcelain"],
                capture_output=True,
                text=True,
            )
            if result.returncode == 0 and result.stdout.strip():
                any_worktree_dirty = True
                break

        self.assertFalse(
            any_worktree_dirty,
            "All worktrees should be git-clean — MockClaude marked tasks DONE "
            "without creating any files in the worktree. "
            "This is the false completion pattern: spec says DONE but the "
            "worktree git diff is empty (no real work was performed). "
            "The daemon currently has NO mechanism to detect this discrepancy.",
        )

        # --- Summary ---
        # At this point we have confirmed all three conditions of false completion:
        #   1. spec["status"] == "completed"        — daemon accepted it as done
        #   2. counts["done"] > 0, pending == 0     — task marked DONE in spec
        #   3. all worktrees git-clean              — no files created
        #
        # The P1 fix should: after each execute phase, run the verify commands
        # listed in each newly-DONE task and refuse to complete the spec unless
        # all verify commands pass with real output.


if __name__ == "__main__":
    unittest.main()
