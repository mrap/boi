# test_hooks.py — Unit tests for BOI integration hooks and lifecycle events.
#
# Tests the hooks module: lifecycle event writing, hook script execution,
# event schema validation, and graceful behavior when hooks are absent.
#
# Uses stdlib unittest only (no pytest dependency).

import json
import os
import stat
import sys
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

# Add parent directory to path so we can import lib modules
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.hooks import (
    get_tasks_added_from_telemetry,
    list_hooks,
    run_completion_hooks,
    run_hook,
    write_lifecycle_event,
    write_spec_completed_event,
    write_spec_failed_event,
)


class HooksTestCase(unittest.TestCase):
    """Base test case with temp dirs for events and hooks."""

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.boi_state = self._tmpdir.name
        self.events_dir = os.path.join(self.boi_state, "events")
        self.hooks_dir = os.path.join(self.boi_state, "hooks")
        self.queue_dir = os.path.join(self.boi_state, "queue")
        os.makedirs(self.events_dir)
        os.makedirs(self.hooks_dir)
        os.makedirs(self.queue_dir)

    def tearDown(self):
        self._tmpdir.cleanup()

    def _create_hook_script(self, name, script_body="exit 0"):
        """Create an executable hook script in the hooks directory."""
        hook_path = os.path.join(self.hooks_dir, f"{name}.sh")
        content = f"#!/bin/bash\n{script_body}\n"
        with open(hook_path, "w") as f:
            f.write(content)
        os.chmod(hook_path, stat.S_IRWXU)
        return hook_path

    def _read_event(self, event_path):
        """Read and parse an event JSON file."""
        with open(event_path) as f:
            return json.load(f)

    def _read_all_events(self):
        """Read all event files from the events directory."""
        events = []
        for f in sorted(os.listdir(self.events_dir)):
            if f.startswith("event-") and f.endswith(".json"):
                path = os.path.join(self.events_dir, f)
                events.append(self._read_event(path))
        return events

    def _create_telemetry_file(self, queue_id, tasks_added_per_iter):
        """Create a telemetry JSON file in the queue directory."""
        data = {
            "queue_id": queue_id,
            "total_iterations": len(tasks_added_per_iter),
            "tasks_added_per_iteration": tasks_added_per_iter,
        }
        path = os.path.join(self.queue_dir, f"{queue_id}.telemetry.json")
        with open(path, "w") as f:
            json.dump(data, f)
        return path


# ─── Lifecycle Event Tests ────────────────────────────────────────────────────


