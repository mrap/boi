# test_daemon_ops.py — Unit tests for BOI daemon batched operations.
#
# Tests the daemon_ops.py module that consolidates multiple per-cycle
# Python operations into single function calls.
#
# Uses stdlib unittest only (no pytest dependency).

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

# Add parent directory to path so we can import lib modules
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.daemon_ops import CompletionContext, get_active_count, pick_next_spec, process_worker_completion
from lib.db import Database


class DaemonOpsTestCase(unittest.TestCase):
    """Base test case with temp dirs mimicking ~/.boi/."""

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.boi_state = self._tmpdir.name
        self.queue_dir = os.path.join(self.boi_state, "queue")
        self.events_dir = os.path.join(self.boi_state, "events")
        self.log_dir = os.path.join(self.boi_state, "logs")
        self.hooks_dir = os.path.join(self.boi_state, "hooks")
        self.script_dir = str(Path(__file__).resolve().parent.parent)
        os.makedirs(self.queue_dir)
        os.makedirs(self.events_dir)
        os.makedirs(self.log_dir)
        os.makedirs(self.hooks_dir)

        # Disable critic by default so existing tests aren't affected
        critic_dir = os.path.join(self.boi_state, "critic")
        os.makedirs(critic_dir, exist_ok=True)
        os.makedirs(os.path.join(critic_dir, "custom"), exist_ok=True)
        config_path = os.path.join(critic_dir, "config.json")
        Path(config_path).write_text(json.dumps({"enabled": False}, indent=2) + "\n")

        # Create SQLite database
        db_path = os.path.join(self.boi_state, "boi.db")
        self.db = Database(db_path, self.queue_dir)
        self.ctx = CompletionContext(
            queue_dir=self.queue_dir,
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            log_dir=self.log_dir,
            script_dir=self.script_dir,
            db=self.db,
        )

    def tearDown(self):
        self.db.close()
        self._tmpdir.cleanup()

    def _create_spec(self, tasks_pending=3, tasks_done=0, tasks_skipped=0):
        """Create a temp spec.md file with the given task counts."""
        if not hasattr(self, "_spec_counter"):
            self._spec_counter = 0
        self._spec_counter += 1
        spec_path = os.path.join(self._tmpdir.name, f"spec-{self._spec_counter}.md")
        lines = ["# Test Spec\n\n**Workspace:** in-place\n\n## Tasks\n"]
        tid = 1
        for _ in range(tasks_done):
            lines.append(
                f"\n### t-{tid}: Done task {tid}\nDONE\n\n"
                "**Spec:** Did it.\n**Verify:** true\n"
            )
            tid += 1
        for _ in range(tasks_pending):
            lines.append(
                f"\n### t-{tid}: Pending task {tid}\nPENDING\n\n"
                "**Spec:** Do it.\n**Verify:** true\n"
            )
            tid += 1
        for _ in range(tasks_skipped):
            lines.append(
                f"\n### t-{tid}: Skipped task {tid}\nSKIPPED\n\n"
                "**Spec:** Skip.\n**Verify:** true\n"
            )
            tid += 1
        Path(spec_path).write_text("".join(lines))
        return spec_path


