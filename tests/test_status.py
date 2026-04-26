# test_status.py — Characterization tests for lib/status.py.
#
# status.py has the highest cyclomatic complexity in the codebase:
#   - format_dashboard: CC=46
#   - format_queue_table: CC=40
#   - format_telemetry_table: moderate
#   - build_queue_status: reads filesystem
#
# These tests lock in current behavior for safe refactoring.
# No live API calls, no real worktrees.

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.status import (
    build_queue_status,
    format_dashboard,
    format_duration,
    format_queue_table,
    format_relative_time,
    format_telemetry_table,
)


# ── Shared fixtures ───────────────────────────────────────────────────────────

def _minimal_entry(**kwargs) -> dict:
    """Return a minimal queue entry dict with sensible defaults."""
    base = {
        "id": "q-001",
        "status": "queued",
        "spec_path": "/tmp/test.spec.md",
        "mode": "execute",
        "worker_id": None,
        "iteration": 0,
        "max_iterations": 30,
        "tasks_done": 0,
        "tasks_total": 5,
    }
    base.update(kwargs)
    return base


def _minimal_status_data(entries=None, workers=None) -> dict:
    """Return a minimal status_data dict."""
    if entries is None:
        entries = []
    if workers is None:
        workers = []
    status_counts = {
        "queued": sum(1 for e in entries if e.get("status") == "queued"),
        "requeued": 0,
        "running": sum(1 for e in entries if e.get("status") == "running"),
        "completed": sum(1 for e in entries if e.get("status") == "completed"),
        "failed": 0,
        "canceled": 0,
        "needs_review": 0,
    }
    return {
        "entries": entries,
        "summary": {"total": len(entries), **status_counts},
        "workers": workers,
    }


def _minimal_telemetry(**kwargs) -> dict:
    """Return a minimal telemetry dict."""
    base = {
        "queue_id": "q-001",
        "spec_name": "test-spec",
        "spec_path": "/tmp/test.spec.md",
        "status": "running",
        "iteration": 3,
        "max_iterations": 30,
        "tasks_done": 2,
        "tasks_total": 5,
        "total_time_seconds": 180,
        "total_tasks_added": 1,
        "total_tasks_skipped": 0,
        "consecutive_failures": 0,
        "iterations": [],
    }
    base.update(kwargs)
    return base


# ── format_duration ────────────────────────────────────────────────────────────

class TestFormatDuration(unittest.TestCase):
    def test_seconds_only(self):
        self.assertEqual(format_duration(45), "45s")

    def test_zero_seconds(self):
        self.assertEqual(format_duration(0), "0s")

    def test_exactly_one_minute(self):
        self.assertEqual(format_duration(60), "1m 00s")

    def test_minutes_and_seconds(self):
        self.assertEqual(format_duration(90), "1m 30s")

    def test_exactly_one_hour(self):
        self.assertEqual(format_duration(3600), "1h 00m")

    def test_hours_and_minutes(self):
        self.assertEqual(format_duration(3661), "1h 01m")

    def test_float_truncated(self):
        # floats are truncated, not rounded
        self.assertEqual(format_duration(59.9), "59s")


# ── format_relative_time ───────────────────────────────────────────────────────

class TestFormatRelativeTime(unittest.TestCase):
    def test_none_returns_em_dash(self):
        result = format_relative_time(None)
        self.assertEqual(result, "\u2014")

    def test_invalid_returns_em_dash(self):
        result = format_relative_time("not-a-date")
        self.assertEqual(result, "\u2014")

    def test_recent_returns_ago_string(self):
        from datetime import datetime, timezone, timedelta
        ts = (datetime.now(timezone.utc) - timedelta(minutes=5)).isoformat()
        result = format_relative_time(ts)
        # Should contain "ago" or a time indicator
        self.assertTrue("ago" in result or "m" in result or "s" in result)


# ── format_queue_table ─────────────────────────────────────────────────────────

