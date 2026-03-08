# conftest.py — Shared test fixtures and helpers for BOI tests.
#
# Provides reusable test infrastructure for all BOI test modules.
# Works with unittest (stdlib) directly. If pytest is installed,
# these also work as pytest fixtures via standard conftest.py discovery.
#
# Fixtures / helpers:
#   - BoiTestCase: base unittest.TestCase with full ~/.boi/ temp tree
#   - make_boi_state(): create a temp dir mimicking ~/.boi/
#   - make_spec(): create a spec.md file with configurable task counts
#   - make_queue_entry(): create a queue JSON file
#   - make_iteration_file(): create an iteration-N.json file
#   - make_telemetry_file(): create a telemetry.json file
#   - make_hook_script(): create an executable hook script
#   - make_worker_pid(): write a PID file simulating a running worker
#   - SAMPLE_SPECS: dict of realistic spec content strings
#
# All helpers use temp directories cleaned up after tests.
# No real Claude calls. No real worktrees.

import json
import os
import stat
import sys
import tempfile
import textwrap
import unittest
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

# Add parent directory to path so we can import lib modules
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))


# ── Sample Spec Content ──────────────────────────────────────────────────


SAMPLE_SPECS = {
    "three_pending": textwrap.dedent("""\
        # Test Spec — Three Pending

        ## Tasks

        ### t-1: Set up database schema
        PENDING

        **Spec:** Create the initial database tables for user accounts.

        **Verify:** python3 -c "print('schema ok')"

        ### t-2: Implement user registration
        PENDING

        **Spec:** Add registration endpoint with email validation.

        **Verify:** python3 -c "print('registration ok')"

        ### t-3: Add authentication middleware
        PENDING

        **Spec:** JWT-based auth middleware for protected routes.

        **Verify:** python3 -c "print('auth ok')"
    """),
    "mixed_status": textwrap.dedent("""\
        # Test Spec — Mixed Status

        ## Tasks

        ### t-1: First task
        DONE

        **Spec:** Already completed.

        **Verify:** echo "done"

        ### t-2: Second task
        PENDING

        **Spec:** Still needs work.

        **Verify:** echo "ok"

        ### t-3: Third task
        PENDING

        **Spec:** Also needs work.

        **Verify:** echo "ok"

        ### t-4: Skipped task
        SKIPPED

        **Spec:** Not relevant anymore.

        **Verify:** echo "n/a"
    """),
    "all_done": textwrap.dedent("""\
        # Test Spec — All Done

        ## Tasks

        ### t-1: First task
        DONE

        **Spec:** Completed.

        **Verify:** echo "done"

        ### t-2: Second task
        DONE

        **Spec:** Also completed.

        **Verify:** echo "done"
    """),
    "single_pending": textwrap.dedent("""\
        # Test Spec — Single Pending

        ## Tasks

        ### t-1: Only task
        PENDING

        **Spec:** Do the thing.

        **Verify:** echo "ok"
    """),
    "self_evolving": textwrap.dedent("""\
        # Test Spec — Self-Evolving

        ## Tasks

        ### t-1: Research phase
        DONE

        **Spec:** Research the problem space and write findings to research.md.

        **Verify:** test -f research.md

        **Self-evolution:** If research reveals additional work, add PENDING tasks.

        ### t-2: Implement solution
        PENDING

        **Spec:** Build the solution based on research findings.

        **Verify:** python3 -m pytest tests/ -v

        ### t-3: Self-evolved task
        PENDING

        **Spec:** Additional work discovered during research.

        **Verify:** echo "ok"
    """),
    "with_deps": textwrap.dedent("""\
        # Test Spec — With Dependencies

        ## Tasks

        ### t-1: Foundation
        PENDING

        **Spec:** Build the foundation.

        **Verify:** echo "foundation ok"

        ### t-2: Depends on foundation
        PENDING

        **Spec:** Build on top of foundation.

        **Verify:** echo "depends ok"

        **Deps:** t-1
    """),
    "large_spec": textwrap.dedent("""\
        # Test Spec — Large

        ## Tasks

        ### t-1: Task one
        DONE

        **Spec:** First task.
        **Verify:** true

        ### t-2: Task two
        DONE

        **Spec:** Second task.
        **Verify:** true

        ### t-3: Task three
        PENDING

        **Spec:** Third task.
        **Verify:** true

        ### t-4: Task four
        PENDING

        **Spec:** Fourth task.
        **Verify:** true

        ### t-5: Task five
        PENDING

        **Spec:** Fifth task.
        **Verify:** true

        ### t-6: Task six
        PENDING

        **Spec:** Sixth task.
        **Verify:** true

        ### t-7: Task seven
        PENDING

        **Spec:** Seventh task.
        **Verify:** true

        ### t-8: Task eight
        PENDING

        **Spec:** Eighth task.
        **Verify:** true
    """),
    "invalid_no_status": textwrap.dedent("""\
        # Invalid Spec — Missing Status

        ## Tasks

        ### t-1: No status line

        **Spec:** This task has no PENDING/DONE/SKIPPED status.

        **Verify:** echo "should fail validation"
    """),
    "invalid_no_spec_section": textwrap.dedent("""\
        # Invalid Spec — Missing Spec Section

        ## Tasks

        ### t-1: No spec section
        PENDING

        **Verify:** echo "no spec section"
    """),
}


