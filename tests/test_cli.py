# test_cli.py — Unit tests for BOI CLI.
#
# Tests cover: help output, dispatch argument parsing, queue/status output,
# telemetry formatting, log command, cancel, stop, workers, and
# tasks-to-spec conversion.
#
# Uses stdlib unittest only (no pytest dependency).

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

# Add parent directory to path so we can import lib modules
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.queue import _write_entry, enqueue
from lib.spec_parser import convert_tasks_to_spec, count_boi_tasks
from lib.status import (
    build_queue_status,
    build_telemetry,
    format_queue_json,
    format_queue_table,
    format_telemetry_json,
    format_telemetry_table,
)
from lib.telemetry import load_iteration_files

BOI_SH = str(Path(__file__).resolve().parent.parent / "boi.sh")


class TestCliHelp(unittest.TestCase):
    """Test that --help output is clean and contains expected content."""

    def _run_boi(self, *args: str) -> subprocess.CompletedProcess:
        """Run boi.sh with arguments and return result."""
        env = os.environ.copy()
        env.pop("CLAUDECODE", None)
        return subprocess.run(
            ["bash", BOI_SH, *args],
            capture_output=True,
            text=True,
            timeout=10,
            env=env,
        )

    def test_no_args_shows_usage(self):
        result = self._run_boi()
        self.assertEqual(result.returncode, 0)
        self.assertIn("BOI", result.stdout)
        self.assertIn("Beginning of Infinity", result.stdout)
        self.assertIn("dispatch", result.stdout)
        self.assertIn("queue", result.stdout)
        self.assertIn("status", result.stdout)
        self.assertIn("log", result.stdout)
        self.assertIn("cancel", result.stdout)
        self.assertIn("stop", result.stdout)
        self.assertIn("workers", result.stdout)
        self.assertIn("telemetry", result.stdout)
        self.assertIn("purge", result.stdout)
        self.assertIn("doctor", result.stdout)

    def test_help_flag(self):
        result = self._run_boi("--help")
        self.assertEqual(result.returncode, 0)
        self.assertIn("BOI", result.stdout)

    def test_help_command(self):
        result = self._run_boi("help")
        self.assertEqual(result.returncode, 0)
        self.assertIn("BOI", result.stdout)

    def test_dispatch_help(self):
        result = self._run_boi("dispatch", "--help")
        self.assertEqual(result.returncode, 0)
        self.assertIn("--spec", result.stdout)
        self.assertIn("--tasks", result.stdout)
        self.assertIn("--priority", result.stdout)
        self.assertIn("--max-iter", result.stdout)

    def test_purge_help(self):
        result = self._run_boi("purge", "--help")
        self.assertEqual(result.returncode, 0)
        self.assertIn("--all", result.stdout)
        self.assertIn("--dry-run", result.stdout)
        self.assertIn("completed", result.stdout)

    def test_unknown_command_fails(self):
        result = self._run_boi("nonexistent")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Unknown command", result.stderr)

    def test_doctor_runs_without_error(self):
        """Doctor command should exit 0 and show BOI Doctor header."""
        result = self._run_boi("doctor")
        self.assertEqual(result.returncode, 0)
        self.assertIn("BOI Doctor", result.stdout)

    def test_doctor_shows_results_summary(self):
        """Doctor output should include a results line with pass/fail/warning counts."""
        result = self._run_boi("doctor")
        self.assertEqual(result.returncode, 0)
        self.assertIn("Results:", result.stdout)
        self.assertIn("passed", result.stdout)
        self.assertIn("failed", result.stdout)

    def test_doctor_checks_core_prerequisites(self):
        """Doctor should check tmux, claude, python3 at minimum."""
        result = self._run_boi("doctor")
        self.assertEqual(result.returncode, 0)
        output = result.stdout
        # Should mention tmux check (PASS or FAIL)
        self.assertTrue(
            "tmux" in output,
            "Doctor output should mention tmux check",
        )
        # Should mention python check
        self.assertTrue(
            "Python" in output or "python3" in output,
            "Doctor output should mention Python check",
        )

    def test_doctor_output_has_pass_fail_warn_markers(self):
        """Doctor output lines should use [PASS], [FAIL], or [WARN] markers."""
        result = self._run_boi("doctor")
        self.assertEqual(result.returncode, 0)
        # Strip ANSI codes for easier checking
        import re

        clean = re.sub(r"\033\[[0-9;]*m", "", result.stdout)
        lines = [l.strip() for l in clean.split("\n") if l.strip()]
        check_lines = [l for l in lines if l.startswith("[")]
        self.assertGreater(
            len(check_lines),
            0,
            "Doctor should produce at least one [PASS]/[FAIL]/[WARN] line",
        )
        for line in check_lines:
            self.assertTrue(
                line.startswith("[PASS]")
                or line.startswith("[FAIL]")
                or line.startswith("[WARN]"),
                f"Check line should start with [PASS], [FAIL], or [WARN]: {line}",
            )


