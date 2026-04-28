# spec_editor.py — Mutate BOI spec files: add, skip, reorder, block tasks.
#
# All writes are atomic (.tmp + os.rename). All mutations acquire
# the queue lock to prevent concurrent edits.

import os
import re
from pathlib import Path

from lib.dag import validate_dag
from lib.locking import queue_lock
from lib.spec_parser import parse_boi_spec, parse_deps_section

# Regex to extract t-N IDs
_TASK_ID_RE = re.compile(r"^###\s+(t-(\d+)):\s+", re.MULTILINE)


def _atomic_write(path: str, content: str) -> None:
    """Write content to path atomically via .tmp + os.rename."""
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        f.write(content)
    os.rename(tmp, path)


def _queue_dir_for(spec_path: str) -> str:
    """Return the directory containing the spec file (used as lock scope)."""
    return str(Path(spec_path).parent)


def add_task(
    spec_path: str,
    title: str,
    spec_text: str = "",
    verify_text: str = "",
) -> str:
    """Append a new PENDING task to a BOI spec file.

    Returns the new task ID (e.g. "t-15").
    """
    if not title.strip():
        raise ValueError("Task title cannot be empty")

    with queue_lock(_queue_dir_for(spec_path)):
        content = Path(spec_path).read_text(encoding="utf-8")

        # Find highest existing t-N ID
        max_n = 0
        for match in _TASK_ID_RE.finditer(content):
            n = int(match.group(2))
            if n > max_n:
                max_n = n

        new_id = f"t-{max_n + 1}"

        # Build the new task section
        section_lines = [
            f"### {new_id}: {title.strip()}",
            "PENDING",
            "",
        ]
        if spec_text.strip():
            section_lines.append(f"**Spec:** {spec_text.strip()}")
            section_lines.append("")
        if verify_text.strip():
            section_lines.append(f"**Verify:** {verify_text.strip()}")
            section_lines.append("")

        new_section = "\n".join(section_lines)

        # Ensure trailing newline before appending
        if not content.endswith("\n"):
            content += "\n"

        content += "\n" + new_section

        _atomic_write(spec_path, content)
        return new_id


def skip_task(spec_path: str, task_id: str, reason: str = "") -> None:
    """Mark a PENDING task as SKIPPED in a BOI spec file.

    Raises ValueError if the task is not PENDING or does not exist.
    """
    heading_pattern = re.compile(
        r"^(###\s+" + re.escape(task_id) + r":\s+.+)$", re.MULTILINE
    )

    with queue_lock(_queue_dir_for(spec_path)):
        content = Path(spec_path).read_text(encoding="utf-8")

        heading_match = heading_pattern.search(content)
        if heading_match is None:
            raise ValueError(f"Task {task_id} not found in spec")

        # Find the status line: first non-blank line after the heading
        lines = content.splitlines()
        heading_line_idx = None
        for i, line in enumerate(lines):
            if line.strip() == heading_match.group(1).strip():
                heading_line_idx = i
                break

        if heading_line_idx is None:
            raise ValueError(f"Task {task_id} heading not found in lines")

        # Scan for the status line
        status_line_idx = None
        for i in range(heading_line_idx + 1, len(lines)):
            stripped = lines[i].strip()
            if stripped:
                status_line_idx = i
                break

        if status_line_idx is None:
            raise ValueError(f"No status line found for task {task_id}")

        status_line = lines[status_line_idx].strip()
        first_word = status_line.split()[0] if status_line.split() else ""

        if first_word == "DONE":
            raise ValueError(f"Cannot skip task {task_id}: already DONE")
        if first_word == "SKIPPED":
            raise ValueError(f"Cannot skip task {task_id}: already SKIPPED")
        if first_word != "PENDING":
            raise ValueError(f"Unexpected status for task {task_id}: {first_word}")

        # Replace the status line
        new_status = "SKIPPED"
        if reason.strip():
            new_status += f" — {reason.strip()}"
        lines[status_line_idx] = new_status

        _atomic_write(spec_path, "\n".join(lines) + "\n")


# Regex to match any ### t-N: heading line
_TASK_SECTION_RE = re.compile(r"^###\s+t-\d+:\s+", re.MULTILINE)


def _extract_task_section(content: str, task_id: str) -> tuple[int, int, str]:
    """Find the start/end offsets of a task section in the raw spec text.

    Returns (start, end, section_text). The section spans from the
    ### t-N: heading to the next ### t- heading or EOF.
    Raises ValueError if the task is not found.
    """
    pattern = re.compile(r"^(###\s+" + re.escape(task_id) + r":\s+.+)$", re.MULTILINE)
    match = pattern.search(content)
    if match is None:
        raise ValueError(f"Task {task_id} not found in spec")

    start = match.start()

    # Find the next task heading after this one
    rest = content[match.end() :]
    next_match = _TASK_SECTION_RE.search(rest)
    if next_match is not None:
        end = match.end() + next_match.start()
    else:
        end = len(content)

    return start, end, content[start:end]


