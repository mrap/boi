# test_generate.py — Tests for Generate-mode spec validation, decomposition, and convergence.
#
# Tests:
#   1. Generate spec validation (valid spec, missing Goal, contains tasks, etc.)
#   2. Decomposition output validation (task count bounds, criteria coverage)
#   3. Convergence algorithm (all criteria met, stalled, good enough, max iterations)

import json
import os
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

# Add parent dir to path for imports
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from lib.evaluate import (
    check_convergence,
    count_criteria_met,
    evaluate_criteria,
    parse_success_criteria,
)
from lib.spec_validator import (
    auto_validate,
    is_generate_spec,
    validate_generate_spec,
    validate_spec,
)


# ── Generate Spec Validation Tests ──────────────────────────────────────


class TestIsGenerateSpec(unittest.TestCase):
    """Detection of Generate-mode specs."""

    def test_generate_title_detected(self):
        content = "# [Generate] Build a CLI Tool\n\n## Goal\nBuild something.\n"
        self.assertTrue(is_generate_spec(content))

    def test_regular_spec_not_detected(self):
        content = "# My Regular Spec\n\n### t-1: Do something\nPENDING\n"
        self.assertFalse(is_generate_spec(content))

    def test_empty_content(self):
        self.assertFalse(is_generate_spec(""))

    def test_only_whitespace(self):
        self.assertFalse(is_generate_spec("   \n\n  \n"))


