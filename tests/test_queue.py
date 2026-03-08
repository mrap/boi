# test_queue.py — Unit tests for BOI spec queue operations.
#
# Tests cover: enqueue, dequeue, requeue, complete, fail,
# priority ordering, DAG blocking, consecutive failure tracking,
# max iterations enforcement, cancel, ID generation, spec copy,
# duplicate detection, and sync_back.
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

from lib.queue import (
    cancel,
    complete,
    DEFAULT_MAX_ITERATIONS,
    DEFAULT_PRIORITY,
    dequeue,
    DuplicateSpecError,
    enqueue,
    fail,
    get_entry,
    get_queue,
    MAX_CONSECUTIVE_FAILURES,
    purge,
    record_failure,
    requeue,
    set_running,
    sync_back_spec,
    update_task_counts,
)


class QueueTestCase(unittest.TestCase):
    """Base test case that creates a temp queue directory and spec files."""

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.queue_dir = os.path.join(self._tmpdir.name, "queue")
        os.makedirs(self.queue_dir)
        # Create a directory for spec files
        self.specs_dir = os.path.join(self._tmpdir.name, "specs")
        os.makedirs(self.specs_dir)

    def tearDown(self):
        self._tmpdir.cleanup()

    def _make_spec(self, name: str = "spec.md", content: str = "") -> str:
        """Create a spec file and return its absolute path."""
        if not content:
            content = (
                "# Test Spec\n\n## Tasks\n\n"
                "### t-1: Test task\nPENDING\n\n"
                "**Spec:** Do something.\n\n"
                "**Verify:** Check it.\n"
            )
        path = os.path.join(self.specs_dir, name)
        with open(path, "w") as f:
            f.write(content)
        return path


# ─── Enqueue Tests ────────────────────────────────────────────────────────────


class TestEnqueue(QueueTestCase):
    def test_enqueue_creates_json_file(self):
        spec = self._make_spec()
        entry = enqueue(self.queue_dir, spec)
        path = Path(self.queue_dir) / f"{entry['id']}.json"
        self.assertTrue(path.is_file())

    def test_enqueue_returns_valid_entry(self):
        spec = self._make_spec()
        entry = enqueue(self.queue_dir, spec)
        self.assertEqual(entry["id"], "q-001")
        # spec_path should point to the copy, not the original
        self.assertIn("q-001.spec.md", entry["spec_path"])
        self.assertEqual(entry["original_spec_path"], os.path.abspath(spec))
        self.assertEqual(entry["status"], "queued")
        self.assertEqual(entry["priority"], DEFAULT_PRIORITY)
        self.assertEqual(entry["iteration"], 0)
        self.assertEqual(entry["max_iterations"], DEFAULT_MAX_ITERATIONS)
        self.assertEqual(entry["blocked_by"], [])
        self.assertIsNone(entry["last_worker"])
        self.assertIsNone(entry["last_iteration_at"])
        self.assertEqual(entry["consecutive_failures"], 0)
        self.assertEqual(entry["tasks_done"], 0)
        self.assertEqual(entry["tasks_total"], 0)
        self.assertTrue(entry["sync_back"])
        self.assertIn("submitted_at", entry)

    def test_enqueue_auto_increments_id(self):
        e1 = enqueue(self.queue_dir, self._make_spec("spec1.md"))
        e2 = enqueue(self.queue_dir, self._make_spec("spec2.md"))
        e3 = enqueue(self.queue_dir, self._make_spec("spec3.md"))
        self.assertEqual(e1["id"], "q-001")
        self.assertEqual(e2["id"], "q-002")
        self.assertEqual(e3["id"], "q-003")

    def test_enqueue_custom_priority(self):
        entry = enqueue(self.queue_dir, self._make_spec(), priority=50)
        self.assertEqual(entry["priority"], 50)

    def test_enqueue_custom_max_iterations(self):
        entry = enqueue(self.queue_dir, self._make_spec(), max_iterations=10)
        self.assertEqual(entry["max_iterations"], 10)

    def test_enqueue_with_blocked_by(self):
        entry = enqueue(
            self.queue_dir, self._make_spec(), blocked_by=["q-001", "q-002"]
        )
        self.assertEqual(entry["blocked_by"], ["q-001", "q-002"])

    def test_enqueue_with_worktree(self):
        entry = enqueue(self.queue_dir, self._make_spec(), checkout="/tmp/worktree-1")
        self.assertEqual(entry["worktree"], "/tmp/worktree-1")

    def test_enqueue_custom_queue_id(self):
        entry = enqueue(self.queue_dir, self._make_spec(), queue_id="q-custom")
        self.assertEqual(entry["id"], "q-custom")

    def test_enqueue_spec_path_is_absolute(self):
        spec = self._make_spec()
        entry = enqueue(self.queue_dir, spec)
        self.assertTrue(os.path.isabs(entry["spec_path"]))

    def test_enqueue_creates_queue_dir_if_missing(self):
        new_queue_dir = os.path.join(self._tmpdir.name, "nonexistent", "queue")
        entry = enqueue(new_queue_dir, self._make_spec())
        self.assertTrue(Path(new_queue_dir).is_dir())
        self.assertEqual(entry["id"], "q-001")

    def test_enqueue_json_is_valid(self):
        entry = enqueue(self.queue_dir, self._make_spec())
        path = Path(self.queue_dir) / f"{entry['id']}.json"
        data = json.loads(path.read_text())
        self.assertEqual(data["id"], entry["id"])
        self.assertEqual(data["status"], "queued")


# ─── Dequeue Tests ────────────────────────────────────────────────────────────


