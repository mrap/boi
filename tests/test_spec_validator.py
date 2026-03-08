# test_spec_validator.py — Unit tests for spec_validator.py
#
# Tests the BOI spec.md validation logic: task heading format,
# status lines, required sections (Spec, Verify), optional warnings,
# and integration with dispatch.

import os
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

# Add parent directory to path so we can import lib modules
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.spec_validator import validate_spec, validate_spec_file, ValidationResult


class TestValidateSpec_ValidSpecs(unittest.TestCase):
    """Test that well-formed specs pass validation."""

    def test_single_pending_task(self):
        spec = textwrap.dedent("""\
            # My Spec

            ## Tasks

            ### t-1: Build the thing
            PENDING

            **Spec:** Do the work.

            **Verify:** python3 -c "print('ok')"
        """)
        result = validate_spec(spec)
        self.assertTrue(result.valid, result.errors)
        self.assertEqual(result.total, 1)
        self.assertEqual(result.pending, 1)
        self.assertEqual(result.done, 0)
        self.assertEqual(result.skipped, 0)
        self.assertEqual(len(result.errors), 0)

    def test_multiple_tasks_all_statuses(self):
        spec = textwrap.dedent("""\
            # Spec

            ### t-1: First task
            DONE — completed successfully

            **Spec:** Did the work.

            **Verify:** echo ok

            ### t-2: Second task
            PENDING

            **Spec:** Do more work.

            **Verify:** echo ok

            ### t-3: Third task
            SKIPPED — not needed

            **Spec:** Was going to do this.

            **Verify:** echo ok
        """)
        result = validate_spec(spec)
        self.assertTrue(result.valid, result.errors)
        self.assertEqual(result.total, 3)
        self.assertEqual(result.pending, 1)
        self.assertEqual(result.done, 1)
        self.assertEqual(result.skipped, 1)

    def test_task_with_self_evolution_section(self):
        spec = textwrap.dedent("""\
            ### t-1: Build it
            PENDING

            **Spec:** Build the feature.

            **Verify:** run tests

            **Self-evolution:** If new requirements emerge, add tasks.
        """)
        result = validate_spec(spec)
        self.assertTrue(result.valid, result.errors)
        self.assertEqual(len(result.warnings), 0)

    def test_spec_with_code_blocks(self):
        """Code blocks inside task body should not confuse the parser."""
        spec = textwrap.dedent("""\
            ### t-1: Create config
            PENDING

            **Spec:** Create the following config:
            ```json
            {"key": "value"}
            ```

            **Verify:** cat config.json
        """)
        result = validate_spec(spec)
        self.assertTrue(result.valid, result.errors)

    def test_done_task_with_notes(self):
        """DONE with trailing notes is valid."""
        spec = textwrap.dedent("""\
            ### t-1: Build thing
            DONE — All 5 tests pass, verified working.

            **Spec:** Build it.

            **Verify:** run tests
        """)
        result = validate_spec(spec)
        self.assertTrue(result.valid, result.errors)
        self.assertEqual(result.done, 1)

    def test_task_ids_not_sequential(self):
        """Non-sequential IDs are valid (gaps allowed after skipping/adding)."""
        spec = textwrap.dedent("""\
            ### t-1: First
            DONE

            **Spec:** Done.
            **Verify:** ok

            ### t-5: Fifth (others removed)
            PENDING

            **Spec:** Do this.
            **Verify:** ok
        """)
        result = validate_spec(spec)
        self.assertTrue(result.valid, result.errors)
        self.assertEqual(result.total, 2)

    def test_verify_on_same_line_as_spec(self):
        """Spec and Verify can be on consecutive lines without blank."""
        spec = textwrap.dedent("""\
            ### t-1: Compact task
            PENDING

            **Spec:** Do the thing.
            **Verify:** check the thing
        """)
        result = validate_spec(spec)
        self.assertTrue(result.valid, result.errors)


