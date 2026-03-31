"""
coordination.py — Multi-agent coordination layer for BOI.

Provides file-level locking and announcement publishing using SQLite.
All functions are safe for concurrent access from multiple processes via WAL mode.

Usage (as library):
    from coordination import acquire_lock, release_lock, check_lock
    from coordination import announce, read_announcements, cleanup_expired

    DB = os.path.expanduser("~/.boi/boi.db")
    acquired = acquire_lock(DB, "me/learnings.md", "boi-worker-1", ttl_seconds=120)
    if acquired:
        # ... write file ...
        release_lock(DB, "me/learnings.md", "boi-worker-1")

Usage (as CLI):
    python3 coordination.py lock   me/learnings.md <agent_id>
    python3 coordination.py unlock me/learnings.md <agent_id>
    python3 coordination.py check  me/learnings.md
    python3 coordination.py announce <topic> <agent_id> <payload_json>
    python3 coordination.py read   <topic_pattern>
"""

import fnmatch
import json
import os
import sqlite3
import sys
import time

# Directory for lock-contention observations (created on demand)
_OBSERVATIONS_DIR = os.path.expanduser("~/.boi/evolution/coordination/observations")


# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------

def _conn(db_path: str) -> sqlite3.Connection:
    """Open a WAL-mode connection.  Callers must close it."""
    conn = sqlite3.connect(db_path, timeout=10, isolation_level=None)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA busy_timeout=10000")
    return conn


def _now() -> int:
    return int(time.time())


# ---------------------------------------------------------------------------
# Lock functions
# ---------------------------------------------------------------------------

def acquire_lock(db_path: str, file_path: str, agent_id: str,
                 ttl_seconds: int = 300) -> bool:
    """Attempt to acquire an exclusive lock on *file_path*.

    Expired locks (acquired_at + ttl_seconds < now) are released first.
    Returns True if the lock was acquired, False if another agent holds it.

    Example::
        ok = acquire_lock(DB, "me/learnings.md", "boi-worker-1", ttl_seconds=120)
    """
    now = _now()
    conn = _conn(db_path)
    try:
        with conn:
            # Release any expired locks on this file
            conn.execute(
                "UPDATE agent_locks SET released_at = ? "
                "WHERE file_path = ? AND released_at IS NULL "
                "  AND (acquired_at + ttl_seconds) < ?",
                (now, file_path, now),
            )

            # Check whether a live lock exists (not held by this agent)
            row = conn.execute(
                "SELECT agent_id FROM agent_locks "
                "WHERE file_path = ? AND released_at IS NULL",
                (file_path,),
            ).fetchone()

            if row is not None:
                if row["agent_id"] == agent_id:
                    # Already held by us — refresh TTL
                    conn.execute(
                        "UPDATE agent_locks SET acquired_at = ?, ttl_seconds = ? "
                        "WHERE file_path = ? AND agent_id = ? AND released_at IS NULL",
                        (now, ttl_seconds, file_path, agent_id),
                    )
                    return True
                return False  # Held by someone else

            try:
                conn.execute(
                    "INSERT INTO agent_locks (file_path, agent_id, acquired_at, ttl_seconds) "
                    "VALUES (?, ?, ?, ?)",
                    (file_path, agent_id, now, ttl_seconds),
                )
                return True
            except sqlite3.IntegrityError:
                # Another agent grabbed it between our check and insert
                return False
    finally:
        conn.close()


def release_lock(db_path: str, file_path: str, agent_id: str) -> bool:
    """Release the lock held by *agent_id* on *file_path*.

    Returns True if released, False if no such lock exists.

    Example::
        released = release_lock(DB, "me/learnings.md", "boi-worker-1")
    """
    now = _now()
    conn = _conn(db_path)
    try:
        with conn:
            cur = conn.execute(
                "UPDATE agent_locks SET released_at = ? "
                "WHERE file_path = ? AND agent_id = ? AND released_at IS NULL",
                (now, file_path, agent_id),
            )
            return cur.rowcount > 0
    finally:
        conn.close()


def check_lock(db_path: str, file_path: str) -> dict | None:
    """Return lock info dict if *file_path* is locked, else None.

    Example::
        info = check_lock(DB, "me/learnings.md")
        # {"id": 1, "file_path": "me/learnings.md", "agent_id": "boi-worker-1",
        #  "acquired_at": 1700000000, "ttl_seconds": 300, "released_at": None}
    """
    now = _now()
    conn = _conn(db_path)
    try:
        row = conn.execute(
            "SELECT * FROM agent_locks "
            "WHERE file_path = ? AND released_at IS NULL "
            "  AND (acquired_at + ttl_seconds) >= ?",
            (file_path, now),
        ).fetchone()
        if row is None:
            return None
        return dict(row)
    finally:
        conn.close()


# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------

def cleanup_expired(db_path: str) -> None:
    """Release expired locks and delete expired announcements.

    Intended to be called from the daemon's polling loop every ~5 seconds.

    Example::
        cleanup_expired(DB)
    """
    now = _now()
    conn = _conn(db_path)
    try:
        with conn:
            conn.execute(
                "UPDATE agent_locks SET released_at = ? "
                "WHERE released_at IS NULL AND (acquired_at + ttl_seconds) < ?",
                (now, now),
            )
            conn.execute(
                "DELETE FROM agent_announcements WHERE expires_at < ?",
                (now,),
            )
    finally:
        conn.close()


# ---------------------------------------------------------------------------
# Contention observation writer
# ---------------------------------------------------------------------------

def write_contention_observation(
    agent_id: str,
    file_path: str,
    duration_seconds: float,
    conflicting_agent_id: str,
    observations_dir: str = _OBSERVATIONS_DIR,
) -> str:
    """Write a JSON observation when a lock wait exceeds 10 seconds.

    Creates *observations_dir* if it does not exist.  Returns the path of the
    written file.

    Example::
        write_contention_observation(
            agent_id="boi-worker-2",
            file_path="me/learnings.md",
            duration_seconds=15.3,
            conflicting_agent_id="boi-worker-1",
        )
    """
    os.makedirs(observations_dir, exist_ok=True)
    ts = _now()
    # Use millisecond suffix to avoid collisions when multiple agents write at once
    ms = int(time.time() * 1000) % 1000
    filename = f"contention-{ts}-{ms}-{agent_id.replace('/', '_')}.json"
    obs = {
        "event": "lock_contention",
        "duration_seconds": round(duration_seconds, 2),
        "agent_id": agent_id,
        "file_path": file_path,
        "timestamp": ts,
        "conflicting_agent_id": conflicting_agent_id,
    }
    tmp_path = os.path.join(observations_dir, filename + ".tmp")
    final_path = os.path.join(observations_dir, filename)
    with open(tmp_path, "w") as fh:
        json.dump(obs, fh, indent=2)
    os.replace(tmp_path, final_path)
    return final_path


# ---------------------------------------------------------------------------
# Announcement functions
# ---------------------------------------------------------------------------

def announce(db_path: str, topic: str, agent_id: str, payload: dict | str,
             priority: int = 50, ttl_seconds: int = 3600) -> int:
    """Post an announcement and return its id.

    *payload* can be a dict (will be JSON-encoded) or a JSON string.

    Example::
        ann_id = announce(DB, "discovery.tool", "boi-worker-1",
                          {"tool": "ripgrep", "version": "13.0"}, priority=30)
    """
    if isinstance(payload, dict):
        payload = json.dumps(payload)
    now = _now()
    expires_at = now + ttl_seconds
    conn = _conn(db_path)
    try:
        with conn:
            cur = conn.execute(
                "INSERT INTO agent_announcements "
                "  (topic, agent_id, payload, priority, created_at, expires_at) "
                "VALUES (?, ?, ?, ?, ?, ?)",
                (topic, agent_id, payload, priority, now, expires_at),
            )
            return cur.lastrowid
    finally:
        conn.close()


def read_announcements(db_path: str, topic_pattern: str,
                       since_timestamp: int = 0,
                       agent_id: str | None = None) -> list[dict]:
    """Read announcements whose topic matches *topic_pattern* (glob syntax).

    Only returns non-expired announcements created after *since_timestamp*.
    If *agent_id* is given, marks each returned announcement as read by that agent.

    Supported patterns: "alert.*", "discovery.tool", "status.*", "*"

    Example::
        msgs = read_announcements(DB, "alert.*", agent_id="boi-worker-2")
    """
    now = _now()
    conn = _conn(db_path)
    try:
        rows = conn.execute(
            "SELECT * FROM agent_announcements "
            "WHERE created_at > ? AND expires_at >= ? "
            "ORDER BY priority ASC, created_at ASC",
            (since_timestamp, now),
        ).fetchall()

        results = []
        for row in rows:
            if not fnmatch.fnmatch(row["topic"], topic_pattern):
                continue
            d = dict(row)
            results.append(d)

        if agent_id and results:
            _mark_read(conn, [r["id"] for r in results], agent_id)

        return results
    finally:
        conn.close()


