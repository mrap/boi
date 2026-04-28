# test_dag_management.py — Tests for first-class DAG dependency management.
#
# Covers:
#   - Parsing ## Dependencies section (Option C format)
#   - DAG validation (cycles, missing refs, orphans, self-ref, duplicates)
#   - CLI dep editing operations (add, rm, set, clear, swap)
#   - Migration from **Blocked by:** to ## Dependencies
#   - Backward compatibility (parser reads both formats)
#   - ASCII visualization

import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from lib.dag import (
    build_adjacency_list,
    check_dep_conflicts,
    critical_path,
    deps_viz,
    find_assignable_tasks,
    topological_sort,
    validate_dag,
    validate_deps_section,
)
from lib.spec_parser import get_dependencies, parse_boi_spec, parse_deps_section


# ─── Test Fixtures ────────────────────────────────────────────────────────────

SPEC_WITH_DEPS_SECTION = """\
# Test Spec

**Mode:** discover

## Dependencies
t-1: (none)
t-2: (none)
t-3: t-1, t-2
t-4: (none)
t-5: t-3

## Tasks

### t-1: First task
DONE

**Spec:** Do the first thing.

**Verify:** echo ok

### t-2: Second task
DONE

**Spec:** Do the second thing.

**Verify:** echo ok

### t-3: Third task
PENDING

**Spec:** Do the third thing.

**Verify:** echo ok

### t-4: Independent task
PENDING

**Spec:** Do something independent.

**Verify:** echo ok

### t-5: Fifth task
PENDING

**Spec:** Do the fifth thing.

**Verify:** echo ok
"""

SPEC_WITH_INLINE_DEPS = """\
# Test Spec

**Mode:** discover

## Tasks

### t-1: First task
DONE

**Spec:** Do the first thing.

**Verify:** echo ok

### t-2: Second task
DONE

**Spec:** Do the second thing.

**Verify:** echo ok

### t-3: Third task
PENDING
**Blocked by:** t-1, t-2

**Spec:** Do the third thing.

**Verify:** echo ok

### t-4: Independent task
PENDING

**Spec:** Do something independent.

**Verify:** echo ok

### t-5: Fifth task
PENDING
**Blocked by:** t-3

**Spec:** Do the fifth thing.

**Verify:** echo ok
"""

SPEC_WITH_BOTH_FORMATS = """\
# Test Spec

**Mode:** discover

## Dependencies
t-1: (none)
t-2: (none)
t-3: t-1, t-2
t-4: (none)
t-5: t-3

## Tasks

### t-1: First task
DONE

**Spec:** Do the first thing.

**Verify:** echo ok

### t-2: Second task
DONE

**Spec:** Do the second thing.

**Verify:** echo ok

### t-3: Third task
PENDING
**Blocked by:** t-1, t-2

**Spec:** Do the third thing.

**Verify:** echo ok
"""

SPEC_NO_DEPS = """\
# Test Spec

**Mode:** discover

## Tasks

### t-1: First task
PENDING

**Spec:** Do the first thing.

**Verify:** echo ok

### t-2: Second task
PENDING

**Spec:** Do the second thing.

**Verify:** echo ok
"""


# ─── Parsing Tests ────────────────────────────────────────────────────────────


