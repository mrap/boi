# telemetry.py — Persistent telemetry tracking for BOI specs.
#
# After each iteration, the daemon calls update_telemetry() which:
#   1. Reads all iteration-N.json files for the spec
#   2. Aggregates them into a summary
#   3. Writes a persistent {id}.telemetry.json file
#
# The telemetry file stores cumulative data so readers (CLI, dashboard)
# can get a quick snapshot without scanning all iteration files.
#
# Schema of {id}.telemetry.json:
#   {
#     "queue_id": "q-001",
#     "total_iterations": 3,
#     "total_time_seconds": 2843,
#     "tasks_completed_per_iteration": [2, 2, 1],
#     "tasks_added_per_iteration": [1, 1, 0],
#     "tasks_skipped_per_iteration": [0, 0, 1],
#     "consecutive_failures": 0,
#     "quality_score_per_iteration": [0.78, 0.81, null],
#     "quality_breakdown": {"code_quality": 0.80, "test_quality": 0.75, ...},
#     "quality_trend": "improving",
#     "quality_alerts": [],
#     "evolution_ratio": 0.25,
#     "productive_failure_rate": 0.0,
#     "mode_per_iteration": ["discover", "discover", "discover"],
#     "tasks_superseded_per_iteration": [0, 0, 0],
#     "challenges_written_per_iteration": [0, 1, 0],
#     "experiments_proposed_per_iteration": [0, 0, 0],
#     "experiments_adopted_per_iteration": [0, 0, 0],
#     "experiments_rejected_per_iteration": [0, 0, 0],
#     "last_updated": "ISO-8601"
#   }

import json
import os
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional


def _telemetry_filename(queue_id: str) -> str:
    """Return the filename for a telemetry file."""
    return f"{queue_id}.telemetry.json"


def _read_telemetry_file(queue_dir: str, queue_id: str) -> Optional[dict[str, Any]]:
    """Read the persistent telemetry file. Returns None if not found."""
    path = Path(queue_dir) / _telemetry_filename(queue_id)
    if not path.is_file():
        return None
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError):
        return None


def _write_telemetry_file(queue_dir: str, queue_id: str, data: dict[str, Any]) -> None:
    """Write a telemetry file atomically."""
    path = Path(queue_dir)
    path.mkdir(parents=True, exist_ok=True)

    filename = _telemetry_filename(queue_id)
    target = path / filename
    tmp = path / f".{filename}.tmp"

    content = json.dumps(data, indent=2, sort_keys=False) + "\n"
    tmp.write_text(content, encoding="utf-8")
    os.rename(str(tmp), str(target))


def load_iteration_files(queue_dir: str, queue_id: str) -> list[dict[str, Any]]:
    """Load all iteration-N.json files for a queue entry.

    Returns a list of iteration metadata dicts sorted by iteration number.
    """
    path = Path(queue_dir)
    if not path.is_dir():
        return []

    iterations: list[dict[str, Any]] = []
    prefix = f"{queue_id}.iteration-"
    for f in sorted(path.iterdir()):
        if not f.name.startswith(prefix) or not f.name.endswith(".json"):
            continue
        try:
            data = json.loads(f.read_text(encoding="utf-8"))
            iterations.append(data)
        except (json.JSONDecodeError, OSError):
            continue

    iterations.sort(key=lambda d: d.get("iteration", 0))
    return iterations


def _compute_quality_trend(scores: list[Optional[float]]) -> str:
    """Compute quality trend from a list of per-iteration scores.

    Returns "improving", "stable", or "declining".
    Skips None entries. Needs at least 2 non-null scores to determine trend.
    """
    valid = [s for s in scores if s is not None]
    if len(valid) < 2:
        return "stable"

    # Use the last 5 scores (or fewer if not enough)
    recent = valid[-5:]
    if len(recent) < 2:
        return "stable"

    # Compute average change over recent window
    total_change = recent[-1] - recent[0]

    if total_change > 0.05:
        return "improving"
    elif total_change < -0.05:
        return "declining"
    return "stable"


def _compute_quality_breakdown(
    iterations: list[dict[str, Any]],
) -> Optional[dict[str, Optional[float]]]:
    """Compute per-category average quality scores across iterations.

    Only considers iterations that have quality_signals data.
    Returns None if no iterations have quality data.
    """
    category_sums: dict[str, float] = {}
    category_counts: dict[str, int] = {}

    for it in iterations:
        signals = it.get("quality_signals")
        if not signals:
            continue
        for cat_name, cat_score in signals.items():
            if cat_score is not None:
                category_sums[cat_name] = category_sums.get(cat_name, 0.0) + cat_score
                category_counts[cat_name] = category_counts.get(cat_name, 0) + 1

    if not category_sums:
        return None

    result: dict[str, Optional[float]] = {}
    for cat_name in ["code_quality", "test_quality", "documentation", "architecture"]:
        if cat_name in category_sums and category_counts[cat_name] > 0:
            result[cat_name] = category_sums[cat_name] / category_counts[cat_name]
        else:
            result[cat_name] = None
    return result


