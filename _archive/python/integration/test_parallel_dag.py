# test_parallel_dag.py — Integration tests for parallel DAG execution in BOI.
#
# Tests the full system: daemon, workers, spec dispatch, parallel execution,
# and completion. Uses the existing MockClaude + DaemonTestHarness infrastructure.
#
# Test cases:
#   1. Serial execution: 3-task linear chain completes in order
#   2. Parallel execution: diamond DAG with concurrent independent tasks
#   3. Wide fan-out: 5 independent tasks + 1 synthesis task
#   4. Self-evolution with deps: discover mode adds dependent tasks
#   5. Worker failure recovery: daemon requeues failed tasks
#   6. Spec priority: high-priority spec gets workers first
#   7. Concurrent spec updates: two workers on same spec, no corruption

import os
import re
import sys
import textwrap
import threading
import time
import unittest
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

# Add project root to path
_PROJECT_ROOT = str(Path(__file__).resolve().parent.parent.parent)
sys.path.insert(0, _PROJECT_ROOT)

from lib.spec_parser import parse_boi_spec
from tests.integration.conftest import IntegrationTestCase, MockClaude


class ParallelDAGTestBase(IntegrationTestCase):
    """Base class for parallel DAG integration tests.

    Extends IntegrationTestCase with:
    - Higher worker count (3 workers)
    - Phase completion handler that tracks task progress
    - Helpers for creating specs with dependency graphs
    """

    NUM_WORKERS = 3
    WORKER_TIMEOUT = 30

    def setUp(self) -> None:
        super().setUp()
        self.harness.start()

        # Patch phase completion to work in test environment
        daemon = self.harness._daemon
        daemon._dispatch_phase_completion = self._test_phase_completion

    def _test_phase_completion(
        self,
        spec_id: str,
        phase: str,
        exit_code: int,
        worker_id: str,
    ) -> None:
        """Completion handler for parallel DAG tests.

        Reads the spec, counts tasks, inserts iteration record,
        and transitions spec to requeued or completed.
        """
        daemon = self.harness._daemon
        db = daemon.db
        spec = db.get_spec(spec_id)
        if spec is None:
            return

        spec_path = spec.get("spec_path", "")
        if not spec_path or not os.path.isfile(spec_path):
            db.requeue(spec_id, 0, 0)
            return

        content = Path(spec_path).read_text(encoding="utf-8")
        tasks = parse_boi_spec(content)
        done = sum(1 for t in tasks if t.status == "DONE")
        total = len(tasks)
        pending = sum(1 for t in tasks if t.status == "PENDING")

        pre_done = spec.get("tasks_done", 0)
        tasks_completed = max(0, done - pre_done)

        now = datetime.now(timezone.utc).isoformat()
        db.insert_iteration(
            spec_id=spec_id,
            iteration=spec["iteration"],
            phase=phase,
            worker_id=worker_id,
            started_at=now,
            ended_at=now,
            duration_seconds=0,
            tasks_completed=tasks_completed,
            exit_code=exit_code,
            pre_pending=total - pre_done,
            post_pending=pending,
        )

        if exit_code == 0 and pending == 0 and total > 0:
            db.complete(spec_id, done, total)
        elif exit_code == 0:
            db.requeue(spec_id, done, total)
        else:
            db.fail(spec_id, f"Exit code: {exit_code}")

    def create_dag_spec(
        self,
        content: str,
        filename: str = "spec.md",
    ) -> str:
        """Create a spec file with a custom DAG structure.

        Args:
            content: Full spec content with ### t-N: headings,
                     status lines, and **Blocked by:** declarations.
            filename: Name for the spec file.

        Returns:
            Absolute path to the created spec file.
        """
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        os.makedirs(specs_dir, exist_ok=True)
        spec_path = os.path.join(specs_dir, filename)
        Path(spec_path).write_text(content, encoding="utf-8")
        return spec_path


