# daemon.py — Python daemon for BOI queue dispatch.
#
# Replaces daemon.sh. Owns the main poll loop: check worker
# completions, dispatch specs to free workers, write state
# snapshots, run self-heal, and maintain a heartbeat file.
#
# All mutable state lives in SQLite (via lib/db.py).
# Workers are spawned as subprocesses in new sessions
# (start_new_session=True) so the daemon can kill entire
# process groups on shutdown or timeout.
#
# Usage:
#   python3 daemon.py                    # Start (daemonizes by default)
#   python3 daemon.py --foreground       # Run in foreground
#   python3 daemon.py --stop             # Stop running daemon
#   python3 daemon.py --config PATH      # Custom config path
#   python3 daemon.py --db PATH          # Custom database path

import argparse
import hashlib
import json
import logging
import os
import re
import resource
import signal
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

sys.path.insert(0, str(Path(__file__).resolve().parent))

from lib.daemon_lock import DaemonLock
from lib.db import Database

# Optional coordination cleanup (graceful degradation if module is missing)
try:
    _coord_lib = os.path.expanduser("~/.boi/lib")
    if _coord_lib not in sys.path:
        sys.path.insert(0, _coord_lib)
    from coordination import cleanup_expired as _coord_cleanup_expired  # type: ignore[import]
except Exception:
    _coord_cleanup_expired = None  # type: ignore[assignment]

# Default daemon constants
DEFAULT_POLL_INTERVAL = 5
DEFAULT_WORKER_TIMEOUT = 1800  # 30 minutes
SELF_HEAL_INTERVAL = 10  # Run self-heal every N poll cycles
DEFAULT_RECONCILE_INTERVAL = 30  # Seconds between periodic liveness checks

logger = logging.getLogger("boi.daemon")


def _raise_fd_limit(target: int = 1024) -> None:
    """Raise the file descriptor soft limit if needed.

    At 10+ parallel workers (each with agent subprocess + pipes),
    the default 256 FD limit on some systems causes SIGTERM kills.
    """
    try:
        soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
        if soft < target:
            resource.setrlimit(resource.RLIMIT_NOFILE, (min(target, hard), hard))
            logger.info("Raised FD limit from %d to %d (hard: %d)", soft, target, hard)
    except (ValueError, OSError) as e:
        logger.warning("Could not raise FD limit: %s", e)


