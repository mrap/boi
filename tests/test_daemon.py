# test_daemon.py — Unit tests for BOI daemon logic.
#
# Tests the daemon's decision-making: crash detection, worker completion,
# requeue logic, consecutive failures, cooldown, and daemon state writing.
#
# These tests exercise the queue.py functions that the daemon.sh calls,
# since the daemon's logic is implemented via Python calls. The shell
# script is the orchestrator; the logic lives in lib/queue.py.
#
# Uses stdlib unittest only (no pytest dependency).

import json
import os
import sys
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path

# Add parent directory to path so we can import lib modules
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.queue import (
    _write_entry,
    cancel,
    complete,
    COOLDOWN_SECONDS,
    dequeue,
    enqueue,
    fail,
    get_entry,
    get_queue,
    MAX_CONSECUTIVE_FAILURES,
    record_failure,
    requeue,
    set_running,
)
from lib.spec_parser import count_boi_tasks


class DaemonTestCase(unittest.TestCase):
    """Base test case with temp dirs mimicking ~/.boi/."""

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.boi_state = self._tmpdir.name
        self.queue_dir = os.path.join(self.boi_state, "queue")
        self.events_dir = os.path.join(self.boi_state, "events")
        self.logs_dir = os.path.join(self.boi_state, "logs")
        self.hooks_dir = os.path.join(self.boi_state, "hooks")
        os.makedirs(self.queue_dir)
        os.makedirs(self.events_dir)
        os.makedirs(self.logs_dir)
        os.makedirs(self.hooks_dir)

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
                f"\n### t-{tid}: Done task {tid}\nDONE\n\n**Spec:** Did it.\n**Verify:** true\n"
            )
            tid += 1
        for _ in range(tasks_pending):
            lines.append(
                f"\n### t-{tid}: Pending task {tid}\nPENDING\n\n**Spec:** Do it.\n**Verify:** true\n"
            )
            tid += 1
        for _ in range(tasks_skipped):
            lines.append(
                f"\n### t-{tid}: Skipped task {tid}\nSKIPPED\n\n**Spec:** Skip.\n**Verify:** true\n"
            )
            tid += 1
        Path(spec_path).write_text("".join(lines))
        return spec_path

    def _write_exit_file(self, queue_id, exit_code):
        """Simulate a worker writing its exit code file."""
        exit_path = os.path.join(self.queue_dir, f"{queue_id}.exit")
        Path(exit_path).write_text(str(exit_code))


# ─── Crash Detection Tests ──────────────────────────────────────────────────


class TestCrashDetection(DaemonTestCase):
    """Test the daemon's crash detection logic (what check_worker_completion does)."""

    def test_no_exit_file_is_crash(self):
        """If no exit file exists after worker dies, it's a crash."""
        spec_path = self._create_spec(tasks_pending=3)
        e = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, e["id"], "w-1")

        # Worker dies without writing exit file -> crash
        # Daemon should call record_failure
        exceeded = record_failure(self.queue_dir, e["id"])
        self.assertFalse(exceeded)

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["consecutive_failures"], 1)
        self.assertIn("cooldown_until", entry)

    def test_nonzero_exit_code_is_crash(self):
        """Non-zero exit code from Claude is treated as a crash."""
        spec_path = self._create_spec(tasks_pending=3)
        e = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, e["id"], "w-1")

        # Worker writes non-zero exit code
        self._write_exit_file(e["id"], 1)

        # Daemon should call record_failure
        exceeded = record_failure(self.queue_dir, e["id"])
        self.assertFalse(exceeded)

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["consecutive_failures"], 1)

    def test_zero_exit_code_is_normal(self):
        """Exit code 0 means normal iteration completion."""
        spec_path = self._create_spec(tasks_pending=3)
        e = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, e["id"], "w-1")

        # Worker writes exit code 0
        self._write_exit_file(e["id"], 0)

        # Daemon reads spec counts and requeues normally (not via record_failure)
        requeue(self.queue_dir, e["id"], tasks_done=1, tasks_total=3)

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["consecutive_failures"], 0)
        self.assertNotIn("cooldown_until", entry)


