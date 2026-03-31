#!/usr/bin/env python3
"""
Idempotent migration: adds agent_locks and agent_announcements tables to boi.db.
Uses stdlib sqlite3 only. Safe to run multiple times.
"""

import sqlite3
import os

DB_PATH = os.path.expanduser("~/.boi/boi.db")


def migrate(db_path: str = DB_PATH) -> None:
    conn = sqlite3.connect(db_path)
    conn.execute("PRAGMA journal_mode=WAL")
    cur = conn.cursor()

    # Check existing tables
    cur.execute("SELECT name FROM sqlite_master WHERE type='table'")
    existing = {row[0] for row in cur.fetchall()}

    if "agent_locks" not in existing:
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
        print("Created table: agent_locks")
    else:
        # Ensure unique partial index exists (added post-initial migration)
        conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_locks_active ON agent_locks(file_path) WHERE released_at IS NULL"
        )
        print("Table agent_locks already exists — skipping")

    if "agent_announcements" not in existing:
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
        conn.execute(
            "CREATE INDEX idx_agent_announcements_topic ON agent_announcements(topic)"
        )
        print("Created table: agent_announcements")
    else:
        print("Table agent_announcements already exists — skipping")

    conn.commit()
    conn.close()


if __name__ == "__main__":
    migrate()
    print("Migration complete.")
