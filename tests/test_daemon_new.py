# test_daemon_new.py — Unit tests for the Python BOI daemon (daemon.py).
#
# Tests cover: dispatch_specs, assign_spec_to_worker, launch_worker,
# check_worker_completions, handle_worker_timeout, process_worker_completion.
# Uses mock subprocess to avoid spawning real workers.

import json
import os
import signal
import subprocess
import sys
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest.mock import MagicMock, patch

# Add parent directory to path so we can import lib modules
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

# Save real Popen class before any patching
_real_popen = subprocess.Popen

from daemon import Daemon
from lib.db import Database


SAMPLE_SPEC = """\
# Test Spec

## Tasks

1. PENDING: First task
   Verify: echo ok

2. PENDING: Second task
   Verify: echo ok
"""

SAMPLE_SPEC_ALL_DONE = """\
# Test Spec

## Tasks

### t-1: First task
DONE

**Verify:** echo ok

### t-2: Second task
DONE

**Verify:** echo ok
"""


class DaemonTestCase(unittest.TestCase):
    """Base test case that creates a temp dir with config and database."""

    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.state_dir = self._tmpdir.name
        self.db_path = os.path.join(self.state_dir, "boi.db")
        self.queue_dir = os.path.join(self.state_dir, "queue")
        self.log_dir = os.path.join(self.state_dir, "logs")
        os.makedirs(self.queue_dir, exist_ok=True)
        os.makedirs(self.log_dir, exist_ok=True)

        # Create worktree directories for workers
        self.worktree_a = os.path.join(self.state_dir, "worktree-a")
        self.worktree_b = os.path.join(self.state_dir, "worktree-b")
        os.makedirs(self.worktree_a, exist_ok=True)
        os.makedirs(self.worktree_b, exist_ok=True)

        # Write config.json
        self.config_path = os.path.join(self.state_dir, "config.json")
        config = {
            "workers": [
                {"id": "w1", "worktree_path": self.worktree_a},
                {"id": "w2", "worktree_path": self.worktree_b},
            ],
        }
        with open(self.config_path, "w", encoding="utf-8") as f:
            json.dump(config, f)

        # Create a spec file for enqueue
        self.spec_file = os.path.join(self.state_dir, "test-spec.md")
        with open(self.spec_file, "w", encoding="utf-8") as f:
            f.write(SAMPLE_SPEC)

        # Build daemon (signal handlers are installed, but harmless in tests)
        self.daemon = Daemon(
            config_path=self.config_path,
            db_path=self.db_path,
            state_dir=self.state_dir,
        )
        # Load workers into DB
        self.daemon.load_workers()

    def tearDown(self) -> None:
        self.daemon.db.close()
        self._tmpdir.cleanup()

    def _enqueue_spec(
        self,
        spec_path: str | None = None,
        phase: str = "execute",
        timeout: int | None = None,
    ) -> str:
        """Helper to enqueue a spec and return the queue ID."""
        path = spec_path or self.spec_file
        result = self.daemon.db.enqueue(spec_path=path)
        spec_id = result["id"]

        # Set phase and timeout if non-default
        updates = []
        params: list = []
        if phase != "execute":
            updates.append("phase = ?")
            params.append(phase)
        if timeout is not None:
            updates.append("worker_timeout_seconds = ?")
            params.append(timeout)
        if updates:
            params.append(spec_id)
            self.daemon.db.conn.execute(
                f"UPDATE specs SET {', '.join(updates)} WHERE id = ?",
                params,
            )
            self.daemon.db.conn.commit()

        return spec_id

    def _make_mock_proc(self, pid: int = 12345) -> MagicMock:
        """Create a mock subprocess.Popen with a given PID."""
        proc = MagicMock(spec=_real_popen)
        proc.pid = pid
        proc.poll.return_value = None  # still running
        return proc


# ─── dispatch_specs tests ──────────────────────────────────────────────


class TestDispatchSpecs(DaemonTestCase):
    """Test Daemon.dispatch_specs()."""

    @patch.object(Daemon, "launch_worker")
    def test_dispatch_assigns_spec_to_free_worker(
        self, mock_launch: MagicMock
    ) -> None:
        """Enqueue one spec, dispatch it. Worker should be busy after."""
        mock_launch.return_value = self._make_mock_proc(pid=1001)

        spec_id = self._enqueue_spec()
        self.daemon.dispatch_specs()

        # Verify spec is now running
        spec = self.daemon.db.get_spec(spec_id)
        self.assertIsNotNone(spec)
        self.assertEqual(spec["status"], "running")

        # Verify worker is assigned
        worker = self.daemon.db.get_worker("w1")
        self.assertEqual(worker["current_spec_id"], spec_id)
        self.assertEqual(worker["current_pid"], 1001)

        # Verify proc is tracked
        self.assertIn("w1", self.daemon.worker_procs)

    @patch.object(Daemon, "launch_worker")
    def test_dispatch_assigns_multiple_specs(
        self, mock_launch: MagicMock
    ) -> None:
        """Two specs, two workers. Both should get assigned."""
        mock_launch.side_effect = [
            self._make_mock_proc(pid=1001),
            self._make_mock_proc(pid=1002),
        ]

        id1 = self._enqueue_spec()
        # Create a second spec file so enqueue doesn't deduplicate
        spec2 = os.path.join(self.state_dir, "spec2.md")
        with open(spec2, "w", encoding="utf-8") as f:
            f.write(SAMPLE_SPEC)
        id2 = self._enqueue_spec(spec_path=spec2)

        self.daemon.dispatch_specs()

        s1 = self.daemon.db.get_spec(id1)
        s2 = self.daemon.db.get_spec(id2)
        self.assertEqual(s1["status"], "running")
        self.assertEqual(s2["status"], "running")
        self.assertEqual(len(self.daemon.worker_procs), 2)

    @patch.object(Daemon, "launch_worker")
    def test_dispatch_stops_when_no_free_worker(
        self, mock_launch: MagicMock
    ) -> None:
        """Three specs, two workers. Third spec stays queued."""
        mock_launch.side_effect = [
            self._make_mock_proc(pid=1001),
            self._make_mock_proc(pid=1002),
        ]

        self._enqueue_spec()
        spec2 = os.path.join(self.state_dir, "spec2.md")
        with open(spec2, "w", encoding="utf-8") as f:
            f.write(SAMPLE_SPEC)
        self._enqueue_spec(spec_path=spec2)

        spec3 = os.path.join(self.state_dir, "spec3.md")
        with open(spec3, "w", encoding="utf-8") as f:
            f.write(SAMPLE_SPEC)
        id3 = self._enqueue_spec(spec_path=spec3)

        self.daemon.dispatch_specs()

        s3 = self.daemon.db.get_spec(id3)
        self.assertIn(s3["status"], ("queued", "requeued"))
        self.assertEqual(len(self.daemon.worker_procs), 2)

    def test_dispatch_no_specs(self) -> None:
        """No specs queued. dispatch_specs does nothing without error."""
        self.daemon.dispatch_specs()
        self.assertEqual(len(self.daemon.worker_procs), 0)

    @patch.object(Daemon, "launch_worker")
    def test_dispatch_respects_shutdown_flag(
        self, mock_launch: MagicMock
    ) -> None:
        """If shutdown requested, dispatch exits immediately."""
        mock_launch.return_value = self._make_mock_proc()
        self._enqueue_spec()
        self.daemon._shutdown_requested = True

        self.daemon.dispatch_specs()

        # Spec should not have been dispatched
        mock_launch.assert_not_called()


