# spec_validator.py — Validate BOI spec.md files before dispatch.
#
# Checks:
#   - All tasks have ### t-N: heading format
#   - Every task has a status line (PENDING, DONE, SKIPPED) immediately after heading
#   - Every task has a **Spec:** section
#   - Every task has a **Verify:** section
#   - Warns if no **Self-evolution:** section (optional but recommended)
#   - Detects duplicate task IDs
#   - Rejects empty specs (no tasks found)
#
# Generate spec validation:
#   - Title must start with # [Generate]
#   - Requires ## Goal section (at least 20 words)
#   - Requires ## Constraints section
#   - Requires ## Success Criteria section (at least 2 checkbox items)
#   - Rejects if any ### t-N: task headings are present
#   - Optional: ## Anti-Goals, ## Seed Ideas
#
# Usage:
#   from lib.spec_validator import validate_spec, validate_spec_file
#   from lib.spec_validator import validate_generate_spec, is_generate_spec
#   result = validate_spec_file("path/to/spec.md")
#   if not result.valid:
#       for error in result.errors:
#           print(f"  ERROR: {error}")

import re
import subprocess
from dataclasses import dataclass, field
from pathlib import Path

from lib.spec_parser import parse_boi_spec

# Regex to match BOI task headings: ### t-N: Title
_BOI_TASK_HEADING_RE = re.compile(r"^###\s+(t-\d+):\s+(.+)$")

# Regex to match top-level bold field lines: **FieldName:** value
_BOLD_FIELD_RE = re.compile(r"^\*\*([^*:]+):\*\*\s*(.+)$")

# Valid status values
_VALID_STATUSES = {
    "PENDING",
    "DONE",
    "SKIPPED",
    "FAILED",
    "EXPERIMENT_PROPOSED",
    "SUPERSEDED",
}


def _extract_spec_header_fields(content: str) -> dict[str, str]:
    """Extract top-level bold field values from spec header (before first task).

    Scans lines before the first ### t-N: heading and collects **Field:** value
    pairs. Keys are lowercased and stripped. Example fields: workspace, target,
    workspace-justification.
    """
    fields: dict[str, str] = {}
    for line in content.splitlines():
        if _BOI_TASK_HEADING_RE.match(line):
            break  # Stop at first task heading
        m = _BOLD_FIELD_RE.match(line.strip())
        if m:
            key = m.group(1).strip().lower()
            value = m.group(2).strip()
            fields[key] = value
    return fields


def _is_git_repo(path: str) -> bool:
    """Return True if *path* is inside a git repository.

    Expands ~ in the path. Returns False if the path does not exist or
    git is not available.
    """
    expanded = Path(path).expanduser()
    if not expanded.exists():
        return False
    try:
        result = subprocess.run(
            ["git", "-C", str(expanded), "rev-parse", "--git-dir"],
            capture_output=True,
            timeout=5,
        )
        return result.returncode == 0
    except (OSError, subprocess.TimeoutExpired):
        return False


def check_workspace_policy(content: str) -> list[str]:
    """Check the spec's Workspace/Target fields against workspace policy.

    Returns a list of warning strings (never errors — does not block dispatch).

    Rules:
    - If Workspace is missing, treat as 'worktree' (safe default, no warning).
    - If Workspace is 'in-place' and Target points to an existing git repo,
      emit a warning — unless Workspace-Justification is provided.
    - worktree / docker always produce no warning.
    """
    fields = _extract_spec_header_fields(content)
    workspace = fields.get("workspace", "worktree").strip().lower()
    target = fields.get("target", "").strip()
    justification = fields.get("workspace-justification", "").strip()

    warnings: list[str] = []

    if workspace == "in-place" and target and not justification:
        if _is_git_repo(target):
            warnings.append(
                f"in-place workspace targeting git repo {target} "
                "— consider worktree or docker"
            )

    return warnings


