# test_spec_branches.py -- Tests for Daemon branch lifecycle methods.
#
# Covers t-1: _create_spec_branch, _ensure_on_spec_branch, _get_original_branch
# Covers t-2: _commit_iteration
# Covers t-3: _merge_spec_branch

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


class BranchTestCase(unittest.TestCase):
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
        self.base_branch_path = os.path.join(
            self.queue_dir, f"{self.spec_id}.base-branch"
        )

    def tearDown(self) -> None:
        self.daemon.db.close()
        self._tmpdir.cleanup()

    def _make_run_ok(self, stdout=""):
        result = MagicMock()
        result.returncode = 0
        result.stdout = stdout
        return result


# ---------------------------------------------------------------------------
# t-1: _create_spec_branch
# ---------------------------------------------------------------------------

class TestCreateSpecBranch(BranchTestCase):
    @patch("subprocess.run")
    def test_creates_new_branch_and_returns_branch_name(self, mock_run):
        # git checkout -b boi/{spec_id} succeeds
        mock_run.return_value = self._make_run_ok()

        result = self.daemon._create_spec_branch(self.spec_id, self.target_repo)

        self.assertEqual(result, f"boi/{self.spec_id}")

    @patch("subprocess.run")
    def test_writes_base_branch_file_on_new_branch(self, mock_run):
        # git rev-parse returns "main\n", checkout-b succeeds
        def side_effect(cmd, **kwargs):
            if "rev-parse" in cmd and "--abbrev-ref" in cmd:
                r = MagicMock()
                r.returncode = 0
                r.stdout = "main\n"
                return r
            return self._make_run_ok()

        mock_run.side_effect = side_effect

        self.daemon._create_spec_branch(self.spec_id, self.target_repo)

        self.assertTrue(
            os.path.exists(self.base_branch_path),
            "base-branch file should be created",
        )
        with open(self.base_branch_path, encoding="utf-8") as fh:
            content = fh.read().strip()
        self.assertEqual(content, "main")

    @patch("subprocess.run")
    def test_resumes_existing_branch_with_checkout(self, mock_run):
        # First call (checkout -b) fails with exit 128, second (checkout) succeeds
        def side_effect(cmd, **kwargs):
            if "checkout" in cmd and "-b" in cmd:
                raise subprocess.CalledProcessError(128, cmd)
            return self._make_run_ok()

        mock_run.side_effect = side_effect

        result = self.daemon._create_spec_branch(self.spec_id, self.target_repo)

        self.assertEqual(result, f"boi/{self.spec_id}")

    @patch("subprocess.run")
    def test_git_failure_returns_empty_string(self, mock_run):
        mock_run.side_effect = subprocess.CalledProcessError(1, "git")

        result = self.daemon._create_spec_branch(self.spec_id, self.target_repo)

        self.assertEqual(result, "")


# ---------------------------------------------------------------------------
# t-1: _ensure_on_spec_branch
# ---------------------------------------------------------------------------

class TestEnsureOnSpecBranch(BranchTestCase):
    @patch("subprocess.run")
    def test_returns_true_when_already_on_spec_branch(self, mock_run):
        branch = f"boi/{self.spec_id}"

        def side_effect(cmd, **kwargs):
            if "rev-parse" in cmd and "--abbrev-ref" in cmd:
                r = MagicMock()
                r.returncode = 0
                r.stdout = branch + "\n"
                return r
            return self._make_run_ok()

        mock_run.side_effect = side_effect

        result = self.daemon._ensure_on_spec_branch(self.spec_id, self.target_repo)

        self.assertTrue(result)

    @patch("subprocess.run")
    def test_checks_out_branch_when_on_wrong_branch(self, mock_run):
        def side_effect(cmd, **kwargs):
            if "rev-parse" in cmd and "--abbrev-ref" in cmd:
                r = MagicMock()
                r.returncode = 0
                r.stdout = "main\n"
                return r
            return self._make_run_ok()

        mock_run.side_effect = side_effect

        result = self.daemon._ensure_on_spec_branch(self.spec_id, self.target_repo)

        self.assertTrue(result)
        # Should have issued a checkout command
        all_cmds = [c.args[0] for c in mock_run.call_args_list]
        checkout_cmds = [c for c in all_cmds if "checkout" in c]
        self.assertTrue(len(checkout_cmds) >= 1, "Expected a checkout command")

    @patch("subprocess.run")
    def test_returns_false_on_git_failure(self, mock_run):
        mock_run.side_effect = subprocess.CalledProcessError(1, "git")

        result = self.daemon._ensure_on_spec_branch(self.spec_id, self.target_repo)

        self.assertFalse(result)