# ─── assign_spec_to_worker tests ──────────────────────────────────────


class TestAssignSpecToWorker(DaemonTestCase):
    """Test Daemon.assign_spec_to_worker()."""

    @patch.object(Daemon, "launch_worker")
    def test_assign_sets_running_before_launch(
        self, mock_launch: MagicMock
    ) -> None:
        """Spec must be 'running' and worker assigned before launch."""
        mock_launch.return_value = self._make_mock_proc(pid=2001)

        spec_id = self._enqueue_spec()
        spec = self.daemon.db.get_spec(spec_id)
        # pick_next_spec transitions to 'assigning'
        spec = self.daemon.db.pick_next_spec()
        worker = self.daemon.db.get_worker("w1")

        self.daemon.assign_spec_to_worker(spec, worker)

        # Verify launch was called
        mock_launch.assert_called_once()
        call_kwargs = mock_launch.call_args
        self.assertEqual(call_kwargs.kwargs["spec_id"], spec_id)
        self.assertEqual(call_kwargs.kwargs["worker_id"], "w1")

    @patch.object(Daemon, "launch_worker")
    def test_assign_registers_process(
        self, mock_launch: MagicMock
    ) -> None:
        """PID is registered in the processes table."""
        mock_launch.return_value = self._make_mock_proc(pid=2002)

        spec_id = self._enqueue_spec()
        spec = self.daemon.db.pick_next_spec()
        worker = self.daemon.db.get_worker("w1")

        self.daemon.assign_spec_to_worker(spec, worker)

        # Check processes table
        procs = self.daemon.db.get_active_processes()
        self.assertEqual(len(procs), 1)
        self.assertEqual(procs[0]["pid"], 2002)
        self.assertEqual(procs[0]["spec_id"], spec_id)
        self.assertEqual(procs[0]["worker_id"], "w1")

    @patch.object(Daemon, "launch_worker")
    def test_assign_frees_worker_on_launch_failure(
        self, mock_launch: MagicMock
    ) -> None:
        """If launch_worker raises, worker is freed and spec is requeued."""
        mock_launch.side_effect = OSError("mock launch failure")

        spec_id = self._enqueue_spec()
        spec = self.daemon.db.pick_next_spec()
        worker = self.daemon.db.get_worker("w1")

        # Should not raise
        self.daemon.assign_spec_to_worker(spec, worker)

        # Worker should be free
        w = self.daemon.db.get_worker("w1")
        self.assertIsNone(w["current_spec_id"])

        # Spec should be requeued
        s = self.daemon.db.get_spec(spec_id)
        self.assertEqual(s["status"], "requeued")

    @patch.object(Daemon, "launch_worker")
    def test_assign_passes_correct_phase(
        self, mock_launch: MagicMock
    ) -> None:
        """Critic phase is passed through to launch_worker and DB."""
        mock_launch.return_value = self._make_mock_proc(pid=3001)

        spec_id = self._enqueue_spec(phase="critic")
        spec = self.daemon.db.pick_next_spec()
        worker = self.daemon.db.get_worker("w1")

        self.daemon.assign_spec_to_worker(spec, worker)

        call_kwargs = mock_launch.call_args.kwargs
        self.assertEqual(call_kwargs["phase"], "critic")

        # Worker should record phase
        w = self.daemon.db.get_worker("w1")
        self.assertEqual(w["current_phase"], "critic")

    @patch.object(Daemon, "launch_worker")
    def test_assign_passes_timeout(
        self, mock_launch: MagicMock
    ) -> None:
        """Spec-level worker_timeout_seconds is passed to launch."""
        mock_launch.return_value = self._make_mock_proc(pid=3002)

        spec_id = self._enqueue_spec(timeout=600)
        spec = self.daemon.db.pick_next_spec()
        worker = self.daemon.db.get_worker("w1")

        self.daemon.assign_spec_to_worker(spec, worker)

        call_kwargs = mock_launch.call_args.kwargs
        self.assertEqual(call_kwargs["timeout"], 600)

    @patch.object(Daemon, "launch_worker")
    def test_assign_increments_iteration_for_execute(
        self, mock_launch: MagicMock
    ) -> None:
        """Execute phase increments iteration from 0 to 1."""
        mock_launch.return_value = self._make_mock_proc(pid=4001)

        spec_id = self._enqueue_spec()
        spec = self.daemon.db.pick_next_spec()
        worker = self.daemon.db.get_worker("w1")

        self.daemon.assign_spec_to_worker(spec, worker)

        s = self.daemon.db.get_spec(spec_id)
        self.assertEqual(s["iteration"], 1)
        self.assertEqual(s["status"], "running")


# ─── launch_worker tests ──────────────────────────────────────────────


