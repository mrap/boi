# integration_boi.py — End-to-end integration tests for BOI.
#
# Tests the full lifecycle of specs through the queue, worker iteration
# simulation, telemetry aggregation, hooks, and DAG-based scheduling.
#
# All tests use mock data and temp directories. No live Claude calls.
# No real worktrees. No external dependencies.
#
# Scenarios:
#   1. Single spec, 3 tasks, completes in 2 iterations
#   2. Self-evolution: worker adds new tasks during iteration
#   3. DAG blocking: spec B blocked by spec A
#   4. Crash recovery: daemon detects crash, requeues spec
#   5. Max iterations: spec fails after hitting iteration limit
#   6. Queue priority: specs dispatched in priority order

import json
import os
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib import queue
from lib.event_log import read_events
from lib.hooks import (
    run_completion_hooks,
    write_spec_completed_event,
    write_spec_failed_event,
)
from lib.spec_parser import count_boi_tasks, parse_boi_spec
from lib.telemetry import update_telemetry
from tests.conftest import BoiTestCase, make_iteration_file, make_queue_entry


def _simulate_iteration_completing_tasks(
    spec_path: str,
    tasks_to_complete: int,
    tasks_to_add: int = 0,
):
    """Simulate a worker iteration that completes N tasks and optionally adds new ones.

    Reads the spec, marks the first N PENDING tasks as DONE,
    optionally appends new PENDING tasks (self-evolution).

    Returns (tasks_completed, tasks_added, pre_pending, post_pending).
    """
    content = Path(spec_path).read_text(encoding="utf-8")
    tasks = parse_boi_spec(content)

    pre_pending = sum(1 for t in tasks if t.status == "PENDING")
    completed = 0

    lines = content.splitlines()
    new_lines = []
    i = 0
    while i < len(lines):
        line = lines[i]
        # Check if this is a task heading
        if line.strip().startswith("### t-"):
            new_lines.append(line)
            i += 1
            # Look for the status line (skip blanks)
            while i < len(lines):
                stripped = lines[i].strip()
                if stripped == "":
                    new_lines.append(lines[i])
                    i += 1
                    continue
                if stripped == "PENDING" and completed < tasks_to_complete:
                    new_lines.append("DONE")
                    completed += 1
                    i += 1
                    break
                else:
                    new_lines.append(lines[i])
                    i += 1
                    break
        else:
            new_lines.append(line)
            i += 1

    # Self-evolution: add new PENDING tasks
    if tasks_to_add > 0:
        existing_ids = [t.id for t in tasks]
        max_id = 0
        for tid in existing_ids:
            try:
                num = int(tid.split("-")[1])
                if num > max_id:
                    max_id = num
            except (IndexError, ValueError):
                pass

        for j in range(tasks_to_add):
            new_id = max_id + j + 1
            new_lines.extend(
                [
                    "",
                    f"### t-{new_id}: Self-evolved task {new_id}",
                    "PENDING",
                    "",
                    "**Spec:** Additional work discovered during iteration.",
                    "",
                    f"**Verify:** echo 'task {new_id} ok'",
                ]
            )

    Path(spec_path).write_text("\n".join(new_lines), encoding="utf-8")

    post_counts = count_boi_tasks(spec_path)
    post_pending = post_counts["pending"]

    return completed, tasks_to_add, pre_pending, post_pending


def _simulate_daemon_check(
    queue_dir: str,
    queue_id: str,
    spec_path: str,
    iteration: int,
    tasks_completed: int,
    tasks_added: int,
    tasks_skipped: int,
    duration_seconds: int,
    exit_code: int,
    pre_pending: int,
    post_pending: int,
):
    """Simulate what the daemon does after a worker iteration finishes.

    1. Write iteration metadata
    2. Update telemetry
    3. Check if spec is complete, requeue, or failed
    """
    # Write iteration file
    make_iteration_file(
        queue_dir=queue_dir,
        queue_id=queue_id,
        iteration=iteration,
        tasks_completed=tasks_completed,
        tasks_added=tasks_added,
        tasks_skipped=tasks_skipped,
        duration_seconds=duration_seconds,
        exit_code=exit_code,
        pre_pending=pre_pending,
        post_pending=post_pending,
    )

    # Update telemetry
    update_telemetry(queue_dir, queue_id)

    # Read spec to get current task counts
    counts = count_boi_tasks(spec_path)
    tasks_done = counts["done"]
    tasks_total = counts["total"]

    # Check result
    entry = queue.get_entry(queue_dir, queue_id)
    if entry is None:
        raise ValueError(f"Queue entry not found: {queue_id}")

    if counts["pending"] == 0:
        # All done
        queue.complete(queue_dir, queue_id, tasks_done, tasks_total)
        return "completed"
    elif entry["iteration"] >= entry["max_iterations"]:
        # Max iterations exceeded
        queue.fail(queue_dir, queue_id, "max iterations reached")
        return "failed"
    else:
        # Still has pending tasks, requeue
        queue.requeue(queue_dir, queue_id, tasks_done, tasks_total)
        return "requeued"


