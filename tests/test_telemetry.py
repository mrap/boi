"""test_telemetry.py — Tests for BOI telemetry module.

Tests cover:
- Telemetry file creation and updates
- Iteration aggregation
- Duration formatting
- Idempotent iteration recording
- Fallback from iteration files when no telemetry file exists
- Quality and mode field aggregation
- Quality trend computation
- Quality breakdown averaging
- Evolution ratio and productive failure rate

Uses stdlib unittest only (no pytest dependency).
"""

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

# Add parent directory to path so we can import lib modules
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.status import format_duration
from lib.telemetry import (
    _compute_evolution_ratio,
    _compute_productive_failure_rate,
    _compute_quality_breakdown,
    _compute_quality_trend,
    _read_telemetry_file,
    _write_telemetry_file,
    compute_evolution_ratio_from_spec,
    compute_first_pass_rate,
    load_iteration_files,
    read_telemetry,
    update_telemetry,
)


class TelemetryTestCase(unittest.TestCase):
    """Base test case that creates temp queue and log directories."""

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.queue_dir = os.path.join(self._tmpdir.name, "queue")
        self.log_dir = os.path.join(self._tmpdir.name, "logs")
        os.makedirs(self.queue_dir)
        os.makedirs(self.log_dir)

    def tearDown(self):
        self._tmpdir.cleanup()

    def _write_queue_entry(self, queue_id, **kwargs):
        """Write a minimal queue entry JSON file."""
        entry = {
            "id": queue_id,
            "spec_path": kwargs.get("spec_path", f"/tmp/spec-{queue_id}.md"),
            "worktree": None,
            "priority": kwargs.get("priority", 100),
            "status": kwargs.get("status", "running"),
            "submitted_at": "2026-03-06T10:00:00+00:00",
            "iteration": kwargs.get("iteration", 1),
            "max_iterations": kwargs.get("max_iterations", 30),
            "blocked_by": [],
            "last_worker": kwargs.get("last_worker"),
            "last_iteration_at": None,
            "consecutive_failures": kwargs.get("consecutive_failures", 0),
            "tasks_done": kwargs.get("tasks_done", 0),
            "tasks_total": kwargs.get("tasks_total", 5),
        }
        path = Path(self.queue_dir) / f"{queue_id}.json"
        path.write_text(json.dumps(entry, indent=2) + "\n", encoding="utf-8")
        return entry

    def _write_iteration_file(self, queue_id, iteration, **kwargs):
        """Write a mock iteration-N.json file."""
        data = {
            "queue_id": queue_id,
            "iteration": iteration,
            "exit_code": kwargs.get("exit_code", 0),
            "duration_seconds": kwargs.get("duration_seconds", 600),
            "started_at": kwargs.get("started_at", "2026-03-06T10:00:00Z"),
            "pre_counts": kwargs.get(
                "pre_counts", {"pending": 3, "done": 0, "skipped": 0, "total": 3}
            ),
            "post_counts": kwargs.get(
                "post_counts", {"pending": 1, "done": 2, "skipped": 0, "total": 3}
            ),
            "tasks_completed": kwargs.get("tasks_completed", 2),
            "tasks_added": kwargs.get("tasks_added", 0),
            "tasks_skipped": kwargs.get("tasks_skipped", 0),
        }
        # Add optional quality/mode fields if provided
        if "mode" in kwargs:
            data["mode"] = kwargs["mode"]
        if "quality_score" in kwargs:
            data["quality_score"] = kwargs["quality_score"]
        if "quality_grade" in kwargs:
            data["quality_grade"] = kwargs["quality_grade"]
        if "quality_signals" in kwargs:
            data["quality_signals"] = kwargs["quality_signals"]
        if "progress_score" in kwargs:
            data["progress_score"] = kwargs["progress_score"]
        if "task_completed" in kwargs:
            data["task_completed"] = kwargs["task_completed"]
        if "tasks_superseded" in kwargs:
            data["tasks_superseded"] = kwargs["tasks_superseded"]
        if "challenges_written" in kwargs:
            data["challenges_written"] = kwargs["challenges_written"]
        if "experiments_proposed" in kwargs:
            data["experiments_proposed"] = kwargs["experiments_proposed"]
        if "experiments_adopted" in kwargs:
            data["experiments_adopted"] = kwargs["experiments_adopted"]
        if "experiments_rejected" in kwargs:
            data["experiments_rejected"] = kwargs["experiments_rejected"]

        path = Path(self.queue_dir) / f"{queue_id}.iteration-{iteration}.json"
        path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
        return data

    def _write_log_file(self, queue_id, iteration, content):
        """Write a mock worker log file."""
        path = Path(self.log_dir) / f"{queue_id}-iter-{iteration}.log"
        path.write_text(content, encoding="utf-8")
        return str(path)


# ─── Read/Write Tests ───────────────────────────────────────────────────────


