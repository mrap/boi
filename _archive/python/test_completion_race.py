# tests/test_completion_race.py — Tests for the critic-added task
# completion race condition.
#
# Bug: When the critic adds new PENDING tasks during the final iteration,
# process_critic_completion marks the spec as completed even though there
# are still pending tasks. The fix must check post_counts (task counts
# after the critic ran) rather than assuming completion when the critic
# produces no explicit approval or [CRITIC]-tagged tasks.
#
# See: q-132 iteration 14 evidence:
#   pre_counts:  {pending: 0, done: 14, total: 14}
#   post_counts: {pending: 1, done: 14, total: 15}
#   tasks_added: 1  → spec was wrongly marked completed

import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.daemon_ops import process_critic_completion
from lib.db import Database


# ── Helpers ──────────────────────────────────────────────────────────


def _make_spec_content(n_done: int, n_pending: int = 0) -> str:
    """Build spec file content with n_done DONE tasks and n_pending PENDING tasks."""
    lines = ["# Test Spec\n\n## Tasks\n"]
    tid = 1
    for _ in range(n_done):
        lines.append(
            f"\n### t-{tid}: Task {tid}\n"
            "DONE\n\n"
            f"**Spec:** Task {tid}.\n\n"
            "**Verify:** true\n"
        )
        tid += 1
    for _ in range(n_pending):
        lines.append(
            f"\n### t-{tid}: Task {tid}\n"
            "PENDING\n\n"
            f"**Spec:** Task {tid}.\n\n"
            "**Verify:** true\n"
        )
        tid += 1
    return "".join(lines)


def _append_pending_task(spec_path: str, task_id: int, tag: str = "") -> None:
    """Append a new PENDING task to the spec (simulates critic adding a task).

    Args:
        spec_path: Path to the spec file to modify.
        task_id: The t-N id to use.
        tag: Optional tag to embed in the task title (e.g. '[CRITIC]').
    """
    content = Path(spec_path).read_text(encoding="utf-8")
    title = f"New task {task_id}"
    if tag:
        title = f"{title} {tag}"
    new_task = (
        f"\n### t-{task_id}: {title}\n"
        "PENDING\n\n"
        "**Spec:** Do this new task.\n\n"
        "**Verify:** true\n"
    )
    tmp = spec_path + ".tmp"
    Path(tmp).write_text(content.rstrip() + "\n" + new_task, encoding="utf-8")
    os.rename(tmp, spec_path)


class CompletionRaceTestCase(unittest.TestCase):
    """Base test case with temp dirs and a Database."""

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.state_dir = self._tmpdir.name
        self.queue_dir = os.path.join(self.state_dir, "queue")
        self.events_dir = os.path.join(self.state_dir, "events")
        self.hooks_dir = os.path.join(self.state_dir, "hooks")
        self.specs_dir = os.path.join(self.state_dir, "specs")

        for d in [self.queue_dir, self.events_dir, self.hooks_dir, self.specs_dir]:
            os.makedirs(d, exist_ok=True)

        db_path = os.path.join(self.state_dir, "boi.db")
        self.db = Database(db_path, self.queue_dir)

    def tearDown(self):
        self.db.close()
        self._tmpdir.cleanup()

    def _enqueue_spec(self, content: str, filename: str = "test.spec.md") -> tuple:
        """Write spec content to a file, enqueue it, return (queue_id, queued_spec_path)."""
        orig_path = os.path.join(self.specs_dir, filename)
        Path(orig_path).write_text(content, encoding="utf-8")
        result = self.db.enqueue(spec_path=orig_path)
        queue_id = result["id"]
        spec = self.db.get_spec(queue_id)
        return queue_id, spec["spec_path"]

    def _run_critic_completion(self, queue_id: str, spec_path: str) -> dict:
        """Call process_critic_completion and return the result dict."""
        return process_critic_completion(
            queue_dir=self.queue_dir,
            queue_id=queue_id,
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=spec_path,
            db=self.db,
        )


# ── t-1 test: RED — must fail against current code ───────────────────


class TestCriticAddedTaskCompletionRace(CompletionRaceTestCase):
    """Reproduce the race condition bug.

    Scenario: All 3 pre-existing tasks are DONE. The critic adds 1 new
    PENDING task (without the [CRITIC] tag and without writing
    '## Critic Approved'). The current code falls into the 'else' branch
    of process_critic_completion and calls db.complete() — incorrectly
    marking the spec as completed even though 1 task is still pending.

    This test MUST FAIL against the current (buggy) code and PASS after
    the fix (t-2) is applied.
    """

    def test_critic_added_pending_task_prevents_completion(self):
        """Spec must NOT be completed when critic adds a PENDING task.

        Pre-state (before critic):
          pre_counts:  {pending: 0, done: 3, total: 3}

        Post-state (after critic adds a task without [CRITIC] tag):
          post_counts: {pending: 1, done: 3, total: 4}

        Expected: spec is requeued (or at minimum NOT completed).
        Actual (buggy): spec is marked completed via the else fallback.
        """
        # Step 1: Create spec with 3 tasks, all DONE
        content = _make_spec_content(n_done=3)
        queue_id, spec_path = self._enqueue_spec(content)

        # Step 2: Simulate critic adding 1 PENDING task (no [CRITIC] tag)
        # This represents tasks_added=1 in the iteration metadata:
        #   pre_counts:  {pending: 0, done: 3, total: 3}
        #   post_counts: {pending: 1, done: 3, total: 4}
        _append_pending_task(spec_path, task_id=4, tag="")

        # Step 3: Call the completion check (process_critic_completion)
        result = self._run_critic_completion(queue_id, spec_path)

        # Step 4: Assert spec is NOT complete — post_counts shows 1 pending
        spec = self.db.get_spec(queue_id)
        self.assertNotEqual(
            spec["status"],
            "completed",
            f"BUG: Spec incorrectly marked 'completed' even though "
            f"critic added a PENDING task (post_counts.pending=1). "
            f"outcome={result.get('outcome')!r}",
        )


