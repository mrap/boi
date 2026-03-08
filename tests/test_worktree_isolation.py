# test_worktree_isolation.py — Tests for worktree-per-spec isolation.
#
# Covers: worktree creation, merge success, merge conflict detection,
# cleanup on cancel/fail, conflict detector, auto-blocking on dispatch,
# max concurrent worktree limits, and the boi merge command flow.
#
# All tests use temp git repos. Subprocess calls for git worktree
# operations are tested against real temp repos where possible,
# with mocks used only where git operations would be impractical.
#
# Uses stdlib unittest only (no pytest dependency).

import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

# Add parent directory to path so we can import lib modules
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.conflict_detector import (
    check_conflicts_before_dequeue,
    detect_conflicts,
    extract_target_paths,
    should_block,
)
from lib.queue import (
    _read_entry,
    _write_entry,
    cancel,
    cleanup_spec_worktree,
    complete,
    count_active_worktrees,
    create_spec_worktree,
    enqueue,
    fail,
    get_entry,
    merge_spec_worktree,
)
from tests.conftest import BoiTestCase, make_queue_entry, make_spec


def _init_temp_git_repo(path: str) -> None:
    """Initialize a bare-bones git repo with an initial commit."""
    os.makedirs(path, exist_ok=True)
    subprocess.run(
        ["git", "init"], cwd=path, capture_output=True, text=True, check=True
    )
    subprocess.run(
        ["git", "config", "user.email", "test@test.com"],
        cwd=path,
        capture_output=True,
        text=True,
    )
    subprocess.run(
        ["git", "config", "user.name", "Test"],
        cwd=path,
        capture_output=True,
        text=True,
    )
    # Create initial commit so branches work
    readme = os.path.join(path, "README.md")
    Path(readme).write_text("# Test repo\n", encoding="utf-8")
    subprocess.run(
        ["git", "add", "README.md"], cwd=path, capture_output=True, text=True
    )
    subprocess.run(
        ["git", "commit", "-m", "Initial commit"],
        cwd=path,
        capture_output=True,
        text=True,
        check=True,
    )


# ─── Worktree Creation Tests ────────────────────────────────────────────────


class TestDispatchWithIsolateCreatesWorktree(BoiTestCase):
    """test_dispatch_with_isolate_creates_worktree — worktree exists after dispatch."""

    def test_dispatch_with_isolate_creates_worktree(self):
        """Creating a spec worktree produces a directory and updates the queue entry."""
        repo_dir = os.path.join(self.boi_state, "repo")
        _init_temp_git_repo(repo_dir)

        # Create a queue entry first
        spec_path = self.create_spec(tasks_pending=2)
        self.create_queue_entry(
            queue_id="q-001",
            spec_path=spec_path,
            status="queued",
            worktree_isolate=True,
        )

        # Patch WORKTREES_DIR to use temp dir
        worktrees_dir = os.path.join(self.boi_state, "worktrees")
        with patch("lib.queue.WORKTREES_DIR", worktrees_dir):
            result = create_spec_worktree(self.queue_dir, "q-001", repo_dir)

        # Worktree directory exists
        self.assertTrue(os.path.isdir(result["worktree_path"]))
        self.assertEqual(result["worktree_branch"], "boi/q-001")

        # Queue entry was updated
        entry = get_entry(self.queue_dir, "q-001")
        self.assertEqual(entry["worktree_path"], result["worktree_path"])
        self.assertEqual(entry["worktree_branch"], "boi/q-001")
        self.assertTrue(entry["worktree_isolate"])

        # Clean up worktree
        subprocess.run(
            ["git", "worktree", "remove", "--force", result["worktree_path"]],
            cwd=repo_dir,
            capture_output=True,
            text=True,
        )


class TestDispatchWithoutIsolateUsesShared(BoiTestCase):
    """test_dispatch_without_isolate_uses_shared — no worktree created, uses shared checkout."""

    def test_dispatch_without_isolate_uses_shared(self):
        """A normal enqueue does not create worktree fields."""
        spec_path = self.create_spec(tasks_pending=2)
        entry = self.create_queue_entry(
            queue_id="q-001",
            spec_path=spec_path,
            status="queued",
        )

        # No worktree fields
        self.assertIsNone(entry.get("worktree_path"))
        self.assertIsNone(entry.get("worktree_branch"))
        self.assertFalse(entry.get("worktree_isolate", False))


# ─── Parallel Isolation Tests ───────────────────────────────────────────────


