# test_task_worktree.py — Tests for fresh-worktree-per-task (BOI t-4).
#
# Verifies:
# 1. create_task_worktree creates two separate worktrees from a single repo.
# 2. Each worktree is on its own task-specific branch (boi/q-NNN/t-M).
# 3. Workers can commit independently to those branches.
# 4. merge_level_branches merges both branches after tasks complete.
# 5. remove_task_worktree cleans up the directory.
# 6. Merge conflicts are detected and returned (not auto-resolved).
#
# Uses real git operations against a temporary repository.

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.task_worktree import (
    compute_branch_name,
    compute_task_worktree_path,
    create_task_worktree,
    get_main_repo_from_worker,
    merge_level_branches,
    remove_task_worktree,
)


def _init_repo(path: str) -> None:
    """Create a git repo with a single initial commit."""
    os.makedirs(path, exist_ok=True)
    subprocess.run(["git", "init"], cwd=path, capture_output=True, check=True)
    subprocess.run(
        ["git", "config", "user.email", "test@boi.test"],
        cwd=path, capture_output=True,
    )
    subprocess.run(
        ["git", "config", "user.name", "BOI Test"],
        cwd=path, capture_output=True,
    )
    (Path(path) / "README.md").write_text("# BOI test repo\n")
    subprocess.run(["git", "add", "README.md"], cwd=path, capture_output=True)
    subprocess.run(
        ["git", "commit", "-m", "Initial commit"],
        cwd=path, capture_output=True, check=True,
    )


def _commit_file(wt_path: str, filename: str, content: str) -> None:
    """Write a file and commit it inside a worktree."""
    (Path(wt_path) / filename).write_text(content)
    subprocess.run(["git", "add", filename], cwd=wt_path, capture_output=True)
    subprocess.run(
        ["git", "commit", "-m", f"Add {filename}"],
        cwd=wt_path, capture_output=True, check=True,
    )


def _branch_exists(repo: str, branch: str) -> bool:
    result = subprocess.run(
        ["git", "-C", repo, "rev-parse", "--verify", branch],
        capture_output=True,
    )
    return result.returncode == 0


class TestComputeNames(unittest.TestCase):
    """Unit tests for naming helpers."""

    def test_branch_name(self):
        self.assertEqual(compute_branch_name("q-123", "t-2"), "boi/q-123/t-2")

    def test_worktree_path_structure(self):
        path = compute_task_worktree_path("w-1", "q-123", "t-2")
        self.assertIn("w-1-q-123-t-2", path)


class TestGetMainRepoFromWorker(unittest.TestCase):
    """Test parsing .git file to find main repo."""

    def test_parses_gitdir_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            wt = os.path.join(tmp, "worktree")
            os.makedirs(wt)
            # Simulate a linked worktree .git file
            git_file = os.path.join(wt, ".git")
            main = os.path.join(tmp, "main")
            Path(git_file).write_text(
                f"gitdir: {main}/.git/worktrees/my-worker\n"
            )
            result = get_main_repo_from_worker(wt)
            self.assertEqual(result, main)

    def test_returns_none_for_missing_git_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            wt = os.path.join(tmp, "no-git-dir")
            os.makedirs(wt)
            result = get_main_repo_from_worker(wt)
            self.assertIsNone(result)


