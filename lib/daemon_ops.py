# daemon_ops.py — Batched daemon operations for BOI.
#
# Reduces Python process overhead by combining multiple per-cycle
# operations into single function calls. Instead of 5-10 separate
# python3 invocations per poll cycle, the daemon can make 1-2 calls.
#
# Two main entry points:
#   - process_worker_completion(...)  — handles all post-iteration logic
#   - pick_next_spec(...)             — dequeues the next eligible spec
#
# Both return JSON-serializable dicts for easy consumption from bash.
#
# All state-mutating functions accept an optional `db` parameter
# (a lib.db.Database instance). When provided, state operations
# go through SQLite instead of the file-based JSON queue.

from __future__ import annotations

import json
import logging
import os
import subprocess
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional, TYPE_CHECKING

_logger = logging.getLogger("boi.daemon_ops")

from lib.critic import (
    generate_critic_prompt,
    parse_critic_result,
    should_run_critic,
    write_quality_to_telemetry,
)
from lib.critic_config import load_critic_config
from lib.evaluate import (
    build_completion_summary,
    check_convergence,
    get_criteria_history,
    is_generate_spec,
    write_completion_summary_to_spec,
)
from lib.event_log import log_event, write_event
from lib.hooks import get_tasks_added_from_telemetry, run_completion_hooks, run_hook
from lib.spec_parser import (
    BoiTask,
    check_status_regression,
    count_boi_tasks,
    parse_boi_spec,
)
from lib.spec_validator import validate_spec_file
from lib.telemetry import update_telemetry

if TYPE_CHECKING:
    from lib.db import Database


@dataclass
class CompletionContext:
    """Bundles path and DB parameters for worker completion handlers."""

    queue_dir: str
    events_dir: str
    hooks_dir: str
    log_dir: str
    script_dir: str
    db: "Database"


def _signal_name(signal_num: int) -> str:
    """Return a human-readable signal name (e.g., 'SIGTERM') for a signal number."""
    import signal as _signal

    try:
        return _signal.Signals(signal_num).name
    except (ValueError, AttributeError):
        return f"SIG{signal_num}"