def _compute_evolution_ratio(iterations: list[dict[str, Any]]) -> Optional[float]:
    """Approximate evolution ratio from iteration-level data.

    Fallback when initial_task_ids is not available. Uses tasks_added
    as a proxy for self-evolved tasks.

    Returns None if no tasks have been completed.
    """
    total_completed = sum(it.get("tasks_completed", 0) for it in iterations)
    if total_completed == 0:
        return None

    total_added = sum(it.get("tasks_added", 0) for it in iterations)
    return min(total_added / total_completed, 1.0) if total_completed > 0 else None


def compute_evolution_ratio_from_spec(
    initial_task_ids: list[str],
    spec_path: str,
) -> Optional[float]:
    """Compute evolution ratio by diffing initial task IDs against current spec.

    Evolution ratio = self_evolved_tasks_completed / total_tasks_completed

    Self-evolved tasks are those present in the current spec but absent from
    the initial snapshot, filtered to DONE status.

    Args:
        initial_task_ids: Task IDs from the spec at dispatch time.
        spec_path: Path to the current spec file.

    Returns None if no tasks have been completed or spec cannot be read.
    """
    from lib.spec_parser import parse_boi_spec

    try:
        content = Path(spec_path).read_text(encoding="utf-8")
    except (OSError, FileNotFoundError):
        return None

    current_tasks = parse_boi_spec(content)
    if not current_tasks:
        return None

    initial_set = set(initial_task_ids)
    total_done = sum(1 for t in current_tasks if t.status == "DONE")
    if total_done == 0:
        return None

    # Self-evolved = present in current spec, absent from initial, and DONE
    self_evolved_done = sum(
        1 for t in current_tasks if t.id not in initial_set and t.status == "DONE"
    )

    return self_evolved_done / total_done


def compute_first_pass_rate(spec_path: str) -> Optional[float]:
    """Compute first-pass completion rate.

    first_pass_rate = tasks_done_without_critic_rejection / total_tasks_done

    A task has a critic rejection if there exists a [CRITIC] task that
    references it (e.g., a task body mentioning the target task ID).
    We approximate by checking if any task with "[CRITIC]" in its title
    exists, which means some tasks failed first-pass review.

    Args:
        spec_path: Path to the current spec file.

    Returns None if no tasks are DONE.
    """
    from lib.spec_parser import parse_boi_spec

    try:
        content = Path(spec_path).read_text(encoding="utf-8")
    except (OSError, FileNotFoundError):
        return None

    tasks = parse_boi_spec(content)
    if not tasks:
        return None

    done_tasks = [t for t in tasks if t.status == "DONE"]
    if not done_tasks:
        return None

    # Find task IDs that were targeted by [CRITIC] tasks.
    # A [CRITIC] task has "[CRITIC]" in its title and references other task IDs
    # in its body (e.g., "Quality score for t-5 is below threshold").
    import re

    critic_targeted_ids: set[str] = set()
    for t in tasks:
        if "[CRITIC]" in t.title:
            # Extract task IDs referenced in the critic task body
            referenced = re.findall(r"\bt-\d+\b", t.body)
            critic_targeted_ids.update(referenced)

    tasks_without_critic = sum(1 for t in done_tasks if t.id not in critic_targeted_ids)
    return tasks_without_critic / len(done_tasks)


def _compute_productive_failure_rate(
    iterations: list[dict[str, Any]],
) -> Optional[float]:
    """Compute ratio of productive failures to total failures.

    A failure is an iteration where no task was marked DONE (tasks_completed=0).
    A productive failure is a failure where tasks were added (tasks_added > 0).

    Returns None if there are no failed iterations.
    """
    failed_iters = [it for it in iterations if it.get("tasks_completed", 0) == 0]
    if not failed_iters:
        return None

    productive = sum(1 for it in failed_iters if it.get("tasks_added", 0) > 0)
    return productive / len(failed_iters)


