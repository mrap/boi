# spec_parser.py — Parse task specs from tasks.md and spec.json formats for BOI.
#
# Supports two input formats:
#   1. tasks.md — Markdown with heading-based task definitions
#   2. spec.json — JSON with a "tasks" array
#
# Both are parsed into a unified list of Task objects.
#
# Also supports BOI spec.md format with ### t-N: headings and
# PENDING/DONE/SKIPPED/FAILED/EXPERIMENT_PROPOSED/SUPERSEDED status lines
# for self-evolving specs.

import json
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


@dataclass
class Task:
    """A single task parsed from a spec file."""

    id: str
    title: str
    spec: str = ""
    files: list[str] = field(default_factory=list)
    deps: list[str] = field(default_factory=list)
    verify: list[str] = field(default_factory=list)
    commit_prefix: str = ""

    def to_dict(self) -> dict[str, Any]:
        """Convert to a plain dict for serialization."""
        return {
            "id": self.id,
            "title": self.title,
            "spec": self.spec,
            "files": list(self.files),
            "deps": list(self.deps),
            "verify": list(self.verify),
            "commit_prefix": self.commit_prefix,
        }


# Regex to match task headings like: ## t-001: Task title
_TASK_HEADING_RE = re.compile(r"^##\s+(t-\d+):\s+(.+)$")

# Regex to match field lines like: - **Spec:** value
_FIELD_RE = re.compile(r"^-\s+\*\*(\w[\w\s]*?):\*\*\s*(.*)$")

# Regex to match continuation lines (indented, not a new field or heading)
_CONTINUATION_RE = re.compile(r"^\s{2,}(.+)$")

# Regex to match BOI spec headings like: ### t-1: Task title
_BOI_TASK_HEADING_RE = re.compile(r"^###\s+(t-\d+):\s+(.+)$")

# Valid status values for BOI specs
_BOI_STATUSES = {
    "PENDING",
    "DONE",
    "SKIPPED",
    "FAILED",
    "EXPERIMENT_PROPOSED",
    "SUPERSEDED",
}

# Regex to match SUPERSEDED status with "by t-N" reference
_SUPERSEDED_RE = re.compile(r"^SUPERSEDED\s+by\s+(t-\d+)(.*)$")

# Regex to match #### subsection headings (Experiment, Discovery)
_SUBSECTION_HEADING_RE = re.compile(r"^####\s+(Experiment|Discovery):\s*(.*)$")


def _parse_comma_list(value: str) -> list[str]:
    """Split a comma-separated value into a trimmed list. Returns [] for 'none'."""
    stripped = value.strip()
    if not stripped or stripped.lower() == "none":
        return []
    return [item.strip() for item in stripped.split(",") if item.strip()]


def _parse_verify(value: str) -> list[str]:
    """Split verify commands by '&&' into individual commands."""
    stripped = value.strip()
    if not stripped or stripped.lower() == "none":
        return []
    return [cmd.strip() for cmd in stripped.split("&&") if cmd.strip()]


def parse_tasks_md(content: str) -> list[Task]:
    """Parse a tasks.md string into a list of Task objects.

    Expected format:
        ## t-001: Task title
        - **Spec:** Description text
        - **Files:** file1.php, file2.php
        - **Deps:** none (or t-001, t-002)
        - **Verify:** python3 -m pytest tests/ && lint check
        - **Commit prefix:** [proj][mod]
    """
    tasks: list[Task] = []
    current_task_id: str | None = None
    current_title: str = ""
    current_fields: dict[str, str] = {}
    last_field: str | None = None

    def _flush() -> None:
        """Convert accumulated fields into a Task and append to tasks list."""
        nonlocal current_task_id, current_title, current_fields, last_field
        if current_task_id is None:
            return

        task = Task(
            id=current_task_id,
            title=current_title,
            spec=current_fields.get("spec", "").strip(),
            files=_parse_comma_list(current_fields.get("files", "")),
            deps=_parse_comma_list(current_fields.get("deps", "")),
            verify=_parse_verify(current_fields.get("verify", "")),
            commit_prefix=current_fields.get("commit prefix", "").strip(),
        )
        tasks.append(task)

        current_task_id = None
        current_title = ""
        current_fields = {}
        last_field = None

    for line in content.splitlines():
        # Check for task heading
        heading_match = _TASK_HEADING_RE.match(line)
        if heading_match:
            _flush()
            current_task_id = heading_match.group(1)
            current_title = heading_match.group(2).strip()
            continue

        # Skip lines outside a task block
        if current_task_id is None:
            continue

        # Check for field line
        field_match = _FIELD_RE.match(line)
        if field_match:
            field_name = field_match.group(1).strip().lower()
            field_value = field_match.group(2).strip()
            current_fields[field_name] = field_value
            last_field = field_name
            continue

        # Check for continuation of previous field (indented text)
        cont_match = _CONTINUATION_RE.match(line)
        if cont_match and last_field is not None:
            current_fields[last_field] += " " + cont_match.group(1).strip()
            continue

    # Flush the last task
    _flush()

    return tasks