# ---------------------------------------------------------------------------
# t-1: _get_original_branch
# ---------------------------------------------------------------------------

class TestGetOriginalBranch(BranchTestCase):
    def test_reads_base_branch_from_file(self):
        with open(self.base_branch_path, "w", encoding="utf-8") as fh:
            fh.write("feature/my-work\n")

        result = self.daemon._get_original_branch(self.spec_id, self.target_repo)

        self.assertEqual(result, "feature/my-work")

    def test_returns_main_when_no_base_branch_file(self):
        # No file written
        result = self.daemon._get_original_branch(self.spec_id, self.target_repo)

        self.assertEqual(result, "main")

    def test_strips_whitespace_from_branch_name(self):
        with open(self.base_branch_path, "w", encoding="utf-8") as fh:
            fh.write("  develop  \n")

        result = self.daemon._get_original_branch(self.spec_id, self.target_repo)

        self.assertEqual(result, "develop")


# ---------------------------------------------------------------------------
# t-2: _commit_iteration
# ---------------------------------------------------------------------------

class TestCommitIteration(BranchTestCase):
    def _write_manifest(self, files):
        manifest_path = os.path.join(self.queue_dir, f"{self.spec_id}.changed-files")
        with open(manifest_path, "w", encoding="utf-8") as fh:
            fh.write("\n".join(files))
        return manifest_path

    @patch("subprocess.run")
    def test_no_manifest_is_noop(self, mock_run):
        # No manifest file exists -- _commit_iteration should do nothing
        with patch.object(
            self.daemon,
            "_ensure_on_spec_branch",
            return_value=True,
        ):
            self.daemon._commit_iteration(self.spec_id, self.target_repo, 1)

        mock_run.assert_not_called()

    @patch("subprocess.run")
    def test_empty_manifest_is_noop(self, mock_run):
        self._write_manifest([])
        with patch.object(
            self.daemon,
            "_ensure_on_spec_branch",
            return_value=True,
        ):
            self.daemon._commit_iteration(self.spec_id, self.target_repo, 1)

        mock_run.assert_not_called()

    @patch("subprocess.run")
    def test_manifest_with_files_commits_on_spec_branch(self, mock_run):
        files = ["src/foo.py", "src/bar.py"]
        manifest_path = self._write_manifest(files)
        mock_run.return_value = self._make_run_ok()

        with patch.object(
            self.daemon,
            "_ensure_on_spec_branch",
            return_value=True,
        ) as mock_ensure:
            self.daemon._commit_iteration(self.spec_id, self.target_repo, 3)

        mock_ensure.assert_called_once_with(self.spec_id, self.target_repo)
        all_cmds = [c.args[0] for c in mock_run.call_args_list]
        # Should have an add and a commit
        add_cmds = [c for c in all_cmds if "add" in c]
        commit_cmds = [c for c in all_cmds if "commit" in c]
        self.assertTrue(len(add_cmds) >= 1, "Expected git add command")
        self.assertTrue(len(commit_cmds) >= 1, "Expected git commit command")
        # Commit message should contain spec_id and iteration
        commit_args = next(c for c in all_cmds if "commit" in c)
        msg_idx = commit_args.index("-m") + 1
        self.assertIn(self.spec_id, commit_args[msg_idx])
        self.assertIn("3", commit_args[msg_idx])

    @patch("subprocess.run")
    def test_manifest_cleared_after_successful_commit(self, mock_run):
        files = ["src/foo.py"]
        manifest_path = self._write_manifest(files)
        mock_run.return_value = self._make_run_ok()

        with patch.object(
            self.daemon,
            "_ensure_on_spec_branch",
            return_value=True,
        ):
            self.daemon._commit_iteration(self.spec_id, self.target_repo, 1)

        with open(manifest_path, encoding="utf-8") as fh:
            content = fh.read().strip()
        self.assertEqual(content, "", "Manifest should be cleared after commit")

    @patch("subprocess.run")
    def test_git_failure_logs_warning_no_raise(self, mock_run):
        files = ["src/foo.py"]
        self._write_manifest(files)
        mock_run.side_effect = subprocess.CalledProcessError(1, "git")

        with patch.object(
            self.daemon,
            "_ensure_on_spec_branch",
            return_value=True,
        ):
            # Must not raise
            try:
                self.daemon._commit_iteration(self.spec_id, self.target_repo, 2)
            except Exception as exc:
                self.fail(f"_commit_iteration raised unexpectedly: {exc}")

    @patch("subprocess.run")
    def test_ensure_branch_false_skips_git_ops(self, mock_run):
        files = ["src/foo.py"]
        self._write_manifest(files)

        with patch.object(
            self.daemon,
            "_ensure_on_spec_branch",
            return_value=False,
        ):
            self.daemon._commit_iteration(self.spec_id, self.target_repo, 1)

        mock_run.assert_not_called()


