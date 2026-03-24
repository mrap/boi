# test_messaging.py — Unit tests for BOI messaging module.
#
# Tests message send/receive/acknowledge via file-based mailbox transport,
# with SQLite as audit/history layer.
#
# TDD: These tests are written before lib/messaging.py exists.

import json
import os
import sys
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))


class TestMessageTypes(unittest.TestCase):
    """Test message type constants and validation."""

    def test_orchestrator_message_types(self):
        from lib.messaging import ORCHESTRATOR_MSG_TYPES

        expected = {
            "CANCEL",
            "SKIP",
            "PREEMPT",
            "DEPRIORITIZE",
            "NEW_DEP",
            "CONTEXT_UPDATE",
        }
        self.assertEqual(set(ORCHESTRATOR_MSG_TYPES), expected)

    def test_worker_message_types(self):
        from lib.messaging import WORKER_MSG_TYPES

        expected = {"PROGRESS", "STUCK", "ESCALATE", "DISCOVERY"}
        self.assertEqual(set(WORKER_MSG_TYPES), expected)

    def test_all_message_types(self):
        from lib.messaging import ALL_MSG_TYPES

        self.assertEqual(len(ALL_MSG_TYPES), 10)


class TestMessageCreation(unittest.TestCase):
    """Test creating message dicts."""

    def test_create_message_minimal(self):
        from lib.messaging import create_message

        msg = create_message(
            msg_type="CANCEL",
            spec_id="q-012",
            sender="cli",
        )

        self.assertEqual(msg["version"], 1)
        self.assertEqual(msg["type"], "CANCEL")
        self.assertEqual(msg["spec_id"], "q-012")
        self.assertEqual(msg["sender"], "cli")
        self.assertIsNone(msg["task_id"])
        self.assertEqual(msg["payload"], {})
        self.assertTrue(msg["id"].startswith("msg-"))
        self.assertIn("timestamp", msg)

    def test_create_message_with_task_id(self):
        from lib.messaging import create_message

        msg = create_message(
            msg_type="SKIP",
            spec_id="q-012",
            sender="cli",
            task_id="t-3",
            payload={"reason": "No longer needed"},
        )

        self.assertEqual(msg["task_id"], "t-3")
        self.assertEqual(msg["payload"]["reason"], "No longer needed")

    def test_create_message_unique_ids(self):
        from lib.messaging import create_message

        ids = set()
        for _ in range(100):
            msg = create_message(msg_type="PROGRESS", spec_id="q-1", sender="worker")
            ids.add(msg["id"])
        self.assertEqual(len(ids), 100)

    def test_create_message_invalid_type(self):
        from lib.messaging import create_message

        with self.assertRaises(ValueError):
            create_message(msg_type="INVALID", spec_id="q-1", sender="cli")

    def test_create_message_timestamp_format(self):
        from lib.messaging import create_message

        msg = create_message(msg_type="CANCEL", spec_id="q-1", sender="cli")
        # ISO 8601 format
        self.assertTrue(msg["timestamp"].endswith("Z"))
        self.assertIn("T", msg["timestamp"])


