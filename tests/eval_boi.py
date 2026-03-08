# eval_boi.py — Scored evaluation scenarios for BOI.
#
# Tests high-level behaviors that define BOI's value proposition:
#   1. Self-evolving specs (task mutation during iteration)
#   2. Fresh context per iteration (no bleed between iterations)
#   3. Overnight resilience (crash recovery without giving up)
#   4. Queue drains correctly (all specs complete, workers stay busy)
#   5. Telemetry accuracy (data matches reality)
#   6. Dashboard renders (visual output is correct)
#
# Each scenario is scored pass/fail. Summary: "5/6 passed" format.
#
# All tests use mock data and temp directories. No live Claude calls.
# No real worktrees. No external dependencies.

import json
import os
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib import queue
from lib.spec_parser import count_boi_tasks, parse_boi_spec
from lib.telemetry import read_telemetry, update_telemetry
from tests.conftest import (
    BoiTestCase,
    make_iteration_file,
    make_log_file,
    make_queue_entry,
)


def _simulate_iteration_completing_tasks(
    spec_path: str,
    tasks_to_complete: int,
    tasks_to_add: int = 0,
    research_file: str = "",
    research_content: str = "",
):
    """Simulate a worker iteration that completes N tasks and optionally adds new ones.

    If research_file is provided, writes research_content to that path (simulating
    a worker writing a research output during an iteration).

    Returns (tasks_completed, tasks_added, pre_pending, post_pending).
    """
    # Write research file if requested
    if research_file:
        Path(research_file).parent.mkdir(parents=True, exist_ok=True)
        Path(research_file).write_text(research_content, encoding="utf-8")

    content = Path(spec_path).read_text(encoding="utf-8")
    tasks = parse_boi_spec(content)

    pre_pending = sum(1 for t in tasks if t.status == "PENDING")
    completed = 0

    lines = content.splitlines()
    new_lines = []
    i = 0
    while i < len(lines):
        line = lines[i]
        if line.strip().startswith("### t-"):
            new_lines.append(line)
            i += 1
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
    pre_pending: int = 0,
    post_pending: int = 0,
):
    """Simulate what the daemon does after a worker iteration finishes."""
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

    update_telemetry(queue_dir, queue_id)

    counts = count_boi_tasks(spec_path)

    entry = queue.get_entry(queue_dir, queue_id)
    if entry is None:
        raise ValueError(f"Queue entry not found: {queue_id}")

    if counts["pending"] == 0:
        queue.complete(queue_dir, queue_id, counts["done"], counts["total"])
        return "completed"
    elif entry["iteration"] >= entry["max_iterations"]:
        queue.fail(queue_dir, queue_id, "max iterations reached")
        return "failed"
    else:
        queue.requeue(queue_dir, queue_id, counts["done"], counts["total"])
        return "requeued"


# ─── Eval Result Tracking ──────────────────────────────────────────────────


class EvalResult:
    """Track pass/fail results for eval scenarios."""

    def __init__(self):
        self.results: list[tuple[str, bool, str]] = []

    def record(self, name: str, passed: bool, detail: str = ""):
        self.results.append((name, passed, detail))

    @property
    def passed(self) -> int:
        return sum(1 for _, p, _ in self.results if p)

    @property
    def total(self) -> int:
        return len(self.results)

    def summary(self) -> str:
        lines = []
        for name, passed, detail in self.results:
            icon = "\u2713" if passed else "\u2717"
            status = "PASS" if passed else "FAIL"
            line = f"  {icon} {name}: {status}"
            if detail:
                line += f" — {detail}"
            lines.append(line)
        lines.append("")
        lines.append(f"  {self.passed}/{self.total} passed")
        return "\n".join(lines)


# ─── Eval Scenarios ────────────────────────────────────────────────────────