class Daemon:
    """BOI dispatch daemon. Polls the queue, assigns specs to workers,
    monitors completions, and runs periodic self-heal.

    Args:
        config_path: Path to config.json with worker definitions.
        db_path: Path to the SQLite database file.
        poll_interval: Seconds between poll cycles.
        state_dir: Path to ~/.boi state directory. Derived from
            db_path parent if not provided.
    """

    def __init__(
        self,
        config_path: str,
        db_path: str,
        poll_interval: int = DEFAULT_POLL_INTERVAL,
        state_dir: Optional[str] = None,
        reconcile_interval: int = DEFAULT_RECONCILE_INTERVAL,
    ) -> None:
        self.config_path = config_path
        self.db_path = db_path
        self.poll_interval = poll_interval

        # Derive state_dir from db_path parent if not given
        if state_dir is None:
            self.state_dir = str(Path(db_path).parent)
        else:
            self.state_dir = state_dir

        self.queue_dir = os.path.join(self.state_dir, "queue")
        self.log_dir = os.path.join(self.state_dir, "logs")
        self.hooks_dir = os.path.join(self.state_dir, "hooks")
        self.script_dir = str(Path(__file__).resolve().parent)
        self.phases_dir = os.path.join(self.state_dir, "phases")

        # PID / lock files
        self.pid_file = os.path.join(self.state_dir, "daemon.pid")
        self._daemon_lock = DaemonLock(self.state_dir)

        # Active worker subprocesses: worker_id -> subprocess.Popen
        self.worker_procs: dict[str, subprocess.Popen] = {}

        # Phase configs loaded from ~/.boi/phases/ (hot-reloaded each cycle)
        self.phase_configs: dict[str, Any] = {}
        self._phase_mtimes: dict[str, float] = {}

        # Default worker timeout (can be overridden per-spec)
        self.default_worker_timeout = DEFAULT_WORKER_TIMEOUT

        # Periodic reconciliation interval (seconds)
        self.reconcile_interval = reconcile_interval
        self._last_reconcile: float = 0.0

        # Shutdown flag
        self._shutdown_requested = False

        # Hash of last written state snapshot (for change detection)
        self._last_snapshot_hash: str = ""

        # Database connection
        self.db = Database(db_path, self.queue_dir)

        # Install signal handlers
        signal.signal(signal.SIGTERM, self._signal_handler)
        signal.signal(signal.SIGINT, self._signal_handler)

        # Raise FD limit to prevent SIGTERM at 10+ parallel workers
        _raise_fd_limit()

    # ── Signal handling ──────────────────────────────────────────────

    def _signal_handler(self, signum: int, _frame: Any) -> None:
        """Handle SIGTERM and SIGINT by requesting shutdown."""
        sig_name = signal.Signals(signum).name
        logger.info("Received %s, initiating shutdown", sig_name)
        self._shutdown_requested = True

    # ── Phase discovery ──────────────────────────────────────────────

    def _load_phases(self) -> None:
        """Discover phase configs from state_dir/phases/ and ~/.boi/phases/.

        Loads all *.phase.toml files from both directories. The state_dir
        phases take precedence over the global ~/.boi/phases/ directory.
        Stores loaded configs in self.phase_configs and tracks mtimes for
        hot-reload detection.
        """
        try:
            from lib.phases import load_phase
        except ImportError:
            return

        phases: dict[str, Any] = {}
        mtimes: dict[str, float] = {}

        # Check both state-local and user-global phases dirs
        phases_dirs = [
            os.path.expanduser("~/.boi/phases"),
            self.phases_dir,
        ]
        for d in phases_dirs:
            if not os.path.isdir(d):
                continue
            try:
                for entry in os.scandir(d):
                    if entry.name.endswith(".phase.toml") and entry.is_file():
                        try:
                            config = load_phase(entry.path)
                            phases[config.name] = config
                            mtimes[entry.path] = entry.stat().st_mtime
                        except Exception as exc:
                            logger.warning(
                                "Failed to load phase file %s: %s",
                                entry.path,
                                exc,
                            )
            except OSError:
                pass

        self.phase_configs = phases
        self._phase_mtimes = mtimes
        if phases:
            logger.info(
                "Loaded %d phase(s): %s",
                len(phases),
                ", ".join(sorted(phases.keys())),
            )

    def _reload_phases_if_changed(self) -> None:
        """Check phase file mtimes and reload if any have changed."""
        changed = False
        for path, old_mtime in list(self._phase_mtimes.items()):
            try:
                if os.path.getmtime(path) != old_mtime:
                    changed = True
                    break
            except OSError:
                changed = True
                break
        # Also check if new phase files appeared in either directory
        if not changed:
            for d in [os.path.expanduser("~/.boi/phases"), self.phases_dir]:
                if not os.path.isdir(d):
                    continue
                try:
                    for entry in os.scandir(d):
                        if (
                            entry.name.endswith(".phase.toml")
                            and entry.is_file()
                            and entry.path not in self._phase_mtimes
                        ):
                            changed = True
                            break
                except OSError:
                    pass
                if changed:
                    break
        if changed:
            logger.debug("Phase files changed, reloading")
            self._load_phases()

    # ── Pipeline routing ─────────────────────────────────────────────

    def _advance_pipeline(self, spec_id: str, current_phase: str) -> None:
        """Advance a spec to the next phase in the configured pipeline.

        Reads the pipeline from ~/.boi/guardrails.toml (defaults to
        ["plan-critique", "execute", "task-verify", "code-review"] if not
        found). If current_phase is the last phase, completes the spec.
        Otherwise, requeues it for the next phase.
        """
        try:
            from lib.guardrails import load_guardrails
            guardrails_path = os.path.join(self.state_dir, "guardrails.toml")
            config = load_guardrails(guardrails_path)
            pipeline = config.pipeline
        except Exception:
            pipeline = ["plan-critique", "execute", "task-verify", "code-review"]

        spec = self.db.get_spec(spec_id)
        if spec is None:
            return

        # Re-parse the spec file for fresh task counts. The DB values may be
        # stale if the critic phase added new tasks after initial dispatch.
        # Use the stored spec_path from DB to support both .md and .yaml formats.
        spec_path = spec.get("spec_path") or os.path.join(self.state_dir, "queue", f"{spec_id}.spec.md")
        if os.path.isfile(spec_path):
            try:
                from lib.spec_parser import count_boi_tasks
                counts = count_boi_tasks(spec_path)
                done = counts.get("done", 0)
                total = counts.get("total", 0)
                self.db.update_spec_fields(spec_id, tasks_done=done, tasks_total=total)
            except Exception:
                logger.debug("Failed to reparse spec %s for fresh counts", spec_id)
                done = spec.get("tasks_done", 0)
                total = spec.get("tasks_total", 0)
        else:
            done = spec.get("tasks_done", 0)
            total = spec.get("tasks_total", 0)

        try:
            idx = pipeline.index(current_phase)
            next_idx = idx + 1
        except ValueError:
            # current_phase not in pipeline — complete
            logger.info(
                "Phase '%s' not in pipeline for %s, completing",
                current_phase,
                spec_id,
            )
            self.db.complete(spec_id, done, total)
            return

        if next_idx >= len(pipeline):
            logger.info(
                "Pipeline complete for %s after phase '%s'",
                spec_id,
                current_phase,
            )
            self.db.complete(spec_id, done, total)
        else:
            next_phase = pipeline[next_idx]
            self.db.requeue(spec_id, done, total)
            self.db.update_spec_fields(spec_id, phase=next_phase)
            logger.info(
                "Pipeline advancing %s: %s → %s",
                spec_id,
                current_phase,
                next_phase,
            )

    def _handle_custom_phase_completion(
        self,
        spec_id: str,
        phase: str,
        phase_config: Any,
        exit_code: int,
        spec_path: str,
    ) -> None:
        """Generic signal-based completion handler for custom phases.

        Reads the spec file for approve_signal or reject_signal and routes
        accordingly. Falls back to on_crash handling if neither signal found
        or if exit_code is non-zero.
        """
        if exit_code != 0:
            # Run post hooks even on crash (advisory — errors are ignored)
            try:
                from lib.guardrail_runner import run_hooks
                run_hooks(
                    hook_point=f"post-{phase}",
                    spec_id=spec_id,
                    spec_path=spec_path,
                    state_dir=self.state_dir,
                    phase_config=phase_config,
                )
            except Exception:
                pass
            on_crash = phase_config.on_crash
            if on_crash == "retry":
                spec = self.db.get_spec(spec_id)
                done = spec.get("tasks_done", 0) if spec else 0
                total = spec.get("tasks_total", 0) if spec else 0
                self.db.requeue(spec_id, done, total)
                logger.info("Custom phase '%s' crashed, requeueing %s", phase, spec_id)
            else:
                self.db.fail(
                    spec_id,
                    f"Phase '{phase}' failed with exit code {exit_code}",
                )
            return

        # Run post-phase guardrail hooks on success
        try:
            from lib.guardrail_runner import run_hooks
            _post_result = run_hooks(
                hook_point=f"post-{phase}",
                spec_id=spec_id,
                spec_path=spec_path,
                state_dir=self.state_dir,
                phase_config=phase_config,
            )
            if not _post_result.get("passed", True):
                # Strict gate blocked — GATE-FAIL task appended, requeue to execute
                spec = self.db.get_spec(spec_id)
                done = spec.get("tasks_done", 0) if spec else 0
                total = spec.get("tasks_total", 0) if spec else 0
                self.db.requeue(spec_id, done, total)
                self.db.update_spec_fields(spec_id, phase="execute")
                logger.warning(
                    "Custom phase '%s' post-hook blocked for %s — requeueing to execute",
                    phase,
                    spec_id,
                )
                return
        except Exception:
            logger.exception(
                "guardrail_runner raised for custom phase '%s' post-hooks for %s",
                phase,
                spec_id,
            )

        # Read spec content for signal detection
        spec_content = ""
        if spec_path and os.path.isfile(spec_path):
            try:
                spec_content = Path(spec_path).read_text(encoding="utf-8")
            except Exception:
                pass

        approve_signal = phase_config.approve_signal
        reject_signal = phase_config.reject_signal

        if approve_signal and approve_signal in spec_content:
            self._advance_pipeline(spec_id, phase)
        elif reject_signal and reject_signal in spec_content:
            on_reject = phase_config.on_reject
            spec = self.db.get_spec(spec_id)
            done = spec.get("tasks_done", 0) if spec else 0
            total = spec.get("tasks_total", 0) if spec else 0
            if on_reject.startswith("requeue:"):
                target_phase = on_reject[len("requeue:"):]
                self.db.requeue(spec_id, done, total)
                self.db.update_spec_fields(spec_id, phase=target_phase)
                logger.info(
                    "Custom phase '%s' rejected for %s, requeueing to '%s'",
                    phase,
                    spec_id,
                    target_phase,
                )
            elif on_reject == "fail":
                self.db.fail(spec_id, f"Phase '{phase}' rejected")
            else:
                self.db.requeue(spec_id, done, total)
        else:
            # Neither signal found — treat as on_crash
            on_crash = phase_config.on_crash
            if on_crash == "retry":
                spec = self.db.get_spec(spec_id)
                done = spec.get("tasks_done", 0) if spec else 0
                total = spec.get("tasks_total", 0) if spec else 0
                self.db.requeue(spec_id, done, total)
            else:
                self.db.fail(
                    spec_id,
                    f"Phase '{phase}' did not produce expected signal",
                )

    # ── Main loop ────────────────────────────────────────────────────

    def run(self) -> None:
        """Main daemon entry point. Loads workers, recovers stuck
        specs, then enters the poll loop."""
        # Ensure directories exist
        for d in [self.queue_dir, self.log_dir, self.hooks_dir]:
            os.makedirs(d, exist_ok=True)

        # Acquire exclusive daemon lock (exits if another daemon is running)
        self._daemon_lock.acquire()

        logger.info("Daemon started (PID %d)", os.getpid())

        # Load worker definitions from config
        self.load_workers()

        # Load phase configs from ~/.boi/phases/ (hot-reloaded each cycle)
        self._load_phases()

        # Startup recovery: reset specs stuck in 'running' with dead PIDs
        recovered = self.db.recover_running_specs()
        if recovered:
            logger.warning(
                "Recovered %d spec(s) stuck in 'running': %s",
                len(recovered),
                ", ".join(recovered),
            )

        # Reconcile orphaned running specs (not caught by recover_running_specs
        # because the workers table no longer has current_spec_id set).
        reconciled = self.reconcile_stale_specs()
        if reconciled:
            logger.warning(
                "Reconciled %d orphaned spec(s): %s",
                len(reconciled),
                ", ".join(reconciled),
            )

        # Set last_reconcile so first periodic check fires after reconcile_interval
        self._last_reconcile = time.time()

        # Poll loop
        poll_cycle = 0
        try:
            while not self._shutdown_requested:
                self.check_worker_completions()
                self.dispatch_specs()
                self.write_state_snapshot()
                self.write_heartbeat()
                self._reload_phases_if_changed()
                if _coord_cleanup_expired is not None:
                    try:
                        _coord_cleanup_expired(self.db_path)
                    except Exception:
                        pass  # coordination tables may not exist yet

                poll_cycle += 1
                if poll_cycle % SELF_HEAL_INTERVAL == 0:
                    self.self_heal()

                # Periodic liveness check for running specs
                now = time.time()
                if now - self._last_reconcile >= self.reconcile_interval:
                    periodic_requeued = self.reconcile_stale_specs()
                    if periodic_requeued:
                        logger.warning(
                            "Periodic reconciliation requeued %d spec(s): %s",
                            len(periodic_requeued),
                            ", ".join(periodic_requeued),
                        )
                    self._last_reconcile = now

                # Sleep in small increments so we can respond to signals
                for _ in range(self.poll_interval * 10):
                    if self._shutdown_requested:
                        break
                    time.sleep(0.1)
        finally:
            self.shutdown()

    # ── Shutdown ─────────────────────────────────────────────────────

    def shutdown(self) -> None:
        """Kill all active workers via process groups, clean up."""
        logger.info("Daemon shutting down")

        # Collect active worker PIDs for process group kills
        active_procs = list(self.worker_procs.items())

        if active_procs:
            # Phase 1: SIGTERM to each process group
            for worker_id, proc in active_procs:
                try:
                    pgid = os.getpgid(proc.pid)
                    os.killpg(pgid, signal.SIGTERM)
                    logger.info(
                        "Sent SIGTERM to process group %d "
                        "(worker %s, pid %d)",
                        pgid,
                        worker_id,
                        proc.pid,
                    )
                except (ProcessLookupError, PermissionError):
                    pass

            # Phase 2: Wait up to 10 seconds for graceful exit
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                still_alive = [
                    (wid, p)
                    for wid, p in active_procs
                    if p.poll() is None
                ]
                if not still_alive:
                    break
                time.sleep(0.5)

            # Phase 3: SIGKILL any remaining
            for worker_id, proc in active_procs:
                if proc.poll() is None:
                    try:
                        pgid = os.getpgid(proc.pid)
                        os.killpg(pgid, signal.SIGKILL)
                        logger.warning(
                            "Sent SIGKILL to process group %d "
                            "(worker %s)",
                            pgid,
                            worker_id,
                        )
                    except (ProcessLookupError, PermissionError):
                        pass

        self.worker_procs.clear()

        # Release daemon lock (also removes PID file)
        self._daemon_lock.release()

        # Close database
        self.db.close()
        logger.info("Daemon stopped")

    # ── Worker loading ───────────────────────────────────────────────

    def load_workers(self) -> None:
        """Read config.json and register each worker in the database."""
        with open(self.config_path, encoding="utf-8") as f:
            config = json.load(f)

        workers = config.get("workers", [])

        # Read global worker timeout if present
        self.default_worker_timeout = config.get(
            "worker_timeout_seconds", DEFAULT_WORKER_TIMEOUT
        )

        for w in workers:
            worker_id = w["id"]
            worktree_path = w.get(
                "worktree_path", w.get("checkout_path", "")
            )
            self.db.register_worker(worker_id, worktree_path)

        logger.info("Loaded %d worker(s) from config", len(workers))

    # ── Startup reconciliation ───────────────────────────────────────

    def reconcile_stale_specs(self) -> list[str]:
        """Requeue specs stuck in 'running' with no live worker.

        Two cases are handled:
        1. No worker row has current_spec_id pointing to the spec
           (orphaned — e.g. workers table reset on restart while spec
           was still marked running).
        2. A worker row claims the spec but the PID is dead or is not
           tracked in this daemon instance's worker_procs (stale from
           a prior daemon run).

        Returns list of spec IDs that were requeued.
        """
        requeued: list[str] = []

        cursor = self.db.conn.execute(
            "SELECT id, last_worker FROM specs WHERE status = 'running'"
        )
        running_specs = cursor.fetchall()

        for spec_row in running_specs:
            spec_id = spec_row["id"]
            last_worker = spec_row["last_worker"]

            worker_row = self.db.conn.execute(
                "SELECT id, current_pid FROM workers WHERE current_spec_id = ?",
                (spec_id,),
            ).fetchone()

            should_requeue = False

            if worker_row is None:
                # No worker claims this spec — orphaned
                should_requeue = True
            else:
                pid = worker_row["current_pid"]
                if pid is None:
                    should_requeue = True
                elif worker_row["id"] not in self.worker_procs:
                    # PID is from a previous daemon instance; not tracked here
                    should_requeue = True
                else:
                    try:
                        os.kill(pid, 0)
                    except (ProcessLookupError, PermissionError):
                        should_requeue = True

            if should_requeue:
                self.db.conn.execute(
                    "UPDATE specs SET status = 'requeued', last_worker = NULL "
                    "WHERE id = ?",
                    (spec_id,),
                )
                logger.warning(
                    "Reconciliation: requeued %s (worker %s is dead)",
                    spec_id,
                    last_worker or "unknown",
                )
                requeued.append(spec_id)

        if requeued:
            self.db.conn.commit()

        return requeued

    # ── Dispatch (Task 7) ───────────────────────────────────────────

    def dispatch_specs(self) -> None:
        """Assign queued specs and parallel tasks to free workers.

        Each tick:
          1. Pick new specs from the queue; parallel specs are populated
             into the tasks table and no worker is consumed.
          2. Dispatch parallel tasks for all running specs (including
             those just populated in step 1).
        Loops until no free worker remains.
        """
        # Phase 1: dispatch new specs from the queue
        while not self._shutdown_requested:
            worker = self.db.get_free_worker()
            if worker is None:
                break

            spec = self.db.pick_next_spec()
            if spec is None:
                break

            try:
                self.assign_spec_to_worker(spec, worker)
            except Exception:
                logger.exception(
                    "Failed to assign %s to %s",
                    spec["id"],
                    worker["id"],
                )

        # Phase 2: dispatch parallel tasks for all running specs
        # (including any parallel specs just transitioned to running above)
        self._dispatch_parallel_tasks()

    def _dispatch_parallel_tasks(self) -> None:
        """Assign free workers to parallel-eligible tasks in running specs.

        For each running spec that has tasks populated in the DB,
        find tasks that are PENDING with all deps satisfied, and assign
        a free worker to each (up to available workers).
        """
        from lib.daemon_ops import find_parallel_assignments

        running_specs = self.db.get_queue()
        running_specs = [s for s in running_specs if s.get("status") == "running"]

        for spec in running_specs:
            spec_id = spec["id"]
            spec_path = spec.get("spec_path", "")
            if not spec_path or not os.path.isfile(spec_path):
                continue

            # Only apply task-level dispatch to specs that have tasks in DB
            db_tasks = self.db.get_tasks_for_spec(spec_id)
            if not db_tasks:
                continue

            eligible = self.db.get_eligible_task_ids(spec_id)
            if not eligible:
                continue

            for task_id in eligible:
                worker = self.db.get_free_worker()
                if worker is None:
                    return  # No free workers remain

                try:
                    self._assign_task_to_worker(spec, worker, task_id)
                except Exception:
                    logger.exception(
                        "Failed to assign task %s of %s to %s",
                        task_id,
                        spec_id,
                        worker["id"],
                    )

    def _assign_task_to_worker(
        self,
        spec: dict[str, Any],
        worker: dict[str, Any],
        task_id: str,
    ) -> None:
        """Assign a single task to a worker for parallel execution.

        The spec is already running — do NOT call set_running here as
        that would incorrectly increment the iteration counter.
        """
        spec_id = spec["id"]
        worker_id = worker["id"]
        phase = spec.get("phase", "execute")

        # Spec is already running; just read the current iteration.
        spec = self.db.get_spec(spec_id)
        assert spec is not None
        iteration = spec["iteration"]

        self.db.assign_worker(worker_id, spec_id, pid=0, phase=phase, task_id=task_id)
        self.db.assign_task_to_worker(spec_id, task_id, worker_id)

        # Create a fresh per-task worktree so parallel tasks are isolated.
        task_worktree_path = worker["worktree_path"]  # fallback
        try:
            from lib.task_worktree import (
                create_task_worktree,
                get_main_repo_from_worker,
            )
            main_repo = get_main_repo_from_worker(worker["worktree_path"])
            if main_repo:
                wt_info = create_task_worktree(main_repo, worker_id, spec_id, task_id)
                task_worktree_path = wt_info["worktree_path"]
                self.db.update_task_worktree(
                    spec_id, task_id,
                    wt_info["worktree_path"],
                    wt_info["branch_name"],
                )
                logger.info(
                    "Fresh worktree for task %s/%s: %s (branch=%s)",
                    spec_id, task_id, task_worktree_path, wt_info["branch_name"],
                )
            else:
                logger.warning(
                    "Could not resolve main repo for worker %s; "
                    "using shared worktree for task %s/%s",
                    worker_id, spec_id, task_id,
                )
        except Exception:
            logger.exception(
                "Failed to create task worktree for %s/%s; "
                "falling back to shared worktree",
                spec_id, task_id,
            )

        timeout = spec.get("worker_timeout_seconds")
        if timeout is None:
            phase_config = self.phase_configs.get(phase)
            if phase_config is not None and phase_config.timeout > 0:
                timeout = phase_config.timeout

        try:
            proc = self.launch_worker(
                spec_id=spec_id,
                worktree=task_worktree_path,
                spec_path=spec["spec_path"],
                iteration=iteration,
                phase=phase,
                worker_id=worker_id,
                timeout=timeout,
                task_id=task_id,
            )
        except Exception:
            logger.exception(
                "Failed to launch worker for task %s of %s on %s",
                task_id, spec_id, worker_id,
            )
            self.db.free_worker(worker_id)
            self.db.complete_task(spec_id, task_id, "FAILED", "Worker launch failed")
            return

        self.db.assign_worker(worker_id, spec_id, pid=proc.pid, phase=phase, task_id=task_id)
        self.db.register_process(
            pid=proc.pid, spec_id=spec_id, worker_id=worker_id,
            iteration=iteration, phase=phase,
        )
        self.worker_procs[worker_id] = proc

        logger.info(
            "Assigned task %s of %s to %s (pid=%d, iteration=%d)",
            task_id, spec_id, worker_id, proc.pid, iteration,
        )

    def assign_spec_to_worker(
        self,
        spec: dict[str, Any],
        worker: dict[str, Any],
    ) -> None:
        """Mark worker busy, launch subprocess, register PID.

        Steps:
          1. set_running (assigning -> running, increments iteration
             for execute phase)
          2. assign_worker in DB (marks worker busy BEFORE launch)
          3. launch_worker subprocess
          4. register_process in DB
        On launch failure, free the worker and requeue the spec.
        """
        spec_id = spec["id"]
        worker_id = worker["id"]
        phase = spec.get("phase", "execute")

        # 1. Transition spec to running (gets iteration set)
        self.db.set_running(spec_id, worker_id, phase)

        # Re-read spec to get the updated iteration
        spec = self.db.get_spec(spec_id)
        assert spec is not None
        iteration = spec["iteration"]

        # For parallel specs (any task has blocked_by deps): populate tasks table
        # and let _dispatch_parallel_tasks handle per-task assignment.
        # The passed-in worker is returned to the pool unused.
        if phase == "execute":
            spec_path = spec.get("spec_path", "")
            try:
                from pathlib import Path as _Path
                from lib.spec_parser import parse_boi_spec as _parse_boi_spec
                content = _Path(spec_path).read_text(encoding="utf-8")
                parsed_tasks = _parse_boi_spec(content)
                if parsed_tasks:
                    self.db.populate_tasks_from_spec(spec_id, parsed_tasks)
                    logger.info(
                        "Parallel spec %s: populated %d tasks, "
                        "worker %s returned to pool",
                        spec_id, len(parsed_tasks), worker_id,
                    )
                    return
            except Exception:
                logger.exception(
                    "Failed to check parallel tasks for %s; using sequential flow",
                    spec_id,
                )

        # 2. Assign worker in DB before launching (no PID yet,
        #    will update after launch)
        self.db.assign_worker(worker_id, spec_id, pid=0, phase=phase)

        # Determine timeout: spec-level first, then phase config, then default
        timeout = spec.get("worker_timeout_seconds")
        if timeout is None:
            phase_config = self.phase_configs.get(phase)
            if phase_config is not None and phase_config.timeout > 0:
                timeout = phase_config.timeout

        try:
            # 3. Launch worker subprocess
            proc = self.launch_worker(
                spec_id=spec_id,
                worktree=worker["worktree_path"],
                spec_path=spec["spec_path"],
                iteration=iteration,
                phase=phase,
                worker_id=worker_id,
                timeout=timeout,
            )
        except Exception:
            logger.exception(
                "Failed to launch worker for %s on %s",
                spec_id,
                worker_id,
            )
            self.db.free_worker(worker_id)
            self.db.requeue(
                spec_id,
                tasks_done=spec.get("tasks_done", 0),
                tasks_total=spec.get("tasks_total", 0),
            )
            return

        # 4. Update worker record with actual PID and register process
        self.db.assign_worker(
            worker_id, spec_id, pid=proc.pid, phase=phase
        )
        self.db.register_process(
            pid=proc.pid,
            spec_id=spec_id,
            worker_id=worker_id,
            iteration=iteration,
            phase=phase,
        )
        self.worker_procs[worker_id] = proc

        logger.info(
            "Assigned %s to %s (pid=%d, iteration=%d, phase=%s)",
            spec_id,
            worker_id,
            proc.pid,
            iteration,
            phase,
        )

    def launch_worker(
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
        """Spawn a worker.py subprocess in a new session.

        Args:
            spec_id: Queue ID of the spec.
            worktree: Path to the worker's checkout/worktree.
            spec_path: Path to the spec file (queue copy).
            iteration: Current iteration number.
            phase: Phase to execute (execute|critic|evaluate|decompose).
            worker_id: ID of the worker slot.
            timeout: Optional per-spec timeout in seconds.
            task_id: Optional specific task ID for parallel execution.

        Returns:
            The Popen object for the spawned worker process.
        """
        worker_script = os.path.join(self.script_dir, "worker.py")

        cmd = [
            sys.executable,
            worker_script,
            spec_id,
            worktree,
            spec_path,
            str(iteration),
            "--phase",
            phase,
        ]

        if timeout is not None:
            cmd.extend(["--timeout", str(timeout)])

        # Set up environment
        env = os.environ.copy()
        env["WORKER_ID"] = worker_id
        if task_id is not None:
            env["BOI_TASK_ID"] = task_id

        # Log file for this iteration
        log_file = os.path.join(
            self.log_dir, f"{spec_id}-iter-{iteration}.log"
        )
        os.makedirs(self.log_dir, exist_ok=True)

        log_fh = open(log_file, "a", encoding="utf-8")

        proc = subprocess.Popen(
            cmd,
            stdout=log_fh,
            stderr=log_fh,
            env=env,
            cwd=worktree,
            start_new_session=True,
        )

        # Close the file handle in the parent process. The child
        # inherits its own copy via the file descriptor.
        log_fh.close()

        return proc

    # ── Completion handling (Task 8) ──────────────────────────────────

    def check_worker_completions(self) -> None:
        """Check active workers for exits and timeouts.

        Iterates all tracked worker subprocesses. If a worker has
        exited (proc.poll() is not None), calls process_worker_completion.
        If a worker is still running but has exceeded its timeout,
        calls handle_worker_timeout.
        """
        for worker_id in list(self.worker_procs.keys()):
            proc = self.worker_procs.get(worker_id)
            if proc is None:
                continue

            exit_code = proc.poll()
            if exit_code is not None:
                # Worker exited normally or with error
                self.process_worker_completion(worker_id, exit_code)
            elif self.is_worker_timed_out(worker_id):
                # Worker exceeded its timeout
                self.handle_worker_timeout(worker_id)

    def is_worker_timed_out(self, worker_id: str) -> bool:
        """Check if a worker has exceeded its spec's timeout.

        Uses the spec's worker_timeout_seconds if set, otherwise
        falls back to the daemon's default_worker_timeout. Compares
        elapsed time since the worker's start_time in the database.

        Returns:
            True if the worker has exceeded its allowed runtime.
        """
        worker = self.db.get_worker(worker_id)
        if worker is None or worker["current_spec_id"] is None:
            return False

        start_time_str = worker.get("start_time")
        if not start_time_str:
            return False

        spec = self.db.get_spec(worker["current_spec_id"])
        if spec is None:
            return False

        timeout = spec.get("worker_timeout_seconds")
        if timeout is None:
            timeout = self.default_worker_timeout

        # Parse start_time (may have |ticks suffix from make_started_at)
        ts_str = start_time_str.split("|")[0]
        try:
            start_dt = datetime.fromisoformat(ts_str)
        except ValueError:
            return False

        elapsed = (
            datetime.now(timezone.utc) - start_dt
        ).total_seconds()
        return elapsed > timeout

    def handle_worker_timeout(self, worker_id: str) -> None:
        """Kill a timed-out worker via its process group.

        Steps:
          1. Send SIGTERM to the process group.
          2. Wait up to 2 seconds for graceful exit.
          3. Send SIGKILL if still alive.
          4. Record exit code 124 (standard timeout) in the DB.
          5. Process the completion as a timeout.
        """
        proc = self.worker_procs.get(worker_id)
        if proc is None:
            return

        worker = self.db.get_worker(worker_id)
        spec_id = worker["current_spec_id"] if worker else None

        logger.warning(
            "Worker %s timed out (spec=%s, pid=%d). Killing.",
            worker_id,
            spec_id,
            proc.pid,
        )

        # Phase 1: SIGTERM to process group
        try:
            pgid = os.getpgid(proc.pid)
            os.killpg(pgid, signal.SIGTERM)
        except (ProcessLookupError, PermissionError):
            pass

        # Phase 2: Wait up to 2 seconds
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                break
            time.sleep(0.1)

        # Phase 3: SIGKILL if still alive
        if proc.poll() is None:
            try:
                pgid = os.getpgid(proc.pid)
                os.killpg(pgid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
            proc.wait()

        # Process as timeout with exit code 124
        self.process_worker_completion(worker_id, exit_code=124)

    def process_worker_completion(
        self,
        worker_id: str,
        exit_code: int,
    ) -> None:
        """Handle a worker that has finished (normally or via timeout).

        Steps:
          1. End the process record in the DB.
          2. Delegate to the phase-specific completion handler from
             daemon_ops.py.
          3. Free the worker.
          4. Remove the subprocess from worker_procs.
        """
        worker = self.db.get_worker(worker_id)
        if worker is None:
            self.worker_procs.pop(worker_id, None)
            return

        spec_id = worker.get("current_spec_id")
        pid = worker.get("current_pid")
        phase = worker.get("current_phase", "execute")
        task_id = worker.get("current_task_id")

        if spec_id is None:
            self.worker_procs.pop(worker_id, None)
            return

        spec = self.db.get_spec(spec_id)
        if spec is None:
            self.db.free_worker(worker_id)
            self.worker_procs.pop(worker_id, None)
            return

        # 1. End process in DB
        if pid:
            self.db.end_process(pid, spec_id, exit_code)

        logger.info(
            "Worker %s completed: spec=%s, phase=%s, task=%s "
            "exit_code=%d, iteration=%d",
            worker_id,
            spec_id,
            phase,
            task_id,
            exit_code,
            spec.get("iteration", 0),
        )

        # 2a. Branch lifecycle: create or ensure spec branch before phase handler
        _early_repo = self._extract_target_repo(spec.get("spec_path", ""))
        if _early_repo:
            if phase == "execute":
                _base_branch_path = os.path.join(
                    self.queue_dir, f"{spec_id}.base-branch"
                )
                if not os.path.exists(_base_branch_path):
                    self._create_spec_branch(spec_id, _early_repo)
            elif phase in ("task-verify", "code-review"):
                self._ensure_on_spec_branch(spec_id, _early_repo)

        # 2b. For parallel task workers: update task state and check spec completion.
        if task_id:
            task_status = "DONE" if exit_code == 0 else "FAILED"
            self.db.complete_task(spec_id, task_id, task_status)
            logger.info(
                "Task %s of %s marked %s (exit_code=%d)",
                task_id, spec_id, task_status, exit_code,
            )

            self._cleanup_task_worktree(spec_id, task_id, worker)

            if self.db.all_tasks_terminal(spec_id):
                db_tasks = self.db.get_tasks_for_spec(spec_id)
                tasks_done = sum(1 for t in db_tasks if t["status"] == "DONE")
                tasks_total = len(db_tasks)

                now_iso = self.db._now_iso()
                spec_fresh = self.db.get_spec(spec_id)
                first_started = min(
                    (t.get("started_at") or now_iso for t in db_tasks),
                    default=now_iso,
                )
                try:
                    dur = int((
                        datetime.fromisoformat(now_iso)
                        - datetime.fromisoformat(first_started)
                    ).total_seconds())
                except Exception:
                    dur = 0
                final_exit = 0 if tasks_done == tasks_total else 1
                try:
                    self.db.insert_iteration(
                        spec_id=spec_id,
                        iteration=(spec_fresh or spec).get("iteration", 0),
                        phase=phase,
                        worker_id=worker_id,
                        started_at=first_started,
                        ended_at=now_iso,
                        duration_seconds=dur,
                        tasks_completed=tasks_done,
                        exit_code=final_exit,
                    )
                except Exception:
                    logger.warning(
                        "Could not insert iteration record for %s: %s",
                        spec_id, "constraint conflict (ignored)",
                    )

                if self.db.any_task_failed(spec_id):
                    self.db.fail(spec_id, reason="One or more parallel tasks failed")
                else:
                    self._merge_task_branches_for_spec(spec_id, db_tasks, worker)
                    self.db.complete(spec_id, tasks_done=tasks_done, tasks_total=tasks_total)
                logger.info(
                    "All tasks terminal for %s (%d/%d done)",
                    spec_id, tasks_done, tasks_total,
                )
            self.db.free_worker(worker_id)
            self.worker_procs.pop(worker_id, None)
            return

        # 2c. Delegate to phase-specific handler for non-parallel workers
        try:
            self._dispatch_phase_completion(
                spec_id=spec_id,
                phase=phase,
                exit_code=exit_code,
                worker_id=worker_id,
            )
        except Exception:
            logger.exception(
                "Phase handler failed for %s (phase=%s)",
                spec_id,
                phase,
            )

        # 2b. Emit lifecycle events based on updated spec status
        try:
            spec_after = self.db.get_spec(spec_id)
            if spec_after is not None:
                new_status = spec_after.get("status", "")
                target_repo = self._extract_target_repo(
                    spec_after.get("spec_path", "")
                )
                tasks_done = spec_after.get("tasks_done", 0)
                tasks_total = spec_after.get("tasks_total", 0)
                iteration_num = spec_after.get("iteration", 0)

                spec_title = self._extract_spec_title(
                    spec_after.get("spec_path", "")
                )

                # Per-iteration commit: stage and commit any changed files to
                # the spec branch after each execute iteration.
                if phase == "execute" and target_repo and new_status in (
                    "queued",
                    "completed",
                ):
                    self._commit_iteration(spec_id, target_repo, iteration_num)

                if new_status == "completed":
                    if target_repo:
                        if not self._merge_spec_branch(spec_id, target_repo):
                            logger.warning(
                                "[spec-branch] Squash-merge failed for %s"
                                " -- branch boi/%s preserved in %s",
                                spec_id,
                                spec_id,
                                target_repo,
                            )
                    ship_ok = self._run_ship_phase(spec_id, spec_after)
                    if not ship_ok:
                        logger.info("[ship] Ship phase failed for %s — skipping completion", spec_id)
                        return
                    self._review_committed_output(
                        spec_id,
                        target_repo,
                        spec_after.get("spec_path", ""),
                    )
                    # Re-check -- review may have requeued the spec
                    spec_refreshed = self.db.get_spec(spec_id)
                    refreshed_status = spec_refreshed.get("status", "") if spec_refreshed else ""
                    if refreshed_status == "completed":
                        self.emit_hex_event("boi.spec.completed", {
                            "spec_id": spec_id,
                            "spec_title": spec_title,
                            "target_repo": target_repo,
                            "tasks_done": tasks_done,
                            "tasks_total": tasks_total,
                        })
                    else:
                        logger.info(
                            "Spec %s was requeued by post-commit review"
                            " -- skipping completion event",
                            spec_id,
                        )
                elif new_status == "failed":
                    if target_repo:
                        logger.info(
                            "Spec %s failed -- changes preserved on branch"
                            " boi/%s in %s",
                            spec_id,
                            spec_id,
                            target_repo,
                        )
                    self.emit_hex_event("boi.spec.failed", {
                        "spec_id": spec_id,
                        "spec_title": spec_title,
                        "failure_reason": spec_after.get(
                            "failure_reason", ""
                        ),
                        "iteration": iteration_num,
                    })

                self.emit_hex_event("boi.iteration.done", {
                    "spec_id": spec_id,
                    "iteration": iteration_num,
                    "tasks_completed": tasks_done,
                    "tasks_added": 0,
                })
        except Exception:
            logger.exception(
                "Failed to emit lifecycle events for %s", spec_id
            )

        # 3. Free worker
        self.db.free_worker(worker_id)

        # 4. Remove from tracked procs
        self.worker_procs.pop(worker_id, None)

    def _cleanup_task_worktree(
        self,
        spec_id: str,
        task_id: str,
        worker: dict[str, Any],
    ) -> None:
        """Remove the dedicated worktree for a completed task, if one was created."""
        try:
            task_rows = self.db.get_tasks_for_spec(spec_id)
            task_row = next((t for t in task_rows if t["task_id"] == task_id), None)
            if task_row is None:
                return
            wt_path = task_row.get("worktree_path") or ""
            if not wt_path or wt_path == worker.get("worktree_path"):
                # No dedicated worktree was created (fell back to shared path).
                return
            from lib.task_worktree import (
                get_main_repo_from_worker,
                remove_task_worktree,
            )
            main_repo = get_main_repo_from_worker(worker["worktree_path"])
            if main_repo:
                remove_task_worktree(main_repo, wt_path)
            else:
                logger.warning(
                    "Cannot resolve main repo to remove task worktree %s", wt_path
                )
        except Exception:
            logger.exception(
                "Error cleaning up task worktree for %s/%s", spec_id, task_id
            )

    def _merge_task_branches_for_spec(
        self,
        spec_id: str,
        db_tasks: list[dict[str, Any]],
        worker: dict[str, Any],
    ) -> None:
        """Merge all DONE task branches into the spec branch at level boundary.

        Only runs if at least one task has a non-null branch_name, indicating
        fresh task worktrees were used. Marks the spec needs_review if any
        merge produces conflicts.
        """
        branches = [t for t in db_tasks if t.get("branch_name")]
        if not branches:
            return

        try:
            from lib.task_worktree import (
                get_main_repo_from_worker,
                merge_level_branches,
            )
            main_repo = get_main_repo_from_worker(worker["worktree_path"])
            if not main_repo:
                logger.warning(
                    "Cannot resolve main repo to merge task branches for %s", spec_id
                )
                return

            result = merge_level_branches(main_repo, spec_id, db_tasks)
            status = result.get("merge_status", "nothing_to_merge")

            if status == "merged":
                logger.info(
                    "Merged %d task branches for %s: %s",
                    len(result.get("merged_tasks", [])),
                    spec_id,
                    result.get("merged_tasks"),
                )
            elif status == "conflict":
                logger.warning(
                    "Merge conflict in spec %s: tasks=%s files=%s",
                    spec_id,
                    result.get("conflicting_tasks"),
                    result.get("conflicting_files"),
                )
                # Mark spec needs_review so a human can resolve the conflict.
                self.db.set_needs_review(
                    spec_id,
                    experiment_tasks=result.get("conflicting_tasks", []),
                )
        except Exception:
            logger.exception(
                "Error merging task branches for %s", spec_id
            )

    def _dispatch_phase_completion(
        self,
        spec_id: str,
        phase: str,
        exit_code: int,
        worker_id: str,
    ) -> None:
        """Delegate to the appropriate daemon_ops phase handler.

        For now, calls the existing daemon_ops functions which use
        the file-based queue. These will be refactored in Phase 3
        (Tasks 10-11) to use Database directly.
        """
        spec = self.db.get_spec(spec_id)
        if spec is None:
            return

        spec_path = spec.get("spec_path", "")
        events_dir = os.path.join(self.state_dir, "events")
        os.makedirs(events_dir, exist_ok=True)

        try:
            from lib import daemon_ops
        except ImportError:
            logger.warning(
                "daemon_ops not available, using fallback "
                "completion logic for %s",
                spec_id,
            )
            self._fallback_completion(spec_id, exit_code)
            return

        # Check for a phase config to determine routing
        phase_config = self.phase_configs.get(phase)

        if phase_config is not None:
            handler = phase_config.completion_handler
            if handler.startswith("builtin:"):
                # Route to existing builtin handler by name
                builtin_name = handler[len("builtin:"):]
                if builtin_name == "execute":
                    ctx = daemon_ops.CompletionContext(
                        queue_dir=self.queue_dir,
                        events_dir=events_dir,
                        hooks_dir=self.hooks_dir,
                        log_dir=self.log_dir,
                        script_dir=self.script_dir,
                        db=self.db,
                    )
                    daemon_ops.process_worker_completion(
                        ctx=ctx,
                        queue_id=spec_id,
                        exit_code=str(exit_code),
                    )
                    return
                elif builtin_name == "task-verify":
                    daemon_ops.process_critic_completion(
                        queue_dir=self.queue_dir,
                        queue_id=spec_id,
                        events_dir=events_dir,
                        hooks_dir=self.hooks_dir,
                        spec_path=spec_path,
                        db=self.db,
                    )
                    return
                elif builtin_name == "decompose":
                    daemon_ops.process_decomposition_completion(
                        queue_dir=self.queue_dir,
                        queue_id=spec_id,
                        events_dir=events_dir,
                        spec_path=spec_path,
                        exit_code=str(exit_code),
                        db=self.db,
                    )
                    return
                elif builtin_name == "evaluate":
                    daemon_ops.process_evaluation_completion(
                        queue_dir=self.queue_dir,
                        queue_id=spec_id,
                        events_dir=events_dir,
                        hooks_dir=self.hooks_dir,
                        spec_path=spec_path,
                        exit_code=str(exit_code),
                        db=self.db,
                    )
                    return
                # Unknown builtin — fall through to legacy routing below
                logger.warning(
                    "Unknown builtin completion_handler '%s' for phase '%s', "
                    "falling back to legacy routing",
                    handler,
                    phase,
                )
            else:
                # No completion_handler set: use generic signal-based handler
                self._handle_custom_phase_completion(
                    spec_id=spec_id,
                    phase=phase,
                    phase_config=phase_config,
                    exit_code=exit_code,
                    spec_path=spec_path,
                )
                return

        # Legacy hardcoded routing (backward compat when no phase configs loaded)
        if phase == "execute":
            ctx = daemon_ops.CompletionContext(
                queue_dir=self.queue_dir,
                events_dir=events_dir,
                hooks_dir=self.hooks_dir,
                log_dir=self.log_dir,
                script_dir=self.script_dir,
                db=self.db,
            )
            daemon_ops.process_worker_completion(
                ctx=ctx,
                queue_id=spec_id,
                exit_code=str(exit_code),
            )
        elif phase == "task-verify":
            daemon_ops.process_critic_completion(
                queue_dir=self.queue_dir,
                queue_id=spec_id,
                events_dir=events_dir,
                hooks_dir=self.hooks_dir,
                spec_path=spec_path,
                db=self.db,
            )
        elif phase == "decompose":
            daemon_ops.process_decomposition_completion(
                queue_dir=self.queue_dir,
                queue_id=spec_id,
                events_dir=events_dir,
                spec_path=spec_path,
                exit_code=str(exit_code),
                db=self.db,
            )
        elif phase == "evaluate":
            daemon_ops.process_evaluation_completion(
                queue_dir=self.queue_dir,
                queue_id=spec_id,
                events_dir=events_dir,
                hooks_dir=self.hooks_dir,
                spec_path=spec_path,
                exit_code=str(exit_code),
                db=self.db,
            )
        else:
            logger.warning("Unknown phase '%s' for %s", phase, spec_id)
            self._fallback_completion(spec_id, exit_code)

    def _fallback_completion(
        self,
        spec_id: str,
        exit_code: int,
    ) -> None:
        """Simple fallback when daemon_ops is not available.

        If exit code is 0, check if all tasks are done and complete
        or requeue. Otherwise, record failure and requeue or fail.
        """
        spec = self.db.get_spec(spec_id)
        if spec is None:
            return

        tasks_done = spec.get("tasks_done", 0)
        tasks_total = spec.get("tasks_total", 0)

        if exit_code == 0:
            # Try to read task counts from spec file
            spec_path = spec.get("spec_path", "")
            if spec_path and os.path.isfile(spec_path):
                try:
                    from lib.spec_parser import parse_boi_spec
                    content = Path(spec_path).read_text(
                        encoding="utf-8"
                    )
                    tasks = parse_boi_spec(content)
                    done = sum(
                        1 for t in tasks if t.status == "DONE"
                    )
                    total = len(tasks)
                    pending = sum(
                        1 for t in tasks if t.status == "PENDING"
                    )
                    if pending == 0 and total > 0:
                        self.db.complete(spec_id, done, total)
                        return
                    self.db.requeue(spec_id, done, total)
                    return
                except Exception:
                    pass
            self.db.requeue(spec_id, tasks_done, tasks_total)
        else:
            max_reached = self.db.record_failure(spec_id)
            if max_reached:
                self.db.fail(
                    spec_id,
                    f"Max consecutive failures reached "
                    f"(last exit code: {exit_code})",
                )
            else:
                # Set status to requeued without clearing failure
                # tracking. db.requeue() resets consecutive_failures
                # and cooldown, which we want to preserve after a
                # crash so the cooldown takes effect.
                with self.db.lock:
                    self.db.conn.execute(
                        "UPDATE specs SET status = 'requeued' "
                        "WHERE id = ?",
                        (spec_id,),
                    )
                    self.db._log_event(
                        "requeued",
                        f"Spec requeued after failure "
                        f"(exit code: {exit_code})",
                        spec_id=spec_id,
                    )
                    self.db.conn.commit()

    # ── Self-heal and state snapshot (Task 9) ───────────────────────

    def write_state_snapshot(self) -> None:
        """Write daemon-state.json with worker assignments and queue counts.

        The snapshot is used by monitoring tools (boi status) to display
        daemon health without querying SQLite directly. Written atomically
        via tmp + os.replace.
        """
        now = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

        # Worker assignments
        workers_snapshot: list[dict[str, Any]] = []
        cursor = self.db.conn.execute("SELECT * FROM workers ORDER BY id")
        for row in cursor:
            w = self.db._row_to_dict(row)
            workers_snapshot.append({
                "id": w["id"],
                "current_spec_id": w.get("current_spec_id"),
                "current_pid": w.get("current_pid"),
                "current_phase": w.get("current_phase"),
                "worktree_path": w.get("worktree_path", ""),
            })

        # Queue counts by status
        count_cursor = self.db.conn.execute(
            "SELECT status, COUNT(*) as cnt FROM specs GROUP BY status"
        )
        counts: dict[str, int] = {}
        for row in count_cursor:
            counts[row["status"]] = row["cnt"]

        total = sum(counts.values())

        state = {
            "timestamp": now,
            "pid": os.getpid(),
            "poll_interval": self.poll_interval,
            "workers": workers_snapshot,
            "queue": {
                "total": total,
                "queued": counts.get("queued", 0),
                "requeued": counts.get("requeued", 0),
                "assigning": counts.get("assigning", 0),
                "running": counts.get("running", 0),
                "completed": counts.get("completed", 0),
                "failed": counts.get("failed", 0),
                "canceled": counts.get("canceled", 0),
                "needs_review": counts.get("needs_review", 0),
            },
        }

        h = hashlib.md5(json.dumps(state, sort_keys=True).encode()).hexdigest()
        if h == self._last_snapshot_hash:
            return
        self._last_snapshot_hash = h

        state_path = os.path.join(self.state_dir, "daemon-state.json")
        tmp = state_path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(state, f, indent=2)
        os.replace(tmp, state_path)

    def self_heal(self) -> None:
        """Detect and recover stuck states.

        Runs periodic checks:
        1. Recover specs stuck in 'assigning' (via db.recover_stale_assigning).
        2. Check needs_review timeouts (auto-reject timed-out experiments).
        3. Delegate to daemon_ops.self_heal for remaining checks (stale
           running, max duration, orphaned workers, circular deps,
           blocked_by cleanup, stale locks).
        4. Act on orphaned worker actions by freeing workers in the DB.
        """
        # 1. Recover stuck-assigning specs
        stale_assigning = self.db.recover_stale_assigning()
        if stale_assigning:
            logger.warning(
                "Self-heal: recovered %d stuck-assigning spec(s): %s",
                len(stale_assigning),
                ", ".join(stale_assigning),
            )

        # 2. Check needs_review timeouts
        events_dir = os.path.join(self.state_dir, "events")
        os.makedirs(events_dir, exist_ok=True)
        try:
            from lib import daemon_ops
            auto_rejected = daemon_ops.check_needs_review_timeouts(
                queue_dir=self.queue_dir,
                events_dir=events_dir,
                state_dir=self.state_dir,
                db=self.db,
            )
            if auto_rejected:
                logger.info(
                    "Self-heal: auto-rejected %d needs_review spec(s): %s",
                    len(auto_rejected),
                    ", ".join(auto_rejected),
                )
        except ImportError:
            logger.debug(
                "daemon_ops not available, skipping needs_review check"
            )
        except Exception:
            logger.exception("Error in check_needs_review_timeouts")

        # 3. Build worker_specs map for daemon_ops.self_heal
        worker_specs: dict[str, str] = {}
        cursor = self.db.conn.execute("SELECT * FROM workers")
        for row in cursor:
            w = self.db._row_to_dict(row)
            worker_specs[w["id"]] = w.get("current_spec_id") or ""

        try:
            from lib import daemon_ops
            actions = daemon_ops.self_heal(
                queue_dir=self.queue_dir,
                worker_specs=worker_specs,
                db=self.db,
            )
            for action in actions:
                logger.info(
                    "Self-heal: %s — %s",
                    action.get("action", "unknown"),
                    action.get("detail", ""),
                )
                # Act on orphaned worker results
                if action.get("action") == "orphaned_worker":
                    wid = action.get("worker_id")
                    if wid:
                        self.db.free_worker(wid)
                        self.worker_procs.pop(wid, None)
        except ImportError:
            logger.debug(
                "daemon_ops not available, skipping self_heal checks"
            )
        except Exception:
            logger.exception("Error in daemon_ops.self_heal")

    # ── Hex-events integration ───────────────────────────────────────

    def emit_hex_event(self, event_type: str, payload: dict) -> None:
        """Emit an event to the hex-events bus via hex_emit.py.

        If ~/.hex-events/hex_emit.py is not installed, logs a debug
        message and returns silently (hex-events is optional).

        Args:
            event_type: Event type string, e.g. "boi.spec.completed".
            payload: Dict of event data to pass as JSON.
        """
        hex_emit = os.path.expanduser("~/.hex-events/hex_emit.py")
        if not os.path.isfile(hex_emit):
            logger.debug(
                "hex_emit.py not found at %s, skipping event %s",
                hex_emit,
                event_type,
            )
            return

        try:
            subprocess.run(
                [sys.executable, hex_emit, event_type, json.dumps(payload)],
                timeout=5,
                capture_output=True,
            )
        except Exception as exc:
            logger.debug(
                "Failed to emit hex event %s: %s", event_type, exc
            )

    @staticmethod
    def _extract_target_repo(spec_path: str) -> str:
        """Extract the target repo path from a spec file's 'Target repo:' or 'Target:' field."""
        _PREFIXES = ("**Target repo:**", "**Target:**")
        try:
            content = Path(spec_path).read_text(encoding="utf-8")
            for line in content.splitlines():
                stripped = line.strip()
                for prefix in _PREFIXES:
                    if stripped.startswith(prefix):
                        value = stripped[len(prefix):].strip().strip("`").strip()
                        if value.startswith("~"):
                            value = str(Path(value).expanduser())
                        return value
        except Exception:
            pass
        return ""

    @staticmethod
    def _extract_spec_title(spec_path: str) -> str:
        """Extract the title from a spec file's first '# ' heading."""
        try:
            content = Path(spec_path).read_text(encoding="utf-8")
            for line in content.splitlines():
                stripped = line.strip()
                if stripped.startswith("# "):
                    return stripped[2:].strip()
        except Exception:
            pass
        return ""

    def _create_spec_branch(self, spec_id: str, target_repo: str) -> str:
        """Create branch boi/{spec_id} in target_repo off HEAD.

        If the branch already exists (resume case), checks it out instead.
        Writes the current branch name to queue/{spec_id}.base-branch so we
        know where to merge back.  Returns the branch name on success, "" on
        any git error (caller falls back to current branch).
        """
        if not target_repo:
            return ""

        branch = f"boi/{spec_id}"
        base_branch_path = os.path.join(self.queue_dir, f"{spec_id}.base-branch")

        # Capture current branch before we switch -- needed for base-branch file
        current_branch = "main"
        try:
            result = subprocess.run(
                ["git", "-C", target_repo, "rev-parse", "--abbrev-ref", "HEAD"],
                check=True,
                capture_output=True,
                text=True,
            )
            current_branch = result.stdout.strip() or "main"
        except subprocess.CalledProcessError:
            pass

        # Attempt to create a new branch; fall back to checkout if it exists
        try:
            subprocess.run(
                ["git", "-C", target_repo, "checkout", "-b", branch],
                check=True,
                capture_output=True,
            )
        except subprocess.CalledProcessError:
            # Branch already exists -- just check it out
            try:
                subprocess.run(
                    ["git", "-C", target_repo, "checkout", branch],
                    check=True,
                    capture_output=True,
                )
            except subprocess.CalledProcessError as exc:
                logger.warning(
                    "[spec-branch] Failed to create/checkout %s in %s: %s",
                    branch,
                    target_repo,
                    exc,
                )
                return ""

        # Write base-branch file only when we just created the branch
        # (file absent means first time; don't overwrite on resume)
        if not os.path.exists(base_branch_path):
            try:
                with open(base_branch_path, "w", encoding="utf-8") as fh:
                    fh.write(current_branch)
            except OSError as exc:
                logger.warning(
                    "[spec-branch] Could not write base-branch file for %s: %s",
                    spec_id,
                    exc,
                )

        return branch

    def _ensure_on_spec_branch(self, spec_id: str, target_repo: str) -> bool:
        """Verify target_repo is on branch boi/{spec_id}; check it out if not.

        Returns True if on the correct branch after the call, False on failure.
        """
        if not target_repo:
            return False

        branch = f"boi/{spec_id}"

        try:
            result = subprocess.run(
                ["git", "-C", target_repo, "rev-parse", "--abbrev-ref", "HEAD"],
                check=True,
                capture_output=True,
                text=True,
            )
            current = result.stdout.strip()
        except subprocess.CalledProcessError as exc:
            logger.warning(
                "[spec-branch] Could not determine current branch in %s: %s",
                target_repo,
                exc,
            )
            return False

        if current == branch:
            return True

        try:
            subprocess.run(
                ["git", "-C", target_repo, "checkout", branch],
                check=True,
                capture_output=True,
            )
            return True
        except subprocess.CalledProcessError as exc:
            logger.warning(
                "[spec-branch] Failed to checkout %s in %s: %s",
                branch,
                target_repo,
                exc,
            )
            return False

    def _get_original_branch(self, spec_id: str, target_repo: str) -> str:
        """Read the original branch name from queue/{spec_id}.base-branch.

        Returns the branch name, or "main" as fallback when the file does not
        exist or cannot be read.
        """
        base_branch_path = os.path.join(self.queue_dir, f"{spec_id}.base-branch")
        try:
            with open(base_branch_path, encoding="utf-8") as fh:
                branch = fh.read().strip()
            return branch if branch else "main"
        except OSError:
            return "main"

    def _merge_spec_branch(self, spec_id: str, target_repo: str) -> bool:
        """Squash-merge the spec branch into the original branch.

        Steps:
        1. Checkout the original branch.
        2. git merge --squash boi/{spec_id}.
        3. Commit with a clean message.
        4. Delete the spec branch.
        5. Clean up the base-branch file.
        Returns True on success, False on any error (branch preserved for inspection).
        """
        original_branch = self._get_original_branch(spec_id, target_repo)
        spec_branch = f"boi/{spec_id}"

        try:
            subprocess.run(
                ["git", "-C", target_repo, "checkout", original_branch],
                check=True,
                capture_output=True,
            )
        except subprocess.CalledProcessError as exc:
            logger.warning(
                "[merge-branch] Failed to checkout %s for %s: %s",
                original_branch,
                spec_id,
                exc,
            )
            return False

        try:
            subprocess.run(
                ["git", "-C", target_repo, "merge", "--squash", spec_branch],
                check=True,
                capture_output=True,
            )
        except subprocess.CalledProcessError as exc:
            logger.warning(
                "[merge-branch] Squash merge failed for %s: %s -- branch %s preserved",
                spec_id,
                exc,
                spec_branch,
            )
            try:
                subprocess.run(
                    ["git", "-C", target_repo, "checkout", original_branch],
                    check=True,
                    capture_output=True,
                )
            except subprocess.CalledProcessError:
                pass
            return False

        commit_msg = f"feat: BOI {spec_id} output -- auto-committed by hex-ops"
        try:
            subprocess.run(
                ["git", "-C", target_repo, "commit", "-m", commit_msg],
                check=True,
                capture_output=True,
            )
        except subprocess.CalledProcessError as exc:
            logger.warning(
                "[merge-branch] Commit after squash merge failed for %s: %s",
                spec_id,
                exc,
            )
            try:
                subprocess.run(
                    ["git", "-C", target_repo, "checkout", original_branch],
                    check=True,
                    capture_output=True,
                )
            except subprocess.CalledProcessError:
                pass
            return False

        try:
            subprocess.run(
                ["git", "-C", target_repo, "branch", "-D", spec_branch],
                check=True,
                capture_output=True,
            )
        except subprocess.CalledProcessError as exc:
            logger.warning(
                "[merge-branch] Failed to delete branch %s: %s", spec_branch, exc
            )

        base_branch_path = os.path.join(self.queue_dir, f"{spec_id}.base-branch")
        try:
            os.remove(base_branch_path)
        except OSError:
            pass

        logger.info(
            "[merge-branch] Squash-merged %s into %s in %s",
            spec_branch,
            original_branch,
            target_repo,
        )
        return True

    def _commit_iteration(
        self, spec_id: str, target_repo: str, iteration: int
    ) -> None:
        """Commit changed files to the spec branch after one execute iteration.

        Calls _ensure_on_spec_branch first; returns early if the branch check
        fails or the changed-files manifest is absent/empty.  Clears the
        manifest after a successful commit so the next iteration starts fresh.
        Git errors are logged as warnings and never re-raised.
        """
        if not self._ensure_on_spec_branch(spec_id, target_repo):
            return

        manifest_path = os.path.join(self.queue_dir, f"{spec_id}.changed-files")
        if not os.path.isfile(manifest_path):
            return

        try:
            with open(manifest_path, encoding="utf-8") as fh:
                files = [ln.strip() for ln in fh if ln.strip()]
        except OSError as exc:
            logger.warning(
                "[iter-commit] Could not read manifest for %s: %s",
                spec_id,
                exc,
            )
            return

        if not files:
            return

        try:
            subprocess.run(
                ["git", "-C", target_repo, "add", "--"] + files,
                check=True,
                capture_output=True,
            )
        except subprocess.CalledProcessError as exc:
            logger.warning(
                "[iter-commit] git add failed for %s iter %d: %s",
                spec_id,
                iteration,
                exc,
            )
            return

        commit_msg = f"wip: BOI {spec_id} iter {iteration}"
        try:
            subprocess.run(
                ["git", "-C", target_repo, "commit", "-m", commit_msg],
                check=True,
                capture_output=True,
            )
            logger.info(
                "[iter-commit] Committed %s iter %d in %s",
                spec_id,
                iteration,
                target_repo,
            )
        except subprocess.CalledProcessError as exc:
            logger.warning(
                "[iter-commit] git commit failed for %s iter %d: %s",
                spec_id,
                iteration,
                exc,
            )
            return

        try:
            with open(manifest_path, "w", encoding="utf-8") as fh:
                fh.write("")
        except OSError as exc:
            logger.warning(
                "[iter-commit] Could not clear manifest for %s: %s",
                spec_id,
                exc,
            )

    def _commit_and_push_output(self, spec_id: str, target_repo: str) -> None:
        """Commit and push the target repo's changes after spec completion.

        Reads the changed-files manifest from ~/.boi/queue/{spec_id}.changed-files
        if it exists and stages only those files; otherwise stages all changes.
        Logs a warning on failure but never raises — spec completion is unaffected.
        """
        ops_log = os.path.join(os.path.expanduser("~"), ".boi", "ops-actions.log")

        if not target_repo:
            logger.warning(
                "[auto-commit] No target_repo for spec %s — skipping commit", spec_id
            )
            return

        # Verify it is a git repo
        try:
            subprocess.run(
                ["git", "-C", target_repo, "rev-parse", "--git-dir"],
                check=True,
                capture_output=True,
            )
        except (subprocess.CalledProcessError, FileNotFoundError):
            logger.warning(
                "[auto-commit] %s is not a git repo — skipping", target_repo
            )
            return

        # Check if repo is dirty
        try:
            result = subprocess.run(
                ["git", "-C", target_repo, "status", "--porcelain"],
                check=True,
                capture_output=True,
                text=True,
            )
            if not result.stdout.strip():
                logger.info(
                    "[auto-commit] %s is clean — nothing to commit", target_repo
                )
                return
        except subprocess.CalledProcessError as exc:
            logger.warning("[auto-commit] git status failed in %s: %s", target_repo, exc)
            return

        # Run pre-commit guardrail hooks before staging/committing
        try:
            from lib.guardrail_runner import run_hooks
            spec_entry = self.db.get_spec(spec_id)
            spec_path_for_hooks = (spec_entry or {}).get("spec_path", "")
            _pre_commit = run_hooks(
                hook_point="pre-commit",
                spec_id=spec_id,
                spec_path=spec_path_for_hooks,
                state_dir=self.state_dir,
            )
            if not _pre_commit.get("passed", True):
                logger.warning(
                    "[auto-commit] pre-commit gate blocked for %s: %s — skipping commit",
                    spec_id,
                    [g["gate"] for g in _pre_commit.get("failed_gates", [])],
                )
                return
        except Exception:
            logger.exception("[auto-commit] pre-commit hook runner raised for %s — proceeding", spec_id)

        # Stage changes
        manifest_path = os.path.join(
            os.path.expanduser("~"), ".boi", "queue", f"{spec_id}.changed-files"
        )
        try:
            if os.path.isfile(manifest_path) and os.path.getsize(manifest_path) > 0:
                with open(manifest_path, encoding="utf-8") as fh:
                    files = [ln.strip() for ln in fh if ln.strip()]
                for filepath in files:
                    full = os.path.join(target_repo, filepath)
                    if os.path.exists(full):
                        subprocess.run(
                            ["git", "-C", target_repo, "add", "--", filepath],
                            check=True,
                            capture_output=True,
                        )
                    else:
                        logger.info(
                            "[auto-commit] Skipping missing manifest file: %s", filepath
                        )
            else:
                logger.warning(
                    "[auto-commit] No changed-files manifest for %s, falling back to git add -A",
                    spec_id,
                )
                subprocess.run(
                    ["git", "-C", target_repo, "add", "-A"],
                    check=True,
                    capture_output=True,
                )
        except subprocess.CalledProcessError as exc:
            logger.warning("[auto-commit] git add failed in %s: %s", target_repo, exc)
            return

        # Commit
        commit_msg = f"feat: BOI {spec_id} output — auto-committed by hex-ops"
        try:
            subprocess.run(
                ["git", "-C", target_repo, "commit", "-m", commit_msg],
                check=True,
                capture_output=True,
            )
            logger.info("[auto-commit] Committed in %s: %s", target_repo, commit_msg)
        except subprocess.CalledProcessError as exc:
            logger.warning(
                "[auto-commit] git commit failed in %s: %s", target_repo, exc
            )
            return

        # Push if remote exists
        push_status = "no-remote"
        try:
            remote_result = subprocess.run(
                ["git", "-C", target_repo, "remote", "-v"],
                check=True,
                capture_output=True,
                text=True,
            )
            if remote_result.stdout.strip():
                try:
                    subprocess.run(
                        ["git", "-C", target_repo, "push"],
                        check=True,
                        capture_output=True,
                    )
                    push_status = "pushed"
                except subprocess.CalledProcessError:
                    push_status = "push-failed"
                    logger.warning(
                        "[auto-commit] git push failed in %s — branch may be diverged",
                        target_repo,
                    )
        except subprocess.CalledProcessError:
            pass

        # Log to ops-actions.log
        try:
            from datetime import datetime as _dt  # already imported at module level
            timestamp = _dt.now().strftime("%Y-%m-%d %H:%M")
            log_line = (
                f"{timestamp} — auto-commit: {spec_id} in {target_repo} (push={push_status})\n"
            )
            os.makedirs(os.path.dirname(ops_log), exist_ok=True)
            with open(ops_log, "a", encoding="utf-8") as fh:
                fh.write(log_line)
            logger.info("[auto-commit] Done: %s", log_line.strip())
        except Exception as exc:  # noqa: BLE001
            logger.warning("[auto-commit] Failed to write ops log: %s", exc)

    def _review_committed_output(
        self, spec_id: str, target_repo: str, spec_path: str
    ) -> None:
        """Run a best-effort code review on the last commit in target_repo.

        Spawns the configured runtime CLI with the committed diff and a
        structured review prompt.  If issues are found, calls _add_review_tasks() to append
        PENDING fix tasks to the spec.  Never raises — this is advisory only.
        """
        if not target_repo or not os.path.isdir(target_repo):
            logger.info(
                "[post-commit-review] No target_repo for %s — skipping", spec_id
            )
            return

        # Get diff of the last commit
        try:
            diff_result = subprocess.run(
                ["git", "-C", target_repo, "diff", "HEAD~1", "HEAD"],
                capture_output=True,
                text=True,
                timeout=30,
            )
            diff = diff_result.stdout.strip()
        except (subprocess.TimeoutExpired, FileNotFoundError, OSError) as exc:
            logger.warning(
                "[post-commit-review] Could not get diff for %s: %s", spec_id, exc
            )
            return

        if not diff:
            logger.info(
                "[post-commit-review] Empty diff for %s — skipping review", spec_id
            )
            return

        prompt = (
            "Review this code diff for bugs, security issues, incorrect logic, "
            "broken tests, and style problems. "
            'Output a JSON object: {"pass": true/false, "issues": [{"severity": '
            '"high|medium|low", "file": "path", "description": "what\'s wrong"}]}. '
            "Only flag real problems. Be concise.\n\n"
            f"```diff\n{diff}\n```"
        )

        # Resolve runtime: spec header > global config > default (claude)
        from lib.runtime import resolve_runtime, get_runtime
        _spec_content = ""
        if spec_path and os.path.isfile(spec_path):
            try:
                _spec_content = Path(spec_path).read_text(encoding="utf-8")
            except OSError:
                pass
        _runtime = get_runtime(resolve_runtime(state_dir=self.state_dir, spec_content=_spec_content))
        _rt_label = _runtime.name

        # Build command: simple prompt invocation (not full worker execution)
        if _runtime.name == "codex":
            _review_cmd = [_runtime.cli_command, "exec", "--dangerously-bypass-approvals-and-sandbox"]
            _run_kwargs: dict = {"input": prompt}
        else:
            _review_cmd = [_runtime.cli_command, "-p", prompt]
            _run_kwargs = {}

        try:
            review_result = subprocess.run(
                _review_cmd,
                capture_output=True,
                text=True,
                timeout=120,
                **_run_kwargs,
            )
            output = review_result.stdout.strip()
        except subprocess.TimeoutExpired:
            logger.warning(
                "[post-commit-review] %s timed out for %s — skipping", _rt_label, spec_id
            )
            return
        except (FileNotFoundError, OSError) as exc:
            logger.warning(
                "[post-commit-review] Could not run %s for %s: %s", _rt_label, spec_id, exc
            )
            return

        if not output:
            logger.warning(
                "[post-commit-review] Empty output from %s for %s", _rt_label, spec_id
            )
            return

        # Parse JSON — runtime may wrap it in a code fence
        try:
            # Strip markdown code fences if present
            if "```" in output:
                lines = output.splitlines()
                json_lines = []
                in_block = False
                for line in lines:
                    if line.strip().startswith("```"):
                        in_block = not in_block
                        continue
                    if in_block:
                        json_lines.append(line)
                if json_lines:
                    output = "\n".join(json_lines)
            review = json.loads(output)
        except (json.JSONDecodeError, ValueError) as exc:
            logger.warning(
                "[post-commit-review] JSON parse failed for %s: %s — raw: %.200s",
                spec_id,
                exc,
                output,
            )
            return

        passed = review.get("pass", True)
        issues = review.get("issues", [])

        if passed or not issues:
            logger.info(
                "[post-commit-review] Review passed for %s", spec_id
            )
            return

        logger.info(
            "[post-commit-review] Review found %d issue(s) for %s",
            len(issues),
            spec_id,
        )
        self._add_review_tasks(spec_id, issues)

    def _add_review_tasks(self, spec_id: str, issues: list) -> None:
        """Append PENDING [REVIEW] fix tasks to the spec for high/medium issues.

        Low-severity issues are logged as advisory only.  After adding tasks,
        the spec is requeued so the daemon picks it up for another iteration.
        Never raises.
        """
        spec_row = self.db.get_spec(spec_id)
        spec_file = (spec_row.get("spec_path") if spec_row else None) or os.path.join(
            os.path.expanduser("~"), ".boi", "queue", f"{spec_id}.spec.md"
        )
        if not os.path.isfile(spec_file):
            logger.warning(
                "[post-commit-review] Spec file not found: %s", spec_file
            )
            return

        actionable = [i for i in issues if i.get("severity") in ("high", "medium")]
        advisory = [i for i in issues if i.get("severity") == "low"]

        for issue in advisory:
            logger.info(
                "[post-commit-review] Advisory (low): %s — %s",
                issue.get("file", "?"),
                issue.get("description", ""),
            )

        if not actionable:
            logger.info(
                "[post-commit-review] No high/medium issues — no tasks added for %s",
                spec_id,
            )
            return

        # Determine next task number
        try:
            with open(spec_file, encoding="utf-8") as fh:
                content = fh.read()
        except OSError as exc:
            logger.warning(
                "[post-commit-review] Could not read spec %s: %s", spec_file, exc
            )
            return

        task_ids = re.findall(r"###\s+t-(\d+):", content)
        next_id = max((int(x) for x in task_ids), default=0) + 1

        new_tasks = []
        for issue in actionable:
            file_hint = issue.get("file", "unknown file")
            description = issue.get("description", "No description")
            severity = issue.get("severity", "medium")
            task_text = (
                f"\n### t-{next_id}: [REVIEW] Fix: {description}\n"
                f"PENDING\n\n"
                f"**Spec:** Fix the following {severity}-severity issue in `{file_hint}`: "
                f"{description}\n\n"
                f"**Verify:** `git diff HEAD~1 HEAD -- {file_hint}` shows the fix applied\n"
                if file_hint != "unknown file"
                else f"**Verify:** `git diff HEAD~1 HEAD --stat` shows relevant files changed\n"
            )
            new_tasks.append(task_text)
            next_id += 1

        tmp_file = spec_file + ".tmp"
        try:
            with open(tmp_file, "w", encoding="utf-8") as fh:
                fh.write(content)
                for task_text in new_tasks:
                    fh.write(task_text)
            os.replace(tmp_file, spec_file)
        except OSError as exc:
            logger.warning(
                "[post-commit-review] Could not write spec %s: %s", spec_file, exc
            )
            if os.path.exists(tmp_file):
                os.unlink(tmp_file)
            return

        # Requeue the spec so the daemon picks it up
        try:
            self.db.update_spec_status(spec_id, "queued")
        except Exception as exc:  # noqa: BLE001
            logger.warning(
                "[post-commit-review] Could not requeue spec %s: %s", spec_id, exc
            )

        logger.info(
            "[post-commit-review] Added %d review task(s) to %s and requeued",
            len(new_tasks),
            spec_id,
        )

    # ── Ship phase ───────────────────────────────────────────────────

    @staticmethod
    def _find_git_root(path: str) -> str:
        """Walk up from ``path`` to find the nearest directory containing .git.

        Returns the absolute path of the git root, or '' if none is found.
        ``path`` may be a file or directory.
        """
        from pathlib import Path as _Path
        current = _Path(path).expanduser().resolve()
        if current.is_file():
            current = current.parent
        for candidate in [current, *current.parents]:
            if (candidate / ".git").exists():
                return str(candidate)
        return ""

    @staticmethod
    def _extract_spec_push_field(spec_path: str) -> str:
        """Read push field from spec (markdown or YAML). Returns 'false' if absent."""
        try:
            from lib.spec_parser import parse_spec_header_fields
            return parse_spec_header_fields(spec_path)["push"]
        except Exception:
            pass
        return "false"

    @staticmethod
    def _extract_spec_target_repos(spec_path: str) -> list[str]:
        """Read target_repos list from spec header. Returns [] if absent."""
        try:
            from lib.spec_parser import parse_spec_header_fields
            from pathlib import Path as _Path
            raw = parse_spec_header_fields(spec_path).get("target_repos", "")
            if not raw:
                return []
            repos = []
            for part in raw.split(","):
                part = part.strip().strip("`")
                if part:
                    if part.startswith("~"):
                        part = str(_Path(part).expanduser())
                    repos.append(part)
            return repos
        except Exception:
            return []

    def _ship_single_repo(
        self,
        repo_path: str,
        spec_id: str,
        commit_msg: str,
        commit_scope: str,
        manifest_path: str,
        push_remote: str,
    ) -> tuple[bool, str]:
        """Add, commit, and optionally push changes in one git repo.

        Returns (success, commit_sha). On 'nothing to commit', returns (True, '').
        On error, returns (False, '').
        """
        import glob as _glob

        # Verify it is a git repo
        try:
            subprocess.run(
                ["git", "-C", repo_path, "rev-parse", "--git-dir"],
                check=True,
                capture_output=True,
            )
        except (subprocess.CalledProcessError, FileNotFoundError):
            logger.info("[ship] %s is not a git repo — skipping", repo_path)
            return True, ""

        # Check if dirty
        try:
            status_result = subprocess.run(
                ["git", "-C", repo_path, "status", "--porcelain"],
                check=True,
                capture_output=True,
                text=True,
            )
            if not status_result.stdout.strip():
                logger.info("[ship] %s is clean — nothing to commit for %s", repo_path, spec_id)
                return True, ""
        except subprocess.CalledProcessError as exc:
            logger.warning("[ship] git status failed in %s: %s", repo_path, exc)
            return True, ""

        # git add
        try:
            if commit_scope:
                matched = _glob.glob(commit_scope, root_dir=repo_path)
                if matched:
                    for f in matched:
                        subprocess.run(
                            ["git", "-C", repo_path, "add", "--", f],
                            check=True, capture_output=True,
                        )
                else:
                    logger.warning("[ship] commit_scope '%s' matched no files in %s", commit_scope, repo_path)
            elif manifest_path and os.path.isfile(manifest_path) and os.path.getsize(manifest_path) > 0:
                with open(manifest_path, encoding="utf-8") as fh:
                    files = [ln.strip() for ln in fh if ln.strip()]
                for filepath in files:
                    full = os.path.join(repo_path, filepath)
                    if os.path.exists(full):
                        subprocess.run(
                            ["git", "-C", repo_path, "add", "--", filepath],
                            check=True, capture_output=True,
                        )
            else:
                subprocess.run(
                    ["git", "-C", repo_path, "add", "-A"],
                    check=True, capture_output=True,
                )
        except subprocess.CalledProcessError as exc:
            logger.warning("[ship] git add failed in %s: %s", repo_path, exc)
            return False, ""

        # git commit
        try:
            subprocess.run(
                ["git", "-C", repo_path, "commit", "-m", commit_msg],
                check=True, capture_output=True,
            )
            sha_result = subprocess.run(
                ["git", "-C", repo_path, "rev-parse", "HEAD"],
                check=True, capture_output=True, text=True,
            )
            commit_sha = sha_result.stdout.strip()
            logger.info("[ship] Committed %s in %s: %s", spec_id, repo_path, commit_sha[:12])
        except subprocess.CalledProcessError as exc:
            stderr = exc.stderr.decode(errors="replace") if isinstance(exc.stderr, bytes) else str(exc.stderr or "")
            if "nothing to commit" in stderr or "nothing added to commit" in stderr:
                logger.info("[ship] Nothing to commit in %s for %s — treating as success", repo_path, spec_id)
                return True, ""
            logger.warning("[ship] git commit failed in %s: %s", repo_path, stderr[:400])
            return False, ""

        # git push
        if push_remote:
            try:
                subprocess.run(
                    ["git", "-C", repo_path, "push", push_remote, "HEAD"],
                    check=True, capture_output=True,
                )
                logger.info("[ship] Pushed %s to %s/%s", spec_id, push_remote, repo_path)
            except subprocess.CalledProcessError as exc:
                logger.warning("[ship] git push failed for %s in %s: %s", spec_id, repo_path, exc)

        return True, commit_sha

    @staticmethod
    def _extract_spec_commit_scope(spec_path: str) -> str:
        """Read commit_scope glob from spec (markdown or YAML). Returns '' if absent."""
        try:
            from lib.spec_parser import parse_spec_header_fields
            return parse_spec_header_fields(spec_path)["commit_scope"]
        except Exception:
            pass
        return ""

    @staticmethod
    def _extract_verify_commands(spec_path: str) -> list[str]:
        """Return verify commands for all DONE tasks in the spec."""
        cmds: list[str] = []
        try:
            from lib.spec_parser import parse_boi_spec
            tasks = parse_boi_spec(spec_path)
            verify_re = re.compile(r"\*\*Verify:\*\*\s+(.*)", re.IGNORECASE)
            for task in tasks:
                if task.status != "DONE":
                    continue
                # Scan body for **Verify:** lines
                for line in task.body.splitlines():
                    m = verify_re.search(line)
                    if m:
                        raw = m.group(1).strip().strip("`")
                        # Split by && into individual commands
                        for cmd in raw.split("&&"):
                            cmd = cmd.strip()
                            if cmd:
                                cmds.append(cmd)
        except Exception as exc:
            logger.warning("[ship] Could not extract verify commands: %s", exc)
        return cmds

    def _run_ship_phase(self, spec_id: str, spec: dict[str, Any]) -> bool:
        """Run the ship phase: verify → commit (multi-repo) → push.

        Returns True if the spec was successfully committed (or had nothing
        to commit). Returns False if verify failed or any commit failed, in
        which case spec status is set to needs_review.

        Multi-repo: if the spec header lists ``**Target-Repos:**`` (markdown)
        or ``target_repos:`` (YAML), each repo is committed separately with
        the same BOI commit message. All commit SHAs are recorded in the
        ship sidecar.
        """
        spec_path = spec.get("spec_path", "")
        spec_title = self._extract_spec_title(spec_path)
        worktree = spec.get("worktree", "")
        target_repo = self._extract_target_repo(spec_path)
        # Prefer explicit target_repo; fall back to worktree path
        primary_repo = target_repo or worktree
        queue_dir = os.path.join(self.state_dir, "queue")

        logger.info("[ship] Starting ship phase for %s", spec_id)

        # ── Step 1: Re-run verify commands for all DONE tasks ──────────
        verify_cmds = self._extract_verify_commands(spec_path)
        if verify_cmds:
            for cmd in verify_cmds:
                logger.info("[ship] Running verify: %s", cmd)
                try:
                    result = subprocess.run(
                        cmd,
                        shell=True,
                        capture_output=True,
                        text=True,
                        timeout=120,
                        cwd=primary_repo or None,
                    )
                    if result.returncode != 0:
                        error_detail = (result.stderr or result.stdout or "")[:500]
                        reason = f"Ship verify failed: `{cmd}` exited {result.returncode}. {error_detail}"
                        logger.warning("[ship] %s", reason)
                        try:
                            self.db.update_spec_fields(spec_id, status="needs_review", failure_reason=reason)
                        except Exception as exc2:
                            logger.warning("[ship] Could not set needs_review: %s", exc2)
                        return False
                except subprocess.TimeoutExpired:
                    reason = f"Ship verify timed out: `{cmd}`"
                    logger.warning("[ship] %s", reason)
                    try:
                        self.db.update_spec_fields(spec_id, status="needs_review", failure_reason=reason)
                    except Exception:
                        pass
                    return False
                except Exception as exc:
                    logger.warning("[ship] Verify command error for `%s`: %s", cmd, exc)
        else:
            logger.info("[ship] No verify commands found for %s — skipping verify gate", spec_id)

        # ── Step 2: Build list of repos to commit ──────────────────────
        # Always include the primary repo; append any additional repos from
        # the spec's target_repos field (multi-repo support).
        repos_to_commit: list[str] = []
        if primary_repo and os.path.isdir(primary_repo):
            repos_to_commit.append(primary_repo)

        additional_repos = self._extract_spec_target_repos(spec_path)
        for extra in additional_repos:
            if extra and os.path.isdir(extra) and extra not in repos_to_commit:
                repos_to_commit.append(extra)

        if not repos_to_commit:
            logger.info("[ship] No repo paths for %s — skipping git operations", spec_id)
            return True

        # ── Step 3: Commit each repo ───────────────────────────────────
        commit_msg = f"feat: BOI {spec_id} — {spec_title}\n\nAuto-committed by BOI ship phase."
        commit_scope = spec.get("commit_scope") or self._extract_spec_commit_scope(spec_path)
        manifest_path = os.path.join(queue_dir, f"{spec_id}.changed-files")
        push_field = (spec.get("push") or self._extract_spec_push_field(spec_path)).lower()
        push_remote = "" if push_field in ("false", "", "no", "0") else (
            "origin" if push_field == "true" else push_field
        )

        all_commits: list[dict[str, str]] = []
        any_failed = False

        for repo_path in repos_to_commit:
            # For additional (non-primary) repos, don't pass the manifest since
            # it contains paths relative to the primary repo.
            manifest = manifest_path if repo_path == primary_repo else ""
            ok, sha = self._ship_single_repo(
                repo_path=repo_path,
                spec_id=spec_id,
                commit_msg=commit_msg,
                commit_scope=commit_scope,
                manifest_path=manifest,
                push_remote=push_remote,
            )
            if not ok:
                reason = f"Ship commit failed in repo: {repo_path}"
                logger.warning("[ship] %s", reason)
                try:
                    self.db.update_spec_fields(spec_id, status="needs_review", failure_reason=reason)
                except Exception:
                    pass
                any_failed = True
            elif sha:
                all_commits.append({"repo": repo_path, "sha": sha})

        if any_failed:
            return False

        # ── Step 4: Record all commit SHAs ────────────────────────────
        if all_commits:
            sidecar_path = os.path.join(queue_dir, f"{spec_id}.ship.json")
            try:
                sidecar = {
                    "spec_id": spec_id,
                    # Backward compat: first commit SHA at top level
                    "commit_sha": all_commits[0]["sha"],
                    "repo": all_commits[0]["repo"],
                    "commits": all_commits,
                }
                tmp = sidecar_path + ".tmp"
                with open(tmp, "w", encoding="utf-8") as fh:
                    json.dump(sidecar, fh)
                os.replace(tmp, sidecar_path)
            except Exception as exc:
                logger.warning("[ship] Could not write ship sidecar: %s", exc)

        logger.info(
            "[ship] Ship phase complete for %s (%d repo(s) committed)",
            spec_id,
            len(all_commits),
        )
        return True

    # ── Helpers ──────────────────────────────────────────────────────

    def write_heartbeat(self) -> None:
        """Write JSON heartbeat to heartbeat.json (consumed by watchdog + fleet)."""
        import json as _json
        heartbeat_path = os.path.join(self.state_dir, "heartbeat.json")
        tmp = heartbeat_path + ".tmp"
        workers_alive = sum(
            1 for p in self.worker_procs.values() if p.poll() is None
        )
        try:
            specs_active = self.db.conn.execute(
                "SELECT COUNT(*) FROM specs WHERE status IN ('running', 'assigning')"
            ).fetchone()[0]
        except Exception:
            specs_active = len(self.worker_procs)
        heartbeat = {
            "ts": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "pid": os.getpid(),
            "specs_active": specs_active,
            "workers_alive": workers_alive,
        }
        with open(tmp, "w", encoding="utf-8") as f:
            _json.dump(heartbeat, f)
        os.replace(tmp, heartbeat_path)



# ── CLI entrypoint ───────────────────────────────────────────────────

def _stop_daemon(pid_file: str) -> None:
    """Stop a running daemon by PID file."""
    if not os.path.isfile(pid_file):
        print("No daemon running (no PID file).")
        return

    with open(pid_file, encoding="utf-8") as f:
        pid = int(f.read().strip())

    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        print(f"Daemon (PID {pid}) is not running. Cleaning up PID file.")
        os.remove(pid_file)
        return

    print(f"Stopping daemon (PID {pid})...")
    os.kill(pid, signal.SIGTERM)

    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        try:
            os.kill(pid, 0)
            time.sleep(1)
        except ProcessLookupError:
            break

    try:
        os.kill(pid, 0)
        print("Daemon did not stop gracefully. Sending SIGKILL.")
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass

    print("Daemon stopped.")
    try:
        os.remove(pid_file)
    except FileNotFoundError:
        pass


def main() -> None:
    """Parse CLI arguments and start/stop the daemon."""
    default_state = os.path.expanduser("~/.boi")
    default_config = os.path.join(default_state, "config.json")
    default_db = os.path.join(default_state, "boi.db")

    parser = argparse.ArgumentParser(
        description="BOI dispatch daemon"
    )
    parser.add_argument(
        "--stop",
        action="store_true",
        help="Stop the running daemon",
    )
    parser.add_argument(
        "--foreground",
        action="store_true",
        help="Run in foreground (don't daemonize)",
    )
    parser.add_argument(
        "--config",
        default=default_config,
        help=f"Path to config.json (default: {default_config})",
    )
    parser.add_argument(
        "--db",
        default=default_db,
        help=f"Path to SQLite database (default: {default_db})",
    )
    parser.add_argument(
        "--poll-interval",
        type=int,
        default=DEFAULT_POLL_INTERVAL,
        help=f"Poll interval in seconds (default: {DEFAULT_POLL_INTERVAL})",
    )

    args = parser.parse_args()
    state_dir = str(Path(args.db).parent)
    pid_file = os.path.join(state_dir, "daemon.pid")

    if args.stop:
        _stop_daemon(pid_file)
        return

    if not os.path.isfile(args.config):
        print(
            f"Error: Config not found at {args.config}. "
            "Run 'boi install' first.",
            file=sys.stderr,
        )
        sys.exit(1)

    # Set up logging
    log_dir = os.path.join(state_dir, "logs")
    os.makedirs(log_dir, exist_ok=True)

    log_format = "[%(asctime)s] [%(levelname)s] %(message)s"
    log_datefmt = "%Y-%m-%dT%H:%M:%SZ"
    logging.Formatter.converter = time.gmtime

    handlers: list[logging.Handler] = [
        logging.FileHandler(
            os.path.join(log_dir, "daemon.log"),
            encoding="utf-8",
        ),
    ]
    if args.foreground:
        handlers.append(logging.StreamHandler(sys.stderr))

    logging.basicConfig(
        level=logging.INFO,
        format=log_format,
        datefmt=log_datefmt,
        handlers=handlers,
    )

    daemon = Daemon(
        config_path=args.config,
        db_path=args.db,
        poll_interval=args.poll_interval,
        state_dir=state_dir,
    )
    daemon.run()


if __name__ == "__main__":
    main()