class TestValidateSpec_MissingStatus(unittest.TestCase):
    """Test detection of missing or malformed status lines."""

    def test_no_status_line(self):
        spec = textwrap.dedent("""\
            ### t-1: Missing status

            **Spec:** Do the work.

            **Verify:** echo ok
        """)
        result = validate_spec(spec)
        self.assertFalse(result.valid)
        self.assertTrue(
            any("status" in e.lower() for e in result.errors), result.errors
        )

    def test_invalid_status_word(self):
        spec = textwrap.dedent("""\
            ### t-1: Bad status
            IN_PROGRESS

            **Spec:** Do the work.

            **Verify:** echo ok
        """)
        result = validate_spec(spec)
        self.assertFalse(result.valid)
        self.assertTrue(
            any("status" in e.lower() for e in result.errors), result.errors
        )

    def test_status_not_immediately_after_heading(self):
        """Status must be on the first non-blank line after heading."""
        spec = textwrap.dedent("""\
            ### t-1: Status too late
            Some random text
            PENDING

            **Spec:** Do the work.

            **Verify:** echo ok
        """)
        result = validate_spec(spec)
        self.assertFalse(result.valid)
        self.assertTrue(
            any("status" in e.lower() for e in result.errors), result.errors
        )


class TestValidateSpec_MissingSections(unittest.TestCase):
    """Test detection of missing Spec and Verify sections."""

    def test_missing_spec_section(self):
        spec = textwrap.dedent("""\
            ### t-1: No spec section
            PENDING

            **Verify:** echo ok
        """)
        result = validate_spec(spec)
        self.assertFalse(result.valid)
        self.assertTrue(any("Spec" in e for e in result.errors), result.errors)

    def test_missing_verify_section(self):
        spec = textwrap.dedent("""\
            ### t-1: No verify section
            PENDING

            **Spec:** Do the work.
        """)
        result = validate_spec(spec)
        self.assertFalse(result.valid)
        self.assertTrue(any("Verify" in e for e in result.errors), result.errors)

    def test_missing_both_sections(self):
        spec = textwrap.dedent("""\
            ### t-1: No sections at all
            PENDING

            Just some text.
        """)
        result = validate_spec(spec)
        self.assertFalse(result.valid)
        errors_str = " ".join(result.errors)
        self.assertIn("Spec", errors_str)
        self.assertIn("Verify", errors_str)


class TestValidateSpec_BadHeadingFormat(unittest.TestCase):
    """Test detection of malformed task headings."""

    def test_wrong_heading_level(self):
        """## instead of ### is for the old mesh format, not BOI."""
        spec = textwrap.dedent("""\
            ## t-1: Wrong heading level
            PENDING

            **Spec:** Do the work.
            **Verify:** echo ok
        """)
        result = validate_spec(spec)
        # Should find 0 tasks (## is not recognized as BOI task heading)
        self.assertEqual(result.total, 0)

    def test_missing_t_prefix(self):
        """Heading without t-N: prefix is not a task."""
        spec = textwrap.dedent("""\
            ### 1: No t prefix
            PENDING

            **Spec:** Do the work.
            **Verify:** echo ok
        """)
        result = validate_spec(spec)
        self.assertEqual(result.total, 0)


class TestValidateSpec_Warnings(unittest.TestCase):
    """Test optional warnings (non-blocking)."""

    def test_warns_missing_self_evolution(self):
        spec = textwrap.dedent("""\
            ### t-1: No self-evolution
            PENDING

            **Spec:** Do the work.

            **Verify:** echo ok
        """)
        result = validate_spec(spec)
        self.assertTrue(result.valid)  # Warnings don't block
        self.assertTrue(
            any("Self-evolution" in w for w in result.warnings), result.warnings
        )

    def test_no_warning_when_self_evolution_present(self):
        spec = textwrap.dedent("""\
            ### t-1: Has self-evolution
            PENDING

            **Spec:** Do the work.

            **Verify:** echo ok

            **Self-evolution:** Add tasks if needed.
        """)
        result = validate_spec(spec)
        self.assertTrue(result.valid)
        self.assertFalse(any("Self-evolution" in w for w in result.warnings))