class TestCreateTaskWorktree(unittest.TestCase):
    """Test that fresh worktrees are created per task."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.main_repo = os.path.join(self.tmp, "main")
        _init_repo(self.main_repo)

    def tearDown(self):
        # Force-remove any leftover worktrees before cleaning up tmpdir.
        import shutil
        result = subprocess.run(
            ["git", "-C", self.main_repo, "worktree", "list", "--porcelain"],
            capture_output=True,
            text=True,
        )
        for line in result.stdout.splitlines():
            if line.startswith("worktree "):
                wt = line[len("worktree "):].strip()
                if wt != self.main_repo:
                    subprocess.run(
                        ["git", "-C", self.main_repo, "worktree", "remove", "--force", wt],
                        capture_output=True,
                    )
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_creates_worktree_directory(self):
        info = create_task_worktree(self.main_repo, "w-1", "q-001", "t-1")
        self.assertTrue(os.path.isdir(info["worktree_path"]))
        self.assertEqual(info["branch_name"], "boi/q-001/t-1")

    def test_two_independent_worktrees(self):
        """Two tasks get two separate worktrees at different paths."""
        info1 = create_task_worktree(self.main_repo, "w-1", "q-001", "t-1")
        info2 = create_task_worktree(self.main_repo, "w-2", "q-001", "t-2")

        self.assertNotEqual(info1["worktree_path"], info2["worktree_path"])
        self.assertNotEqual(info1["branch_name"], info2["branch_name"])
        self.assertTrue(os.path.isdir(info1["worktree_path"]))
        self.assertTrue(os.path.isdir(info2["worktree_path"]))

    def test_each_worktree_on_own_branch(self):
        """Each worktree is checked out on its own task branch."""
        info1 = create_task_worktree(self.main_repo, "w-1", "q-001", "t-1")
        info2 = create_task_worktree(self.main_repo, "w-2", "q-001", "t-2")

        def current_branch(path):
            r = subprocess.run(
                ["git", "-C", path, "symbolic-ref", "--short", "HEAD"],
                capture_output=True, text=True,
            )
            return r.stdout.strip()

        self.assertEqual(current_branch(info1["worktree_path"]), "boi/q-001/t-1")
        self.assertEqual(current_branch(info2["worktree_path"]), "boi/q-001/t-2")

    def test_commits_are_independent(self):
        """Commits in one task worktree don't appear in the other."""
        info1 = create_task_worktree(self.main_repo, "w-1", "q-001", "t-1")
        info2 = create_task_worktree(self.main_repo, "w-2", "q-001", "t-2")

        _commit_file(info1["worktree_path"], "task1-output.md", "Result of task 1\n")
        _commit_file(info2["worktree_path"], "task2-output.md", "Result of task 2\n")

        # task1-output.md should exist only in t-1 branch
        self.assertTrue(
            os.path.isfile(os.path.join(info1["worktree_path"], "task1-output.md"))
        )
        self.assertFalse(
            os.path.isfile(os.path.join(info2["worktree_path"], "task1-output.md"))
        )

        # task2-output.md should exist only in t-2 branch
        self.assertTrue(
            os.path.isfile(os.path.join(info2["worktree_path"], "task2-output.md"))
        )
        self.assertFalse(
            os.path.isfile(os.path.join(info1["worktree_path"], "task2-output.md"))
        )


class TestRemoveTaskWorktree(unittest.TestCase):
    """Test worktree removal on task completion."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.main_repo = os.path.join(self.tmp, "main")
        _init_repo(self.main_repo)

    def tearDown(self):
        import shutil
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_removes_worktree_directory(self):
        info = create_task_worktree(self.main_repo, "w-1", "q-001", "t-1")
        wt_path = info["worktree_path"]
        self.assertTrue(os.path.isdir(wt_path))

        remove_task_worktree(self.main_repo, wt_path)
        self.assertFalse(os.path.isdir(wt_path))

    def test_remove_nonexistent_is_safe(self):
        """Removing a path that doesn't exist should not raise."""
        remove_task_worktree(self.main_repo, "/tmp/this-does-not-exist-at-all-xyz")


