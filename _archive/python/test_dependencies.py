# test_dependencies.py — Unit tests for BOI spec-level dependency support.
#
# Tests cover: single/multiple dependency dispatch, circular dependency
# detection, missing dependency validation, pick_next_spec respecting
# dependencies, and failure cascade propagation.
#
# Uses stdlib unittest only (no pytest dependency).

import os
import sys
import tempfile
import unittest
from pathlib import Path

# Add parent directory to path so we can import lib modules
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.db import Database


class DepTestCase(unittest.TestCase):
    """Base test case with a temp Database and helper to create spec files."""

    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.db_path = os.path.join(self._tmpdir.name, "boi.db")
        self.queue_dir = os.path.join(self._tmpdir.name, "queue")
        self.specs_dir = os.path.join(self._tmpdir.name, "specs")
        os.makedirs(self.specs_dir)
        self.db = Database(self.db_path, self.queue_dir)

    def tearDown(self) -> None:
        self.db.close()
        self._tmpdir.cleanup()

    def _make_spec(self, name: str = "spec") -> str:
        """Create a minimal spec file and return its path."""
        path = os.path.join(self.specs_dir, f"{name}.md")
        Path(path).write_text(
            f"# {name}\n\n### t-1: Task\nPENDING\n\n"
            "**Spec:** Do the thing.\n\n**Verify:** true\n",
            encoding="utf-8",
        )
        return path


# ─── Dispatch with Dependencies ───────────────────────────────────────


class TestDispatchWithSingleDependency(DepTestCase):
    """Verify a single dependency is stored in spec_dependencies."""

    def test_dependency_stored_in_db(self) -> None:
        spec_a = self._make_spec("a")
        spec_b = self._make_spec("b")

        result_a = self.db.enqueue(spec_a, queue_id="q-001")
        result_b = self.db.enqueue(spec_b, queue_id="q-002", blocked_by=["q-001"])

        self.assertEqual(result_a["id"], "q-001")
        self.assertEqual(result_b["id"], "q-002")

        # Verify dependency row exists
        rows = self.db.conn.execute(
            "SELECT * FROM spec_dependencies WHERE spec_id = 'q-002'"
        ).fetchall()
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["blocks_on"], "q-001")

    def test_dependent_spec_is_queued(self) -> None:
        spec_a = self._make_spec("a")
        spec_b = self._make_spec("b")

        self.db.enqueue(spec_a, queue_id="q-001")
        result_b = self.db.enqueue(spec_b, queue_id="q-002", blocked_by=["q-001"])

        self.assertEqual(result_b["status"], "queued")


class TestDispatchWithMultipleDependencies(DepTestCase):
    """Verify multiple dependencies are all stored correctly."""

    def test_multiple_deps_stored(self) -> None:
        spec_a = self._make_spec("a")
        spec_b = self._make_spec("b")
        spec_c = self._make_spec("c")

        self.db.enqueue(spec_a, queue_id="q-001")
        self.db.enqueue(spec_b, queue_id="q-002")
        self.db.enqueue(spec_c, queue_id="q-003", blocked_by=["q-001", "q-002"])

        rows = self.db.conn.execute(
            "SELECT blocks_on FROM spec_dependencies WHERE spec_id = 'q-003' "
            "ORDER BY blocks_on"
        ).fetchall()
        self.assertEqual(len(rows), 2)
        self.assertEqual(rows[0]["blocks_on"], "q-001")
        self.assertEqual(rows[1]["blocks_on"], "q-002")


# ─── Circular Dependency Detection ────────────────────────────────────


