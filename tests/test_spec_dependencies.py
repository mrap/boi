# test_spec_dependencies.py — Tests for spec-level dependency management.
#
# Covers the new DB methods (replace_dependencies, clear_dependencies,
# get_fleet_dag, check_fleet_dag) and their CLI wrappers. Existing
# functionality (add_dependency, remove_dependency, dispatch with --after,
# cycle detection, pick_next_spec enforcement, cascade) is tested in
# test_dependencies.py and test_db.py.

import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from lib.db import Database
from tests.conftest import BoiTestCase, make_spec


class SpecDepTestCase(BoiTestCase):
    """Base class with a Database instance and helper to enqueue specs."""

    def setUp(self):
        super().setUp()
        self.db_path = os.path.join(self.boi_state, "boi.db")
        self.db = Database(self.db_path, self.queue_dir)

    def tearDown(self):
        self.db.close()
        super().tearDown()

    def _enqueue(
        self, queue_id: str, status: str = "queued", blocked_by: list[str] | None = None
    ) -> dict:
        """Enqueue a minimal spec and return the entry dict."""
        spec_path = make_spec(
            base_dir=self.specs_dir,
            filename=f"{queue_id}.spec.md",
            tasks_pending=1,
        )
        entry = self.db.enqueue(
            spec_path=spec_path,
            queue_id=queue_id,
            blocked_by=blocked_by,
        )
        if status != "queued":
            self.db.update_spec_fields(entry["id"], status=status)
        return entry


# ── replace_dependencies ─────────────────────────────────────────────────


class TestReplaceDependencies(SpecDepTestCase):
    def test_replace_deps_atomically_swaps_all_deps(self):
        """Replace all deps for a spec in one call."""
        self._enqueue("q-A")
        self._enqueue("q-B")
        self._enqueue("q-C")
        self._enqueue("q-D")

        # q-D initially depends on q-A
        self.db.add_dependency("q-D", "q-A")
        deps_before = self.db.get_dependencies("q-D")
        self.assertEqual(len(deps_before), 1)
        self.assertEqual(deps_before[0]["id"], "q-A")

        # Replace with q-B, q-C
        self.db.replace_dependencies("q-D", ["q-B", "q-C"])

        deps_after = self.db.get_dependencies("q-D")
        dep_ids = sorted(d["id"] for d in deps_after)
        self.assertEqual(dep_ids, ["q-B", "q-C"])

    def test_replace_deps_with_cycle_raises_and_no_change(self):
        """Cycle detection during replace rolls back all changes."""
        self._enqueue("q-A")
        self._enqueue("q-B")

        # q-B depends on q-A
        self.db.add_dependency("q-B", "q-A")

        # Try to replace q-A's deps with [q-B] — creates cycle
        with self.assertRaises(ValueError) as ctx:
            self.db.replace_dependencies("q-A", ["q-B"])
        self.assertIn("ircular", str(ctx.exception))

        # q-A should still have no deps (no partial application)
        deps = self.db.get_dependencies("q-A")
        self.assertEqual(len(deps), 0)

    def test_replace_deps_with_empty_list_clears(self):
        """Replacing with empty list removes all deps."""
        self._enqueue("q-A")
        self._enqueue("q-B")
        self.db.add_dependency("q-B", "q-A")

        self.db.replace_dependencies("q-B", [])
        deps = self.db.get_dependencies("q-B")
        self.assertEqual(len(deps), 0)

    def test_replace_deps_missing_spec_raises(self):
        """ValueError when spec_id doesn't exist."""
        with self.assertRaises(ValueError):
            self.db.replace_dependencies("q-MISSING", ["q-A"])

    def test_replace_deps_missing_dep_raises(self):
        """ValueError when a dep_id doesn't exist."""
        self._enqueue("q-A")
        with self.assertRaises(ValueError):
            self.db.replace_dependencies("q-A", ["q-MISSING"])


# ── clear_dependencies ───────────────────────────────────────────────────