class TestParallelIsolatedSpecsDontBlock(BoiTestCase):
    """test_parallel_isolated_specs_dont_block — two isolated specs run simultaneously."""

    def test_parallel_isolated_specs_dont_block(self):
        """Worktree-isolated specs don't conflict even if they touch the same files."""
        # Create two specs referencing the same file
        spec_content_a = textwrap.dedent("""\
            # Spec A
            ## Tasks
            ### t-1: Modify shared file
            PENDING
            **Spec:** Update `~/project/shared.py` with feature A.
            **Verify:** true
        """)
        spec_content_b = textwrap.dedent("""\
            # Spec B
            ## Tasks
            ### t-1: Modify shared file
            PENDING
            **Spec:** Update `~/project/shared.py` with feature B.
            **Verify:** true
        """)

        spec_a = self.create_spec(content=spec_content_a, filename="spec_a.md")
        spec_b = self.create_spec(content=spec_content_b, filename="spec_b.md")

        self.create_queue_entry(
            queue_id="q-001",
            spec_path=spec_a,
            status="running",
            worktree_isolate=True,
        )
        self.create_queue_entry(
            queue_id="q-002",
            spec_path=spec_b,
            status="queued",
            worktree_isolate=True,
        )

        # detect_conflicts skips isolated specs
        conflicts = detect_conflicts(self.queue_dir)
        self.assertEqual(len(conflicts), 0)

        # should_block returns empty for isolated specs
        blockers = should_block(self.queue_dir, "q-002")
        self.assertEqual(len(blockers), 0)


# ─── Merge Tests ────────────────────────────────────────────────────────────


class TestMergeOnCompletionSucceeds(BoiTestCase):
    """test_merge_on_completion_succeeds — clean merge after spec completes."""

    def test_merge_on_completion_succeeds(self):
        """Merging a worktree branch with non-conflicting changes succeeds."""
        repo_dir = os.path.join(self.boi_state, "repo")
        _init_temp_git_repo(repo_dir)

        spec_path = self.create_spec(tasks_pending=1)
        self.create_queue_entry(
            queue_id="q-001",
            spec_path=spec_path,
            status="running",
        )

        worktrees_dir = os.path.join(self.boi_state, "worktrees")
        with patch("lib.queue.WORKTREES_DIR", worktrees_dir):
            wt_result = create_spec_worktree(self.queue_dir, "q-001", repo_dir)

        # Make a change in the worktree
        new_file = os.path.join(wt_result["worktree_path"], "feature.txt")
        Path(new_file).write_text("new feature\n", encoding="utf-8")
        subprocess.run(
            ["git", "add", "feature.txt"],
            cwd=wt_result["worktree_path"],
            capture_output=True,
            text=True,
        )
        subprocess.run(
            ["git", "commit", "-m", "Add feature"],
            cwd=wt_result["worktree_path"],
            capture_output=True,
            text=True,
            check=True,
        )

        # Merge should succeed
        with patch("lib.queue.WORKTREES_DIR", worktrees_dir):
            merge_result = merge_spec_worktree(self.queue_dir, "q-001")

        self.assertEqual(merge_result["merge_status"], "merged")

        # The merged file should exist in the main repo
        self.assertTrue(os.path.isfile(os.path.join(repo_dir, "feature.txt")))

        # Queue entry updated
        entry = get_entry(self.queue_dir, "q-001")
        self.assertEqual(entry["merge_status"], "merged")