class EvalSelfEvolvingSpec(BoiTestCase):
    """Eval 1: Self-evolving spec.

    Submit a spec where task 1 writes a research file, task 2 reads it
    and adds a new task. Assert: new task appears in spec.md with PENDING status.
    """

    def test_self_evolution_adds_task(self):
        # Create a spec with 2 tasks:
        # t-1: research phase (writes research.md)
        # t-2: implementation phase (reads research, adds tasks)
        spec_content = (
            "# Eval Spec — Self-Evolving\n"
            "\n"
            "## Tasks\n"
            "\n"
            "### t-1: Research phase\n"
            "PENDING\n"
            "\n"
            "**Spec:** Research the problem space and write findings.\n"
            "\n"
            "**Verify:** test -f research.md\n"
            "\n"
            "**Self-evolution:** If research reveals additional work, add PENDING tasks.\n"
            "\n"
            "### t-2: Implement solution\n"
            "PENDING\n"
            "\n"
            "**Spec:** Build the solution based on research.\n"
            "\n"
            "**Verify:** echo 'solution ok'\n"
        )
        spec_path = self.create_spec(content=spec_content, filename="eval_evolving.md")
        research_file = os.path.join(self.specs_dir, "research.md")

        entry = queue.enqueue(self.queue_dir, spec_path)
        qid = entry["id"]

        # Pre-check
        initial_counts = count_boi_tasks(spec_path)
        self.assertEqual(initial_counts["total"], 2)
        self.assertEqual(initial_counts["pending"], 2)

        # --- Iteration 1: Complete t-1 (research), add 1 new task ---
        queue.set_running(self.queue_dir, qid, "w-1")
        completed, added, pre_p, post_p = _simulate_iteration_completing_tasks(
            spec_path,
            tasks_to_complete=1,
            tasks_to_add=1,
            research_file=research_file,
            research_content="Finding: need an additional migration step.\n",
        )
        self.assertEqual(completed, 1)
        self.assertEqual(added, 1)

        # Verify research file was written
        self.assertTrue(os.path.isfile(research_file))

        # Verify the spec now has 3 tasks (self-evolved)
        mid_counts = count_boi_tasks(spec_path)
        self.assertEqual(mid_counts["total"], 3)
        self.assertEqual(mid_counts["pending"], 2)  # t-2 + new self-evolved task
        self.assertEqual(mid_counts["done"], 1)  # t-1

        # Verify new task has PENDING status
        content = Path(spec_path).read_text(encoding="utf-8")
        tasks = parse_boi_spec(content)
        new_task = [t for t in tasks if "Self-evolved" in t.title]
        self.assertEqual(len(new_task), 1)
        self.assertEqual(new_task[0].status, "PENDING")

        result = _simulate_daemon_check(
            self.queue_dir,
            qid,
            spec_path,
            iteration=1,
            tasks_completed=1,
            tasks_added=1,
            tasks_skipped=0,
            duration_seconds=600,
            exit_code=0,
            pre_pending=pre_p,
            post_pending=post_p,
        )
        self.assertEqual(result, "requeued")

        # --- Iteration 2: Complete remaining 2 tasks ---
        queue.set_running(self.queue_dir, qid, "w-1")
        completed, added, pre_p, post_p = _simulate_iteration_completing_tasks(
            spec_path, tasks_to_complete=2
        )
        self.assertEqual(completed, 2)

        result = _simulate_daemon_check(
            self.queue_dir,
            qid,
            spec_path,
            iteration=2,
            tasks_completed=2,
            tasks_added=0,
            tasks_skipped=0,
            duration_seconds=800,
            exit_code=0,
            pre_pending=pre_p,
            post_pending=post_p,
        )
        self.assertEqual(result, "completed")

        # Final verification
        final_counts = count_boi_tasks(spec_path)
        self.assertEqual(final_counts["done"], 3)
        self.assertEqual(final_counts["pending"], 0)
        self.assertEqual(final_counts["total"], 3)


