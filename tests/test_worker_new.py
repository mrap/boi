# test_worker_new.py — Tests for the Python BOI worker (worker.py).
#
# Tests cover: launch_tmux, wait_for_tmux, post_process, timeout
# handling, argparse CLI entry point.
# Uses mock subprocess to avoid spawning real tmux sessions.

import json
import os
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path
from unittest.mock import MagicMock, call, patch

# Add parent directory to path so we can import worker module
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from worker import Worker, WorkerHooks, main, TMUX_SOCKET

SAMPLE_SPEC = textwrap.dedent("""\
    # Test Spec

    ## Tasks

    ### t-1: First task
    DONE

    **Spec:** Do the first thing.

    **Verify:** echo "done"

    ### t-2: Second task
    PENDING

    **Spec:** Do the second thing.

    **Verify:** echo "ok"

    ### t-3: Third task
    PENDING

    **Spec:** Do the third thing.

    **Verify:** echo "ok"
""")

# Spec after one task completed by Claude
SAMPLE_SPEC_ONE_DONE = textwrap.dedent("""\
    # Test Spec

    ## Tasks

    ### t-1: First task
    DONE

    **Spec:** Do the first thing.

    **Verify:** echo "done"

    ### t-2: Second task
    DONE

    **Spec:** Do the second thing.

    **Verify:** echo "ok"

    ### t-3: Third task
    PENDING

    **Spec:** Do the third thing.

    **Verify:** echo "ok"
""")

# Spec with tasks added (self-evolution)
SAMPLE_SPEC_EVOLVED = textwrap.dedent("""\
    # Test Spec

    ## Tasks

    ### t-1: First task
    DONE

    **Spec:** Do the first thing.

    **Verify:** echo "done"

    ### t-2: Second task
    DONE

    **Spec:** Do the second thing.

    **Verify:** echo "ok"

    ### t-3: Third task
    PENDING

    **Spec:** Do the third thing.

    **Verify:** echo "ok"

    ### t-4: New self-evolved task
    PENDING

    **Spec:** Handle the edge case.

    **Verify:** echo "ok"
""")

ALL_DONE_SPEC = textwrap.dedent("""\
    # All Done Spec

    ## Tasks

    ### t-1: First task
    DONE

    **Spec:** Done.

    **Verify:** echo "done"

    ### t-2: Second task
    DONE

    **Spec:** Done.

    **Verify:** echo "done"
""")


class WorkerTestCase(unittest.TestCase):
    """Base test case that sets up temp dirs and a Worker instance."""

    def setUp(self) -> None:
        self._tmpdir = tempfile.mkdtemp(prefix="boi-worker-test-")
        self.state_dir = os.path.join(self._tmpdir, ".boi")
        self.queue_dir = os.path.join(self.state_dir, "queue")
        self.log_dir = os.path.join(self.state_dir, "logs")
        self.worktree = os.path.join(self._tmpdir, "worktree")
        os.makedirs(self.queue_dir, exist_ok=True)
        os.makedirs(self.log_dir, exist_ok=True)
        os.makedirs(self.worktree, exist_ok=True)

        self.spec_path = os.path.join(
            self.queue_dir, "q-test.spec.md"
        )
        self._write_spec(SAMPLE_SPEC)

        # Create minimal template files so generate_run_script works
        templates_dir = os.path.join(
            str(Path(__file__).resolve().parent.parent),
            "templates",
        )
        self._templates_exist = os.path.isdir(templates_dir)

    def _write_spec(self, content: str) -> None:
        with open(self.spec_path, "w") as f:
            f.write(content)

    def _make_worker(self, **kwargs) -> Worker:
        defaults = {
            "spec_id": "q-test",
            "worktree": self.worktree,
            "spec_path": self.spec_path,
            "iteration": 1,
            "state_dir": self.state_dir,
        }
        defaults.update(kwargs)
        return Worker(**defaults)

    def tearDown(self) -> None:
        shutil.rmtree(self._tmpdir, ignore_errors=True)


# ── launch_tmux tests ────────────────────────────────────────────────