class TestValidateGenerateSpec(unittest.TestCase):
    """Validation of Generate-mode spec structure."""

    def _make_valid_spec(self):
        return textwrap.dedent("""\
            # [Generate] Build a Config Manager CLI

            ## Goal
            Create a command-line tool for managing application configuration files.
            The tool should support reading, writing, and validating YAML and JSON
            configuration files with schema validation and environment variable
            substitution for deployment flexibility.

            ## Constraints
            - Python 3.10+ only
            - No external dependencies beyond stdlib
            - Must handle files up to 10MB

            ## Success Criteria
            - [ ] Can read and parse YAML config files
            - [ ] Can write config files with proper formatting
            - [ ] Schema validation catches missing required fields
            - [ ] Environment variable substitution works in all value types
        """)

    def test_valid_spec_passes(self):
        result = validate_generate_spec(self._make_valid_spec())
        self.assertTrue(result.valid)
        self.assertEqual(len(result.errors), 0)

    def test_missing_generate_title(self):
        content = textwrap.dedent("""\
            # Build Something

            ## Goal
            Create a command-line tool for managing application configuration files.
            The tool should support reading, writing, and validating YAML and JSON
            configuration files with schema validation and environment variable
            substitution for deployment flexibility.

            ## Constraints
            - No deps

            ## Success Criteria
            - [ ] Works
            - [ ] Tests pass
        """)
        result = validate_generate_spec(content)
        self.assertFalse(result.valid)
        self.assertTrue(any("# [Generate]" in e for e in result.errors))

    def test_missing_goal_section(self):
        content = textwrap.dedent("""\
            # [Generate] Build Something

            ## Constraints
            - No deps

            ## Success Criteria
            - [ ] Works
            - [ ] Tests pass
        """)
        result = validate_generate_spec(content)
        self.assertFalse(result.valid)
        self.assertTrue(any("Goal" in e for e in result.errors))

    def test_goal_too_short(self):
        content = textwrap.dedent("""\
            # [Generate] Build Something

            ## Goal
            Build a CLI tool.

            ## Constraints
            - No deps

            ## Success Criteria
            - [ ] Works
            - [ ] Tests pass
        """)
        result = validate_generate_spec(content)
        self.assertFalse(result.valid)
        self.assertTrue(any("too short" in e for e in result.errors))

    def test_missing_constraints(self):
        content = textwrap.dedent("""\
            # [Generate] Build Something

            ## Goal
            Create a command-line tool for managing application configuration files.
            The tool should support reading, writing, and validating YAML and JSON
            configuration files with schema validation and environment variable
            substitution for deployment flexibility.

            ## Success Criteria
            - [ ] Works
            - [ ] Tests pass
        """)
        result = validate_generate_spec(content)
        self.assertFalse(result.valid)
        self.assertTrue(any("Constraints" in e for e in result.errors))

    def test_missing_success_criteria(self):
        content = textwrap.dedent("""\
            # [Generate] Build Something

            ## Goal
            Create a command-line tool for managing application configuration files.
            The tool should support reading, writing, and validating YAML and JSON
            configuration files with schema validation and environment variable
            substitution for deployment flexibility.

            ## Constraints
            - No deps
        """)
        result = validate_generate_spec(content)
        self.assertFalse(result.valid)
        self.assertTrue(any("Success Criteria" in e for e in result.errors))

    def test_insufficient_criteria_checkboxes(self):
        content = textwrap.dedent("""\
            # [Generate] Build Something

            ## Goal
            Create a command-line tool for managing application configuration files.
            The tool should support reading, writing, and validating YAML and JSON
            configuration files with schema validation and environment variable
            substitution for deployment flexibility.

            ## Constraints
            - No deps

            ## Success Criteria
            - [ ] Only one criterion
        """)
        result = validate_generate_spec(content)
        self.assertFalse(result.valid)
        self.assertTrue(any("at least 2" in e for e in result.errors))

    def test_contains_task_headings_rejected(self):
        content = textwrap.dedent("""\
            # [Generate] Build Something

            ## Goal
            Create a command-line tool for managing application configuration files.
            The tool should support reading, writing, and validating YAML and JSON
            configuration files with schema validation and environment variable
            substitution for deployment flexibility.

            ## Constraints
            - No deps

            ## Success Criteria
            - [ ] Works
            - [ ] Tests pass

            ### t-1: First task
            PENDING

            **Spec:** Do something.
            **Verify:** echo ok
        """)
        result = validate_generate_spec(content)
        self.assertFalse(result.valid)
        self.assertTrue(any("task headings" in e.lower() for e in result.errors))

    def test_multiple_errors_reported(self):
        content = textwrap.dedent("""\
            # Wrong Title

            ## Success Criteria
            - [ ] Only one
        """)
        result = validate_generate_spec(content)
        self.assertFalse(result.valid)
        # Should have errors for: title, Goal, Constraints, criteria count
        self.assertGreaterEqual(len(result.errors), 3)

    def test_optional_sections_accepted(self):
        """Anti-Goals and Seed Ideas sections should not cause errors."""
        content = textwrap.dedent("""\
            # [Generate] Build Something

            ## Goal
            Create a command-line tool for managing application configuration files.
            The tool should support reading, writing, and validating YAML and JSON
            configuration files with schema validation and environment variable
            substitution for deployment flexibility.

            ## Anti-Goals
            - Not building a GUI
            - Not supporting Windows

            ## Constraints
            - No deps

            ## Seed Ideas
            - Use argparse for CLI
            - YAML via PyYAML

            ## Success Criteria
            - [ ] Works
            - [ ] Tests pass
        """)
        result = validate_generate_spec(content)
        self.assertTrue(result.valid)


class TestAutoValidateDispatch(unittest.TestCase):
    """auto_validate dispatches to correct validator based on spec type."""

    def test_dispatches_to_generate_validator(self):
        content = textwrap.dedent("""\
            # [Generate] Build Something

            ## Goal
            Create a command-line tool for managing application configuration files.
            The tool should support reading, writing, and validating YAML and JSON
            configuration files with schema validation and environment variable
            substitution for deployment flexibility.

            ## Constraints
            - No deps

            ## Success Criteria
            - [ ] Works
            - [ ] Tests pass
        """)
        result = auto_validate(content)
        self.assertTrue(result.valid)

    def test_dispatches_to_standard_validator(self):
        content = textwrap.dedent("""\
            # Regular Spec

            ### t-1: Do something
            PENDING

            **Spec:** Do the thing.
            **Verify:** echo ok
        """)
        result = auto_validate(content)
        self.assertTrue(result.valid)

    def test_generate_spec_with_tasks_fails_generate_validation(self):
        """A Generate spec with tasks should fail Generate validation, not standard."""
        content = textwrap.dedent("""\
            # [Generate] Build Something

            ## Goal
            Create a command-line tool for managing application configuration files.
            The tool should support reading, writing, and validating YAML and JSON
            configuration files with schema validation and environment variable
            substitution for deployment flexibility.

            ## Constraints
            - No deps

            ## Success Criteria
            - [ ] Works
            - [ ] Tests pass

            ### t-1: A task
            PENDING

            **Spec:** S.
            **Verify:** V.
        """)
        result = auto_validate(content)
        self.assertFalse(result.valid)
        self.assertTrue(any("task headings" in e.lower() for e in result.errors))