class TestTelemetryReadWrite(TelemetryTestCase):
    def test_write_and_read(self):
        """Telemetry file is written and readable."""
        data = {
            "queue_id": "q-001",
            "total_iterations": 2,
            "total_time_seconds": 1200,
        }
        _write_telemetry_file(self.queue_dir, "q-001", data)

        result = _read_telemetry_file(self.queue_dir, "q-001")
        self.assertIsNotNone(result)
        self.assertEqual(result["queue_id"], "q-001")
        self.assertEqual(result["total_iterations"], 2)
        self.assertEqual(result["total_time_seconds"], 1200)

    def test_read_nonexistent(self):
        """Reading a nonexistent telemetry file returns None."""
        result = _read_telemetry_file(self.queue_dir, "q-999")
        self.assertIsNone(result)

    def test_read_malformed(self):
        """Reading a malformed telemetry file returns None."""
        path = Path(self.queue_dir) / "q-001.telemetry.json"
        path.write_text("not json!", encoding="utf-8")
        result = _read_telemetry_file(self.queue_dir, "q-001")
        self.assertIsNone(result)

    def test_write_creates_dir(self):
        """Writing creates the queue directory if needed."""
        new_dir = os.path.join(self._tmpdir.name, "new_queue")
        _write_telemetry_file(new_dir, "q-001", {"queue_id": "q-001"})
        self.assertTrue(os.path.isfile(os.path.join(new_dir, "q-001.telemetry.json")))

    def test_write_atomic(self):
        """No .tmp file remains after write."""
        _write_telemetry_file(self.queue_dir, "q-001", {"queue_id": "q-001"})
        tmp_path = Path(self.queue_dir) / ".q-001.telemetry.json.tmp"
        self.assertFalse(tmp_path.exists())

    def test_overwrite(self):
        """Writing overwrites existing data."""
        _write_telemetry_file(self.queue_dir, "q-001", {"total_iterations": 1})
        _write_telemetry_file(self.queue_dir, "q-001", {"total_iterations": 5})
        result = _read_telemetry_file(self.queue_dir, "q-001")
        self.assertEqual(result["total_iterations"], 5)


# ─── Load Iteration Files Tests ─────────────────────────────────────────────


class TestLoadIterationFiles(TelemetryTestCase):
    def test_empty_dir(self):
        """No iteration files returns empty list."""
        result = load_iteration_files(self.queue_dir, "q-001")
        self.assertEqual(result, [])

    def test_single_iteration(self):
        """Single iteration file loads correctly."""
        self._write_iteration_file("q-001", 1, tasks_completed=2, duration_seconds=300)
        result = load_iteration_files(self.queue_dir, "q-001")
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]["iteration"], 1)
        self.assertEqual(result[0]["tasks_completed"], 2)
        self.assertEqual(result[0]["duration_seconds"], 300)

    def test_multiple_iterations_sorted(self):
        """Multiple iterations are returned sorted by iteration number."""
        self._write_iteration_file("q-001", 3, tasks_completed=1)
        self._write_iteration_file("q-001", 1, tasks_completed=2)
        self._write_iteration_file("q-001", 2, tasks_completed=3)
        result = load_iteration_files(self.queue_dir, "q-001")
        self.assertEqual(len(result), 3)
        self.assertEqual([r["iteration"] for r in result], [1, 2, 3])

    def test_different_queue_ids_isolated(self):
        """Iteration files from different specs don't mix."""
        self._write_iteration_file("q-001", 1, tasks_completed=2)
        self._write_iteration_file("q-002", 1, tasks_completed=5)
        result = load_iteration_files(self.queue_dir, "q-001")
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]["tasks_completed"], 2)

    def test_nonexistent_dir(self):
        """Nonexistent directory returns empty list."""
        result = load_iteration_files("/nonexistent/path", "q-001")
        self.assertEqual(result, [])

    def test_skip_malformed(self):
        """Malformed iteration files are skipped."""
        self._write_iteration_file("q-001", 1, tasks_completed=2)
        # Write a malformed iteration file
        bad_path = Path(self.queue_dir) / "q-001.iteration-2.json"
        bad_path.write_text("not valid json", encoding="utf-8")
        result = load_iteration_files(self.queue_dir, "q-001")
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]["iteration"], 1)


# ─── Update Telemetry Tests ─────────────────────────────────────────────────


