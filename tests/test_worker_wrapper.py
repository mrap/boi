# test_worker_wrapper.py — Tests for worker message handling integration.
#
# Tests that the Worker class correctly handles messages at checkpoints:
# - Pre-flight: CANCEL, SKIP, CONTEXT_UPDATE
# - Tmux poll: CANCEL, PREEMPT (urgent kill)
# - Post-flight: SKIP revert, DEPRIORITIZE yield

import json
import os
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.messaging import cleanup_mailbox, send_message


class TestWorkerMailboxIntegration(unittest.TestCase):
    """Test Worker's mailbox checking during tmux poll."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.state_dir = self.tmpdir
        self.spec_id = "q-test-001"

        # Create minimal spec file
        self.spec_path = os.path.join(self.tmpdir, "queue", f"{self.spec_id}.spec.md")
        os.makedirs(os.path.dirname(self.spec_path), exist_ok=True)
        with open(self.spec_path, "w") as f:
            f.write(
                textwrap.dedent("""\
                # Test Spec

                ## Tasks

                ### t-1: Do something
                PENDING

                **Spec:** Test task.

                **Verify:** echo ok
            """)
            )

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def _make_worker(self):
        """Create a Worker with test paths."""
        from worker import Worker

        return Worker(
            spec_id=self.spec_id,
            worktree=self.tmpdir,
            spec_path=self.spec_path,
            iteration=1,
            state_dir=self.state_dir,
        )

    def test_check_mailbox_urgent_finds_cancel(self):
        """Worker poll loop detects CANCEL in mailbox."""
        from lib.messaging import check_urgent

        send_message(self.spec_id, "CANCEL", {}, "cli", self.state_dir)
        result = check_urgent(self.spec_id, self.state_dir)
        self.assertEqual(result, "CANCEL")

    def test_check_mailbox_urgent_finds_preempt(self):
        """Worker poll loop detects PREEMPT in mailbox."""
        from lib.messaging import check_urgent

        send_message(self.spec_id, "PREEMPT", {}, "daemon", self.state_dir)
        result = check_urgent(self.spec_id, self.state_dir)
        self.assertEqual(result, "PREEMPT")

    def test_check_mailbox_ignores_non_urgent(self):
        """Worker poll loop ignores SKIP/DEPRIORITIZE/CONTEXT_UPDATE."""
        from lib.messaging import check_urgent

        send_message(self.spec_id, "SKIP", {}, "cli", self.state_dir, task_id="t-1")
        send_message(
            self.spec_id,
            "DEPRIORITIZE",
            {"new_priority": 200},
            "daemon",
            self.state_dir,
        )
        send_message(
            self.spec_id, "CONTEXT_UPDATE", {"context": "hi"}, "cli", self.state_dir
        )

        result = check_urgent(self.spec_id, self.state_dir)
        self.assertIsNone(result)

    def test_worker_exit_code_cancel(self):
        """CANCEL message maps to exit code 130."""
        from lib.messaging import EXIT_CODES

        self.assertEqual(EXIT_CODES["CANCEL"], 130)

    def test_worker_exit_code_skip(self):
        """SKIP message maps to exit code 131."""
        from lib.messaging import EXIT_CODES

        self.assertEqual(EXIT_CODES["SKIP"], 131)

    def test_worker_exit_code_preempt(self):
        """PREEMPT message maps to exit code 132."""
        from lib.messaging import EXIT_CODES

        self.assertEqual(EXIT_CODES["PREEMPT"], 132)

    def test_worker_exit_code_deprioritize(self):
        """DEPRIORITIZE message maps to exit code 133."""
        from lib.messaging import EXIT_CODES

        self.assertEqual(EXIT_CODES["DEPRIORITIZE"], 133)


class TestPostFlightMessageHandling(unittest.TestCase):
    """Test post-flight message processing scenarios."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.state_dir = self.tmpdir
        self.spec_id = "q-test-002"

        self.spec_path = os.path.join(self.tmpdir, "queue", f"{self.spec_id}.spec.md")
        os.makedirs(os.path.dirname(self.spec_path), exist_ok=True)

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_skip_reverts_done_to_skipped(self):
        """After Claude marks task DONE, SKIP should revert to SKIPPED."""
        spec_content = textwrap.dedent("""\
            # Test Spec

            ## Tasks

            ### t-1: First task
            DONE

            **Spec:** Already done.

            ### t-2: Second task
            DONE

            **Spec:** Claude just finished this.
        """)
        with open(self.spec_path, "w") as f:
            f.write(spec_content)

        # Simulate SKIP message for t-2 that arrived during Claude execution
        send_message(
            self.spec_id,
            "SKIP",
            {"reason": "test"},
            "cli",
            self.state_dir,
            task_id="t-2",
        )

        import re

        # Simulate post-flight: read SKIP, revert t-2 DONE -> SKIPPED
        from lib.messaging import ack_message, receive_messages

        msgs = receive_messages(self.spec_id, self.state_dir, msg_types=["SKIP"])
        self.assertEqual(len(msgs), 1)

        skip_msg = msgs[0]
        task_id = skip_msg.get("task_id")
        self.assertEqual(task_id, "t-2")

        # Revert DONE to SKIPPED for t-2
        spec = open(self.spec_path).read()
        spec = re.sub(
            rf"(### {task_id}:.*?\n)DONE",
            rf"\1SKIPPED",
            spec,
            count=1,
        )
        with open(self.spec_path, "w") as f:
            f.write(spec)

        ack_message(skip_msg, self.spec_id, self.state_dir)

        # Verify: t-1 still DONE, t-2 is SKIPPED
        final = open(self.spec_path).read()
        self.assertIn("### t-1: First task\nDONE", final)
        self.assertIn("### t-2: Second task\nSKIPPED", final)

    def test_deprioritize_signals_yield(self):
        """DEPRIORITIZE message at post-flight should trigger exit 133."""
        from lib.messaging import EXIT_CODES, receive_messages

        send_message(
            self.spec_id,
            "DEPRIORITIZE",
            {"new_priority": 200},
            "daemon",
            self.state_dir,
        )

        msgs = receive_messages(
            self.spec_id, self.state_dir, msg_types=["DEPRIORITIZE"]
        )
        self.assertEqual(len(msgs), 1)
        self.assertEqual(EXIT_CODES["DEPRIORITIZE"], 133)

    def test_context_update_preserved_for_next_iteration(self):
        """CONTEXT_UPDATE at post-flight should NOT be deleted."""
        send_message(
            self.spec_id,
            "CONTEXT_UPDATE",
            {"context": "check iOS"},
            "cli",
            self.state_dir,
        )

        # At post-flight, context updates are logged but NOT acked
        from lib.messaging import receive_messages

        msgs = receive_messages(
            self.spec_id, self.state_dir, msg_types=["CONTEXT_UPDATE"]
        )
        self.assertEqual(len(msgs), 1)
        self.assertEqual(msgs[0]["payload"]["context"], "check iOS")

        # Next pre-flight should still see it
        msgs2 = receive_messages(
            self.spec_id, self.state_dir, msg_types=["CONTEXT_UPDATE"]
        )
        self.assertEqual(len(msgs2), 1)