class TestProcessWorkerCompletion(DaemonOpsTestCase):
    """Tests for process_worker_completion()."""

    def test_all_tasks_done_marks_completed(self):
        """When all tasks are DONE, outcome is 'completed'."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=3)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "completed")
        self.assertEqual(result["pending_count"], 0)
        self.assertEqual(result["done_count"], 3)

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "completed")

    def test_pending_tasks_requeues(self):
        """When pending tasks remain, outcome is 'requeued'."""
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "requeued")
        self.assertEqual(result["pending_count"], 2)

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "requeued")

    def test_max_iterations_fails(self):
        """When max iterations reached with pending tasks, outcome is 'failed'."""
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = self.db.enqueue(spec_path, max_iterations=1)
        self.db.set_running(entry["id"], "w-1")

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "failed")
        self.assertEqual(result["reason"], "max_iterations")

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "failed")

    def test_no_exit_code_crashes(self):
        """When exit_code is None (no exit file), outcome is 'crashed'."""
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code=None,
        )

        self.assertEqual(result["outcome"], "crashed")

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "requeued")
        self.assertEqual(updated["consecutive_failures"], 1)

    def test_nonzero_exit_code_crashes(self):
        """When exit_code is non-zero, outcome is 'crashed'."""
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="1",
        )

        self.assertEqual(result["outcome"], "crashed")

    def test_consecutive_failures_exceed_threshold(self):
        """When too many consecutive failures, outcome is 'failed'."""
        spec_path = self._create_spec(tasks_pending=2)
        entry = self.db.enqueue(spec_path)

        # Simulate 4 prior failures
        self.db.update_spec_fields(entry["id"], consecutive_failures=4)

        self.db.set_running(entry["id"], "w-1")

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code=None,
        )

        self.assertEqual(result["outcome"], "failed")
        self.assertEqual(result["reason"], "consecutive_failures")

    def test_writes_events(self):
        """process_worker_completion writes events to events_dir."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=2)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        event_files = list(Path(self.events_dir).glob("event-*.json"))
        self.assertGreater(len(event_files), 0)

        event = json.loads(event_files[0].read_text())
        self.assertEqual(event["type"], "spec_completed")
        self.assertEqual(event["queue_id"], entry["id"])

    def test_writes_telemetry(self):
        """process_worker_completion writes telemetry file."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=1)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        telemetry_file = Path(self.queue_dir) / f"{entry['id']}.telemetry.json"
        self.assertTrue(telemetry_file.exists())

    def test_missing_queue_entry_returns_error(self):
        """When queue entry doesn't exist, returns error outcome."""
        result = process_worker_completion(
            ctx=self.ctx,
            queue_id="q-999",
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "error")

    def test_runs_hooks_on_completion(self):
        """Hooks are called on completion."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=1)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        # Create a hook that writes a marker file
        marker = os.path.join(self._tmpdir.name, "hook_ran")
        hook_path = os.path.join(self.hooks_dir, "on-complete.sh")
        Path(hook_path).write_text(f"#!/bin/bash\ntouch {marker}\n")
        os.chmod(hook_path, 0o755)

        process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        self.assertTrue(os.path.exists(marker))

    def test_runs_fail_hook_on_failure(self):
        """on-fail hook runs when spec fails."""
        spec_path = self._create_spec(tasks_pending=2)
        entry = self.db.enqueue(spec_path, max_iterations=1)
        self.db.set_running(entry["id"], "w-1")

        marker = os.path.join(self._tmpdir.name, "fail_hook_ran")
        hook_path = os.path.join(self.hooks_dir, "on-fail.sh")
        Path(hook_path).write_text(f"#!/bin/bash\ntouch {marker}\n")
        os.chmod(hook_path, 0o755)

        process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        self.assertTrue(os.path.exists(marker))


class TestFailureDiagnostics(DaemonOpsTestCase):
    """Tests for failure reason and log_tail capture in iteration metadata."""

    def _write_log_file(self, queue_id, iteration, lines):
        """Write a mock log file for a worker iteration."""
        log_file = os.path.join(self.log_dir, f"{queue_id}-iter-{iteration}.log")
        Path(log_file).write_text("\n".join(lines) + "\n")
        return log_file

    def test_crash_captures_failure_reason_in_iteration_meta(self):
        """When worker crashes (no exit file), failure_reason is written to iteration JSON."""
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        # Write a mock log file
        self._write_log_file(
            entry["id"], 1, ["line 1", "line 2", "error: something broke"]
        )

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code=None,
        )

        self.assertEqual(result["outcome"], "crashed")
        self.assertIn("failure_reason", result)
        self.assertIn("no exit file", result["failure_reason"])

        # Check iteration metadata file
        iter_file = os.path.join(self.queue_dir, f"{entry['id']}.iteration-1.json")
        self.assertTrue(os.path.isfile(iter_file))
        meta = json.loads(Path(iter_file).read_text())
        self.assertIn("failure_reason", meta)
        self.assertIn("no exit file", meta["failure_reason"])
        self.assertIn("log_tail", meta)
        self.assertIsInstance(meta["log_tail"], list)
        self.assertIn("error: something broke", meta["log_tail"])
        self.assertTrue(meta.get("crash", False))

    def test_signal_exit_requeues_without_crash(self):
        """Exit code 137 (SIGKILL) is treated as signal death, not a crash."""
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        self._write_log_file(entry["id"], 1, ["running...", "segfault"])

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="137",
        )

        # Exit 137 (SIGKILL) is a signal death, not a real crash.
        # Signal path requeues without logging an iteration file.
        self.assertEqual(result["outcome"], "signal_requeued")

    def test_timeout_captures_failure_reason(self):
        """Timeout passes specific timeout message as failure_reason."""
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        self._write_log_file(entry["id"], 1, ["still running..."])

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code=None,
            timeout=True,
        )

        self.assertEqual(result["outcome"], "crashed")
        self.assertIn("timed out", result["failure_reason"])

        iter_file = os.path.join(self.queue_dir, f"{entry['id']}.iteration-1.json")
        meta = json.loads(Path(iter_file).read_text())
        self.assertIn("timed out", meta["failure_reason"])

    def test_consecutive_failures_include_last_error(self):
        """When consecutive failure limit hit, failure_reason includes last error."""
        spec_path = self._create_spec(tasks_pending=2)
        entry = self.db.enqueue(spec_path)

        # Simulate 4 prior failures
        self.db.update_spec_fields(entry["id"], consecutive_failures=4)

        self.db.set_running(entry["id"], "w-1")

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code=None,
        )

        self.assertEqual(result["outcome"], "failed")
        self.assertIn("5 consecutive failures", result["failure_reason"])
        self.assertIn("no exit file", result["failure_reason"])

        # Check queue entry has the failure_reason
        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "failed")
        self.assertIn("failure_reason", updated)
        self.assertIn("5 consecutive failures", updated["failure_reason"])

    def test_log_tail_captures_last_20_lines(self):
        """log_tail captures the last 20 lines from the worker log."""
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        # Write a log with 30 lines
        lines = [f"log line {i}" for i in range(30)]
        self._write_log_file(entry["id"], 1, lines)

        process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code=None,
        )

        iter_file = os.path.join(self.queue_dir, f"{entry['id']}.iteration-1.json")
        meta = json.loads(Path(iter_file).read_text())
        self.assertEqual(len(meta["log_tail"]), 20)
        self.assertEqual(meta["log_tail"][0], "log line 10")
        self.assertEqual(meta["log_tail"][-1], "log line 29")

    def test_no_log_file_empty_log_tail(self):
        """When no log file exists, log_tail is empty list."""
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code=None,
        )

        iter_file = os.path.join(self.queue_dir, f"{entry['id']}.iteration-1.json")
        meta = json.loads(Path(iter_file).read_text())
        self.assertEqual(meta["log_tail"], [])

    def test_existing_iteration_file_updated_with_diagnostics(self):
        """If iteration file already exists (from worker), diagnostics are merged in."""
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        # Pre-create iteration file (as the worker would)
        iter_file = os.path.join(self.queue_dir, f"{entry['id']}.iteration-1.json")
        existing_meta = {
            "queue_id": entry["id"],
            "iteration": 1,
            "exit_code": 1,
            "duration_seconds": 120,
            "tasks_completed": 0,
        }
        Path(iter_file).write_text(json.dumps(existing_meta, indent=2) + "\n")

        self._write_log_file(entry["id"], 1, ["error line"])

        process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="1",
        )

        meta = json.loads(Path(iter_file).read_text())
        # Original fields preserved
        self.assertEqual(meta["duration_seconds"], 120)
        self.assertEqual(meta["tasks_completed"], 0)
        # Diagnostics added
        self.assertIn("failure_reason", meta)
        self.assertEqual(meta["failure_reason"], "Worker exited with code 1.")
        self.assertIn("log_tail", meta)
        self.assertIn("error line", meta["log_tail"])


class TestPickNextSpec(DaemonOpsTestCase):
    """Tests for pick_next_spec()."""

    def test_returns_none_when_empty(self):
        """Returns None when queue is empty."""
        result = pick_next_spec(self.queue_dir, db=self.db)
        self.assertIsNone(result)

    def test_returns_queued_spec(self):
        """Returns the next queued spec."""
        spec_path = self._create_spec()
        entry = self.db.enqueue(spec_path)

        result = pick_next_spec(self.queue_dir, db=self.db)
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], entry["id"])
        self.assertEqual(result["spec_path"], entry["spec_path"])

    def test_returns_highest_priority(self):
        """Returns the highest-priority spec (lowest priority number)."""
        spec1 = self._create_spec()
        spec2 = os.path.join(self._tmpdir.name, "spec2.md")
        Path(spec2).write_text(Path(spec1).read_text())

        self.db.enqueue(spec1, priority=200)
        entry2 = self.db.enqueue(spec2, priority=50)

        result = pick_next_spec(self.queue_dir, db=self.db)
        self.assertEqual(result["id"], entry2["id"])

    def test_skips_running_specs(self):
        """Does not return specs that are already running."""
        spec_path = self._create_spec()
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        result = pick_next_spec(self.queue_dir, db=self.db)
        self.assertIsNone(result)

    def test_returns_requeued_spec(self):
        """Returns requeued specs."""

        spec_path = self._create_spec()
        entry = self.db.enqueue(spec_path)
        self.db.pick_next_spec()  # moves to running state
        self.db.set_running(entry["id"], "w-1")
        self.db.requeue(entry["id"])

        result = pick_next_spec(self.queue_dir, db=self.db)
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], entry["id"])


class TestGetActiveCount(DaemonOpsTestCase):
    """Tests for get_active_count()."""

    def test_empty_queue(self):
        """Returns 0 for empty queue."""
        self.assertEqual(get_active_count(self.queue_dir, db=self.db), 0)

    def test_counts_active_statuses(self):
        """Counts queued, requeued, running specs."""
        spec_path = self._create_spec()
        self.db.enqueue(spec_path)  # queued
        self.assertEqual(get_active_count(self.queue_dir, db=self.db), 1)

    def test_excludes_terminal_statuses(self):
        """Does not count completed, failed, canceled specs."""

        e1 = self.db.enqueue(self._create_spec())
        e2 = self.db.enqueue(self._create_spec())
        e3 = self.db.enqueue(self._create_spec())
        e4 = self.db.enqueue(self._create_spec())

        self.db.complete(e1["id"], 0, 0)
        self.db.fail(e2["id"], "test")
        self.db.cancel(e3["id"])
        # e4 remains queued

        self.assertEqual(get_active_count(self.queue_dir, db=self.db), 1)


class TestBatchedCallCount(DaemonOpsTestCase):
    """Verify that the batched approach reduces Python calls."""

    def test_completion_is_single_call(self):
        """process_worker_completion does everything in one function call.

        Before the optimization, check_worker_completion in the daemon
        would make 5-10 separate Python invocations per cycle. Now it
        makes a single call to process_worker_completion.
        """
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        # This single call replaces what used to be 5-10 separate Python calls:
        # 1. get_entry (iteration, max_iter)
        # 2. count_boi_tasks
        # 3. get_entry (spec_path)
        # 4. get_tasks_added_from_telemetry
        # 5. complete/requeue/fail
        # 6. write_event
        # 7. update_telemetry
        # 8. run_hook (on-complete)
        # 9. run_hook (on-fail)
        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        # Verify it handled everything in one call
        self.assertIn(result["outcome"], ("completed", "requeued", "failed", "crashed"))
        # Verify queue entry was updated
        updated = self.db.get_spec(entry["id"])
        self.assertNotEqual(updated["status"], "running")
        # Verify events were written
        event_files = list(Path(self.events_dir).glob("event-*.json"))
        self.assertGreater(len(event_files), 0)
        # Verify telemetry was written
        telemetry_file = Path(self.queue_dir) / f"{entry['id']}.telemetry.json"
        self.assertTrue(telemetry_file.exists())


class TestPostIterationValidation(DaemonOpsTestCase):
    """Tests for post-iteration spec validation (t-4)."""

    def _create_malformed_spec(self):
        """Create a spec file that will fail validation (missing Spec: section)."""
        if not hasattr(self, "_spec_counter"):
            self._spec_counter = 0
        self._spec_counter += 1
        spec_path = os.path.join(self._tmpdir.name, f"spec-{self._spec_counter}.md")
        # Missing **Spec:** section makes validation fail
        content = (
            "# Test Spec\n\n## Tasks\n\n"
            "### t-1: Malformed task\n"
            "PENDING\n\n"
            "No spec section here.\n"
            "**Verify:** true\n"
        )
        Path(spec_path).write_text(content)
        return spec_path

    def test_malformed_spec_triggers_crash(self):
        """When spec fails validation after iteration, treat as crash."""
        # First enqueue with a valid spec
        valid_spec = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = self.db.enqueue(valid_spec)
        self.db.set_running(entry["id"], "w-1")

        # Now replace the copied spec with a malformed one
        copied_spec = self.db.get_spec(entry["id"])["spec_path"]
        malformed_content = (
            "# Test Spec\n\n## Tasks\n\n"
            "### t-1: Malformed task\n"
            "PENDING\n\n"
            "No spec section here.\n"
            "**Verify:** true\n"
        )
        Path(copied_spec).write_text(malformed_content)

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "validation_failed")
        self.assertIn("validation_errors", result)
        self.assertGreater(len(result["validation_errors"]), 0)

        # Should have recorded a failure
        updated = self.db.get_spec(entry["id"])
        self.assertGreater(updated["consecutive_failures"], 0)

    def test_valid_spec_passes_validation(self):
        """When spec is valid, proceed normally (requeued or completed)."""
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        # Should proceed normally, not validation_failed
        self.assertIn(result["outcome"], ("completed", "requeued"))

    def test_done_to_pending_regression_detected(self):
        """Detect when a task regresses from DONE to PENDING."""
        # Create spec with a DONE task and a PENDING task
        spec_path = self._create_spec(tasks_pending=1, tasks_done=1)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        # Verify pre_iteration_tasks was saved
        updated_entry = self.db.get_spec(entry["id"])
        self.assertIsNotNone(updated_entry.get("pre_iteration_tasks"))
        _pre_tasks = updated_entry["pre_iteration_tasks"]
        if isinstance(_pre_tasks, str):
            import json as _j; _pre_tasks = _j.loads(_pre_tasks)
        # t-1 should be DONE, t-2 should be PENDING
        self.assertEqual(_pre_tasks["t-1"], "DONE")
        self.assertEqual(_pre_tasks["t-2"], "PENDING")

        # Now modify the copied spec to regress t-1 from DONE to PENDING
        copied_spec = updated_entry["spec_path"]
        regressed_content = (
            "# Test Spec\n\n**Workspace:** in-place\n\n## Tasks\n\n"
            "### t-1: Done task 1\nPENDING\n\n"
            "**Spec:** Did it.\n**Verify:** true\n\n"
            "### t-2: Pending task 2\nPENDING\n\n"
            "**Spec:** Do it.\n**Verify:** true\n"
        )
        Path(copied_spec).write_text(regressed_content)

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        # Should still proceed (regression is a warning, not a failure)
        self.assertIn(result["outcome"], ("completed", "requeued"))
        # But should report the regression
        self.assertIn("status_regressions", result)
        self.assertEqual(len(result["status_regressions"]), 1)
        self.assertIn("t-1", result["status_regressions"][0])

        # Should have written a regression event
        event_files = sorted(Path(self.events_dir).glob("event-*.json"))
        regression_events = [
            json.loads(f.read_text())
            for f in event_files
            if json.loads(f.read_text()).get("type") == "status_regression_detected"
        ]
        self.assertEqual(len(regression_events), 1)

    def test_validation_errors_written_to_iteration_metadata(self):
        """Validation errors are written to the iteration metadata JSON."""
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        # Create iteration metadata file
        iteration = self.db.get_spec(entry["id"])["iteration"]
        iter_meta_path = os.path.join(
            self.queue_dir, f"{entry['id']}.iteration-{iteration}.json"
        )
        Path(iter_meta_path).write_text(json.dumps({"iteration": iteration}) + "\n")

        # Corrupt the spec
        copied_spec = self.db.get_spec(entry["id"])["spec_path"]
        Path(copied_spec).write_text(
            "# Test Spec\n\n## Tasks\n\n"
            "### t-1: Bad task\nPENDING\n\n"
            "No spec section.\n**Verify:** true\n"
        )

        process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        # Check that validation errors were written to iteration metadata
        meta = json.loads(Path(iter_meta_path).read_text())
        self.assertIn("validation_errors", meta)
        self.assertGreater(len(meta["validation_errors"]), 0)


class TestCriticIntegration(DaemonOpsTestCase):
    """Tests for critic integration in process_worker_completion."""

    def setUp(self):
        super().setUp()
        # Set up critic config directory
        self.critic_dir = os.path.join(self.boi_state, "critic")
        self.custom_dir = os.path.join(self.critic_dir, "custom")
        os.makedirs(self.critic_dir, exist_ok=True)
        os.makedirs(self.custom_dir, exist_ok=True)

        # Write default critic config
        config = {
            "enabled": True,
            "trigger": "on_complete",
            "max_passes": 2,
            "checks": ["spec-integrity"],
            "custom_checks_dir": "custom",
            "timeout_seconds": 600,
        }
        config_path = os.path.join(self.critic_dir, "config.json")
        Path(config_path).write_text(json.dumps(config, indent=2) + "\n")

        # Create a minimal check file so generate_critic_prompt works
        checks_dir = os.path.join(self.script_dir, "templates", "checks")
        os.makedirs(checks_dir, exist_ok=True)

    def test_critic_triggered_on_completion(self):
        """When all tasks are DONE and critic is enabled, outcome is 'critic_review'."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=3)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "critic_review")
        self.assertIn("critic_prompt_path", result)
        self.assertEqual(result["critic_pass"], 1)

        # Spec should be requeued (so daemon picks it up for critic worker)
        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "requeued")

    def test_critic_pass_counting(self):
        """critic_passes increments with each critic review."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=3)
        entry = self.db.enqueue(spec_path)

        # Simulate one prior critic pass
        self.db.update_spec_fields(entry["id"], critic_passes=1)

        self.db.set_running(entry["id"], "w-1")

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "critic_review")
        self.assertEqual(result["critic_pass"], 2)

    def test_max_passes_enforcement(self):
        """When max_passes reached, completes without critic."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=3)
        entry = self.db.enqueue(spec_path)

        # Set critic_passes to match max_passes
        self.db.update_spec_fields(entry["id"], critic_passes=2)

        self.db.set_running(entry["id"], "w-1")

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "completed")
        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "completed")

    def test_critic_disabled_skips_review(self):
        """When critic is disabled, completes without review."""
        # Disable critic
        config = {"enabled": False}
        config_path = os.path.join(self.critic_dir, "config.json")
        Path(config_path).write_text(json.dumps(config, indent=2) + "\n")

        spec_path = self._create_spec(tasks_pending=0, tasks_done=3)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "completed")

    def test_critic_writes_event(self):
        """Critic review trigger writes an event."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=2)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        event_files = sorted(Path(self.events_dir).glob("event-*.json"))
        events = [json.loads(f.read_text()) for f in event_files]
        critic_events = [e for e in events if e["type"] == "critic_review_triggered"]
        self.assertEqual(len(critic_events), 1)
        self.assertEqual(critic_events[0]["queue_id"], entry["id"])

    def test_critic_prompt_file_written(self):
        """Critic prompt file is written to queue dir."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=2)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        prompt_path = result.get("critic_prompt_path", "")
        self.assertTrue(os.path.isfile(prompt_path))
        content = Path(prompt_path).read_text()
        # Should contain spec content
        self.assertIn("Test Spec", content)