class TestLaunchTmux(WorkerTestCase):
    """Tests for Worker.launch_tmux()."""

    @patch("worker.subprocess.run")
    def test_launch_creates_session(self, mock_run: MagicMock) -> None:
        """launch_tmux should create a detached tmux session."""
        worker = self._make_worker()
        worker.pre_counts = {"pending": 2, "done": 1, "total": 3}

        # has-session returns 1 (no stale session)
        # new-session returns 0
        # list-panes returns PID
        mock_run.side_effect = [
            MagicMock(returncode=1),    # has-session (no stale)
            MagicMock(returncode=0),    # new-session
            MagicMock(returncode=0, stdout="12345\n"),  # list-panes
        ]

        # Need the run script to exist for tmux command
        with open(worker.run_script, "w") as f:
            f.write("#!/bin/bash\necho ok\n")

        rc = worker.launch_tmux()
        self.assertEqual(rc, 0)

        # Verify new-session was called with correct args
        new_session_call = mock_run.call_args_list[1]
        cmd = new_session_call[0][0]
        self.assertIn("new-session", cmd)
        self.assertIn("-d", cmd)
        self.assertIn(worker.tmux_session, cmd)

    @patch("worker.subprocess.run")
    def test_launch_kills_stale_session(
        self, mock_run: MagicMock
    ) -> None:
        """launch_tmux should kill existing stale sessions."""
        worker = self._make_worker()
        worker.pre_counts = {"pending": 2, "done": 1, "total": 3}

        mock_run.side_effect = [
            MagicMock(returncode=0),    # has-session (stale exists)
            MagicMock(returncode=0),    # kill-session
            MagicMock(returncode=0),    # new-session
            MagicMock(returncode=0, stdout="12345\n"),  # list-panes
        ]

        with open(worker.run_script, "w") as f:
            f.write("#!/bin/bash\necho ok\n")

        rc = worker.launch_tmux()
        self.assertEqual(rc, 0)

        # Verify kill-session was called
        kill_call = mock_run.call_args_list[1]
        cmd = kill_call[0][0]
        self.assertIn("kill-session", cmd)

    @patch("worker.subprocess.run")
    def test_launch_removes_stale_exit_file(
        self, mock_run: MagicMock
    ) -> None:
        """launch_tmux should remove stale .exit file."""
        worker = self._make_worker()
        worker.pre_counts = {"pending": 2, "done": 1, "total": 3}

        # Create a stale exit file
        with open(worker.exit_file, "w") as f:
            f.write("1")

        mock_run.side_effect = [
            MagicMock(returncode=1),    # has-session
            MagicMock(returncode=0),    # new-session
            MagicMock(returncode=0, stdout="12345\n"),  # list-panes
        ]

        with open(worker.run_script, "w") as f:
            f.write("#!/bin/bash\necho ok\n")

        rc = worker.launch_tmux()
        self.assertEqual(rc, 0)
        self.assertFalse(
            os.path.exists(worker.exit_file),
            "Stale exit file should be removed before launch",
        )

    @patch("worker.subprocess.run")
    def test_launch_writes_pid_file(
        self, mock_run: MagicMock
    ) -> None:
        """launch_tmux should write the pane PID to a .pid file."""
        worker = self._make_worker()
        worker.pre_counts = {"pending": 2, "done": 1, "total": 3}

        mock_run.side_effect = [
            MagicMock(returncode=1),    # has-session
            MagicMock(returncode=0),    # new-session
            MagicMock(returncode=0, stdout="99999\n"),  # list-panes
        ]

        with open(worker.run_script, "w") as f:
            f.write("#!/bin/bash\necho ok\n")

        rc = worker.launch_tmux()
        self.assertEqual(rc, 0)

        pid_file = os.path.join(
            worker.queue_dir, "q-test.pid"
        )
        self.assertTrue(os.path.isfile(pid_file))
        with open(pid_file) as f:
            self.assertEqual(f.read().strip(), "99999")

    @patch("worker.subprocess.run")
    def test_launch_fails_if_tmux_errors(
        self, mock_run: MagicMock
    ) -> None:
        """launch_tmux returns 1 if tmux new-session fails."""
        worker = self._make_worker()
        worker.pre_counts = {"pending": 2, "done": 1, "total": 3}

        mock_run.side_effect = [
            MagicMock(returncode=1),  # has-session
            subprocess.CalledProcessError(
                1, "tmux", stderr="server exited"
            ),
        ]

        with open(worker.run_script, "w") as f:
            f.write("#!/bin/bash\necho ok\n")

        rc = worker.launch_tmux()
        self.assertEqual(rc, 1)

    @patch("worker.subprocess.run")
    def test_launch_fails_if_no_pane_pid(
        self, mock_run: MagicMock
    ) -> None:
        """launch_tmux returns 1 if pane PID is empty."""
        worker = self._make_worker()
        worker.pre_counts = {"pending": 2, "done": 1, "total": 3}

        mock_run.side_effect = [
            MagicMock(returncode=1),    # has-session
            MagicMock(returncode=0),    # new-session
            MagicMock(returncode=0, stdout=""),  # list-panes empty
        ]

        with open(worker.run_script, "w") as f:
            f.write("#!/bin/bash\necho ok\n")

        rc = worker.launch_tmux()
        self.assertEqual(rc, 1)