def _mark_read(conn: sqlite3.Connection, ids: list[int], agent_id: str) -> None:
    """Update read_by JSON arrays for the given announcement ids."""
    for ann_id in ids:
        row = conn.execute(
            "SELECT read_by FROM agent_announcements WHERE id = ?", (ann_id,)
        ).fetchone()
        if row is None:
            continue
        try:
            readers: list = json.loads(row["read_by"] or "[]")
        except json.JSONDecodeError:
            readers = []
        if agent_id not in readers:
            readers.append(agent_id)
            conn.execute(
                "UPDATE agent_announcements SET read_by = ? WHERE id = ?",
                (json.dumps(readers), ann_id),
            )


# ---------------------------------------------------------------------------
# CLI interface
# ---------------------------------------------------------------------------

def _cli() -> None:
    DB = os.path.expanduser("~/.boi/boi.db")
    args = sys.argv[1:]
    if not args:
        print("Usage: coordination.py <lock|unlock|check|announce|read> ...")
        sys.exit(1)

    cmd = args[0]

    if cmd == "lock":
        if len(args) < 3:
            print("Usage: coordination.py lock <file_path> <agent_id> [ttl_seconds]")
            sys.exit(1)
        file_path = args[1]
        agent_id = args[2]
        ttl = int(args[3]) if len(args) > 3 else 300
        # Retry up to 30 seconds with 5-second intervals
        _LOCK_WAIT_MAX = 30
        _LOCK_RETRY_INTERVAL = 5
        _OBSERVATION_THRESHOLD = 10  # seconds before writing a contention observation
        wait_start = time.time()
        observation_written = False
        while True:
            ok = acquire_lock(DB, file_path, agent_id, ttl_seconds=ttl)
            if ok:
                print(f"locked: {file_path} by {agent_id}")
                sys.exit(0)
            elapsed = time.time() - wait_start
            info = check_lock(DB, file_path)
            holder = info["agent_id"] if info else "unknown"
            if elapsed >= _LOCK_WAIT_MAX:
                print(f"lock_failed: {file_path} held by {holder} after {elapsed:.0f}s wait", file=sys.stderr)
                sys.exit(1)
            # Write contention observation once when threshold crossed
            if not observation_written and elapsed >= _OBSERVATION_THRESHOLD:
                observation_written = True
                try:
                    obs_path = write_contention_observation(
                        agent_id=agent_id,
                        file_path=file_path,
                        duration_seconds=elapsed,
                        conflicting_agent_id=holder,
                    )
                    print(f"contention_observation: {obs_path}", file=sys.stderr)
                except Exception as exc:
                    print(f"observation_write_failed: {exc}", file=sys.stderr)
            print(f"lock_wait: {file_path} held by {holder}, retrying in {_LOCK_RETRY_INTERVAL}s...", file=sys.stderr)
            time.sleep(_LOCK_RETRY_INTERVAL)

    elif cmd == "unlock":
        if len(args) < 3:
            print("Usage: coordination.py unlock <file_path> <agent_id>")
            sys.exit(1)
        file_path = args[1]
        agent_id = args[2]
        ok = release_lock(DB, file_path, agent_id)
        if ok:
            print(f"unlocked: {file_path}")
        else:
            print(f"not_locked: {file_path} was not held by {agent_id}", file=sys.stderr)
            sys.exit(1)

    elif cmd == "check":
        if len(args) < 2:
            print("Usage: coordination.py check <file_path>")
            sys.exit(1)
        file_path = args[1]
        info = check_lock(DB, file_path)
        if info:
            print(json.dumps(info, indent=2))
        else:
            print(f"free: {file_path}")

    elif cmd == "announce":
        if len(args) < 4:
            print("Usage: coordination.py announce <topic> <agent_id> <payload_json> [priority] [ttl_seconds]")
            sys.exit(1)
        topic = args[1]
        agent_id = args[2]
        payload = args[3]
        priority = int(args[4]) if len(args) > 4 else 50
        ttl = int(args[5]) if len(args) > 5 else 3600
        ann_id = announce(DB, topic, agent_id, payload, priority=priority, ttl_seconds=ttl)
        print(f"announced: id={ann_id} topic={topic}")

    elif cmd == "read":
        if len(args) < 2:
            print("Usage: coordination.py read <topic_pattern> [since_timestamp] [agent_id]")
            sys.exit(1)
        pattern = args[1]
        since = int(args[2]) if len(args) > 2 else 0
        reader_id = args[3] if len(args) > 3 else None
        msgs = read_announcements(DB, pattern, since_timestamp=since, agent_id=reader_id)
        print(json.dumps(msgs, indent=2))

    else:
        print(f"Unknown command: {cmd}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    _cli()