# ─── Consecutive Failures & Cooldown Tests ───────────────────────────────────


class TestConsecutiveFailuresAndCooldown(DaemonTestCase):
    """Test consecutive failure tracking and cooldown behavior."""

    def test_record_failure_sets_cooldown(self):
        """record_failure should set cooldown_until."""
        spec_path = self._create_spec(tasks_pending=3)
        e = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, e["id"], "w-1")

        record_failure(self.queue_dir, e["id"])

        entry = get_entry(self.queue_dir, e["id"])
        self.assertIn("cooldown_until", entry)
        # Cooldown should be in the future
        cooldown = datetime.fromisoformat(entry["cooldown_until"])
        now = datetime.now(timezone.utc)
        self.assertGreater(cooldown, now)

    def test_dequeue_skips_cooled_down_entries(self):
        """Specs in cooldown should not be dequeued."""
        spec_path = self._create_spec(tasks_pending=3)
        e = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, e["id"], "w-1")

        record_failure(self.queue_dir, e["id"])
        # Set status to requeued (daemon does this after crash)
        entry = get_entry(self.queue_dir, e["id"])
        entry["status"] = "requeued"
        _write_entry(self.queue_dir, entry)

        # Dequeue should skip it (still cooling down)
        result = dequeue(self.queue_dir)
        self.assertIsNone(result)

    def test_dequeue_picks_up_after_cooldown_expires(self):
        """After cooldown expires, spec should be dequeue-able again."""
        spec_path = self._create_spec(tasks_pending=3)
        e = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, e["id"], "w-1")

        record_failure(self.queue_dir, e["id"])
        entry = get_entry(self.queue_dir, e["id"])
        entry["status"] = "requeued"
        # Set cooldown to the past
        entry["cooldown_until"] = (
            datetime.now(timezone.utc) - timedelta(seconds=1)
        ).isoformat()
        _write_entry(self.queue_dir, entry)

        result = dequeue(self.queue_dir)
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], e["id"])

    def test_successful_requeue_clears_cooldown(self):
        """requeue() (successful iteration) should clear cooldown."""
        spec_path = self._create_spec(tasks_pending=3)
        e = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, e["id"], "w-1")

        # Record a failure (sets cooldown)
        record_failure(self.queue_dir, e["id"])
        entry = get_entry(self.queue_dir, e["id"])
        self.assertIn("cooldown_until", entry)

        # Successful requeue should clear it
        requeue(self.queue_dir, e["id"], tasks_done=1, tasks_total=3)
        entry = get_entry(self.queue_dir, e["id"])
        self.assertNotIn("cooldown_until", entry)
        self.assertEqual(entry["consecutive_failures"], 0)

    def test_max_failures_triggers_fail(self):
        """After MAX_CONSECUTIVE_FAILURES, spec should be marked failed."""
        spec_path = self._create_spec(tasks_pending=3)
        e = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, e["id"], "w-1")

        for i in range(MAX_CONSECUTIVE_FAILURES - 1):
            exceeded = record_failure(self.queue_dir, e["id"])
            self.assertFalse(exceeded)

        exceeded = record_failure(self.queue_dir, e["id"])
        self.assertTrue(exceeded)

        # Daemon would call fail() at this point
        fail(self.queue_dir, e["id"], "Consecutive failures exceeded threshold")
        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["status"], "failed")

    def test_multiple_crashes_then_recovery(self):
        """If a spec crashes twice then succeeds, failures reset."""
        spec_path = self._create_spec(tasks_pending=3)
        e = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, e["id"], "w-1")

        # Two crashes
        record_failure(self.queue_dir, e["id"])
        record_failure(self.queue_dir, e["id"])
        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["consecutive_failures"], 2)

        # Successful iteration resets
        requeue(self.queue_dir, e["id"], tasks_done=1, tasks_total=3)
        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["consecutive_failures"], 0)
        self.assertNotIn("cooldown_until", entry)

    def test_cooldown_seconds_constant(self):
        """COOLDOWN_SECONDS should be a positive number."""
        self.assertGreater(COOLDOWN_SECONDS, 0)
        self.assertEqual(COOLDOWN_SECONDS, 60)


