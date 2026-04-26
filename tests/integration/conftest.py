# conftest.py — Integration test infrastructure for BOI.
#
# Provides:
#   MockClaude    — A Python script that simulates Claude behavior.
#                   Configurable: complete tasks, add tasks, fail, timeout.
#                   Phase-aware: execute, critic, evaluate, decompose.
#   DaemonTestHarness — Start/stop daemon in-process with test config,
#                       wait for state transitions, verify DB state.
#   create_test_config — Build a temp config with worktree dirs and DB.
#
# No real Claude calls. No real worktrees beyond temp dirs.

import json
import os
import re
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
import unittest
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

# Add project root to path
_PROJECT_ROOT = str(Path(__file__).resolve().parent.parent.parent)
sys.path.insert(0, _PROJECT_ROOT)

from lib.db import Database


# ── MockClaude ───────────────────────────────────────────────────────


class MockClaude:
    """A configurable mock that replaces Claude in integration tests.

    MockClaude is a Python script generator. It produces a standalone
    Python script that reads a spec file, optionally modifies it
    (marking tasks DONE, adding tasks, writing critic output), waits
    a configurable time, and exits with a configurable code.

    The generated script is self-contained (no imports from boi libs)
    so it can run in any environment.

    Phase behaviors:
      execute   — Mark N PENDING tasks as DONE in the spec file.
      critic    — Write '## Critic Approved' or add [CRITIC] PENDING
                  tasks to the spec.
      evaluate  — Write evaluation results (criteria checkboxes).
      decompose — Add N PENDING tasks to the spec.

    Args:
        phase: Phase to simulate (execute|critic|evaluate|decompose).
        tasks_to_complete: Number of PENDING tasks to mark DONE
            (execute phase).
        exit_code: Exit code for the script (default 0).
        delay_seconds: Seconds to sleep before exiting (default 0).
        add_tasks: Number of new PENDING tasks to add (for critic
            or decompose phases).
        critic_approve: If True, critic writes approval marker
            instead of adding tasks.
        criteria_to_meet: List of criteria indices (0-based) to
            check off in evaluate phase.
        fail_silently: If True, exit without modifying the spec.
    """

    def __init__(
        self,
        phase: str = "execute",
        tasks_to_complete: int = 1,
        exit_code: int = 0,
        delay_seconds: float = 0,
        add_tasks: int = 0,
        critic_approve: bool = False,
        criteria_to_meet: Optional[list[int]] = None,
        fail_silently: bool = False,
    ) -> None:
        self.phase = phase
        self.tasks_to_complete = tasks_to_complete
        self.exit_code = exit_code
        self.delay_seconds = delay_seconds
        self.add_tasks = add_tasks
        self.critic_approve = critic_approve
        self.criteria_to_meet = criteria_to_meet or []
        self.fail_silently = fail_silently
        self.task_id: Optional[str] = None

    def generate_script(self, spec_path: str, output_dir: str) -> str:
        """Generate a standalone Python script that simulates Claude.

        The script reads the spec at spec_path, applies the configured
        behavior, and exits. It writes its own exit code to a .exit
        file next to the spec.

        Args:
            spec_path: Path to the spec file to modify.
            output_dir: Directory to write the mock script into.

        Returns:
            Path to the generated mock script.
        """
        suffix = f"_{self.task_id}" if self.task_id else ""
        script_path = os.path.join(output_dir, f"mock_claude{suffix}.py")
        script = self._build_script(spec_path)
        Path(script_path).write_text(script, encoding="utf-8")
        os.chmod(script_path, 0o755)
        return script_path

    def _build_script(self, spec_path: str) -> str:
        """Build the Python script source code."""
        parts = [
            "#!/usr/bin/env python3",
            "import re, sys, time, os",
            "",
            f"SPEC_PATH = {spec_path!r}",
            f"PHASE = {self.phase!r}",
            f"TASKS_TO_COMPLETE = {self.tasks_to_complete}",
            f"EXIT_CODE = {self.exit_code}",
            f"DELAY_SECONDS = {self.delay_seconds}",
            f"ADD_TASKS = {self.add_tasks}",
            f"CRITIC_APPROVE = {self.critic_approve}",
            f"CRITERIA_TO_MEET = {self.criteria_to_meet!r}",
            f"FAIL_SILENTLY = {self.fail_silently}",
            # task_id: if set, mark only this specific task DONE
            f"TASK_ID = {self.task_id!r}",
            "",
        ]

        parts.append(textwrap.dedent("""\
            def read_spec():
                with open(SPEC_PATH, 'r') as f:
                    return f.read()

            def write_spec(content):
                tmp = SPEC_PATH + '.tmp'
                with open(tmp, 'w') as f:
                    f.write(content)
                os.rename(tmp, SPEC_PATH)

            def mark_tasks_done(content, count):
                \"\"\"Mark the first `count` PENDING tasks as DONE.
                If TASK_ID is set, mark only that specific task DONE.\"\"\"
                if TASK_ID:
                    # Mark only the specific assigned task DONE
                    lines = content.split('\\n')
                    result = []
                    in_target = False
                    for line in lines:
                        if re.match(r'^###\\s+' + re.escape(TASK_ID) + r':', line):
                            in_target = True
                        elif re.match(r'^###\\s+t-\\d+:', line):
                            in_target = False
                        if in_target and line.strip() == 'PENDING':
                            result.append(line.replace('PENDING', 'DONE'))
                            in_target = False  # only mark first PENDING in section
                        else:
                            result.append(line)
                    return '\\n'.join(result)
                # Fallback: mark first `count` PENDING tasks
                marked = 0
                lines = content.split('\\n')
                result = []
                for line in lines:
                    if marked < count and line.strip() == 'PENDING':
                        result.append(line.replace('PENDING', 'DONE'))
                        marked += 1
                    else:
                        result.append(line)
                return '\\n'.join(result)

            def add_pending_tasks(content, count, prefix=''):
                \"\"\"Append new PENDING tasks to the spec.\"\"\"
                new_tasks = []
                # Find highest existing task number
                nums = re.findall(r'### t-(\\d+)', content)
                next_id = max(int(n) for n in nums) + 1 if nums else 1
                for i in range(count):
                    tid = next_id + i
                    label = f'{prefix}' if prefix else 'Added'
                    new_tasks.append(
                        f'\\n### t-{tid}: {label} task {tid}\\n'
                        f'PENDING\\n\\n'
                        f'**Spec:** {label} task {tid}.\\n\\n'
                        f'**Verify:** true\\n'
                    )
                return content.rstrip() + '\\n' + ''.join(new_tasks)

            def handle_execute():
                content = read_spec()
                content = mark_tasks_done(content, TASKS_TO_COMPLETE)
                write_spec(content)

            def handle_critic():
                content = read_spec()
                if CRITIC_APPROVE:
                    # Write approval marker at end of spec
                    content = content.rstrip() + '\\n\\n## Critic Approved\\n'
                    write_spec(content)
                elif ADD_TASKS > 0:
                    content = add_pending_tasks(
                        content, ADD_TASKS, prefix='[CRITIC]'
                    )
                    write_spec(content)
                # else: no output (crash simulation)

            def handle_evaluate():
                content = read_spec()
                # Find criteria checkboxes and check specified ones
                lines = content.split('\\n')
                result = []
                criteria_idx = 0
                for line in lines:
                    if re.match(r'^\\s*- \\[ \\]', line):
                        if criteria_idx in CRITERIA_TO_MEET:
                            line = line.replace('- [ ]', '- [x]', 1)
                        criteria_idx += 1
                    result.append(line)
                write_spec('\\n'.join(result))

            def handle_decompose():
                content = read_spec()
                if ADD_TASKS > 0:
                    content = add_pending_tasks(
                        content, ADD_TASKS, prefix='Decomposed'
                    )
                    write_spec(content)

            def main():
                if DELAY_SECONDS > 0:
                    time.sleep(DELAY_SECONDS)

                if FAIL_SILENTLY:
                    sys.exit(EXIT_CODE)

                if PHASE == 'execute':
                    handle_execute()
                elif PHASE == 'critic':
                    handle_critic()
                elif PHASE == 'evaluate':
                    handle_evaluate()
                elif PHASE == 'decompose':
                    handle_decompose()

                sys.exit(EXIT_CODE)

            if __name__ == '__main__':
                main()
        """))

        return "\n".join(parts)

    def generate_run_script(
        self,
        spec_path: str,
        exit_file: str,
        output_dir: str,
    ) -> str:
        """Generate a bash run script that calls the mock Claude script.

        This replaces the real worker's run.sh: it invokes the mock
        script, then writes the exit code to the exit file.

        Args:
            spec_path: Path to the spec file.
            exit_file: Path to write the exit code.
            output_dir: Directory to write scripts into.

        Returns:
            Path to the generated bash run script.
        """
        mock_script = self.generate_script(spec_path, output_dir)

        suffix = f"_{self.task_id}" if self.task_id else ""
        run_script_path = os.path.join(output_dir, f"mock_run{suffix}.sh")
        run_script = textwrap.dedent(f"""\
            #!/bin/bash
            set -uo pipefail
            {sys.executable} {mock_script}
            _EXIT=$?
            echo "$_EXIT" > {exit_file}
            exit $_EXIT
        """)
        Path(run_script_path).write_text(run_script, encoding="utf-8")
        os.chmod(run_script_path, 0o755)
        return run_script_path