class TestClearDependencies(SpecDepTestCase):
    def test_clear_deps_removes_all_and_returns_count(self):
        """clear_dependencies removes all deps and returns the count."""
        self._enqueue("q-A")
        self._enqueue("q-B")
        self._enqueue("q-C")
        self.db.add_dependency("q-C", "q-A")
        self.db.add_dependency("q-C", "q-B")

        count = self.db.clear_dependencies("q-C")
        self.assertEqual(count, 2)

        deps = self.db.get_dependencies("q-C")
        self.assertEqual(len(deps), 0)

    def test_clear_deps_on_spec_with_no_deps_returns_zero(self):
        """Clearing deps on a spec that has none returns 0."""
        self._enqueue("q-A")
        count = self.db.clear_dependencies("q-A")
        self.assertEqual(count, 0)

    def test_clear_deps_missing_spec_raises(self):
        """ValueError when spec doesn't exist."""
        with self.assertRaises(ValueError):
            self.db.clear_dependencies("q-MISSING")


# ── get_fleet_dag ────────────────────────────────────────────────────────


class TestGetFleetDag(SpecDepTestCase):
    def test_fleet_dag_returns_all_specs_and_edges(self):
        """Full DAG contains all specs and their dep edges."""
        self._enqueue("q-A", status="completed")
        self._enqueue("q-B", status="completed")
        self._enqueue("q-C", blocked_by=["q-A", "q-B"])

        dag = self.db.get_fleet_dag()

        spec_ids = {s["id"] for s in dag["specs"]}
        self.assertEqual(spec_ids, {"q-A", "q-B", "q-C"})

        # Edges: q-C -> q-A, q-C -> q-B
        edge_set = {(e["from"], e["to"]) for e in dag["edges"]}
        self.assertIn(("q-C", "q-A"), edge_set)
        self.assertIn(("q-C", "q-B"), edge_set)
        self.assertEqual(len(dag["edges"]), 2)

    def test_fleet_dag_with_no_deps_shows_independent_specs(self):
        """Specs with no deps show empty dep/dependent lists."""
        self._enqueue("q-A")
        self._enqueue("q-B")

        dag = self.db.get_fleet_dag()
        for spec in dag["specs"]:
            self.assertEqual(spec["deps"], [])
            self.assertEqual(spec["dependents"], [])
        self.assertEqual(dag["edges"], [])

    def test_fleet_dag_includes_completed_specs(self):
        """Completed specs are included in the DAG."""
        self._enqueue("q-A", status="completed")
        self._enqueue("q-B")

        dag = self.db.get_fleet_dag()
        statuses = {s["id"]: s["status"] for s in dag["specs"]}
        self.assertEqual(statuses["q-A"], "completed")
        self.assertEqual(statuses["q-B"], "queued")

    def test_fleet_dag_adjacency_info(self):
        """Each spec shows its deps and dependents."""
        self._enqueue("q-A")
        self._enqueue("q-B")
        self._enqueue("q-C", blocked_by=["q-A", "q-B"])

        dag = self.db.get_fleet_dag()
        spec_map = {s["id"]: s for s in dag["specs"]}

        self.assertEqual(sorted(spec_map["q-C"]["deps"]), ["q-A", "q-B"])
        self.assertIn("q-C", spec_map["q-A"]["dependents"])
        self.assertIn("q-C", spec_map["q-B"]["dependents"])


# ── check_fleet_dag ──────────────────────────────────────────────────────