# ── Helper Functions ─────────────────────────────────────────────────────


def make_boi_state(base_dir: Optional[str] = None) -> dict[str, str]:
    """Create a temp directory tree mimicking ~/.boi/.

    If base_dir is provided, creates subdirs inside it.
    Otherwise creates a new temp directory (caller must clean up).

    Returns a dict with paths:
        {
            "root": "/tmp/xxx",
            "queue": "/tmp/xxx/queue",
            "logs": "/tmp/xxx/logs",
            "events": "/tmp/xxx/events",
            "hooks": "/tmp/xxx/hooks",
        }
    """
    if base_dir is None:
        base_dir = tempfile.mkdtemp(prefix="boi-test-")

    dirs = {
        "root": base_dir,
        "queue": os.path.join(base_dir, "queue"),
        "logs": os.path.join(base_dir, "logs"),
        "events": os.path.join(base_dir, "events"),
        "hooks": os.path.join(base_dir, "hooks"),
    }

    for d in dirs.values():
        os.makedirs(d, exist_ok=True)

    return dirs


def make_spec(
    base_dir: str,
    tasks_pending: int = 3,
    tasks_done: int = 0,
    tasks_skipped: int = 0,
    filename: str = "spec.md",
    content: Optional[str] = None,
) -> str:
    """Create a spec.md file with the given task counts.

    If content is provided, writes it directly (ignores task count args).
    Otherwise generates a spec with the specified number of tasks.

    Returns the absolute path to the created file.
    """
    spec_path = os.path.join(base_dir, filename)

    if content is not None:
        Path(spec_path).write_text(content, encoding="utf-8")
        return spec_path

    lines = ["# Test Spec\n\n## Tasks\n"]
    tid = 1

    for _ in range(tasks_done):
        lines.append(
            f"\n### t-{tid}: Done task {tid}\n"
            "DONE\n\n"
            f"**Spec:** Completed task {tid}.\n\n"
            "**Verify:** true\n"
        )
        tid += 1

    for _ in range(tasks_pending):
        lines.append(
            f"\n### t-{tid}: Pending task {tid}\n"
            "PENDING\n\n"
            f"**Spec:** Do task {tid}.\n\n"
            "**Verify:** true\n"
        )
        tid += 1

    for _ in range(tasks_skipped):
        lines.append(
            f"\n### t-{tid}: Skipped task {tid}\n"
            "SKIPPED\n\n"
            f"**Spec:** Skipped task {tid}.\n\n"
            "**Verify:** true\n"
        )
        tid += 1

    Path(spec_path).write_text("".join(lines), encoding="utf-8")
    return spec_path


def make_queue_entry(
    queue_dir: str,
    queue_id: str = "q-001",
    spec_path: str = "/tmp/spec.md",
    status: str = "queued",
    priority: int = 100,
    iteration: int = 0,
    max_iterations: int = 30,
    blocked_by: Optional[list[str]] = None,
    last_worker: Optional[str] = None,
    consecutive_failures: int = 0,
    tasks_done: int = 0,
    tasks_total: int = 0,
    checkout: Optional[str] = None,  # alias: worktree
    cooldown_until: Optional[str] = None,
    **extra: Any,
) -> dict[str, Any]:
    """Create a queue entry JSON file with configurable fields.

    Returns the entry dict that was written.
    """
    entry: dict[str, Any] = {
        "id": queue_id,
        "spec_path": spec_path,
        "worktree": checkout,
        "priority": priority,
        "status": status,
        "submitted_at": datetime.now(timezone.utc).isoformat(),
        "iteration": iteration,
        "max_iterations": max_iterations,
        "blocked_by": list(blocked_by) if blocked_by else [],
        "last_worker": last_worker,
        "last_iteration_at": None,
        "consecutive_failures": consecutive_failures,
        "tasks_done": tasks_done,
        "tasks_total": tasks_total,
    }

    if cooldown_until is not None:
        entry["cooldown_until"] = cooldown_until

    entry.update(extra)

    os.makedirs(queue_dir, exist_ok=True)
    filepath = os.path.join(queue_dir, f"{queue_id}.json")
    Path(filepath).write_text(json.dumps(entry, indent=2) + "\n", encoding="utf-8")
    return entry


