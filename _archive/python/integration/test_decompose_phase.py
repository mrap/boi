# test_decompose_phase.py — Integration test for the decompose phase lifecycle.
#
# Verifies:
#   Full decompose->execute->complete lifecycle:
#     - Dispatch a Decompose-mode spec (no initial tasks).
#     - MockClaude (decompose phase) generates 5 PENDING tasks with
#       valid Spec/Verify sections and an ## Approach section.
#     - Daemon validates decomposition (3-30 tasks, has Spec/Verify).
#     - Daemon transitions to execute phase.
#     - MockClaude (execute phase) completes all tasks.
#     - Spec marked completed.

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


# ── Custom MockClaude for decompose that writes valid spec ────────


class DecomposeMockClaude:
    """A mock Claude for the decompose phase that writes a fully valid
    decomposed spec (with ## Approach section and proper task format).

    The standard MockClaude.handle_decompose() uses add_pending_tasks()
    which produces tasks with **Spec:** and **Verify:** sections, but
    the decomposition validator also requires an ## Approach section.
    This custom mock handles that.

    Args:
        num_tasks: Number of PENDING tasks to generate (3-30).
        exit_code: Exit code for the script (default 0).
        delay_seconds: Seconds to sleep before exiting.
        fail_silently: If True, exit without modifying the spec.
    """

    def __init__(
        self,
        num_tasks: int = 5,
        exit_code: int = 0,
        delay_seconds: float = 0,
        fail_silently: bool = False,
    ) -> None:
        self.num_tasks = num_tasks
        self.exit_code = exit_code
        self.delay_seconds = delay_seconds
        self.fail_silently = fail_silently

    def generate_script(self, spec_path: str, output_dir: str) -> str:
        """Generate a standalone Python script that writes a valid
        decomposed spec with ## Approach section and proper tasks."""
        script_path = os.path.join(output_dir, "mock_claude.py")
        script = self._build_script(spec_path)
        Path(script_path).write_text(script, encoding="utf-8")
        os.chmod(script_path, 0o755)
        return script_path

    def _build_script(self, spec_path: str) -> str:
        lines = [
            "#!/usr/bin/env python3",
            "import sys, time, os",
            "",
            f"SPEC_PATH = {spec_path!r}",
            f"NUM_TASKS = {self.num_tasks}",
            f"EXIT_CODE = {self.exit_code}",
            f"DELAY_SECONDS = {self.delay_seconds}",
            f"FAIL_SILENTLY = {self.fail_silently}",
            "",
            "def main():",
            "    if DELAY_SECONDS > 0:",
            "        time.sleep(DELAY_SECONDS)",
            "    if FAIL_SILENTLY:",
            "        sys.exit(EXIT_CODE)",
            "",
            "    # Read existing spec and append approach + tasks",
            "    with open(SPEC_PATH, 'r') as f:",
            "        content = f.read()",
            "",
            "    parts = [content.rstrip(), '', '## Approach', '',",
            "             'Decompose the goal into concrete tasks.', '']",
            "",
            "    for i in range(1, NUM_TASKS + 1):",
            "        parts.append(f'### t-{i}: Decomposed task {i}')",
            "        parts.append('PENDING')",
            "        parts.append('')",
            "        parts.append(f'**Spec:** Implement decomposed task {i}.')",
            "        parts.append('')",
            "        parts.append('**Verify:** true')",
            "        parts.append('')",
            "",
            "    tmp = SPEC_PATH + '.tmp'",
            "    with open(tmp, 'w') as f:",
            "        f.write('\\n'.join(parts))",
            "    os.rename(tmp, SPEC_PATH)",
            "    sys.exit(EXIT_CODE)",
            "",
            "if __name__ == '__main__':",
            "    main()",
        ]
        return "\n".join(lines)

    def generate_run_script(
        self,
        spec_path: str,
        exit_file: str,
        output_dir: str,
    ) -> str:
        """Generate a bash run script wrapping the mock decompose script."""
        mock_script = self.generate_script(spec_path, output_dir)

        run_script_path = os.path.join(output_dir, "mock_run.sh")
        run_script = (
            f"#!/bin/bash\n"
            f"set -uo pipefail\n"
            f"{sys.executable} {mock_script}\n"
            f"_EXIT=$?\n"
            f'echo "$_EXIT" > {exit_file}\n'
            f"exit $_EXIT\n"
        )
        Path(run_script_path).write_text(run_script, encoding="utf-8")
        os.chmod(run_script_path, 0o755)
        return run_script_path