def validate_dependencies(content: str) -> list[str]:
    """Validate the dependency DAG in a BOI spec.

    Checks:
    1. Unmet dependencies: **Blocked by:** t-X where t-X doesn't exist
    2. Cycle detection via Kahn's algorithm

    Returns list of error strings. Empty = valid.
    """
    tasks = parse_boi_spec(content)
    task_ids = {t.id for t in tasks}
    errors: list[str] = []

    # Build adjacency list from blocked_by fields
    edges: list[tuple[str, str]] = []  # (dependency, dependent)
    for task in tasks:
        for dep in task.blocked_by:
            if dep not in task_ids:
                errors.append(
                    f"{task.id}: blocked by {dep} which doesn't exist"
                )
            else:
                edges.append((dep, task.id))

    # Cycle detection (Kahn's algorithm)
    in_degree: dict[str, int] = {t.id: 0 for t in tasks}
    adj: dict[str, list[str]] = {t.id: [] for t in tasks}
    for src, dst in edges:
        if src in adj:  # skip edges with unmet deps
            adj[src].append(dst)
            in_degree[dst] += 1

    queue = [tid for tid in in_degree if in_degree[tid] == 0]
    visited = 0
    while queue:
        node = queue.pop(0)
        visited += 1
        for neighbor in adj[node]:
            in_degree[neighbor] -= 1
            if in_degree[neighbor] == 0:
                queue.append(neighbor)

    if visited != len(tasks):
        errors.append("Dependency cycle detected in task graph")

    return errors


def check_task_sizing(task_id: str, body: str) -> list[str]:
    """Check a single task's body for sizing heuristics.

    Returns a list of warning strings (never errors).

    Heuristics:
    - Spec text > 2000 chars: "Consider splitting"
    - Spec text < 50 chars: "May be too vague"
    - >= 3 file write references: "Consider splitting mutations"
    - Combining keywords ("and also", "additionally", "plus"): "May be combining multiple objectives"
    """
    warnings: list[str] = []
    body_len = len(body)

    if body_len > 2000:
        warnings.append(
            f"{task_id}: Task spec is very long ({body_len} chars). "
            "Consider splitting into smaller tasks."
        )
    elif body_len < 50:
        warnings.append(
            f"{task_id}: Task spec is very short ({body_len} chars). "
            "May be too vague."
        )

    # Count file write patterns (write to X.md, output to X, create X.py, etc.)
    write_patterns = re.findall(
        r"(?:write|output|create|save|generate)\s+(?:to\s+|)\S+\.(?:md|py|json|txt|yaml|yml|sh|sql|csv|html)",
        body,
        re.IGNORECASE,
    )
    if len(write_patterns) >= 3:
        warnings.append(
            f"{task_id}: Task references {len(write_patterns)} write operations. "
            "Consider splitting into one mutation per task."
        )

    # Check for combining keywords
    combining_keywords = ["and also", "additionally", "plus "]
    body_lower = body.lower()
    found_keywords = [kw for kw in combining_keywords if kw in body_lower]
    if found_keywords:
        warnings.append(
            f"{task_id}: Task may be combining multiple objectives "
            f"(found: {', '.join(repr(k) for k in found_keywords)})."
        )

    return warnings


@dataclass
class ValidationResult:
    """Result of validating a BOI spec.md file."""

    valid: bool = True
    errors: list[str] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)
    total: int = 0
    pending: int = 0
    done: int = 0
    skipped: int = 0

    def summary(self) -> str:
        """Human-readable validation summary."""
        if self.valid:
            parts = []
            if self.pending > 0:
                parts.append(f"{self.pending} PENDING")
            if self.done > 0:
                parts.append(f"{self.done} DONE")
            if self.skipped > 0:
                parts.append(f"{self.skipped} SKIPPED")
            counts = ", ".join(parts) if parts else "0"
            return f"Valid: {self.total} tasks ({counts})"
        else:
            return f"Invalid: {len(self.errors)} error(s) in {self.total} tasks"


@dataclass
class _ParsedTask:
    """Internal representation of a parsed task for validation."""

    task_id: str
    title: str
    status: str | None = None
    has_spec: bool = False
    has_verify: bool = False
    has_self_evolution: bool = False
    line_number: int = 0


def _extract_task_body(content: str, task_id: str) -> str:
    """Extract the body text of a specific task from spec content.

    Returns everything between this task's heading and the next task heading.
    """
    lines = content.splitlines()
    in_task = False
    body_lines: list[str] = []

    for line in lines:
        heading_match = _BOI_TASK_HEADING_RE.match(line)
        if heading_match:
            if in_task:
                break  # Hit next task heading
            if heading_match.group(1) == task_id:
                in_task = True
            continue
        if in_task:
            body_lines.append(line)

    return "\n".join(body_lines)