def reorder_task(spec_path: str, task_id: str) -> None:
    """Move a PENDING task to be the next task after all DONE tasks.

    This physically reorders the task in the spec file so workers
    automatically pick it next (workers pick the first PENDING task
    by document order).

    Raises ValueError if:
    - The task does not exist
    - The task is DONE or SKIPPED
    - The task is already the next PENDING task (no-op, no error)
    """
    with queue_lock(_queue_dir_for(spec_path)):
        content = Path(spec_path).read_text(encoding="utf-8")
        tasks = parse_boi_spec(content)

        # Validate the task exists and is PENDING
        target = None
        for t in tasks:
            if t.id == task_id:
                target = t
                break
        if target is None:
            raise ValueError(f"Task {task_id} not found in spec")
        if target.status == "DONE":
            raise ValueError(f"Cannot reorder task {task_id}: already DONE")
        if target.status == "SKIPPED":
            raise ValueError(f"Cannot reorder task {task_id}: already SKIPPED")

        # Find if target is already the first PENDING task
        first_pending = None
        for t in tasks:
            if t.status == "PENDING":
                first_pending = t
                break
        if first_pending is not None and first_pending.id == task_id:
            return  # Already the next PENDING task, no-op

        # Extract the target task section from raw text
        start, end, section = _extract_task_section(content, task_id)

        # Remove the target section from its current position
        before = content[:start]
        after = content[end:]
        content_without = before + after

        # Find insertion point: right after the last DONE task section
        # Re-parse after removal to get updated positions
        done_tasks = [t for t in tasks if t.status == "DONE"]
        if done_tasks:
            last_done_id = done_tasks[-1].id
            _, last_done_end, _ = _extract_task_section(content_without, last_done_id)
            insert_pos = last_done_end
        else:
            # No DONE tasks: insert at the first task heading position
            first_heading = _TASK_SECTION_RE.search(content_without)
            if first_heading is not None:
                insert_pos = first_heading.start()
            else:
                # No tasks at all (shouldn't happen since we found our task)
                insert_pos = len(content_without)

        # Ensure section ends with a newline for clean formatting
        if not section.endswith("\n"):
            section += "\n"

        new_content = (
            content_without[:insert_pos] + section + content_without[insert_pos:]
        )

        _atomic_write(spec_path, new_content)


def block_task(spec_path: str, task_id: str, blocked_by: str) -> None:
    """Mark a PENDING task as blocked by another task.

    Inserts or appends to a **Blocked by:** line after the status line.

    Raises ValueError if:
    - Either task_id or blocked_by does not exist
    - task_id is not PENDING
    - task_id == blocked_by (self-blocking)
    """
    if task_id == blocked_by:
        raise ValueError(f"Task {task_id} cannot block itself")

    with queue_lock(_queue_dir_for(spec_path)):
        content = Path(spec_path).read_text(encoding="utf-8")
        tasks = parse_boi_spec(content)

        # Validate both tasks exist
        task_ids = {t.id for t in tasks}
        if task_id not in task_ids:
            raise ValueError(f"Task {task_id} not found in spec")
        if blocked_by not in task_ids:
            raise ValueError(f"Task {blocked_by} not found in spec")

        # Validate target is PENDING
        target = next(t for t in tasks if t.id == task_id)
        if target.status == "DONE":
            raise ValueError(f"Cannot block task {task_id}: already DONE")
        if target.status == "SKIPPED":
            raise ValueError(f"Cannot block task {task_id}: already SKIPPED")
        if target.status != "PENDING":
            raise ValueError(f"Cannot block task {task_id}: status is {target.status}")

        lines = content.splitlines()

        # Find the heading line for task_id
        heading_pattern = re.compile(
            r"^###\s+" + re.escape(task_id) + r":\s+", re.MULTILINE
        )
        heading_line_idx = None
        for i, line in enumerate(lines):
            if heading_pattern.match(line):
                heading_line_idx = i
                break

        if heading_line_idx is None:
            raise ValueError(f"Task {task_id} heading not found in lines")

        # Find the status line (first non-blank line after heading)
        status_line_idx = None
        for i in range(heading_line_idx + 1, len(lines)):
            if lines[i].strip():
                status_line_idx = i
                break

        if status_line_idx is None:
            raise ValueError(f"No status line found for task {task_id}")

        # Check if a **Blocked by:** line already exists right after status
        blocked_line_idx = None
        for i in range(status_line_idx + 1, len(lines)):
            stripped = lines[i].strip()
            if stripped == "":
                continue
            if stripped.startswith("**Blocked by:**"):
                blocked_line_idx = i
            break

        if blocked_line_idx is not None:
            # Append to existing blocked-by line
            existing = lines[blocked_line_idx].rstrip()
            # Extract current deps, add new one if not already present
            after_prefix = existing.split("**Blocked by:**")[1].strip()
            current_deps = [d.strip() for d in after_prefix.split(",") if d.strip()]
            if blocked_by not in current_deps:
                current_deps.append(blocked_by)
            lines[blocked_line_idx] = f"**Blocked by:** {', '.join(current_deps)}"
        else:
            # Insert new blocked-by line after status line
            # Find the right place: after status line, before the blank line or **Spec:**
            insert_idx = status_line_idx + 1
            # Skip blank lines between status and spec
            while insert_idx < len(lines) and lines[insert_idx].strip() == "":
                insert_idx += 1
            # Insert before the **Spec:** line (or whatever comes next)
            lines.insert(insert_idx, "")
            lines.insert(insert_idx, f"**Blocked by:** {blocked_by}")

        _atomic_write(spec_path, "\n".join(lines) + "\n")


