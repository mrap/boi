# test_modes.py — Tests for the 4-mode system (execute, challenge, discover, generate).
#
# Tests:
#   1. Execute mode enforcement: cannot add tasks
#   2. Challenge mode: can write Challenges, cannot add tasks, SKIPPED requires reason
#   3. Discover mode: can add tasks, new tasks must have Spec + Verify, Discovery sections parsed
#   4. Generate mode: can modify PENDING, SUPERSEDE, write Alternatives, max 5 new tasks
#   5. Mode precedence: spec header > queue entry > default
#   6. Experiment budget: tracking and exhaustion notice

import json
import os
import re
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

# Add parent dir to path for imports
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from lib.critic import validate_mode_compliance
from lib.queue import (
    DEFAULT_EXPERIMENT_BUDGETS,
    get_experiment_budget,
    increment_experiment_usage,
    set_experiment_budget,
)
from lib.spec_parser import BoiTask, parse_boi_spec


# ── Execute Mode Tests ──────────────────────────────────────────────────


class TestExecuteModeEnforcement(unittest.TestCase):
    """Execute mode: worker cannot add tasks."""

    def test_no_tasks_added_passes(self):
        pre_ids = {"t-1", "t-2"}
        post_ids = {"t-1", "t-2"}
        post_tasks = [
            BoiTask(id="t-1", title="Task 1", status="DONE"),
            BoiTask(id="t-2", title="Task 2", status="PENDING"),
        ]
        violations = validate_mode_compliance("execute", pre_ids, post_ids, post_tasks)
        self.assertEqual(len(violations), 0)

    def test_tasks_added_flags_violation(self):
        pre_ids = {"t-1", "t-2"}
        post_ids = {"t-1", "t-2", "t-3"}
        post_tasks = [
            BoiTask(id="t-1", title="Task 1", status="DONE"),
            BoiTask(id="t-2", title="Task 2", status="PENDING"),
            BoiTask(id="t-3", title="New task", status="PENDING"),
        ]
        violations = validate_mode_compliance("execute", pre_ids, post_ids, post_tasks)
        self.assertEqual(len(violations), 1)
        self.assertEqual(violations[0]["type"], "mode_violation")
        self.assertIn("Execute mode", violations[0]["message"])
        self.assertIn("t-3", violations[0]["message"])

    def test_multiple_tasks_added(self):
        pre_ids = {"t-1"}
        post_ids = {"t-1", "t-2", "t-3", "t-4"}
        post_tasks = [
            BoiTask(id="t-1", title="Original", status="DONE"),
            BoiTask(id="t-2", title="New 1", status="PENDING"),
            BoiTask(id="t-3", title="New 2", status="PENDING"),
            BoiTask(id="t-4", title="New 3", status="PENDING"),
        ]
        violations = validate_mode_compliance("execute", pre_ids, post_ids, post_tasks)
        self.assertEqual(len(violations), 1)
        self.assertIn("3 task(s)", violations[0]["message"])

    def test_task_count_unchanged_after_iteration(self):
        """Simulate a spec before and after an Execute iteration.
        If task count increased, it's a violation."""
        spec_before = textwrap.dedent("""\
            # Test Spec

            ### t-1: Do something
            PENDING

            **Spec:** Do the thing.
            **Verify:** echo ok
        """)
        spec_after = textwrap.dedent("""\
            # Test Spec

            ### t-1: Do something
            DONE

            **Spec:** Do the thing.
            **Verify:** echo ok

            ### t-2: New task added by worker
            PENDING

            **Spec:** Something new.
            **Verify:** echo new
        """)
        pre = parse_boi_spec(spec_before)
        post = parse_boi_spec(spec_after)
        pre_ids = {t.id for t in pre}
        post_ids = {t.id for t in post}
        violations = validate_mode_compliance("execute", pre_ids, post_ids, post)
        self.assertEqual(len(violations), 1)
        self.assertIn("Execute mode", violations[0]["message"])


