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

from lib.daemon_ops import get_active_count, pick_next_spec, process_worker_completion
from lib.queue import enqueue, get_entry, set_running


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

    def tearDown(self):
        self._tmpdir.cleanup()

    def _create_spec(self, tasks_pending=3, tasks_done=0, tasks_skipped=0):
        """Create a temp spec.md file with the given task counts."""
        if not hasattr(self, "_spec_counter"):
            self._spec_counter = 0
        self._spec_counter += 1
        spec_path = os.path.join(self._tmpdir.name, f"spec-{self._spec_counter}.md")
        lines = ["# Test Spec\n\n## Tasks\n"]
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
        entry = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, entry["id"], "w-1")

        result = process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "completed")
        self.assertEqual(result["pending_count"], 0)
        self.assertEqual(result["done_count"], 3)

        updated = get_entry(self.queue_dir, entry["id"])
        self.assertEqual(updated["status"], "completed")

    def test_pending_tasks_requeues(self):
        """When pending tasks remain, outcome is 'requeued'."""
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, entry["id"], "w-1")

        result = process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "requeued")
        self.assertEqual(result["pending_count"], 2)

        updated = get_entry(self.queue_dir, entry["id"])
        self.assertEqual(updated["status"], "requeued")

    def test_max_iterations_fails(self):
        """When max iterations reached with pending tasks, outcome is 'failed'."""
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = enqueue(self.queue_dir, spec_path, max_iterations=1)
        set_running(self.queue_dir, entry["id"], "w-1")

        result = process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "failed")
        self.assertEqual(result["reason"], "max_iterations")

        updated = get_entry(self.queue_dir, entry["id"])
        self.assertEqual(updated["status"], "failed")

    def test_no_exit_code_crashes(self):
        """When exit_code is None (no exit file), outcome is 'crashed'."""
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, entry["id"], "w-1")

        result = process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code=None,
        )

        self.assertEqual(result["outcome"], "crashed")

        updated = get_entry(self.queue_dir, entry["id"])
        self.assertEqual(updated["status"], "requeued")
        self.assertEqual(updated["consecutive_failures"], 1)

    def test_nonzero_exit_code_crashes(self):
        """When exit_code is non-zero, outcome is 'crashed'."""
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, entry["id"], "w-1")

        result = process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code="1",
        )

        self.assertEqual(result["outcome"], "crashed")

    def test_consecutive_failures_exceed_threshold(self):
        """When too many consecutive failures, outcome is 'failed'."""
        spec_path = self._create_spec(tasks_pending=2)
        entry = enqueue(self.queue_dir, spec_path)

        # Simulate 4 prior failures
        from lib.queue import _read_entry, _write_entry

        e = _read_entry(self.queue_dir, entry["id"])
        e["consecutive_failures"] = 4
        _write_entry(self.queue_dir, e)

        set_running(self.queue_dir, entry["id"], "w-1")

        result = process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code=None,
        )

        self.assertEqual(result["outcome"], "failed")
        self.assertEqual(result["reason"], "consecutive_failures")

    def test_writes_events(self):
        """process_worker_completion writes events to events_dir."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=2)
        entry = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, entry["id"], "w-1")

        process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
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
        entry = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, entry["id"], "w-1")

        process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code="0",
        )

        telemetry_file = Path(self.queue_dir) / f"{entry['id']}.telemetry.json"
        self.assertTrue(telemetry_file.exists())

    def test_missing_queue_entry_returns_error(self):
        """When queue entry doesn't exist, returns error outcome."""
        result = process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id="q-999",
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "error")

    def test_runs_hooks_on_completion(self):
        """Hooks are called on completion."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=1)
        entry = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, entry["id"], "w-1")

        # Create a hook that writes a marker file
        marker = os.path.join(self._tmpdir.name, "hook_ran")
        hook_path = os.path.join(self.hooks_dir, "on-complete.sh")
        Path(hook_path).write_text(f"#!/bin/bash\ntouch {marker}\n")
        os.chmod(hook_path, 0o755)

        process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code="0",
        )

        self.assertTrue(os.path.exists(marker))

    def test_runs_fail_hook_on_failure(self):
        """on-fail hook runs when spec fails."""
        spec_path = self._create_spec(tasks_pending=2)
        entry = enqueue(self.queue_dir, spec_path, max_iterations=1)
        set_running(self.queue_dir, entry["id"], "w-1")

        marker = os.path.join(self._tmpdir.name, "fail_hook_ran")
        hook_path = os.path.join(self.hooks_dir, "on-fail.sh")
        Path(hook_path).write_text(f"#!/bin/bash\ntouch {marker}\n")
        os.chmod(hook_path, 0o755)

        process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code="0",
        )

        self.assertTrue(os.path.exists(marker))


class TestPickNextSpec(DaemonOpsTestCase):
    """Tests for pick_next_spec()."""

    def test_returns_none_when_empty(self):
        """Returns None when queue is empty."""
        result = pick_next_spec(self.queue_dir)
        self.assertIsNone(result)

    def test_returns_queued_spec(self):
        """Returns the next queued spec."""
        spec_path = self._create_spec()
        entry = enqueue(self.queue_dir, spec_path)

        result = pick_next_spec(self.queue_dir)
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], entry["id"])
        self.assertEqual(result["spec_path"], entry["spec_path"])

    def test_returns_highest_priority(self):
        """Returns the highest-priority spec (lowest priority number)."""
        spec1 = self._create_spec()
        spec2 = os.path.join(self._tmpdir.name, "spec2.md")
        Path(spec2).write_text(Path(spec1).read_text())

        enqueue(self.queue_dir, spec1, priority=200)
        entry2 = enqueue(self.queue_dir, spec2, priority=50)

        result = pick_next_spec(self.queue_dir)
        self.assertEqual(result["id"], entry2["id"])

    def test_skips_running_specs(self):
        """Does not return specs that are already running."""
        spec_path = self._create_spec()
        entry = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, entry["id"], "w-1")

        result = pick_next_spec(self.queue_dir)
        self.assertIsNone(result)

    def test_returns_requeued_spec(self):
        """Returns requeued specs."""
        from lib.queue import requeue

        spec_path = self._create_spec()
        entry = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, entry["id"], "w-1")
        requeue(self.queue_dir, entry["id"])

        result = pick_next_spec(self.queue_dir)
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], entry["id"])


class TestGetActiveCount(DaemonOpsTestCase):
    """Tests for get_active_count()."""

    def test_empty_queue(self):
        """Returns 0 for empty queue."""
        self.assertEqual(get_active_count(self.queue_dir), 0)

    def test_counts_active_statuses(self):
        """Counts queued, requeued, running specs."""
        spec_path = self._create_spec()
        enqueue(self.queue_dir, spec_path)  # queued
        self.assertEqual(get_active_count(self.queue_dir), 1)

    def test_excludes_terminal_statuses(self):
        """Does not count completed, failed, canceled specs."""
        from lib.queue import cancel, complete, fail

        e1 = enqueue(self.queue_dir, self._create_spec())
        e2 = enqueue(self.queue_dir, self._create_spec())
        e3 = enqueue(self.queue_dir, self._create_spec())
        e4 = enqueue(self.queue_dir, self._create_spec())

        complete(self.queue_dir, e1["id"])
        fail(self.queue_dir, e2["id"], "test")
        cancel(self.queue_dir, e3["id"])
        # e4 remains queued

        self.assertEqual(get_active_count(self.queue_dir), 1)


class TestBatchedCallCount(DaemonOpsTestCase):
    """Verify that the batched approach reduces Python calls."""

    def test_completion_is_single_call(self):
        """process_worker_completion does everything in one function call.

        Before the optimization, check_worker_completion in daemon.sh
        would make 5-10 separate Python invocations per cycle. Now it
        makes a single call to process_worker_completion.
        """
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, entry["id"], "w-1")

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
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code="0",
        )

        # Verify it handled everything in one call
        self.assertIn(result["outcome"], ("completed", "requeued", "failed", "crashed"))
        # Verify queue entry was updated
        updated = get_entry(self.queue_dir, entry["id"])
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
        entry = enqueue(self.queue_dir, valid_spec)
        set_running(self.queue_dir, entry["id"], "w-1")

        # Now replace the copied spec with a malformed one
        copied_spec = get_entry(self.queue_dir, entry["id"])["spec_path"]
        malformed_content = (
            "# Test Spec\n\n## Tasks\n\n"
            "### t-1: Malformed task\n"
            "PENDING\n\n"
            "No spec section here.\n"
            "**Verify:** true\n"
        )
        Path(copied_spec).write_text(malformed_content)

        result = process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "validation_failed")
        self.assertIn("validation_errors", result)
        self.assertGreater(len(result["validation_errors"]), 0)

        # Should have recorded a failure
        updated = get_entry(self.queue_dir, entry["id"])
        self.assertGreater(updated["consecutive_failures"], 0)

    def test_valid_spec_passes_validation(self):
        """When spec is valid, proceed normally (requeued or completed)."""
        spec_path = self._create_spec(tasks_pending=2, tasks_done=1)
        entry = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, entry["id"], "w-1")

        result = process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code="0",
        )

        # Should proceed normally, not validation_failed
        self.assertIn(result["outcome"], ("completed", "requeued"))

    def test_done_to_pending_regression_detected(self):
        """Detect when a task regresses from DONE to PENDING."""
        # Create spec with a DONE task and a PENDING task
        spec_path = self._create_spec(tasks_pending=1, tasks_done=1)
        entry = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, entry["id"], "w-1")

        # Verify pre_iteration_tasks was saved
        updated_entry = get_entry(self.queue_dir, entry["id"])
        self.assertIn("pre_iteration_tasks", updated_entry)
        # t-1 should be DONE, t-2 should be PENDING
        self.assertEqual(updated_entry["pre_iteration_tasks"]["t-1"], "DONE")
        self.assertEqual(updated_entry["pre_iteration_tasks"]["t-2"], "PENDING")

        # Now modify the copied spec to regress t-1 from DONE to PENDING
        copied_spec = updated_entry["spec_path"]
        regressed_content = (
            "# Test Spec\n\n## Tasks\n\n"
            "### t-1: Done task 1\nPENDING\n\n"
            "**Spec:** Did it.\n**Verify:** true\n\n"
            "### t-2: Pending task 2\nPENDING\n\n"
            "**Spec:** Do it.\n**Verify:** true\n"
        )
        Path(copied_spec).write_text(regressed_content)

        result = process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
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
        entry = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, entry["id"], "w-1")

        # Create iteration metadata file
        iteration = get_entry(self.queue_dir, entry["id"])["iteration"]
        iter_meta_path = os.path.join(
            self.queue_dir, f"{entry['id']}.iteration-{iteration}.json"
        )
        Path(iter_meta_path).write_text(json.dumps({"iteration": iteration}) + "\n")

        # Corrupt the spec
        copied_spec = get_entry(self.queue_dir, entry["id"])["spec_path"]
        Path(copied_spec).write_text(
            "# Test Spec\n\n## Tasks\n\n"
            "### t-1: Bad task\nPENDING\n\n"
            "No spec section.\n**Verify:** true\n"
        )

        process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
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
        entry = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, entry["id"], "w-1")

        result = process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "critic_review")
        self.assertIn("critic_prompt_path", result)
        self.assertEqual(result["critic_pass"], 1)

        # Spec should be requeued (so daemon picks it up for critic worker)
        updated = get_entry(self.queue_dir, entry["id"])
        self.assertEqual(updated["status"], "requeued")

    def test_critic_pass_counting(self):
        """critic_passes increments with each critic review."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=3)
        entry = enqueue(self.queue_dir, spec_path)

        # Simulate one prior critic pass
        from lib.queue import _read_entry, _write_entry

        e = _read_entry(self.queue_dir, entry["id"])
        e["critic_passes"] = 1
        _write_entry(self.queue_dir, e)

        set_running(self.queue_dir, entry["id"], "w-1")

        result = process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "critic_review")
        self.assertEqual(result["critic_pass"], 2)

    def test_max_passes_enforcement(self):
        """When max_passes reached, completes without critic."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=3)
        entry = enqueue(self.queue_dir, spec_path)

        # Set critic_passes to match max_passes
        from lib.queue import _read_entry, _write_entry

        e = _read_entry(self.queue_dir, entry["id"])
        e["critic_passes"] = 2  # max_passes is 2
        _write_entry(self.queue_dir, e)

        set_running(self.queue_dir, entry["id"], "w-1")

        result = process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "completed")
        updated = get_entry(self.queue_dir, entry["id"])
        self.assertEqual(updated["status"], "completed")

    def test_critic_disabled_skips_review(self):
        """When critic is disabled, completes without review."""
        # Disable critic
        config = {"enabled": False}
        config_path = os.path.join(self.critic_dir, "config.json")
        Path(config_path).write_text(json.dumps(config, indent=2) + "\n")

        spec_path = self._create_spec(tasks_pending=0, tasks_done=3)
        entry = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, entry["id"], "w-1")

        result = process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "completed")

    def test_critic_writes_event(self):
        """Critic review trigger writes an event."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=2)
        entry = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, entry["id"], "w-1")

        process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
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
        entry = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, entry["id"], "w-1")

        result = process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
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
        entry = enqueue(self.queue_dir, spec_path)

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
        )

        self.assertEqual(result["outcome"], "critic_approved")

        updated = get_entry(self.queue_dir, entry["id"])
        self.assertEqual(updated["status"], "completed")
        self.assertEqual(updated["critic_passes"], 1)

    def test_critic_task_injection(self):
        """When critic adds [CRITIC] tasks, spec is requeued."""
        from lib.daemon_ops import process_critic_completion

        spec_path = self._create_spec(tasks_pending=0, tasks_done=2)
        entry = enqueue(self.queue_dir, spec_path)

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
        )

        self.assertEqual(result["outcome"], "critic_tasks_added")
        self.assertEqual(result["critic_tasks_added"], 1)

        updated = get_entry(self.queue_dir, entry["id"])
        self.assertEqual(updated["status"], "requeued")
        self.assertEqual(updated["critic_passes"], 1)

    def test_critic_no_output_completes(self):
        """When critic produces no valid output, completes anyway."""
        from lib.daemon_ops import process_critic_completion

        spec_path = self._create_spec(tasks_pending=0, tasks_done=2)
        entry = enqueue(self.queue_dir, spec_path)

        # Spec has no Critic Approved and no CRITIC tasks
        copied_spec = entry["spec_path"]

        result = process_critic_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=copied_spec,
        )

        self.assertEqual(result["outcome"], "critic_approved")
        updated = get_entry(self.queue_dir, entry["id"])
        self.assertEqual(updated["status"], "completed")

    def test_critic_passes_increment(self):
        """critic_passes is incremented on each call."""
        from lib.daemon_ops import process_critic_completion
        from lib.queue import _read_entry, _write_entry

        spec_path = self._create_spec(tasks_pending=0, tasks_done=2)
        entry = enqueue(self.queue_dir, spec_path)

        # Set prior critic passes
        e = _read_entry(self.queue_dir, entry["id"])
        e["critic_passes"] = 1
        _write_entry(self.queue_dir, e)

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
        )

        updated = get_entry(self.queue_dir, entry["id"])
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
        from lib.queue import _read_entry, _write_entry

        spec_path = self._create_spec(tasks_pending=0, tasks_done=3)
        entry = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, entry["id"], "w-1")

        # Step 1: Process worker completion — should return critic_review
        result = process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code="0",
        )
        self.assertEqual(result["outcome"], "critic_review")

        # Step 2: Simulate daemon setting phase to "critic" (as daemon.sh now does)
        e = _read_entry(self.queue_dir, entry["id"])
        e["phase"] = "critic"
        _write_entry(self.queue_dir, e)
        set_running(self.queue_dir, entry["id"], "w-1")

        # Step 3: Simulate critic worker completing with approval
        # Add "## Critic Approved" to spec
        content = Path(spec_path).read_text()
        content += "\n## Critic Approved\n\nAll checks passed.\n"
        Path(spec_path).write_text(content)

        from lib.daemon_ops import process_critic_completion

        critic_result = process_critic_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=spec_path,
        )
        self.assertEqual(critic_result["outcome"], "critic_approved")

        # Verify spec is now completed
        updated = get_entry(self.queue_dir, entry["id"])
        self.assertEqual(updated["status"], "completed")

    def test_critic_review_requeues_for_critic_worker(self):
        """critic_review outcome requeues spec so daemon can launch critic worker."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=3)
        entry = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, entry["id"], "w-1")

        result = process_worker_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            log_dir=self.log_dir,
            hooks_dir=self.hooks_dir,
            script_dir=self.script_dir,
            exit_code="0",
        )

        self.assertEqual(result["outcome"], "critic_review")

        # Verify prompt file exists for critic worker
        prompt_path = result.get("critic_prompt_path", "")
        self.assertTrue(os.path.isfile(prompt_path))

        # Entry should be requeued (daemon will pick it up and launch critic)
        updated = get_entry(self.queue_dir, entry["id"])
        self.assertEqual(updated["status"], "requeued")


class TestSelfHeal(DaemonOpsTestCase):
    """Tests for self_heal() and its sub-functions."""

    def test_stale_running_spec_recovered(self):
        """Running spec with dead PID should be reset to requeued."""
        from lib.daemon_ops import self_heal

        spec_path = self._create_spec(tasks_pending=2)
        entry = enqueue(self.queue_dir, spec_path)
        qid = entry["id"]
        set_running(self.queue_dir, qid, "w-1")

        # Write a PID file with a definitely-dead PID
        pid_file = os.path.join(self.queue_dir, f"{qid}.pid")
        Path(pid_file).write_text("999999999\n")

        actions = self_heal(self.queue_dir, {"w-1": qid})

        # Should have recovered the spec
        stale_actions = [a for a in actions if a["action"] == "stale_running_recovered"]
        self.assertEqual(len(stale_actions), 1)
        self.assertIn(qid, stale_actions[0]["detail"])

        # Entry should now be requeued
        updated = get_entry(self.queue_dir, qid)
        self.assertEqual(updated["status"], "requeued")

        # PID file should be cleaned up
        self.assertFalse(os.path.isfile(pid_file))

    def test_stale_running_no_pid_file(self):
        """Running spec with no PID file at all should be recovered."""
        from lib.daemon_ops import self_heal

        spec_path = self._create_spec(tasks_pending=2)
        entry = enqueue(self.queue_dir, spec_path)
        qid = entry["id"]
        set_running(self.queue_dir, qid, "w-1")

        # No PID file exists at all
        actions = self_heal(self.queue_dir, {"w-1": qid})

        stale_actions = [a for a in actions if a["action"] == "stale_running_recovered"]
        self.assertEqual(len(stale_actions), 1)

        updated = get_entry(self.queue_dir, qid)
        self.assertEqual(updated["status"], "requeued")

    def test_orphaned_worker_freed(self):
        """Worker assigned to completed spec should be reported as orphaned."""
        from lib.daemon_ops import self_heal
        from lib.queue import complete

        spec_path = self._create_spec(tasks_pending=0, tasks_done=3)
        entry = enqueue(self.queue_dir, spec_path)
        qid = entry["id"]
        set_running(self.queue_dir, qid, "w-1")
        complete(self.queue_dir, qid, 3, 3)

        # Worker still thinks it's assigned to this spec
        worker_specs = {"w-1": qid, "w-2": ""}
        actions = self_heal(self.queue_dir, worker_specs)

        orphan_actions = [a for a in actions if a["action"] == "orphaned_worker"]
        self.assertEqual(len(orphan_actions), 1)
        self.assertEqual(orphan_actions[0]["worker_id"], "w-1")
        self.assertEqual(orphan_actions[0]["queue_id"], qid)

    def test_orphaned_worker_missing_spec(self):
        """Worker assigned to nonexistent spec should be reported as orphaned."""
        from lib.daemon_ops import self_heal

        worker_specs = {"w-1": "q-nonexistent", "w-2": ""}
        actions = self_heal(self.queue_dir, worker_specs)

        orphan_actions = [a for a in actions if a["action"] == "orphaned_worker"]
        self.assertEqual(len(orphan_actions), 1)
        self.assertIn("missing", orphan_actions[0]["detail"])

    def test_blocked_by_completed_spec_unblocked(self):
        """Spec blocked by a completed spec should have that dep removed."""
        from lib.daemon_ops import self_heal
        from lib.queue import complete

        # Create blocker spec and complete it
        blocker_path = self._create_spec(tasks_pending=0, tasks_done=1)
        blocker = enqueue(self.queue_dir, blocker_path)
        set_running(self.queue_dir, blocker["id"], "w-1")
        complete(self.queue_dir, blocker["id"], 1, 1)

        # Create blocked spec
        blocked_path = self._create_spec(tasks_pending=2)
        blocked = enqueue(self.queue_dir, blocked_path, blocked_by=[blocker["id"]])

        # Verify it's blocked
        entry = get_entry(self.queue_dir, blocked["id"])
        self.assertEqual(entry["blocked_by"], [blocker["id"]])

        actions = self_heal(self.queue_dir, {})

        cleanup_actions = [a for a in actions if a["action"] == "blocked_by_cleaned"]
        self.assertEqual(len(cleanup_actions), 1)

        updated = get_entry(self.queue_dir, blocked["id"])
        self.assertEqual(updated["blocked_by"], [])

    def test_blocked_by_missing_spec_unblocked(self):
        """Spec blocked by a nonexistent spec should have that dep removed."""
        from lib.daemon_ops import self_heal
        from lib.queue import _read_entry, _write_entry

        spec_path = self._create_spec(tasks_pending=2)
        entry = enqueue(self.queue_dir, spec_path)
        qid = entry["id"]

        # Manually set blocked_by to a nonexistent spec
        raw = _read_entry(self.queue_dir, qid)
        raw["blocked_by"] = ["q-999"]
        _write_entry(self.queue_dir, raw)

        actions = self_heal(self.queue_dir, {})

        cleanup_actions = [a for a in actions if a["action"] == "blocked_by_cleaned"]
        self.assertEqual(len(cleanup_actions), 1)
        self.assertIn("missing", cleanup_actions[0]["detail"])

        updated = get_entry(self.queue_dir, qid)
        self.assertEqual(updated["blocked_by"], [])

    def test_circular_dependency_detected(self):
        """Circular dependency A->B->C->A should cancel all specs in cycle."""
        from lib.daemon_ops import self_heal
        from lib.queue import _read_entry, _write_entry

        # Create 3 specs
        specs = []
        for _ in range(3):
            path = self._create_spec(tasks_pending=1)
            specs.append(enqueue(self.queue_dir, path))

        a_id, b_id, c_id = specs[0]["id"], specs[1]["id"], specs[2]["id"]

        # Create cycle: A blocked by C, B blocked by A, C blocked by B
        for qid, dep in [(a_id, c_id), (b_id, a_id), (c_id, b_id)]:
            raw = _read_entry(self.queue_dir, qid)
            raw["blocked_by"] = [dep]
            _write_entry(self.queue_dir, raw)

        actions = self_heal(self.queue_dir, {})

        cycle_actions = [
            a for a in actions if a["action"] == "circular_dependency_canceled"
        ]
        # All 3 should be canceled
        self.assertEqual(len(cycle_actions), 3)

        for spec in specs:
            updated = get_entry(self.queue_dir, spec["id"])
            self.assertEqual(updated["status"], "canceled")
            self.assertIn("Circular", updated.get("failure_reason", ""))

    def test_stale_lock_no_action_when_no_lock(self):
        """No lock file should produce no action."""
        from lib.daemon_ops import self_heal

        actions = self_heal(self.queue_dir, {})

        lock_actions = [a for a in actions if "lock" in a.get("action", "")]
        self.assertEqual(len(lock_actions), 0)

    def test_idle_workers_no_false_orphans(self):
        """Idle workers (empty spec assignment) should NOT be reported as orphaned."""
        from lib.daemon_ops import self_heal

        worker_specs = {"w-1": "", "w-2": ""}
        actions = self_heal(self.queue_dir, worker_specs)

        orphan_actions = [a for a in actions if a["action"] == "orphaned_worker"]
        self.assertEqual(len(orphan_actions), 0)

    def test_self_heal_multiple_issues(self):
        """Self-heal should fix multiple issues in a single call."""
        from lib.daemon_ops import self_heal
        from lib.queue import _read_entry, _write_entry, complete

        # Issue 1: stale running spec
        spec1_path = self._create_spec(tasks_pending=2)
        entry1 = enqueue(self.queue_dir, spec1_path)
        set_running(self.queue_dir, entry1["id"], "w-1")
        pid_file = os.path.join(self.queue_dir, f"{entry1['id']}.pid")
        Path(pid_file).write_text("999999999\n")

        # Issue 2: blocked by missing spec
        spec2_path = self._create_spec(tasks_pending=1)
        entry2 = enqueue(self.queue_dir, spec2_path)
        raw2 = _read_entry(self.queue_dir, entry2["id"])
        raw2["blocked_by"] = ["q-missing"]
        _write_entry(self.queue_dir, raw2)

        # Issue 3: orphaned worker
        spec3_path = self._create_spec(tasks_pending=0, tasks_done=1)
        entry3 = enqueue(self.queue_dir, spec3_path)
        set_running(self.queue_dir, entry3["id"], "w-2")
        complete(self.queue_dir, entry3["id"], 1, 1)

        worker_specs = {"w-1": entry1["id"], "w-2": entry3["id"]}
        actions = self_heal(self.queue_dir, worker_specs)

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
        from lib.queue import _read_entry, _write_entry

        spec_path = self._create_spec(tasks_pending=2)
        entry = enqueue(self.queue_dir, spec_path)
        qid = entry["id"]
        set_running(self.queue_dir, qid, "w-1")

        # Manually backdate first_running_at to exceed the max duration
        e = _read_entry(self.queue_dir, qid)
        # Set worker_timeout_seconds=60, max_iterations=5 -> max_duration=300s
        e["worker_timeout_seconds"] = 60
        e["max_iterations"] = 5
        # Set first_running_at to 10 minutes ago (600s > 300s limit)
        e["first_running_at"] = (
            datetime.now(timezone.utc) - timedelta(seconds=600)
        ).isoformat()
        _write_entry(self.queue_dir, e)

        # Write a PID file with a live PID (our own) so stale_running doesn't
        # trigger first
        pid_file = os.path.join(self.queue_dir, f"{qid}.pid")
        Path(pid_file).write_text(str(os.getpid()) + "\n")

        actions = self_heal(self.queue_dir, {"w-1": qid})

        # Should have force-failed
        duration_actions = [
            a for a in actions if a["action"] == "max_running_duration_exceeded"
        ]
        self.assertEqual(len(duration_actions), 1)
        self.assertIn(qid, duration_actions[0]["detail"])

        # Entry should now be failed with the right reason
        updated = get_entry(self.queue_dir, qid)
        self.assertEqual(updated["status"], "failed")
        self.assertEqual(updated["failure_reason"], "Maximum running duration exceeded")

        # PID file should be cleaned up
        self.assertFalse(os.path.isfile(pid_file))

    def test_max_running_duration_not_exceeded(self):
        """Spec running within max duration should NOT be force-failed."""
        from datetime import datetime, timedelta, timezone

        from lib.daemon_ops import self_heal
        from lib.queue import _read_entry, _write_entry

        spec_path = self._create_spec(tasks_pending=2)
        entry = enqueue(self.queue_dir, spec_path)
        qid = entry["id"]
        set_running(self.queue_dir, qid, "w-1")

        # Set first_running_at to just 10 seconds ago (well within default limit)
        e = _read_entry(self.queue_dir, qid)
        e["first_running_at"] = (
            datetime.now(timezone.utc) - timedelta(seconds=10)
        ).isoformat()
        _write_entry(self.queue_dir, e)

        # Write a live PID so stale_running doesn't trigger
        pid_file = os.path.join(self.queue_dir, f"{qid}.pid")
        Path(pid_file).write_text(str(os.getpid()) + "\n")

        actions = self_heal(self.queue_dir, {"w-1": qid})

        # Should NOT have any max_running_duration actions
        duration_actions = [
            a for a in actions if a["action"] == "max_running_duration_exceeded"
        ]
        self.assertEqual(len(duration_actions), 0)

        # Entry should still be running
        updated = get_entry(self.queue_dir, qid)
        self.assertEqual(updated["status"], "running")

    def test_first_running_at_set_on_first_run(self):
        """first_running_at should be set when spec first enters running status."""
        spec_path = self._create_spec(tasks_pending=2)
        entry = enqueue(self.queue_dir, spec_path)
        qid = entry["id"]

        # Before running, no first_running_at
        pre = get_entry(self.queue_dir, qid)
        self.assertNotIn("first_running_at", pre)

        # After first set_running, first_running_at should be set
        set_running(self.queue_dir, qid, "w-1")
        post = get_entry(self.queue_dir, qid)
        self.assertIn("first_running_at", post)
        first_time = post["first_running_at"]

        # After requeue and second set_running, first_running_at should be preserved
        from lib.queue import _read_entry, _write_entry

        e = _read_entry(self.queue_dir, qid)
        e["status"] = "requeued"
        _write_entry(self.queue_dir, e)

        set_running(self.queue_dir, qid, "w-2")
        post2 = get_entry(self.queue_dir, qid)
        self.assertEqual(post2["first_running_at"], first_time)


if __name__ == "__main__":
    unittest.main()