class TestDequeue(QueueTestCase):
    def test_dequeue_returns_highest_priority(self):
        enqueue(self.queue_dir, self._make_spec("low.md"), priority=200)
        enqueue(self.queue_dir, self._make_spec("high.md"), priority=50)
        enqueue(self.queue_dir, self._make_spec("mid.md"), priority=100)

        result = dequeue(self.queue_dir)
        self.assertIsNotNone(result)
        self.assertEqual(result["priority"], 50)

    def test_dequeue_returns_none_when_empty(self):
        result = dequeue(self.queue_dir)
        self.assertIsNone(result)

    def test_dequeue_skips_running_specs(self):
        e1 = enqueue(self.queue_dir, self._make_spec("spec1.md"), priority=50)
        enqueue(self.queue_dir, self._make_spec("spec2.md"), priority=100)
        set_running(self.queue_dir, e1["id"], "w-1")

        result = dequeue(self.queue_dir)
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], "q-002")

    def test_dequeue_skips_completed_specs(self):
        e1 = enqueue(self.queue_dir, self._make_spec("spec1.md"), priority=50)
        enqueue(self.queue_dir, self._make_spec("spec2.md"), priority=100)
        complete(self.queue_dir, e1["id"])

        result = dequeue(self.queue_dir)
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], "q-002")

    def test_dequeue_skips_failed_specs(self):
        e1 = enqueue(self.queue_dir, self._make_spec("spec1.md"), priority=50)
        enqueue(self.queue_dir, self._make_spec("spec2.md"), priority=100)
        fail(self.queue_dir, e1["id"], reason="max iterations")

        result = dequeue(self.queue_dir)
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], "q-002")

    def test_dequeue_includes_requeued_specs(self):
        e1 = enqueue(self.queue_dir, self._make_spec("spec1.md"), priority=50)
        enqueue(self.queue_dir, self._make_spec("spec2.md"), priority=100)
        set_running(self.queue_dir, e1["id"], "w-1")
        requeue(self.queue_dir, e1["id"], tasks_done=2, tasks_total=5)

        result = dequeue(self.queue_dir)
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], "q-001")
        self.assertEqual(result["status"], "requeued")

    def test_dequeue_skips_blocked_by_external_ids(self):
        enqueue(self.queue_dir, self._make_spec(), priority=50)

        result = dequeue(self.queue_dir, blocked_ids={"q-001"})
        self.assertIsNone(result)

    def test_dequeue_returns_none_when_all_blocked(self):
        enqueue(self.queue_dir, self._make_spec(), priority=50, blocked_by=["q-999"])

        result = dequeue(self.queue_dir)
        self.assertIsNone(result)

    def test_dequeue_does_not_change_status(self):
        enqueue(self.queue_dir, self._make_spec())
        result = dequeue(self.queue_dir)
        self.assertIsNotNone(result)

        entry = get_entry(self.queue_dir, result["id"])
        self.assertEqual(entry["status"], "queued")


# ─── DAG Blocking Tests ──────────────────────────────────────────────────────


class TestDAGBlocking(QueueTestCase):
    def test_dag_blocked_spec_not_dequeued(self):
        e1 = enqueue(self.queue_dir, self._make_spec("dep.md"), priority=100)
        enqueue(
            self.queue_dir,
            self._make_spec("blocked.md"),
            priority=50,
            blocked_by=[e1["id"]],
        )

        result = dequeue(self.queue_dir)
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], e1["id"])

    def test_dag_unblocked_after_dependency_completes(self):
        e1 = enqueue(self.queue_dir, self._make_spec("dep.md"), priority=100)
        e2 = enqueue(
            self.queue_dir,
            self._make_spec("blocked.md"),
            priority=50,
            blocked_by=[e1["id"]],
        )

        complete(self.queue_dir, e1["id"])

        result = dequeue(self.queue_dir)
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], e2["id"])

    def test_dag_multiple_dependencies(self):
        e1 = enqueue(self.queue_dir, self._make_spec("dep1.md"), priority=100)
        e2 = enqueue(self.queue_dir, self._make_spec("dep2.md"), priority=100)
        e3 = enqueue(
            self.queue_dir,
            self._make_spec("blocked.md"),
            priority=50,
            blocked_by=[e1["id"], e2["id"]],
        )

        # Complete only one dependency
        complete(self.queue_dir, e1["id"])

        result = dequeue(self.queue_dir)
        self.assertIsNotNone(result)
        # e3 still blocked (e2 not done), so should get e2
        self.assertEqual(result["id"], e2["id"])

        # Complete second dependency
        complete(self.queue_dir, e2["id"])

        result = dequeue(self.queue_dir)
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], e3["id"])

    def test_dag_nonexistent_dependency_blocks(self):
        enqueue(
            self.queue_dir,
            self._make_spec("blocked.md"),
            priority=50,
            blocked_by=["q-nonexistent"],
        )

        result = dequeue(self.queue_dir)
        self.assertIsNone(result)

    def test_dag_chain_of_dependencies(self):
        e1 = enqueue(self.queue_dir, self._make_spec("first.md"), priority=100)
        e2 = enqueue(
            self.queue_dir,
            self._make_spec("second.md"),
            priority=100,
            blocked_by=[e1["id"]],
        )
        e3 = enqueue(
            self.queue_dir,
            self._make_spec("third.md"),
            priority=100,
            blocked_by=[e2["id"]],
        )

        # Only e1 should be dequeue-able
        result = dequeue(self.queue_dir)
        self.assertEqual(result["id"], e1["id"])

        # Complete e1 -> e2 unblocked
        complete(self.queue_dir, e1["id"])
        result = dequeue(self.queue_dir)
        self.assertEqual(result["id"], e2["id"])

        # Complete e2 -> e3 unblocked
        complete(self.queue_dir, e2["id"])
        result = dequeue(self.queue_dir)
        self.assertEqual(result["id"], e3["id"])


# ─── Priority Ordering Tests ─────────────────────────────────────────────────


class TestPriorityOrdering(QueueTestCase):
    def test_lower_number_is_higher_priority(self):
        enqueue(self.queue_dir, self._make_spec("low.md"), priority=200)
        enqueue(self.queue_dir, self._make_spec("high.md"), priority=10)
        enqueue(self.queue_dir, self._make_spec("mid.md"), priority=100)

        result = dequeue(self.queue_dir)
        self.assertEqual(result["priority"], 10)

    def test_same_priority_returns_all(self):
        enqueue(self.queue_dir, self._make_spec("a.md"), priority=100)
        enqueue(self.queue_dir, self._make_spec("b.md"), priority=100)

        entries = get_queue(self.queue_dir)
        queued = [e for e in entries if e["status"] == "queued"]
        self.assertEqual(len(queued), 2)

    def test_requeued_keeps_priority(self):
        e1 = enqueue(self.queue_dir, self._make_spec(), priority=50)
        set_running(self.queue_dir, e1["id"], "w-1")
        requeue(self.queue_dir, e1["id"], tasks_done=2, tasks_total=5)

        entry = get_entry(self.queue_dir, e1["id"])
        self.assertEqual(entry["priority"], 50)

    def test_get_queue_sorted_by_priority(self):
        enqueue(self.queue_dir, self._make_spec("c.md"), priority=300)
        enqueue(self.queue_dir, self._make_spec("a.md"), priority=100)
        enqueue(self.queue_dir, self._make_spec("b.md"), priority=200)

        entries = get_queue(self.queue_dir)
        priorities = [e["priority"] for e in entries]
        self.assertEqual(priorities, [100, 200, 300])


