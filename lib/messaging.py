# messaging.py — File-based mailbox messaging for BOI workers.
#
# Provides inter-process communication between the daemon, workers,
# and CLI via JSON message files in ~/.boi/mailbox/.
#
# Transport:
#   - Orchestrator -> Worker: ~/.boi/mailbox/{spec_id}/{ts}-{TYPE}.json
#   - Worker -> Daemon:       ~/.boi/mailbox/daemon/{ts}-{TYPE}-{spec_id}.json
#
# Messages are written atomically (write .tmp, then mv).
# Acknowledgment = deleting the message file.
#
# See docs/messaging-protocol-design.md for full protocol spec.

import json
import os
import secrets
import shutil
import time
from datetime import datetime, timezone
from typing import Optional

# ── Message Types ────────────────────────────────────────────────────────

ORCHESTRATOR_MSG_TYPES = [
    "CANCEL",
    "SKIP",
    "PREEMPT",
    "DEPRIORITIZE",
    "NEW_DEP",
    "CONTEXT_UPDATE",
]

WORKER_MSG_TYPES = [
    "PROGRESS",
    "STUCK",
    "ESCALATE",
    "DISCOVERY",
]

ALL_MSG_TYPES = ORCHESTRATOR_MSG_TYPES + WORKER_MSG_TYPES

URGENT_MSG_TYPES = {"CANCEL", "PREEMPT"}

# ── Exit Codes ───────────────────────────────────────────────────────────

EXIT_CODES = {
    "CANCEL": 130,
    "SKIP": 131,
    "PREEMPT": 132,
    "DEPRIORITIZE": 133,
}

_EXIT_REASONS = {
    130: "canceled",
    131: "task_skipped",
    132: "preempted",
    133: "deprioritized",
    0: "normal",
}


def exit_reason(code: int) -> str:
    """Map an exit code to a human-readable reason."""
    return _EXIT_REASONS.get(code, "failure")


# ── Message Creation ─────────────────────────────────────────────────────


def create_message(
    msg_type: str,
    spec_id: str,
    sender: str,
    task_id: Optional[str] = None,
    payload: Optional[dict] = None,
) -> dict:
    """Create a message dict with the standard envelope.

    Args:
        msg_type: One of ALL_MSG_TYPES.
        spec_id: Target spec queue ID (e.g. "q-012").
        sender: Who sent it ("daemon", "worker", "cli", "hex-events").
        task_id: Optional task ID (e.g. "t-3").
        payload: Optional type-specific payload dict.

    Returns:
        Message dict ready for serialization.

    Raises:
        ValueError: If msg_type is not recognized.
    """
    if msg_type not in ALL_MSG_TYPES:
        raise ValueError(
            f"Unknown message type: {msg_type}. Must be one of: {ALL_MSG_TYPES}"
        )

    ts_ms = int(time.time() * 1000)
    rand = secrets.token_hex(3)

    return {
        "version": 1,
        "id": f"msg-{ts_ms}-{rand}",
        "type": msg_type,
        "spec_id": spec_id,
        "task_id": task_id,
        "timestamp": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "sender": sender,
        "payload": payload or {},
    }


# ── Send ─────────────────────────────────────────────────────────────────


def send_message(
    spec_id: str,
    msg_type: str,
    payload: dict,
    sender: str,
    state_dir: str,
    task_id: Optional[str] = None,
    direction: str = "to_worker",
) -> dict:
    """Write a message to the file-based mailbox.

    Args:
        spec_id: Target spec queue ID.
        msg_type: Message type (e.g. "CANCEL", "PROGRESS").
        payload: Type-specific payload dict.
        sender: Who sent it.
        state_dir: Path to ~/.boi state directory.
        task_id: Optional task ID for task-scoped messages.
        direction: "to_worker" or "to_daemon".

    Returns:
        The message dict that was written.
    """
    msg = create_message(msg_type, spec_id, sender, task_id, payload)

    rand = secrets.token_hex(3)
    ts_ms = int(time.time() * 1000)

    if direction == "to_daemon":
        mailbox_dir = os.path.join(state_dir, "mailbox", "daemon")
        filename = f"{ts_ms}-{rand}-{msg_type}-{spec_id}.json"
    else:
        mailbox_dir = os.path.join(state_dir, "mailbox", spec_id)
        filename = f"{ts_ms}-{rand}-{msg_type}.json"

    os.makedirs(mailbox_dir, exist_ok=True)

    tmp_path = os.path.join(mailbox_dir, f".{filename}.tmp")
    final_path = os.path.join(mailbox_dir, filename)

    with open(tmp_path, "w", encoding="utf-8") as f:
        json.dump(msg, f, indent=2)
    os.rename(tmp_path, final_path)

    # Store the filename in the message for ack lookups
    msg["_filename"] = filename
    msg["_direction"] = direction

    return msg


