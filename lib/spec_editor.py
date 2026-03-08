# spec_editor.py — Mutate BOI spec files: add, skip, reorder, block tasks.
#
# All writes are atomic (.tmp + os.rename). All mutations acquire
# the queue lock to prevent concurrent edits.

import os
import re
from pathlib import Path

from lib.locking import queue_lock
from lib.spec_parser import parse_boi_spec

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