def validate_spec(content: str) -> ValidationResult:
    """Validate a BOI spec.md string.

    Returns a ValidationResult with errors, warnings, and task counts.
    """
    result = ValidationResult()
    tasks: list[_ParsedTask] = []
    seen_ids: dict[str, int] = {}  # task_id -> line_number

    lines = content.splitlines()
    current_task: _ParsedTask | None = None

    for i, line in enumerate(lines, 1):
        # Check for task heading
        heading_match = _BOI_TASK_HEADING_RE.match(line)
        if heading_match:
            # Flush previous task
            if current_task is not None:
                tasks.append(current_task)

            task_id = heading_match.group(1)
            title = heading_match.group(2).strip()
            current_task = _ParsedTask(task_id=task_id, title=title, line_number=i)

            # Check for duplicate IDs
            if task_id in seen_ids:
                result.errors.append(
                    f"Duplicate task ID '{task_id}' at line {i} "
                    f"(first seen at line {seen_ids[task_id]})"
                )
            seen_ids[task_id] = i
            continue

        if current_task is None:
            continue

        # Look for status line (first non-blank line after heading)
        if current_task.status is None:
            stripped = line.strip()
            if not stripped:
                continue  # Skip blank lines before status
            first_word = stripped.split()[0] if stripped.split() else ""
            if first_word in _VALID_STATUSES:
                current_task.status = first_word
            else:
                # Non-blank, non-status line found before status
                result.errors.append(
                    f"Task {current_task.task_id}: missing status line after heading "
                    f"(line {current_task.line_number}). Expected a valid status "
                    "(PENDING, DONE, SKIPPED, FAILED, EXPERIMENT_PROPOSED, or SUPERSEDED) "
                    "on its own line immediately after the heading."
                )
                # Set a sentinel so we don't keep looking
                current_task.status = "__MISSING__"
            continue

        # Check for required sections in body
        if line.strip().startswith("**Spec:**"):
            current_task.has_spec = True
        elif line.strip().startswith("**Verify:**"):
            current_task.has_verify = True
        elif line.strip().startswith("**Self-evolution:**"):
            current_task.has_self_evolution = True

    # Flush last task
    if current_task is not None:
        tasks.append(current_task)

    # Check for empty spec
    if len(tasks) == 0:
        result.valid = False
        result.errors.append(
            "No tasks found. A BOI spec must have at least one ### t-N: task heading."
        )
        return result

    # Validate each task
    for task in tasks:
        if task.status is None or task.status == "__MISSING__":
            if task.status is None:
                result.errors.append(
                    f"Task {task.task_id}: missing status line after heading "
                    f"(line {task.line_number}). Expected PENDING, DONE, or SKIPPED."
                )

        if not task.has_spec:
            result.errors.append(
                f"Task {task.task_id}: missing **Spec:** section (line {task.line_number})."
            )

        if not task.has_verify:
            result.errors.append(
                f"Task {task.task_id}: missing **Verify:** section (line {task.line_number})."
            )

        if not task.has_self_evolution:
            result.warnings.append(
                f"Task {task.task_id}: no **Self-evolution:** section (optional but recommended)."
            )

    # Count statuses
    result.total = len(tasks)
    for task in tasks:
        if task.status == "PENDING":
            result.pending += 1
        elif task.status == "DONE":
            result.done += 1
        elif task.status == "SKIPPED":
            result.skipped += 1

    # Validate dependency graph (produces errors for cycles and unmet deps)
    dep_errors = validate_dependencies(content)
    result.errors.extend(dep_errors)

    # Check task sizing heuristics (produces warnings, never errors)
    for task in tasks:
        if task.status and task.status not in ("__MISSING__",):
            # Build body from lines between this task heading and next
            task_body = _extract_task_body(content, task.task_id)
            result.warnings.extend(check_task_sizing(task.task_id, task_body))

    # Check workspace policy (produces warnings, never errors)
    result.warnings.extend(check_workspace_policy(content))

    # Set valid flag
    if result.errors:
        result.valid = False

    return result


def validate_spec_file(filepath: str) -> ValidationResult:
    """Validate a BOI spec.md file from disk.

    Returns a ValidationResult. If the file doesn't exist or can't be read,
    returns an invalid result with an appropriate error.
    """
    path = Path(filepath)
    if not path.is_file():
        result = ValidationResult(valid=False)
        result.errors.append(f"File not found: {filepath}")
        return result

    try:
        content = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as e:
        result = ValidationResult(valid=False)
        result.errors.append(f"Cannot read file {filepath}: {e}")
        return result

    return validate_spec(content)


# Regex patterns for Generate spec validation
_GENERATE_TITLE_RE = re.compile(r"^#\s+\[Generate\]")
_SECTION_HEADING_RE = re.compile(r"^##\s+(.+)$")
_CHECKBOX_RE = re.compile(r"^\s*-\s+\[\s*[xX ]?\s*\]")