# ── Receive ──────────────────────────────────────────────────────────────


def receive_messages(
    spec_id: str,
    state_dir: str,
    msg_types: Optional[list] = None,
) -> list:
    """Read all pending messages for a spec from its mailbox.

    Args:
        spec_id: Spec queue ID.
        state_dir: Path to ~/.boi state directory.
        msg_types: Optional filter. Only return messages of these types.

    Returns:
        List of message dicts, sorted by timestamp (filename order).
    """
    mailbox_dir = os.path.join(state_dir, "mailbox", spec_id)
    if not os.path.isdir(mailbox_dir):
        return []

    messages = []
    try:
        for fname in sorted(os.listdir(mailbox_dir)):
            if not fname.endswith(".json") or fname.startswith("."):
                continue

            if msg_types:
                # Filename: {ts}-{rand}-{TYPE}.json
                matched = False
                for mt in msg_types:
                    if f"-{mt}.json" in fname:
                        matched = True
                        break
                if not matched:
                    continue

            fpath = os.path.join(mailbox_dir, fname)
            try:
                with open(fpath, encoding="utf-8") as f:
                    msg = json.load(f)
                msg["_filename"] = fname
                msg["_direction"] = "to_worker"
                messages.append(msg)
            except (json.JSONDecodeError, OSError):
                continue
    except OSError:
        pass

    return messages


def receive_daemon_messages(
    state_dir: str,
    spec_id: Optional[str] = None,
) -> list:
    """Read messages sent to the daemon.

    Args:
        state_dir: Path to ~/.boi state directory.
        spec_id: Optional filter by spec ID.

    Returns:
        List of message dicts, sorted by timestamp.
    """
    daemon_dir = os.path.join(state_dir, "mailbox", "daemon")
    if not os.path.isdir(daemon_dir):
        return []

    messages = []
    try:
        for fname in sorted(os.listdir(daemon_dir)):
            if not fname.endswith(".json") or fname.startswith("."):
                continue

            if spec_id and not fname.endswith(f"-{spec_id}.json"):
                continue

            fpath = os.path.join(daemon_dir, fname)
            try:
                with open(fpath, encoding="utf-8") as f:
                    msg = json.load(f)
                msg["_filename"] = fname
                msg["_direction"] = "to_daemon"
                messages.append(msg)
            except (json.JSONDecodeError, OSError):
                continue
    except OSError:
        pass

    return messages


# ── Acknowledge ──────────────────────────────────────────────────────────


def ack_message(msg: dict, spec_id: str, state_dir: str) -> None:
    """Acknowledge a worker-bound message by deleting its file.

    Idempotent: no error if the file is already gone.
    """
    filename = msg.get("_filename")
    if not filename:
        return

    fpath = os.path.join(state_dir, "mailbox", spec_id, filename)
    try:
        os.remove(fpath)
    except FileNotFoundError:
        pass


def ack_daemon_message(msg: dict, state_dir: str) -> None:
    """Acknowledge a daemon-bound message by deleting its file.

    Idempotent: no error if the file is already gone.
    """
    filename = msg.get("_filename")
    if not filename:
        return

    fpath = os.path.join(state_dir, "mailbox", "daemon", filename)
    try:
        os.remove(fpath)
    except FileNotFoundError:
        pass


# ── Urgent Check ─────────────────────────────────────────────────────────


def check_urgent(spec_id: str, state_dir: str) -> Optional[str]:
    """Check for urgent messages (CANCEL, PREEMPT) in a spec's mailbox.

    Used by the worker's tmux poll loop for fast response.

    Returns:
        The urgent message type if found, None otherwise.
    """
    mailbox_dir = os.path.join(state_dir, "mailbox", spec_id)
    if not os.path.isdir(mailbox_dir):
        return None

    try:
        for fname in sorted(os.listdir(mailbox_dir)):
            if fname.startswith("."):
                continue
            for urgent_type in ("CANCEL", "PREEMPT"):
                if fname.endswith(f"-{urgent_type}.json"):
                    return urgent_type
    except OSError:
        pass

    return None


# ── Cleanup ──────────────────────────────────────────────────────────────


def cleanup_mailbox(spec_id: str, state_dir: str) -> int:
    """Remove a spec's mailbox directory and all messages.

    Args:
        spec_id: Spec queue ID.
        state_dir: Path to ~/.boi state directory.

    Returns:
        Number of unprocessed message files that were removed.
    """
    mailbox_dir = os.path.join(state_dir, "mailbox", spec_id)
    if not os.path.isdir(mailbox_dir):
        return 0

    count = 0
    try:
        for fname in os.listdir(mailbox_dir):
            if fname.endswith(".json") and not fname.startswith("."):
                count += 1
    except OSError:
        pass

    shutil.rmtree(mailbox_dir, ignore_errors=True)
    return count