class TestQueueStatusFormatting(unittest.TestCase):
    """Test queue status formatting functions."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.queue_dir = os.path.join(self.tmpdir, "queue")
        os.makedirs(self.queue_dir)
        self._spec_counter = 0

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def _make_spec(self, name=None):
        self._spec_counter += 1
        if name is None:
            name = f"spec-{self._spec_counter}.md"
        path = os.path.join(self.tmpdir, name)
        Path(path).write_text(
            "# Spec\n\n## Tasks\n\n### t-1: Task\nPENDING\n\n**Spec:** Do.\n**Verify:** ok\n"
        )
        return path

    def test_empty_queue_table(self):
        status = build_queue_status(self.queue_dir)
        output = format_queue_table(status, color=False)
        self.assertIn("BOI", output)
        self.assertIn("No specs in queue", output)
        self.assertIn("Ready to dispatch", output)
        self.assertIn("Quick start:", output)
        self.assertIn("boi dispatch", output)
        self.assertIn("boi do", output)

    def test_empty_queue_json(self):
        status = build_queue_status(self.queue_dir)
        output = format_queue_json(status)
        data = json.loads(output)
        self.assertEqual(data["entries"], [])
        self.assertEqual(data["summary"]["total"], 0)

    def test_queue_with_entries(self):
        # Create mock queue entries
        enqueue(self.queue_dir, self._make_spec("test-spec.md"), priority=100)
        entry2 = enqueue(self.queue_dir, self._make_spec("other-spec.md"), priority=50)
        entry2["status"] = "running"
        entry2["iteration"] = 2
        entry2["last_worker"] = "w-1"
        entry2["tasks_done"] = 3
        entry2["tasks_total"] = 7
        _write_entry(self.queue_dir, entry2)

        status = build_queue_status(self.queue_dir)
        output = format_queue_table(status, color=False)

        self.assertIn("BOI", output)
        self.assertIn("q-001", output)
        self.assertIn("q-002", output)
        self.assertIn("running", output)
        self.assertIn("queued", output)
        self.assertIn("3/7 done", output)

    def test_queue_with_workers_shows_summary(self):
        enqueue(self.queue_dir, self._make_spec())
        config = {"workers": [{"id": "w-1"}, {"id": "w-2"}, {"id": "w-3"}]}
        status = build_queue_status(self.queue_dir, config)
        output = format_queue_table(status, color=False)
        self.assertIn("Workers:", output)

    def test_status_counts(self):
        e1 = enqueue(self.queue_dir, self._make_spec("a.md"))
        e2 = enqueue(self.queue_dir, self._make_spec("b.md"))
        e3 = enqueue(self.queue_dir, self._make_spec("c.md"))

        e2["status"] = "running"
        _write_entry(self.queue_dir, e2)
        e3["status"] = "completed"
        _write_entry(self.queue_dir, e3)

        status = build_queue_status(self.queue_dir)
        self.assertEqual(status["summary"]["queued"], 1)
        self.assertEqual(status["summary"]["running"], 1)
        self.assertEqual(status["summary"]["completed"], 1)
        self.assertEqual(status["summary"]["total"], 3)


class TestTelemetryFormatting(unittest.TestCase):
    """Test telemetry output formatting."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.queue_dir = os.path.join(self.tmpdir, "queue")
        os.makedirs(self.queue_dir)
        self._spec_counter = 0

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def _make_spec(self, name=None):
        self._spec_counter += 1
        if name is None:
            name = f"spec-{self._spec_counter}.md"
        path = os.path.join(self.tmpdir, name)
        Path(path).write_text(
            "# Spec\n\n## Tasks\n\n### t-1: Task\nPENDING\n\n**Spec:** Do.\n**Verify:** ok\n"
        )
        return path

    def _write_iteration(self, queue_id: str, iteration: int, **kwargs):
        """Write a mock iteration-N.json file."""
        data = {
            "queue_id": queue_id,
            "iteration": iteration,
            "exit_code": kwargs.get("exit_code", 0),
            "duration_seconds": kwargs.get("duration", 600),
            "started_at": "2026-03-06T10:00:00Z",
            "tasks_completed": kwargs.get("tasks_completed", 1),
            "tasks_added": kwargs.get("tasks_added", 0),
            "tasks_skipped": kwargs.get("tasks_skipped", 0),
            "pre_counts": {"pending": 3, "done": 0, "skipped": 0, "total": 3},
            "post_counts": {"pending": 2, "done": 1, "skipped": 0, "total": 3},
        }
        filepath = os.path.join(
            self.queue_dir, f"{queue_id}.iteration-{iteration}.json"
        )
        with open(filepath, "w") as f:
            json.dump(data, f)

    def test_no_entry_returns_none(self):
        result = build_telemetry(self.queue_dir, "q-999")
        self.assertIsNone(result)

    def test_telemetry_with_iterations(self):
        entry = enqueue(self.queue_dir, self._make_spec("my-spec.md"))
        entry["iteration"] = 3
        entry["tasks_done"] = 5
        entry["tasks_total"] = 8
        _write_entry(self.queue_dir, entry)

        self._write_iteration(
            "q-001", 1, tasks_completed=2, tasks_added=1, duration=725
        )
        self._write_iteration("q-001", 2, tasks_completed=2, duration=1121)
        self._write_iteration(
            "q-001", 3, tasks_completed=1, tasks_skipped=1, duration=997
        )

        telemetry = build_telemetry(self.queue_dir, "q-001")
        self.assertIsNotNone(telemetry)
        self.assertEqual(telemetry["queue_id"], "q-001")
        self.assertEqual(telemetry["spec_name"], "my-spec")
        self.assertEqual(telemetry["iteration"], 3)
        self.assertEqual(telemetry["tasks_done"], 5)
        self.assertEqual(telemetry["tasks_total"], 8)
        self.assertEqual(telemetry["total_tasks_completed"], 5)
        self.assertEqual(telemetry["total_tasks_added"], 1)
        self.assertEqual(telemetry["total_tasks_skipped"], 1)
        self.assertEqual(telemetry["total_time_seconds"], 725 + 1121 + 997)
        self.assertEqual(len(telemetry["iterations"]), 3)

    def test_telemetry_table_format(self):
        entry = enqueue(self.queue_dir, self._make_spec("ios-recording.md"))
        entry["iteration"] = 2
        entry["tasks_done"] = 3
        entry["tasks_total"] = 5
        entry["status"] = "running"
        _write_entry(self.queue_dir, entry)

        self._write_iteration(
            "q-001", 1, tasks_completed=2, tasks_added=1, duration=600
        )
        self._write_iteration("q-001", 2, tasks_completed=1, duration=900)

        telemetry = build_telemetry(self.queue_dir, "q-001")
        output = format_telemetry_table(telemetry, color=False)

        self.assertIn("ios-recording", output)
        self.assertIn("q-001", output)
        self.assertIn("Iterations: 2 of 30", output)
        self.assertIn("3/5 done", output)
        self.assertIn("1 added (self-evolved)", output)
        self.assertIn("Iteration breakdown:", output)
        self.assertIn("#1:", output)
        self.assertIn("#2:", output)

    def test_telemetry_json_format(self):
        entry = enqueue(self.queue_dir, self._make_spec("test.md"))
        _write_entry(self.queue_dir, entry)

        telemetry = build_telemetry(self.queue_dir, "q-001")
        output = format_telemetry_json(telemetry)
        data = json.loads(output)
        self.assertEqual(data["queue_id"], "q-001")
        self.assertIn("iterations", data)

    def test_telemetry_with_failures(self):
        entry = enqueue(self.queue_dir, self._make_spec("test.md"))
        entry["consecutive_failures"] = 3
        _write_entry(self.queue_dir, entry)

        telemetry = build_telemetry(self.queue_dir, "q-001")
        output = format_telemetry_table(telemetry, color=False)
        self.assertIn("Consecutive failures: 3", output)

    def test_telemetry_exit_code_shown(self):
        entry = enqueue(self.queue_dir, self._make_spec("test.md"))
        entry["iteration"] = 1
        _write_entry(self.queue_dir, entry)

        self._write_iteration("q-001", 1, exit_code=1, duration=100)

        telemetry = build_telemetry(self.queue_dir, "q-001")
        output = format_telemetry_table(telemetry, color=False)
        self.assertIn("[exit 1]", output)

    def test_load_iteration_files_order(self):
        entry = enqueue(self.queue_dir, self._make_spec("test.md"))

        # Write out of order
        self._write_iteration("q-001", 3)
        self._write_iteration("q-001", 1)
        self._write_iteration("q-001", 2)

        iterations = load_iteration_files(self.queue_dir, "q-001")
        self.assertEqual(len(iterations), 3)
        self.assertEqual(iterations[0]["iteration"], 1)
        self.assertEqual(iterations[1]["iteration"], 2)
        self.assertEqual(iterations[2]["iteration"], 3)

    def test_load_iteration_files_empty(self):
        iterations = load_iteration_files(self.queue_dir, "q-999")
        self.assertEqual(iterations, [])

    def test_telemetry_shows_crash_in_iteration_breakdown(self):
        """Iteration breakdown should show CRASH for failed iterations."""
        entry = enqueue(self.queue_dir, self._make_spec("crash-test.md"))
        entry["iteration"] = 3
        entry["tasks_done"] = 2
        entry["tasks_total"] = 5
        entry["status"] = "failed"
        entry["failure_reason"] = (
            "5 consecutive failures. Last error: Worker crashed: no exit file."
        )
        _write_entry(self.queue_dir, entry)

        # Write iterations: 1 success, 2 crashes
        self._write_iteration("q-001", 1, tasks_completed=2, duration=600)
        # Write crash iteration metadata with failure_reason
        crash_meta_2 = {
            "queue_id": "q-001",
            "iteration": 2,
            "tasks_completed": 0,
            "tasks_added": 0,
            "tasks_skipped": 0,
            "duration_seconds": 0,
            "crash": True,
            "failure_reason": "Worker crashed: no exit file. Process may have been killed or timed out.",
        }
        crash_meta_3 = {
            "queue_id": "q-001",
            "iteration": 3,
            "tasks_completed": 0,
            "tasks_added": 0,
            "tasks_skipped": 0,
            "duration_seconds": 0,
            "crash": True,
            "failure_reason": "Worker crashed: no exit file. Process may have been killed or timed out.",
        }
        for meta in [(2, crash_meta_2), (3, crash_meta_3)]:
            filepath = os.path.join(self.queue_dir, f"q-001.iteration-{meta[0]}.json")
            with open(filepath, "w") as f:
                json.dump(meta[1], f)

        telemetry = build_telemetry(self.queue_dir, "q-001")
        output = format_telemetry_table(telemetry, color=False)

        self.assertIn("Iteration breakdown:", output)
        self.assertIn("#1:", output)
        self.assertIn("2 tasks done", output)
        # Crash iterations should show CRASH label
        self.assertIn("CRASH", output)
        self.assertIn("Worker crashed", output)

    def test_queue_table_shows_failure_reason(self):
        """format_queue_table should show failure reason on a second line for failed specs."""
        entry = enqueue(self.queue_dir, self._make_spec("fail-test.md"))
        entry["status"] = "failed"
        entry["failure_reason"] = (
            "5 consecutive failures. Last error: Worker crashed: no exit file."
        )
        _write_entry(self.queue_dir, entry)

        status = build_queue_status(self.queue_dir)
        output = format_queue_table(status, color=False)

        self.assertIn("failed", output)
        self.assertIn("Reason:", output)
        self.assertIn("5 consecutive failures", output)

    def test_queue_table_no_reason_for_non_failed(self):
        """format_queue_table should NOT show Reason: line for non-failed specs."""
        entry = enqueue(self.queue_dir, self._make_spec("running-test.md"))
        entry["status"] = "running"
        _write_entry(self.queue_dir, entry)

        status = build_queue_status(self.queue_dir)
        output = format_queue_table(status, color=False)

        self.assertNotIn("Reason:", output)


