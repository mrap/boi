"""
test_coordination.py — Tests for coordination.py

Tests:
  (a) acquire/release cycle
  (b) lock conflict detection
  (c) TTL expiry
  (d) announce/read cycle
  (e) topic pattern matching
  (f) concurrent access from two threads
"""

import json
import os
import sys
import tempfile
import threading
import time

# Ensure lib dir is on path
sys.path.insert(0, os.path.dirname(__file__))
from coordination import (
    acquire_lock,
    announce,
    check_lock,
    cleanup_expired,
    read_announcements,
    release_lock,
)


def make_db():
    """Create a fresh in-memory-style temp DB with the required tables."""
    import sqlite3
    tmp = tempfile.NamedTemporaryFile(suffix=".db", delete=False)
    tmp.close()
    db = tmp.name
    conn = sqlite3.connect(db, isolation_level=None)
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("""
        CREATE TABLE agent_locks (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            file_path   TEXT    NOT NULL,
            agent_id    TEXT    NOT NULL,
            acquired_at INTEGER NOT NULL,
            ttl_seconds INTEGER NOT NULL DEFAULT 300,
            released_at INTEGER
        )
    """)
    conn.execute(
        "CREATE INDEX idx_agent_locks_file_path ON agent_locks(file_path)"
    )
    conn.execute(
        "CREATE UNIQUE INDEX idx_agent_locks_active ON agent_locks(file_path) WHERE released_at IS NULL"
    )
    conn.execute("""
        CREATE TABLE agent_announcements (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            topic      TEXT    NOT NULL,
            agent_id   TEXT    NOT NULL,
            payload    TEXT    NOT NULL,
            priority   INTEGER NOT NULL DEFAULT 50,
            created_at INTEGER NOT NULL,
            expires_at INTEGER NOT NULL,
            read_by    TEXT    DEFAULT '[]'
        )
    """)
    conn.close()
    return db


def cleanup_db(db):
    try:
        os.unlink(db)
    except OSError:
        pass


PASS = "\033[32mPASS\033[0m"
FAIL = "\033[31mFAIL\033[0m"
failures = []


def check(name, condition, detail=""):
    if condition:
        print(f"  {PASS}  {name}")
    else:
        print(f"  {FAIL}  {name}{': ' + detail if detail else ''}")
        failures.append(name)


# ---------------------------------------------------------------------------
# (a) acquire / release cycle
# ---------------------------------------------------------------------------
def test_acquire_release():
    print("\n(a) acquire/release cycle")
    db = make_db()
    try:
        ok = acquire_lock(db, "me/test.md", "worker-1")
        check("acquire returns True", ok)

        info = check_lock(db, "me/test.md")
        check("check_lock returns dict", info is not None)
        check("check_lock.agent_id correct", info["agent_id"] == "worker-1")

        released = release_lock(db, "me/test.md", "worker-1")
        check("release returns True", released)

        info2 = check_lock(db, "me/test.md")
        check("check_lock returns None after release", info2 is None)
    finally:
        cleanup_db(db)


# ---------------------------------------------------------------------------
# (b) lock conflict detection
# ---------------------------------------------------------------------------
def test_conflict():
    print("\n(b) lock conflict detection")
    db = make_db()
    try:
        acquire_lock(db, "me/shared.md", "worker-A")
        ok = acquire_lock(db, "me/shared.md", "worker-B")
        check("second acquire returns False", not ok)

        # Releasing from wrong agent also returns False
        released = release_lock(db, "me/shared.md", "worker-B")
        check("release from wrong agent returns False", not released)

        # Correct agent can release
        released = release_lock(db, "me/shared.md", "worker-A")
        check("release from correct agent returns True", released)

        # Now worker-B can acquire
        ok2 = acquire_lock(db, "me/shared.md", "worker-B")
        check("acquire after release succeeds", ok2)
    finally:
        cleanup_db(db)


# ---------------------------------------------------------------------------
# (c) TTL expiry
# ---------------------------------------------------------------------------
def test_ttl_expiry():
    print("\n(c) TTL expiry")
    db = make_db()
    try:
        # Acquire with TTL=1 second
        acquire_lock(db, "me/ttl.md", "worker-1", ttl_seconds=1)

        info = check_lock(db, "me/ttl.md")
        check("lock held immediately", info is not None)

        time.sleep(2)

        # check_lock should treat expired lock as free
        info2 = check_lock(db, "me/ttl.md")
        check("lock expired after TTL", info2 is None)

        # cleanup_expired should release it in DB too
        cleanup_expired(db)
        # A new acquire should succeed
        ok = acquire_lock(db, "me/ttl.md", "worker-2")
        check("acquire succeeds after cleanup_expired", ok)
    finally:
        cleanup_db(db)


