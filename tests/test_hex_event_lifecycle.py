# test_hex_event_lifecycle.py — Tests for emit_hex_event and lifecycle event
# calls in daemon.py.
#
# Tests cover:
# - emit_hex_event calls hex_emit.py with correct args
# - emit_hex_event does not crash when hex_emit.py is missing
# - lifecycle events fired at the right points in process_worker_completion

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
from lib.db import Database

SAMPLE_SPEC_COMPLETED = """\
# Test Spec

**Target:** ~/projects/example-repo

## Tasks

### t-1: First task
DONE

**Verify:** echo ok
"""

SAMPLE_SPEC_PENDING = """\
# Test Spec

**Target:** ~/projects/example-repo

## Tasks

### t-1: First task
PENDING

**Verify:** echo ok
"""


class HexEventTestCase(unittest.TestCase):
    """Base class that creates a minimal Daemon instance for tests."""

    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.state_dir = self._tmpdir.name
        self.db_path = os.path.join(self.state_dir, "boi.db")
        self.queue_dir = os.path.join(self.state_dir, "queue")
        self.log_dir = os.path.join(self.state_dir, "logs")
        os.makedirs(self.queue_dir, exist_ok=True)
        os.makedirs(self.log_dir, exist_ok=True)

        self.worktree_a = os.path.join(self.state_dir, "worktree-a")
        os.makedirs(self.worktree_a, exist_ok=True)

        self.config_path = os.path.join(self.state_dir, "config.json")
        config = {"workers": [{"id": "w1", "worktree_path": self.worktree_a}]}
        with open(self.config_path, "w", encoding="utf-8") as f:
            json.dump(config, f)

        self.daemon = Daemon(
            config_path=self.config_path,
            db_path=self.db_path,
            state_dir=self.state_dir,
        )
        self.daemon.load_workers()

    def tearDown(self) -> None:
        self.daemon.db.close()
        self._tmpdir.cleanup()

    def _write_spec(self, content: str) -> str:
        """Write a spec file and return its path."""
        spec_path = os.path.join(self.state_dir, "test.spec.md")
        with open(spec_path, "w", encoding="utf-8") as f:
            f.write(content)
        return spec_path


# ── emit_hex_event unit tests ────────────────────────────────────────────────

class TestEmitHexEventMissing(HexEventTestCase):
    """emit_hex_event does not crash when hex_emit.py is absent."""

    def test_no_crash_when_hex_emit_missing(self):
        """Should log debug and return without calling subprocess."""
        fake_path = os.path.join(self.state_dir, "nonexistent_hex_emit.py")
        with patch("os.path.expanduser", return_value=fake_path):
            with patch("subprocess.run") as mock_run:
                # Must not raise
                self.daemon.emit_hex_event("boi.spec.completed", {"spec_id": "q-1"})
                mock_run.assert_not_called()

    def test_no_crash_on_subprocess_exception(self):
        """Should swallow subprocess errors gracefully."""
        fake_emit = os.path.join(self.state_dir, "hex_emit.py")
        Path(fake_emit).touch()
        with patch("os.path.expanduser", return_value=fake_emit):
            with patch("subprocess.run", side_effect=OSError("boom")):
                # Must not raise
                self.daemon.emit_hex_event("boi.spec.completed", {"spec_id": "q-1"})


class TestEmitHexEventCallArgs(HexEventTestCase):
    """emit_hex_event calls hex_emit.py with correct arguments."""

    def setUp(self):
        super().setUp()
        # Create a fake hex_emit.py so the file-existence check passes
        self.fake_emit = os.path.join(self.state_dir, "hex_emit.py")
        Path(self.fake_emit).touch()

    def test_calls_hex_emit_with_event_type_and_json_payload(self):
        payload = {"spec_id": "q-42", "target_repo": "~/github.com/mrap/boi"}
        with patch("os.path.expanduser", return_value=self.fake_emit):
            with patch("subprocess.run") as mock_run:
                self.daemon.emit_hex_event("boi.spec.completed", payload)
                mock_run.assert_called_once()
                args, kwargs = mock_run.call_args
                cmd = args[0]
                self.assertEqual(cmd[0], sys.executable)
                self.assertEqual(cmd[1], self.fake_emit)
                self.assertEqual(cmd[2], "boi.spec.completed")
                # Third arg is JSON-encoded payload
                parsed = json.loads(cmd[3])
                self.assertEqual(parsed["spec_id"], "q-42")
                self.assertEqual(kwargs.get("timeout"), 5)

    def test_calls_hex_emit_for_failed_event(self):
        payload = {"spec_id": "q-7", "failure_reason": "timeout", "iteration": 3}
        with patch("os.path.expanduser", return_value=self.fake_emit):
            with patch("subprocess.run") as mock_run:
                self.daemon.emit_hex_event("boi.spec.failed", payload)
                args, _ = mock_run.call_args
                self.assertEqual(args[0][2], "boi.spec.failed")

    def test_calls_hex_emit_for_iteration_done_event(self):
        payload = {"spec_id": "q-9", "iteration": 2, "tasks_completed": 3, "tasks_added": 0}
        with patch("os.path.expanduser", return_value=self.fake_emit):
            with patch("subprocess.run") as mock_run:
                self.daemon.emit_hex_event("boi.iteration.done", payload)
                args, _ = mock_run.call_args
                self.assertEqual(args[0][2], "boi.iteration.done")