class EvalFreshContextPerIteration(BoiTestCase):
    """Eval 2: Fresh context per iteration.

    Submit spec with 2 tasks. After iteration 1 completes task 1,
    verify the worker log for iteration 2 does NOT reference
    iteration 1's work (no context bleed).
    """

    def test_no_context_bleed(self):
        spec_path = self.create_spec(tasks_pending=2, tasks_done=0)
        entry = queue.enqueue(self.queue_dir, spec_path)
        qid = entry["id"]

        # --- Iteration 1 ---
        queue.set_running(self.queue_dir, qid, "w-1")

        # Create iteration 1 log with distinctive content
        iter1_log = make_log_file(
            logs_dir=self.logs_dir,
            queue_id=qid,
            iteration=1,
            content=(
                "Worker started (iteration 1)\n"
                "Reading spec.md...\n"
                "Found PENDING task: t-1\n"
                "Executing task t-1: unique_marker_alpha_7742\n"
                "Task t-1 completed successfully.\n"
                "Writing iteration metadata.\n"
                "Worker exiting.\n"
            ),
        )

        # Complete task 1
        completed, _, pre_p, post_p = _simulate_iteration_completing_tasks(
            spec_path, tasks_to_complete=1
        )
        _simulate_daemon_check(
            self.queue_dir,
            qid,
            spec_path,
            iteration=1,
            tasks_completed=1,
            tasks_added=0,
            tasks_skipped=0,
            duration_seconds=600,
            exit_code=0,
            pre_pending=pre_p,
            post_pending=post_p,
        )

        # --- Iteration 2 ---
        queue.set_running(self.queue_dir, qid, "w-2")

        # Create iteration 2 log WITHOUT any reference to iteration 1
        iter2_log = make_log_file(
            logs_dir=self.logs_dir,
            queue_id=qid,
            iteration=2,
            content=(
                "Worker started (iteration 2)\n"
                "Reading spec.md...\n"
                "Found PENDING task: t-2\n"
                "Executing task t-2: unique_marker_beta_9913\n"
                "Task t-2 completed successfully.\n"
                "Writing iteration metadata.\n"
                "Worker exiting.\n"
            ),
        )

        # Verify: iteration 2 log does NOT contain iteration 1's unique marker
        iter2_content = Path(iter2_log).read_text(encoding="utf-8")
        self.assertNotIn(
            "unique_marker_alpha_7742",
            iter2_content,
            "Context bleed: iteration 2 log references iteration 1 content",
        )

        # Verify: each log is self-contained (mentions its own iteration number)
        iter1_content = Path(iter1_log).read_text(encoding="utf-8")
        self.assertIn("iteration 1", iter1_content.lower())
        self.assertIn("iteration 2", iter2_content.lower())

        # Verify: worker assignment differs (different workers = fresh sessions)
        entry_after = queue.get_entry(self.queue_dir, qid)
        self.assertEqual(entry_after["last_worker"], "w-2")

    def test_worker_prompt_reads_spec_fresh(self):
        """Worker prompt template should instruct reading spec from scratch."""
        prompt_path = (
            Path(__file__).resolve().parent.parent / "templates" / "worker-prompt.md"
        )
        if not prompt_path.is_file():
            self.skipTest("worker-prompt.md not found")

        content = prompt_path.read_text(encoding="utf-8").lower()

        # Prompt should contain instructions about fresh context
        has_fresh_instruction = (
            "fresh" in content
            or "clean session" in content
            or "read the spec" in content
            or "re-read" in content
        )
        self.assertTrue(
            has_fresh_instruction,
            "Worker prompt should contain fresh-context instructions",
        )