class TestLaunchWorker(DaemonTestCase):
    """Test Daemon.launch_worker() builds correct command and spawns."""

    @patch("subprocess.Popen")
    def test_launch_builds_correct_command(
        self, mock_popen: MagicMock
    ) -> None:
        """Verify command includes worker.py, args, and phase flag."""
        mock_proc = self._make_mock_proc(pid=5001)
        mock_popen.return_value = mock_proc

        spec_path = os.path.join(self.queue_dir, "q-001.spec.md")
        with open(spec_path, "w") as f:
            f.write("test")

        result = self.daemon.launch_worker(
            spec_id="q-001",
            worktree=self.worktree_a,
            spec_path=spec_path,
            iteration=3,
            phase="execute",
            worker_id="w1",
        )

        mock_popen.assert_called_once()
        cmd = mock_popen.call_args.args[0]

        # Verify command structure
        self.assertIn("worker.py", cmd[1])
        self.assertIn("q-001", cmd)
        self.assertIn(self.worktree_a, cmd)
        self.assertIn(spec_path, cmd)
        self.assertIn("3", cmd)
        self.assertIn("--phase", cmd)
        self.assertIn("execute", cmd)

    @patch("subprocess.Popen")
    def test_launch_includes_timeout_flag(
        self, mock_popen: MagicMock
    ) -> None:
        """Timeout is passed as --timeout CLI flag."""
        mock_popen.return_value = self._make_mock_proc()

        spec_path = os.path.join(self.queue_dir, "q-001.spec.md")
        with open(spec_path, "w") as f:
            f.write("test")

        self.daemon.launch_worker(
            spec_id="q-001",
            worktree=self.worktree_a,
            spec_path=spec_path,
            iteration=1,
            phase="execute",
            worker_id="w1",
            timeout=900,
        )

        cmd = mock_popen.call_args.args[0]
        self.assertIn("--timeout", cmd)
        self.assertIn("900", cmd)

    @patch("subprocess.Popen")
    def test_launch_no_timeout_omits_flag(
        self, mock_popen: MagicMock
    ) -> None:
        """When timeout is None, --timeout flag is omitted."""
        mock_popen.return_value = self._make_mock_proc()

        spec_path = os.path.join(self.queue_dir, "q-001.spec.md")
        with open(spec_path, "w") as f:
            f.write("test")

        self.daemon.launch_worker(
            spec_id="q-001",
            worktree=self.worktree_a,
            spec_path=spec_path,
            iteration=1,
            phase="execute",
            worker_id="w1",
            timeout=None,
        )

        cmd = mock_popen.call_args.args[0]
        self.assertNotIn("--timeout", cmd)

    @patch("subprocess.Popen")
    def test_launch_uses_start_new_session(
        self, mock_popen: MagicMock
    ) -> None:
        """Worker must be spawned with start_new_session=True."""
        mock_popen.return_value = self._make_mock_proc()

        spec_path = os.path.join(self.queue_dir, "q-001.spec.md")
        with open(spec_path, "w") as f:
            f.write("test")

        self.daemon.launch_worker(
            spec_id="q-001",
            worktree=self.worktree_a,
            spec_path=spec_path,
            iteration=1,
            phase="execute",
            worker_id="w1",
        )

        kwargs = mock_popen.call_args.kwargs
        self.assertTrue(kwargs.get("start_new_session"))

    @patch("subprocess.Popen")
    def test_launch_sets_worker_id_env(
        self, mock_popen: MagicMock
    ) -> None:
        """WORKER_ID environment variable is set to the worker ID."""
        mock_popen.return_value = self._make_mock_proc()

        spec_path = os.path.join(self.queue_dir, "q-001.spec.md")
        with open(spec_path, "w") as f:
            f.write("test")

        self.daemon.launch_worker(
            spec_id="q-001",
            worktree=self.worktree_a,
            spec_path=spec_path,
            iteration=1,
            phase="execute",
            worker_id="w1",
        )

        env = mock_popen.call_args.kwargs.get("env", {})
        self.assertEqual(env.get("WORKER_ID"), "w1")

    @patch("subprocess.Popen")
    def test_launch_sets_cwd_to_worktree(
        self, mock_popen: MagicMock
    ) -> None:
        """Subprocess cwd is set to the worker's worktree path."""
        mock_popen.return_value = self._make_mock_proc()

        spec_path = os.path.join(self.queue_dir, "q-001.spec.md")
        with open(spec_path, "w") as f:
            f.write("test")

        self.daemon.launch_worker(
            spec_id="q-001",
            worktree=self.worktree_a,
            spec_path=spec_path,
            iteration=1,
            phase="execute",
            worker_id="w1",
        )

        kwargs = mock_popen.call_args.kwargs
        self.assertEqual(kwargs["cwd"], self.worktree_a)

    @patch("subprocess.Popen")
    def test_launch_creates_log_file(
        self, mock_popen: MagicMock
    ) -> None:
        """Log file for the iteration is created in the log directory."""
        mock_popen.return_value = self._make_mock_proc()

        spec_path = os.path.join(self.queue_dir, "q-001.spec.md")
        with open(spec_path, "w") as f:
            f.write("test")

        self.daemon.launch_worker(
            spec_id="q-001",
            worktree=self.worktree_a,
            spec_path=spec_path,
            iteration=2,
            phase="execute",
            worker_id="w1",
        )

        expected_log = os.path.join(
            self.log_dir, "q-001-iter-2.log"
        )
        self.assertTrue(os.path.exists(expected_log))

    @patch("subprocess.Popen")
    def test_launch_critic_phase(
        self, mock_popen: MagicMock
    ) -> None:
        """Critic phase is passed correctly in the command."""
        mock_popen.return_value = self._make_mock_proc()

        spec_path = os.path.join(self.queue_dir, "q-001.spec.md")
        with open(spec_path, "w") as f:
            f.write("test")

        self.daemon.launch_worker(
            spec_id="q-001",
            worktree=self.worktree_a,
            spec_path=spec_path,
            iteration=1,
            phase="critic",
            worker_id="w1",
        )

        cmd = mock_popen.call_args.args[0]
        phase_idx = cmd.index("--phase")
        self.assertEqual(cmd[phase_idx + 1], "critic")


# ─── Integration-style tests (mock subprocess only) ───────────────────