class TestUpdateTelemetry(TelemetryTestCase):
    def test_first_iteration(self):
        """First iteration creates telemetry file."""
        self._write_queue_entry("q-001", consecutive_failures=0)
        self._write_iteration_file(
            "q-001",
            1,
            tasks_completed=2,
            tasks_added=1,
            tasks_skipped=0,
            duration_seconds=600,
        )

        result = update_telemetry(self.queue_dir, "q-001")

        self.assertEqual(result["queue_id"], "q-001")
        self.assertEqual(result["total_iterations"], 1)
        self.assertEqual(result["total_time_seconds"], 600)
        self.assertEqual(result["tasks_completed_per_iteration"], [2])
        self.assertEqual(result["tasks_added_per_iteration"], [1])
        self.assertEqual(result["tasks_skipped_per_iteration"], [0])

        # Verify file was written
        saved = _read_telemetry_file(self.queue_dir, "q-001")
        self.assertIsNotNone(saved)
        self.assertEqual(saved["total_iterations"], 1)

    def test_multiple_iterations(self):
        """Aggregates across multiple iterations."""
        self._write_queue_entry("q-001")
        self._write_iteration_file(
            "q-001",
            1,
            tasks_completed=2,
            tasks_added=1,
            tasks_skipped=0,
            duration_seconds=600,
        )
        self._write_iteration_file(
            "q-001",
            2,
            tasks_completed=3,
            tasks_added=0,
            tasks_skipped=1,
            duration_seconds=900,
        )
        self._write_iteration_file(
            "q-001",
            3,
            tasks_completed=1,
            tasks_added=2,
            tasks_skipped=0,
            duration_seconds=450,
        )

        result = update_telemetry(self.queue_dir, "q-001")

        self.assertEqual(result["total_iterations"], 3)
        self.assertEqual(result["total_time_seconds"], 1950)
        self.assertEqual(result["tasks_completed_per_iteration"], [2, 3, 1])
        self.assertEqual(result["tasks_added_per_iteration"], [1, 0, 2])
        self.assertEqual(result["tasks_skipped_per_iteration"], [0, 1, 0])

    def test_consecutive_failures_from_queue(self):
        """Reads consecutive_failures from queue entry."""
        self._write_queue_entry("q-001", consecutive_failures=3)
        self._write_iteration_file("q-001", 1, tasks_completed=1, duration_seconds=100)

        result = update_telemetry(self.queue_dir, "q-001")
        self.assertEqual(result["consecutive_failures"], 3)

    def test_idempotent(self):
        """Calling update_telemetry twice produces same results."""
        self._write_queue_entry("q-001")
        self._write_iteration_file("q-001", 1, tasks_completed=2, duration_seconds=600)

        result1 = update_telemetry(self.queue_dir, "q-001")
        result2 = update_telemetry(self.queue_dir, "q-001")

        # Remove last_updated for comparison (timestamps differ)
        result1.pop("last_updated", None)
        result2.pop("last_updated", None)
        self.assertEqual(result1, result2)

    def test_no_queue_entry(self):
        """Works even without a queue entry (returns 0 failures)."""
        self._write_iteration_file("q-001", 1, tasks_completed=2, duration_seconds=600)

        result = update_telemetry(self.queue_dir, "q-001")
        self.assertEqual(result["consecutive_failures"], 0)

    def test_no_iteration_files(self):
        """Returns empty aggregation when no iteration files exist."""
        self._write_queue_entry("q-001")
        result = update_telemetry(self.queue_dir, "q-001")

        self.assertEqual(result["total_iterations"], 0)
        self.assertEqual(result["total_time_seconds"], 0)
        self.assertEqual(result["tasks_completed_per_iteration"], [])

    def test_writes_file_to_disk(self):
        """Telemetry file is persisted to disk."""
        self._write_queue_entry("q-001")
        self._write_iteration_file("q-001", 1, tasks_completed=2, duration_seconds=600)

        update_telemetry(self.queue_dir, "q-001")

        telemetry_path = Path(self.queue_dir) / "q-001.telemetry.json"
        self.assertTrue(telemetry_path.is_file())
        data = json.loads(telemetry_path.read_text(encoding="utf-8"))
        self.assertEqual(data["queue_id"], "q-001")


# ─── Quality Fields in Update Telemetry ──────────────────────────────────────


