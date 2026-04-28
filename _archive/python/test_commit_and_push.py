# test_commit_and_push.py — Unit tests for Daemon._commit_and_push_output.
#
# Verifies:
#   1. Commit and push are called when target_repo is valid and dirty
#   2. When target_repo is missing/empty, commit is skipped with a warning
#   3. When git commit fails, the spec still completes (no crash)
#   4. When git push fails, the spec still completes with a warning logged

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock, call, patch

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from daemon import Daemon


class CommitAndPushTestCase(unittest.TestCase):
    """Base: minimal Daemon instance with temp dirs."""

    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.state_dir = self._tmpdir.name
        self.db_path = os.path.join(self.state_dir, "boi.db")
        self.queue_dir = os.path.join(self.state_dir, "queue")
        self.log_dir = os.path.join(self.state_dir, "logs")
        os.makedirs(self.queue_dir, exist_ok=True)
        os.makedirs(self.log_dir, exist_ok=True)

        # Two workers (required by Daemon.__init__)
        wt_a = os.path.join(self.state_dir, "wt-a")
        wt_b = os.path.join(self.state_dir, "wt-b")
        os.makedirs(wt_a, exist_ok=True)
        os.makedirs(wt_b, exist_ok=True)

        config_path = os.path.join(self.state_dir, "config.json")
        with open(config_path, "w", encoding="utf-8") as fh:
            json.dump(
                {"workers": [
                    {"id": "w1", "worktree_path": wt_a},
                    {"id": "w2", "worktree_path": wt_b},
                ]},
                fh,
            )

        self.daemon = Daemon(
            config_path=config_path,
            db_path=self.db_path,
            state_dir=self.state_dir,
        )
        self.daemon.load_workers()

    def tearDown(self) -> None:
        self.daemon.db.close()
        self._tmpdir.cleanup()

    # ── helpers ────────────────────────────────────────────────────────

    def _make_run(self, stdout="", returncode=0):
        """Return a subprocess.run mock that succeeds with optional stdout."""
        result = MagicMock()
        result.stdout = stdout
        result.returncode = returncode
        return result

    def _make_failing_run(self):
        """Return a subprocess.run mock that raises CalledProcessError."""
        raise subprocess.CalledProcessError(1, "git")


# ─── Test 1: dirty repo → commit and push called ──────────────────────


class TestCommitAndPushHappyPath(CommitAndPushTestCase):

    @patch("daemon.subprocess.run")
    def test_dirty_repo_commits_and_pushes(self, mock_run: MagicMock) -> None:
        """When the repo is dirty and has a remote, commit and push are called."""
        spec_id = "q-test-001"
        target_repo = "/fake/repo"

        # Sequence of subprocess.run calls:
        #   1. git rev-parse --git-dir  → success (is a git repo)
        #   2. git status --porcelain   → dirty output
        #   3. git add -A               → success (no manifest)
        #   4. git commit -m ...        → success
        #   5. git remote -v            → has remote
        #   6. git push                 → success
        responses = [
            self._make_run(),             # rev-parse
            self._make_run("M foo.py"),   # status --porcelain
            self._make_run(),             # add -A
            self._make_run(),             # commit
            self._make_run("origin ..."), # remote -v
            self._make_run(),             # push
        ]
        mock_run.side_effect = responses

        # Should not raise
        self.daemon._commit_and_push_output(spec_id, target_repo)

        # git commit must have been called
        # cmd layout: ["git", "-C", repo, "commit", ...]
        commit_calls = [
            c for c in mock_run.call_args_list
            if c.args and len(c.args[0]) >= 4 and c.args[0][3] == "commit"
        ]
        self.assertTrue(commit_calls, "Expected git commit to be called")

        # git push must have been called
        push_calls = [
            c for c in mock_run.call_args_list
            if c.args and len(c.args[0]) >= 4 and c.args[0][3] == "push"
        ]
        self.assertTrue(push_calls, "Expected git push to be called")


# ─── Test 2: missing target_repo → skip with warning ──────────────────


class TestCommitAndPushNoTargetRepo(CommitAndPushTestCase):

    @patch("daemon.subprocess.run")
    def test_empty_target_repo_skips_commit(self, mock_run: MagicMock) -> None:
        """When target_repo is empty, no git commands are issued."""
        self.daemon._commit_and_push_output("q-test-002", "")

        mock_run.assert_not_called()

    @patch("daemon.subprocess.run")
    def test_none_target_repo_skips_commit(self, mock_run: MagicMock) -> None:
        """When target_repo is None (passed as empty string), no git commands issued."""
        # The method signature is str, so pass empty string
        self.daemon._commit_and_push_output("q-test-002b", "")

        mock_run.assert_not_called()


# ─── Test 3: git commit fails → spec completes, no exception ──────────