class TestValidateSpec_EdgeCases(unittest.TestCase):
    """Test edge cases and boundary conditions."""

    def test_empty_string(self):
        result = validate_spec("")
        # No tasks found, but that's technically valid (empty spec)
        self.assertEqual(result.total, 0)
        # An empty spec with no tasks should be flagged
        self.assertFalse(result.valid)
        self.assertTrue(any("no tasks" in e.lower() for e in result.errors))

    def test_no_tasks_just_preamble(self):
        spec = textwrap.dedent("""\
            # My Great Spec

            ## Vision
            This is going to be awesome.

            ## Architecture
            We'll build it with Python.
        """)
        result = validate_spec(spec)
        self.assertFalse(result.valid)
        self.assertTrue(any("no tasks" in e.lower() for e in result.errors))

    def test_many_tasks(self):
        """15 tasks should all be found and validated."""
        lines = ["# Big Spec\n"]
        for i in range(1, 16):
            lines.append(f"### t-{i}: Task {i}")
            lines.append("PENDING")
            lines.append("")
            lines.append(f"**Spec:** Do task {i}.")
            lines.append(f"**Verify:** check task {i}")
            lines.append("")
        spec = "\n".join(lines)
        result = validate_spec(spec)
        self.assertTrue(result.valid, result.errors)
        self.assertEqual(result.total, 15)
        self.assertEqual(result.pending, 15)

    def test_duplicate_task_ids(self):
        """Duplicate task IDs should generate an error."""
        spec = textwrap.dedent("""\
            ### t-1: First
            PENDING

            **Spec:** Do it.
            **Verify:** ok

            ### t-1: Duplicate!
            PENDING

            **Spec:** Do it again.
            **Verify:** ok
        """)
        result = validate_spec(spec)
        self.assertFalse(result.valid)
        self.assertTrue(any("duplicate" in e.lower() for e in result.errors))

    def test_mixed_valid_and_invalid_tasks(self):
        """One bad task shouldn't hide that the good tasks were found."""
        spec = textwrap.dedent("""\
            ### t-1: Good task
            PENDING

            **Spec:** All good here.
            **Verify:** echo ok

            ### t-2: Bad task (no Verify)
            PENDING

            **Spec:** Missing verify.
        """)
        result = validate_spec(spec)
        self.assertFalse(result.valid)
        self.assertEqual(result.total, 2)
        # Error should mention t-2
        self.assertTrue(any("t-2" in e for e in result.errors))


class TestValidateSpec_SummaryString(unittest.TestCase):
    """Test the summary() method on ValidationResult."""

    def test_valid_summary(self):
        spec = textwrap.dedent("""\
            ### t-1: Task one
            PENDING

            **Spec:** Work.
            **Verify:** check

            ### t-2: Task two
            DONE

            **Spec:** Done.
            **Verify:** check
        """)
        result = validate_spec(spec)
        summary = result.summary()
        self.assertIn("Valid", summary)
        self.assertIn("2 tasks", summary)
        self.assertIn("1 PENDING", summary)
        self.assertIn("1 DONE", summary)

    def test_invalid_summary(self):
        spec = textwrap.dedent("""\
            ### t-1: Bad
            PENDING

            Just text, no sections.
        """)
        result = validate_spec(spec)
        summary = result.summary()
        self.assertIn("Invalid", summary)


class TestValidateSpecFile(unittest.TestCase):
    """Test file-based validation."""

    def test_valid_file(self):
        spec_content = textwrap.dedent("""\
            ### t-1: Task
            PENDING

            **Spec:** Do it.
            **Verify:** check
        """)
        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
            f.write(spec_content)
            f.flush()
            try:
                result = validate_spec_file(f.name)
                self.assertTrue(result.valid, result.errors)
            finally:
                os.unlink(f.name)

    def test_nonexistent_file(self):
        result = validate_spec_file("/nonexistent/spec.md")
        self.assertFalse(result.valid)
        self.assertTrue(
            any(
                "not found" in e.lower() or "does not exist" in e.lower()
                for e in result.errors
            )
        )

    def test_unreadable_content(self):
        """Binary/unreadable content should fail gracefully."""
        with tempfile.NamedTemporaryFile(mode="wb", suffix=".md", delete=False) as f:
            f.write(b"\x00\x01\x02\x03")
            f.flush()
            try:
                result = validate_spec_file(f.name)
                # Should either fail validation or find 0 tasks
                if result.total == 0:
                    self.assertFalse(result.valid)
            finally:
                os.unlink(f.name)