# ── wait_for_tmux tests ──────────────────────────────────────────────


class TestWaitForTmux(WorkerTestCase):
    """Tests for Worker.wait_for_tmux()."""

    @patch("worker.time.sleep")
    @patch("worker.subprocess.run")
    def test_wait_returns_exit_code(
        self, mock_run: MagicMock, mock_sleep: MagicMock
    ) -> None:
        """wait_for_tmux returns exit code from .exit file."""
        worker = self._make_worker()
        worker.pre_counts = {"pending": 2, "done": 1, "total": 3}

        # First call: session exists. Second: gone.
        mock_run.side_effect = [
            MagicMock(returncode=0),  # has-session (alive)
            MagicMock(returncode=1),  # has-session (gone)
        ]

        # Write exit file
        with open(worker.exit_file, "w") as f:
            f.write("0")

        exit_code = worker.wait_for_tmux()
        self.assertEqual(exit_code, 0)

    @patch("worker.time.sleep")
    @patch("worker.subprocess.run")
    def test_wait_returns_nonzero_exit(
        self, mock_run: MagicMock, mock_sleep: MagicMock
    ) -> None:
        """wait_for_tmux returns non-zero exit codes."""
        worker = self._make_worker()
        worker.pre_counts = {"pending": 2, "done": 1, "total": 3}

        mock_run.side_effect = [
            MagicMock(returncode=1),  # has-session (gone immediately)
        ]

        with open(worker.exit_file, "w") as f:
            f.write("2")

        exit_code = worker.wait_for_tmux()
        self.assertEqual(exit_code, 2)

    @patch("worker.time.sleep")
    @patch("worker.subprocess.run")
    def test_wait_returns_1_if_no_exit_file(
        self, mock_run: MagicMock, mock_sleep: MagicMock
    ) -> None:
        """wait_for_tmux returns 1 if .exit file is missing."""
        worker = self._make_worker()
        worker.pre_counts = {"pending": 2, "done": 1, "total": 3}

        mock_run.side_effect = [
            MagicMock(returncode=1),  # has-session (gone)
        ]

        exit_code = worker.wait_for_tmux()
        self.assertEqual(exit_code, 1)

    @patch("worker.time.monotonic")
    @patch("worker.time.sleep")
    @patch("worker.subprocess.run")
    def test_wait_raises_timeout(
        self,
        mock_run: MagicMock,
        mock_sleep: MagicMock,
        mock_monotonic: MagicMock,
    ) -> None:
        """wait_for_tmux raises TimeoutError when timeout exceeded."""
        worker = self._make_worker(timeout_seconds=30)
        worker.pre_counts = {"pending": 2, "done": 1, "total": 3}

        # Session always alive
        mock_run.return_value = MagicMock(returncode=0)

        # Simulate time passing: first call at 0, second at 31
        mock_monotonic.side_effect = [0.0, 31.0]

        with self.assertRaises(TimeoutError):
            worker.wait_for_tmux()

    @patch("worker.time.monotonic")
    @patch("worker.time.sleep")
    @patch("worker.subprocess.run")
    def test_wait_no_timeout_when_none(
        self,
        mock_run: MagicMock,
        mock_sleep: MagicMock,
        mock_monotonic: MagicMock,
    ) -> None:
        """wait_for_tmux does not timeout when timeout_seconds=None."""
        worker = self._make_worker(timeout_seconds=None)
        worker.pre_counts = {"pending": 2, "done": 1, "total": 3}

        # Session alive twice, then gone
        mock_run.side_effect = [
            MagicMock(returncode=0),  # alive
            MagicMock(returncode=0),  # alive
            MagicMock(returncode=1),  # gone
        ]
        mock_monotonic.side_effect = [0.0, 100.0, 200.0]

        with open(worker.exit_file, "w") as f:
            f.write("0")

        exit_code = worker.wait_for_tmux()
        self.assertEqual(exit_code, 0)

    @patch("worker.time.sleep")
    @patch("worker.subprocess.run")
    def test_wait_polls_at_interval(
        self, mock_run: MagicMock, mock_sleep: MagicMock
    ) -> None:
        """wait_for_tmux sleeps TMUX_POLL_INTERVAL between polls."""
        worker = self._make_worker()
        worker.pre_counts = {"pending": 2, "done": 1, "total": 3}

        mock_run.side_effect = [
            MagicMock(returncode=0),  # alive
            MagicMock(returncode=0),  # alive
            MagicMock(returncode=1),  # gone
        ]

        with open(worker.exit_file, "w") as f:
            f.write("0")

        worker.wait_for_tmux()
        # Should have slept twice (once per alive poll)
        self.assertEqual(mock_sleep.call_count, 2)
        from worker import TMUX_POLL_INTERVAL

        mock_sleep.assert_called_with(TMUX_POLL_INTERVAL)