# ─── Status Transition Tests ─────────────────────────────────────────────────


class TestStatusTransitions(QueueTestCase):
    def test_queued_to_running(self):
        e = enqueue(self.queue_dir, self._make_spec())
        self.assertEqual(e["status"], "queued")

        set_running(self.queue_dir, e["id"], "w-1")
        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["status"], "running")
        self.assertEqual(entry["last_worker"], "w-1")
        self.assertEqual(entry["iteration"], 1)
        self.assertIsNotNone(entry["last_iteration_at"])

    def test_running_to_requeued(self):
        e = enqueue(self.queue_dir, self._make_spec())
        set_running(self.queue_dir, e["id"], "w-1")
        requeue(self.queue_dir, e["id"], tasks_done=3, tasks_total=8)

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["status"], "requeued")
        self.assertEqual(entry["tasks_done"], 3)
        self.assertEqual(entry["tasks_total"], 8)

    def test_running_to_completed(self):
        e = enqueue(self.queue_dir, self._make_spec())
        set_running(self.queue_dir, e["id"], "w-1")
        complete(self.queue_dir, e["id"], tasks_done=5, tasks_total=5)

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["status"], "completed")
        self.assertEqual(entry["tasks_done"], 5)
        self.assertEqual(entry["tasks_total"], 5)

    def test_running_to_failed(self):
        e = enqueue(self.queue_dir, self._make_spec())
        set_running(self.queue_dir, e["id"], "w-1")
        fail(self.queue_dir, e["id"], reason="max iterations reached")

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["status"], "failed")
        self.assertEqual(entry["failure_reason"], "max iterations reached")

    def test_requeued_to_running_increments_iteration(self):
        e = enqueue(self.queue_dir, self._make_spec())
        set_running(self.queue_dir, e["id"], "w-1")
        requeue(self.queue_dir, e["id"])
        set_running(self.queue_dir, e["id"], "w-2")

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["iteration"], 2)
        self.assertEqual(entry["last_worker"], "w-2")

    def test_multiple_iterations_track_count(self):
        e = enqueue(self.queue_dir, self._make_spec())
        for i in range(5):
            set_running(self.queue_dir, e["id"], f"w-{i}")
            requeue(self.queue_dir, e["id"])

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["iteration"], 5)


# ─── Requeue Tests ────────────────────────────────────────────────────────────


class TestRequeue(QueueTestCase):
    def test_requeue_resets_consecutive_failures(self):
        e = enqueue(self.queue_dir, self._make_spec())
        set_running(self.queue_dir, e["id"], "w-1")

        record_failure(self.queue_dir, e["id"])
        record_failure(self.queue_dir, e["id"])
        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["consecutive_failures"], 2)

        requeue(self.queue_dir, e["id"], tasks_done=1, tasks_total=5)
        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["consecutive_failures"], 0)

    def test_requeue_updates_task_counts(self):
        e = enqueue(self.queue_dir, self._make_spec())
        set_running(self.queue_dir, e["id"], "w-1")
        requeue(self.queue_dir, e["id"], tasks_done=3, tasks_total=10)

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["tasks_done"], 3)
        self.assertEqual(entry["tasks_total"], 10)


# ─── Consecutive Failure Tests ────────────────────────────────────────────────


class TestConsecutiveFailures(QueueTestCase):
    def test_record_failure_increments(self):
        e = enqueue(self.queue_dir, self._make_spec())
        set_running(self.queue_dir, e["id"], "w-1")

        exceeded = record_failure(self.queue_dir, e["id"])
        self.assertFalse(exceeded)

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["consecutive_failures"], 1)

    def test_max_consecutive_failures_exceeded(self):
        e = enqueue(self.queue_dir, self._make_spec())
        set_running(self.queue_dir, e["id"], "w-1")

        for i in range(MAX_CONSECUTIVE_FAILURES - 1):
            exceeded = record_failure(self.queue_dir, e["id"])
            self.assertFalse(exceeded)

        exceeded = record_failure(self.queue_dir, e["id"])
        self.assertTrue(exceeded)

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["consecutive_failures"], MAX_CONSECUTIVE_FAILURES)

    def test_failure_count_persists_across_reads(self):
        e = enqueue(self.queue_dir, self._make_spec())
        record_failure(self.queue_dir, e["id"])
        record_failure(self.queue_dir, e["id"])

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["consecutive_failures"], 2)


# ─── Cancel Tests ─────────────────────────────────────────────────────────────


class TestCancel(QueueTestCase):
    def test_cancel_sets_status(self):
        e = enqueue(self.queue_dir, self._make_spec())
        cancel(self.queue_dir, e["id"])

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["status"], "canceled")

    def test_canceled_not_dequeued(self):
        e = enqueue(self.queue_dir, self._make_spec())
        cancel(self.queue_dir, e["id"])

        result = dequeue(self.queue_dir)
        self.assertIsNone(result)


# ─── Get Queue Tests ──────────────────────────────────────────────────────────


