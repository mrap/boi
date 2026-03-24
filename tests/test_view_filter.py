# test_view_filter.py — Tests for _apply_view_filter time-based logic.

import os
import sys
import unittest
from datetime import datetime, timedelta, timezone

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from lib.status import _apply_view_filter


def _make_entry(
    qid: str,
    status: str = "queued",
    last_iteration_at: str = "",
) -> dict:
    return {
        "id": qid,
        "status": status,
        "original_spec_path": f"/specs/{qid}.md",
        "spec_path": f"/queue/{qid}.spec.md",
        "tasks_done": 0,
        "tasks_total": 5,
        "blocked_by": [],
        "last_iteration_at": last_iteration_at,
        "mode": "execute",
        "priority": 10,
        "iteration": 1,
        "max_iterations": 30,
        "last_worker": None,
    }


def _ts(hours_ago: float) -> str:
    """Return an ISO timestamp string for a time N hours in the past."""
    dt = datetime.now(timezone.utc) - timedelta(hours=hours_ago)
    return dt.isoformat()


class TestApplyViewFilterAll(unittest.TestCase):
    def test_all_returns_all_entries_unchanged(self):
        entries = [
            _make_entry("q-001", status="running"),
            _make_entry("q-002", status="completed"),
            _make_entry("q-003", status="canceled"),
            _make_entry("q-004", status="failed"),
            _make_entry("q-005", status="queued"),
        ]
        result = _apply_view_filter(entries, "all")
        self.assertEqual(len(result), 5)
        ids = [e["id"] for e in result]
        self.assertIn("q-001", ids)
        self.assertIn("q-002", ids)
        self.assertIn("q-003", ids)
        self.assertIn("q-004", ids)
        self.assertIn("q-005", ids)

    def test_all_with_empty_entries(self):
        result = _apply_view_filter([], "all")
        self.assertEqual(result, [])

    def test_all_returns_same_objects(self):
        entries = [_make_entry("q-001", status="running")]
        result = _apply_view_filter(entries, "all")
        self.assertIs(result, entries)


class TestApplyViewFilterRunning(unittest.TestCase):
    def test_running_returns_only_running_requeued_assigning(self):
        entries = [
            _make_entry("q-001", status="running"),
            _make_entry("q-002", status="requeued"),
            _make_entry("q-003", status="assigning"),
            _make_entry("q-004", status="queued"),
            _make_entry("q-005", status="completed"),
            _make_entry("q-006", status="failed"),
            _make_entry("q-007", status="canceled"),
            _make_entry("q-008", status="needs_review"),
        ]
        result = _apply_view_filter(entries, "running")
        ids = [e["id"] for e in result]
        self.assertIn("q-001", ids)
        self.assertIn("q-002", ids)
        self.assertIn("q-003", ids)
        self.assertNotIn("q-004", ids)
        self.assertNotIn("q-005", ids)
        self.assertNotIn("q-006", ids)
        self.assertNotIn("q-007", ids)
        self.assertNotIn("q-008", ids)
        self.assertEqual(len(result), 3)

    def test_running_empty_when_none_running(self):
        entries = [
            _make_entry("q-001", status="queued"),
            _make_entry("q-002", status="completed"),
        ]
        result = _apply_view_filter(entries, "running")
        self.assertEqual(result, [])

    def test_running_with_empty_entries(self):
        result = _apply_view_filter([], "running")
        self.assertEqual(result, [])


class TestApplyViewFilterRecent(unittest.TestCase):
    def test_recent_returns_n_most_recent_by_timestamp(self):
        entries = [
            _make_entry("q-001", last_iteration_at=_ts(10)),
            _make_entry("q-002", last_iteration_at=_ts(2)),
            _make_entry("q-003", last_iteration_at=_ts(5)),
            _make_entry("q-004", last_iteration_at=_ts(1)),
            _make_entry("q-005", last_iteration_at=_ts(20)),
        ]
        result = _apply_view_filter(entries, "recent:3")
        ids = [e["id"] for e in result]
        # Most recent: q-004 (1h), q-002 (2h), q-003 (5h)
        self.assertEqual(ids[0], "q-004")
        self.assertEqual(ids[1], "q-002")
        self.assertEqual(ids[2], "q-003")
        self.assertEqual(len(result), 3)

    def test_recent_returns_all_if_fewer_than_n(self):
        entries = [
            _make_entry("q-001", last_iteration_at=_ts(1)),
            _make_entry("q-002", last_iteration_at=_ts(2)),
        ]
        result = _apply_view_filter(entries, "recent:10")
        self.assertEqual(len(result), 2)

    def test_recent_defaults_to_10_on_invalid_n(self):
        entries = [_make_entry(f"q-{i:03d}", last_iteration_at=_ts(i)) for i in range(1, 16)]
        result = _apply_view_filter(entries, "recent:bad")
        self.assertEqual(len(result), 10)

    def test_recent_handles_missing_timestamps(self):
        entries = [
            _make_entry("q-001", last_iteration_at=_ts(1)),
            _make_entry("q-002", last_iteration_at=""),  # no timestamp → sorts last
        ]
        result = _apply_view_filter(entries, "recent:2")
        self.assertEqual(result[0]["id"], "q-001")
        self.assertEqual(len(result), 2)