# ─── Dependencies Section Editing ────────────────────────────────────────────

# Regex for dependency lines in the ## Dependencies section
_DEP_LINE_RE = re.compile(r"^(t-\d+):\s*(.*)$")
_DEP_SECTION_HEADING_RE = re.compile(r"^##\s+Dependencies\s*$")


def _rewrite_dep_line(line: str, task_id: str, new_deps: list[str]) -> str:
    """Rewrite a single dependency line with new deps."""
    if new_deps:
        return f"{task_id}: {', '.join(new_deps)}"
    return f"{task_id}: (none)"


def _edit_deps_section(content: str, edits: dict[str, list[str]]) -> str:
    """Apply edits to the ## Dependencies section.

    edits maps task_id -> new dependency list.
    Only lines matching edited task_ids are changed.
    """
    lines = content.splitlines()
    in_section = False
    result: list[str] = []

    for line in lines:
        if _DEP_SECTION_HEADING_RE.match(line.strip()):
            in_section = True
            result.append(line)
            continue

        if in_section and line.startswith("## "):
            in_section = False
            result.append(line)
            continue

        if in_section:
            m = _DEP_LINE_RE.match(line.strip())
            if m and m.group(1) in edits:
                task_id = m.group(1)
                result.append(_rewrite_dep_line(line, task_id, edits[task_id]))
                continue

        result.append(line)

    return "\n".join(result)


def add_dep(spec_path: str, task_id: str, dep_id: str) -> None:
    """Add a dependency edge: task_id depends on dep_id.

    Raises ValueError if adding would create a cycle.
    """
    with queue_lock(_queue_dir_for(spec_path)):
        content = Path(spec_path).read_text(encoding="utf-8")
        deps = parse_deps_section(content)
        if deps is None:
            raise ValueError("No ## Dependencies section found in spec")

        # Get current deps for task
        current = list(deps.get(task_id, []))
        if dep_id in current:
            return  # Already present, no-op

        # Check cycle: temporarily add the edge and validate
        new_deps = dict(deps)
        new_deps[task_id] = current + [dep_id]
        task_ids = set(new_deps.keys())
        errors = validate_dag(new_deps, task_ids)
        cycle_errors = [e for e in errors if "cycle" in e.lower()]
        if cycle_errors:
            raise ValueError("Cannot add dependency: would create a cycle")

        content = _edit_deps_section(content, {task_id: current + [dep_id]})
        _atomic_write(spec_path, content)


def remove_dep(spec_path: str, task_id: str, dep_id: str) -> None:
    """Remove a dependency edge: task_id no longer depends on dep_id."""
    with queue_lock(_queue_dir_for(spec_path)):
        content = Path(spec_path).read_text(encoding="utf-8")
        deps = parse_deps_section(content)
        if deps is None:
            raise ValueError("No ## Dependencies section found in spec")

        current = list(deps.get(task_id, []))
        if dep_id not in current:
            return  # Not present, no-op

        current.remove(dep_id)
        content = _edit_deps_section(content, {task_id: current})
        _atomic_write(spec_path, content)


def set_deps(spec_path: str, task_id: str, dep_ids: list[str]) -> None:
    """Replace all dependencies for task_id."""
    with queue_lock(_queue_dir_for(spec_path)):
        content = Path(spec_path).read_text(encoding="utf-8")
        deps = parse_deps_section(content)
        if deps is None:
            raise ValueError("No ## Dependencies section found in spec")

        content = _edit_deps_section(content, {task_id: dep_ids})
        _atomic_write(spec_path, content)