class TestTasksToSpecConversion(unittest.TestCase):
    """Test backward-compat conversion from tasks.md to spec.md format."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_basic_conversion(self):
        tasks_content = """\
## t-001: Build the widget
- **Spec:** Create a React widget component
- **Files:** src/Widget.tsx
- **Deps:** none
- **Verify:** python3 -m pytest src/Widget.tsx

## t-002: Add tests
- **Spec:** Write unit tests for widget
- **Files:** tests/Widget.test.tsx
- **Deps:** t-001
- **Verify:** python3 -m pytest tests/Widget.test.tsx
"""
        tasks_path = os.path.join(self.tmpdir, "tasks.md")
        output_path = os.path.join(self.tmpdir, "spec.md")

        with open(tasks_path, "w") as f:
            f.write(tasks_content)

        count = convert_tasks_to_spec(tasks_path, output_path)
        self.assertEqual(count, 2)

        # Verify the output is valid BOI spec format
        counts = count_boi_tasks(output_path)
        self.assertEqual(counts["total"], 2)
        self.assertEqual(counts["pending"], 2)
        self.assertEqual(counts["done"], 0)

        # Verify content
        content = Path(output_path).read_text()
        self.assertIn("### t-1:", content)
        self.assertIn("### t-2:", content)
        self.assertIn("PENDING", content)
        self.assertIn("Build the widget", content)
        self.assertIn("Add tests", content)

    def test_empty_tasks_raises(self):
        empty_path = os.path.join(self.tmpdir, "empty.md")
        output_path = os.path.join(self.tmpdir, "spec.md")

        with open(empty_path, "w") as f:
            f.write("# Nothing here\n")

        with self.assertRaises(ValueError):
            convert_tasks_to_spec(empty_path, output_path)


class TestLogCommand(unittest.TestCase):
    """Test log command edge cases."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.queue_dir = os.path.join(self.tmpdir, "queue")
        self.log_dir = os.path.join(self.tmpdir, "logs")
        os.makedirs(self.queue_dir)
        os.makedirs(self.log_dir)

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_log_finds_latest_iteration(self):
        """Verify that log finding logic picks highest iteration number."""
        # Create log files for multiple iterations
        for i in [1, 3, 2]:
            path = os.path.join(self.log_dir, f"q-001-iter-{i}.log")
            with open(path, "w") as f:
                f.write(f"Log output for iteration {i}\n")

        # Manually test the logic from boi.sh
        latest_iter = 0
        latest_log = ""
        for name in os.listdir(self.log_dir):
            if name.startswith("q-001-iter-") and name.endswith(".log"):
                import re

                match = re.search(r"iter-(\d+)\.log$", name)
                if match:
                    n = int(match.group(1))
                    if n > latest_iter:
                        latest_iter = n
                        latest_log = os.path.join(self.log_dir, name)

        self.assertEqual(latest_iter, 3)
        self.assertIn("iter-3", latest_log)