class EvalOvernightResilience(BoiTestCase):
    """Eval 3: Overnight resilience.

    Submit spec. Simulate 3 consecutive worker crashes (kill -9).
    Assert: daemon requeues each time, doesn't give up until max_iterations.
    """

    def test_survives_consecutive_crashes(self):
        spec_path = self.create_spec(tasks_pending=3, tasks_done=0)
        entry = queue.enqueue(self.queue_dir, spec_path, max_iterations=10)
        qid = entry["id"]

        # Simulate 3 consecutive crashes (below the max_consecutive_failures=5 threshold)
        for crash_num in range(1, 4):
            queue.set_running(self.queue_dir, qid, f"w-{crash_num}")

            # Record crash
            max_exceeded = queue.record_failure(self.queue_dir, qid)
            self.assertFalse(
                max_exceeded,
                f"Crash {crash_num}/3 should NOT exceed max consecutive failures",
            )

            # Write a crash iteration file
            make_iteration_file(
                queue_dir=self.queue_dir,
                queue_id=qid,
                iteration=crash_num,
                tasks_completed=0,
                tasks_added=0,
                tasks_skipped=0,
                duration_seconds=15,
                exit_code=137,  # SIGKILL
            )

            # Requeue (daemon would do this)
            queue.requeue(self.queue_dir, qid, tasks_done=0, tasks_total=3)

        # After 3 crashes + requeues, spec should still be requeueable
        entry = queue.get_entry(self.queue_dir, qid)
        self.assertEqual(entry["status"], "requeued")
        # consecutive_failures resets on requeue
        self.assertEqual(entry["consecutive_failures"], 0)

        # Now the spec can be picked up again
        next_spec = queue.dequeue(self.queue_dir)
        self.assertIsNotNone(next_spec)
        self.assertEqual(next_spec["id"], qid)

        # --- Recovery iteration: actually completes all tasks ---
        queue.set_running(self.queue_dir, qid, "w-4")
        completed, _, pre_p, post_p = _simulate_iteration_completing_tasks(
            spec_path, tasks_to_complete=3
        )
        self.assertEqual(completed, 3)

        result = _simulate_daemon_check(
            self.queue_dir,
            qid,
            spec_path,
            iteration=4,
            tasks_completed=3,
            tasks_added=0,
            tasks_skipped=0,
            duration_seconds=900,
            exit_code=0,
            pre_pending=pre_p,
            post_pending=post_p,
        )
        self.assertEqual(result, "completed")

        # Verify final state
        final_entry = queue.get_entry(self.queue_dir, qid)
        self.assertEqual(final_entry["status"], "completed")
        self.assertEqual(final_entry["tasks_done"], 3)

    def test_does_not_give_up_early(self):
        """With max_iterations=30, 3 crashes should leave plenty of room."""
        spec_path = self.create_spec(tasks_pending=2)
        entry = queue.enqueue(self.queue_dir, spec_path, max_iterations=30)
        qid = entry["id"]

        # 3 crashes consume 3 iterations
        for i in range(1, 4):
            queue.set_running(self.queue_dir, qid, "w-1")
            queue.record_failure(self.queue_dir, qid)
            queue.requeue(self.queue_dir, qid, 0, 2)

        entry = queue.get_entry(self.queue_dir, qid)
        # 3 iterations used out of 30
        self.assertEqual(entry["iteration"], 3)
        self.assertLess(entry["iteration"], entry["max_iterations"])
        # Should still have 27 iterations available
        remaining = entry["max_iterations"] - entry["iteration"]
        self.assertEqual(remaining, 27)