def is_generate_spec(content: str) -> bool:
    """Check if spec content is a Generate-mode spec (title starts with # [Generate])."""
    for line in content.splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        if stripped.startswith("#"):
            return bool(_GENERATE_TITLE_RE.match(stripped))
        # First non-blank, non-heading line means no title found
        break
    return False


def validate_generate_spec(content: str) -> ValidationResult:
    """Validate a Generate-mode spec.

    Generate specs have a goal-only format:
    - Title starts with # [Generate]
    - Requires ## Goal section (at least 20 words)
    - Requires ## Constraints section
    - Requires ## Success Criteria section (at least 2 checkbox items: - [ ])
    - Rejects if any ### t-N: task headings are present
    - Optional sections: ## Anti-Goals, ## Seed Ideas
    """
    result = ValidationResult()
    lines = content.splitlines()

    # Check title
    title_found = False
    for line in lines:
        stripped = line.strip()
        if not stripped:
            continue
        if stripped.startswith("#"):
            if _GENERATE_TITLE_RE.match(stripped):
                title_found = True
            else:
                result.errors.append(
                    f"Generate spec title must start with '# [Generate]'. "
                    f"Found: '{stripped[:60]}'"
                )
            break

    if not title_found and not result.errors:
        result.errors.append(
            "Generate spec must have a title starting with '# [Generate]'."
        )

    # Parse sections
    sections: dict[str, list[str]] = {}
    current_section: str | None = None
    current_lines: list[str] = []

    for line in lines:
        section_match = _SECTION_HEADING_RE.match(line.strip())
        if section_match:
            if current_section is not None:
                sections[current_section] = current_lines
            current_section = section_match.group(1).strip()
            current_lines = []
        elif current_section is not None:
            current_lines.append(line)

    # Flush last section
    if current_section is not None:
        sections[current_section] = current_lines

    # Check ## Goal section
    if "Goal" not in sections:
        result.errors.append("Missing required '## Goal' section.")
    else:
        goal_text = " ".join(line.strip() for line in sections["Goal"] if line.strip())
        word_count = len(goal_text.split())
        if word_count < 20:
            result.errors.append(
                f"'## Goal' section is too short ({word_count} words). "
                "Minimum 20 words required to describe the goal clearly."
            )

    # Check ## Constraints section
    if "Constraints" not in sections:
        result.errors.append("Missing required '## Constraints' section.")

    # Check ## Success Criteria section
    if "Success Criteria" not in sections:
        result.errors.append("Missing required '## Success Criteria' section.")
    else:
        checkbox_count = sum(
            1 for line in sections["Success Criteria"] if _CHECKBOX_RE.match(line)
        )
        if checkbox_count < 2:
            result.errors.append(
                f"'## Success Criteria' must have at least 2 checkbox items (- [ ]). "
                f"Found {checkbox_count}."
            )

    # Reject if any ### t-N: task headings are present
    for i, line in enumerate(lines, 1):
        if _BOI_TASK_HEADING_RE.match(line.strip()):
            result.errors.append(
                f"Generate specs must not contain task headings. "
                f"Found '### t-N:' at line {i}. "
                "Tasks are generated during the decomposition phase."
            )
            break  # One error is enough

    # Set valid flag
    if result.errors:
        result.valid = False

    return result


def validate_generate_spec_file(filepath: str) -> ValidationResult:
    """Validate a Generate-mode spec file from disk."""
    path = Path(filepath)
    if not path.is_file():
        result = ValidationResult(valid=False)
        result.errors.append(f"File not found: {filepath}")
        return result

    try:
        content = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as e:
        result = ValidationResult(valid=False)
        result.errors.append(f"Cannot read file {filepath}: {e}")
        return result

    return validate_generate_spec(content)


def auto_validate(content: str) -> ValidationResult:
    """Auto-detect spec type and validate accordingly.

    If the spec title starts with # [Generate], uses Generate validation.
    Otherwise, uses standard task-based validation.
    """
    if is_generate_spec(content):
        return validate_generate_spec(content)
    return validate_spec(content)


def auto_validate_file(filepath: str) -> ValidationResult:
    """Auto-detect spec type from file and validate accordingly."""
    path = Path(filepath)
    if not path.is_file():
        result = ValidationResult(valid=False)
        result.errors.append(f"File not found: {filepath}")
        return result

    try:
        content = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as e:
        result = ValidationResult(valid=False)
        result.errors.append(f"Cannot read file {filepath}: {e}")
        return result

    return auto_validate(content)