# ─── Integration Test Scenarios ─────────────────────────────────────────────


class TestSingleSpecCompletesInTwoIterations(BoiTestCase):
    """Scenario 1: Single spec, 3 tasks, completes in 2 iterations.

    Submit spec with 3 PENDING tasks.
    Iteration 1: worker completes 2 tasks.
    Iteration 2: worker completes 1 task.
    Assert: spec marked completed after 2 iterations.
    """

    def test_lifecycle(self):
        # Setup: 3 PENDING tasks
        spec_path = self.create_spec(tasks_pending=3, tasks_done=0)
        entry = queue.enqueue(self.queue_dir, spec_path, priority=50)
        qid = entry["id"]

        # Verify initial state
        counts = count_boi_tasks(spec_path)
        self.assertEqual(counts["pending"], 3)
        self.assertEqual(counts["done"], 0)
        self.assertEqual(counts["total"], 3)

        # --- Iteration 1: complete 2 tasks ---
        queue.set_running(self.queue_dir, qid, "w-1")
        completed, added, pre_pend, post_pend = _simulate_iteration_completing_tasks(
            spec_path, tasks_to_complete=2
        )
        self.assertEqual(completed, 2)

        result = _simulate_daemon_check(
            queue_dir=self.queue_dir,
            queue_id=qid,
            spec_path=spec_path,
            iteration=1,
            tasks_completed=2,
            tasks_added=0,
            tasks_skipped=0,
            duration_seconds=720,
            exit_code=0,
            pre_pending=pre_pend,
            post_pending=post_pend,
        )
        self.assertEqual(result, "requeued")

        # Verify queue state after iteration 1
        entry = queue.get_entry(self.queue_dir, qid)
        self.assertEqual(entry["status"], "requeued")
        self.assertEqual(entry["tasks_done"], 2)
        self.assertEqual(entry["tasks_total"], 3)

        # --- Iteration 2: complete remaining 1 task ---
        queue.set_running(self.queue_dir, qid, "w-1")
        completed, added, pre_pend, post_pend = _simulate_iteration_completing_tasks(
            spec_path, tasks_to_complete=1
        )
        self.assertEqual(completed, 1)

        result = _simulate_daemon_check(
            queue_dir=self.queue_dir,
            queue_id=qid,
            spec_path=spec_path,
            iteration=2,
            tasks_completed=1,
            tasks_added=0,
            tasks_skipped=0,
            duration_seconds=540,
            exit_code=0,
            pre_pending=pre_pend,
            post_pending=post_pend,
        )
        self.assertEqual(result, "completed")

        # Verify final state
        entry = queue.get_entry(self.queue_dir, qid)
        self.assertEqual(entry["status"], "completed")
        self.assertEqual(entry["tasks_done"], 3)
        self.assertEqual(entry["tasks_total"], 3)

        # Verify telemetry
        from lib.telemetry import read_telemetry

        telem = read_telemetry(self.queue_dir, qid)
        self.assertIsNotNone(telem)
        self.assertEqual(telem["total_iterations"], 2)
        self.assertEqual(telem["tasks_completed_per_iteration"], [2, 1])
        self.assertEqual(telem["tasks_added_per_iteration"], [0, 0])
        self.assertEqual(telem["total_time_seconds"], 720 + 540)

        # Verify spec file itself
        final_counts = count_boi_tasks(spec_path)
        self.assertEqual(final_counts["pending"], 0)
        self.assertEqual(final_counts["done"], 3)


