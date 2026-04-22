# test_per_iteration_commit.py -- Unit tests for Daemon._commit_iteration_output.
#
# Covers:
#   1. No manifest file -- no-op
#   2. Empty manifest file -- no-op
#   3. Manifest with files -- stages and commits
#   4. Git failure (add fails) -- logs warning, returns, does not raise
#   5. Manifest cleared after successful commit

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


class IterCommitTestCase(unittest.TestCase):
    """Base: minimal Daemon instance with temp state dir."""

    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.state_dir = self._tmpdir.name
        self.db_path = os.path.join(self.state_dir, "boi.db")
        self.queue_dir = os.path.join(self.state_dir, "queue")
        self.log_dir = os.path.join(self.state_dir, "logs")
        os.makedirs(self.queue_dir, exist_ok=True)
        os.makedirs(self.log_dir, exist_ok=True)

        wt_a = os.path.join(self.state_dir, "wt-a")
        wt_b = os.path.join(self.state_dir, "wt-b")
        os.makedirs(wt_a, exist_ok=True)
        os.makedirs(wt_b, exist_ok=True)

        config_path = os.path.join(self.state_dir, "config.json")
        with open(config_path, "w", encoding="utf-8") as fh:
            json.dump(
                {
                    "workers": [
                        {"id": "w1", "worktree_path": wt_a},
                        {"id": "w2", "worktree_path": wt_b},
                    ]
                },
                fh,
            )

        self.daemon = Daemon(
            config_path=config_path,
            db_path=self.db_path,
            state_dir=self.state_dir,
        )
        self.daemon.load_workers()

        self.spec_id = "q-test-001"
        self.target_repo = "/fake/repo"
        self.iteration = 3
        self.manifest_path = os.path.join(
            self.state_dir, "queue", f"{self.spec_id}.changed-files"
        )

    def tearDown(self) -> None:
        self.daemon.db.close()
        self._tmpdir.cleanup()

    def _write_manifest(self, files):
        with open(self.manifest_path, "w", encoding="utf-8") as fh:
            fh.write("\n".join(files) + ("\n" if files else ""))

    def _make_run_ok(self):
        result = MagicMock()
        result.returncode = 0
        result.stdout = ""
        return result


# -- Test 1: no manifest file -- no-op ------------------------------------


class TestNoManifest(IterCommitTestCase):
    @patch("subprocess.run")
    def test_no_git_calls_when_manifest_missing(self, mock_run):
        # Manifest does not exist
        assert not os.path.exists(self.manifest_path)

        self.daemon._commit_iteration_output(
            self.spec_id, self.target_repo, self.iteration
        )

        mock_run.assert_not_called()


# -- Test 2: empty manifest -- no-op --------------------------------------


class TestEmptyManifest(IterCommitTestCase):
    @patch("subprocess.run")
    def test_no_git_calls_when_manifest_empty(self, mock_run):
        self._write_manifest([])

        self.daemon._commit_iteration_output(
            self.spec_id, self.target_repo, self.iteration
        )

        mock_run.assert_not_called()


# -- Test 3: manifest with files -- stages and commits --------------------


class TestManifestWithFiles(IterCommitTestCase):
    @patch("subprocess.run")
    def test_stages_and_commits_manifest_files(self, mock_run):
        self._write_manifest(["src/foo.py", "src/bar.py"])
        mock_run.return_value = self._make_run_ok()

        self.daemon._commit_iteration_output(
            self.spec_id, self.target_repo, self.iteration
        )

        calls = mock_run.call_args_list
        # Should have at least 2 add calls and 1 commit call
        add_calls = [c for c in calls if "add" in c.args[0]]
        commit_calls = [c for c in calls if "commit" in c.args[0]]

        self.assertTrue(len(add_calls) >= 1, "Expected at least one git add call")
        self.assertEqual(len(commit_calls), 1, "Expected exactly one git commit call")

        # Verify commit message contains spec_id and iteration
        commit_args = commit_calls[0].args[0]
        commit_msg_arg = " ".join(str(a) for a in commit_args)
        self.assertIn(self.spec_id, commit_msg_arg)
        self.assertIn(str(self.iteration), commit_msg_arg)
        self.assertIn("wip", commit_msg_arg)


# -- Test 4: git failure -- logs warning, does not raise ------------------


class TestGitFailure(IterCommitTestCase):
    @patch("subprocess.run")
    def test_git_add_failure_logs_warning_does_not_raise(self, mock_run):
        self._write_manifest(["src/foo.py"])
        mock_run.side_effect = subprocess.CalledProcessError(1, "git")

        with self.assertLogs("boi.daemon", level="WARNING") as cm:
            # Must not raise
            self.daemon._commit_iteration_output(
                self.spec_id, self.target_repo, self.iteration
            )

        self.assertTrue(
            any("WARNING" in line or "warning" in line.lower() for line in cm.output),
            "Expected a warning log entry",
        )

    @patch("subprocess.run")
    def test_git_commit_failure_logs_warning_does_not_raise(self, mock_run):
        self._write_manifest(["src/foo.py"])

        def side_effect(cmd, **kwargs):
            if "commit" in cmd:
                raise subprocess.CalledProcessError(1, "git commit")
            return self._make_run_ok()

        mock_run.side_effect = side_effect

        with self.assertLogs("boi.daemon", level="WARNING") as cm:
            self.daemon._commit_iteration_output(
                self.spec_id, self.target_repo, self.iteration
            )

        self.assertTrue(
            any("WARNING" in line or "warning" in line.lower() for line in cm.output),
        )


# -- Test 5: manifest cleared after successful commit ---------------------


class TestManifestClearedAfterCommit(IterCommitTestCase):
    @patch("subprocess.run")
    def test_manifest_cleared_after_commit(self, mock_run):
        self._write_manifest(["src/foo.py"])
        mock_run.return_value = self._make_run_ok()

        self.daemon._commit_iteration_output(
            self.spec_id, self.target_repo, self.iteration
        )

        # Manifest should be empty (cleared) after a successful commit
        self.assertTrue(os.path.exists(self.manifest_path), "Manifest file should still exist")
        with open(self.manifest_path, encoding="utf-8") as fh:
            content = fh.read().strip()
        self.assertEqual(content, "", "Manifest should be empty after commit")

    @patch("subprocess.run")
    def test_manifest_not_cleared_on_git_failure(self, mock_run):
        self._write_manifest(["src/foo.py"])
        mock_run.side_effect = subprocess.CalledProcessError(1, "git add")

        with self.assertLogs("boi.daemon", level="WARNING"):
            self.daemon._commit_iteration_output(
                self.spec_id, self.target_repo, self.iteration
            )

        # Manifest should still have content
        with open(self.manifest_path, encoding="utf-8") as fh:
            content = fh.read().strip()
        self.assertNotEqual(content, "", "Manifest should NOT be cleared after git failure")


if __name__ == "__main__":
    unittest.main()
