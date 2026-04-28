"""Tests for lib.conflict_detector — file-level conflict detection."""

import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from lib.conflict_detector import (
    check_conflicts_before_dequeue,
    detect_conflicts,
    extract_target_paths,
    should_block,
)


class TestExtractTargetPaths(unittest.TestCase):
    """Tests for extract_target_paths()."""

    def _write_spec(self, content: str) -> str:
        """Write a spec to a temp file, return the path."""
        fd, path = tempfile.mkstemp(suffix=".spec.md")
        os.write(fd, content.encode("utf-8"))
        os.close(fd)
        self.addCleanup(os.unlink, path)
        return path

    def test_extracts_backtick_paths_from_spec_section(self):
        spec = self._write_spec(
            "### t-1: Do stuff\nPENDING\n\n"
            "**Spec:** Update `~/boi-oss/lib/queue.py` and `~/boi-oss/boi.sh`.\n\n"
            "**Verify:** Tests pass.\n"
        )
        paths = extract_target_paths(spec)
        expanded_home = os.path.expanduser("~")
        self.assertIn(os.path.join(expanded_home, "boi-oss/lib/queue.py"), paths)
        self.assertIn(os.path.join(expanded_home, "boi-oss/boi.sh"), paths)

    def test_extracts_paths_from_files_section(self):
        spec = self._write_spec(
            "### t-1: Do stuff\nPENDING\n\n"
            "**Files:**\n"
            "- lib/foo.py\n"
            "- lib/bar.py\n\n"
            "**Verify:** Tests pass.\n"
        )
        paths = extract_target_paths(spec)
        self.assertIn(os.path.normpath("lib/foo.py"), paths)
        self.assertIn(os.path.normpath("lib/bar.py"), paths)

    def test_stops_at_verify_section(self):
        spec = self._write_spec(
            "### t-1: Do stuff\nPENDING\n\n"
            "**Spec:** Update `~/boi-oss/lib/queue.py`.\n\n"
            "**Verify:** Run `~/boi-oss/tests/test_queue.py`.\n"
        )
        paths = extract_target_paths(spec)
        expanded_home = os.path.expanduser("~")
        self.assertIn(os.path.join(expanded_home, "boi-oss/lib/queue.py"), paths)
        # test_queue.py should NOT be extracted (it's in Verify, not Spec)
        self.assertNotIn(
            os.path.join(expanded_home, "boi-oss/tests/test_queue.py"), paths
        )

    def test_stops_at_task_boundary(self):
        spec = self._write_spec(
            "### t-1: First task\nPENDING\n\n"
            "**Spec:** Update `lib/a.py`.\n\n"
            "### t-2: Second task\nPENDING\n\n"
            "**Spec:** Update `lib/b.py`.\n\n"
        )
        paths = extract_target_paths(spec)
        # Both tasks' paths should be found (we extract from entire spec)
        self.assertIn(os.path.normpath("lib/a.py"), paths)
        self.assertIn(os.path.normpath("lib/b.py"), paths)

    def test_returns_empty_for_missing_file(self):
        paths = extract_target_paths("/nonexistent/spec.md")
        self.assertEqual(paths, set())

    def test_returns_empty_for_no_paths(self):
        spec = self._write_spec(
            "### t-1: Do stuff\nPENDING\n\n"
            "**Spec:** Just do some refactoring.\n\n"
            "**Verify:** Tests pass.\n"
        )
        paths = extract_target_paths(spec)
        self.assertEqual(paths, set())