# ─── Worker Assignment Tests ────────────────────────────────────────────────


class TestWorkerAssignment(DaemonTestCase):
    """Test the daemon's spec-to-worker assignment logic."""

    def test_assigns_highest_priority_first(self):
        """Daemon picks highest priority (lowest number) spec."""
        spec1 = self._create_spec(tasks_pending=3)
        spec2 = self._create_spec(tasks_pending=3)

        enqueue(self.queue_dir, spec1, priority=200)
        enqueue(self.queue_dir, spec2, priority=50)

        result = dequeue(self.queue_dir)
        self.assertEqual(result["priority"], 50)

    def test_skips_dag_blocked_specs(self):
        """DAG-blocked specs are skipped even if they have higher priority."""
        spec1 = self._create_spec(tasks_pending=3)
        spec2 = self._create_spec(tasks_pending=3)

        e1 = enqueue(self.queue_dir, spec1, priority=100)
        enqueue(self.queue_dir, spec2, priority=50, blocked_by=[e1["id"]])

        result = dequeue(self.queue_dir)
        self.assertEqual(result["id"], e1["id"])

    def test_set_running_records_worker(self):
        """set_running records worker ID and increments iteration."""
        spec = self._create_spec(tasks_pending=3)
        e = enqueue(self.queue_dir, spec)
        set_running(self.queue_dir, e["id"], "w-2")

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["status"], "running")
        self.assertEqual(entry["last_worker"], "w-2")
        self.assertEqual(entry["iteration"], 1)

    def test_multiple_workers_get_different_specs(self):
        """Multiple workers each get a different spec."""
        spec1 = self._create_spec(tasks_pending=3)
        spec2 = self._create_spec(tasks_pending=3)
        spec3 = self._create_spec(tasks_pending=3)

        e1 = enqueue(self.queue_dir, spec1, priority=10)
        e2 = enqueue(self.queue_dir, spec2, priority=20)
        e3 = enqueue(self.queue_dir, spec3, priority=30)

        # Worker 1 picks up e1
        r1 = dequeue(self.queue_dir)
        set_running(self.queue_dir, r1["id"], "w-1")

        # Worker 2 picks up e2 (e1 is now running)
        r2 = dequeue(self.queue_dir)
        set_running(self.queue_dir, r2["id"], "w-2")

        # Worker 3 picks up e3
        r3 = dequeue(self.queue_dir)
        set_running(self.queue_dir, r3["id"], "w-3")

        self.assertEqual(r1["id"], e1["id"])
        self.assertEqual(r2["id"], e2["id"])
        self.assertEqual(r3["id"], e3["id"])


# ─── Completion Detection Tests ─────────────────────────────────────────────