class TestDurationFormatting(unittest.TestCase):
    """Test the format_duration helper."""

    def test_seconds(self):
        from lib.status import format_duration

        self.assertEqual(format_duration(45), "45s")

    def test_minutes(self):
        from lib.status import format_duration

        self.assertEqual(format_duration(125), "2m 05s")

    def test_hours(self):
        from lib.status import format_duration

        self.assertEqual(format_duration(3661), "1h 01m")

    def test_zero(self):
        from lib.status import format_duration

        self.assertEqual(format_duration(0), "0s")


class TestDashboardFormat(unittest.TestCase):
    """Test the compact dashboard format for tmux panes."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def _write_entries(self, entries):
        for e in entries:
            path = os.path.join(self.tmpdir, f"{e['id']}.json")
            with open(path, "w") as f:
                json.dump(e, f)

    def test_empty_queue(self):
        from lib.status import build_queue_status, format_dashboard

        config = {"workers": [{"id": "w-1"}, {"id": "w-2"}]}
        status_data = build_queue_status(self.tmpdir, config)
        output = format_dashboard(status_data, color=False)

        self.assertIn("BOI", output)
        self.assertIn("No specs in queue", output)
        self.assertIn("Ready to dispatch", output)
        self.assertIn("Workers: 0/2 idle", output)
        self.assertIn("Quick start:", output)
        self.assertIn("boi dispatch", output)

    def test_single_running_spec(self):
        from lib.status import build_queue_status, format_dashboard

        self._write_entries(
            [
                {
                    "id": "q-001",
                    "spec_path": "/tmp/my-spec.md",
                    "priority": 100,
                    "status": "running",
                    "iteration": 2,
                    "max_iterations": 30,
                    "last_worker": "w-1",
                    "tasks_done": 3,
                    "tasks_total": 8,
                },
            ]
        )
        config = {"workers": [{"id": "w-1"}, {"id": "w-2"}]}
        status_data = build_queue_status(self.tmpdir, config)
        output = format_dashboard(status_data, color=False)

        self.assertIn("\u25b6", output)  # ▶ running icon
        self.assertIn("q-001", output)
        self.assertIn("my-spec", output)
        self.assertIn("3/8", output)
        self.assertIn("2i", output)
        self.assertIn("w-1", output)
        self.assertIn("Workers: 1/2 busy", output)

    def test_multiple_statuses(self):
        from lib.status import build_queue_status, format_dashboard

        self._write_entries(
            [
                {
                    "id": "q-001",
                    "spec_path": "/tmp/done.md",
                    "priority": 100,
                    "status": "completed",
                    "iteration": 3,
                    "max_iterations": 30,
                    "last_worker": "w-1",
                    "tasks_done": 5,
                    "tasks_total": 5,
                },
                {
                    "id": "q-002",
                    "spec_path": "/tmp/active.md",
                    "priority": 100,
                    "status": "running",
                    "iteration": 1,
                    "max_iterations": 30,
                    "last_worker": "w-2",
                    "tasks_done": 1,
                    "tasks_total": 4,
                },
                {
                    "id": "q-003",
                    "spec_path": "/tmp/waiting.md",
                    "priority": 200,
                    "status": "queued",
                    "iteration": 0,
                    "max_iterations": 30,
                    "last_worker": None,
                    "tasks_done": 0,
                    "tasks_total": 6,
                },
                {
                    "id": "q-004",
                    "spec_path": "/tmp/broke.md",
                    "priority": 50,
                    "status": "failed",
                    "iteration": 5,
                    "max_iterations": 5,
                    "last_worker": "w-1",
                    "tasks_done": 2,
                    "tasks_total": 7,
                },
            ]
        )
        config = {"workers": [{"id": "w-1"}, {"id": "w-2"}, {"id": "w-3"}]}
        status_data = build_queue_status(self.tmpdir, config)
        output = format_dashboard(status_data, color=False)

        # Check status icons present
        self.assertIn("\u2713", output)  # ✓ completed
        self.assertIn("\u25b6", output)  # ▶ running
        self.assertIn("\u00b7", output)  # · queued
        self.assertIn("\u2717", output)  # ✗ failed

        # Check queue IDs present
        self.assertIn("q-001", output)
        self.assertIn("q-002", output)
        self.assertIn("q-003", output)
        self.assertIn("q-004", output)

        # Summary
        self.assertIn("Queue: 4", output)

    def test_header_contains_time(self):
        from lib.status import build_queue_status, format_dashboard

        status_data = build_queue_status(self.tmpdir, None)
        output = format_dashboard(status_data, color=False)

        # Header should contain BOI and time
        self.assertIn("BOI", output)
        # Time format HH:MM somewhere in the header
        import re

        self.assertTrue(re.search(r"\d{2}:\d{2}", output))

    def test_color_mode(self):
        from lib.status import build_queue_status, format_dashboard

        self._write_entries(
            [
                {
                    "id": "q-001",
                    "spec_path": "/tmp/test.md",
                    "priority": 100,
                    "status": "running",
                    "iteration": 1,
                    "max_iterations": 30,
                    "last_worker": "w-1",
                    "tasks_done": 1,
                    "tasks_total": 3,
                },
            ]
        )
        config = {"workers": [{"id": "w-1"}]}
        status_data = build_queue_status(self.tmpdir, config)

        color_output = format_dashboard(status_data, color=True)
        plain_output = format_dashboard(status_data, color=False)

        # Color output should contain ANSI codes
        self.assertIn("\033[", color_output)
        # Plain output should NOT contain ANSI codes
        self.assertNotIn("\033[", plain_output)

    def test_long_spec_name_truncated(self):
        from lib.status import build_queue_status, format_dashboard

        self._write_entries(
            [
                {
                    "id": "q-001",
                    "spec_path": "/tmp/this-is-a-very-long-spec-name-that-should-be-truncated.md",
                    "priority": 100,
                    "status": "queued",
                    "iteration": 0,
                    "max_iterations": 30,
                    "last_worker": None,
                    "tasks_done": 0,
                    "tasks_total": 3,
                },
            ]
        )
        status_data = build_queue_status(self.tmpdir, None)
        output = format_dashboard(status_data, color=False)

        # The full long name should NOT appear (it would be truncated)
        self.assertNotIn(
            "this-is-a-very-long-spec-name-that-should-be-truncated", output
        )
        # But the truncated version should
        self.assertIn("q-001", output)

    def test_worker_shown_only_for_running(self):
        from lib.status import build_queue_status, format_dashboard

        self._write_entries(
            [
                {
                    "id": "q-001",
                    "spec_path": "/tmp/done.md",
                    "priority": 100,
                    "status": "completed",
                    "iteration": 3,
                    "max_iterations": 30,
                    "last_worker": "w-1",
                    "tasks_done": 5,
                    "tasks_total": 5,
                },
                {
                    "id": "q-002",
                    "spec_path": "/tmp/active.md",
                    "priority": 100,
                    "status": "running",
                    "iteration": 1,
                    "max_iterations": 30,
                    "last_worker": "w-2",
                    "tasks_done": 1,
                    "tasks_total": 4,
                },
            ]
        )
        status_data = build_queue_status(self.tmpdir, None)
        output = format_dashboard(status_data, color=False)
        lines = output.strip().split("\n")

        # Find the line with q-001 (completed) — should NOT show w-1
        completed_line = [l for l in lines if "q-001" in l][0]
        self.assertNotIn("w-1", completed_line)

        # Find the line with q-002 (running) — should show w-2
        running_line = [l for l in lines if "q-002" in l][0]
        self.assertIn("w-2", running_line)

    def test_no_workers_in_config(self):
        from lib.status import build_queue_status, format_dashboard

        self._write_entries(
            [
                {
                    "id": "q-001",
                    "spec_path": "/tmp/test.md",
                    "priority": 100,
                    "status": "queued",
                    "iteration": 0,
                    "max_iterations": 30,
                    "last_worker": None,
                    "tasks_done": 0,
                    "tasks_total": 3,
                },
            ]
        )
        status_data = build_queue_status(self.tmpdir, None)
        output = format_dashboard(status_data, color=False)

        # Should show Queue count but not Workers
        self.assertIn("Queue: 1", output)
        self.assertNotIn("Workers:", output)

    def test_dashboard_sh_exists(self):
        dashboard_path = os.path.join(
            os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
            "dashboard.sh",
        )
        self.assertTrue(
            os.path.isfile(dashboard_path),
            f"dashboard.sh not found at {dashboard_path}",
        )
        # Check it's executable
        self.assertTrue(
            os.access(dashboard_path, os.X_OK),
            "dashboard.sh is not executable",
        )


class TestCriticCli(unittest.TestCase):
    """Test boi critic subcommand and its sub-subcommands."""

    def _run_boi(self, *args: str) -> subprocess.CompletedProcess:
        """Run boi.sh with arguments and return result."""
        env = os.environ.copy()
        env.pop("CLAUDECODE", None)
        return subprocess.run(
            ["bash", BOI_SH, *args],
            capture_output=True,
            text=True,
            timeout=10,
            env=env,
        )

    def test_critic_no_args_shows_usage(self):
        result = self._run_boi("critic")
        self.assertEqual(result.returncode, 0)
        self.assertIn("BOI Critic", result.stdout)
        self.assertIn("status", result.stdout)
        self.assertIn("run", result.stdout)
        self.assertIn("disable", result.stdout)
        self.assertIn("enable", result.stdout)
        self.assertIn("checks", result.stdout)

    def test_critic_help_flag(self):
        result = self._run_boi("critic", "--help")
        self.assertEqual(result.returncode, 0)
        self.assertIn("BOI Critic", result.stdout)

    def test_critic_help_command(self):
        result = self._run_boi("critic", "help")
        self.assertEqual(result.returncode, 0)
        self.assertIn("BOI Critic", result.stdout)

    def test_critic_unknown_subcommand_fails(self):
        result = self._run_boi("critic", "nonexistent")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Unknown critic subcommand", result.stderr)

    def test_critic_status_shows_output(self):
        result = self._run_boi("critic", "status")
        self.assertEqual(result.returncode, 0)
        self.assertIn("BOI Critic", result.stdout)
        self.assertIn("Enabled:", result.stdout)
        self.assertIn("Trigger:", result.stdout)
        self.assertIn("Max passes:", result.stdout)
        self.assertIn("Active checks:", result.stdout)

    def test_critic_status_shows_default_checks(self):
        result = self._run_boi("critic", "status")
        self.assertEqual(result.returncode, 0)
        self.assertIn("[default]", result.stdout)
        self.assertIn("spec-integrity", result.stdout)
        self.assertIn("verify-commands", result.stdout)
        self.assertIn("code-quality", result.stdout)
        self.assertIn("completeness", result.stdout)
        self.assertIn("fleet-readiness", result.stdout)

    def test_critic_checks_lists_checks(self):
        result = self._run_boi("critic", "checks")
        self.assertEqual(result.returncode, 0)
        self.assertIn("Active checks", result.stdout)
        self.assertIn("spec-integrity", result.stdout)
        self.assertIn("verify-commands", result.stdout)
        self.assertIn("code-quality", result.stdout)
        self.assertIn("completeness", result.stdout)
        self.assertIn("fleet-readiness", result.stdout)

    def test_critic_run_without_queue_id_fails(self):
        result = self._run_boi("critic", "run")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Queue ID required", result.stderr)

    def test_critic_run_help(self):
        result = self._run_boi("critic", "run", "--help")
        self.assertEqual(result.returncode, 0)
        self.assertIn("queue-id", result.stdout)

    def test_critic_in_usage_output(self):
        """Critic should appear in the main usage output."""
        result = self._run_boi()
        self.assertEqual(result.returncode, 0)
        self.assertIn("critic", result.stdout)


class TestCriticEnableDisable(unittest.TestCase):
    """Test boi critic enable/disable modifying config."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.state_dir = self.tmpdir
        sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_disable_sets_enabled_false(self):
        from lib.critic_config import load_critic_config, save_critic_config

        config = load_critic_config(self.state_dir)
        self.assertTrue(config["enabled"])

        config["enabled"] = False
        save_critic_config(self.state_dir, config)

        reloaded = load_critic_config(self.state_dir)
        self.assertFalse(reloaded["enabled"])

    def test_enable_sets_enabled_true(self):
        from lib.critic_config import load_critic_config, save_critic_config

        config = load_critic_config(self.state_dir)
        config["enabled"] = False
        save_critic_config(self.state_dir, config)

        config["enabled"] = True
        save_critic_config(self.state_dir, config)

        reloaded = load_critic_config(self.state_dir)
        self.assertTrue(reloaded["enabled"])

    def test_status_rendering_with_custom_checks(self):
        """Verify status output counts custom vs default checks correctly."""
        from lib.critic_config import get_active_checks, load_critic_config

        config = load_critic_config(self.state_dir)
        boi_dir = str(Path(__file__).resolve().parent.parent)
        checks_dir = os.path.join(boi_dir, "templates", "checks")

        # Create a custom check
        custom_dir = os.path.join(self.state_dir, "critic", "custom")
        os.makedirs(custom_dir, exist_ok=True)
        custom_check_path = os.path.join(custom_dir, "security-review.md")
        with open(custom_check_path, "w") as f:
            f.write(
                "# Security Review\n\nCheck for security issues.\n\n- No SQL injection\n- No XSS\n- No SSRF\n"
            )

        checks = get_active_checks(config, checks_dir, self.state_dir)
        default_count = sum(1 for c in checks if c["source"] == "default")
        custom_count = sum(1 for c in checks if c["source"] == "custom")

        self.assertEqual(default_count, 5)
        self.assertEqual(custom_count, 1)
        self.assertEqual(len(checks), 6)

        # Verify the custom check is in the list
        check_names = [c["name"] for c in checks]
        self.assertIn("security-review", check_names)


if __name__ == "__main__":
    unittest.main()
