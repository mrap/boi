# locking.py — File-based queue locking for BOI.
#
# Provides a context manager that uses fcntl.flock to prevent
# concurrent queue mutations. Tries non-blocking first, falls
# back to blocking with a warning.

import fcntl
import sys
from contextlib import contextmanager
from pathlib import Path
from typing import Generator


@contextmanager
def queue_lock(queue_dir: str) -> Generator[None, None, None]:
    """Acquire an exclusive flock on {queue_dir}/.lock.

    Tries LOCK_EX|LOCK_NB first. If contention is detected,
    logs a warning and blocks until the lock is available.
    The lock is released automatically on context exit.
    """
    path = Path(queue_dir)
    path.mkdir(parents=True, exist_ok=True)
    lock_file = path / ".lock"

    fd = open(lock_file, "w")
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except (BlockingIOError, OSError):
            print(
                f"Warning: queue lock contention on {lock_file}, blocking...",
                file=sys.stderr,
            )
            fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        fcntl.flock(fd, fcntl.LOCK_UN)
        fd.close()
