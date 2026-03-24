"""test_dependency_validation.py — Tests for dependency graph validation in BOI specs.

Tests:
- blocked_by parsing in spec_parser.py (BoiTask.blocked_by field)
- validate_dependencies() in spec_validator.py (cycle detection, unmet deps, orphans)
- check_task_sizing() in spec_validator.py (task-sizing heuristics)

All tests use mock data. No live API calls.
Uses unittest (stdlib only, no pytest dependency).
"""

import sys
import textwrap
import unittest
from pathlib import Path

BOI_ROOT = str(Path(__file__).resolve().parent.parent)
sys.path.insert(0, BOI_ROOT)

from lib.spec_parser import parse_boi_spec, BoiTask
from lib.spec_validator import validate_spec, validate_dependencies, check_task_sizing


# ── blocked_by Parsing Tests ─────────────────────────────────────────────────


class TestBlockedByParsing(unittest.TestCase):
    """Tests for parsing **Blocked by:** lines into BoiTask.blocked_by field."""

    def test_no_blocked_by(self):
        """Task without Blocked by line should have empty blocked_by list."""
        content = textwrap.dedent("""\
            ### t-1: Independent task
            PENDING

            **Spec:** Do something.
            **Verify:** echo ok
        """)
        tasks = parse_boi_spec(content)
        self.assertEqual(len(tasks), 1)
        self.assertEqual(tasks[0].blocked_by, [])

    def test_single_blocked_by(self):
        """Task blocked by a single dependency."""
        content = textwrap.dedent("""\
            ### t-1: First task
            PENDING

            **Spec:** Do first thing.
            **Verify:** echo ok

            ### t-2: Depends on t-1
            PENDING
            **Blocked by:** t-1

            **Spec:** Do second thing.
            **Verify:** echo ok
        """)
        tasks = parse_boi_spec(content)
        self.assertEqual(len(tasks), 2)
        self.assertEqual(tasks[0].blocked_by, [])
        self.assertEqual(tasks[1].blocked_by, ["t-1"])

    def test_multiple_blocked_by(self):
        """Task blocked by multiple dependencies."""
        content = textwrap.dedent("""\
            ### t-1: Research A
            PENDING

            **Spec:** Research A.
            **Verify:** echo ok

            ### t-2: Research B
            PENDING

            **Spec:** Research B.
            **Verify:** echo ok

            ### t-3: Synthesize A and B
            PENDING
            **Blocked by:** t-1, t-2

            **Spec:** Combine results.
            **Verify:** echo ok
        """)
        tasks = parse_boi_spec(content)
        self.assertEqual(len(tasks), 3)
        self.assertEqual(tasks[2].blocked_by, ["t-1", "t-2"])

    def test_blocked_by_with_extra_whitespace(self):
        """Blocked by line with extra whitespace around deps."""
        content = textwrap.dedent("""\
            ### t-1: First
            PENDING

            **Spec:** First.
            **Verify:** echo ok

            ### t-2: Second
            PENDING
            **Blocked by:**   t-1 ,  t-3

            **Spec:** Second.
            **Verify:** echo ok

            ### t-3: Third
            PENDING

            **Spec:** Third.
            **Verify:** echo ok
        """)
        tasks = parse_boi_spec(content)
        self.assertEqual(tasks[1].blocked_by, ["t-1", "t-3"])

    def test_blocked_by_after_blank_line(self):
        """Blocked by line can appear anywhere in task body, not just after status."""
        content = textwrap.dedent("""\
            ### t-1: First
            DONE

            **Spec:** First.
            **Verify:** echo ok

            ### t-2: Second
            PENDING

            **Blocked by:** t-1

            **Spec:** Do work.
            **Verify:** echo ok
        """)
        tasks = parse_boi_spec(content)
        self.assertEqual(tasks[1].blocked_by, ["t-1"])

    def test_blocked_by_preserved_across_statuses(self):
        """Blocked by should be parsed regardless of task status."""
        content = textwrap.dedent("""\
            ### t-1: First
            DONE

            **Spec:** First.
            **Verify:** echo ok

            ### t-2: Depends on first
            DONE
            **Blocked by:** t-1

            **Spec:** Second.
            **Verify:** echo ok
        """)
        tasks = parse_boi_spec(content)
        self.assertEqual(tasks[1].blocked_by, ["t-1"])


