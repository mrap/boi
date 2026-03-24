# dag.py — DAG utilities for parallel task execution in BOI.
#
# Pure functions operating on lists of BoiTask objects.
# No I/O, no state management, no BOI-specific config.

from __future__ import annotations

from collections import deque

from lib.spec_parser import BoiTask

# Statuses that count as "resolved" for dependency purposes.
# A task blocked by a DONE or SKIPPED task is considered unblocked.
_RESOLVED_STATUSES = {"DONE", "SKIPPED"}


def build_adjacency_list(tasks: list[BoiTask]) -> dict[str, list[str]]:
    """Build forward adjacency list from blocked_by relationships.

    Key = dependency task ID, Value = list of task IDs it unblocks.
    Every task ID appears as a key (even if it unblocks nothing).
    """
    adj: dict[str, list[str]] = {t.id: [] for t in tasks}
    for task in tasks:
        for dep in task.blocked_by:
            if dep in adj:
                adj[dep].append(task.id)
    return adj


def topological_sort(tasks: list[BoiTask]) -> list[str]:
    """Return task IDs in topological order using Kahn's algorithm.

    Raises ValueError if a cycle is detected.
    """
    if not tasks:
        return []

    task_ids = {t.id for t in tasks}
    in_degree: dict[str, int] = {t.id: 0 for t in tasks}
    adj = build_adjacency_list(tasks)

    for task in tasks:
        for dep in task.blocked_by:
            if dep in task_ids:
                in_degree[task.id] += 1

    queue: deque[str] = deque(tid for tid, deg in in_degree.items() if deg == 0)
    result: list[str] = []

    while queue:
        node = queue.popleft()
        result.append(node)
        for neighbor in adj[node]:
            in_degree[neighbor] -= 1
            if in_degree[neighbor] == 0:
                queue.append(neighbor)

    if len(result) != len(tasks):
        raise ValueError("Cycle detected in task dependency graph")

    return result


def find_assignable_tasks(
    tasks: list[BoiTask],
    in_progress: set[str] | None = None,
) -> list[str]:
    """Return task IDs that are ready to be assigned to a worker.

    A task is assignable if:
    - status is PENDING
    - all blocked_by dependencies have a resolved status (DONE or SKIPPED)
    - not in the in_progress set (already being worked on)
    """
    in_progress = in_progress or set()
    status_by_id = {t.id: t.status for t in tasks}

    result: list[str] = []
    for task in tasks:
        if task.status != "PENDING":
            continue
        if task.id in in_progress:
            continue

        # Check all dependencies are resolved
        all_deps_resolved = True
        for dep in task.blocked_by:
            dep_status = status_by_id.get(dep)
            if dep_status not in _RESOLVED_STATUSES:
                all_deps_resolved = False
                break

        if all_deps_resolved:
            result.append(task.id)

    return result


def downstream_count(tasks: list[BoiTask], task_id: str) -> int:
    """Count how many tasks are transitively downstream of task_id.

    Uses BFS on the forward adjacency list to find all reachable nodes.
    Does not double-count in diamond patterns.
    """
    adj = build_adjacency_list(tasks)
    if task_id not in adj:
        return 0

    visited: set[str] = set()
    queue: deque[str] = deque(adj[task_id])

    while queue:
        node = queue.popleft()
        if node in visited:
            continue
        visited.add(node)
        queue.extend(adj.get(node, []))

    return len(visited)


def critical_path(tasks: list[BoiTask]) -> list[str]:
    """Find the longest dependency chain (by task count).

    Uses dynamic programming on the topologically sorted DAG.
    Returns the list of task IDs forming the critical path.
    """
    if not tasks:
        return []

    order = topological_sort(tasks)
    task_map = {t.id: t for t in tasks}

    # dist[tid] = length of longest path ending at tid
    dist: dict[str, int] = {tid: 1 for tid in order}
    # prev[tid] = predecessor on the longest path
    prev: dict[str, str | None] = {tid: None for tid in order}

    for tid in order:
        task = task_map[tid]
        for dep in task.blocked_by:
            if dep in dist and dist[dep] + 1 > dist[tid]:
                dist[tid] = dist[dep] + 1
                prev[tid] = dep

    # Find the endpoint of the longest path
    end = max(order, key=lambda t: dist[t])

    # Reconstruct path
    path: list[str] = []
    current: str | None = end
    while current is not None:
        path.append(current)
        current = prev[current]

    path.reverse()
    return path
