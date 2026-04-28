# db_migrate.py — Migrate JSON queue + event files to SQLite.
#
# Reads q-*.json files from the queue directory and event-*.json
# files from the events directory, imports everything into the
# SQLite database, then archives the original JSON files to
# archive/ subdirectories.
#
# This is the forward migration path (JSON -> SQLite). The inverse
# operation (SQLite -> JSON) lives in lib/db_to_json.py.

import json
import os
import shutil
from pathlib import Path
from typing import Any, Optional

from lib.db import Database
from lib.event_log import EVENT_FILENAME_PATTERN


def migrate_queue_to_db(
    db: Database,
    queue_dir: Optional[str] = None,
    events_dir: Optional[str] = None,
    archive: bool = True,
) -> dict[str, int]:
    """Migrate JSON queue files and event files to SQLite.

    Reads q-*.json files from queue_dir (specs + iterations),
    event-*.json files from events_dir, imports everything into
    SQLite, then optionally archives the original JSON files.

    Args:
        db: An open Database instance.
        queue_dir: Queue directory with q-*.json files.
            Defaults to db.queue_dir.
        events_dir: Events directory with event-*.json files.
            Defaults to <state_dir>/events where state_dir
            is the parent of queue_dir.
        archive: If True, move migrated JSON files to archive/
            subdirectories. If False, leave them in place.

    Returns:
        Dict with counts: specs, events.
    """
    q_dir = Path(queue_dir or db.queue_dir)

    # Determine events dir (sibling of queue dir)
    if events_dir is None:
        state_dir = q_dir.parent
        ev_dir = state_dir / "events"
    else:
        ev_dir = Path(events_dir)

    # 1. Migrate specs + iterations (reuses existing db method)
    specs_count = db.migrate_from_json(str(q_dir))

    # 2. Migrate event files
    events_count = _migrate_events(db, ev_dir)

    # 3. Archive original JSON files
    if archive:
        _archive_queue_files(q_dir)
        _archive_event_files(ev_dir)

    return {
        "specs": specs_count,
        "events": events_count,
    }


def _migrate_events(db: Database, events_dir: Path) -> int:
    """Import event-NNNNN.json files into the SQLite events table.

    Reads each event file, extracts timestamp, spec_id, event_type,
    message, data, and level fields, and inserts into the events
    table. Skips malformed files.

    Returns the number of events imported.
    """
    if not events_dir.is_dir():
        return 0

    imported = 0

    with db.lock:
        for f in sorted(events_dir.iterdir()):
            if not EVENT_FILENAME_PATTERN.match(f.name):
                continue

            try:
                event = json.loads(f.read_text(encoding="utf-8"))
            except (json.JSONDecodeError, OSError):
                continue

            timestamp = event.get("timestamp", "")
            if not timestamp:
                continue

            event_type = event.get("event_type", "")
            if not event_type:
                continue

            spec_id = event.get("spec_id")
            message = event.get("message")
            data = event.get("data")
            level = event.get("level", "info")

            # Encode data as JSON string if it's a dict/list
            data_str = None
            if data is not None:
                if isinstance(data, (dict, list)):
                    data_str = json.dumps(data)
                else:
                    data_str = str(data)

            try:
                db.conn.execute(
                    "INSERT INTO events "
                    "(timestamp, spec_id, event_type, message, "
                    "data, level) "
                    "VALUES (?, ?, ?, ?, ?, ?)",
                    (
                        timestamp,
                        spec_id,
                        event_type,
                        message,
                        data_str,
                        level,
                    ),
                )
                imported += 1
            except Exception:
                continue

        if imported > 0:
            db.conn.commit()

    return imported


def _archive_queue_files(queue_dir: Path) -> int:
    """Move q-*.json files to queue/archive/.

    Archives both q-NNN.json (spec entries) and
    q-NNN.iteration-N.json (iteration metadata) files.
    Skips telemetry files and non-JSON files.

    Returns the number of files archived.
    """
    if not queue_dir.is_dir():
        return 0

    archive_dir = queue_dir / "archive"
    archive_dir.mkdir(parents=True, exist_ok=True)

    archived = 0
    for f in sorted(queue_dir.iterdir()):
        if not f.is_file():
            continue
        if not f.name.startswith("q-"):
            continue
        if not f.name.endswith(".json"):
            continue
        # Skip telemetry files
        if ".telemetry" in f.name:
            continue

        dest = archive_dir / f.name
        shutil.move(str(f), str(dest))
        archived += 1

    return archived


def _archive_event_files(events_dir: Path) -> int:
    """Move event-NNNNN.json files to events/archive/.

    Returns the number of files archived.
    """
    if not events_dir.is_dir():
        return 0

    archive_dir = events_dir / "archive"
    archive_dir.mkdir(parents=True, exist_ok=True)

    archived = 0
    for f in sorted(events_dir.iterdir()):
        if not f.is_file():
            continue
        if not EVENT_FILENAME_PATTERN.match(f.name):
            continue

        dest = archive_dir / f.name
        shutil.move(str(f), str(dest))
        archived += 1

    return archived
