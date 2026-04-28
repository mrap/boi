# test_dispatch_integration.py — Integration tests for BOI dispatch lifecycle.
#
# Tests the full spec dispatch flow end-to-end using library functions only.
# No Claude Code required. No daemons. Pure Python.
#
# Covers:
#   1. Dispatch creates a queued spec in the database
#   2. Queue copy is created in queue/
#   3. Status transitions: queued → in_progress → completed
#   4. DAG dependencies: spec B blocked until spec A completes

import os
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

# Add parent directory to path so lib modules can be imported
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.cli_ops import dispatch
from lib.db import Database


# ─── Minimal test spec content ───────────────────────────────────────────────

MINIMAL_SPEC = textwrap.dedent("""\
    # Test Spec

    **Emergency:** true
    **Mode:** execute

    ## Tasks

    ### t-1: Test task
    PENDING

    **Spec:** Write "hello" to /tmp/test-output.txt

    **Verify:** `test -f /tmp/test-output.txt`
""")

SPEC_A_CONTENT = textwrap.dedent("""\
    # Spec A

    **Emergency:** true

    ## Tasks

    ### t-1: Foundation task
    PENDING

    **Spec:** Lay the foundation.

    **Verify:** true
""")

SPEC_B_CONTENT = textwrap.dedent("""\
    # Spec B

    **Emergency:** true

    ## Tasks

    ### t-1: Dependent task
    PENDING

    **Spec:** Build on top of the foundation.

    **Verify:** true
""")


# ─── Test helpers ─────────────────────────────────────────────────────────────


def _make_boi_env(tmpdir: str) -> dict[str, str]:
    """Create a minimal ~/.boi/ layout in tmpdir.

    Returns a dict with:
        root    — base temp dir
        queue   — queue/ subdir (where spec copies land)
        db_path — path to boi.db
    """
    queue_dir = os.path.join(tmpdir, "queue")
    os.makedirs(queue_dir, exist_ok=True)
    return {
        "root": tmpdir,
        "queue": queue_dir,
        "db_path": os.path.join(tmpdir, "boi.db"),
    }


def _write_spec(tmpdir: str, filename: str, content: str) -> str:
    """Write a spec file to tmpdir. Returns absolute path."""
    path = os.path.join(tmpdir, filename)
    Path(path).write_text(content, encoding="utf-8")
    return path


def _make_db(env: dict[str, str]) -> Database:
    """Open the test Database (schema is created on first open)."""
    return Database(env["db_path"], env["queue"])


# ─── Test: dispatch → queued ──────────────────────────────────────────────────


class TestDispatchCreatesQueuedSpec(unittest.TestCase):
    """Dispatching a spec creates a queued database entry and a queue copy."""

    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.env = _make_boi_env(self._tmpdir.name)
        self.spec_path = _write_spec(self._tmpdir.name, "test-spec.md", MINIMAL_SPEC)

    def tearDown(self) -> None:
        self._tmpdir.cleanup()

    def test_dispatch_returns_spec_id(self) -> None:
        result = dispatch(self.env["queue"], self.spec_path)
        self.assertIn("id", result)
        self.assertTrue(result["id"].startswith("q-"))

    def test_dispatch_status_is_queued(self) -> None:
        result = dispatch(self.env["queue"], self.spec_path)
        db = _make_db(self.env)
        try:
            spec = db.get_spec(result["id"])
            self.assertIsNotNone(spec)
            self.assertEqual(spec["status"], "queued")
        finally:
            db.close()

    def test_dispatch_copies_spec_to_queue_dir(self) -> None:
        result = dispatch(self.env["queue"], self.spec_path)
        spec_id = result["id"]
        copy_path = os.path.join(self.env["queue"], f"{spec_id}.spec.md")
        self.assertTrue(
            os.path.isfile(copy_path),
            f"Expected queue copy at {copy_path}",
        )

    def test_dispatch_records_original_spec_path(self) -> None:
        result = dispatch(self.env["queue"], self.spec_path)
        db = _make_db(self.env)
        try:
            spec = db.get_spec(result["id"])
            self.assertEqual(
                spec["original_spec_path"],
                os.path.abspath(self.spec_path),
            )
        finally:
            db.close()

    def test_dispatch_counts_tasks(self) -> None:
        result = dispatch(self.env["queue"], self.spec_path)
        # MINIMAL_SPEC has 1 PENDING task
        self.assertEqual(result["tasks"], 1)
        self.assertEqual(result["pending"], 1)

    def test_dispatch_phase_is_execute(self) -> None:
        result = dispatch(self.env["queue"], self.spec_path)
        self.assertEqual(result["phase"], "execute")

    def test_duplicate_dispatch_raises(self) -> None:
        from lib.db import DuplicateSpecError

        dispatch(self.env["queue"], self.spec_path)
        with self.assertRaises(DuplicateSpecError):
            dispatch(self.env["queue"], self.spec_path)