class TestSelfEvolution(BoiTestCase):
    """Scenario 2: Self-evolution.

    Submit spec with 2 tasks.
    Iteration 1: worker completes 1 task, adds 1 new PENDING task.
    Iteration 2: worker completes 2 remaining tasks.
    Assert: spec requeued after iter 1, completed after iter 2, 3 total tasks.
    """

    def test_self_evolution(self):
        # Setup: 2 PENDING tasks
        spec_path = self.create_spec(tasks_pending=2, tasks_done=0)
        entry = queue.enqueue(self.queue_dir, spec_path)
        qid = entry["id"]

        initial_counts = count_boi_tasks(spec_path)
        self.assertEqual(initial_counts["total"], 2)
        self.assertEqual(initial_counts["pending"], 2)

        # --- Iteration 1: complete 1 task, add 1 new task ---
        queue.set_running(self.queue_dir, qid, "w-2")
        completed, added, pre_pend, post_pend = _simulate_iteration_completing_tasks(
            spec_path, tasks_to_complete=1, tasks_to_add=1
        )
        self.assertEqual(completed, 1)
        self.assertEqual(added, 1)

        # After iter 1: 1 DONE, 1 original PENDING, 1 new PENDING = 2 PENDING
        mid_counts = count_boi_tasks(spec_path)
        self.assertEqual(mid_counts["done"], 1)
        self.assertEqual(mid_counts["pending"], 2)
        self.assertEqual(mid_counts["total"], 3)  # self-evolved!

        result = _simulate_daemon_check(
            queue_dir=self.queue_dir,
            queue_id=qid,
            spec_path=spec_path,
            iteration=1,
            tasks_completed=1,
            tasks_added=1,
            tasks_skipped=0,
            duration_seconds=900,
            exit_code=0,
            pre_pending=pre_pend,
            post_pending=post_pend,
        )
        self.assertEqual(result, "requeued")

        # --- Iteration 2: complete remaining 2 tasks ---
        queue.set_running(self.queue_dir, qid, "w-2")
        completed, added, pre_pend, post_pend = _simulate_iteration_completing_tasks(
            spec_path, tasks_to_complete=2
        )
        self.assertEqual(completed, 2)

        result = _simulate_daemon_check(
            queue_dir=self.queue_dir,
            queue_id=qid,
            spec_path=spec_path,
            iteration=2,
            tasks_completed=2,
            tasks_added=0,
            tasks_skipped=0,
            duration_seconds=1100,
            exit_code=0,
            pre_pending=pre_pend,
            post_pending=post_pend,
        )
        self.assertEqual(result, "completed")

        # Verify final state
        entry = queue.get_entry(self.queue_dir, qid)
        self.assertEqual(entry["status"], "completed")
        self.assertEqual(entry["tasks_done"], 3)
        self.assertEqual(entry["tasks_total"], 3)

        # Verify telemetry tracks the self-evolution
        from lib.telemetry import read_telemetry

        telem = read_telemetry(self.queue_dir, qid)
        self.assertIsNotNone(telem)
        self.assertEqual(telem["total_iterations"], 2)
        self.assertEqual(telem["tasks_completed_per_iteration"], [1, 2])
        self.assertEqual(telem["tasks_added_per_iteration"], [1, 0])
        total_added = sum(telem["tasks_added_per_iteration"])
        self.assertEqual(total_added, 1)

        # Verify spec file has 3 tasks all DONE
        final_counts = count_boi_tasks(spec_path)
        self.assertEqual(final_counts["pending"], 0)
        self.assertEqual(final_counts["done"], 3)
        self.assertEqual(final_counts["total"], 3)


class TestDAGBlocking(BoiTestCase):
    """Scenario 3: DAG blocking.

    Submit spec A and spec B (blocked by A).
    Assert: B stays queued until A completes. Then B runs.
    """

    def test_dag_blocking(self):
        # Setup: Spec A (no blockers) and Spec B (blocked by A)
        spec_a_path = self.create_spec(
            tasks_pending=1, tasks_done=0, filename="spec_a.md"
        )
        spec_b_path = self.create_spec(
            tasks_pending=2, tasks_done=0, filename="spec_b.md"
        )

        queue.enqueue(self.queue_dir, spec_a_path, priority=100, queue_id="q-001")
        queue.enqueue(
            self.queue_dir,
            spec_b_path,
            priority=50,
            blocked_by=["q-001"],
            queue_id="q-002",
        )

        # B has higher priority (50 < 100), but is blocked by A
        # Dequeue should return A, not B
        next_spec = queue.dequeue(self.queue_dir)
        self.assertIsNotNone(next_spec)
        self.assertEqual(next_spec["id"], "q-001")

        # B should NOT be dequeued while A is incomplete
        # Set A to running first
        queue.set_running(self.queue_dir, "q-001", "w-1")

        # Try dequeuing again (only queued/requeued are eligible)
        next_spec = queue.dequeue(self.queue_dir)
        # B is still blocked, and A is running, so nothing available
        self.assertIsNone(next_spec)

        # Complete spec A
        _simulate_iteration_completing_tasks(spec_a_path, tasks_to_complete=1)
        queue.complete(self.queue_dir, "q-001", tasks_done=1, tasks_total=1)

        # Now B should be unblocked
        next_spec = queue.dequeue(self.queue_dir)
        self.assertIsNotNone(next_spec)
        self.assertEqual(next_spec["id"], "q-002")

        # Queue's DAG blocking already verified above via dequeue behavior

    def test_dag_transitive_blocking(self):
        """Spec C blocked by B, which is blocked by A. C can't run until both complete."""
        spec_a = self.create_spec(tasks_pending=1, filename="a.md")
        spec_b = self.create_spec(tasks_pending=1, filename="b.md")
        spec_c = self.create_spec(tasks_pending=1, filename="c.md")

        queue.enqueue(self.queue_dir, spec_a, queue_id="q-001")
        queue.enqueue(self.queue_dir, spec_b, blocked_by=["q-001"], queue_id="q-002")
        queue.enqueue(self.queue_dir, spec_c, blocked_by=["q-002"], queue_id="q-003")

        # Only A should be dequeueable
        next_spec = queue.dequeue(self.queue_dir)
        self.assertEqual(next_spec["id"], "q-001")

        # Complete A
        queue.set_running(self.queue_dir, "q-001", "w-1")
        queue.complete(self.queue_dir, "q-001")

        # Now B should be dequeueable, but not C
        next_spec = queue.dequeue(self.queue_dir)
        self.assertEqual(next_spec["id"], "q-002")

        # Complete B
        queue.set_running(self.queue_dir, "q-002", "w-1")
        queue.complete(self.queue_dir, "q-002")

        # Now C should be dequeueable
        next_spec = queue.dequeue(self.queue_dir)
        self.assertEqual(next_spec["id"], "q-003")