class TestDispatchEndToEnd(DaemonTestCase):
    """End-to-end dispatch flow with mocked subprocess."""

    @patch("subprocess.Popen")
    def test_full_dispatch_flow(
        self, mock_popen: MagicMock
    ) -> None:
        """Enqueue -> dispatch -> verify all DB state is correct."""
        mock_popen.return_value = self._make_mock_proc(pid=9001)

        spec_id = self._enqueue_spec()

        # Before dispatch
        spec = self.daemon.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "queued")
        self.assertEqual(spec["iteration"], 0)

        # Dispatch
        self.daemon.dispatch_specs()

        # After dispatch
        spec = self.daemon.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "running")
        self.assertEqual(spec["iteration"], 1)
        self.assertEqual(spec["phase"], "execute")
        self.assertEqual(spec["last_worker"], "w1")
        self.assertIsNotNone(spec["first_running_at"])
        self.assertIsNotNone(spec["last_iteration_at"])

        # Worker state
        w1 = self.daemon.db.get_worker("w1")
        self.assertEqual(w1["current_spec_id"], spec_id)
        self.assertEqual(w1["current_pid"], 9001)
        self.assertEqual(w1["current_phase"], "execute")

        # Process tracking
        procs = self.daemon.db.get_active_processes()
        self.assertEqual(len(procs), 1)
        self.assertEqual(procs[0]["pid"], 9001)

        # Events
        events = self.daemon.db.get_events(spec_id=spec_id)
        event_types = [e["event_type"] for e in events]
        self.assertIn("running", event_types)
        self.assertIn("worker_assigned", event_types)
        self.assertIn("process_started", event_types)

    @patch("subprocess.Popen")
    def test_dispatch_decompose_phase(
        self, mock_popen: MagicMock
    ) -> None:
        """Decompose phase does not increment iteration."""
        mock_popen.return_value = self._make_mock_proc(pid=9002)

        spec_id = self._enqueue_spec(phase="decompose")
        self.daemon.dispatch_specs()

        spec = self.daemon.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "running")
        # decompose doesn't increment iteration
        self.assertEqual(spec["iteration"], 0)
        self.assertEqual(spec["phase"], "decompose")


# ─── check_worker_completions tests ────────────────────────────────────


class TestCheckWorkerCompletions(DaemonTestCase):
    """Test Daemon.check_worker_completions()."""

    @patch.object(Daemon, "launch_worker")
    @patch.object(Daemon, "_dispatch_phase_completion")
    def test_detects_exited_worker(
        self,
        mock_dispatch: MagicMock,
        mock_launch: MagicMock,
    ) -> None:
        """When a worker exits, process_worker_completion is called."""
        mock_proc = self._make_mock_proc(pid=7001)
        mock_launch.return_value = mock_proc

        spec_id = self._enqueue_spec()
        self.daemon.dispatch_specs()

        # Simulate worker exiting with code 0
        mock_proc.poll.return_value = 0

        self.daemon.check_worker_completions()

        # Worker should be freed
        w = self.daemon.db.get_worker("w1")
        self.assertIsNone(w["current_spec_id"])

        # Process should be ended in DB
        procs = self.daemon.db.get_active_processes()
        self.assertEqual(len(procs), 0)

        # Worker proc should be removed from tracking
        self.assertNotIn("w1", self.daemon.worker_procs)

    @patch.object(Daemon, "launch_worker")
    @patch.object(Daemon, "_dispatch_phase_completion")
    def test_ignores_still_running_worker(
        self,
        mock_dispatch: MagicMock,
        mock_launch: MagicMock,
    ) -> None:
        """Workers still running (poll returns None) are not processed."""
        mock_proc = self._make_mock_proc(pid=7002)
        mock_launch.return_value = mock_proc

        spec_id = self._enqueue_spec()
        self.daemon.dispatch_specs()

        # poll returns None = still running
        mock_proc.poll.return_value = None

        self.daemon.check_worker_completions()

        # Worker should still be busy
        w = self.daemon.db.get_worker("w1")
        self.assertEqual(w["current_spec_id"], spec_id)
        self.assertIn("w1", self.daemon.worker_procs)

    @patch.object(Daemon, "launch_worker")
    @patch.object(Daemon, "_dispatch_phase_completion")
    def test_handles_nonzero_exit_code(
        self,
        mock_dispatch: MagicMock,
        mock_launch: MagicMock,
    ) -> None:
        """Non-zero exit codes are passed through correctly."""
        mock_proc = self._make_mock_proc(pid=7003)
        mock_launch.return_value = mock_proc

        spec_id = self._enqueue_spec()
        self.daemon.dispatch_specs()

        # Worker exits with error
        mock_proc.poll.return_value = 1

        self.daemon.check_worker_completions()

        # Phase handler should have been called with exit_code=1
        mock_dispatch.assert_called_once_with(
            spec_id=spec_id,
            phase="execute",
            exit_code=1,
            worker_id="w1",
        )

    @patch.object(Daemon, "launch_worker")
    @patch.object(Daemon, "_dispatch_phase_completion")
    def test_handles_multiple_workers(
        self,
        mock_dispatch: MagicMock,
        mock_launch: MagicMock,
    ) -> None:
        """Multiple workers exiting in the same cycle are all handled."""
        proc1 = self._make_mock_proc(pid=7004)
        proc2 = self._make_mock_proc(pid=7005)
        mock_launch.side_effect = [proc1, proc2]

        self._enqueue_spec()
        spec2_file = os.path.join(self.state_dir, "spec2.md")
        with open(spec2_file, "w", encoding="utf-8") as f:
            f.write(SAMPLE_SPEC)
        self._enqueue_spec(spec_path=spec2_file)

        self.daemon.dispatch_specs()
        self.assertEqual(len(self.daemon.worker_procs), 2)

        # Both exit
        proc1.poll.return_value = 0
        proc2.poll.return_value = 0

        self.daemon.check_worker_completions()

        self.assertEqual(len(self.daemon.worker_procs), 0)
        self.assertEqual(mock_dispatch.call_count, 2)


# ─── is_worker_timed_out tests ─────────────────────────────────────────


