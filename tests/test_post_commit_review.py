# test_post_commit_review.py — Unit tests for _review_committed_output and
# _add_review_tasks in daemon.py.
#
# Tests:
#   1. pass=true  → _add_review_tasks is NOT called, no tasks added
#   2. pass=false + high-severity issues → [REVIEW] PENDING tasks appended
#   3. all issues low severity → no tasks added (advisory log only)
#   4. claude -p raises TimeoutExpired → returns without error
#   5. claude -p returns invalid JSON → returns without error
#   6. Review prompt passed to claude contains the git diff output

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

FAKE_DIFF = "diff --git a/foo.py b/foo.py\n@@ -1 +1 @@\n+hello world"
SPEC_CONTENT = "# Test Spec\n\n### t-1: Existing task\nDONE\n\n"


def _make_daemon(tmpdir: str) -> Daemon:
    db_path = os.path.join(tmpdir, "boi.db")
    queue_dir = os.path.join(tmpdir, "queue")
    log_dir = os.path.join(tmpdir, "logs")
    wt_a = os.path.join(tmpdir, "wt-a")
    wt_b = os.path.join(tmpdir, "wt-b")
    for d in (queue_dir, log_dir, wt_a, wt_b):
        os.makedirs(d, exist_ok=True)
    config_path = os.path.join(tmpdir, "config.json")
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
    d = Daemon(config_path=config_path, db_path=db_path, state_dir=tmpdir)
    d.load_workers()
    return d


def _run_result(stdout: str = "", returncode: int = 0) -> MagicMock:
    r = MagicMock()
    r.stdout = stdout
    r.returncode = returncode
    return r


class PostCommitReviewBase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.state_dir = self._tmpdir.name
        self.daemon = _make_daemon(self.state_dir)
        # A real directory so os.path.isdir(target_repo) passes
        self.target_repo = os.path.join(self.state_dir, "fake_repo")
        os.makedirs(self.target_repo, exist_ok=True)
        # Boi queue under tmpdir — used when patching expanduser
        self.boi_queue = os.path.join(self.state_dir, ".boi", "queue")
        os.makedirs(self.boi_queue, exist_ok=True)

    def tearDown(self) -> None:
        self.daemon.db.close()
        self._tmpdir.cleanup()

    def _write_spec(self, spec_id: str, content: str = SPEC_CONTENT) -> str:
        path = os.path.join(self.boi_queue, f"{spec_id}.spec.md")
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(content)
        return path


# ── Test 1: pass=true → _add_review_tasks is NOT called ──────────────────────


class TestReviewPassTrue(PostCommitReviewBase):
    @patch("daemon.subprocess.run")
    def test_pass_true_does_not_add_tasks(self, mock_run: MagicMock) -> None:
        mock_run.side_effect = [
            _run_result(stdout=FAKE_DIFF),                         # git diff
            _run_result(stdout='{"pass": true, "issues": []}'),    # claude -p
        ]
        self.daemon._add_review_tasks = MagicMock()

        self.daemon._review_committed_output("s-1", self.target_repo, "")

        self.daemon._add_review_tasks.assert_not_called()


# ── Test 2: pass=false + high issue → [REVIEW] tasks appended ────────────────


class TestReviewFailHighSeverity(PostCommitReviewBase):
    @patch("daemon.subprocess.run")
    @patch("daemon.os.path.expanduser")
    def test_high_severity_appends_review_task(
        self, mock_expanduser: MagicMock, mock_run: MagicMock
    ) -> None:
        # Redirect ~  to tmpdir so _add_review_tasks writes to our temp queue
        mock_expanduser.return_value = os.path.join(self.state_dir, ".boi_home")
        boi_queue = os.path.join(self.state_dir, ".boi_home", ".boi", "queue")
        os.makedirs(boi_queue, exist_ok=True)

        spec_id = "test-spec-high"
        spec_file = os.path.join(boi_queue, f"{spec_id}.spec.md")
        with open(spec_file, "w", encoding="utf-8") as fh:
            fh.write(SPEC_CONTENT)

        review_json = json.dumps(
            {
                "pass": False,
                "issues": [
                    {
                        "severity": "high",
                        "file": "foo.py",
                        "description": "null pointer dereference",
                    }
                ],
            }
        )
        mock_run.side_effect = [
            _run_result(stdout=FAKE_DIFF),           # git diff
            _run_result(stdout=review_json),          # claude -p
        ]
        self.daemon.db.update_spec_status = MagicMock()

        self.daemon._review_committed_output(spec_id, self.target_repo, "")

        with open(spec_file, encoding="utf-8") as fh:
            content = fh.read()

        self.assertIn("[REVIEW] Fix: null pointer dereference", content)
        self.assertIn("PENDING", content)


