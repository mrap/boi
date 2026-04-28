# lib/workspace_guard.py — Worktree boundary checking.
#
# Detects when a BOI worker writes files to the main repo instead of
# its assigned worktree. Emits boi.workspace.leak events via hex_emit.py
# if the emitter is available (graceful degradation if not installed).

import json
import logging
import os
import subprocess
from typing import Optional

logger = logging.getLogger("boi.workspace_guard")

HEX_EMIT_PATH = os.path.expanduser("~/.hex-events/hex_emit.py")


def get_main_repo(worktree_path: str) -> Optional[str]:
    """Return the main repo path for a given worktree.

    Uses ``git worktree list --porcelain``; the first entry is always
    the main (non-linked) worktree.

    Args:
        worktree_path: Path to any worktree (main or linked).

    Returns:
        Absolute path to the main repo, or None on failure.
    """
    try:
        result = subprocess.run(
            ["git", "-C", worktree_path, "worktree", "list", "--porcelain"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if result.returncode != 0:
            return None
        # First "worktree " line is always the main worktree
        for line in result.stdout.splitlines():
            if line.startswith("worktree "):
                return line[len("worktree "):].strip()
        return None
    except Exception:
        logger.exception(
            "Failed to get main repo for worktree %s", worktree_path
        )
        return None


def snapshot_git_status(repo_path: str) -> set:
    """Return the current ``git status --porcelain`` output as a set of lines.

    Args:
        repo_path: Path to any git repo (main or worktree).

    Returns:
        Set of non-empty status lines, or empty set on failure.
    """
    try:
        result = subprocess.run(
            ["git", "-C", repo_path, "status", "--porcelain"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if result.returncode != 0:
            return set()
        return {line for line in result.stdout.splitlines() if line.strip()}
    except Exception:
        logger.exception(
            "Failed to snapshot git status for %s", repo_path
        )
        return set()


def diff_status(pre: set, post: set) -> list:
    """Return file paths that are *new* in post relative to pre.

    Parses ``git status --porcelain`` lines (format: ``XY path``).

    Args:
        pre: Status line set before worker ran.
        post: Status line set after worker ran.

    Returns:
        Sorted list of newly dirty file paths.
    """
    new_lines = post - pre
    leaked: list = []
    for line in sorted(new_lines):
        parts = line.split(None, 1)
        if len(parts) >= 2:
            leaked.append(parts[1].strip())
        else:
            leaked.append(line)
    return leaked


def emit_leak_event(
    spec_id: str,
    worker_id: str,
    leaked_files: list,
    worktree_path: str,
) -> None:
    """Emit ``boi.workspace.leak`` event via hex_emit.py.

    Silently skips if hex_emit.py is not installed.

    Args:
        spec_id: Queue ID of the spec being processed.
        worker_id: Worker slot identifier.
        leaked_files: List of file paths that leaked into the main repo.
        worktree_path: Path to the worker's worktree.
    """
    payload = json.dumps({
        "spec_id": spec_id,
        "worker_id": worker_id,
        "leaked_files": leaked_files,
        "worktree_path": worktree_path,
    })

    if not os.path.isfile(HEX_EMIT_PATH):
        logger.debug(
            "hex_emit.py not found at %s — skipping event emit",
            HEX_EMIT_PATH,
        )
        return

    try:
        subprocess.run(
            [
                "python3",
                HEX_EMIT_PATH,
                "boi.workspace.leak",
                payload,
                "boi.worker",
            ],
            capture_output=True,
            text=True,
            timeout=10,
        )
        logger.info(
            "Emitted boi.workspace.leak for spec %s (%d files)",
            spec_id,
            len(leaked_files),
        )
    except Exception:
        logger.exception("Failed to emit boi.workspace.leak event")


class WorkspaceBoundaryChecker:
    """Detect when a worker writes outside its assigned worktree.

    Usage::

        checker = WorkspaceBoundaryChecker(worktree, spec_id, worker_id)
        checker.snapshot_before()      # call before worker runs
        # ... worker executes ...
        leaked = checker.check_after() # call after worker finishes

    If ``leaked`` is non-empty, a WARNING is logged and a
    ``boi.workspace.leak`` event is emitted (if hex-events is installed).

    Args:
        worktree_path: Absolute path to the worker's linked worktree.
        spec_id: Queue ID of the spec (used in event payload).
        worker_id: Worker slot identifier (used in event payload).
    """

    def __init__(
        self,
        worktree_path: str,
        spec_id: str = "",
        worker_id: str = "",
    ) -> None:
        self.worktree_path = worktree_path
        self.spec_id = spec_id
        self.worker_id = worker_id
        self.main_repo: Optional[str] = None
        self._pre_status: set = set()
        self._active = False

    def snapshot_before(self) -> None:
        """Capture the main repo's git status before the worker runs.

        Skips the check when:
        - The worktree IS the main repo (in-place execution).
        - The main repo path cannot be determined.
        """
        self.main_repo = get_main_repo(self.worktree_path)
        if self.main_repo is None:
            logger.warning(
                "workspace_guard: could not resolve main repo for %s",
                self.worktree_path,
            )
            return

        # In-place: worktree == main repo — no boundary to enforce
        if os.path.realpath(self.main_repo) == os.path.realpath(
            self.worktree_path
        ):
            logger.debug(
                "workspace_guard: worker is in main repo (in-place) — "
                "boundary check disabled"
            )
            self.main_repo = None
            return

        self._pre_status = snapshot_git_status(self.main_repo)
        self._active = True
        logger.debug(
            "workspace_guard: pre-snapshot captured (%d lines) for %s",
            len(self._pre_status),
            self.main_repo,
        )

    def check_after(self) -> list:
        """Compare main repo state against pre-snapshot; flag leaks.

        Returns:
            List of newly-dirty file paths in the main repo.
            Empty list if no boundary violation was detected (or check
            was skipped because the worker ran in-place).
        """
        if not self._active or self.main_repo is None:
            return []

        post_status = snapshot_git_status(self.main_repo)
        leaked = diff_status(self._pre_status, post_status)

        if leaked:
            logger.warning(
                "WORKSPACE BOUNDARY VIOLATION: spec=%s worker=%s "
                "leaked %d file(s) to main repo %s: %s",
                self.spec_id,
                self.worker_id,
                len(leaked),
                self.main_repo,
                leaked,
            )
            emit_leak_event(
                spec_id=self.spec_id,
                worker_id=self.worker_id,
                leaked_files=leaked,
                worktree_path=self.worktree_path,
            )
        else:
            logger.debug(
                "workspace_guard: boundary clean — no leaks from %s to %s",
                self.worktree_path,
                self.main_repo,
            )

        return leaked