class TestGetQueue(QueueTestCase):
    def test_empty_queue(self):
        entries = get_queue(self.queue_dir)
        self.assertEqual(entries, [])

    def test_nonexistent_directory(self):
        entries = get_queue(os.path.join(self._tmpdir.name, "nonexistent"))
        self.assertEqual(entries, [])

    def test_returns_all_entries(self):
        enqueue(self.queue_dir, self._make_spec("spec1.md"))
        enqueue(self.queue_dir, self._make_spec("spec2.md"))
        enqueue(self.queue_dir, self._make_spec("spec3.md"))

        entries = get_queue(self.queue_dir)
        self.assertEqual(len(entries), 3)

    def test_skips_telemetry_files(self):
        enqueue(self.queue_dir, self._make_spec())

        telemetry_path = Path(self.queue_dir) / "q-001.telemetry.json"
        telemetry_path.write_text('{"iterations": 3}')

        entries = get_queue(self.queue_dir)
        self.assertEqual(len(entries), 1)

    def test_skips_iteration_files(self):
        enqueue(self.queue_dir, self._make_spec())

        iter_path = Path(self.queue_dir) / "q-001.iteration-1.json"
        iter_path.write_text('{"tasks_done": 2}')

        entries = get_queue(self.queue_dir)
        self.assertEqual(len(entries), 1)

    def test_skips_malformed_json(self):
        enqueue(self.queue_dir, self._make_spec())

        bad_path = Path(self.queue_dir) / "q-999.json"
        bad_path.write_text("not json at all")

        entries = get_queue(self.queue_dir)
        self.assertEqual(len(entries), 1)

    def test_skips_files_without_id(self):
        enqueue(self.queue_dir, self._make_spec())

        no_id_path = Path(self.queue_dir) / "q-998.json"
        no_id_path.write_text('{"status": "queued"}')

        entries = get_queue(self.queue_dir)
        self.assertEqual(len(entries), 1)


# ─── Get Entry Tests ──────────────────────────────────────────────────────────


class TestGetEntry(QueueTestCase):
    def test_get_existing_entry(self):
        e = enqueue(self.queue_dir, self._make_spec())
        entry = get_entry(self.queue_dir, e["id"])
        self.assertIsNotNone(entry)
        self.assertEqual(entry["id"], e["id"])

    def test_get_nonexistent_entry(self):
        entry = get_entry(self.queue_dir, "q-999")
        self.assertIsNone(entry)


# ─── Update Task Counts Tests ────────────────────────────────────────────────


class TestUpdateTaskCounts(QueueTestCase):
    def test_update_counts(self):
        e = enqueue(self.queue_dir, self._make_spec())
        update_task_counts(self.queue_dir, e["id"], tasks_done=5, tasks_total=8)

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["tasks_done"], 5)
        self.assertEqual(entry["tasks_total"], 8)

    def test_update_nonexistent_raises(self):
        with self.assertRaises(ValueError):
            update_task_counts(self.queue_dir, "q-999", tasks_done=1, tasks_total=1)


# ─── Error Handling Tests ─────────────────────────────────────────────────────


class TestErrorHandling(QueueTestCase):
    def test_set_running_nonexistent_raises(self):
        with self.assertRaises(ValueError):
            set_running(self.queue_dir, "q-999", "w-1")

    def test_requeue_nonexistent_raises(self):
        with self.assertRaises(ValueError):
            requeue(self.queue_dir, "q-999")

    def test_complete_nonexistent_raises(self):
        with self.assertRaises(ValueError):
            complete(self.queue_dir, "q-999")

    def test_fail_nonexistent_raises(self):
        with self.assertRaises(ValueError):
            fail(self.queue_dir, "q-999")

    def test_record_failure_nonexistent_raises(self):
        with self.assertRaises(ValueError):
            record_failure(self.queue_dir, "q-999")

    def test_cancel_nonexistent_raises(self):
        with self.assertRaises(ValueError):
            cancel(self.queue_dir, "q-999")


# ─── Atomic Write Tests ──────────────────────────────────────────────────────


class TestAtomicWrites(QueueTestCase):
    def test_no_tmp_files_left_behind(self):
        enqueue(self.queue_dir, self._make_spec())

        tmp_files = list(Path(self.queue_dir).glob("*.tmp"))
        self.assertEqual(len(tmp_files), 0)

    def test_unique_ids_across_many_enqueues(self):
        """Multiple enqueue calls generate unique IDs."""
        ids = set()
        for i in range(20):
            entry = enqueue(self.queue_dir, self._make_spec(f"spec{i}.md"))
            ids.add(entry["id"])
        self.assertEqual(len(ids), 20)


# ─── End-to-End Queue Lifecycle Tests ─────────────────────────────────────────


class TestQueueLifecycle(QueueTestCase):
    def test_full_lifecycle_single_spec(self):
        """queued -> running -> requeued -> running -> completed."""
        e = enqueue(self.queue_dir, self._make_spec())
        self.assertEqual(get_entry(self.queue_dir, e["id"])["status"], "queued")

        # Iteration 1: pick up, do some work, requeue
        set_running(self.queue_dir, e["id"], "w-1")
        self.assertEqual(get_entry(self.queue_dir, e["id"])["status"], "running")

        requeue(self.queue_dir, e["id"], tasks_done=3, tasks_total=5)
        self.assertEqual(get_entry(self.queue_dir, e["id"])["status"], "requeued")
        self.assertEqual(get_entry(self.queue_dir, e["id"])["iteration"], 1)

        # Iteration 2: pick up again, complete
        set_running(self.queue_dir, e["id"], "w-2")
        self.assertEqual(get_entry(self.queue_dir, e["id"])["iteration"], 2)

        complete(self.queue_dir, e["id"], tasks_done=5, tasks_total=5)
        self.assertEqual(get_entry(self.queue_dir, e["id"])["status"], "completed")

    def test_full_lifecycle_with_repeated_failures(self):
        """queued -> running -> failure cycle -> still trackable."""
        e = enqueue(self.queue_dir, self._make_spec(), max_iterations=3)

        for i in range(3):
            set_running(self.queue_dir, e["id"], f"w-{i}")
            record_failure(self.queue_dir, e["id"])
            requeue(self.queue_dir, e["id"])

        entry = get_entry(self.queue_dir, e["id"])
        # After 3 iterations
        self.assertEqual(entry["iteration"], 3)

    def test_multi_spec_queue_drain(self):
        """Multiple specs get dequeued in priority order and complete."""
        e1 = enqueue(self.queue_dir, self._make_spec("high.md"), priority=10)
        e2 = enqueue(self.queue_dir, self._make_spec("mid.md"), priority=50)
        e3 = enqueue(self.queue_dir, self._make_spec("low.md"), priority=100)

        # Worker picks up highest priority
        picked = dequeue(self.queue_dir)
        self.assertEqual(picked["id"], e1["id"])
        set_running(self.queue_dir, e1["id"], "w-1")
        complete(self.queue_dir, e1["id"])

        # Next highest
        picked = dequeue(self.queue_dir)
        self.assertEqual(picked["id"], e2["id"])
        set_running(self.queue_dir, e2["id"], "w-1")
        complete(self.queue_dir, e2["id"])

        # Last one
        picked = dequeue(self.queue_dir)
        self.assertEqual(picked["id"], e3["id"])
        set_running(self.queue_dir, e3["id"], "w-1")
        complete(self.queue_dir, e3["id"])

        # Queue is drained
        self.assertIsNone(dequeue(self.queue_dir))

    def test_dag_lifecycle(self):
        """Specs with DAG dependencies complete in correct order."""
        e1 = enqueue(self.queue_dir, self._make_spec("base.md"), priority=100)
        e2 = enqueue(
            self.queue_dir,
            self._make_spec("dependent.md"),
            priority=50,
            blocked_by=[e1["id"]],
        )

        # e2 has higher priority (50) but is blocked
        picked = dequeue(self.queue_dir)
        self.assertEqual(picked["id"], e1["id"])

        set_running(self.queue_dir, e1["id"], "w-1")
        complete(self.queue_dir, e1["id"])

        # Now e2 is unblocked
        picked = dequeue(self.queue_dir)
        self.assertEqual(picked["id"], e2["id"])

        set_running(self.queue_dir, e2["id"], "w-1")
        complete(self.queue_dir, e2["id"])

        self.assertIsNone(dequeue(self.queue_dir))