class TestCompletionDetection(DaemonTestCase):
    """Test the daemon's logic for detecting when a spec is done."""

    def test_zero_pending_means_completed(self):
        """When spec has 0 PENDING tasks, daemon should mark completed."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=5)
        e = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, e["id"], "w-1")

        counts = count_boi_tasks(spec_path)
        self.assertEqual(counts["pending"], 0)
        self.assertEqual(counts["done"], 5)

        complete(self.queue_dir, e["id"], counts["done"], counts["total"])
        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["status"], "completed")

    def test_pending_remaining_means_requeue(self):
        """When pending tasks remain, daemon should requeue."""
        spec_path = self._create_spec(tasks_pending=3, tasks_done=2)
        e = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, e["id"], "w-1")

        counts = count_boi_tasks(spec_path)
        self.assertEqual(counts["pending"], 3)

        requeue(self.queue_dir, e["id"], counts["done"], counts["total"])
        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["status"], "requeued")

    def test_max_iterations_means_fail(self):
        """When max iterations reached with pending tasks, daemon should fail."""
        spec_path = self._create_spec(tasks_pending=3, tasks_done=2)
        e = enqueue(self.queue_dir, spec_path, max_iterations=3)

        # Simulate 3 iterations
        for i in range(3):
            set_running(self.queue_dir, e["id"], f"w-{i}")
            requeue(self.queue_dir, e["id"])

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["iteration"], 3)

        # Daemon would check: iteration >= max_iterations -> fail
        if entry["iteration"] >= entry["max_iterations"]:
            fail(self.queue_dir, e["id"], "Max iterations reached")

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["status"], "failed")


# ─── Requeue Logic Tests ────────────────────────────────────────────────────


class TestRequeueLogic(DaemonTestCase):
    """Test the daemon's requeue behavior across iterations."""

    def test_requeued_spec_can_be_picked_up_again(self):
        """After requeue, spec should be dequeue-able again."""
        spec_path = self._create_spec(tasks_pending=3)
        e = enqueue(self.queue_dir, spec_path)

        set_running(self.queue_dir, e["id"], "w-1")
        requeue(self.queue_dir, e["id"], tasks_done=1, tasks_total=3)

        result = dequeue(self.queue_dir)
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], e["id"])

    def test_requeued_keeps_priority(self):
        """Requeued spec keeps its original priority."""
        spec_path = self._create_spec(tasks_pending=3)
        e = enqueue(self.queue_dir, spec_path, priority=42)

        set_running(self.queue_dir, e["id"], "w-1")
        requeue(self.queue_dir, e["id"], tasks_done=1, tasks_total=3)

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["priority"], 42)

    def test_iteration_count_increments_across_requeues(self):
        """Each set_running call increments iteration."""
        spec_path = self._create_spec(tasks_pending=5)
        e = enqueue(self.queue_dir, spec_path)

        for i in range(4):
            set_running(self.queue_dir, e["id"], f"w-{i % 3}")
            requeue(self.queue_dir, e["id"])

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["iteration"], 4)


# ─── Daemon State File Tests ────────────────────────────────────────────────


