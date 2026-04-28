# test_db_migrate.py — Tests for JSON <-> SQLite migration.
#
# Verifies that:
# 1. export_queue_to_json() writes correct JSON files
# 2. Round-trip: JSON -> SQLite -> JSON produces identical entries
# 3. migrate_queue_to_db() migrates specs, iterations, and events
# 4. JSON files are archived after migration

import json
import os
import tempfile
import unittest
from pathlib import Path
from typing import Any

from lib.db import Database
from lib.db_migrate import (
    migrate_queue_to_db,
    _migrate_events,
    _archive_queue_files,
    _archive_event_files,
)
from lib.db_to_json import export_queue_to_json


class ExportTestCase(unittest.TestCase):
    """Base test case with temp dir and Database."""

    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.db_path = os.path.join(self._tmpdir.name, "boi.db")
        self.queue_dir = os.path.join(self._tmpdir.name, "queue")
        self.db = Database(self.db_path, self.queue_dir)

    def tearDown(self) -> None:
        self.db.close()
        self._tmpdir.cleanup()

    def _make_spec_file(
        self,
        content: str = "# Test Spec\n\n## Tasks\n\n1. PENDING: Do something\n",
    ) -> str:
        """Write a temp spec file and return its absolute path."""
        spec_path = os.path.join(self._tmpdir.name, "test-spec.md")
        Path(spec_path).write_text(content, encoding="utf-8")
        return spec_path

    def _make_spec_file_named(
        self, name: str, content: str = "# Spec\n"
    ) -> str:
        """Write a named temp spec file and return its absolute path."""
        spec_path = os.path.join(self._tmpdir.name, name)
        Path(spec_path).write_text(content, encoding="utf-8")
        return spec_path

    def _read_json(self, queue_id: str) -> dict[str, Any]:
        """Read a q-NNN.json file from the queue dir."""
        path = Path(self.queue_dir) / f"{queue_id}.json"
        return json.loads(path.read_text(encoding="utf-8"))