class TestSendMessage(unittest.TestCase):
    """Test writing messages to the file-based mailbox."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.state_dir = self.tmpdir

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_send_to_worker_creates_mailbox_dir(self):
        from lib.messaging import send_message

        send_message(
            spec_id="q-012",
            msg_type="CANCEL",
            payload={"reason": "test"},
            sender="cli",
            state_dir=self.state_dir,
        )

        mailbox = os.path.join(self.state_dir, "mailbox", "q-012")
        self.assertTrue(os.path.isdir(mailbox))

    def test_send_to_worker_writes_json_file(self):
        from lib.messaging import send_message

        msg = send_message(
            spec_id="q-012",
            msg_type="CANCEL",
            payload={"reason": "test"},
            sender="cli",
            state_dir=self.state_dir,
        )

        mailbox = os.path.join(self.state_dir, "mailbox", "q-012")
        files = os.listdir(mailbox)
        self.assertEqual(len(files), 1)
        self.assertTrue(files[0].endswith("-CANCEL.json"))

        # Verify content
        with open(os.path.join(mailbox, files[0])) as f:
            data = json.load(f)
        self.assertEqual(data["type"], "CANCEL")
        self.assertEqual(data["spec_id"], "q-012")

    def test_send_to_daemon_writes_to_daemon_mailbox(self):
        from lib.messaging import send_message

        send_message(
            spec_id="q-012",
            msg_type="PROGRESS",
            payload={"status": "running tests"},
            sender="worker",
            state_dir=self.state_dir,
            direction="to_daemon",
        )

        daemon_mailbox = os.path.join(self.state_dir, "mailbox", "daemon")
        self.assertTrue(os.path.isdir(daemon_mailbox))
        files = os.listdir(daemon_mailbox)
        self.assertEqual(len(files), 1)
        self.assertTrue(files[0].endswith(f"-PROGRESS-q-012.json"))

    def test_send_returns_message_dict(self):
        from lib.messaging import send_message

        msg = send_message(
            spec_id="q-012",
            msg_type="SKIP",
            payload={},
            sender="cli",
            state_dir=self.state_dir,
            task_id="t-3",
        )

        self.assertEqual(msg["type"], "SKIP")
        self.assertEqual(msg["task_id"], "t-3")

    def test_send_atomic_write(self):
        """Verify no .tmp files remain after send."""
        from lib.messaging import send_message

        send_message(
            spec_id="q-012",
            msg_type="CANCEL",
            payload={},
            sender="cli",
            state_dir=self.state_dir,
        )

        mailbox = os.path.join(self.state_dir, "mailbox", "q-012")
        for f in os.listdir(mailbox):
            self.assertFalse(f.endswith(".tmp"), f"Found temp file: {f}")

    def test_send_multiple_messages_ordered(self):
        from lib.messaging import send_message

        for i in range(5):
            send_message(
                spec_id="q-012",
                msg_type="PROGRESS",
                payload={"step": i},
                sender="worker",
                state_dir=self.state_dir,
                direction="to_daemon",
            )

        daemon_mailbox = os.path.join(self.state_dir, "mailbox", "daemon")
        files = sorted(os.listdir(daemon_mailbox))
        self.assertEqual(len(files), 5)
        # Files should be naturally ordered by timestamp prefix
        self.assertEqual(files, sorted(files))


class TestReceiveMessages(unittest.TestCase):
    """Test reading messages from the mailbox."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.state_dir = self.tmpdir

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_receive_empty_mailbox(self):
        from lib.messaging import receive_messages

        msgs = receive_messages(
            spec_id="q-012",
            state_dir=self.state_dir,
        )
        self.assertEqual(msgs, [])

    def test_receive_nonexistent_mailbox(self):
        from lib.messaging import receive_messages

        msgs = receive_messages(
            spec_id="q-nonexistent",
            state_dir=self.state_dir,
        )
        self.assertEqual(msgs, [])

    def test_receive_returns_sorted_messages(self):
        from lib.messaging import receive_messages, send_message

        send_message(
            "q-012", "SKIP", {"reason": "first"}, "cli", self.state_dir, task_id="t-1"
        )
        time.sleep(0.01)  # ensure different timestamps
        send_message("q-012", "CANCEL", {"reason": "second"}, "cli", self.state_dir)

        msgs = receive_messages("q-012", self.state_dir)
        self.assertEqual(len(msgs), 2)
        # Should be sorted by timestamp (filename)
        self.assertEqual(msgs[0]["type"], "SKIP")
        self.assertEqual(msgs[1]["type"], "CANCEL")

    def test_receive_filter_by_type(self):
        from lib.messaging import receive_messages, send_message

        send_message("q-012", "SKIP", {}, "cli", self.state_dir, task_id="t-1")
        send_message("q-012", "CANCEL", {}, "cli", self.state_dir)
        send_message(
            "q-012", "CONTEXT_UPDATE", {"context": "hi"}, "cli", self.state_dir
        )

        msgs = receive_messages(
            "q-012", self.state_dir, msg_types=["CANCEL", "PREEMPT"]
        )
        self.assertEqual(len(msgs), 1)
        self.assertEqual(msgs[0]["type"], "CANCEL")

    def test_receive_daemon_messages(self):
        from lib.messaging import receive_daemon_messages, send_message

        send_message(
            "q-012",
            "PROGRESS",
            {"status": "test"},
            "worker",
            self.state_dir,
            direction="to_daemon",
        )
        send_message(
            "q-015",
            "PROGRESS",
            {"status": "other"},
            "worker",
            self.state_dir,
            direction="to_daemon",
        )

        msgs = receive_daemon_messages(self.state_dir, spec_id="q-012")
        self.assertEqual(len(msgs), 1)
        self.assertEqual(msgs[0]["spec_id"], "q-012")

    def test_receive_daemon_messages_all(self):
        from lib.messaging import receive_daemon_messages, send_message

        send_message(
            "q-012",
            "PROGRESS",
            {"status": "a"},
            "worker",
            self.state_dir,
            direction="to_daemon",
        )
        send_message(
            "q-015",
            "STUCK",
            {"reason": "b"},
            "worker",
            self.state_dir,
            direction="to_daemon",
        )

        msgs = receive_daemon_messages(self.state_dir)
        self.assertEqual(len(msgs), 2)


