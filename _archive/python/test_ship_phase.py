# test_ship_phase.py — Tests for the BOI ship phase (_run_ship_phase, _ship_single_repo).
#
# Coverage:
#   1. Verify passing → files committed, returns True
#   2. Verify failing → status = needs_review, no commit, returns False
#   3. push: true → commit + push executed
#   4. push: false → commit only, no push
#   5. commit_scope → only matching files staged
#   6. multi-repo → separate commits per repo
#   7. nothing to commit → returns True, no error

import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path
from unittest.mock import MagicMock, call, patch

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from daemon import Daemon


_DONE_SPEC = textwrap.dedent("""\
    # Test Ship Spec

    ### t-1: Do the thing
    DONE

    **Spec:** Do the thing.

    **Verify:** echo "task ok"
""")


class ShipPhaseTestCase(unittest.TestCase):
    """Base: Daemon with temp state dir and a real spec file."""

    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.state_dir = self._tmpdir.name
        self.db_path = os.path.join(self.state_dir, "boi.db")
        self.queue_dir = os.path.join(self.state_dir, "queue")
        os.makedirs(self.queue_dir, exist_ok=True)
        os.makedirs(os.path.join(self.state_dir, "logs"), exist_ok=True)

        wt_a = os.path.join(self.state_dir, "wt-a")
        wt_b = os.path.join(self.state_dir, "wt-b")
        os.makedirs(wt_a, exist_ok=True)
        os.makedirs(wt_b, exist_ok=True)

        config_path = os.path.join(self.state_dir, "config.json")
        with open(config_path, "w", encoding="utf-8") as fh:
            json.dump({"workers": [
                {"id": "w1", "worktree_path": wt_a},
                {"id": "w2", "worktree_path": wt_b},
            ]}, fh)

        self.daemon = Daemon(
            config_path=config_path,
            db_path=self.db_path,
            state_dir=self.state_dir,
        )
        self.daemon.load_workers()

        # Create a fake repo dir (real dir so os.path.isdir passes)
        self.repo_dir = os.path.join(self.state_dir, "fake-repo")
        os.makedirs(self.repo_dir, exist_ok=True)

        # Write a spec file with one DONE task
        self.spec_path = os.path.join(self.state_dir, "q-test.spec.md")
        Path(self.spec_path).write_text(_DONE_SPEC, encoding="utf-8")

    def tearDown(self) -> None:
        self.daemon.db.close()
        self._tmpdir.cleanup()

    def _ok(self, stdout="", returncode=0) -> MagicMock:
        r = MagicMock()
        r.stdout = stdout
        r.returncode = returncode
        r.stderr = b""
        return r

    def _fail(self, returncode=1, stderr=b"error") -> MagicMock:
        r = MagicMock()
        r.stdout = ""
        r.returncode = returncode
        r.stderr = stderr
        return r

    def _git_success_side_effect(self, push_remote=""):
        """Returns a side_effect function that handles typical git call sequences."""
        sha = "abc1234def5678900000000000000000000000000"

        def side_effect(cmd, **kwargs):
            # shell=True calls are verify commands; list calls are git
            if isinstance(cmd, str):
                return self._ok()
            if not isinstance(cmd, list):
                return self._ok()
            subcmd = next((c for c in cmd if not c.startswith("-") and c not in ("git",)), "")
            if "rev-parse" in cmd and "--git-dir" in cmd:
                return self._ok()
            if "rev-parse" in cmd and "HEAD" in cmd:
                return self._ok(stdout=sha)
            if "status" in cmd:
                return self._ok(stdout="M foo.py")
            if "add" in cmd:
                return self._ok()
            if "commit" in cmd:
                return self._ok()
            if "push" in cmd:
                return self._ok()
            return self._ok()

        return side_effect

    def _make_spec(self, spec_id: str) -> dict:
        return {
            "spec_path": self.spec_path,
            "worktree": self.repo_dir,
            "push": "false",
            "commit_scope": "",
            "title": "Test Ship Spec",
        }