# ── Test 3: all issues low severity → no tasks added ─────────────────────────


class TestReviewFailLowSeverityOnly(PostCommitReviewBase):
    @patch("daemon.subprocess.run")
    @patch("daemon.os.path.expanduser")
    def test_low_severity_only_no_tasks_added(
        self, mock_expanduser: MagicMock, mock_run: MagicMock
    ) -> None:
        mock_expanduser.return_value = os.path.join(self.state_dir, ".boi_home")
        boi_queue = os.path.join(self.state_dir, ".boi_home", ".boi", "queue")
        os.makedirs(boi_queue, exist_ok=True)

        spec_id = "test-spec-low"
        spec_file = os.path.join(boi_queue, f"{spec_id}.spec.md")
        with open(spec_file, "w", encoding="utf-8") as fh:
            fh.write(SPEC_CONTENT)

        review_json = json.dumps(
            {
                "pass": False,
                "issues": [
                    {
                        "severity": "low",
                        "file": "foo.py",
                        "description": "missing docstring",
                    }
                ],
            }
        )
        mock_run.side_effect = [
            _run_result(stdout=FAKE_DIFF),
            _run_result(stdout=review_json),
        ]
        self.daemon.db.update_spec_status = MagicMock()

        self.daemon._review_committed_output(spec_id, self.target_repo, "")

        with open(spec_file, encoding="utf-8") as fh:
            content = fh.read()

        self.assertNotIn("[REVIEW]", content)
        self.daemon.db.update_spec_status.assert_not_called()


# ── Test 4: TimeoutExpired → returns without error ────────────────────────────


class TestReviewTimeout(PostCommitReviewBase):
    @patch("daemon.subprocess.run")
    def test_timeout_returns_without_error(self, mock_run: MagicMock) -> None:
        mock_run.side_effect = [
            _run_result(stdout=FAKE_DIFF),                       # git diff succeeds
            subprocess.TimeoutExpired(cmd=["claude"], timeout=120),  # claude times out
        ]
        # Should not raise
        self.daemon._review_committed_output("s-timeout", self.target_repo, "")


# ── Test 5: invalid JSON → returns without error ──────────────────────────────


class TestReviewInvalidJSON(PostCommitReviewBase):
    @patch("daemon.subprocess.run")
    def test_invalid_json_returns_without_error(self, mock_run: MagicMock) -> None:
        mock_run.side_effect = [
            _run_result(stdout=FAKE_DIFF),
            _run_result(stdout="not json at all {{{{"),
        ]
        # Should not raise
        self.daemon._review_committed_output("s-badjson", self.target_repo, "")


# ── Test 6: prompt contains git diff ─────────────────────────────────────────


class TestReviewPromptContainsDiff(PostCommitReviewBase):
    @patch("daemon.subprocess.run")
    def test_prompt_contains_diff(self, mock_run: MagicMock) -> None:
        mock_run.side_effect = [
            _run_result(stdout=FAKE_DIFF),
            _run_result(stdout='{"pass": true, "issues": []}'),
        ]

        self.daemon._review_committed_output("s-prompt", self.target_repo, "")

        # Second call is to claude -p; check the prompt arg contains the diff
        claude_call_args = mock_run.call_args_list[1]
        cmd = claude_call_args[0][0]  # positional first arg (the list)
        full_prompt = " ".join(cmd)
        self.assertIn(FAKE_DIFF, full_prompt)


# ── Test 7: code fence stripping — prose outside fence is ignored ─────────────


class TestReviewCodeFenceStripping(PostCommitReviewBase):
    @patch("daemon.subprocess.run")
    def test_code_fence_stripped_correctly(self, mock_run: MagicMock) -> None:
        """claude response with prose + fenced JSON block should parse without error."""
        fenced_response = (
            "Here is the review:\n"
            "```json\n"
            '{"pass": true, "issues": []}\n'
            "```\n"
            "Let me know if you need more details."
        )
        mock_run.side_effect = [
            _run_result(stdout=FAKE_DIFF),
            _run_result(stdout=fenced_response),
        ]
        self.daemon._add_review_tasks = MagicMock()

        # Should not raise — JSON parsing should succeed despite surrounding prose
        self.daemon._review_committed_output("s-fence", self.target_repo, "")

        # pass=true so no tasks added
        self.daemon._add_review_tasks.assert_not_called()


if __name__ == "__main__":
    unittest.main()