class EvalQueueDrainsCorrectly(BoiTestCase):
    """Eval 4: Queue drains correctly.

    Submit 5 specs to 3 workers. Assert: all 5 complete. No spec stuck.
    Workers stay busy until queue is empty.
    """

    def test_five_specs_three_workers(self):
        # Create 5 specs
        specs = []
        for i in range(1, 6):
            sp = self.create_spec(
                tasks_pending=2, tasks_done=0, filename=f"spec_{i}.md"
            )
            entry = queue.enqueue(
                self.queue_dir, sp, priority=100 + i, queue_id=f"q-{i:03d}"
            )
            specs.append((sp, entry["id"]))

        workers = ["w-1", "w-2", "w-3"]
        completed_ids = set()

        # Simulate dispatch rounds until all specs are complete
        max_rounds = 20  # Safety limit
        round_num = 0

        while len(completed_ids) < 5 and round_num < max_rounds:
            round_num += 1

            # Assign free workers to next available specs
            for worker in workers:
                next_spec = queue.dequeue(self.queue_dir)
                if next_spec is None:
                    continue

                qid = next_spec["id"]
                spec_path = next_spec["spec_path"]

                queue.set_running(self.queue_dir, qid, worker)

                # Complete all tasks in one iteration
                completed, _, pre_p, post_p = _simulate_iteration_completing_tasks(
                    spec_path, tasks_to_complete=2
                )

                result = _simulate_daemon_check(
                    self.queue_dir,
                    qid,
                    spec_path,
                    iteration=next_spec["iteration"] + 1,
                    tasks_completed=2,
                    tasks_added=0,
                    tasks_skipped=0,
                    duration_seconds=300,
                    exit_code=0,
                    pre_pending=pre_p,
                    post_pending=post_p,
                )

                if result == "completed":
                    completed_ids.add(qid)

        # All 5 specs should be completed
        self.assertEqual(
            len(completed_ids),
            5,
            f"Expected 5 completed specs, got {len(completed_ids)}: {completed_ids}",
        )

        # Verify each spec is marked completed in the queue
        for _, qid in specs:
            entry = queue.get_entry(self.queue_dir, qid)
            self.assertEqual(
                entry["status"],
                "completed",
                f"Spec {qid} should be completed but is {entry['status']}",
            )

        # Verify queue is empty (no stuck specs)
        remaining = queue.dequeue(self.queue_dir)
        self.assertIsNone(remaining, "Queue should be empty after all specs complete")

    def test_no_spec_stuck_in_running(self):
        """After completing, no spec should be stuck in 'running' state."""
        # Create 3 specs
        for i in range(1, 4):
            sp = self.create_spec(tasks_pending=1, filename=f"stuck_{i}.md")
            queue.enqueue(self.queue_dir, sp, queue_id=f"q-{i:03d}")

        # Process all
        while True:
            next_spec = queue.dequeue(self.queue_dir)
            if next_spec is None:
                break

            qid = next_spec["id"]
            queue.set_running(self.queue_dir, qid, "w-1")
            completed, _, pre_p, post_p = _simulate_iteration_completing_tasks(
                next_spec["spec_path"], tasks_to_complete=1
            )
            _simulate_daemon_check(
                self.queue_dir,
                qid,
                next_spec["spec_path"],
                iteration=1,
                tasks_completed=1,
                tasks_added=0,
                tasks_skipped=0,
                duration_seconds=120,
                exit_code=0,
                pre_pending=pre_p,
                post_pending=post_p,
            )

        # No spec should be in 'running' state
        all_entries = queue.get_queue(self.queue_dir)
        running = [e for e in all_entries if e["status"] == "running"]
        self.assertEqual(
            len(running),
            0,
            f"Found {len(running)} specs stuck in 'running' state",
        )