class TestIsWorkerTimedOut(DaemonTestCase):
    """Test Daemon.is_worker_timed_out()."""

    @patch.object(Daemon, "launch_worker")
    def test_not_timed_out_within_limit(
        self, mock_launch: MagicMock
    ) -> None:
        """Worker within timeout returns False."""
        mock_launch.return_value = self._make_mock_proc(pid=8001)

        self._enqueue_spec(timeout=3600)
        self.daemon.dispatch_specs()

        self.assertFalse(self.daemon.is_worker_timed_out("w1"))

    def test_not_timed_out_no_worker(self) -> None:
        """Non-existent worker returns False."""
        self.assertFalse(
            self.daemon.is_worker_timed_out("nonexistent")
        )

    def test_not_timed_out_idle_worker(self) -> None:
        """Idle worker (no spec) returns False."""
        self.assertFalse(self.daemon.is_worker_timed_out("w1"))

    @patch.object(Daemon, "launch_worker")
    def test_timed_out_exceeded(
        self, mock_launch: MagicMock
    ) -> None:
        """Worker exceeding timeout returns True."""
        mock_launch.return_value = self._make_mock_proc(pid=8002)

        spec_id = self._enqueue_spec(timeout=1)
        self.daemon.dispatch_specs()

        # Backdate the worker's start_time to make it exceed timeout
        past_time = (
            datetime.now(timezone.utc) - timedelta(seconds=10)
        ).isoformat()
        self.daemon.db.conn.execute(
            "UPDATE workers SET start_time = ? WHERE id = ?",
            (past_time, "w1"),
        )
        self.daemon.db.conn.commit()

        self.assertTrue(self.daemon.is_worker_timed_out("w1"))

    @patch.object(Daemon, "launch_worker")
    def test_uses_default_timeout(
        self, mock_launch: MagicMock
    ) -> None:
        """When spec has no timeout, uses daemon's default."""
        mock_launch.return_value = self._make_mock_proc(pid=8003)

        # Enqueue without per-spec timeout
        spec_id = self._enqueue_spec()
        self.daemon.dispatch_specs()

        # Set a very short default timeout
        self.daemon.default_worker_timeout = 1

        # Backdate worker start time
        past_time = (
            datetime.now(timezone.utc) - timedelta(seconds=10)
        ).isoformat()
        self.daemon.db.conn.execute(
            "UPDATE workers SET start_time = ? WHERE id = ?",
            (past_time, "w1"),
        )
        self.daemon.db.conn.commit()

        self.assertTrue(self.daemon.is_worker_timed_out("w1"))


# ─── handle_worker_timeout tests ───────────────────────────────────────


class TestHandleWorkerTimeout(DaemonTestCase):
    """Test Daemon.handle_worker_timeout()."""

    @patch.object(Daemon, "launch_worker")
    @patch.object(Daemon, "_dispatch_phase_completion")
    @patch("os.getpgid")
    @patch("os.killpg")
    def test_kills_process_group_and_records_124(
        self,
        mock_killpg: MagicMock,
        mock_getpgid: MagicMock,
        mock_dispatch: MagicMock,
        mock_launch: MagicMock,
    ) -> None:
        """Timeout kills process group and records exit code 124."""
        mock_proc = self._make_mock_proc(pid=9101)
        # After SIGTERM, proc exits during the wait loop.
        # poll() is called: (1) in while loop -> None (still running),
        # (2) in while loop -> 0 (exited, break), (3) in SIGKILL
        # check -> 0 (already exited, skip SIGKILL).
        mock_proc.poll.side_effect = [None, 0, 0]
        mock_proc.wait.return_value = None
        mock_launch.return_value = mock_proc
        mock_getpgid.return_value = 9101

        spec_id = self._enqueue_spec()
        self.daemon.dispatch_specs()

        self.daemon.handle_worker_timeout("w1")

        # SIGTERM should have been sent to process group
        mock_killpg.assert_any_call(9101, signal.SIGTERM)

        # Phase handler should receive exit code 124
        mock_dispatch.assert_called_once_with(
            spec_id=spec_id,
            phase="execute",
            exit_code=124,
            worker_id="w1",
        )

        # Worker should be freed
        w = self.daemon.db.get_worker("w1")
        self.assertIsNone(w["current_spec_id"])

        # Process in DB should have exit code 124
        events = self.daemon.db.get_events(spec_id=spec_id)
        process_ended = [
            e for e in events if e["event_type"] == "process_ended"
        ]
        self.assertEqual(len(process_ended), 1)
        import json as _json
        data = _json.loads(process_ended[0]["data"])
        self.assertEqual(data["exit_code"], 124)

    @patch.object(Daemon, "launch_worker")
    @patch.object(Daemon, "_dispatch_phase_completion")
    @patch("os.getpgid")
    @patch("os.killpg")
    def test_escalates_to_sigkill(
        self,
        mock_killpg: MagicMock,
        mock_getpgid: MagicMock,
        mock_dispatch: MagicMock,
        mock_launch: MagicMock,
    ) -> None:
        """If SIGTERM doesn't stop the process, SIGKILL is sent."""
        mock_proc = self._make_mock_proc(pid=9102)
        # Process never exits from SIGTERM within the 2s window.
        # poll returns None repeatedly, then returns after SIGKILL
        mock_proc.poll.return_value = None
        mock_proc.wait.return_value = None
        mock_launch.return_value = mock_proc
        mock_getpgid.return_value = 9102

        spec_id = self._enqueue_spec()
        self.daemon.dispatch_specs()

        # Patch time.sleep to avoid waiting
        with patch("time.sleep"):
            with patch("time.monotonic") as mock_time:
                # First call: start, subsequent: past deadline
                mock_time.side_effect = [0, 0, 3, 3]
                self.daemon.handle_worker_timeout("w1")

        # Both SIGTERM and SIGKILL should have been sent
        killpg_signals = [
            call.args[1] for call in mock_killpg.call_args_list
        ]
        self.assertIn(signal.SIGTERM, killpg_signals)
        self.assertIn(signal.SIGKILL, killpg_signals)

    def test_noop_for_unknown_worker(self) -> None:
        """Timeout on a worker with no proc is a no-op."""
        # Should not raise
        self.daemon.handle_worker_timeout("w1")


# ─── process_worker_completion tests ───────────────────────────────────


