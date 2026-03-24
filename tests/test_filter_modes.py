# test_filter_modes.py — Tests for dashboard status filtering.

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from lib.status import filter_entries, format_dashboard


def _make_entry(
    qid: str,
    status: str = "queued",
    spec_name: str = "test-spec",
    tasks_done: int = 0,
    tasks_total: int = 5,
    mode: str = "execute",
) -> dict:
    return {
        "id": qid,
        "status": status,
        "original_spec_path": f"/specs/{spec_name}.md",
        "spec_path": f"/queue/{qid}.spec.md",
        "tasks_done": tasks_done,
        "tasks_total": tasks_total,
        "blocked_by": [],
        "last_iteration_at": "",
        "mode": mode,
        "priority": 10,
        "iteration": 1,
        "max_iterations": 30,
        "last_worker": None,
    }


def _make_status_data(entries: list[dict]) -> dict:
    status_counts = {
        "running": 0,
        "queued": 0,
        "completed": 0,
        "canceled": 0,
        "requeued": 0,
        "failed": 0,
        "needs_review": 0,
    }
    for e in entries:
        s = e.get("status", "queued")
        if s in status_counts:
            status_counts[s] += 1
    return {
        "entries": entries,
        "summary": {"total": len(entries), **status_counts},
        "workers": [{"id": "w-1"}, {"id": "w-2"}],
    }


class TestFilterEntries(unittest.TestCase):
    """Test the filter_entries() function directly."""

    def _entries(self):
        return [
            _make_entry("q-001", status="running"),
            _make_entry("q-002", status="queued"),
            _make_entry("q-003", status="completed"),
            _make_entry("q-004", status="canceled"),
            _make_entry("q-005", status="failed"),
            _make_entry("q-006", status="requeued"),
            _make_entry("q-007", status="needs_review"),
        ]

    def test_filter_all_returns_everything(self):
        entries = self._entries()
        result = filter_entries(entries, filter_status="all")
        self.assertEqual(len(result), 7)

    def test_filter_running(self):
        entries = self._entries()
        result = filter_entries(entries, filter_status="running")
        statuses = {e["status"] for e in result}
        self.assertTrue(statuses <= {"running", "requeued"})
        self.assertEqual(len(result), 2)  # q-001 (running) + q-006 (requeued)

    def test_filter_queued(self):
        entries = self._entries()
        result = filter_entries(entries, filter_status="queued")
        statuses = {e["status"] for e in result}
        self.assertTrue(statuses <= {"queued", "needs_review", "failed"})
        self.assertEqual(len(result), 3)  # q-002, q-005, q-007

    def test_filter_completed(self):
        entries = self._entries()
        result = filter_entries(entries, filter_status="completed")
        statuses = {e["status"] for e in result}
        self.assertTrue(statuses <= {"completed", "canceled"})
        self.assertEqual(len(result), 2)  # q-003, q-004

    def test_show_completed_false_hides_completed_and_canceled(self):
        entries = self._entries()
        result = filter_entries(entries, filter_status="all", show_completed=False)
        statuses = {e["status"] for e in result}
        self.assertNotIn("completed", statuses)
        self.assertNotIn("canceled", statuses)
        self.assertEqual(len(result), 5)  # everything except completed/canceled

    def test_show_completed_false_with_filter_running(self):
        entries = self._entries()
        result = filter_entries(entries, filter_status="running", show_completed=False)
        # show_completed=False has no effect when filtering to running
        self.assertEqual(len(result), 2)

    def test_show_completed_false_with_filter_completed_returns_nothing(self):
        entries = self._entries()
        # show_completed=False removes completed/canceled, then filter asks for completed only
        result = filter_entries(
            entries, filter_status="completed", show_completed=False
        )
        self.assertEqual(len(result), 0)

    def test_empty_entries(self):
        result = filter_entries([], filter_status="running")
        self.assertEqual(result, [])

    def test_all_same_status(self):
        entries = [_make_entry(f"q-{i:03d}", status="running") for i in range(5)]
        result = filter_entries(entries, filter_status="running")
        self.assertEqual(len(result), 5)
        result = filter_entries(entries, filter_status="queued")
        self.assertEqual(len(result), 0)