# ── post_process tests ───────────────────────────────────────────────


class TestPostProcess(WorkerTestCase):
    """Tests for Worker.post_process()."""

    def test_post_process_writes_iteration_json(self) -> None:
        """post_process writes iteration metadata JSON file."""
        worker = self._make_worker()
        worker.pre_counts = {
            "pending": 2, "done": 1, "skipped": 1, "total": 4,
        }

        # Simulate Claude completing one task
        self._write_spec(SAMPLE_SPEC_ONE_DONE)

        # Write exit file
        with open(worker.exit_file, "w") as f:
            f.write("0")

        worker.post_process()

        self.assertTrue(os.path.isfile(worker.iteration_file))
        with open(worker.iteration_file) as f:
            data = json.load(f)

        self.assertEqual(data["queue_id"], "q-test")
        self.assertEqual(data["iteration"], 1)
        self.assertEqual(data["exit_code"], 0)
        self.assertEqual(data["tasks_completed"], 1)
        self.assertEqual(data["tasks_added"], 0)
        self.assertEqual(data["tasks_skipped"], 0)

        self.assertEqual(data["pre_counts"]["pending"], 2)
        self.assertEqual(data["pre_counts"]["done"], 1)
        self.assertEqual(data["post_counts"]["pending"], 1)
        self.assertEqual(data["post_counts"]["done"], 2)

    def test_post_process_detects_self_evolution(self) -> None:
        """post_process detects when tasks are added (self-evolution)."""
        worker = self._make_worker()
        worker.pre_counts = {
            "pending": 2, "done": 1, "skipped": 0, "total": 3,
        }

        # Simulate Claude completing one task and adding one
        self._write_spec(SAMPLE_SPEC_EVOLVED)

        with open(worker.exit_file, "w") as f:
            f.write("0")

        worker.post_process()

        with open(worker.iteration_file) as f:
            data = json.load(f)

        self.assertEqual(data["tasks_completed"], 1)
        self.assertEqual(data["tasks_added"], 1)

    def test_post_process_clamps_negative_deltas(self) -> None:
        """Deltas are clamped to zero (never negative)."""
        worker = self._make_worker()
        # Pretend we started with more done than the spec shows now
        worker.pre_counts = {
            "pending": 0, "done": 5, "skipped": 2, "total": 7,
        }

        self._write_spec(SAMPLE_SPEC)  # 1 done, 2 pending

        with open(worker.exit_file, "w") as f:
            f.write("0")

        worker.post_process()

        with open(worker.iteration_file) as f:
            data = json.load(f)

        self.assertEqual(data["tasks_completed"], 0)
        self.assertEqual(data["tasks_added"], 0)
        self.assertEqual(data["tasks_skipped"], 0)

    def test_post_process_handles_missing_exit_file(self) -> None:
        """post_process defaults exit_code to 1 if .exit file missing."""
        worker = self._make_worker()
        worker.pre_counts = {
            "pending": 2, "done": 1, "skipped": 0, "total": 3,
        }

        # No exit file created

        worker.post_process()

        with open(worker.iteration_file) as f:
            data = json.load(f)

        self.assertEqual(data["exit_code"], 1)


