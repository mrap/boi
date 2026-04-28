# test_workspace_boundary.py — Tests for worktree boundary checking.
#
# Tests:
#   - WorkspaceBoundaryChecker: clean worker (worktree only) produces no leak
#   - WorkspaceBoundaryChecker: worker writes to main repo produces leak list
#   - diff_status helper correctly identifies new dirty files
#   - snapshot_git_status returns current porcelain lines
#   - emit_leak_event gracefully skips when hex_emit.py is absent
#   - get_main_repo returns first worktree entry
#
# All tests use temp git repos. No hex-events dependency required.

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

BOI_ROOT = str(Path(__file__).resolve().parent.parent)
sys.path.insert(0, BOI_ROOT)

from lib.workspace_guard import (
    WorkspaceBoundaryChecker,
    diff_status,
    emit_leak_event,
    get_main_repo,
    snapshot_git_status,
)


def _git(*args, cwd: str) -> None:
    """Run a git command in cwd, raising on failure."""
    subprocess.run(
        ["git"] + list(args),
        cwd=cwd,
        check=True,
        capture_output=True,
    )


def _make_git_repo(tmpdir: str) -> str:
    """Create a minimal git repo and return its path."""
    repo = os.path.join(tmpdir, "main_repo")
    os.makedirs(repo)
    _git("init", cwd=repo)
    _git("config", "user.email", "test@test.com", cwd=repo)
    _git("config", "user.name", "Test", cwd=repo)
    # Initial commit so worktrees can be created
    readme = os.path.join(repo, "README.md")
    with open(readme, "w") as f:
        f.write("test repo\n")
    _git("add", "README.md", cwd=repo)
    _git("commit", "-m", "init", cwd=repo)
    return repo


def _make_worktree(main_repo: str, tmpdir: str, name: str = "wt") -> str:
    """Create a linked worktree for main_repo and return its path."""
    wt_path = os.path.join(tmpdir, name)
    _git("worktree", "add", wt_path, cwd=main_repo)
    return wt_path


class TestDiffStatus(unittest.TestCase):
    """Unit tests for the diff_status helper."""

    def test_no_new_lines_returns_empty(self):
        pre = {" M foo.py", "?? bar.txt"}
        post = {" M foo.py", "?? bar.txt"}
        self.assertEqual(diff_status(pre, post), [])

    def test_new_line_returns_filename(self):
        pre = set()
        post = {" M src/main.py"}
        self.assertEqual(diff_status(pre, post), ["src/main.py"])

    def test_multiple_new_lines_sorted(self):
        pre = set()
        post = {" M b.py", " M a.py", "?? c.txt"}
        result = diff_status(pre, post)
        self.assertEqual(result, sorted(["a.py", "b.py", "c.txt"]))

    def test_removed_lines_not_reported(self):
        pre = {" M old.py"}
        post = set()
        self.assertEqual(diff_status(pre, post), [])

    def test_handles_unknown_format(self):
        pre = set()
        post = {"WEIRDLINE"}
        result = diff_status(pre, post)
        # Falls back to the whole line when can't split
        self.assertIn("WEIRDLINE", result)


class TestSnapshotGitStatus(unittest.TestCase):
    """Tests for snapshot_git_status against real temp repos."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()

    def tearDown(self):
        import shutil
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_clean_repo_returns_empty_set(self):
        repo = _make_git_repo(self.tmpdir)
        status = snapshot_git_status(repo)
        self.assertIsInstance(status, set)
        self.assertEqual(len(status), 0)

    def test_dirty_repo_returns_nonempty_set(self):
        repo = _make_git_repo(self.tmpdir)
        with open(os.path.join(repo, "new_file.txt"), "w") as f:
            f.write("dirty\n")
        status = snapshot_git_status(repo)
        self.assertGreater(len(status), 0)

    def test_invalid_path_returns_empty_set(self):
        status = snapshot_git_status("/nonexistent/path/12345")
        self.assertEqual(status, set())


class TestGetMainRepo(unittest.TestCase):
    """Tests for get_main_repo."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()

    def tearDown(self):
        import shutil
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_main_repo_returns_itself(self):
        repo = _make_git_repo(self.tmpdir)
        result = get_main_repo(repo)
        self.assertIsNotNone(result)
        self.assertEqual(os.path.realpath(result), os.path.realpath(repo))

    def test_linked_worktree_returns_main_repo(self):
        repo = _make_git_repo(self.tmpdir)
        wt = _make_worktree(repo, self.tmpdir)
        result = get_main_repo(wt)
        self.assertIsNotNone(result)
        self.assertEqual(os.path.realpath(result), os.path.realpath(repo))

    def test_invalid_path_returns_none(self):
        result = get_main_repo("/nonexistent/12345")
        self.assertIsNone(result)


class TestEmitLeakEvent(unittest.TestCase):
    """Tests for emit_leak_event graceful degradation."""

    def test_skips_silently_when_hex_emit_absent(self):
        """No exception raised when hex_emit.py does not exist."""
        with patch("lib.workspace_guard.HEX_EMIT_PATH", "/nonexistent/hex_emit.py"):
            # Should not raise
            emit_leak_event("q-001", "worker-1", ["leaked.py"], "/wt")

    def test_calls_hex_emit_when_present(self):
        with patch("lib.workspace_guard.os.path.isfile", return_value=True), \
             patch("lib.workspace_guard.subprocess.run") as mock_run:
            mock_run.return_value = MagicMock(returncode=0)
            emit_leak_event("q-001", "worker-1", ["leaked.py"], "/wt")
            mock_run.assert_called_once()
            call_args = mock_run.call_args[0][0]
            self.assertIn("boi.workspace.leak", call_args)
            # Verify payload contains expected keys
            payload_str = call_args[call_args.index("boi.workspace.leak") + 1]
            payload = json.loads(payload_str)
            self.assertEqual(payload["spec_id"], "q-001")
            self.assertEqual(payload["leaked_files"], ["leaked.py"])