def parse_spec_json(data: dict[str, Any]) -> list[Task]:
    """Parse a spec.json dict into a list of Task objects."""
    if "tasks" not in data:
        raise ValueError("spec.json must contain a 'tasks' key")

    tasks: list[Task] = []
    for entry in data["tasks"]:
        if "id" not in entry:
            raise ValueError(f"Task entry missing required 'id' field: {entry}")
        if "title" not in entry:
            raise ValueError(f"Task entry missing required 'title' field: {entry}")

        task = Task(
            id=entry["id"],
            title=entry["title"],
            spec=entry.get("spec", ""),
            files=list(entry.get("files", [])),
            deps=list(entry.get("deps", [])),
            verify=list(entry.get("verify", [])),
            commit_prefix=entry.get("commit_prefix", ""),
        )
        tasks.append(task)

    return tasks


def parse_file(filepath: str) -> list[Task]:
    """Parse a task file (auto-detect format by extension).

    Supports .md (tasks.md format) and .json (spec.json format).
    Raises ValueError for unsupported extensions.
    Raises FileNotFoundError if the file doesn't exist.
    """
    path = Path(filepath)
    if not path.is_file():
        raise FileNotFoundError(f"File not found: {filepath}")

    content = path.read_text(encoding="utf-8")

    if path.suffix == ".md":
        return parse_tasks_md(content)
    elif path.suffix == ".json":
        data = json.loads(content)
        return parse_spec_json(data)
    else:
        raise ValueError(
            f"Unsupported file extension: {path.suffix} (expected .md or .json)"
        )


# ─── BOI Spec Parsing (self-evolving spec.md format) ─────────────────────────


# Regex to match **Blocked by:** lines
_BLOCKED_BY_RE = re.compile(r"^\*\*Blocked\s+by:\*\*\s*(.+)$")


@dataclass
class BoiTask:
    """A single task from a BOI self-evolving spec.md file."""

    id: str
    title: str
    status: str  # PENDING, DONE, SKIPPED, FAILED, EXPERIMENT_PROPOSED, SUPERSEDED
    body: str = ""  # Everything after the status line
    superseded_by: str = ""  # For SUPERSEDED status: the t-N that replaces this task
    experiment: str = ""  # Content of #### Experiment: subsection
    discovery: str = ""  # Content of #### Discovery: subsection
    blocked_by: list[str] = field(default_factory=list)  # Task IDs this task depends on


