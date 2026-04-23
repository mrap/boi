# lib/task_worktree.py — Fresh-worktree-per-task for parallel BOI execution.
#
# Each parallel task gets its own git worktree, isolated from the shared
# worker checkout. This eliminates stale-state bugs when multiple tasks
# run concurrently within the same spec.
#
# API:
#   create_task_worktree(main_repo, worker_id, spec_id, task_id) -> dict
#   remove_task_worktree(main_repo, worktree_path) -> None
#   merge_level_branches(main_repo, spec_id, task_branches) -> dict

from __future__ import annotations

import logging
import os
import subprocess
from pathlib import Path
from typing import Optional

logger = logging.getLogger("boi.task_worktree")

WORKTREES_DIR = os.path.expanduser("~/.boi/worktrees")


def get_main_repo_from_worker(worktree_path: str) -> Optional[str]:
    """Derive the main git repo path from a worker's worktree .git file.

    Worker worktrees have a .git FILE (not directory) containing:
      gitdir: /path/to/main/.git/worktrees/<name>

    We parse that file to extract the main repo root.
    Falls back to trying git commands if the .git file is absent.
    """
    git_path = os.path.join(worktree_path, ".git")
    if os.path.isfile(git_path):
        try:
            content = Path(git_path).read_text(encoding="utf-8").strip()
            if content.startswith("gitdir:"):
                gitdir = content[len("gitdir:"):].strip()
                # gitdir looks like: /path/to/.git/worktrees/<name>
                # Strip everything from /.git/ onward to get repo root.
                marker = "/.git/"
                idx = gitdir.find(marker)
                if idx != -1:
                    return gitdir[:idx]
        except OSError:
            pass

    # Fallback: try git command from the worktree itself.
    try:
        result = subprocess.run(
            ["git", "-C", worktree_path, "rev-parse", "--absolute-git-dir"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if result.returncode == 0:
            git_dir = result.stdout.strip()
            if git_dir.endswith("/.git"):
                return git_dir[:-len("/.git")]
            # Linked worktree: .git/worktrees/<name>
            marker = "/.git/"
            idx = git_dir.find(marker)
            if idx != -1:
                return git_dir[:idx]
    except Exception:
        pass

    return None


def compute_task_worktree_path(worker_id: str, spec_id: str, task_id: str) -> str:
    """Return the path where the task's worktree should be created.

    Convention: ~/.boi/worktrees/<worker_id>-<spec_id>-<task_id>
    E.g. ~/.boi/worktrees/w-1-q-123-t-2
    """
    name = f"{worker_id}-{spec_id}-{task_id}"
    return os.path.join(WORKTREES_DIR, name)


def compute_branch_name(spec_id: str, task_id: str) -> str:
    """Return the task-specific git branch name.

    Convention: boi/<spec_id>/<task_id>  e.g. boi/q-123/t-2
    """
    return f"boi/{spec_id}/{task_id}"


def create_task_worktree(
    main_repo: str,
    worker_id: str,
    spec_id: str,
    task_id: str,
) -> dict:
    """Create a fresh git worktree for a single parallel task.

    Steps:
    1. Compute the worktree path and branch name.
    2. Run: git worktree add -b <branch> <path> HEAD
    3. Return dict with worktree_path and branch_name.

    Args:
        main_repo: Absolute path to the main git repository.
        worker_id: Worker slot ID (e.g. "w-1").
        spec_id: Spec queue ID (e.g. "q-123").
        task_id: Task ID within the spec (e.g. "t-2").

    Returns:
        {"worktree_path": str, "branch_name": str}

    Raises:
        RuntimeError if git worktree add fails.
    """
    wt_path = compute_task_worktree_path(worker_id, spec_id, task_id)
    branch = compute_branch_name(spec_id, task_id)

    # Remove stale worktree dir if it somehow exists.
    if os.path.isdir(wt_path):
        _force_remove_worktree(main_repo, wt_path)

    # Clean up any stale branch reference.
    _delete_branch_if_exists(main_repo, branch)

    result = subprocess.run(
        ["git", "-C", main_repo, "worktree", "add", "-b", branch, wt_path, "HEAD"],
        capture_output=True,
        text=True,
    )

    if result.returncode != 0:
        raise RuntimeError(
            f"git worktree add failed for {spec_id}/{task_id}: "
            f"{result.stderr.strip()}"
        )

    logger.info(
        "Created task worktree: path=%s branch=%s", wt_path, branch
    )
    return {"worktree_path": wt_path, "branch_name": branch}


def remove_task_worktree(main_repo: str, worktree_path: str) -> None:
    """Remove a task's worktree after completion.

    Uses --force because the worktree may have untracked changes.
    Logs errors but does not raise.
    """
    if not os.path.exists(worktree_path):
        logger.debug("Worktree already gone: %s", worktree_path)
        return

    _force_remove_worktree(main_repo, worktree_path)


def _force_remove_worktree(main_repo: str, worktree_path: str) -> None:
    result = subprocess.run(
        ["git", "-C", main_repo, "worktree", "remove", "--force", worktree_path],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        logger.warning(
            "git worktree remove failed for %s: %s",
            worktree_path,
            result.stderr.strip(),
        )
    else:
        logger.info("Removed task worktree: %s", worktree_path)


def _delete_branch_if_exists(main_repo: str, branch: str) -> None:
    result = subprocess.run(
        ["git", "-C", main_repo, "branch", "-D", branch],
        capture_output=True,
        text=True,
    )
    if result.returncode == 0:
        logger.debug("Deleted stale branch: %s", branch)


def get_task_level(task_branches: list[dict]) -> dict[str, int]:
    """Return a mapping from branch_name to its dependency level (0-based).

    Uses the `depends_on` field of each task record to compute levels.
    Tasks with no dependencies are level 0.

    Args:
        task_branches: list of dicts with keys: task_id, branch_name, depends_on (JSON list).

    Returns:
        {task_id: level_number}
    """
    import json

    dep_map: dict[str, list[str]] = {}
    for t in task_branches:
        raw = t.get("depends_on", "[]") or "[]"
        try:
            deps = json.loads(raw) if isinstance(raw, str) else raw
        except (json.JSONDecodeError, TypeError):
            deps = []
        dep_map[t["task_id"]] = [d for d in deps if d]

    levels: dict[str, int] = {}

    def _level(tid: str, visited: set[str]) -> int:
        if tid in levels:
            return levels[tid]
        if tid in visited:
            return 0
        visited.add(tid)
        deps = dep_map.get(tid, [])
        if not deps:
            levels[tid] = 0
        else:
            levels[tid] = max(_level(d, visited) for d in deps if d in dep_map) + 1
        return levels[tid]

    for t in task_branches:
        _level(t["task_id"], set())

    return levels


def merge_level_branches(
    main_repo: str,
    spec_id: str,
    task_records: list[dict],
) -> dict:
    """Merge all task branches at a completed dependency level into a spec branch.

    Creates (or updates) a spec branch `boi/<spec_id>` and merges each task
    branch into it in task-ID order.

    Args:
        main_repo: Absolute path to the main git repository.
        spec_id: Spec queue ID.
        task_records: list of DB task dicts (must have task_id, branch_name,
                      depends_on, status).

    Returns:
        dict with keys:
          - merge_status: "merged" | "conflict" | "nothing_to_merge"
          - merged_tasks: list of task IDs successfully merged
          - conflicting_tasks: list of task IDs that caused conflicts
          - conflicting_files: list of file paths with conflicts
    """
    spec_branch = f"boi-spec/{spec_id}"
    done_tasks = [t for t in task_records if t.get("status") == "DONE" and t.get("branch_name")]

    if not done_tasks:
        return {
            "merge_status": "nothing_to_merge",
            "merged_tasks": [],
            "conflicting_tasks": [],
            "conflicting_files": [],
        }

    # Ensure spec branch exists; create from HEAD if not.
    _ensure_spec_branch(main_repo, spec_branch)

    merged: list[str] = []
    conflicting_tasks: list[str] = []
    conflicting_files: list[str] = []

    for task in sorted(done_tasks, key=lambda t: t["task_id"]):
        branch = task["branch_name"]
        task_id = task["task_id"]

        # Check branch exists
        check = subprocess.run(
            ["git", "-C", main_repo, "rev-parse", "--verify", branch],
            capture_output=True,
            text=True,
        )
        if check.returncode != 0:
            logger.warning("Branch %s not found, skipping merge", branch)
            continue

        conflict_result = _merge_into_branch(main_repo, spec_branch, branch, task_id, spec_id)
        if conflict_result["status"] == "merged":
            merged.append(task_id)
        else:
            conflicting_tasks.append(task_id)
            conflicting_files.extend(conflict_result.get("files", []))
            # Abort merge to leave repo clean
            subprocess.run(
                ["git", "-C", main_repo, "merge", "--abort"],
                capture_output=True,
                text=True,
            )

    if conflicting_tasks:
        return {
            "merge_status": "conflict",
            "merged_tasks": merged,
            "conflicting_tasks": conflicting_tasks,
            "conflicting_files": conflicting_files,
        }

    return {
        "merge_status": "merged" if merged else "nothing_to_merge",
        "merged_tasks": merged,
        "conflicting_tasks": [],
        "conflicting_files": [],
    }


def _ensure_spec_branch(main_repo: str, spec_branch: str) -> None:
    """Create spec_branch at HEAD if it doesn't exist."""
    check = subprocess.run(
        ["git", "-C", main_repo, "rev-parse", "--verify", spec_branch],
        capture_output=True,
        text=True,
    )
    if check.returncode != 0:
        subprocess.run(
            ["git", "-C", main_repo, "branch", spec_branch, "HEAD"],
            capture_output=True,
            text=True,
            check=True,
        )
        logger.info("Created spec branch: %s", spec_branch)


def _merge_into_branch(
    main_repo: str,
    target_branch: str,
    source_branch: str,
    task_id: str,
    spec_id: str,
) -> dict:
    """Checkout target_branch and merge source_branch into it.

    Returns {"status": "merged"|"conflict", "files": [...conflicting files...]}
    """
    # Save current HEAD to restore after.
    head_result = subprocess.run(
        ["git", "-C", main_repo, "symbolic-ref", "--short", "HEAD"],
        capture_output=True,
        text=True,
    )
    original_head = head_result.stdout.strip() if head_result.returncode == 0 else None

    # Checkout target branch.
    co = subprocess.run(
        ["git", "-C", main_repo, "checkout", target_branch],
        capture_output=True,
        text=True,
    )
    if co.returncode != 0:
        logger.warning("Could not checkout %s: %s", target_branch, co.stderr.strip())
        return {"status": "conflict", "files": []}

    # Attempt merge.
    merge = subprocess.run(
        [
            "git",
            "-C",
            main_repo,
            "merge",
            "--no-ff",
            "-m",
            f"Merge task {task_id} of {spec_id}",
            source_branch,
        ],
        capture_output=True,
        text=True,
    )

    if merge.returncode == 0:
        logger.info("Merged %s into %s", source_branch, target_branch)
        _restore_head(main_repo, original_head)
        return {"status": "merged", "files": []}

    # Conflict: find conflicting files.
    conflicting = _get_conflicting_files(main_repo)
    logger.warning(
        "Merge conflict: %s -> %s (%d files)",
        source_branch, target_branch, len(conflicting)
    )
    # Abort to clean up.
    subprocess.run(
        ["git", "-C", main_repo, "merge", "--abort"],
        capture_output=True,
        text=True,
    )
    _restore_head(main_repo, original_head)
    return {"status": "conflict", "files": conflicting}


def _restore_head(main_repo: str, original_head: Optional[str]) -> None:
    if original_head:
        subprocess.run(
            ["git", "-C", main_repo, "checkout", original_head],
            capture_output=True,
            text=True,
        )


def _get_conflicting_files(main_repo: str) -> list[str]:
    result = subprocess.run(
        ["git", "-C", main_repo, "diff", "--name-only", "--diff-filter=U"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        return []
    return [line.strip() for line in result.stdout.splitlines() if line.strip()]