class TestDetectConflicts(unittest.TestCase):
    """Tests for detect_conflicts()."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.queue_dir = os.path.join(self.tmpdir, "queue")
        os.makedirs(self.queue_dir)
        self.addCleanup(lambda: __import__("shutil").rmtree(self.tmpdir))

    def _write_entry(self, queue_id, status, spec_content, worktree_isolate=False):
        """Write a queue entry and its spec file."""
        spec_path = os.path.join(self.queue_dir, f"{queue_id}.spec.md")
        Path(spec_path).write_text(spec_content, encoding="utf-8")

        entry = {
            "id": queue_id,
            "spec_path": spec_path,
            "status": status,
            "priority": 100,
            "worktree_isolate": worktree_isolate,
        }
        entry_path = os.path.join(self.queue_dir, f"{queue_id}.json")
        Path(entry_path).write_text(
            json.dumps(entry, indent=2) + "\n", encoding="utf-8"
        )
        return entry

    def test_detects_overlap_between_two_specs(self):
        self._write_entry(
            "q-001",
            "running",
            "### t-1: A\nPENDING\n\n**Spec:** Update `lib/shared.py`.\n",
        )
        self._write_entry(
            "q-002",
            "queued",
            "### t-1: B\nPENDING\n\n**Spec:** Update `lib/shared.py`.\n",
        )
        conflicts = detect_conflicts(self.queue_dir)
        self.assertEqual(len(conflicts), 1)
        self.assertEqual(conflicts[0]["spec_a"], "q-001")
        self.assertEqual(conflicts[0]["spec_b"], "q-002")
        self.assertIn(os.path.normpath("lib/shared.py"), conflicts[0]["shared_files"])

    def test_no_false_positive_for_unrelated_specs(self):
        self._write_entry(
            "q-001",
            "running",
            "### t-1: A\nPENDING\n\n**Spec:** Update `lib/foo.py`.\n",
        )
        self._write_entry(
            "q-002",
            "queued",
            "### t-1: B\nPENDING\n\n**Spec:** Update `lib/bar.py`.\n",
        )
        conflicts = detect_conflicts(self.queue_dir)
        self.assertEqual(len(conflicts), 0)

    def test_skips_isolated_specs(self):
        self._write_entry(
            "q-001",
            "running",
            "### t-1: A\nPENDING\n\n**Spec:** Update `lib/shared.py`.\n",
            worktree_isolate=True,
        )
        self._write_entry(
            "q-002",
            "queued",
            "### t-1: B\nPENDING\n\n**Spec:** Update `lib/shared.py`.\n",
        )
        conflicts = detect_conflicts(self.queue_dir)
        self.assertEqual(len(conflicts), 0)

    def test_skips_completed_specs(self):
        self._write_entry(
            "q-001",
            "completed",
            "### t-1: A\nDONE\n\n**Spec:** Update `lib/shared.py`.\n",
        )
        self._write_entry(
            "q-002",
            "queued",
            "### t-1: B\nPENDING\n\n**Spec:** Update `lib/shared.py`.\n",
        )
        conflicts = detect_conflicts(self.queue_dir)
        self.assertEqual(len(conflicts), 0)


class TestShouldBlock(unittest.TestCase):
    """Tests for should_block()."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.queue_dir = os.path.join(self.tmpdir, "queue")
        os.makedirs(self.queue_dir)
        self.addCleanup(lambda: __import__("shutil").rmtree(self.tmpdir))

    def _write_entry(self, queue_id, status, spec_content, worktree_isolate=False):
        spec_path = os.path.join(self.queue_dir, f"{queue_id}.spec.md")
        Path(spec_path).write_text(spec_content, encoding="utf-8")

        entry = {
            "id": queue_id,
            "spec_path": spec_path,
            "status": status,
            "priority": 100,
            "worktree_isolate": worktree_isolate,
        }
        entry_path = os.path.join(self.queue_dir, f"{queue_id}.json")
        Path(entry_path).write_text(
            json.dumps(entry, indent=2) + "\n", encoding="utf-8"
        )
        return entry

    def test_blocks_when_running_spec_shares_files(self):
        self._write_entry(
            "q-001",
            "running",
            "### t-1: A\nPENDING\n\n**Spec:** Update `lib/shared.py`.\n",
        )
        self._write_entry(
            "q-002",
            "queued",
            "### t-1: B\nPENDING\n\n**Spec:** Update `lib/shared.py`.\n",
        )
        blockers = should_block(self.queue_dir, "q-002")
        self.assertEqual(len(blockers), 1)
        self.assertEqual(blockers[0]["blocking_id"], "q-001")

    def test_no_block_for_unrelated_specs(self):
        self._write_entry(
            "q-001",
            "running",
            "### t-1: A\nPENDING\n\n**Spec:** Update `lib/foo.py`.\n",
        )
        self._write_entry(
            "q-002",
            "queued",
            "### t-1: B\nPENDING\n\n**Spec:** Update `lib/bar.py`.\n",
        )
        blockers = should_block(self.queue_dir, "q-002")
        self.assertEqual(len(blockers), 0)

    def test_no_block_for_isolated_new_spec(self):
        self._write_entry(
            "q-001",
            "running",
            "### t-1: A\nPENDING\n\n**Spec:** Update `lib/shared.py`.\n",
        )
        self._write_entry(
            "q-002",
            "queued",
            "### t-1: B\nPENDING\n\n**Spec:** Update `lib/shared.py`.\n",
            worktree_isolate=True,
        )
        blockers = should_block(self.queue_dir, "q-002")
        self.assertEqual(len(blockers), 0)

    def test_no_block_when_running_spec_is_isolated(self):
        self._write_entry(
            "q-001",
            "running",
            "### t-1: A\nPENDING\n\n**Spec:** Update `lib/shared.py`.\n",
            worktree_isolate=True,
        )
        self._write_entry(
            "q-002",
            "queued",
            "### t-1: B\nPENDING\n\n**Spec:** Update `lib/shared.py`.\n",
        )
        blockers = should_block(self.queue_dir, "q-002")
        self.assertEqual(len(blockers), 0)

    def test_only_checks_running_specs(self):
        # q-001 is queued (not running), so it shouldn't block q-002
        self._write_entry(
            "q-001",
            "queued",
            "### t-1: A\nPENDING\n\n**Spec:** Update `lib/shared.py`.\n",
        )
        self._write_entry(
            "q-002",
            "queued",
            "### t-1: B\nPENDING\n\n**Spec:** Update `lib/shared.py`.\n",
        )
        blockers = should_block(self.queue_dir, "q-002")
        self.assertEqual(len(blockers), 0)