class TestFormatQueueTable(unittest.TestCase):
    def test_empty_queue_returns_string(self):
        status_data = _minimal_status_data()
        result = format_queue_table(status_data, color=False, width=120)
        self.assertIsInstance(result, str)
        self.assertTrue(len(result) > 0)

    def test_empty_queue_mentions_no_specs(self):
        status_data = _minimal_status_data()
        result = format_queue_table(status_data, color=False, width=120)
        self.assertIn("No specs", result)

    def test_single_entry_appears_in_output(self):
        entries = [_minimal_entry(id="q-042", status="queued")]
        status_data = _minimal_status_data(entries=entries)
        result = format_queue_table(status_data, color=False, width=120)
        self.assertIn("q-042", result)

    def test_running_entry_appears(self):
        entries = [_minimal_entry(id="q-007", status="running", worker_id="w-1")]
        status_data = _minimal_status_data(entries=entries)
        result = format_queue_table(status_data, color=False, width=120)
        self.assertIn("q-007", result)

    def test_completed_entry_appears(self):
        entries = [_minimal_entry(id="q-010", status="completed", tasks_done=5, tasks_total=5)]
        status_data = _minimal_status_data(entries=entries)
        # view_mode="all" to show completed entries regardless of age
        result = format_queue_table(status_data, color=False, width=120, view_mode="all")
        self.assertIn("q-010", result)

    def test_color_off_no_escape_sequences(self):
        entries = [_minimal_entry(id="q-001", status="running")]
        status_data = _minimal_status_data(entries=entries)
        result = format_queue_table(status_data, color=False, width=120)
        self.assertNotIn("\033[", result)

    def test_header_present(self):
        status_data = _minimal_status_data()
        result = format_queue_table(status_data, color=False, width=120)
        self.assertIn("No specs in queue", result)

    def test_multiple_entries(self):
        entries = [
            _minimal_entry(id="q-001", status="queued"),
            _minimal_entry(id="q-002", status="running", worker_id="w-1"),
            _minimal_entry(id="q-003", status="completed"),
        ]
        status_data = _minimal_status_data(entries=entries)
        # view_mode="all" to show completed entries regardless of age
        result = format_queue_table(status_data, color=False, width=120, view_mode="all")
        self.assertIn("q-001", result)
        self.assertIn("q-002", result)
        self.assertIn("q-003", result)

    def test_worker_count_shown_when_workers_present(self):
        workers = [{"id": "w-1"}, {"id": "w-2"}]
        status_data = _minimal_status_data(workers=workers)
        result = format_queue_table(status_data, color=False, width=120)
        # Summary line should mention workers
        self.assertIn("Workers", result)

    def test_narrow_width_does_not_crash(self):
        entries = [_minimal_entry(id="q-001", status="queued")]
        status_data = _minimal_status_data(entries=entries)
        result = format_queue_table(status_data, color=False, width=40)
        self.assertIsInstance(result, str)


# ── format_telemetry_table ─────────────────────────────────────────────────────

class TestFormatTelemetryTable(unittest.TestCase):
    def test_returns_string(self):
        telem = _minimal_telemetry()
        result = format_telemetry_table(telem, color=False)
        self.assertIsInstance(result, str)
        self.assertTrue(len(result) > 0)

    def test_spec_name_in_output(self):
        telem = _minimal_telemetry(spec_name="my-great-spec")
        result = format_telemetry_table(telem, color=False)
        self.assertIn("my-great-spec", result)

    def test_status_in_output(self):
        telem = _minimal_telemetry(status="completed")
        result = format_telemetry_table(telem, color=False)
        self.assertIn("completed", result)

    def test_iteration_count_in_output(self):
        telem = _minimal_telemetry(iteration=7, max_iterations=30)
        result = format_telemetry_table(telem, color=False)
        self.assertIn("7", result)
        self.assertIn("30", result)

    def test_cost_shown_when_present(self):
        telem = _minimal_telemetry(
            cost={"total_cost_usd": 1.2345, "model_breakdown": {"claude-3": 1.2345}}
        )
        result = format_telemetry_table(telem, color=False)
        self.assertIn("cost", result.lower())
        self.assertIn("1.2345", result)

    def test_no_escape_sequences_when_color_off(self):
        telem = _minimal_telemetry()
        result = format_telemetry_table(telem, color=False)
        self.assertNotIn("\033[", result)

    def test_consecutive_failures_shown(self):
        telem = _minimal_telemetry(consecutive_failures=3)
        result = format_telemetry_table(telem, color=False)
        self.assertIn("failure", result.lower())

    def test_self_evolved_shown_when_tasks_added(self):
        telem = _minimal_telemetry(total_tasks_added=2)
        result = format_telemetry_table(telem, color=False)
        self.assertIn("self-evolved", result)


# ── format_dashboard ───────────────────────────────────────────────────────────

