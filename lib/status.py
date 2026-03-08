# status.py — Format status output for BOI CLI.
#
# Reads from the queue directory and builds a status snapshot.
# Two output modes:
#   - Human-readable table (for terminal, with color)
#   - JSON (for programmatic consumption)

import json
import os
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from lib.telemetry import load_iteration_files


# ANSI color codes
GREEN = "\033[0;32m"
YELLOW = "\033[1;33m"
RED = "\033[0;31m"
DIM = "\033[2m"
BOLD = "\033[1m"
NC = "\033[0m"

# ANSI codes for specific use
CYAN = "\033[0;36m"
MAGENTA = "\033[0;35m"

# Status -> color mapping
STATUS_COLORS: dict[str, str] = {
    "completed": GREEN,
    "running": YELLOW,
    "queued": DIM,
    "requeued": YELLOW,
    "failed": RED,
    "canceled": DIM,
    "needs_review": MAGENTA,
    "needs_merge": MAGENTA,
}


def format_duration(seconds: int | float) -> str:
    """Format seconds into a human-readable duration string."""
    seconds = int(seconds)
    if seconds < 60:
        return f"{seconds}s"
    minutes = seconds // 60
    remaining = seconds % 60
    if minutes < 60:
        return f"{minutes}m {remaining:02d}s"
    hours = minutes // 60
    remaining_mins = minutes % 60
    return f"{hours}h {remaining_mins:02d}m"


def _colorize(text: str, color: str) -> str:
    """Wrap text in ANSI color codes."""
    if not color:
        return text
    return f"{color}{text}{NC}"


def load_queue(queue_dir: str) -> list[dict[str, Any]]:
    """Load all queue entries from the queue directory.

    Returns a list of queue entry dicts sorted by priority (lower first).
    """
    path = Path(queue_dir)
    if not path.is_dir():
        return []

    entries = []
    for f in sorted(path.iterdir()):
        if not f.name.startswith("q-") or not f.name.endswith(".json"):
            continue
        # Skip telemetry and iteration files
        if ".telemetry" in f.name or ".iteration-" in f.name:
            continue
        try:
            data = json.loads(f.read_text(encoding="utf-8"))
            if "id" in data:
                entries.append(data)
        except (json.JSONDecodeError, OSError):
            continue

    entries.sort(key=lambda e: e.get("priority", 100))
    return entries


def build_queue_status(
    queue_dir: str, config: dict[str, Any] | None = None
) -> dict[str, Any]:
    """Build a status snapshot from the queue directory.

    Returns a dict with:
        - entries: list of queue entry dicts
        - summary: counts by status
        - workers: worker info from config
    """
    entries = load_queue(queue_dir)

    status_counts: dict[str, int] = {
        "queued": 0,
        "requeued": 0,
        "running": 0,
        "completed": 0,
        "failed": 0,
        "canceled": 0,
        "needs_review": 0,
        "needs_merge": 0,
    }

    for entry in entries:
        status = entry.get("status", "queued")
        if status in status_counts:
            status_counts[status] += 1

    workers = []
    if config:
        workers = config.get("workers", [])

    # Enrich entries with telemetry data (quality, progress)
    for entry in entries:
        qid = entry.get("id", "")
        if not qid:
            continue
        telem = _load_telemetry_for_entry(queue_dir, qid)
        if telem:
            entry["_telemetry"] = telem

    return {
        "entries": entries,
        "summary": {
            "total": len(entries),
            **status_counts,
        },
        "workers": workers,
    }


def _load_telemetry_for_entry(queue_dir: str, queue_id: str) -> dict[str, Any] | None:
    """Load telemetry data for a queue entry if available."""
    from lib.telemetry import read_telemetry

    return read_telemetry(queue_dir, queue_id)