# ── run() timeout integration ────────────────────────────────────────


class TestRunTimeout(WorkerTestCase):
    """Tests for timeout handling in Worker.run()."""

    @patch("worker.Worker._kill_tmux_session")
    @patch("worker.Worker.wait_for_tmux")
    @patch("worker.Worker.launch_tmux")
    @patch("worker.Worker.generate_run_script")
    def test_run_timeout_writes_exit_124(
        self,
        mock_gen: MagicMock,
        mock_launch: MagicMock,
        mock_wait: MagicMock,
        mock_kill: MagicMock,
    ) -> None:
        """run() writes exit code 124 and kills tmux on timeout."""
        worker = self._make_worker(timeout_seconds=30)
        mock_launch.return_value = 0
        mock_wait.side_effect = TimeoutError("timed out")

        exit_code = worker.run()
        self.assertEqual(exit_code, 124)

        # Verify .exit file contains 124
        with open(worker.exit_file) as f:
            self.assertEqual(f.read().strip(), "124")

        # Verify tmux session was killed
        mock_kill.assert_called_once()

    @patch("worker.Worker.post_process")
    @patch("worker.Worker.wait_for_tmux")
    @patch("worker.Worker.launch_tmux")
    @patch("worker.Worker.generate_run_script")
    def test_run_timeout_still_calls_post_process(
        self,
        mock_gen: MagicMock,
        mock_launch: MagicMock,
        mock_wait: MagicMock,
        mock_post: MagicMock,
    ) -> None:
        """run() calls post_process even after timeout."""
        worker = self._make_worker(timeout_seconds=30)
        mock_launch.return_value = 0
        mock_wait.side_effect = TimeoutError("timed out")

        worker.run()
        mock_post.assert_called_once()


# ── Session name tests ───────────────────────────────────────────────


class TestTmuxSessionName(WorkerTestCase):
    """Tests for tmux session naming."""

    def test_session_name_without_worker_id(self) -> None:
        """Session name is boi-{spec_id} without worker_id."""
        worker = self._make_worker(worker_id="")
        self.assertEqual(worker.tmux_session, "boi-q-test")

    def test_session_name_with_worker_id(self) -> None:
        """Session name includes worker_id when set."""
        worker = self._make_worker(worker_id="w1")
        self.assertEqual(worker.tmux_session, "boi-q-test-w1")


# ── CLI argparse tests ───────────────────────────────────────────────


class TestMainArgparse(WorkerTestCase):
    """Tests for the main() CLI entry point."""

    @patch("worker.Worker.run")
    def test_main_parses_positional_args(
        self, mock_run: MagicMock
    ) -> None:
        """main() parses positional args correctly."""
        mock_run.return_value = 0

        with patch(
            "sys.argv",
            [
                "worker.py",
                "q-001",
                self.worktree,
                self.spec_path,
                "3",
            ],
        ):
            exit_code = main()

        self.assertEqual(exit_code, 0)
        mock_run.assert_called_once()

    @patch("worker.Worker.run")
    def test_main_parses_optional_args(
        self, mock_run: MagicMock
    ) -> None:
        """main() parses --phase, --timeout, --mode, --project."""
        mock_run.return_value = 0

        with patch(
            "sys.argv",
            [
                "worker.py",
                "q-002",
                self.worktree,
                self.spec_path,
                "5",
                "--phase", "critic",
                "--timeout", "120",
                "--mode", "challenge",
                "--project", "myproject",
                "--state-dir", self.state_dir,
            ],
        ):
            exit_code = main()

        self.assertEqual(exit_code, 0)

    def test_main_missing_args_exits_2(self) -> None:
        """main() exits with 2 when required args missing."""
        with patch("sys.argv", ["worker.py"]):
            with self.assertRaises(SystemExit) as ctx:
                main()
            self.assertEqual(ctx.exception.code, 2)

    def test_main_invalid_phase_exits_2(self) -> None:
        """main() exits with 2 for invalid --phase value."""
        with patch(
            "sys.argv",
            [
                "worker.py",
                "q-001",
                self.worktree,
                self.spec_path,
                "1",
                "--phase", "bogus",
            ],
        ):
            with self.assertRaises(SystemExit) as ctx:
                main()
            self.assertEqual(ctx.exception.code, 2)


