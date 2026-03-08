# event_log.py — Append-only numbered JSON event log for BOI.
#
# Events are stored as individual JSON files in a directory:
#   event-00001.json, event-00002.json, ...
# Each file contains one JSON object. Writes are atomic (write .tmp, then mv).
# Events are read back in sequence-number order.

import json
import os
import re
from pathlib import Path
from typing import Any, Optional


EVENT_FILENAME_PATTERN = re.compile(r"^event-(\d{5})\.json$")


def _event_filename(seq: int) -> str:
    """Return the filename for a given sequence number."""
    return f"event-{seq:05d}.json"


def get_next_sequence(events_dir: str) -> int:
    """Return the next sequence number to use (1-indexed).

    Scans existing event files and returns max + 1.
    Returns 1 if the directory is empty or doesn't exist.
    """
    path = Path(events_dir)
    if not path.is_dir():
        return 1

    max_seq = 0
    for entry in path.iterdir():
        match = EVENT_FILENAME_PATTERN.match(entry.name)
        if match:
            seq = int(match.group(1))
            if seq > max_seq:
                max_seq = seq

    return max_seq + 1


MAX_WRITE_RETRIES = 3


def write_event(events_dir: str, event: dict[str, Any]) -> int:
    """Write an event to the log. Returns the assigned sequence number.

    Writes atomically: data goes to a .tmp file first, then is renamed.
    Retries up to MAX_WRITE_RETRIES times with an incremented sequence
    number if a FileExistsError race is detected.
    """
    path = Path(events_dir)
    path.mkdir(parents=True, exist_ok=True)

    last_error: FileExistsError | None = None
    for attempt in range(MAX_WRITE_RETRIES):
        seq = get_next_sequence(events_dir)
        filename = _event_filename(seq)
        target = path / filename
        tmp = path / f".{filename}.tmp"

        # Add sequence number to event data
        enriched = {"seq": seq, **event}
        data = json.dumps(enriched, indent=2, sort_keys=False) + "\n"

        # Atomic write: .tmp then mv
        try:
            tmp.write_text(data, encoding="utf-8")
            try:
                fd = os.open(str(target), os.O_CREAT | os.O_EXCL | os.O_WRONLY)
                os.close(fd)
            except FileExistsError:
                tmp.unlink()
                last_error = FileExistsError(f"Event file already exists: {target}")
                continue
            os.rename(str(tmp), str(target))
            return seq
        except BaseException:
            if tmp.exists():
                tmp.unlink()
            raise

    # All retries exhausted
    raise last_error  # type: ignore[misc]


def read_events(events_dir: str) -> list[dict[str, Any]]:
    """Read all events from the log in sequence order.

    Returns an empty list if the directory doesn't exist or is empty.
    Skips malformed files (logs a warning to stderr).
    """
    path = Path(events_dir)
    if not path.is_dir():
        return []

    events: list[tuple[int, dict[str, Any]]] = []
    for entry in sorted(path.iterdir()):
        match = EVENT_FILENAME_PATTERN.match(entry.name)
        if not match:
            continue
        seq = int(match.group(1))
        try:
            data = json.loads(entry.read_text(encoding="utf-8"))
            events.append((seq, data))
        except (json.JSONDecodeError, OSError) as e:
            import sys

            print(
                f"Warning: skipping malformed event {entry.name}: {e}", file=sys.stderr
            )

    events.sort(key=lambda x: x[0])
    return [ev for _, ev in events]


def read_event(events_dir: str, seq: int) -> Optional[dict[str, Any]]:
    """Read a single event by sequence number. Returns None if not found."""
    path = Path(events_dir) / _event_filename(seq)
    if not path.is_file():
        return None
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError):
        return None


def count_events(events_dir: str) -> int:
    """Return the number of event files in the directory."""
    path = Path(events_dir)
    if not path.is_dir():
        return 0
    return sum(1 for e in path.iterdir() if EVENT_FILENAME_PATTERN.match(e.name))
