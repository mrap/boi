"""test_event_log.py — Tests for BOI event log.

Tests all functions in lib/event_log.py:
- write_event: atomic event writing with sequence numbering
- read_events: read all events in order
- read_event: read single event by sequence number
- count_events: count event files
- get_next_sequence: determine next sequence number

All tests use temp directories. No live API calls.
Uses unittest (stdlib only, no pytest dependency).
"""

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

BOI_ROOT = str(Path(__file__).resolve().parent.parent)
sys.path.insert(0, BOI_ROOT)

from lib.event_log import (
    count_events,
    get_next_sequence,
    read_event,
    read_events,
    write_event,
)


class TestGetNextSequence(unittest.TestCase):
    """Tests for get_next_sequence."""

    def setUp(self):
        self.tmp_dir = tempfile.mkdtemp(prefix="boi-events-")

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmp_dir, ignore_errors=True)

    def test_empty_dir(self):
        self.assertEqual(get_next_sequence(self.tmp_dir), 1)

    def test_nonexistent_dir(self):
        self.assertEqual(get_next_sequence("/tmp/nonexistent-boi-test"), 1)

    def test_after_one_event(self):
        Path(os.path.join(self.tmp_dir, "event-00001.json")).write_text("{}")
        self.assertEqual(get_next_sequence(self.tmp_dir), 2)

    def test_after_multiple_events(self):
        for i in range(1, 4):
            Path(os.path.join(self.tmp_dir, f"event-{i:05d}.json")).write_text("{}")
        self.assertEqual(get_next_sequence(self.tmp_dir), 4)

    def test_ignores_non_event_files(self):
        Path(os.path.join(self.tmp_dir, "event-00001.json")).write_text("{}")
        Path(os.path.join(self.tmp_dir, "something-else.json")).write_text("{}")
        Path(os.path.join(self.tmp_dir, "event-00002.txt")).write_text("{}")
        self.assertEqual(get_next_sequence(self.tmp_dir), 2)

    def test_handles_gaps(self):
        Path(os.path.join(self.tmp_dir, "event-00001.json")).write_text("{}")
        Path(os.path.join(self.tmp_dir, "event-00005.json")).write_text("{}")
        self.assertEqual(get_next_sequence(self.tmp_dir), 6)


class TestWriteEvent(unittest.TestCase):
    """Tests for write_event."""

    def setUp(self):
        self.tmp_dir = tempfile.mkdtemp(prefix="boi-events-")

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmp_dir, ignore_errors=True)

    def test_creates_event_file(self):
        seq = write_event(self.tmp_dir, {"type": "spec_completed", "queue_id": "q-001"})
        self.assertEqual(seq, 1)
        filepath = os.path.join(self.tmp_dir, "event-00001.json")
        self.assertTrue(os.path.isfile(filepath))

    def test_event_content_includes_seq(self):
        write_event(self.tmp_dir, {"type": "test"})
        filepath = os.path.join(self.tmp_dir, "event-00001.json")
        data = json.loads(Path(filepath).read_text())
        self.assertEqual(data["seq"], 1)
        self.assertEqual(data["type"], "test")

    def test_sequential_numbering(self):
        seq1 = write_event(self.tmp_dir, {"type": "first"})
        seq2 = write_event(self.tmp_dir, {"type": "second"})
        seq3 = write_event(self.tmp_dir, {"type": "third"})
        self.assertEqual(seq1, 1)
        self.assertEqual(seq2, 2)
        self.assertEqual(seq3, 3)

    def test_creates_directory_if_missing(self):
        events_dir = os.path.join(self.tmp_dir, "subdir", "events")
        seq = write_event(events_dir, {"type": "test"})
        self.assertEqual(seq, 1)
        self.assertTrue(os.path.isdir(events_dir))

    def test_no_tmp_files_left(self):
        write_event(self.tmp_dir, {"type": "test"})
        tmp_files = [f for f in os.listdir(self.tmp_dir) if f.startswith(".")]
        self.assertEqual(len(tmp_files), 0)

    def test_preserves_all_event_fields(self):
        event = {
            "type": "spec_completed",
            "queue_id": "q-001",
            "spec_path": "/tmp/spec.md",
            "iterations": 3,
            "tasks_done": 8,
            "tasks_added": 2,
        }
        write_event(self.tmp_dir, event)
        filepath = os.path.join(self.tmp_dir, "event-00001.json")
        data = json.loads(Path(filepath).read_text())
        for key, value in event.items():
            self.assertEqual(data[key], value)