# ── _read_exit_code tests ────────────────────────────────────────────


class TestReadExitCode(WorkerTestCase):
    """Tests for Worker._read_exit_code()."""

    def test_reads_valid_exit_code(self) -> None:
        worker = self._make_worker()
        with open(worker.exit_file, "w") as f:
            f.write("42\n")
        self.assertEqual(worker._read_exit_code(), 42)

    def test_returns_1_for_missing_file(self) -> None:
        worker = self._make_worker()
        self.assertEqual(worker._read_exit_code(), 1)

    def test_returns_1_for_invalid_content(self) -> None:
        worker = self._make_worker()
        with open(worker.exit_file, "w") as f:
            f.write("not_a_number\n")
        self.assertEqual(worker._read_exit_code(), 1)

    def test_reads_zero(self) -> None:
        worker = self._make_worker()
        with open(worker.exit_file, "w") as f:
            f.write("0")
        self.assertEqual(worker._read_exit_code(), 0)

    def test_reads_timeout_code_124(self) -> None:
        worker = self._make_worker()
        with open(worker.exit_file, "w") as f:
            f.write("124")
        self.assertEqual(worker._read_exit_code(), 124)


# ── _tmux_session_exists / _kill_tmux_session tests ──────────────────


class TestTmuxHelpers(WorkerTestCase):
    """Tests for tmux helper methods."""

    @patch("worker.subprocess.run")
    def test_session_exists_true(
        self, mock_run: MagicMock
    ) -> None:
        worker = self._make_worker()
        mock_run.return_value = MagicMock(returncode=0)
        self.assertTrue(worker._tmux_session_exists())

    @patch("worker.subprocess.run")
    def test_session_exists_false(
        self, mock_run: MagicMock
    ) -> None:
        worker = self._make_worker()
        mock_run.return_value = MagicMock(returncode=1)
        self.assertFalse(worker._tmux_session_exists())

    @patch("worker.subprocess.run")
    def test_kill_session_calls_tmux(
        self, mock_run: MagicMock
    ) -> None:
        worker = self._make_worker()
        worker._kill_tmux_session()
        cmd = mock_run.call_args[0][0]
        self.assertIn("kill-session", cmd)
        self.assertIn(worker.tmux_session, cmd)
        self.assertIn("-L", cmd)
        self.assertIn(TMUX_SOCKET, cmd)


# ── WorkerHooks tests ─────────────────────────────────────────────