def clear_deps(spec_path: str, task_id: str) -> None:
    """Remove all dependencies for task_id, making it independent."""
    set_deps(spec_path, task_id, [])


def swap_deps(spec_path: str, task_a: str, task_b: str) -> None:
    """Swap the dependency positions of two tasks.

    All references to task_a become task_b and vice versa.
    """
    with queue_lock(_queue_dir_for(spec_path)):
        content = Path(spec_path).read_text(encoding="utf-8")
        deps = parse_deps_section(content)
        if deps is None:
            raise ValueError("No ## Dependencies section found in spec")

        # Build new deps with swapped references
        new_deps: dict[str, list[str]] = {}
        for tid, task_deps in deps.items():
            # Swap the task ID itself
            new_tid = task_b if tid == task_a else (task_a if tid == task_b else tid)
            # Swap references in dependency lists
            new_task_deps = []
            for d in task_deps:
                if d == task_a:
                    new_task_deps.append(task_b)
                elif d == task_b:
                    new_task_deps.append(task_a)
                else:
                    new_task_deps.append(d)
            new_deps[new_tid] = new_task_deps

        # Validate no cycles
        task_ids = set(new_deps.keys())
        errors = validate_dag(new_deps, task_ids)
        cycle_errors = [e for e in errors if "cycle" in e.lower()]
        if cycle_errors:
            raise ValueError("Cannot swap: would create a cycle")

        # Rewrite the entire deps section
        lines = content.splitlines()
        result: list[str] = []
        in_section = False
        section_written = False

        for line in lines:
            if _DEP_SECTION_HEADING_RE.match(line.strip()):
                in_section = True
                result.append(line)
                continue

            if in_section and line.startswith("## "):
                # Write all new deps before leaving section
                if not section_written:
                    sorted_ids = sorted(
                        new_deps.keys(), key=lambda x: int(x.split("-")[1])
                    )
                    for tid in sorted_ids:
                        d = new_deps[tid]
                        if d:
                            result.append(f"{tid}: {', '.join(d)}")
                        else:
                            result.append(f"{tid}: (none)")
                    section_written = True
                in_section = False
                result.append(line)
                continue

            if in_section:
                # Skip old dep lines, we'll rewrite them
                m = _DEP_LINE_RE.match(line.strip())
                if m:
                    continue
                if not line.strip():
                    continue  # Skip blank lines in section
                result.append(line)
                continue

            result.append(line)

        # If section was at end of file
        if in_section and not section_written:
            sorted_ids = sorted(new_deps.keys(), key=lambda x: int(x.split("-")[1]))
            for tid in sorted_ids:
                d = new_deps[tid]
                if d:
                    result.append(f"{tid}: {', '.join(d)}")
                else:
                    result.append(f"{tid}: (none)")

        _atomic_write(spec_path, "\n".join(result) + "\n")


def migrate_deps(spec_path: str) -> None:
    """Migrate from **Blocked by:** inline format to ## Dependencies section.

    If a ## Dependencies section already exists, this is a no-op.
    """
    with queue_lock(_queue_dir_for(spec_path)):
        content = Path(spec_path).read_text(encoding="utf-8")

        # Check if section already exists
        if parse_deps_section(content) is not None:
            return  # Already has section, no-op

        tasks = parse_boi_spec(content)

        # Build dependency map
        deps_map: dict[str, list[str]] = {}
        for task in tasks:
            deps_map[task.id] = list(task.blocked_by) if task.blocked_by else []

        # Generate section
        dep_lines = ["## Dependencies"]
        sorted_ids = sorted(deps_map.keys(), key=lambda x: int(x.split("-")[1]))
        for task_id in sorted_ids:
            task_deps = deps_map[task_id]
            if task_deps:
                dep_lines.append(f"{task_id}: {', '.join(task_deps)}")
            else:
                dep_lines.append(f"{task_id}: (none)")

        dep_section = "\n".join(dep_lines) + "\n"

        # Insert before ## Tasks heading
        tasks_heading_re = re.compile(r"^## Tasks\s*$", re.MULTILINE)
        m = tasks_heading_re.search(content)
        if m:
            content = content[: m.start()] + dep_section + "\n" + content[m.start() :]
        else:
            # No ## Tasks heading; insert before first ### t-N:
            first_task_re = re.compile(r"^### t-\d+:", re.MULTILINE)
            m = first_task_re.search(content)
            if m:
                content = (
                    content[: m.start()] + dep_section + "\n" + content[m.start() :]
                )
            else:
                content += "\n" + dep_section

        _atomic_write(spec_path, content)