# ─── Test 1: verify passing → files committed, returns True ──────────────


class TestVerifyPassingFilesCommitted(ShipPhaseTestCase):

    @patch("daemon.subprocess.run")
    def test_verify_passing_commits_files(self, mock_run: MagicMock) -> None:
        mock_run.side_effect = self._git_success_side_effect()

        result = self.daemon._run_ship_phase("q-test-001", self._make_spec("q-test-001"))

        self.assertTrue(result, "_run_ship_phase should return True when verify passes")

        # At least one commit call must have been made
        commit_calls = [
            c for c in mock_run.call_args_list
            if isinstance(c.args[0], list) and "commit" in c.args[0]
        ]
        self.assertTrue(commit_calls, "Expected git commit to be called")


# ─── Test 2: verify failing → needs_review, no commit ────────────────────


class TestVerifyFailingNeedsReview(ShipPhaseTestCase):

    @patch("daemon.subprocess.run")
    def test_verify_failure_sets_needs_review(self, mock_run: MagicMock) -> None:
        def side_effect(cmd, **kwargs):
            if isinstance(cmd, str):
                # shell=True verify command — fail it
                return self._fail(returncode=1, stderr=b"assertion failed")
            return self._ok()

        mock_run.side_effect = side_effect

        # Patch db to capture update calls
        self.daemon.db.update_spec_fields = MagicMock()

        result = self.daemon._run_ship_phase("q-test-002", self._make_spec("q-test-002"))

        self.assertFalse(result, "_run_ship_phase should return False when verify fails")
        self.daemon.db.update_spec_fields.assert_called_once()
        call_kwargs = self.daemon.db.update_spec_fields.call_args
        self.assertEqual(call_kwargs.kwargs.get("status"), "needs_review")

        # No git commit should have run
        commit_calls = [
            c for c in mock_run.call_args_list
            if isinstance(c.args[0], list) and "commit" in c.args[0]
        ]
        self.assertFalse(commit_calls, "git commit must NOT be called when verify fails")


# ─── Test 3: push: true → commit + push ──────────────────────────────────


class TestPushTrueExecutesPush(ShipPhaseTestCase):

    @patch("daemon.subprocess.run")
    def test_push_true_calls_git_push(self, mock_run: MagicMock) -> None:
        mock_run.side_effect = self._git_success_side_effect(push_remote="origin")

        spec = self._make_spec("q-test-003")
        spec["push"] = "true"

        self.daemon._run_ship_phase("q-test-003", spec)

        push_calls = [
            c for c in mock_run.call_args_list
            if isinstance(c.args[0], list) and "push" in c.args[0]
        ]
        self.assertTrue(push_calls, "git push must be called when push=true")
        # Must push to origin
        push_cmd = push_calls[0].args[0]
        self.assertIn("origin", push_cmd)


# ─── Test 4: push: false → commit only, no push ──────────────────────────


class TestPushFalseNoPush(ShipPhaseTestCase):

    @patch("daemon.subprocess.run")
    def test_push_false_skips_git_push(self, mock_run: MagicMock) -> None:
        mock_run.side_effect = self._git_success_side_effect()

        spec = self._make_spec("q-test-004")
        spec["push"] = "false"

        result = self.daemon._run_ship_phase("q-test-004", spec)

        self.assertTrue(result)
        push_calls = [
            c for c in mock_run.call_args_list
            if isinstance(c.args[0], list) and "push" in c.args[0]
        ]
        self.assertFalse(push_calls, "git push must NOT be called when push=false")


# ─── Test 5: commit_scope → only matching files staged ───────────────────