def update_telemetry(
    queue_dir: str,
    queue_id: str,
) -> dict[str, Any]:
    """Aggregate iteration data and write a persistent telemetry file.

    Reads all iteration-N.json files for the spec, computes cumulative
    metrics, and writes the result to {id}.telemetry.json.

    Returns the telemetry data dict.
    """
    iterations = load_iteration_files(queue_dir, queue_id)

    tasks_completed_per_iter: list[int] = []
    tasks_added_per_iter: list[int] = []
    tasks_skipped_per_iter: list[int] = []
    tasks_superseded_per_iter: list[int] = []
    quality_score_per_iter: list[Optional[float]] = []
    mode_per_iter: list[Optional[str]] = []
    challenges_written_per_iter: list[int] = []
    experiments_proposed_per_iter: list[int] = []
    experiments_adopted_per_iter: list[int] = []
    experiments_rejected_per_iter: list[int] = []
    total_time = 0

    for it in iterations:
        tasks_completed_per_iter.append(it.get("tasks_completed", 0))
        tasks_added_per_iter.append(it.get("tasks_added", 0))
        tasks_skipped_per_iter.append(it.get("tasks_skipped", 0))
        tasks_superseded_per_iter.append(it.get("tasks_superseded", 0))
        quality_score_per_iter.append(it.get("quality_score"))
        mode_per_iter.append(it.get("mode"))
        challenges_written_per_iter.append(it.get("challenges_written", 0))
        experiments_proposed_per_iter.append(it.get("experiments_proposed", 0))
        experiments_adopted_per_iter.append(it.get("experiments_adopted", 0))
        experiments_rejected_per_iter.append(it.get("experiments_rejected", 0))
        total_time += it.get("duration_seconds", 0)

    # Read queue entry for consecutive_failures and initial_task_ids
    consecutive_failures = 0
    initial_task_ids: list[str] = []
    spec_path = ""
    entry_path = Path(queue_dir) / f"{queue_id}.json"
    if entry_path.is_file():
        try:
            entry = json.loads(entry_path.read_text(encoding="utf-8"))
            consecutive_failures = entry.get("consecutive_failures", 0)
            initial_task_ids = entry.get("initial_task_ids", [])
            spec_path = entry.get("spec_path", "")
        except (json.JSONDecodeError, OSError):
            pass

    # Compute quality summary fields
    quality_breakdown = _compute_quality_breakdown(iterations)
    quality_trend = _compute_quality_trend(quality_score_per_iter)

    # Compute quality alerts from non-null scores
    quality_alerts: list[dict[str, str]] = []
    valid_scores = [s for s in quality_score_per_iter if s is not None]
    if valid_scores:
        from lib.quality import detect_trend_alerts

        quality_alerts = detect_trend_alerts(valid_scores)

    # Compute Deutschian progress metrics
    # Prefer spec-based evolution ratio when initial_task_ids is available
    if initial_task_ids and spec_path:
        evolution_ratio = compute_evolution_ratio_from_spec(initial_task_ids, spec_path)
    else:
        evolution_ratio = _compute_evolution_ratio(iterations)

    productive_failure_rate = _compute_productive_failure_rate(iterations)

    # First-pass completion rate (tasks not targeted by critic)
    first_pass_rate: Optional[float] = None
    if spec_path:
        first_pass_rate = compute_first_pass_rate(spec_path)

    telemetry = {
        "queue_id": queue_id,
        "total_iterations": len(iterations),
        "total_time_seconds": total_time,
        "tasks_completed_per_iteration": tasks_completed_per_iter,
        "tasks_added_per_iteration": tasks_added_per_iter,
        "tasks_skipped_per_iteration": tasks_skipped_per_iter,
        "consecutive_failures": consecutive_failures,
        # Quality fields
        "quality_score_per_iteration": quality_score_per_iter,
        "quality_breakdown": quality_breakdown,
        "quality_trend": quality_trend,
        "quality_alerts": quality_alerts,
        # Mode fields
        "mode_per_iteration": mode_per_iter,
        "tasks_superseded_per_iteration": tasks_superseded_per_iter,
        "challenges_written_per_iteration": challenges_written_per_iter,
        "experiments_proposed_per_iteration": experiments_proposed_per_iter,
        "experiments_adopted_per_iteration": experiments_adopted_per_iter,
        "experiments_rejected_per_iteration": experiments_rejected_per_iter,
        # Deutschian progress metrics
        "evolution_ratio": evolution_ratio,
        "productive_failure_rate": productive_failure_rate,
        "first_pass_rate": first_pass_rate,
        "last_updated": datetime.now(timezone.utc).isoformat(),
    }

    _write_telemetry_file(queue_dir, queue_id, telemetry)
    return telemetry


def read_telemetry(queue_dir: str, queue_id: str) -> Optional[dict[str, Any]]:
    """Read the persistent telemetry file for a spec.

    Returns the telemetry dict, or None if not found.
    """
    return _read_telemetry_file(queue_dir, queue_id)
