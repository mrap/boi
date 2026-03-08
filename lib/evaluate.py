# evaluate.py — Evaluate phase for Generate-mode specs.
#
# After all tasks are DONE and the critic approves, Generate specs
# enter the EVALUATE phase. The evaluator checks each Success Criterion
# against the implementation, generates tasks for unmet criteria, and
# determines convergence.
#
# Convergence conditions:
#   - All criteria met + critic approved (goal_achieved)
#   - Max iterations reached (max_iterations)
#   - No progress for 5 consecutive iterations (stalled)
#   - Diminishing returns: last 3 iters < 1 criterion improvement each,
#     and > 80% criteria met (good_enough)
#
# Phase transitions: decompose -> execute -> evaluate -> completed

import json
import os
import re
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional


DEFAULT_GENERATE_MAX_ITERATIONS = 50
STALL_THRESHOLD = 5  # Consecutive iterations with no progress
DIMINISHING_RETURNS_WINDOW = 3  # Iterations to check for diminishing returns
DIMINISHING_RETURNS_MIN_MET_RATIO = 0.80  # 80% criteria met for "good enough"


@dataclass
class EvaluationResult:
    """Result of evaluating a Generate spec's Success Criteria."""

    criteria_total: int = 0
    criteria_met: int = 0
    criteria_unmet: int = 0
    all_met: bool = False
    status: str = "needs_work"  # goal_achieved | needs_work
    unmet_criteria: list[str] = field(default_factory=list)


@dataclass
class ConvergenceResult:
    """Result of the convergence check."""

    should_stop: bool = False
    reason: str = ""  # goal_achieved | good_enough | stalled | max_iterations
    criteria_met: int = 0
    criteria_total: int = 0
    iterations_used: int = 0
    max_iterations: int = DEFAULT_GENERATE_MAX_ITERATIONS


@dataclass
class CompletionSummary:
    """Final completion summary for a Generate spec."""

    status: str  # goal_achieved | good_enough | stalled | max_iterations
    iterations_used: int
    max_iterations: int
    time_elapsed_seconds: float
    criteria_met: int
    criteria_total: int
    unmet_criteria: list[str] = field(default_factory=list)
    files_built: list[str] = field(default_factory=list)
    follow_ups: list[str] = field(default_factory=list)


def parse_success_criteria(content: str) -> list[dict[str, Any]]:
    """Parse Success Criteria checkboxes from spec content.

    Returns a list of dicts with:
      - text: criterion text
      - checked: bool (whether the checkbox is checked)
      - line_number: 0-based line number in the content
    """
    criteria: list[dict[str, Any]] = []
    in_criteria_section = False
    lines = content.split("\n")

    for i, line in enumerate(lines):
        stripped = line.strip()
        # Detect ## Success Criteria heading
        if re.match(r"^##\s+Success\s+Criteria\s*$", stripped, re.IGNORECASE):
            in_criteria_section = True
            continue
        # Exit section on next ## heading
        if (
            in_criteria_section
            and re.match(r"^##\s+", stripped)
            and not re.match(r"^##\s+Success\s+Criteria", stripped, re.IGNORECASE)
        ):
            break
        if not in_criteria_section:
            continue

        # Match checkbox lines: - [ ] or - [x] or - [X]
        checkbox_match = re.match(r"^-\s+\[([ xX])\]\s+(.+)$", stripped)
        if checkbox_match:
            checked = checkbox_match.group(1).lower() == "x"
            text = checkbox_match.group(2).strip()
            criteria.append(
                {
                    "text": text,
                    "checked": checked,
                    "line_number": i,
                }
            )

    return criteria


def count_criteria_met(content: str) -> tuple[int, int]:
    """Count how many Success Criteria are met (checked) vs total.

    Returns (met, total).
    """
    criteria = parse_success_criteria(content)
    met = sum(1 for c in criteria if c["checked"])
    return met, len(criteria)