class TestCommitScopeFiltersFiles(ShipPhaseTestCase):

    @patch("daemon.subprocess.run")
    @patch("glob.glob", return_value=["app.py", "util.py"])
    def test_commit_scope_stages_only_matching_files(
        self, mock_glob: MagicMock, mock_run: MagicMock
    ) -> None:
        sha = "deadbeef" * 5

        def side_effect(cmd, **kwargs):
            if isinstance(cmd, str):
                return self._ok()
            if "rev-parse" in cmd and "--git-dir" in cmd:
                return self._ok()
            if "rev-parse" in cmd and "HEAD" in cmd:
                return self._ok(stdout=sha)
            if "status" in cmd:
                return self._ok(stdout="M app.py\nM util.py\n?? notes.txt")
            if "add" in cmd:
                return self._ok()
            if "commit" in cmd:
                return self._ok()
            return self._ok()

        mock_run.side_effect = side_effect

        spec = self._make_spec("q-test-005")
        spec["commit_scope"] = "*.py"

        # Spec has no verify commands on non-DONE tasks — but DONE task verify is echo
        result = self.daemon._run_ship_phase("q-test-005", spec)

        self.assertTrue(result)
        mock_glob.assert_called_once_with("*.py", root_dir=self.repo_dir)

        # git add calls must use -- <file> form (individual file staging)
        add_calls = [
            c for c in mock_run.call_args_list
            if isinstance(c.args[0], list) and "add" in c.args[0] and "--" in c.args[0]
        ]
        self.assertEqual(len(add_calls), 2, "Expected 2 individual git add calls")
        staged = {c.args[0][-1] for c in add_calls}
        self.assertEqual(staged, {"app.py", "util.py"})

        # git add -A must NOT have been called
        add_all_calls = [
            c for c in mock_run.call_args_list
            if isinstance(c.args[0], list) and "add" in c.args[0] and "-A" in c.args[0]
        ]
        self.assertFalse(add_all_calls, "git add -A must not be called when commit_scope is set")


# ─── Test 6: multi-repo → separate commits per repo ─────────────────────


class TestMultiRepoSeparateCommits(ShipPhaseTestCase):

    @patch("daemon.subprocess.run")
    def test_multi_repo_commits_each_repo_separately(self, mock_run: MagicMock) -> None:
        sha = "cafe0000" * 5

        # Create a second real repo directory
        repo2 = os.path.join(self.state_dir, "fake-repo-2")
        os.makedirs(repo2, exist_ok=True)

        def side_effect(cmd, **kwargs):
            if isinstance(cmd, str):
                return self._ok()
            if "rev-parse" in cmd and "--git-dir" in cmd:
                return self._ok()
            if "rev-parse" in cmd and "HEAD" in cmd:
                return self._ok(stdout=sha)
            if "status" in cmd:
                return self._ok(stdout="M file.txt")
            if "add" in cmd:
                return self._ok()
            if "commit" in cmd:
                return self._ok()
            return self._ok()

        mock_run.side_effect = side_effect

        # Patch _extract_spec_target_repos to return the second repo
        with patch.object(
            Daemon, "_extract_spec_target_repos", return_value=[repo2]
        ):
            result = self.daemon._run_ship_phase("q-test-006", self._make_spec("q-test-006"))

        self.assertTrue(result)

        # Collect all repos that received a git commit call
        committed_repos = set()
        for c in mock_run.call_args_list:
            args = c.args[0] if c.args else []
            if isinstance(args, list) and "commit" in args:
                # git -C <repo> commit ...
                idx = args.index("-C")
                committed_repos.add(args[idx + 1])

        self.assertIn(self.repo_dir, committed_repos, "Primary repo must be committed")
        self.assertIn(repo2, committed_repos, "Secondary repo must be committed")
        self.assertEqual(len(committed_repos), 2, "Exactly 2 repos should be committed")

        # Ship sidecar must record both commits
        sidecar_path = os.path.join(self.queue_dir, "q-test-006.ship.json")
        self.assertTrue(os.path.exists(sidecar_path), "Ship sidecar should be written")
        with open(sidecar_path, encoding="utf-8") as fh:
            sidecar = json.load(fh)
        self.assertEqual(len(sidecar["commits"]), 2)


# ─── Test 7: nothing to commit → returns True, no error ──────────────────