# ── Decomposition Output Validation Tests ────────────────────────────────


class TestDecompositionOutputValidation(unittest.TestCase):
    """Validation of decomposed Generate spec output."""

    def test_valid_decomposed_spec(self):
        """A spec with tasks in valid format passes standard validation."""
        content = textwrap.dedent("""\
            # [Generate] Build a Config Manager CLI

            ## Goal
            Create a CLI tool. This is a long enough goal description to pass
            the twenty word minimum requirement for the Generate spec validator
            with enough detail to be meaningful.

            ## Approach
            We will build the CLI in three phases: scaffolding, core logic, and testing.
            <!-- Criterion mapping: YAML reading -> t-1, Writing -> t-2, Validation -> t-3 -->

            ## Constraints
            - Python only

            ## Success Criteria
            - [ ] Can read YAML
            - [ ] Can write config
            - [ ] Has tests

            ### t-1: Scaffold project structure
            PENDING

            **Spec:** Create directory structure.
            **Verify:** ls src/ tests/

            ### t-2: Implement config reader
            PENDING

            **Spec:** Read YAML files.
            **Verify:** python3 -c "import config; config.read('test.yaml')"

            ### t-3: Add unit tests
            PENDING

            **Spec:** Write tests.
            **Verify:** python3 -m pytest tests/ -v
        """)
        # Standard validator should accept the tasks
        result = validate_spec(content)
        self.assertTrue(result.valid)
        self.assertEqual(result.total, 3)

    def test_task_count_within_bounds(self):
        """Decomposed tasks should be within 3-30 range."""
        # Build a spec with exactly 3 tasks (lower bound)
        tasks = []
        for i in range(1, 4):
            tasks.append(f"""### t-{i}: Task {i}
PENDING

**Spec:** Do task {i}.
**Verify:** echo ok
""")
        content = "# Test\n\n" + "\n".join(tasks)
        result = validate_spec(content)
        self.assertTrue(result.valid)
        self.assertEqual(result.total, 3)

    def test_decomposition_too_few_tasks_detected(self):
        """If decomposition produces < 3 tasks, we can detect it via count."""
        content = textwrap.dedent("""\
            # Test

            ### t-1: Only task
            PENDING

            **Spec:** Do it.
            **Verify:** echo ok
        """)
        result = validate_spec(content)
        # Validator accepts any non-zero task count, but the daemon checks bounds
        self.assertTrue(result.valid)
        self.assertEqual(result.total, 1)
        # Daemon-level check: 1 < 3 = too few
        self.assertLess(result.total, 3)

    def test_tasks_without_spec_section_fails(self):
        """All decomposed tasks must have Spec sections."""
        content = textwrap.dedent("""\
            # Test

            ### t-1: Missing spec
            PENDING

            **Verify:** echo ok

            ### t-2: Has spec
            PENDING

            **Spec:** Do the thing.
            **Verify:** echo ok
        """)
        result = validate_spec(content)
        self.assertFalse(result.valid)
        self.assertTrue(any("Spec" in e for e in result.errors))

    def test_tasks_without_verify_section_fails(self):
        """All decomposed tasks must have Verify sections."""
        content = textwrap.dedent("""\
            # Test

            ### t-1: Missing verify
            PENDING

            **Spec:** Do something.

            ### t-2: Has verify
            PENDING

            **Spec:** Do the thing.
            **Verify:** echo ok
        """)
        result = validate_spec(content)
        self.assertFalse(result.valid)
        self.assertTrue(any("Verify" in e for e in result.errors))