def evaluate_criteria(spec_path: str) -> EvaluationResult:
    """Evaluate the Success Criteria in a Generate spec.

    Reads the spec file and checks which criteria are met.
    Does NOT modify the spec file.
    """
    if not Path(spec_path).is_file():
        return EvaluationResult()

    content = Path(spec_path).read_text(encoding="utf-8")
    criteria = parse_success_criteria(content)

    if not criteria:
        return EvaluationResult(status="goal_achieved", all_met=True)

    met = sum(1 for c in criteria if c["checked"])
    unmet = [c["text"] for c in criteria if not c["checked"]]

    all_met = met == len(criteria)

    return EvaluationResult(
        criteria_total=len(criteria),
        criteria_met=met,
        criteria_unmet=len(criteria) - met,
        all_met=all_met,
        status="goal_achieved" if all_met else "needs_work",
        unmet_criteria=unmet,
    )


def check_convergence(
    queue_entry: dict[str, Any],
    spec_path: str,
    criteria_history: list[int] | None = None,
) -> ConvergenceResult:
    """Check if a Generate spec has converged and should stop iterating.

    Args:
        queue_entry: The queue entry dict.
        spec_path: Path to the spec file.
        criteria_history: List of criteria_met counts per iteration.
            If None, only checks max_iterations and all-criteria-met.

    Returns:
        ConvergenceResult with should_stop and reason.
    """
    iteration = queue_entry.get("iteration", 0)
    max_iter = queue_entry.get("max_iterations", DEFAULT_GENERATE_MAX_ITERATIONS)

    # Read current criteria state
    eval_result = evaluate_criteria(spec_path)

    result = ConvergenceResult(
        criteria_met=eval_result.criteria_met,
        criteria_total=eval_result.criteria_total,
        iterations_used=iteration,
        max_iterations=max_iter,
    )

    # Check 1: All criteria met (ideal)
    if eval_result.all_met:
        result.should_stop = True
        result.reason = "goal_achieved"
        return result

    # Check 2: Max iterations reached
    if iteration >= max_iter:
        result.should_stop = True
        result.reason = "max_iterations"
        return result

    # Check 3: Stalled — no progress for N consecutive iterations
    if criteria_history and len(criteria_history) >= STALL_THRESHOLD:
        recent = criteria_history[-STALL_THRESHOLD:]
        if len(set(recent)) == 1:
            # All the same value for the last N iterations
            result.should_stop = True
            result.reason = "stalled"
            return result

    # Check 4: Diminishing returns
    if criteria_history and len(criteria_history) >= DIMINISHING_RETURNS_WINDOW:
        recent = criteria_history[-DIMINISHING_RETURNS_WINDOW:]
        # Check if improvement < 1 per iteration
        improvements = [recent[i + 1] - recent[i] for i in range(len(recent) - 1)]
        all_low_improvement = all(imp < 1 for imp in improvements)

        # Check if > 80% criteria met
        if eval_result.criteria_total > 0:
            met_ratio = eval_result.criteria_met / eval_result.criteria_total
        else:
            met_ratio = 1.0

        if all_low_improvement and met_ratio >= DIMINISHING_RETURNS_MIN_MET_RATIO:
            result.should_stop = True
            result.reason = "good_enough"
            return result

    return result


def build_completion_summary(
    status: str,
    queue_entry: dict[str, Any],
    spec_path: str,
    start_time: str | None = None,
) -> CompletionSummary:
    """Build a CompletionSummary for a completed Generate spec.

    Args:
        status: Convergence reason (goal_achieved, good_enough, stalled, max_iterations).
        queue_entry: The queue entry dict.
        spec_path: Path to the spec file.
        start_time: ISO timestamp of when the spec was submitted.
    """
    eval_result = evaluate_criteria(spec_path)

    # Calculate elapsed time
    elapsed = 0.0
    if start_time:
        try:
            start_dt = datetime.fromisoformat(start_time)
            if start_dt.tzinfo is None:
                start_dt = start_dt.replace(tzinfo=timezone.utc)
            now = datetime.now(timezone.utc)
            elapsed = (now - start_dt).total_seconds()
        except (ValueError, TypeError):
            pass

    # List files built (from spec file references)
    files_built: list[str] = []
    if Path(spec_path).is_file():
        content = Path(spec_path).read_text(encoding="utf-8")
        # Extract file paths mentioned in Spec: sections
        file_refs = re.findall(r"`~/[^`]+`", content)
        files_built = sorted(set(ref.strip("`") for ref in file_refs))

    # Generate follow-ups for unmet criteria
    follow_ups: list[str] = []
    if eval_result.unmet_criteria:
        for criterion in eval_result.unmet_criteria:
            follow_ups.append(f"Address unmet criterion: {criterion}")

    return CompletionSummary(
        status=status,
        iterations_used=queue_entry.get("iteration", 0),
        max_iterations=queue_entry.get(
            "max_iterations", DEFAULT_GENERATE_MAX_ITERATIONS
        ),
        time_elapsed_seconds=elapsed,
        criteria_met=eval_result.criteria_met,
        criteria_total=eval_result.criteria_total,
        unmet_criteria=eval_result.unmet_criteria,
        files_built=files_built,
        follow_ups=follow_ups,
    )