# ── DaemonTestHarness ────────────────────────────────────────────────


class DaemonTestHarness:
    """Start/stop a BOI daemon in-process for integration testing.

    The harness runs the daemon's poll loop in a background thread,
    allowing tests to dispatch specs, wait for state transitions,
    and inspect the database.

    The daemon uses a mock worker launcher that invokes MockClaude
    scripts instead of real Claude sessions.

    Args:
        state_dir: Root of the temp ~/.boi directory.
        config_path: Path to config.json.
        db_path: Path to the SQLite database.
        mock_claude_factory: Callable that returns a MockClaude
            instance for a given spec_id and phase. If None, uses
            a default MockClaude that completes one task per
            iteration.
    """

    def __init__(
        self,
        state_dir: str,
        config_path: str,
        db_path: str,
        mock_claude_factory: Optional[
            "type[MockClaudeFactory]"
        ] = None,
    ) -> None:
        self.state_dir = state_dir
        self.config_path = config_path
        self.db_path = db_path
        self.queue_dir = os.path.join(state_dir, "queue")
        self.log_dir = os.path.join(state_dir, "logs")
        self.mock_claude_factory = mock_claude_factory

        self._daemon = None
        self._thread: Optional[threading.Thread] = None
        self._stopped = threading.Event()

        # Database for test assertions (separate connection)
        self.db = Database(db_path, self.queue_dir)

    def start(self) -> None:
        """Start the daemon in a background thread.

        Patches the daemon's launch_worker to use MockClaude
        instead of real tmux/Claude sessions.
        """
        from daemon import Daemon

        self._daemon = Daemon(
            config_path=self.config_path,
            db_path=self.db_path,
            poll_interval=1,
            state_dir=self.state_dir,
        )

        # Patch launch_worker to use mock
        original_launch = self._daemon.launch_worker
        self._daemon.launch_worker = self._mock_launch_worker

        # Ensure directories exist
        for d in [self.queue_dir, self.log_dir]:
            os.makedirs(d, exist_ok=True)

        self._stopped.clear()

        def _run_daemon() -> None:
            try:
                self._daemon.run()
            except Exception:
                pass
            finally:
                self._stopped.set()

        self._thread = threading.Thread(
            target=_run_daemon, daemon=True
        )
        self._thread.start()

        # Wait briefly for daemon to initialize
        time.sleep(0.3)

    def stop(self, timeout: float = 10) -> None:
        """Stop the daemon and wait for the thread to exit."""
        if self._daemon is not None:
            self._daemon._shutdown_requested = True

        if self._thread is not None:
            self._thread.join(timeout=timeout)

        self._stopped.wait(timeout=2)

    def _mock_launch_worker(
        self,
        spec_id: str,
        worktree: str,
        spec_path: str,
        iteration: int,
        phase: str,
        worker_id: str,
        timeout: Optional[int] = None,
        task_id: Optional[str] = None,
    ) -> subprocess.Popen:
        """Launch a mock worker process instead of real Claude.

        Creates a MockClaude instance (via factory or default),
        generates its script, and runs it as a subprocess with
        start_new_session=True (matching real daemon behavior).
        """
        # Get mock configuration
        if self.mock_claude_factory is not None:
            mock = self.mock_claude_factory(spec_id, phase, iteration)
        else:
            mock = MockClaude(
                phase=phase, tasks_to_complete=1, exit_code=0
            )
        mock.task_id = task_id

        # Generate mock script in queue dir
        script_dir = os.path.join(self.queue_dir, f"{spec_id}-mock")
        os.makedirs(script_dir, exist_ok=True)

        exit_file = os.path.join(self.queue_dir, f"{spec_id}.exit")
        run_script = mock.generate_run_script(
            spec_path=spec_path,
            exit_file=exit_file,
            output_dir=script_dir,
        )

        # Log file
        log_file = os.path.join(
            self.log_dir, f"{spec_id}-iter-{iteration}.log"
        )
        os.makedirs(self.log_dir, exist_ok=True)
        log_fh = open(log_file, "a", encoding="utf-8")

        proc = subprocess.Popen(
            ["bash", run_script],
            stdout=log_fh,
            stderr=log_fh,
            cwd=worktree,
            start_new_session=True,
        )

        log_fh.close()
        return proc

    def wait_for_status(
        self,
        spec_id: str,
        target_status: str,
        timeout: float = 30,
        poll_interval: float = 0.3,
    ) -> dict[str, Any]:
        """Wait for a spec to reach a target status.

        Args:
            spec_id: The spec ID to watch.
            target_status: Status to wait for (e.g. 'completed').
            timeout: Max seconds to wait.
            poll_interval: Seconds between polls.

        Returns:
            The spec dict when it reaches the target status.

        Raises:
            TimeoutError: If the spec doesn't reach the target
                status within the timeout.
        """
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            spec = self.db.get_spec(spec_id)
            if spec is not None and spec["status"] == target_status:
                return spec
            time.sleep(poll_interval)

        spec = self.db.get_spec(spec_id)
        current = spec["status"] if spec else "not found"
        raise TimeoutError(
            f"Spec {spec_id} did not reach '{target_status}' "
            f"within {timeout}s (current: {current})"
        )

    def wait_for_any_status(
        self,
        spec_id: str,
        target_statuses: list[str],
        timeout: float = 30,
        poll_interval: float = 0.3,
    ) -> dict[str, Any]:
        """Wait for a spec to reach any of the target statuses.

        Returns:
            The spec dict when it reaches any target status.

        Raises:
            TimeoutError: If timeout exceeded.
        """
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            spec = self.db.get_spec(spec_id)
            if spec is not None and spec["status"] in target_statuses:
                return spec
            time.sleep(poll_interval)

        spec = self.db.get_spec(spec_id)
        current = spec["status"] if spec else "not found"
        raise TimeoutError(
            f"Spec {spec_id} did not reach any of "
            f"{target_statuses} within {timeout}s "
            f"(current: {current})"
        )

    def wait_for_iteration(
        self,
        spec_id: str,
        target_iteration: int,
        timeout: float = 30,
        poll_interval: float = 0.3,
    ) -> dict[str, Any]:
        """Wait for a spec to reach a target iteration number.

        Returns:
            The spec dict when iteration >= target_iteration.

        Raises:
            TimeoutError: If timeout exceeded.
        """
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            spec = self.db.get_spec(spec_id)
            if (
                spec is not None
                and spec["iteration"] >= target_iteration
            ):
                return spec
            time.sleep(poll_interval)

        spec = self.db.get_spec(spec_id)
        current = spec["iteration"] if spec else "not found"
        raise TimeoutError(
            f"Spec {spec_id} did not reach iteration "
            f"{target_iteration} within {timeout}s "
            f"(current: {current})"
        )

    def get_iterations(
        self, spec_id: str
    ) -> list[dict[str, Any]]:
        """Get all iteration records for a spec."""
        return self.db.get_iterations(spec_id)

    def get_events(
        self,
        spec_id: Optional[str] = None,
        limit: int = 100,
    ) -> list[dict[str, Any]]:
        """Get events, optionally filtered by spec_id."""
        return self.db.get_events(spec_id=spec_id, limit=limit)