class TestCircularDependencyDetection(DepTestCase):
    """Verify circular dependencies are rejected at dispatch time.

    The circular detection in db.py uses _find_dependency_path(dep_id, queue_id)
    which checks if dep_id has a transitive path to queue_id in existing edges.
    This catches cycles when queue_id already exists in the dependency graph
    (e.g., re-dispatch with a manually specified queue_id that's already a
    dependency of dep_id).
    """

    def test_find_dependency_path_detects_direct_cycle(self) -> None:
        """Verify _find_dependency_path detects when dep transitively reaches target."""
        spec_a = self._make_spec("a")
        spec_b = self._make_spec("b")

        self.db.enqueue(spec_a, queue_id="q-001")
        self.db.enqueue(spec_b, queue_id="q-002", blocked_by=["q-001"])

        # q-002 depends on q-001, so path from q-002 to q-001 should exist
        path = self.db._find_dependency_path("q-002", "q-001")
        self.assertIsNotNone(path)

    def test_find_dependency_path_detects_transitive_cycle(self) -> None:
        """Verify _find_dependency_path works for multi-hop chains."""
        spec_a = self._make_spec("a")
        spec_b = self._make_spec("b")
        spec_c = self._make_spec("c")

        self.db.enqueue(spec_a, queue_id="q-001")
        self.db.enqueue(spec_b, queue_id="q-002", blocked_by=["q-001"])
        self.db.enqueue(spec_c, queue_id="q-003", blocked_by=["q-002"])

        # q-003 -> q-002 -> q-001: path from q-003 to q-001 should exist
        path = self.db._find_dependency_path("q-003", "q-001")
        self.assertIsNotNone(path)

    def test_find_dependency_path_returns_none_for_no_cycle(self) -> None:
        """No path should exist where there's no dependency chain."""
        spec_a = self._make_spec("a")
        spec_b = self._make_spec("b")

        self.db.enqueue(spec_a, queue_id="q-001")
        self.db.enqueue(spec_b, queue_id="q-002")

        # q-001 and q-002 are independent
        path = self.db._find_dependency_path("q-001", "q-002")
        self.assertIsNone(path)

    def test_linear_chain_no_false_positive(self) -> None:
        """A -> B -> C is not circular; adding D -> C should succeed."""
        spec_a = self._make_spec("a")
        spec_b = self._make_spec("b")
        spec_c = self._make_spec("c")
        spec_d = self._make_spec("d")

        self.db.enqueue(spec_a, queue_id="q-001")
        self.db.enqueue(spec_b, queue_id="q-002", blocked_by=["q-001"])
        self.db.enqueue(spec_c, queue_id="q-003", blocked_by=["q-002"])

        # D -> C is fine (no cycle)
        result = self.db.enqueue(spec_d, queue_id="q-004", blocked_by=["q-003"])
        self.assertEqual(result["id"], "q-004")


# ─── Missing Dependency Validation ────────────────────────────────────


class TestMissingDependencyValidation(DepTestCase):
    """Verify missing dependencies are rejected at dispatch time."""

    def test_nonexistent_dependency_rejected(self) -> None:
        spec = self._make_spec("a")
        with self.assertRaises(ValueError) as ctx:
            self.db.enqueue(spec, queue_id="q-001", blocked_by=["q-999"])
        self.assertIn("not found", str(ctx.exception))

    def test_multiple_missing_deps_listed(self) -> None:
        spec = self._make_spec("a")
        with self.assertRaises(ValueError) as ctx:
            self.db.enqueue(spec, queue_id="q-001", blocked_by=["q-888", "q-999"])
        msg = str(ctx.exception)
        self.assertIn("q-888", msg)
        self.assertIn("q-999", msg)

    def test_failed_dependency_warns_but_allows(self) -> None:
        """A dependency with status 'failed' should warn but still enqueue."""
        spec_a = self._make_spec("a")
        spec_b = self._make_spec("b")

        self.db.enqueue(spec_a, queue_id="q-001")
        self.db.fail("q-001", reason="intentional")

        # Should succeed (with warning to stderr)
        result = self.db.enqueue(spec_b, queue_id="q-002", blocked_by=["q-001"])
        self.assertEqual(result["id"], "q-002")

    def test_canceled_dependency_warns_but_allows(self) -> None:
        """A dependency with status 'canceled' should warn but still enqueue."""
        spec_a = self._make_spec("a")
        spec_b = self._make_spec("b")

        self.db.enqueue(spec_a, queue_id="q-001")
        self.db.cancel("q-001")

        result = self.db.enqueue(spec_b, queue_id="q-002", blocked_by=["q-001"])
        self.assertEqual(result["id"], "q-002")