class TestFormatDashboard(unittest.TestCase):
    def test_returns_string(self):
        status_data = _minimal_status_data()
        result = format_dashboard(status_data, color=False, width=80)
        self.assertIsInstance(result, str)
        self.assertTrue(len(result) > 0)

    def test_boi_header_present(self):
        status_data = _minimal_status_data()
        result = format_dashboard(status_data, color=False, width=80)
        self.assertIn("BOI", result)

    def test_empty_queue_no_crash(self):
        status_data = _minimal_status_data()
        result = format_dashboard(status_data, color=False, width=80)
        self.assertIn("No specs", result)

    def test_entry_id_in_output(self):
        entries = [_minimal_entry(id="q-099", status="running", worker_id="w-1")]
        status_data = _minimal_status_data(entries=entries)
        result = format_dashboard(status_data, color=False, width=80)
        self.assertIn("q-099", result)

    def test_multiple_statuses(self):
        entries = [
            _minimal_entry(id="q-001", status="completed"),
            _minimal_entry(id="q-002", status="running", worker_id="w-1"),
            _minimal_entry(id="q-003", status="queued"),
        ]
        status_data = _minimal_status_data(entries=entries)
        # view_mode="all" to show completed entries regardless of age
        result = format_dashboard(status_data, color=False, width=80, view_mode="all")
        self.assertIn("q-001", result)
        self.assertIn("q-002", result)
        self.assertIn("q-003", result)

    def test_no_escape_sequences_when_color_off(self):
        entries = [_minimal_entry(id="q-001", status="running")]
        status_data = _minimal_status_data(entries=entries)
        result = format_dashboard(status_data, color=False, width=80)
        self.assertNotIn("\033[", result)

    def test_filter_status_active_shown_in_header(self):
        status_data = _minimal_status_data()
        result = format_dashboard(status_data, color=False, width=80, filter_status="running")
        self.assertIn("filter", result.lower())
        self.assertIn("running", result)

    def test_sort_mode_shown_in_header(self):
        status_data = _minimal_status_data()
        result = format_dashboard(status_data, color=False, width=80, sort_mode="status")
        self.assertIn("sort", result.lower())

    def test_show_completed_false_filters_completed(self):
        entries = [
            _minimal_entry(id="q-001", status="completed"),
            _minimal_entry(id="q-002", status="running", worker_id="w-1"),
        ]
        status_data = _minimal_status_data(entries=entries)
        result = format_dashboard(
            status_data, color=False, width=80, show_completed=False
        )
        # completed entry should be hidden
        self.assertNotIn("q-001", result)
        self.assertIn("q-002", result)

    def test_narrow_width_does_not_crash(self):
        entries = [_minimal_entry(id="q-001", status="queued")]
        status_data = _minimal_status_data(entries=entries)
        result = format_dashboard(status_data, color=False, width=40)
        self.assertIsInstance(result, str)

    def test_workers_summary_in_footer(self):
        workers = [{"id": "w-1"}, {"id": "w-2"}]
        entries = [_minimal_entry(id="q-001", status="running", worker_id="w-1")]
        status_data = _minimal_status_data(entries=entries, workers=workers)
        result = format_dashboard(status_data, color=False, width=80)
        self.assertIn("Workers", result)


# ── build_queue_status ────────────────────────────────────────────────────────

class TestBuildQueueStatus(unittest.TestCase):
    def test_empty_dir_returns_empty_entries(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            result = build_queue_status(tmpdir)
        self.assertIn("entries", result)
        self.assertEqual(result["entries"], [])

    def test_empty_dir_returns_summary(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            result = build_queue_status(tmpdir)
        self.assertIn("summary", result)
        self.assertEqual(result["summary"]["total"], 0)

    def test_empty_dir_returns_workers_empty(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            result = build_queue_status(tmpdir)
        self.assertEqual(result["workers"], [])

    def test_config_workers_passed_through(self):
        workers = [{"id": "w-1"}, {"id": "w-2"}]
        with tempfile.TemporaryDirectory() as tmpdir:
            result = build_queue_status(tmpdir, config={"workers": workers})
        self.assertEqual(result["workers"], workers)

    def test_single_json_entry_loaded(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            entry = {
                "id": "q-001",
                "status": "queued",
                "spec_path": "/tmp/test.spec.md",
            }
            (Path(tmpdir) / "q-001.json").write_text(json.dumps(entry))
            result = build_queue_status(tmpdir)
        self.assertEqual(len(result["entries"]), 1)
        self.assertEqual(result["entries"][0]["id"], "q-001")

    def test_summary_counts_by_status(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            for i, status in enumerate(["queued", "running", "completed"]):
                entry = {"id": f"q-00{i+1}", "status": status, "spec_path": "/tmp/x.md"}
                (Path(tmpdir) / f"q-00{i+1}.json").write_text(json.dumps(entry))
            result = build_queue_status(tmpdir)
        summary = result["summary"]
        self.assertEqual(summary["queued"], 1)
        self.assertEqual(summary["running"], 1)
        self.assertEqual(summary["completed"], 1)
        self.assertEqual(summary["total"], 3)

    def test_result_structure_has_required_keys(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            result = build_queue_status(tmpdir)
        self.assertIn("entries", result)
        self.assertIn("summary", result)
        self.assertIn("workers", result)


if __name__ == "__main__":
    unittest.main()