class TestUpdateTelemetryQuality(TelemetryTestCase):
    def test_quality_fields_present(self):
        """Quality and mode fields are present in telemetry output."""
        self._write_queue_entry("q-001")
        self._write_iteration_file(
            "q-001",
            1,
            tasks_completed=1,
            mode="discover",
            quality_score=0.82,
            quality_grade="B",
            quality_signals={
                "code_quality": 0.85,
                "test_quality": 0.75,
                "documentation": 0.90,
                "architecture": 0.78,
            },
            progress_score=0.51,
            challenges_written=1,
            experiments_proposed=0,
        )
        result = update_telemetry(self.queue_dir, "q-001")

        self.assertIn("quality_score_per_iteration", result)
        self.assertIn("quality_breakdown", result)
        self.assertIn("quality_trend", result)
        self.assertIn("quality_alerts", result)
        self.assertIn("mode_per_iteration", result)
        self.assertIn("evolution_ratio", result)
        self.assertIn("productive_failure_rate", result)
        self.assertIn("tasks_superseded_per_iteration", result)
        self.assertIn("challenges_written_per_iteration", result)
        self.assertIn("experiments_proposed_per_iteration", result)
        self.assertIn("experiments_adopted_per_iteration", result)
        self.assertIn("experiments_rejected_per_iteration", result)

    def test_quality_score_per_iteration(self):
        """Quality scores are collected per iteration."""
        self._write_queue_entry("q-001")
        self._write_iteration_file("q-001", 1, tasks_completed=1, quality_score=0.78)
        self._write_iteration_file("q-001", 2, tasks_completed=1, quality_score=0.82)
        self._write_iteration_file("q-001", 3, tasks_completed=1)  # no quality

        result = update_telemetry(self.queue_dir, "q-001")
        self.assertEqual(result["quality_score_per_iteration"], [0.78, 0.82, None])

    def test_mode_per_iteration(self):
        """Modes are collected per iteration."""
        self._write_queue_entry("q-001")
        self._write_iteration_file("q-001", 1, tasks_completed=1, mode="discover")
        self._write_iteration_file("q-001", 2, tasks_completed=1, mode="discover")
        self._write_iteration_file("q-001", 3, tasks_completed=1, mode="execute")

        result = update_telemetry(self.queue_dir, "q-001")
        self.assertEqual(
            result["mode_per_iteration"], ["discover", "discover", "execute"]
        )

    def test_quality_breakdown_averages(self):
        """Quality breakdown averages category scores across iterations."""
        self._write_queue_entry("q-001")
        self._write_iteration_file(
            "q-001",
            1,
            tasks_completed=1,
            quality_signals={
                "code_quality": 0.80,
                "test_quality": 0.70,
                "documentation": 0.90,
                "architecture": 0.60,
            },
        )
        self._write_iteration_file(
            "q-001",
            2,
            tasks_completed=1,
            quality_signals={
                "code_quality": 0.90,
                "test_quality": 0.80,
                "documentation": 0.80,
                "architecture": 0.80,
            },
        )

        result = update_telemetry(self.queue_dir, "q-001")
        breakdown = result["quality_breakdown"]
        self.assertIsNotNone(breakdown)
        self.assertAlmostEqual(breakdown["code_quality"], 0.85)
        self.assertAlmostEqual(breakdown["test_quality"], 0.75)
        self.assertAlmostEqual(breakdown["documentation"], 0.85)
        self.assertAlmostEqual(breakdown["architecture"], 0.70)

    def test_quality_breakdown_null_when_no_quality(self):
        """Quality breakdown is None when no iterations have quality data."""
        self._write_queue_entry("q-001")
        self._write_iteration_file("q-001", 1, tasks_completed=1)

        result = update_telemetry(self.queue_dir, "q-001")
        self.assertIsNone(result["quality_breakdown"])

    def test_quality_trend_improving(self):
        """Quality trend is 'improving' when scores go up."""
        self._write_queue_entry("q-001")
        self._write_iteration_file("q-001", 1, tasks_completed=1, quality_score=0.60)
        self._write_iteration_file("q-001", 2, tasks_completed=1, quality_score=0.70)
        self._write_iteration_file("q-001", 3, tasks_completed=1, quality_score=0.80)

        result = update_telemetry(self.queue_dir, "q-001")
        self.assertEqual(result["quality_trend"], "improving")

    def test_quality_trend_declining(self):
        """Quality trend is 'declining' when scores drop."""
        self._write_queue_entry("q-001")
        self._write_iteration_file("q-001", 1, tasks_completed=1, quality_score=0.90)
        self._write_iteration_file("q-001", 2, tasks_completed=1, quality_score=0.80)
        self._write_iteration_file("q-001", 3, tasks_completed=1, quality_score=0.70)

        result = update_telemetry(self.queue_dir, "q-001")
        self.assertEqual(result["quality_trend"], "declining")

    def test_quality_trend_stable(self):
        """Quality trend is 'stable' when scores don't change much."""
        self._write_queue_entry("q-001")
        self._write_iteration_file("q-001", 1, tasks_completed=1, quality_score=0.80)
        self._write_iteration_file("q-001", 2, tasks_completed=1, quality_score=0.81)
        self._write_iteration_file("q-001", 3, tasks_completed=1, quality_score=0.80)

        result = update_telemetry(self.queue_dir, "q-001")
        self.assertEqual(result["quality_trend"], "stable")

    def test_quality_alerts_from_trend(self):
        """Quality alerts are populated from trend detection."""
        self._write_queue_entry("q-001")
        # Create declining quality: 3 consecutive drops > 0.10 total
        self._write_iteration_file("q-001", 1, tasks_completed=1, quality_score=0.90)
        self._write_iteration_file("q-001", 2, tasks_completed=1, quality_score=0.85)
        self._write_iteration_file("q-001", 3, tasks_completed=1, quality_score=0.78)
        self._write_iteration_file("q-001", 4, tasks_completed=1, quality_score=0.70)

        result = update_telemetry(self.queue_dir, "q-001")
        self.assertIsInstance(result["quality_alerts"], list)
        # Should have a declining quality alert
        types = [a["type"] for a in result["quality_alerts"]]
        self.assertIn("declining_quality", types)

    def test_mode_fields_aggregated(self):
        """Mode-specific fields (superseded, challenges, experiments) are aggregated."""
        self._write_queue_entry("q-001")
        self._write_iteration_file(
            "q-001",
            1,
            tasks_completed=1,
            mode="challenge",
            tasks_superseded=0,
            challenges_written=2,
            experiments_proposed=1,
            experiments_adopted=0,
            experiments_rejected=0,
        )
        self._write_iteration_file(
            "q-001",
            2,
            tasks_completed=1,
            mode="challenge",
            tasks_superseded=1,
            challenges_written=1,
            experiments_proposed=0,
            experiments_adopted=1,
            experiments_rejected=0,
        )

        result = update_telemetry(self.queue_dir, "q-001")
        self.assertEqual(result["tasks_superseded_per_iteration"], [0, 1])
        self.assertEqual(result["challenges_written_per_iteration"], [2, 1])
        self.assertEqual(result["experiments_proposed_per_iteration"], [1, 0])
        self.assertEqual(result["experiments_adopted_per_iteration"], [0, 1])
        self.assertEqual(result["experiments_rejected_per_iteration"], [0, 0])

    def test_no_quality_data_gives_null_fields(self):
        """When no quality data exists, quality fields are null/empty."""
        self._write_queue_entry("q-001")
        self._write_iteration_file("q-001", 1, tasks_completed=1)

        result = update_telemetry(self.queue_dir, "q-001")
        self.assertEqual(result["quality_score_per_iteration"], [None])
        self.assertIsNone(result["quality_breakdown"])
        self.assertEqual(result["quality_trend"], "stable")
        self.assertEqual(result["quality_alerts"], [])


# ─── Quality Trend Computation Tests ────────────────────────────────────────