def warn_if_empty_diff(spec_id: str, worktree_path: str, newly_done: int) -> None:
    """Log a warning if tasks were marked DONE but no real changes exist.

    Two-phase check:
    1. `git diff HEAD~1 HEAD --stat` — catches false completions where the
       worker committed but the commit was empty or trivial.
    2. `git status --porcelain` — catches false completions where the worker
       never committed at all (working tree is clean).

    If HEAD~1 doesn't exist (first commit only), falls back to phase 2.

    This is a log-only check — it does NOT block completion.

    Args:
        spec_id: The spec's queue ID (used in the warning message).
        worktree_path: Path to the git worktree to inspect.
        newly_done: Number of tasks that were newly marked DONE this iteration.
    """
    if newly_done <= 0 or not worktree_path:
        return
    try:
        # Phase 1: Check if the last commit had content
        diff_result = subprocess.run(
            ["git", "-C", worktree_path, "diff", "HEAD~1", "HEAD", "--stat"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if diff_result.returncode == 0:
            if not diff_result.stdout.strip():
                _logger.warning(
                    "spec %s — tasks marked DONE but last commit diff is empty "
                    "(possible false completion)",
                    spec_id,
                )
            return  # Commit exists and has content — no warning needed

        # Phase 1 failed (no HEAD~1 or other error) — fall through to phase 2
        has_parent = "unknown revision" not in diff_result.stderr and "bad revision" not in diff_result.stderr
        if has_parent:
            _logger.debug(
                "warn_if_empty_diff: git diff failed for spec %s worktree %s: %s",
                spec_id,
                worktree_path,
                diff_result.stderr.strip(),
            )

        # Phase 2: Check working tree for uncommitted changes
        status_result = subprocess.run(
            ["git", "-C", worktree_path, "status", "--porcelain"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if status_result.returncode != 0:
            _logger.debug(
                "warn_if_empty_diff: git status failed for spec %s worktree %s: %s",
                spec_id,
                worktree_path,
                status_result.stderr.strip(),
            )
        elif not status_result.stdout.strip():
            _logger.warning(
                "spec %s — tasks marked DONE but git diff is empty "
                "(possible false completion)",
                spec_id,
            )
    except Exception as exc:
        _logger.debug(
            "warn_if_empty_diff: failed to check git diff for spec %s worktree %s: %s",
            spec_id,
            worktree_path,
            exc,
        )


def _parse_json_field(value: Any, default: Any = None) -> Any:
    """Parse a JSON string field from a db row.

    Handles both formats: dict/list (from JSON queue entries)
    and JSON strings (from SQLite rows).
    """
    if value is None:
        return default
    if isinstance(value, (dict, list)):
        return value
    if isinstance(value, str):
        try:
            return json.loads(value)
        except (json.JSONDecodeError, ValueError):
            return default
    return value


def _read_log_tail(
    log_dir: str, queue_id: str, iteration: int, lines: int = 20
) -> list[str]:
    """Read the last N lines from a worker log file.

    Returns a list of strings (one per line), or empty list if log not found.
    """
    log_file = Path(log_dir) / f"{queue_id}-iter-{iteration}.log"
    if not log_file.is_file():
        return []
    try:
        content = log_file.read_text(encoding="utf-8", errors="replace")
        all_lines = content.splitlines()
        return all_lines[-lines:] if len(all_lines) > lines else all_lines
    except OSError:
        return []


def _write_failure_to_iteration_meta(
    queue_dir: str,
    queue_id: str,
    iteration: int,
    failure_reason: str,
    log_tail: list[str],
    exit_code: Optional[str] = None,
) -> None:
    """Write or update iteration metadata JSON with failure diagnostics.

    If the iteration file already exists (written by the worker), adds
    failure_reason and log_tail fields. Otherwise creates a new file.
    """
    iter_meta_path = Path(queue_dir) / f"{queue_id}.iteration-{iteration}.json"

    if iter_meta_path.is_file():
        try:
            meta = json.loads(iter_meta_path.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError):
            meta = {}
    else:
        meta = {
            "queue_id": queue_id,
            "iteration": iteration,
        }

    meta["failure_reason"] = failure_reason
    meta["log_tail"] = log_tail
    if exit_code is not None:
        meta["exit_code"] = int(exit_code) if exit_code.isdigit() else exit_code
    else:
        meta["crash"] = True

    tmp = iter_meta_path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(meta, indent=2) + "\n", encoding="utf-8")
    os.rename(str(tmp), str(iter_meta_path))


def process_worker_completion(
    ctx: CompletionContext,
    queue_id: str,
    exit_code: Optional[str] = None,
    timeout: bool = False,
) -> dict[str, Any]:
    """Process a worker completion in a single call.

    Handles all post-iteration logic:
    1. Reads the queue entry (iteration, max_iterations, spec_path)
    2. Determines outcome based on exit_code and task counts
    3. Updates queue entry status
    4. Writes events
    5. Updates telemetry
    6. Captures failure diagnostics (failure_reason, log_tail)

    Args:
        ctx: CompletionContext bundling queue_dir, events_dir, hooks_dir,
             log_dir, script_dir, and db.
        queue_id: The spec queue ID (e.g., "q-001")
        exit_code: Worker exit code as string, or None if no exit file found
        timeout: True if the worker was killed due to timeout

    Returns:
        A dict with:
          outcome: "completed" | "requeued" | "failed" | "crashed"
          iteration: current iteration number
          max_iterations: max allowed iterations
          pending_count: remaining pending tasks
          done_count: completed tasks
          total_count: total tasks
          spec_path: path to the spec file
          reason: failure reason (if failed)
          failure_reason: human-readable failure reason (if failed/crashed)
    """
    timestamp = datetime.now(timezone.utc).isoformat()

    # Step 1: Read queue entry
    entry = ctx.db.get_spec(queue_id)
    if entry is None:
        return {"outcome": "error", "reason": f"Queue entry not found: {queue_id}"}

    iteration = entry.get("iteration", 0)
    max_iter = entry.get("max_iterations", 30)
    spec_path = entry.get("spec_path", "")

    result: dict[str, Any] = {
        "iteration": iteration,
        "max_iterations": max_iter,
        "spec_path": spec_path,
    }

    # Message-driven exits: handled separately from crashes
    _MESSAGE_EXIT_CODES = {"130", "131", "132", "133"}
    if exit_code in _MESSAGE_EXIT_CODES:
        from lib.messaging import cleanup_mailbox, exit_reason

        reason = exit_reason(int(exit_code))
        result["outcome"] = reason
        result["exit_code"] = int(exit_code)

        # Clean up the spec's mailbox
        state_dir = str(Path(ctx.queue_dir).parent)
        cleanup_mailbox(queue_id, state_dir)

        write_event(
            ctx.events_dir,
            {
                "type": f"spec_{reason}",
                "queue_id": queue_id,
                "spec_path": spec_path,
                "iteration": iteration,
                "exit_code": int(exit_code),
                "timestamp": timestamp,
            },
        )

        # 130 (canceled) / 132 (preempted): mark canceled or requeue
        if exit_code == "130":
            ctx.db.cancel(queue_id)
        elif exit_code == "131":
            # task_skipped: requeue for next task
            ctx.db.requeue(queue_id)
        elif exit_code == "132":
            # preempted: requeue for higher-priority work
            ctx.db.requeue(queue_id)
        elif exit_code == "133":
            # deprioritized: requeue at lower priority
            ctx.db.requeue(queue_id)

        return result

    # Signal death: exit code 128+ means the worker was killed by a signal
    # (e.g., SIGTERM=143, SIGKILL=137). These are external kills, NOT worker
    # bugs — do NOT count toward consecutive_failures.
    if exit_code is not None and exit_code.isdigit() and int(exit_code) >= 128:
        exit_code_int = int(exit_code)
        signal_num = exit_code_int - 128
        signal_name = _signal_name(signal_num)

        log_event(
            queue_id,
            "signal_death",
            f"Worker killed by {signal_name} (exit {exit_code_int}), requeuing",
            {"iteration": iteration, "signal": signal_num, "exit_code": exit_code_int},
            "warn",
        )

        # Requeue without incrementing consecutive_failures
        ctx.db.signal_requeue(queue_id)

        write_event(
            ctx.events_dir,
            {
                "type": "spec_signal_requeued",
                "queue_id": queue_id,
                "spec_path": spec_path,
                "iteration": iteration,
                "exit_code": exit_code_int,
                "signal": signal_num,
                "signal_name": signal_name,
                "timestamp": timestamp,
            },
        )

        result["outcome"] = "signal_requeued"
        result["exit_code"] = exit_code_int
        result["signal_name"] = signal_name
        return result

    # Crash: no exit file, or non-zero exit code
    if exit_code is None or exit_code != "0":
        failure_reason = _get_failure_reason(exit_code, timeout, entry)
        crash_result = _handle_crash(
            ctx,
            queue_id,
            entry,
            iteration,
            max_iter,
            spec_path,
            timestamp,
            failure_reason=failure_reason,
            exit_code_str=exit_code,
        )
        result.update(crash_result)
        return result

    # Step 2: Normal exit — validate spec first
    validation_error = _validate_spec_or_get_error(
        ctx.queue_dir, queue_id, spec_path, iteration
    )
    if validation_error is not None:
        result["validation_errors"] = validation_error["validation_errors"]
        failure_reason = validation_error["failure_reason"]
        crash_result = _handle_crash(
            ctx,
            queue_id,
            entry,
            iteration,
            max_iter,
            spec_path,
            timestamp,
            failure_reason=failure_reason,
            exit_code_str=exit_code,
        )
        result.update(crash_result)
        result["outcome"] = "validation_failed"
        return result

    # Check for status regression (DONE -> PENDING)
    regressions = _check_regression_and_record(
        entry, spec_path, queue_id, iteration, timestamp, ctx.events_dir
    )
    if regressions:
        result["status_regressions"] = regressions

    # Count tasks from spec
    counts = (
        count_boi_tasks(spec_path)
        if spec_path
        else {
            "pending": 0,
            "done": 0,
            "skipped": 0,
            "total": 0,
        }
    )
    pending_count = counts["pending"]
    done_count = counts["done"]
    total_count = counts["total"]

    result["pending_count"] = pending_count
    result["done_count"] = done_count
    result["total_count"] = total_count

    # Empty-diff detection: warn if tasks were marked DONE but the worktree
    # has no new changes (possible false completion / no real work performed).
    newly_done = done_count - entry.get("tasks_done", 0)
    if newly_done > 0:
        last_worker = entry.get("last_worker")
        if last_worker:
            try:
                worker_row = ctx.db.get_worker(last_worker)
                worktree_path = (worker_row or {}).get("worktree_path", "")
                warn_if_empty_diff(queue_id, worktree_path, newly_done)
            except Exception:
                pass  # Never let this block the completion path

    # Get tasks_added from telemetry
    tasks_added = get_tasks_added_from_telemetry(ctx.queue_dir, queue_id)

    # Check for EXPERIMENT_PROPOSED tasks — pause for human review
    experiment_proposed_tasks = _get_experiment_proposed_tasks(spec_path)
    if experiment_proposed_tasks:
        return _handle_experiment_proposed_return(
            result,
            experiment_proposed_tasks,
            ctx.db,
            ctx.queue_dir,
            queue_id,
            done_count,
            total_count,
            ctx.hooks_dir,
            ctx.events_dir,
            spec_path,
            iteration,
            timestamp,
        )

    # Step 3: Determine outcome and update queue entry

    # Max-iter guard — must fire BEFORE any other outcome logic.
    # Without this, specs with all tasks DONE bypass the max-iter check
    # because it was in an elif after the pending_count==0 branch.
    if iteration >= max_iter and pending_count > 0:
        # Max iterations reached with pending tasks — fail
        ctx.db.fail(queue_id, "Max iterations reached")
        result["outcome"] = "failed"
        result["reason"] = "max_iterations"

        write_event(
            ctx.events_dir,
            {
                "type": "spec_failed",
                "queue_id": queue_id,
                "spec_path": spec_path,
                "iterations": iteration,
                "tasks_done": done_count,
                "reason": "max_iterations",
                "timestamp": timestamp,
            },
        )

        run_completion_hooks(ctx.hooks_dir, queue_id, spec_path, is_failure=True)
        return result

    if pending_count == 0:
        # All tasks done — check if critic should run (only if under max-iter)
        if iteration >= max_iter:
            # Past max-iter: complete without critic, don't requeue
            ctx.db.complete(queue_id, done_count, total_count)
            result["outcome"] = "completed"

            write_event(
                ctx.events_dir,
                {
                    "type": "spec_completed",
                    "queue_id": queue_id,
                    "spec_path": spec_path,
                    "iterations": iteration,
                    "tasks_done": done_count,
                    "tasks_added": tasks_added,
                    "note": "completed_at_max_iter_without_critic",
                    "timestamp": timestamp,
                },
            )

            run_completion_hooks(ctx.hooks_dir, queue_id, spec_path, is_failure=False)
            return result

        result.update(
            _apply_critic_or_complete(
                entry,
                ctx,
                queue_id,
                done_count,
                total_count,
                spec_path,
                iteration,
                tasks_added,
                timestamp,
            )
        )

    else:
        # Still has pending tasks (and under max-iter, checked above), requeue
        ctx.db.requeue(queue_id, done_count, total_count)
        result["outcome"] = "requeued"

        write_event(
            ctx.events_dir,
            {
                "type": "spec_requeued",
                "queue_id": queue_id,
                "pending": pending_count,
                "iteration": iteration,
                "timestamp": timestamp,
            },
        )

    # Step 6: Update telemetry
    try:
        update_telemetry(ctx.queue_dir, queue_id)
    except Exception:
        pass  # Telemetry failure should never block the daemon

    return result


def _handle_crash(
    ctx: CompletionContext,
    queue_id: str,
    entry: dict[str, Any],
    iteration: int,
    max_iter: int,
    spec_path: str,
    timestamp: str,
    failure_reason: str = "",
    exit_code_str: Optional[str] = None,
) -> dict[str, Any]:
    """Handle a crash or failed iteration.

    Records failure, applies cooldown or fails permanently.
    Captures failure_reason and log_tail in iteration metadata.
    Returns a partial result dict to merge into the caller's result.
    """
    result: dict[str, Any] = {
        "pending_count": 0,
        "done_count": 0,
        "total_count": 0,
    }

    # Capture log tail for diagnostics
    log_tail = _read_log_tail(ctx.log_dir, queue_id, iteration)

    # Record the failure and check threshold
    max_exceeded = ctx.db.record_failure(queue_id)

    if max_exceeded:
        final_reason = (
            f"5 consecutive failures. Last error: {failure_reason}"
            if failure_reason
            else "Consecutive failures exceeded threshold"
        )
        ctx.db.fail(queue_id, final_reason)
        result["outcome"] = "failed"
        result["reason"] = "consecutive_failures"
        result["failure_reason"] = final_reason

        write_event(
            ctx.events_dir,
            {
                "type": "spec_failed",
                "queue_id": queue_id,
                "spec_path": spec_path,
                "iterations": iteration,
                "reason": "consecutive_failures",
                "failure_reason": final_reason,
                "timestamp": timestamp,
            },
        )

        log_event(
            queue_id,
            "crashed",
            f"Consecutive failures exceeded: {failure_reason}",
            {"iteration": iteration, "reason": "consecutive_failures"},
            "error",
        )

        run_completion_hooks(ctx.hooks_dir, queue_id, spec_path, is_failure=True)

    elif iteration >= max_iter:
        ctx.db.fail(queue_id, "Max iterations reached (with crashes)")
        result["outcome"] = "failed"
        result["reason"] = "max_iterations_with_crashes"
        result["failure_reason"] = (
            failure_reason or "Max iterations reached (with crashes)"
        )

        write_event(
            ctx.events_dir,
            {
                "type": "spec_failed",
                "queue_id": queue_id,
                "spec_path": spec_path,
                "iterations": iteration,
                "reason": "max_iterations_with_crashes",
                "failure_reason": failure_reason,
                "timestamp": timestamp,
            },
        )

        log_event(
            queue_id,
            "crashed",
            f"Max iterations reached with crashes: {failure_reason}",
            {"iteration": iteration, "reason": "max_iterations_with_crashes"},
            "error",
        )

        run_completion_hooks(ctx.hooks_dir, queue_id, spec_path, is_failure=True)

    else:
        # Requeue with cooldown (keep consecutive_failures intact)
        ctx.db.crash_requeue(queue_id)

        result["outcome"] = "crashed"
        result["failure_reason"] = failure_reason

        write_event(
            ctx.events_dir,
            {
                "type": "spec_crash_requeued",
                "queue_id": queue_id,
                "iteration": iteration,
                "failure_reason": failure_reason,
                "timestamp": timestamp,
            },
        )

        log_event(
            queue_id,
            "crashed",
            f"Worker crashed, requeued with cooldown: {failure_reason}",
            {"iteration": iteration},
            "warn",
        )

    # Write failure diagnostics to iteration metadata
    if failure_reason:
        try:
            _write_failure_to_iteration_meta(
                ctx.queue_dir,
                queue_id,
                iteration,
                failure_reason,
                log_tail,
                exit_code=exit_code_str,
            )
        except Exception:
            pass  # Diagnostics failure should never block the daemon

    # Update telemetry
    try:
        update_telemetry(ctx.queue_dir, queue_id)
    except Exception:
        pass

    return result


# ---------------------------------------------------------------------------
# Helpers extracted from process_worker_completion to reduce cyclomatic
# complexity. Each helper handles one cohesive sub-concern.
# ---------------------------------------------------------------------------


def _get_failure_reason(
    exit_code: Optional[str],
    timeout: bool,
    entry: dict[str, Any],
) -> str:
    """Map crash/failure exit conditions to a human-readable failure reason."""
    if exit_code is None:
        if timeout:
            timeout_seconds = entry.get("worker_timeout_seconds") or 1800
            timeout_minutes = timeout_seconds // 60
            return f"Worker timed out after {timeout_minutes} minutes."
        return (
            "Worker crashed: no exit file. Process may have been killed or timed out."
        )
    return f"Worker exited with code {exit_code}."


def _validate_spec_or_get_error(
    queue_dir: str,
    queue_id: str,
    spec_path: str,
    iteration: int,
) -> Optional[dict[str, Any]]:
    """Validate the spec file post-iteration.

    Returns a dict with 'validation_errors' and 'failure_reason' keys
    if the spec is invalid, or None if valid (or no spec_path).
    """
    if not spec_path:
        return None
    validation = validate_spec_file(spec_path)
    if validation.valid:
        return None

    error_summary = "; ".join(validation.errors[:3])
    failure_reason = f"Post-iteration spec validation failed: {error_summary}"

    # Write validation errors to iteration metadata
    iter_meta_path = Path(queue_dir) / f"{queue_id}.iteration-{iteration}.json"
    if iter_meta_path.is_file():
        try:
            meta = json.loads(iter_meta_path.read_text(encoding="utf-8"))
            meta["validation_errors"] = validation.errors
            meta["failure_reason"] = failure_reason
            tmp = iter_meta_path.with_suffix(".json.tmp")
            tmp.write_text(json.dumps(meta, indent=2) + "\n", encoding="utf-8")
            os.rename(str(tmp), str(iter_meta_path))
        except Exception:
            pass

    return {"validation_errors": validation.errors, "failure_reason": failure_reason}


def _check_regression_and_record(
    entry: dict[str, Any],
    spec_path: str,
    queue_id: str,
    iteration: int,
    timestamp: str,
    events_dir: str,
) -> Optional[list[str]]:
    """Check for task status regressions (DONE -> PENDING).

    Writes a status_regression_detected event if regressions are found.
    Returns a list of regression descriptions, or None if none found.
    """
    pre_iteration_tasks = _parse_json_field(
        entry.get("pre_iteration_tasks"), default={}
    )
    if not spec_path or not pre_iteration_tasks:
        return None
    try:
        content = Path(spec_path).read_text(encoding="utf-8")
        current_tasks = parse_boi_spec(content)
        previous_tasks = [
            BoiTask(id=tid, title="", status=status)
            for tid, status in pre_iteration_tasks.items()
        ]
        regressions = check_status_regression(previous_tasks, current_tasks)
        if regressions:
            regression_list = [
                f"{r.task_id}: {r.previous_status} -> {r.current_status}"
                for r in regressions
            ]
            write_event(
                events_dir,
                {
                    "type": "status_regression_detected",
                    "queue_id": queue_id,
                    "regressions": regression_list,
                    "iteration": iteration,
                    "timestamp": timestamp,
                },
            )
            return regression_list
    except Exception:
        pass  # Regression detection failure should never block the daemon
    return None


def _get_experiment_proposed_tasks(spec_path: str) -> list[str]:
    """Return IDs of tasks with EXPERIMENT_PROPOSED status in the spec."""
    if not spec_path:
        return []
    try:
        content = Path(spec_path).read_text(encoding="utf-8")
        current_tasks = parse_boi_spec(content)
        return [t.id for t in current_tasks if t.status == "EXPERIMENT_PROPOSED"]
    except Exception:
        return []


def _handle_experiment_proposed_return(
    result: dict[str, Any],
    experiment_proposed_tasks: list[str],
    db: Database,
    queue_dir: str,
    queue_id: str,
    done_count: int,
    total_count: int,
    hooks_dir: str,
    events_dir: str,
    spec_path: str,
    iteration: int,
    timestamp: str,
) -> dict[str, Any]:
    """Handle EXPERIMENT_PROPOSED tasks — pause the spec for human review."""
    # Increment experiment usage count
    db.increment_experiment_usage(queue_id, count=len(experiment_proposed_tasks))

    # Pause spec for human review
    db.set_needs_review(
        queue_id,
        experiment_proposed_tasks,
        done_count,
        total_count,
    )
    result["outcome"] = "needs_review"
    result["experiment_tasks"] = experiment_proposed_tasks

    write_event(
        events_dir,
        {
            "type": "experiment_proposed",
            "queue_id": queue_id,
            "spec_path": spec_path,
            "experiment_tasks": experiment_proposed_tasks,
            "iteration": iteration,
            "timestamp": timestamp,
        },
    )

    # Run on-experiment hook if present
    run_hook(hooks_dir, "on-experiment", queue_id, spec_path)

    # Update telemetry
    try:
        update_telemetry(queue_dir, queue_id)
    except Exception:
        pass

    return result


def _apply_critic_or_complete(
    entry: dict[str, Any],
    ctx: CompletionContext,
    queue_id: str,
    done_count: int,
    total_count: int,
    spec_path: str,
    iteration: int,
    tasks_added: int,
    timestamp: str,
) -> dict[str, Any]:
    """Determine whether to run the critic or complete the spec.

    Called when pending_count == 0 and iteration < max_iter.
    Returns a partial result dict to merge into the caller's result.
    """
    state_dir = str(Path(ctx.queue_dir).parent)
    critic_config = load_critic_config(state_dir)
    boi_dir = ctx.script_dir  # script_dir points to ~/boi/

    if should_run_critic(entry, critic_config):
        critic_passes = entry.get("critic_passes", 0)
        try:
            critic_start = time.monotonic()
            prompt = generate_critic_prompt(
                spec_path=spec_path,
                queue_id=queue_id,
                iteration=critic_passes + 1,
                config=critic_config,
                boi_dir=boi_dir,
                state_dir=state_dir,
                queue_entry=entry,
            )
            prompt_path = os.path.join(ctx.queue_dir, f"{queue_id}.critic-prompt.md")
            tmp_path = prompt_path + ".tmp"
            Path(tmp_path).write_text(prompt, encoding="utf-8")
            os.replace(tmp_path, prompt_path)

            critic_elapsed = round(time.monotonic() - critic_start, 3)

            write_event(
                ctx.events_dir,
                {
                    "type": "critic_review_triggered",
                    "queue_id": queue_id,
                    "spec_path": spec_path,
                    "critic_pass": critic_passes + 1,
                    "critic_elapsed_seconds": critic_elapsed,
                    "timestamp": timestamp,
                },
            )

            # Requeue so daemon can pick it up for critic worker
            ctx.db.requeue(queue_id, done_count, total_count)
            ctx.db.update_spec_fields(queue_id, phase="critic")

            return {
                "outcome": "critic_review",
                "critic_prompt_path": prompt_path,
                "critic_pass": critic_passes + 1,
            }

        except Exception as exc:
            # If critic prompt generation fails, complete anyway
            ctx.db.complete(queue_id, done_count, total_count)

            write_event(
                ctx.events_dir,
                {
                    "type": "spec_completed",
                    "queue_id": queue_id,
                    "spec_path": spec_path,
                    "iterations": iteration,
                    "tasks_done": done_count,
                    "tasks_added": tasks_added,
                    "critic_error": str(exc),
                    "timestamp": timestamp,
                },
            )

            run_completion_hooks(ctx.hooks_dir, queue_id, spec_path, is_failure=False)

            return {"outcome": "completed", "critic_error": str(exc)}

    else:
        # Critic disabled or max passes reached — complete normally
        ctx.db.complete(queue_id, done_count, total_count)

        write_event(
            ctx.events_dir,
            {
                "type": "spec_completed",
                "queue_id": queue_id,
                "spec_path": spec_path,
                "iterations": iteration,
                "tasks_done": done_count,
                "tasks_added": tasks_added,
                "timestamp": timestamp,
            },
        )

        run_completion_hooks(ctx.hooks_dir, queue_id, spec_path, is_failure=False)

        return {"outcome": "completed"}


def pick_next_spec(
    queue_dir: str,
    db: Database,
) -> Optional[dict[str, Any]]:
    """Dequeue and return the next eligible spec in one call.

    Returns a dict with id, spec_path, iteration, max_iterations
    for the next eligible spec, or None if no spec is available.

    Args:
        queue_dir: Path to ~/.boi/queue/
        db: Optional Database instance for SQLite-backed state.
    """
    entry = db.pick_next_spec()
    if entry is None:
        return None

    return {
        "id": entry["id"],
        "spec_path": entry.get("spec_path", ""),
        "iteration": entry.get("iteration", 0),
        "max_iterations": entry.get("max_iterations", 30),
    }


def find_parallel_assignments(
    db: Database,
    spec_id: str,
    spec_path: str,
) -> list[str]:
    """Find all task IDs that can be assigned to workers in parallel.

    Parses the spec file, determines which tasks are already in_progress
    (assigned to a worker), and returns the list of newly assignable
    task IDs sorted by downstream impact (most-unblocking first).

    Args:
        db: Database instance.
        spec_id: The spec queue ID.
        spec_path: Path to the spec file on disk.

    Returns:
        List of task IDs ready for assignment, ordered by priority
        (tasks that unblock the most work come first).
    """
    from lib.dag import downstream_count, find_assignable_tasks

    spec_content = Path(spec_path).read_text(encoding="utf-8")
    tasks = parse_boi_spec(spec_content)
    if not tasks:
        return []

    in_progress = db.get_in_progress_task_ids(spec_id)
    assignable = find_assignable_tasks(tasks, in_progress=in_progress)

    # Sort by downstream impact: tasks unblocking the most work first
    assignable.sort(
        key=lambda tid: downstream_count(tasks, tid),
        reverse=True,
    )

    return assignable


def get_active_count(
    queue_dir: str,
    db: Database,
) -> int:
    """Count specs with active statuses (queued, requeued, running, needs_review).

    Args:
        queue_dir: Path to ~/.boi/queue/
        db: Optional Database instance for SQLite-backed state.
    """
    entries = db.get_queue()
    return sum(
        1
        for e in entries
        if e.get("status") in ("queued", "requeued", "running", "needs_review")
    )


def process_critic_completion(
    queue_dir: str,
    queue_id: str,
    events_dir: str,
    hooks_dir: str,
    spec_path: str,
    db: Database,
) -> dict[str, Any]:
    """Process a critic worker's completion.

    After a critic worker exits, this function:
    1. Parses the spec file for `## Critic Approved` or new `[CRITIC]` tasks
    2. Increments critic_passes in the queue entry
    3. If approved: marks the spec completed
    4. If new tasks: requeues for regular workers to handle

    Args:
        queue_dir: Path to ~/.boi/queue/
        queue_id: The spec queue ID
        events_dir: Path to ~/.boi/events/
        hooks_dir: Path to ~/.boi/hooks/
        spec_path: Path to the spec file
        db: Optional Database instance for SQLite-backed state.

    Returns:
        A dict with:
          outcome: "critic_approved" | "critic_tasks_added" | "error"
          critic_tasks_added: int (if tasks added)
    """
    timestamp = datetime.now(timezone.utc).isoformat()

    # Parse critic result from spec (now includes quality score data)
    critic_result = parse_critic_result(spec_path)

    # Write quality score to telemetry if available
    quality_score = critic_result.get("quality_score")
    quality_signals = critic_result.get("quality_signals")
    quality_gate = critic_result.get("quality_gate", "unknown")
    try:
        write_quality_to_telemetry(
            queue_dir, queue_id, quality_score, quality_signals, quality_gate
        )
    except Exception:
        pass  # Quality telemetry failure should not block critic flow

    # Increment critic_passes
    entry = db.get_spec(queue_id)
    if entry is None:
        return {"outcome": "error", "reason": f"Queue entry not found: {queue_id}"}

    new_critic_passes = (entry.get("critic_passes") or 0) + 1
    db.update_spec_fields(queue_id, critic_passes=new_critic_passes)
    entry["critic_passes"] = new_critic_passes

    if critic_result["approved"]:
        # Critic approved — check if this is a Generate spec needing evaluation
        from lib.spec_parser import count_boi_tasks

        counts = (
            count_boi_tasks(spec_path)
            if spec_path
            else {
                "pending": 0,
                "done": 0,
                "total": 0,
            }
        )

        # For Generate specs, transition to evaluate phase instead of completing
        if is_generate_spec(entry) and entry.get("phase") != "evaluate":
            db.update_spec_fields(queue_id, phase="evaluate")
            db.requeue(queue_id, counts["done"], counts["total"])

            write_event(
                events_dir,
                {
                    "type": "evaluate_phase_entered",
                    "queue_id": queue_id,
                    "spec_path": spec_path,
                    "critic_pass": entry["critic_passes"],
                    "quality_score": quality_score,
                    "quality_gate": quality_gate,
                    "timestamp": timestamp,
                },
            )

            return {"outcome": "evaluate_phase_entered", "phase": "evaluate"}

        db.complete(queue_id, counts["done"], counts["total"])

        write_event(
            events_dir,
            {
                "type": "critic_approved",
                "queue_id": queue_id,
                "spec_path": spec_path,
                "critic_pass": entry["critic_passes"],
                "quality_score": quality_score,
                "quality_gate": quality_gate,
                "timestamp": timestamp,
            },
        )

        run_completion_hooks(hooks_dir, queue_id, spec_path, is_failure=False)

        return {"outcome": "critic_approved"}

    elif critic_result["critic_tasks_added"] > 0:
        # Critic added tasks — requeue for regular workers (execute phase)
        from lib.spec_parser import count_boi_tasks

        counts = count_boi_tasks(spec_path)
        db.requeue(queue_id, counts["done"], counts["total"])
        db.update_spec_fields(queue_id, phase="execute")

        write_event(
            events_dir,
            {
                "type": "critic_tasks_added",
                "queue_id": queue_id,
                "spec_path": spec_path,
                "critic_pass": entry["critic_passes"],
                "tasks_added": critic_result["critic_tasks_added"],
                "quality_score": quality_score,
                "quality_gate": quality_gate,
                "timestamp": timestamp,
            },
        )

        return {
            "outcome": "critic_tasks_added",
            "critic_tasks_added": critic_result["critic_tasks_added"],
        }

    else:
        # No approval and no tasks added — critic didn't produce valid output.
        # Check post_counts: if any tasks are still PENDING (e.g. critic added a
        # plain PENDING task without the [CRITIC] tag), requeue instead of
        # completing. Only complete when post_counts["pending"] == 0.
        from lib.spec_parser import count_boi_tasks

        counts = (
            count_boi_tasks(spec_path)
            if spec_path
            else {
                "pending": 0,
                "done": 0,
                "total": 0,
            }
        )

        if counts["pending"] > 0:
            # Pending tasks exist — requeue for regular workers to handle.
            db.requeue(queue_id, counts["done"], counts["total"])
            db.update_spec_fields(queue_id, phase="execute")

            write_event(
                events_dir,
                {
                    "type": "critic_tasks_added",
                    "queue_id": queue_id,
                    "spec_path": spec_path,
                    "critic_pass": entry["critic_passes"],
                    "tasks_added": counts["pending"],
                    "timestamp": timestamp,
                },
            )

            return {
                "outcome": "critic_tasks_added",
                "critic_tasks_added": counts["pending"],
            }

        db.complete(queue_id, counts["done"], counts["total"])

        write_event(
            events_dir,
            {
                "type": "critic_no_output",
                "queue_id": queue_id,
                "spec_path": spec_path,
                "critic_pass": entry["critic_passes"],
                "timestamp": timestamp,
            },
        )

        run_completion_hooks(hooks_dir, queue_id, spec_path, is_failure=False)

        return {"outcome": "critic_approved"}


def process_decomposition_completion(
    queue_dir: str,
    queue_id: str,
    events_dir: str,
    spec_path: str,
    db: Database,
    exit_code: Optional[str] = None,
) -> dict[str, Any]:
    """Process a decomposition worker's completion.

    After a decomposition worker exits, this function:
    1. Checks exit code for crash
    2. Validates the spec now has tasks (3-30)
    3. Validates all tasks have Spec and Verify sections
    4. Checks that at least one Success Criterion maps to a task
    5. If valid: transitions phase to 'execute', requeues
    6. If invalid: retries once (via retry_count), then fails

    Args:
        queue_dir: Path to ~/.boi/queue/
        queue_id: The spec queue ID
        events_dir: Path to ~/.boi/events/
        spec_path: Path to the spec file
        exit_code: Worker exit code as string, or None if crashed
        db: Optional Database instance for SQLite-backed state.

    Returns:
        A dict with:
          outcome: "decomposition_complete" | "decomposition_retry" | "decomposition_failed"
          phase: current phase
          task_count: number of tasks found (if any)
          errors: validation errors (if any)
    """
    timestamp = datetime.now(timezone.utc).isoformat()

    entry = db.get_spec(queue_id)
    if entry is None:
        return {"outcome": "error", "reason": f"Queue entry not found: {queue_id}"}

    retry_count = entry.get("decomposition_retries") or 0

    # Check for crash
    if exit_code is None or exit_code != "0":
        return _handle_decomposition_failure(
            queue_dir,
            queue_id,
            entry,
            retry_count,
            events_dir,
            spec_path,
            timestamp,
            reason=f"Worker exited with code {exit_code}"
            if exit_code
            else "Worker crashed (no exit code)",
            db=db,
        )

    # Validate the decomposed spec
    errors: list[str] = []

    if not Path(spec_path).is_file():
        return _handle_decomposition_failure(
            queue_dir,
            queue_id,
            entry,
            retry_count,
            events_dir,
            spec_path,
            timestamp,
            reason="Spec file not found after decomposition",
            db=db,
        )

    content = Path(spec_path).read_text(encoding="utf-8")

    # Use standard spec validation on the decomposed output
    validation = validate_spec_file(spec_path)
    if not validation.valid:
        errors.extend(validation.errors)

    # Check task count bounds (3-30)
    task_count = validation.total
    if task_count < 3:
        errors.append(f"Too few tasks ({task_count}). Minimum is 3.")
    elif task_count > 30:
        errors.append(f"Too many tasks ({task_count}). Maximum is 30.")

    # Check for Approach section
    if "## Approach" not in content:
        errors.append("Missing '## Approach' section.")

    if errors:
        return _handle_decomposition_failure(
            queue_dir,
            queue_id,
            entry,
            retry_count,
            events_dir,
            spec_path,
            timestamp,
            reason="Validation failed: " + "; ".join(errors),
            errors=errors,
            db=db,
        )

    # Validation passed — transition to execute phase
    db.update_spec_fields(
        queue_id,
        phase="execute",
        decomposition_retries=0,
        tasks_total=task_count,
        tasks_done=validation.done,
        worker_timeout_seconds=None,
    )
    db.requeue(queue_id, validation.done, task_count)

    write_event(
        events_dir,
        {
            "type": "decomposition_complete",
            "queue_id": queue_id,
            "spec_path": spec_path,
            "task_count": task_count,
            "timestamp": timestamp,
        },
    )

    return {
        "outcome": "decomposition_complete",
        "phase": "execute",
        "task_count": task_count,
    }


def _handle_decomposition_failure(
    queue_dir: str,
    queue_id: str,
    entry: dict[str, Any],
    retry_count: int,
    events_dir: str,
    spec_path: str,
    timestamp: str,
    db: Database,
    reason: str = "",
    errors: list[str] | None = None,
) -> dict[str, Any]:
    """Handle a decomposition failure — retry once, then fail permanently."""
    if retry_count < 1:
        # Retry: increment retry counter and requeue
        db.update_spec_fields(
            queue_id,
            decomposition_retries=retry_count + 1,
            status="requeued",
        )

        write_event(
            events_dir,
            {
                "type": "decomposition_retry",
                "queue_id": queue_id,
                "spec_path": spec_path,
                "retry_count": retry_count + 1,
                "reason": reason,
                "timestamp": timestamp,
            },
        )

        result: dict[str, Any] = {
            "outcome": "decomposition_retry",
            "phase": "decompose",
            "retry_count": retry_count + 1,
            "reason": reason,
        }
        if errors:
            result["errors"] = errors
        return result
    else:
        # Max retries reached — fail permanently
        db.fail(queue_id, f"decomposition_failed: {reason}")

        write_event(
            events_dir,
            {
                "type": "decomposition_failed",
                "queue_id": queue_id,
                "spec_path": spec_path,
                "reason": reason,
                "timestamp": timestamp,
            },
        )

        result = {
            "outcome": "decomposition_failed",
            "phase": "decompose",
            "reason": reason,
        }
        if errors:
            result["errors"] = errors
        return result


DEFAULT_EXPERIMENT_TIMEOUT_HOURS = 24


def _load_experiment_timeout(state_dir: str) -> float:
    """Load experiment_timeout_hours from config, default 24."""
    config_path = os.path.join(state_dir, "config.json")
    try:
        data = json.loads(Path(config_path).read_text(encoding="utf-8"))
        return float(
            data.get("experiment_timeout_hours", DEFAULT_EXPERIMENT_TIMEOUT_HOURS)
        )
    except (json.JSONDecodeError, OSError, TypeError, ValueError):
        return DEFAULT_EXPERIMENT_TIMEOUT_HOURS


def check_needs_review_timeouts(
    queue_dir: str,
    events_dir: str,
    state_dir: str,
    db: Database,
) -> list[str]:
    """Check for specs in needs_review that have exceeded the timeout.

    For each timed-out spec:
    1. Reset EXPERIMENT_PROPOSED tasks back to PENDING in the spec file.
    2. Append '**Decision:** AUTO-REJECTED (review timeout)' to experiment sections.
    3. Requeue the spec.
    4. Write an event.

    Args:
        queue_dir: Path to ~/.boi/queue/
        events_dir: Path to ~/.boi/events/
        state_dir: Path to ~/.boi/
        db: Optional Database instance for SQLite-backed state.

    Returns a list of queue IDs that were auto-rejected.
    """
    entries = db.get_queue()

    timeout_hours = _load_experiment_timeout(state_dir)
    now = datetime.now(timezone.utc)
    timestamp = now.isoformat()
    auto_rejected: list[str] = []

    for entry in entries:
        if entry.get("status") != "needs_review":
            continue

        review_since = entry.get("needs_review_since", "")
        if not review_since:
            continue

        try:
            review_dt = datetime.fromisoformat(review_since)
            # Ensure timezone-aware comparison
            if review_dt.tzinfo is None:
                review_dt = review_dt.replace(tzinfo=timezone.utc)
            elapsed_hours = (now - review_dt).total_seconds() / 3600.0
        except (ValueError, TypeError):
            continue

        if elapsed_hours < timeout_hours:
            continue

        # Timeout exceeded — auto-reject all experiments
        queue_id = entry["id"]
        spec_path = entry.get("spec_path", "")

        if spec_path and Path(spec_path).is_file():
            try:
                _auto_reject_experiments(spec_path)
            except Exception:
                pass  # Best effort

        # Requeue the spec
        counts = (
            count_boi_tasks(spec_path)
            if spec_path
            else {"pending": 0, "done": 0, "total": 0}
        )
        db.requeue(queue_id, counts["done"], counts["total"])
        db.update_spec_fields(
            queue_id,
            experiment_tasks=None,
            needs_review_since=None,
        )

        write_event(
            events_dir,
            {
                "type": "experiment_auto_rejected",
                "queue_id": queue_id,
                "spec_path": spec_path,
                "timeout_hours": timeout_hours,
                "elapsed_hours": round(elapsed_hours, 2),
                "timestamp": timestamp,
            },
        )

        auto_rejected.append(queue_id)

    return auto_rejected


def _auto_reject_experiments(spec_path: str) -> None:
    """Reset EXPERIMENT_PROPOSED tasks to PENDING and append rejection notices."""
    import re

    content = Path(spec_path).read_text(encoding="utf-8")

    # Replace EXPERIMENT_PROPOSED status lines with PENDING
    content = content.replace("EXPERIMENT_PROPOSED", "PENDING")

    # Append rejection notice after each #### Experiment: section
    lines = content.split("\n")
    result_lines: list[str] = []
    in_experiment = False

    for i, line in enumerate(lines):
        # Detect start of experiment section
        if re.match(r"^####\s+Experiment:", line):
            in_experiment = True
            result_lines.append(line)
            continue

        # Detect end of experiment section (next heading of any level)
        if in_experiment and re.match(r"^#{1,4}\s+", line):
            # Insert rejection notice before the next heading
            result_lines.append("")
            result_lines.append("**Decision:** AUTO-REJECTED (review timeout)")
            result_lines.append("")
            in_experiment = False

        result_lines.append(line)

    # If we ended the file still inside an experiment section
    if in_experiment:
        result_lines.append("")
        result_lines.append("**Decision:** AUTO-REJECTED (review timeout)")
        result_lines.append("")

    new_content = "\n".join(result_lines)
    tmp = spec_path + ".tmp"
    Path(tmp).write_text(new_content, encoding="utf-8")
    os.replace(tmp, spec_path)


def _handle_evaluate_crash(
    queue_dir: str,
    queue_id: str,
    events_dir: str,
    entry: dict[str, Any],
    db: Database,
    timestamp: str,
) -> dict[str, Any]:
    """Handle a crashed evaluation worker. Returns a result dict describing the outcome."""
    crash_result = db.record_failure(queue_id)

    if crash_result:
        db.fail(queue_id, "Evaluation crashed: consecutive failures")
        write_event(
            events_dir,
            {
                "type": "evaluate_failed",
                "queue_id": queue_id,
                "reason": "consecutive_failures",
                "timestamp": timestamp,
            },
        )
        return {"outcome": "evaluate_crashed", "reason": "consecutive_failures"}

    tasks_done = entry.get("tasks_done") or 0
    tasks_total = entry.get("tasks_total") or 0
    db.requeue(queue_id, tasks_done, tasks_total)
    write_event(
        events_dir,
        {
            "type": "evaluate_crash_requeued",
            "queue_id": queue_id,
            "timestamp": timestamp,
        },
    )
    return {"outcome": "evaluate_crashed", "phase": "evaluate"}


def _complete_evaluated_spec(
    queue_dir: str,
    queue_id: str,
    events_dir: str,
    hooks_dir: str,
    spec_path: str,
    counts: dict[str, int],
    entry: dict[str, Any],
    status: str,
    convergence: Any,
    db: Database,
    timestamp: str,
) -> dict[str, Any]:
    """Write completion summary, mark spec done, emit event, run hooks."""
    summary = build_completion_summary(
        status=status,
        queue_entry=entry,
        spec_path=spec_path,
        start_time=entry.get("submitted_at"),
    )
    write_completion_summary_to_spec(spec_path, summary)

    db.complete(queue_id, counts["done"], counts["total"])

    write_event(
        events_dir,
        {
            "type": "generate_completed",
            "queue_id": queue_id,
            "spec_path": spec_path,
            "status": status,
            "criteria_met": convergence.criteria_met,
            "criteria_total": convergence.criteria_total,
            "iterations_used": convergence.iterations_used,
            "timestamp": timestamp,
        },
    )

    run_completion_hooks(hooks_dir, queue_id, spec_path, is_failure=False)

    return {
        "criteria_met": convergence.criteria_met,
        "criteria_total": convergence.criteria_total,
        "iterations_used": convergence.iterations_used,
        "outcome": "evaluate_converged",
        "status": status,
        "phase": "completed",
    }


def _loop_back_to_execute(
    queue_dir: str,
    queue_id: str,
    events_dir: str,
    spec_path: str,
    counts: dict[str, int],
    entry: dict[str, Any],
    convergence: Any,
    pending_count: int,
    db: Database,
    timestamp: str,
) -> dict[str, Any]:
    """Requeue spec back to execute phase when evaluation finds unmet criteria."""
    db.update_spec_fields(queue_id, phase="execute")
    db.requeue(queue_id, counts["done"], counts["total"])

    write_event(
        events_dir,
        {
            "type": "evaluate_loop_back",
            "queue_id": queue_id,
            "spec_path": spec_path,
            "pending_count": pending_count,
            "criteria_met": convergence.criteria_met,
            "criteria_total": convergence.criteria_total,
            "timestamp": timestamp,
        },
    )

    return {
        "criteria_met": convergence.criteria_met,
        "criteria_total": convergence.criteria_total,
        "iterations_used": convergence.iterations_used,
        "outcome": "evaluate_loop_back",
        "phase": "execute",
        "pending_count": pending_count,
    }


def process_evaluation_completion(
    queue_dir: str,
    queue_id: str,
    events_dir: str,
    hooks_dir: str,
    spec_path: str,
    db: Database,
    exit_code: Optional[str] = None,
) -> dict[str, Any]:
    """Process an evaluation worker's completion.

    After an evaluation worker exits, this function:
    1. Checks exit code for crash
    2. Reads the spec to check criteria status
    3. Runs convergence algorithm
    4. If all criteria met: marks spec COMPLETED with summary
    5. If unmet criteria with new tasks: transitions back to execute phase
    6. If stalled/max_iterations/good_enough: marks COMPLETED with summary

    Args:
        queue_dir: Path to ~/.boi/queue/
        queue_id: The spec queue ID
        events_dir: Path to ~/.boi/events/
        hooks_dir: Path to ~/.boi/hooks/
        spec_path: Path to the spec file
        exit_code: Worker exit code as string, or None if crashed
        db: Optional Database instance for SQLite-backed state.

    Returns:
        A dict with:
          outcome: "evaluate_complete" | "evaluate_loop_back" |
                   "evaluate_converged" | "evaluate_crashed"
          phase: current phase
          status: convergence status
    """
    timestamp = datetime.now(timezone.utc).isoformat()

    entry = db.get_spec(queue_id)
    if entry is None:
        return {"outcome": "error", "reason": f"Queue entry not found: {queue_id}"}

    if exit_code is None or exit_code != "0":
        return _handle_evaluate_crash(
            queue_dir, queue_id, events_dir, entry, db, timestamp
        )

    counts = (
        count_boi_tasks(spec_path)
        if spec_path
        else {"pending": 0, "done": 0, "total": 0}
    )
    pending_count = counts["pending"]

    criteria_history = get_criteria_history(queue_dir, queue_id)
    convergence = check_convergence(entry, spec_path, criteria_history)

    if convergence.should_stop:
        return _complete_evaluated_spec(
            queue_dir,
            queue_id,
            events_dir,
            hooks_dir,
            spec_path,
            counts,
            entry,
            convergence.reason,
            convergence,
            db,
            timestamp,
        )

    if pending_count > 0:
        return _loop_back_to_execute(
            queue_dir,
            queue_id,
            events_dir,
            spec_path,
            counts,
            entry,
            convergence,
            pending_count,
            db,
            timestamp,
        )

    # No new tasks and no convergence — all criteria met by the evaluator (goal_achieved)
    return _complete_evaluated_spec(
        queue_dir,
        queue_id,
        events_dir,
        hooks_dir,
        spec_path,
        counts,
        entry,
        "goal_achieved",
        convergence,
        db,
        timestamp,
    )


# ─── Self-Healing ──────────────────────────────────────────────────────────────


def self_heal(
    queue_dir: str,
    worker_specs: dict[str, str],
    db: Database,
) -> list[dict[str, Any]]:
    """Detect and recover from stuck states automatically.

    Runs a battery of checks to find and fix deadlocked/stuck specs.

    Args:
        queue_dir: Path to ~/.boi/queue/
        worker_specs: Dict mapping worker_id -> queue_id for currently
                      assigned workers (from daemon's in-memory state).
                      Empty string value means the worker is idle.
        db: Optional Database instance for SQLite-backed state.

    Returns:
        A list of dicts describing each self-heal action taken:
          {"action": str, "queue_id": str, "detail": str}
    """
    actions: list[dict[str, Any]] = []

    queue = db.get_queue()

    actions.extend(_heal_stale_running_specs(queue_dir, db=db))
    actions.extend(_heal_max_running_duration(queue_dir, db=db, queue=queue))
    actions.extend(_heal_orphaned_workers(queue_dir, worker_specs, db=db))
    actions.extend(_heal_circular_dependencies(queue_dir, db=db, queue=queue))
    actions.extend(_heal_blocked_by_cleanup(queue_dir, db=db))
    actions.extend(_heal_stale_lock(queue_dir))

    return actions


def _heal_stale_running_specs(
    queue_dir: str,
    db: Database,
) -> list[dict[str, Any]]:
    """Find specs with status 'running' where no worker PID is alive.

    Reset them to 'requeued' so they can be picked up again.
    Uses db.recover_running_specs() which checks PIDs from the processes table.
    """
    recovered = db.recover_running_specs()
    return [
        {
            "action": "stale_running_recovered",
            "queue_id": qid,
            "detail": f"spec {qid} stuck in running with dead PID, reset to requeued",
        }
        for qid in recovered
    ]


DEFAULT_WORKER_TIMEOUT_SECONDS = 1800  # 30 minutes


def _heal_max_running_duration(
    queue_dir: str,
    db: Database,
    queue: list[dict[str, Any]],
) -> list[dict[str, Any]]:
    """Force-fail specs that have been running longer than their max duration.

    If a spec has been in 'running' status for longer than
    max_running_duration_seconds (default: worker_timeout_seconds * max_iterations),
    force-fail it. This catches cases where the PID check is broken (PID file
    missing, PID reused by another process) and the spec sits in 'running' forever.
    """
    entries = queue

    actions: list[dict[str, Any]] = []
    now = datetime.now(timezone.utc)

    for entry in entries:
        if entry.get("status") != "running":
            continue

        first_running_at = entry.get("first_running_at")
        if not first_running_at:
            continue

        try:
            started = datetime.fromisoformat(first_running_at)
            # Ensure timezone-aware comparison
            if started.tzinfo is None:
                started = started.replace(tzinfo=timezone.utc)
        except (ValueError, TypeError):
            continue

        elapsed_seconds = (now - started).total_seconds()

        # Compute max running duration
        worker_timeout = (
            entry.get("worker_timeout_seconds") or DEFAULT_WORKER_TIMEOUT_SECONDS
        )
        max_iterations = entry.get("max_iterations") or 30
        max_duration = entry.get(
            "max_running_duration_seconds",
            worker_timeout * max_iterations,
        )

        if elapsed_seconds >= max_duration:
            queue_id = entry["id"]
            elapsed_min = int(elapsed_seconds / 60)
            max_min = int(max_duration / 60)

            # Re-check current status: a prior healer (e.g. stale_running) may
            # have already changed the status since the queue snapshot was taken.
            current = db.get_spec(queue_id)
            if not current or current.get("status") != "running":
                continue

            # Force-fail the spec
            db.fail(queue_id, "Maximum running duration exceeded")

            # Clean up PID/exit files
            for suffix in [".pid", ".exit"]:
                stale = os.path.join(queue_dir, f"{queue_id}{suffix}")
                if os.path.isfile(stale):
                    try:
                        os.remove(stale)
                    except OSError:
                        pass

            actions.append(
                {
                    "action": "max_running_duration_exceeded",
                    "queue_id": queue_id,
                    "detail": (
                        f"spec {queue_id} running for {elapsed_min}m "
                        f"(max: {max_min}m), force-failed"
                    ),
                }
            )

    return actions


def _heal_orphaned_workers(
    queue_dir: str,
    worker_specs: dict[str, str],
    db: Database,
) -> list[dict[str, Any]]:
    """Find workers assigned to specs that are already in a terminal state.

    Returns actions describing which workers should be freed. The actual
    freeing of daemon-side state (WORKER_CURRENT_SPEC etc.) must be done
    by the caller in bash since those are bash-level variables.
    """
    terminal_statuses = {"completed", "failed", "canceled"}
    actions: list[dict[str, Any]] = []

    for worker_id, queue_id in worker_specs.items():
        if not queue_id:
            continue

        entry = db.get_spec(queue_id)
        if entry is None:
            # Queue entry doesn't exist at all. Worker is orphaned.
            actions.append(
                {
                    "action": "orphaned_worker",
                    "queue_id": queue_id,
                    "worker_id": worker_id,
                    "detail": f"worker {worker_id} assigned to missing spec {queue_id}, should be freed",
                }
            )
            continue

        if entry.get("status") in terminal_statuses:
            actions.append(
                {
                    "action": "orphaned_worker",
                    "queue_id": queue_id,
                    "worker_id": worker_id,
                    "detail": f"worker {worker_id} assigned to {entry.get('status')} spec {queue_id}, should be freed",
                }
            )

    return actions


def _heal_circular_dependencies(
    queue_dir: str,
    db: Database,
    queue: list[dict[str, Any]],
) -> list[dict[str, Any]]:
    """Detect circular dependencies in blocked_by chains.

    If spec A blocks B blocks C blocks A, cancel all specs in the cycle.
    Must run BEFORE _heal_blocked_by_cleanup to detect cycles first.
    """
    entries = queue

    actions: list[dict[str, Any]] = []

    # Build adjacency: spec_id -> list of specs it's blocked by
    blocked_by_map: dict[str, list[str]] = {}
    entry_map: dict[str, dict[str, Any]] = {}
    for entry in entries:
        qid = entry["id"]
        entry_map[qid] = entry

    # Always load deps from spec_dependencies table when db is available
    cursor = db.conn.execute(
        "SELECT spec_id, blocks_on FROM spec_dependencies"
    )
    for row in cursor:
        spec_id = row["spec_id"]
        blocks_on = row["blocks_on"]
        if spec_id not in blocked_by_map:
            blocked_by_map[spec_id] = []
        blocked_by_map[spec_id].append(blocks_on)

    # Find all cycles using DFS
    visited: set[str] = set()
    in_cycle: set[str] = set()

    def find_cycle(node: str, path: list[str], path_set: set[str]) -> None:
        if node in path_set:
            # Found a cycle. Extract the cycle portion.
            cycle_start = path.index(node)
            cycle = path[cycle_start:]
            in_cycle.update(cycle)
            return
        if node in visited:
            return
        visited.add(node)
        path.append(node)
        path_set.add(node)
        for dep in blocked_by_map.get(node, []):
            find_cycle(dep, path, path_set)
        path.pop()
        path_set.discard(node)

    for qid in blocked_by_map:
        if qid not in visited:
            find_cycle(qid, [], set())

    # Cancel all specs in cycles
    for qid in in_cycle:
        entry = entry_map.get(qid)
        if entry and entry.get("status") not in ("completed", "failed", "canceled"):
            db.cancel(qid)
            db.update_spec_fields(
                qid, failure_reason="Circular dependency detected"
            )
            actions.append(
                {
                    "action": "circular_dependency_canceled",
                    "queue_id": qid,
                    "detail": f"spec {qid} canceled due to circular dependency",
                }
            )

    return actions


def _heal_blocked_by_cleanup(
    queue_dir: str,
    db: Database,
) -> list[dict[str, Any]]:
    """Clean up blocked_by references to completed/failed/canceled/missing specs.

    For each spec with blocked_by:
    - If a blocking spec is in a terminal state, remove it from blocked_by.
    - If a blocking spec ID doesn't exist, remove it from blocked_by.

    Operates on the spec_dependencies table.
    """
    return _heal_blocked_by_cleanup_db(db)


def _heal_blocked_by_cleanup_db(db: Database) -> list[dict[str, Any]]:
    """DB-backed blocked_by cleanup using spec_dependencies table."""
    terminal_statuses = {"completed", "failed", "canceled"}
    actions: list[dict[str, Any]] = []

    # Get all dependencies
    cursor = db.conn.execute(
        "SELECT sd.spec_id, sd.blocks_on, s.status "
        "FROM spec_dependencies sd "
        "LEFT JOIN specs s ON sd.blocks_on = s.id"
    )
    to_remove: list[tuple[str, str, str]] = []
    for row in cursor:
        spec_id = row["spec_id"]
        blocks_on = row["blocks_on"]
        dep_status = row["status"]
        if dep_status is None:
            to_remove.append((spec_id, blocks_on, f"{blocks_on} (missing)"))
        elif dep_status in terminal_statuses:
            to_remove.append((spec_id, blocks_on, f"{blocks_on} ({dep_status})"))

    # Group removals by spec_id for reporting
    removals_by_spec: dict[str, list[str]] = {}
    for spec_id, blocks_on, label in to_remove:
        removals_by_spec.setdefault(spec_id, []).append(label)
        with db.lock:
            db.conn.execute(
                "DELETE FROM spec_dependencies WHERE spec_id = ? AND blocks_on = ?",
                (spec_id, blocks_on),
            )
            db.conn.commit()

    for spec_id, removed in removals_by_spec.items():
        actions.append(
            {
                "action": "blocked_by_cleaned",
                "queue_id": spec_id,
                "detail": f"removed blocking deps: {', '.join(removed)}",
            }
        )

    return actions


def _heal_stale_lock(queue_dir: str) -> list[dict[str, Any]]:
    """Remove queue/.lock if the PID holding it is dead.

    Note: This checks the flock on the lock file. If the file exists but
    no process holds the lock (flock is released on process death), we
    leave it alone (the file itself is harmless; flock is what matters).

    We check if we can acquire a non-blocking flock. If we can, the lock
    is not held by anyone, so the file is stale and can be cleaned up.
    If we can't, someone is actively holding it, which is fine.
    """
    import fcntl

    lock_path = os.path.join(queue_dir, ".lock")
    actions: list[dict[str, Any]] = []

    if not os.path.isfile(lock_path):
        return actions

    try:
        fd = open(lock_path, "w")
        try:
            # Try non-blocking lock. If we get it, no one holds it.
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            # We got the lock. Release it immediately.
            fcntl.flock(fd, fcntl.LOCK_UN)
            # The lock file exists but no one holds it. This is normal
            # (the file persists between daemon runs). No action needed.
        except (BlockingIOError, OSError):
            # Lock is held by a live process. That's fine.
            pass
        finally:
            fd.close()
    except OSError:
        pass

    return actions