class TestMergeConflictDetected(BoiTestCase):
    """test_merge_conflict_detected — conflicting changes set needs_merge status."""

    def test_merge_conflict_detected(self):
        """Merging a branch that conflicts with main detects the conflict."""
        repo_dir = os.path.join(self.boi_state, "repo")
        _init_temp_git_repo(repo_dir)

        spec_path = self.create_spec(tasks_pending=1)
        self.create_queue_entry(
            queue_id="q-001",
            spec_path=spec_path,
            status="running",
        )

        worktrees_dir = os.path.join(self.boi_state, "worktrees")
        with patch("lib.queue.WORKTREES_DIR", worktrees_dir):
            wt_result = create_spec_worktree(self.queue_dir, "q-001", repo_dir)

        # Make a change in the worktree branch
        wt_file = os.path.join(wt_result["worktree_path"], "README.md")
        Path(wt_file).write_text("Worktree change\n", encoding="utf-8")
        subprocess.run(
            ["git", "add", "README.md"],
            cwd=wt_result["worktree_path"],
            capture_output=True,
            text=True,
        )
        subprocess.run(
            ["git", "commit", "-m", "Worktree change"],
            cwd=wt_result["worktree_path"],
            capture_output=True,
            text=True,
            check=True,
        )

        # Make a conflicting change on main
        main_file = os.path.join(repo_dir, "README.md")
        Path(main_file).write_text("Main change\n", encoding="utf-8")
        subprocess.run(
            ["git", "add", "README.md"],
            cwd=repo_dir,
            capture_output=True,
            text=True,
        )
        subprocess.run(
            ["git", "commit", "-m", "Main change"],
            cwd=repo_dir,
            capture_output=True,
            text=True,
            check=True,
        )

        # Merge should detect conflict
        merge_result = merge_spec_worktree(self.queue_dir, "q-001")

        self.assertEqual(merge_result["merge_status"], "conflict")
        self.assertIn("conflicting_files", merge_result)
        self.assertIn("README.md", merge_result["conflicting_files"])

        # Queue entry updated
        entry = get_entry(self.queue_dir, "q-001")
        self.assertEqual(entry["merge_status"], "conflict")

        # Clean up the worktree (it stays after conflict)
        subprocess.run(
            ["git", "worktree", "remove", "--force", wt_result["worktree_path"]],
            cwd=repo_dir,
            capture_output=True,
            text=True,
        )
        subprocess.run(
            ["git", "branch", "-D", "boi/q-001"],
            cwd=repo_dir,
            capture_output=True,
            text=True,
        )


# ─── Cleanup Tests ──────────────────────────────────────────────────────────


class TestCancelCleansWorktree(BoiTestCase):
    """test_cancel_cleans_worktree — cancel removes worktree and branch."""

    def test_cancel_cleans_worktree(self):
        """Cleaning up a spec's worktree removes the directory and branch."""
        repo_dir = os.path.join(self.boi_state, "repo")
        _init_temp_git_repo(repo_dir)

        spec_path = self.create_spec(tasks_pending=1)
        self.create_queue_entry(
            queue_id="q-001",
            spec_path=spec_path,
            status="running",
        )

        worktrees_dir = os.path.join(self.boi_state, "worktrees")
        with patch("lib.queue.WORKTREES_DIR", worktrees_dir):
            wt_result = create_spec_worktree(self.queue_dir, "q-001", repo_dir)

        wt_path = wt_result["worktree_path"]
        self.assertTrue(os.path.isdir(wt_path))

        # Cancel and cleanup
        cleaned = cleanup_spec_worktree(self.queue_dir, "q-001")
        self.assertTrue(cleaned)

        # Worktree directory removed
        self.assertFalse(os.path.isdir(wt_path))

        # Queue entry cleared
        entry = get_entry(self.queue_dir, "q-001")
        self.assertIsNone(entry["worktree_path"])
        self.assertIsNone(entry["worktree_branch"])

        # Branch deleted
        result = subprocess.run(
            ["git", "branch", "--list", "boi/q-001"],
            cwd=repo_dir,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.stdout.strip(), "")


class TestFailCleansWorktree(BoiTestCase):
    """test_fail_cleans_worktree — failed spec cleans up."""

    def test_fail_cleans_worktree(self):
        """cleanup_spec_worktree works when called after a spec failure."""
        repo_dir = os.path.join(self.boi_state, "repo")
        _init_temp_git_repo(repo_dir)

        spec_path = self.create_spec(tasks_pending=1)
        self.create_queue_entry(
            queue_id="q-002",
            spec_path=spec_path,
            status="running",
        )

        worktrees_dir = os.path.join(self.boi_state, "worktrees")
        with patch("lib.queue.WORKTREES_DIR", worktrees_dir):
            wt_result = create_spec_worktree(self.queue_dir, "q-002", repo_dir)

        wt_path = wt_result["worktree_path"]
        self.assertTrue(os.path.isdir(wt_path))

        # Cleanup on fail
        cleaned = cleanup_spec_worktree(self.queue_dir, "q-002")
        self.assertTrue(cleaned)
        self.assertFalse(os.path.isdir(wt_path))

    def test_cleanup_returns_false_for_no_worktree(self):
        """cleanup_spec_worktree returns False when no worktree exists."""
        self.create_queue_entry(
            queue_id="q-003",
            spec_path="/tmp/spec.md",
            status="failed",
        )
        cleaned = cleanup_spec_worktree(self.queue_dir, "q-003")
        self.assertFalse(cleaned)


# ─── Conflict Detector Tests ────────────────────────────────────────────────