class TestProcessCriticCompletion(DaemonOpsTestCase):
    """Tests for process_critic_completion()."""

    def setUp(self):
        super().setUp()
        # Set up critic dirs
        self.critic_dir = os.path.join(self.boi_state, "critic")
        os.makedirs(self.critic_dir, exist_ok=True)
        os.makedirs(os.path.join(self.critic_dir, "custom"), exist_ok=True)

    def test_critic_approval_detection(self):
        """When spec has ## Critic Approved, marks completed."""
        from lib.daemon_ops import process_critic_completion

        spec_path = self._create_spec(tasks_pending=0, tasks_done=2)
        entry = self.db.enqueue(spec_path)

        # Append Critic Approved to the copied spec
        copied_spec = entry["spec_path"]
        content = Path(copied_spec).read_text()
        content += "\n## Critic Approved\n\n2026-03-06\n"
        Path(copied_spec).write_text(content)

        result = process_critic_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=copied_spec,
        
            db=self.db,
        )

        self.assertEqual(result["outcome"], "critic_approved")

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "completed")
        self.assertEqual(updated["critic_passes"], 1)

    def test_critic_task_injection(self):
        """When critic adds [CRITIC] tasks, spec is requeued."""
        from lib.daemon_ops import process_critic_completion

        spec_path = self._create_spec(tasks_pending=0, tasks_done=2)
        entry = self.db.enqueue(spec_path)

        # Add a CRITIC task to the copied spec
        copied_spec = entry["spec_path"]
        content = Path(copied_spec).read_text()
        content += (
            "\n### t-99: [CRITIC] Fix missing error handling\n"
            "PENDING\n\n"
            "**Spec:** Add error handling to function X.\n\n"
            "**Verify:** python3 -m pytest tests/test_file.py\n"
        )
        Path(copied_spec).write_text(content)

        result = process_critic_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=copied_spec,
        
            db=self.db,
        )

        self.assertEqual(result["outcome"], "critic_tasks_added")
        self.assertEqual(result["critic_tasks_added"], 1)

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "requeued")
        self.assertEqual(updated["critic_passes"], 1)

    def test_critic_no_output_completes(self):
        """When critic produces no valid output, completes anyway."""
        from lib.daemon_ops import process_critic_completion

        spec_path = self._create_spec(tasks_pending=0, tasks_done=2)
        entry = self.db.enqueue(spec_path)

        # Spec has no Critic Approved and no CRITIC tasks
        copied_spec = entry["spec_path"]

        result = process_critic_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=copied_spec,
        
            db=self.db,
        )

        self.assertEqual(result["outcome"], "critic_approved")
        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "completed")

    def test_critic_passes_increment(self):
        """critic_passes is incremented on each call."""
        from lib.daemon_ops import process_critic_completion

        spec_path = self._create_spec(tasks_pending=0, tasks_done=2)
        entry = self.db.enqueue(spec_path)

        # Set prior critic passes
        self.db.update_spec_fields(entry["id"], critic_passes=1)

        # Append Critic Approved
        copied_spec = entry["spec_path"]
        content = Path(copied_spec).read_text()
        content += "\n## Critic Approved\n"
        Path(copied_spec).write_text(content)

        process_critic_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=copied_spec,
        
            db=self.db,
        )

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["critic_passes"], 2)


class TestCriticHelpers(unittest.TestCase):
    """Tests for critic helper functions."""

    def test_should_run_critic_enabled(self):
        """should_run_critic returns True when enabled and under max passes."""
        from lib.critic import should_run_critic

        config = {"enabled": True, "max_passes": 2, "trigger": "on_complete"}
        entry = {"critic_passes": 0}
        self.assertTrue(should_run_critic(entry, config))

    def test_should_run_critic_disabled(self):
        """should_run_critic returns False when disabled."""
        from lib.critic import should_run_critic

        config = {"enabled": False, "max_passes": 2, "trigger": "on_complete"}
        entry = {"critic_passes": 0}
        self.assertFalse(should_run_critic(entry, config))

    def test_should_run_critic_max_passes_reached(self):
        """should_run_critic returns False when max passes reached."""
        from lib.critic import should_run_critic

        config = {"enabled": True, "max_passes": 2, "trigger": "on_complete"}
        entry = {"critic_passes": 2}
        self.assertFalse(should_run_critic(entry, config))

    def test_should_run_critic_no_critic_passes_field(self):
        """should_run_critic handles missing critic_passes field."""
        from lib.critic import should_run_critic

        config = {"enabled": True, "max_passes": 2, "trigger": "on_complete"}
        entry = {}  # No critic_passes field
        self.assertTrue(should_run_critic(entry, config))

    def test_parse_critic_result_approved(self):
        """parse_critic_result detects Critic Approved section."""
        from lib.critic import parse_critic_result

        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
            f.write(
                "# Spec\n\n## Tasks\n\n### t-1: Done\nDONE\n\n## Critic Approved\n\n2026-03-06\n"
            )
            f.flush()
            result = parse_critic_result(f.name)
            self.assertTrue(result["approved"])
            self.assertEqual(result["critic_tasks_added"], 0)
            os.unlink(f.name)

    def test_parse_critic_result_tasks_added(self):
        """parse_critic_result counts [CRITIC] PENDING tasks."""
        from lib.critic import parse_critic_result

        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
            f.write(
                "# Spec\n\n## Tasks\n\n"
                "### t-1: Done\nDONE\n\n"
                "### t-2: [CRITIC] Fix thing\nPENDING\n\n"
                "**Spec:** Fix it.\n**Verify:** true\n\n"
                "### t-3: [CRITIC] Fix other\nPENDING\n\n"
                "**Spec:** Fix it too.\n**Verify:** true\n"
            )
            f.flush()
            result = parse_critic_result(f.name)
            self.assertFalse(result["approved"])
            self.assertEqual(result["critic_tasks_added"], 2)
            os.unlink(f.name)

    def test_parse_critic_result_nonexistent_file(self):
        """parse_critic_result handles missing file gracefully."""
        from lib.critic import parse_critic_result

        result = parse_critic_result("/nonexistent/path.md")
        self.assertFalse(result["approved"])
        self.assertEqual(result["critic_tasks_added"], 0)

    def test_generate_critic_prompt(self):
        """generate_critic_prompt produces a prompt with spec content and checks."""
        from lib.critic import generate_critic_prompt

        with tempfile.TemporaryDirectory() as tmpdir:
            # Set up minimal state_dir and boi_dir
            state_dir = os.path.join(tmpdir, "state")
            boi_dir = os.path.join(tmpdir, "boi")
            checks_dir = os.path.join(boi_dir, "templates", "checks")
            critic_dir = os.path.join(state_dir, "critic")
            custom_dir = os.path.join(critic_dir, "custom")

            os.makedirs(checks_dir)
            os.makedirs(custom_dir)

            # Write a simple critic prompt template
            template_path = os.path.join(boi_dir, "templates", "critic-prompt.md")
            Path(template_path).write_text(
                "Spec: {{SPEC_CONTENT}}\nChecks: {{CHECKS}}\n"
                "Queue: {{QUEUE_ID}}\nIter: {{ITERATION}}\n"
            )

            # Write config
            config_path = os.path.join(critic_dir, "config.json")
            config = {
                "enabled": True,
                "checks": ["test-check"],
                "custom_checks_dir": "custom",
            }
            Path(config_path).write_text(json.dumps(config))

            # Write a check file
            Path(os.path.join(checks_dir, "test-check.md")).write_text(
                "# Test Check\n- Item 1\n"
            )

            # Write a spec file
            spec_path = os.path.join(tmpdir, "spec.md")
            Path(spec_path).write_text("# My Spec\n## Tasks\n### t-1: Task\nDONE\n")

            result = generate_critic_prompt(
                spec_path=spec_path,
                queue_id="q-001",
                iteration=1,
                config=config,
                boi_dir=boi_dir,
                state_dir=state_dir,
            )

            self.assertIn("My Spec", result)
            self.assertIn("Test Check", result)
            self.assertIn("q-001", result)
            self.assertIn("1", result)