# ---------------------------------------------------------------------------
# (d) announce / read cycle
# ---------------------------------------------------------------------------
def test_announce_read():
    print("\n(d) announce/read cycle")
    db = make_db()
    try:
        ann_id = announce(db, "discovery.tool", "worker-1",
                          {"tool": "ripgrep"}, ttl_seconds=60)
        check("announce returns int id", isinstance(ann_id, int) and ann_id > 0)

        msgs = read_announcements(db, "discovery.tool", agent_id="worker-2")
        check("read returns 1 announcement", len(msgs) == 1)
        check("payload is correct",
              json.loads(msgs[0]["payload"]) == {"tool": "ripgrep"})

        # Check read_by was updated
        import sqlite3
        conn = sqlite3.connect(db)
        row = conn.execute(
            "SELECT read_by FROM agent_announcements WHERE id = ?", (ann_id,)
        ).fetchone()
        conn.close()
        readers = json.loads(row[0])
        check("read_by includes reader agent", "worker-2" in readers)
    finally:
        cleanup_db(db)


# ---------------------------------------------------------------------------
# (e) topic pattern matching
# ---------------------------------------------------------------------------
def test_topic_pattern():
    print("\n(e) topic pattern matching")
    db = make_db()
    try:
        announce(db, "alert.security", "worker-1", {"msg": "sec"}, ttl_seconds=60)
        announce(db, "alert.perf",     "worker-1", {"msg": "perf"}, ttl_seconds=60)
        announce(db, "discovery.tool", "worker-1", {"msg": "tool"}, ttl_seconds=60)
        announce(db, "status.landing", "worker-1", {"msg": "land"}, ttl_seconds=60)

        alerts = read_announcements(db, "alert.*")
        check("alert.* matches 2 alerts", len(alerts) == 2)

        all_msgs = read_announcements(db, "*")
        check("* matches all 4", len(all_msgs) == 4)

        exact = read_announcements(db, "discovery.tool")
        check("exact topic matches 1", len(exact) == 1)

        none = read_announcements(db, "nonexistent.*")
        check("no-match returns empty list", len(none) == 0)
    finally:
        cleanup_db(db)


# ---------------------------------------------------------------------------
# (f) concurrent access from two threads
# ---------------------------------------------------------------------------
def test_concurrent():
    print("\n(f) concurrent access from two threads")
    db = make_db()
    results = {"a": None, "b": None}
    errors = []

    def worker_a():
        try:
            results["a"] = acquire_lock(db, "me/concurrent.md", "worker-A", ttl_seconds=30)
        except Exception as e:
            errors.append(f"worker-A: {e}")

    def worker_b():
        try:
            results["b"] = acquire_lock(db, "me/concurrent.md", "worker-B", ttl_seconds=30)
        except Exception as e:
            errors.append(f"worker-B: {e}")

    t1 = threading.Thread(target=worker_a)
    t2 = threading.Thread(target=worker_b)
    t1.start()
    t2.start()
    t1.join()
    t2.join()

    check("no threading errors", len(errors) == 0, str(errors))
    check("exactly one winner",
          (results["a"] is True) != (results["b"] is True),
          f"a={results['a']} b={results['b']}")

    # Loser should fail; total locks held == 1
    import sqlite3
    conn = sqlite3.connect(db)
    count = conn.execute(
        "SELECT COUNT(*) FROM agent_locks WHERE released_at IS NULL"
    ).fetchone()[0]
    conn.close()
    check("exactly one active lock in DB", count == 1)
    cleanup_db(db)


# ---------------------------------------------------------------------------
# Run all tests
# ---------------------------------------------------------------------------
if __name__ == "__main__":
    test_acquire_release()
    test_conflict()
    test_ttl_expiry()
    test_announce_read()
    test_topic_pattern()
    test_concurrent()

    print()
    if failures:
        print(f"FAILED: {len(failures)} test(s): {', '.join(failures)}")
        sys.exit(1)
    else:
        print("All tests passed.")