# ---------------------------------------------------------------------------
# t-3: _merge_spec_branch
# ---------------------------------------------------------------------------

class TestMergeSpecBranch(BranchTestCase):
    def _write_base_branch(self, branch_name: str) -> None:
        with open(self.base_branch_path, "w", encoding="utf-8") as fh:
            fh.write(branch_name + "\n")

    @patch("subprocess.run")
    def test_successful_squash_merge_returns_true(self, mock_run):
        self._write_base_branch("main")
        mock_run.return_value = self._make_run_ok()

        result = self.daemon._merge_spec_branch(self.spec_id, self.target_repo)

        self.assertTrue(result)
        all_cmds = [c.args[0] for c in mock_run.call_args_list]
        # checkout original branch
        checkout_cmds = [c for c in all_cmds if "checkout" in c]
        self.assertTrue(any("main" in c for c in checkout_cmds), "Should checkout main")
        # squash merge
        merge_cmds = [c for c in all_cmds if "merge" in c]
        self.assertTrue(any("--squash" in c for c in merge_cmds), "Should use --squash")
        # commit
        commit_cmds = [c for c in all_cmds if "commit" in c]
        self.assertTrue(len(commit_cmds) >= 1, "Should commit after squash merge")
        # delete branch
        branch_del_cmds = [c for c in all_cmds if "branch" in c and "-D" in c]
        self.assertTrue(len(branch_del_cmds) >= 1, "Should delete spec branch")

    @patch("subprocess.run")
    def test_commit_message_contains_spec_id(self, mock_run):
        self._write_base_branch("main")
        mock_run.return_value = self._make_run_ok()

        self.daemon._merge_spec_branch(self.spec_id, self.target_repo)

        all_cmds = [c.args[0] for c in mock_run.call_args_list]
        commit_cmds = [c for c in all_cmds if "commit" in c]
        self.assertTrue(len(commit_cmds) >= 1)
        msg_idx = commit_cmds[0].index("-m") + 1
        self.assertIn(self.spec_id, commit_cmds[0][msg_idx])

    @patch("subprocess.run")
    def test_cleans_up_base_branch_file_on_success(self, mock_run):
        self._write_base_branch("main")
        mock_run.return_value = self._make_run_ok()

        self.daemon._merge_spec_branch(self.spec_id, self.target_repo)

        self.assertFalse(
            os.path.exists(self.base_branch_path),
            "base-branch file should be deleted on success",
        )

    @patch("subprocess.run")
    def test_merge_conflict_returns_false_and_preserves_branch(self, mock_run):
        self._write_base_branch("main")

        def side_effect(cmd, **kwargs):
            if "merge" in cmd:
                raise subprocess.CalledProcessError(1, cmd)
            return self._make_run_ok()

        mock_run.side_effect = side_effect

        result = self.daemon._merge_spec_branch(self.spec_id, self.target_repo)

        self.assertFalse(result)
        # Spec branch should NOT have been deleted
        all_cmds = [c.args[0] for c in mock_run.call_args_list]
        branch_del_cmds = [c for c in all_cmds if "branch" in c and "-D" in c]
        self.assertEqual(len(branch_del_cmds), 0, "Branch should be preserved on conflict")

    @patch("subprocess.run")
    def test_checkout_failure_returns_false(self, mock_run):
        self._write_base_branch("main")

        def side_effect(cmd, **kwargs):
            if "checkout" in cmd:
                raise subprocess.CalledProcessError(1, cmd)
            return self._make_run_ok()

        mock_run.side_effect = side_effect

        result = self.daemon._merge_spec_branch(self.spec_id, self.target_repo)

        self.assertFalse(result)

    @patch("subprocess.run")
    def test_falls_back_to_main_when_no_base_branch_file(self, mock_run):
        # No base-branch file -- should fall back to "main"
        mock_run.return_value = self._make_run_ok()

        result = self.daemon._merge_spec_branch(self.spec_id, self.target_repo)

        self.assertTrue(result)
        all_cmds = [c.args[0] for c in mock_run.call_args_list]
        checkout_cmds = [c for c in all_cmds if "checkout" in c]
        self.assertTrue(any("main" in c for c in checkout_cmds))

    @patch("subprocess.run")
    def test_attempts_checkout_back_on_failure(self, mock_run):
        self._write_base_branch("main")
        call_count = [0]

        def side_effect(cmd, **kwargs):
            call_count[0] += 1
            # First checkout succeeds, merge fails, recovery checkout should fire
            if "merge" in cmd:
                raise subprocess.CalledProcessError(1, cmd)
            return self._make_run_ok()

        mock_run.side_effect = side_effect

        self.daemon._merge_spec_branch(self.spec_id, self.target_repo)

        # Should have tried at least 2 checkouts: initial + recovery
        all_cmds = [c.args[0] for c in mock_run.call_args_list]
        checkout_cmds = [c for c in all_cmds if "checkout" in c]
        self.assertGreaterEqual(len(checkout_cmds), 2, "Should attempt recovery checkout")


