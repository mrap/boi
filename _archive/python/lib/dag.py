# dag.py — DAG utilities for parallel task execution in BOI.
#
# Pure functions operating on lists of BoiTask objects.
# No I/O, no state management, no BOI-specific config.

from __future__ import annotations

import re
from collections import deque

from lib.spec_parser import BoiTask, parse_boi_spec, parse_deps_section

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


def validate_dag(deps: dict[str, list[str]], task_ids: set[str]) -> list[str]:
    """Validate a dependency graph without needing full BoiTask objects.

    Checks:
    1. Self-references (t-1: t-1)
    2. Missing references (dependency on a task that doesn't exist)
    3. Cycles (via Kahn's algorithm)

    Returns list of error strings. Empty = valid.
    """
    errors: list[str] = []

    # Check self-references and missing refs
    for task_id, task_deps in deps.items():
        for dep in task_deps:
            if dep == task_id:
                errors.append(f"{task_id}: self-dependency is not allowed")
            elif dep not in task_ids and dep not in deps:
                errors.append(f"{task_id}: depends on {dep} which doesn't exist")

    # Cycle detection (Kahn's algorithm)
    all_ids = set(deps.keys()) | task_ids
    in_degree: dict[str, int] = {tid: 0 for tid in all_ids}
    adj: dict[str, list[str]] = {tid: [] for tid in all_ids}

    for task_id, task_deps in deps.items():
        for dep in task_deps:
            if dep in adj and dep != task_id:
                adj[dep].append(task_id)
                in_degree[task_id] += 1

    queue: deque[str] = deque(tid for tid, deg in in_degree.items() if deg == 0)
    visited = 0
    while queue:
        node = queue.popleft()
        visited += 1
        for neighbor in adj[node]:
            in_degree[neighbor] -= 1
            if in_degree[neighbor] == 0:
                queue.append(neighbor)

    if visited != len(all_ids):
        errors.append("Cycle detected in task dependency graph")

    return errors


def validate_deps_section(content: str) -> list[str]:
    """Validate the ## Dependencies section for structural issues.

    Checks for duplicate task IDs in the section.
    """
    _DEP_SECTION_HEADING = re.compile(r"^##\s+Dependencies\s*$")
    _DEP_LINE = re.compile(r"^(t-\d+):\s*(.*)$")

    errors: list[str] = []
    in_section = False
    seen: set[str] = set()

    for line in content.splitlines():
        if _DEP_SECTION_HEADING.match(line.strip()):
            in_section = True
            continue
        if in_section and line.startswith("## "):
            break
        if not in_section:
            continue

        m = _DEP_LINE.match(line.strip())
        if m:
            task_id = m.group(1)
            if task_id in seen:
                errors.append(f"Duplicate entry for {task_id} in Dependencies section")
            seen.add(task_id)

    return errors


def check_dep_conflicts(content: str) -> list[str]:
    """Warn if Dependencies section and Blocked by lines disagree."""
    section_deps = parse_deps_section(content)
    if section_deps is None:
        return []

    # Parse tasks using the raw inline parser (bypass section override)
    # by manually extracting blocked_by from task bodies
    _BLOCKED_BY_RE = re.compile(r"^\*\*Blocked\s+by:\*\*\s*(.+)$")
    _TASK_HEADING_RE = re.compile(r"^###\s+(t-\d+):\s+(.+)$")

    inline_deps: dict[str, list[str]] = {}
    current_id: str | None = None

    for line in content.splitlines():
        heading_match = _TASK_HEADING_RE.match(line)
        if heading_match:
            current_id = heading_match.group(1)
            continue
        if current_id is not None:
            blocked_match = _BLOCKED_BY_RE.match(line.strip())
            if blocked_match:
                deps_str = blocked_match.group(1).strip()
                inline_deps[current_id] = [
                    d.strip() for d in deps_str.split(",") if d.strip()
                ]

    warnings: list[str] = []
    for task_id, inline in inline_deps.items():
        section = section_deps.get(task_id, [])
        if set(inline) != set(section):
            warnings.append(
                f"{task_id}: Dependencies section says {section}, "
                f"but **Blocked by:** says {inline}. "
                "Dependencies section takes precedence."
            )
    return warnings


def deps_viz(deps: dict[str, list[str]]) -> str:
    """Generate an ASCII visualization of the dependency graph.

    Shows each edge as: dep ──> task
    Independent tasks are shown as: task (independent)
    Includes critical path summary.
    """
    lines: list[str] = []

    # Build reverse map: for each task, which tasks depend on it
    has_dependents: set[str] = set()
    for task_id, task_deps in deps.items():
        for dep in task_deps:
            has_dependents.add(dep)

    # Sort by task number
    sorted_ids = sorted(deps.keys(), key=lambda x: int(x.split("-")[1]))

    for task_id in sorted_ids:
        task_deps = deps[task_id]
        if not task_deps:
            if task_id not in has_dependents:
                lines.append(f"{task_id} (independent)")
            else:
                lines.append(f"{task_id} (root)")
        else:
            for dep in task_deps:
                lines.append(f"{dep} ──> {task_id}")

    # Critical path
    # Build BoiTask-like objects for critical_path computation
    fake_tasks = []
    for task_id in sorted_ids:
        fake_tasks.append(
            BoiTask(
                id=task_id,
                title="",
                status="PENDING",
                blocked_by=deps.get(task_id, []),
            )
        )
    if fake_tasks:
        try:
            cp = critical_path(fake_tasks)
            if len(cp) > 1:
                lines.append("")
                lines.append(f"Critical path: {' -> '.join(cp)} ({len(cp)} tasks)")
        except ValueError:
            lines.append("")
            lines.append("Critical path: <cycle detected>")

    return "\n".join(lines)
