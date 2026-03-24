# test_status_consistency.py — Verify consistent output across all status modes.
#
# Requirement (q-209 t-3): all display modes must use the same filter/render
# pipeline. These tests check that:
#   1. default and watch (first frame) show the same spec list
#   2. default and dag show the same set of specs
#   3. --all shows more specs than default
#   4. --running shows subset of default
#   5. rows span >80% of the requested terminal width
#   6. "Showing N of M" appears when filtered < total
#   7. separator line spans full terminal width

import sys
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.status import filter_specs, format_spec_row, get_terminal_width, render_status


# ── Fixtures ──────────────────────────────────────────────────────────────────

def _now_iso() -> str:
    return datetime.now(timezone.utc).isoformat()


def _ago_iso(hours: float) -> str:
    return (datetime.now(timezone.utc) - timedelta(hours=hours)).isoformat()


def _entry(qid: str, status: str, last_at: str | None = None, **kwargs) -> dict:
    base = {
        "id": qid,
        "status": status,
        "spec_path": f"/tmp/{qid}.spec.md",
        "mode": "execute",
        "worker_id": None,
        "iteration": 1,
        "max_iterations": 30,
        "tasks_done": 1,
        "tasks_total": 5,
        "last_iteration_at": last_at,
        "submitted_at": last_at or _ago_iso(48),
        "priority": 100,
    }
    base.update(kwargs)
    return base


def _mixed_specs() -> list[dict]:
    """Return a mix of statuses: some visible by default, some not."""
    return [
        _entry("q-001", "running",   last_at=_now_iso()),
        _entry("q-002", "queued",    last_at=_ago_iso(1)),
        _entry("q-003", "completed", last_at=_ago_iso(2)),   # recent — shown in default
        _entry("q-004", "completed", last_at=_ago_iso(10)),  # old — hidden in default
        _entry("q-005", "failed",    last_at=_ago_iso(12)),  # old — hidden in default
        _entry("q-006", "canceled",  last_at=_ago_iso(1)),   # always hidden in default
        _entry("q-007", "needs_review", last_at=_ago_iso(1)),
    ]


def _active_only_specs() -> list[dict]:
    """Only running/queued specs — default == all."""
    return [
        _entry("q-010", "running", last_at=_now_iso()),
        _entry("q-011", "queued",  last_at=_ago_iso(0.5)),
    ]


def _extract_spec_ids(output: str) -> set[str]:
    """Return the set of q-NNN IDs visible in output."""
    ids = set()
    for line in output.splitlines():
        stripped = line.strip()
        # Match bare "q-NNN" at start of a line (after optional DAG tree chars)
        for part in stripped.split():
            if part.startswith("q-") and len(part) > 2:
                candidate = part.rstrip(",;")
                if candidate[2:].isdigit():
                    ids.add(candidate)
                    break
    return ids


# ── Test: same spec IDs in default vs watch (first frame) ─────────────────────

class TestDefaultVsWatch(unittest.TestCase):
    """watch=True uses the same render_status path; first frame must be identical."""

    def setUp(self):
        self.specs = _mixed_specs()
        self.filtered = filter_specs(self.specs, "default")
        self.total = len(self.specs)
        self.summary = {"total": self.total, "running": 1, "queued": 1,
                        "completed": 1, "failed": 0, "canceled": 0, "needs_review": 1}

    def _render(self, watch: bool) -> str:
        return render_status(
            self.filtered,
            sort="queue",
            watch=watch,
            columns=120,
            color=False,
            summary=self.summary,
            total_count=self.total,
            view_mode="default",
        )

    def test_same_spec_ids(self):
        default_out = self._render(watch=False)
        watch_out = self._render(watch=True)
        self.assertEqual(
            _extract_spec_ids(default_out),
            _extract_spec_ids(watch_out),
            "default and watch (first frame) must show identical spec IDs",
        )

    def test_same_line_count(self):
        default_out = self._render(watch=False)
        watch_out = self._render(watch=True)
        self.assertEqual(
            len(default_out.splitlines()),
            len(watch_out.splitlines()),
        )


# ── Test: same spec IDs in default vs dag ─────────────────────────────────────

