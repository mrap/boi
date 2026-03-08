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
from dataclasses import dataclass, field
from pathlib import Path

# Regex to match BOI task headings: ### t-N: Title
_BOI_TASK_HEADING_RE = re.compile(r"^###\s+(t-\d+):\s+(.+)$")

# Valid status values
_VALID_STATUSES = {
    "PENDING",
    "DONE",
    "SKIPPED",
    "FAILED",
    "EXPERIMENT_PROPOSED",
    "SUPERSEDED",
}


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