class TestQualityTrend(unittest.TestCase):
    def test_improving(self):
        self.assertEqual(_compute_quality_trend([0.60, 0.70, 0.80]), "improving")

    def test_declining(self):
        self.assertEqual(_compute_quality_trend([0.90, 0.80, 0.70]), "declining")

    def test_stable(self):
        self.assertEqual(_compute_quality_trend([0.80, 0.81, 0.80]), "stable")

    def test_single_score(self):
        self.assertEqual(_compute_quality_trend([0.80]), "stable")

    def test_empty(self):
        self.assertEqual(_compute_quality_trend([]), "stable")

    def test_all_none(self):
        self.assertEqual(_compute_quality_trend([None, None, None]), "stable")

    def test_mixed_none(self):
        """None values are skipped."""
        self.assertEqual(_compute_quality_trend([0.60, None, 0.80]), "improving")

    def test_small_change_is_stable(self):
        """Changes within 0.05 threshold are stable."""
        self.assertEqual(_compute_quality_trend([0.80, 0.82, 0.84]), "stable")


# ─── Quality Breakdown Tests ────────────────────────────────────────────────


class TestQualityBreakdown(unittest.TestCase):
    def test_single_iteration(self):
        iters = [
            {
                "quality_signals": {
                    "code_quality": 0.80,
                    "test_quality": 0.70,
                    "documentation": 0.90,
                    "architecture": 0.60,
                }
            }
        ]
        result = _compute_quality_breakdown(iters)
        self.assertAlmostEqual(result["code_quality"], 0.80)
        self.assertAlmostEqual(result["test_quality"], 0.70)

    def test_no_quality_data(self):
        iters = [{"tasks_completed": 1}]
        result = _compute_quality_breakdown(iters)
        self.assertIsNone(result)

    def test_partial_categories(self):
        """Handles iterations where some categories are None."""
        iters = [
            {
                "quality_signals": {
                    "code_quality": 0.80,
                    "test_quality": None,
                    "documentation": 0.90,
                    "architecture": 0.70,
                }
            }
        ]
        result = _compute_quality_breakdown(iters)
        self.assertAlmostEqual(result["code_quality"], 0.80)
        self.assertIsNone(result["test_quality"])
        self.assertAlmostEqual(result["documentation"], 0.90)


# ─── Evolution Ratio Tests ──────────────────────────────────────────────────


class TestEvolutionRatio(unittest.TestCase):
    def test_no_completions(self):
        iters = [{"tasks_completed": 0, "tasks_added": 0}]
        self.assertIsNone(_compute_evolution_ratio(iters))

    def test_no_additions(self):
        iters = [{"tasks_completed": 5, "tasks_added": 0}]
        self.assertAlmostEqual(_compute_evolution_ratio(iters), 0.0)

    def test_some_additions(self):
        iters = [
            {"tasks_completed": 3, "tasks_added": 0},
            {"tasks_completed": 1, "tasks_added": 2},
        ]
        # total_completed=4, total_added=2, ratio=2/4=0.5
        self.assertAlmostEqual(_compute_evolution_ratio(iters), 0.5)

    def test_capped_at_one(self):
        iters = [
            {"tasks_completed": 1, "tasks_added": 5},
        ]
        # ratio would be 5/1=5.0, capped at 1.0
        self.assertAlmostEqual(_compute_evolution_ratio(iters), 1.0)


# ─── Productive Failure Rate Tests ──────────────────────────────────────────


class TestProductiveFailureRate(unittest.TestCase):
    def test_no_failures(self):
        iters = [{"tasks_completed": 1, "tasks_added": 0}]
        self.assertIsNone(_compute_productive_failure_rate(iters))

    def test_all_productive_failures(self):
        iters = [
            {"tasks_completed": 0, "tasks_added": 1},
            {"tasks_completed": 0, "tasks_added": 2},
        ]
        self.assertAlmostEqual(_compute_productive_failure_rate(iters), 1.0)

    def test_mixed_failures(self):
        iters = [
            {"tasks_completed": 0, "tasks_added": 1},  # productive
            {"tasks_completed": 0, "tasks_added": 0},  # unproductive
            {"tasks_completed": 1, "tasks_added": 0},  # not a failure
        ]
        # 2 failures, 1 productive -> 0.5
        self.assertAlmostEqual(_compute_productive_failure_rate(iters), 0.5)

    def test_no_productive_failures(self):
        iters = [
            {"tasks_completed": 0, "tasks_added": 0},
            {"tasks_completed": 0, "tasks_added": 0},
        ]
        self.assertAlmostEqual(_compute_productive_failure_rate(iters), 0.0)


# ─── Read Telemetry Tests ────────────────────────────────────────────────────


class TestReadTelemetry(TelemetryTestCase):
    def test_read_existing(self):
        """Reads persisted telemetry file."""
        _write_telemetry_file(
            self.queue_dir,
            "q-001",
            {
                "queue_id": "q-001",
                "total_iterations": 5,
            },
        )
        result = read_telemetry(self.queue_dir, "q-001")
        self.assertIsNotNone(result)
        self.assertEqual(result["total_iterations"], 5)

    def test_read_nonexistent(self):
        """Returns None for nonexistent spec."""
        result = read_telemetry(self.queue_dir, "q-999")
        self.assertIsNone(result)


# ─── Format Duration Tests ──────────────────────────────────────────────────