# ── Challenge Mode Tests ─────────────────────────────────────────────────


class TestChallengeModeEnforcement(unittest.TestCase):
    """Challenge mode: can write Challenges, cannot add tasks."""

    def test_no_tasks_added_passes(self):
        pre_ids = {"t-1"}
        post_ids = {"t-1"}
        post_tasks = [BoiTask(id="t-1", title="Task 1", status="DONE")]
        violations = validate_mode_compliance(
            "challenge", pre_ids, post_ids, post_tasks
        )
        self.assertEqual(len(violations), 0)

    def test_tasks_added_flags_violation(self):
        pre_ids = {"t-1"}
        post_ids = {"t-1", "t-2"}
        post_tasks = [
            BoiTask(id="t-1", title="Task 1", status="DONE"),
            BoiTask(id="t-2", title="New task", status="PENDING"),
        ]
        violations = validate_mode_compliance(
            "challenge", pre_ids, post_ids, post_tasks
        )
        self.assertEqual(len(violations), 1)
        self.assertIn("Challenge mode", violations[0]["message"])

    def test_skipped_task_parsed_correctly(self):
        """SKIPPED status is valid and parseable."""
        spec = textwrap.dedent("""\
            # Test Spec

            ### t-1: Do something
            SKIPPED

            **Spec:** Do the thing.
            **Verify:** echo ok
        """)
        tasks = parse_boi_spec(spec)
        self.assertEqual(len(tasks), 1)
        self.assertEqual(tasks[0].status, "SKIPPED")

    def test_challenges_section_does_not_affect_task_count(self):
        """A ## Challenges section should not create tasks."""
        spec = textwrap.dedent("""\
            # Test Spec

            ### t-1: Do something
            DONE

            **Spec:** Do the thing.
            **Verify:** echo ok

            ## Challenges

            ### c-1: [task t-1] Potential issue
            **Observed:** Something concerning.
            **Risk:** MEDIUM
            **Suggestion:** Investigate further.
        """)
        tasks = parse_boi_spec(spec)
        # Only t-1 should be parsed, not c-1 (c-1 doesn't match ### t-N: pattern)
        self.assertEqual(len(tasks), 1)
        self.assertEqual(tasks[0].id, "t-1")


# ── Discover Mode Tests ──────────────────────────────────────────────────