class TestDaemonState(DaemonTestCase):
    """Test the daemon state file writing logic (Python equivalent)."""

    def test_daemon_state_structure(self):
        """Daemon state should contain expected fields."""
        spec_path = self._create_spec(tasks_pending=3)
        e = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, e["id"], "w-1")

        # Simulate what write_daemon_state does
        entries = get_queue(self.queue_dir)
        counts = {
            "queued": 0,
            "requeued": 0,
            "running": 0,
            "completed": 0,
            "failed": 0,
            "canceled": 0,
        }
        for entry in entries:
            s = entry.get("status", "queued")
            if s in counts:
                counts[s] += 1

        state = {
            "timestamp": datetime.now(timezone.utc).isoformat(),
            "daemon_pid": os.getpid(),
            "queue_summary": counts,
            "total_specs": len(entries),
            "worker_count": 3,
            "specs": [
                {
                    "id": entry["id"],
                    "status": entry.get("status"),
                    "priority": entry.get("priority"),
                    "iteration": entry.get("iteration", 0),
                    "max_iterations": entry.get("max_iterations", 30),
                    "last_worker": entry.get("last_worker"),
                    "tasks_done": entry.get("tasks_done", 0),
                    "tasks_total": entry.get("tasks_total", 0),
                    "consecutive_failures": entry.get("consecutive_failures", 0),
                }
                for entry in entries
                if entry.get("status") in ("queued", "requeued", "running")
            ],
        }

        state_file = os.path.join(self.boi_state, "daemon-state.json")
        with open(state_file, "w") as f:
            json.dump(state, f, indent=2)

        # Verify
        data = json.loads(Path(state_file).read_text())
        self.assertIn("timestamp", data)
        self.assertIn("daemon_pid", data)
        self.assertIn("queue_summary", data)
        self.assertEqual(data["queue_summary"]["running"], 1)
        self.assertEqual(data["total_specs"], 1)
        self.assertEqual(len(data["specs"]), 1)
        self.assertEqual(data["specs"][0]["id"], e["id"])
        self.assertEqual(data["specs"][0]["status"], "running")

    def test_daemon_state_only_shows_active_specs(self):
        """Daemon state should only list active (not completed/failed) specs."""
        spec1 = self._create_spec(tasks_pending=3)
        spec2 = self._create_spec(tasks_pending=3)
        spec3 = self._create_spec(tasks_pending=3)

        e1 = enqueue(self.queue_dir, spec1)
        e2 = enqueue(self.queue_dir, spec2)
        e3 = enqueue(self.queue_dir, spec3)

        set_running(self.queue_dir, e1["id"], "w-1")
        complete(self.queue_dir, e1["id"])

        entries = get_queue(self.queue_dir)
        active_specs = [
            e for e in entries if e.get("status") in ("queued", "requeued", "running")
        ]

        self.assertEqual(len(active_specs), 2)
        active_ids = {s["id"] for s in active_specs}
        self.assertIn(e2["id"], active_ids)
        self.assertIn(e3["id"], active_ids)
        self.assertNotIn(e1["id"], active_ids)

    def test_daemon_state_file_is_valid_json(self):
        """Daemon state file should be valid JSON when written atomically."""
        state_file = os.path.join(self.boi_state, "daemon-state.json")
        state = {
            "timestamp": datetime.now(timezone.utc).isoformat(),
            "daemon_pid": os.getpid(),
            "queue_summary": {
                "queued": 1,
                "running": 0,
                "completed": 0,
                "failed": 0,
                "requeued": 0,
                "canceled": 0,
            },
            "total_specs": 1,
            "worker_count": 3,
            "specs": [],
        }
        tmp = state_file + ".tmp"
        with open(tmp, "w") as f:
            json.dump(state, f, indent=2)
            f.write("\n")
        os.rename(tmp, state_file)

        data = json.loads(Path(state_file).read_text())
        self.assertEqual(data["total_specs"], 1)


# ─── Full Daemon Scenario Tests ─────────────────────────────────────────────