class TestCriticReviewDaemonIntegration(DaemonOpsTestCase):
    """Tests for daemon handling of critic_review outcome.

    Verifies that when process_worker_completion returns critic_review,
    the daemon's expected follow-up actions (phase transition, critic worker
    launch, and critic completion processing) work correctly.
    """

    def setUp(self):
        super().setUp()
        # Enable critic
        self.critic_dir = os.path.join(self.boi_state, "critic")
        os.makedirs(self.critic_dir, exist_ok=True)
        os.makedirs(os.path.join(self.critic_dir, "custom"), exist_ok=True)
        config = {
            "enabled": True,
            "trigger": "on_complete",
            "max_passes": 2,
            "checks": ["spec-integrity"],
            "custom_checks_dir": "custom",
            "timeout_seconds": 600,
        }
        config_path = os.path.join(self.critic_dir, "config.json")
        Path(config_path).write_text(json.dumps(config, indent=2) + "\n")
        checks_dir = os.path.join(self.script_dir, "templates", "checks")
        os.makedirs(checks_dir, exist_ok=True)

    def test_critic_review_triggers_phase_transition(self):
        """After critic_review outcome, setting phase to 'critic' enables
        process_critic_completion to be called on next completion."""

        spec_path = self._create_spec(tasks_pending=0, tasks_done=3)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        # Step 1: Process worker completion — should return critic_review
        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )
        self.assertEqual(result["outcome"], "critic_review")

        # Step 2: Simulate daemon setting phase to "critic" (as daemon.py does)
        self.db.update_spec_fields(entry["id"], phase="critic")
        self.db.set_running(entry["id"], "w-1", phase="critic")

        # Step 3: Simulate critic worker completing with approval
        # Add "## Critic Approved" to the copied spec
        _spec_copy = self.db.get_spec(entry["id"])["spec_path"]
        _c = Path(_spec_copy).read_text()
        _c += "\n## Critic Approved\n\nAll checks passed.\n"
        Path(_spec_copy).write_text(_c)

        from lib.daemon_ops import process_critic_completion

        critic_result = process_critic_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=_spec_copy,
            db=self.db,
        )
        self.assertEqual(critic_result["outcome"], "critic_approved")

        # Verify spec is now completed
        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "completed")

    def test_critic_review_requeues_for_critic_worker(self):
        """critic_review outcome requeues spec so daemon can launch critic worker."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=3)
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "critic_review")

        # Verify prompt file exists for critic worker
        prompt_path = result.get("critic_prompt_path", "")
        self.assertTrue(os.path.isfile(prompt_path))

        # Entry should be requeued (daemon will pick it up and launch critic)
        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "requeued")


class TestSelfHeal(DaemonOpsTestCase):
    """Tests for self_heal() and its sub-functions."""

    def test_stale_running_spec_recovered(self):
        """Running spec with dead PID should be reset to requeued."""
        from lib.daemon_ops import self_heal

        spec_path = self._create_spec(tasks_pending=2)
        entry = self.db.enqueue(spec_path)
        qid = entry["id"]
        self.db.set_running(qid, "w-1")

        # Register worker and assign to spec so recover_running_specs can find it
        self.db.register_worker("w-1", self.queue_dir)
        self.db.conn.execute(
            "UPDATE workers SET current_spec_id=?, current_pid=NULL WHERE id=?",
            (qid, "w-1"),
        )
        self.db.conn.commit()

        # DB path: spec in running with no process record -> auto-recovered
        actions = self_heal(self.queue_dir, {"w-1": qid}, db=self.db)

        # Should have recovered the spec
        stale_actions = [a for a in actions if a["action"] == "stale_running_recovered"]
        self.assertEqual(len(stale_actions), 1)
        self.assertIn(qid, stale_actions[0]["detail"])

        # Entry should now be requeued
        updated = self.db.get_spec(qid)
        self.assertEqual(updated["status"], "requeued")


    def test_stale_running_no_pid_file(self):
        """Running spec with no PID file at all should be recovered."""
        from lib.daemon_ops import self_heal

        spec_path = self._create_spec(tasks_pending=2)
        entry = self.db.enqueue(spec_path)
        qid = entry["id"]
        self.db.set_running(qid, "w-1")

        # Register worker and assign to spec so recover_running_specs can find it
        self.db.register_worker("w-1", self.queue_dir)
        self.db.conn.execute(
            "UPDATE workers SET current_spec_id=?, current_pid=NULL WHERE id=?",
            (qid, "w-1"),
        )
        self.db.conn.commit()

        # DB path: spec in running with no process record -> auto-recovered
        actions = self_heal(self.queue_dir, {"w-1": qid}, db=self.db)

        stale_actions = [a for a in actions if a["action"] == "stale_running_recovered"]
        self.assertEqual(len(stale_actions), 1)

        updated = self.db.get_spec(qid)
        self.assertEqual(updated["status"], "requeued")

    def test_orphaned_worker_freed(self):
        """Worker assigned to completed spec should be reported as orphaned."""
        from lib.daemon_ops import self_heal

        spec_path = self._create_spec(tasks_pending=0, tasks_done=3)
        entry = self.db.enqueue(spec_path)
        qid = entry["id"]
        self.db.set_running(qid, "w-1")
        self.db.complete(qid, 3, 3)

        # Worker still thinks it's assigned to this spec
        worker_specs = {"w-1": qid, "w-2": ""}
        actions = self_heal(self.queue_dir, worker_specs, db=self.db)

        orphan_actions = [a for a in actions if a["action"] == "orphaned_worker"]
        self.assertEqual(len(orphan_actions), 1)
        self.assertEqual(orphan_actions[0]["worker_id"], "w-1")
        self.assertEqual(orphan_actions[0]["queue_id"], qid)

    def test_orphaned_worker_missing_spec(self):
        """Worker assigned to nonexistent spec should be reported as orphaned."""
        from lib.daemon_ops import self_heal

        worker_specs = {"w-1": "q-nonexistent", "w-2": ""}
        actions = self_heal(self.queue_dir, worker_specs, db=self.db)

        orphan_actions = [a for a in actions if a["action"] == "orphaned_worker"]
        self.assertEqual(len(orphan_actions), 1)
        self.assertIn("missing", orphan_actions[0]["detail"])

    def test_blocked_by_completed_spec_unblocked(self):
        """Spec blocked by a completed spec should have that dep removed."""
        from lib.daemon_ops import self_heal

        # Create blocker spec and complete it
        blocker_path = self._create_spec(tasks_pending=0, tasks_done=1)
        blocker = self.db.enqueue(blocker_path)
        self.db.set_running(blocker["id"], "w-1")
        self.db.complete(blocker["id"], 1, 1)

        # Create blocked spec
        blocked_path = self._create_spec(tasks_pending=2)
        blocked = self.db.enqueue(blocked_path, blocked_by=[blocker["id"]])

        # Verify it's blocked (via spec_dependencies table)
        dep_count = self.db.conn.execute(
            "SELECT COUNT(*) FROM spec_dependencies WHERE spec_id=? AND blocks_on=?",
            (blocked["id"], blocker["id"]),
        ).fetchone()[0]
        self.assertEqual(dep_count, 1)

        actions = self_heal(self.queue_dir, {}, db=self.db)

        cleanup_actions = [a for a in actions if a["action"] == "blocked_by_cleaned"]
        self.assertEqual(len(cleanup_actions), 1)

        dep_count_after = self.db.conn.execute(
            "SELECT COUNT(*) FROM spec_dependencies WHERE spec_id=?",
            (blocked["id"],),
        ).fetchone()[0]
        self.assertEqual(dep_count_after, 0)

    def test_blocked_by_missing_spec_unblocked(self):
        """Spec blocked by a nonexistent spec should have that dep removed."""
        from lib.daemon_ops import self_heal
        spec_path = self._create_spec(tasks_pending=2)
        entry = self.db.enqueue(spec_path)
        qid = entry["id"]

        # Manually set blocked_by to a nonexistent spec via DB
        # Disable FK so we can insert a nonexistent blocks_on
        self.db.conn.execute("PRAGMA foreign_keys=OFF")
        self.db.conn.execute(
            "INSERT OR IGNORE INTO spec_dependencies (spec_id, blocks_on) VALUES (?, ?)",
            (qid, "q-999"),
        )
        self.db.conn.commit()
        self.db.conn.execute("PRAGMA foreign_keys=ON")

        actions = self_heal(self.queue_dir, {}, db=self.db)

        cleanup_actions = [a for a in actions if a["action"] == "blocked_by_cleaned"]
        self.assertEqual(len(cleanup_actions), 1)
        self.assertIn("missing", cleanup_actions[0]["detail"])

        dep_count_after = self.db.conn.execute(
            "SELECT COUNT(*) FROM spec_dependencies WHERE spec_id=?",
            (qid,),
        ).fetchone()[0]
        self.assertEqual(dep_count_after, 0)

    def test_circular_dependency_detected(self):
        """Circular dependency A->B->C->A should cancel all specs in cycle."""
        from lib.daemon_ops import self_heal
        # Create 3 specs
        specs = []
        for _ in range(3):
            path = self._create_spec(tasks_pending=1)
            specs.append(self.db.enqueue(path))

        a_id, b_id, c_id = specs[0]["id"], specs[1]["id"], specs[2]["id"]

        # Create cycle: A blocked by C, B blocked by A, C blocked by B
        for qid, dep in [(a_id, c_id), (b_id, a_id), (c_id, b_id)]:
            self.db.conn.execute(
                "INSERT OR IGNORE INTO spec_dependencies (spec_id, blocks_on) VALUES (?, ?)",
                (qid, dep),
            )
        self.db.conn.commit()

        actions = self_heal(self.queue_dir, {}, db=self.db)

        cycle_actions = [
            a for a in actions if a["action"] == "circular_dependency_canceled"
        ]
        # All 3 should be canceled
        self.assertEqual(len(cycle_actions), 3)

        for spec in specs:
            updated = self.db.get_spec(spec["id"])
            self.assertEqual(updated["status"], "canceled")

    def test_stale_lock_no_action_when_no_lock(self):
        """No lock file should produce no action."""
        from lib.daemon_ops import self_heal

        actions = self_heal(self.queue_dir, {}, db=self.db)

        lock_actions = [a for a in actions if "lock" in a.get("action", "")]
        self.assertEqual(len(lock_actions), 0)

    def test_idle_workers_no_false_orphans(self):
        """Idle workers (empty spec assignment) should NOT be reported as orphaned."""
        from lib.daemon_ops import self_heal

        worker_specs = {"w-1": "", "w-2": ""}
        actions = self_heal(self.queue_dir, worker_specs, db=self.db)

        orphan_actions = [a for a in actions if a["action"] == "orphaned_worker"]
        self.assertEqual(len(orphan_actions), 0)

    def test_self_heal_multiple_issues(self):
        """Self-heal should fix multiple issues in a single call."""
        from lib.daemon_ops import self_heal

        # Issue 1: stale running spec
        spec1_path = self._create_spec(tasks_pending=2)
        entry1 = self.db.enqueue(spec1_path)
        self.db.set_running(entry1["id"], "w-1")
        pid_file = os.path.join(self.queue_dir, f"{entry1['id']}.pid")
        Path(pid_file).write_text("999999999\n")
        # Register worker so recover_running_specs can detect it as stale
        self.db.register_worker("w-1", self.queue_dir)
        self.db.conn.execute(
            "UPDATE workers SET current_spec_id=?, current_pid=999999999 WHERE id=?",
            (entry1["id"], "w-1"),
        )
        self.db.conn.commit()

        # Issue 2: blocked by missing spec
        spec2_path = self._create_spec(tasks_pending=1)
        entry2 = self.db.enqueue(spec2_path)
        # Disable FK so we can insert a nonexistent blocks_on
        self.db.conn.execute("PRAGMA foreign_keys=OFF")
        self.db.conn.execute(
            "INSERT OR IGNORE INTO spec_dependencies (spec_id, blocks_on) VALUES (?, ?)",
            (entry2["id"], "q-missing"),
        )
        self.db.conn.commit()
        self.db.conn.execute("PRAGMA foreign_keys=ON")

        # Issue 3: orphaned worker
        spec3_path = self._create_spec(tasks_pending=0, tasks_done=1)
        entry3 = self.db.enqueue(spec3_path)
        self.db.set_running(entry3["id"], "w-2")
        self.db.complete(entry3["id"], 1, 1)

        worker_specs = {"w-1": entry1["id"], "w-2": entry3["id"]}
        actions = self_heal(self.queue_dir, worker_specs, db=self.db)

        # Should have at least 3 actions (one for each issue)
        self.assertGreaterEqual(len(actions), 3)

        action_types = {a["action"] for a in actions}
        self.assertIn("stale_running_recovered", action_types)
        self.assertIn("blocked_by_cleaned", action_types)
        self.assertIn("orphaned_worker", action_types)

    def test_max_running_duration_exceeded(self):
        """Spec running longer than max duration should be force-failed."""
        from datetime import datetime, timedelta, timezone

        from lib.daemon_ops import self_heal
        spec_path = self._create_spec(tasks_pending=2)
        entry = self.db.enqueue(spec_path)
        qid = entry["id"]
        self.db.set_running(qid, "w-1")

        # Set worker_timeout_seconds=60, max_iterations=5 -> max_duration=300s
        self.db.conn.execute(
            "UPDATE specs SET worker_timeout_seconds=60, max_iterations=5, "
            "first_running_at=? WHERE id=?",
            ((datetime.now(timezone.utc) - timedelta(seconds=600)).isoformat(), qid),
        )
        self.db.conn.commit()

        # DB path: add fake process record so stale_running check doesn't trigger
        # processes schema: (pid, spec_id, worker_id, iteration, phase, started_at)
        self.db.conn.execute(
            "INSERT OR IGNORE INTO processes (pid, spec_id, worker_id, iteration, phase, started_at) "
            "VALUES (?, ?, 'w-1', 1, 'execute', ?)",
            (os.getpid(), qid,
             (datetime.now(timezone.utc) - timedelta(seconds=600)).isoformat()),
        )
        self.db.conn.commit()

        actions = self_heal(self.queue_dir, {"w-1": qid}, db=self.db)

        # Should have force-failed
        duration_actions = [
            a for a in actions if a["action"] == "max_running_duration_exceeded"
        ]
        self.assertEqual(len(duration_actions), 1)
        self.assertIn(qid, duration_actions[0]["detail"])

        # Entry should now be failed with the right reason
        updated = self.db.get_spec(qid)
        self.assertEqual(updated["status"], "failed")
        self.assertEqual(updated["failure_reason"], "Maximum running duration exceeded")


    def test_max_running_duration_not_exceeded(self):
        """Spec running within max duration should NOT be force-failed."""
        from datetime import datetime, timedelta, timezone

        from lib.daemon_ops import self_heal
        spec_path = self._create_spec(tasks_pending=2)
        entry = self.db.enqueue(spec_path)
        qid = entry["id"]
        self.db.set_running(qid, "w-1")

        # Set first_running_at to just 10 seconds ago (well within default limit)
        self.db.conn.execute(
            "UPDATE specs SET first_running_at=? WHERE id=?",
            ((datetime.now(timezone.utc) - timedelta(seconds=10)).isoformat(), qid),
        )
        self.db.conn.commit()

        # DB path: add fake process record
        # processes schema: (pid, spec_id, worker_id, iteration, phase, started_at)
        self.db.conn.execute(
            "INSERT OR IGNORE INTO processes (pid, spec_id, worker_id, iteration, phase, started_at) "
            "VALUES (?, ?, 'w-1', 1, 'execute', ?)",
            (os.getpid(), qid,
             (datetime.now(timezone.utc) - timedelta(seconds=10)).isoformat()),
        )
        self.db.conn.commit()

        actions = self_heal(self.queue_dir, {"w-1": qid}, db=self.db)

        # Should NOT have any max_running_duration actions
        duration_actions = [
            a for a in actions if a["action"] == "max_running_duration_exceeded"
        ]
        self.assertEqual(len(duration_actions), 0)

        # Entry should still be running
        updated = self.db.get_spec(qid)
        self.assertEqual(updated["status"], "running")

    def test_first_running_at_set_on_first_run(self):
        """first_running_at should be set when spec first enters running status."""
        spec_path = self._create_spec(tasks_pending=2)
        entry = self.db.enqueue(spec_path)
        qid = entry["id"]

        # Before running, first_running_at should be None
        pre = self.db.get_spec(qid)
        self.assertIsNone(pre.get("first_running_at"))

        # After first set_running, first_running_at should be set
        self.db.set_running(qid, "w-1")
        post = self.db.get_spec(qid)
        self.assertIn("first_running_at", post)
        first_time = post["first_running_at"]

        # After requeue and second set_running, first_running_at should be preserved
        self.db.update_spec_fields(qid, status="requeued")

        self.db.set_running(qid, "w-2")
        post2 = self.db.get_spec(qid)
        self.assertEqual(post2["first_running_at"], first_time)


if __name__ == "__main__":
    unittest.main()


class TestBugFixMaxIterAllTasksDone(DaemonOpsTestCase):
    """Regression tests for Bug 1: max-iter not enforced when all tasks DONE."""

    def test_max_iter_enforced_when_all_tasks_done(self):
        """Max-iter should fail the spec even when pending_count == 0."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=3)
        entry = self.db.enqueue(spec_path, max_iterations=1)
        self.db.set_running(entry["id"], "w-1")  # iteration becomes 1

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        # With critic disabled, all tasks done + at max iter = should complete, not loop
        # The key assertion: outcome must NOT be "critic_review" (which causes infinite requeue)
        self.assertIn(result["outcome"], ("completed", "failed"))
        updated = self.db.get_spec(entry["id"])
        self.assertIn(updated["status"], ("completed", "failed"))
        # Must NOT be requeued
        self.assertNotEqual(updated["status"], "requeued")

    def test_max_iter_enforced_with_critic_enabled(self):
        """Even with critic enabled, max-iter should stop iteration.

        The real bug: when critic is enabled and generate_critic_prompt succeeds,
        the spec gets requeued at line 415. The max-iter check at line 458 is
        structurally unreachable because it's in an elif after pending_count==0.
        We mock generate_critic_prompt to succeed and verify requeue doesn't happen.
        """
        spec_path = self._create_spec(tasks_pending=0, tasks_done=3)
        entry = self.db.enqueue(spec_path, max_iterations=1)
        self.db.set_running(entry["id"], "w-1")  # iteration becomes 1

        # Enable critic
        critic_dir = os.path.join(self.boi_state, "critic")
        config_path = os.path.join(critic_dir, "config.json")
        Path(config_path).write_text(json.dumps({
            "enabled": True,
            "max_passes": 2,
            "checks": [{"name": "test", "prompt": "check it"}],
        }, indent=2) + "\n")

        # Patch generate_critic_prompt to succeed (return a string, don't throw)
        import unittest.mock as mock
        with mock.patch("lib.daemon_ops.generate_critic_prompt", return_value="mock critic prompt"):
            result = process_worker_completion(
                ctx=self.ctx,
                queue_id=entry["id"],
                exit_code="0",
            )

        # At max-iter, should NOT requeue for critic — should fail or complete
        updated = self.db.get_spec(entry["id"])
        self.assertNotEqual(updated["status"], "requeued",
                          "Spec should not be requeued when at max iterations, "
                          "even if critic wants to run. "
                          f"Got outcome={result['outcome']}, status={updated['status']}")

    def test_max_iter_vastly_exceeded_still_stops(self):
        """Simulate the actual bug: iteration=800, max_iter=10, all tasks done."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=3)
        entry = self.db.enqueue(spec_path, max_iterations=10)

        # Manually set iteration to 800 to simulate the bug state
        self.db.conn.execute(
            "UPDATE specs SET iteration=800, status='running' WHERE id=?",
            (entry["id"],),
        )
        self.db.conn.commit()

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        updated = self.db.get_spec(entry["id"])
        self.assertNotEqual(updated["status"], "requeued",
                          f"Spec at iteration 800 (max 10) must not be requeued! "
                          f"Got outcome={result['outcome']}, status={updated['status']}")


class TestBugFixDoubleAssignment(DaemonOpsTestCase):
    """Regression tests for Bug 2: TOCTOU race in dequeue."""

    def test_dequeue_prevents_double_pickup(self):
        """After pick_next_spec returns a spec, the same spec should not be picked again."""
        from lib.daemon_ops import pick_next_spec

        spec_path = self._create_spec(tasks_pending=2)
        self.db.enqueue(spec_path)

        first = pick_next_spec(self.queue_dir, db=self.db)
        self.assertIsNotNone(first)

        second = pick_next_spec(self.queue_dir, db=self.db)
        # Second pick must return None (first is now 'assigning', not 'queued')
        self.assertIsNone(second,
                         "pick_next_spec() returned the same spec twice — TOCTOU race condition!")


# ─── Database-backed tests ─────────────────────────────────────────────────


from lib.db import Database


class DaemonOpsDBTestCase(unittest.TestCase):
    """Base test case that creates a Database + temp dirs for db-backed tests."""

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.boi_state = self._tmpdir.name
        self.queue_dir = os.path.join(self.boi_state, "queue")
        self.events_dir = os.path.join(self.boi_state, "events")
        self.log_dir = os.path.join(self.boi_state, "logs")
        self.hooks_dir = os.path.join(self.boi_state, "hooks")
        self.script_dir = str(Path(__file__).resolve().parent.parent)
        os.makedirs(self.queue_dir, exist_ok=True)
        os.makedirs(self.events_dir)
        os.makedirs(self.log_dir)
        os.makedirs(self.hooks_dir)

        # Disable critic by default
        critic_dir = os.path.join(self.boi_state, "critic")
        os.makedirs(critic_dir, exist_ok=True)
        os.makedirs(os.path.join(critic_dir, "custom"), exist_ok=True)
        config_path = os.path.join(critic_dir, "config.json")
        Path(config_path).write_text(
            json.dumps({"enabled": False}, indent=2) + "\n"
        )

        db_path = os.path.join(self.boi_state, "boi.db")
        self.db = Database(db_path, self.queue_dir)
        self.ctx = CompletionContext(
            queue_dir=self.queue_dir,
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            log_dir=self.log_dir,
            script_dir=self.script_dir,
            db=self.db,
        )

    def tearDown(self):
        self.db.close()
        self._tmpdir.cleanup()

    def _create_spec(self, tasks_pending=3, tasks_done=0, tasks_skipped=0):
        """Create a temp spec.md file with the given task counts."""
        if not hasattr(self, "_spec_counter"):
            self._spec_counter = 0
        self._spec_counter += 1
        spec_path = os.path.join(
            self._tmpdir.name, f"spec-{self._spec_counter}.md"
        )
        lines = ["# Test Spec\n\n**Workspace:** in-place\n\n## Tasks\n"]
        tid = 1
        for _ in range(tasks_done):
            lines.append(
                f"\n### t-{tid}: Done task {tid}\nDONE\n\n"
                "**Spec:** Did it.\n**Verify:** true\n"
            )
            tid += 1
        for _ in range(tasks_pending):
            lines.append(
                f"\n### t-{tid}: Pending task {tid}\nPENDING\n\n"
                "**Spec:** Do it.\n**Verify:** true\n"
            )
            tid += 1
        for _ in range(tasks_skipped):
            lines.append(
                f"\n### t-{tid}: Skipped task {tid}\nSKIPPED\n\n"
                "**Spec:** Skip.\n**Verify:** true\n"
            )
            tid += 1
        Path(spec_path).write_text("".join(lines))
        return spec_path

    def _enqueue_and_run(self, spec_path, max_iterations=30):
        """Enqueue a spec into the database and set it running."""
        entry = self.db.enqueue(spec_path, max_iterations=max_iterations)
        spec = self.db.pick_next_spec()
        self.db.set_running(spec["id"], "w-1", phase="execute")
        return entry


class TestProcessWorkerCompletionDB(DaemonOpsDBTestCase):
    """Tests for process_worker_completion() with Database backend."""

    def test_all_tasks_done_marks_completed(self):
        """When all tasks DONE, outcome is 'completed' via db."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=3)
        entry = self._enqueue_and_run(spec_path)

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "completed")
        self.assertEqual(result["pending_count"], 0)
        self.assertEqual(result["done_count"], 3)

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "completed")

    def test_pending_tasks_requeues(self):
        """When pending tasks remain, outcome is 'requeued' via db."""
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = self._enqueue_and_run(spec_path)

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "requeued")
        self.assertEqual(result["pending_count"], 2)

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "requeued")

    def test_max_iterations_fails(self):
        """When max iterations reached with pending tasks, fails via db."""
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = self._enqueue_and_run(spec_path, max_iterations=1)

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "failed")
        self.assertEqual(result["reason"], "max_iterations")

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "failed")

    def test_no_exit_code_crashes(self):
        """When exit_code is None, outcome is 'crashed' via db."""
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = self._enqueue_and_run(spec_path)

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code=None,
        )

        self.assertEqual(result["outcome"], "crashed")

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "requeued")
        self.assertEqual(updated["consecutive_failures"], 1)

    def test_nonzero_exit_code_crashes(self):
        """When exit_code is non-zero, outcome is 'crashed' via db."""
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = self._enqueue_and_run(spec_path)

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="1",
        )

        self.assertEqual(result["outcome"], "crashed")

    def test_consecutive_failures_exceed_threshold(self):
        """When too many consecutive failures, outcome is 'failed' via db."""
        spec_path = self._create_spec(tasks_pending=2)
        entry = self._enqueue_and_run(spec_path)

        # Simulate 4 prior failures via db
        for _ in range(4):
            self.db.record_failure(entry["id"])

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code=None,
        )

        self.assertEqual(result["outcome"], "failed")
        self.assertEqual(result["reason"], "consecutive_failures")

    def test_writes_events(self):
        """process_worker_completion writes events to events_dir via db."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=2)
        entry = self._enqueue_and_run(spec_path)

        process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        event_files = [
            f for f in os.listdir(self.events_dir) if f.endswith(".json")
        ]
        self.assertGreater(len(event_files), 0)

    def test_missing_entry_returns_error(self):
        """When queue_id doesn't exist, returns error via db."""
        result = process_worker_completion(
            ctx=self.ctx,
            queue_id="q-999",
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "error")

    def test_timeout_crash_has_failure_reason(self):
        """Timeout crash should include timeout duration in reason."""
        spec_path = self._create_spec(tasks_pending=2)
        entry = self._enqueue_and_run(spec_path)

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code=None,
            timeout=True,
        )

        self.assertEqual(result["outcome"], "crashed")
        self.assertIn("timed out", result["failure_reason"])

    def test_all_done_at_max_iter_completes_without_critic(self):
        """All tasks DONE at max-iter completes without critic via db."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=3)
        entry = self._enqueue_and_run(spec_path, max_iterations=1)

        result = process_worker_completion(
            ctx=self.ctx,
            queue_id=entry["id"],
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "completed")
        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "completed")


class TestPickNextSpecDB(DaemonOpsDBTestCase):
    """Tests for pick_next_spec() with Database backend."""

    def test_returns_next_spec(self):
        """pick_next_spec returns the next queued spec via db."""
        spec_path = self._create_spec(tasks_pending=2)
        entry = self.db.enqueue(spec_path)

        result = pick_next_spec(self.queue_dir, db=self.db)

        self.assertIsNotNone(result)
        self.assertEqual(result["id"], entry["id"])
        self.assertIn("spec_path", result)
        self.assertIn("iteration", result)
        self.assertIn("max_iterations", result)

    def test_returns_none_when_empty(self):
        """pick_next_spec returns None when no specs are queued."""
        result = pick_next_spec(self.queue_dir, db=self.db)
        self.assertIsNone(result)

    def test_returns_none_when_all_running(self):
        """pick_next_spec returns None when all specs are running."""
        spec_path = self._create_spec(tasks_pending=2)
        self.db.enqueue(spec_path)
        spec = self.db.pick_next_spec()
        self.db.set_running(spec["id"], "w-1")

        result = pick_next_spec(self.queue_dir, db=self.db)
        self.assertIsNone(result)

    def test_priority_ordering(self):
        """Higher priority (lower number) specs are returned first."""
        spec1 = self._create_spec(tasks_pending=2)
        spec2 = self._create_spec(tasks_pending=2)

        self.db.enqueue(spec1, priority=200)
        entry2 = self.db.enqueue(spec2, priority=50)

        result = pick_next_spec(self.queue_dir, db=self.db)
        self.assertEqual(result["id"], entry2["id"])


class TestGetActiveCountDB(DaemonOpsDBTestCase):
    """Tests for get_active_count() with Database backend."""

    def test_counts_active_specs(self):
        """Active count includes queued, requeued, running, needs_review."""
        spec1 = self._create_spec(tasks_pending=2)
        spec2 = self._create_spec(tasks_pending=2)
        spec3 = self._create_spec(tasks_pending=0, tasks_done=3)

        self.db.enqueue(spec1)  # queued
        self.db.enqueue(spec2)  # queued
        entry3 = self.db.enqueue(spec3)
        self.db.complete(entry3["id"], 3, 3)  # completed

        count = get_active_count(self.queue_dir, db=self.db)
        self.assertEqual(count, 2)

    def test_empty_queue_returns_zero(self):
        """Empty queue returns 0."""
        count = get_active_count(self.queue_dir, db=self.db)
        self.assertEqual(count, 0)

    def test_excludes_terminal_statuses(self):
        """Completed, failed, canceled specs not counted."""
        spec1 = self._create_spec(tasks_pending=2)
        spec2 = self._create_spec(tasks_pending=2)
        spec3 = self._create_spec(tasks_pending=2)

        e1 = self.db.enqueue(spec1)
        e2 = self.db.enqueue(spec2)
        e3 = self.db.enqueue(spec3)

        self.db.fail(e1["id"], "test")
        self.db.cancel(e2["id"])
        self.db.complete(e3["id"], 0, 0)

        count = get_active_count(self.queue_dir, db=self.db)
        self.assertEqual(count, 0)


class TestCrashRequeueDB(DaemonOpsDBTestCase):
    """Tests for crash_requeue preserving failure state."""

    def test_crash_requeue_preserves_consecutive_failures(self):
        """crash_requeue sets status to requeued without resetting failures."""
        spec_path = self._create_spec(tasks_pending=2)
        entry = self._enqueue_and_run(spec_path)

        # Record a failure (sets consecutive_failures=1, cooldown)
        self.db.record_failure(entry["id"])

        # crash_requeue should keep the failure count
        self.db.crash_requeue(entry["id"])

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "requeued")
        self.assertEqual(updated["consecutive_failures"], 1)
        self.assertIsNotNone(updated["cooldown_until"])

    def test_normal_requeue_resets_failures(self):
        """Normal requeue resets consecutive_failures to 0."""
        spec_path = self._create_spec(tasks_pending=2)
        entry = self._enqueue_and_run(spec_path)

        self.db.record_failure(entry["id"])
        self.db.requeue(entry["id"], 0, 2)

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "requeued")
        self.assertEqual(updated["consecutive_failures"], 0)
        self.assertIsNone(updated["cooldown_until"])


class TestParseJsonField(unittest.TestCase):
    """Tests for _parse_json_field helper."""

    def test_dict_passthrough(self):
        """Dict values are returned as-is."""
        from lib.daemon_ops import _parse_json_field
        self.assertEqual(_parse_json_field({"a": 1}), {"a": 1})

    def test_list_passthrough(self):
        """List values are returned as-is."""
        from lib.daemon_ops import _parse_json_field
        self.assertEqual(_parse_json_field([1, 2]), [1, 2])

    def test_json_string_parsed(self):
        """JSON strings are parsed."""
        from lib.daemon_ops import _parse_json_field
        self.assertEqual(
            _parse_json_field('{"t-1": "DONE"}'), {"t-1": "DONE"}
        )

    def test_none_returns_default(self):
        """None returns the default value."""
        from lib.daemon_ops import _parse_json_field
        self.assertEqual(_parse_json_field(None, default={}), {})

    def test_invalid_json_returns_default(self):
        """Invalid JSON string returns the default."""
        from lib.daemon_ops import _parse_json_field
        self.assertEqual(_parse_json_field("not json", default={}), {})


# ─── DB-backed tests for phase handlers and self-heal ─────────────────────────

from lib.daemon_ops import (
    check_needs_review_timeouts,
    process_critic_completion,
    process_decomposition_completion,
    process_evaluation_completion,
    self_heal,
)
from lib.db import Database


class TestProcessCriticCompletionDB(DaemonOpsDBTestCase):
    """Tests for process_critic_completion() with Database backend."""

    def test_critic_approval_marks_completed(self):
        """Critic approved marks spec completed via db."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=2)
        entry = self._enqueue_and_run(spec_path)

        # Append Critic Approved to the copied spec
        copied_spec = self.db.get_spec(entry["id"])["spec_path"]
        content = Path(copied_spec).read_text()
        content += "\n## Critic Approved\n\n2026-03-09\n"
        Path(copied_spec).write_text(content)

        result = process_critic_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=copied_spec,
            db=self.db,
        )

        self.assertEqual(result["outcome"], "critic_approved")

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "completed")
        self.assertEqual(updated["critic_passes"], 1)

    def test_critic_tasks_added_requeues(self):
        """When critic adds tasks, spec is requeued via db."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=2)
        entry = self._enqueue_and_run(spec_path)

        # Add a CRITIC task to the spec
        copied_spec = self.db.get_spec(entry["id"])["spec_path"]
        content = Path(copied_spec).read_text()
        content += (
            "\n### t-99: [CRITIC] Fix issue\n"
            "PENDING\n\n"
            "**Spec:** Fix it.\n\n"
            "**Verify:** true\n"
        )
        Path(copied_spec).write_text(content)

        result = process_critic_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=copied_spec,
            db=self.db,
        )

        self.assertEqual(result["outcome"], "critic_tasks_added")
        self.assertEqual(result["critic_tasks_added"], 1)

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "requeued")
        self.assertEqual(updated["critic_passes"], 1)

    def test_critic_no_output_completes(self):
        """When critic has no valid output, completes via db."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=2)
        entry = self._enqueue_and_run(spec_path)

        copied_spec = self.db.get_spec(entry["id"])["spec_path"]

        result = process_critic_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=copied_spec,
            db=self.db,
        )

        self.assertEqual(result["outcome"], "critic_approved")
        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "completed")

    def test_critic_passes_increment_db(self):
        """critic_passes increments correctly via db."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=2)
        entry = self._enqueue_and_run(spec_path)

        # Set prior passes
        self.db.update_spec_fields(entry["id"], critic_passes=2)

        copied_spec = self.db.get_spec(entry["id"])["spec_path"]
        content = Path(copied_spec).read_text()
        content += "\n## Critic Approved\n"
        Path(copied_spec).write_text(content)

        process_critic_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=copied_spec,
            db=self.db,
        )

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["critic_passes"], 3)

    def test_missing_entry_returns_error(self):
        """When queue_id doesn't exist, returns error via db."""
        result = process_critic_completion(
            queue_dir=self.queue_dir,
            queue_id="q-999",
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path="/nonexistent",
            db=self.db,
        )

        self.assertEqual(result["outcome"], "error")