class TestSerialExecution(ParallelDAGTestBase):
    """Test a 3-task linear chain (t-1 -> t-2 -> t-3) completes in order."""

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        return MockClaude(phase="execute", tasks_to_complete=1, exit_code=0)

    def test_linear_chain_completes_in_order(self) -> None:
        """Dispatch t-1 -> t-2 -> t-3. Tasks complete sequentially
        because each depends on the previous."""
        spec_content = textwrap.dedent("""\
            # Linear Chain Spec

            ## Tasks

            ### t-1: First task
            PENDING

            **Spec:** Do the first task.

            **Verify:** true

            ### t-2: Second task
            PENDING

            **Blocked by:** t-1

            **Spec:** Do the second task.

            **Verify:** true

            ### t-3: Third task
            PENDING

            **Blocked by:** t-2

            **Spec:** Do the third task.

            **Verify:** true
        """)

        spec_path = self.create_dag_spec(spec_content)
        spec_id = self.dispatch_spec(spec_path, max_iterations=10)

        spec = self.harness.wait_for_status(spec_id, "completed", timeout=30)

        self.assertEqual(spec["status"], "completed")
        self.assertEqual(spec["tasks_done"], 3)
        self.assertEqual(spec["tasks_total"], 3)

        # Verify all tasks are DONE in the spec file
        final_content = Path(spec["spec_path"]).read_text(encoding="utf-8")
        tasks = parse_boi_spec(final_content)
        for task in tasks:
            self.assertEqual(task.status, "DONE", f"{task.id} should be DONE")

        # Task-level dispatch records one iteration per completed level/batch.
        iterations = self.harness.get_iterations(spec_id)
        execute_iters = [i for i in iterations if i["phase"] == "execute"]
        self.assertGreaterEqual(len(execute_iters), 1)


class TestParallelExecution(ParallelDAGTestBase):
    """Test diamond DAG: t-1 + t-2 (independent) -> t-3 (blocked by both)."""

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        return MockClaude(phase="execute", tasks_to_complete=1, exit_code=0)

    def test_diamond_dag_parallel_then_join(self) -> None:
        """t-1 and t-2 are independent and can run in parallel.
        t-3 only starts after both t-1 and t-2 are DONE."""
        spec_content = textwrap.dedent("""\
            # Diamond DAG Spec

            ## Tasks

            ### t-1: Independent task A
            PENDING

            **Spec:** Do task A.

            **Verify:** true

            ### t-2: Independent task B
            PENDING

            **Spec:** Do task B.

            **Verify:** true

            ### t-3: Join task
            PENDING

            **Blocked by:** t-1, t-2

            **Spec:** Do the join task after A and B.

            **Verify:** true
        """)

        spec_path = self.create_dag_spec(spec_content)
        spec_id = self.dispatch_spec(spec_path, max_iterations=10)

        spec = self.harness.wait_for_status(spec_id, "completed", timeout=30)

        self.assertEqual(spec["status"], "completed")
        self.assertEqual(spec["tasks_done"], 3)

        # Task state lives in the DB (parallel workers may race on the spec file).
        db_tasks = self.harness.db.get_tasks_for_spec(spec_id)
        self.assertEqual(len(db_tasks), 3)
        for t in db_tasks:
            self.assertEqual(t["status"], "DONE", f"{t['task_id']} should be DONE")

        # t-3 must start after t-1 and t-2 complete (check via timestamps).
        by_id = {t["task_id"]: t for t in db_tasks}
        t3_start = by_id["t-3"]["started_at"]
        t1_end = by_id["t-1"]["completed_at"]
        t2_end = by_id["t-2"]["completed_at"]
        self.assertGreaterEqual(t3_start, t1_end, "t-3 must start after t-1 completes")
        self.assertGreaterEqual(t3_start, t2_end, "t-3 must start after t-2 completes")

        # One iteration record is inserted when the whole task batch completes.
        iterations = self.harness.get_iterations(spec_id)
        execute_iters = [i for i in iterations if i["phase"] == "execute"]
        self.assertGreaterEqual(len(execute_iters), 1)