class TestExportQueueToJson(ExportTestCase):
    """Verify export_queue_to_json() writes correct JSON files."""

    def test_export_empty_db(self) -> None:
        count = export_queue_to_json(self.db)
        self.assertEqual(count, 0)

    def test_export_single_spec(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec, priority=50)
        count = export_queue_to_json(self.db)
        self.assertEqual(count, 1)
        entry = self._read_json("q-001")
        self.assertEqual(entry["id"], "q-001")
        self.assertEqual(entry["priority"], 50)
        self.assertEqual(entry["status"], "queued")

    def test_export_multiple_specs(self) -> None:
        for i in range(3):
            spec = self._make_spec_file_named(f"spec-{i}.md")
            self.db.enqueue(spec, priority=100 + i)
        count = export_queue_to_json(self.db)
        self.assertEqual(count, 3)
        for i in range(3):
            sid = f"q-{i+1:03d}"
            self.assertTrue(
                (Path(self.queue_dir) / f"{sid}.json").is_file()
            )

    def test_export_preserves_sync_back_true(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec, sync_back=True)
        export_queue_to_json(self.db)
        entry = self._read_json("q-001")
        self.assertIs(entry["sync_back"], True)

    def test_export_preserves_sync_back_false(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec, sync_back=False)
        export_queue_to_json(self.db)
        entry = self._read_json("q-001")
        self.assertIs(entry["sync_back"], False)

    def test_export_preserves_blocked_by(self) -> None:
        spec_a = self._make_spec_file_named("a.md")
        spec_b = self._make_spec_file_named("b.md")
        self.db.enqueue(spec_a, queue_id="q-001")
        self.db.enqueue(spec_b, queue_id="q-002", blocked_by=["q-001"])
        export_queue_to_json(self.db)
        entry = self._read_json("q-002")
        self.assertEqual(entry["blocked_by"], ["q-001"])

    def test_export_preserves_iteration(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        # iteration is managed by set_running(), use direct SQL for test
        self.db.conn.execute(
            "UPDATE specs SET iteration = 7 WHERE id = 'q-001'"
        )
        self.db.conn.commit()
        export_queue_to_json(self.db)
        entry = self._read_json("q-001")
        self.assertEqual(entry["iteration"], 7)

    def test_export_preserves_status(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.update_spec_fields("q-001", status="completed")
        export_queue_to_json(self.db)
        entry = self._read_json("q-001")
        self.assertEqual(entry["status"], "completed")

    def test_export_preserves_project(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec, project="my-project")
        export_queue_to_json(self.db)
        entry = self._read_json("q-001")
        self.assertEqual(entry["project"], "my-project")

    def test_export_preserves_initial_task_ids(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        export_queue_to_json(self.db)
        entry = self._read_json("q-001")
        self.assertIsInstance(entry["initial_task_ids"], list)

    def test_export_to_custom_dir(self) -> None:
        alt_dir = os.path.join(self._tmpdir.name, "alt-queue")
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        count = export_queue_to_json(self.db, queue_dir=alt_dir)
        self.assertEqual(count, 1)
        self.assertTrue(
            (Path(alt_dir) / "q-001.json").is_file()
        )

    def test_export_preserves_failure_reason(self) -> None:
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.update_spec_fields(
            "q-001", status="failed", failure_reason="too many crashes"
        )
        export_queue_to_json(self.db)
        entry = self._read_json("q-001")
        self.assertEqual(entry["failure_reason"], "too many crashes")


class TestRoundTrip(ExportTestCase):
    """Verify JSON -> SQLite -> JSON round-trip is lossless."""

    def _write_queue_json(
        self, queue_id: str, entry: dict[str, Any]
    ) -> None:
        """Write a JSON queue entry file."""
        path = Path(self.queue_dir) / f"{queue_id}.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            json.dumps(entry, indent=2) + "\n", encoding="utf-8"
        )

    def _make_json_entry(
        self, queue_id: str, **overrides: Any
    ) -> dict[str, Any]:
        """Create a standard JSON queue entry dict."""
        entry: dict[str, Any] = {
            "id": queue_id,
            "spec_path": f"/tmp/queue/{queue_id}.spec.md",
            "original_spec_path": f"/tmp/specs/{queue_id}.md",
            "worktree": None,
            "priority": 100,
            "status": "queued",
            "phase": "execute",
            "submitted_at": "2026-01-01T00:00:00+00:00",
            "iteration": 0,
            "max_iterations": 30,
            "blocked_by": [],
            "last_worker": None,
            "last_iteration_at": None,
            "first_running_at": None,
            "consecutive_failures": 0,
            "cooldown_until": None,
            "tasks_done": 0,
            "tasks_total": 0,
            "sync_back": True,
            "project": None,
            "initial_task_ids": [],
        }
        entry.update(overrides)
        return entry

    # Fields that are always present and must survive round-trip.
    CORE_FIELDS = [
        "id",
        "spec_path",
        "original_spec_path",
        "worktree",
        "priority",
        "status",
        "submitted_at",
        "iteration",
        "max_iterations",
        "blocked_by",
        "last_worker",
        "last_iteration_at",
        "consecutive_failures",
        "tasks_done",
        "tasks_total",
        "sync_back",
        "project",
        "initial_task_ids",
    ]

    def _assert_entries_match(
        self,
        original: dict[str, Any],
        exported: dict[str, Any],
    ) -> None:
        """Assert core fields match between original and exported."""
        for field in self.CORE_FIELDS:
            orig_val = original.get(field)
            exp_val = exported.get(field)
            self.assertEqual(
                orig_val,
                exp_val,
                f"Field '{field}' mismatch: {orig_val!r} != {exp_val!r}",
            )

    def test_round_trip_single_entry(self) -> None:
        """JSON -> SQLite -> JSON preserves all core fields."""
        original = self._make_json_entry("q-001", priority=42)
        self._write_queue_json("q-001", original)

        self.db.migrate_from_json()

        # Export to a separate directory to avoid overwriting source
        export_dir = os.path.join(self._tmpdir.name, "exported")
        export_queue_to_json(self.db, queue_dir=export_dir)

        exported = json.loads(
            (Path(export_dir) / "q-001.json").read_text(encoding="utf-8")
        )
        self._assert_entries_match(original, exported)

    def test_round_trip_multiple_entries(self) -> None:
        """Round-trip works for multiple specs."""
        originals = {}
        for i in range(1, 4):
            sid = f"q-{i:03d}"
            entry = self._make_json_entry(sid, priority=100 + i)
            self._write_queue_json(sid, entry)
            originals[sid] = entry

        self.db.migrate_from_json()

        export_dir = os.path.join(self._tmpdir.name, "exported")
        count = export_queue_to_json(self.db, queue_dir=export_dir)
        self.assertEqual(count, 3)

        for sid, original in originals.items():
            exported = json.loads(
                (Path(export_dir) / f"{sid}.json").read_text(
                    encoding="utf-8"
                )
            )
            self._assert_entries_match(original, exported)

    def test_round_trip_preserves_sync_back_false(self) -> None:
        original = self._make_json_entry("q-001", sync_back=False)
        self._write_queue_json("q-001", original)
        self.db.migrate_from_json()

        export_dir = os.path.join(self._tmpdir.name, "exported")
        export_queue_to_json(self.db, queue_dir=export_dir)
        exported = json.loads(
            (Path(export_dir) / "q-001.json").read_text(encoding="utf-8")
        )
        self.assertIs(exported["sync_back"], False)

    def test_round_trip_preserves_sync_back_true(self) -> None:
        original = self._make_json_entry("q-001", sync_back=True)
        self._write_queue_json("q-001", original)
        self.db.migrate_from_json()

        export_dir = os.path.join(self._tmpdir.name, "exported")
        export_queue_to_json(self.db, queue_dir=export_dir)
        exported = json.loads(
            (Path(export_dir) / "q-001.json").read_text(encoding="utf-8")
        )
        self.assertIs(exported["sync_back"], True)

    def test_round_trip_preserves_blocked_by(self) -> None:
        entry_a = self._make_json_entry("q-001")
        entry_b = self._make_json_entry(
            "q-002", blocked_by=["q-001"]
        )
        self._write_queue_json("q-001", entry_a)
        self._write_queue_json("q-002", entry_b)
        self.db.migrate_from_json()

        export_dir = os.path.join(self._tmpdir.name, "exported")
        export_queue_to_json(self.db, queue_dir=export_dir)
        exported = json.loads(
            (Path(export_dir) / "q-002.json").read_text(encoding="utf-8")
        )
        self.assertEqual(exported["blocked_by"], ["q-001"])

    def test_round_trip_preserves_completed_status(self) -> None:
        original = self._make_json_entry(
            "q-001",
            status="completed",
            iteration=5,
            tasks_done=10,
            tasks_total=10,
        )
        self._write_queue_json("q-001", original)
        self.db.migrate_from_json()

        export_dir = os.path.join(self._tmpdir.name, "exported")
        export_queue_to_json(self.db, queue_dir=export_dir)
        exported = json.loads(
            (Path(export_dir) / "q-001.json").read_text(encoding="utf-8")
        )
        self.assertEqual(exported["status"], "completed")
        self.assertEqual(exported["iteration"], 5)
        self.assertEqual(exported["tasks_done"], 10)
        self.assertEqual(exported["tasks_total"], 10)

    def test_round_trip_preserves_project(self) -> None:
        original = self._make_json_entry(
            "q-001", project="my-cool-project"
        )
        self._write_queue_json("q-001", original)
        self.db.migrate_from_json()

        export_dir = os.path.join(self._tmpdir.name, "exported")
        export_queue_to_json(self.db, queue_dir=export_dir)
        exported = json.loads(
            (Path(export_dir) / "q-001.json").read_text(encoding="utf-8")
        )
        self.assertEqual(exported["project"], "my-cool-project")

    def test_round_trip_preserves_initial_task_ids(self) -> None:
        original = self._make_json_entry(
            "q-001", initial_task_ids=["task-1", "task-2"]
        )
        self._write_queue_json("q-001", original)
        self.db.migrate_from_json()

        export_dir = os.path.join(self._tmpdir.name, "exported")
        export_queue_to_json(self.db, queue_dir=export_dir)
        exported = json.loads(
            (Path(export_dir) / "q-001.json").read_text(encoding="utf-8")
        )
        self.assertEqual(
            exported["initial_task_ids"], ["task-1", "task-2"]
        )

    def test_round_trip_preserves_worktree(self) -> None:
        original = self._make_json_entry(
            "q-001", worktree="/home/user/worktree-1"
        )
        self._write_queue_json("q-001", original)
        self.db.migrate_from_json()

        export_dir = os.path.join(self._tmpdir.name, "exported")
        export_queue_to_json(self.db, queue_dir=export_dir)
        exported = json.loads(
            (Path(export_dir) / "q-001.json").read_text(encoding="utf-8")
        )
        self.assertEqual(exported["worktree"], "/home/user/worktree-1")

    def test_round_trip_preserves_consecutive_failures(self) -> None:
        original = self._make_json_entry(
            "q-001", consecutive_failures=3
        )
        self._write_queue_json("q-001", original)
        self.db.migrate_from_json()

        export_dir = os.path.join(self._tmpdir.name, "exported")
        export_queue_to_json(self.db, queue_dir=export_dir)
        exported = json.loads(
            (Path(export_dir) / "q-001.json").read_text(encoding="utf-8")
        )
        self.assertEqual(exported["consecutive_failures"], 3)


class MigrateTestCase(unittest.TestCase):
    """Base test case for migrate_queue_to_db tests."""

    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.state_dir = self._tmpdir.name
        self.db_path = os.path.join(self.state_dir, "boi.db")
        self.queue_dir = os.path.join(self.state_dir, "queue")
        self.events_dir = os.path.join(self.state_dir, "events")
        os.makedirs(self.queue_dir, exist_ok=True)
        os.makedirs(self.events_dir, exist_ok=True)
        self.db = Database(self.db_path, self.queue_dir)

    def tearDown(self) -> None:
        self.db.close()
        self._tmpdir.cleanup()

    def _write_queue_json(
        self, queue_id: str, entry: dict[str, Any]
    ) -> None:
        """Write a JSON queue entry file."""
        path = Path(self.queue_dir) / f"{queue_id}.json"
        path.write_text(
            json.dumps(entry, indent=2) + "\n", encoding="utf-8"
        )

    def _write_iteration_json(
        self, queue_id: str, iteration: int, data: dict[str, Any]
    ) -> None:
        """Write an iteration metadata JSON file."""
        path = (
            Path(self.queue_dir)
            / f"{queue_id}.iteration-{iteration}.json"
        )
        path.write_text(
            json.dumps(data, indent=2) + "\n", encoding="utf-8"
        )

    def _write_event_json(
        self, seq: int, event: dict[str, Any]
    ) -> None:
        """Write an event JSON file."""
        path = Path(self.events_dir) / f"event-{seq:05d}.json"
        enriched = {"seq": seq, **event}
        path.write_text(
            json.dumps(enriched, indent=2) + "\n", encoding="utf-8"
        )

    def _make_json_entry(
        self, queue_id: str, **overrides: Any
    ) -> dict[str, Any]:
        """Create a standard JSON queue entry dict."""
        entry: dict[str, Any] = {
            "id": queue_id,
            "spec_path": os.path.join(
                self.queue_dir, f"{queue_id}.spec.md"
            ),
            "original_spec_path": f"/tmp/specs/{queue_id}.md",
            "worktree": None,
            "priority": 100,
            "status": "queued",
            "phase": "execute",
            "submitted_at": "2026-01-01T00:00:00+00:00",
            "iteration": 0,
            "max_iterations": 30,
            "blocked_by": [],
            "last_worker": None,
            "last_iteration_at": None,
            "first_running_at": None,
            "consecutive_failures": 0,
            "cooldown_until": None,
            "tasks_done": 0,
            "tasks_total": 0,
            "sync_back": True,
            "project": None,
            "initial_task_ids": [],
        }
        entry.update(overrides)
        return entry


class TestMigrateQueueToDb(MigrateTestCase):
    """Verify migrate_queue_to_db() imports specs into SQLite."""

    def test_migrate_empty_dirs(self) -> None:
        """No files to migrate returns zero counts."""
        result = migrate_queue_to_db(
            self.db, self.queue_dir, self.events_dir, archive=False
        )
        self.assertEqual(result["specs"], 0)
        self.assertEqual(result["events"], 0)

    def test_migrate_single_spec(self) -> None:
        """Single q-NNN.json is imported into specs table."""
        entry = self._make_json_entry("q-001", priority=42)
        self._write_queue_json("q-001", entry)

        result = migrate_queue_to_db(
            self.db, self.queue_dir, self.events_dir, archive=False
        )
        self.assertEqual(result["specs"], 1)

        spec = self.db.get_spec("q-001")
        self.assertIsNotNone(spec)
        self.assertEqual(spec["priority"], 42)
        self.assertEqual(spec["status"], "queued")

    def test_migrate_multiple_specs(self) -> None:
        """Multiple q-NNN.json files are all imported."""
        for i in range(1, 4):
            sid = f"q-{i:03d}"
            entry = self._make_json_entry(sid, priority=100 + i)
            self._write_queue_json(sid, entry)

        result = migrate_queue_to_db(
            self.db, self.queue_dir, self.events_dir, archive=False
        )
        self.assertEqual(result["specs"], 3)

        for i in range(1, 4):
            sid = f"q-{i:03d}"
            spec = self.db.get_spec(sid)
            self.assertIsNotNone(spec)
            self.assertEqual(spec["priority"], 100 + i)

    def test_migrate_preserves_completed_status(self) -> None:
        """Completed specs retain their status and task counts."""
        entry = self._make_json_entry(
            "q-001",
            status="completed",
            iteration=5,
            tasks_done=10,
            tasks_total=10,
        )
        self._write_queue_json("q-001", entry)

        migrate_queue_to_db(
            self.db, self.queue_dir, self.events_dir, archive=False
        )

        spec = self.db.get_spec("q-001")
        self.assertEqual(spec["status"], "completed")
        self.assertEqual(spec["iteration"], 5)
        self.assertEqual(spec["tasks_done"], 10)

    def test_migrate_preserves_blocked_by(self) -> None:
        """Spec dependencies are migrated to spec_dependencies table."""
        entry_a = self._make_json_entry("q-001")
        entry_b = self._make_json_entry(
            "q-002", blocked_by=["q-001"]
        )
        self._write_queue_json("q-001", entry_a)
        self._write_queue_json("q-002", entry_b)

        migrate_queue_to_db(
            self.db, self.queue_dir, self.events_dir, archive=False
        )

        cursor = self.db.conn.execute(
            "SELECT blocks_on FROM spec_dependencies "
            "WHERE spec_id = 'q-002'"
        )
        deps = [row["blocks_on"] for row in cursor]
        self.assertEqual(deps, ["q-001"])


class TestMigrateIterations(MigrateTestCase):
    """Verify iteration JSON files are migrated to iterations table."""

    def test_migrate_iteration_files(self) -> None:
        """Iteration-N.json files are imported into iterations table."""
        entry = self._make_json_entry("q-001")
        self._write_queue_json("q-001", entry)

        self._write_iteration_json("q-001", 1, {
            "iteration": 1,
            "phase": "execute",
            "worker_id": "w-1",
            "started_at": "2026-01-01T00:00:00+00:00",
            "ended_at": "2026-01-01T00:05:00+00:00",
            "duration_seconds": 300,
            "tasks_completed": 2,
            "tasks_added": 0,
            "tasks_skipped": 0,
            "exit_code": 0,
        })

        migrate_queue_to_db(
            self.db, self.queue_dir, self.events_dir, archive=False
        )

        iterations = self.db.get_iterations("q-001")
        self.assertEqual(len(iterations), 1)
        self.assertEqual(iterations[0]["iteration"], 1)
        self.assertEqual(iterations[0]["phase"], "execute")
        self.assertEqual(iterations[0]["tasks_completed"], 2)
        self.assertEqual(iterations[0]["duration_seconds"], 300)

    def test_migrate_multiple_iterations(self) -> None:
        """Multiple iteration files for a spec are all imported."""
        entry = self._make_json_entry("q-001")
        self._write_queue_json("q-001", entry)

        for i in range(1, 4):
            self._write_iteration_json("q-001", i, {
                "iteration": i,
                "phase": "execute",
                "worker_id": "w-1",
                "started_at": f"2026-01-01T0{i}:00:00+00:00",
                "ended_at": f"2026-01-01T0{i}:05:00+00:00",
                "duration_seconds": 300,
                "tasks_completed": 1,
                "exit_code": 0,
            })

        migrate_queue_to_db(
            self.db, self.queue_dir, self.events_dir, archive=False
        )

        iterations = self.db.get_iterations("q-001")
        self.assertEqual(len(iterations), 3)


class TestMigrateEvents(MigrateTestCase):
    """Verify event JSON files are migrated to events table."""

    def test_migrate_single_event(self) -> None:
        """Single event file is imported into events table."""
        self._write_event_json(1, {
            "timestamp": "2026-01-01T00:00:00+00:00",
            "spec_id": "q-001",
            "event_type": "dispatched",
            "message": "Spec dispatched",
            "level": "info",
        })

        result = migrate_queue_to_db(
            self.db, self.queue_dir, self.events_dir, archive=False
        )
        self.assertEqual(result["events"], 1)

        events = self.db.get_events(spec_id="q-001")
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["event_type"], "dispatched")
        self.assertEqual(events[0]["message"], "Spec dispatched")

    def test_migrate_multiple_events(self) -> None:
        """Multiple event files are imported in sequence order."""
        for i in range(1, 4):
            self._write_event_json(i, {
                "timestamp": f"2026-01-01T00:0{i}:00+00:00",
                "spec_id": "q-001",
                "event_type": f"event-{i}",
                "message": f"Event {i}",
                "level": "info",
            })

        result = migrate_queue_to_db(
            self.db, self.queue_dir, self.events_dir, archive=False
        )
        self.assertEqual(result["events"], 3)

        events = self.db.get_events(spec_id="q-001")
        self.assertEqual(len(events), 3)

    def test_migrate_event_with_data(self) -> None:
        """Event data dict is serialized to JSON in the events table."""
        self._write_event_json(1, {
            "timestamp": "2026-01-01T00:00:00+00:00",
            "spec_id": "q-001",
            "event_type": "completed",
            "message": "Done",
            "data": {"tasks_done": 5, "exit_code": 0},
            "level": "info",
        })

        migrate_queue_to_db(
            self.db, self.queue_dir, self.events_dir, archive=False
        )

        events = self.db.get_events(spec_id="q-001")
        self.assertEqual(len(events), 1)
        data = json.loads(events[0]["data"])
        self.assertEqual(data["tasks_done"], 5)
        self.assertEqual(data["exit_code"], 0)

    def test_migrate_event_without_spec_id(self) -> None:
        """Events without spec_id are imported with NULL spec_id."""
        self._write_event_json(1, {
            "timestamp": "2026-01-01T00:00:00+00:00",
            "event_type": "daemon_started",
            "message": "Daemon started",
            "level": "info",
        })

        result = migrate_queue_to_db(
            self.db, self.queue_dir, self.events_dir, archive=False
        )
        self.assertEqual(result["events"], 1)

        events = self.db.get_events()
        # Filter to our event (exclude any auto-generated migration events)
        daemon_events = [
            e for e in events if e["event_type"] == "daemon_started"
        ]
        self.assertEqual(len(daemon_events), 1)
        self.assertIsNone(daemon_events[0]["spec_id"])

    def test_migrate_skips_malformed_event(self) -> None:
        """Malformed event files are skipped without error."""
        # Write a valid event
        self._write_event_json(1, {
            "timestamp": "2026-01-01T00:00:00+00:00",
            "event_type": "valid",
            "message": "Valid event",
            "level": "info",
        })
        # Write a malformed event (no event_type)
        self._write_event_json(2, {
            "timestamp": "2026-01-01T00:01:00+00:00",
            "message": "Missing event_type",
        })

        result = migrate_queue_to_db(
            self.db, self.queue_dir, self.events_dir, archive=False
        )
        self.assertEqual(result["events"], 1)

    def test_migrate_no_events_dir(self) -> None:
        """Missing events directory returns 0 events."""
        missing_dir = os.path.join(self.state_dir, "nonexistent")
        result = migrate_queue_to_db(
            self.db, self.queue_dir, missing_dir, archive=False
        )
        self.assertEqual(result["events"], 0)


class TestMigrateArchiving(MigrateTestCase):
    """Verify JSON files are archived after migration."""

    def test_archive_queue_files(self) -> None:
        """Queue JSON files are moved to queue/archive/."""
        entry = self._make_json_entry("q-001")
        self._write_queue_json("q-001", entry)

        migrate_queue_to_db(
            self.db, self.queue_dir, self.events_dir, archive=True
        )

        # Original should be gone
        self.assertFalse(
            (Path(self.queue_dir) / "q-001.json").exists()
        )
        # Archived copy should exist
        archive_dir = Path(self.queue_dir) / "archive"
        self.assertTrue(
            (archive_dir / "q-001.json").exists()
        )

    def test_archive_iteration_files(self) -> None:
        """Iteration JSON files are moved to queue/archive/."""
        entry = self._make_json_entry("q-001")
        self._write_queue_json("q-001", entry)
        self._write_iteration_json("q-001", 1, {
            "iteration": 1,
            "phase": "execute",
            "worker_id": "w-1",
            "started_at": "2026-01-01T00:00:00+00:00",
            "ended_at": "2026-01-01T00:05:00+00:00",
            "duration_seconds": 300,
            "exit_code": 0,
        })

        migrate_queue_to_db(
            self.db, self.queue_dir, self.events_dir, archive=True
        )

        # Original should be gone
        self.assertFalse(
            (
                Path(self.queue_dir) / "q-001.iteration-1.json"
            ).exists()
        )
        # Archived copy should exist
        archive_dir = Path(self.queue_dir) / "archive"
        self.assertTrue(
            (archive_dir / "q-001.iteration-1.json").exists()
        )

    def test_archive_event_files(self) -> None:
        """Event JSON files are moved to events/archive/."""
        self._write_event_json(1, {
            "timestamp": "2026-01-01T00:00:00+00:00",
            "event_type": "test",
            "message": "Test event",
            "level": "info",
        })

        migrate_queue_to_db(
            self.db, self.queue_dir, self.events_dir, archive=True
        )

        # Original should be gone
        self.assertFalse(
            (Path(self.events_dir) / "event-00001.json").exists()
        )
        # Archived copy should exist
        archive_dir = Path(self.events_dir) / "archive"
        self.assertTrue(
            (archive_dir / "event-00001.json").exists()
        )

    def test_no_archive_when_disabled(self) -> None:
        """With archive=False, JSON files remain in place."""
        entry = self._make_json_entry("q-001")
        self._write_queue_json("q-001", entry)
        self._write_event_json(1, {
            "timestamp": "2026-01-01T00:00:00+00:00",
            "event_type": "test",
            "message": "Test",
            "level": "info",
        })

        migrate_queue_to_db(
            self.db, self.queue_dir, self.events_dir, archive=False
        )

        # Originals should still exist
        self.assertTrue(
            (Path(self.queue_dir) / "q-001.json").exists()
        )
        self.assertTrue(
            (Path(self.events_dir) / "event-00001.json").exists()
        )
        # No archive dirs created
        self.assertFalse(
            (Path(self.queue_dir) / "archive").exists()
        )
        self.assertFalse(
            (Path(self.events_dir) / "archive").exists()
        )

    def test_non_queue_files_not_archived(self) -> None:
        """Spec copies and other non-JSON files are not archived."""
        entry = self._make_json_entry("q-001")
        self._write_queue_json("q-001", entry)

        # Write a spec copy (should NOT be archived)
        spec_copy = Path(self.queue_dir) / "q-001.spec.md"
        spec_copy.write_text("# Spec\n", encoding="utf-8")

        # Write a telemetry file (should NOT be archived)
        telem = Path(self.queue_dir) / "q-001.telemetry.json"
        telem.write_text("{}", encoding="utf-8")

        migrate_queue_to_db(
            self.db, self.queue_dir, self.events_dir, archive=True
        )

        # Spec copy and telemetry should remain
        self.assertTrue(spec_copy.exists())
        self.assertTrue(telem.exists())


class TestMigrateFullFlow(MigrateTestCase):
    """End-to-end migration: specs + iterations + events together."""

    def test_full_migration(self) -> None:
        """Migrates specs, iterations, and events in one call."""
        # Create 2 specs
        for i in range(1, 3):
            sid = f"q-{i:03d}"
            entry = self._make_json_entry(sid, priority=100 + i)
            self._write_queue_json(sid, entry)

        # Create iteration files for q-001
        self._write_iteration_json("q-001", 1, {
            "iteration": 1,
            "phase": "execute",
            "worker_id": "w-1",
            "started_at": "2026-01-01T00:00:00+00:00",
            "ended_at": "2026-01-01T00:05:00+00:00",
            "duration_seconds": 300,
            "tasks_completed": 2,
            "exit_code": 0,
        })

        # Create events
        self._write_event_json(1, {
            "timestamp": "2026-01-01T00:00:00+00:00",
            "spec_id": "q-001",
            "event_type": "dispatched",
            "message": "Dispatched q-001",
            "level": "info",
        })
        self._write_event_json(2, {
            "timestamp": "2026-01-01T00:01:00+00:00",
            "spec_id": "q-002",
            "event_type": "dispatched",
            "message": "Dispatched q-002",
            "level": "info",
        })

        result = migrate_queue_to_db(
            self.db, self.queue_dir, self.events_dir, archive=True
        )

        # Verify counts
        self.assertEqual(result["specs"], 2)
        self.assertEqual(result["events"], 2)

        # Verify specs in DB
        self.assertIsNotNone(self.db.get_spec("q-001"))
        self.assertIsNotNone(self.db.get_spec("q-002"))

        # Verify iterations in DB
        iterations = self.db.get_iterations("q-001")
        self.assertEqual(len(iterations), 1)

        # Verify events in DB
        events_q1 = self.db.get_events(spec_id="q-001")
        self.assertGreaterEqual(len(events_q1), 1)
        dispatched = [
            e for e in events_q1 if e["event_type"] == "dispatched"
        ]
        self.assertEqual(len(dispatched), 1)

        # Verify archiving
        archive_q = Path(self.queue_dir) / "archive"
        archive_e = Path(self.events_dir) / "archive"
        self.assertTrue(archive_q.is_dir())
        self.assertTrue(archive_e.is_dir())
        self.assertTrue(
            (archive_q / "q-001.json").exists()
        )
        self.assertTrue(
            (archive_e / "event-00001.json").exists()
        )


if __name__ == "__main__":
    unittest.main()