class TestParseDepsSection(unittest.TestCase):
    """Tests for parsing the ## Dependencies section."""

    def test_parse_basic_deps_section(self):
        result = parse_deps_section(SPEC_WITH_DEPS_SECTION)
        self.assertIsNotNone(result)
        self.assertEqual(result["t-1"], [])
        self.assertEqual(result["t-2"], [])
        self.assertEqual(set(result["t-3"]), {"t-1", "t-2"})
        self.assertEqual(result["t-4"], [])
        self.assertEqual(result["t-5"], ["t-3"])

    def test_parse_no_deps_section_returns_none(self):
        result = parse_deps_section(SPEC_NO_DEPS)
        self.assertIsNone(result)

    def test_parse_none_keyword(self):
        content = """\
## Dependencies
t-1: (none)
t-2: (none)
"""
        result = parse_deps_section(content)
        self.assertIsNotNone(result)
        self.assertEqual(result["t-1"], [])
        self.assertEqual(result["t-2"], [])

    def test_parse_empty_deps(self):
        content = """\
## Dependencies
t-1:
t-2: t-1
"""
        result = parse_deps_section(content)
        self.assertIsNotNone(result)
        self.assertEqual(result["t-1"], [])
        self.assertEqual(result["t-2"], ["t-1"])

    def test_parse_ignores_blank_lines(self):
        content = """\
## Dependencies
t-1: (none)

t-2: t-1

t-3: t-1, t-2
"""
        result = parse_deps_section(content)
        self.assertIsNotNone(result)
        self.assertEqual(len(result), 3)

    def test_parse_stops_at_next_heading(self):
        content = """\
## Dependencies
t-1: (none)
t-2: t-1

## Tasks

### t-1: First
PENDING
"""
        result = parse_deps_section(content)
        self.assertIsNotNone(result)
        self.assertEqual(len(result), 2)

    def test_parse_ignores_non_matching_lines(self):
        content = """\
## Dependencies
# This is a comment
t-1: (none)
Some non-matching line
t-2: t-1
"""
        result = parse_deps_section(content)
        self.assertIsNotNone(result)
        self.assertEqual(len(result), 2)


class TestDepsIntegrationWithParser(unittest.TestCase):
    """Tests that parse_boi_spec uses ## Dependencies section when present."""

    def test_deps_section_populates_blocked_by(self):
        tasks = parse_boi_spec(SPEC_WITH_DEPS_SECTION)
        task_map = {t.id: t for t in tasks}

        self.assertEqual(task_map["t-1"].blocked_by, [])
        self.assertEqual(task_map["t-2"].blocked_by, [])
        self.assertEqual(set(task_map["t-3"].blocked_by), {"t-1", "t-2"})
        self.assertEqual(task_map["t-4"].blocked_by, [])
        self.assertEqual(task_map["t-5"].blocked_by, ["t-3"])

    def test_deps_section_takes_precedence_over_inline(self):
        """When both formats exist, ## Dependencies is authoritative."""
        content = """\
# Test

## Dependencies
t-1: (none)
t-2: (none)
t-3: t-1

## Tasks

### t-1: First
DONE

**Spec:** x
**Verify:** echo ok

### t-2: Second
DONE

**Spec:** x
**Verify:** echo ok

### t-3: Third
PENDING
**Blocked by:** t-1, t-2

**Spec:** x
**Verify:** echo ok
"""
        tasks = parse_boi_spec(content)
        task_map = {t.id: t for t in tasks}
        # Section says only t-1, so that's what we get
        self.assertEqual(task_map["t-3"].blocked_by, ["t-1"])

    def test_inline_deps_still_work_without_section(self):
        """Legacy specs without ## Dependencies still parse correctly."""
        tasks = parse_boi_spec(SPEC_WITH_INLINE_DEPS)
        task_map = {t.id: t for t in tasks}

        self.assertEqual(set(task_map["t-3"].blocked_by), {"t-1", "t-2"})
        self.assertEqual(task_map["t-5"].blocked_by, ["t-3"])

    def test_tasks_not_in_section_treated_as_independent(self):
        """Tasks listed in ## Tasks but not in ## Dependencies are independent."""
        content = """\
# Test

## Dependencies
t-1: (none)
t-2: t-1

## Tasks

### t-1: First
DONE

**Spec:** x
**Verify:** echo ok

### t-2: Second
PENDING

**Spec:** x
**Verify:** echo ok

### t-3: Third (unlisted)
PENDING

**Spec:** x
**Verify:** echo ok
"""
        tasks = parse_boi_spec(content)
        task_map = {t.id: t for t in tasks}
        self.assertEqual(task_map["t-3"].blocked_by, [])


# ─── DAG Validation Tests ─────────────────────────────────────────────────────