class TestProcessWorkerCompletion(DaemonTestCase):
    """Test Daemon.process_worker_completion()."""

    @patch.object(Daemon, "launch_worker")
    @patch.object(Daemon, "_dispatch_phase_completion")
    def test_ends_process_frees_worker(
        self,
        mock_dispatch: MagicMock,
        mock_launch: MagicMock,
    ) -> None:
        """Completion ends process record and frees worker."""
        mock_launch.return_value = self._make_mock_proc(pid=10001)

        spec_id = self._enqueue_spec()
        self.daemon.dispatch_specs()

        self.daemon.process_worker_completion("w1", exit_code=0)

        # Worker freed
        w = self.daemon.db.get_worker("w1")
        self.assertIsNone(w["current_spec_id"])

        # Process record ended
        procs = self.daemon.db.get_active_processes()
        self.assertEqual(len(procs), 0)

        # Removed from worker_procs
        self.assertNotIn("w1", self.daemon.worker_procs)

    @patch.object(Daemon, "launch_worker")
    @patch.object(Daemon, "_dispatch_phase_completion")
    def test_dispatches_correct_phase(
        self,
        mock_dispatch: MagicMock,
        mock_launch: MagicMock,
    ) -> None:
        """Phase from worker record is passed to phase handler."""
        mock_launch.return_value = self._make_mock_proc(pid=10002)

        spec_id = self._enqueue_spec(phase="critic")
        self.daemon.dispatch_specs()

        self.daemon.process_worker_completion("w1", exit_code=0)

        mock_dispatch.assert_called_once_with(
            spec_id=spec_id,
            phase="critic",
            exit_code=0,
            worker_id="w1",
        )

    @patch.object(Daemon, "launch_worker")
    @patch.object(Daemon, "_dispatch_phase_completion")
    def test_handles_phase_handler_exception(
        self,
        mock_dispatch: MagicMock,
        mock_launch: MagicMock,
    ) -> None:
        """If phase handler raises, worker is still freed."""
        mock_launch.return_value = self._make_mock_proc(pid=10003)
        mock_dispatch.side_effect = RuntimeError("handler crashed")

        spec_id = self._enqueue_spec()
        self.daemon.dispatch_specs()

        # Should not propagate the exception
        self.daemon.process_worker_completion("w1", exit_code=0)

        # Worker still freed despite handler error
        w = self.daemon.db.get_worker("w1")
        self.assertIsNone(w["current_spec_id"])
        self.assertNotIn("w1", self.daemon.worker_procs)

    def test_noop_for_idle_worker(self) -> None:
        """Completing an idle worker (no spec) is a no-op."""
        # Should not raise
        self.daemon.process_worker_completion("w1", exit_code=0)


# ─── _dispatch_phase_completion tests ──────────────────────────────────


class TestDispatchPhaseCompletion(DaemonTestCase):
    """Test Daemon._dispatch_phase_completion fallback logic."""

    @patch.object(Daemon, "launch_worker")
    def test_fallback_requeues_on_pending_tasks(
        self,
        mock_launch: MagicMock,
    ) -> None:
        """Fallback handler requeues if spec has pending tasks."""
        mock_launch.return_value = self._make_mock_proc(pid=11001)

        spec_id = self._enqueue_spec()
        self.daemon.dispatch_specs()

        # Bypass daemon_ops and use fallback
        self.daemon._fallback_completion(spec_id, exit_code=0)

        spec = self.daemon.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "requeued")

    @patch.object(Daemon, "launch_worker")
    def test_fallback_completes_when_no_pending(
        self,
        mock_launch: MagicMock,
    ) -> None:
        """Fallback handler completes if all tasks are DONE."""
        # Create a spec file with all tasks in DONE status
        done_spec = os.path.join(self.state_dir, "done-spec.md")
        with open(done_spec, "w", encoding="utf-8") as f:
            f.write(SAMPLE_SPEC_ALL_DONE)

        mock_launch.return_value = self._make_mock_proc(pid=11002)

        spec_id = self._enqueue_spec(spec_path=done_spec)
        self.daemon.dispatch_specs()

        self.daemon._fallback_completion(spec_id, exit_code=0)

        spec = self.daemon.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "completed")

    @patch.object(Daemon, "launch_worker")
    def test_fallback_records_failure_on_nonzero(
        self,
        mock_launch: MagicMock,
    ) -> None:
        """Fallback handler records failure on non-zero exit."""
        mock_launch.return_value = self._make_mock_proc(pid=11003)

        spec_id = self._enqueue_spec()
        self.daemon.dispatch_specs()

        self.daemon._fallback_completion(spec_id, exit_code=1)

        spec = self.daemon.db.get_spec(spec_id)
        # Should be requeued (first failure, not at max yet)
        self.assertEqual(spec["status"], "requeued")
        self.assertEqual(spec["consecutive_failures"], 1)


# ─── End-to-end completion flow ────────────────────────────────────────


class TestCompletionEndToEnd(DaemonTestCase):
    """End-to-end test: dispatch, worker exits, completion handled."""

    @patch.object(Daemon, "launch_worker")
    @patch.object(Daemon, "_dispatch_phase_completion")
    def test_dispatch_then_complete(
        self,
        mock_dispatch: MagicMock,
        mock_launch: MagicMock,
    ) -> None:
        """Full cycle: enqueue -> dispatch -> exit -> completion."""
        mock_proc = self._make_mock_proc(pid=12001)
        mock_launch.return_value = mock_proc

        spec_id = self._enqueue_spec()

        # Dispatch
        self.daemon.dispatch_specs()
        self.assertEqual(len(self.daemon.worker_procs), 1)

        # Worker still running
        mock_proc.poll.return_value = None
        self.daemon.check_worker_completions()
        self.assertEqual(len(self.daemon.worker_procs), 1)

        # Worker exits
        mock_proc.poll.return_value = 0
        self.daemon.check_worker_completions()

        # Everything cleaned up
        self.assertEqual(len(self.daemon.worker_procs), 0)
        w = self.daemon.db.get_worker("w1")
        self.assertIsNone(w["current_spec_id"])

        mock_dispatch.assert_called_once_with(
            spec_id=spec_id,
            phase="execute",
            exit_code=0,
            worker_id="w1",
        )

    @patch.object(Daemon, "launch_worker")
    @patch.object(Daemon, "_dispatch_phase_completion")
    def test_timeout_during_check(
        self,
        mock_dispatch: MagicMock,
        mock_launch: MagicMock,
    ) -> None:
        """Worker times out during check_worker_completions."""
        mock_proc = self._make_mock_proc(pid=12002)
        mock_proc.poll.return_value = None
        mock_proc.wait.return_value = None
        mock_launch.return_value = mock_proc

        spec_id = self._enqueue_spec(timeout=1)
        self.daemon.dispatch_specs()

        # Backdate start_time to exceed timeout
        past_time = (
            datetime.now(timezone.utc) - timedelta(seconds=60)
        ).isoformat()
        self.daemon.db.conn.execute(
            "UPDATE workers SET start_time = ? WHERE id = ?",
            (past_time, "w1"),
        )
        self.daemon.db.conn.commit()

        with patch("os.getpgid", return_value=12002):
            with patch("os.killpg"):
                self.daemon.check_worker_completions()

        # Worker should be freed, exit code should be 124
        self.assertEqual(len(self.daemon.worker_procs), 0)
        mock_dispatch.assert_called_once_with(
            spec_id=spec_id,
            phase="execute",
            exit_code=124,
            worker_id="w1",
        )