class TestFormatDuration(unittest.TestCase):
    def test_seconds(self):
        self.assertEqual(format_duration(45), "45s")

    def test_minutes(self):
        self.assertEqual(format_duration(125), "2m 05s")

    def test_hours(self):
        self.assertEqual(format_duration(3661), "1h 01m")

    def test_zero(self):
        self.assertEqual(format_duration(0), "0s")

    def test_exactly_one_minute(self):
        self.assertEqual(format_duration(60), "1m 00s")

    def test_float_input(self):
        self.assertEqual(format_duration(90.7), "1m 30s")


# ─── Integration with Queue (telemetry doesn't break queue.get_queue) ─────


class TestTelemetryQueueIsolation(TelemetryTestCase):
    def _make_spec(self, name="spec.md"):
        path = os.path.join(self._tmpdir.name, name)
        Path(path).write_text(
            "# Spec\n\n## Tasks\n\n### t-1: Task\nPENDING\n\n**Spec:** Do.\n**Verify:** ok\n"
        )
        return path

    def test_telemetry_file_not_in_queue(self):
        """Telemetry files don't pollute get_queue results."""
        from lib.queue import enqueue, get_queue

        enqueue(self.queue_dir, self._make_spec(), queue_id="q-001")
        _write_telemetry_file(
            self.queue_dir,
            "q-001",
            {
                "queue_id": "q-001",
                "total_iterations": 3,
            },
        )

        entries = get_queue(self.queue_dir)
        self.assertEqual(len(entries), 1)
        self.assertEqual(entries[0]["id"], "q-001")

    def test_iteration_files_not_in_queue(self):
        """Iteration files don't pollute get_queue results."""
        from lib.queue import enqueue, get_queue

        enqueue(self.queue_dir, self._make_spec("spec2.md"), queue_id="q-001")
        self._write_iteration_file("q-001", 1, tasks_completed=2)

        entries = get_queue(self.queue_dir)
        self.assertEqual(len(entries), 1)


# ─── End-to-End: 3 Iterations with Quality ──────────────────────────────────


class TestEndToEndQualityTelemetry(TelemetryTestCase):
    def test_three_iterations_with_quality(self):
        """Simulate 3 iterations with quality data and verify all fields."""
        self._write_queue_entry("q-001")

        # Iteration 1: discover mode, quality 0.78
        self._write_iteration_file(
            "q-001",
            1,
            tasks_completed=1,
            tasks_added=1,
            tasks_skipped=0,
            mode="discover",
            quality_score=0.78,
            quality_grade="C",
            quality_signals={
                "code_quality": 0.80,
                "test_quality": 0.70,
                "documentation": 0.85,
                "architecture": 0.75,
            },
            progress_score=0.30,
            task_completed="t-1",
            challenges_written=0,
            experiments_proposed=0,
            experiments_adopted=0,
            experiments_rejected=0,
        )

        # Iteration 2: discover mode, quality 0.82
        self._write_iteration_file(
            "q-001",
            2,
            tasks_completed=1,
            tasks_added=0,
            tasks_skipped=0,
            mode="discover",
            quality_score=0.82,
            quality_grade="B",
            quality_signals={
                "code_quality": 0.85,
                "test_quality": 0.75,
                "documentation": 0.88,
                "architecture": 0.80,
            },
            progress_score=0.45,
            task_completed="t-2",
            challenges_written=1,
            experiments_proposed=0,
            experiments_adopted=0,
            experiments_rejected=0,
        )

        # Iteration 3: discover mode, quality 0.85
        self._write_iteration_file(
            "q-001",
            3,
            tasks_completed=1,
            tasks_added=0,
            tasks_skipped=0,
            mode="discover",
            quality_score=0.85,
            quality_grade="B",
            quality_signals={
                "code_quality": 0.88,
                "test_quality": 0.80,
                "documentation": 0.90,
                "architecture": 0.82,
            },
            progress_score=0.60,
            task_completed="t-3",
            challenges_written=0,
            experiments_proposed=0,
            experiments_adopted=0,
            experiments_rejected=0,
        )

        result = update_telemetry(self.queue_dir, "q-001")

        # Verify basic fields
        self.assertEqual(result["total_iterations"], 3)

        # Verify quality fields
        self.assertEqual(result["quality_score_per_iteration"], [0.78, 0.82, 0.85])
        self.assertEqual(result["quality_trend"], "improving")
        self.assertEqual(result["quality_alerts"], [])  # no alerts for improving

        # Verify breakdown averages
        breakdown = result["quality_breakdown"]
        self.assertIsNotNone(breakdown)
        self.assertAlmostEqual(breakdown["code_quality"], (0.80 + 0.85 + 0.88) / 3)
        self.assertAlmostEqual(breakdown["test_quality"], (0.70 + 0.75 + 0.80) / 3)

        # Verify mode fields
        self.assertEqual(
            result["mode_per_iteration"], ["discover", "discover", "discover"]
        )
        self.assertEqual(result["challenges_written_per_iteration"], [0, 1, 0])

        # Verify Deutschian metrics
        # total_completed=3, total_added=1 -> evolution_ratio=1/3
        # Note: without initial_task_ids in queue entry, uses iteration-based approx
        self.assertAlmostEqual(result["evolution_ratio"], 1.0 / 3.0, places=4)
        # No failed iterations -> productive_failure_rate is None
        self.assertIsNone(result["productive_failure_rate"])
        # first_pass_rate: no spec_path in queue entry -> None
        # (the test queue entry has a dummy spec_path)

        # Verify persisted to disk
        saved = read_telemetry(self.queue_dir, "q-001")
        self.assertIsNotNone(saved)
        self.assertEqual(saved["quality_score_per_iteration"], [0.78, 0.82, 0.85])
        self.assertEqual(saved["quality_trend"], "improving")