class TestWriteLifecycleEvent(HooksTestCase):
    """Tests for write_lifecycle_event()."""

    def test_basic_event_written(self):
        """Event file is created with correct type and queue_id."""
        path = write_lifecycle_event(self.events_dir, "spec_completed", "q-001")
        self.assertTrue(os.path.isfile(path))
        event = self._read_event(path)
        self.assertEqual(event["type"], "spec_completed")
        self.assertEqual(event["queue_id"], "q-001")
        self.assertIn("timestamp", event)

    def test_event_includes_spec_path(self):
        """Event includes spec_path when provided."""
        path = write_lifecycle_event(
            self.events_dir,
            "spec_completed",
            "q-001",
            spec_path="/path/to/spec.md",
        )
        event = self._read_event(path)
        self.assertEqual(event["spec_path"], "/path/to/spec.md")

    def test_event_includes_iterations(self):
        """Event includes iterations when > 0."""
        path = write_lifecycle_event(
            self.events_dir, "spec_completed", "q-001", iterations=5
        )
        event = self._read_event(path)
        self.assertEqual(event["iterations"], 5)

    def test_event_includes_tasks_done(self):
        """Event includes tasks_done when > 0."""
        path = write_lifecycle_event(
            self.events_dir, "spec_completed", "q-001", tasks_done=8
        )
        event = self._read_event(path)
        self.assertEqual(event["tasks_done"], 8)

    def test_event_includes_tasks_added(self):
        """Event includes tasks_added when > 0 (self-evolution tracking)."""
        path = write_lifecycle_event(
            self.events_dir, "spec_completed", "q-001", tasks_added=2
        )
        event = self._read_event(path)
        self.assertEqual(event["tasks_added"], 2)

    def test_event_omits_zero_optional_fields(self):
        """Optional fields with zero/empty values are omitted."""
        path = write_lifecycle_event(self.events_dir, "spec_started", "q-001")
        event = self._read_event(path)
        self.assertNotIn("spec_path", event)
        self.assertNotIn("iterations", event)
        self.assertNotIn("tasks_done", event)
        self.assertNotIn("tasks_added", event)
        self.assertNotIn("reason", event)

    def test_event_includes_reason(self):
        """Event includes reason when provided."""
        path = write_lifecycle_event(
            self.events_dir,
            "spec_failed",
            "q-001",
            reason="max_iterations",
        )
        event = self._read_event(path)
        self.assertEqual(event["reason"], "max_iterations")

    def test_event_includes_worker_id(self):
        """Event includes worker_id when provided."""
        path = write_lifecycle_event(
            self.events_dir,
            "spec_started",
            "q-001",
            worker_id="w-1",
        )
        event = self._read_event(path)
        self.assertEqual(event["worker_id"], "w-1")

    def test_event_custom_timestamp(self):
        """Event uses the provided timestamp instead of auto-generating."""
        ts = "2024-01-15T08:23:00+00:00"
        path = write_lifecycle_event(
            self.events_dir, "spec_completed", "q-001", timestamp=ts
        )
        event = self._read_event(path)
        self.assertEqual(event["timestamp"], ts)

    def test_event_auto_timestamp(self):
        """Event auto-generates a timestamp when none provided."""
        path = write_lifecycle_event(self.events_dir, "spec_completed", "q-001")
        event = self._read_event(path)
        # Should be a valid ISO-8601 timestamp
        self.assertIn("T", event["timestamp"])

    def test_event_extra_fields(self):
        """Extra fields are merged into the event."""
        path = write_lifecycle_event(
            self.events_dir,
            "spec_completed",
            "q-001",
            extra={"custom_key": "custom_value"},
        )
        event = self._read_event(path)
        self.assertEqual(event["custom_key"], "custom_value")

    def test_event_sequence_number(self):
        """Events get incrementing sequence numbers."""
        path1 = write_lifecycle_event(self.events_dir, "spec_started", "q-001")
        path2 = write_lifecycle_event(self.events_dir, "spec_completed", "q-001")
        event1 = self._read_event(path1)
        event2 = self._read_event(path2)
        self.assertEqual(event1["seq"], 1)
        self.assertEqual(event2["seq"], 2)

    def test_events_dir_created_if_missing(self):
        """Events directory is created automatically if it doesn't exist."""
        new_events_dir = os.path.join(self.boi_state, "new_events")
        path = write_lifecycle_event(new_events_dir, "spec_completed", "q-001")
        self.assertTrue(os.path.isfile(path))

    def test_multiple_events_written(self):
        """Multiple events are written as separate files."""
        write_lifecycle_event(self.events_dir, "spec_started", "q-001")
        write_lifecycle_event(self.events_dir, "spec_completed", "q-001")
        write_lifecycle_event(self.events_dir, "spec_started", "q-002")

        events = self._read_all_events()
        self.assertEqual(len(events), 3)
        self.assertEqual(events[0]["type"], "spec_started")
        self.assertEqual(events[1]["type"], "spec_completed")
        self.assertEqual(events[2]["type"], "spec_started")


class TestWriteSpecCompletedEvent(HooksTestCase):
    """Tests for write_spec_completed_event() convenience function."""

    def test_completed_event_schema(self):
        """Completed event matches the t-8 spec schema."""
        path = write_spec_completed_event(
            events_dir=self.events_dir,
            queue_id="q-001",
            spec_path="/path/to/spec.md",
            iterations=3,
            tasks_done=8,
            tasks_added=2,
            timestamp="2024-01-15T08:23:00+00:00",
        )
        event = self._read_event(path)

        # Verify exact schema from t-8
        self.assertEqual(event["type"], "spec_completed")
        self.assertEqual(event["queue_id"], "q-001")
        self.assertEqual(event["spec_path"], "/path/to/spec.md")
        self.assertEqual(event["iterations"], 3)
        self.assertEqual(event["tasks_done"], 8)
        self.assertEqual(event["tasks_added"], 2)
        self.assertEqual(event["timestamp"], "2024-01-15T08:23:00+00:00")

    def test_completed_event_no_self_evolution(self):
        """Completed event with 0 tasks_added omits the field."""
        path = write_spec_completed_event(
            events_dir=self.events_dir,
            queue_id="q-002",
            spec_path="/path/to/spec.md",
            iterations=1,
            tasks_done=5,
            tasks_added=0,
        )
        event = self._read_event(path)
        self.assertNotIn("tasks_added", event)

    def test_completed_event_with_total(self):
        """Completed event includes tasks_total when provided."""
        path = write_spec_completed_event(
            events_dir=self.events_dir,
            queue_id="q-001",
            spec_path="/path/to/spec.md",
            iterations=2,
            tasks_done=8,
            tasks_total=8,
        )
        event = self._read_event(path)
        self.assertEqual(event["tasks_total"], 8)


