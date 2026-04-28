# review.py — Experiment review operations for BOI.
#
# Handles the `boi review <queue-id>` workflow:
#   - Lists EXPERIMENT_PROPOSED tasks with summaries
#   - Supports adopt, reject, defer actions
#   - Updates spec file and queue entry accordingly
#
# Interactive prompts are handled in boi.sh; this module provides
# the data retrieval and mutation functions.

import json
import os
import re
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

from lib.event_log import write_event
from lib.locking import queue_lock
from lib.queue import _read_entry, _write_entry, get_entry, requeue
from lib.spec_parser import BoiTask, count_boi_tasks, parse_boi_spec


def get_experiments_for_review(queue_dir: str, queue_id: str) -> dict[str, Any]:
    """Get experiment details for a queue entry in needs_review status.

    Returns a dict with:
      valid: bool
      error: str (if not valid)
      queue_id: str
      spec_path: str
      experiments: list of dicts with task_id, title, experiment_content
    """
    entry = get_entry(queue_dir, queue_id)
    if entry is None:
        return {"valid": False, "error": f"Queue entry not found: {queue_id}"}

    if entry.get("status") != "needs_review":
        return {
            "valid": False,
            "error": (
                f"Spec '{queue_id}' is not in needs_review status "
                f"(current: {entry.get('status', 'unknown')}). "
                "Only specs with EXPERIMENT_PROPOSED tasks can be reviewed."
            ),
        }

    spec_path = entry.get("spec_path", "")
    if not spec_path or not Path(spec_path).is_file():
        return {"valid": False, "error": f"Spec file not found: {spec_path}"}

    content = Path(spec_path).read_text(encoding="utf-8")
    tasks = parse_boi_spec(content)

    experiments = []
    for task in tasks:
        if task.status != "EXPERIMENT_PROPOSED":
            continue
        experiments.append(
            {
                "task_id": task.id,
                "title": task.title,
                "experiment_content": task.experiment,
                "body": task.body,
            }
        )

    if not experiments:
        return {
            "valid": False,
            "error": (
                "No EXPERIMENT_PROPOSED tasks found in spec, "
                "but queue entry is in needs_review status. "
                "This may indicate the spec was modified manually."
            ),
        }

    return {
        "valid": True,
        "queue_id": queue_id,
        "spec_path": spec_path,
        "experiments": experiments,
    }


def adopt_experiment(
    queue_dir: str,
    queue_id: str,
    task_id: str,
    events_dir: str,
) -> dict[str, Any]:
    """Adopt an experiment: mark the task DONE and tag the experiment [ADOPTED].

    Note: The spec says to merge experiment branches via `git merge`, but since
    experiments may not always create branches (budget may be 0 or worker may
    not use source control), we handle the spec-file-level changes here.
    Branch merging (if applicable) should be done by the caller in bash.

    Returns:
      success: bool
      error: str (if not success)
    """
    entry = get_entry(queue_dir, queue_id)
    if entry is None:
        return {"success": False, "error": f"Queue entry not found: {queue_id}"}

    spec_path = entry.get("spec_path", "")
    if not spec_path or not Path(spec_path).is_file():
        return {"success": False, "error": f"Spec file not found: {spec_path}"}

    content = Path(spec_path).read_text(encoding="utf-8")

    # Replace EXPERIMENT_PROPOSED with DONE for this specific task
    # Find the task heading, then replace its status line
    content = _replace_task_status(content, task_id, "EXPERIMENT_PROPOSED", "DONE")

    # Tag the experiment section with [ADOPTED]
    content = _tag_experiment_section(content, task_id, "[ADOPTED]")

    # Write atomically
    tmp = spec_path + ".tmp"
    Path(tmp).write_text(content, encoding="utf-8")
    os.replace(tmp, spec_path)

    # Write event
    write_event(
        events_dir,
        {
            "type": "experiment_adopted",
            "queue_id": queue_id,
            "task_id": task_id,
            "timestamp": datetime.now(timezone.utc).isoformat(),
        },
    )

    return {"success": True}