class TestCrashRecovery(BoiTestCase):
    """Scenario 4: Crash recovery.

    Submit spec. Worker crashes mid-iteration.
    Daemon detects crash, records failure, requeues spec.
    Next iteration picks it up and completes successfully.
    """

    def test_crash_and_recovery(self):
        spec_path = self.create_spec(tasks_pending=2, tasks_done=0)
        entry = queue.enqueue(self.queue_dir, spec_path)
        qid = entry["id"]

        # --- Iteration 1: Worker crashes (simulated) ---
        queue.set_running(self.queue_dir, qid, "w-1")

        # Record failure (daemon detects dead PID)
        max_exceeded = queue.record_failure(self.queue_dir, qid)
        self.assertFalse(max_exceeded)  # 1 failure, not at max yet

        # Daemon requeues with the crash tracked
        entry = queue.get_entry(self.queue_dir, qid)
        self.assertEqual(entry["consecutive_failures"], 1)
        self.assertIn("cooldown_until", entry)

        # Write a crash iteration file (exit code != 0, no tasks completed)
        make_iteration_file(
            queue_dir=self.queue_dir,
            queue_id=qid,
            iteration=1,
            tasks_completed=0,
            tasks_added=0,
            tasks_skipped=0,
            duration_seconds=30,
            exit_code=1,
        )

        # Requeue the spec
        queue.requeue(self.queue_dir, qid, tasks_done=0, tasks_total=2)

        # After successful requeue, consecutive_failures should reset
        entry = queue.get_entry(self.queue_dir, qid)
        self.assertEqual(entry["status"], "requeued")
        self.assertEqual(entry["consecutive_failures"], 0)

        # --- Iteration 2: Worker succeeds ---
        queue.set_running(self.queue_dir, qid, "w-2")
        completed, added, pre_pend, post_pend = _simulate_iteration_completing_tasks(
            spec_path, tasks_to_complete=2
        )
        self.assertEqual(completed, 2)

        result = _simulate_daemon_check(
            queue_dir=self.queue_dir,
            queue_id=qid,
            spec_path=spec_path,
            iteration=2,
            tasks_completed=2,
            tasks_added=0,
            tasks_skipped=0,
            duration_seconds=800,
            exit_code=0,
            pre_pending=pre_pend,
            post_pending=post_pend,
        )
        self.assertEqual(result, "completed")

        # Verify final state
        entry = queue.get_entry(self.queue_dir, qid)
        self.assertEqual(entry["status"], "completed")

    def test_consecutive_crashes_exhaust_limit(self):
        """5 consecutive crashes should mark spec as failed."""
        spec_path = self.create_spec(tasks_pending=3)
        entry = queue.enqueue(self.queue_dir, spec_path, max_iterations=30)
        qid = entry["id"]

        for crash_num in range(1, 6):
            queue.set_running(self.queue_dir, qid, "w-1")
            exceeded = queue.record_failure(self.queue_dir, qid)

            if crash_num < 5:
                self.assertFalse(exceeded)
                # Set back to requeued for next iteration
                entry = queue.get_entry(self.queue_dir, qid)
                entry["status"] = "requeued"
                filepath = os.path.join(self.queue_dir, f"{qid}.json")
                Path(filepath).write_text(
                    json.dumps(entry, indent=2) + "\n", encoding="utf-8"
                )
            else:
                self.assertTrue(exceeded)

        # After 5 consecutive failures, mark as failed
        queue.fail(self.queue_dir, qid, "max consecutive failures exceeded")
        entry = queue.get_entry(self.queue_dir, qid)
        self.assertEqual(entry["status"], "failed")
        self.assertEqual(entry["failure_reason"], "max consecutive failures exceeded")