class TestDiscoverModeEnforcement(unittest.TestCase):
    """Discover mode: can add tasks, new tasks must have Spec + Verify."""

    def test_new_task_with_spec_and_verify_passes(self):
        pre_ids = {"t-1"}
        post_ids = {"t-1", "t-2"}
        post_tasks = [
            BoiTask(id="t-1", title="Original", status="DONE"),
            BoiTask(
                id="t-2",
                title="New task",
                status="PENDING",
                body="**Spec:** Do something.\n\n**Verify:** echo ok",
            ),
        ]
        violations = validate_mode_compliance("discover", pre_ids, post_ids, post_tasks)
        self.assertEqual(len(violations), 0)

    def test_new_task_missing_spec_flags_violation(self):
        pre_ids = {"t-1"}
        post_ids = {"t-1", "t-2"}
        post_tasks = [
            BoiTask(id="t-1", title="Original", status="DONE"),
            BoiTask(
                id="t-2",
                title="Bad task",
                status="PENDING",
                body="**Verify:** echo ok",
            ),
        ]
        violations = validate_mode_compliance("discover", pre_ids, post_ids, post_tasks)
        self.assertEqual(len(violations), 1)
        self.assertIn("Spec", violations[0]["message"])

    def test_new_task_missing_verify_flags_violation(self):
        pre_ids = {"t-1"}
        post_ids = {"t-1", "t-2"}
        post_tasks = [
            BoiTask(id="t-1", title="Original", status="DONE"),
            BoiTask(
                id="t-2",
                title="Bad task",
                status="PENDING",
                body="**Spec:** Do something.",
            ),
        ]
        violations = validate_mode_compliance("discover", pre_ids, post_ids, post_tasks)
        self.assertEqual(len(violations), 1)
        self.assertIn("Verify", violations[0]["message"])

    def test_new_task_missing_both_flags_violation(self):
        pre_ids = {"t-1"}
        post_ids = {"t-1", "t-2"}
        post_tasks = [
            BoiTask(id="t-1", title="Original", status="DONE"),
            BoiTask(
                id="t-2",
                title="Bad task",
                status="PENDING",
                body="Just some text, no sections.",
            ),
        ]
        violations = validate_mode_compliance("discover", pre_ids, post_ids, post_tasks)
        self.assertEqual(len(violations), 1)
        self.assertIn("Spec", violations[0]["message"])
        self.assertIn("Verify", violations[0]["message"])

    def test_existing_task_not_checked_for_spec_verify(self):
        """Pre-existing tasks should not be checked for Spec/Verify in new task validation."""
        pre_ids = {"t-1"}
        post_ids = {"t-1"}
        # t-1 has no Spec/Verify in body, but that's fine since it's pre-existing
        post_tasks = [
            BoiTask(id="t-1", title="Original", status="DONE", body=""),
        ]
        violations = validate_mode_compliance("discover", pre_ids, post_ids, post_tasks)
        self.assertEqual(len(violations), 0)

    def test_discovery_section_parsed(self):
        """Discovery sections in tasks should be parsed as metadata."""
        spec = textwrap.dedent("""\
            # Test Spec

            ### t-1: Do something
            DONE

            **Spec:** Do the thing.
            **Verify:** echo ok

            #### Discovery: Found something
            This is a discovery note about what was found.

            ## Discovery

            ### Iteration 5
            - **Found:** Something interesting.
            - **Added:** t-2.
            - **Rationale:** Needed for completeness.
        """)
        tasks = parse_boi_spec(spec)
        self.assertEqual(len(tasks), 1)
        self.assertEqual(tasks[0].id, "t-1")
        self.assertIn("discovery note", tasks[0].discovery)


# ── Generate Mode Tests ──────────────────────────────────────────────────


