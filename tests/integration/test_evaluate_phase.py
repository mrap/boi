# test_evaluate_phase.py — Integration test for the evaluate phase lifecycle.
#
# Verifies:
#   (a) Spec completes when all Success Criteria are met (goal_achieved).
#   (b) Spec fails after max_iterations with partial criteria.
#   (c) Stall detection: 5 iterations with no criteria progress.

import os
import re
import sys
import unittest
from datetime import datetime, timezone
from pathlib import Path

# Add project root to path
_PROJECT_ROOT = str(Path(__file__).resolve().parent.parent.parent)
sys.path.insert(0, _PROJECT_ROOT)

from tests.integration.conftest import (
    IntegrationTestCase,
    MockClaude,
)


# ── Helper: Create Generate-mode spec ─────────────────────────────


def _create_generate_spec(
    specs_dir: str,
    num_criteria: int = 3,
    num_tasks: int = 1,
    filename: str = "spec.md",
) -> str:
    """Create a Generate-mode spec with Success Criteria checkboxes.

    Args:
        specs_dir: Directory to write the spec file.
        num_criteria: Number of unchecked Success Criteria.
        num_tasks: Number of initial PENDING tasks.
        filename: Name for the spec file.

    Returns:
        Absolute path to the created spec file.
    """
    os.makedirs(specs_dir, exist_ok=True)
    spec_path = os.path.join(specs_dir, filename)

    lines = [
        "# [Generate] Test Spec\n",
        "\n",
        "**Mode:** generate\n",
        "\n",
        "## Success Criteria\n",
        "\n",
    ]
    for i in range(1, num_criteria + 1):
        lines.append(f"- [ ] Criterion {i}\n")
    lines.append("\n")
    lines.append("## Tasks\n")

    for tid in range(1, num_tasks + 1):
        lines.append(
            f"\n### t-{tid}: Initial task {tid}\n"
            "PENDING\n\n"
            f"**Spec:** Set up initial work {tid}.\n\n"
            "**Verify:** true\n"
        )

    Path(spec_path).write_text("".join(lines), encoding="utf-8")
    return spec_path


# ── Helper: Add PENDING tasks for unmet criteria ──────────────────


def _add_tasks_for_unmet(spec_path: str, unmet: list[str]) -> None:
    """Append PENDING tasks for each unmet criterion to the spec."""
    content = Path(spec_path).read_text(encoding="utf-8")
    nums = re.findall(r"### t-(\d+)", content)
    next_id = max(int(n) for n in nums) + 1 if nums else 1

    new_tasks = []
    for i, criterion in enumerate(unmet):
        tid = next_id + i
        new_tasks.append(
            f"\n### t-{tid}: Address: {criterion}\n"
            "PENDING\n\n"
            f"**Spec:** Implement: {criterion}.\n\n"
            "**Verify:** true\n"
        )

    new_content = content.rstrip() + "\n" + "".join(new_tasks)
    tmp = spec_path + ".tmp"
    Path(tmp).write_text(new_content, encoding="utf-8")
    os.rename(tmp, spec_path)


# ── Shared evaluate completion handler ────────────────────────────