# ─── Test: status transitions ─────────────────────────────────────────────────


class TestStatusTransitions(unittest.TestCase):
    """Specs can transition through queued → running → completed."""

    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.env = _make_boi_env(self._tmpdir.name)
        self.spec_path = _write_spec(self._tmpdir.name, "spec.md", MINIMAL_SPEC)
        self.db = _make_db(self.env)

    def tearDown(self) -> None:
        self.db.close()
        self._tmpdir.cleanup()

    def _dispatch(self) -> str:
        result = dispatch(self.env["queue"], self.spec_path)
        return result["id"]

    def test_initial_status_is_queued(self) -> None:
        spec_id = self._dispatch()
        spec = self.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "queued")

    def test_transition_to_running(self) -> None:
        spec_id = self._dispatch()

        # Register a fake worker, then assign it so set_running works
        self.db.register_worker("w-1", "/tmp/worktree")
        self.db.update_spec_fields(spec_id, status="assigning")
        self.db.set_running(spec_id, "w-1", phase="execute")

        spec = self.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "running")
        self.assertEqual(spec["phase"], "execute")

    def test_transition_to_completed(self) -> None:
        spec_id = self._dispatch()

        self.db.register_worker("w-1", "/tmp/worktree")
        self.db.update_spec_fields(spec_id, status="assigning")
        self.db.set_running(spec_id, "w-1", phase="execute")
        self.db.complete(spec_id, tasks_done=1, tasks_total=1)

        spec = self.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "completed")
        self.assertEqual(spec["tasks_done"], 1)
        self.assertEqual(spec["tasks_total"], 1)

    def test_full_lifecycle_queued_running_completed(self) -> None:
        spec_id = self._dispatch()

        spec = self.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "queued")

        self.db.register_worker("w-1", "/tmp/worktree")
        self.db.update_spec_fields(spec_id, status="assigning")
        self.db.set_running(spec_id, "w-1")

        spec = self.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "running")

        self.db.complete(spec_id, tasks_done=1, tasks_total=1)

        spec = self.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "completed")

    def test_transition_to_failed(self) -> None:
        spec_id = self._dispatch()
        self.db.fail(spec_id, reason="worker crashed")

        spec = self.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "failed")
        self.assertEqual(spec["failure_reason"], "worker crashed")

    def test_requeue_after_partial_work(self) -> None:
        spec_id = self._dispatch()

        self.db.register_worker("w-1", "/tmp/worktree")
        self.db.update_spec_fields(spec_id, status="assigning")
        self.db.set_running(spec_id, "w-1")
        self.db.requeue(spec_id, tasks_done=0, tasks_total=1)

        spec = self.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "requeued")


# ─── Test: DAG dependencies ───────────────────────────────────────────────────