# ─── Purge Tests ─────────────────────────────────────────────────────────────


class TestPurge(QueueTestCase):
    """Tests for the purge() function."""

    def setUp(self):
        super().setUp()
        self.log_dir = os.path.join(self._tmpdir.name, "logs")
        os.makedirs(self.log_dir)

    def _create_ancillary_files(self, queue_id: str) -> list[str]:
        """Create telemetry, iteration, pid, exit, and log files for a queue entry."""
        created = []
        # Telemetry file
        tel = Path(self.queue_dir) / f"{queue_id}.telemetry.json"
        tel.write_text('{"iterations": 2}')
        created.append(str(tel))
        # Iteration files
        for i in [1, 2]:
            it = Path(self.queue_dir) / f"{queue_id}.iteration-{i}.json"
            it.write_text(f'{{"iteration": {i}}}')
            created.append(str(it))
        # PID and exit files
        pid = Path(self.queue_dir) / f"{queue_id}.pid"
        pid.write_text("12345")
        created.append(str(pid))
        ext = Path(self.queue_dir) / f"{queue_id}.exit"
        ext.write_text("0")
        created.append(str(ext))
        # Log files
        for i in [1, 2]:
            log = Path(self.log_dir) / f"{queue_id}-iter-{i}.log"
            log.write_text(f"log for iteration {i}")
            created.append(str(log))
        return created

    def test_purge_removes_completed_specs(self):
        e = enqueue(self.queue_dir, self._make_spec())
        complete(self.queue_dir, e["id"])
        self._create_ancillary_files(e["id"])

        results = purge(self.queue_dir, self.log_dir)
        self.assertEqual(len(results), 1)
        self.assertEqual(results[0]["id"], e["id"])
        self.assertEqual(results[0]["status"], "completed")

        # Entry file should be gone
        self.assertIsNone(get_entry(self.queue_dir, e["id"]))

    def test_purge_removes_failed_specs(self):
        e = enqueue(self.queue_dir, self._make_spec())
        fail(self.queue_dir, e["id"], reason="crash")

        results = purge(self.queue_dir, self.log_dir)
        self.assertEqual(len(results), 1)
        self.assertEqual(results[0]["status"], "failed")
        self.assertIsNone(get_entry(self.queue_dir, e["id"]))

    def test_purge_removes_canceled_specs(self):
        e = enqueue(self.queue_dir, self._make_spec())
        cancel(self.queue_dir, e["id"])

        results = purge(self.queue_dir, self.log_dir)
        self.assertEqual(len(results), 1)
        self.assertEqual(results[0]["status"], "canceled")

    def test_purge_skips_queued_by_default(self):
        enqueue(self.queue_dir, self._make_spec())

        results = purge(self.queue_dir, self.log_dir)
        self.assertEqual(len(results), 0)
        # Entry should still exist
        self.assertEqual(len(get_queue(self.queue_dir)), 1)

    def test_purge_skips_running_by_default(self):
        e = enqueue(self.queue_dir, self._make_spec())
        set_running(self.queue_dir, e["id"], "w-1")

        results = purge(self.queue_dir, self.log_dir)
        self.assertEqual(len(results), 0)

    def test_purge_all_removes_everything(self):
        e1 = enqueue(self.queue_dir, self._make_spec("spec1.md"))
        e2 = enqueue(self.queue_dir, self._make_spec("spec2.md"))
        complete(self.queue_dir, e1["id"])
        # e2 is still queued

        all_statuses = [
            "queued",
            "running",
            "requeued",
            "completed",
            "failed",
            "canceled",
        ]
        results = purge(self.queue_dir, self.log_dir, statuses=all_statuses)
        self.assertEqual(len(results), 2)
        self.assertEqual(len(get_queue(self.queue_dir)), 0)

    def test_purge_dry_run_does_not_delete(self):
        e = enqueue(self.queue_dir, self._make_spec())
        complete(self.queue_dir, e["id"])
        self._create_ancillary_files(e["id"])

        results = purge(self.queue_dir, self.log_dir, dry_run=True)
        self.assertEqual(len(results), 1)
        self.assertGreater(len(results[0]["files_removed"]), 0)

        # Files should still exist
        self.assertIsNotNone(get_entry(self.queue_dir, e["id"]))
        self.assertTrue(Path(self.queue_dir, f"{e['id']}.telemetry.json").is_file())

    def test_purge_removes_all_ancillary_files(self):
        e = enqueue(self.queue_dir, self._make_spec())
        complete(self.queue_dir, e["id"])
        created = self._create_ancillary_files(e["id"])

        results = purge(self.queue_dir, self.log_dir)

        # All created files plus the entry file itself should be removed
        for fp in created:
            self.assertFalse(Path(fp).exists(), f"File should be removed: {fp}")
        self.assertIsNone(get_entry(self.queue_dir, e["id"]))

    def test_purge_empty_queue_returns_empty(self):
        results = purge(self.queue_dir, self.log_dir)
        self.assertEqual(results, [])

    def test_purge_mixed_statuses(self):
        e1 = enqueue(self.queue_dir, self._make_spec("a.md"))
        e2 = enqueue(self.queue_dir, self._make_spec("b.md"))
        e3 = enqueue(self.queue_dir, self._make_spec("c.md"))
        complete(self.queue_dir, e1["id"])
        fail(self.queue_dir, e2["id"])
        # e3 stays queued

        results = purge(self.queue_dir, self.log_dir)
        purged_ids = {r["id"] for r in results}
        self.assertEqual(purged_ids, {e1["id"], e2["id"]})
        # e3 should survive
        self.assertIsNotNone(get_entry(self.queue_dir, e3["id"]))

    def test_purge_removes_spec_copy_file(self):
        """Purge should also remove the spec copy in the queue directory."""
        e = enqueue(self.queue_dir, self._make_spec())
        spec_copy = Path(self.queue_dir) / f"{e['id']}.spec.md"
        self.assertTrue(spec_copy.is_file())

        complete(self.queue_dir, e["id"])
        purge(self.queue_dir, self.log_dir)

        self.assertFalse(spec_copy.is_file())