def _evaluate_completion_handler(
    harness,
    spec_id,
    phase,
    exit_code,
    worker_id,
    criteria_history_map,
):
    """Completion handler that drives execute->evaluate->execute cycles.

    After execute completes all tasks, triggers evaluate phase.
    After evaluate:
      - goal_achieved: complete spec.
      - max_iterations/stalled: fail spec.
      - Otherwise: add tasks for unmet criteria, back to execute.

    Args:
        harness: DaemonTestHarness.
        spec_id: Spec being processed.
        phase: Phase that completed.
        exit_code: Worker exit code.
        worker_id: Worker that ran the phase.
        criteria_history_map: Dict mapping spec_id to list of
            criteria_met counts per evaluate iteration.
    """
    db = harness._daemon.db
    spec = db.get_spec(spec_id)
    if spec is None:
        return

    now = datetime.now(timezone.utc).isoformat()
    spec_path = spec.get("spec_path", "")
    if not spec_path or not os.path.isfile(spec_path):
        db.requeue(spec_id, 0, 0)
        return

    from lib.spec_parser import parse_boi_spec

    content = Path(spec_path).read_text(encoding="utf-8")
    tasks = parse_boi_spec(content)
    done = sum(1 for t in tasks if t.status == "DONE")
    total = len(tasks)
    pending = sum(1 for t in tasks if t.status == "PENDING")

    pre_done = spec.get("tasks_done", 0)
    tasks_completed = max(0, done - pre_done)

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

    if exit_code != 0:
        db.requeue(spec_id, done, total)
        return

    if phase == "execute":
        if pending == 0 and total > 0:
            # All tasks done: trigger evaluate phase
            with db.lock:
                db.conn.execute(
                    "UPDATE specs SET "
                    "status = 'requeued', "
                    "phase = 'evaluate', "
                    "tasks_done = ?, "
                    "tasks_total = ? "
                    "WHERE id = ?",
                    (done, total, spec_id),
                )
                db._log_event(
                    "requeued",
                    "Triggering evaluate phase",
                    spec_id=spec_id,
                )
                db.conn.commit()
        else:
            db.requeue(spec_id, done, total)

    elif phase == "evaluate":
        from lib.evaluate import (
            build_completion_summary,
            check_convergence,
            evaluate_criteria,
            write_completion_summary_to_spec,
        )

        eval_result = evaluate_criteria(spec_path)

        # Track criteria history
        if spec_id not in criteria_history_map:
            criteria_history_map[spec_id] = []
        criteria_history_map[spec_id].append(eval_result.criteria_met)

        convergence = check_convergence(
            spec, spec_path, criteria_history_map[spec_id]
        )

        if convergence.should_stop:
            # Write completion summary to spec file
            summary = build_completion_summary(
                status=convergence.reason,
                queue_entry=spec,
                spec_path=spec_path,
                start_time=spec.get("submitted_at"),
            )
            write_completion_summary_to_spec(spec_path, summary)

            if convergence.reason == "goal_achieved":
                db.complete(spec_id, done, total)
            else:
                # max_iterations or stalled: fail
                db.fail(
                    spec_id,
                    f"Evaluate converged: {convergence.reason}. "
                    f"Criteria: {convergence.criteria_met}/"
                    f"{convergence.criteria_total}",
                )
        else:
            # Not converged: add tasks for unmet criteria, loop back
            if eval_result.unmet_criteria:
                _add_tasks_for_unmet(spec_path, eval_result.unmet_criteria)

            # Re-count tasks after adding
            content = Path(spec_path).read_text(encoding="utf-8")
            tasks = parse_boi_spec(content)
            new_done = sum(1 for t in tasks if t.status == "DONE")
            new_total = len(tasks)

            with db.lock:
                db.conn.execute(
                    "UPDATE specs SET "
                    "status = 'requeued', "
                    "phase = 'execute', "
                    "tasks_done = ?, "
                    "tasks_total = ? "
                    "WHERE id = ?",
                    (new_done, new_total, spec_id),
                )
                db._log_event(
                    "requeued",
                    f"Evaluate: {eval_result.criteria_met}/"
                    f"{eval_result.criteria_total} criteria met, "
                    "back to execute",
                    spec_id=spec_id,
                )
                db.conn.commit()


# ── Test (a): All criteria met → goal_achieved ────────────────────


