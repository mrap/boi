#!/usr/bin/env python3
"""Integration test for the hex-ops BOI → hex-events bridge.

Tests:
  1. boi.spec.completed triggers ops-completion-verify and ops-spec-digest policies
  2. boi.spec.failed x3 with same reason triggers ops-failure-pattern policy

Uses real hex-events DB and policies, but processes events in-process (no daemon
dependency). Events are emitted via hex_emit.py subprocess as required.

Usage:
  python3 test_hex_ops_bridge.py
"""
import json
import os
import sqlite3
import subprocess
import sys
import time
import uuid

HEX_EVENTS_DIR = os.path.expanduser("~/.hex-events")
HEX_EMIT = os.path.join(HEX_EVENTS_DIR, "hex_emit.py")
EVENTS_DB = os.path.join(HEX_EVENTS_DIR, "events.db")
DIGEST_FILE = os.path.expanduser("~/.boi/ops-digest.md")

FAIL = False
PASS = True


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def ensure_dedup_column(db_path: str):
    """Ensure dedup_key column exists so hex_emit.py doesn't crash on schema setup."""
    conn = sqlite3.connect(db_path)
    cols = [row[1] for row in conn.execute("PRAGMA table_info(events)").fetchall()]
    if "dedup_key" not in cols:
        conn.execute("ALTER TABLE events ADD COLUMN dedup_key TEXT")
        conn.commit()
    conn.close()


def emit_event(event_type: str, payload: dict, source: str = "test-hex-ops-bridge") -> int:
    """Emit an event via hex_emit.py subprocess. Returns the inserted event id."""
    payload_json = json.dumps(payload)
    result = subprocess.run(
        [sys.executable, HEX_EMIT, event_type, payload_json, source],
        capture_output=True, text=True, timeout=10,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"hex_emit.py failed ({result.returncode}): {result.stderr.strip()}"
        )
    # Parse: "Event 42: boi.spec.completed"
    line = result.stdout.strip()
    try:
        eid = int(line.split()[1].rstrip(":"))
    except (IndexError, ValueError):
        raise RuntimeError(f"Unexpected hex_emit output: {line!r}")
    print(f"    emitted: {line}")
    return eid


def setup_hex_events_path():
    """Add hex-events to sys.path so we can import its modules."""
    if HEX_EVENTS_DIR not in sys.path:
        sys.path.insert(0, HEX_EVENTS_DIR)


def process_event_in_process(event_id: int) -> bool:
    """Load event from DB and process it using hex-events policy engine.

    Imports hex-events modules directly to process without requiring the daemon.
    Returns True if the event was found and processed.
    """
    setup_hex_events_path()
    from db import EventsDB
    from policy import load_policies
    from hex_eventd import _process_event_policies

    db = EventsDB(EVENTS_DB)
    event_row = db.conn.execute(
        "SELECT * FROM events WHERE id = ?", (event_id,)
    ).fetchone()
    if not event_row:
        db.close()
        return False

    # Mark as processed if already done (e.g., daemon already handled it)
    if event_row["processed_at"]:
        db.close()
        return True

    policies = load_policies(os.path.join(HEX_EVENTS_DIR, "policies"))

    # Track max id BEFORE processing to detect newly emitted downstream events
    max_id_before = db.conn.execute("SELECT COALESCE(MAX(id), 0) FROM events").fetchone()[0]
    _process_event_policies(dict(event_row), policies, db)

    # Process downstream events emitted by this action (e.g. ops.spec.verified)
    # Only process events with IDs > max_id_before to avoid draining the backlog
    max_rounds = 5
    current_max = max_id_before
    for _ in range(max_rounds):
        new_unprocessed = db.conn.execute(
            "SELECT * FROM events WHERE id > ? AND processed_at IS NULL ORDER BY id",
            (current_max,),
        ).fetchall()
        if not new_unprocessed:
            break
        current_max = new_unprocessed[-1]["id"]
        for row in new_unprocessed:
            _process_event_policies(dict(row), policies, db)

    db.close()
    return True


def query_events_since(event_type: str, after_id: int) -> list[dict]:
    """Return all events of event_type with id > after_id."""
    conn = sqlite3.connect(EVENTS_DB)
    conn.row_factory = sqlite3.Row
    rows = conn.execute(
        "SELECT * FROM events WHERE event_type = ? AND id > ? ORDER BY id",
        (event_type, after_id),
    ).fetchall()
    conn.close()
    return [dict(r) for r in rows]


def get_max_event_id() -> int:
    """Return current max event id (watermark)."""
    conn = sqlite3.connect(EVENTS_DB)
    row = conn.execute("SELECT COALESCE(MAX(id), 0) FROM events").fetchone()
    conn.close()
    return row[0]