def parse_boi_spec(content: str) -> list[BoiTask]:
    """Parse a BOI spec.md file and extract tasks with their statuses.

    Expected format:
        ### t-1: Task title
        PENDING

        **Spec:** ...
        **Verify:** ...

    Also supports:
        EXPERIMENT_PROPOSED
        SUPERSEDED by t-9
        FAILED
        #### Experiment: description
        #### Discovery: description
    """
    tasks: list[BoiTask] = []
    current_id: str | None = None
    current_title: str = ""
    current_status: str | None = None
    current_superseded_by: str = ""
    current_blocked_by: list[str] = []
    current_body_lines: list[str] = []
    current_subsection: str | None = None  # "experiment" or "discovery"
    current_experiment_lines: list[str] = []
    current_discovery_lines: list[str] = []

    def _flush() -> None:
        nonlocal current_id, current_title, current_status, current_superseded_by
        nonlocal current_blocked_by, current_body_lines, current_subsection
        nonlocal current_experiment_lines, current_discovery_lines
        if current_id is not None and current_status is not None:
            tasks.append(
                BoiTask(
                    id=current_id,
                    title=current_title,
                    status=current_status,
                    body="\n".join(current_body_lines).strip(),
                    superseded_by=current_superseded_by,
                    experiment="\n".join(current_experiment_lines).strip(),
                    discovery="\n".join(current_discovery_lines).strip(),
                    blocked_by=list(current_blocked_by),
                )
            )
        current_id = None
        current_title = ""
        current_status = None
        current_superseded_by = ""
        current_blocked_by = []
        current_body_lines = []
        current_subsection = None
        current_experiment_lines = []
        current_discovery_lines = []

    for line in content.splitlines():
        heading_match = _BOI_TASK_HEADING_RE.match(line)
        if heading_match:
            _flush()
            current_id = heading_match.group(1)
            current_title = heading_match.group(2).strip()
            continue

        if current_id is not None and current_status is None:
            stripped = line.strip()
            # Status line must be one of the valid statuses
            # May have trailing notes after the status word
            # Special handling for SUPERSEDED which has "by t-N"
            if stripped:
                # Check for SUPERSEDED by t-N first
                superseded_match = _SUPERSEDED_RE.match(stripped)
                if superseded_match:
                    current_status = "SUPERSEDED"
                    current_superseded_by = superseded_match.group(1)
                    continue
                first_word = stripped.split()[0] if stripped.split() else ""
                if first_word in _BOI_STATUSES:
                    current_status = first_word
                    continue
            # Skip blank lines before status
            continue

        if current_id is not None and current_status is not None:
            # Check for **Blocked by:** line
            blocked_match = _BLOCKED_BY_RE.match(line.strip())
            if blocked_match:
                deps_str = blocked_match.group(1).strip()
                current_blocked_by = [
                    d.strip() for d in deps_str.split(",") if d.strip()
                ]
                current_body_lines.append(line)
                continue

            # Check for #### subsection headings
            subsection_match = _SUBSECTION_HEADING_RE.match(line)
            if subsection_match:
                current_subsection = subsection_match.group(1).lower()
                # Include the heading line in body but route content to metadata
                current_body_lines.append(line)
                continue

            # Route content to current subsection if inside one
            if current_subsection == "experiment":
                # A new #### heading or ### heading ends this subsection
                if line.startswith("#### ") and not _SUBSECTION_HEADING_RE.match(line):
                    current_subsection = None
                else:
                    current_experiment_lines.append(line)
            elif current_subsection == "discovery":
                if line.startswith("#### ") and not _SUBSECTION_HEADING_RE.match(line):
                    current_subsection = None
                else:
                    current_discovery_lines.append(line)

            current_body_lines.append(line)

    _flush()
    return tasks


def count_boi_tasks(filepath: str) -> dict[str, int]:
    """Count task statuses in a BOI spec.md file.

    Returns a dict with keys: pending, done, skipped, failed,
    experiment_proposed, superseded, total.

    Counting rules:
    - EXPERIMENT_PROPOSED counts as incomplete (like PENDING).
    - SUPERSEDED is excluded from total (task was replaced).
    - FAILED counts as incomplete.
    - total excludes SUPERSEDED tasks.
    """
    path = Path(filepath)
    if not path.is_file():
        return {
            "pending": 0,
            "done": 0,
            "skipped": 0,
            "failed": 0,
            "experiment_proposed": 0,
            "superseded": 0,
            "total": 0,
        }

    content = path.read_text(encoding="utf-8")
    tasks = parse_boi_spec(content)

    counts = {
        "pending": 0,
        "done": 0,
        "skipped": 0,
        "failed": 0,
        "experiment_proposed": 0,
        "superseded": 0,
        "total": 0,
    }
    for task in tasks:
        status_key = task.status.lower()
        if status_key in counts:
            counts[status_key] += 1

    # Total excludes SUPERSEDED tasks (they were replaced)
    counts["total"] = len(tasks) - counts["superseded"]

    return counts


@dataclass
class StatusRegression:
    """A detected DONE -> PENDING regression in a task."""

    task_id: str
    previous_status: str
    current_status: str


def check_status_regression(
    previous_tasks: list[BoiTask], current_tasks: list[BoiTask]
) -> list[StatusRegression]:
    """Detect tasks that regressed from DONE back to PENDING.

    Compares the previous task list with the current one and returns
    a list of StatusRegression objects for any task whose status went
    from DONE to a non-DONE status.

    Args:
        previous_tasks: Tasks parsed from the spec before the iteration.
        current_tasks: Tasks parsed from the spec after the iteration.

    Returns:
        List of StatusRegression objects. Empty if no regressions found.
    """
    prev_by_id = {t.id: t for t in previous_tasks}
    regressions: list[StatusRegression] = []

    for task in current_tasks:
        prev = prev_by_id.get(task.id)
        if prev is not None and prev.status == "DONE" and task.status != "DONE":
            regressions.append(
                StatusRegression(
                    task_id=task.id,
                    previous_status=prev.status,
                    current_status=task.status,
                )
            )

    return regressions


