# test_sort_modes.py — Tests for dashboard sort modes.

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from lib.status import (
    _sort_by_dag,
    _sort_by_name,
    _sort_by_progress,
    _sort_by_queue,
    _sort_by_recent,
    _sort_by_status,
    build_queue_status,
    format_dashboard,
    sort_entries,
)


def _make_entry(
    qid: str,
    status: str = "queued",
    spec_name: str = "test-spec",
    tasks_done: int = 0,
    tasks_total: int = 5,
    blocked_by: list[str] | None = None,
    last_iteration_at: str = "",
    mode: str = "execute",
    priority: int = 10,
) -> dict:
    return {
        "id": qid,
        "status": status,
        "original_spec_path": f"/specs/{spec_name}.md",
        "spec_path": f"/queue/{qid}.spec.md",
        "tasks_done": tasks_done,
        "tasks_total": tasks_total,
        "blocked_by": blocked_by or [],
        "last_iteration_at": last_iteration_at,
        "mode": mode,
        "priority": priority,
        "iteration": 1,
        "max_iterations": 30,
        "last_worker": None,
    }


class TestSortByQueue(unittest.TestCase):
    def test_sorts_by_queue_id(self):
        entries = [
            _make_entry("q-003"),
            _make_entry("q-001"),
            _make_entry("q-002"),
        ]
        result = _sort_by_queue(entries)
        ids = [e["id"] for e in result]
        self.assertEqual(ids, ["q-001", "q-002", "q-003"])

    def test_empty_list(self):
        self.assertEqual(_sort_by_queue([]), [])


class TestSortByStatus(unittest.TestCase):
    def test_running_first_then_queued_then_completed(self):
        entries = [
            _make_entry("q-001", status="completed"),
            _make_entry("q-002", status="running"),
            _make_entry("q-003", status="queued"),
        ]
        result = _sort_by_status(entries)
        statuses = [e["status"] for e in result]
        self.assertEqual(statuses, ["running", "queued", "completed"])

    def test_within_same_status_sorts_by_id(self):
        entries = [
            _make_entry("q-003", status="running"),
            _make_entry("q-001", status="running"),
            _make_entry("q-002", status="running"),
        ]
        result = _sort_by_status(entries)
        ids = [e["id"] for e in result]
        self.assertEqual(ids, ["q-001", "q-002", "q-003"])

    def test_all_status_groups(self):
        entries = [
            _make_entry("q-006", status="canceled"),
            _make_entry("q-005", status="completed"),
            _make_entry("q-004", status="queued"),
            _make_entry("q-003", status="failed"),
            _make_entry("q-002", status="needs_review"),
            _make_entry("q-001", status="running"),
        ]
        result = _sort_by_status(entries)
        statuses = [e["status"] for e in result]
        # running(0), needs_review(1), failed(1), queued(2), completed(3), canceled(3)
        self.assertEqual(statuses[0], "running")
        self.assertIn(statuses[1], ["needs_review", "failed"])
        self.assertIn(statuses[2], ["needs_review", "failed"])
        self.assertEqual(statuses[3], "queued")
        self.assertIn(statuses[4], ["completed", "canceled"])


class TestSortByProgress(unittest.TestCase):
    def test_higher_completion_first(self):
        entries = [
            _make_entry("q-001", tasks_done=1, tasks_total=10),  # 10%
            _make_entry("q-002", tasks_done=8, tasks_total=10),  # 80%
            _make_entry("q-003", tasks_done=5, tasks_total=10),  # 50%
        ]
        result = _sort_by_progress(entries)
        ids = [e["id"] for e in result]
        self.assertEqual(ids, ["q-002", "q-003", "q-001"])

    def test_zero_total_treated_as_zero_pct(self):
        entries = [
            _make_entry("q-001", tasks_done=0, tasks_total=0),
            _make_entry("q-002", tasks_done=3, tasks_total=5),
        ]
        result = _sort_by_progress(entries)
        self.assertEqual(result[0]["id"], "q-002")

    def test_tiebreak_by_id(self):
        entries = [
            _make_entry("q-003", tasks_done=5, tasks_total=10),
            _make_entry("q-001", tasks_done=5, tasks_total=10),
        ]
        result = _sort_by_progress(entries)
        ids = [e["id"] for e in result]
        self.assertEqual(ids, ["q-001", "q-003"])