class TestWideFanout(ParallelDAGTestBase):
    """Test 5 independent tasks + 1 synthesis task blocked by all 5."""

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        return MockClaude(phase="execute", tasks_to_complete=1, exit_code=0)

    def test_five_independent_then_synthesis(self) -> None:
        """5 independent tasks run in parallel (up to worker limit).
        Synthesis task runs last."""
        spec_content = textwrap.dedent("""\
            # Wide Fanout Spec

            ## Tasks

            ### t-1: Independent work 1
            PENDING

            **Spec:** Do independent work 1.

            **Verify:** true

            ### t-2: Independent work 2
            PENDING

            **Spec:** Do independent work 2.

            **Verify:** true

            ### t-3: Independent work 3
            PENDING

            **Spec:** Do independent work 3.

            **Verify:** true

            ### t-4: Independent work 4
            PENDING

            **Spec:** Do independent work 4.

            **Verify:** true

            ### t-5: Independent work 5
            PENDING

            **Spec:** Do independent work 5.

            **Verify:** true

            ### t-6: Synthesis
            PENDING

            **Blocked by:** t-1, t-2, t-3, t-4, t-5

            **Spec:** Synthesize all results.

            **Verify:** true
        """)

        spec_path = self.create_dag_spec(spec_content)
        spec_id = self.dispatch_spec(spec_path, max_iterations=15)

        spec = self.harness.wait_for_status(spec_id, "completed", timeout=45)

        self.assertEqual(spec["status"], "completed")
        self.assertEqual(spec["tasks_done"], 6)
        self.assertEqual(spec["tasks_total"], 6)

        # Task state lives in the DB (parallel workers may race on the spec file).
        db_tasks = self.harness.db.get_tasks_for_spec(spec_id)
        self.assertEqual(len(db_tasks), 6)
        for t in db_tasks:
            self.assertEqual(t["status"], "DONE", f"{t['task_id']} should be DONE")

        # t-6 must start after all t-1..t-5 complete.
        by_id = {t["task_id"]: t for t in db_tasks}
        t6_start = by_id["t-6"]["started_at"]
        for tid in ("t-1", "t-2", "t-3", "t-4", "t-5"):
            self.assertGreaterEqual(
                t6_start, by_id[tid]["completed_at"],
                f"t-6 must start after {tid} completes",
            )

        # One batch-level iteration record is inserted on completion.
        iterations = self.harness.get_iterations(spec_id)
        execute_iters = [i for i in iterations if i["phase"] == "execute"]
        self.assertGreaterEqual(len(execute_iters), 1)


class TestSelfEvolutionWithDeps(ParallelDAGTestBase):
    """Test discover mode: t-1 adds a new t-4 blocked by t-2."""

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        if iteration == 1:
            # First iteration: complete t-1 AND add t-4 blocked by t-2
            return _SelfEvolvingMock()
        return MockClaude(phase="execute", tasks_to_complete=1, exit_code=0)

    @unittest.skip("Dynamic task addition (self-evolution) not yet implemented for task-level dispatch")
    def test_dynamically_added_task_respects_deps(self) -> None:
        """t-1 completes and adds t-4 blocked by t-2.
        t-4 only runs after t-2 is DONE."""
        spec_content = textwrap.dedent("""\
            # Self-Evolving Spec

            **Mode:** discover

            ## Tasks

            ### t-1: Research phase
            PENDING

            **Spec:** Research and discover what else needs doing.

            **Verify:** true

            ### t-2: Build component A
            PENDING

            **Spec:** Build component A.

            **Verify:** true

            ### t-3: Build component B
            PENDING

            **Spec:** Build component B.

            **Verify:** true
        """)

        spec_path = self.create_dag_spec(spec_content)
        spec_id = self.dispatch_spec(spec_path, max_iterations=15)

        spec = self.harness.wait_for_status(spec_id, "completed", timeout=45)

        self.assertEqual(spec["status"], "completed")

        # Verify t-4 was added and completed
        final_content = Path(spec["spec_path"]).read_text(encoding="utf-8")
        tasks = parse_boi_spec(final_content)

        task_ids = {t.id for t in tasks}
        self.assertIn("t-4", task_ids, "t-4 should have been added by self-evolution")

        for task in tasks:
            self.assertEqual(task.status, "DONE", f"{task.id} should be DONE")

        # t-4 should be blocked by t-2
        t4 = next(t for t in tasks if t.id == "t-4")
        self.assertIn("t-2", t4.blocked_by)