# ─── Evolution Ratio From Spec Tests ──────────────────────────────────────


class TestEvolutionRatioFromSpec(unittest.TestCase):
    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.spec_dir = self._tmpdir.name

    def tearDown(self):
        self._tmpdir.cleanup()

    def _write_spec(self, content, name="spec.md"):
        path = os.path.join(self.spec_dir, name)
        Path(path).write_text(content, encoding="utf-8")
        return path

    def test_no_done_tasks(self):
        """Returns None when no tasks are DONE."""
        spec = self._write_spec("# Spec\n\n### t-1: Task A\nPENDING\n\n**Spec:** Do.\n")
        result = compute_evolution_ratio_from_spec(["t-1"], spec)
        self.assertIsNone(result)

    def test_no_self_evolved_tasks(self):
        """Returns 0.0 when all done tasks were in the original set."""
        spec = self._write_spec(
            "# Spec\n\n### t-1: Task A\nDONE\n\n**Spec:** Do.\n"
            "### t-2: Task B\nDONE\n\n**Spec:** Do.\n"
        )
        result = compute_evolution_ratio_from_spec(["t-1", "t-2"], spec)
        self.assertAlmostEqual(result, 0.0)

    def test_some_self_evolved(self):
        """Counts self-evolved DONE tasks correctly."""
        spec = self._write_spec(
            "# Spec\n\n### t-1: Task A\nDONE\n\n**Spec:** Original.\n"
            "### t-2: Task B\nDONE\n\n**Spec:** Original.\n"
            "### t-3: Task C\nDONE\n\n**Spec:** Self-evolved.\n"
            "### t-4: Task D\nSKIPPED\n\n**Spec:** Self-evolved but skipped.\n"
        )
        # Original had t-1, t-2. t-3 is self-evolved and DONE, t-4 is self-evolved but SKIPPED.
        # total done = 3 (t-1, t-2, t-3), self-evolved done = 1 (t-3)
        result = compute_evolution_ratio_from_spec(["t-1", "t-2"], spec)
        self.assertAlmostEqual(result, 1.0 / 3.0)

    def test_all_self_evolved(self):
        """All DONE tasks are self-evolved (original had none)."""
        spec = self._write_spec(
            "# Spec\n\n### t-1: New A\nDONE\n\n**Spec:** Do.\n"
            "### t-2: New B\nDONE\n\n**Spec:** Do.\n"
        )
        result = compute_evolution_ratio_from_spec([], spec)
        self.assertAlmostEqual(result, 1.0)

    def test_missing_spec_file(self):
        """Returns None when spec file doesn't exist."""
        result = compute_evolution_ratio_from_spec(["t-1"], "/nonexistent/spec.md")
        self.assertIsNone(result)

    def test_verification_scenario(self):
        """Verify per spec: 3 self-evolved tasks, 2 DONE, 1 SKIPPED.
        evolution_ratio = 2 / (original_done + 2)."""
        spec = self._write_spec(
            "# Spec\n\n"
            "### t-1: Original A\nDONE\n\n**Spec:** Do.\n"
            "### t-2: Original B\nDONE\n\n**Spec:** Do.\n"
            "### t-3: Original C\nDONE\n\n**Spec:** Do.\n"
            "### t-4: Self-evolved A\nDONE\n\n**Spec:** Do.\n"
            "### t-5: Self-evolved B\nDONE\n\n**Spec:** Do.\n"
            "### t-6: Self-evolved C\nSKIPPED\n\n**Spec:** Skipped.\n"
        )
        # Original: t-1, t-2, t-3 (3 done). Self-evolved: t-4 (DONE), t-5 (DONE), t-6 (SKIPPED)
        # total_done = 5, self_evolved_done = 2
        # evolution_ratio = 2 / (3 + 2) = 2/5 = 0.4
        result = compute_evolution_ratio_from_spec(["t-1", "t-2", "t-3"], spec)
        self.assertAlmostEqual(result, 2.0 / 5.0)


# ─── First-Pass Completion Rate Tests ──────────────────────────────────────