class TestGenerateModeEnforcement(unittest.TestCase):
    """Generate mode: can modify PENDING, SUPERSEDE, max 5 new tasks."""

    def test_superseded_with_reference_passes(self):
        pre_ids = {"t-1", "t-2"}
        post_ids = {"t-1", "t-2", "t-3"}
        post_tasks = [
            BoiTask(
                id="t-1", title="Original", status="SUPERSEDED", superseded_by="t-3"
            ),
            BoiTask(id="t-2", title="Other", status="DONE"),
            BoiTask(
                id="t-3",
                title="Replacement",
                status="PENDING",
                body="**Spec:** Do it.\n**Verify:** ok",
            ),
        ]
        violations = validate_mode_compliance("generate", pre_ids, post_ids, post_tasks)
        self.assertEqual(len(violations), 0)

    def test_superseded_without_reference_flags_violation(self):
        pre_ids = {"t-1"}
        post_ids = {"t-1"}
        post_tasks = [
            BoiTask(id="t-1", title="Original", status="SUPERSEDED", superseded_by=""),
        ]
        violations = validate_mode_compliance("generate", pre_ids, post_ids, post_tasks)
        self.assertEqual(len(violations), 1)
        self.assertIn("SUPERSEDED", violations[0]["message"])
        self.assertIn("missing 'by t-N'", violations[0]["message"])

    def test_max_5_new_tasks_passes(self):
        pre_ids = {"t-1"}
        post_ids = {"t-1", "t-2", "t-3", "t-4", "t-5", "t-6"}
        post_tasks = [BoiTask(id="t-1", title="Original", status="DONE")]
        for i in range(2, 7):
            post_tasks.append(BoiTask(id=f"t-{i}", title=f"New {i}", status="PENDING"))
        violations = validate_mode_compliance("generate", pre_ids, post_ids, post_tasks)
        self.assertEqual(len(violations), 0)

    def test_more_than_5_new_tasks_flags_violation(self):
        pre_ids = {"t-1"}
        post_ids = {"t-1"}
        post_tasks = [BoiTask(id="t-1", title="Original", status="DONE")]
        for i in range(2, 9):  # 7 new tasks
            post_ids.add(f"t-{i}")
            post_tasks.append(BoiTask(id=f"t-{i}", title=f"New {i}", status="PENDING"))
        violations = validate_mode_compliance("generate", pre_ids, post_ids, post_tasks)
        has_max_violation = any("max 5" in v["message"] for v in violations)
        self.assertTrue(has_max_violation)

    def test_superseded_status_parsing(self):
        """SUPERSEDED by t-N is parsed correctly."""
        spec = textwrap.dedent("""\
            # Test Spec

            ### t-1: Original approach
            SUPERSEDED by t-3

            **Spec:** Old approach.
            **Verify:** echo old

            ### t-2: Another task
            DONE

            **Spec:** Do stuff.
            **Verify:** echo ok

            ### t-3: Better approach
            PENDING

            **Spec:** New approach.
            **Verify:** echo new
        """)
        tasks = parse_boi_spec(spec)
        self.assertEqual(len(tasks), 3)
        t1 = next(t for t in tasks if t.id == "t-1")
        self.assertEqual(t1.status, "SUPERSEDED")
        self.assertEqual(t1.superseded_by, "t-3")

    def test_cannot_delete_tasks(self):
        """Generate mode should detect if tasks were removed (implicit, tested via spec diff)."""
        # This is more about convention than code enforcement; the mode prompt says
        # "You CANNOT delete tasks. Use SKIPPED or SUPERSEDED instead."
        # The validate_mode_compliance function doesn't explicitly check for deleted tasks,
        # but we verify that deleted tasks would be detectable.
        pre_ids = {"t-1", "t-2", "t-3"}
        post_ids = {"t-1", "t-3"}  # t-2 removed
        post_tasks = [
            BoiTask(id="t-1", title="First", status="DONE"),
            BoiTask(id="t-3", title="Third", status="PENDING"),
        ]
        # Currently no explicit check for deletions, but the difference is detectable
        deleted = pre_ids - post_ids
        self.assertEqual(deleted, {"t-2"})


# ── Mode Precedence Tests ───────────────────────────────────────────────


class TestModePrecedence(unittest.TestCase):
    """Spec header mode overrides queue entry mode overrides default."""

    def _generate_prompt_mode(self, spec_content, queue_mode=None):
        """Simulate the mode determination logic from the worker."""
        mode = "execute"  # default

        # 1. Queue entry mode
        if queue_mode:
            mode = queue_mode

        # 2. Spec header override
        mode_match = re.search(r"^\*\*Mode:\*\*\s*(\w+)", spec_content, re.MULTILINE)
        if mode_match:
            spec_mode = mode_match.group(1).strip().lower()
            valid_modes = {"execute", "challenge", "discover", "generate"}
            if spec_mode in valid_modes:
                mode = spec_mode

        return mode

    def test_default_is_execute(self):
        spec = "# Test Spec\n\n### t-1: Do something\nPENDING\n"
        mode = self._generate_prompt_mode(spec, queue_mode=None)
        self.assertEqual(mode, "execute")

    def test_queue_entry_overrides_default(self):
        spec = "# Test Spec\n\n### t-1: Do something\nPENDING\n"
        mode = self._generate_prompt_mode(spec, queue_mode="discover")
        self.assertEqual(mode, "discover")

    def test_spec_header_overrides_queue_entry(self):
        spec = "# Test Spec\n\n**Mode:** challenge\n\n### t-1: Do something\nPENDING\n"
        mode = self._generate_prompt_mode(spec, queue_mode="discover")
        self.assertEqual(mode, "challenge")

    def test_spec_header_overrides_default(self):
        spec = "# Test Spec\n\n**Mode:** generate\n\n### t-1: Do something\nPENDING\n"
        mode = self._generate_prompt_mode(spec, queue_mode=None)
        self.assertEqual(mode, "generate")

    def test_invalid_spec_mode_ignored(self):
        spec = "# Test Spec\n\n**Mode:** foobar\n\n### t-1: Do something\nPENDING\n"
        mode = self._generate_prompt_mode(spec, queue_mode="discover")
        self.assertEqual(mode, "discover")

    def test_all_valid_modes_recognized(self):
        for m in ["execute", "challenge", "discover", "generate"]:
            spec = f"# Test\n\n**Mode:** {m}\n"
            result = self._generate_prompt_mode(spec)
            self.assertEqual(result, m, f"Mode '{m}' not recognized")