# ─── write_state_snapshot tests ────────────────────────────────────────


class TestWriteStateSnapshot(DaemonTestCase):
    """Test Daemon.write_state_snapshot()."""

    def test_writes_daemon_state_json(self) -> None:
        """State snapshot file is created with expected structure."""
        self.daemon.write_state_snapshot()

        state_path = os.path.join(self.state_dir, "daemon-state.json")
        self.assertTrue(os.path.isfile(state_path))

        with open(state_path, encoding="utf-8") as f:
            state = json.load(f)

        self.assertIn("timestamp", state)
        self.assertIn("pid", state)
        self.assertIn("poll_interval", state)
        self.assertIn("workers", state)
        self.assertIn("queue", state)

        # PID should be current process
        self.assertEqual(state["pid"], os.getpid())

    def test_snapshot_includes_worker_assignments(self) -> None:
        """Workers list reflects current assignments."""
        self.daemon.write_state_snapshot()

        state_path = os.path.join(self.state_dir, "daemon-state.json")
        with open(state_path, encoding="utf-8") as f:
            state = json.load(f)

        workers = state["workers"]
        self.assertEqual(len(workers), 2)

        worker_ids = {w["id"] for w in workers}
        self.assertEqual(worker_ids, {"w1", "w2"})

        # Both idle
        for w in workers:
            self.assertIsNone(w["current_spec_id"])

    @patch.object(Daemon, "launch_worker")
    def test_snapshot_shows_busy_worker(
        self, mock_launch: MagicMock
    ) -> None:
        """Assigned worker shows spec and PID in snapshot."""
        mock_launch.return_value = self._make_mock_proc(pid=20001)

        spec_id = self._enqueue_spec()
        self.daemon.dispatch_specs()
        self.daemon.write_state_snapshot()

        state_path = os.path.join(self.state_dir, "daemon-state.json")
        with open(state_path, encoding="utf-8") as f:
            state = json.load(f)

        busy = [
            w for w in state["workers"]
            if w["current_spec_id"] is not None
        ]
        self.assertEqual(len(busy), 1)
        self.assertEqual(busy[0]["current_spec_id"], spec_id)
        self.assertEqual(busy[0]["current_pid"], 20001)
        self.assertEqual(busy[0]["current_phase"], "execute")

    def test_snapshot_queue_counts_empty(self) -> None:
        """Queue counts are all zero when no specs are queued."""
        self.daemon.write_state_snapshot()

        state_path = os.path.join(self.state_dir, "daemon-state.json")
        with open(state_path, encoding="utf-8") as f:
            state = json.load(f)

        q = state["queue"]
        self.assertEqual(q["total"], 0)
        self.assertEqual(q["queued"], 0)
        self.assertEqual(q["running"], 0)

    def test_snapshot_queue_counts_with_specs(self) -> None:
        """Queue counts reflect actual spec statuses."""
        self._enqueue_spec()

        spec2 = os.path.join(self.state_dir, "spec2.md")
        with open(spec2, "w", encoding="utf-8") as f:
            f.write(SAMPLE_SPEC)
        self._enqueue_spec(spec_path=spec2)

        self.daemon.write_state_snapshot()

        state_path = os.path.join(self.state_dir, "daemon-state.json")
        with open(state_path, encoding="utf-8") as f:
            state = json.load(f)

        q = state["queue"]
        self.assertEqual(q["total"], 2)
        self.assertEqual(q["queued"], 2)

    def test_snapshot_overwrites_cleanly(self) -> None:
        """Second snapshot replaces content (no merge/append)."""
        self._enqueue_spec()
        self.daemon.write_state_snapshot()

        state_path = os.path.join(self.state_dir, "daemon-state.json")
        with open(state_path, encoding="utf-8") as f:
            first = json.load(f)
        self.assertEqual(first["queue"]["queued"], 1)

        # Cancel the spec, write again
        specs = self.daemon.db.get_queue()
        self.daemon.db.cancel(specs[0]["id"])
        self.daemon.write_state_snapshot()

        with open(state_path, encoding="utf-8") as f:
            second = json.load(f)
        self.assertEqual(second["queue"]["queued"], 0)
        self.assertEqual(second["queue"]["canceled"], 1)


# ─── write_heartbeat tests ────────────────────────────────────────────


class TestWriteHeartbeat(DaemonTestCase):
    """Test Daemon.write_heartbeat()."""

    def test_writes_heartbeat_file(self) -> None:
        """Heartbeat file is created with a valid timestamp."""
        self.daemon.write_heartbeat()

        hb_path = os.path.join(self.state_dir, "daemon-heartbeat")
        self.assertTrue(os.path.isfile(hb_path))

        content = Path(hb_path).read_text(encoding="utf-8").strip()
        # Should be a valid ISO timestamp ending with Z
        self.assertTrue(content.endswith("Z"))
        # Verify it parses
        datetime.strptime(content, "%Y-%m-%dT%H:%M:%SZ")

    def test_heartbeat_overwrites_previous(self) -> None:
        """Second write replaces the file (no append/corruption)."""
        self.daemon.write_heartbeat()
        hb_path = os.path.join(self.state_dir, "daemon-heartbeat")

        # Overwrite with junk, then write heartbeat again
        Path(hb_path).write_text("corrupted\nextra\n")
        self.daemon.write_heartbeat()

        # Should be a single clean line again
        lines = Path(hb_path).read_text(encoding="utf-8").strip().split("\n")
        self.assertEqual(len(lines), 1)
        datetime.strptime(lines[0], "%Y-%m-%dT%H:%M:%SZ")

    def test_heartbeat_atomic_write(self) -> None:
        """No .tmp file remains after write."""
        self.daemon.write_heartbeat()

        tmp_path = os.path.join(
            self.state_dir, "daemon-heartbeat.tmp"
        )
        self.assertFalse(os.path.exists(tmp_path))