class TestCheckConflictsBeforeDequeue(unittest.TestCase):
    """Tests for check_conflicts_before_dequeue()."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.queue_dir = os.path.join(self.tmpdir, "queue")
        os.makedirs(self.queue_dir)
        self.addCleanup(lambda: __import__("shutil").rmtree(self.tmpdir))

    def _write_entry(self, queue_id, status, spec_content, worktree_isolate=False):
        spec_path = os.path.join(self.queue_dir, f"{queue_id}.spec.md")
        Path(spec_path).write_text(spec_content, encoding="utf-8")

        entry = {
            "id": queue_id,
            "spec_path": spec_path,
            "status": status,
            "priority": 100,
            "worktree_isolate": worktree_isolate,
        }
        entry_path = os.path.join(self.queue_dir, f"{queue_id}.json")
        Path(entry_path).write_text(
            json.dumps(entry, indent=2) + "\n", encoding="utf-8"
        )

    def test_returns_conflicting_ids(self):
        self._write_entry(
            "q-001",
            "running",
            "### t-1: A\nPENDING\n\n**Spec:** Update `lib/shared.py`.\n",
        )
        self._write_entry(
            "q-002",
            "queued",
            "### t-1: B\nPENDING\n\n**Spec:** Update `lib/shared.py`.\n",
        )
        result = check_conflicts_before_dequeue(self.queue_dir, "q-002")
        self.assertEqual(result, ["q-001"])

    def test_returns_empty_for_no_conflicts(self):
        self._write_entry(
            "q-001",
            "running",
            "### t-1: A\nPENDING\n\n**Spec:** Update `lib/foo.py`.\n",
        )
        self._write_entry(
            "q-002",
            "queued",
            "### t-1: B\nPENDING\n\n**Spec:** Update `lib/bar.py`.\n",
        )
        result = check_conflicts_before_dequeue(self.queue_dir, "q-002")
        self.assertEqual(result, [])


if __name__ == "__main__":
    unittest.main()