class TestConflictDetectorFindsOverlap(BoiTestCase):
    """test_conflict_detector_finds_overlap — two specs touching same file are detected."""

    def test_conflict_detector_finds_overlap(self):
        """detect_conflicts finds specs with overlapping file paths."""
        spec_content_a = textwrap.dedent("""\
            # Spec A
            ## Tasks
            ### t-1: Update main
            PENDING
            **Spec:** Modify `~/project/main.py` and `~/project/utils.py`.
            **Verify:** true
        """)
        spec_content_b = textwrap.dedent("""\
            # Spec B
            ## Tasks
            ### t-1: Also update main
            PENDING
            **Spec:** Modify `~/project/main.py` for different feature.
            **Verify:** true
        """)

        spec_a = self.create_spec(content=spec_content_a, filename="spec_a.md")
        spec_b = self.create_spec(content=spec_content_b, filename="spec_b.md")

        self.create_queue_entry(
            queue_id="q-001",
            spec_path=spec_a,
            status="running",
        )
        self.create_queue_entry(
            queue_id="q-002",
            spec_path=spec_b,
            status="queued",
        )

        conflicts = detect_conflicts(self.queue_dir)
        self.assertEqual(len(conflicts), 1)
        self.assertEqual(conflicts[0]["spec_a"], "q-001")
        self.assertEqual(conflicts[0]["spec_b"], "q-002")

        # The shared file is main.py (normalized path)
        shared = conflicts[0]["shared_files"]
        self.assertTrue(
            any("main.py" in f for f in shared),
            f"Expected main.py in shared files: {shared}",
        )


class TestConflictDetectorNoFalsePositive(BoiTestCase):
    """test_conflict_detector_no_false_positive — unrelated specs are not blocked."""

    def test_conflict_detector_no_false_positive(self):
        """Specs that reference different files don't conflict."""
        spec_content_a = textwrap.dedent("""\
            # Spec A
            ## Tasks
            ### t-1: Update frontend
            PENDING
            **Spec:** Modify `~/project/frontend/app.js`.
            **Verify:** true
        """)
        spec_content_b = textwrap.dedent("""\
            # Spec B
            ## Tasks
            ### t-1: Update backend
            PENDING
            **Spec:** Modify `~/project/backend/server.py`.
            **Verify:** true
        """)

        spec_a = self.create_spec(content=spec_content_a, filename="spec_a.md")
        spec_b = self.create_spec(content=spec_content_b, filename="spec_b.md")

        self.create_queue_entry(
            queue_id="q-001",
            spec_path=spec_a,
            status="running",
        )
        self.create_queue_entry(
            queue_id="q-002",
            spec_path=spec_b,
            status="queued",
        )

        conflicts = detect_conflicts(self.queue_dir)
        self.assertEqual(len(conflicts), 0)

        blockers = should_block(self.queue_dir, "q-002")
        self.assertEqual(len(blockers), 0)


class TestAutoBlockOnDispatch(BoiTestCase):
    """test_auto_block_on_dispatch — dispatch auto-adds blocked_by for conflicting specs."""

    def test_auto_block_on_dispatch(self):
        """should_block returns conflicting running spec IDs."""
        spec_content_a = textwrap.dedent("""\
            # Spec A
            ## Tasks
            ### t-1: Edit boi.sh
            PENDING
            **Spec:** Update `~/boi-oss/boi.sh` with new command.
            **Verify:** true
        """)
        spec_content_b = textwrap.dedent("""\
            # Spec B
            ## Tasks
            ### t-1: Also edit boi.sh
            PENDING
            **Spec:** Update `~/boi-oss/boi.sh` with another command.
            **Verify:** true
        """)

        spec_a = self.create_spec(content=spec_content_a, filename="spec_a.md")
        spec_b = self.create_spec(content=spec_content_b, filename="spec_b.md")

        # q-001 is running
        self.create_queue_entry(
            queue_id="q-001",
            spec_path=spec_a,
            status="running",
        )
        # q-002 is newly dispatched
        self.create_queue_entry(
            queue_id="q-002",
            spec_path=spec_b,
            status="queued",
        )

        blockers = should_block(self.queue_dir, "q-002")
        self.assertEqual(len(blockers), 1)
        self.assertEqual(blockers[0]["blocking_id"], "q-001")
        self.assertTrue(
            any("boi.sh" in f for f in blockers[0]["shared_files"]),
            f"Expected boi.sh in shared files: {blockers[0]['shared_files']}",
        )


# ─── Max Concurrent Worktrees Test ──────────────────────────────────────────