# ── Helper: Create decompose-mode spec ───────────────────────────


def _create_decompose_spec(specs_dir: str, filename: str = "spec.md") -> str:
    """Create a decompose-mode spec with no initial tasks.

    The spec has a goal but no ### t-N headings. The decompose phase
    will add tasks.

    Args:
        specs_dir: Directory to write the spec file.
        filename: Name for the spec file.

    Returns:
        Absolute path to the created spec file.
    """
    os.makedirs(specs_dir, exist_ok=True)
    spec_path = os.path.join(specs_dir, filename)

    content = (
        "# [Decompose] Test Spec\n"
        "\n"
        "## Goal\n"
        "\n"
        "Build a comprehensive test suite for the decompose phase "
        "of the BOI reliability rewrite. This goal is sufficiently "
        "long to meet the minimum word count requirement for "
        "validation purposes and describes what we want to achieve.\n"
        "\n"
        "## Constraints\n"
        "\n"
        "- Must use unittest.TestCase\n"
        "- Must be self-contained\n"
        "\n"
    )

    Path(spec_path).write_text(content, encoding="utf-8")
    return spec_path


# ── Decompose completion handler ─────────────────────────────────


def _decompose_completion_handler(
    harness,
    spec_id,
    phase,
    exit_code,
    worker_id,
):
    """Completion handler that drives the decompose->execute->complete cycle.

    After decompose completes:
      - Validates the spec (3-30 tasks, Spec/Verify sections, Approach).
      - If valid: transitions to execute phase.
      - If invalid: fails spec.

    After execute completes:
      - If all tasks done: marks completed.
      - Otherwise: requeues for another execute iteration.

    Args:
        harness: The DaemonTestHarness.
        spec_id: The spec being processed.
        phase: Phase that just completed.
        exit_code: Worker exit code.
        worker_id: Worker that ran the phase.
    """
    db = harness._daemon.db
    spec = db.get_spec(spec_id)
    if spec is None:
        return

    now = datetime.now(timezone.utc).isoformat()
    spec_path = spec.get("spec_path", "")
    if not spec_path or not os.path.isfile(spec_path):
        db.fail(spec_id, "Spec file not found")
        return

    if phase == "decompose":
        _handle_decompose_completion(
            db, spec_id, spec, spec_path, exit_code, worker_id, now
        )
    elif phase == "execute":
        _handle_execute_completion(
            db, spec_id, spec, spec_path, exit_code, worker_id, now
        )


def _handle_decompose_completion(
    db, spec_id, spec, spec_path, exit_code, worker_id, now
):
    """Handle decompose phase completion: validate and transition."""
    from lib.spec_validator import validate_spec_file

    # Record iteration
    db.insert_iteration(
        spec_id=spec_id,
        iteration=spec["iteration"],
        phase="decompose",
        worker_id=worker_id,
        started_at=now,
        ended_at=now,
        duration_seconds=0,
        tasks_completed=0,
        exit_code=exit_code,
        pre_pending=0,
        post_pending=0,
    )

    if exit_code != 0:
        db.fail(spec_id, f"Decompose worker exited with code {exit_code}")
        return

    content = Path(spec_path).read_text(encoding="utf-8")

    # Validate the decomposed spec
    validation = validate_spec_file(spec_path)
    errors = []

    if not validation.valid:
        errors.extend(validation.errors)

    task_count = validation.total
    if task_count < 3:
        errors.append(f"Too few tasks ({task_count}). Minimum is 3.")
    elif task_count > 30:
        errors.append(f"Too many tasks ({task_count}). Maximum is 30.")

    if "## Approach" not in content:
        errors.append("Missing '## Approach' section.")

    if errors:
        db.fail(
            spec_id,
            "decomposition_failed: " + "; ".join(errors),
        )
        return

    # Validation passed: transition to execute phase
    with db.lock:
        db.conn.execute(
            "UPDATE specs SET "
            "status = 'requeued', "
            "phase = 'execute', "
            "tasks_done = ?, "
            "tasks_total = ? "
            "WHERE id = ?",
            (validation.done, task_count, spec_id),
        )
        db._log_event(
            "requeued",
            f"Decomposition complete: {task_count} tasks. "
            "Transitioning to execute phase.",
            spec_id=spec_id,
        )
        db.conn.commit()