# ── MockClaudeFactory protocol ───────────────────────────────────────


# A factory callable: (spec_id, phase, iteration) -> MockClaude
MockClaudeFactory = Any  # Callable[[str, str, int], MockClaude]


# ── create_test_config ───────────────────────────────────────────────


def create_test_config(
    num_workers: int = 2,
    base_dir: Optional[str] = None,
    worker_timeout: int = 60,
) -> dict[str, str]:
    """Create a temporary test configuration for integration tests.

    Sets up:
      - A temp state directory (~/.boi equivalent)
      - Worktree directories for each worker
      - A config.json with worker definitions
      - An empty SQLite database

    Args:
        num_workers: Number of worker slots to configure.
        base_dir: Base directory for temp files. If None, creates
            a new temp directory (caller must clean up).
        worker_timeout: Default worker timeout in seconds.

    Returns:
        A dict with paths:
            {
                "base_dir": "/tmp/xxx",
                "state_dir": "/tmp/xxx/.boi",
                "config_path": "/tmp/xxx/.boi/config.json",
                "db_path": "/tmp/xxx/.boi/boi.db",
                "queue_dir": "/tmp/xxx/.boi/queue",
                "log_dir": "/tmp/xxx/.boi/logs",
                "worktrees": ["/tmp/xxx/worktree-1", ...],
            }
    """
    if base_dir is None:
        base_dir = tempfile.mkdtemp(prefix="boi-integ-")

    state_dir = os.path.join(base_dir, ".boi")
    queue_dir = os.path.join(state_dir, "queue")
    log_dir = os.path.join(state_dir, "logs")
    events_dir = os.path.join(state_dir, "events")
    hooks_dir = os.path.join(state_dir, "hooks")

    for d in [state_dir, queue_dir, log_dir, events_dir, hooks_dir]:
        os.makedirs(d, exist_ok=True)

    # Create worktree directories
    worktrees = []
    workers = []
    for i in range(1, num_workers + 1):
        wt = os.path.join(base_dir, f"worktree-{i}")
        os.makedirs(wt, exist_ok=True)
        worktrees.append(wt)
        workers.append({
            "id": f"w-{i}",
            "worktree_path": wt,
        })

    # Write config.json
    config = {
        "workers": workers,
        "worker_timeout_seconds": worker_timeout,
        "created_at": datetime.now(timezone.utc).isoformat(),
    }
    config_path = os.path.join(state_dir, "config.json")
    Path(config_path).write_text(
        json.dumps(config, indent=2) + "\n", encoding="utf-8"
    )

    # Database path (will be created on first access)
    db_path = os.path.join(state_dir, "boi.db")

    return {
        "base_dir": base_dir,
        "state_dir": state_dir,
        "config_path": config_path,
        "db_path": db_path,
        "queue_dir": queue_dir,
        "log_dir": log_dir,
        "worktrees": worktrees,
    }