class TestWorkspaceBoundaryChecker(unittest.TestCase):
    """Tests for WorkspaceBoundaryChecker against real temp repos."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.main_repo = _make_git_repo(self.tmpdir)
        self.worktree = _make_worktree(self.main_repo, self.tmpdir)

    def tearDown(self):
        import shutil
        # Remove linked worktree first to avoid git errors
        try:
            _git(
                "worktree", "remove", "--force", self.worktree,
                cwd=self.main_repo,
            )
        except Exception:
            pass
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def _write_file(self, repo_path: str, name: str, content: str = "x\n") -> str:
        path = os.path.join(repo_path, name)
        with open(path, "w") as f:
            f.write(content)
        return path

    def test_clean_worker_no_leak(self):
        """Worker writes only to worktree → no leak detected."""
        checker = WorkspaceBoundaryChecker(
            worktree_path=self.worktree,
            spec_id="q-test",
            worker_id="w-1",
        )
        checker.snapshot_before()

        # Worker writes ONLY to its worktree
        self._write_file(self.worktree, "output.txt")

        with patch("lib.workspace_guard.emit_leak_event") as mock_emit:
            leaked = checker.check_after()

        self.assertEqual(leaked, [])
        mock_emit.assert_not_called()

    def test_leaking_worker_detected(self):
        """Worker writes to main repo → leak detected and event emitted."""
        checker = WorkspaceBoundaryChecker(
            worktree_path=self.worktree,
            spec_id="q-test",
            worker_id="w-1",
        )
        checker.snapshot_before()

        # Worker writes to the MAIN repo (violation)
        self._write_file(self.main_repo, "leaked_file.txt")

        with patch("lib.workspace_guard.emit_leak_event") as mock_emit:
            leaked = checker.check_after()

        self.assertEqual(len(leaked), 1)
        self.assertIn("leaked_file.txt", leaked[0])
        mock_emit.assert_called_once()
        call_kwargs = mock_emit.call_args
        self.assertEqual(call_kwargs[1]["spec_id"], "q-test")
        self.assertEqual(call_kwargs[1]["worker_id"], "w-1")
        self.assertEqual(len(call_kwargs[1]["leaked_files"]), 1)

    def test_both_worktree_and_main_repo_writes(self):
        """Worker writes to both worktree and main repo → only main repo leak flagged."""
        checker = WorkspaceBoundaryChecker(
            worktree_path=self.worktree,
            spec_id="q-test",
            worker_id="w-2",
        )
        checker.snapshot_before()

        # Legitimate write to worktree
        self._write_file(self.worktree, "legit_output.txt")
        # Illegal write to main repo
        self._write_file(self.main_repo, "bad_file.py")

        with patch("lib.workspace_guard.emit_leak_event") as mock_emit:
            leaked = checker.check_after()

        self.assertEqual(len(leaked), 1)
        self.assertIn("bad_file.py", leaked[0])
        mock_emit.assert_called_once()

    def test_no_snapshot_before_check_after_returns_empty(self):
        """If snapshot_before was never called, check_after returns empty."""
        checker = WorkspaceBoundaryChecker(
            worktree_path=self.worktree,
            spec_id="q-test",
            worker_id="w-3",
        )
        # Do NOT call snapshot_before()
        with patch("lib.workspace_guard.emit_leak_event") as mock_emit:
            leaked = checker.check_after()
        self.assertEqual(leaked, [])
        mock_emit.assert_not_called()

    def test_in_place_worker_skips_check(self):
        """When worktree IS main repo (in-place), boundary check is disabled."""
        checker = WorkspaceBoundaryChecker(
            worktree_path=self.main_repo,  # same as main repo
            spec_id="q-test",
            worker_id="w-4",
        )
        checker.snapshot_before()
        # Even if main repo gets written to, no leak reported (check disabled)
        self._write_file(self.main_repo, "in_place_file.txt")
        with patch("lib.workspace_guard.emit_leak_event") as mock_emit:
            leaked = checker.check_after()
        self.assertEqual(leaked, [])
        mock_emit.assert_not_called()

    def test_preexisting_dirty_files_not_flagged(self):
        """Files dirty before worker started are not counted as leaks."""
        # Make the main repo already dirty before the worker runs
        self._write_file(self.main_repo, "preexisting_dirty.txt")

        checker = WorkspaceBoundaryChecker(
            worktree_path=self.worktree,
            spec_id="q-test",
            worker_id="w-5",
        )
        checker.snapshot_before()

        # Worker writes something NEW to the main repo
        self._write_file(self.main_repo, "new_leak.py")

        with patch("lib.workspace_guard.emit_leak_event") as mock_emit:
            leaked = checker.check_after()

        # Only the new file should be flagged
        self.assertEqual(len(leaked), 1)
        self.assertIn("new_leak.py", leaked[0])


if __name__ == "__main__":
    unittest.main()