class TestDagDependencies(unittest.TestCase):
    """Spec B remains blocked until spec A completes."""

    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.env = _make_boi_env(self._tmpdir.name)
        self.spec_a_path = _write_spec(self._tmpdir.name, "spec-a.md", SPEC_A_CONTENT)
        self.spec_b_path = _write_spec(self._tmpdir.name, "spec-b.md", SPEC_B_CONTENT)
        self.db = _make_db(self.env)

    def tearDown(self) -> None:
        self.db.close()
        self._tmpdir.cleanup()

    def test_spec_b_is_not_picked_while_spec_a_is_queued(self) -> None:
        result_a = dispatch(self.env["queue"], self.spec_a_path)
        spec_a_id = result_a["id"]

        result_b = dispatch(
            self.env["queue"],
            self.spec_b_path,
            blocked_by=[spec_a_id],
        )
        spec_b_id = result_b["id"]

        # B depends on A. pick_next_spec should return A, not B.
        self.db.register_worker("w-1", "/tmp/worktree")
        picked = self.db.pick_next_spec()
        self.assertIsNotNone(picked)
        self.assertEqual(picked["id"], spec_a_id, "Should pick A first (B is blocked)")

        # B should still be queued (not picked)
        spec_b = self.db.get_spec(spec_b_id)
        self.assertEqual(spec_b["status"], "queued")

    def test_spec_b_becomes_eligible_after_spec_a_completes(self) -> None:
        result_a = dispatch(self.env["queue"], self.spec_a_path)
        spec_a_id = result_a["id"]

        result_b = dispatch(
            self.env["queue"],
            self.spec_b_path,
            blocked_by=[spec_a_id],
        )
        spec_b_id = result_b["id"]

        self.db.register_worker("w-1", "/tmp/worktree")

        # Complete A
        self.db.update_spec_fields(spec_a_id, status="assigning")
        self.db.set_running(spec_a_id, "w-1")
        self.db.complete(spec_a_id, tasks_done=1, tasks_total=1)

        # Now B should be eligible
        self.db.free_worker("w-1")
        picked = self.db.pick_next_spec()
        self.assertIsNotNone(picked)
        self.assertEqual(picked["id"], spec_b_id, "B should be picked after A completes")

    def test_dependency_recorded_in_db(self) -> None:
        result_a = dispatch(self.env["queue"], self.spec_a_path)
        spec_a_id = result_a["id"]

        result_b = dispatch(
            self.env["queue"],
            self.spec_b_path,
            blocked_by=[spec_a_id],
        )
        spec_b_id = result_b["id"]

        deps = self.db.get_dependencies(spec_b_id)
        self.assertEqual(len(deps), 1)
        self.assertEqual(deps[0]["id"], spec_a_id)

    def test_circular_dependency_raises(self) -> None:
        result_a = dispatch(self.env["queue"], self.spec_a_path)
        spec_a_id = result_a["id"]

        result_b = dispatch(
            self.env["queue"],
            self.spec_b_path,
            blocked_by=[spec_a_id],
        )
        spec_b_id = result_b["id"]

        # Trying to add A → B after B → A is already set would create a cycle
        with self.assertRaises(ValueError, msg="Circular dependency should be detected"):
            self.db.add_dependency(spec_a_id, spec_b_id)

    def test_dependency_cascade_fails_dependent(self) -> None:
        """Failing spec A should cascade-fail spec B."""
        result_a = dispatch(self.env["queue"], self.spec_a_path)
        spec_a_id = result_a["id"]

        result_b = dispatch(
            self.env["queue"],
            self.spec_b_path,
            blocked_by=[spec_a_id],
        )
        spec_b_id = result_b["id"]

        self.db.fail(spec_a_id, reason="upstream error")

        spec_b = self.db.get_spec(spec_b_id)
        self.assertEqual(spec_b["status"], "failed")
        self.assertIn("dependency_failed", spec_b["failure_reason"])


# ─── Test: queue state inspection ────────────────────────────────────────────


class TestQueueStateInspection(unittest.TestCase):
    """get_queue() returns all specs with correct fields after dispatch."""

    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.env = _make_boi_env(self._tmpdir.name)
        self.db = _make_db(self.env)

    def tearDown(self) -> None:
        self.db.close()
        self._tmpdir.cleanup()

    def _dispatch(self, filename: str, content: str = MINIMAL_SPEC) -> str:
        spec_path = _write_spec(self._tmpdir.name, filename, content)
        result = dispatch(self.env["queue"], spec_path)
        return result["id"]

    def test_empty_queue_returns_empty_list(self) -> None:
        self.assertEqual(self.db.get_queue(), [])

    def test_single_dispatched_spec_in_queue(self) -> None:
        spec_id = self._dispatch("a.md")
        queue = self.db.get_queue()
        self.assertEqual(len(queue), 1)
        self.assertEqual(queue[0]["id"], spec_id)

    def test_multiple_specs_all_in_queue(self) -> None:
        ids = [self._dispatch(f"spec-{i}.md") for i in range(3)]
        queue = self.db.get_queue()
        queued_ids = {s["id"] for s in queue}
        for spec_id in ids:
            self.assertIn(spec_id, queued_ids)

    def test_get_spec_returns_none_for_missing(self) -> None:
        self.assertIsNone(self.db.get_spec("q-999"))

    def test_dispatched_spec_has_expected_fields(self) -> None:
        spec_id = self._dispatch("spec.md")
        spec = self.db.get_spec(spec_id)

        required_fields = [
            "id", "status", "phase", "submitted_at",
            "priority", "iteration", "max_iterations",
            "tasks_done", "tasks_total",
        ]
        for field in required_fields:
            self.assertIn(field, spec, f"Missing field: {field}")


if __name__ == "__main__":
    unittest.main()