class TestDagValidation(unittest.TestCase):
    """Tests for DAG validation (cycles, missing refs, etc.)."""

    def test_valid_dag(self):
        deps = {"t-1": [], "t-2": [], "t-3": ["t-1", "t-2"], "t-5": ["t-3"]}
        task_ids = {"t-1", "t-2", "t-3", "t-5"}
        errors = validate_dag(deps, task_ids)
        self.assertEqual(errors, [])

    def test_cycle_detection(self):
        deps = {"t-1": ["t-3"], "t-2": [], "t-3": ["t-1"]}
        task_ids = {"t-1", "t-2", "t-3"}
        errors = validate_dag(deps, task_ids)
        self.assertTrue(any("cycle" in e.lower() for e in errors))

    def test_missing_ref(self):
        deps = {"t-1": [], "t-2": ["t-99"]}
        task_ids = {"t-1", "t-2"}
        errors = validate_dag(deps, task_ids)
        self.assertTrue(any("t-99" in e for e in errors))

    def test_self_reference(self):
        deps = {"t-1": ["t-1"]}
        task_ids = {"t-1"}
        errors = validate_dag(deps, task_ids)
        self.assertTrue(any("self" in e.lower() for e in errors))

    def test_duplicate_entries(self):
        """Duplicate task IDs in the deps section should be detected."""
        content = """\
## Dependencies
t-1: (none)
t-1: t-2
"""
        errors = validate_deps_section(content)
        self.assertTrue(any("duplicate" in e.lower() for e in errors))


# ─── Dep Editor Tests ─────────────────────────────────────────────────────────


class TestDepEditor(unittest.TestCase):
    """Tests for dependency editing operations."""

    def _write_spec(self, content: str) -> str:
        """Write spec content to a temp file and return the path."""
        fd, path = tempfile.mkstemp(suffix=".spec.md")
        with os.fdopen(fd, "w") as f:
            f.write(content)
        return path

    def test_add_dep(self):
        from lib.spec_editor import add_dep

        path = self._write_spec(SPEC_WITH_DEPS_SECTION)
        try:
            add_dep(path, "t-4", "t-2")
            with open(path) as f:
                content = f.read()
            deps = parse_deps_section(content)
            self.assertIn("t-2", deps["t-4"])
        finally:
            os.unlink(path)

    def test_add_dep_prevents_cycle(self):
        from lib.spec_editor import add_dep

        path = self._write_spec(SPEC_WITH_DEPS_SECTION)
        try:
            # t-3 depends on t-1. Adding t-1 depends on t-3 creates cycle.
            with self.assertRaises(ValueError) as ctx:
                add_dep(path, "t-1", "t-3")
            self.assertIn("cycle", str(ctx.exception).lower())
        finally:
            os.unlink(path)

    def test_add_dep_no_duplicate(self):
        from lib.spec_editor import add_dep

        path = self._write_spec(SPEC_WITH_DEPS_SECTION)
        try:
            # t-3 already depends on t-1
            add_dep(path, "t-3", "t-1")
            with open(path) as f:
                deps = parse_deps_section(f.read())
            self.assertEqual(deps["t-3"].count("t-1"), 1)
        finally:
            os.unlink(path)

    def test_remove_dep(self):
        from lib.spec_editor import remove_dep

        path = self._write_spec(SPEC_WITH_DEPS_SECTION)
        try:
            remove_dep(path, "t-3", "t-1")
            with open(path) as f:
                content = f.read()
            deps = parse_deps_section(content)
            self.assertNotIn("t-1", deps["t-3"])
            self.assertIn("t-2", deps["t-3"])
        finally:
            os.unlink(path)

    def test_remove_last_dep_becomes_none(self):
        from lib.spec_editor import remove_dep

        content = """\
# Test

## Dependencies
t-1: (none)
t-2: t-1

## Tasks

### t-1: First
DONE

**Spec:** x
**Verify:** echo ok

### t-2: Second
PENDING

**Spec:** x
**Verify:** echo ok
"""
        path = self._write_spec(content)
        try:
            remove_dep(path, "t-2", "t-1")
            with open(path) as f:
                result = f.read()
            deps = parse_deps_section(result)
            self.assertEqual(deps["t-2"], [])
        finally:
            os.unlink(path)

    def test_set_deps(self):
        from lib.spec_editor import set_deps

        path = self._write_spec(SPEC_WITH_DEPS_SECTION)
        try:
            set_deps(path, "t-5", ["t-1", "t-4"])
            with open(path) as f:
                content = f.read()
            deps = parse_deps_section(content)
            self.assertEqual(set(deps["t-5"]), {"t-1", "t-4"})
        finally:
            os.unlink(path)

    def test_clear_deps(self):
        from lib.spec_editor import clear_deps

        path = self._write_spec(SPEC_WITH_DEPS_SECTION)
        try:
            clear_deps(path, "t-3")
            with open(path) as f:
                content = f.read()
            deps = parse_deps_section(content)
            self.assertEqual(deps["t-3"], [])
        finally:
            os.unlink(path)

    def test_swap_deps(self):
        from lib.spec_editor import swap_deps

        path = self._write_spec(SPEC_WITH_DEPS_SECTION)
        try:
            # Before: t-3: t-1, t-2 and t-5: t-3
            # After swap(t-3, t-5): t-5: t-1, t-2 and t-3: t-5
            swap_deps(path, "t-3", "t-5")
            with open(path) as f:
                content = f.read()
            deps = parse_deps_section(content)
            self.assertEqual(set(deps["t-5"]), {"t-1", "t-2"})
            self.assertEqual(deps["t-3"], ["t-5"])
        finally:
            os.unlink(path)