# ---------------------------------------------------------------------------
# t-4: process_worker_completion branch lifecycle wiring
# ---------------------------------------------------------------------------


class TestProcessWorkerCompletionBranchLifecycle(BranchTestCase):
    """Tests that process_worker_completion wires branch lifecycle correctly."""

    def _make_worker(self, phase="execute"):
        return {
            "id": "w1",
            "current_spec_id": self.spec_id,
            "current_pid": None,
            "current_phase": phase,
        }

    def _make_spec(self, status="queued"):
        return {
            "id": self.spec_id,
            "spec_path": os.path.join(self.state_dir, "test.spec.md"),
            "status": status,
            "tasks_done": 1,
            "tasks_total": 2,
            "iteration": 1,
            "failure_reason": "",
        }

    def _run_pwc(self, phase="execute", after_status="queued",
                 base_branch_exists=False, target_repo=None,
                 merge_result=True):
        """Run process_worker_completion with standard mocks; return mock objects."""
        if target_repo is None:
            target_repo = self.target_repo
        if base_branch_exists:
            with open(self.base_branch_path, "w", encoding="utf-8") as fh:
                fh.write("main\n")

        initial_spec = self._make_spec("running")
        after_spec = self._make_spec(after_status)
        # "completed" path calls db.get_spec a third time (spec_refreshed)
        get_spec_returns = [initial_spec, after_spec]
        if after_status == "completed":
            get_spec_returns.append(after_spec)

        mocks = {}
        with patch.object(self.daemon.db, "get_worker", return_value=self._make_worker(phase)), \
             patch.object(self.daemon.db, "get_spec", side_effect=get_spec_returns), \
             patch.object(self.daemon.db, "end_process"), \
             patch.object(self.daemon.db, "free_worker"), \
             patch.object(self.daemon, "_dispatch_phase_completion"), \
             patch.object(self.daemon, "_extract_target_repo", return_value=target_repo), \
             patch.object(self.daemon, "_extract_spec_title", return_value="Test"), \
             patch.object(self.daemon, "_create_spec_branch", return_value=f"boi/{self.spec_id}") as m_create, \
             patch.object(self.daemon, "_ensure_on_spec_branch", return_value=True) as m_ensure, \
             patch.object(self.daemon, "_commit_iteration") as m_commit_iter, \
             patch.object(self.daemon, "_merge_spec_branch", return_value=merge_result) as m_merge, \
             patch.object(self.daemon, "_commit_and_push_output") as m_commit_push, \
             patch.object(self.daemon, "_review_committed_output"), \
             patch.object(self.daemon, "emit_hex_event"):
            self.daemon.process_worker_completion(worker_id="w1", exit_code=0)
            mocks["create"] = m_create
            mocks["ensure"] = m_ensure
            mocks["commit_iter"] = m_commit_iter
            mocks["merge"] = m_merge
            mocks["commit_push"] = m_commit_push
        return mocks

    def test_first_iteration_creates_branch(self):
        """When no base-branch file exists, _create_spec_branch is called."""
        mocks = self._run_pwc(phase="execute", after_status="queued", base_branch_exists=False)
        mocks["create"].assert_called_once_with(self.spec_id, self.target_repo)

    def test_subsequent_iterations_do_not_recreate_branch(self):
        """When base-branch file exists, _create_spec_branch is NOT called."""
        mocks = self._run_pwc(phase="execute", after_status="queued", base_branch_exists=True)
        mocks["create"].assert_not_called()

    def test_per_iteration_commit_on_queued_status(self):
        """_commit_iteration is called when spec is requeued."""
        mocks = self._run_pwc(phase="execute", after_status="queued")
        mocks["commit_iter"].assert_called_once_with(self.spec_id, self.target_repo, 1)

    def test_per_iteration_commit_on_completed_status(self):
        """_commit_iteration is called when spec is completed."""
        mocks = self._run_pwc(phase="execute", after_status="completed")
        mocks["commit_iter"].assert_called_once_with(self.spec_id, self.target_repo, 1)

    def test_completion_calls_merge_spec_branch(self):
        """On completion with target_repo, _merge_spec_branch is called."""
        mocks = self._run_pwc(phase="execute", after_status="completed")
        mocks["merge"].assert_called_once_with(self.spec_id, self.target_repo)

    def test_completion_skips_commit_and_push_when_target_repo_set(self):
        """_commit_and_push_output is NOT called when target_repo is available."""
        mocks = self._run_pwc(phase="execute", after_status="completed")
        mocks["commit_push"].assert_not_called()

    def test_no_target_repo_skips_merge(self):
        """Without target_repo, _merge_spec_branch is not called on completion."""
        mocks = self._run_pwc(phase="execute", after_status="completed", target_repo="")
        mocks["merge"].assert_not_called()

    def test_failure_preserves_branch_no_merge(self):
        """On failure, _merge_spec_branch is NOT called."""
        mocks = self._run_pwc(phase="execute", after_status="failed")
        mocks["merge"].assert_not_called()

    def test_task_verify_ensures_on_spec_branch(self):
        """task-verify phase calls _ensure_on_spec_branch before dispatch."""
        mocks = self._run_pwc(phase="task-verify", after_status="queued")
        mocks["ensure"].assert_called_once_with(self.spec_id, self.target_repo)

    def test_non_execute_phase_does_not_create_branch(self):
        """task-verify phase does NOT call _create_spec_branch."""
        mocks = self._run_pwc(phase="task-verify", after_status="queued")
        mocks["create"].assert_not_called()

    def test_no_target_repo_skips_all_branch_logic(self):
        """Specs without target_repo skip all branch lifecycle calls."""
        mocks = self._run_pwc(phase="execute", after_status="queued", target_repo="")
        mocks["create"].assert_not_called()
        mocks["ensure"].assert_not_called()
        mocks["commit_iter"].assert_not_called()


if __name__ == "__main__":
    unittest.main()