class TestWriteSpecFailedEvent(HooksTestCase):
    """Tests for write_spec_failed_event() convenience function."""

    def test_failed_event_schema(self):
        """Failed event includes type, queue_id, spec_path, reason."""
        path = write_spec_failed_event(
            events_dir=self.events_dir,
            queue_id="q-001",
            spec_path="/path/to/spec.md",
            iterations=30,
            reason="max_iterations",
        )
        event = self._read_event(path)
        self.assertEqual(event["type"], "spec_failed")
        self.assertEqual(event["queue_id"], "q-001")
        self.assertEqual(event["spec_path"], "/path/to/spec.md")
        self.assertEqual(event["iterations"], 30)
        self.assertEqual(event["reason"], "max_iterations")

    def test_failed_event_consecutive_failures(self):
        """Failed event for consecutive failures includes reason."""
        path = write_spec_failed_event(
            events_dir=self.events_dir,
            queue_id="q-003",
            spec_path="/path/to/spec.md",
            iterations=5,
            reason="consecutive_failures",
        )
        event = self._read_event(path)
        self.assertEqual(event["reason"], "consecutive_failures")

    def test_failed_event_with_partial_progress(self):
        """Failed event includes tasks_done for partial progress."""
        path = write_spec_failed_event(
            events_dir=self.events_dir,
            queue_id="q-001",
            spec_path="/path/to/spec.md",
            iterations=30,
            tasks_done=5,
            tasks_added=1,
            reason="max_iterations",
        )
        event = self._read_event(path)
        self.assertEqual(event["tasks_done"], 5)
        self.assertEqual(event["tasks_added"], 1)


# ─── Hook Execution Tests ────────────────────────────────────────────────────


class TestRunHook(HooksTestCase):
    """Tests for run_hook()."""

    def test_hook_runs_when_present(self):
        """Hook script is executed when it exists and is executable."""
        self._create_hook_script("on-complete", "exit 0")
        result = run_hook(self.hooks_dir, "on-complete", "q-001", "/path/to/spec.md")
        self.assertEqual(result, 0)

    def test_hook_returns_none_when_absent(self):
        """Returns None when hook script doesn't exist."""
        result = run_hook(self.hooks_dir, "on-complete", "q-001", "/path/to/spec.md")
        self.assertIsNone(result)

    def test_hook_receives_queue_id_and_spec_path(self):
        """Hook script receives queue_id and spec_path as arguments."""
        marker = os.path.join(self.boi_state, "hook_args.txt")
        self._create_hook_script(
            "on-complete",
            f'echo "$1 $2" > "{marker}"',
        )
        run_hook(self.hooks_dir, "on-complete", "q-001", "/path/to/spec.md")
        with open(marker) as f:
            args = f.read().strip()
        self.assertEqual(args, "q-001 /path/to/spec.md")

    def test_hook_failure_does_not_raise(self):
        """Hook script failure does not raise an exception."""
        self._create_hook_script("on-complete", "exit 1")
        result = run_hook(self.hooks_dir, "on-complete", "q-001", "/path/to/spec.md")
        self.assertEqual(result, 1)

    def test_hook_not_executable_returns_none(self):
        """Non-executable hook file is skipped (returns None)."""
        hook_path = os.path.join(self.hooks_dir, "on-complete.sh")
        with open(hook_path, "w") as f:
            f.write("#!/bin/bash\nexit 0\n")
        # Don't set execute bit
        os.chmod(hook_path, stat.S_IRUSR | stat.S_IWUSR)
        result = run_hook(self.hooks_dir, "on-complete", "q-001", "/path/to/spec.md")
        self.assertIsNone(result)

    def test_hook_nonexistent_hooks_dir(self):
        """Returns None if hooks directory doesn't exist."""
        result = run_hook(
            "/nonexistent/hooks", "on-complete", "q-001", "/path/to/spec.md"
        )
        self.assertIsNone(result)

    def test_on_fail_hook(self):
        """on-fail hook script runs correctly."""
        marker = os.path.join(self.boi_state, "fail_marker.txt")
        self._create_hook_script("on-fail", f'echo "failed" > "{marker}"')
        result = run_hook(self.hooks_dir, "on-fail", "q-001", "/path/to/spec.md")
        self.assertEqual(result, 0)
        with open(marker) as f:
            self.assertEqual(f.read().strip(), "failed")