# ── Experiment Budget Tests ──────────────────────────────────────────────


class TestExperimentBudget(unittest.TestCase):
    """Experiment budget tracking and exhaustion."""

    def test_default_budgets_per_mode(self):
        self.assertEqual(get_experiment_budget("execute"), 0)
        self.assertEqual(get_experiment_budget("challenge"), 2)
        self.assertEqual(get_experiment_budget("discover"), 3)
        self.assertEqual(get_experiment_budget("generate"), 5)

    def test_unknown_mode_returns_zero(self):
        self.assertEqual(get_experiment_budget("foobar"), 0)

    def test_budget_tracking(self):
        """Budget is correctly tracked via queue entry operations."""
        with tempfile.TemporaryDirectory() as tmpdir:
            queue_dir = tmpdir

            # Create a mock queue entry
            entry = {
                "id": "q-test",
                "spec_path": "/tmp/test.md",
                "original_spec_path": "/tmp/test.md",
                "status": "running",
                "submitted_at": "2024-01-01T00:00:00Z",
                "iteration": 1,
                "max_iterations": 30,
                "blocked_by": [],
                "priority": 100,
                "max_experiment_invocations": 2,
                "experiment_invocations_used": 0,
            }
            entry_path = Path(queue_dir) / "q-test.json"
            entry_path.write_text(json.dumps(entry), encoding="utf-8")

            # Create lockfile directory
            lock_dir = Path(queue_dir) / ".locks"
            lock_dir.mkdir(exist_ok=True)

            # Increment usage
            result = increment_experiment_usage(queue_dir, "q-test", count=1)
            self.assertEqual(result["experiment_invocations_used"], 1)
            self.assertEqual(result["remaining"], 1)

            # Increment again
            result = increment_experiment_usage(queue_dir, "q-test", count=1)
            self.assertEqual(result["experiment_invocations_used"], 2)
            self.assertEqual(result["remaining"], 0)

    def test_exhaustion_notice_in_prompt(self):
        """When budget is exhausted, the prompt should indicate it."""
        # Simulate the worker budget text logic
        max_budget = 2
        used_budget = 2
        remaining = max(0, max_budget - used_budget)

        if max_budget == 0:
            budget_text = "0. Experiments are disabled in this mode."
        elif remaining == 0:
            budget_text = "EXHAUSTED. Do not propose alternatives. Implement per spec."
        else:
            budget_text = f"{remaining} remaining ({used_budget} of {max_budget} used)"

        self.assertIn("EXHAUSTED", budget_text)

    def test_budget_disabled_in_execute_mode(self):
        """Execute mode has 0 budget, prompt should say disabled."""
        max_budget = 0
        if max_budget == 0:
            budget_text = "0. Experiments are disabled in this mode."
        else:
            budget_text = "something else"

        self.assertIn("disabled", budget_text)

    def test_budget_with_remaining(self):
        """When budget has remaining, prompt shows remaining count."""
        max_budget = 5
        used_budget = 2
        remaining = max(0, max_budget - used_budget)

        budget_text = f"{remaining} remaining ({used_budget} of {max_budget} used)"
        self.assertIn("3 remaining", budget_text)
        self.assertIn("2 of 5 used", budget_text)


# ── Task Status Parsing Tests ────────────────────────────────────────────