def _get_quality_display(entry: dict[str, Any], color: bool = True) -> tuple[str, str]:
    """Get quality and progress display strings for a queue entry.

    Returns (quality_str, progress_str).
    """
    telem = entry.get("_telemetry")
    if not telem:
        return ("\u2014", "\u2014")  # em dash

    # Quality: use latest non-null score
    scores = telem.get("quality_score_per_iteration", [])
    valid_scores = [s for s in scores if s is not None]
    if not valid_scores:
        quality_str = "\u2014"
    else:
        from lib.quality import format_quality_display, grade

        latest_score = valid_scores[-1]
        letter = grade(latest_score)
        quality_str = format_quality_display(latest_score, letter)
        # Colorize quality grade
        if color:
            if latest_score >= 0.85:
                quality_str = _colorize(quality_str, GREEN)
            elif latest_score < 0.50:
                quality_str = _colorize(quality_str, RED)

    # Progress: completion * quality
    tasks_done = entry.get("tasks_done", 0)
    tasks_total = entry.get("tasks_total", 0)
    if tasks_total > 0 and valid_scores:
        from lib.quality import compute_progress_score

        completion = tasks_done / tasks_total
        progress = compute_progress_score(completion, valid_scores[-1])
        progress_str = f"{int(progress * 100)}%"
    elif tasks_total > 0:
        # No quality data, show raw completion
        progress_str = f"{int(tasks_done / tasks_total * 100)}%"
    else:
        progress_str = "0%"

    return (quality_str, progress_str)


def _get_generate_detail(entry: dict[str, Any]) -> list[str]:
    """Get Generate mode detail lines for an entry.

    Returns a list of indented detail lines, or empty list if not Generate.
    """
    mode = entry.get("mode", "execute")
    if mode != "generate":
        return []

    lines = []
    phase = entry.get("phase", "execute")
    phase_display = phase.upper()

    # Phase progress (rough: decompose=1, execute=2, evaluate=3)
    phase_nums = {"decompose": 1, "execute": 2, "evaluate": 3}
    phase_num = phase_nums.get(phase, 2)
    lines.append(f"  Phase: {phase_display} ({phase_num}/3)")

    # Success criteria (from queue entry if tracked)
    criteria_met = entry.get("criteria_met", 0)
    criteria_total = entry.get("criteria_total", 0)
    if criteria_total > 0:
        lines.append(f"  Success Criteria: {criteria_met}/{criteria_total} met")

    # Experiment budget
    max_exp = entry.get("max_experiment_invocations", 0)
    used_exp = entry.get("experiment_invocations_used", 0)
    if max_exp > 0:
        remaining = max_exp - used_exp
        lines.append(f"  Experiment budget: {remaining}/{max_exp} remaining")

    return lines


def _get_quality_alerts(entries: list[dict[str, Any]], color: bool = True) -> list[str]:
    """Get quality alert warning lines from all entries.

    Returns warning lines like:
      ⚠ q-001: Quality declining (dropped 0.18 in last iteration)
    """
    alert_lines = []
    for entry in entries:
        telem = entry.get("_telemetry")
        if not telem:
            continue

        alerts = telem.get("quality_alerts", [])
        qid = entry.get("id", "?")
        for alert in alerts:
            msg = alert.get("message", "Unknown alert")
            line = f"  \u26a0 {qid}: {msg}"
            if color:
                line = _colorize(line, YELLOW)
            alert_lines.append(line)
    return alert_lines


def _get_terminal_width() -> int:
    """Get terminal width with fallback to 80."""
    try:
        return os.get_terminal_size().columns
    except (ValueError, OSError):
        return 80


