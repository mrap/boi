# test_fleet_dag_integration.py — End-to-end integration test for the
# spec-level dependency system. Exercises the full fleet DAG lifecycle
# including fan-in, mid-flight restructuring, and DAG visualization.
#
# Corresponds to q-023 t-5.

import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from lib.db import Database
from tests.conftest import BoiTestCase, make_spec


class TestFleetDagLifecycle(BoiTestCase):
    """Full lifecycle test of spec-level dependencies.

    Simulates:
      spec-A (independent)
      spec-B (independent)
      spec-C depends on spec-A, spec-B (fan-in)
      spec-D depends on spec-C (linear)

    Then exercises mid-flight add/remove of dependencies.
    """

    def setUp(self):
        super().setUp()
        self.db_path = os.path.join(self.boi_state, "boi.db")
        self.db = Database(self.db_path, self.queue_dir)

    def tearDown(self):
        self.db.close()
        super().tearDown()

    def _enqueue(self, qid, status="queued", blocked_by=None):
        spec_path = make_spec(
            base_dir=self.specs_dir,
            filename=f"{qid}.spec.md",
            tasks_pending=1,
        )
        entry = self.db.enqueue(
            spec_path=spec_path, queue_id=qid, blocked_by=blocked_by
        )
        if status != "queued":
            self.db.update_spec_fields(entry["id"], status=status)
        return entry

    def _dag_summary(self):
        """Return a compact summary of the current DAG state."""
        dag = self.db.get_fleet_dag()
        specs = {s["id"]: s["status"] for s in dag["specs"]}
        edges = [(e["from"], e["to"]) for e in dag["edges"]]
        return specs, edges

    # ── Step 1: Dispatch 4 specs with DAG ────────────────────────────────

    def test_step1_dispatch_dag(self):
        """Dispatch spec-A, spec-B (independent), spec-C (fan-in), spec-D (linear)."""
        self._enqueue("spec-A")
        self._enqueue("spec-B")
        self._enqueue("spec-C", blocked_by=["spec-A", "spec-B"])
        self._enqueue("spec-D", blocked_by=["spec-C"])

        dag = self.db.get_fleet_dag()
        self.assertEqual(len(dag["specs"]), 4)

        spec_map = {s["id"]: s for s in dag["specs"]}
        self.assertEqual(sorted(spec_map["spec-C"]["deps"]), ["spec-A", "spec-B"])
        self.assertEqual(spec_map["spec-D"]["deps"], ["spec-C"])
        self.assertEqual(spec_map["spec-A"]["deps"], [])
        self.assertEqual(spec_map["spec-B"]["deps"], [])

        edges = {(e["from"], e["to"]) for e in dag["edges"]}
        self.assertEqual(
            edges,
            {
                ("spec-C", "spec-A"),
                ("spec-C", "spec-B"),
                ("spec-D", "spec-C"),
            },
        )

    # ── Step 2: spec-C stays queued while A and B run ────────────────────

    def test_step2_fan_in_stays_queued(self):
        """spec-C cannot be picked while both A and B are still queued/running."""
        self._enqueue("spec-A")
        self._enqueue("spec-B")
        self._enqueue("spec-C", blocked_by=["spec-A", "spec-B"])
        self._enqueue("spec-D", blocked_by=["spec-C"])

        # Pick should return spec-A or spec-B, never spec-C or spec-D
        picked1 = self.db.pick_next_spec(blocked_ids=set())
        self.assertIn(picked1["id"], ("spec-A", "spec-B"))

        picked2 = self.db.pick_next_spec(blocked_ids={picked1["id"]})
        other = "spec-B" if picked1["id"] == "spec-A" else "spec-A"
        self.assertEqual(picked2["id"], other)

        # No more pickable specs
        picked3 = self.db.pick_next_spec(blocked_ids={picked1["id"], picked2["id"]})
        self.assertIsNone(picked3)

    # ── Step 3: Complete spec-A, spec-C still queued ─────────────────────

    def test_step3_partial_fan_in_still_blocked(self):
        """After completing spec-A, spec-C is still blocked (waiting on spec-B)."""
        self._enqueue("spec-A")
        self._enqueue("spec-B")
        self._enqueue("spec-C", blocked_by=["spec-A", "spec-B"])
        self._enqueue("spec-D", blocked_by=["spec-C"])

        self.db.update_spec_fields("spec-A", status="completed")

        # spec-B is pickable, spec-C is still blocked
        picked = self.db.pick_next_spec(blocked_ids=set())
        self.assertEqual(picked["id"], "spec-B")

        # With spec-B blocked (running), nothing else is pickable
        picked2 = self.db.pick_next_spec(blocked_ids={"spec-B"})
        self.assertIsNone(picked2)

    # ── Step 4: Mid-flight remove dep, spec-C starts ─────────────────────

    def test_step4_mid_flight_remove_dep_unblocks(self):
        """Remove spec-C's dependency on spec-B. spec-C starts immediately."""
        self._enqueue("spec-A")
        self._enqueue("spec-B")
        self._enqueue("spec-C", blocked_by=["spec-A", "spec-B"])
        self._enqueue("spec-D", blocked_by=["spec-C"])

        self.db.update_spec_fields("spec-A", status="completed")

        # Remove spec-C -> spec-B dependency
        self.db.remove_dependency("spec-C", "spec-B")

        deps = self.db.get_dependencies("spec-C")
        dep_ids = [d["id"] for d in deps]
        self.assertNotIn("spec-B", dep_ids)

        # spec-C's only remaining dep (spec-A) is completed, so spec-C is pickable
        # Use blocked_ids to avoid pick_next_spec mutating status
        picked1 = self.db.pick_next_spec(blocked_ids=set())
        self.assertIsNotNone(picked1)
        first = picked1["id"]
        self.assertIn(first, ("spec-B", "spec-C"))

        picked2 = self.db.pick_next_spec(blocked_ids={first})
        self.assertIsNotNone(picked2)
        second = picked2["id"]
        self.assertIn(second, ("spec-B", "spec-C"))
        self.assertNotEqual(first, second)

        # spec-D is still blocked on spec-C
        picked3 = self.db.pick_next_spec(blocked_ids={first, second})
        self.assertIsNone(picked3)

    # ── Step 5: Complete spec-C, spec-D starts ───────────────────────────

    def test_step5_linear_chain_unblocks(self):
        """After spec-C completes, spec-D is unblocked."""
        self._enqueue("spec-A")
        self._enqueue("spec-B")
        self._enqueue("spec-C", blocked_by=["spec-A", "spec-B"])
        self._enqueue("spec-D", blocked_by=["spec-C"])

        self.db.update_spec_fields("spec-A", status="completed")
        self.db.remove_dependency("spec-C", "spec-B")
        self.db.update_spec_fields("spec-C", status="completed")

        # spec-D should now be pickable (spec-B is also still queued)
        picked1 = self.db.pick_next_spec(blocked_ids=set())
        self.assertIsNotNone(picked1)
        self.assertIn(picked1["id"], ("spec-B", "spec-D"))

        picked2 = self.db.pick_next_spec(blocked_ids={picked1["id"]})
        self.assertIsNotNone(picked2)
        self.assertEqual({picked1["id"], picked2["id"]}, {"spec-B", "spec-D"})

    # ── Step 6: Mid-flight add dep to spec-D ─────────────────────────────

    def test_step6_mid_flight_add_dep_pauses(self):
        """Add dependency spec-D -> spec-B mid-flight. spec-D pauses."""
        self._enqueue("spec-A")
        self._enqueue("spec-B")
        self._enqueue("spec-C", blocked_by=["spec-A", "spec-B"])
        self._enqueue("spec-D", blocked_by=["spec-C"])

        self.db.update_spec_fields("spec-A", status="completed")
        self.db.remove_dependency("spec-C", "spec-B")
        self.db.update_spec_fields("spec-C", status="completed")

        # Add dep: spec-D now also depends on spec-B
        self.db.add_dependency("spec-D", "spec-B")

        deps = self.db.get_dependencies("spec-D")
        dep_ids = sorted(d["id"] for d in deps)
        self.assertIn("spec-B", dep_ids)

        # spec-D should NOT be pickable (spec-B still queued)
        # Only spec-B should be pickable
        picked = self.db.pick_next_spec(blocked_ids=set())
        self.assertEqual(picked["id"], "spec-B")

        picked2 = self.db.pick_next_spec(blocked_ids={"spec-B"})
        self.assertIsNone(picked2)

    # ── Step 7: Complete spec-B, spec-D resumes ──────────────────────────

    def test_step7_completing_added_dep_unblocks(self):
        """Complete spec-B. spec-D (which gained a dep on spec-B) resumes."""
        self._enqueue("spec-A")
        self._enqueue("spec-B")
        self._enqueue("spec-C", blocked_by=["spec-A", "spec-B"])
        self._enqueue("spec-D", blocked_by=["spec-C"])

        self.db.update_spec_fields("spec-A", status="completed")
        self.db.remove_dependency("spec-C", "spec-B")
        self.db.update_spec_fields("spec-C", status="completed")
        self.db.add_dependency("spec-D", "spec-B")

        # Complete spec-B
        self.db.update_spec_fields("spec-B", status="completed")

        # Now spec-D should be pickable (all deps completed)
        picked = self.db.pick_next_spec(blocked_ids=set())
        self.assertIsNotNone(picked)
        self.assertEqual(picked["id"], "spec-D")

    # ── Step 8: DAG visualization at each step ───────────────────────────

    def test_step8_dag_viz_at_each_step(self):
        """get_fleet_dag and check_fleet_dag work at every state transition."""
        # Initial dispatch
        self._enqueue("spec-A")
        self._enqueue("spec-B")
        self._enqueue("spec-C", blocked_by=["spec-A", "spec-B"])
        self._enqueue("spec-D", blocked_by=["spec-C"])

        dag = self.db.get_fleet_dag()
        self.assertEqual(len(dag["specs"]), 4)
        self.assertEqual(len(dag["edges"]), 3)
        issues = self.db.check_fleet_dag()
        self.assertEqual(issues, [])

        # After completing spec-A
        self.db.update_spec_fields("spec-A", status="completed")
        dag = self.db.get_fleet_dag()
        statuses = {s["id"]: s["status"] for s in dag["specs"]}
        self.assertEqual(statuses["spec-A"], "completed")
        self.assertEqual(statuses["spec-C"], "queued")
        issues = self.db.check_fleet_dag()
        self.assertEqual(issues, [])

        # After removing spec-C -> spec-B dep
        self.db.remove_dependency("spec-C", "spec-B")
        dag = self.db.get_fleet_dag()
        edges = {(e["from"], e["to"]) for e in dag["edges"]}
        self.assertNotIn(("spec-C", "spec-B"), edges)
        self.assertIn(("spec-C", "spec-A"), edges)
        self.assertIn(("spec-D", "spec-C"), edges)

        # After completing spec-C
        self.db.update_spec_fields("spec-C", status="completed")
        dag = self.db.get_fleet_dag()
        statuses = {s["id"]: s["status"] for s in dag["specs"]}
        self.assertEqual(statuses["spec-C"], "completed")

        # After adding spec-D -> spec-B dep
        self.db.add_dependency("spec-D", "spec-B")
        dag = self.db.get_fleet_dag()
        edges = {(e["from"], e["to"]) for e in dag["edges"]}
        self.assertIn(("spec-D", "spec-B"), edges)
        self.assertIn(("spec-D", "spec-C"), edges)

        # After completing spec-B
        self.db.update_spec_fields("spec-B", status="completed")
        dag = self.db.get_fleet_dag()
        statuses = {s["id"]: s["status"] for s in dag["specs"]}
        self.assertEqual(statuses["spec-B"], "completed")
        issues = self.db.check_fleet_dag()
        self.assertEqual(issues, [])

        # After completing spec-D (everything done)
        self.db.update_spec_fields("spec-D", status="completed")
        dag = self.db.get_fleet_dag()
        for s in dag["specs"]:
            self.assertEqual(s["status"], "completed")

    # ── Full lifecycle in a single test ──────────────────────────────────

    def _pickable_ids(self):
        """Return set of spec IDs that would be picked, without mutating state.

        Uses blocked_ids to simulate multiple picks without changing status.
        Rolls back any status changes by restoring picked specs to queued.
        """
        ids = set()
        picked = self.db.pick_next_spec(blocked_ids=set())
        while picked is not None:
            ids.add(picked["id"])
            # Restore to queued so future calls in this test work
            self.db.update_spec_fields(picked["id"], status="queued")
            picked = self.db.pick_next_spec(blocked_ids=ids)
        return ids

    def test_full_lifecycle(self):
        """Complete lifecycle covering all 8 steps from the spec."""
        # Step 1: Dispatch DAG
        self._enqueue("spec-A")
        self._enqueue("spec-B")
        self._enqueue("spec-C", blocked_by=["spec-A", "spec-B"])
        self._enqueue("spec-D", blocked_by=["spec-C"])

        dag = self.db.get_fleet_dag()
        self.assertEqual(len(dag["specs"]), 4)
        self.assertEqual(len(dag["edges"]), 3)

        # Step 2: spec-C stays queued while A and B run
        pickable = self._pickable_ids()
        self.assertEqual(pickable, {"spec-A", "spec-B"})

        # Step 3: Complete spec-A. spec-C still queued (waiting on B)
        self.db.update_spec_fields("spec-A", status="completed")
        pickable = self._pickable_ids()
        self.assertEqual(pickable, {"spec-B"})

        # Step 4: Remove spec-C -> spec-B. spec-C starts immediately.
        self.db.remove_dependency("spec-C", "spec-B")
        pickable = self._pickable_ids()
        self.assertIn("spec-B", pickable)
        self.assertIn("spec-C", pickable)
        self.assertNotIn("spec-D", pickable)

        # Step 5: Complete spec-C. spec-D starts.
        self.db.update_spec_fields("spec-C", status="completed")
        pickable = self._pickable_ids()
        self.assertIn("spec-D", pickable)
        self.assertIn("spec-B", pickable)

        # Step 6: Add dep spec-D -> spec-B. spec-D pauses.
        self.db.add_dependency("spec-D", "spec-B")
        pickable = self._pickable_ids()
        self.assertEqual(pickable, {"spec-B"})

        # Step 7: Complete spec-B. spec-D resumes.
        self.db.update_spec_fields("spec-B", status="completed")
        pickable = self._pickable_ids()
        self.assertEqual(pickable, {"spec-D"})

        # Step 8: DAG shows final state
        dag = self.db.get_fleet_dag()
        edges = {(e["from"], e["to"]) for e in dag["edges"]}
        self.assertIn(("spec-C", "spec-A"), edges)
        self.assertIn(("spec-D", "spec-C"), edges)
        self.assertIn(("spec-D", "spec-B"), edges)
        self.assertNotIn(("spec-C", "spec-B"), edges)  # was removed

        issues = self.db.check_fleet_dag()
        self.assertEqual(issues, [])


if __name__ == "__main__":
    unittest.main()