# ─── pick_next_spec Respects Dependencies ─────────────────────────────


class TestPickNextSpecRespectsDependencies(DepTestCase):
    """Verify daemon doesn't pick specs with unmet dependencies."""

    def test_blocked_spec_not_picked(self) -> None:
        spec_a = self._make_spec("a")
        spec_b = self._make_spec("b")

        self.db.enqueue(spec_a, queue_id="q-001")
        self.db.enqueue(spec_b, queue_id="q-002", blocked_by=["q-001"])

        # Pick should return q-001 (no deps), not q-002 (blocked)
        picked = self.db.pick_next_spec()
        self.assertIsNotNone(picked)
        self.assertEqual(picked["id"], "q-001")

    def test_blocked_spec_skipped_for_unblocked(self) -> None:
        """If q-002 is blocked but q-003 is free, pick q-003."""
        spec_a = self._make_spec("a")
        spec_b = self._make_spec("b")
        spec_c = self._make_spec("c")

        self.db.enqueue(spec_a, queue_id="q-001")
        self.db.enqueue(spec_b, queue_id="q-002", blocked_by=["q-001"])
        self.db.enqueue(spec_c, queue_id="q-003")

        # Pick q-001 first (highest priority by order)
        picked1 = self.db.pick_next_spec()
        self.assertEqual(picked1["id"], "q-001")

        # Now q-001 is 'assigning', pick next: q-002 blocked, q-003 free
        picked2 = self.db.pick_next_spec()
        self.assertIsNotNone(picked2)
        self.assertEqual(picked2["id"], "q-003")

    def test_spec_pickable_after_dependency_completes(self) -> None:
        spec_a = self._make_spec("a")
        spec_b = self._make_spec("b")

        self.db.enqueue(spec_a, queue_id="q-001")
        self.db.enqueue(spec_b, queue_id="q-002", blocked_by=["q-001"])

        # q-002 should NOT be picked while q-001 is queued
        # First pick q-001
        picked = self.db.pick_next_spec()
        self.assertEqual(picked["id"], "q-001")

        # q-002 still blocked (q-001 is 'assigning', not 'completed')
        picked2 = self.db.pick_next_spec()
        self.assertIsNone(picked2)

        # Complete q-001
        self.db.conn.execute("UPDATE specs SET status = 'running' WHERE id = 'q-001'")
        self.db.conn.commit()
        self.db.complete("q-001")

        # Now q-002 should be pickable
        picked3 = self.db.pick_next_spec()
        self.assertIsNotNone(picked3)
        self.assertEqual(picked3["id"], "q-002")

    def test_multiple_deps_all_must_complete(self) -> None:
        spec_a = self._make_spec("a")
        spec_b = self._make_spec("b")
        spec_c = self._make_spec("c")

        self.db.enqueue(spec_a, queue_id="q-001")
        self.db.enqueue(spec_b, queue_id="q-002")
        self.db.enqueue(spec_c, queue_id="q-003", blocked_by=["q-001", "q-002"])

        # Pick q-001 and q-002
        p1 = self.db.pick_next_spec()
        p2 = self.db.pick_next_spec()
        self.assertEqual(p1["id"], "q-001")
        self.assertEqual(p2["id"], "q-002")

        # q-003 blocked (both deps in 'assigning')
        p3 = self.db.pick_next_spec()
        self.assertIsNone(p3)

        # Complete only q-001
        self.db.conn.execute("UPDATE specs SET status = 'running' WHERE id = 'q-001'")
        self.db.conn.commit()
        self.db.complete("q-001")

        # Still blocked (q-002 not completed)
        p4 = self.db.pick_next_spec()
        self.assertIsNone(p4)

        # Complete q-002
        self.db.conn.execute("UPDATE specs SET status = 'running' WHERE id = 'q-002'")
        self.db.conn.commit()
        self.db.complete("q-002")

        # Now q-003 should be pickable
        p5 = self.db.pick_next_spec()
        self.assertIsNotNone(p5)
        self.assertEqual(p5["id"], "q-003")


# ─── Failed Dependency Propagation ────────────────────────────────────