def make_iteration_file(
    queue_dir: str,
    queue_id: str = "q-001",
    iteration: int = 1,
    tasks_completed: int = 2,
    tasks_added: int = 0,
    tasks_skipped: int = 0,
    duration_seconds: int = 600,
    exit_code: int = 0,
    pre_pending: int = 3,
    post_pending: int = 1,
) -> str:
    """Create an iteration-N.json file for a queue entry.

    Returns the path to the created file.
    """
    data = {
        "iteration": iteration,
        "queue_id": queue_id,
        "tasks_completed": tasks_completed,
        "tasks_added": tasks_added,
        "tasks_skipped": tasks_skipped,
        "duration_seconds": duration_seconds,
        "exit_code": exit_code,
        "pre_pending": pre_pending,
        "post_pending": post_pending,
        "timestamp": datetime.now(timezone.utc).isoformat(),
    }

    os.makedirs(queue_dir, exist_ok=True)
    filepath = os.path.join(queue_dir, f"{queue_id}.iteration-{iteration}.json")
    Path(filepath).write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
    return filepath


def make_telemetry_file(
    queue_dir: str,
    queue_id: str = "q-001",
    total_iterations: int = 3,
    total_time_seconds: int = 2843,
    tasks_completed_per_iteration: Optional[list[int]] = None,
    tasks_added_per_iteration: Optional[list[int]] = None,
    tasks_skipped_per_iteration: Optional[list[int]] = None,
    consecutive_failures: int = 0,
) -> str:
    """Create a telemetry.json file for a queue entry.

    Returns the path to the created file.
    """
    if tasks_completed_per_iteration is None:
        tasks_completed_per_iteration = [2, 2, 1]
    if tasks_added_per_iteration is None:
        tasks_added_per_iteration = [1, 1, 0]
    if tasks_skipped_per_iteration is None:
        tasks_skipped_per_iteration = [0, 0, 1]

    data = {
        "queue_id": queue_id,
        "total_iterations": total_iterations,
        "total_time_seconds": total_time_seconds,
        "tasks_completed_per_iteration": tasks_completed_per_iteration,
        "tasks_added_per_iteration": tasks_added_per_iteration,
        "tasks_skipped_per_iteration": tasks_skipped_per_iteration,
        "consecutive_failures": consecutive_failures,
        "last_updated": datetime.now(timezone.utc).isoformat(),
    }

    os.makedirs(queue_dir, exist_ok=True)
    filepath = os.path.join(queue_dir, f"{queue_id}.telemetry.json")
    Path(filepath).write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
    return filepath


def make_hook_script(
    hooks_dir: str,
    name: str = "on-complete",
    body: str = "exit 0",
    executable: bool = True,
) -> str:
    """Create a hook script in the hooks directory.

    Returns the path to the created script.
    """
    os.makedirs(hooks_dir, exist_ok=True)
    script_path = os.path.join(hooks_dir, f"{name}.sh")
    content = f"#!/bin/bash\n{body}\n"
    Path(script_path).write_text(content, encoding="utf-8")

    if executable:
        os.chmod(script_path, os.stat(script_path).st_mode | stat.S_IEXEC)

    return script_path


def make_worker_pid(
    state_dir: str,
    worker_id: str = "w-1",
    pid: Optional[int] = None,
) -> str:
    """Write a PID file simulating a running worker.

    If pid is None, uses the current process PID (always "alive").

    Returns the path to the PID file.
    """
    if pid is None:
        pid = os.getpid()

    os.makedirs(state_dir, exist_ok=True)
    pid_path = os.path.join(state_dir, f"{worker_id}.pid")
    Path(pid_path).write_text(str(pid) + "\n", encoding="utf-8")
    return pid_path


