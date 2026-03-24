# status.py — Format status output for BOI CLI.
#
# Reads from the queue directory (JSON) or SQLite database and
# builds a status snapshot. Prefers SQLite when boi.db exists in
# the state directory (parent of queue_dir), falling back to JSON.
#
# Two output modes:
#   - Human-readable table (for terminal, with color)
#   - JSON (for programmatic consumption)

import json
import os
import sqlite3
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from lib.telemetry import load_iteration_files


# ANSI color codes — Catppuccin Latte-compatible (true color, works on light bg)
GREEN = "\033[38;2;64;160;43m"      # Latte green (#40a02b)
YELLOW = "\033[38;2;223;142;29m"    # Latte yellow/peach (#df8e1d)
RED = "\033[38;2;210;15;57m"        # Latte red (#d20f39)
DIM = "\033[38;2;108;111;133m"      # Latte subtext0 (#6c6f85)
BOLD = "\033[1m"
NC = "\033[0m"

# ANSI codes for specific use
CYAN = "\033[38;2;4;165;229m"       # Latte sapphire (#04a5e5)
MAGENTA = "\033[38;2;136;57;239m"   # Latte mauve (#8839ef)

# Status -> color mapping
STATUS_COLORS: dict[str, str] = {
    "completed": GREEN,
    "running": YELLOW,
    "queued": DIM,
    "requeued": YELLOW,
    "failed": RED,
    "canceled": DIM,
    "needs_review": MAGENTA,
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
    """Load all queue entries from the queue directory (JSON files).

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


def load_queue_from_db(db_path: str) -> list[dict[str, Any]]:
    """Load all queue entries from the SQLite database.

    Opens a read-only connection and returns all specs sorted by
    priority. Falls back to an empty list on any database error.
    """
    try:
        conn = sqlite3.connect(
            f"file:{db_path}?mode=ro",
            uri=True,
            timeout=5,
        )
        conn.row_factory = sqlite3.Row
        try:
            cursor = conn.execute(
                "SELECT * FROM specs ORDER BY priority ASC, submitted_at ASC"
            )
            return [dict(row) for row in cursor]
        except sqlite3.OperationalError:
            return []
        finally:
            conn.close()
    except sqlite3.OperationalError:
        return []


def _get_db_path(queue_dir: str) -> str | None:
    """Return the path to boi.db if it exists in the state directory.

    The state directory is the parent of the queue directory.
    Returns None if boi.db does not exist.
    """
    state_dir = str(Path(queue_dir).parent)
    db_path = os.path.join(state_dir, "boi.db")
    if os.path.isfile(db_path):
        return db_path
    return None


def _load_all_deps_from_db(db_path: str) -> dict[str, dict[str, Any]]:
    """Load all dependency relationships for all specs from the database.

    Returns a dict mapping queue_id -> {
        'blocked_by': list of (id, status) tuples — specs this spec waits on,
        'blocking':   list of (id, status) tuples — specs waiting on this spec,
    }
    """
    try:
        conn = sqlite3.connect(
            f"file:{db_path}?mode=ro",
            uri=True,
            timeout=5,
        )
        conn.row_factory = sqlite3.Row
        try:
            rows = conn.execute(
                "SELECT sd.spec_id, s_dep.status AS dep_status, "
                "       sd.blocks_on, s_on.status AS on_status "
                "FROM spec_dependencies sd "
                "JOIN specs s_dep ON s_dep.id = sd.spec_id "
                "JOIN specs s_on  ON s_on.id  = sd.blocks_on"
            ).fetchall()

            result: dict[str, dict[str, Any]] = {}
            for row in rows:
                spec_id = row["spec_id"]
                dep_status = row["dep_status"]
                blocks_on_id = row["blocks_on"]
                on_status = row["on_status"]

                if spec_id not in result:
                    result[spec_id] = {"blocked_by": [], "blocking": []}
                result[spec_id]["blocked_by"].append((blocks_on_id, on_status))

                if blocks_on_id not in result:
                    result[blocks_on_id] = {"blocked_by": [], "blocking": []}
                result[blocks_on_id]["blocking"].append((spec_id, dep_status))

            return result
        except sqlite3.OperationalError:
            return {}
        finally:
            conn.close()
    except sqlite3.OperationalError:
        return {}


def build_queue_status(
    queue_dir: str, config: dict[str, Any] | None = None
) -> dict[str, Any]:
    """Build a status snapshot from SQLite or the queue directory.

    Prefers SQLite (boi.db in state dir) when available, falling
    back to JSON queue files.

    Returns a dict with:
        - entries: list of queue entry dicts
        - summary: counts by status
        - workers: worker info from config
    """
    db_path = _get_db_path(queue_dir)
    if db_path is not None:
        entries = load_queue_from_db(db_path)
    else:
        entries = load_queue(queue_dir)

    status_counts: dict[str, int] = {
        "queued": 0,
        "requeued": 0,
        "running": 0,
        "completed": 0,
        "failed": 0,
        "canceled": 0,
        "needs_review": 0,
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

    # Enrich entries with dependency info (blocked_by / blocking)
    if db_path is not None:
        all_deps = _load_all_deps_from_db(db_path)
        for entry in entries:
            qid = entry.get("id", "")
            if qid in all_deps:
                entry["_deps"] = all_deps[qid]

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


def _get_blocked_specs_display(
    entries: list[dict[str, Any]], color: bool = True
) -> list[str]:
    """Get display lines for specs blocked by unmet dependencies.

    Returns warning lines like:
      ⏳ q-007: waiting on q-003 (queued), q-005 (running)
    """
    lines = []
    header_added = False
    for entry in entries:
        deps_info = entry.get("_deps", {})
        blocked_by = deps_info.get("blocked_by", [])
        unmet = [(dep_id, dep_status) for dep_id, dep_status in blocked_by if dep_status != "completed"]
        if not unmet:
            continue
        if not header_added:
            header = "Blocked:"
            if color:
                header = _colorize(header, YELLOW)
            lines.append(header)
            header_added = True
        qid = entry.get("id", "?")
        dep_parts = [f"{dep_id} ({dep_status})" for dep_id, dep_status in unmet]
        line = f"  \u23f3 {qid}: waiting on {', '.join(dep_parts)}"
        if color:
            line = _colorize(line, YELLOW)
        lines.append(line)
    return lines


def _get_terminal_width() -> int:
    """Get terminal width, trying multiple sources with fallback to 120.

    Sources tried in order:
    1. os.get_terminal_size() on stdout (fd 1)
    2. os.get_terminal_size() on stderr (fd 2)
    3. /dev/tty (works even when stdout/stderr are piped)
    4. $COLUMNS environment variable
    5. Fallback: 120

    Enforces a minimum of 80 columns.
    """
    import sys

    # Try stdout
    try:
        cols = os.get_terminal_size(1).columns
        return max(80, cols)
    except (ValueError, OSError):
        pass

    # Try stderr
    try:
        cols = os.get_terminal_size(2).columns
        return max(80, cols)
    except (ValueError, OSError):
        pass

    # Try /dev/tty directly (works when stdout/stderr are piped)
    try:
        with open("/dev/tty") as tty:
            cols = os.get_terminal_size(tty.fileno()).columns
            return max(80, cols)
    except (ValueError, OSError):
        pass

    # Try $COLUMNS env var (set by bash/zsh)
    cols_env = os.environ.get("COLUMNS", "")
    if cols_env.isdigit():
        return max(80, int(cols_env))

    # Default fallback
    return 120


def _sort_entries_for_display(entries: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Sort entries: running first, then queued/requeued, then completed/canceled.

    Within each group, preserve original priority order.
    """
    order = {
        "running": 0,
        "requeued": 0,
        "needs_review": 1,
        "failed": 1,
        "queued": 2,
        "completed": 3,
        "canceled": 3,
    }
    return sorted(entries, key=lambda e: order.get(e.get("status", "queued"), 2))


def _sort_by_queue(entries: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Sort by queue ID (q-001, q-002, ...)."""
    return sorted(entries, key=lambda e: e.get("id", "q-999"))


def _sort_by_status(entries: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Sort by status: running first, then queued, then completed. Within each group, by queue ID."""
    order = {
        "running": 0,
        "requeued": 0,
        "needs_review": 1,
        "failed": 1,
        "queued": 2,
        "completed": 3,
        "canceled": 3,
    }
    return sorted(
        entries,
        key=lambda e: (order.get(e.get("status", "queued"), 2), e.get("id", "q-999")),
    )


def _get_completion_pct(entry: dict[str, Any]) -> float:
    """Get completion percentage for an entry."""
    tasks_total = entry.get("tasks_total", 0)
    tasks_done = entry.get("tasks_done", 0)
    if tasks_total <= 0:
        return 0.0
    return tasks_done / tasks_total


def _sort_by_progress(entries: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Sort by completion percentage descending."""
    return sorted(
        entries, key=lambda e: (-_get_completion_pct(e), e.get("id", "q-999"))
    )


def _sort_by_name(entries: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Sort alphabetically by spec name."""

    def get_name(e: dict[str, Any]) -> str:
        spec_path = e.get("original_spec_path", e.get("spec_path", ""))
        return (
            os.path.splitext(os.path.basename(spec_path))[0].lower()
            if spec_path
            else "zzz"
        )

    return sorted(entries, key=get_name)


def _sort_by_recent(entries: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Sort by last activity (last_iteration_at) descending. Most recent first."""

    def get_last_activity(e: dict[str, Any]) -> str:
        return e.get("last_iteration_at", "") or ""

    return sorted(entries, key=get_last_activity, reverse=True)


def _sort_by_dag(entries: list[dict[str, Any]]) -> list[tuple[dict[str, Any], int]]:
    """Topological sort by blocked_by dependencies.

    Returns list of (entry, depth) tuples where depth is the dependency depth.
    Entries with no blockers come first, then their dependents.
    Handles cycles gracefully by breaking them and logging a warning.
    """
    # Build adjacency: id -> list of IDs that depend on it
    entry_map: dict[str, dict[str, Any]] = {}
    children: dict[str, list[str]] = {}
    parents: dict[str, list[str]] = {}

    for e in entries:
        eid = e.get("id", "")
        entry_map[eid] = e
        blocked_by = e.get("blocked_by", []) or []
        parents[eid] = [
            b for b in blocked_by if b in {x.get("id", "") for x in entries}
        ]
        for b in parents[eid]:
            children.setdefault(b, []).append(eid)

    # Kahn's algorithm for topological sort
    in_degree: dict[str, int] = {e.get("id", ""): 0 for e in entries}
    for eid, plist in parents.items():
        in_degree[eid] = len(plist)

    queue_ids: list[str] = sorted([eid for eid, deg in in_degree.items() if deg == 0])
    result: list[tuple[dict[str, Any], int]] = []
    depth_map: dict[str, int] = {}
    visited: set[str] = set()

    while queue_ids:
        current = queue_ids.pop(0)
        if current in visited:
            continue
        visited.add(current)

        # Depth is max(parent depths) + 1, or 0 for roots
        parent_depths = [depth_map.get(p, 0) for p in parents.get(current, [])]
        depth = (max(parent_depths) + 1) if parent_depths else 0
        depth_map[current] = depth

        if current in entry_map:
            result.append((entry_map[current], depth))

        for child in sorted(children.get(current, [])):
            in_degree[child] -= 1
            if in_degree[child] <= 0:
                queue_ids.append(child)

    # Handle cycles: any unvisited nodes are in cycles
    for e in entries:
        eid = e.get("id", "")
        if eid not in visited:
            # Break cycle: add with depth 0
            import sys

            print(
                f"Warning: cycle detected involving {eid}, breaking cycle",
                file=sys.stderr,
            )
            result.append((e, 0))

    return result


def filter_entries(
    entries: list[dict[str, Any]],
    filter_status: str = "all",
    show_completed: bool = True,
) -> list[dict[str, Any]]:
    """Filter entries by status.

    Args:
        entries: List of queue entry dicts.
        filter_status: "all", "running", "queued", or "completed".
        show_completed: If False, hide completed/canceled specs (independent of filter_status).

    Returns:
        Filtered list of entries.
    """
    result = entries

    # Apply show_completed toggle (hides completed/canceled regardless of filter)
    if not show_completed:
        result = [e for e in result if e.get("status") not in ("completed", "canceled")]

    # Apply status filter
    if filter_status == "running":
        result = [e for e in result if e.get("status") in ("running", "requeued")]
    elif filter_status == "queued":
        result = [
            e for e in result if e.get("status") in ("queued", "needs_review", "failed")
        ]
    elif filter_status == "completed":
        result = [e for e in result if e.get("status") in ("completed", "canceled")]
    # "all" = no filtering

    return result


def sort_entries(
    entries: list[dict[str, Any]], sort_mode: str = "queue"
) -> list[dict[str, Any]] | list[tuple[dict[str, Any], int]]:
    """Sort entries by the given mode.

    For most modes, returns list[dict]. For "dag" mode, returns list[(dict, depth)].
    """
    if sort_mode == "queue":
        return _sort_by_queue(entries)
    elif sort_mode == "status":
        return _sort_by_status(entries)
    elif sort_mode == "progress":
        return _sort_by_progress(entries)
    elif sort_mode == "dag":
        return _sort_by_dag(entries)
    elif sort_mode == "name":
        return _sort_by_name(entries)
    elif sort_mode == "recent":
        return _sort_by_recent(entries)
    else:
        return _sort_by_queue(entries)


def _apply_view_filter(
    entries: list[dict[str, Any]], view_mode: str
) -> list[dict[str, Any]]:
    """Filter entries based on view_mode.

    view_mode:
        "all"       — no filtering (show everything)
        "default"   — running/queued/needs_review + completed/failed in last 24h
        "running"   — only running/requeued/assigning
        "recent:N"  — last N entries by most recent activity timestamp
    """
    if view_mode == "all":
        return entries

    if view_mode == "running":
        return [
            e for e in entries
            if e.get("status") in ("running", "requeued", "assigning")
        ]

    if view_mode.startswith("recent:"):
        try:
            n = int(view_mode.split(":", 1)[1])
        except (ValueError, IndexError):
            n = 10

        def _ts(e: dict[str, Any]) -> datetime:
            ts_str = e.get("last_iteration_at") or e.get("submitted_at") or ""
            try:
                return datetime.fromisoformat(ts_str.replace("Z", "+00:00"))
            except Exception:
                return datetime.min.replace(tzinfo=timezone.utc)

        return sorted(entries, key=_ts, reverse=True)[:n]

    # default: active specs + failed within 24h + completed within 6h
    # Canceled is never shown in default view.
    now = datetime.now(timezone.utc)
    from datetime import timedelta
    cutoff_failed = now - timedelta(hours=24)
    cutoff_completed = now - timedelta(hours=6)

    def _last_ts(e: dict[str, Any]) -> datetime | None:
        ts_str = e.get("last_iteration_at") or e.get("submitted_at") or ""
        if not ts_str:
            return None
        try:
            return datetime.fromisoformat(ts_str.replace("Z", "+00:00"))
        except Exception:
            return None

    result = []
    for e in entries:
        status = e.get("status", "")
        if status in ("running", "requeued", "queued", "needs_review", "assigning"):
            result.append(e)
        elif status == "failed":
            ts = _last_ts(e)
            if ts is not None and ts >= cutoff_failed:
                result.append(e)
        elif status == "completed":
            ts = _last_ts(e)
            if ts is not None and ts >= cutoff_completed:
                result.append(e)
        # canceled: never shown in default view
    return result


def format_queue_table(
    status_data: dict[str, Any],
    color: bool = True,
    width: int | None = None,
    view_mode: str = "default",
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
    all_entries = status_data.get("entries", [])
    summary = status_data.get("summary", {})
    workers = status_data.get("workers", [])

    # Apply view filter
    entries = _apply_view_filter(all_entries, view_mode)
    total_entry_count = len(all_entries)
    shown_entry_count = len(entries)

    # Terminal width (minimum 80)
    term_w = max(80, width if width is not None else _get_terminal_width())

    # Fixed column widths (including trailing space as separator)
    COL_MODE = 10  # "execute   " (8 + 2 space)
    COL_WORKER = 8  # "w-1     " (6 + 2 space)
    COL_ITER = 8  # "10/30   " (7 + 1 space)
    COL_TASKS = 13  # "6/6 done     " (12 + 1 space)
    COL_DEPS = 14  # "⏳ q-001,q-002" dep info
    COL_STATUS = 12  # "running" right-padded

    fixed_cols = COL_MODE + COL_WORKER + COL_ITER + COL_TASKS + COL_DEPS + COL_STATUS
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
        f"{'MODE':<{COL_MODE}}"
        f"{'WORKER':<{COL_WORKER}}"
        f"{'ITER':<{COL_ITER}}"
        f"{'TASKS':<{COL_TASKS}}"
        f"{'Deps':<{COL_DEPS}}"
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

        # Track first running spec for hint
        if not first_running_id and status == "running":
            first_running_id = qid

        # Quality annotation (compact, appended to status)
        quality_str, _progress_str = _get_quality_display(entry, color=False)
        quality_suffix = ""
        if quality_str != "\u2014" and quality_str != "-":
            quality_suffix = f" [{quality_str}]"

        # Deps column
        deps_info = entry.get("_deps", {})
        blocked_by = deps_info.get("blocked_by", [])  # list of (id, status) tuples
        blocking = deps_info.get("blocking", [])       # list of (id, status) tuples
        unmet_deps = [dep_id for dep_id, dep_status in blocked_by if dep_status != "completed"]
        if unmet_deps:
            dep_ids_str = ",".join(unmet_deps)
            deps_str = f"\u23f3 {dep_ids_str}"  # ⏳
        elif blocking:
            blocking_ids = ",".join(bid for bid, _ in blocking)
            deps_str = f"\u2192 {blocking_ids}"  # →
        else:
            deps_str = "\u2014"  # em dash

        # Truncate deps_str to column width
        if len(deps_str) > COL_DEPS - 1:
            deps_str = deps_str[:COL_DEPS - 2] + "\u2026"

        # Build the plain-text row
        row_text = (
            f"{label:<{col_spec}}"
            f"{mode_str:<{COL_MODE}}"
            f"{worker:<{COL_WORKER}}"
            f"{iter_str:<{COL_ITER}}"
            f"{tasks_str:<{COL_TASKS}}"
            f"{deps_str:<{COL_DEPS}}"
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
                        f"{mode_str:<{COL_MODE}}"
                        f"{worker:<{COL_WORKER}}"
                        f"{iter_str:<{COL_ITER}}"
                        f"{tasks_str:<{COL_TASKS}}"
                        f"{deps_str:<{COL_DEPS}}"
                    )
                    colored_status = _colorize(status + quality_suffix, status_color)
                    row_text = f"{row_prefix}{colored_status}"

        lines.append(row_text)

        # Show failure reason on a second line for failed specs
        if status == "failed":
            fail_reason = entry.get("failure_reason", "")
            if fail_reason:
                reason_line = f"       Reason: {fail_reason}"
                if color:
                    reason_line = _colorize(reason_line, RED)
                lines.append(reason_line)

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

    # Blocked specs section: list specs with unmet dependencies
    blocked_lines = _get_blocked_specs_display(entries, color)
    if blocked_lines:
        for bl in blocked_lines:
            lines.append(bl)
        lines.append("")

    # Summary line (single line)
    total_workers = len(workers)
    running = summary.get("running", 0)
    queued = summary.get("queued", 0) + summary.get("requeued", 0)
    completed = summary.get("completed", 0)
    needs_review = summary.get("needs_review", 0)
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

    # "Showing X of Y" summary when filtered
    if view_mode != "all" and shown_entry_count < total_entry_count:
        if view_mode == "default":
            showing_hint = (
                f"Showing {shown_entry_count} of {total_entry_count} specs"
                " (running + last 6h). Use --all for full history."
            )
        else:
            showing_hint = (
                f"Showing {shown_entry_count} of {total_entry_count} specs."
                " Use --all to see all."
            )
        if color:
            showing_hint = f"{DIM}{showing_hint}{NC}"
        lines.append(showing_hint)

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
        iterations = _telem_to_iteration_list(telem)
        # Enrich iterations with failure data from iteration-N.json files
        _enrich_iterations_with_failure_data(iterations, queue_dir, queue_id)
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
            "iterations": iterations,
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


def _enrich_iterations_with_failure_data(
    iterations: list[dict[str, Any]], queue_dir: str, queue_id: str
) -> None:
    """Enrich iteration dicts with failure_reason, crash, and log_tail from iteration files.

    Reads iteration-N.json files and merges failure data into the iteration list
    in-place. This is needed because the telemetry arrays don't store failure details.
    """
    iter_files = load_iteration_files(queue_dir, queue_id)
    # Build a lookup by iteration number
    iter_file_map: dict[int, dict[str, Any]] = {}
    for f in iter_files:
        iter_num = f.get("iteration", 0)
        if iter_num > 0:
            iter_file_map[iter_num] = f

    for it in iterations:
        it_num = it.get("iteration", 0)
        file_data = iter_file_map.get(it_num)
        if not file_data:
            continue
        # Merge failure fields if present
        if "failure_reason" in file_data:
            it["failure_reason"] = file_data["failure_reason"]
        if file_data.get("crash"):
            it["crash"] = True
        if "exit_code" in file_data and file_data["exit_code"] != 0:
            it["exit_code"] = file_data["exit_code"]
        if "duration_seconds" in file_data and file_data["duration_seconds"]:
            it["duration_seconds"] = file_data["duration_seconds"]
        if "log_tail" in file_data:
            it["log_tail"] = file_data["log_tail"]


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
            it_crash = it.get("crash", False)
            it_failure = it.get("failure_reason", "")

            # Show crash/failure iterations differently
            if it_crash or it_failure:
                label = "CRASH" if it_crash else "FAIL"
                reason = it_failure or "Unknown error"
                time_str = format_duration(it_duration) if it_duration else ""
                time_suffix = f" ({time_str})" if time_str else ""
                line = f"  #{it_num}: {label} - {reason}{time_suffix}"
                if color:
                    line = _colorize(line, RED)
                lines.append(line)
                continue

            parts = [f"{it_done} tasks done"]
            parts.append(f"{it_added} added")
            parts.append(f"{it_skipped} skipped")
            time_str = format_duration(it_duration)

            suffix_parts = [f"({time_str})"]

            exit_note = ""
            if it_exit != 0 and it_exit is not None:
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
}


def format_dashboard(
    status_data: dict[str, Any],
    color: bool = True,
    width: int | None = None,
    sort_mode: str = "queue",
    filter_status: str = "all",
    show_completed: bool = True,
    selected_row: int = 0,
) -> str:
    """Format queue status as a compact dashboard for tmux panes.

    Adapts to terminal width. Color-coded by status. Shows mode and quality.

    Output:
        ═══ BOI ═══════════════════════════════════════════════ 08:23 ══
         ✓ q-001 add-dark-mode    disc  5/8  3i  B(0.78)
         ▶ q-002 api-endpoints    exec  2/9  1i  ---      w-1
         · q-003 polish-onboard   chal  0/5  0i  ---
        Workers: 1/3 busy | Queue: 3
    """
    entries = status_data.get("entries", [])
    summary = status_data.get("summary", {})
    workers = status_data.get("workers", [])

    term_w = width if width is not None else _get_terminal_width()
    # Dashboard targets narrower widths
    dash_w = min(term_w, 80)

    lines: list[str] = []

    # Track total entries before filtering for "Showing X of Y" display
    total_entries = len(entries)

    # Header bar with filter/sort indicators
    now = datetime.now()
    time_str = now.strftime("%H:%M")
    header_text = "\u2550\u2550\u2550 BOI "
    # Add filter/sort indicators to header
    indicators: list[str] = []
    if filter_status != "all":
        indicators.append(f"filter: {filter_status}")
    if sort_mode != "queue":
        indicators.append(f"sort: {sort_mode}")
    if not show_completed:
        indicators.append("completed: hidden")
    if indicators:
        indicator_str = " [" + "] [".join(indicators) + "] "
        header_text += indicator_str
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

    # Sort entries for display using the requested sort mode
    sorted_result = sort_entries(entries, sort_mode)

    # Build display rows: for DAG mode, entries come with depth for indentation
    display_items: list[tuple[dict[str, Any], int]] = []
    if sort_mode == "dag":
        # sort_entries returns list[(entry, depth)] for dag mode
        display_items = sorted_result  # type: ignore[assignment]
    else:
        display_items = [(e, 0) for e in sorted_result]  # type: ignore[misc]

    # Apply filtering after sorting (preserves sort order)
    filtered_items: list[tuple[dict[str, Any], int]] = []
    for entry, depth in display_items:
        status = entry.get("status", "queued")
        # show_completed toggle
        if not show_completed and status in ("completed", "canceled"):
            continue
        # status filter
        if filter_status == "running" and status not in ("running", "requeued"):
            continue
        if filter_status == "queued" and status not in (
            "queued",
            "needs_review",
            "failed",
        ):
            continue
        if filter_status == "completed" and status not in ("completed", "canceled"):
            continue
        filtered_items.append((entry, depth))

    shown_count = len(filtered_items)

    # Clamp selected_row to valid range
    if shown_count > 0:
        selected_row = max(0, min(selected_row, shown_count - 1))
    else:
        selected_row = 0

    # Track queue IDs in display order for row-to-id mapping
    visible_queue_ids: list[str] = []

    for row_idx, (entry, depth) in enumerate(filtered_items):
        status = entry.get("status", "queued")
        icon = STATUS_ICONS.get(status, "?")

        qid = entry.get("id", "?")
        visible_queue_ids.append(qid)
        spec_path = entry.get("original_spec_path", entry.get("spec_path", ""))
        spec_name = (
            os.path.splitext(os.path.basename(spec_path))[0] if spec_path else "?"
        )
        # DAG indentation: 2 spaces per depth level
        indent = "  " * depth if sort_mode == "dag" else ""
        # Truncate spec name if needed (with ellipsis)
        label = f"{indent}{qid} {spec_name}"
        effective_max = max_label_len
        if len(label) > effective_max:
            label = label[: effective_max - 1] + "\u2026"

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

        # Row selection indicator
        is_selected = row_idx == selected_row
        sel_marker = "\u25b8" if is_selected else " "  # ▸ or space

        # Build row with fixed-width columns
        row = (
            f"{sel_marker}{icon} {label:<{effective_max}}"
            f" {mode_str:<5}"
            f" {tasks_str:>5}"
            f" {iter_str:>3}"
            f"  {quality_compact:<9}"
            f"{worker_str}"
        )

        if color:
            status_color = STATUS_COLORS.get(status, "")
            if is_selected:
                # Selected row: bold with status color
                if status in ("completed", "canceled"):
                    row = f"{BOLD}{DIM}{row}{NC}"
                elif status_color:
                    row = f"{BOLD}{status_color}{row}{NC}"
                else:
                    row = f"{BOLD}{row}{NC}"
            elif status in ("completed", "canceled"):
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
    # Show "Showing X of Y specs" when filtering hides entries
    if shown_count < total_entries:
        parts.append(f"Showing {shown_count} of {total_entries} specs")
    else:
        parts.append(f"Queue: {total_specs}")
    summary_line = " | ".join(parts)
    lines.append(summary_line)

    # Emit visible queue IDs as a machine-readable footer line for dashboard.sh
    # Format: __QUEUE_IDS__:q-001,q-002,q-003
    if visible_queue_ids:
        lines.append(f"__QUEUE_IDS__:{','.join(visible_queue_ids)}")

    return "\n".join(lines)


def get_visible_queue_ids(
    status_data: dict[str, Any],
    sort_mode: str = "queue",
    filter_status: str = "all",
    show_completed: bool = True,
) -> list[str]:
    """Return the ordered list of queue IDs as they appear in the dashboard.

    This mirrors the sorting/filtering logic in format_dashboard() so the
    bash dashboard can map row index to queue ID.
    """
    entries = status_data.get("entries", [])

    sorted_result = sort_entries(entries, sort_mode)

    display_items: list[tuple[dict[str, Any], int]] = []
    if sort_mode == "dag":
        display_items = sorted_result  # type: ignore[assignment]
    else:
        display_items = [(e, 0) for e in sorted_result]  # type: ignore[misc]

    queue_ids: list[str] = []
    for entry, _depth in display_items:
        status = entry.get("status", "queued")
        if not show_completed and status in ("completed", "canceled"):
            continue
        if filter_status == "running" and status not in ("running", "requeued"):
            continue
        if filter_status == "queued" and status not in (
            "queued",
            "needs_review",
            "failed",
        ):
            continue
        if filter_status == "completed" and status not in ("completed", "canceled"):
            continue
        queue_ids.append(entry.get("id", "?"))

    return queue_ids