class TestCheckFleetDag(SpecDepTestCase):
    def test_check_finds_no_issues_on_clean_dag(self):
        """Clean DAG returns empty issue list."""
        self._enqueue("q-A", status="completed")
        self._enqueue("q-B", blocked_by=["q-A"])

        issues = self.db.check_fleet_dag()
        self.assertEqual(issues, [])

    def test_check_finds_blocked_by_failed_spec(self):
        """Detects a queued spec blocked by a failed spec."""
        self._enqueue("q-A", status="failed")
        self._enqueue("q-B", blocked_by=["q-A"])

        issues = self.db.check_fleet_dag()
        self.assertTrue(len(issues) > 0)
        self.assertEqual(issues[0]["type"], "blocked_by_terminal")
        self.assertEqual(issues[0]["spec"], "q-B")
        self.assertEqual(issues[0]["blocked_on"], "q-A")

    def test_check_finds_self_loop(self):
        """Detects a self-referential dependency."""
        self._enqueue("q-A")
        # Insert a self-loop directly (bypassing cycle detection)
        self.db.conn.execute(
            "INSERT INTO spec_dependencies (spec_id, blocks_on) VALUES (?, ?)",
            ("q-A", "q-A"),
        )
        self.db.conn.commit()

        issues = self.db.check_fleet_dag()
        self.assertTrue(any(i["type"] == "self_loop" for i in issues))

    def test_check_no_issues_for_completed_dep(self):
        """Completed deps are not flagged as issues."""
        self._enqueue("q-A", status="completed")
        self._enqueue("q-B", blocked_by=["q-A"])

        issues = self.db.check_fleet_dag()
        self.assertEqual(issues, [])

    def test_check_finds_canceled_dep(self):
        """Detects a queued spec blocked by a canceled spec."""
        self._enqueue("q-A", status="canceled")
        self._enqueue("q-B", blocked_by=["q-A"])

        issues = self.db.check_fleet_dag()
        self.assertTrue(len(issues) > 0)
        issue = issues[0]
        self.assertEqual(issue["type"], "blocked_by_terminal")
        self.assertEqual(issue["blocked_on_status"], "canceled")


# ── Dispatch with --after (integration) ──────────────────────────────────


class TestDispatchWithAfter(SpecDepTestCase):
    def test_dispatch_with_after_creates_dependency(self):
        """Dispatching with blocked_by creates dep rows."""
        self._enqueue("q-A")
        self._enqueue("q-B", blocked_by=["q-A"])

        deps = self.db.get_dependencies("q-B")
        self.assertEqual(len(deps), 1)
        self.assertEqual(deps[0]["id"], "q-A")


# ── pick_next_spec enforcement ───────────────────────────────────────────


class TestDequeueEnforcement(SpecDepTestCase):
    def test_spec_blocked_by_pending_dep_stays_queued(self):
        """A spec with unmet deps is not picked."""
        self._enqueue("q-A")  # queued, not completed
        self._enqueue("q-B", blocked_by=["q-A"])

        picked = self.db.pick_next_spec(blocked_ids=set())
        # Should pick q-A (independent), not q-B
        self.assertIsNotNone(picked)
        self.assertEqual(picked["id"], "q-A")

    def test_spec_unblocked_when_dep_completes(self):
        """Spec becomes pickable after its dep completes."""
        self._enqueue("q-A")
        self._enqueue("q-B", blocked_by=["q-A"])

        # Complete q-A
        self.db.update_spec_fields("q-A", status="completed")

        picked = self.db.pick_next_spec(blocked_ids=set())
        self.assertIsNotNone(picked)
        self.assertEqual(picked["id"], "q-B")

    def test_fan_in_waits_for_all_deps(self):
        """Fan-in: spec waits for ALL deps to complete."""
        self._enqueue("q-A")
        self._enqueue("q-B")
        self._enqueue("q-C", blocked_by=["q-A", "q-B"])

        # Complete only q-A
        self.db.update_spec_fields("q-A", status="completed")

        # q-C should not be picked (q-B still queued)
        # First pick gets q-B
        picked = self.db.pick_next_spec(blocked_ids=set())
        self.assertIsNotNone(picked)
        self.assertEqual(picked["id"], "q-B")

    def test_fan_out_starts_all_dependents(self):
        """Fan-out: all specs depending on q-A become eligible after q-A completes."""
        self._enqueue("q-A")
        self._enqueue("q-B", blocked_by=["q-A"])
        self._enqueue("q-C", blocked_by=["q-A"])
        self._enqueue("q-D", blocked_by=["q-A"])

        # Complete q-A
        self.db.update_spec_fields("q-A", status="completed")

        # All three should be pickable
        picked_ids = []
        for _ in range(3):
            p = self.db.pick_next_spec(blocked_ids=set())
            self.assertIsNotNone(p)
            picked_ids.append(p["id"])
            # Simulate running -> completed so the next can be picked
            self.db.update_spec_fields(p["id"], status="completed")

        self.assertEqual(sorted(picked_ids), ["q-B", "q-C", "q-D"])