class TestProcessDecompositionCompletionDB(DaemonOpsDBTestCase):
    """Tests for process_decomposition_completion() with Database backend."""

    def _create_decomposed_spec(self, task_count=5):
        """Create a valid decomposed spec file."""
        if not hasattr(self, "_spec_counter"):
            self._spec_counter = 0
        self._spec_counter += 1
        spec_path = os.path.join(
            self._tmpdir.name, f"spec-{self._spec_counter}.md"
        )
        lines = ["# Decomposed Spec\n\n**Workspace:** in-place\n\n## Approach\n\nDo stuff.\n\n## Tasks\n"]
        for i in range(1, task_count + 1):
            lines.append(
                f"\n### t-{i}: Task {i}\n"
                "PENDING\n\n"
                f"**Spec:** Do task {i}.\n\n"
                "**Verify:** true\n"
            )
        Path(spec_path).write_text("".join(lines))
        return spec_path

    def test_valid_decomposition_transitions_to_execute(self):
        """Valid decomposition transitions phase to execute via db."""
        spec_path = self._create_decomposed_spec(task_count=5)
        entry = self.db.enqueue(spec_path)
        self.db.update_spec_fields(entry["id"], phase="decompose")
        # Pick and run
        spec = self.db.pick_next_spec()
        self.db.set_running(spec["id"], "w-1", phase="decompose")

        copied_spec = self.db.get_spec(entry["id"])["spec_path"]
        # Write a valid decomposed spec to the copied location
        decomposed = self._create_decomposed_spec(task_count=5)
        import shutil
        shutil.copy2(decomposed, copied_spec)

        result = process_decomposition_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            spec_path=copied_spec,
            exit_code="0",
            db=self.db,
        )

        self.assertEqual(result["outcome"], "decomposition_complete")
        self.assertEqual(result["phase"], "execute")
        self.assertEqual(result["task_count"], 5)

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["phase"], "execute")
        self.assertEqual(updated["status"], "requeued")

    def test_crash_retries_once(self):
        """Crash retries once, then fails on second crash via db."""
        spec_path = self._create_spec(tasks_pending=0)
        entry = self.db.enqueue(spec_path)
        self.db.update_spec_fields(entry["id"], phase="decompose")
        spec = self.db.pick_next_spec()
        self.db.set_running(spec["id"], "w-1", phase="decompose")

        copied_spec = self.db.get_spec(entry["id"])["spec_path"]

        # First crash — should retry
        result = process_decomposition_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            spec_path=copied_spec,
            exit_code=None,
            db=self.db,
        )

        self.assertEqual(result["outcome"], "decomposition_retry")
        self.assertEqual(result["retry_count"], 1)

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "requeued")
        self.assertEqual(updated["decomposition_retries"], 1)

    def test_second_crash_fails_permanently(self):
        """Second crash after retry fails permanently via db."""
        spec_path = self._create_spec(tasks_pending=0)
        entry = self.db.enqueue(spec_path)
        self.db.update_spec_fields(
            entry["id"], phase="decompose", decomposition_retries=1
        )
        spec = self.db.pick_next_spec()
        self.db.set_running(spec["id"], "w-1", phase="decompose")

        copied_spec = self.db.get_spec(entry["id"])["spec_path"]

        result = process_decomposition_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            spec_path=copied_spec,
            exit_code=None,
            db=self.db,
        )

        self.assertEqual(result["outcome"], "decomposition_failed")

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "failed")

    def test_missing_entry_returns_error(self):
        """When queue_id doesn't exist, returns error via db."""
        result = process_decomposition_completion(
            queue_dir=self.queue_dir,
            queue_id="q-999",
            events_dir=self.events_dir,
            spec_path="/nonexistent",
            exit_code="0",
            db=self.db,
        )

        self.assertEqual(result["outcome"], "error")