# ─── self_heal tests ──────────────────────────────────────────────────


class TestSelfHeal(DaemonTestCase):
    """Test Daemon.self_heal()."""

    def test_self_heal_no_errors_when_empty(self) -> None:
        """self_heal runs without error on an empty queue."""
        self.daemon.self_heal()

    def test_self_heal_recovers_stale_assigning(self) -> None:
        """Specs stuck in 'assigning' are recovered to 'requeued'."""
        spec_id = self._enqueue_spec()

        # Manually set to assigning with a stale timestamp
        past = (
            datetime.now(timezone.utc) - timedelta(seconds=120)
        ).isoformat()
        self.daemon.db.conn.execute(
            "UPDATE specs SET status = 'assigning', "
            "assigning_at = ? WHERE id = ?",
            (past, spec_id),
        )
        self.daemon.db.conn.commit()

        self.daemon.self_heal()

        spec = self.daemon.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "requeued")

    def test_self_heal_does_not_recover_recent_assigning(self) -> None:
        """Recently-assigned specs are NOT recovered."""
        spec_id = self._enqueue_spec()

        # Set to assigning just now
        now = datetime.now(timezone.utc).isoformat()
        self.daemon.db.conn.execute(
            "UPDATE specs SET status = 'assigning', "
            "assigning_at = ? WHERE id = ?",
            (now, spec_id),
        )
        self.daemon.db.conn.commit()

        self.daemon.self_heal()

        spec = self.daemon.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "assigning")

    @patch.object(Daemon, "launch_worker")
    def test_self_heal_frees_orphaned_workers(
        self, mock_launch: MagicMock
    ) -> None:
        """Workers assigned to completed specs are freed."""
        mock_launch.return_value = self._make_mock_proc(pid=30001)

        spec_id = self._enqueue_spec()
        self.daemon.dispatch_specs()

        # Mark spec as completed directly in DB (simulate external
        # completion). Worker is still marked as busy.
        self.daemon.db.conn.execute(
            "UPDATE specs SET status = 'completed' WHERE id = ?",
            (spec_id,),
        )
        self.daemon.db.conn.commit()

        self.daemon.self_heal()

        # Worker should be freed
        w = self.daemon.db.get_worker("w1")
        self.assertIsNone(w["current_spec_id"])
        self.assertNotIn("w1", self.daemon.worker_procs)

    @patch("lib.daemon_ops.check_needs_review_timeouts")
    @patch("lib.daemon_ops.self_heal")
    def test_self_heal_still_recovers_assigning_when_ops_fails(
        self,
        mock_heal: MagicMock,
        mock_review: MagicMock,
    ) -> None:
        """Even if daemon_ops raises, stuck-assigning recovery still runs."""
        mock_review.side_effect = RuntimeError("ops broken")
        mock_heal.side_effect = RuntimeError("ops broken")

        spec_id = self._enqueue_spec()
        past = (
            datetime.now(timezone.utc) - timedelta(seconds=120)
        ).isoformat()
        self.daemon.db.conn.execute(
            "UPDATE specs SET status = 'assigning', "
            "assigning_at = ? WHERE id = ?",
            (past, spec_id),
        )
        self.daemon.db.conn.commit()

        # Should not raise, and assigning recovery should still work
        self.daemon.self_heal()

        spec = self.daemon.db.get_spec(spec_id)
        self.assertEqual(spec["status"], "requeued")

    @patch("lib.daemon_ops.check_needs_review_timeouts")
    @patch("lib.daemon_ops.self_heal")
    def test_self_heal_calls_check_needs_review(
        self,
        mock_heal: MagicMock,
        mock_review: MagicMock,
    ) -> None:
        """self_heal delegates to check_needs_review_timeouts."""
        mock_review.return_value = []
        mock_heal.return_value = []

        self.daemon.self_heal()

        mock_review.assert_called_once_with(
            queue_dir=self.queue_dir,
            events_dir=os.path.join(self.state_dir, "events"),
            state_dir=self.state_dir,
            db=self.daemon.db,
        )

    @patch("lib.daemon_ops.check_needs_review_timeouts")
    @patch("lib.daemon_ops.self_heal")
    def test_self_heal_calls_daemon_ops_self_heal(
        self,
        mock_heal: MagicMock,
        mock_review: MagicMock,
    ) -> None:
        """self_heal delegates to daemon_ops.self_heal with worker map."""
        mock_review.return_value = []
        mock_heal.return_value = []

        self.daemon.self_heal()

        mock_heal.assert_called_once()
        call_kwargs = mock_heal.call_args.kwargs
        self.assertEqual(call_kwargs["queue_dir"], self.queue_dir)
        # worker_specs should have both workers
        ws = call_kwargs["worker_specs"]
        self.assertIn("w1", ws)
        self.assertIn("w2", ws)
        # Both idle => empty string values
        self.assertEqual(ws["w1"], "")
        self.assertEqual(ws["w2"], "")

    @patch("lib.daemon_ops.check_needs_review_timeouts")
    @patch("lib.daemon_ops.self_heal")
    def test_self_heal_exception_does_not_propagate(
        self,
        mock_heal: MagicMock,
        mock_review: MagicMock,
    ) -> None:
        """Exceptions in daemon_ops don't crash the daemon."""
        mock_review.side_effect = RuntimeError("review exploded")
        mock_heal.return_value = []

        # Should not raise
        self.daemon.self_heal()


if __name__ == "__main__":
    unittest.main()