class TestSortByName(unittest.TestCase):
    def test_alphabetical(self):
        entries = [
            _make_entry("q-001", spec_name="zeta-spec"),
            _make_entry("q-002", spec_name="alpha-spec"),
            _make_entry("q-003", spec_name="middle-spec"),
        ]
        result = _sort_by_name(entries)
        names = [
            os.path.splitext(os.path.basename(e["original_spec_path"]))[0]
            for e in result
        ]
        self.assertEqual(names, ["alpha-spec", "middle-spec", "zeta-spec"])

    def test_case_insensitive(self):
        entries = [
            _make_entry("q-001", spec_name="Bravo"),
            _make_entry("q-002", spec_name="alpha"),
        ]
        result = _sort_by_name(entries)
        self.assertEqual(result[0]["id"], "q-002")


class TestSortByRecent(unittest.TestCase):
    def test_most_recent_first(self):
        entries = [
            _make_entry("q-001", last_iteration_at="2026-03-01T10:00:00"),
            _make_entry("q-002", last_iteration_at="2026-03-03T10:00:00"),
            _make_entry("q-003", last_iteration_at="2026-03-02T10:00:00"),
        ]
        result = _sort_by_recent(entries)
        ids = [e["id"] for e in result]
        self.assertEqual(ids, ["q-002", "q-003", "q-001"])

    def test_no_timestamp_sorts_last(self):
        entries = [
            _make_entry("q-001", last_iteration_at=""),
            _make_entry("q-002", last_iteration_at="2026-03-03T10:00:00"),
        ]
        result = _sort_by_recent(entries)
        self.assertEqual(result[0]["id"], "q-002")
        self.assertEqual(result[1]["id"], "q-001")


class TestSortByDag(unittest.TestCase):
    def test_linear_chain(self):
        entries = [
            _make_entry("q-003", blocked_by=["q-002"]),
            _make_entry("q-001"),
            _make_entry("q-002", blocked_by=["q-001"]),
        ]
        result = _sort_by_dag(entries)
        ids = [e["id"] for e, _ in result]
        depths = [d for _, d in result]
        # q-001 first (root), then q-002, then q-003
        self.assertEqual(ids, ["q-001", "q-002", "q-003"])
        self.assertEqual(depths, [0, 1, 2])

    def test_independent_specs(self):
        entries = [
            _make_entry("q-002"),
            _make_entry("q-001"),
            _make_entry("q-003"),
        ]
        result = _sort_by_dag(entries)
        ids = [e["id"] for e, _ in result]
        # All independent, sorted by id
        self.assertEqual(ids, ["q-001", "q-002", "q-003"])
        depths = [d for _, d in result]
        self.assertEqual(depths, [0, 0, 0])

    def test_mixed_deps_and_independent(self):
        entries = [
            _make_entry("q-001"),
            _make_entry("q-002", blocked_by=["q-001"]),
            _make_entry("q-003"),  # independent
        ]
        result = _sort_by_dag(entries)
        ids = [e["id"] for e, _ in result]
        # q-001 and q-003 are roots, q-002 depends on q-001
        self.assertIn("q-001", ids[:2])  # root
        self.assertIn("q-003", ids[:2])  # root
        self.assertEqual(ids[2], "q-002")  # dependent

    def test_cycle_handling(self):
        """Cycles should be broken gracefully without crashing."""
        entries = [
            _make_entry("q-001", blocked_by=["q-002"]),
            _make_entry("q-002", blocked_by=["q-001"]),
        ]
        # Should not raise
        result = _sort_by_dag(entries)
        ids = [e["id"] for e, _ in result]
        # Both entries should appear (cycle broken)
        self.assertEqual(len(result), 2)
        self.assertIn("q-001", ids)
        self.assertIn("q-002", ids)

    def test_dependency_outside_entries_ignored(self):
        """blocked_by referencing IDs not in entries should be ignored."""
        entries = [
            _make_entry("q-002", blocked_by=["q-999"]),  # q-999 not in entries
            _make_entry("q-001"),
        ]
        result = _sort_by_dag(entries)
        ids = [e["id"] for e, _ in result]
        # q-002's dependency on q-999 is ignored, both are roots
        self.assertEqual(len(result), 2)

    def test_diamond_dependency(self):
        """Diamond: q-004 depends on q-002 and q-003, both depend on q-001."""
        entries = [
            _make_entry("q-004", blocked_by=["q-002", "q-003"]),
            _make_entry("q-002", blocked_by=["q-001"]),
            _make_entry("q-003", blocked_by=["q-001"]),
            _make_entry("q-001"),
        ]
        result = _sort_by_dag(entries)
        ids = [e["id"] for e, _ in result]
        depths = {e["id"]: d for e, d in result}
        # q-001 must come first
        self.assertEqual(ids[0], "q-001")
        self.assertEqual(depths["q-001"], 0)
        # q-002 and q-003 at depth 1
        self.assertEqual(depths["q-002"], 1)
        self.assertEqual(depths["q-003"], 1)
        # q-004 at depth 2
        self.assertEqual(depths["q-004"], 2)


