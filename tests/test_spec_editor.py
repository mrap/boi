"""Tests for lib/spec_editor.py — add_task and skip_task functions."""

import os

# Adjust path so we can import from lib/
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from lib.spec_editor import add_task, block_task, reorder_task, skip_task

SAMPLE_SPEC = """\
# Test Spec

## Tasks

### t-1: First task
DONE

**Spec:** Do the first thing.

**Verify:** echo "done"

### t-2: Second task
PENDING

**Spec:** Do the second thing.

**Verify:** echo "check"

### t-3: Third task
PENDING

**Spec:** Do the third thing.

**Verify:** echo "verify"
"""


class TestAddTask(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.spec_path = os.path.join(self.tmpdir, "test.spec.md")
        Path(self.spec_path).write_text(SAMPLE_SPEC, encoding="utf-8")

    def tearDown(self):
        for f in Path(self.tmpdir).iterdir():
            f.unlink()
        os.rmdir(self.tmpdir)

    def test_add_task_basic(self):
        new_id = add_task(self.spec_path, "Fourth task", "Do fourth", "echo ok")
        self.assertEqual(new_id, "t-4")
        content = Path(self.spec_path).read_text(encoding="utf-8")
        self.assertIn("### t-4: Fourth task", content)
        self.assertIn("PENDING", content.split("### t-4:")[1])
        self.assertIn("**Spec:** Do fourth", content)
        self.assertIn("**Verify:** echo ok", content)

    def test_add_task_no_spec_no_verify(self):
        new_id = add_task(self.spec_path, "Bare task")
        self.assertEqual(new_id, "t-4")
        content = Path(self.spec_path).read_text(encoding="utf-8")
        self.assertIn("### t-4: Bare task", content)
        # Should not have Spec or Verify lines in the new section
        after_heading = content.split("### t-4: Bare task")[1]
        self.assertNotIn("**Spec:**", after_heading)
        self.assertNotIn("**Verify:**", after_heading)

    def test_add_task_increments_id(self):
        add_task(self.spec_path, "Task four")
        new_id = add_task(self.spec_path, "Task five")
        self.assertEqual(new_id, "t-5")
        content = Path(self.spec_path).read_text(encoding="utf-8")
        self.assertIn("### t-5: Task five", content)

    def test_add_task_empty_title_raises(self):
        with self.assertRaises(ValueError):
            add_task(self.spec_path, "")

    def test_add_task_whitespace_title_raises(self):
        with self.assertRaises(ValueError):
            add_task(self.spec_path, "   ")

    def test_add_task_empty_spec(self):
        """Empty spec (no tasks yet) should create t-1."""
        empty_spec = "# Empty Spec\n\n## Tasks\n"
        Path(self.spec_path).write_text(empty_spec, encoding="utf-8")
        new_id = add_task(self.spec_path, "First task")
        self.assertEqual(new_id, "t-1")

    def test_add_task_atomic_write(self):
        """Verify no .tmp file is left behind."""
        add_task(self.spec_path, "Atomic test")
        tmp_path = self.spec_path + ".tmp"
        self.assertFalse(os.path.exists(tmp_path))


class TestSkipTask(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.spec_path = os.path.join(self.tmpdir, "test.spec.md")
        Path(self.spec_path).write_text(SAMPLE_SPEC, encoding="utf-8")

    def tearDown(self):
        for f in Path(self.tmpdir).iterdir():
            f.unlink()
        os.rmdir(self.tmpdir)

    def test_skip_pending_task(self):
        skip_task(self.spec_path, "t-2")
        content = Path(self.spec_path).read_text(encoding="utf-8")
        # Find the status line after t-2 heading
        lines = content.splitlines()
        for i, line in enumerate(lines):
            if "### t-2:" in line:
                # Next non-blank line should be SKIPPED
                for j in range(i + 1, len(lines)):
                    if lines[j].strip():
                        self.assertEqual(lines[j].strip(), "SKIPPED")
                        break
                break

    def test_skip_with_reason(self):
        skip_task(self.spec_path, "t-2", reason="Not needed anymore")
        content = Path(self.spec_path).read_text(encoding="utf-8")
        self.assertIn("SKIPPED — Not needed anymore", content)

    def test_skip_done_task_raises(self):
        with self.assertRaises(ValueError) as ctx:
            skip_task(self.spec_path, "t-1")
        self.assertIn("already DONE", str(ctx.exception))

    def test_skip_nonexistent_task_raises(self):
        with self.assertRaises(ValueError) as ctx:
            skip_task(self.spec_path, "t-99")
        self.assertIn("not found", str(ctx.exception))

    def test_skip_already_skipped_raises(self):
        skip_task(self.spec_path, "t-2")
        with self.assertRaises(ValueError) as ctx:
            skip_task(self.spec_path, "t-2")
        self.assertIn("already SKIPPED", str(ctx.exception))

    def test_skip_preserves_other_tasks(self):
        skip_task(self.spec_path, "t-2")
        content = Path(self.spec_path).read_text(encoding="utf-8")
        # t-1 should still be DONE
        self.assertIn("### t-1: First task", content)
        # t-3 should still be PENDING
        after_t3 = content.split("### t-3:")[1]
        self.assertIn("PENDING", after_t3.split("\n")[1])

    def test_skip_atomic_write(self):
        skip_task(self.spec_path, "t-2")
        tmp_path = self.spec_path + ".tmp"
        self.assertFalse(os.path.exists(tmp_path))


SAMPLE_SPEC_FOR_REORDER = """\
# Test Spec

## Tasks

### t-1: First task
DONE

**Spec:** Do the first thing.

**Verify:** echo "done"

### t-2: Second task
DONE

**Spec:** Do the second thing.

**Verify:** echo "done2"

### t-3: Third task
PENDING

**Spec:** Do the third thing.

**Verify:** echo "check3"

### t-4: Fourth task
PENDING

**Spec:** Do the fourth thing.

**Verify:** echo "check4"

### t-5: Fifth task
PENDING

**Spec:** Do the fifth thing.

**Verify:** echo "check5"
"""


class TestReorderTask(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.spec_path = os.path.join(self.tmpdir, "test.spec.md")
        Path(self.spec_path).write_text(SAMPLE_SPEC_FOR_REORDER, encoding="utf-8")

    def tearDown(self):
        for f in Path(self.tmpdir).iterdir():
            f.unlink()
        os.rmdir(self.tmpdir)

    def _task_order(self) -> list[str]:
        """Return task IDs in document order."""
        import re

        content = Path(self.spec_path).read_text(encoding="utf-8")
        return re.findall(r"###\s+(t-\d+):", content)

    def test_reorder_moves_task_before_other_pending(self):
        """Moving t-5 should place it right after the last DONE task (t-2)."""
        reorder_task(self.spec_path, "t-5")
        order = self._task_order()
        # t-5 should now be right after t-2 (the last DONE)
        self.assertEqual(order, ["t-1", "t-2", "t-5", "t-3", "t-4"])

    def test_reorder_middle_pending_task(self):
        """Moving t-4 should place it right after t-2."""
        reorder_task(self.spec_path, "t-4")
        order = self._task_order()
        self.assertEqual(order, ["t-1", "t-2", "t-4", "t-3", "t-5"])

    def test_reorder_already_next_is_noop(self):
        """t-3 is already the first PENDING task, so reorder is a no-op."""
        content_before = Path(self.spec_path).read_text(encoding="utf-8")
        reorder_task(self.spec_path, "t-3")
        content_after = Path(self.spec_path).read_text(encoding="utf-8")
        self.assertEqual(content_before, content_after)

    def test_reorder_done_task_raises(self):
        with self.assertRaises(ValueError) as ctx:
            reorder_task(self.spec_path, "t-1")
        self.assertIn("DONE", str(ctx.exception))

    def test_reorder_nonexistent_task_raises(self):
        with self.assertRaises(ValueError) as ctx:
            reorder_task(self.spec_path, "t-99")
        self.assertIn("not found", str(ctx.exception))

    def test_reorder_skipped_task_raises(self):
        skip_task(self.spec_path, "t-3")
        with self.assertRaises(ValueError) as ctx:
            reorder_task(self.spec_path, "t-3")
        self.assertIn("SKIPPED", str(ctx.exception))

    def test_reorder_preserves_task_content(self):
        """After reorder, the moved task's body should be intact."""
        reorder_task(self.spec_path, "t-5")
        content = Path(self.spec_path).read_text(encoding="utf-8")
        # t-5's spec text should still be present
        self.assertIn("Do the fifth thing.", content)
        self.assertIn('echo "check5"', content)

    def test_reorder_no_done_tasks(self):
        """When no tasks are DONE, reordered task goes to the front."""
        spec_no_done = """\
# Test Spec

## Tasks

### t-1: First task
PENDING

**Spec:** first

### t-2: Second task
PENDING

**Spec:** second

### t-3: Third task
PENDING

**Spec:** third
"""
        Path(self.spec_path).write_text(spec_no_done, encoding="utf-8")
        reorder_task(self.spec_path, "t-3")
        order = self._task_order()
        self.assertEqual(order, ["t-3", "t-1", "t-2"])


SAMPLE_SPEC_FOR_BLOCK = """\
# Test Spec

## Tasks

### t-1: First task
DONE

**Spec:** Do the first thing.

**Verify:** echo "done"

### t-2: Second task
PENDING

**Spec:** Do the second thing.

**Verify:** echo "check"

### t-3: Third task
PENDING

**Spec:** Do the third thing.

**Verify:** echo "verify"

### t-4: Fourth task
PENDING

**Spec:** Do the fourth thing.

**Verify:** echo "check4"
"""


class TestBlockTask(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.spec_path = os.path.join(self.tmpdir, "test.spec.md")
        Path(self.spec_path).write_text(SAMPLE_SPEC_FOR_BLOCK, encoding="utf-8")

    def tearDown(self):
        for f in Path(self.tmpdir).iterdir():
            f.unlink()
        os.rmdir(self.tmpdir)

    def test_block_creates_blocked_by_line(self):
        block_task(self.spec_path, "t-3", "t-2")
        content = Path(self.spec_path).read_text(encoding="utf-8")
        self.assertIn("**Blocked by:** t-2", content)
        # Verify it's in t-3's section (between t-3 heading and t-4 heading)
        t3_section = content.split("### t-3:")[1].split("### t-4:")[0]
        self.assertIn("**Blocked by:** t-2", t3_section)

    def test_block_appends_to_existing(self):
        block_task(self.spec_path, "t-3", "t-1")
        block_task(self.spec_path, "t-3", "t-2")
        content = Path(self.spec_path).read_text(encoding="utf-8")
        t3_section = content.split("### t-3:")[1].split("### t-4:")[0]
        self.assertIn("**Blocked by:** t-1, t-2", t3_section)

    def test_block_does_not_duplicate_dependency(self):
        block_task(self.spec_path, "t-3", "t-1")
        block_task(self.spec_path, "t-3", "t-1")
        content = Path(self.spec_path).read_text(encoding="utf-8")
        t3_section = content.split("### t-3:")[1].split("### t-4:")[0]
        self.assertIn("**Blocked by:** t-1", t3_section)
        # Should not have "t-1, t-1"
        self.assertNotIn("t-1, t-1", t3_section)

    def test_block_nonexistent_target_raises(self):
        with self.assertRaises(ValueError) as ctx:
            block_task(self.spec_path, "t-99", "t-1")
        self.assertIn("not found", str(ctx.exception))

    def test_block_nonexistent_dependency_raises(self):
        with self.assertRaises(ValueError) as ctx:
            block_task(self.spec_path, "t-2", "t-99")
        self.assertIn("not found", str(ctx.exception))

    def test_block_self_raises(self):
        with self.assertRaises(ValueError) as ctx:
            block_task(self.spec_path, "t-2", "t-2")
        self.assertIn("cannot block itself", str(ctx.exception))

    def test_block_done_task_raises(self):
        with self.assertRaises(ValueError) as ctx:
            block_task(self.spec_path, "t-1", "t-2")
        self.assertIn("already DONE", str(ctx.exception))

    def test_block_skipped_task_raises(self):
        skip_task(self.spec_path, "t-2")
        with self.assertRaises(ValueError) as ctx:
            block_task(self.spec_path, "t-2", "t-1")
        self.assertIn("already SKIPPED", str(ctx.exception))

    def test_block_preserves_spec_content(self):
        block_task(self.spec_path, "t-2", "t-1")
        content = Path(self.spec_path).read_text(encoding="utf-8")
        self.assertIn("Do the second thing.", content)
        self.assertIn("Do the third thing.", content)
        self.assertIn("### t-1: First task", content)

    def test_block_atomic_write(self):
        block_task(self.spec_path, "t-2", "t-1")
        self.assertFalse(os.path.exists(self.spec_path + ".tmp"))


if __name__ == "__main__":
    unittest.main()