class TestProcessEvaluationCompletionDB(DaemonOpsDBTestCase):
    """Tests for process_evaluation_completion() with Database backend."""

    def test_crash_records_failure_via_db(self):
        """Crash records failure and requeues via db."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=2)
        entry = self._enqueue_and_run(spec_path)
        self.db.update_spec_fields(entry["id"], phase="evaluate")

        result = process_evaluation_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=self.db.get_spec(entry["id"])["spec_path"],
            exit_code=None,
            db=self.db,
        )

        self.assertEqual(result["outcome"], "evaluate_crashed")
        self.assertEqual(result.get("phase"), "evaluate")

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "requeued")

    def test_consecutive_crash_failures_fail_permanently(self):
        """Consecutive evaluation crashes fail permanently via db."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=2)
        entry = self._enqueue_and_run(spec_path)
        self.db.update_spec_fields(entry["id"], phase="evaluate")

        # Simulate 4 prior failures
        for _ in range(4):
            self.db.record_failure(entry["id"])

        result = process_evaluation_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=self.db.get_spec(entry["id"])["spec_path"],
            exit_code=None,
            db=self.db,
        )

        self.assertEqual(result["outcome"], "evaluate_crashed")
        self.assertIn("consecutive_failures", result.get("reason", ""))

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "failed")

    def test_missing_entry_returns_error(self):
        """When queue_id doesn't exist, returns error via db."""
        result = process_evaluation_completion(
            queue_dir=self.queue_dir,
            queue_id="q-999",
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path="/nonexistent",
            exit_code="0",
            db=self.db,
        )

        self.assertEqual(result["outcome"], "error")