# ─── Migration Tests ──────────────────────────────────────────────────────────


class TestMigration(unittest.TestCase):
    """Tests for migrating from **Blocked by:** to ## Dependencies format."""

    def test_migrate_creates_deps_section(self):
        from lib.spec_editor import migrate_deps

        fd, path = tempfile.mkstemp(suffix=".spec.md")
        with os.fdopen(fd, "w") as f:
            f.write(SPEC_WITH_INLINE_DEPS)
        try:
            migrate_deps(path)
            with open(path) as f:
                content = f.read()
            deps = parse_deps_section(content)
            self.assertIsNotNone(deps)
            self.assertEqual(deps["t-1"], [])
            self.assertEqual(set(deps["t-3"]), {"t-1", "t-2"})
            self.assertEqual(deps["t-5"], ["t-3"])
        finally:
            os.unlink(path)

    def test_migrate_no_op_if_section_exists(self):
        from lib.spec_editor import migrate_deps

        fd, path = tempfile.mkstemp(suffix=".spec.md")
        with os.fdopen(fd, "w") as f:
            f.write(SPEC_WITH_DEPS_SECTION)
        try:
            migrate_deps(path)
            with open(path) as f:
                content = f.read()
            self.assertEqual(content.count("## Dependencies"), 1)
        finally:
            os.unlink(path)

    def test_migrate_preserves_task_content(self):
        from lib.spec_editor import migrate_deps

        fd, path = tempfile.mkstemp(suffix=".spec.md")
        with os.fdopen(fd, "w") as f:
            f.write(SPEC_WITH_INLINE_DEPS)
        try:
            migrate_deps(path)
            with open(path) as f:
                content = f.read()
            for tid in ["### t-1:", "### t-2:", "### t-3:", "### t-4:", "### t-5:"]:
                self.assertIn(tid, content)
        finally:
            os.unlink(path)


# ─── Visualization Tests ─────────────────────────────────────────────────────