class TestEvaluateAllCriteriaMet(IntegrationTestCase):
    """Dispatch a Generate-mode spec with 3 Success Criteria.

    Execute completes all tasks. Evaluate checks all criteria.
    Convergence: goal_achieved. Spec completed.
    """

    NUM_WORKERS = 1

    def setUp(self) -> None:
        super().setUp()
        self._criteria_history: dict[str, list[int]] = {}
        self.harness.start()

        history_ref = self._criteria_history
        harness_ref = self.harness
        daemon = self.harness._daemon
        daemon._dispatch_phase_completion = (
            lambda spec_id, phase, exit_code, worker_id: (
                _evaluate_completion_handler(
                    harness_ref, spec_id, phase, exit_code,
                    worker_id, history_ref,
                )
            )
        )

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        """Execute: complete all tasks.
        Evaluate: check all 3 criteria."""
        if phase == "execute":
            return MockClaude(
                phase="execute",
                tasks_to_complete=99,
                exit_code=0,
            )
        elif phase == "evaluate":
            return MockClaude(
                phase="evaluate",
                criteria_to_meet=[0, 1, 2],
                exit_code=0,
            )
        return MockClaude(exit_code=0)

    def test_completes_when_all_criteria_met(self) -> None:
        """Spec completes with goal_achieved when all criteria met."""
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        spec_path = _create_generate_spec(
            specs_dir, num_criteria=3, num_tasks=2
        )
        spec_id = self.dispatch_spec(spec_path, max_iterations=10)

        spec = self.harness.wait_for_status(
            spec_id, "completed", timeout=30
        )

        self.assertEqual(spec["status"], "completed")

    def test_spec_file_has_all_criteria_checked(self) -> None:
        """After completion, all criteria should be [x]."""
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        spec_path = _create_generate_spec(
            specs_dir, num_criteria=3, num_tasks=2,
            filename="spec2.md",
        )
        spec_id = self.dispatch_spec(spec_path, max_iterations=10)

        self.harness.wait_for_status(
            spec_id, "completed", timeout=30
        )

        spec = self.db.get_spec(spec_id)
        content = Path(spec["spec_path"]).read_text(encoding="utf-8")

        from lib.evaluate import parse_success_criteria

        criteria = parse_success_criteria(content)
        self.assertEqual(len(criteria), 3)
        for c in criteria:
            self.assertTrue(
                c["checked"],
                f"Criterion should be checked: {c['text']}",
            )

    def test_completion_summary_shows_goal_achieved(self) -> None:
        """Completion summary should show goal_achieved status."""
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        spec_path = _create_generate_spec(
            specs_dir, num_criteria=3, num_tasks=2,
            filename="spec3.md",
        )
        spec_id = self.dispatch_spec(spec_path, max_iterations=10)

        self.harness.wait_for_status(
            spec_id, "completed", timeout=30
        )

        spec = self.db.get_spec(spec_id)
        content = Path(spec["spec_path"]).read_text(encoding="utf-8")

        self.assertIn("## Completion Summary", content)
        self.assertIn("goal_achieved", content)

    def test_iterations_include_evaluate_phase(self) -> None:
        """Iterations table should include an evaluate-phase entry."""
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        spec_path = _create_generate_spec(
            specs_dir, num_criteria=3, num_tasks=2,
            filename="spec4.md",
        )
        spec_id = self.dispatch_spec(spec_path, max_iterations=10)

        self.harness.wait_for_status(
            spec_id, "completed", timeout=30
        )

        iterations = self.harness.get_iterations(spec_id)
        phases = [it["phase"] for it in iterations]

        self.assertIn("execute", phases)
        self.assertIn("evaluate", phases)

    def test_events_show_evaluate_transitions(self) -> None:
        """Events should record evaluate phase transitions."""
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        spec_path = _create_generate_spec(
            specs_dir, num_criteria=3, num_tasks=2,
            filename="spec5.md",
        )
        spec_id = self.dispatch_spec(spec_path, max_iterations=10)

        self.harness.wait_for_status(
            spec_id, "completed", timeout=30
        )

        events = self.harness.get_events(spec_id=spec_id)
        event_types = [e["event_type"] for e in events]

        self.assertIn("queued", event_types)
        self.assertIn("running", event_types)
        self.assertIn("completed", event_types)

        # At least one requeue event mentioning evaluate
        requeue_messages = [
            e.get("message", "")
            for e in events
            if e["event_type"] == "requeued"
        ]
        eval_msgs = [
            m for m in requeue_messages if "evaluate" in m.lower()
        ]
        self.assertGreater(
            len(eval_msgs), 0,
            "Expected at least one requeue event mentioning evaluate. "
            f"Got requeue messages: {requeue_messages}",
        )


# ── Test (b): Max iterations with partial criteria → failed ───────