class TestMaxIterations(BoiTestCase):
    """Scenario 5: Max iterations.

    Submit spec with an impossible task. Set max_iterations=3.
    Each iteration completes 0 tasks (spec never finishes).
    Assert: spec marked failed after 3 iterations.
    """

    def test_max_iterations_exceeded(self):
        # A spec with tasks that never get completed
        spec_path = self.create_spec(tasks_pending=5, tasks_done=0)
        entry = queue.enqueue(self.queue_dir, spec_path, max_iterations=3)
        qid = entry["id"]

        for iter_num in range(1, 4):
            queue.set_running(self.queue_dir, qid, "w-1")

            # Worker runs but doesn't complete any tasks
            counts = count_boi_tasks(spec_path)
            make_iteration_file(
                queue_dir=self.queue_dir,
                queue_id=qid,
                iteration=iter_num,
                tasks_completed=0,
                tasks_added=0,
                tasks_skipped=0,
                duration_seconds=300,
                exit_code=0,
                pre_pending=counts["pending"],
                post_pending=counts["pending"],
            )
            update_telemetry(self.queue_dir, qid)

            entry = queue.get_entry(self.queue_dir, qid)
            if entry["iteration"] >= entry["max_iterations"]:
                queue.fail(self.queue_dir, qid, "max iterations reached")
            else:
                queue.requeue(self.queue_dir, qid, counts["done"], counts["total"])

        # Verify failed
        entry = queue.get_entry(self.queue_dir, qid)
        self.assertEqual(entry["status"], "failed")
        self.assertEqual(entry["failure_reason"], "max iterations reached")
        self.assertEqual(entry["iteration"], 3)

        # Verify telemetry shows 3 iterations with 0 completed
        from lib.telemetry import read_telemetry

        telem = read_telemetry(self.queue_dir, qid)
        self.assertIsNotNone(telem)
        self.assertEqual(telem["total_iterations"], 3)
        self.assertEqual(telem["tasks_completed_per_iteration"], [0, 0, 0])

        # Verify spec still has 5 PENDING tasks
        final_counts = count_boi_tasks(spec_path)
        self.assertEqual(final_counts["pending"], 5)
        self.assertEqual(final_counts["done"], 0)


class TestQueuePriority(BoiTestCase):
    """Scenario 6: Queue priority.

    Submit 3 specs with priorities 100, 50, 200.
    Assert: spec with priority 50 runs first.
    """

    def test_priority_ordering(self):
        spec_a = self.create_spec(tasks_pending=2, filename="spec_a.md")
        spec_b = self.create_spec(tasks_pending=2, filename="spec_b.md")
        spec_c = self.create_spec(tasks_pending=2, filename="spec_c.md")

        queue.enqueue(self.queue_dir, spec_a, priority=100, queue_id="q-001")
        queue.enqueue(self.queue_dir, spec_b, priority=50, queue_id="q-002")
        queue.enqueue(self.queue_dir, spec_c, priority=200, queue_id="q-003")

        # First dequeue: should return priority 50 (q-002)
        first = queue.dequeue(self.queue_dir)
        self.assertIsNotNone(first)
        self.assertEqual(first["id"], "q-002")
        self.assertEqual(first["priority"], 50)

        # Mark first as running
        queue.set_running(self.queue_dir, "q-002", "w-1")

        # Second dequeue: should return priority 100 (q-001)
        second = queue.dequeue(self.queue_dir)
        self.assertIsNotNone(second)
        self.assertEqual(second["id"], "q-001")
        self.assertEqual(second["priority"], 100)

        # Mark second as running
        queue.set_running(self.queue_dir, "q-001", "w-2")

        # Third dequeue: should return priority 200 (q-003)
        third = queue.dequeue(self.queue_dir)
        self.assertIsNotNone(third)
        self.assertEqual(third["id"], "q-003")
        self.assertEqual(third["priority"], 200)

    def test_requeued_preserves_priority(self):
        """Requeued specs keep their original priority (not deprioritized)."""
        spec = self.create_spec(tasks_pending=3)
        entry = queue.enqueue(self.queue_dir, spec, priority=25)
        qid = entry["id"]

        # Run one iteration
        queue.set_running(self.queue_dir, qid, "w-1")
        queue.requeue(self.queue_dir, qid, tasks_done=1, tasks_total=3)

        # Priority should still be 25
        entry = queue.get_entry(self.queue_dir, qid)
        self.assertEqual(entry["priority"], 25)
        self.assertEqual(entry["status"], "requeued")