class TestFirstPassRate(unittest.TestCase):
    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.spec_dir = self._tmpdir.name

    def tearDown(self):
        self._tmpdir.cleanup()

    def _write_spec(self, content, name="spec.md"):
        path = os.path.join(self.spec_dir, name)
        Path(path).write_text(content, encoding="utf-8")
        return path

    def test_no_done_tasks(self):
        """Returns None when no tasks are DONE."""
        spec = self._write_spec("# Spec\n\n### t-1: Task A\nPENDING\n\n**Spec:** Do.\n")
        result = compute_first_pass_rate(spec)
        self.assertIsNone(result)

    def test_all_first_pass(self):
        """Returns 1.0 when no critic tasks exist."""
        spec = self._write_spec(
            "# Spec\n\n### t-1: Task A\nDONE\n\n**Spec:** Do.\n"
            "### t-2: Task B\nDONE\n\n**Spec:** Do.\n"
        )
        result = compute_first_pass_rate(spec)
        self.assertAlmostEqual(result, 1.0)

    def test_some_critic_rejections(self):
        """Tasks targeted by [CRITIC] tasks reduce the rate."""
        spec = self._write_spec(
            "# Spec\n\n"
            "### t-1: Task A\nDONE\n\n**Spec:** Do.\n"
            "### t-2: Task B\nDONE\n\n**Spec:** Do.\n"
            "### t-3: [CRITIC] Quality score for t-1 below threshold\n"
            "DONE\n\n**Spec:** Fix t-1 quality.\n"
        )
        # t-1 was targeted by critic (t-3 references t-1 in body+title)
        # t-2 was not targeted, t-3 is a critic task itself (DONE)
        # Done tasks: t-1, t-2, t-3. Targeted: t-1. Without critic: t-2, t-3 = 2/3
        result = compute_first_pass_rate(spec)
        self.assertAlmostEqual(result, 2.0 / 3.0)

    def test_critic_targeting_multiple(self):
        """Critic task references multiple task IDs."""
        spec = self._write_spec(
            "# Spec\n\n"
            "### t-1: Task A\nDONE\n\n**Spec:** Do.\n"
            "### t-2: Task B\nDONE\n\n**Spec:** Do.\n"
            "### t-3: Task C\nDONE\n\n**Spec:** Do.\n"
            "### t-4: [CRITIC] Fix quality for t-1 and t-2\n"
            "PENDING\n\n**Spec:** Issues found in t-1 error handling and t-2 tests.\n"
        )
        # t-1, t-2 targeted by critic. t-3 not targeted. t-4 is PENDING (not DONE).
        # Done tasks: t-1, t-2, t-3. Targeted: t-1, t-2. Without critic: t-3 = 1/3
        result = compute_first_pass_rate(spec)
        self.assertAlmostEqual(result, 1.0 / 3.0)

    def test_missing_spec_file(self):
        """Returns None when spec file doesn't exist."""
        result = compute_first_pass_rate("/nonexistent/spec.md")
        self.assertIsNone(result)

    def test_no_critic_tasks(self):
        """All tasks pass first time when no critic tasks exist."""
        spec = self._write_spec(
            "# Spec\n\n"
            "### t-1: Task A\nDONE\n\n**Spec:** Do.\n"
            "### t-2: Task B\nDONE\n\n**Spec:** Do.\n"
            "### t-3: Task C\nDONE\n\n**Spec:** Do.\n"
        )
        result = compute_first_pass_rate(spec)
        self.assertAlmostEqual(result, 1.0)


# ─── Productive Failure Rate Verification ──────────────────────────────────


class TestProductiveFailureRateVerification(unittest.TestCase):
    def test_verification_scenario(self):
        """Verify per spec: 2 failed iterations, 1 productive -> rate = 0.5."""
        iters = [
            {"tasks_completed": 0, "tasks_added": 1},  # productive failure
            {"tasks_completed": 0, "tasks_added": 0},  # unproductive failure
            {"tasks_completed": 2, "tasks_added": 0},  # not a failure
        ]
        self.assertAlmostEqual(_compute_productive_failure_rate(iters), 0.5)


# ─── End-to-End: Deutschian Metrics with Spec-Based Evolution ────────────


class TestEndToEndDeutschianMetrics(TelemetryTestCase):
    def test_spec_based_evolution_ratio(self):
        """Verify evolution_ratio uses spec diffing when initial_task_ids present."""
        # Create a spec file with original + self-evolved tasks
        spec_content = (
            "# Spec\n\n"
            "### t-1: Original A\nDONE\n\n**Spec:** Do.\n**Verify:** ok\n"
            "### t-2: Original B\nDONE\n\n**Spec:** Do.\n**Verify:** ok\n"
            "### t-3: Self-evolved C\nDONE\n\n**Spec:** Do.\n**Verify:** ok\n"
        )
        spec_path = os.path.join(self.queue_dir, "q-test.spec.md")
        Path(spec_path).write_text(spec_content, encoding="utf-8")

        # Write queue entry with initial_task_ids and spec_path
        entry = {
            "id": "q-test",
            "spec_path": spec_path,
            "original_spec_path": spec_path,
            "worktree": None,
            "priority": 100,
            "status": "running",
            "submitted_at": "2026-03-06T10:00:00+00:00",
            "iteration": 3,
            "max_iterations": 30,
            "blocked_by": [],
            "last_worker": None,
            "last_iteration_at": None,
            "consecutive_failures": 0,
            "tasks_done": 3,
            "tasks_total": 3,
            "initial_task_ids": ["t-1", "t-2"],
        }
        entry_path = Path(self.queue_dir) / "q-test.json"
        entry_path.write_text(json.dumps(entry, indent=2) + "\n", encoding="utf-8")

        # Write iteration files
        self._write_iteration_file("q-test", 1, tasks_completed=1, tasks_added=0)
        self._write_iteration_file("q-test", 2, tasks_completed=1, tasks_added=1)
        self._write_iteration_file("q-test", 3, tasks_completed=1, tasks_added=0)

        result = update_telemetry(self.queue_dir, "q-test")

        # With spec-based: t-3 is self-evolved and DONE
        # total_done=3, self_evolved_done=1 -> ratio=1/3
        self.assertAlmostEqual(result["evolution_ratio"], 1.0 / 3.0, places=4)

        # first_pass_rate: no critic tasks -> 1.0
        self.assertAlmostEqual(result["first_pass_rate"], 1.0)

        # Verify first_pass_rate is persisted
        saved = read_telemetry(self.queue_dir, "q-test")
        self.assertAlmostEqual(saved["first_pass_rate"], 1.0)


if __name__ == "__main__":
    unittest.main()