# ── IntegrationTestCase base class ──────────────────────────────────


class IntegrationTestCase(unittest.TestCase):
    """Base test case for BOI integration tests.

    Sets up a complete temp environment with config, database,
    worktree dirs, and a DaemonTestHarness. Subclasses can
    override `mock_claude_factory` to customize MockClaude
    behavior per test.

    Provides:
        self.config    — dict from create_test_config()
        self.harness   — DaemonTestHarness instance
        self.db        — Database instance for assertions
    """

    NUM_WORKERS = 2
    WORKER_TIMEOUT = 60

    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.config = create_test_config(
            num_workers=self.NUM_WORKERS,
            base_dir=self._tmpdir.name,
            worker_timeout=self.WORKER_TIMEOUT,
        )
        self.harness = DaemonTestHarness(
            state_dir=self.config["state_dir"],
            config_path=self.config["config_path"],
            db_path=self.config["db_path"],
            mock_claude_factory=self.mock_claude_factory,
        )
        self.db = self.harness.db

    def tearDown(self) -> None:
        self.harness.stop(timeout=5)
        self.db.close()
        self._tmpdir.cleanup()

    def mock_claude_factory(
        self, spec_id: str, phase: str, iteration: int
    ) -> MockClaude:
        """Default mock factory: complete 1 task per execute iteration.

        Override in subclasses for custom behavior.
        """
        if phase == "execute":
            return MockClaude(
                phase="execute", tasks_to_complete=1, exit_code=0
            )
        elif phase == "task-verify":
            return MockClaude(
                phase="task-verify", critic_approve=True, exit_code=0
            )
        elif phase == "evaluate":
            return MockClaude(
                phase="evaluate", exit_code=0
            )
        elif phase == "decompose":
            return MockClaude(
                phase="decompose", add_tasks=5, exit_code=0
            )
        return MockClaude(exit_code=0)

    def create_spec(
        self,
        tasks_pending: int = 3,
        tasks_done: int = 0,
        content: Optional[str] = None,
        filename: str = "spec.md",
    ) -> str:
        """Create a spec file in a temp directory.

        Args:
            tasks_pending: Number of PENDING tasks to generate.
            tasks_done: Number of DONE tasks to generate.
            content: Raw spec content (overrides task counts).
            filename: Name for the spec file.

        Returns:
            Absolute path to the created spec file.
        """
        specs_dir = os.path.join(self._tmpdir.name, "specs")
        os.makedirs(specs_dir, exist_ok=True)
        spec_path = os.path.join(specs_dir, filename)

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

        Path(spec_path).write_text(
            "".join(lines), encoding="utf-8"
        )
        return spec_path

    def dispatch_spec(
        self,
        spec_path: str,
        priority: int = 100,
        max_iterations: int = 30,
        worktree: Optional[str] = None,
        blocked_by: Optional[list[str]] = None,
        worker_timeout_seconds: Optional[int] = None,
    ) -> str:
        """Enqueue a spec via the database and return its queue ID.

        Args:
            spec_path: Path to the spec file.
            priority: Queue priority (lower = higher).
            max_iterations: Max execute-phase iterations.
            worktree: Optional worktree override.
            blocked_by: List of spec IDs this spec depends on.
            worker_timeout_seconds: Per-spec timeout.

        Returns:
            The queue ID assigned to the spec.
        """
        result = self.db.enqueue(
            spec_path=spec_path,
            priority=priority,
            max_iterations=max_iterations,
            checkout=worktree,
            blocked_by=blocked_by,
        )
        spec_id = result["id"]

        if worker_timeout_seconds is not None:
            with self.db.lock:
                self.db.conn.execute(
                    "UPDATE specs SET worker_timeout_seconds = ? "
                    "WHERE id = ?",
                    (worker_timeout_seconds, spec_id),
                )
                self.db.conn.commit()

        return spec_id