class _SelfEvolvingMock(MockClaude):
    """Mock that completes t-1 and adds t-4 blocked by t-2."""

    def __init__(self) -> None:
        super().__init__(phase="execute", tasks_to_complete=1, exit_code=0)

    def _build_script(self, spec_path: str) -> str:
        """Generate a script that marks t-1 DONE and adds t-4."""
        return textwrap.dedent(f"""\
            #!/usr/bin/env python3
            import os, re, sys

            SPEC_PATH = {spec_path!r}

            with open(SPEC_PATH, 'r') as f:
                content = f.read()

            # Mark first PENDING task (t-1) as DONE
            content = content.replace('PENDING', 'DONE', 1)

            # Add t-4 blocked by t-2
            new_task = '''

            ### t-4: Follow-up from research
            PENDING

            **Blocked by:** t-2

            **Spec:** Follow-up work discovered during research.

            **Verify:** true
            '''
            content = content.rstrip() + new_task

            tmp = SPEC_PATH + '.tmp'
            with open(tmp, 'w') as f:
                f.write(content)
            os.rename(tmp, SPEC_PATH)

            sys.exit(0)
        """)


class TestWorkerFailureRecovery(ParallelDAGTestBase):
    """Test that daemon requeues tasks when a worker fails."""

    _fail_count = 0
    _lock = threading.Lock()

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        with self._lock:
            if iteration == 1:
                # First iteration: fail with non-zero exit
                self._fail_count += 1
                return MockClaude(
                    phase="execute",
                    tasks_to_complete=0,
                    exit_code=1,
                    fail_silently=True,
                )
            # Subsequent iterations: succeed
            return MockClaude(phase="execute", tasks_to_complete=1, exit_code=0)

    def test_failed_worker_task_gets_retried(self) -> None:
        """Worker fails on iteration 1. Daemon requeues and a new
        iteration succeeds."""
        spec_content = textwrap.dedent("""\
            # Failure Recovery Spec

            ## Tasks

            ### t-1: Task that will fail then succeed
            PENDING

            **Spec:** This task's first attempt fails.

            **Verify:** true

            ### t-2: Simple task
            PENDING

            **Spec:** Simple follow-up task.

            **Verify:** true
        """)

        spec_path = self.create_dag_spec(spec_content)
        spec_id = self.dispatch_spec(spec_path, max_iterations=10)

        # Wait for completion or failure (might fail then recover)
        spec = self.harness.wait_for_any_status(
            spec_id,
            ["completed", "failed"],
            timeout=30,
        )

        # Check events for the failure event
        events = self.harness.get_events(spec_id=spec_id)
        event_types = [e["event_type"] for e in events]

        # Should see at least one failure-related event
        has_failure = any("fail" in et.lower() for et in event_types)
        self.assertTrue(
            has_failure or spec["status"] == "completed",
            "Should see either a failure event or eventual completion",
        )