class TestEvaluateMaxIterations(IntegrationTestCase):
    """Dispatch a Generate spec with 4 criteria. MockClaude checks
    1 criterion per evaluate pass. With max_iterations=3, only 3
    criteria get checked. Spec fails with max_iterations reason."""

    NUM_WORKERS = 1

    def setUp(self) -> None:
        super().setUp()
        self._criteria_history: dict[str, list[int]] = {}
        self.harness.start()

        history_ref = self._criteria_history
        harness_ref = self.harness
        daemon = self.harness._daemon
        daemon._dispatch_phase_completion = (
            lambda spec_id, phase, exit_code, worker_id: (
                _evaluate_completion_handler(
                    harness_ref, spec_id, phase, exit_code,
                    worker_id, history_ref,
                )
            )
        )

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        """Execute: complete all tasks.
        Evaluate: check first unchecked criterion only."""
        if phase == "execute":
            return MockClaude(
                phase="execute",
                tasks_to_complete=99,
                exit_code=0,
            )
        elif phase == "evaluate":
            # Check the first unchecked criterion each time
            return MockClaude(
                phase="evaluate",
                criteria_to_meet=[0],
                exit_code=0,
            )
        return MockClaude(exit_code=0)

    def test_fails_at_max_iterations(self) -> None:
        """Spec should fail after 3 execute iterations with 1
        criterion still unchecked."""
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        spec_path = _create_generate_spec(
            specs_dir, num_criteria=4, num_tasks=1
        )
        spec_id = self.dispatch_spec(spec_path, max_iterations=3)

        spec = self.harness.wait_for_status(
            spec_id, "failed", timeout=45
        )

        self.assertEqual(spec["status"], "failed")
        self.assertIn(
            "max_iterations",
            spec.get("failure_reason", ""),
        )

    def test_partial_criteria_met(self) -> None:
        """After max_iterations, 3 of 4 criteria should be met."""
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        spec_path = _create_generate_spec(
            specs_dir, num_criteria=4, num_tasks=1,
            filename="spec2.md",
        )
        spec_id = self.dispatch_spec(spec_path, max_iterations=3)

        self.harness.wait_for_status(
            spec_id, "failed", timeout=45
        )

        spec = self.db.get_spec(spec_id)
        content = Path(spec["spec_path"]).read_text(encoding="utf-8")

        from lib.evaluate import parse_success_criteria

        criteria = parse_success_criteria(content)
        checked = sum(1 for c in criteria if c["checked"])

        self.assertEqual(
            checked, 3,
            f"Expected 3 of 4 criteria met, got {checked}",
        )

    def test_completion_summary_shows_max_iterations(self) -> None:
        """Completion summary should show max_iterations reason."""
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        spec_path = _create_generate_spec(
            specs_dir, num_criteria=4, num_tasks=1,
            filename="spec3.md",
        )
        spec_id = self.dispatch_spec(spec_path, max_iterations=3)

        self.harness.wait_for_status(
            spec_id, "failed", timeout=45
        )

        spec = self.db.get_spec(spec_id)
        content = Path(spec["spec_path"]).read_text(encoding="utf-8")

        self.assertIn("## Completion Summary", content)
        self.assertIn("max_iterations", content)

    def test_exactly_three_execute_iterations(self) -> None:
        """Should have exactly 3 execute and 3 evaluate iterations."""
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        spec_path = _create_generate_spec(
            specs_dir, num_criteria=4, num_tasks=1,
            filename="spec4.md",
        )
        spec_id = self.dispatch_spec(spec_path, max_iterations=3)

        self.harness.wait_for_status(
            spec_id, "failed", timeout=45
        )

        iterations = self.harness.get_iterations(spec_id)
        exec_iters = [
            it for it in iterations if it["phase"] == "execute"
        ]
        eval_iters = [
            it for it in iterations if it["phase"] == "evaluate"
        ]

        self.assertEqual(
            len(exec_iters), 3,
            f"Expected 3 execute iterations, got "
            f"{len(exec_iters)}. All: {iterations}",
        )
        self.assertEqual(
            len(eval_iters), 3,
            f"Expected 3 evaluate iterations, got "
            f"{len(eval_iters)}. All: {iterations}",
        )


# ── Test (c): Stall detection (no progress for N iterations) ─────
#
# Patches STALL_THRESHOLD to 3 (from default 5) to reduce the number
# of worker launches needed. This avoids a known test-infrastructure
# issue where the daemon's completion handler silently fails after
# ~10 sequential worker launches in a single test.