class TestCommitAndPushCommitFails(CommitAndPushTestCase):

    @patch("daemon.subprocess.run")
    def test_commit_failure_does_not_raise(self, mock_run: MagicMock) -> None:
        """When git commit raises CalledProcessError, no exception propagates."""
        target_repo = "/fake/repo"

        def side_effect(cmd, **kwargs):
            subcmd = cmd[2] if len(cmd) > 2 else ""
            if subcmd == "rev-parse":
                return self._make_run()
            if subcmd == "status":
                return self._make_run("M bar.py")
            if subcmd == "add":
                return self._make_run()
            if subcmd == "commit":
                raise subprocess.CalledProcessError(1, "git commit")
            return self._make_run()

        mock_run.side_effect = side_effect

        # Must not raise — spec completion is unaffected
        try:
            self.daemon._commit_and_push_output("q-test-003", target_repo)
        except Exception as exc:
            self.fail(f"_commit_and_push_output raised unexpectedly: {exc}")

        # push should NOT be called when commit failed
        push_calls = [
            c for c in mock_run.call_args_list
            if c.args and len(c.args[0]) >= 4 and c.args[0][3] == "push"
        ]
        self.assertFalse(push_calls, "git push should not be called after commit failure")


# ─── Test 4: git push fails → spec completes with warning ─────────────


class TestCommitAndPushPushFails(CommitAndPushTestCase):

    @patch("daemon.subprocess.run")
    def test_push_failure_does_not_raise(self, mock_run: MagicMock) -> None:
        """When git push raises CalledProcessError, no exception propagates."""
        target_repo = "/fake/repo"

        def side_effect(cmd, **kwargs):
            subcmd = cmd[2] if len(cmd) > 2 else ""
            if subcmd == "rev-parse":
                return self._make_run()
            if subcmd == "status":
                return self._make_run("M baz.py")
            if subcmd == "add":
                return self._make_run()
            if subcmd == "commit":
                return self._make_run()
            if subcmd == "remote":
                return self._make_run("origin ...")
            if subcmd == "push":
                raise subprocess.CalledProcessError(1, "git push")
            return self._make_run()

        mock_run.side_effect = side_effect

        # Must not raise
        try:
            self.daemon._commit_and_push_output("q-test-004", target_repo)
        except Exception as exc:
            self.fail(f"_commit_and_push_output raised unexpectedly: {exc}")


# ─── Test 5: changed-files manifest → only those files staged ─────────


class TestCommitAndPushManifest(CommitAndPushTestCase):

    @patch("daemon.subprocess.run")
    def test_manifest_files_are_staged_individually(self, mock_run: MagicMock) -> None:
        """When a .changed-files manifest exists, each listed file is git-add'd."""
        spec_id = "q-manifest-001"
        target_repo = "/fake/repo"

        # Write a manifest
        manifest_path = os.path.join(self.queue_dir, f"{spec_id}.changed-files")
        with open(manifest_path, "w", encoding="utf-8") as fh:
            fh.write("src/foo.py\nsrc/bar.py\n")

        # Override the queue_dir the daemon uses for manifest lookup
        # The method builds path from os.path.expanduser("~/.boi/queue/…")
        # so we patch os.path.isfile and open to point at our temp file.
        real_isfile = os.path.isfile

        def patched_isfile(path):
            if path.endswith(f"{spec_id}.changed-files"):
                return True
            return real_isfile(path)

        real_getsize = os.path.getsize

        def patched_getsize(path):
            if path.endswith(f"{spec_id}.changed-files"):
                return real_getsize(manifest_path)
            return real_getsize(path)

        real_open = open

        def patched_open(path, *args, **kwargs):
            if isinstance(path, str) and path.endswith(f"{spec_id}.changed-files"):
                return real_open(manifest_path, *args, **kwargs)
            return real_open(path, *args, **kwargs)

        real_exists = os.path.exists

        def patched_exists(path):
            # Make manifest-listed files appear to exist
            if path in (
                os.path.join(target_repo, "src/foo.py"),
                os.path.join(target_repo, "src/bar.py"),
            ):
                return True
            return real_exists(path)

        responses = [
            self._make_run(),             # rev-parse
            self._make_run("M src/foo.py"),  # status
            self._make_run(),             # add src/foo.py
            self._make_run(),             # add src/bar.py
            self._make_run(),             # commit
            self._make_run(""),           # remote -v (no remote)
        ]
        mock_run.side_effect = responses

        with patch("os.path.isfile", patched_isfile), \
             patch("os.path.getsize", patched_getsize), \
             patch("builtins.open", patched_open), \
             patch("os.path.exists", patched_exists):
            self.daemon._commit_and_push_output(spec_id, target_repo)

        # Expect two individual `git add -- <file>` calls
        add_calls = [
            c for c in mock_run.call_args_list
            if c.args and len(c.args[0]) >= 5 and c.args[0][3] == "add" and "--" in c.args[0]
        ]
        self.assertEqual(len(add_calls), 2)
        staged_files = {c.args[0][-1] for c in add_calls}
        self.assertEqual(staged_files, {"src/foo.py", "src/bar.py"})


if __name__ == "__main__":
    unittest.main()