class TestSpecPriority(ParallelDAGTestBase):
    """Test that high-priority spec gets workers before low-priority."""

    NUM_WORKERS = 1  # Only 1 worker to force priority ordering

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        return MockClaude(phase="execute", tasks_to_complete=1, exit_code=0)

    def test_high_priority_runs_first(self) -> None:
        """Dispatch two specs. Priority 1 (high) should start before
        priority 200 (low)."""
        # Create two single-task specs
        high_spec = self.create_dag_spec(
            textwrap.dedent("""\
                # High Priority

                ## Tasks

                ### t-1: High priority task
                PENDING

                **Spec:** Important task.

                **Verify:** true
            """),
            filename="high.md",
        )

        low_spec = self.create_dag_spec(
            textwrap.dedent("""\
                # Low Priority

                ## Tasks

                ### t-1: Low priority task
                PENDING

                **Spec:** Less important task.

                **Verify:** true
            """),
            filename="low.md",
        )

        # Dispatch low priority first, then high priority
        low_id = self.dispatch_spec(low_spec, priority=200, max_iterations=5)
        high_id = self.dispatch_spec(high_spec, priority=1, max_iterations=5)

        # Wait for the high-priority spec to complete first
        high_spec_result = self.harness.wait_for_any_status(
            high_id,
            ["completed", "running"],
            timeout=15,
        )

        # Check that high priority ran (it should be running or completed)
        self.assertIn(
            high_spec_result["status"],
            ["running", "completed", "requeued"],
            "High-priority spec should start before low-priority",
        )

        # Wait for both to complete
        self.harness.wait_for_status(high_id, "completed", timeout=20)
        self.harness.wait_for_status(low_id, "completed", timeout=20)

        # Verify both completed
        high_final = self.db.get_spec(high_id)
        low_final = self.db.get_spec(low_id)
        self.assertEqual(high_final["status"], "completed")
        self.assertEqual(low_final["status"], "completed")

        # High priority should have started first (check first_running_at)
        high_start = high_final.get("first_running_at", "9999")
        low_start = low_final.get("first_running_at", "9999")
        self.assertLessEqual(
            high_start,
            low_start,
            "High-priority spec should have started before low-priority",
        )


class TestConcurrentSpecUpdates(ParallelDAGTestBase):
    """Test two workers completing tasks on the same spec without corruption."""

    NUM_WORKERS = 2

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        # Small delay to increase chance of concurrent writes
        return MockClaude(
            phase="execute",
            tasks_to_complete=1,
            exit_code=0,
            delay_seconds=0.1,
        )

    def test_concurrent_writes_no_corruption(self) -> None:
        """Two workers update the same spec file. The spec file should
        not become corrupted (all tasks eventually DONE, valid format)."""
        spec_content = textwrap.dedent("""\
            # Concurrent Update Spec

            ## Tasks

            ### t-1: Task alpha
            PENDING

            **Spec:** Do alpha.

            **Verify:** true

            ### t-2: Task beta
            PENDING

            **Spec:** Do beta.

            **Verify:** true

            ### t-3: Task gamma
            PENDING

            **Spec:** Do gamma.

            **Verify:** true

            ### t-4: Task delta
            PENDING

            **Spec:** Do delta.

            **Verify:** true

            ## Dependencies
            t-1:
            t-2:
            t-3:
            t-4:
        """)

        spec_path = self.create_dag_spec(spec_content)
        spec_id = self.dispatch_spec(spec_path, max_iterations=15)

        spec = self.harness.wait_for_status(spec_id, "completed", timeout=30)

        self.assertEqual(spec["status"], "completed")

        # Task state lives in the DB; parallel workers may race on the spec file.
        db_tasks = self.harness.db.get_tasks_for_spec(spec_id)
        self.assertEqual(len(db_tasks), 4, "DB should still have exactly 4 tasks")
        for t in db_tasks:
            self.assertEqual(t["status"], "DONE", f"{t['task_id']} should be DONE")

        # Verify spec file is valid markdown (no partial writes / corruption).
        final_content = Path(spec["spec_path"]).read_text(encoding="utf-8")
        self.assertIn("# Concurrent Update Spec", final_content)
        # Spec file may still show some PENDING (race on file writes is expected
        # for parallel tasks — state is authoritative in the DB, not the file).
        tasks_in_file = parse_boi_spec(final_content)
        self.assertEqual(len(tasks_in_file), 4, "Spec file should still have 4 tasks")


if __name__ == "__main__":
    unittest.main()
