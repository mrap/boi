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
import re
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


def format_relative_time(ts: str | None) -> str:
    """Format a timestamp as a relative time string like '5m ago'.

    Returns em dash for None or invalid input.
    """
    if ts is None:
        return "\u2014"
    try:
        dt = datetime.fromisoformat(ts.replace("Z", "+00:00"))
        now = datetime.now(timezone.utc)
        delta = now - dt
        seconds = int(delta.total_seconds())
        if seconds < 60:
            return f"{seconds}s ago"
        minutes = seconds // 60
        if minutes < 60:
            return f"{minutes}m ago"
        hours = minutes // 60
        if hours < 24:
            return f"{hours}h ago"
        days = hours // 24
        return f"{days}d ago"
    except Exception:
        return "\u2014"


def _colorize(text: str, color: str) -> str:
    """Wrap text in ANSI color codes."""
    if not color:
        return text
    return f"{color}{text}{NC}"


def _progress_bar(done: int, total: int, width: int = 20, color: bool = True, status: str = "") -> str:
    """Generate a Unicode progress bar.

    Args:
        done: completed tasks
        total: total tasks
        width: bar width in characters (default 20)
        color: whether to add ANSI color codes
        status: spec status for color selection (running/completed/failed/queued)

    Returns: string like "████████████░░░░░░░░"
    """
    FILLED = "\u2588"  # █
    EMPTY = "\u2591"   # ░

    if total <= 0:
        bar = EMPTY * width
    else:
        filled_count = min(width, round(done / total * width))
        bar = FILLED * filled_count + EMPTY * (width - filled_count)

    if color:
        color_code = {
            "completed": GREEN,
            "running": YELLOW,
            "failed": RED,
            "queued": DIM,
            "requeued": YELLOW,
            "canceled": DIM,
        }.get(status, "")
        if color_code:
            bar = f"{color_code}{bar}{NC}"

    return bar


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
        "queue_dir": queue_dir,
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


def get_terminal_width() -> int:
    """Public interface for terminal width detection. Delegates to _get_terminal_width."""
    return _get_terminal_width()


def _get_terminal_height() -> int:
    """Get terminal height (rows), trying multiple sources with fallback to 9999.

    Sources tried in order:
    1. os.get_terminal_size() on stdout (fd 1)
    2. os.get_terminal_size() on stderr (fd 2)
    3. /dev/tty (works even when stdout/stderr are piped)
    4. $LINES environment variable
    5. Fallback: 9999 (effectively unlimited — no truncation)

    Returns 9999 when height is unknown to avoid spurious truncation.
    """
    # Try stdout
    try:
        rows = os.get_terminal_size(1).lines
        return max(10, rows)
    except (ValueError, OSError):
        pass

    # Try stderr
    try:
        rows = os.get_terminal_size(2).lines
        return max(10, rows)
    except (ValueError, OSError):
        pass

    # Try /dev/tty directly (works when stdout/stderr are piped)
    try:
        with open("/dev/tty") as tty:
            rows = os.get_terminal_size(tty.fileno()).lines
            return max(10, rows)
    except (ValueError, OSError):
        pass

    # Try $LINES env var (set by bash/zsh)
    lines_env = os.environ.get("LINES", "")
    if lines_env.isdigit():
        return max(10, int(lines_env))

    # Default fallback: effectively unlimited (no truncation when height unknown)
    return 9999


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