# ── validate_dependencies Tests ──────────────────────────────────────────────


class TestValidateDependencies(unittest.TestCase):
    """Tests for validate_dependencies() in spec_validator.py."""

    def test_no_dependencies(self):
        """Spec with no dependencies should have no errors."""
        content = textwrap.dedent("""\
            ### t-1: First
            PENDING

            **Spec:** Do something.
            **Verify:** echo ok

            ### t-2: Second
            PENDING

            **Spec:** Do something else.
            **Verify:** echo ok
        """)
        errors = validate_dependencies(content)
        self.assertEqual(errors, [])

    def test_valid_linear_chain(self):
        """Linear dependency chain (t-1 -> t-2 -> t-3) is valid."""
        content = textwrap.dedent("""\
            ### t-1: First
            PENDING

            **Spec:** Do first thing.
            **Verify:** echo ok

            ### t-2: Second
            PENDING
            **Blocked by:** t-1

            **Spec:** Do second thing.
            **Verify:** echo ok

            ### t-3: Third
            PENDING
            **Blocked by:** t-2

            **Spec:** Do third thing.
            **Verify:** echo ok
        """)
        errors = validate_dependencies(content)
        self.assertEqual(errors, [])

    def test_valid_diamond_dag(self):
        """Diamond DAG (t-1/t-2 -> t-3) is valid."""
        content = textwrap.dedent("""\
            ### t-1: Research A
            PENDING

            **Spec:** Research A.
            **Verify:** echo ok

            ### t-2: Research B
            PENDING

            **Spec:** Research B.
            **Verify:** echo ok

            ### t-3: Synthesize
            PENDING
            **Blocked by:** t-1, t-2

            **Spec:** Combine A and B.
            **Verify:** echo ok
        """)
        errors = validate_dependencies(content)
        self.assertEqual(errors, [])

    def test_cycle_detected_two_tasks(self):
        """Two tasks blocking each other should be detected as a cycle."""
        content = textwrap.dedent("""\
            ### t-1: First
            PENDING
            **Blocked by:** t-2

            **Spec:** Do first.
            **Verify:** echo ok

            ### t-2: Second
            PENDING
            **Blocked by:** t-1

            **Spec:** Do second.
            **Verify:** echo ok
        """)
        errors = validate_dependencies(content)
        self.assertTrue(any("cycle" in e.lower() for e in errors))

    def test_cycle_detected_three_tasks(self):
        """Three-task cycle (t-1 -> t-2 -> t-3 -> t-1) should be detected."""
        content = textwrap.dedent("""\
            ### t-1: First
            PENDING
            **Blocked by:** t-3

            **Spec:** Do first.
            **Verify:** echo ok

            ### t-2: Second
            PENDING
            **Blocked by:** t-1

            **Spec:** Do second.
            **Verify:** echo ok

            ### t-3: Third
            PENDING
            **Blocked by:** t-2

            **Spec:** Do third.
            **Verify:** echo ok
        """)
        errors = validate_dependencies(content)
        self.assertTrue(any("cycle" in e.lower() for e in errors))

    def test_unmet_dependency(self):
        """Blocked by a task ID that doesn't exist should be an error."""
        content = textwrap.dedent("""\
            ### t-1: First
            PENDING

            **Spec:** Do first.
            **Verify:** echo ok

            ### t-2: Second
            PENDING
            **Blocked by:** t-99

            **Spec:** Do second.
            **Verify:** echo ok
        """)
        errors = validate_dependencies(content)
        self.assertTrue(any("t-99" in e and "doesn't exist" in e for e in errors))

    def test_self_dependency(self):
        """A task blocked by itself should be detected as a cycle."""
        content = textwrap.dedent("""\
            ### t-1: Self-referential
            PENDING
            **Blocked by:** t-1

            **Spec:** Do something.
            **Verify:** echo ok
        """)
        errors = validate_dependencies(content)
        self.assertTrue(any("cycle" in e.lower() for e in errors))

    def test_complex_valid_dag(self):
        """Wide fan-out + fan-in DAG is valid."""
        content = textwrap.dedent("""\
            ### t-1: Source A
            PENDING

            **Spec:** Source A.
            **Verify:** echo ok

            ### t-2: Source B
            PENDING

            **Spec:** Source B.
            **Verify:** echo ok

            ### t-3: Source C
            PENDING

            **Spec:** Source C.
            **Verify:** echo ok

            ### t-4: Process A+B
            PENDING
            **Blocked by:** t-1, t-2

            **Spec:** Process.
            **Verify:** echo ok

            ### t-5: Final
            PENDING
            **Blocked by:** t-3, t-4

            **Spec:** Final.
            **Verify:** echo ok
        """)
        errors = validate_dependencies(content)
        self.assertEqual(errors, [])