class TestMaxConcurrentWorktreesRespected(BoiTestCase):
    """test_max_concurrent_worktrees_respected — respects worktree limit."""

    def test_max_concurrent_worktrees_respected(self):
        """count_active_worktrees counts only active worktree-isolated specs."""
        spec_path = self.create_spec(tasks_pending=1)

        # Create 3 active isolated specs
        for i in range(1, 4):
            self.create_queue_entry(
                queue_id=f"q-{i:03d}",
                spec_path=spec_path,
                status="running",
                worktree_isolate=True,
                worktree_path=f"/tmp/worktrees/q-{i:03d}",
            )

        # Create 1 completed isolated spec (should not count)
        self.create_queue_entry(
            queue_id="q-004",
            spec_path=spec_path,
            status="completed",
            worktree_isolate=True,
            worktree_path="/tmp/worktrees/q-004",
        )

        # Create 1 non-isolated running spec (should not count)
        self.create_queue_entry(
            queue_id="q-005",
            spec_path=spec_path,
            status="running",
        )

        count = count_active_worktrees(self.queue_dir)
        self.assertEqual(count, 3)

    def test_pick_next_spec_respects_max_worktrees(self):
        """pick_next_spec returns None when max worktrees reached."""
        from lib.daemon_ops import pick_next_spec

        spec_path = self.create_spec(tasks_pending=1)

        # Fill up to max (2 for this test)
        for i in range(1, 3):
            self.create_queue_entry(
                queue_id=f"q-{i:03d}",
                spec_path=spec_path,
                status="running",
                worktree_isolate=True,
                worktree_path=f"/tmp/worktrees/q-{i:03d}",
            )

        # Add an isolated spec waiting
        self.create_queue_entry(
            queue_id="q-003",
            spec_path=spec_path,
            status="queued",
            worktree_isolate=True,
        )

        result = pick_next_spec(self.queue_dir, for_worktree=True, max_worktrees=2)
        self.assertIsNone(result)


# ─── Merge Command Tests ────────────────────────────────────────────────────


class TestMergeCommandResolves(BoiTestCase):
    """test_merge_command_resolves — boi merge completes successfully."""

    def test_merge_command_resolves(self):
        """merge_spec_worktree succeeds for a branch with no conflicts."""
        repo_dir = os.path.join(self.boi_state, "repo")
        _init_temp_git_repo(repo_dir)

        spec_path = self.create_spec(tasks_pending=1)
        self.create_queue_entry(
            queue_id="q-001",
            spec_path=spec_path,
            status="completed",
        )

        worktrees_dir = os.path.join(self.boi_state, "worktrees")
        with patch("lib.queue.WORKTREES_DIR", worktrees_dir):
            wt_result = create_spec_worktree(self.queue_dir, "q-001", repo_dir)

        # Add a non-conflicting change
        new_file = os.path.join(wt_result["worktree_path"], "new_feature.py")
        Path(new_file).write_text("def feature(): pass\n", encoding="utf-8")
        subprocess.run(
            ["git", "add", "new_feature.py"],
            cwd=wt_result["worktree_path"],
            capture_output=True,
            text=True,
        )
        subprocess.run(
            ["git", "commit", "-m", "Add new_feature.py"],
            cwd=wt_result["worktree_path"],
            capture_output=True,
            text=True,
            check=True,
        )

        # Merge
        merge_result = merge_spec_worktree(self.queue_dir, "q-001")
        self.assertEqual(merge_result["merge_status"], "merged")

        # Verify file is on main
        self.assertTrue(os.path.isfile(os.path.join(repo_dir, "new_feature.py")))


class TestMergeAbortCleansUp(BoiTestCase):
    """test_merge_abort_cleans_up — boi merge --abort removes everything."""

    def test_merge_abort_cleans_up(self):
        """cleanup_spec_worktree (abort path) removes worktree and branch completely."""
        repo_dir = os.path.join(self.boi_state, "repo")
        _init_temp_git_repo(repo_dir)

        spec_path = self.create_spec(tasks_pending=1)
        self.create_queue_entry(
            queue_id="q-001",
            spec_path=spec_path,
            status="needs_merge",
        )

        worktrees_dir = os.path.join(self.boi_state, "worktrees")
        with patch("lib.queue.WORKTREES_DIR", worktrees_dir):
            wt_result = create_spec_worktree(self.queue_dir, "q-001", repo_dir)

        wt_path = wt_result["worktree_path"]
        self.assertTrue(os.path.isdir(wt_path))

        # Abort (cleanup without merge)
        cleaned = cleanup_spec_worktree(self.queue_dir, "q-001")
        self.assertTrue(cleaned)

        # Directory gone
        self.assertFalse(os.path.isdir(wt_path))

        # Branch gone
        result = subprocess.run(
            ["git", "branch", "--list", "boi/q-001"],
            cwd=repo_dir,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.stdout.strip(), "")

        # Queue entry cleared
        entry = get_entry(self.queue_dir, "q-001")
        self.assertIsNone(entry["worktree_path"])
        self.assertIsNone(entry["worktree_branch"])
        self.assertIsNone(entry["merge_status"])