def _apply_status_filter(
    entries: list[dict[str, Any]],
    filter_status: str = "all",
    show_completed: bool = True,
) -> list[dict[str, Any]]:
    """Filter entries by interactive status filter (internal use only).

    Used by format_dashboard for tmux dashboard filter_status / show_completed.
    For CLI view filtering, use filter_specs() instead.

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


def filter_specs(specs: list[dict[str, Any]], mode: str) -> list[dict[str, Any]]:
    """Single public filter function called by every display mode.

    Delegates to _apply_view_filter. No other code should call _apply_view_filter
    directly — use this instead.

    mode: "default" | "all" | "running" | "recent:N"
    """
    return _apply_view_filter(specs, mode)


def sort_entries(
    entries: list[dict[str, Any]], sort_mode: str = "queue"
) -> list[dict[str, Any]] | list[tuple[dict[str, Any], int]]:
    """Sort entries by the given mode.

    For most modes, returns list[dict]. For "dag" mode, returns list[(dict, depth)].
    """
    if sort_mode == "queue":
        return _sort_by_queue(entries)
    elif sort_mode == "display":
        # Running first, then queued/failed/needs_review, then completed — preserves priority order within groups
        return _sort_entries_for_display(entries)
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

    # default: active specs + completed within 6h
    # Failed and canceled are never shown in default view (summary line shows failed count).
    now = datetime.now(timezone.utc)
    from datetime import timedelta
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
        elif status in ("completed", "failed"):
            ts = _last_ts(e)
            if ts is not None and ts >= cutoff_completed:
                result.append(e)
        # canceled: never shown in default view
    return result


# ─── Unified column widths ───────────────────────────────────────────────────
# These constants are shared by format_spec_row and render_status so that all
# display modes use identical column geometry.

_COL_MODE = 10    # "execute   " (8 + 2 space)
_COL_WORKER = 8   # "w-1     "  (6 + 2 space)
_COL_ITER = 8     # "10 (3m) "  (7 + 1 space)
_COL_TASKS = 8    # "6/6     " (count only — bar moved to its own line)
_COL_DEPS = 14    # dep info column
_COL_STATUS = 12  # "running" right-padded
_COL_FIXED = _COL_MODE + _COL_WORKER + _COL_ITER + _COL_TASKS + _COL_DEPS + _COL_STATUS

# Mode abbreviations for compact dashboard style
_MODE_ABBREV: dict[str, str] = {
    "execute": "exec",
    "challenge": "chal",
    "discover": "disc",
    "generate": "gen",
}
# Right-side fixed columns for compact layout: mode(5) + tasks(6) + iter(4) + quality(9) + worker(5) + spacing
_COL_COMPACT_RIGHT = 30

# Fixed columns for "recently finished" compact rows: tasks(8) + time(10) + status(12)
# Finished rows show count only — bar moved to its own line below spec name
_COL_FINISHED_TASKS = 8
_COL_FINISHED_TIME = 10
_COL_FINISHED_STATUS = 12
_COL_FINISHED_FIXED = _COL_FINISHED_TASKS + _COL_FINISHED_TIME + _COL_FINISHED_STATUS


def _format_finished_row(spec: dict[str, Any], columns: int, color: bool = True) -> str:
    """Format a recently finished spec as a compact row.

    Columns: ID+name (variable), task count, time ago, status.
    No mode/worker/iter columns — those are irrelevant for finished specs.
    For specs with more than 1 task, the caller emits a bar line below this row.
    """
    status = spec.get("status", "completed")
    qid = spec.get("id", "?")
    spec_path = spec.get("original_spec_path", spec.get("spec_path", ""))
    spec_name = os.path.splitext(os.path.basename(spec_path))[0] if spec_path else "?"

    col_spec = max(20, columns - _COL_FINISHED_FIXED)
    label = f"{qid}  {spec_name}"
    max_label = col_spec - 2
    if len(label) > max_label:
        label = label[: max_label - 1] + "\u2026"

    tasks_done = spec.get("tasks_done", 0)
    tasks_total = spec.get("tasks_total", 0)
    if tasks_total > 0:
        tasks_str = f"{tasks_done}/{tasks_total}"
    else:
        tasks_str = "-"

    time_ago = format_relative_time(spec.get("last_iteration_at"))

    row_text = (
        f"{label:<{col_spec}}"
        f"{tasks_str:<{_COL_FINISHED_TASKS}}"
        f"  {time_ago:<{_COL_FINISHED_TIME - 2}}"
        f"{status}"
    )

    if color:
        status_color = STATUS_COLORS.get(status, "")
        if status == "failed":
            row_text = f"{RED}{row_text}{NC}"
        elif status in ("completed", "canceled"):
            row_text = f"{DIM}{row_text}{NC}"
        elif status_color:
            row_text = f"{status_color}{row_text}{NC}"

    return row_text


def _format_spec_row_compact(
    spec: dict[str, Any],
    columns: int,
    color: bool = True,
    selected: bool = False,
) -> str:
    """Compact dashboard row: icon + label + mode_abbrev + tasks + quality.

    Used by format_spec_row(style='compact') which is called by render_status
    in compact mode (i.e., from format_dashboard).
    """
    status = spec.get("status", "queued")
    icon = STATUS_ICONS.get(status, "?")

    qid = spec.get("id", "?")
    spec_path = spec.get("original_spec_path", spec.get("spec_path", ""))
    spec_name = os.path.splitext(os.path.basename(spec_path))[0] if spec_path else "?"

    depth = spec.get("_dag_depth", 0)
    indent = "  " * depth

    # 4 = sel_marker(1) + icon(1) + space(1) + space(1)
    max_label_len = max(20, columns - _COL_COMPACT_RIGHT - 4)
    label = f"{indent}{qid} {spec_name}"
    if len(label) > max_label_len:
        label = label[: max_label_len - 1] + "\u2026"

    mode = spec.get("mode", "execute")
    mode_str = _MODE_ABBREV.get(mode, mode[:4])

    tasks_done = spec.get("tasks_done", 0)
    tasks_total = spec.get("tasks_total", 0)
    tasks_str = f"{tasks_done}/{tasks_total}" if tasks_total > 0 else "\u2014"

    iteration = spec.get("iteration", 0)
    iter_str = f"{iteration}i"

    telem = spec.get("_telemetry")
    quality_compact = "\u2014"
    if telem:
        scores = telem.get("quality_score_per_iteration", [])
        valid = [s for s in scores if s is not None]
        if valid:
            from lib.quality import grade as _grade

            latest = valid[-1]
            quality_compact = f"{_grade(latest)}({latest:.2f})"

    worker = spec.get("last_worker") or ""
    worker_str = f"  {worker}" if worker and status == "running" else ""

    sel_marker = "\u25b8" if selected else " "  # ▸ or space

    row = (
        f"{sel_marker}{icon} {label:<{max_label_len}}"
        f" {mode_str:<5}"
        f" {tasks_str:>5}"
        f" {iter_str:>3}"
        f"  {quality_compact:<9}"
        f"{worker_str}"
    )

    if color:
        status_color = STATUS_COLORS.get(status, "")
        if selected:
            if status in ("completed", "canceled"):
                row = f"{BOLD}{DIM}{row}{NC}"
            elif status_color:
                row = f"{BOLD}{status_color}{row}{NC}"
            else:
                row = f"{BOLD}{row}{NC}"
        elif status in ("completed", "canceled"):
            row = f"{DIM}{row}{NC}"
        # Non-selected active rows: no whole-row ANSI wrap so grep patterns work

    return row


def _get_iteration_elapsed(spec: dict[str, Any], queue_dir: str) -> str:
    """Return human-friendly elapsed time for the current running iteration.

    Reads started_at from the latest iteration file if present, otherwise
    falls back to last_iteration_at / first_running_at on the queue entry.
    Returns "" if no timing data is available.
    """
    if not queue_dir:
        return ""
    qid = spec.get("id", "")
    if not qid:
        return ""

    iteration = spec.get("iteration", 0)
    iter_path = Path(queue_dir) / f"{qid}.iteration-{iteration}.json"

    started_at_str = ""
    if iter_path.is_file():
        try:
            data = json.loads(iter_path.read_text(encoding="utf-8"))
            started_at_str = data.get("started_at", "")
        except (json.JSONDecodeError, OSError):
            pass

    if not started_at_str:
        started_at_str = (
            spec.get("last_iteration_at", "")
            or spec.get("first_running_at", "")
        )

    if not started_at_str:
        return ""

    try:
        started_at = datetime.fromisoformat(started_at_str.replace("Z", "+00:00"))
        elapsed = (datetime.now(timezone.utc) - started_at).total_seconds()
        if elapsed < 0:
            return ""
        if elapsed < 60:
            return f"{int(elapsed)}s"
        elif elapsed < 3600:
            return f"{int(elapsed // 60)}m"
        else:
            h = int(elapsed // 3600)
            m = int((elapsed % 3600) // 60)
            return f"{h}h{m}m" if m else f"{h}h"
    except Exception:
        return ""


def _get_current_task(spec: dict[str, Any], max_width: int = 60) -> str:
    """Return the first PENDING task heading from the spec file.

    Reads spec_path from the spec entry, parses task headings matching
    ``### t-N:`` and returns the first one whose status line is PENDING.
    Formatted as "t-N: {task title}" truncated to ~max_width chars.
    Returns "" if no pending task is found or the file cannot be read.
    """
    spec_path = spec.get("spec_path", "")
    if not spec_path:
        return ""
    try:
        content = Path(spec_path).read_text(encoding="utf-8")
    except OSError:
        return ""

    heading_re = re.compile(r"^### (t-\d+):\s*(.+)$", re.MULTILINE)
    lines = content.splitlines()
    line_index: dict[int, tuple[str, str]] = {}
    for m in heading_re.finditer(content):
        lineno = content[: m.start()].count("\n")
        line_index[lineno] = (m.group(1), m.group(2).strip())

    for lineno, (task_id, title) in sorted(line_index.items()):
        # Find the first non-empty line after the heading
        for next_line in lines[lineno + 1 :]:
            stripped = next_line.strip()
            if stripped:
                if stripped == "PENDING":
                    label = f"{task_id}: {title}"
                    if max_width > 0 and len(label) > max_width:
                        label = label[: max_width - 1] + "\u2026"
                    return label
                break  # non-empty, non-PENDING — not pending

    return ""


def format_spec_row(
    spec: dict[str, Any],
    columns: int,
    style: str = "default",
    color: bool = True,
    selected: bool = False,
    queue_dir: str = "",
) -> str:
    """Format a single spec entry as a display row at the given width.

    style "default": tabular layout (same as standard status output)
    style "dag":     same layout with depth-based indentation (reads spec["_dag_depth"])
    style "compact": compact dashboard layout with quality column and selection marker

    This is the single row formatter called by render_status for all display modes.
    """
    if style == "compact":
        return _format_spec_row_compact(spec, columns, color=color, selected=selected)
    col_spec = max(20, columns - _COL_FIXED)

    qid = spec.get("id", "?")
    spec_path = spec.get("original_spec_path", spec.get("spec_path", ""))
    spec_name = os.path.splitext(os.path.basename(spec_path))[0] if spec_path else "?"

    mode = spec.get("mode", "execute")
    status = spec.get("status", "queued")

    # DAG indentation: 2 spaces per depth level, with status icon prefix
    # Icons are only added for active statuses (running/queued/failed/needs_review).
    # Completed/canceled rows are dimmed as a whole row and have no icon prefix,
    # keeping the grep pattern "[▸▶✓✗] q-" consistent with the default "^q-" count.
    depth = spec.get("_dag_depth", 0) if style == "dag" else 0
    indent = "  " * depth
    if style == "dag" and status not in ("completed", "canceled"):
        icon = STATUS_ICONS.get(status, "")
        dag_prefix = f"{icon} " if icon else ""
    else:
        dag_prefix = ""
    label = f"{dag_prefix}{indent}{qid}  {spec_name}"
    max_label = col_spec - 2
    if len(label) > max_label:
        label = label[: max_label - 1] + "\u2026"

    worker = spec.get("last_worker") or "-"
    if status not in ("running", "requeued"):
        worker = "-"

    iteration = spec.get("iteration", 0)
    iter_str = f"{iteration}" if status != "queued" else "-"
    if status == "running":
        elapsed = _get_iteration_elapsed(spec, queue_dir)
        if elapsed:
            iter_str = f"{iter_str} ({elapsed})"

    tasks_done = spec.get("tasks_done", 0)
    tasks_total = spec.get("tasks_total", 0)
    if tasks_total > 0:
        tasks_str = f"{tasks_done}/{tasks_total}"
    else:
        tasks_str = "-"

    quality_str, _ = _get_quality_display(spec, color=False)
    quality_suffix = (
        f" [{quality_str}]"
        if quality_str not in ("\u2014", "-")
        else ""
    )

    deps_info = spec.get("_deps", {})
    blocked_by = deps_info.get("blocked_by", [])
    blocking = deps_info.get("blocking", [])
    unmet_deps = [dep_id for dep_id, dep_status in blocked_by if dep_status != "completed"]
    if unmet_deps:
        deps_str = f"\u23f3 {','.join(unmet_deps)}"  # ⏳
    elif blocking:
        deps_str = f"\u2192 {','.join(bid for bid, _ in blocking)}"  # →
    else:
        deps_str = "\u2014"
    if len(deps_str) > _COL_DEPS - 1:
        deps_str = deps_str[: _COL_DEPS - 2] + "\u2026"

    row_text = (
        f"{label:<{col_spec}}"
        f"{mode:<{_COL_MODE}}"
        f"{worker:<{_COL_WORKER}}"
        f"{iter_str:<{_COL_ITER}}"
        f"{tasks_str:<{_COL_TASKS}}"
        f"{deps_str:<{_COL_DEPS}}"
        f"{status}{quality_suffix}"
    )

    if color:
        if status in ("completed", "canceled"):
            row_text = f"{DIM}{row_text}{NC}"
        else:
            status_color = STATUS_COLORS.get(status, "")
            if status_color:
                row_prefix = (
                    f"{label:<{col_spec}}"
                    f"{mode:<{_COL_MODE}}"
                    f"{worker:<{_COL_WORKER}}"
                    f"{iter_str:<{_COL_ITER}}"
                    f"{tasks_str:<{_COL_TASKS}}"
                    f"{deps_str:<{_COL_DEPS}}"
                )
                row_text = f"{row_prefix}{_colorize(status + quality_suffix, status_color)}"

    return row_text


# ─── Minimal display symbols (narrow-terminal / minimal view) ────────────────
# Used by: _format_running_row_minimal(), _format_finished_row_minimal()
# Intentionally different from STATUS_ICONS: these are chosen for compactness
# in narrow terminals (e.g., tmux sidebar ≤40 chars). Smaller glyphs (▸ vs ▶,
# ○ vs ·) reduce visual weight. Includes blocked/assigning which the compact
# dashboard doesn't render separately.
_STATUS_SYMBOLS: dict[str, str] = {
    "running": "\u25b8",      # ▸ right-pointing small triangle
    "requeued": "\u21bb",     # ↻ clockwise arrow
    "completed": "\u2713",    # ✓ checkmark
    "failed": "\u2717",       # ✗ ballot X
    "queued": "\u25cb",       # ○ hollow circle
    "blocked": "\u2298",      # ⊘ circled slash
    "needs_review": "\u25c6", # ◆ diamond
    "canceled": "\u00b7",     # · middle dot
    "assigning": "\u25b8",    # ▸ same as running
}


def _format_running_row_minimal(
    spec: dict[str, Any],
    color: bool = True,
    queue_dir: str = "",
    columns: int = 0,
) -> list[str]:
    """Format an active spec as 2-3 minimal lines.

    Line 1: ▸ q-326  hex-module-rigor              7/9   8m
    Line 2:          → t-8: Add hex-doctor to startup
    Line 3:          ████████████████████████████████░░░░░░░░

    When ``columns`` is 0 or unset, falls back to a compact 30-char name width
    (original behavior). When set, the spec name column expands to fill the
    available terminal width and the current-task arrow uses the rest of the
    line instead of truncating at 60 chars.
    """
    status = spec.get("status", "queued")
    symbol = _STATUS_SYMBOLS.get(status, "\u25cb")
    color_code = STATUS_COLORS.get(status, "")

    qid = spec.get("id", "?")
    spec_path = spec.get("original_spec_path", spec.get("spec_path", ""))
    spec_name = os.path.splitext(os.path.basename(spec_path))[0] if spec_path else "?"
    # Strip redundant .spec suffix (filenames like foo.spec.md → foo.spec → foo)
    if spec_name.endswith(".spec"):
        spec_name = spec_name[:-5]

    tasks_done = spec.get("tasks_done", 0)
    tasks_total = spec.get("tasks_total", 0)
    tasks_str = f"{tasks_done}/{tasks_total}" if tasks_total > 0 else "\u2014"

    # Show E2E phase when marker file is present (written by worker._run_e2e_phase)
    if queue_dir and qid:
        e2e_marker = Path(queue_dir) / f"{qid}.e2e-phase"
        if e2e_marker.is_file():
            tasks_str = f"{tasks_done}/{tasks_total} \u00b7 E2E verifying..."

    elapsed = ""
    if status in ("running", "requeued", "assigning"):
        elapsed = _get_iteration_elapsed(spec, queue_dir)

    # Fixed overhead on line 1:
    #   "{symbol} " (2) + "{qid}  " (len(qid)+2) + "  {tasks_str}" (len+2)
    #   + optional "  {elapsed}" (len+2)
    fixed = 2 + len(qid) + 2 + 2 + len(tasks_str)
    if elapsed:
        fixed += 2 + len(elapsed)

    name_width = max(30, columns - fixed) if columns > 0 else 30
    if len(spec_name) > name_width:
        spec_name = spec_name[: name_width - 1] + "\u2026"

    line1 = f"{symbol} {qid}  {spec_name:<{name_width}}  {tasks_str}"
    if elapsed:
        line1 += f"  {elapsed}"

    if color and color_code:
        line1 = f"{color_code}{line1}{NC}"

    lines = [line1]

    # Lines 2 & 3 indented 9 spaces to align under the spec name
    indent = " " * 9

    # Line 2: current task arrow (running/requeued only)
    if status in ("running", "requeued"):
        # Indent (9) + "→ " (2) = 11 chars of overhead before task label
        task_max = columns - 11 if columns > 0 else 60
        current_task = _get_current_task(spec, max_width=max(60, task_max))
        if current_task:
            task_line = f"{indent}\u2192 {current_task}"
            if color:
                task_line = f"{DIM}{task_line}{NC}"
            lines.append(task_line)

    # Line 3: progress bar — scales with terminal width (indent=9 chars overhead)
    if tasks_total > 0:
        bar_width = max(40, columns - len(indent)) if columns > 0 else 40
        bar = _progress_bar(tasks_done, tasks_total, width=bar_width, color=color, status=status)
        bar_line = f"{indent}{bar}"
        lines.append(bar_line)

    return lines


def _format_finished_row_minimal(
    spec: dict[str, Any],
    color: bool = True,
    columns: int = 0,
) -> str:
    """Format a recently finished spec as a single minimal line.

    ✓ q-329  context-switching-research    5/6   8m ago
    ✗ q-318  hex-events-daemon-fix         5/5   50m ago

    When ``columns`` is 0 or unset, falls back to a compact 30-char name width
    (original behavior). When set, expands to fill the available width.
    """
    status = spec.get("status", "completed")
    symbol = _STATUS_SYMBOLS.get(status, "\u00b7")

    qid = spec.get("id", "?")
    spec_path = spec.get("original_spec_path", spec.get("spec_path", ""))
    spec_name = os.path.splitext(os.path.basename(spec_path))[0] if spec_path else "?"
    # Strip redundant .spec suffix (filenames like foo.spec.md → foo.spec → foo)
    if spec_name.endswith(".spec"):
        spec_name = spec_name[:-5]

    tasks_done = spec.get("tasks_done", 0)
    tasks_total = spec.get("tasks_total", 0)
    tasks_str = f"{tasks_done}/{tasks_total}" if tasks_total > 0 else "\u2014"

    time_ago = format_relative_time(spec.get("last_iteration_at"))

    # Fixed overhead:
    #   "{symbol} " (2) + "{qid}  " (len(qid)+2) + "  {tasks_str}" (len+2)
    #   + "  {time_ago}" (len+2)
    fixed = 2 + len(qid) + 2 + 2 + len(tasks_str) + 2 + len(time_ago)
    name_width = max(30, columns - fixed) if columns > 0 else 30
    if len(spec_name) > name_width:
        spec_name = spec_name[: name_width - 1] + "\u2026"

    line = f"{symbol} {qid}  {spec_name:<{name_width}}  {tasks_str}  {time_ago}"

    if color:
        if status == "failed":
            line = f"{RED}{line}{NC}"
        elif status in ("completed", "canceled"):
            line = f"{DIM}{line}{NC}"
        else:
            color_code = STATUS_COLORS.get(status, "")
            if color_code:
                line = f"{color_code}{line}{NC}"

    return line


def render_status(
    specs: list[dict[str, Any]],
    sort: str = "queue",
    watch: bool = False,
    columns: int | None = None,
    color: bool = True,
    summary: dict[str, Any] | None = None,
    workers: list[Any] | None = None,
    total_count: int | None = None,
    view_mode: str = "default",
    style: str = "default",
    selected_row: int = -1,
    emit_queue_ids: bool = False,
    queue_dir: str = "",
    verbose: bool = False,
) -> str:
    """Unified status renderer — single entry point for all display modes.

    Takes pre-filtered specs (use filter_specs() first), sorts them, and
    renders header + rows + summary footer.

    style "default" or "dag": full tabular layout with column headers.
    style "compact": compact dashboard layout (no column headers, quality
        column, selection marker). Called by format_dashboard.

    For watch mode, callers should call this in a loop and overwrite the
    screen with cursor-home (no clear) between frames.
    """
    if columns is None:
        columns = get_terminal_width()
    term_w = max(80, columns)

    if summary is None:
        summary = {}
    if workers is None:
        workers = []
    if total_count is None:
        total_count = len(specs)

    lines: list[str] = []

    # ── Compact mode (dashboard) ──────────────────────────────────────────────
    if style == "compact":
        # Truly empty queue — no specs at all, not just filtered
        if not specs and total_count == 0:
            lines.append(" No specs in queue. Ready to dispatch.")
            _tw = len(workers)
            if _tw:
                lines.append(f" Workers: 0/{_tw} idle")
            lines.append("")
            lines.append(" Quick start:")
            lines.append("   boi dispatch my-spec.md          Dispatch a spec file")
            lines.append(
                '   boi do "build a REST API"        Describe what you want (uses AI)'
            )
            lines.append("   boi status                       Check progress")
            lines.append("   boi --help                       See all commands")
            return "\n".join(lines)

        # Sort — DAG mode embeds _dag_depth into each entry dict
        _sorted = sort_entries(specs, sort)
        if sort == "dag":
            _display: list[dict[str, Any]] = []
            for _e, _d in _sorted:  # type: ignore[misc]
                _e["_dag_depth"] = _d
                _display.append(_e)
        else:
            _display = _sorted  # type: ignore[assignment]

        _visible_ids: list[str] = []
        for _idx, _entry in enumerate(_display):
            lines.append(
                format_spec_row(
                    _entry,
                    term_w,
                    style="compact",
                    color=color,
                    selected=(_idx == selected_row),
                )
            )
            _visible_ids.append(_entry.get("id", ""))

        # Quality alerts
        for _al in _get_quality_alerts(specs, color):
            lines.append(_al)

        # Summary line (compact: "Workers: N/M busy | X running, Y queued")
        _tw = len(workers)
        _run = summary.get("running", 0)
        _q = summary.get("queued", 0) + summary.get("requeued", 0)
        _fail = summary.get("failed", 0)
        _done = summary.get("completed", 0)
        _parts: list[str] = []
        if _tw:
            _parts.append(f"Workers: {_run}/{_tw} busy")
        _cnts = ", ".join(
            p
            for p in [
                f"{_run} running" if _run else "",
                f"{_q} queued" if _q else "",
                f"{_fail} failed" if _fail else "",
                f"{_done} completed" if _done else "",
            ]
            if p
        )
        if _cnts:
            _parts.append(_cnts)
        lines.append(" | ".join(_parts) if _parts else f"Queue: {summary.get('total', 0)}")

        # Showing N of M hint
        _shown = len(specs)
        if view_mode != "all" and _shown < total_count:
            if view_mode == "default":
                _hint = (
                    f"Showing {_shown} of {total_count} specs"
                    " (running + last 6h). Use --all for full history."
                )
            else:
                _hint = f"Showing {_shown} of {total_count} specs. Use --all to see all."
            if color:
                _hint = f"{DIM}{_hint}{NC}"
            lines.append(_hint)

        # Machine-readable footer for dashboard.sh
        if emit_queue_ids and _visible_ids:
            lines.append(f"__QUEUE_IDS__:{','.join(_visible_ids)}")

        return "\n".join(lines)
    # ── End compact mode ──────────────────────────────────────────────────────

    if not specs:
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

    # Split into active (running/queued) and finished (completed/failed/canceled)
    _active_statuses = {"running", "requeued", "queued", "needs_review", "assigning"}
    active_specs = [e for e in specs if e.get("status", "") in _active_statuses]
    finished_specs = [e for e in specs if e.get("status", "") not in _active_statuses]

    # Sort finished by last_iteration_at descending (most recent first)
    finished_specs = _sort_by_recent(finished_specs)

    has_active = bool(active_specs)
    has_finished = bool(finished_specs)

    col_spec = max(20, term_w - _COL_FIXED)
    row_style = "dag" if sort == "dag" else "default"
    first_running_id = ""

    # ── RUNNING section ───────────────────────────────────────────────────────
    if has_active:
        running_header = "RUNNING"
        if color:
            if verbose:
                running_header = f"{BOLD}{running_header}{NC}"
            else:
                running_header = f"{DIM}{running_header}{NC}"
        lines.append(running_header)

        sorted_active = sort_entries(active_specs, sort)
        if sort == "dag":
            display_active: list[dict[str, Any]] = []
            for entry, depth in sorted_active:  # type: ignore[misc]
                entry["_dag_depth"] = depth
                display_active.append(entry)
        else:
            display_active = sorted_active  # type: ignore[assignment]

        if verbose:
            col_header = (
                f"{'SPEC':<{col_spec}}"
                f"{'MODE':<{_COL_MODE}}"
                f"{'WORKER':<{_COL_WORKER}}"
                f"{'ITER':<{_COL_ITER}}"
                f"{'TASKS':<{_COL_TASKS}}"
                f"{'Deps':<{_COL_DEPS}}"
                f"{'STATUS'}"
            )
            if color:
                col_header = f"{BOLD}{col_header}{NC}"
            lines.append(col_header)

            sep = "\u2500" * term_w
            if color:
                sep = f"{DIM}{sep}{NC}"
            lines.append(sep)

            generate_details: list[tuple[int, list[str]]] = []

            for entry in display_active:
                entry_status = entry.get("status", "queued")
                if not first_running_id and entry_status == "running":
                    first_running_id = entry.get("id", "")

                row = format_spec_row(entry, term_w, style=row_style, color=color, queue_dir=queue_dir)
                lines.append(row)

                if entry_status == "running":
                    current_task = _get_current_task(entry)
                    if current_task:
                        task_line = f"       \u2192 {current_task}"
                        if color:
                            task_line = f"{DIM}{task_line}{NC}"
                        lines.append(task_line)
                    entry_done = entry.get("tasks_done", 0)
                    entry_total = entry.get("tasks_total", 0)
                    if entry_total > 0:
                        bar_str = _progress_bar(entry_done, entry_total, width=40, color=False, status=entry_status)
                        bar_line = f"       {bar_str}  {entry_done}/{entry_total}"
                        if color:
                            bar_line = f"{DIM}{bar_line}{NC}"
                        lines.append(bar_line)

                gen_detail = _get_generate_detail(entry)
                if gen_detail:
                    generate_details.append((len(lines), gen_detail))

            for insert_idx, detail_lines in reversed(generate_details):
                for i, dl in enumerate(detail_lines):
                    lines.insert(insert_idx + i, dl)
        else:
            # Minimal format: symbol + ID + name + tasks + elapsed, then task arrow + bar
            for entry in display_active:
                entry_status = entry.get("status", "queued")
                if not first_running_id and entry_status == "running":
                    first_running_id = entry.get("id", "")
                for row_line in _format_running_row_minimal(entry, color=color, queue_dir=queue_dir, columns=term_w):
                    lines.append(row_line)

        lines.append("")

    # ── RECENTLY FINISHED section ─────────────────────────────────────────────
    if has_finished:
        finished_header = "RECENTLY FINISHED"
        if color:
            finished_header = f"{DIM}{finished_header}{NC}"
        lines.append(finished_header)

        _finished_cap = 8
        _cap_active = view_mode != "all"
        _display_finished = finished_specs[:_finished_cap] if _cap_active else finished_specs

        # Height-based cap: if terminal height is limited, truncate RECENTLY FINISHED
        # to keep the footer always visible. Reserve 3 lines for footer area
        # (footer line + optional hint + optional showing hint), and 3 for section
        # overhead (header already appended above + blank above + trailing blank).
        _term_h = _get_terminal_height()
        _height_cap = max(0, _term_h - len(lines) - 6)
        if _height_cap < len(_display_finished):
            _display_finished = finished_specs[:_height_cap]

        _hidden_count = len(finished_specs) - len(_display_finished)

        for entry in _display_finished:
            done = entry.get("tasks_done", 0)
            total = entry.get("tasks_total", 0)
            if verbose:
                lines.append(_format_finished_row(entry, term_w, color=color))
                if total > 1:
                    bar_str = _progress_bar(done, total, width=40, color=False, status=entry.get("status", "completed"))
                    bar_line = f"       {bar_str}  {done}/{total}"
                    if color:
                        bar_line = f"{DIM}{bar_line}{NC}"
                    lines.append(bar_line)
            else:
                lines.append(_format_finished_row_minimal(entry, color=color, columns=term_w))

        if _hidden_count > 0:
            _more_line = f"  ... and {_hidden_count} more completed in last 6h"
            if color:
                _more_line = f"{DIM}{_more_line}{NC}"
            lines.append(_more_line)

        lines.append("")

    alert_lines = _get_quality_alerts(specs, color)
    if alert_lines:
        for al in alert_lines:
            lines.append(al)
        lines.append("")

    blocked_lines = _get_blocked_specs_display(active_specs, color)
    if blocked_lines:
        for bl in blocked_lines:
            lines.append(bl)
        lines.append("")

    # Summary line — compact: "5/5 busy | 5▸ 3○ 29✗ 256✓"
    total_workers = len(workers)
    running = summary.get("running", 0)
    queued = summary.get("queued", 0) + summary.get("requeued", 0)
    completed = summary.get("completed", 0)
    needs_review = summary.get("needs_review", 0)
    failed = summary.get("failed", 0)

    parts: list[str] = []
    if total_workers:
        parts.append(f"{running}/{total_workers} busy")

    stat_parts: list[str] = []
    if running:
        stat_parts.append(f"{running}\u25b8")    # N▸ running
    if queued:
        stat_parts.append(f"{queued}\u25cb")     # N○ queued
    if needs_review:
        stat_parts.append(f"{needs_review}\u25c6")  # N◆ needs_review
    if failed:
        stat_parts.append(f"{failed}\u2717")     # N✗ failed
    if completed:
        stat_parts.append(f"{completed}\u2713")  # N✓ completed

    if stat_parts:
        parts.append("  ".join(stat_parts))

    lines.append(" | ".join(parts) if parts else f"Total: {summary.get('total', 0)}")

    if first_running_id:
        hint = f"Run 'boi log {first_running_id}' to see worker output"
        if color:
            hint = f"{DIM}{hint}{NC}"
        lines.append(hint)

    shown_count = len(specs)
    if view_mode != "all" and shown_count < total_count:
        if view_mode == "default":
            showing_hint = (
                f"Showing {shown_count} of {total_count} specs"
                " (running + last 6h). Use --all for full history."
            )
        else:
            showing_hint = (
                f"Showing {shown_count} of {total_count} specs."
                " Use --all to see all."
            )
        if color:
            showing_hint = f"{DIM}{showing_hint}{NC}"
        lines.append(showing_hint)

    return "\n".join(lines)


def format_queue_table(
    status_data: dict[str, Any],
    color: bool = True,
    width: int | None = None,
    view_mode: str = "default",
    sort: str = "display",
    verbose: bool = False,
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
    queue_dir = status_data.get("queue_dir", "")
    total_count = len(all_entries)

    entries = filter_specs(all_entries, view_mode)
    columns = max(80, width if width is not None else get_terminal_width())

    return render_status(
        entries,
        sort=sort,
        watch=False,
        columns=columns,
        color=color,
        summary=summary,
        workers=workers,
        total_count=total_count,
        view_mode=view_mode,
        queue_dir=queue_dir,
        verbose=verbose,
    )


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

    cost = telemetry.get("cost")
    if cost:
        total_cost = cost.get("total_cost_usd")
        if total_cost is not None:
            lines.append(f"Cost: ${total_cost}")

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
# Used by: format_compact_row() (compact dashboard) and DAG-style table rows.
# Intentionally different from _STATUS_SYMBOLS: these are chosen for wider
# terminal displays where slightly bolder glyphs (▶ vs ▸, · vs ○) look better
# in a columnar table. Does not need blocked/assigning — the compact dashboard
# shows those statuses via color and label only.
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
    view_mode: str = "default",
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
    all_entries = status_data.get("entries", [])
    total_all = len(all_entries)  # total before any filter — used for "Showing N of M"
    entries = filter_specs(all_entries, view_mode)
    summary = status_data.get("summary", {})
    workers = status_data.get("workers", [])

    term_w = width if width is not None else get_terminal_width()
    dash_w = max(80, term_w)

    # ── Dashboard-specific header bar (═══ BOI ═══ timestamp ══) ────────────
    now = datetime.now()
    time_str = now.strftime("%H:%M")
    header_text = "\u2550\u2550\u2550 BOI "
    indicators: list[str] = []
    if filter_status != "all":
        indicators.append(f"filter: {filter_status}")
    if sort_mode != "queue":
        indicators.append(f"sort: {sort_mode}")
    if not show_completed:
        indicators.append("completed: hidden")
    if indicators:
        header_text += " [" + "] [".join(indicators) + "] "
    right = f" {time_str} \u2550\u2550"
    fill_len = max(1, dash_w - len(header_text) - len(right))
    header_line = header_text + ("\u2550" * fill_len) + right
    if color:
        header_line = f"{BOLD}{header_line}{NC}"

    # Handle empty queue (after view filter, before secondary filter)
    if not entries:
        lines: list[str] = [header_line]
        lines.append(" No specs in queue. Ready to dispatch.")
        if workers:
            lines.append(f" Workers: 0/{len(workers)} idle")
        lines.append("")
        lines.append(" Quick start:")
        lines.append("   boi dispatch my-spec.md          Dispatch a spec file")
        lines.append(
            '   boi do "build a REST API"        Describe what you want (uses AI)'
        )
        lines.append("   boi status                       Check progress")
        lines.append("   boi --help                       See all commands")
        return "\n".join(lines)

    # Apply secondary interactive filter (tmux dashboard filter_status / show_completed)
    if filter_status != "all" or not show_completed:
        entries = _apply_status_filter(
            entries,
            filter_status=filter_status,
            show_completed=show_completed,
        )

    # Clamp selected_row to valid range
    shown_count = len(entries)
    if shown_count > 0:
        selected_row = max(0, min(selected_row, shown_count - 1))
    else:
        selected_row = 0

    # Delegate core table output to unified renderer
    body = render_status(
        entries,
        sort=sort_mode,
        watch=False,
        columns=dash_w,
        color=color,
        summary=summary,
        workers=workers,
        total_count=total_all,
        view_mode=view_mode,
        style="compact",
        selected_row=selected_row,
        emit_queue_ids=True,
    )

    return header_line + "\n" + body


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


# ─── Sidebar compact view (40-char max) ────────────────────────────────────────


def _sidebar_spec_name(entry: dict[str, Any], max_len: int = 16) -> str:
    """Extract a short display name from spec path."""
    path = entry.get("original_spec_path") or entry.get("spec_path", "")
    name = os.path.basename(path)
    # Strip common suffixes
    for suffix in (".spec.md", ".md"):
        if name.endswith(suffix):
            name = name[: -len(suffix)]
            break
    if len(name) > max_len:
        name = name[:max_len]
    return name


def _sidebar_progress_bar(done: int, total: int, width: int = 8) -> str:
    """Render a compact block progress bar."""
    if total <= 0:
        return "░" * width
    filled = round(done / total * width)
    filled = max(0, min(width, filled))
    return "█" * filled + "░" * (width - filled)


def _sidebar_next_task(entry: dict[str, Any]) -> str:
    """Return the next PENDING task id, or empty string."""
    pre = entry.get("pre_iteration_tasks", "{}")
    if isinstance(pre, str):
        try:
            pre = json.loads(pre)
        except Exception:
            return ""
    return next((t for t, s in pre.items() if s == "PENDING"), "")


def format_sidebar(
    status_data: dict[str, Any],
    color: bool = True,
    max_width: int = 40,
) -> str:
    """Format queue status as a minimal sidebar view (≤40 chars wide).

    Output example (max_width=40):
        BOI: 2 running, 1 queued
        ▸ hex-events-fix  ████░░░░ 2/5 t-3
        ▸ status-compact  ░░░░░░░░ 0/3 t-1
        · progress-bars   queued

        Done:
          smoke-test       3m ago
          upgrade-audit    15m ago
    """
    entries = status_data.get("entries", [])
    summary = status_data.get("summary", {})

    running_count = summary.get("running", 0) + summary.get("requeued", 0)
    queued_count = summary.get("queued", 0)
    failed_count = summary.get("failed", 0)
    needs_review_count = summary.get("needs_review", 0)
    pending_count = queued_count + failed_count + needs_review_count

    lines: list[str] = []

    # ── Header ──────────────────────────────────────────────────────────────
    if running_count == 0 and pending_count == 0:
        header = "BOI: idle"
    else:
        parts: list[str] = []
        if running_count:
            parts.append(f"{running_count} running")
        if pending_count:
            parts.append(f"{pending_count} queued")
        header = "BOI: " + ", ".join(parts)

    if color:
        lines.append(f"{BOLD}{header[:max_width]}{NC}")
    else:
        lines.append(header[:max_width])

    # ── Active specs (running + queued) ────────────────────────────────────
    active_statuses = {"running", "requeued", "queued", "needs_review", "failed"}
    active = [e for e in entries if e.get("status", "") in active_statuses]

    # Sort: running first, then queued
    active.sort(
        key=lambda e: 0 if e.get("status", "") in ("running", "requeued") else 1
    )

    # Separate running from queued so we can cap queued display
    running_entries = [e for e in active if e.get("status", "") in ("running", "requeued")]
    queued_entries = [e for e in active if e.get("status", "") not in ("running", "requeued")]
    max_queued_shown = 3

    for entry in running_entries:
        name = _sidebar_spec_name(entry, max_len=15)
        done = entry.get("tasks_done", 0)
        total = entry.get("tasks_total", 0)
        bar = _sidebar_progress_bar(done, total, width=8)
        fraction = f"{done}/{total}"
        next_task = _sidebar_next_task(entry)
        # Layout: "▸ <name15> <bar8> <frac5> <task>"
        # Fixed: 2 + 15 + 1 + 8 + 1 + 5 + 1 + 4 = 37 max
        name_padded = name.ljust(15)[:15]
        frac_padded = fraction.ljust(5)[:5]
        task_part = next_task[:4] if next_task else ""
        row = f"▸ {name_padded} {bar} {frac_padded} {task_part}".rstrip()
        row = row[:max_width]
        color_code = STATUS_COLORS.get("running", "")
        if color and color_code:
            lines.append(f"{color_code}{row}{NC}")
        else:
            lines.append(row)

    for entry in queued_entries[:max_queued_shown]:
        name = _sidebar_spec_name(entry, max_len=15)
        name_padded = name.ljust(15)[:15]
        row = f"· {name_padded} queued"
        row = row[:max_width]
        color_code = STATUS_COLORS.get("queued", "")
        if color and color_code:
            lines.append(f"{color_code}{row}{NC}")
        else:
            lines.append(row)

    hidden = len(queued_entries) - max_queued_shown
    if hidden > 0:
        row = f"  +{hidden} more queued"
        row = row[:max_width]
        if color:
            lines.append(f"{DIM}{row}{NC}")
        else:
            lines.append(row)

    # ── Recently completed ─────────────────────────────────────────────────
    completed = [
        e
        for e in entries
        if e.get("status", "") in ("completed", "canceled")
    ]
    # Sort by last_iteration_at descending, take 3
    def _sort_key(e: dict[str, Any]) -> str:
        return e.get("last_iteration_at") or e.get("submitted_at") or ""

    completed.sort(key=_sort_key, reverse=True)
    recent = completed[:3]

    if recent:
        lines.append("")
        if color:
            lines.append(f"{DIM}Done:{NC}")
        else:
            lines.append("Done:")
        for entry in recent:
            name = _sidebar_spec_name(entry, max_len=16)
            time_ago = format_relative_time(
                entry.get("last_iteration_at") or entry.get("submitted_at")
            )
            # Layout: "  <name16>  <time>"  max ~38 chars
            name_padded = name.ljust(16)[:16]
            row = f"  {name_padded} {time_ago}"
            row = row[:max_width]
            if color:
                lines.append(f"{DIM}{row}{NC}")
            else:
                lines.append(row)

    return "\n".join(lines)