# ─── Completion Hooks Tests ──────────────────────────────────────────────────


class TestRunCompletionHooks(HooksTestCase):
    """Tests for run_completion_hooks()."""

    def test_success_runs_only_on_complete(self):
        """On success, only on-complete hook runs (not on-fail)."""
        self._create_hook_script("on-complete")
        self._create_hook_script("on-fail")
        results = run_completion_hooks(
            self.hooks_dir, "q-001", "/path/to/spec.md", is_failure=False
        )
        self.assertEqual(results["on-complete"], 0)
        self.assertNotIn("on-fail", results)

    def test_failure_runs_both_hooks(self):
        """On failure, both on-complete and on-fail hooks run."""
        self._create_hook_script("on-complete")
        self._create_hook_script("on-fail")
        results = run_completion_hooks(
            self.hooks_dir, "q-001", "/path/to/spec.md", is_failure=True
        )
        self.assertEqual(results["on-complete"], 0)
        self.assertEqual(results["on-fail"], 0)

    def test_no_hooks_present(self):
        """Works fine when no hooks exist."""
        results = run_completion_hooks(
            self.hooks_dir, "q-001", "/path/to/spec.md", is_failure=False
        )
        self.assertIsNone(results["on-complete"])

    def test_only_on_fail_present(self):
        """If only on-fail exists and it's a failure, it runs."""
        self._create_hook_script("on-fail")
        results = run_completion_hooks(
            self.hooks_dir, "q-001", "/path/to/spec.md", is_failure=True
        )
        self.assertIsNone(results["on-complete"])
        self.assertEqual(results["on-fail"], 0)

    def test_hooks_dir_missing(self):
        """Works fine when hooks directory doesn't exist."""
        results = run_completion_hooks(
            "/nonexistent/hooks", "q-001", "/path/to/spec.md"
        )
        self.assertIsNone(results["on-complete"])


# ─── List Hooks Tests ────────────────────────────────────────────────────────


class TestListHooks(HooksTestCase):
    """Tests for list_hooks()."""

    def test_empty_hooks_dir(self):
        """Returns empty list when no hooks present."""
        hooks = list_hooks(self.hooks_dir)
        self.assertEqual(hooks, [])

    def test_lists_hook_names(self):
        """Returns hook names (without .sh) sorted alphabetically."""
        self._create_hook_script("on-complete")
        self._create_hook_script("on-fail")
        self._create_hook_script("custom-notify")
        hooks = list_hooks(self.hooks_dir)
        self.assertEqual(hooks, ["custom-notify", "on-complete", "on-fail"])

    def test_ignores_non_sh_files(self):
        """Ignores files that aren't .sh scripts."""
        self._create_hook_script("on-complete")
        # Create a non-.sh file
        with open(os.path.join(self.hooks_dir, "README.md"), "w") as f:
            f.write("# Hooks\n")
        hooks = list_hooks(self.hooks_dir)
        self.assertEqual(hooks, ["on-complete"])

    def test_nonexistent_dir(self):
        """Returns empty list for nonexistent directory."""
        hooks = list_hooks("/nonexistent/hooks")
        self.assertEqual(hooks, [])


# ─── Tasks Added from Telemetry Tests ────────────────────────────────────────


class TestGetTasksAddedFromTelemetry(HooksTestCase):
    """Tests for get_tasks_added_from_telemetry()."""

    def test_sums_tasks_added(self):
        """Correctly sums tasks_added_per_iteration array."""
        self._create_telemetry_file("q-001", [1, 2, 0])
        total = get_tasks_added_from_telemetry(self.queue_dir, "q-001")
        self.assertEqual(total, 3)

    def test_zero_when_no_telemetry(self):
        """Returns 0 when telemetry file doesn't exist."""
        total = get_tasks_added_from_telemetry(self.queue_dir, "q-999")
        self.assertEqual(total, 0)

    def test_zero_when_empty_array(self):
        """Returns 0 when tasks_added_per_iteration is empty."""
        self._create_telemetry_file("q-001", [])
        total = get_tasks_added_from_telemetry(self.queue_dir, "q-001")
        self.assertEqual(total, 0)

    def test_handles_corrupt_telemetry(self):
        """Returns 0 when telemetry file is corrupt."""
        path = os.path.join(self.queue_dir, "q-001.telemetry.json")
        with open(path, "w") as f:
            f.write("not valid json")
        total = get_tasks_added_from_telemetry(self.queue_dir, "q-001")
        self.assertEqual(total, 0)