class TestApplyViewFilterDefault(unittest.TestCase):
    def test_default_always_shows_active_statuses(self):
        entries = [
            _make_entry("q-001", status="running"),
            _make_entry("q-002", status="requeued"),
            _make_entry("q-003", status="queued"),
            _make_entry("q-004", status="needs_review"),
            _make_entry("q-005", status="assigning"),
        ]
        result = _apply_view_filter(entries, "default")
        ids = [e["id"] for e in result]
        self.assertIn("q-001", ids)
        self.assertIn("q-002", ids)
        self.assertIn("q-003", ids)
        self.assertIn("q-004", ids)
        self.assertIn("q-005", ids)

    def test_default_canceled_never_shown(self):
        entries = [
            _make_entry("q-001", status="canceled", last_iteration_at=_ts(0.1)),
            _make_entry("q-002", status="running"),
        ]
        result = _apply_view_filter(entries, "default")
        ids = [e["id"] for e in result]
        self.assertNotIn("q-001", ids)
        self.assertIn("q-002", ids)

    def test_default_completed_within_6h_shown(self):
        entries = [
            _make_entry("q-001", status="completed", last_iteration_at=_ts(5)),
        ]
        result = _apply_view_filter(entries, "default")
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]["id"], "q-001")

    def test_default_completed_at_6h_boundary_shown(self):
        # 5h59m = shown (within 6h)
        entries = [
            _make_entry("q-001", status="completed", last_iteration_at=_ts(5.98)),
        ]
        result = _apply_view_filter(entries, "default")
        self.assertEqual(len(result), 1)

    def test_default_completed_older_than_6h_hidden(self):
        entries = [
            _make_entry("q-001", status="completed", last_iteration_at=_ts(7)),
        ]
        result = _apply_view_filter(entries, "default")
        self.assertEqual(len(result), 0)

    def test_default_failed_within_24h_shown(self):
        entries = [
            _make_entry("q-001", status="failed", last_iteration_at=_ts(23)),
        ]
        result = _apply_view_filter(entries, "default")
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]["id"], "q-001")

    def test_default_failed_at_24h_boundary_shown(self):
        # 23h59m = shown (within 24h)
        entries = [
            _make_entry("q-001", status="failed", last_iteration_at=_ts(23.98)),
        ]
        result = _apply_view_filter(entries, "default")
        self.assertEqual(len(result), 1)

    def test_default_failed_older_than_24h_hidden(self):
        entries = [
            _make_entry("q-001", status="failed", last_iteration_at=_ts(25)),
        ]
        result = _apply_view_filter(entries, "default")
        self.assertEqual(len(result), 0)

    def test_default_completed_no_timestamp_hidden(self):
        # No timestamp → can't determine age → hidden
        entries = [
            _make_entry("q-001", status="completed", last_iteration_at=""),
        ]
        result = _apply_view_filter(entries, "default")
        self.assertEqual(len(result), 0)

    def test_default_failed_no_timestamp_hidden(self):
        entries = [
            _make_entry("q-001", status="failed", last_iteration_at=""),
        ]
        result = _apply_view_filter(entries, "default")
        self.assertEqual(len(result), 0)

    def test_default_mixed_scenario(self):
        entries = [
            _make_entry("q-001", status="running"),
            _make_entry("q-002", status="queued"),
            _make_entry("q-003", status="completed", last_iteration_at=_ts(5)),   # shown
            _make_entry("q-004", status="completed", last_iteration_at=_ts(8)),   # hidden
            _make_entry("q-005", status="failed", last_iteration_at=_ts(20)),     # shown
            _make_entry("q-006", status="failed", last_iteration_at=_ts(26)),     # hidden
            _make_entry("q-007", status="canceled", last_iteration_at=_ts(0.5)), # never shown
            _make_entry("q-008", status="needs_review"),
        ]
        result = _apply_view_filter(entries, "default")
        ids = [e["id"] for e in result]
        self.assertIn("q-001", ids)
        self.assertIn("q-002", ids)
        self.assertIn("q-003", ids)
        self.assertNotIn("q-004", ids)
        self.assertIn("q-005", ids)
        self.assertNotIn("q-006", ids)
        self.assertNotIn("q-007", ids)
        self.assertIn("q-008", ids)
        self.assertEqual(len(result), 5)


if __name__ == "__main__":
    unittest.main()