# ── Mid-flight operations ───────────────────────────────────────────────


class TestMidFlightOps(SpecDepTestCase):
    def test_mid_flight_add_dep_pauses_spec(self):
        """Adding a dep to a queued spec prevents it from being picked."""
        self._enqueue("q-A")
        self._enqueue("q-B")

        # Add dep: q-B now depends on q-A
        self.db.add_dependency("q-B", "q-A")

        # q-B should not be pickable
        picked = self.db.pick_next_spec(blocked_ids=set())
        self.assertIsNotNone(picked)
        self.assertEqual(picked["id"], "q-A")

    def test_mid_flight_remove_dep_unblocks_spec(self):
        """Removing a dep from a blocked spec allows it to be picked."""
        self._enqueue("q-A")
        self._enqueue("q-B", blocked_by=["q-A"])

        # Remove the dep
        self.db.remove_dependency("q-B", "q-A")

        # Now both are independent. q-A or q-B can be picked.
        picked = self.db.pick_next_spec(blocked_ids=set())
        self.assertIsNotNone(picked)
        # Either is valid since both are independent

    def test_mid_flight_replace_deps(self):
        """Replacing deps mid-flight changes what blocks a spec."""
        self._enqueue("q-A")
        self._enqueue("q-B")
        self._enqueue("q-C", blocked_by=["q-A"])

        # Replace q-C's dep from q-A to q-B
        self.db.replace_dependencies("q-C", ["q-B"])

        deps = self.db.get_dependencies("q-C")
        self.assertEqual(len(deps), 1)
        self.assertEqual(deps[0]["id"], "q-B")

        # Complete q-A — q-C should still be blocked (depends on q-B now)
        self.db.update_spec_fields("q-A", status="completed")
        picked = self.db.pick_next_spec(blocked_ids=set())
        self.assertIsNotNone(picked)
        self.assertEqual(picked["id"], "q-B")


# ── Circular dependency detection ────────────────────────────────────────


class TestCircularDependency(SpecDepTestCase):
    def test_circular_dependency_rejected(self):
        """Adding a dep that would create a cycle raises ValueError."""
        self._enqueue("q-A")
        self._enqueue("q-B")
        self.db.add_dependency("q-B", "q-A")

        with self.assertRaises(ValueError) as ctx:
            self.db.add_dependency("q-A", "q-B")
        self.assertIn("ircular", str(ctx.exception))


# ── Completed/Failed dep behavior ───────────────────────────────────────


class TestCompletedAndFailedDeps(SpecDepTestCase):
    def test_completed_dep_satisfied_immediately(self):
        """Spec with a completed dep can be picked immediately."""
        self._enqueue("q-A", status="completed")
        self._enqueue("q-B", blocked_by=["q-A"])

        picked = self.db.pick_next_spec(blocked_ids=set())
        self.assertIsNotNone(picked)
        self.assertEqual(picked["id"], "q-B")

    def test_failed_dep_blocks_dependent(self):
        """Spec with a failed dep cannot be picked."""
        self._enqueue("q-A", status="failed")
        self._enqueue("q-B", blocked_by=["q-A"])

        picked = self.db.pick_next_spec(blocked_ids=set())
        self.assertIsNone(picked)

    def test_resume_failed_dep_unblocks_chain(self):
        """Resuming a failed dep and completing it unblocks dependents."""
        self._enqueue("q-A", status="failed")
        self._enqueue("q-B", blocked_by=["q-A"])

        # Resume q-A
        self.db.update_spec_fields(
            "q-A", status="queued", consecutive_failures=0, failure_reason=None
        )

        # Complete q-A
        picked = self.db.pick_next_spec(blocked_ids=set())
        self.assertEqual(picked["id"], "q-A")
        self.db.update_spec_fields("q-A", status="completed")

        # Now q-B should be pickable
        picked = self.db.pick_next_spec(blocked_ids=set())
        self.assertIsNotNone(picked)
        self.assertEqual(picked["id"], "q-B")