class TestDefaultVsDag(unittest.TestCase):
    """dag sort reorders specs; the set of visible specs must be the same."""

    def setUp(self):
        self.specs = _mixed_specs()
        self.filtered = filter_specs(self.specs, "default")
        self.total = len(self.specs)
        self.summary = {"total": self.total, "running": 1, "queued": 1,
                        "completed": 1, "failed": 0, "canceled": 0, "needs_review": 1}

    def _render(self, sort: str) -> str:
        return render_status(
            self.filtered,
            sort=sort,
            columns=120,
            color=False,
            summary=self.summary,
            total_count=self.total,
            view_mode="default",
        )

    def test_same_spec_ids_as_default(self):
        default_ids = _extract_spec_ids(self._render("queue"))
        dag_ids = _extract_spec_ids(self._render("dag"))
        # DAG may add tree chars — compare the q-IDs only
        self.assertEqual(
            default_ids,
            dag_ids,
            "dag and default must show the same spec IDs",
        )


# ── Test: --all shows more specs than default ─────────────────────────────────

class TestAllVsDefault(unittest.TestCase):
    """filter_specs("all") must return more entries than filter_specs("default")
    when there are old/canceled specs in the queue."""

    def test_all_has_more_than_default(self):
        specs = _mixed_specs()
        default_filtered = filter_specs(specs, "default")
        all_filtered = filter_specs(specs, "all")
        self.assertGreater(
            len(all_filtered),
            len(default_filtered),
            "all mode must return more specs than default when queue has old/canceled entries",
        )

    def test_all_includes_canceled(self):
        specs = _mixed_specs()
        all_filtered = filter_specs(specs, "all")
        ids = {e["id"] for e in all_filtered}
        self.assertIn("q-006", ids, "all mode must include canceled specs")

    def test_default_excludes_canceled(self):
        specs = _mixed_specs()
        default_filtered = filter_specs(specs, "default")
        ids = {e["id"] for e in default_filtered}
        self.assertNotIn("q-006", ids, "default mode must exclude canceled specs")

    def test_all_is_full_list(self):
        specs = _mixed_specs()
        self.assertEqual(len(filter_specs(specs, "all")), len(specs))


# ── Test: --running shows subset of default ───────────────────────────────────

class TestRunningSubset(unittest.TestCase):
    """filter_specs("running") returns only running/requeued/assigning specs,
    which is a subset of what default shows."""

    def test_running_is_subset_of_default(self):
        specs = _mixed_specs()
        default_ids = {e["id"] for e in filter_specs(specs, "default")}
        running_ids = {e["id"] for e in filter_specs(specs, "running")}
        self.assertTrue(
            running_ids.issubset(default_ids),
            f"running IDs {running_ids} must be a subset of default IDs {default_ids}",
        )

    def test_running_only_has_running_status(self):
        specs = _mixed_specs()
        for entry in filter_specs(specs, "running"):
            self.assertIn(
                entry["status"],
                ("running", "requeued", "assigning"),
                "running mode must only return running/requeued/assigning entries",
            )

    def test_running_fewer_than_default(self):
        specs = _mixed_specs()
        default_count = len(filter_specs(specs, "default"))
        running_count = len(filter_specs(specs, "running"))
        self.assertLess(running_count, default_count)


# ── Test: rows span >80% of terminal width ────────────────────────────────────

class TestRowWidth(unittest.TestCase):
    """format_spec_row must produce rows that use at least 80% of the columns."""

    COLUMNS = 120
    MIN_FRACTION = 0.80

    def _row_visible_len(self, row: str) -> int:
        """Strip ANSI escape sequences and return visible length."""
        import re
        return len(re.sub(r"\x1b\[[^m]*m", "", row))

    def test_default_row_width(self):
        spec = _entry("q-001", "running", last_at=_now_iso())
        row = format_spec_row(spec, self.COLUMNS, style="default", color=False)
        visible = self._row_visible_len(row)
        min_len = int(self.COLUMNS * self.MIN_FRACTION)
        self.assertGreaterEqual(
            visible, min_len,
            f"default row visible length {visible} < {min_len} (80% of {self.COLUMNS})",
        )

    def test_dag_row_width(self):
        spec = _entry("q-001", "running", last_at=_now_iso())
        spec["_dag_depth"] = 0
        row = format_spec_row(spec, self.COLUMNS, style="dag", color=False)
        visible = self._row_visible_len(row)
        min_len = int(self.COLUMNS * self.MIN_FRACTION)
        self.assertGreaterEqual(
            visible, min_len,
            f"dag row visible length {visible} < {min_len} (80% of {self.COLUMNS})",
        )

    def test_queued_row_width(self):
        spec = _entry("q-002", "queued", last_at=_ago_iso(1))
        row = format_spec_row(spec, self.COLUMNS, style="default", color=False)
        visible = self._row_visible_len(row)
        min_len = int(self.COLUMNS * self.MIN_FRACTION)
        self.assertGreaterEqual(visible, min_len)