class TestDaemonScenarios(DaemonTestCase):
    """End-to-end scenarios that simulate daemon loop iterations."""

    def test_scenario_normal_completion(self):
        """Spec queued -> running -> iteration exits 0 -> 0 pending -> completed."""
        spec_path = self._create_spec(tasks_pending=0, tasks_done=3)
        e = enqueue(self.queue_dir, spec_path)

        # Daemon picks it up
        result = dequeue(self.queue_dir)
        self.assertEqual(result["id"], e["id"])
        set_running(self.queue_dir, e["id"], "w-1")

        # Worker exits with code 0
        self._write_exit_file(e["id"], 0)

        # Daemon reads spec: 0 pending
        counts = count_boi_tasks(spec_path)
        self.assertEqual(counts["pending"], 0)

        complete(self.queue_dir, e["id"], counts["done"], counts["total"])
        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["status"], "completed")

    def test_scenario_normal_requeue(self):
        """Spec queued -> running -> exits 0 -> pending > 0 -> requeued."""
        spec_path = self._create_spec(tasks_pending=3, tasks_done=2)
        e = enqueue(self.queue_dir, spec_path)

        result = dequeue(self.queue_dir)
        set_running(self.queue_dir, e["id"], "w-1")

        self._write_exit_file(e["id"], 0)

        counts = count_boi_tasks(spec_path)
        self.assertEqual(counts["pending"], 3)

        requeue(self.queue_dir, e["id"], counts["done"], counts["total"])
        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["status"], "requeued")
        self.assertEqual(entry["consecutive_failures"], 0)

    def test_scenario_crash_and_recovery(self):
        """Worker crashes, requeues with cooldown, then succeeds next time."""
        spec_path = self._create_spec(tasks_pending=3)
        e = enqueue(self.queue_dir, spec_path)

        # First iteration: crash (no exit file)
        set_running(self.queue_dir, e["id"], "w-1")
        # No exit file written -> crash
        exceeded = record_failure(self.queue_dir, e["id"])
        self.assertFalse(exceeded)
        entry = get_entry(self.queue_dir, e["id"])
        entry["status"] = "requeued"
        _write_entry(self.queue_dir, entry)

        # Should not dequeue during cooldown
        result = dequeue(self.queue_dir)
        self.assertIsNone(result)

        # Expire cooldown
        entry = get_entry(self.queue_dir, e["id"])
        entry["cooldown_until"] = (
            datetime.now(timezone.utc) - timedelta(seconds=1)
        ).isoformat()
        _write_entry(self.queue_dir, entry)

        # Now dequeue-able
        result = dequeue(self.queue_dir)
        self.assertIsNotNone(result)

        # Second iteration succeeds
        set_running(self.queue_dir, e["id"], "w-2")
        requeue(self.queue_dir, e["id"], tasks_done=1, tasks_total=3)
        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["consecutive_failures"], 0)
        self.assertNotIn("cooldown_until", entry)

    def test_scenario_repeated_crashes_fail(self):
        """5 consecutive crashes -> spec marked failed."""
        spec_path = self._create_spec(tasks_pending=3)
        e = enqueue(self.queue_dir, spec_path, max_iterations=30)

        for i in range(MAX_CONSECUTIVE_FAILURES):
            set_running(self.queue_dir, e["id"], f"w-{i % 3}")
            exceeded = record_failure(self.queue_dir, e["id"])

            if exceeded:
                fail(self.queue_dir, e["id"], "Consecutive failures exceeded threshold")
                break
            else:
                entry = get_entry(self.queue_dir, e["id"])
                entry["status"] = "requeued"
                entry["cooldown_until"] = (
                    datetime.now(timezone.utc) - timedelta(seconds=1)
                ).isoformat()
                _write_entry(self.queue_dir, entry)

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["status"], "failed")
        self.assertEqual(entry["consecutive_failures"], MAX_CONSECUTIVE_FAILURES)

    def test_scenario_max_iterations_after_normal_iterations(self):
        """Spec hits max iterations through normal requeues (no crashes)."""
        spec_path = self._create_spec(tasks_pending=5)
        e = enqueue(self.queue_dir, spec_path, max_iterations=3)

        for i in range(3):
            set_running(self.queue_dir, e["id"], f"w-{i}")
            requeue(self.queue_dir, e["id"])

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["iteration"], 3)

        # Daemon detects iteration >= max_iterations
        if entry["iteration"] >= entry["max_iterations"]:
            fail(self.queue_dir, e["id"], "Max iterations reached")

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["status"], "failed")

    def test_scenario_multiple_specs_limited_workers(self):
        """3 specs but only 2 workers: third spec waits."""
        spec1 = self._create_spec(tasks_pending=3)
        spec2 = self._create_spec(tasks_pending=3)
        spec3 = self._create_spec(tasks_pending=3)

        e1 = enqueue(self.queue_dir, spec1, priority=10)
        e2 = enqueue(self.queue_dir, spec2, priority=20)
        e3 = enqueue(self.queue_dir, spec3, priority=30)

        # Two workers pick up first two specs
        r1 = dequeue(self.queue_dir)
        set_running(self.queue_dir, r1["id"], "w-1")
        r2 = dequeue(self.queue_dir)
        set_running(self.queue_dir, r2["id"], "w-2")

        self.assertEqual(r1["id"], e1["id"])
        self.assertEqual(r2["id"], e2["id"])

        # Third dequeue should get e3 (still queued)
        r3 = dequeue(self.queue_dir)
        self.assertIsNotNone(r3)
        self.assertEqual(r3["id"], e3["id"])

    def test_scenario_dag_dependency_lifecycle(self):
        """Spec B blocked by A: B only runs after A completes."""
        spec_a = self._create_spec(tasks_pending=0, tasks_done=2)
        spec_b = self._create_spec(tasks_pending=3)

        ea = enqueue(self.queue_dir, spec_a, priority=100)
        eb = enqueue(self.queue_dir, spec_b, priority=50, blocked_by=[ea["id"]])

        # B has higher priority but is blocked
        result = dequeue(self.queue_dir)
        self.assertEqual(result["id"], ea["id"])

        set_running(self.queue_dir, ea["id"], "w-1")
        counts = count_boi_tasks(spec_a)
        complete(self.queue_dir, ea["id"], counts["done"], counts["total"])

        # Now B is unblocked
        result = dequeue(self.queue_dir)
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], eb["id"])