class TestCheckNeedsReviewTimeoutsDB(DaemonOpsDBTestCase):
    """Tests for check_needs_review_timeouts() with Database backend."""

    def test_no_needs_review_specs(self):
        """Returns empty list when no specs need review via db."""
        spec_path = self._create_spec(tasks_pending=2)
        self.db.enqueue(spec_path)

        result = check_needs_review_timeouts(
            queue_dir=self.queue_dir,
            events_dir=self.events_dir,
            state_dir=self.boi_state,
            db=self.db,
        )

        self.assertEqual(result, [])

    def test_timed_out_spec_auto_rejected(self):
        """Spec in needs_review past timeout is auto-rejected via db."""
        from datetime import datetime, timedelta, timezone

        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = self._enqueue_and_run(spec_path)

        # Manually set needs_review with old timestamp
        old_time = (
            datetime.now(timezone.utc) - timedelta(hours=25)
        ).isoformat()
        self.db.update_spec_fields(
            entry["id"],
            status="needs_review",
            needs_review_since=old_time,
        )

        result = check_needs_review_timeouts(
            queue_dir=self.queue_dir,
            events_dir=self.events_dir,
            state_dir=self.boi_state,
            db=self.db,
        )

        self.assertEqual(len(result), 1)
        self.assertEqual(result[0], entry["id"])

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "requeued")
        self.assertIsNone(updated["needs_review_since"])
        self.assertIsNone(updated["experiment_tasks"])

    def test_not_timed_out_spec_untouched(self):
        """Spec in needs_review within timeout is not rejected via db."""
        from datetime import datetime, timedelta, timezone

        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = self._enqueue_and_run(spec_path)

        # Set needs_review with recent timestamp
        recent_time = (
            datetime.now(timezone.utc) - timedelta(hours=1)
        ).isoformat()
        self.db.update_spec_fields(
            entry["id"],
            status="needs_review",
            needs_review_since=recent_time,
        )

        result = check_needs_review_timeouts(
            queue_dir=self.queue_dir,
            events_dir=self.events_dir,
            state_dir=self.boi_state,
            db=self.db,
        )

        self.assertEqual(result, [])

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "needs_review")