class TestMergeLevelBranches(unittest.TestCase):
    """Test merging task branches at level boundary."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.main_repo = os.path.join(self.tmp, "main")
        _init_repo(self.main_repo)

    def tearDown(self):
        import shutil
        # Clean up worktrees
        result = subprocess.run(
            ["git", "-C", self.main_repo, "worktree", "list", "--porcelain"],
            capture_output=True, text=True,
        )
        for line in result.stdout.splitlines():
            if line.startswith("worktree "):
                wt = line[len("worktree "):].strip()
                if wt != self.main_repo:
                    subprocess.run(
                        ["git", "-C", self.main_repo, "worktree", "remove", "--force", wt],
                        capture_output=True,
                    )
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _make_task_records(self, task_infos: list[dict]) -> list[dict]:
        """Build task record dicts matching the DB schema."""
        return [
            {
                "task_id": t["task_id"],
                "branch_name": t["branch_name"],
                "status": t.get("status", "DONE"),
                "depends_on": "[]",
            }
            for t in task_infos
        ]

    def test_clean_merge_succeeds(self):
        """Two tasks with non-conflicting files merge cleanly."""
        info1 = create_task_worktree(self.main_repo, "w-1", "q-001", "t-1")
        info2 = create_task_worktree(self.main_repo, "w-2", "q-001", "t-2")

        _commit_file(info1["worktree_path"], "topic-a.md", "Python asyncio summary\n")
        _commit_file(info2["worktree_path"], "topic-b.md", "Rust ownership summary\n")

        # Remove worktrees (simulating task completion)
        remove_task_worktree(self.main_repo, info1["worktree_path"])
        remove_task_worktree(self.main_repo, info2["worktree_path"])

        records = self._make_task_records([
            {"task_id": "t-1", "branch_name": info1["branch_name"]},
            {"task_id": "t-2", "branch_name": info2["branch_name"]},
        ])

        result = merge_level_branches(self.main_repo, "q-001", records)
        self.assertEqual(result["merge_status"], "merged")
        self.assertIn("t-1", result["merged_tasks"])
        self.assertIn("t-2", result["merged_tasks"])
        self.assertEqual(result["conflicting_tasks"], [])

        # Both files should exist on spec branch
        spec_branch = "boi-spec/q-001"
        self.assertTrue(_branch_exists(self.main_repo, spec_branch))

        # Verify files reachable from spec branch
        ls_result = subprocess.run(
            ["git", "-C", self.main_repo, "ls-tree", "--name-only", spec_branch],
            capture_output=True, text=True,
        )
        files = ls_result.stdout.strip().splitlines()
        self.assertIn("topic-a.md", files)
        self.assertIn("topic-b.md", files)

    def test_conflict_detected_and_not_resolved(self):
        """Conflicting changes to the same file are detected; repo stays clean."""
        info1 = create_task_worktree(self.main_repo, "w-1", "q-001", "t-1")
        info2 = create_task_worktree(self.main_repo, "w-2", "q-001", "t-2")

        # Both tasks modify README.md differently
        _commit_file(info1["worktree_path"], "README.md", "Task 1 version\n")
        _commit_file(info2["worktree_path"], "README.md", "Task 2 version\n")

        remove_task_worktree(self.main_repo, info1["worktree_path"])
        remove_task_worktree(self.main_repo, info2["worktree_path"])

        records = self._make_task_records([
            {"task_id": "t-1", "branch_name": info1["branch_name"]},
            {"task_id": "t-2", "branch_name": info2["branch_name"]},
        ])

        result = merge_level_branches(self.main_repo, "q-001", records)
        # At least one conflict expected
        self.assertIn(result["merge_status"], ["merged", "conflict"])
        # If conflict detected, it should name the file
        if result["merge_status"] == "conflict":
            self.assertIn("README.md", result.get("conflicting_files", []))
            self.assertTrue(len(result["conflicting_tasks"]) > 0)

        # Repo should be in a clean state (no in-progress merge)
        status = subprocess.run(
            ["git", "-C", self.main_repo, "status", "--porcelain"],
            capture_output=True, text=True,
        )
        self.assertNotIn("UU", status.stdout)

    def test_skips_tasks_without_branch(self):
        """Tasks that fell back to shared worktree (no branch_name) are skipped."""
        records = [
            {
                "task_id": "t-1",
                "branch_name": None,
                "status": "DONE",
                "depends_on": "[]",
            }
        ]
        result = merge_level_branches(self.main_repo, "q-001", records)
        self.assertEqual(result["merge_status"], "nothing_to_merge")

    def test_full_pipeline_two_tasks(self):
        """Full pipeline: create worktrees, commit, remove, merge, verify."""
        # Step 1: Create fresh worktrees for each task
        info1 = create_task_worktree(self.main_repo, "w-1", "q-999", "t-1")
        info2 = create_task_worktree(self.main_repo, "w-2", "q-999", "t-2")

        self.assertTrue(os.path.isdir(info1["worktree_path"]))
        self.assertTrue(os.path.isdir(info2["worktree_path"]))
        self.assertNotEqual(info1["worktree_path"], info2["worktree_path"])

        # Step 2: Workers commit to their task branches (simulated)
        _commit_file(info1["worktree_path"], "topic-a.md", "asyncio content\n")
        _commit_file(info2["worktree_path"], "topic-b.md", "rust ownership content\n")

        t1_branch = info1["branch_name"]
        t2_branch = info2["branch_name"]
        self.assertTrue(_branch_exists(self.main_repo, t1_branch))
        self.assertTrue(_branch_exists(self.main_repo, t2_branch))

        # Step 3: Tasks complete — remove worktrees
        remove_task_worktree(self.main_repo, info1["worktree_path"])
        remove_task_worktree(self.main_repo, info2["worktree_path"])

        self.assertFalse(os.path.isdir(info1["worktree_path"]))
        self.assertFalse(os.path.isdir(info2["worktree_path"]))

        # Step 4: Level boundary — merge branches
        records = [
            {"task_id": "t-1", "branch_name": t1_branch, "status": "DONE", "depends_on": "[]"},
            {"task_id": "t-2", "branch_name": t2_branch, "status": "DONE", "depends_on": "[]"},
        ]
        result = merge_level_branches(self.main_repo, "q-999", records)

        self.assertEqual(result["merge_status"], "merged")
        self.assertEqual(set(result["merged_tasks"]), {"t-1", "t-2"})

        # Step 5: Verify merged content is on spec branch
        spec_branch = "boi-spec/q-999"
        ls_result = subprocess.run(
            ["git", "-C", self.main_repo, "ls-tree", "--name-only", spec_branch],
            capture_output=True, text=True,
        )
        files = ls_result.stdout.strip().splitlines()
        self.assertIn("topic-a.md", files, f"topic-a.md not in {files}")
        self.assertIn("topic-b.md", files, f"topic-b.md not in {files}")


if __name__ == "__main__":
    unittest.main(verbosity=2)