class TestFormatDashboardFilter(unittest.TestCase):
    """Test that format_dashboard applies filtering correctly."""

    def test_filter_all_shows_all_entries(self):
        entries = [
            _make_entry("q-001", status="running"),
            _make_entry("q-002", status="queued"),
            _make_entry("q-003", status="completed"),
        ]
        data = _make_status_data(entries)
        # view_mode="all" to bypass time-based filter (entries have no timestamps)
        output = format_dashboard(data, color=False, width=80, filter_status="all", view_mode="all")
        self.assertIn("q-001", output)
        self.assertIn("q-002", output)
        self.assertIn("q-003", output)

    def test_filter_running_hides_non_running(self):
        entries = [
            _make_entry("q-001", status="running"),
            _make_entry("q-002", status="queued"),
            _make_entry("q-003", status="completed"),
        ]
        data = _make_status_data(entries)
        output = format_dashboard(data, color=False, width=80, filter_status="running")
        self.assertIn("q-001", output)
        self.assertNotIn("q-002", output)
        self.assertNotIn("q-003", output)

    def test_filter_queued_hides_non_queued(self):
        entries = [
            _make_entry("q-001", status="running"),
            _make_entry("q-002", status="queued"),
            _make_entry("q-003", status="completed"),
        ]
        data = _make_status_data(entries)
        output = format_dashboard(data, color=False, width=80, filter_status="queued")
        self.assertNotIn("q-001", output)
        self.assertIn("q-002", output)
        self.assertNotIn("q-003", output)

    def test_filter_completed_hides_non_completed(self):
        entries = [
            _make_entry("q-001", status="running"),
            _make_entry("q-002", status="queued"),
            _make_entry("q-003", status="completed"),
        ]
        data = _make_status_data(entries)
        # view_mode="all" to bypass time-based filter (entries have no timestamps)
        output = format_dashboard(
            data, color=False, width=80, filter_status="completed", view_mode="all"
        )
        self.assertNotIn("q-001", output)
        self.assertNotIn("q-002", output)
        self.assertIn("q-003", output)

    def test_show_completed_false(self):
        entries = [
            _make_entry("q-001", status="running"),
            _make_entry("q-002", status="completed"),
            _make_entry("q-003", status="canceled"),
        ]
        data = _make_status_data(entries)
        output = format_dashboard(data, color=False, width=80, show_completed=False)
        self.assertIn("q-001", output)
        self.assertNotIn("q-002", output)
        self.assertNotIn("q-003", output)

    def test_showing_count_when_filtered(self):
        entries = [
            _make_entry("q-001", status="running"),
            _make_entry("q-002", status="queued"),
            _make_entry("q-003", status="completed"),
        ]
        data = _make_status_data(entries)
        output = format_dashboard(data, color=False, width=80, filter_status="running")
        self.assertIn("Showing 1 of 3 specs", output)

    def test_no_showing_count_when_unfiltered(self):
        entries = [
            _make_entry("q-001", status="running"),
            _make_entry("q-002", status="queued"),
        ]
        data = _make_status_data(entries)
        output = format_dashboard(data, color=False, width=80, filter_status="all")
        self.assertNotIn("Showing", output)
        # Summary line should contain count info (format: "Workers: N/M busy | X running, Y queued")
        self.assertIn("running", output)
        self.assertIn("queued", output)

    def test_filter_indicator_in_header(self):
        entries = [_make_entry("q-001", status="running")]
        data = _make_status_data(entries)
        output = format_dashboard(data, color=False, width=80, filter_status="running")
        self.assertIn("filter: running", output)

    def test_sort_indicator_in_header(self):
        entries = [_make_entry("q-001")]
        data = _make_status_data(entries)
        output = format_dashboard(data, color=False, width=80, sort_mode="progress")
        self.assertIn("sort: progress", output)

    def test_no_indicator_for_defaults(self):
        entries = [_make_entry("q-001")]
        data = _make_status_data(entries)
        output = format_dashboard(
            data, color=False, width=80, filter_status="all", sort_mode="queue"
        )
        self.assertNotIn("filter:", output)
        self.assertNotIn("sort:", output)

    def test_completed_hidden_indicator_in_header(self):
        entries = [_make_entry("q-001", status="running")]
        data = _make_status_data(entries)
        output = format_dashboard(data, color=False, width=80, show_completed=False)
        self.assertIn("completed: hidden", output)

    def test_multiple_indicators(self):
        entries = [
            _make_entry("q-001", status="running"),
            _make_entry("q-002", status="completed"),
        ]
        data = _make_status_data(entries)
        output = format_dashboard(
            data,
            color=False,
            width=80,
            filter_status="running",
            sort_mode="progress",
            show_completed=False,
        )
        self.assertIn("filter: running", output)
        self.assertIn("sort: progress", output)
        self.assertIn("completed: hidden", output)

    def test_filter_with_sort_preserves_sort_order(self):
        entries = [
            _make_entry("q-001", status="running", tasks_done=1, tasks_total=10),
            _make_entry("q-002", status="running", tasks_done=8, tasks_total=10),
            _make_entry("q-003", status="completed", tasks_done=10, tasks_total=10),
        ]
        data = _make_status_data(entries)
        output = format_dashboard(
            data,
            color=False,
            width=80,
            sort_mode="progress",
            filter_status="running",
        )
        # q-002 (80%) before q-001 (10%), q-003 hidden
        pos_002 = output.find("q-002")
        pos_001 = output.find("q-001")
        self.assertLess(pos_002, pos_001)
        self.assertNotIn("q-003", output)

    def test_filter_running_shows_zero_when_none_running(self):
        entries = [
            _make_entry("q-001", status="queued"),
            _make_entry("q-002", status="completed"),
        ]
        data = _make_status_data(entries)
        output = format_dashboard(data, color=False, width=80, filter_status="running")
        self.assertIn("Showing 0 of 2 specs", output)
        self.assertNotIn("q-001", output)
        self.assertNotIn("q-002", output)


if __name__ == "__main__":
    unittest.main()