# ─── Locking Tests ───────────────────────────────────────────────────────────


class TestQueueLocking(QueueTestCase):
    """Tests for flock-based queue locking."""

    def test_lock_file_created_on_enqueue(self):
        """Enqueue should create .lock file in queue_dir."""
        enqueue(self.queue_dir, self._make_spec())
        lock_path = Path(self.queue_dir) / ".lock"
        self.assertTrue(lock_path.exists())

    def test_lock_released_after_enqueue(self):
        """After enqueue completes, the lock should be released (re-acquirable)."""
        import fcntl

        enqueue(self.queue_dir, self._make_spec())

        # Should be able to acquire the lock non-blocking
        lock_path = Path(self.queue_dir) / ".lock"
        with open(lock_path, "w") as fd:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            fcntl.flock(fd, fcntl.LOCK_UN)

    def test_lock_released_on_exception(self):
        """Lock should be released even if the operation raises."""
        import fcntl

        # set_running on a nonexistent ID raises ValueError
        enqueue(self.queue_dir, self._make_spec())  # ensure queue_dir exists with lock
        with self.assertRaises(ValueError):
            set_running(self.queue_dir, "q-nonexistent", "w-1")

        # Lock should still be acquirable
        lock_path = Path(self.queue_dir) / ".lock"
        with open(lock_path, "w") as fd:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            fcntl.flock(fd, fcntl.LOCK_UN)

    def test_concurrent_enqueue_unique_ids(self):
        """Sequential enqueue calls under the same lock produce unique IDs."""
        ids = set()
        for i in range(10):
            entry = enqueue(self.queue_dir, self._make_spec(f"spec{i}.md"))
            ids.add(entry["id"])
        self.assertEqual(len(ids), 10)

    def test_id_generation_inside_lock(self):
        """ID generation happens inside the lock, preventing TOCTOU.

        Verify that even after manually creating a file that would collide
        with the next auto-generated ID, enqueue still produces the correct
        next ID because it reads the directory listing under lock.
        """
        # Enqueue first to get q-001
        enqueue(self.queue_dir, self._make_spec("spec1.md"))

        # Manually create q-002.json (simulating a race)
        collision_path = Path(self.queue_dir) / "q-002.json"
        collision_path.write_text('{"id": "q-002", "status": "queued"}')

        # Next enqueue should skip q-002 and create q-003
        entry = enqueue(self.queue_dir, self._make_spec("spec3.md"))
        self.assertEqual(entry["id"], "q-003")


# ─── Queue Lock Context Manager Tests ────────────────────────────────────────


class TestQueueLockContextManager(QueueTestCase):
    """Tests for the queue_lock context manager itself."""

    def test_lock_is_exclusive(self):
        """Second non-blocking lock attempt should fail while lock is held."""
        import fcntl

        from lib.locking import queue_lock

        # Ensure queue dir exists
        os.makedirs(self.queue_dir, exist_ok=True)

        with queue_lock(self.queue_dir):
            lock_path = Path(self.queue_dir) / ".lock"
            with open(lock_path, "w") as fd:
                with self.assertRaises((BlockingIOError, OSError)):
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)

    def test_lock_creates_directory(self):
        """queue_lock should create the queue_dir if it doesn't exist."""
        from lib.locking import queue_lock

        new_dir = os.path.join(self._tmpdir.name, "new_queue_dir")
        self.assertFalse(os.path.exists(new_dir))

        with queue_lock(new_dir):
            self.assertTrue(os.path.isdir(new_dir))

    def test_lock_reentrant_after_release(self):
        """Lock can be acquired again after it's released."""
        from lib.locking import queue_lock

        os.makedirs(self.queue_dir, exist_ok=True)

        with queue_lock(self.queue_dir):
            pass

        # Should succeed without blocking
        with queue_lock(self.queue_dir):
            pass


# ─── Spec Copy Tests (t-2) ───────────────────────────────────────────────────