class TestAcknowledgeMessage(unittest.TestCase):
    """Test acknowledging (deleting) processed messages."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.state_dir = self.tmpdir

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_ack_deletes_file(self):
        from lib.messaging import ack_message, receive_messages, send_message

        send_message("q-012", "CANCEL", {}, "cli", self.state_dir)

        msgs = receive_messages("q-012", self.state_dir)
        self.assertEqual(len(msgs), 1)

        ack_message(msgs[0], "q-012", self.state_dir)

        # Should be empty now
        msgs2 = receive_messages("q-012", self.state_dir)
        self.assertEqual(len(msgs2), 0)

    def test_ack_daemon_message(self):
        from lib.messaging import (
            ack_daemon_message,
            receive_daemon_messages,
            send_message,
        )

        send_message(
            "q-012",
            "PROGRESS",
            {"status": "x"},
            "worker",
            self.state_dir,
            direction="to_daemon",
        )

        msgs = receive_daemon_messages(self.state_dir, spec_id="q-012")
        self.assertEqual(len(msgs), 1)

        ack_daemon_message(msgs[0], self.state_dir)

        msgs2 = receive_daemon_messages(self.state_dir, spec_id="q-012")
        self.assertEqual(len(msgs2), 0)

    def test_ack_idempotent(self):
        """Acknowledging a message twice should not error."""
        from lib.messaging import ack_message, receive_messages, send_message

        send_message("q-012", "CANCEL", {}, "cli", self.state_dir)
        msgs = receive_messages("q-012", self.state_dir)

        ack_message(msgs[0], "q-012", self.state_dir)
        # Second ack should not raise
        ack_message(msgs[0], "q-012", self.state_dir)


class TestCheckMailboxUrgent(unittest.TestCase):
    """Test the urgent message check (used in tmux poll loop)."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.state_dir = self.tmpdir

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_no_urgent_messages(self):
        from lib.messaging import check_urgent

        result = check_urgent("q-012", self.state_dir)
        self.assertIsNone(result)

    def test_cancel_is_urgent(self):
        from lib.messaging import check_urgent, send_message

        send_message("q-012", "CANCEL", {}, "cli", self.state_dir)
        result = check_urgent("q-012", self.state_dir)
        self.assertEqual(result, "CANCEL")

    def test_preempt_is_urgent(self):
        from lib.messaging import check_urgent, send_message

        send_message("q-012", "PREEMPT", {}, "daemon", self.state_dir)
        result = check_urgent("q-012", self.state_dir)
        self.assertEqual(result, "PREEMPT")

    def test_skip_is_not_urgent(self):
        from lib.messaging import check_urgent, send_message

        send_message("q-012", "SKIP", {}, "cli", self.state_dir, task_id="t-1")
        result = check_urgent("q-012", self.state_dir)
        self.assertIsNone(result)

    def test_cancel_takes_priority(self):
        from lib.messaging import check_urgent, send_message

        send_message("q-012", "SKIP", {}, "cli", self.state_dir, task_id="t-1")
        send_message("q-012", "CANCEL", {}, "cli", self.state_dir)
        send_message(
            "q-012", "CONTEXT_UPDATE", {"context": "hi"}, "cli", self.state_dir
        )

        result = check_urgent("q-012", self.state_dir)
        self.assertEqual(result, "CANCEL")


class TestCleanupMailbox(unittest.TestCase):
    """Test mailbox cleanup operations."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.state_dir = self.tmpdir

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_cleanup_removes_spec_mailbox(self):
        from lib.messaging import cleanup_mailbox, send_message

        send_message("q-012", "CANCEL", {}, "cli", self.state_dir)
        mailbox = os.path.join(self.state_dir, "mailbox", "q-012")
        self.assertTrue(os.path.isdir(mailbox))

        cleanup_mailbox("q-012", self.state_dir)
        self.assertFalse(os.path.isdir(mailbox))

    def test_cleanup_nonexistent_mailbox(self):
        from lib.messaging import cleanup_mailbox

        # Should not raise
        cleanup_mailbox("q-nonexistent", self.state_dir)

    def test_cleanup_returns_unprocessed_count(self):
        from lib.messaging import cleanup_mailbox, send_message

        send_message("q-012", "CANCEL", {}, "cli", self.state_dir)
        send_message("q-012", "SKIP", {}, "cli", self.state_dir, task_id="t-1")

        count = cleanup_mailbox("q-012", self.state_dir)
        self.assertEqual(count, 2)


class TestExitCodeMapping(unittest.TestCase):
    """Test message type to exit code mapping."""

    def test_exit_codes(self):
        from lib.messaging import EXIT_CODES

        self.assertEqual(EXIT_CODES["CANCEL"], 130)
        self.assertEqual(EXIT_CODES["SKIP"], 131)
        self.assertEqual(EXIT_CODES["PREEMPT"], 132)
        self.assertEqual(EXIT_CODES["DEPRIORITIZE"], 133)

    def test_exit_reason_lookup(self):
        from lib.messaging import exit_reason

        self.assertEqual(exit_reason(130), "canceled")
        self.assertEqual(exit_reason(131), "task_skipped")
        self.assertEqual(exit_reason(132), "preempted")
        self.assertEqual(exit_reason(133), "deprioritized")
        self.assertEqual(exit_reason(0), "normal")
        self.assertEqual(exit_reason(1), "failure")


if __name__ == "__main__":
    unittest.main()