# ── _extract_target_repo unit tests ─────────────────────────────────────────

class TestExtractTargetRepo(HexEventTestCase):
    def test_extracts_target_from_spec(self):
        spec_path = self._write_spec(SAMPLE_SPEC_COMPLETED)
        result = Daemon._extract_target_repo(spec_path)
        self.assertEqual(result, "~/projects/example-repo")

    def test_returns_empty_string_for_missing_file(self):
        result = Daemon._extract_target_repo("/does/not/exist.md")
        self.assertEqual(result, "")

    def test_returns_empty_string_when_no_target_field(self):
        spec_path = self._write_spec("# Spec\nNo target here\n")
        result = Daemon._extract_target_repo(spec_path)
        self.assertEqual(result, "")


# ── Lifecycle event integration in process_worker_completion ─────────────────

class TestLifecycleEventsOnCompletion(HexEventTestCase):
    """Verify lifecycle events are emitted during process_worker_completion."""

    def _enqueue_and_run(self, spec_content: str, exit_code: int, db_status: str):
        """
        Set up a spec, put it in 'running', assign to w1, then call
        process_worker_completion. Returns the spec_id.
        """
        spec_path = self._write_spec(spec_content)
        result = self.daemon.db.enqueue(spec_path=spec_path)
        spec_id = result["id"]

        # Put spec + worker into running state
        self.daemon.db.set_running(spec_id, "w1", "execute")
        self.daemon.db.assign_worker("w1", spec_id, pid=9999, phase="execute")

        mock_proc = MagicMock()
        mock_proc.pid = 9999
        self.daemon.worker_procs["w1"] = mock_proc

        # Force the DB status to whatever we want to test
        with self.daemon.db.lock:
            self.daemon.db.conn.execute(
                "UPDATE specs SET status = ?, tasks_done = 1, tasks_total = 1 WHERE id = ?",
                (db_status, spec_id),
            )
            self.daemon.db.conn.commit()

        return spec_id

    def test_completed_status_emits_boi_spec_completed(self):
        spec_path = self._write_spec(SAMPLE_SPEC_COMPLETED)
        result = self.daemon.db.enqueue(spec_path=spec_path)
        spec_id = result["id"]
        self.daemon.db.set_running(spec_id, "w1", "execute")
        self.daemon.db.assign_worker("w1", spec_id, pid=9999, phase="execute")
        mock_proc = MagicMock()
        mock_proc.pid = 9999
        self.daemon.worker_procs["w1"] = mock_proc

        # Patch _dispatch_phase_completion to set status to completed
        def fake_dispatch(**kwargs):
            with self.daemon.db.lock:
                self.daemon.db.conn.execute(
                    "UPDATE specs SET status='completed', tasks_done=1, tasks_total=1 WHERE id=?",
                    (spec_id,),
                )
                self.daemon.db.conn.commit()

        emitted = []

        def fake_emit(event_type, payload):
            emitted.append((event_type, payload))

        with patch.object(self.daemon, "_dispatch_phase_completion", side_effect=fake_dispatch):
            with patch.object(self.daemon, "emit_hex_event", side_effect=fake_emit):
                self.daemon.process_worker_completion("w1", exit_code=0)

        event_types = [e[0] for e in emitted]
        self.assertIn("boi.spec.completed", event_types)
        self.assertIn("boi.iteration.done", event_types)
        self.assertNotIn("boi.spec.failed", event_types)

        # Check payload of boi.spec.completed
        completed_payload = next(p for t, p in emitted if t == "boi.spec.completed")
        self.assertEqual(completed_payload["spec_id"], spec_id)
        self.assertIn("target_repo", completed_payload)
        self.assertIn("tasks_done", completed_payload)
        self.assertIn("tasks_total", completed_payload)
        self.assertIn("spec_title", completed_payload)
        self.assertIsInstance(completed_payload["spec_title"], str)
        self.assertTrue(len(completed_payload["spec_title"]) > 0)

    def test_failed_status_emits_boi_spec_failed(self):
        spec_path = self._write_spec(SAMPLE_SPEC_PENDING)
        result = self.daemon.db.enqueue(spec_path=spec_path)
        spec_id = result["id"]
        self.daemon.db.set_running(spec_id, "w1", "execute")
        self.daemon.db.assign_worker("w1", spec_id, pid=9999, phase="execute")
        mock_proc = MagicMock()
        mock_proc.pid = 9999
        self.daemon.worker_procs["w1"] = mock_proc

        def fake_dispatch(**kwargs):
            with self.daemon.db.lock:
                self.daemon.db.conn.execute(
                    "UPDATE specs SET status='failed', failure_reason='max failures' WHERE id=?",
                    (spec_id,),
                )
                self.daemon.db.conn.commit()

        emitted = []

        def fake_emit(event_type, payload):
            emitted.append((event_type, payload))

        with patch.object(self.daemon, "_dispatch_phase_completion", side_effect=fake_dispatch):
            with patch.object(self.daemon, "emit_hex_event", side_effect=fake_emit):
                self.daemon.process_worker_completion("w1", exit_code=1)

        event_types = [e[0] for e in emitted]
        self.assertIn("boi.spec.failed", event_types)
        self.assertIn("boi.iteration.done", event_types)
        self.assertNotIn("boi.spec.completed", event_types)

        failed_payload = next(p for t, p in emitted if t == "boi.spec.failed")
        self.assertEqual(failed_payload["spec_id"], spec_id)
        self.assertIn("failure_reason", failed_payload)
        self.assertIn("iteration", failed_payload)
        self.assertIn("spec_title", failed_payload)
        self.assertIsInstance(failed_payload["spec_title"], str)
        self.assertTrue(len(failed_payload["spec_title"]) > 0)

    def test_requeued_status_emits_only_iteration_done(self):
        spec_path = self._write_spec(SAMPLE_SPEC_PENDING)
        result = self.daemon.db.enqueue(spec_path=spec_path)
        spec_id = result["id"]
        self.daemon.db.set_running(spec_id, "w1", "execute")
        self.daemon.db.assign_worker("w1", spec_id, pid=9999, phase="execute")
        mock_proc = MagicMock()
        mock_proc.pid = 9999
        self.daemon.worker_procs["w1"] = mock_proc

        def fake_dispatch(**kwargs):
            with self.daemon.db.lock:
                self.daemon.db.conn.execute(
                    "UPDATE specs SET status='requeued' WHERE id=?",
                    (spec_id,),
                )
                self.daemon.db.conn.commit()

        emitted = []

        def fake_emit(event_type, payload):
            emitted.append((event_type, payload))

        with patch.object(self.daemon, "_dispatch_phase_completion", side_effect=fake_dispatch):
            with patch.object(self.daemon, "emit_hex_event", side_effect=fake_emit):
                self.daemon.process_worker_completion("w1", exit_code=0)

        event_types = [e[0] for e in emitted]
        self.assertIn("boi.iteration.done", event_types)
        self.assertNotIn("boi.spec.completed", event_types)
        self.assertNotIn("boi.spec.failed", event_types)

    def test_iteration_done_payload_structure(self):
        spec_path = self._write_spec(SAMPLE_SPEC_PENDING)
        result = self.daemon.db.enqueue(spec_path=spec_path)
        spec_id = result["id"]
        self.daemon.db.set_running(spec_id, "w1", "execute")
        self.daemon.db.assign_worker("w1", spec_id, pid=9999, phase="execute")
        mock_proc = MagicMock()
        mock_proc.pid = 9999
        self.daemon.worker_procs["w1"] = mock_proc

        def fake_dispatch(**kwargs):
            pass  # leave status as running/requeued

        emitted = []

        def fake_emit(event_type, payload):
            emitted.append((event_type, payload))

        with patch.object(self.daemon, "_dispatch_phase_completion", side_effect=fake_dispatch):
            with patch.object(self.daemon, "emit_hex_event", side_effect=fake_emit):
                self.daemon.process_worker_completion("w1", exit_code=0)

        iter_events = [(t, p) for t, p in emitted if t == "boi.iteration.done"]
        self.assertTrue(len(iter_events) >= 1)
        _, payload = iter_events[0]
        self.assertEqual(payload["spec_id"], spec_id)
        self.assertIn("iteration", payload)
        self.assertIn("tasks_completed", payload)
        self.assertIn("tasks_added", payload)


if __name__ == "__main__":
    unittest.main()