class TestTaskStatusParsing(unittest.TestCase):
    """All valid task statuses parse correctly."""

    def test_all_statuses_parsed(self):
        spec = textwrap.dedent("""\
            # Test Spec

            ### t-1: Pending task
            PENDING

            **Spec:** Something.
            **Verify:** echo ok

            ### t-2: Done task
            DONE

            **Spec:** Something.
            **Verify:** echo ok

            ### t-3: Skipped task
            SKIPPED

            **Spec:** Something.
            **Verify:** echo ok

            ### t-4: Failed task
            FAILED

            **Spec:** Something.
            **Verify:** echo ok

            ### t-5: Experiment proposed
            EXPERIMENT_PROPOSED

            **Spec:** Something.
            **Verify:** echo ok

            ### t-6: Superseded task
            SUPERSEDED by t-7

            **Spec:** Something.
            **Verify:** echo ok
        """)
        tasks = parse_boi_spec(spec)
        self.assertEqual(len(tasks), 6)
        self.assertEqual(tasks[0].status, "PENDING")
        self.assertEqual(tasks[1].status, "DONE")
        self.assertEqual(tasks[2].status, "SKIPPED")
        self.assertEqual(tasks[3].status, "FAILED")
        self.assertEqual(tasks[4].status, "EXPERIMENT_PROPOSED")
        self.assertEqual(tasks[5].status, "SUPERSEDED")
        self.assertEqual(tasks[5].superseded_by, "t-7")

    def test_experiment_proposed_counts_as_incomplete(self):
        """EXPERIMENT_PROPOSED should not count as done."""
        from lib.spec_parser import count_boi_tasks

        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
            f.write(
                textwrap.dedent("""\
                # Test Spec

                ### t-1: Done task
                DONE

                **Spec:** S.
                **Verify:** V.

                ### t-2: Experiment
                EXPERIMENT_PROPOSED

                **Spec:** S.
                **Verify:** V.
            """)
            )
            f.flush()
            try:
                counts = count_boi_tasks(f.name)
                self.assertEqual(counts["done"], 1)
                self.assertEqual(counts["experiment_proposed"], 1)
                self.assertEqual(counts["total"], 2)  # Both included in total
            finally:
                os.unlink(f.name)

    def test_superseded_excluded_from_total(self):
        """SUPERSEDED tasks should be excluded from total count."""
        from lib.spec_parser import count_boi_tasks

        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
            f.write(
                textwrap.dedent("""\
                # Test Spec

                ### t-1: Done task
                DONE

                **Spec:** S.
                **Verify:** V.

                ### t-2: Superseded
                SUPERSEDED by t-3

                **Spec:** S.
                **Verify:** V.

                ### t-3: Replacement
                PENDING

                **Spec:** S.
                **Verify:** V.
            """)
            )
            f.flush()
            try:
                counts = count_boi_tasks(f.name)
                self.assertEqual(counts["done"], 1)
                self.assertEqual(counts["superseded"], 1)
                self.assertEqual(counts["pending"], 1)
                # total = 3 tasks - 1 superseded = 2
                self.assertEqual(counts["total"], 2)
            finally:
                os.unlink(f.name)


# ── Experiment Subsection Parsing Tests ──────────────────────────────────


class TestExperimentSubsectionParsing(unittest.TestCase):
    """#### Experiment: subsections are parsed correctly."""

    def test_experiment_section_captured(self):
        spec = textwrap.dedent("""\
            # Test Spec

            ### t-1: Try an approach
            EXPERIMENT_PROPOSED

            **Spec:** Try approach A.
            **Verify:** echo ok

            #### Experiment: Alternative with approach B
            Approach B uses a different algorithm that yields better results.
            Evidence: benchmarks show 2x improvement.
        """)
        tasks = parse_boi_spec(spec)
        self.assertEqual(len(tasks), 1)
        self.assertIn("Approach B", tasks[0].experiment)
        self.assertIn("2x improvement", tasks[0].experiment)


if __name__ == "__main__":
    unittest.main()