class TestValidateSpec_RealWorldSpec(unittest.TestCase):
    """Test against a realistic BOI spec similar to the actual boi-rebrand spec."""

    def test_realistic_spec_validates(self):
        spec = textwrap.dedent("""\
            # Project Alpha — Spec

            ## Vision
            Build something great.

            ## Tasks

            ### t-1: Scaffold project structure
            DONE — All files created.

            **Spec:** Create the directory structure with src/, tests/, docs/.

            **Verify:** ls src/ tests/ docs/

            **Self-evolution:** Add more directories if needed.

            ### t-2: Implement core logic
            PENDING

            **Spec:** Write the main algorithm in src/core.py. Must handle:
            - Input validation
            - Processing
            - Output formatting

            ```python
            def process(data):
                # implementation here
                pass
            ```

            **Verify:** python3 -m pytest tests/test_core.py -v

            **Self-evolution:** If edge cases found, add test tasks.

            ### t-3: Write tests
            PENDING

            **Spec:** Create comprehensive unit tests.

            **Verify:** python3 -m pytest tests/ -v --tb=short

            ### t-4: Documentation
            SKIPPED — Will do in follow-up.

            **Spec:** Write README and API docs.

            **Verify:** cat README.md
        """)
        result = validate_spec(spec)
        self.assertTrue(result.valid, result.errors)
        self.assertEqual(result.total, 4)
        self.assertEqual(result.pending, 2)
        self.assertEqual(result.done, 1)
        self.assertEqual(result.skipped, 1)


if __name__ == "__main__":
    unittest.main()


# Import Generate-spec validation functions
from lib.spec_validator import auto_validate, is_generate_spec, validate_generate_spec


class TestIsGenerateSpec(unittest.TestCase):
    """Test detection of Generate-mode specs."""

    def test_generate_spec_detected(self):
        spec = "# [Generate] Build a CLI tool\n\n## Goal\nDo stuff."
        self.assertTrue(is_generate_spec(spec))

    def test_standard_spec_not_detected(self):
        spec = "# My Standard Spec\n\n### t-1: Task\nPENDING\n"
        self.assertFalse(is_generate_spec(spec))

    def test_empty_string(self):
        self.assertFalse(is_generate_spec(""))

    def test_blank_lines_before_title(self):
        spec = "\n\n# [Generate] Something\n\n## Goal\nStuff."
        self.assertTrue(is_generate_spec(spec))

    def test_generate_in_body_not_title(self):
        spec = "# Normal Spec\n\n[Generate] is mentioned here but not in title."
        self.assertFalse(is_generate_spec(spec))


class TestValidateGenerateSpec_Valid(unittest.TestCase):
    """Test that well-formed Generate specs pass validation."""

    def _make_valid_spec(self):
        return textwrap.dedent("""\
            # [Generate] Build a config management CLI

            ## Goal
            Build a command-line tool for managing application configuration files.
            The tool should support reading, writing, and validating YAML and JSON
            configuration files with schema enforcement and environment variable
            interpolation for production deployments.

            ## Constraints
            - Python stdlib only, no pip dependencies
            - Must work on Linux and macOS
            - Config files must be human-readable

            ## Success Criteria
            - [ ] CLI accepts read, write, and validate subcommands
            - [ ] YAML and JSON formats are both supported
            - [ ] Schema validation rejects invalid configs with clear error messages
        """)

    def test_valid_generate_spec(self):
        result = validate_generate_spec(self._make_valid_spec())
        self.assertTrue(result.valid, result.errors)
        self.assertEqual(len(result.errors), 0)

    def test_valid_with_optional_sections(self):
        spec = self._make_valid_spec() + textwrap.dedent("""\

            ## Anti-Goals
            - Do not build a GUI
            - Do not support XML

            ## Seed Ideas
            - Use argparse for CLI parsing
            - Consider click library if we relax the no-pip constraint
        """)
        result = validate_generate_spec(spec)
        self.assertTrue(result.valid, result.errors)

    def test_valid_with_many_criteria(self):
        spec = textwrap.dedent("""\
            # [Generate] Multi-criteria spec

            ## Goal
            Build a comprehensive testing framework that supports unit tests,
            integration tests, and end-to-end tests with parallel execution,
            retry logic, and detailed reporting capabilities.

            ## Constraints
            - Must be fast

            ## Success Criteria
            - [ ] Unit test runner works
            - [ ] Integration test support
            - [ ] E2E test support
            - [ ] Parallel execution
            - [ ] Retry logic for flaky tests
            - [ ] HTML report generation
        """)
        result = validate_generate_spec(spec)
        self.assertTrue(result.valid, result.errors)


