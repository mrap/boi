# project.py — Project CRUD for BOI.
#
# Projects live at ~/.boi/projects/{name}/. Each project has:
#   - project.json: metadata (name, description, created_at, etc.)
#   - context.md: freeform context for workers
#
# All writes are atomic (.tmp + os.rename).

import json
import os
import re
import shutil
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional


PROJECTS_DIR = os.path.expanduser("~/.boi/projects")
_NAME_RE = re.compile(r"^[a-zA-Z0-9][a-zA-Z0-9-]*$")


def _atomic_write_json(path: str, data: dict[str, Any]) -> None:
    """Write a JSON dict to path atomically via .tmp + os.rename."""
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=2, sort_keys=False)
        f.write("\n")
    os.rename(tmp, path)


def _validate_name(name: str) -> None:
    """Validate project name: alphanumeric + hyphens, no spaces."""
    if not name or not _NAME_RE.match(name):
        raise ValueError(
            f"Invalid project name '{name}': "
            "must be alphanumeric with hyphens only (no spaces)"
        )


def create_project(name: str, description: str = "") -> dict[str, Any]:
    """Create a new project directory with metadata.

    Returns the project dict.
    Raises ValueError if name is invalid or project already exists.
    """
    _validate_name(name)

    project_dir = os.path.join(PROJECTS_DIR, name)
    if os.path.exists(project_dir):
        raise ValueError(f"Project '{name}' already exists")

    os.makedirs(project_dir, exist_ok=True)

    project = {
        "name": name,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "description": description,
        "default_priority": 100,
        "default_max_iter": 30,
        "tags": [],
    }

    _atomic_write_json(os.path.join(project_dir, "project.json"), project)

    # Create empty context.md
    context_path = os.path.join(project_dir, "context.md")
    tmp = context_path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        f.write(f"# {name} Context\n")
    os.rename(tmp, context_path)

    return project


def list_projects() -> list[dict[str, Any]]:
    """List all projects with spec counts from the queue.

    Returns list of project dicts, each with an added 'spec_count' field.
    """
    projects_path = Path(PROJECTS_DIR)
    if not projects_path.is_dir():
        return []

    # Count specs per project from the queue
    queue_dir = os.path.expanduser("~/.boi/queue")
    project_spec_counts: dict[str, int] = {}
    queue_path = Path(queue_dir)
    if queue_path.is_dir():
        for f in queue_path.iterdir():
            if not f.name.startswith("q-") or not f.name.endswith(".json"):
                continue
            if ".telemetry" in f.name or ".iteration-" in f.name:
                continue
            try:
                data = json.loads(f.read_text(encoding="utf-8"))
                proj = data.get("project")
                if proj:
                    project_spec_counts[proj] = project_spec_counts.get(proj, 0) + 1
            except (json.JSONDecodeError, OSError):
                continue

    results = []
    for entry in sorted(projects_path.iterdir()):
        if not entry.is_dir():
            continue
        pjson = entry / "project.json"
        if not pjson.is_file():
            continue
        try:
            data = json.loads(pjson.read_text(encoding="utf-8"))
            data["spec_count"] = project_spec_counts.get(
                data.get("name", entry.name), 0
            )
            results.append(data)
        except (json.JSONDecodeError, OSError):
            continue

    return results


def get_project(name: str) -> Optional[dict[str, Any]]:
    """Read and return project metadata, or None if not found."""
    pjson = os.path.join(PROJECTS_DIR, name, "project.json")
    if not os.path.isfile(pjson):
        return None
    try:
        with open(pjson, "r", encoding="utf-8") as f:
            return json.load(f)
    except (json.JSONDecodeError, OSError):
        return None


def get_project_context(name: str) -> str:
    """Read and return context.md contents, or empty string."""
    ctx = os.path.join(PROJECTS_DIR, name, "context.md")
    if not os.path.isfile(ctx):
        return ""
    try:
        with open(ctx, "r", encoding="utf-8") as f:
            return f.read()
    except OSError:
        return ""


def delete_project(name: str) -> None:
    """Remove the project directory. Does NOT cancel running specs."""
    project_dir = os.path.join(PROJECTS_DIR, name)
    if not os.path.isdir(project_dir):
        raise ValueError(f"Project '{name}' not found")
    shutil.rmtree(project_dir)