def reject_experiment(
    queue_dir: str,
    queue_id: str,
    task_id: str,
    events_dir: str,
    reason: str = "",
) -> dict[str, Any]:
    """Reject an experiment: reset the task to PENDING and tag [REJECTED].

    Returns:
      success: bool
      error: str (if not success)
    """
    entry = get_entry(queue_dir, queue_id)
    if entry is None:
        return {"success": False, "error": f"Queue entry not found: {queue_id}"}

    spec_path = entry.get("spec_path", "")
    if not spec_path or not Path(spec_path).is_file():
        return {"success": False, "error": f"Spec file not found: {spec_path}"}

    content = Path(spec_path).read_text(encoding="utf-8")

    # Replace EXPERIMENT_PROPOSED with PENDING for this specific task
    content = _replace_task_status(content, task_id, "EXPERIMENT_PROPOSED", "PENDING")

    # Tag the experiment section with [REJECTED]
    tag = "[REJECTED]"
    if reason:
        tag = f"[REJECTED] {reason}"
    content = _tag_experiment_section(content, task_id, tag)

    # Write atomically
    tmp = spec_path + ".tmp"
    Path(tmp).write_text(content, encoding="utf-8")
    os.replace(tmp, spec_path)

    # Write event
    write_event(
        events_dir,
        {
            "type": "experiment_rejected",
            "queue_id": queue_id,
            "task_id": task_id,
            "reason": reason,
            "timestamp": datetime.now(timezone.utc).isoformat(),
        },
    )

    return {"success": True}


def finalize_review(queue_dir: str, queue_id: str, events_dir: str) -> dict[str, Any]:
    """After all experiments are reviewed, check if spec can be requeued.

    If no EXPERIMENT_PROPOSED tasks remain, change status to queued.

    Returns:
      requeued: bool
      remaining_experiments: int
    """
    entry = get_entry(queue_dir, queue_id)
    if entry is None:
        return {"requeued": False, "remaining_experiments": 0}

    spec_path = entry.get("spec_path", "")
    if not spec_path or not Path(spec_path).is_file():
        return {"requeued": False, "remaining_experiments": 0}

    content = Path(spec_path).read_text(encoding="utf-8")
    tasks = parse_boi_spec(content)

    remaining = [t for t in tasks if t.status == "EXPERIMENT_PROPOSED"]

    if remaining:
        return {"requeued": False, "remaining_experiments": len(remaining)}

    # No experiments left — requeue
    counts = count_boi_tasks(spec_path)
    requeue(queue_dir, queue_id, counts["done"], counts["total"])

    # Clear needs_review fields
    with queue_lock(queue_dir):
        updated_entry = _read_entry(queue_dir, queue_id)
        if updated_entry:
            updated_entry.pop("experiment_tasks", None)
            updated_entry.pop("needs_review_since", None)
            _write_entry(queue_dir, updated_entry)

    write_event(
        events_dir,
        {
            "type": "review_completed",
            "queue_id": queue_id,
            "timestamp": datetime.now(timezone.utc).isoformat(),
        },
    )

    return {"requeued": True, "remaining_experiments": 0}


def _replace_task_status(
    content: str, task_id: str, old_status: str, new_status: str
) -> str:
    """Replace a task's status line in the spec content.

    Finds the ### t-N: heading, then replaces the next non-blank line
    that matches old_status with new_status.
    """
    lines = content.split("\n")
    task_heading_re = re.compile(rf"^###\s+{re.escape(task_id)}:\s+")
    found_heading = False
    result = []

    for line in lines:
        if task_heading_re.match(line):
            found_heading = True
            result.append(line)
            continue

        if found_heading and line.strip() == old_status:
            result.append(new_status)
            found_heading = False
            continue

        # If we found the heading but hit a non-blank non-status line,
        # stop looking (malformed spec)
        if found_heading and line.strip() and line.strip() != old_status:
            found_heading = False

        result.append(line)

    return "\n".join(result)


def _tag_experiment_section(content: str, task_id: str, tag: str) -> str:
    """Append a decision tag to the experiment section of a specific task.

    Finds the #### Experiment: heading under the given task, then appends
    the tag line after the experiment section content.
    """
    lines = content.split("\n")
    task_heading_re = re.compile(rf"^###\s+{re.escape(task_id)}:\s+")
    experiment_heading_re = re.compile(r"^####\s+Experiment:")
    next_heading_re = re.compile(r"^#{1,4}\s+")

    in_task = False
    in_experiment = False
    experiment_end_idx = None
    result = list(lines)

    for i, line in enumerate(lines):
        if task_heading_re.match(line):
            in_task = True
            continue

        # A new ### heading ends the task section
        if in_task and re.match(r"^###\s+t-\d+:", line):
            in_task = False
            if in_experiment:
                experiment_end_idx = i
                in_experiment = False
            continue

        if in_task and experiment_heading_re.match(line):
            in_experiment = True
            continue

        # End of experiment section (next heading of any level)
        if in_experiment and next_heading_re.match(line):
            experiment_end_idx = i
            in_experiment = False
            continue

    # If experiment runs to end of file
    if in_experiment:
        experiment_end_idx = len(lines)

    if experiment_end_idx is not None:
        # Insert the decision tag before the next heading
        decision_lines = ["", f"**Decision:** {tag}", ""]
        result = (
            result[:experiment_end_idx] + decision_lines + result[experiment_end_idx:]
        )

    return "\n".join(result)