class TestValidateGenerateSpec_MissingTitle(unittest.TestCase):
    """Test Generate spec title validation."""

    def test_wrong_title_format(self):
        spec = textwrap.dedent("""\
            # Build a CLI tool

            ## Goal
            Build a command-line tool for managing application configuration files.
            The tool should support reading, writing, and validating YAML and JSON
            configuration files with schema enforcement.

            ## Constraints
            - Python only

            ## Success Criteria
            - [ ] CLI works
            - [ ] Tests pass
        """)
        result = validate_generate_spec(spec)
        self.assertFalse(result.valid)
        self.assertTrue(any("[Generate]" in e for e in result.errors))

    def test_generate_lowercase(self):
        spec = textwrap.dedent("""\
            # [generate] lowercase

            ## Goal
            Build a command-line tool for managing application configuration files.
            The tool should support reading, writing, and validating YAML and JSON
            configuration files with schema enforcement.

            ## Constraints
            - Python only

            ## Success Criteria
            - [ ] CLI works
            - [ ] Tests pass
        """)
        result = validate_generate_spec(spec)
        self.assertFalse(result.valid)
        self.assertTrue(any("[Generate]" in e for e in result.errors))


class TestValidateGenerateSpec_MissingGoal(unittest.TestCase):
    """Test missing or short Goal section."""

    def test_missing_goal(self):
        spec = textwrap.dedent("""\
            # [Generate] No goal spec

            ## Constraints
            - Be fast

            ## Success Criteria
            - [ ] It works
            - [ ] Tests pass
        """)
        result = validate_generate_spec(spec)
        self.assertFalse(result.valid)
        self.assertTrue(any("Goal" in e for e in result.errors))

    def test_goal_too_short(self):
        spec = textwrap.dedent("""\
            # [Generate] Short goal

            ## Goal
            Build a CLI tool.

            ## Constraints
            - Be fast

            ## Success Criteria
            - [ ] It works
            - [ ] Tests pass
        """)
        result = validate_generate_spec(spec)
        self.assertFalse(result.valid)
        self.assertTrue(any("too short" in e.lower() for e in result.errors))

    def test_goal_exactly_20_words(self):
        spec = textwrap.dedent("""\
            # [Generate] Borderline goal

            ## Goal
            one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty

            ## Constraints
            - Be fast

            ## Success Criteria
            - [ ] It works
            - [ ] Tests pass
        """)
        result = validate_generate_spec(spec)
        self.assertTrue(result.valid, result.errors)


class TestValidateGenerateSpec_MissingConstraints(unittest.TestCase):
    """Test missing Constraints section."""

    def test_missing_constraints(self):
        spec = textwrap.dedent("""\
            # [Generate] No constraints

            ## Goal
            Build a command-line tool for managing application configuration files.
            The tool should support reading, writing, and validating YAML and JSON
            configuration files with schema enforcement.

            ## Success Criteria
            - [ ] It works
            - [ ] Tests pass
        """)
        result = validate_generate_spec(spec)
        self.assertFalse(result.valid)
        self.assertTrue(any("Constraints" in e for e in result.errors))