class TestSpecCopy(QueueTestCase):
    """Tests for spec file copy on dispatch (t-2)."""

    def test_spec_copy_exists_in_queue_dir(self):
        """Enqueue should create a copy of the spec file in queue_dir."""
        spec = self._make_spec("my-spec.md", content="# My Spec\n")
        entry = enqueue(self.queue_dir, spec)

        copy_path = Path(self.queue_dir) / f"{entry['id']}.spec.md"
        self.assertTrue(copy_path.is_file())
        self.assertEqual(copy_path.read_text(), "# My Spec\n")

    def test_spec_copy_is_independent(self):
        """Modifying the original spec after enqueue should not affect the copy."""
        spec = self._make_spec("original.md", content="# Original\n")
        entry = enqueue(self.queue_dir, spec)

        # Modify original
        with open(spec, "w") as f:
            f.write("# Modified\n")

        # Copy should be unchanged
        copy_path = Path(self.queue_dir) / f"{entry['id']}.spec.md"
        self.assertEqual(copy_path.read_text(), "# Original\n")

    def test_original_spec_path_field_present(self):
        """Queue entry should have original_spec_path pointing to user's file."""
        spec = self._make_spec()
        entry = enqueue(self.queue_dir, spec)

        self.assertEqual(entry["original_spec_path"], os.path.abspath(spec))
        # spec_path should point to the copy
        self.assertNotEqual(entry["spec_path"], entry["original_spec_path"])
        self.assertIn(entry["id"], entry["spec_path"])

    def test_spec_path_points_to_copy(self):
        """spec_path in the entry should be the copy, not the original."""
        spec = self._make_spec()
        entry = enqueue(self.queue_dir, spec)

        self.assertTrue(os.path.isfile(entry["spec_path"]))
        self.assertIn(".spec.md", entry["spec_path"])


# ─── Duplicate Detection Tests (t-2) ─────────────────────────────────────────


class TestDuplicateDetection(QueueTestCase):
    """Tests for duplicate spec dispatch rejection (t-2)."""

    def test_duplicate_dispatch_rejected_queued(self):
        """Dispatching the same spec while it's queued should raise."""
        spec = self._make_spec()
        enqueue(self.queue_dir, spec)

        with self.assertRaises(DuplicateSpecError) as ctx:
            enqueue(self.queue_dir, spec)

        self.assertIn("q-001", str(ctx.exception))
        self.assertIn("queued", str(ctx.exception))

    def test_duplicate_dispatch_rejected_running(self):
        """Dispatching the same spec while it's running should raise."""
        spec = self._make_spec()
        e = enqueue(self.queue_dir, spec)
        set_running(self.queue_dir, e["id"], "w-1")

        with self.assertRaises(DuplicateSpecError):
            enqueue(self.queue_dir, spec)

    def test_duplicate_dispatch_rejected_requeued(self):
        """Dispatching the same spec while it's requeued should raise."""
        spec = self._make_spec()
        e = enqueue(self.queue_dir, spec)
        set_running(self.queue_dir, e["id"], "w-1")
        requeue(self.queue_dir, e["id"])

        with self.assertRaises(DuplicateSpecError):
            enqueue(self.queue_dir, spec)

    def test_duplicate_allowed_after_completion(self):
        """Dispatching the same spec after completion should succeed."""
        spec = self._make_spec()
        e = enqueue(self.queue_dir, spec)
        complete(self.queue_dir, e["id"])

        # Should not raise
        e2 = enqueue(self.queue_dir, spec)
        self.assertEqual(e2["id"], "q-002")

    def test_duplicate_allowed_after_cancel(self):
        """Dispatching the same spec after cancel should succeed."""
        spec = self._make_spec()
        e = enqueue(self.queue_dir, spec)
        cancel(self.queue_dir, e["id"])

        # Should not raise
        e2 = enqueue(self.queue_dir, spec)
        self.assertEqual(e2["id"], "q-002")

    def test_duplicate_allowed_after_failure(self):
        """Dispatching the same spec after failure should succeed."""
        spec = self._make_spec()
        e = enqueue(self.queue_dir, spec)
        fail(self.queue_dir, e["id"])

        # Should not raise
        e2 = enqueue(self.queue_dir, spec)
        self.assertEqual(e2["id"], "q-002")

    def test_different_specs_allowed(self):
        """Dispatching different specs should always succeed."""
        spec1 = self._make_spec("spec1.md")
        spec2 = self._make_spec("spec2.md")

        e1 = enqueue(self.queue_dir, spec1)
        e2 = enqueue(self.queue_dir, spec2)

        self.assertEqual(e1["id"], "q-001")
        self.assertEqual(e2["id"], "q-002")

    def test_duplicate_error_has_useful_fields(self):
        """DuplicateSpecError should have original_spec_path, existing_id, existing_status."""
        spec = self._make_spec()
        enqueue(self.queue_dir, spec)

        with self.assertRaises(DuplicateSpecError) as ctx:
            enqueue(self.queue_dir, spec)

        err = ctx.exception
        self.assertEqual(err.original_spec_path, os.path.abspath(spec))
        self.assertEqual(err.existing_id, "q-001")
        self.assertEqual(err.existing_status, "queued")


# ─── Sync Back Tests (t-2) ───────────────────────────────────────────────────


class TestSyncBack(QueueTestCase):
    """Tests for sync_back on completion (t-2)."""

    def test_sync_back_on_completion(self):
        """Completing a spec should copy the final version back to original location."""
        spec = self._make_spec("original.md", content="# Original\n")
        e = enqueue(self.queue_dir, spec)

        # Modify the copy (simulating Claude's edits)
        copy_path = e["spec_path"]
        with open(copy_path, "w") as f:
            f.write("# Modified by Claude\n")

        # Complete should sync back
        complete(self.queue_dir, e["id"])

        # Original should now have Claude's changes
        with open(spec) as f:
            self.assertEqual(f.read(), "# Modified by Claude\n")

    def test_sync_back_disabled(self):
        """When sync_back=False, completion should not modify the original."""
        spec = self._make_spec("original.md", content="# Original\n")
        e = enqueue(self.queue_dir, spec, sync_back=False)

        # Modify the copy
        copy_path = e["spec_path"]
        with open(copy_path, "w") as f:
            f.write("# Modified by Claude\n")

        # Complete should NOT sync back
        complete(self.queue_dir, e["id"])

        # Original should be unchanged
        with open(spec) as f:
            self.assertEqual(f.read(), "# Original\n")

    def test_sync_back_default_is_true(self):
        """sync_back should default to True."""
        spec = self._make_spec()
        e = enqueue(self.queue_dir, spec)
        self.assertTrue(e["sync_back"])

    def test_sync_back_spec_function(self):
        """sync_back_spec should work independently of complete()."""
        spec = self._make_spec("original.md", content="# Start\n")
        e = enqueue(self.queue_dir, spec)

        # Modify the copy
        with open(e["spec_path"], "w") as f:
            f.write("# Updated\n")

        result = sync_back_spec(self.queue_dir, e["id"])
        self.assertTrue(result)

        with open(spec) as f:
            self.assertEqual(f.read(), "# Updated\n")

    def test_sync_back_spec_returns_false_when_disabled(self):
        """sync_back_spec should return False when sync_back is disabled."""
        spec = self._make_spec()
        e = enqueue(self.queue_dir, spec, sync_back=False)

        result = sync_back_spec(self.queue_dir, e["id"])
        self.assertFalse(result)

    def test_sync_back_spec_returns_false_for_missing_entry(self):
        """sync_back_spec should return False for nonexistent entries."""
        result = sync_back_spec(self.queue_dir, "q-999")
        self.assertFalse(result)