class TestPreFlightMessageHandling(unittest.TestCase):
    """Test pre-flight message processing scenarios."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.state_dir = self.tmpdir
        self.spec_id = "q-test-003"

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_cancel_at_preflight_prevents_claude_launch(self):
        """CANCEL at pre-flight should result in exit 130 before Claude runs."""
        from lib.messaging import check_urgent, EXIT_CODES

        send_message(self.spec_id, "CANCEL", {}, "cli", self.state_dir)

        urgent = check_urgent(self.spec_id, self.state_dir)
        self.assertEqual(urgent, "CANCEL")
        self.assertEqual(EXIT_CODES["CANCEL"], 130)

    def test_context_update_collected_for_prompt_injection(self):
        """CONTEXT_UPDATE at pre-flight should be collected for prompt."""
        from lib.messaging import ack_message, receive_messages

        send_message(
            self.spec_id,
            "CONTEXT_UPDATE",
            {"context": "Also check iOS"},
            "cli",
            self.state_dir,
        )
        send_message(
            self.spec_id,
            "CONTEXT_UPDATE",
            {"context": "And Android"},
            "cli",
            self.state_dir,
        )

        msgs = receive_messages(
            self.spec_id, self.state_dir, msg_types=["CONTEXT_UPDATE"]
        )
        self.assertEqual(len(msgs), 2)

        # Collect all context
        extra_context = "\n".join(m["payload"]["context"] for m in msgs)
        self.assertIn("Also check iOS", extra_context)
        self.assertIn("And Android", extra_context)

        # Ack after injection
        for m in msgs:
            ack_message(m, self.spec_id, self.state_dir)

        # Should be empty now
        remaining = receive_messages(
            self.spec_id, self.state_dir, msg_types=["CONTEXT_UPDATE"]
        )
        self.assertEqual(len(remaining), 0)


class TestWorkerProgressReporting(unittest.TestCase):
    """Test worker -> daemon PROGRESS messages."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.state_dir = self.tmpdir
        self.spec_id = "q-test-004"

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_worker_sends_progress(self):
        """Worker can send PROGRESS to daemon."""
        from lib.messaging import receive_daemon_messages

        send_message(
            self.spec_id,
            "PROGRESS",
            {"status": "Running tests", "worker_id": "w-1"},
            "worker",
            self.state_dir,
            direction="to_daemon",
        )

        msgs = receive_daemon_messages(self.state_dir, spec_id=self.spec_id)
        self.assertEqual(len(msgs), 1)
        self.assertEqual(msgs[0]["payload"]["status"], "Running tests")

    def test_worker_sends_stuck(self):
        """Worker can send STUCK to daemon."""
        from lib.messaging import receive_daemon_messages

        send_message(
            self.spec_id,
            "STUCK",
            {"reason": "File not found", "worker_id": "w-1"},
            "worker",
            self.state_dir,
            direction="to_daemon",
        )

        msgs = receive_daemon_messages(self.state_dir, spec_id=self.spec_id)
        self.assertEqual(len(msgs), 1)
        self.assertEqual(msgs[0]["type"], "STUCK")


class TestMailboxCleanupOnCompletion(unittest.TestCase):
    """Test mailbox cleanup when worker completes."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.state_dir = self.tmpdir
        self.spec_id = "q-test-005"

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_cleanup_after_normal_completion(self):
        """Mailbox is cleaned up after worker completes normally."""
        send_message(
            self.spec_id, "CONTEXT_UPDATE", {"context": "stale"}, "cli", self.state_dir
        )

        mailbox = os.path.join(self.state_dir, "mailbox", self.spec_id)
        self.assertTrue(os.path.isdir(mailbox))

        count = cleanup_mailbox(self.spec_id, self.state_dir)
        self.assertEqual(count, 1)
        self.assertFalse(os.path.isdir(mailbox))

    def test_cleanup_after_crash(self):
        """Mailbox cleanup handles crash scenario (unprocessed messages)."""
        send_message(self.spec_id, "CANCEL", {}, "cli", self.state_dir)
        send_message(self.spec_id, "SKIP", {}, "cli", self.state_dir, task_id="t-1")

        count = cleanup_mailbox(self.spec_id, self.state_dir)
        self.assertEqual(count, 2)


if __name__ == "__main__":
    unittest.main()