class TestWorkerHooks(WorkerTestCase):
    """Tests for the WorkerHooks extension point."""

    def test_default_hooks_returns_empty(self) -> None:
        """DefaultWorkerHooks.pre_iteration returns empty string."""
        hooks = WorkerHooks()
        result = hooks.pre_iteration("/some/spec.md", "/some/worktree")
        self.assertEqual(result, "")

    def test_worker_hooks_output_included_in_prompt(self) -> None:
        """Hook output is injected into the generated prompt."""
        # Create a minimal template with the WORKTREE_CONTEXT placeholder
        templates_dir = os.path.join(self._tmpdir, "templates")
        modes_dir = os.path.join(templates_dir, "modes")
        os.makedirs(modes_dir, exist_ok=True)

        template = (
            "# Prompt\n"
            "{{ITERATION}} {{QUEUE_ID}} {{SPEC_PATH}}\n"
            "{{PENDING_COUNT}}\n"
            "{{MODE_RULES}}\n"
            "{{PROJECT}}\n"
            "{{PROJECT_CONTEXT}}\n"
            "{{WORKTREE_CONTEXT}}\n"
            "{{SPEC_CONTENT}}\n"
        )
        with open(
            os.path.join(templates_dir, "worker-prompt.md"), "w"
        ) as f:
            f.write(template)

        with open(
            os.path.join(modes_dir, "execute.md"), "w"
        ) as f:
            f.write("## Mode: Execute\n")

        # Create a custom hooks subclass
        class TestHooks(WorkerHooks):
            def pre_iteration(
                self, spec_path: str, worktree: str
            ) -> str:
                return (
                    f"## Injected Context\n\n"
                    f"Worktree: {worktree}\n"
                    f"Spec: {spec_path}\n"
                )

        hooks = TestHooks()
        worker = self._make_worker(hooks=hooks)
        worker.pre_counts = {
            "pending": 2, "done": 1, "skipped": 0, "total": 3,
        }

        # Patch the template path to use our temp template
        with patch(
            "worker.TEMPLATE_PATH",
            os.path.join(templates_dir, "worker-prompt.md"),
        ), patch(
            "worker.MODES_DIR", modes_dir,
        ):
            worker._generate_execute_prompt()

        # Read the generated prompt
        with open(worker.prompt_file) as f:
            prompt_content = f.read()

        self.assertIn("## Injected Context", prompt_content)
        self.assertIn(
            f"Worktree: {self.worktree}", prompt_content
        )
        self.assertIn(
            f"Spec: {self.spec_path}", prompt_content
        )

    def test_worker_hooks_none_produces_empty_context(self) -> None:
        """No hooks (None) results in empty worktree context."""
        templates_dir = os.path.join(self._tmpdir, "templates")
        modes_dir = os.path.join(templates_dir, "modes")
        os.makedirs(modes_dir, exist_ok=True)

        template = "BEFORE|{{WORKTREE_CONTEXT}}|AFTER\n"
        with open(
            os.path.join(templates_dir, "worker-prompt.md"), "w"
        ) as f:
            f.write(template)

        # Provide all required placeholders
        template = (
            "{{ITERATION}}{{QUEUE_ID}}{{SPEC_PATH}}"
            "{{PENDING_COUNT}}{{MODE_RULES}}{{PROJECT}}"
            "{{PROJECT_CONTEXT}}"
            "BEFORE|{{WORKTREE_CONTEXT}}|AFTER"
            "{{SPEC_CONTENT}}"
        )
        with open(
            os.path.join(templates_dir, "worker-prompt.md"), "w"
        ) as f:
            f.write(template)

        with open(
            os.path.join(modes_dir, "execute.md"), "w"
        ) as f:
            f.write("")

        worker = self._make_worker(hooks=None)
        worker.pre_counts = {
            "pending": 2, "done": 1, "skipped": 0, "total": 3,
        }

        with patch(
            "worker.TEMPLATE_PATH",
            os.path.join(templates_dir, "worker-prompt.md"),
        ), patch(
            "worker.MODES_DIR", modes_dir,
        ):
            worker._generate_execute_prompt()

        with open(worker.prompt_file) as f:
            prompt_content = f.read()

        self.assertIn("BEFORE||AFTER", prompt_content)

    def test_worker_hooks_exception_is_caught(self) -> None:
        """If hook raises, worker catches it and injects nothing."""
        templates_dir = os.path.join(self._tmpdir, "templates")
        modes_dir = os.path.join(templates_dir, "modes")
        os.makedirs(modes_dir, exist_ok=True)

        template = (
            "{{ITERATION}}{{QUEUE_ID}}{{SPEC_PATH}}"
            "{{PENDING_COUNT}}{{MODE_RULES}}{{PROJECT}}"
            "{{PROJECT_CONTEXT}}"
            "BEFORE|{{WORKTREE_CONTEXT}}|AFTER"
            "{{SPEC_CONTENT}}"
        )
        with open(
            os.path.join(templates_dir, "worker-prompt.md"), "w"
        ) as f:
            f.write(template)

        with open(
            os.path.join(modes_dir, "execute.md"), "w"
        ) as f:
            f.write("")

        class BrokenHooks(WorkerHooks):
            def pre_iteration(
                self, spec_path: str, worktree: str
            ) -> str:
                raise RuntimeError("Hook exploded")

        worker = self._make_worker(hooks=BrokenHooks())
        worker.pre_counts = {
            "pending": 2, "done": 1, "skipped": 0, "total": 3,
        }

        with patch(
            "worker.TEMPLATE_PATH",
            os.path.join(templates_dir, "worker-prompt.md"),
        ), patch(
            "worker.MODES_DIR", modes_dir,
        ):
            # Should not raise
            worker._generate_execute_prompt()

        with open(worker.prompt_file) as f:
            prompt_content = f.read()

        # Hook output should be empty since exception was caught
        self.assertIn("BEFORE||AFTER", prompt_content)
        self.assertNotIn("Hook exploded", prompt_content)


if __name__ == "__main__":
    unittest.main()