# ── Convergence Algorithm Tests ─────────────────────────────────────────


class TestConvergenceAlgorithm(unittest.TestCase):
    """Convergence detection for Generate specs."""

    def _make_spec_file(self, criteria_lines):
        """Create a temp spec file with given criteria checkbox lines."""
        content = textwrap.dedent("""\
            # [Generate] Test

            ## Goal
            Build something useful.

            ## Constraints
            - None

            ## Success Criteria
        """)
        content += "\n".join(criteria_lines) + "\n"

        f = tempfile.NamedTemporaryFile(
            mode="w", suffix=".md", delete=False, encoding="utf-8"
        )
        f.write(content)
        f.flush()
        f.close()
        return f.name

    def test_all_criteria_met_converges(self):
        spec_path = self._make_spec_file(
            [
                "- [x] Feature A works",
                "- [x] Feature B works",
                "- [x] Feature C works",
            ]
        )
        try:
            entry = {"iteration": 5, "max_iterations": 50}
            result = check_convergence(entry, spec_path)
            self.assertTrue(result.should_stop)
            self.assertEqual(result.reason, "goal_achieved")
            self.assertEqual(result.criteria_met, 3)
            self.assertEqual(result.criteria_total, 3)
        finally:
            os.unlink(spec_path)

    def test_max_iterations_converges(self):
        spec_path = self._make_spec_file(
            [
                "- [x] Feature A works",
                "- [ ] Feature B works",
            ]
        )
        try:
            entry = {"iteration": 50, "max_iterations": 50}
            result = check_convergence(entry, spec_path)
            self.assertTrue(result.should_stop)
            self.assertEqual(result.reason, "max_iterations")
        finally:
            os.unlink(spec_path)

    def test_stalled_converges(self):
        """No progress for 5 consecutive iterations."""
        spec_path = self._make_spec_file(
            [
                "- [x] Feature A works",
                "- [ ] Feature B works",
                "- [ ] Feature C works",
            ]
        )
        try:
            entry = {"iteration": 10, "max_iterations": 50}
            # 5 iterations with same criteria_met count
            history = [1, 1, 1, 1, 1]
            result = check_convergence(entry, spec_path, criteria_history=history)
            self.assertTrue(result.should_stop)
            self.assertEqual(result.reason, "stalled")
        finally:
            os.unlink(spec_path)

    def test_not_stalled_with_progress(self):
        """Progress within last 5 iterations prevents stall detection."""
        spec_path = self._make_spec_file(
            [
                "- [x] Feature A works",
                "- [x] Feature B works",
                "- [ ] Feature C works",
            ]
        )
        try:
            entry = {"iteration": 10, "max_iterations": 50}
            history = [1, 1, 1, 2, 2]
            result = check_convergence(entry, spec_path, criteria_history=history)
            self.assertFalse(result.should_stop)
        finally:
            os.unlink(spec_path)

    def test_good_enough_converges(self):
        """Diminishing returns with >80% criteria met."""
        # 4/5 = 80% met
        spec_path = self._make_spec_file(
            [
                "- [x] A",
                "- [x] B",
                "- [x] C",
                "- [x] D",
                "- [ ] E",
            ]
        )
        try:
            entry = {"iteration": 15, "max_iterations": 50}
            # Last 3 iterations: improvement < 1 each
            history = [3, 4, 4, 4]
            result = check_convergence(entry, spec_path, criteria_history=history)
            self.assertTrue(result.should_stop)
            self.assertEqual(result.reason, "good_enough")
        finally:
            os.unlink(spec_path)

    def test_not_good_enough_below_threshold(self):
        """Diminishing returns but only 50% criteria met: don't stop."""
        spec_path = self._make_spec_file(
            [
                "- [x] A",
                "- [ ] B",
                "- [ ] C",
                "- [ ] D",
            ]
        )
        try:
            entry = {"iteration": 15, "max_iterations": 50}
            history = [1, 1, 1, 1]
            result = check_convergence(entry, spec_path, criteria_history=history)
            # Should be stalled (same value 5 times would be needed, only 4 here)
            # but not good_enough (only 25% met, well below 80%)
            if result.should_stop:
                self.assertNotEqual(result.reason, "good_enough")
        finally:
            os.unlink(spec_path)

    def test_no_criteria_converges_immediately(self):
        """Spec with no criteria section is treated as goal_achieved."""
        content = "# [Generate] Test\n\n## Goal\nBuild something.\n"
        f = tempfile.NamedTemporaryFile(
            mode="w", suffix=".md", delete=False, encoding="utf-8"
        )
        f.write(content)
        f.flush()
        f.close()
        try:
            entry = {"iteration": 1, "max_iterations": 50}
            result = check_convergence(entry, f.name)
            self.assertTrue(result.should_stop)
            self.assertEqual(result.reason, "goal_achieved")
        finally:
            os.unlink(f.name)

    def test_partial_criteria_continues(self):
        """Not all criteria met and within limits: should continue."""
        spec_path = self._make_spec_file(
            [
                "- [x] A",
                "- [ ] B",
                "- [ ] C",
            ]
        )
        try:
            entry = {"iteration": 3, "max_iterations": 50}
            result = check_convergence(entry, spec_path)
            self.assertFalse(result.should_stop)
        finally:
            os.unlink(spec_path)