class TestHooksAndEventsIntegration(BoiTestCase):
    """Test that hooks and events fire correctly during spec lifecycle."""

    def test_completion_writes_event_and_runs_hook(self):
        """When a spec completes, event is written and hook is called."""
        spec_path = self.create_spec(tasks_pending=1)
        entry = queue.enqueue(self.queue_dir, spec_path)
        qid = entry["id"]

        # Create a hook that writes a marker file
        marker_path = os.path.join(self.boi_state, "hook-ran.txt")
        self.create_hook(
            name="on-complete",
            body=f'echo "$1 $2" > "{marker_path}"',
        )

        # Simulate completing the spec
        queue.set_running(self.queue_dir, qid, "w-1")
        _simulate_iteration_completing_tasks(spec_path, tasks_to_complete=1)
        queue.complete(self.queue_dir, qid, tasks_done=1, tasks_total=1)

        # Write completion event
        write_spec_completed_event(
            events_dir=self.events_dir,
            queue_id=qid,
            spec_path=spec_path,
            iterations=1,
            tasks_done=1,
            tasks_added=0,
            tasks_total=1,
        )

        # Run hooks
        hook_results = run_completion_hooks(
            self.hooks_dir, qid, spec_path, is_failure=False
        )

        # Verify event was written
        events = read_events(self.events_dir)
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["type"], "spec_completed")
        self.assertEqual(events[0]["queue_id"], qid)
        self.assertEqual(events[0]["tasks_done"], 1)

        # Verify hook ran
        self.assertEqual(hook_results["on-complete"], 0)
        self.assertTrue(os.path.isfile(marker_path))
        marker_content = Path(marker_path).read_text().strip()
        self.assertIn(qid, marker_content)

    def test_failure_writes_event_and_runs_both_hooks(self):
        """When a spec fails, both on-complete and on-fail hooks fire."""
        spec_path = self.create_spec(tasks_pending=3)
        entry = queue.enqueue(self.queue_dir, spec_path, max_iterations=1)
        qid = entry["id"]

        # Create hooks
        complete_marker = os.path.join(self.boi_state, "complete-ran.txt")
        fail_marker = os.path.join(self.boi_state, "fail-ran.txt")
        self.create_hook(
            name="on-complete", body=f'echo "completed" > "{complete_marker}"'
        )
        self.create_hook(name="on-fail", body=f'echo "failed" > "{fail_marker}"')

        # Simulate failure
        queue.set_running(self.queue_dir, qid, "w-1")
        queue.fail(self.queue_dir, qid, "max iterations reached")

        # Write failure event
        write_spec_failed_event(
            events_dir=self.events_dir,
            queue_id=qid,
            spec_path=spec_path,
            iterations=1,
            tasks_done=0,
            reason="max iterations reached",
        )

        # Run hooks
        hook_results = run_completion_hooks(
            self.hooks_dir, qid, spec_path, is_failure=True
        )

        # Verify events
        events = read_events(self.events_dir)
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["type"], "spec_failed")

        # Verify both hooks ran
        self.assertEqual(hook_results["on-complete"], 0)
        self.assertEqual(hook_results["on-fail"], 0)
        self.assertTrue(os.path.isfile(complete_marker))
        self.assertTrue(os.path.isfile(fail_marker))

    def test_no_hooks_still_works(self):
        """Spec lifecycle works fine when no hooks are installed."""
        spec_path = self.create_spec(tasks_pending=1)
        entry = queue.enqueue(self.queue_dir, spec_path)
        qid = entry["id"]

        # Complete without hooks
        queue.set_running(self.queue_dir, qid, "w-1")
        queue.complete(self.queue_dir, qid, tasks_done=1, tasks_total=1)

        hook_results = run_completion_hooks(
            self.hooks_dir, qid, spec_path, is_failure=False
        )
        # on-complete returns None (no hook found)
        self.assertIsNone(hook_results["on-complete"])

        # Spec is still completed
        entry = queue.get_entry(self.queue_dir, qid)
        self.assertEqual(entry["status"], "completed")