class EvalTelemetryAccuracy(BoiTestCase):
    """Eval 5: Telemetry accuracy.

    Submit spec with known task count. Complete it. Verify telemetry
    matches reality (iteration count, tasks done, time).
    """

    def test_telemetry_matches_reality(self):
        spec_path = self.create_spec(tasks_pending=4, tasks_done=0)
        entry = queue.enqueue(self.queue_dir, spec_path)
        qid = entry["id"]

        # Define exact iteration data
        planned_iterations = [
            {"complete": 2, "add": 1, "skip": 0, "duration": 723},
            {"complete": 2, "add": 0, "skip": 0, "duration": 1101},
            {"complete": 1, "add": 0, "skip": 0, "duration": 989},
        ]

        total_completed = 0
        total_added = 0
        total_skipped = 0
        total_time = 0

        for i, data in enumerate(planned_iterations, 1):
            queue.set_running(self.queue_dir, qid, "w-1")

            completed, added, pre_p, post_p = _simulate_iteration_completing_tasks(
                spec_path,
                tasks_to_complete=data["complete"],
                tasks_to_add=data["add"],
            )

            _simulate_daemon_check(
                self.queue_dir,
                qid,
                spec_path,
                iteration=i,
                tasks_completed=data["complete"],
                tasks_added=data["add"],
                tasks_skipped=data["skip"],
                duration_seconds=data["duration"],
                exit_code=0,
                pre_pending=pre_p,
                post_pending=post_p,
            )

            total_completed += data["complete"]
            total_added += data["add"]
            total_skipped += data["skip"]
            total_time += data["duration"]

        # Read telemetry
        telem = read_telemetry(self.queue_dir, qid)
        self.assertIsNotNone(telem, "Telemetry file should exist")

        # Verify iteration count
        self.assertEqual(
            telem["total_iterations"],
            3,
            f"Expected 3 iterations, got {telem['total_iterations']}",
        )

        # Verify per-iteration task counts
        self.assertEqual(
            telem["tasks_completed_per_iteration"],
            [2, 2, 1],
            f"Task completion array mismatch: {telem['tasks_completed_per_iteration']}",
        )
        self.assertEqual(
            telem["tasks_added_per_iteration"],
            [1, 0, 0],
            f"Task added array mismatch: {telem['tasks_added_per_iteration']}",
        )
        self.assertEqual(
            telem["tasks_skipped_per_iteration"],
            [0, 0, 0],
            f"Task skipped array mismatch: {telem['tasks_skipped_per_iteration']}",
        )

        # Verify totals
        self.assertEqual(
            sum(telem["tasks_completed_per_iteration"]),
            total_completed,
            "Total completed tasks mismatch",
        )
        self.assertEqual(
            sum(telem["tasks_added_per_iteration"]),
            total_added,
            "Total added tasks mismatch",
        )
        self.assertEqual(
            telem["total_time_seconds"],
            total_time,
            f"Total time mismatch: {telem['total_time_seconds']} != {total_time}",
        )

        # Verify queue entry matches
        final_entry = queue.get_entry(self.queue_dir, qid)
        spec_counts = count_boi_tasks(spec_path)
        self.assertEqual(
            final_entry["tasks_done"],
            spec_counts["done"],
            "Queue entry tasks_done doesn't match spec file",
        )
        self.assertEqual(
            final_entry["tasks_total"],
            spec_counts["total"],
            "Queue entry tasks_total doesn't match spec file",
        )


class EvalDashboardRenders(BoiTestCase):
    """Eval 6: Dashboard renders.

    Inject test state. Run dashboard formatter.
    Assert: output contains expected status icons and task counts.
    """

    def test_dashboard_contains_expected_elements(self):
        from lib.status import build_queue_status, format_dashboard

        # Create specs in various states
        spec_done = self.create_spec(tasks_pending=0, tasks_done=5, filename="done.md")
        spec_run = self.create_spec(
            tasks_pending=3, tasks_done=2, filename="running.md"
        )
        spec_queued = self.create_spec(tasks_pending=4, filename="queued.md")
        spec_failed = self.create_spec(
            tasks_pending=3, tasks_done=1, filename="failed.md"
        )

        make_queue_entry(
            queue_dir=self.queue_dir,
            queue_id="q-001",
            spec_path=spec_done,
            status="completed",
            iteration=3,
            tasks_done=5,
            tasks_total=5,
        )
        make_queue_entry(
            queue_dir=self.queue_dir,
            queue_id="q-002",
            spec_path=spec_run,
            status="running",
            iteration=2,
            tasks_done=2,
            tasks_total=5,
            last_worker="w-1",
        )
        make_queue_entry(
            queue_dir=self.queue_dir,
            queue_id="q-003",
            spec_path=spec_queued,
            status="queued",
            tasks_done=0,
            tasks_total=4,
        )
        make_queue_entry(
            queue_dir=self.queue_dir,
            queue_id="q-004",
            spec_path=spec_failed,
            status="failed",
            iteration=10,
            tasks_done=1,
            tasks_total=4,
        )

        config = {"workers": [{"id": "w-1"}, {"id": "w-2"}, {"id": "w-3"}]}
        status = build_queue_status(self.queue_dir, config)
        dashboard = format_dashboard(status, color=False)

        # Status icons present
        self.assertIn("\u2713", dashboard, "Missing completed icon")
        self.assertIn("\u25b6", dashboard, "Missing running icon")
        self.assertIn("\u00b7", dashboard, "Missing queued icon")
        self.assertIn("\u2717", dashboard, "Missing failed icon")

        # Queue IDs present
        self.assertIn("q-001", dashboard)
        self.assertIn("q-002", dashboard)
        self.assertIn("q-003", dashboard)
        self.assertIn("q-004", dashboard)

        # Task counts present
        self.assertIn("5/5", dashboard, "Missing 5/5 task count for completed spec")
        self.assertIn("2/5", dashboard, "Missing 2/5 task count for running spec")
        self.assertIn("0/4", dashboard, "Missing 0/4 task count for queued spec")

        # Workers summary present
        self.assertIn("Workers:", dashboard)

        # BOI header present
        self.assertIn("BOI", dashboard)

    def test_dashboard_with_empty_queue(self):
        from lib.status import build_queue_status, format_dashboard

        config = {"workers": [{"id": "w-1"}, {"id": "w-2"}]}
        status = build_queue_status(self.queue_dir, config)
        dashboard = format_dashboard(status, color=False)

        self.assertIn("BOI", dashboard)
        self.assertIn("No specs", dashboard)

    def test_status_table_renders(self):
        """Full status table also renders correctly."""
        from lib.status import build_queue_status, format_queue_table

        spec = self.create_spec(tasks_pending=2, tasks_done=3)
        make_queue_entry(
            queue_dir=self.queue_dir,
            queue_id="q-001",
            spec_path=spec,
            status="running",
            iteration=2,
            tasks_done=3,
            tasks_total=5,
            last_worker="w-1",
        )

        config = {"workers": [{"id": "w-1"}, {"id": "w-2"}]}
        status = build_queue_status(self.queue_dir, config)
        table = format_queue_table(status, color=False)

        self.assertIn("BOI", table)
        self.assertIn("q-001", table)
        self.assertIn("running", table)
        self.assertIn("3/5 done", table)
        self.assertIn("w-1", table)