# ─── Spec Parsing Integration ───────────────────────────────────────────────


class TestSpecParsing(DaemonTestCase):
    """Test that daemon correctly reads spec task counts."""

    def test_count_pending_tasks(self):
        spec_path = self._create_spec(tasks_pending=5, tasks_done=3)
        counts = count_boi_tasks(spec_path)
        self.assertEqual(counts["pending"], 5)
        self.assertEqual(counts["done"], 3)
        self.assertEqual(counts["total"], 8)

    def test_count_all_done(self):
        spec_path = self._create_spec(tasks_pending=0, tasks_done=5)
        counts = count_boi_tasks(spec_path)
        self.assertEqual(counts["pending"], 0)
        self.assertEqual(counts["done"], 5)

    def test_count_with_skipped(self):
        spec_path = self._create_spec(tasks_pending=2, tasks_done=3, tasks_skipped=1)
        counts = count_boi_tasks(spec_path)
        self.assertEqual(counts["pending"], 2)
        self.assertEqual(counts["done"], 3)
        self.assertEqual(counts["skipped"], 1)
        self.assertEqual(counts["total"], 6)

    def test_empty_spec(self):
        spec_path = os.path.join(self._tmpdir.name, "empty.md")
        Path(spec_path).write_text("# No tasks here\n")
        counts = count_boi_tasks(spec_path)
        self.assertEqual(counts["total"], 0)
        self.assertEqual(counts["pending"], 0)

    def test_nonexistent_spec(self):
        counts = count_boi_tasks("/nonexistent/spec.md")
        self.assertEqual(counts["total"], 0)


# ─── Hook Tests ──────────────────────────────────────────────────────────────


class TestHooks(DaemonTestCase):
    """Test that hooks are properly detected."""

    def test_hook_script_exists(self):
        """Hook script can be created and detected."""
        hook_path = os.path.join(self.hooks_dir, "on-complete.sh")
        Path(hook_path).write_text('#!/bin/bash\necho "completed: $1"\n')
        os.chmod(hook_path, 0o755)
        self.assertTrue(os.access(hook_path, os.X_OK))

    def test_hook_directory_created(self):
        """Hooks directory exists after setUp."""
        self.assertTrue(os.path.isdir(self.hooks_dir))


# ─── Worker Timeout Tests ──────────────────────────────────────────────────