class TestTelemetryAccuracy(BoiTestCase):
    """Test that telemetry accurately reflects the spec lifecycle."""

    def test_telemetry_matches_iterations(self):
        """Telemetry data should accurately aggregate all iteration files."""
        spec_path = self.create_spec(tasks_pending=5, tasks_done=0)
        entry = queue.enqueue(self.queue_dir, spec_path)
        qid = entry["id"]

        # Simulate 3 iterations with known data
        iter_data = [
            {"completed": 2, "added": 1, "skipped": 0, "duration": 720},
            {"completed": 2, "added": 0, "skipped": 0, "duration": 1100},
            {"completed": 2, "added": 0, "skipped": 1, "duration": 980},
        ]

        for i, data in enumerate(iter_data, 1):
            make_iteration_file(
                queue_dir=self.queue_dir,
                queue_id=qid,
                iteration=i,
                tasks_completed=data["completed"],
                tasks_added=data["added"],
                tasks_skipped=data["skipped"],
                duration_seconds=data["duration"],
                exit_code=0,
            )

        # Update telemetry
        telem = update_telemetry(self.queue_dir, qid)

        # Verify accuracy
        self.assertEqual(telem["total_iterations"], 3)
        self.assertEqual(telem["total_time_seconds"], 720 + 1100 + 980)
        self.assertEqual(telem["tasks_completed_per_iteration"], [2, 2, 2])
        self.assertEqual(telem["tasks_added_per_iteration"], [1, 0, 0])
        self.assertEqual(telem["tasks_skipped_per_iteration"], [0, 0, 1])

    def test_telemetry_persists_across_reads(self):
        """Telemetry file should persist and be readable after writing."""
        spec_path = self.create_spec(tasks_pending=2)
        entry = queue.enqueue(self.queue_dir, spec_path)
        qid = entry["id"]

        make_iteration_file(
            queue_dir=self.queue_dir,
            queue_id=qid,
            iteration=1,
            tasks_completed=1,
            duration_seconds=500,
        )
        update_telemetry(self.queue_dir, qid)

        # Read back
        from lib.telemetry import read_telemetry

        telem = read_telemetry(self.queue_dir, qid)
        self.assertIsNotNone(telem)
        self.assertEqual(telem["queue_id"], qid)
        self.assertEqual(telem["total_iterations"], 1)
        self.assertEqual(telem["total_time_seconds"], 500)


class TestStatusOutputIntegration(BoiTestCase):
    """Test that status.py correctly renders data from the full lifecycle."""

    def test_status_table_with_mixed_states(self):
        """Status table should render specs in various states correctly."""
        from lib.status import build_queue_status, format_queue_table

        # Create specs in different states
        spec_a = self.create_spec(tasks_pending=0, tasks_done=5, filename="a.md")
        spec_b = self.create_spec(tasks_pending=3, tasks_done=2, filename="b.md")
        spec_c = self.create_spec(tasks_pending=4, filename="c.md")

        make_queue_entry(
            queue_dir=self.queue_dir,
            queue_id="q-001",
            spec_path=spec_a,
            status="completed",
            iteration=3,
            tasks_done=5,
            tasks_total=5,
            priority=100,
        )
        make_queue_entry(
            queue_dir=self.queue_dir,
            queue_id="q-002",
            spec_path=spec_b,
            status="running",
            iteration=2,
            tasks_done=2,
            tasks_total=5,
            last_worker="w-1",
            priority=50,
        )
        make_queue_entry(
            queue_dir=self.queue_dir,
            queue_id="q-003",
            spec_path=spec_c,
            status="queued",
            tasks_done=0,
            tasks_total=4,
            priority=200,
        )

        status = build_queue_status(
            self.queue_dir,
            config={"workers": [{"id": "w-1"}, {"id": "w-2"}, {"id": "w-3"}]},
        )

        # Verify structure
        self.assertEqual(len(status["entries"]), 3)
        self.assertEqual(status["summary"]["running"], 1)
        self.assertEqual(status["summary"]["completed"], 1)
        self.assertEqual(status["summary"]["queued"], 1)

        # Render (no color for test assertions)
        table = format_queue_table(status, color=False)
        self.assertIn("BOI", table)
        self.assertIn("q-001", table)
        self.assertIn("q-002", table)
        self.assertIn("q-003", table)
        self.assertIn("completed", table)
        self.assertIn("running", table)
        self.assertIn("queued", table)

    def test_telemetry_table_renders(self):
        """Telemetry table should render with iteration breakdown."""
        from lib.status import build_telemetry, format_telemetry_table

        spec = self.create_spec(tasks_pending=1, tasks_done=4)
        make_queue_entry(
            queue_dir=self.queue_dir,
            queue_id="q-001",
            spec_path=spec,
            status="completed",
            iteration=3,
            tasks_done=5,
            tasks_total=5,
        )

        # Write iteration files
        for i, (comp, added, skip, dur) in enumerate(
            [(2, 1, 0, 720), (2, 0, 0, 1100), (1, 0, 1, 980)], 1
        ):
            make_iteration_file(
                queue_dir=self.queue_dir,
                queue_id="q-001",
                iteration=i,
                tasks_completed=comp,
                tasks_added=added,
                tasks_skipped=skip,
                duration_seconds=dur,
            )

        telem = build_telemetry(self.queue_dir, "q-001")
        self.assertIsNotNone(telem)

        table = format_telemetry_table(telem, color=False)
        self.assertIn("Iteration breakdown:", table)
        self.assertIn("#1:", table)
        self.assertIn("#2:", table)
        self.assertIn("#3:", table)
        self.assertIn("tasks done", table)