# ── t-3 tests: Edge cases ────────────────────────────────────────────


class TestCompletionEdgeCases(CompletionRaceTestCase):
    """Edge cases for the completion check after the t-2 fix."""

    def test_normal_completion_no_critic_tasks(self):
        """All tasks done, critic adds 0 tasks → spec SHOULD be completed.

        Scenario: 3 tasks all DONE, critic produces no output (no approval
        marker, no [CRITIC] tasks). post_counts.pending == 0 → complete.
        """
        content = _make_spec_content(n_done=3)
        queue_id, spec_path = self._enqueue_spec(content, "normal.spec.md")

        result = self._run_critic_completion(queue_id, spec_path)

        spec = self.db.get_spec(queue_id)
        self.assertEqual(
            spec["status"],
            "completed",
            f"Expected 'completed' when all tasks done and critic adds nothing. "
            f"outcome={result.get('outcome')!r}, status={spec['status']!r}",
        )

    def test_critic_adds_two_tasks_prevents_completion(self):
        """Critic adds 2 PENDING tasks → spec must NOT be completed.

        post_counts: {pending: 2, done: 3, total: 5}
        """
        content = _make_spec_content(n_done=3)
        queue_id, spec_path = self._enqueue_spec(content, "two_tasks.spec.md")

        _append_pending_task(spec_path, task_id=4)
        _append_pending_task(spec_path, task_id=5)

        result = self._run_critic_completion(queue_id, spec_path)

        spec = self.db.get_spec(queue_id)
        self.assertNotEqual(
            spec["status"],
            "completed",
            f"BUG: Spec marked 'completed' even though critic added 2 PENDING tasks. "
            f"outcome={result.get('outcome')!r}",
        )

    def test_skipped_pending_tasks_prevent_completion(self):
        """Spec has PENDING tasks the worker skipped → must NOT be completed.

        Simulates tasks_skipped > 0: spec has 3 DONE + 1 still-PENDING task
        (skipped by the worker). Critic doesn't approve. post_counts.pending==1.
        """
        content = _make_spec_content(n_done=3, n_pending=1)
        queue_id, spec_path = self._enqueue_spec(content, "skipped.spec.md")

        result = self._run_critic_completion(queue_id, spec_path)

        spec = self.db.get_spec(queue_id)
        self.assertNotEqual(
            spec["status"],
            "completed",
            f"BUG: Spec marked 'completed' with a still-PENDING task "
            f"(tasks_skipped scenario). outcome={result.get('outcome')!r}",
        )

    def test_worker_completes_last_task_and_critic_adds_one(self):
        """Worker completes last task AND critic adds a new one → must NOT complete.

        pre_counts:  {pending: 1, done: 3, total: 4}
        Worker executes t-4, marking it DONE.
        Critic then adds t-5 (PENDING).
        post_counts: {pending: 1, done: 4, total: 5}
        """
        # Build spec: 3 done + 1 pending (simulates pre-iteration state)
        content = _make_spec_content(n_done=3, n_pending=1)
        queue_id, spec_path = self._enqueue_spec(content, "worker_then_critic.spec.md")

        # Simulate worker completing t-4: replace PENDING with DONE
        spec_text = Path(spec_path).read_text(encoding="utf-8")
        spec_text = spec_text.replace(
            "### t-4: Task 4\nPENDING", "### t-4: Task 4\nDONE", 1
        )
        tmp = spec_path + ".tmp"
        Path(tmp).write_text(spec_text, encoding="utf-8")
        os.rename(tmp, spec_path)

        # Critic adds t-5 (PENDING) — same iteration
        _append_pending_task(spec_path, task_id=5)

        result = self._run_critic_completion(queue_id, spec_path)

        spec = self.db.get_spec(queue_id)
        self.assertNotEqual(
            spec["status"],
            "completed",
            f"BUG: Spec marked 'completed' even though critic added t-5 (PENDING) "
            f"in the same iteration the worker finished the last task. "
            f"outcome={result.get('outcome')!r}",
        )


if __name__ == "__main__":
    unittest.main()