class TestEvaluateStallDetection(IntegrationTestCase):
    """Dispatch a Generate spec with 5 criteria. MockClaude checks
    2 criteria on iteration 1, then no more. After 3 iterations
    with criteria_met stuck at 2, stall detection triggers.

    Uses patched STALL_THRESHOLD=3 to keep worker launches within
    the reliable range of the test harness."""

    NUM_WORKERS = 1
    TEST_STALL_THRESHOLD = 3

    def setUp(self) -> None:
        super().setUp()
        self._criteria_history: dict[str, list[int]] = {}
        self._eval_call_counts: dict[str, int] = {}

        # Patch STALL_THRESHOLD before starting daemon
        from lib import evaluate
        self._original_stall_threshold = evaluate.STALL_THRESHOLD
        evaluate.STALL_THRESHOLD = self.TEST_STALL_THRESHOLD

        self.harness.start()

        history_ref = self._criteria_history
        harness_ref = self.harness
        daemon = self.harness._daemon
        daemon._dispatch_phase_completion = (
            lambda spec_id, phase, exit_code, worker_id: (
                _evaluate_completion_handler(
                    harness_ref, spec_id, phase, exit_code,
                    worker_id, history_ref,
                )
            )
        )

    def tearDown(self) -> None:
        # Restore original STALL_THRESHOLD
        from lib import evaluate
        evaluate.STALL_THRESHOLD = self._original_stall_threshold
        super().tearDown()

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        """Execute: complete all tasks.
        Evaluate: check criteria 0,1 on first call, nothing after."""
        if phase == "execute":
            return MockClaude(
                phase="execute",
                tasks_to_complete=99,
                exit_code=0,
            )
        elif phase == "evaluate":
            key = f"{spec_id}-eval"
            count = self._eval_call_counts.get(key, 0)
            self._eval_call_counts[key] = count + 1

            if count == 0:
                # First evaluate: check criteria 0 and 1
                return MockClaude(
                    phase="evaluate",
                    criteria_to_meet=[0, 1],
                    exit_code=0,
                )
            else:
                # Subsequent evaluates: no new criteria
                return MockClaude(
                    phase="evaluate",
                    criteria_to_meet=[],
                    exit_code=0,
                )
        return MockClaude(exit_code=0)

    def test_stall_detection_triggers(self) -> None:
        """Spec should fail after repeated iterations with no criteria
        progress (criteria_met stuck at 2)."""
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        spec_path = _create_generate_spec(
            specs_dir, num_criteria=5, num_tasks=1
        )
        spec_id = self.dispatch_spec(spec_path, max_iterations=20)

        spec = self.harness.wait_for_status(
            spec_id, "failed", timeout=45
        )

        self.assertEqual(spec["status"], "failed")
        self.assertIn(
            "stalled",
            spec.get("failure_reason", ""),
        )

    def test_criteria_stuck_at_two(self) -> None:
        """After stall, only 2 of 5 criteria should be met."""
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        spec_path = _create_generate_spec(
            specs_dir, num_criteria=5, num_tasks=1,
            filename="spec2.md",
        )
        spec_id = self.dispatch_spec(spec_path, max_iterations=20)

        self.harness.wait_for_status(
            spec_id, "failed", timeout=45
        )

        spec = self.db.get_spec(spec_id)
        content = Path(spec["spec_path"]).read_text(encoding="utf-8")

        from lib.evaluate import parse_success_criteria

        criteria = parse_success_criteria(content)
        checked = sum(1 for c in criteria if c["checked"])

        self.assertEqual(
            checked, 2,
            f"Expected 2 of 5 criteria met (stalled), got {checked}",
        )

    def test_completion_summary_shows_stalled(self) -> None:
        """Completion summary should show stalled status."""
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        spec_path = _create_generate_spec(
            specs_dir, num_criteria=5, num_tasks=1,
            filename="spec3.md",
        )
        spec_id = self.dispatch_spec(spec_path, max_iterations=20)

        self.harness.wait_for_status(
            spec_id, "failed", timeout=45
        )

        spec = self.db.get_spec(spec_id)
        content = Path(spec["spec_path"]).read_text(encoding="utf-8")

        self.assertIn("## Completion Summary", content)
        self.assertIn("stalled", content)

    def test_evaluate_iterations_match_threshold(self) -> None:
        """Should have at least STALL_THRESHOLD evaluate iterations
        for stall detection to trigger."""
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        spec_path = _create_generate_spec(
            specs_dir, num_criteria=5, num_tasks=1,
            filename="spec4.md",
        )
        spec_id = self.dispatch_spec(spec_path, max_iterations=20)

        self.harness.wait_for_status(
            spec_id, "failed", timeout=45
        )

        iterations = self.harness.get_iterations(spec_id)
        eval_iters = [
            it for it in iterations if it["phase"] == "evaluate"
        ]

        self.assertGreaterEqual(
            len(eval_iters), self.TEST_STALL_THRESHOLD,
            f"Expected at least {self.TEST_STALL_THRESHOLD} evaluate "
            f"iterations for stall detection, got {len(eval_iters)}",
        )

    def test_events_show_evaluate_loop(self) -> None:
        """Events should show multiple evaluate loop-back transitions
        before stall detection."""
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        spec_path = _create_generate_spec(
            specs_dir, num_criteria=5, num_tasks=1,
            filename="spec5.md",
        )
        spec_id = self.dispatch_spec(spec_path, max_iterations=20)

        self.harness.wait_for_status(
            spec_id, "failed", timeout=45
        )

        events = self.harness.get_events(spec_id=spec_id)
        requeue_messages = [
            e.get("message", "")
            for e in events
            if e["event_type"] == "requeued"
        ]

        # Multiple requeue events mentioning evaluate
        eval_requeues = [
            m for m in requeue_messages
            if "evaluate" in m.lower() or "criteria" in m.lower()
        ]
        # Need at least (threshold - 1) loop-back requeues
        min_requeues = self.TEST_STALL_THRESHOLD - 1
        self.assertGreaterEqual(
            len(eval_requeues), min_requeues,
            f"Expected at least {min_requeues} evaluate-related "
            f"requeue events, got {len(eval_requeues)}. "
            f"Messages: {requeue_messages}",
        )


if __name__ == "__main__":
    unittest.main()
