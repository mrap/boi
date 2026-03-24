# daemon_lock.py — Single-instance daemon locking for BOI.
#
# Uses fcntl.flock for atomic locking (no race conditions).
# The lock file lives at {state_dir}/daemon.lock, separate from
# the PID file at {state_dir}/daemon.pid.
#
# Two main exports:
#   - DaemonLock     — context-manager-style lock object
#   - daemon_status  — check if daemon is running and return info

import fcntl
import os
import sys
import time
from pathlib import Path
from typing import Optional


class DaemonLock:
    """Exclusive daemon lock using fcntl.flock.

    Ensures only one daemon instance runs at a time. The lock is
    held for the lifetime of the daemon process and released on
    stop or crash (the OS releases flocks when the FD is closed).

    Args:
        state_dir: Path to the BOI state directory (e.g. ~/.boi).
    """

    def __init__(self, state_dir: str) -> None:
        self.state_dir = state_dir
        self.lock_path = os.path.join(state_dir, "daemon.lock")
        self.pid_path = os.path.join(state_dir, "daemon.pid")
        self._lock_fd: Optional[int] = None
        self._lock_file: Optional[object] = None

    def acquire(self) -> None:
        """Acquire the daemon lock. Exits with SystemExit if held."""
        os.makedirs(self.state_dir, exist_ok=True)

        self._lock_file = open(self.lock_path, "w")
        try:
            fcntl.flock(self._lock_file, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except (BlockingIOError, OSError):
            # Lock is held by another process
            self._lock_file.close()
            self._lock_file = None

            # Read PID for the error message
            existing_pid = self._read_pid()
            if existing_pid is not None:
                print(
                    f"Daemon already running (PID {existing_pid})",
                    file=sys.stderr,
                )
            else:
                print("Daemon already running", file=sys.stderr)
            raise SystemExit(1)

        # Lock acquired. Write PID file atomically.
        self._write_pid(os.getpid())

    def release(self) -> None:
        """Release the daemon lock and clean up PID file."""
        if self._lock_file is not None:
            fcntl.flock(self._lock_file, fcntl.LOCK_UN)
            self._lock_file.close()
            self._lock_file = None

        # Clean up PID file
        try:
            os.remove(self.pid_path)
        except FileNotFoundError:
            pass

    def _write_pid(self, pid: int) -> None:
        """Atomically write PID to the PID file."""
        tmp = self.pid_path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            f.write(str(pid) + "\n")
        os.replace(tmp, self.pid_path)

    def _read_pid(self) -> Optional[int]:
        """Read PID from the PID file, or None if missing/invalid."""
        try:
            with open(self.pid_path, encoding="utf-8") as f:
                return int(f.read().strip())
        except (FileNotFoundError, ValueError):
            return None


def daemon_status(state_dir: str) -> dict:
    """Check whether the daemon is running.

    Returns a dict with:
        running: bool
        pid: int (only if running)
        uptime: float in seconds (only if running, estimated from heartbeat)
    """
    lock_path = os.path.join(state_dir, "daemon.lock")
    pid_path = os.path.join(state_dir, "daemon.pid")

    # Try to acquire the lock non-blocking. If we can, daemon is NOT running.
    if not os.path.exists(lock_path):
        return {"running": False}

    try:
        fd = open(lock_path, "w")
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            # We got the lock, so no daemon holds it
            fcntl.flock(fd, fcntl.LOCK_UN)
            fd.close()
            return {"running": False}
        except (BlockingIOError, OSError):
            # Lock is held by another process => daemon is running
            fd.close()
    except OSError:
        return {"running": False}

    # Daemon is running. Read PID.
    pid = None
    try:
        with open(pid_path, encoding="utf-8") as f:
            pid = int(f.read().strip())
    except (FileNotFoundError, ValueError):
        pass

    result: dict = {"running": True}
    if pid is not None:
        result["pid"] = pid

        # Estimate uptime from heartbeat file start time
        heartbeat_path = os.path.join(state_dir, "daemon-heartbeat")
        try:
            stat = os.stat(pid_path)
            result["uptime"] = time.time() - stat.st_mtime
        except OSError:
            pass

    return result