def write_completion_summary_to_spec(
    spec_path: str, summary: CompletionSummary
) -> None:
    """Append a ## Completion Summary section to the spec file."""
    if not Path(spec_path).is_file():
        return

    content = Path(spec_path).read_text(encoding="utf-8")

    # Build summary markdown
    lines = [
        "",
        "## Completion Summary",
        "",
        f"**Status:** {summary.status}",
        f"**Iterations:** {summary.iterations_used} / {summary.max_iterations}",
        f"**Time elapsed:** {summary.time_elapsed_seconds:.0f} seconds",
        f"**Criteria met:** {summary.criteria_met} / {summary.criteria_total}",
        "",
    ]

    if summary.unmet_criteria:
        lines.append("### Unmet Criteria")
        for criterion in summary.unmet_criteria:
            lines.append(f"- {criterion}")
        lines.append("")

    if summary.files_built:
        lines.append("### What Was Built")
        for f in summary.files_built:
            lines.append(f"- `{f}`")
        lines.append("")

    if summary.follow_ups:
        lines.append("### Recommended Follow-Ups")
        for fu in summary.follow_ups:
            lines.append(f"- {fu}")
        lines.append("")

    summary_text = "\n".join(lines)

    # Remove existing Completion Summary if present
    content = re.sub(r"\n## Completion Summary\n.*$", "", content, flags=re.DOTALL)

    new_content = content.rstrip() + "\n" + summary_text

    tmp = spec_path + ".tmp"
    Path(tmp).write_text(new_content, encoding="utf-8")
    os.replace(tmp, spec_path)


def get_criteria_history(queue_dir: str, queue_id: str) -> list[int]:
    """Load criteria_met counts from per-iteration telemetry files.

    Returns a list of criteria_met counts, one per iteration, in order.
    """
    history: list[int] = []
    queue_path = Path(queue_dir)

    iteration = 1
    while True:
        iter_file = queue_path / f"{queue_id}.iteration-{iteration}.json"
        if not iter_file.is_file():
            break
        try:
            data = json.loads(iter_file.read_text(encoding="utf-8"))
            criteria_met = data.get("criteria_met")
            if criteria_met is not None:
                history.append(criteria_met)
            else:
                # If not tracked yet, try to compute from post_counts
                # This is a best-effort fallback
                post_done = data.get("post_counts", {}).get("done", 0)
                history.append(post_done)
        except (json.JSONDecodeError, OSError):
            pass
        iteration += 1

    return history


def is_generate_spec(queue_entry: dict[str, Any]) -> bool:
    """Check if a queue entry is for a Generate-mode spec."""
    mode = queue_entry.get("mode", "")
    if mode == "generate":
        return True

    spec_path = queue_entry.get("spec_path", "")
    if spec_path and Path(spec_path).is_file():
        content = Path(spec_path).read_text(encoding="utf-8")
        # Check for [Generate] in title
        if re.search(r"^\#\s+\[Generate\]", content, re.MULTILINE):
            return True
        # Check for **Mode:** generate in header
        mode_match = re.search(
            r"^\*\*Mode:\*\*\s*generate", content, re.MULTILINE | re.IGNORECASE
        )
        if mode_match:
            return True

    return False