def make_log_file(
    logs_dir: str,
    queue_id: str = "q-001",
    iteration: int = 1,
    content: str = "Worker started\nProcessing task t-1\nTask completed\n",
) -> str:
    """Create a log file for a worker iteration.

    Returns the path to the created log file.
    """
    os.makedirs(logs_dir, exist_ok=True)
    log_path = os.path.join(logs_dir, f"{queue_id}-iter-{iteration}.log")
    Path(log_path).write_text(content, encoding="utf-8")
    return log_path


def make_config(
    state_dir: str,
    workers: Optional[list[dict[str, Any]]] = None,
) -> str:
    """Create a config.json file mimicking BOI configuration.

    Returns the path to the created config file.
    """
    if workers is None:
        workers = [
            {"id": "w-1", "worktree_path": "/tmp/worktree-1"},
            {"id": "w-2", "worktree_path": "/tmp/worktree-2"},
            {"id": "w-3", "worktree_path": "/tmp/worktree-3"},
        ]

    config = {
        "workers": workers,
        "created_at": datetime.now(timezone.utc).isoformat(),
    }

    config_path = os.path.join(state_dir, "config.json")
    Path(config_path).write_text(json.dumps(config, indent=2) + "\n", encoding="utf-8")
    return config_path


# ── Base Test Case ───────────────────────────────────────────────────────


class BoiTestCase(unittest.TestCase):
    """Base test case with a full ~/.boi/ temp directory tree.

    Provides:
        self.boi_state   — root of the temp state dir
        self.queue_dir   — path to queue/
        self.logs_dir    — path to logs/
        self.events_dir  — path to events/
        self.hooks_dir   — path to hooks/
        self.specs_dir   — path for temp spec files

    Helper methods:
        self.create_spec(...)         — create a spec.md file
        self.create_queue_entry(...)  — create a queue JSON file
        self.create_iteration(...)    — create an iteration file
        self.create_telemetry(...)    — create a telemetry file
        self.create_hook(...)         — create a hook script
        self.create_config(...)       — create a config.json
        self.create_log(...)          — create a log file
    """

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        paths = make_boi_state(self._tmpdir.name)
        self.boi_state = paths["root"]
        self.queue_dir = paths["queue"]
        self.logs_dir = paths["logs"]
        self.events_dir = paths["events"]
        self.hooks_dir = paths["hooks"]
        # Extra dir for spec files (not part of ~/.boi/ but useful in tests)
        self.specs_dir = os.path.join(self._tmpdir.name, "specs")
        os.makedirs(self.specs_dir)

    def tearDown(self):
        self._tmpdir.cleanup()

    def create_spec(self, **kwargs) -> str:
        """Create a spec.md file. Delegates to make_spec().

        Default base_dir is self.specs_dir.
        """
        kwargs.setdefault("base_dir", self.specs_dir)
        return make_spec(**kwargs)

    def create_queue_entry(self, **kwargs) -> dict[str, Any]:
        """Create a queue entry JSON file. Delegates to make_queue_entry().

        Default queue_dir is self.queue_dir.
        """
        kwargs.setdefault("queue_dir", self.queue_dir)
        return make_queue_entry(**kwargs)

    def create_iteration(self, **kwargs) -> str:
        """Create an iteration file. Delegates to make_iteration_file().

        Default queue_dir is self.queue_dir.
        """
        kwargs.setdefault("queue_dir", self.queue_dir)
        return make_iteration_file(**kwargs)

    def create_telemetry(self, **kwargs) -> str:
        """Create a telemetry file. Delegates to make_telemetry_file().

        Default queue_dir is self.queue_dir.
        """
        kwargs.setdefault("queue_dir", self.queue_dir)
        return make_telemetry_file(**kwargs)

    def create_hook(self, **kwargs) -> str:
        """Create a hook script. Delegates to make_hook_script().

        Default hooks_dir is self.hooks_dir.
        """
        kwargs.setdefault("hooks_dir", self.hooks_dir)
        return make_hook_script(**kwargs)

    def create_config(self, **kwargs) -> str:
        """Create a config.json. Delegates to make_config().

        Default state_dir is self.boi_state.
        """
        kwargs.setdefault("state_dir", self.boi_state)
        return make_config(**kwargs)

    def create_log(self, **kwargs) -> str:
        """Create a log file. Delegates to make_log_file().

        Default logs_dir is self.logs_dir.
        """
        kwargs.setdefault("logs_dir", self.logs_dir)
        return make_log_file(**kwargs)