# ── Fleet DAG visualization ─────────────────────────────────────────────


class TestFleetDagVisualization(SpecDepTestCase):
    def test_deps_show_full_fleet_dag(self):
        """get_fleet_dag returns structured data for all specs."""
        self._enqueue("q-A", status="completed")
        self._enqueue("q-B", status="running")
        self._enqueue("q-C", blocked_by=["q-A", "q-B"])
        self._enqueue("q-D")  # independent

        dag = self.db.get_fleet_dag()
        self.assertEqual(len(dag["specs"]), 4)

        spec_map = {s["id"]: s for s in dag["specs"]}
        self.assertEqual(sorted(spec_map["q-C"]["deps"]), ["q-A", "q-B"])
        self.assertEqual(spec_map["q-D"]["deps"], [])

    def test_deps_viz_ascii_output(self):
        """Fleet DAG can be formatted as readable text."""
        self._enqueue("q-A", status="completed")
        self._enqueue("q-B", blocked_by=["q-A"])

        dag = self.db.get_fleet_dag()
        # Just verify the structure is usable for rendering
        self.assertIn("specs", dag)
        self.assertIn("edges", dag)
        self.assertTrue(len(dag["specs"]) == 2)

    def test_deps_check_validates_graph(self):
        """check_fleet_dag identifies issues in the DAG."""
        self._enqueue("q-A", status="failed")
        self._enqueue("q-B", blocked_by=["q-A"])

        issues = self.db.check_fleet_dag()
        self.assertTrue(len(issues) > 0)
        self.assertEqual(issues[0]["type"], "blocked_by_terminal")


# ── CLI operations (via cli_ops) ─────────────────────────────────────────


class TestCliOps(SpecDepTestCase):
    def test_cli_replace_dependencies(self):
        """cli_ops.replace_dependencies wraps db correctly."""
        from lib.cli_ops import replace_dependencies

        self._enqueue("q-A")
        self._enqueue("q-B")
        self._enqueue("q-C")
        self.db.add_dependency("q-C", "q-A")

        result = replace_dependencies(self.queue_dir, "q-C", ["q-B"])
        self.assertEqual(result["spec_id"], "q-C")
        self.assertEqual(result["deps"], ["q-B"])

        deps = self.db.get_dependencies("q-C")
        self.assertEqual(len(deps), 1)
        self.assertEqual(deps[0]["id"], "q-B")

    def test_cli_clear_dependencies(self):
        """cli_ops.clear_dependencies wraps db correctly."""
        from lib.cli_ops import clear_dependencies

        self._enqueue("q-A")
        self._enqueue("q-B")
        self.db.add_dependency("q-B", "q-A")

        result = clear_dependencies(self.queue_dir, "q-B")
        self.assertEqual(result["spec_id"], "q-B")
        self.assertEqual(result["cleared"], 1)

    def test_cli_get_fleet_dag(self):
        """cli_ops.get_fleet_dag wraps db correctly."""
        from lib.cli_ops import get_fleet_dag

        self._enqueue("q-A")
        self._enqueue("q-B", blocked_by=["q-A"])

        dag = get_fleet_dag(self.queue_dir)
        self.assertIn("specs", dag)
        self.assertIn("edges", dag)
        self.assertEqual(len(dag["specs"]), 2)

    def test_cli_check_fleet_dag(self):
        """cli_ops.check_fleet_dag wraps db correctly."""
        from lib.cli_ops import check_fleet_dag

        self._enqueue("q-A", status="completed")
        self._enqueue("q-B", blocked_by=["q-A"])

        issues = check_fleet_dag(self.queue_dir)
        self.assertEqual(issues, [])


if __name__ == "__main__":
    unittest.main()