class TestNothingToCommitSuccess(ShipPhaseTestCase):

    @patch("daemon.subprocess.run")
    def test_clean_repo_returns_true_no_error(self, mock_run: MagicMock) -> None:
        def side_effect(cmd, **kwargs):
            if isinstance(cmd, str):
                return self._ok()
            if "rev-parse" in cmd and "--git-dir" in cmd:
                return self._ok()
            if "status" in cmd:
                return self._ok(stdout="")  # clean working tree
            return self._ok()

        mock_run.side_effect = side_effect

        result = self.daemon._run_ship_phase("q-test-007", self._make_spec("q-test-007"))

        self.assertTrue(result, "_run_ship_phase must return True even when nothing to commit")

        # No commit should have been attempted
        commit_calls = [
            c for c in mock_run.call_args_list
            if isinstance(c.args[0], list) and "commit" in c.args[0]
        ]
        self.assertFalse(commit_calls, "git commit must not be called when repo is clean")


# ─── Direct _ship_single_repo unit tests ────────────────────────────────


class TestShipSingleRepoDirect(ShipPhaseTestCase):
    """Direct tests of _ship_single_repo for edge cases."""

    @patch("daemon.subprocess.run")
    def test_not_a_git_repo_returns_true_skipped(self, mock_run: MagicMock) -> None:
        mock_run.side_effect = subprocess.CalledProcessError(128, "git")

        ok, sha = self.daemon._ship_single_repo(
            repo_path="/nonexistent",
            spec_id="q-x",
            commit_msg="feat: BOI q-x",
            commit_scope="",
            manifest_path="",
            push_remote="",
        )

        self.assertTrue(ok, "Non-git directory should be treated as success (skip)")
        self.assertEqual(sha, "")

    @patch("daemon.subprocess.run")
    def test_git_commit_nothing_to_commit_returns_true(self, mock_run: MagicMock) -> None:
        sha_call = self._ok(stdout="")

        def side_effect(cmd, **kwargs):
            if "rev-parse" in cmd and "--git-dir" in cmd:
                return self._ok()
            if "status" in cmd:
                return self._ok(stdout="M file.txt")
            if "add" in cmd:
                return self._ok()
            if "commit" in cmd:
                err = subprocess.CalledProcessError(1, "git commit")
                err.stderr = b"nothing to commit, working tree clean"
                raise err
            return self._ok()

        mock_run.side_effect = side_effect

        ok, sha = self.daemon._ship_single_repo(
            repo_path=self.repo_dir,
            spec_id="q-x",
            commit_msg="feat: BOI q-x",
            commit_scope="",
            manifest_path="",
            push_remote="",
        )

        self.assertTrue(ok)
        self.assertEqual(sha, "")

    @patch("daemon.subprocess.run")
    def test_named_remote_push(self, mock_run: MagicMock) -> None:
        sha = "beefdead" * 5

        def side_effect(cmd, **kwargs):
            if "rev-parse" in cmd and "--git-dir" in cmd:
                return self._ok()
            if "rev-parse" in cmd and "HEAD" in cmd:
                return self._ok(stdout=sha)
            if "status" in cmd:
                return self._ok(stdout="M x.py")
            if "add" in cmd:
                return self._ok()
            if "commit" in cmd:
                return self._ok()
            if "push" in cmd:
                return self._ok()
            return self._ok()

        mock_run.side_effect = side_effect

        ok, _ = self.daemon._ship_single_repo(
            repo_path=self.repo_dir,
            spec_id="q-x",
            commit_msg="feat: BOI q-x",
            commit_scope="",
            manifest_path="",
            push_remote="upstream",
        )

        self.assertTrue(ok)
        push_calls = [
            c for c in mock_run.call_args_list
            if isinstance(c.args[0], list) and "push" in c.args[0]
        ]
        self.assertTrue(push_calls)
        self.assertIn("upstream", push_calls[0].args[0])


if __name__ == "__main__":
    unittest.main()