def _handle_execute_completion(
    db, spec_id, spec, spec_path, exit_code, worker_id, now
):
    """Handle execute phase completion after decompose."""
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
        phase="execute",
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

    if pending == 0 and total > 0:
        db.complete(spec_id, done, total)
    else:
        db.requeue(spec_id, done, total)


# ── Test: Full decompose->execute->complete lifecycle ────────────


class TestDecomposeLifecycle(IntegrationTestCase):
    """Full decompose->execute->complete lifecycle.

    Flow:
      1. Dispatch a decompose-mode spec (no initial tasks).
      2. Daemon runs decompose phase.
      3. MockClaude generates 5 valid PENDING tasks.
      4. Daemon validates decomposition (3-30 tasks, Spec/Verify).
      5. Daemon transitions to execute phase.
      6. MockClaude completes all tasks.
      7. Spec marked completed.
    """

    NUM_WORKERS = 1

    def setUp(self) -> None:
        super().setUp()
        self.harness.start()

        harness_ref = self.harness
        daemon = self.harness._daemon
        daemon._dispatch_phase_completion = (
            lambda spec_id, phase, exit_code, worker_id: (
                _decompose_completion_handler(
                    harness_ref, spec_id, phase, exit_code, worker_id
                )
            )
        )

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        """Decompose: generate 5 valid tasks.
        Execute: complete all available tasks."""
        if phase == "decompose":
            # Return a DecomposeMockClaude wrapped as MockClaude-compatible
            # The harness calls generate_run_script, which both support.
            return DecomposeMockClaude(num_tasks=5, exit_code=0)
        elif phase == "execute":
            return MockClaude(
                phase="execute",
                tasks_to_complete=99,
                exit_code=0,
            )
        return MockClaude(exit_code=0)

    def _dispatch_decompose_spec(
        self, spec_path: str, max_iterations: int = 10
    ) -> str:
        """Dispatch a spec and set its phase to 'decompose'.

        The standard enqueue sets phase='execute'. For decompose-mode
        specs, we update the phase to 'decompose' after enqueue.
        """
        spec_id = self.dispatch_spec(spec_path, max_iterations=max_iterations)

        with self.db.lock:
            self.db.conn.execute(
                "UPDATE specs SET phase = 'decompose' WHERE id = ?",
                (spec_id,),
            )
            self.db.conn.commit()

        return spec_id

    def test_full_decompose_execute_lifecycle(self) -> None:
        """Dispatch decompose spec, decompose generates 5 tasks,
        execute completes them all, spec marked completed."""
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        spec_path = _create_decompose_spec(specs_dir)
        spec_id = self._dispatch_decompose_spec(spec_path)

        spec = self.harness.wait_for_status(
            spec_id, "completed", timeout=45
        )

        self.assertEqual(spec["status"], "completed")
        self.assertEqual(spec["tasks_done"], 5)
        self.assertEqual(spec["tasks_total"], 5)

    def test_spec_file_has_all_tasks_done(self) -> None:
        """After completion, all 5 decomposed tasks should be DONE."""
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        spec_path = _create_decompose_spec(specs_dir, filename="spec2.md")
        spec_id = self._dispatch_decompose_spec(spec_path)

        self.harness.wait_for_status(
            spec_id, "completed", timeout=45
        )

        spec = self.db.get_spec(spec_id)
        content = Path(spec["spec_path"]).read_text(encoding="utf-8")

        self.assertNotIn("\nPENDING\n", content)

        from lib.spec_parser import parse_boi_spec

        tasks = parse_boi_spec(content)
        self.assertEqual(len(tasks), 5)
        for task in tasks:
            self.assertEqual(task.status, "DONE")

    def test_spec_file_has_approach_section(self) -> None:
        """After decompose, the spec should contain ## Approach."""
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        spec_path = _create_decompose_spec(specs_dir, filename="spec3.md")
        spec_id = self._dispatch_decompose_spec(spec_path)

        self.harness.wait_for_status(
            spec_id, "completed", timeout=45
        )

        spec = self.db.get_spec(spec_id)
        content = Path(spec["spec_path"]).read_text(encoding="utf-8")

        self.assertIn("## Approach", content)

    def test_iterations_include_decompose_and_execute(self) -> None:
        """Iterations table should have both decompose and execute entries."""
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        spec_path = _create_decompose_spec(specs_dir, filename="spec4.md")
        spec_id = self._dispatch_decompose_spec(spec_path)

        self.harness.wait_for_status(
            spec_id, "completed", timeout=45
        )

        iterations = self.harness.get_iterations(spec_id)
        phases = [it["phase"] for it in iterations]

        self.assertIn("decompose", phases)
        self.assertIn("execute", phases)

        decompose_iters = [
            it for it in iterations if it["phase"] == "decompose"
        ]
        execute_iters = [
            it for it in iterations if it["phase"] == "execute"
        ]

        # Exactly 1 decompose iteration
        self.assertEqual(
            len(decompose_iters), 1,
            f"Expected 1 decompose iteration, got "
            f"{len(decompose_iters)}. All: {iterations}",
        )

        # At least 1 execute iteration
        self.assertGreaterEqual(
            len(execute_iters), 1,
            f"Expected at least 1 execute iteration, got "
            f"{len(execute_iters)}. All: {iterations}",
        )

    def test_events_show_decompose_transitions(self) -> None:
        """Events should record decompose phase transitions."""
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        spec_path = _create_decompose_spec(specs_dir, filename="spec5.md")
        spec_id = self._dispatch_decompose_spec(spec_path)

        self.harness.wait_for_status(
            spec_id, "completed", timeout=45
        )

        events = self.harness.get_events(spec_id=spec_id)
        event_types = [e["event_type"] for e in events]

        self.assertIn("queued", event_types)
        self.assertIn("running", event_types)
        self.assertIn("requeued", event_types)
        self.assertIn("completed", event_types)

        # Check that a requeue event mentions decomposition
        requeue_messages = [
            e.get("message", "")
            for e in events
            if e["event_type"] == "requeued"
        ]
        decompose_msgs = [
            m for m in requeue_messages
            if "decompos" in m.lower()
        ]
        self.assertGreater(
            len(decompose_msgs), 0,
            "Expected at least one requeue event mentioning decompose. "
            f"Got requeue messages: {requeue_messages}",
        )

    def test_phase_transitions_correctly(self) -> None:
        """After decompose, the spec phase should be 'execute' before
        completion, and status should reach 'completed'."""
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        spec_path = _create_decompose_spec(specs_dir, filename="spec6.md")
        spec_id = self._dispatch_decompose_spec(spec_path)

        # Wait for completion
        spec = self.harness.wait_for_status(
            spec_id, "completed", timeout=45
        )

        # Verify final state
        self.assertEqual(spec["status"], "completed")

        # The events should show at least one 'running' event for
        # decompose and at least one for execute
        events = self.harness.get_events(spec_id=spec_id)
        running_events = [
            e for e in events if e["event_type"] == "running"
        ]
        self.assertGreaterEqual(
            len(running_events), 2,
            f"Expected at least 2 running events (decompose + execute), "
            f"got {len(running_events)}",
        )


if __name__ == "__main__":
    unittest.main()