# ── check_task_sizing Tests ──────────────────────────────────────────────────


class TestCheckTaskSizing(unittest.TestCase):
    """Tests for check_task_sizing() heuristic warnings."""

    def test_normal_task_no_warnings(self):
        """A normal-sized task should produce no warnings."""
        warnings = check_task_sizing(
            "t-1",
            "**Spec:** Read the config file and update the connection string.\n\n"
            "**Verify:** echo ok"
        )
        self.assertEqual(warnings, [])

    def test_very_long_spec_warns(self):
        """Task with > 2000 chars in spec body should warn."""
        long_body = "x " * 1100  # ~2200 chars
        warnings = check_task_sizing("t-1", f"**Spec:** {long_body}\n\n**Verify:** echo ok")
        self.assertTrue(any("long" in w.lower() or "split" in w.lower() for w in warnings))

    def test_very_short_spec_warns(self):
        """Task with < 50 chars in spec body should warn."""
        warnings = check_task_sizing("t-1", "**Spec:** Do it.\n\n**Verify:** echo ok")
        self.assertTrue(any("short" in w.lower() or "vague" in w.lower() for w in warnings))

    def test_multiple_write_operations_warns(self):
        """Task that writes to 3+ files should warn about splitting."""
        body = (
            "**Spec:** Write to output1.md. Then create output2.py. "
            "Then save to output3.json.\n\n**Verify:** echo ok"
        )
        warnings = check_task_sizing("t-1", body)
        self.assertTrue(any("split" in w.lower() or "mutation" in w.lower() for w in warnings))

    def test_combining_keywords_warns(self):
        """Task with combining keywords should warn."""
        body = (
            "**Spec:** Implement the login form and also add the password reset flow, "
            "additionally set up email verification.\n\n**Verify:** echo ok"
        )
        warnings = check_task_sizing("t-1", body)
        self.assertTrue(any("combining" in w.lower() or "multiple" in w.lower() for w in warnings))


# ── Integration: validate_spec includes dependency validation ────────────────


class TestValidateSpecWithDependencies(unittest.TestCase):
    """Test that validate_spec() integrates dependency validation."""

    def test_cycle_causes_validation_error(self):
        """A spec with a dependency cycle should fail validation."""
        content = textwrap.dedent("""\
            # Spec

            ### t-1: First
            PENDING
            **Blocked by:** t-2

            **Spec:** Do first.
            **Verify:** echo ok

            ### t-2: Second
            PENDING
            **Blocked by:** t-1

            **Spec:** Do second.
            **Verify:** echo ok
        """)
        result = validate_spec(content)
        self.assertFalse(result.valid)
        self.assertTrue(any("cycle" in e.lower() for e in result.errors))

    def test_unmet_dep_causes_validation_error(self):
        """A spec with unmet dependency should fail validation."""
        content = textwrap.dedent("""\
            # Spec

            ### t-1: First
            PENDING
            **Blocked by:** t-99

            **Spec:** Do first.
            **Verify:** echo ok
        """)
        result = validate_spec(content)
        self.assertFalse(result.valid)
        self.assertTrue(any("t-99" in e for e in result.errors))

    def test_task_sizing_produces_warnings(self):
        """Task sizing heuristics should appear as warnings, not errors."""
        short_spec = textwrap.dedent("""\
            # Spec

            ### t-1: Do it
            PENDING

            **Spec:** Do it.
            **Verify:** echo ok
        """)
        result = validate_spec(short_spec)
        # Should still be valid (warnings don't block)
        self.assertTrue(result.valid, result.errors)
        # Should have a sizing warning
        self.assertTrue(any("short" in w.lower() or "vague" in w.lower() for w in result.warnings))


if __name__ == "__main__":
    unittest.main()
