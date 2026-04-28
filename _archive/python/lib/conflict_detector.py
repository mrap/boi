# conflict_detector.py — File-level conflict detection for BOI specs.
#
# Automatically detects when two specs target the same files, enabling
# smart DAG planning. Specs that don't conflict can run in parallel;
# specs that do are automatically serialized via blocked_by.
#
# Three main entry points:
#   - extract_target_paths(spec_path) — parse a spec and extract file paths
#   - detect_conflicts(queue_dir) — find all conflicting spec pairs
#   - should_block(queue_dir, new_spec_id) — check if a new spec conflicts
#     with any running spec

import os
import re
from pathlib import Path
from typing import Any


def extract_target_paths(spec_path: str) -> set[str]:
    """Parse a spec.md and extract all file paths from Spec: and Files: sections.

    Looks for paths in:
    - **Spec:** sections (backtick-quoted paths like `~/boi-oss/lib/foo.py`)
    - **Files:** sections (lines listing file paths)
    - Inline code references that look like file paths

    Returns a set of normalized file paths.
    """
    try:
        content = Path(spec_path).read_text(encoding="utf-8")
    except OSError:
        return set()

    paths: set[str] = set()

    # Pattern: backtick-quoted paths that look like file paths
    # Matches `~/path/to/file.ext` or `/path/to/file.ext` or `path/to/file.ext`
    backtick_pattern = re.compile(r"`(~?/?[\w./-]+\.\w+)`")

    # Pattern: backtick-quoted directory paths (ending with /)
    dir_pattern = re.compile(r"`(~?/?[\w./-]+/)`")

    in_spec_section = False
    in_files_section = False

    for line in content.splitlines():
        stripped = line.strip()

        # Track if we're in a **Spec:** or **Files:** section
        if stripped.startswith("**Spec:**"):
            in_spec_section = True
            in_files_section = False
        elif stripped.startswith("**Files:**"):
            in_files_section = True
            in_spec_section = False
        elif stripped.startswith("**Verify:**") or stripped.startswith(
            "**Self-evolution:**"
        ):
            in_spec_section = False
            in_files_section = False
        elif stripped.startswith("### t-"):
            # New task boundary
            in_spec_section = False
            in_files_section = False

        if in_spec_section or in_files_section:
            # Extract backtick-quoted file paths
            for match in backtick_pattern.finditer(line):
                path = match.group(1)
                paths.add(_normalize_path(path))

            # Extract backtick-quoted directory paths
            for match in dir_pattern.finditer(line):
                path = match.group(1)
                paths.add(_normalize_path(path))

        # In Files: sections, also pick up bare path lines (- path/to/file.ext)
        if in_files_section:
            bare = stripped.lstrip("- ").strip()
            if bare and "/" in bare and not bare.startswith("#"):
                # Filter out things that are clearly not paths
                if not bare.startswith("http") and not " " in bare:
                    paths.add(_normalize_path(bare))

    return paths


def _normalize_path(path: str) -> str:
    """Normalize a path for comparison.

    Expands ~ and resolves to a canonical form, but keeps relative paths
    relative (just normalizes separators and removes trailing slashes).
    """
    path = path.strip().rstrip("/")
    if path.startswith("~/"):
        path = os.path.expanduser(path)
    return os.path.normpath(path)


def detect_conflicts(queue_dir: str) -> list[dict[str, Any]]:
    """Find all conflicting spec pairs among queued/running specs.

    Returns a list of dicts, each with:
        spec_a: queue ID of first spec
        spec_b: queue ID of second spec
        shared_files: list of files both specs reference
    """
    from lib.queue import get_queue

    entries = get_queue(queue_dir)
    active_statuses = {"queued", "requeued", "running"}

    # Build path sets for active, non-isolated specs
    spec_paths: dict[str, set[str]] = {}
    for entry in entries:
        if entry.get("status") not in active_statuses:
            continue
        if entry.get("worktree_isolate"):
            continue  # Isolated specs don't conflict
        spec_path = entry.get("spec_path", "")
        if spec_path:
            spec_paths[entry["id"]] = extract_target_paths(spec_path)

    # Compare all pairs
    conflicts: list[dict[str, Any]] = []
    ids = sorted(spec_paths.keys())
    for i, id_a in enumerate(ids):
        for id_b in ids[i + 1 :]:
            shared = spec_paths[id_a] & spec_paths[id_b]
            if shared:
                conflicts.append(
                    {
                        "spec_a": id_a,
                        "spec_b": id_b,
                        "shared_files": sorted(shared),
                    }
                )

    return conflicts


def should_block(queue_dir: str, new_spec_id: str) -> list[dict[str, Any]]:
    """Check if a new spec conflicts with any running spec.

    Called during dispatch for non-isolated specs. Checks if the new spec's
    target files overlap with any currently running spec's target files.

    Returns a list of dicts with:
        blocking_id: queue ID of the conflicting running spec
        shared_files: list of files that overlap
    """
    from lib.queue import get_entry, get_queue

    new_entry = get_entry(queue_dir, new_spec_id)
    if new_entry is None:
        return []

    # Don't check isolated specs
    if new_entry.get("worktree_isolate"):
        return []

    new_spec_path = new_entry.get("spec_path", "")
    if not new_spec_path:
        return []

    new_paths = extract_target_paths(new_spec_path)
    if not new_paths:
        return []

    entries = get_queue(queue_dir)
    blockers: list[dict[str, Any]] = []

    for entry in entries:
        if entry["id"] == new_spec_id:
            continue
        if entry.get("status") != "running":
            continue
        if entry.get("worktree_isolate"):
            continue  # Isolated specs don't conflict

        entry_spec_path = entry.get("spec_path", "")
        if not entry_spec_path:
            continue

        entry_paths = extract_target_paths(entry_spec_path)
        shared = new_paths & entry_paths
        if shared:
            blockers.append(
                {
                    "blocking_id": entry["id"],
                    "shared_files": sorted(shared),
                }
            )

    return blockers


def check_conflicts_before_dequeue(queue_dir: str, candidate_id: str) -> list[str]:
    """Re-check conflicts for a candidate spec before dequeuing.

    Called by pick_next_spec() to ensure a spec that was unblocked at
    dispatch time hasn't become conflicted with a spec that started
    running since then.

    Returns a list of conflicting running spec IDs, or empty if no conflicts.
    """
    blockers = should_block(queue_dir, candidate_id)
    return [b["blocking_id"] for b in blockers]