# ─── Integration: Events + Hooks Together ─────────────────────────────────────


class TestEventsAndHooksTogether(HooksTestCase):
    """Tests verifying BOI works with and without hooks."""

    def test_boi_works_without_hooks(self):
        """BOI functions correctly when no hooks exist at all."""
        # Write events (should work fine)
        path = write_spec_completed_event(
            events_dir=self.events_dir,
            queue_id="q-001",
            spec_path="/path/to/spec.md",
            iterations=3,
            tasks_done=8,
        )
        self.assertTrue(os.path.isfile(path))

        # Run hooks (should return None, not error)
        results = run_completion_hooks(self.hooks_dir, "q-001", "/path/to/spec.md")
        self.assertIsNone(results["on-complete"])

        # List hooks (should be empty)
        hooks = list_hooks(self.hooks_dir)
        self.assertEqual(hooks, [])

    def test_full_completion_flow(self):
        """Full completion flow: event written, on-complete hook fires."""
        # Create hook that writes a marker file
        marker = os.path.join(self.boi_state, "completed.txt")
        self._create_hook_script("on-complete", f'echo "$1" > "{marker}"')

        # Write completion event
        path = write_spec_completed_event(
            events_dir=self.events_dir,
            queue_id="q-001",
            spec_path="/path/to/spec.md",
            iterations=3,
            tasks_done=8,
            tasks_added=2,
        )

        # Run hooks
        results = run_completion_hooks(self.hooks_dir, "q-001", "/path/to/spec.md")

        # Verify event written
        event = self._read_event(path)
        self.assertEqual(event["type"], "spec_completed")
        self.assertEqual(event["tasks_added"], 2)

        # Verify hook ran
        self.assertEqual(results["on-complete"], 0)
        with open(marker) as f:
            self.assertEqual(f.read().strip(), "q-001")

    def test_full_failure_flow(self):
        """Full failure flow: event written, on-complete and on-fail fire."""
        complete_marker = os.path.join(self.boi_state, "complete.txt")
        fail_marker = os.path.join(self.boi_state, "fail.txt")
        self._create_hook_script(
            "on-complete", f'echo "complete:$1" > "{complete_marker}"'
        )
        self._create_hook_script("on-fail", f'echo "fail:$1" > "{fail_marker}"')

        # Write failure event
        path = write_spec_failed_event(
            events_dir=self.events_dir,
            queue_id="q-001",
            spec_path="/path/to/spec.md",
            iterations=30,
            reason="max_iterations",
        )

        # Run hooks (failure)
        results = run_completion_hooks(
            self.hooks_dir, "q-001", "/path/to/spec.md", is_failure=True
        )

        # Verify event
        event = self._read_event(path)
        self.assertEqual(event["type"], "spec_failed")
        self.assertEqual(event["reason"], "max_iterations")

        # Verify both hooks ran
        self.assertEqual(results["on-complete"], 0)
        self.assertEqual(results["on-fail"], 0)
        with open(complete_marker) as f:
            self.assertEqual(f.read().strip(), "complete:q-001")
        with open(fail_marker) as f:
            self.assertEqual(f.read().strip(), "fail:q-001")

    def test_events_pollable_by_external_systems(self):
        """Events directory can be polled by external systems."""
        # Write several events
        write_lifecycle_event(self.events_dir, "spec_started", "q-001")
        write_spec_completed_event(self.events_dir, "q-001", "/spec.md", 3, 8)
        write_lifecycle_event(self.events_dir, "spec_started", "q-002")
        write_spec_failed_event(
            self.events_dir, "q-002", "/spec2.md", 30, reason="max_iterations"
        )

        # External system polls events directory
        events = self._read_all_events()
        self.assertEqual(len(events), 4)

        # Can filter by type
        completed = [e for e in events if e["type"] == "spec_completed"]
        failed = [e for e in events if e["type"] == "spec_failed"]
        self.assertEqual(len(completed), 1)
        self.assertEqual(len(failed), 1)
        self.assertEqual(completed[0]["queue_id"], "q-001")
        self.assertEqual(failed[0]["queue_id"], "q-002")


if __name__ == "__main__":
    unittest.main()