# ── Test: "Showing N of M" appears when filtered < total ─────────────────────

class TestShowingNofM(unittest.TestCase):
    """render_status must include "Showing N of M" when view filter hides specs."""

    def test_showing_hint_present_in_default(self):
        specs = _mixed_specs()
        filtered = filter_specs(specs, "default")
        total = len(specs)
        # Only show hint when filtered < total
        self.assertLess(len(filtered), total, "fixture must have specs hidden by default filter")
        out = render_status(
            filtered,
            sort="queue",
            columns=120,
            color=False,
            total_count=total,
            view_mode="default",
        )
        self.assertIn("Showing", out)
        self.assertIn("of", out)
        self.assertIn(str(len(filtered)), out)
        self.assertIn(str(total), out)

    def test_no_hint_when_all_shown(self):
        specs = _active_only_specs()
        filtered = filter_specs(specs, "default")
        total = len(specs)
        # All active specs pass the default filter
        self.assertEqual(len(filtered), total, "fixture must show all specs in default mode")
        out = render_status(
            filtered,
            sort="queue",
            columns=120,
            color=False,
            total_count=total,
            view_mode="default",
        )
        self.assertNotIn("Showing", out)

    def test_no_hint_in_all_mode(self):
        specs = _mixed_specs()
        filtered = filter_specs(specs, "all")
        total = len(specs)
        out = render_status(
            filtered,
            sort="queue",
            columns=120,
            color=False,
            total_count=total,
            view_mode="all",
        )
        self.assertNotIn("Showing", out)


# ── Test: separator line spans full terminal width ────────────────────────────

class TestSeparatorWidth(unittest.TestCase):
    """The ─── separator line must span the full terminal width in all sort modes."""

    COLUMNS = 120

    def _find_separator(self, output: str) -> str | None:
        for line in output.splitlines():
            stripped = line.strip()
            if stripped and all(c in "─\u2500" for c in stripped):
                return stripped
        return None

    def _render(self, sort: str) -> str:
        specs = _mixed_specs()
        filtered = filter_specs(specs, "default")
        return render_status(
            filtered,
            sort=sort,
            columns=self.COLUMNS,
            color=False,
            total_count=len(specs),
            view_mode="default",
        )

    def test_separator_width_queue(self):
        out = self._render("queue")
        sep = self._find_separator(out)
        self.assertIsNotNone(sep, "separator line not found in queue mode output")
        self.assertEqual(len(sep), self.COLUMNS, f"separator is {len(sep)} chars, expected {self.COLUMNS}")

    def test_separator_width_dag(self):
        out = self._render("dag")
        sep = self._find_separator(out)
        self.assertIsNotNone(sep, "separator line not found in dag mode output")
        self.assertEqual(len(sep), self.COLUMNS)

    def test_separator_width_status(self):
        out = self._render("status")
        sep = self._find_separator(out)
        self.assertIsNotNone(sep, "separator line not found in status mode output")
        self.assertEqual(len(sep), self.COLUMNS)


# ── Test: filter_specs is the single public filter entry point ────────────────

class TestFilterSpecsIsPublicInterface(unittest.TestCase):
    """filter_specs must exist and handle all documented mode strings."""

    def test_all_modes_return_list(self):
        specs = _mixed_specs()
        for mode in ("default", "all", "running"):
            result = filter_specs(specs, mode)
            self.assertIsInstance(result, list, f"filter_specs({mode!r}) must return list")

    def test_empty_input(self):
        for mode in ("default", "all", "running"):
            result = filter_specs([], mode)
            self.assertEqual(result, [], f"filter_specs({mode!r}, []) must return []")

    def test_running_mode_excludes_queued(self):
        specs = [
            _entry("q-001", "running"),
            _entry("q-002", "queued"),
            _entry("q-003", "completed"),
        ]
        result = filter_specs(specs, "running")
        ids = {e["id"] for e in result}
        self.assertIn("q-001", ids)
        self.assertNotIn("q-002", ids)
        self.assertNotIn("q-003", ids)


# ── Test: get_terminal_width is callable ──────────────────────────────────────

class TestGetTerminalWidth(unittest.TestCase):
    def test_returns_int(self):
        result = get_terminal_width()
        self.assertIsInstance(result, int)

    def test_returns_positive(self):
        result = get_terminal_width()
        self.assertGreater(result, 0)


if __name__ == "__main__":
    unittest.main()