class TestWorkerTimeout(DaemonTestCase):
    """Test per-iteration worker timeout configuration and detection."""

    def test_timeout_stored_in_queue_entry(self):
        """Queue entry can store a per-spec worker_timeout_seconds."""
        spec_path = self._create_spec(tasks_pending=3)
        e = enqueue(self.queue_dir, spec_path)

        # Simulate what boi dispatch --timeout does
        e["worker_timeout_seconds"] = 900  # 15 minutes
        _write_entry(self.queue_dir, e)

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["worker_timeout_seconds"], 900)

    def test_timeout_not_set_by_default(self):
        """Queue entries without explicit timeout have no worker_timeout_seconds field."""
        spec_path = self._create_spec(tasks_pending=3)
        e = enqueue(self.queue_dir, spec_path)

        entry = get_entry(self.queue_dir, e["id"])
        self.assertNotIn("worker_timeout_seconds", entry)

    def test_timeout_config_default(self):
        """Config without worker_timeout_seconds uses default 1800."""
        config_path = os.path.join(self.boi_state, "config.json")
        config = {
            "version": "1",
            "workers": [{"id": "w-1", "worktree_path": "/tmp/test"}],
        }
        Path(config_path).write_text(json.dumps(config))

        with open(config_path) as f:
            c = json.load(f)

        timeout = c.get("worker_timeout_seconds", 1800)
        self.assertEqual(timeout, 1800)

    def test_timeout_config_override(self):
        """Config with worker_timeout_seconds overrides the default."""
        config_path = os.path.join(self.boi_state, "config.json")
        config = {
            "version": "1",
            "workers": [{"id": "w-1", "worktree_path": "/tmp/test"}],
            "worker_timeout_seconds": 600,
        }
        Path(config_path).write_text(json.dumps(config))

        with open(config_path) as f:
            c = json.load(f)

        timeout = c.get("worker_timeout_seconds", 1800)
        self.assertEqual(timeout, 600)

    def test_per_spec_timeout_overrides_config(self):
        """Per-spec timeout takes precedence over config timeout."""
        # Simulate config with 1800
        config_timeout = 1800

        # Simulate per-spec override
        spec_path = self._create_spec(tasks_pending=3)
        e = enqueue(self.queue_dir, spec_path)
        e["worker_timeout_seconds"] = 300
        _write_entry(self.queue_dir, e)

        entry = get_entry(self.queue_dir, e["id"])
        spec_timeout = entry.get("worker_timeout_seconds")

        # The daemon resolves timeout: spec override > config > default
        effective = spec_timeout if spec_timeout is not None else config_timeout
        self.assertEqual(effective, 300)

    def test_timeout_detection_requeues_spec(self):
        """A timed-out worker should end up requeued after record_failure."""
        spec_path = self._create_spec(tasks_pending=3)
        e = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, e["id"], "w-1")

        # Timeout triggers record_failure + status set to requeued
        record_failure(self.queue_dir, e["id"])
        entry = get_entry(self.queue_dir, e["id"])
        entry["status"] = "requeued"
        _write_entry(self.queue_dir, entry)

        # Verify the spec is requeued with cooldown
        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["status"], "requeued")
        self.assertEqual(entry["consecutive_failures"], 1)
        self.assertIn("cooldown_until", entry)

    def test_repeated_timeouts_fail_spec(self):
        """MAX_CONSECUTIVE_FAILURES timeouts should fail the spec permanently."""
        spec_path = self._create_spec(tasks_pending=3)
        e = enqueue(self.queue_dir, spec_path)

        for i in range(MAX_CONSECUTIVE_FAILURES):
            set_running(self.queue_dir, e["id"], f"w-{i % 3}")
            exceeded = record_failure(self.queue_dir, e["id"])
            if exceeded:
                fail(self.queue_dir, e["id"], "Consecutive timeouts exceeded threshold")
                break
            entry = get_entry(self.queue_dir, e["id"])
            entry["status"] = "requeued"
            entry["cooldown_until"] = (
                datetime.now(timezone.utc) - timedelta(seconds=1)
            ).isoformat()
            _write_entry(self.queue_dir, entry)

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["status"], "failed")
        self.assertEqual(entry["consecutive_failures"], MAX_CONSECUTIVE_FAILURES)

    def test_timeout_crash_triggers_record_failure(self):
        """A timed-out worker should be treated as a crash (record_failure)."""
        spec_path = self._create_spec(tasks_pending=3)
        e = enqueue(self.queue_dir, spec_path)
        set_running(self.queue_dir, e["id"], "w-1")

        # Timeout means no exit file — same as crash
        # Daemon calls record_failure when timeout occurs
        exceeded = record_failure(self.queue_dir, e["id"])
        self.assertFalse(exceeded)

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["consecutive_failures"], 1)
        self.assertIn("cooldown_until", entry)


if __name__ == "__main__":
    unittest.main()