# ─── Path Extraction Tests ──────────────────────────────────────────────────


class TestPathExtraction(BoiTestCase):
    """Additional tests for extract_target_paths."""

    def test_extracts_backtick_paths(self):
        """extract_target_paths finds file paths in backticks within Spec: sections."""
        spec_content = textwrap.dedent("""\
            # My Spec
            ## Tasks
            ### t-1: Do stuff
            PENDING
            **Spec:** Update `~/boi-oss/lib/queue.py` and `~/boi-oss/boi.sh`.
            **Verify:** true
        """)
        spec_path = self.create_spec(content=spec_content, filename="paths.md")
        paths = extract_target_paths(spec_path)
        normalized = {os.path.basename(p) for p in paths}
        self.assertIn("queue.py", normalized)
        self.assertIn("boi.sh", normalized)

    def test_extracts_files_section_paths(self):
        """extract_target_paths finds paths in Files: sections."""
        spec_content = textwrap.dedent("""\
            # My Spec
            ## Tasks
            ### t-1: Do stuff
            PENDING
            **Files:**
            - ~/boi-oss/lib/daemon_ops.py
            - ~/boi-oss/tests/test_daemon.py
            **Verify:** true
        """)
        spec_path = self.create_spec(content=spec_content, filename="files.md")
        paths = extract_target_paths(spec_path)
        normalized = {os.path.basename(p) for p in paths}
        self.assertIn("daemon_ops.py", normalized)
        self.assertIn("test_daemon.py", normalized)

    def test_returns_empty_for_missing_file(self):
        """extract_target_paths returns empty set for nonexistent spec."""
        paths = extract_target_paths("/nonexistent/spec.md")
        self.assertEqual(len(paths), 0)


# ─── Conflict Re-check Before Dequeue Tests ─────────────────────────────────


class TestConflictRecheckBeforeDequeue(BoiTestCase):
    """Tests for check_conflicts_before_dequeue."""

    def test_recheck_finds_new_conflict(self):
        """A spec that was unblocked at dispatch can become blocked later."""
        spec_content_a = textwrap.dedent("""\
            # Spec A
            ## Tasks
            ### t-1: Edit config
            PENDING
            **Spec:** Modify `~/project/config.json`.
            **Verify:** true
        """)
        spec_content_b = textwrap.dedent("""\
            # Spec B
            ## Tasks
            ### t-1: Also edit config
            PENDING
            **Spec:** Modify `~/project/config.json`.
            **Verify:** true
        """)

        spec_a = self.create_spec(content=spec_content_a, filename="a.md")
        spec_b = self.create_spec(content=spec_content_b, filename="b.md")

        # q-001 started running after q-002 was dispatched
        self.create_queue_entry(
            queue_id="q-001",
            spec_path=spec_a,
            status="running",
        )
        self.create_queue_entry(
            queue_id="q-002",
            spec_path=spec_b,
            status="queued",
        )

        conflicting = check_conflicts_before_dequeue(self.queue_dir, "q-002")
        self.assertIn("q-001", conflicting)

    def test_recheck_no_conflict_for_isolated(self):
        """Isolated specs are never blocked by conflict detection."""
        spec_content = textwrap.dedent("""\
            # Spec
            ## Tasks
            ### t-1: Edit config
            PENDING
            **Spec:** Modify `~/project/config.json`.
            **Verify:** true
        """)

        spec = self.create_spec(content=spec_content, filename="iso.md")

        self.create_queue_entry(
            queue_id="q-001",
            spec_path=spec,
            status="running",
        )
        self.create_queue_entry(
            queue_id="q-002",
            spec_path=spec,
            status="queued",
            worktree_isolate=True,
        )

        conflicting = check_conflicts_before_dequeue(self.queue_dir, "q-002")
        self.assertEqual(len(conflicting), 0)


if __name__ == "__main__":
    unittest.main()