def convert_tasks_to_spec(tasks_filepath: str, output_filepath: str) -> int:
    """Convert a mesh-format tasks.md into a BOI spec.md file.

    Reads the old ## t-NNN: format and writes ### t-N: format with
    PENDING status lines.

    Returns the number of tasks converted.
    """
    tasks = parse_file(tasks_filepath)
    if not tasks:
        raise ValueError(f"No tasks found in {tasks_filepath}")

    lines = [
        f"# Spec (converted from {Path(tasks_filepath).name})",
        "",
        "## Tasks",
        "",
    ]

    for i, task in enumerate(tasks, 1):
        # Normalize ID to t-N format
        task_id = f"t-{i}"
        lines.append(f"### {task_id}: {task.title}")
        lines.append("PENDING")
        lines.append("")

        if task.spec:
            lines.append(f"**Spec:** {task.spec}")
            lines.append("")

        if task.files:
            lines.append(f"**Files:** {', '.join(task.files)}")
            lines.append("")

        if task.verify:
            lines.append(f"**Verify:** {' && '.join(task.verify)}")
            lines.append("")

        if task.deps:
            lines.append(f"**Deps:** {', '.join(task.deps)}")
            lines.append("")

    content = "\n".join(lines) + "\n"
    Path(output_filepath).write_text(content, encoding="utf-8")
    return len(tasks)


# ─── Error Log Parsing ──────────────────────────────────────────────────────

# Regex to match the ## Error Log heading
_ERROR_LOG_HEADING_RE = re.compile(r"^##\s+Error\s+Log\s*$")

# Regex to match individual error entries: ### [iter-N] description
_ERROR_ENTRY_RE = re.compile(r"^###\s+\[iter-(\d+)\]\s+(.+)$")


@dataclass
class ErrorLogEntry:
    """A single entry in the ## Error Log section."""

    iteration: int
    description: str
    body: str = ""


def parse_error_log(content: str) -> list[ErrorLogEntry]:
    """Parse the ## Error Log section from a BOI spec.

    Extracts error entries formatted as:
        ## Error Log

        ### [iter-N] Brief description
        What was tried and why it failed. What future workers should avoid.

    Returns a list of ErrorLogEntry objects. Returns empty list if no Error Log
    section exists. Error Log sections are informational and do not affect
    task counting.
    """
    entries: list[ErrorLogEntry] = []
    in_error_log = False
    current_entry: ErrorLogEntry | None = None
    current_body_lines: list[str] = []

    def _flush_entry() -> None:
        nonlocal current_entry, current_body_lines
        if current_entry is not None:
            current_entry.body = "\n".join(current_body_lines).strip()
            entries.append(current_entry)
        current_entry = None
        current_body_lines = []

    for line in content.splitlines():
        # Check for ## Error Log heading
        if _ERROR_LOG_HEADING_RE.match(line):
            in_error_log = True
            continue

        # If we hit another ## heading, stop parsing the Error Log
        if (
            in_error_log
            and line.startswith("## ")
            and not _ERROR_LOG_HEADING_RE.match(line)
        ):
            _flush_entry()
            in_error_log = False
            continue

        if not in_error_log:
            continue

        # Check for ### [iter-N] entry heading
        entry_match = _ERROR_ENTRY_RE.match(line)
        if entry_match:
            _flush_entry()
            current_entry = ErrorLogEntry(
                iteration=int(entry_match.group(1)),
                description=entry_match.group(2).strip(),
            )
            continue

        # Accumulate body lines for current entry
        if current_entry is not None:
            current_body_lines.append(line)

    _flush_entry()
    return entries


def extract_error_log_section(content: str) -> str:
    """Extract the raw ## Error Log section text from a BOI spec.

    Returns the full text of the Error Log section (including heading),
    or empty string if no Error Log section exists. Useful for injecting
    into worker prompts.
    """
    lines = content.splitlines()
    result_lines: list[str] = []
    in_error_log = False

    for line in lines:
        if _ERROR_LOG_HEADING_RE.match(line):
            in_error_log = True
            result_lines.append(line)
            continue

        if (
            in_error_log
            and line.startswith("## ")
            and not _ERROR_LOG_HEADING_RE.match(line)
        ):
            break

        if in_error_log:
            result_lines.append(line)

    return "\n".join(result_lines).strip() if result_lines else ""