# ── Criteria Parsing Tests ──────────────────────────────────────────────


class TestCriteriaParsing(unittest.TestCase):
    """Parse Success Criteria checkboxes."""

    def test_count_criteria_met(self):
        content = textwrap.dedent("""\
            ## Success Criteria
            - [x] Feature A
            - [ ] Feature B
            - [x] Feature C
        """)
        met, total = count_criteria_met(content)
        self.assertEqual(met, 2)
        self.assertEqual(total, 3)

    def test_evaluate_criteria_result(self):
        content = textwrap.dedent("""\
            ## Success Criteria
            - [x] A
            - [ ] B
            - [x] C
            - [ ] D
        """)
        with tempfile.NamedTemporaryFile(
            mode="w", suffix=".md", delete=False, encoding="utf-8"
        ) as f:
            f.write(content)
            f.flush()
            try:
                result = evaluate_criteria(f.name)
                self.assertEqual(result.criteria_total, 4)
                self.assertEqual(result.criteria_met, 2)
                self.assertEqual(result.criteria_unmet, 2)
                self.assertFalse(result.all_met)
                self.assertEqual(result.status, "needs_work")
                self.assertEqual(len(result.unmet_criteria), 2)
                self.assertIn("B", result.unmet_criteria)
                self.assertIn("D", result.unmet_criteria)
            finally:
                os.unlink(f.name)

    def test_all_criteria_met_status(self):
        content = "## Success Criteria\n- [x] A\n- [x] B\n"
        with tempfile.NamedTemporaryFile(
            mode="w", suffix=".md", delete=False, encoding="utf-8"
        ) as f:
            f.write(content)
            f.flush()
            try:
                result = evaluate_criteria(f.name)
                self.assertTrue(result.all_met)
                self.assertEqual(result.status, "goal_achieved")
            finally:
                os.unlink(f.name)

    def test_checkbox_variants(self):
        """Both [x] and [X] should count as checked."""
        content = "## Success Criteria\n- [x] Lower\n- [X] Upper\n- [ ] Unchecked\n"
        criteria = parse_success_criteria(content)
        self.assertEqual(len(criteria), 3)
        self.assertTrue(criteria[0]["checked"])
        self.assertTrue(criteria[1]["checked"])
        self.assertFalse(criteria[2]["checked"])


if __name__ == "__main__":
    unittest.main()