class TestDepsVisualization(unittest.TestCase):
    """Tests for ASCII DAG visualization."""

    def test_viz_basic(self):
        deps = {
            "t-1": [],
            "t-2": [],
            "t-3": ["t-1", "t-2"],
            "t-4": [],
            "t-5": ["t-3"],
        }
        output = deps_viz(deps)
        self.assertIn("t-1", output)
        self.assertIn("t-3", output)
        self.assertIn("t-5", output)

    def test_viz_independent_tasks(self):
        deps = {"t-1": [], "t-2": [], "t-3": []}
        output = deps_viz(deps)
        self.assertIn("t-1", output)
        self.assertIn("t-2", output)
        self.assertIn("t-3", output)

    def test_viz_linear_chain(self):
        deps = {"t-1": [], "t-2": ["t-1"], "t-3": ["t-2"]}
        output = deps_viz(deps)
        self.assertIn("t-1", output)
        self.assertIn("t-2", output)
        self.assertIn("t-3", output)


# ─── Backward Compatibility Tests ─────────────────────────────────────────────


class TestBackwardCompatibility(unittest.TestCase):
    """Tests ensuring old specs without ## Dependencies still work."""

    def test_find_assignable_with_inline_deps(self):
        """find_assignable_tasks works with inline deps (no section)."""
        tasks = parse_boi_spec(SPEC_WITH_INLINE_DEPS)
        assignable = find_assignable_tasks(tasks)
        self.assertIn("t-3", assignable)
        self.assertIn("t-4", assignable)
        self.assertNotIn("t-5", assignable)

    def test_find_assignable_with_deps_section(self):
        """find_assignable_tasks works with ## Dependencies section."""
        tasks = parse_boi_spec(SPEC_WITH_DEPS_SECTION)
        assignable = find_assignable_tasks(tasks)
        self.assertIn("t-3", assignable)
        self.assertIn("t-4", assignable)
        self.assertNotIn("t-5", assignable)

    def test_topological_sort_with_deps_section(self):
        tasks = parse_boi_spec(SPEC_WITH_DEPS_SECTION)
        order = topological_sort(tasks)
        self.assertLess(order.index("t-1"), order.index("t-3"))
        self.assertLess(order.index("t-2"), order.index("t-3"))
        self.assertLess(order.index("t-3"), order.index("t-5"))

    def test_critical_path_with_deps_section(self):
        tasks = parse_boi_spec(SPEC_WITH_DEPS_SECTION)
        path = critical_path(tasks)
        self.assertIn("t-3", path)
        self.assertIn("t-5", path)


# ─── get_dependencies Unified Interface Tests ────────────────────────────────


class TestGetDependencies(unittest.TestCase):
    """Tests for the unified get_dependencies function."""

    def test_returns_section_deps_when_available(self):
        deps = get_dependencies(SPEC_WITH_DEPS_SECTION)
        self.assertEqual(deps["t-1"], [])
        self.assertEqual(set(deps["t-3"]), {"t-1", "t-2"})

    def test_returns_inline_deps_as_fallback(self):
        deps = get_dependencies(SPEC_WITH_INLINE_DEPS)
        self.assertEqual(set(deps["t-3"]), {"t-1", "t-2"})
        self.assertEqual(deps["t-5"], ["t-3"])

    def test_returns_empty_deps_when_none(self):
        deps = get_dependencies(SPEC_NO_DEPS)
        self.assertEqual(deps["t-1"], [])
        self.assertEqual(deps["t-2"], [])


# ─── Conflict Detection Tests ────────────────────────────────────────────────


class TestConflictDetection(unittest.TestCase):
    """Tests for detecting conflicts between section and inline deps."""

    def test_no_conflict_when_matching(self):
        warnings = check_dep_conflicts(SPEC_WITH_BOTH_FORMATS)
        self.assertEqual(len(warnings), 0)

    def test_conflict_detected(self):
        content = """\
# Test

## Dependencies
t-1: (none)
t-2: (none)
t-3: t-1

## Tasks

### t-1: First
DONE

**Spec:** x
**Verify:** echo ok

### t-2: Second
DONE

**Spec:** x
**Verify:** echo ok

### t-3: Third
PENDING
**Blocked by:** t-1, t-2

**Spec:** x
**Verify:** echo ok
"""
        warnings = check_dep_conflicts(content)
        self.assertGreater(len(warnings), 0)
        self.assertIn("t-3", warnings[0])


if __name__ == "__main__":
    unittest.main()