class TestSelfHealDB(DaemonOpsDBTestCase):
    """Tests for self_heal() and _heal_* functions with Database backend."""

    def test_empty_queue_no_actions(self):
        """Self-heal on empty queue produces no actions via db."""
        actions = self_heal(
            queue_dir=self.queue_dir,
            worker_specs={},
            db=self.db,
        )
        self.assertEqual(actions, [])

    def test_orphaned_worker_detected_via_db(self):
        """Worker assigned to completed spec detected as orphaned via db."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=2)
        entry = self._enqueue_and_run(spec_path)
        self.db.complete(entry["id"], 2, 2)

        actions = self_heal(
            queue_dir=self.queue_dir,
            worker_specs={"w-1": entry["id"]},
            db=self.db,
        )

        orphan_actions = [a for a in actions if a["action"] == "orphaned_worker"]
        self.assertEqual(len(orphan_actions), 1)
        self.assertEqual(orphan_actions[0]["queue_id"], entry["id"])
        self.assertEqual(orphan_actions[0]["worker_id"], "w-1")

    def test_orphaned_worker_missing_spec_via_db(self):
        """Worker assigned to nonexistent spec detected via db."""
        actions = self_heal(
            queue_dir=self.queue_dir,
            worker_specs={"w-1": "q-nonexistent"},
            db=self.db,
        )

        orphan_actions = [a for a in actions if a["action"] == "orphaned_worker"]
        self.assertEqual(len(orphan_actions), 1)
        self.assertEqual(orphan_actions[0]["queue_id"], "q-nonexistent")

    def test_idle_workers_no_false_orphans_via_db(self):
        """Idle workers (empty spec) not reported as orphaned via db."""
        actions = self_heal(
            queue_dir=self.queue_dir,
            worker_specs={"w-1": "", "w-2": ""},
            db=self.db,
        )

        orphan_actions = [a for a in actions if a["action"] == "orphaned_worker"]
        self.assertEqual(len(orphan_actions), 0)

    def test_max_running_duration_exceeded_via_db(self):
        """Spec running too long is force-failed via db."""
        from datetime import datetime, timedelta, timezone

        spec_path = self._create_spec(tasks_pending=2)
        entry = self._enqueue_and_run(spec_path)

        # Set first_running_at to long ago (force exceed the limit)
        old_time = (
            datetime.now(timezone.utc) - timedelta(hours=100)
        ).isoformat()
        self.db.conn.execute(
            "UPDATE specs SET first_running_at = ? WHERE id = ?",
            (old_time, entry["id"]),
        )
        self.db.conn.commit()

        actions = self_heal(
            queue_dir=self.queue_dir,
            worker_specs={},
            db=self.db,
        )

        max_dur_actions = [
            a for a in actions
            if a["action"] == "max_running_duration_exceeded"
        ]
        self.assertEqual(len(max_dur_actions), 1)

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "failed")

    def test_max_running_duration_not_exceeded_via_db(self):
        """Spec running within limit is not force-failed via db."""
        spec_path = self._create_spec(tasks_pending=2)
        entry = self._enqueue_and_run(spec_path)

        actions = self_heal(
            queue_dir=self.queue_dir,
            worker_specs={},
            db=self.db,
        )

        max_dur_actions = [
            a for a in actions
            if a["action"] == "max_running_duration_exceeded"
        ]
        self.assertEqual(len(max_dur_actions), 0)

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "running")


class TestUpdateSpecFieldsDB(DaemonOpsDBTestCase):
    """Tests for Database.update_spec_fields()."""

    def test_update_single_field(self):
        """Update a single field."""
        spec_path = self._create_spec(tasks_pending=2)
        entry = self.db.enqueue(spec_path)

        self.db.update_spec_fields(entry["id"], phase="critic")

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["phase"], "critic")

    def test_update_multiple_fields(self):
        """Update multiple fields at once."""
        spec_path = self._create_spec(tasks_pending=2)
        entry = self.db.enqueue(spec_path)

        self.db.update_spec_fields(
            entry["id"],
            phase="evaluate",
            critic_passes=3,
            tasks_done=5,
        )

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["phase"], "evaluate")
        self.assertEqual(updated["critic_passes"], 3)
        self.assertEqual(updated["tasks_done"], 5)

    def test_invalid_column_raises(self):
        """Updating an invalid column raises ValueError."""
        spec_path = self._create_spec(tasks_pending=2)
        entry = self.db.enqueue(spec_path)

        with self.assertRaises(ValueError):
            self.db.update_spec_fields(
                entry["id"], nonexistent_column="value"
            )

    def test_missing_spec_raises(self):
        """Updating a nonexistent spec raises ValueError."""
        with self.assertRaises(ValueError):
            self.db.update_spec_fields("q-nonexistent", phase="critic")

    def test_update_no_fields_is_noop(self):
        """Calling with no fields is a no-op."""
        spec_path = self._create_spec(tasks_pending=2)
        entry = self.db.enqueue(spec_path)

        # Should not raise
        self.db.update_spec_fields(entry["id"])