class TestDashboardIntegration(BoiTestCase):
    """Test the dashboard renders correctly with real queue data."""

    def test_dashboard_with_specs(self):
        from lib.status import build_queue_status, format_dashboard

        spec_a = self.create_spec(tasks_pending=0, tasks_done=5, filename="a.md")
        spec_b = self.create_spec(tasks_pending=3, tasks_done=2, filename="b.md")

        make_queue_entry(
            queue_dir=self.queue_dir,
            queue_id="q-001",
            spec_path=spec_a,
            status="completed",
            iteration=3,
            tasks_done=5,
            tasks_total=5,
        )
        make_queue_entry(
            queue_dir=self.queue_dir,
            queue_id="q-002",
            spec_path=spec_b,
            status="running",
            iteration=1,
            tasks_done=2,
            tasks_total=5,
            last_worker="w-1",
        )

        status = build_queue_status(
            self.queue_dir,
            config={"workers": [{"id": "w-1"}, {"id": "w-2"}]},
        )
        dashboard = format_dashboard(status, color=False)

        self.assertIn("BOI", dashboard)
        self.assertIn("q-001", dashboard)
        self.assertIn("q-002", dashboard)
        self.assertIn("5/5", dashboard)
        self.assertIn("2/5", dashboard)
        self.assertIn("Workers:", dashboard)


class TestFullLifecycleWithHooksAndTelemetry(BoiTestCase):
    """Full end-to-end: dispatch -> iterate -> complete -> events -> hooks -> telemetry."""

    def test_full_lifecycle(self):
        # Create a spec with 4 tasks
        spec_path = self.create_spec(tasks_pending=4, tasks_done=0)

        # Create hooks
        complete_marker = os.path.join(self.boi_state, "lifecycle-complete.txt")
        self.create_hook(
            name="on-complete",
            body=f'echo "$1 done at $(date)" > "{complete_marker}"',
        )

        # 1. Dispatch (enqueue)
        entry = queue.enqueue(self.queue_dir, spec_path, priority=75)
        qid = entry["id"]
        self.assertEqual(entry["status"], "queued")

        # 2. Iteration 1: complete 2 tasks, add 1
        queue.set_running(self.queue_dir, qid, "w-1")
        completed, added, pre_p, post_p = _simulate_iteration_completing_tasks(
            spec_path, tasks_to_complete=2, tasks_to_add=1
        )
        result = _simulate_daemon_check(
            self.queue_dir,
            qid,
            spec_path,
            iteration=1,
            tasks_completed=2,
            tasks_added=1,
            tasks_skipped=0,
            duration_seconds=900,
            exit_code=0,
            pre_pending=pre_p,
            post_pending=post_p,
        )
        self.assertEqual(result, "requeued")

        # 3. Iteration 2: complete remaining 3 tasks
        queue.set_running(self.queue_dir, qid, "w-2")
        completed, added, pre_p, post_p = _simulate_iteration_completing_tasks(
            spec_path, tasks_to_complete=3
        )
        result = _simulate_daemon_check(
            self.queue_dir,
            qid,
            spec_path,
            iteration=2,
            tasks_completed=3,
            tasks_added=0,
            tasks_skipped=0,
            duration_seconds=1200,
            exit_code=0,
            pre_pending=pre_p,
            post_pending=post_p,
        )
        self.assertEqual(result, "completed")

        # 4. Write completion event
        write_spec_completed_event(
            events_dir=self.events_dir,
            queue_id=qid,
            spec_path=spec_path,
            iterations=2,
            tasks_done=5,
            tasks_added=1,
            tasks_total=5,
        )

        # 5. Run hooks
        hook_results = run_completion_hooks(
            self.hooks_dir, qid, spec_path, is_failure=False
        )
        self.assertEqual(hook_results["on-complete"], 0)

        # 6. Verify everything
        # Queue entry
        entry = queue.get_entry(self.queue_dir, qid)
        self.assertEqual(entry["status"], "completed")
        self.assertEqual(entry["tasks_done"], 5)
        self.assertEqual(entry["tasks_total"], 5)

        # Spec file
        final_counts = count_boi_tasks(spec_path)
        self.assertEqual(final_counts["pending"], 0)
        self.assertEqual(final_counts["done"], 5)
        self.assertEqual(final_counts["total"], 5)

        # Telemetry
        from lib.telemetry import read_telemetry

        telem = read_telemetry(self.queue_dir, qid)
        self.assertEqual(telem["total_iterations"], 2)
        self.assertEqual(sum(telem["tasks_completed_per_iteration"]), 5)
        self.assertEqual(sum(telem["tasks_added_per_iteration"]), 1)
        self.assertEqual(telem["total_time_seconds"], 2100)

        # Events
        events = read_events(self.events_dir)
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["type"], "spec_completed")
        self.assertEqual(events[0]["tasks_done"], 5)
        self.assertEqual(events[0]["tasks_added"], 1)

        # Hook marker
        self.assertTrue(os.path.isfile(complete_marker))


if __name__ == "__main__":
    unittest.main()
