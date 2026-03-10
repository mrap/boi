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
import json
import logging
import os
import signal
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

sys.path.insert(0, str(Path(__file__).resolve().parent))

from lib.db import Database

# Default daemon constants
DEFAULT_POLL_INTERVAL = 5
DEFAULT_WORKER_TIMEOUT = 1800  # 30 minutes
SELF_HEAL_INTERVAL = 10  # Run self-heal every N poll cycles

logger = logging.getLogger("boi.daemon")


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

        # PID / lock files
        self.pid_file = os.path.join(self.state_dir, "daemon.pid")
        self.lock_file = os.path.join(self.state_dir, "daemon.lock")

        # Active worker subprocesses: worker_id -> subprocess.Popen
        self.worker_procs: dict[str, subprocess.Popen] = {}

        # Default worker timeout (can be overridden per-spec)
        self.default_worker_timeout = DEFAULT_WORKER_TIMEOUT

        # Shutdown flag
        self._shutdown_requested = False

        # Database connection
        self.db = Database(db_path, self.queue_dir)

        # Install signal handlers
        signal.signal(signal.SIGTERM, self._signal_handler)
        signal.signal(signal.SIGINT, self._signal_handler)

    # ── Signal handling ──────────────────────────────────────────────

    def _signal_handler(self, signum: int, frame: Any) -> None:
        """Handle SIGTERM and SIGINT by requesting shutdown."""
        sig_name = signal.Signals(signum).name
        logger.info("Received %s, initiating shutdown", sig_name)
        self._shutdown_requested = True

    # ── Main loop ────────────────────────────────────────────────────

    def run(self) -> None:
        """Main daemon entry point. Loads workers, recovers stuck
        specs, then enters the poll loop."""
        # Ensure directories exist
        for d in [self.queue_dir, self.log_dir, self.hooks_dir]:
            os.makedirs(d, exist_ok=True)

        # Write PID file
        self._write_pid_file()

        logger.info("Daemon started (PID %d)", os.getpid())

        # Load worker definitions from config
        self.load_workers()

        # Startup recovery: reset specs stuck in 'running' with dead PIDs
        recovered = self.db.recover_running_specs()
        if recovered:
            logger.warning(
                "Recovered %d spec(s) stuck in 'running': %s",
                len(recovered),
                ", ".join(recovered),
            )

        # Poll loop
        poll_cycle = 0
        try:
            while not self._shutdown_requested:
                self.check_worker_completions()
                self.dispatch_specs()
                self.write_state_snapshot()
                self.write_heartbeat()

                poll_cycle += 1
                if poll_cycle % SELF_HEAL_INTERVAL == 0:
                    self.self_heal()

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

        # Remove PID file
        try:
            os.remove(self.pid_file)
        except FileNotFoundError:
            pass

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

    # ── Dispatch (Task 7) ───────────────────────────────────────────

    def dispatch_specs(self) -> None:
        """Assign queued specs to free workers.

        Loops until either no free worker or no eligible spec remains.
        Each iteration: get a free worker, pick the next spec, assign it.
        """
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

        # 2. Assign worker in DB before launching (no PID yet,
        #    will update after launch)
        self.db.assign_worker(worker_id, spec_id, pid=0, phase=phase)

        try:
            # 3. Launch worker subprocess
            proc = self.launch_worker(
                spec_id=spec_id,
                worktree=worker["worktree_path"],
                spec_path=spec["spec_path"],
                iteration=iteration,
                phase=phase,
                worker_id=worker_id,
                timeout=spec.get("worker_timeout_seconds"),
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
            "Worker %s completed: spec=%s, phase=%s, "
            "exit_code=%d, iteration=%d",
            worker_id,
            spec_id,
            phase,
            exit_code,
            spec.get("iteration", 0),
        )

        # 2. Delegate to phase-specific handler
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

        # 3. Free worker
        self.db.free_worker(worker_id)

        # 4. Remove from tracked procs
        self.worker_procs.pop(worker_id, None)

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

        if phase == "execute":
            daemon_ops.process_worker_completion(
                queue_dir=self.queue_dir,
                queue_id=spec_id,
                events_dir=events_dir,
                log_dir=self.log_dir,
                hooks_dir=self.hooks_dir,
                spec_path=spec_path,
                exit_code=str(exit_code),
            )
        elif phase == "critic":
            daemon_ops.process_critic_completion(
                queue_dir=self.queue_dir,
                queue_id=spec_id,
                events_dir=events_dir,
                hooks_dir=self.hooks_dir,
                spec_path=spec_path,
                exit_code=str(exit_code),
            )
        elif phase == "decompose":
            daemon_ops.process_decomposition_completion(
                queue_dir=self.queue_dir,
                queue_id=spec_id,
                events_dir=events_dir,
                spec_path=spec_path,
                exit_code=str(exit_code),
            )
        elif phase == "evaluate":
            daemon_ops.process_evaluation_completion(
                queue_dir=self.queue_dir,
                queue_id=spec_id,
                events_dir=events_dir,
                hooks_dir=self.hooks_dir,
                spec_path=spec_path,
                exit_code=str(exit_code),
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

    # ── Helpers ──────────────────────────────────────────────────────

    def write_heartbeat(self) -> None:
        """Write current UTC timestamp to daemon-heartbeat file."""
        heartbeat_path = os.path.join(self.state_dir, "daemon-heartbeat")
        tmp = heartbeat_path + ".tmp"
        now = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        with open(tmp, "w", encoding="utf-8") as f:
            f.write(now + "\n")
        os.replace(tmp, heartbeat_path)

    def _write_pid_file(self) -> None:
        """Atomically write current PID to the PID file."""
        tmp = self.pid_file + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            f.write(str(os.getpid()))
        os.replace(tmp, self.pid_file)


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