def _sort_entries_for_display(entries: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Sort entries: running first, then queued/requeued, then completed/canceled.

    Within each group, preserve original priority order.
    """
    order = {
        "running": 0,
        "requeued": 0,
        "needs_review": 1,
        "needs_merge": 1,
        "failed": 1,
        "queued": 2,
        "completed": 3,
        "canceled": 3,
    }
    return sorted(entries, key=lambda e: order.get(e.get("status", "queued"), 2))


def format_queue_table(
    status_data: dict[str, Any], color: bool = True, width: int | None = None
) -> str:
    """Format queue status as a human-readable, full-width table.

    Adapts to terminal width. Fixed columns for structured data,
    flexible SPEC column gets remaining space.

    Output:
        BOI

        SPEC                           MODE      WORKER  ITER    TASKS        STATUS
        ─────────────────────────────────────────────────────────────────────────────
        q-005  ux-polish               execute   w-1     10/30   6/6 done     running
        ...

        Workers: 3/3 busy  |  3 running, 2 queued, 7 completed
    """
    entries = status_data.get("entries", [])
    summary = status_data.get("summary", {})
    workers = status_data.get("workers", [])

    # Terminal width
    term_w = width if width is not None else _get_terminal_width()

    # Fixed column widths (including trailing space as separator)
    COL_ISO = 9  # "isolate  " (7 + 2 space)
    COL_MODE = 10  # "execute   " (8 + 2 space)
    COL_WORKER = 8  # "w-1     " (6 + 2 space)
    COL_ITER = 8  # "10/30   " (7 + 1 space)
    COL_TASKS = 13  # "6/6 done     " (12 + 1 space)
    COL_STATUS = 12  # "running" right-padded

    fixed_cols = COL_ISO + COL_MODE + COL_WORKER + COL_ITER + COL_TASKS + COL_STATUS
    # SPEC column gets remaining space, minimum 20
    col_spec = max(20, term_w - fixed_cols)

    lines: list[str] = []

    header = "BOI"
    if color:
        header = f"{BOLD}{header}{NC}"
    lines.append(header)
    lines.append("")

    if not entries:
        lines.append("No specs in queue. Ready to dispatch.")
        total_workers = len(workers)
        if total_workers:
            lines.append(f"Workers: 0/{total_workers} busy")
        lines.append("")
        lines.append("Quick start:")
        lines.append("  boi dispatch my-spec.md          Dispatch a spec file")
        lines.append(
            '  boi do "build a REST API"        Describe what you want (uses AI)'
        )
        lines.append("  boi status                       Check progress")
        lines.append("  boi --help                       See all commands")
        return "\n".join(lines)

    # Column header
    col_header = (
        f"{'SPEC':<{col_spec}}"
        f"{'ISO':<{COL_ISO}}"
        f"{'MODE':<{COL_MODE}}"
        f"{'WORKER':<{COL_WORKER}}"
        f"{'ITER':<{COL_ITER}}"
        f"{'TASKS':<{COL_TASKS}}"
        f"{'STATUS'}"
    )
    if color:
        col_header = f"{BOLD}{col_header}{NC}"
    lines.append(col_header)

    # Separator line spanning full width
    sep = "\u2500" * term_w
    if color:
        sep = f"{DIM}{sep}{NC}"
    lines.append(sep)

    # Sort: running first, queued, then completed
    sorted_entries = _sort_entries_for_display(entries)

    generate_details: list[tuple[int, list[str]]] = []
    first_running_id = ""

    for entry in sorted_entries:
        qid = entry.get("id", "?")
        spec_path = entry.get("original_spec_path", entry.get("spec_path", ""))
        spec_name = (
            os.path.splitext(os.path.basename(spec_path))[0] if spec_path else "?"
        )
        label = f"{qid}  {spec_name}"
        # Truncate with ellipsis if too long
        max_label = col_spec - 2  # leave padding
        if len(label) > max_label:
            label = label[: max_label - 1] + "\u2026"

        mode = entry.get("mode", "execute")
        mode_str = mode

        iso_type = "isolate" if entry.get("worktree_isolate") else "shared"

        worker = entry.get("last_worker") or "-"
        if entry.get("status") not in ("running", "requeued"):
            worker = "-"
        iteration = entry.get("iteration", 0)
        max_iter = entry.get("max_iterations", 30)
        iter_str = f"{iteration}/{max_iter}" if entry.get("status") != "queued" else "-"

        # Task progress
        tasks_done = entry.get("tasks_done", 0)
        tasks_total = entry.get("tasks_total", 0)
        if tasks_total > 0:
            tasks_str = f"{tasks_done}/{tasks_total} done"
        else:
            tasks_str = "-"

        status = entry.get("status", "queued")
        # Override display for specs needing merge
        merge_st = entry.get("merge_status")
        if merge_st == "conflict":
            status = "needs_merge"

        # Track first running spec for hint
        if not first_running_id and status == "running":
            first_running_id = qid

        # Quality annotation (compact, appended to status)
        quality_str, _progress_str = _get_quality_display(entry, color=False)
        quality_suffix = ""
        if quality_str != "\u2014" and quality_str != "-":
            quality_suffix = f" [{quality_str}]"

        # Build the plain-text row
        row_text = (
            f"{label:<{col_spec}}"
            f"{iso_type:<{COL_ISO}}"
            f"{mode_str:<{COL_MODE}}"
            f"{worker:<{COL_WORKER}}"
            f"{iter_str:<{COL_ITER}}"
            f"{tasks_str:<{COL_TASKS}}"
            f"{status}{quality_suffix}"
        )

        if color:
            is_completed = status in ("completed", "canceled")
            if is_completed:
                # Dim the entire row for completed specs
                row_text = f"{DIM}{row_text}{NC}"
            else:
                # Colorize just the status portion
                status_color = STATUS_COLORS.get(status, "")
                if status_color:
                    # Rebuild with colored status
                    row_prefix = (
                        f"{label:<{col_spec}}"
                        f"{iso_type:<{COL_ISO}}"
                        f"{mode_str:<{COL_MODE}}"
                        f"{worker:<{COL_WORKER}}"
                        f"{iter_str:<{COL_ITER}}"
                        f"{tasks_str:<{COL_TASKS}}"
                    )
                    colored_status = _colorize(status + quality_suffix, status_color)
                    row_text = f"{row_prefix}{colored_status}"

        lines.append(row_text)

        # Collect Generate mode detail blocks
        gen_detail = _get_generate_detail(entry)
        if gen_detail:
            generate_details.append((len(lines), gen_detail))

    # Insert Generate mode detail blocks (after their parent row)
    for insert_idx, detail_lines in reversed(generate_details):
        for i, dl in enumerate(detail_lines):
            lines.insert(insert_idx + i, dl)

    lines.append("")

    # Quality alerts
    alert_lines = _get_quality_alerts(entries, color)
    if alert_lines:
        for al in alert_lines:
            lines.append(al)
        lines.append("")

    # Summary line (single line)
    total_workers = len(workers)
    running = summary.get("running", 0)
    queued = summary.get("queued", 0) + summary.get("requeued", 0)
    completed = summary.get("completed", 0)
    needs_review = summary.get("needs_review", 0)
    needs_merge = summary.get("needs_merge", 0)
    failed = summary.get("failed", 0)

    parts: list[str] = []
    if total_workers:
        parts.append(f"Workers: {running}/{total_workers} busy")

    count_parts: list[str] = []
    if running:
        count_parts.append(f"{running} running")
    if queued:
        count_parts.append(f"{queued} queued")
    if needs_review:
        count_parts.append(f"{needs_review} needs review")
    if needs_merge:
        count_parts.append(f"{needs_merge} needs merge")
    if failed:
        count_parts.append(f"{failed} failed")
    if completed:
        count_parts.append(f"{completed} completed")

    if count_parts:
        parts.append(", ".join(count_parts))

    lines.append("  |  ".join(parts) if parts else f"Total: {summary.get('total', 0)}")

    # Footer hint
    if first_running_id:
        hint = f"Run 'boi log {first_running_id}' to see worker output"
        if color:
            hint = f"{DIM}{hint}{NC}"
        lines.append(hint)

    return "\n".join(lines)


def format_queue_json(status_data: dict[str, Any]) -> str:
    """Format queue status as JSON."""
    return json.dumps(status_data, indent=2, sort_keys=False)


# ─── Telemetry ──────────────────────────────────────────────────────────────


def build_telemetry(queue_dir: str, queue_id: str) -> dict[str, Any] | None:
    """Build telemetry data for a single spec.

    Prefers the dedicated telemetry file ({id}.telemetry.json) if available.
    Falls back to aggregating from iteration-N.json files.
    Returns None if the queue entry doesn't exist.
    """
    entry_path = Path(queue_dir) / f"{queue_id}.json"
    if not entry_path.is_file():
        return None

    try:
        entry = json.loads(entry_path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError):
        return None

    spec_path = entry.get("original_spec_path", entry.get("spec_path", ""))
    spec_name = os.path.splitext(os.path.basename(spec_path))[0] if spec_path else "?"

    # Try dedicated telemetry file first
    from lib.telemetry import read_telemetry as _read_telem

    telem = _read_telem(queue_dir, queue_id)
    if telem is not None:
        return {
            "queue_id": queue_id,
            "spec_name": spec_name,
            "spec_path": spec_path,
            "status": entry.get("status", "?"),
            "iteration": entry.get("iteration", 0),
            "max_iterations": entry.get("max_iterations", 30),
            "tasks_done": entry.get("tasks_done", 0),
            "tasks_total": entry.get("tasks_total", 0),
            "total_time_seconds": telem.get("total_time_seconds", 0),
            "total_tasks_completed": sum(
                telem.get("tasks_completed_per_iteration", [])
            ),
            "total_tasks_added": sum(telem.get("tasks_added_per_iteration", [])),
            "total_tasks_skipped": sum(telem.get("tasks_skipped_per_iteration", [])),
            "consecutive_failures": telem.get(
                "consecutive_failures", entry.get("consecutive_failures", 0)
            ),
            "iterations": _telem_to_iteration_list(telem),
            # Deutschian progress metrics
            "evolution_ratio": telem.get("evolution_ratio"),
            "productive_failure_rate": telem.get("productive_failure_rate"),
            "first_pass_rate": telem.get("first_pass_rate"),
        }

    # Fallback: aggregate from iteration files
    iterations = load_iteration_files(queue_dir, queue_id)

    total_time = sum(it.get("duration_seconds", 0) for it in iterations)
    total_tasks_completed = sum(it.get("tasks_completed", 0) for it in iterations)
    total_tasks_added = sum(it.get("tasks_added", 0) for it in iterations)
    total_tasks_skipped = sum(it.get("tasks_skipped", 0) for it in iterations)

    return {
        "queue_id": queue_id,
        "spec_name": spec_name,
        "spec_path": spec_path,
        "status": entry.get("status", "?"),
        "iteration": entry.get("iteration", 0),
        "max_iterations": entry.get("max_iterations", 30),
        "tasks_done": entry.get("tasks_done", 0),
        "tasks_total": entry.get("tasks_total", 0),
        "total_time_seconds": total_time,
        "total_tasks_completed": total_tasks_completed,
        "total_tasks_added": total_tasks_added,
        "total_tasks_skipped": total_tasks_skipped,
        "consecutive_failures": entry.get("consecutive_failures", 0),
        "iterations": iterations,
    }


def _telem_to_iteration_list(telem: dict[str, Any]) -> list[dict[str, Any]]:
    """Convert per-iteration arrays from telemetry.json to iteration dicts.

    This bridges the telemetry file format (arrays of values per metric)
    to the iteration list format used by format_telemetry_table.
    """
    completed = telem.get("tasks_completed_per_iteration", [])
    added = telem.get("tasks_added_per_iteration", [])
    skipped = telem.get("tasks_skipped_per_iteration", [])
    count = max(len(completed), len(added), len(skipped))

    result = []
    for i in range(count):
        result.append(
            {
                "iteration": i + 1,
                "tasks_completed": completed[i] if i < len(completed) else 0,
                "tasks_added": added[i] if i < len(added) else 0,
                "tasks_skipped": skipped[i] if i < len(skipped) else 0,
                "duration_seconds": 0,  # Not stored per-iteration in telemetry arrays
                "exit_code": 0,
            }
        )
    return result


def format_telemetry_table(telemetry: dict[str, Any], color: bool = True) -> str:
    """Format telemetry data as a human-readable report."""
    lines = []

    spec_name = telemetry.get("spec_name", "?")
    queue_id = telemetry.get("queue_id", "?")
    iteration = telemetry.get("iteration", 0)
    max_iter = telemetry.get("max_iterations", 30)
    tasks_done = telemetry.get("tasks_done", 0)
    tasks_total = telemetry.get("tasks_total", 0)
    total_added = telemetry.get("total_tasks_added", 0)
    total_skipped = telemetry.get("total_tasks_skipped", 0)
    total_time = telemetry.get("total_time_seconds", 0)
    status = telemetry.get("status", "?")

    header = f"Spec: {spec_name} ({queue_id})"
    if color:
        header = f"{BOLD}{header}{NC}"
    lines.append(header)

    status_color = STATUS_COLORS.get(status, "")
    status_display = _colorize(status, status_color) if color else status
    lines.append(f"Status: {status_display}")
    lines.append(f"Iterations: {iteration} of {max_iter}")
    lines.append(f"Total time: {format_duration(total_time)}")

    task_parts = [f"{tasks_done}/{tasks_total} done"]
    if total_added:
        task_parts.append(f"{total_added} added (self-evolved)")
    if total_skipped:
        task_parts.append(f"{total_skipped} skipped")
    lines.append(f"Tasks: {', '.join(task_parts)}")

    failures = telemetry.get("consecutive_failures", 0)
    if failures:
        lines.append(f"Consecutive failures: {failures}")

    # Deutschian progress metrics
    evo_ratio = telemetry.get("evolution_ratio")
    pfr = telemetry.get("productive_failure_rate")
    fpr = telemetry.get("first_pass_rate")
    if evo_ratio is not None or pfr is not None or fpr is not None:
        lines.append("")
        lines.append("Progress metrics:")
        if evo_ratio is not None:
            pct = f"{evo_ratio:.0%}"
            lines.append(f"  Evolution ratio: {pct} (self-evolved tasks / total done)")
        if pfr is not None:
            pct = f"{pfr:.0%}"
            lines.append(
                f"  Productive failure rate: {pct} (failed iters that added tasks)"
            )
        if fpr is not None:
            pct = f"{fpr:.0%}"
            lines.append(
                f"  First-pass rate: {pct} (tasks done without critic rejection)"
            )

    iterations = telemetry.get("iterations", [])
    if iterations:
        lines.append("")
        lines.append("Iteration breakdown:")
        for it in iterations:
            it_num = it.get("iteration", "?")
            it_done = it.get("tasks_completed", 0)
            it_added = it.get("tasks_added", 0)
            it_skipped = it.get("tasks_skipped", 0)
            it_duration = it.get("duration_seconds", 0)
            it_exit = it.get("exit_code", 0)

            parts = [f"{it_done} tasks done"]
            parts.append(f"{it_added} added")
            parts.append(f"{it_skipped} skipped")
            time_str = format_duration(it_duration)

            suffix_parts = [f"({time_str})"]

            exit_note = ""
            if it_exit != 0:
                exit_note = f" [exit {it_exit}]"
                if color:
                    exit_note = _colorize(exit_note, RED)

            suffix = " ".join(suffix_parts)
            lines.append(f"  #{it_num}: {', '.join(parts)} {suffix}{exit_note}")

    return "\n".join(lines)


def format_telemetry_json(telemetry: dict[str, Any]) -> str:
    """Format telemetry data as JSON."""
    return json.dumps(telemetry, indent=2, sort_keys=False)


# ─── Dashboard (compact view) ──────────────────────────────────────────────


# Status icons (no color — color is applied separately)
STATUS_ICONS: dict[str, str] = {
    "completed": "\u2713",  # ✓
    "running": "\u25b6",  # ▶
    "queued": "\u00b7",  # ·
    "requeued": "\u25b6",  # ▶ (same as running, will be picked up soon)
    "failed": "\u2717",  # ✗
    "canceled": "\u2013",  # –
    "needs_review": "\u2757",  # ❗
    "needs_merge": "\U0001f500",  # 🔀
}


def format_dashboard(
    status_data: dict[str, Any], color: bool = True, width: int | None = None
) -> str:
    """Format queue status as a compact dashboard for tmux panes.

    Adapts to terminal width. Color-coded by status. Shows mode and quality.

    Output:
        ═══ BOI ═══════════════════════════════════════════════ 08:23 ══
         ✓ q-001 ios-recording    disc  5/8  3i  B(0.78)
         ▶ q-002 topic-chats      exec  2/9  1i  ---      w-1
         · q-003 heartbeat        chal  0/5  0i  ---
        Workers: 1/3 busy | Queue: 3
    """
    entries = status_data.get("entries", [])
    summary = status_data.get("summary", {})
    workers = status_data.get("workers", [])

    term_w = width if width is not None else _get_terminal_width()
    # Dashboard targets narrower widths
    dash_w = min(term_w, 80)

    lines: list[str] = []

    # Header bar
    now = datetime.now()
    time_str = now.strftime("%H:%M")
    header_text = "\u2550\u2550\u2550 BOI "
    right = f" {time_str} \u2550\u2550"
    fill_len = dash_w - len(header_text) - len(right)
    if fill_len < 1:
        fill_len = 1
    header_line = header_text + ("\u2550" * fill_len) + right
    if color:
        header_line = f"{BOLD}{header_line}{NC}"
    lines.append(header_line)

    if not entries:
        lines.append(" No specs in queue. Ready to dispatch.")
        total_workers = len(workers)
        if total_workers:
            lines.append(f" Workers: 0/{total_workers} idle")
        lines.append("")
        lines.append(" Quick start:")
        lines.append("   boi dispatch my-spec.md          Dispatch a spec file")
        lines.append(
            '   boi do "build a REST API"        Describe what you want (uses AI)'
        )
        lines.append("   boi status                       Check progress")
        lines.append("   boi --help                       See all commands")
        return "\n".join(lines)

    # Mode abbreviations for compact display
    mode_abbrev: dict[str, str] = {
        "execute": "exec",
        "challenge": "chal",
        "discover": "disc",
        "generate": "gen",
    }

    # Fixed right-side columns: mode(5) + tasks(6) + iter(4) + quality(9) + worker(5) + spacing
    # We calculate the label (qid + spec_name) width as flexible
    RIGHT_FIXED = 30  # approximate space for mode, tasks, iter, quality, worker
    max_label_len = max(20, dash_w - RIGHT_FIXED - 4)  # 4 = icon + spaces

    # Sort entries for display
    sorted_entries = _sort_entries_for_display(entries)

    for entry in sorted_entries:
        status = entry.get("status", "queued")
        icon = STATUS_ICONS.get(status, "?")

        qid = entry.get("id", "?")
        spec_path = entry.get("original_spec_path", entry.get("spec_path", ""))
        spec_name = (
            os.path.splitext(os.path.basename(spec_path))[0] if spec_path else "?"
        )
        # Truncate spec name if needed (with ellipsis)
        label = f"{qid} {spec_name}"
        if len(label) > max_label_len:
            label = label[: max_label_len - 1] + "\u2026"

        mode = entry.get("mode", "execute")
        mode_str = mode_abbrev.get(mode, mode[:4])

        tasks_done = entry.get("tasks_done", 0)
        tasks_total = entry.get("tasks_total", 0)
        tasks_str = f"{tasks_done}/{tasks_total}" if tasks_total > 0 else "\u2014"

        iteration = entry.get("iteration", 0)
        iter_str = f"{iteration}i"

        # Quality compact display
        telem = entry.get("_telemetry")
        quality_compact = "\u2014"
        if telem:
            scores = telem.get("quality_score_per_iteration", [])
            valid = [s for s in scores if s is not None]
            if valid:
                from lib.quality import grade as _grade

                latest = valid[-1]
                quality_compact = f"{_grade(latest)}({latest:.2f})"

        worker = entry.get("last_worker") or ""
        worker_str = f"  {worker}" if worker and status == "running" else ""

        # Build row with fixed-width columns
        row = (
            f" {icon} {label:<{max_label_len}}"
            f" {mode_str:<5}"
            f" {tasks_str:>5}"
            f" {iter_str:>3}"
            f"  {quality_compact:<9}"
            f"{worker_str}"
        )

        if color:
            status_color = STATUS_COLORS.get(status, "")
            if status in ("completed", "canceled"):
                row = f"{DIM}{row}{NC}"
            elif status_color:
                row = f"{status_color}{row}{NC}"

        lines.append(row)

    # Quality alerts (compact)
    alert_lines = _get_quality_alerts(entries, color)
    if alert_lines:
        for al in alert_lines:
            lines.append(al)

    # Summary line
    total_workers = len(workers)
    running = summary.get("running", 0)
    total_specs = summary.get("total", 0)

    parts: list[str] = []
    if total_workers:
        parts.append(f"Workers: {running}/{total_workers} busy")
    parts.append(f"Queue: {total_specs}")
    summary_line = " | ".join(parts)
    lines.append(summary_line)

    return "\n".join(lines)