class TestSortEntries(unittest.TestCase):
    """Test the sort_entries dispatcher."""

    def test_unknown_mode_defaults_to_queue(self):
        entries = [_make_entry("q-002"), _make_entry("q-001")]
        result = sort_entries(entries, "unknown_mode")
        ids = [e["id"] for e in result]
        self.assertEqual(ids, ["q-001", "q-002"])

    def test_dag_returns_tuples(self):
        entries = [_make_entry("q-001")]
        result = sort_entries(entries, "dag")
        self.assertIsInstance(result[0], tuple)

    def test_non_dag_returns_dicts(self):
        for mode in ["queue", "status", "progress", "name", "recent"]:
            entries = [_make_entry("q-001")]
            result = sort_entries(entries, mode)
            self.assertIsInstance(result[0], dict, f"mode={mode} should return dicts")


class TestFormatDashboardSortIntegration(unittest.TestCase):
    """Test that format_dashboard renders correctly with different sort modes."""

    def _make_status_data(self, entries: list[dict]) -> dict:
        return {
            "entries": entries,
            "summary": {
                "total": len(entries),
                "running": 0,
                "queued": len(entries),
                "completed": 0,
            },
            "workers": [],
        }

    def test_sort_mode_queue(self):
        entries = [_make_entry("q-003"), _make_entry("q-001"), _make_entry("q-002")]
        data = self._make_status_data(entries)
        output = format_dashboard(data, color=False, width=80, sort_mode="queue")
        # q-001 should appear before q-003
        pos_001 = output.find("q-001")
        pos_003 = output.find("q-003")
        self.assertGreater(pos_001, 0)
        self.assertGreater(pos_003, 0)
        self.assertLess(pos_001, pos_003)

    def test_sort_mode_progress(self):
        entries = [
            _make_entry("q-001", tasks_done=1, tasks_total=10),
            _make_entry("q-002", tasks_done=8, tasks_total=10),
        ]
        data = self._make_status_data(entries)
        output = format_dashboard(data, color=False, width=80, sort_mode="progress")
        # q-002 (80%) should appear before q-001 (10%)
        pos_002 = output.find("q-002")
        pos_001 = output.find("q-001")
        self.assertLess(pos_002, pos_001)

    def test_sort_mode_dag_indentation(self):
        entries = [
            _make_entry("q-002", blocked_by=["q-001"]),
            _make_entry("q-001"),
        ]
        data = self._make_status_data(entries)
        output = format_dashboard(data, color=False, width=80, sort_mode="dag")
        lines = output.split("\n")
        # Find lines with q-001 and q-002
        q001_line = [l for l in lines if "q-001" in l]
        q002_line = [l for l in lines if "q-002" in l]
        self.assertTrue(q001_line, "q-001 should be in output")
        self.assertTrue(q002_line, "q-002 should be in output")
        # q-002 should be indented (has more leading spaces than q-001)

    def test_sort_mode_name(self):
        entries = [
            _make_entry("q-001", spec_name="zeta"),
            _make_entry("q-002", spec_name="alpha"),
        ]
        data = self._make_status_data(entries)
        output = format_dashboard(data, color=False, width=80, sort_mode="name")
        pos_alpha = output.find("alpha")
        pos_zeta = output.find("zeta")
        self.assertLess(pos_alpha, pos_zeta)

    def test_all_modes_render_without_error(self):
        entries = [
            _make_entry("q-001", status="running", tasks_done=3, tasks_total=5),
            _make_entry("q-002", status="completed", tasks_done=5, tasks_total=5),
            _make_entry("q-003", status="queued"),
        ]
        data = self._make_status_data(entries)
        for mode in ["queue", "status", "progress", "dag", "name", "recent"]:
            output = format_dashboard(data, color=False, width=80, sort_mode=mode)
            self.assertIn("q-001", output, f"mode={mode} missing q-001")
            self.assertIn("q-002", output, f"mode={mode} missing q-002")
            self.assertIn("q-003", output, f"mode={mode} missing q-003")


if __name__ == "__main__":
    unittest.main()