class TestFailedDependencyPropagation(DepTestCase):
    """Verify failure cascades to dependent specs."""

    def test_fail_cascades_to_dependent(self) -> None:
        spec_a = self._make_spec("a")
        spec_b = self._make_spec("b")

        self.db.enqueue(spec_a, queue_id="q-001")
        self.db.enqueue(spec_b, queue_id="q-002", blocked_by=["q-001"])

        # Fail q-001
        self.db.fail("q-001", reason="test failure")

        # q-002 should be failed with dependency_failed reason
        spec_b_row = self.db.get_spec("q-002")
        self.assertEqual(spec_b_row["status"], "failed")
        self.assertIn("dependency_failed", spec_b_row["failure_reason"])
        self.assertIn("q-001", spec_b_row["failure_reason"])

    def test_cancel_cascades_to_dependent(self) -> None:
        spec_a = self._make_spec("a")
        spec_b = self._make_spec("b")

        self.db.enqueue(spec_a, queue_id="q-001")
        self.db.enqueue(spec_b, queue_id="q-002", blocked_by=["q-001"])

        # Cancel q-001
        self.db.cancel("q-001")

        spec_b_row = self.db.get_spec("q-002")
        self.assertEqual(spec_b_row["status"], "failed")
        self.assertIn("dependency_failed", spec_b_row["failure_reason"])
        self.assertIn("canceled", spec_b_row["failure_reason"])

    def test_transitive_failure_cascade(self) -> None:
        """A -> B -> C: failing A should cascade to B and C."""
        spec_a = self._make_spec("a")
        spec_b = self._make_spec("b")
        spec_c = self._make_spec("c")

        self.db.enqueue(spec_a, queue_id="q-001")
        self.db.enqueue(spec_b, queue_id="q-002", blocked_by=["q-001"])
        self.db.enqueue(spec_c, queue_id="q-003", blocked_by=["q-002"])

        self.db.fail("q-001", reason="root cause")

        b = self.db.get_spec("q-002")
        c = self.db.get_spec("q-003")
        self.assertEqual(b["status"], "failed")
        self.assertEqual(c["status"], "failed")
        self.assertIn("q-001", b["failure_reason"])
        self.assertIn("q-002", c["failure_reason"])

    def test_running_spec_not_cascaded(self) -> None:
        """Only queued/requeued specs are cascaded. Running specs are not."""
        spec_a = self._make_spec("a")
        spec_b = self._make_spec("b")

        self.db.enqueue(spec_a, queue_id="q-001")
        self.db.enqueue(spec_b, queue_id="q-002", blocked_by=["q-001"])

        # Set q-002 to running (simulating it was already picked)
        self.db.conn.execute("UPDATE specs SET status = 'running' WHERE id = 'q-002'")
        self.db.conn.commit()

        # Fail q-001
        self.db.fail("q-001", reason="test")

        # q-002 should still be running (not cascaded)
        spec_b_row = self.db.get_spec("q-002")
        self.assertEqual(spec_b_row["status"], "running")

    def test_failure_logged_as_event(self) -> None:
        spec_a = self._make_spec("a")
        spec_b = self._make_spec("b")

        self.db.enqueue(spec_a, queue_id="q-001")
        self.db.enqueue(spec_b, queue_id="q-002", blocked_by=["q-001"])

        self.db.fail("q-001", reason="test")

        events = self.db.conn.execute(
            "SELECT * FROM events WHERE spec_id = 'q-002' "
            "AND event_type = 'dependency_failed'"
        ).fetchall()
        self.assertEqual(len(events), 1)
        self.assertIn("q-001", events[0]["message"])

    def test_no_cascade_without_dependencies(self) -> None:
        """Failing a spec with no dependents should not affect others."""
        spec_a = self._make_spec("a")
        spec_b = self._make_spec("b")

        self.db.enqueue(spec_a, queue_id="q-001")
        self.db.enqueue(spec_b, queue_id="q-002")

        self.db.fail("q-001", reason="test")

        spec_b_row = self.db.get_spec("q-002")
        self.assertEqual(spec_b_row["status"], "queued")


if __name__ == "__main__":
    unittest.main()