def check_digest_appended(before_size: int, queue_id: str) -> tuple[bool, str]:
    """Return (ok, message) checking whether digest was updated with queue_id."""
    if not os.path.exists(DIGEST_FILE):
        return False, f"digest file does not exist: {DIGEST_FILE}"
    with open(DIGEST_FILE) as f:
        content = f.read()
    after_size = len(content.encode())
    if after_size <= before_size:
        return False, f"digest not grown (was {before_size}B, now {after_size}B)"
    if queue_id in content:
        return True, f"found {queue_id!r} in digest (grew by {after_size - before_size}B)"
    return False, f"digest grew but {queue_id!r} not found in new content"


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

def test_completed_event() -> bool:
    print("\n=== Test 1: boi.spec.completed → verify + digest ===")

    watermark = get_max_event_id()
    digest_before = (
        os.path.getsize(DIGEST_FILE) if os.path.exists(DIGEST_FILE) else 0
    )

    # Use a real spec path so ops-completion-verify can open it
    spec_path = os.path.expanduser("~/.boi/queue/q-153.spec.md")
    queue_id = "test-q-integ-completed"
    payload = {
        "queue_id": queue_id,
        "spec_path": spec_path,
        "tasks_done": 5,
        "tasks_total": 6,
        "outcome": "completed",
        "reason": "all tasks done",
    }

    print(f"  Emitting boi.spec.completed (queue_id={queue_id}) ...")
    trigger_id = emit_event("boi.spec.completed", payload)

    print(f"  Processing event {trigger_id} via policy engine ...")
    ok = process_event_in_process(trigger_id)
    if not ok:
        print(f"  FAIL: event {trigger_id} not found in DB")
        return FAIL
    print(f"  OK: event processed")

    # Check for ops.spec.verified or ops.spec.output-missing
    verified = query_events_since("ops.spec.verified", watermark)
    missing = query_events_since("ops.spec.output-missing", watermark)

    if verified:
        print(f"  PASS: ops.spec.verified emitted (id={verified[0]['id']})")
    elif missing:
        print(
            f"  PASS: ops.spec.output-missing emitted (id={missing[0]['id']}) "
            "— no test -f paths found in spec"
        )
    else:
        print("  FAIL: neither ops.spec.verified nor ops.spec.output-missing was emitted")
        return FAIL

    # Check digest
    ok, msg = check_digest_appended(digest_before, queue_id)
    if ok:
        print(f"  PASS: digest appended — {msg}")
    else:
        print(f"  FAIL: digest not updated — {msg}")
        return FAIL

    return PASS


def test_failure_pattern() -> bool:
    print("\n=== Test 2: boi.spec.failed x3 → ops.pattern.detected ===")

    watermark = get_max_event_id()
    run_id = uuid.uuid4().hex[:8]
    reason = f"test-integ-{run_id}: connection refused to target service"

    print("  Emitting 3 boi.spec.failed events with same reason ...")
    fail_ids = []
    for i in range(3):
        payload = {
            "queue_id": f"test-q-fail-{i + 1}",
            "spec_path": "~/.boi/queue/nonexistent.spec.md",
            "tasks_done": 0,
            "tasks_total": 3,
            "outcome": "failed",
            "reason": reason,
        }
        eid = emit_event("boi.spec.failed", payload)
        fail_ids.append(eid)

    print(f"  Processing failure events {fail_ids} via policy engine ...")
    for eid in fail_ids:
        process_event_in_process(eid)
    print("  OK: failure events processed")

    # Check for ops.pattern.detected
    pattern_events = query_events_since("ops.pattern.detected", watermark)
    if pattern_events:
        print(f"  PASS: ops.pattern.detected emitted (id={pattern_events[0]['id']})")
    else:
        print("  FAIL: ops.pattern.detected not emitted after 3 failures with same reason")
        return FAIL

    return PASS


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    # Preflight
    if not os.path.exists(HEX_EMIT):
        print(f"ERROR: hex_emit.py not found: {HEX_EMIT}")
        sys.exit(1)
    if not os.path.exists(EVENTS_DB):
        print(f"ERROR: events.db not found: {EVENTS_DB}")
        sys.exit(1)

    # Ensure DB schema is up-to-date (dedup_key column) before calling hex_emit.py
    ensure_dedup_column(EVENTS_DB)

    print("hex-ops bridge integration test")
    print(f"  DB:       {EVENTS_DB}")
    print(f"  hex_emit: {HEX_EMIT}")
    print(f"  digest:   {DIGEST_FILE}")

    results = [
        ("boi.spec.completed → verify + digest", test_completed_event()),
        ("boi.spec.failed x3 → pattern.detected", test_failure_pattern()),
    ]

    print("\n=== Results ===")
    passed = sum(1 for _, ok in results if ok)
    for name, ok in results:
        print(f"  {'PASS' if ok else 'FAIL'}: {name}")

    print(f"\n{passed}/{len(results)} passed")
    sys.exit(0 if passed == len(results) else 1)


if __name__ == "__main__":
    main()