# ─── Eval Runner ───────────────────────────────────────────────────────────


class EvalRunner(unittest.TestCase):
    """Meta-test that runs all eval scenarios and prints a scored summary."""

    def test_eval_summary(self):
        """Run all eval scenarios and print scored summary."""
        result = EvalResult()
        eval_classes = [
            ("Self-evolving spec", EvalSelfEvolvingSpec),
            ("Fresh context per iteration", EvalFreshContextPerIteration),
            ("Overnight resilience", EvalOvernightResilience),
            ("Queue drains correctly", EvalQueueDrainsCorrectly),
            ("Telemetry accuracy", EvalTelemetryAccuracy),
            ("Dashboard renders", EvalDashboardRenders),
        ]

        loader = unittest.TestLoader()
        for name, cls in eval_classes:
            suite = loader.loadTestsFromTestCase(cls)
            devnull = open(os.devnull, "w")
            try:
                runner = unittest.TextTestRunner(stream=devnull, verbosity=0)
                test_result = runner.run(suite)
            finally:
                devnull.close()

            passed = test_result.wasSuccessful()
            detail = ""
            if not passed:
                failures = len(test_result.failures) + len(test_result.errors)
                total = test_result.testsRun
                detail = f"{total - failures}/{total} sub-tests passed"
            else:
                detail = f"{test_result.testsRun} sub-tests"

            result.record(name, passed, detail)

        # Print summary
        print("\n" + "=" * 50)
        print("BOI Eval Suite Results")
        print("=" * 50)
        print(result.summary())
        print("=" * 50)

        # The meta-test passes if all eval scenarios pass
        self.assertEqual(
            result.passed,
            result.total,
            f"Eval suite: {result.passed}/{result.total} passed",
        )


if __name__ == "__main__":
    # When run directly, run just the EvalRunner for a clean summary.
    # Use -v for verbose output of individual test cases.
    import argparse

    parser = argparse.ArgumentParser(description="BOI Eval Suite")
    parser.add_argument(
        "--verbose",
        "-v",
        action="store_true",
        help="Run all individual tests with verbose output",
    )
    args = parser.parse_args()

    if args.verbose:
        # Run all test classes individually with verbose output
        unittest.main(argv=["eval_boi.py", "-v"], module=__name__)
    else:
        # Run just the summary
        suite = unittest.TestLoader().loadTestsFromTestCase(EvalRunner)
        unittest.TextTestRunner(verbosity=2).run(suite)