class TestReadEvents(unittest.TestCase):
    """Tests for read_events."""

    def setUp(self):
        self.tmp_dir = tempfile.mkdtemp(prefix="boi-events-")

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmp_dir, ignore_errors=True)

    def test_empty_dir(self):
        events = read_events(self.tmp_dir)
        self.assertEqual(len(events), 0)

    def test_nonexistent_dir(self):
        events = read_events("/tmp/nonexistent-boi-events-test")
        self.assertEqual(len(events), 0)

    def test_reads_all_events_in_order(self):
        write_event(self.tmp_dir, {"type": "first", "order": 1})
        write_event(self.tmp_dir, {"type": "second", "order": 2})
        write_event(self.tmp_dir, {"type": "third", "order": 3})

        events = read_events(self.tmp_dir)
        self.assertEqual(len(events), 3)
        self.assertEqual(events[0]["type"], "first")
        self.assertEqual(events[1]["type"], "second")
        self.assertEqual(events[2]["type"], "third")

    def test_skips_malformed_json(self):
        write_event(self.tmp_dir, {"type": "good"})
        # Write a malformed event file
        Path(os.path.join(self.tmp_dir, "event-00002.json")).write_text(
            "not valid json{{{", encoding="utf-8"
        )
        write_event(self.tmp_dir, {"type": "also_good"})

        events = read_events(self.tmp_dir)
        self.assertEqual(len(events), 2)

    def test_ignores_non_event_files(self):
        write_event(self.tmp_dir, {"type": "real"})
        Path(os.path.join(self.tmp_dir, "other.json")).write_text("{}")
        Path(os.path.join(self.tmp_dir, "readme.md")).write_text("# Hi")

        events = read_events(self.tmp_dir)
        self.assertEqual(len(events), 1)


class TestReadEvent(unittest.TestCase):
    """Tests for read_event (single event)."""

    def setUp(self):
        self.tmp_dir = tempfile.mkdtemp(prefix="boi-events-")

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmp_dir, ignore_errors=True)

    def test_reads_existing_event(self):
        write_event(self.tmp_dir, {"type": "test", "data": "hello"})
        event = read_event(self.tmp_dir, 1)
        self.assertIsNotNone(event)
        self.assertEqual(event["type"], "test")
        self.assertEqual(event["data"], "hello")

    def test_returns_none_for_missing(self):
        event = read_event(self.tmp_dir, 999)
        self.assertIsNone(event)

    def test_returns_none_for_malformed(self):
        Path(os.path.join(self.tmp_dir, "event-00001.json")).write_text("bad json")
        event = read_event(self.tmp_dir, 1)
        self.assertIsNone(event)


class TestCountEvents(unittest.TestCase):
    """Tests for count_events."""

    def setUp(self):
        self.tmp_dir = tempfile.mkdtemp(prefix="boi-events-")

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmp_dir, ignore_errors=True)

    def test_empty_dir(self):
        self.assertEqual(count_events(self.tmp_dir), 0)

    def test_nonexistent_dir(self):
        self.assertEqual(count_events("/tmp/nonexistent-boi-count-test"), 0)

    def test_counts_events(self):
        write_event(self.tmp_dir, {"type": "a"})
        write_event(self.tmp_dir, {"type": "b"})
        write_event(self.tmp_dir, {"type": "c"})
        self.assertEqual(count_events(self.tmp_dir), 3)

    def test_ignores_non_event_files(self):
        write_event(self.tmp_dir, {"type": "real"})
        Path(os.path.join(self.tmp_dir, "other.json")).write_text("{}")
        self.assertEqual(count_events(self.tmp_dir), 1)


if __name__ == "__main__":
    unittest.main()