# ─── Crash Recovery Tests (t-3) ──────────────────────────────────────────────


class TestRecoverRunningSpecs(QueueTestCase):
    """Tests for recover_running_specs() (t-3)."""

    def test_recover_running_spec_dead_pid(self):
        """Running spec with dead/missing PID should be reset to requeued."""
        from lib.queue import recover_running_specs

        spec = self._make_spec()
        e = enqueue(self.queue_dir, spec)
        set_running(self.queue_dir, e["id"], "w-1")

        # Confirm it's running
        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["status"], "running")
        old_iteration = entry["iteration"]

        # No PID file exists, so recovery should kick in
        count = recover_running_specs(self.queue_dir)
        self.assertEqual(count, 1)

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["status"], "requeued")
        self.assertEqual(entry["iteration"], old_iteration + 1)

    def test_recover_running_spec_with_dead_pid_file(self):
        """Running spec with PID file pointing to dead process should be recovered."""
        from lib.queue import recover_running_specs

        spec = self._make_spec()
        e = enqueue(self.queue_dir, spec)
        set_running(self.queue_dir, e["id"], "w-1")

        # Write a PID file with a definitely-dead PID
        pid_file = os.path.join(self.queue_dir, f"{e['id']}.pid")
        with open(pid_file, "w") as f:
            f.write("999999999")  # PID that almost certainly doesn't exist

        count = recover_running_specs(self.queue_dir)
        self.assertEqual(count, 1)

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["status"], "requeued")

    def test_recover_skips_alive_pid(self):
        """Running spec with alive PID should NOT be recovered."""
        from lib.queue import recover_running_specs

        spec = self._make_spec()
        e = enqueue(self.queue_dir, spec)
        set_running(self.queue_dir, e["id"], "w-1")

        # Write PID file with our own PID (which is alive)
        pid_file = os.path.join(self.queue_dir, f"{e['id']}.pid")
        with open(pid_file, "w") as f:
            f.write(str(os.getpid()))

        count = recover_running_specs(self.queue_dir)
        self.assertEqual(count, 0)

        entry = get_entry(self.queue_dir, e["id"])
        self.assertEqual(entry["status"], "running")

    def test_recover_skips_non_running_entries(self):
        """Only entries with status 'running' should be considered for recovery."""
        from lib.queue import recover_running_specs

        spec1 = self._make_spec("s1.md")
        spec2 = self._make_spec("s2.md")
        spec3 = self._make_spec("s3.md")

        e1 = enqueue(self.queue_dir, spec1)  # queued
        e2 = enqueue(self.queue_dir, spec2)  # completed
        complete(self.queue_dir, e2["id"])
        e3 = enqueue(self.queue_dir, spec3)  # failed
        fail(self.queue_dir, e3["id"])

        count = recover_running_specs(self.queue_dir)
        self.assertEqual(count, 0)

    def test_recover_multiple_running_specs(self):
        """Multiple running specs with dead PIDs should all be recovered."""
        from lib.queue import recover_running_specs

        specs = [self._make_spec(f"s{i}.md") for i in range(3)]
        entries = [enqueue(self.queue_dir, s) for s in specs]

        for e in entries:
            set_running(self.queue_dir, e["id"], "w-1")

        count = recover_running_specs(self.queue_dir)
        self.assertEqual(count, 3)

        for e in entries:
            entry = get_entry(self.queue_dir, e["id"])
            self.assertEqual(entry["status"], "requeued")

    def test_is_pid_alive_helper(self):
        """_is_pid_alive should return True for current process, False for dead."""
        from lib.queue import _is_pid_alive

        self.assertTrue(_is_pid_alive(os.getpid()))
        self.assertFalse(_is_pid_alive(999999999))


# ─── Heartbeat Staleness Tests (t-3) ─────────────────────────────────────────


class TestHeartbeatStaleness(unittest.TestCase):
    """Tests for daemon heartbeat staleness detection logic."""

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.state_dir = self._tmpdir.name

    def tearDown(self):
        self._tmpdir.cleanup()

    def test_fresh_heartbeat_is_not_stale(self):
        """A heartbeat written just now should not be stale."""
        from datetime import datetime, timezone

        hb_file = os.path.join(self.state_dir, "daemon-heartbeat")
        ts = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        with open(hb_file, "w") as f:
            f.write(ts)

        hb_ts = Path(hb_file).read_text().strip()
        hb = datetime.fromisoformat(hb_ts.replace("Z", "+00:00"))
        now = datetime.now(timezone.utc)
        from datetime import timedelta

        self.assertFalse((now - hb) > timedelta(seconds=30))

    def test_stale_heartbeat_detected(self):
        """A heartbeat from >30s ago should be detected as stale."""
        from datetime import datetime, timedelta, timezone

        old_time = datetime.now(timezone.utc) - timedelta(seconds=60)
        hb_file = os.path.join(self.state_dir, "daemon-heartbeat")
        ts = old_time.strftime("%Y-%m-%dT%H:%M:%SZ")
        with open(hb_file, "w") as f:
            f.write(ts)

        hb_ts = Path(hb_file).read_text().strip()
        hb = datetime.fromisoformat(hb_ts.replace("Z", "+00:00"))
        now = datetime.now(timezone.utc)
        self.assertTrue((now - hb) > timedelta(seconds=30))

    def test_missing_heartbeat_file(self):
        """Missing heartbeat file should not cause errors."""
        hb_file = os.path.join(self.state_dir, "daemon-heartbeat")
        self.assertFalse(os.path.isfile(hb_file))


if __name__ == "__main__":
    unittest.main()