class TestValidateGenerateSpec_MissingCriteria(unittest.TestCase):
    """Test missing or insufficient Success Criteria."""

    def test_missing_criteria_section(self):
        spec = textwrap.dedent("""\
            # [Generate] No criteria

            ## Goal
            Build a command-line tool for managing application configuration files.
            The tool should support reading, writing, and validating YAML and JSON
            configuration files with schema enforcement.

            ## Constraints
            - Be fast
        """)
        result = validate_generate_spec(spec)
        self.assertFalse(result.valid)
        self.assertTrue(any("Success Criteria" in e for e in result.errors))

    def test_too_few_checkboxes(self):
        spec = textwrap.dedent("""\
            # [Generate] One checkbox

            ## Goal
            Build a command-line tool for managing application configuration files.
            The tool should support reading, writing, and validating YAML and JSON
            configuration files with schema enforcement.

            ## Constraints
            - Be fast

            ## Success Criteria
            - [ ] Only one criterion
        """)
        result = validate_generate_spec(spec)
        self.assertFalse(result.valid)
        self.assertTrue(any("at least 2" in e for e in result.errors))

    def test_zero_checkboxes(self):
        spec = textwrap.dedent("""\
            # [Generate] No checkboxes

            ## Goal
            Build a command-line tool for managing application configuration files.
            The tool should support reading, writing, and validating YAML and JSON
            configuration files with schema enforcement.

            ## Constraints
            - Be fast

            ## Success Criteria
            Some text but no checkbox items at all.
        """)
        result = validate_generate_spec(spec)
        self.assertFalse(result.valid)
        self.assertTrue(any("at least 2" in e for e in result.errors))

    def test_checked_boxes_count(self):
        """Already-checked boxes should still count as valid criteria."""
        spec = textwrap.dedent("""\
            # [Generate] Checked boxes

            ## Goal
            Build a command-line tool for managing application configuration files.
            The tool should support reading, writing, and validating YAML and JSON
            configuration files with schema enforcement.

            ## Constraints
            - Be fast

            ## Success Criteria
            - [x] Already done criterion
            - [ ] Not yet done criterion
        """)
        result = validate_generate_spec(spec)
        self.assertTrue(result.valid, result.errors)


class TestValidateGenerateSpec_ContainsTasks(unittest.TestCase):
    """Test rejection when Generate spec contains task headings."""

    def test_rejects_tasks(self):
        spec = textwrap.dedent("""\
            # [Generate] Has tasks

            ## Goal
            Build a command-line tool for managing application configuration files.
            The tool should support reading, writing, and validating YAML and JSON
            configuration files with schema enforcement.

            ## Constraints
            - Be fast

            ## Success Criteria
            - [ ] It works
            - [ ] Tests pass

            ### t-1: Should not be here
            PENDING

            **Spec:** This task should not exist in a Generate spec.
            **Verify:** echo ok
        """)
        result = validate_generate_spec(spec)
        self.assertFalse(result.valid)
        self.assertTrue(any("task headings" in e.lower() for e in result.errors))


class TestValidateGenerateSpec_MultipleErrors(unittest.TestCase):
    """Test that multiple errors are all reported."""

    def test_all_errors_at_once(self):
        spec = textwrap.dedent("""\
            # Wrong Title

            ## Success Criteria
            - [ ] Only one
        """)
        result = validate_generate_spec(spec)
        self.assertFalse(result.valid)
        error_text = " ".join(result.errors)
        self.assertIn("[Generate]", error_text)
        self.assertIn("Goal", error_text)
        self.assertIn("Constraints", error_text)
        self.assertIn("at least 2", error_text)


class TestAutoValidate(unittest.TestCase):
    """Test auto_validate dispatches to the correct validator."""

    def test_auto_validate_standard_spec(self):
        spec = textwrap.dedent("""\
            # Standard Spec

            ### t-1: Do something
            PENDING

            **Spec:** Work.
            **Verify:** echo ok
        """)
        result = auto_validate(spec)
        self.assertTrue(result.valid, result.errors)
        self.assertEqual(result.total, 1)

    def test_auto_validate_generate_spec(self):
        spec = textwrap.dedent("""\
            # [Generate] Build something

            ## Goal
            Build a command-line tool for managing application configuration files.
            The tool should support reading, writing, and validating YAML and JSON
            configuration files with schema enforcement.

            ## Constraints
            - Python only

            ## Success Criteria
            - [ ] CLI works
            - [ ] Tests pass
        """)
        result = auto_validate(spec)
        self.assertTrue(result.valid, result.errors)

    def test_auto_validate_invalid_generate_spec(self):
        spec = textwrap.dedent("""\
            # [Generate] Missing stuff
        """)
        result = auto_validate(spec)
        self.assertFalse(result.valid)
        self.assertTrue(any("Goal" in e for e in result.errors))
