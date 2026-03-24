# test_messaging.py — Integration tests for BOI messaging system.
#
# Tests the full messaging lifecycle: send message, worker processes it,
# daemon handles the exit code correctly.
#
# No Docker required. Uses mock data and temp directories.

import json
import os
import re
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent.parent))

from lib.db import Database
from lib.messaging import (
    ack_message,
    check_urgent,
    cleanup_mailbox,
    receive_messages,
    send_message,
)


class TestMessagingWithDatabase(unittest.TestCase):
    """Test messaging integration with SQLite database."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.state_dir = self.tmpdir
        self.queue_dir = os.path.join(self.tmpdir, "queue")
        os.makedirs(self.queue_dir, exist_ok=True)

        self.db_path = os.path.join(self.tmpdir, "boi.db")
        self.db = Database(self.db_path, self.queue_dir)

    def tearDown(self):
        self.db.close()
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_record_message_in_db(self):
        """Messages sent via file mailbox can also be recorded in SQLite."""
        msg = send_message("q-012", "CANCEL", {"reason": "test"}, "cli", self.state_dir)
        self.db.record_message(msg)

        unacked = self.db.get_unacked_messages(spec_id="q-012")
        self.assertEqual(len(unacked), 1)
        self.assertEqual(unacked[0]["msg_type"], "CANCEL")

    def test_ack_message_in_db(self):
        """Acknowledging a message updates the DB."""
        msg = send_message("q-012", "CANCEL", {"reason": "test"}, "cli", self.state_dir)
        self.db.record_message(msg)

        self.db.ack_message_db(msg["id"])

        unacked = self.db.get_unacked_messages(spec_id="q-012")
        self.assertEqual(len(unacked), 0)

    def test_get_latest_progress(self):
        """Can retrieve the most recent PROGRESS message for a spec."""
        send_message(
            "q-012",
            "PROGRESS",
            {"status": "step 1"},
            "worker",
            self.state_dir,
            direction="to_daemon",
        )
        msg2 = send_message(
            "q-012",
            "PROGRESS",
            {"status": "step 2"},
            "worker",
            self.state_dir,
            direction="to_daemon",
        )
        self.db.record_message(msg2)

        # Only msg2 was recorded in DB
        latest = self.db.get_latest_progress("q-012")
        self.assertIsNotNone(latest)
        payload = json.loads(latest["payload"])
        self.assertEqual(payload["status"], "step 2")

    def test_cleanup_old_messages(self):
        """Old acknowledged messages can be cleaned up."""
        msg = send_message("q-012", "CANCEL", {}, "cli", self.state_dir)
        self.db.record_message(msg)
        self.db.ack_message_db(msg["id"])

        # With max_age_days=0, should delete the acked message
        deleted = self.db.cleanup_old_messages(max_age_days=0)
        self.assertEqual(deleted, 1)


class TestSkipDuringExecution(unittest.TestCase):
    """Integration test: SKIP message sent while worker executes a task."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.state_dir = self.tmpdir
        self.spec_id = "q-int-001"

        self.spec_path = os.path.join(self.tmpdir, f"{self.spec_id}.spec.md")
        with open(self.spec_path, "w") as f:
            f.write(
                textwrap.dedent("""\
                # Integration Test Spec

                ## Tasks

                ### t-1: First task
                DONE

                **Spec:** Already completed.

                ### t-2: Second task
                PENDING

                **Spec:** Worker is executing this.

                ### t-3: Third task
                PENDING

                **Spec:** Queued for later.
            """)
            )

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_skip_mid_execution_full_flow(self):
        """Full flow: user sends SKIP for t-2, worker finishes, post-flight reverts."""
        # 1. Worker is executing t-2. User sends SKIP.
        send_message(
            self.spec_id,
            "SKIP",
            {"reason": "No longer needed"},
            "cli",
            self.state_dir,
            task_id="t-2",
        )

        # 2. Claude finishes and marks t-2 DONE (simulated)
        spec = open(self.spec_path).read()
        spec = spec.replace(
            "### t-2: Second task\nPENDING", "### t-2: Second task\nDONE"
        )
        with open(self.spec_path, "w") as f:
            f.write(spec)

        # 3. Post-flight: check for SKIP messages
        msgs = receive_messages(self.spec_id, self.state_dir, msg_types=["SKIP"])
        self.assertEqual(len(msgs), 1)

        skip_msg = msgs[0]
        task_id = skip_msg["task_id"]
        self.assertEqual(task_id, "t-2")

        # 4. Revert DONE -> SKIPPED
        spec = open(self.spec_path).read()
        spec = re.sub(
            rf"(### {task_id}:.*?\n)DONE",
            rf"\1SKIPPED",
            spec,
            count=1,
        )
        with open(self.spec_path, "w") as f:
            f.write(spec)

        # 5. Ack the message
        ack_message(skip_msg, self.spec_id, self.state_dir)

        # 6. Verify final state
        final = open(self.spec_path).read()
        self.assertIn("### t-1: First task\nDONE", final)
        self.assertIn("### t-2: Second task\nSKIPPED", final)
        self.assertIn("### t-3: Third task\nPENDING", final)

        # 7. Verify message was acked (mailbox empty)
        remaining = receive_messages(self.spec_id, self.state_dir)
        self.assertEqual(len(remaining), 0)


class TestCancelDuringExecution(unittest.TestCase):
    """Integration test: CANCEL sent while worker is running."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.state_dir = self.tmpdir
        self.spec_id = "q-int-002"

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_cancel_detected_as_urgent(self):
        """CANCEL is detected by the urgent check in the poll loop."""
        send_message(
            self.spec_id, "CANCEL", {"reason": "User requested"}, "cli", self.state_dir
        )

        urgent = check_urgent(self.spec_id, self.state_dir)
        self.assertEqual(urgent, "CANCEL")

        # After handling, cleanup the mailbox
        count = cleanup_mailbox(self.spec_id, self.state_dir)
        self.assertEqual(count, 1)


class TestContextUpdateInjection(unittest.TestCase):
    """Integration test: CONTEXT_UPDATE injected into prompt."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.state_dir = self.tmpdir
        self.spec_id = "q-int-003"

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_context_updates_collected_and_injected(self):
        """Multiple CONTEXT_UPDATE messages are concatenated into the prompt."""
        send_message(
            self.spec_id,
            "CONTEXT_UPDATE",
            {"context": "Check the iOS implementation"},
            "cli",
            self.state_dir,
        )
        send_message(
            self.spec_id,
            "CONTEXT_UPDATE",
            {"context": "Also verify Android compatibility"},
            "cli",
            self.state_dir,
        )

        # Pre-flight: collect all CONTEXT_UPDATE messages
        msgs = receive_messages(
            self.spec_id, self.state_dir, msg_types=["CONTEXT_UPDATE"]
        )
        self.assertEqual(len(msgs), 2)

        # Build extra context for prompt
        extra_context = "\n".join(m["payload"]["context"] for m in msgs)
        self.assertIn("Check the iOS implementation", extra_context)
        self.assertIn("Also verify Android compatibility", extra_context)

        # Ack after injection
        for m in msgs:
            ack_message(m, self.spec_id, self.state_dir)

        # Verify cleanup
        remaining = receive_messages(self.spec_id, self.state_dir)
        self.assertEqual(len(remaining), 0)


if __name__ == "__main__":
    unittest.main()
