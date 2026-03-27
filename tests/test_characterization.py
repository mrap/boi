# test_characterization.py — Characterization tests for critical untested paths.
#
# These tests lock in the *current behavior* of complex, previously untested
# code paths in daemon_ops.py. They exist to catch unintended behavior changes
# during refactoring — not to test ideal behavior.
#
# Coverage targets (all file-based / non-DB path):
#   - process_decomposition_completion  (CC=14, 0 file-based tests before t-2, db=self.db)
#   - process_evaluation_completion     (CC=17, 0 file-based tests before t-2, db=self.db)
#   - check_needs_review_timeouts       (CC=14, 0 file-based tests before t-2, db=self.db)
#
# Uses stdlib unittest only. No live API calls, no real worktrees.

import json
import os
import sys
import tempfile
import textwrap
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.db import Database

from lib.daemon_ops import (
    check_needs_review_timeouts,
    process_decomposition_completion,
    process_evaluation_completion,
    self_heal,
)
from lib.queue import _read_entry, _write_entry, enqueue, set_running


# ── Shared helpers ────────────────────────────────────────────────────────────

VALID_DECOMPOSED_SPEC = textwrap.dedent("""\
    # Test Generate Spec

    **Workspace:** in-place

    ## Approach

    We will do it in three steps.

    ## Tasks

    ### t-1: First task
    PENDING

    **Spec:** Do the first thing.

    **Verify:** true

    ### t-2: Second task
    PENDING

    **Spec:** Do the second thing.

    **Verify:** true

    ### t-3: Third task
    PENDING

    **Spec:** Do the third thing.

    **Verify:** true
""")

GENERATE_SPEC_ALL_MET = textwrap.dedent("""\
    # [Generate] Test Spec

    **Mode:** generate

    ## Success Criteria

    - [x] Criterion one is done
    - [x] Criterion two is done

    ## Tasks

    ### t-1: Task one
    DONE

    **Spec:** Already done.

    **Verify:** true
""")

GENERATE_SPEC_UNMET = textwrap.dedent("""\
    # [Generate] Test Spec

    **Mode:** generate

    ## Success Criteria

    - [x] Criterion one is done
    - [ ] Criterion two is NOT done

    ## Tasks

    ### t-1: Task one
    DONE

    **Spec:** Already done.

    **Verify:** true

    ### t-2: New evaluation task
    PENDING

    **Spec:** Fix criterion two.

    **Verify:** true
""")


class DecompositionTestCase(unittest.TestCase):
    """Base for process_decomposition_completion tests (file-based path)."""

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.boi_state = self._tmpdir.name
        self.queue_dir = os.path.join(self.boi_state, "queue")
        self.events_dir = os.path.join(self.boi_state, "events")
        os.makedirs(self.queue_dir)
        os.makedirs(self.events_dir)
        # Create SQLite database
        db_path = os.path.join(self.boi_state, "boi.db")
        self.db = Database(db_path, self.queue_dir)


    def tearDown(self):
        self.db.close()
        self._tmpdir.cleanup()

    def _make_spec(self, content: str) -> str:
        path = os.path.join(self._tmpdir.name, "spec.md")
        Path(path).write_text(content, encoding="utf-8")
        return path

    def _enqueue(self, spec_path: str) -> dict:
        entry = self.db.enqueue(spec_path)
        # Put into decompose phase via DB
        self.db.update_spec_fields(entry["id"], phase="decompose")
        self.db.set_running(entry["id"], "w-1", phase="decompose")
        return self.db.get_spec(entry["id"])


class TestProcessDecompositionCompletion(DecompositionTestCase):
    """Characterization tests for process_decomposition_completion(db=self.db) file-based path."""

    def test_crash_first_attempt_retries(self):
        """First crash → decomposition_retry (retry_count becomes 1)."""
        spec_path = self._make_spec(VALID_DECOMPOSED_SPEC)
        entry = self._enqueue(spec_path)

        result = process_decomposition_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            spec_path=spec_path,
            exit_code=None,  # crash,
            db=self.db,
        )

        self.assertEqual(result["outcome"], "decomposition_retry")
        self.assertEqual(result["phase"], "decompose")
        self.assertEqual(result["retry_count"], 1)
        # Queue entry should be requeued
        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "requeued")
        self.assertEqual(updated["decomposition_retries"], 1)

    def test_crash_second_attempt_fails_permanently(self):
        """Second crash → decomposition_failed (max retries)."""
        spec_path = self._make_spec(VALID_DECOMPOSED_SPEC)
        entry = self._enqueue(spec_path)
        # Simulate already having retried once
        self.db.update_spec_fields(entry["id"], decomposition_retries=1)

        result = process_decomposition_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            spec_path=spec_path,
            exit_code=None,
        
            db=self.db,
        )

        self.assertEqual(result["outcome"], "decomposition_failed")
        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "failed")

    def test_nonzero_exit_code_retries(self):
        """Non-zero exit code → same retry path as crash."""
        spec_path = self._make_spec(VALID_DECOMPOSED_SPEC)
        entry = self._enqueue(spec_path)

        result = process_decomposition_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            spec_path=spec_path,
            exit_code="1",
        
            db=self.db,
        )

        self.assertEqual(result["outcome"], "decomposition_retry")

    def test_valid_spec_transitions_to_execute(self):
        """Valid decomposed spec → decomposition_complete, phase=execute."""
        spec_path = self._make_spec(VALID_DECOMPOSED_SPEC)
        entry = self._enqueue(spec_path)

        result = process_decomposition_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            spec_path=spec_path,
            exit_code="0",
        
            db=self.db,
        )

        self.assertEqual(result["outcome"], "decomposition_complete")
        self.assertEqual(result["phase"], "execute")
        self.assertEqual(result["task_count"], 3)
        # Queue entry should be requeued for execute phase
        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "requeued")
        self.assertEqual(updated["phase"], "execute")

    def test_too_few_tasks_retries(self):
        """Spec with fewer than 3 tasks → decomposition_retry (validation failure)."""
        spec_path = self._make_spec(textwrap.dedent("""\
            # Thin Spec

            ## Approach

            Just one thing.

            ## Tasks

            ### t-1: Only task
            PENDING

            **Spec:** Do it.

            **Verify:** true
        """))
        entry = self._enqueue(spec_path)

        result = process_decomposition_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            spec_path=spec_path,
            exit_code="0",
        
            db=self.db,
        )

        self.assertEqual(result["outcome"], "decomposition_retry")
        self.assertIn("errors", result)
        self.assertTrue(any("Too few tasks" in e for e in result["errors"]))

    def test_missing_approach_section_retries(self):
        """Spec without ## Approach → decomposition_retry."""
        spec_path = self._make_spec(textwrap.dedent("""\
            # Spec Without Approach

            ## Tasks

            ### t-1: Task one
            PENDING

            **Spec:** Do it.

            **Verify:** true

            ### t-2: Task two
            PENDING

            **Spec:** Do it.

            **Verify:** true

            ### t-3: Task three
            PENDING

            **Spec:** Do it.

            **Verify:** true
        """))
        entry = self._enqueue(spec_path)

        result = process_decomposition_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            spec_path=spec_path,
            exit_code="0",
        
            db=self.db,
        )

        self.assertEqual(result["outcome"], "decomposition_retry")
        self.assertIn("errors", result)

    def test_missing_queue_entry_returns_error(self):
        """Missing queue entry → error outcome."""
        spec_path = self._make_spec(VALID_DECOMPOSED_SPEC)

        result = process_decomposition_completion(
            queue_dir=self.queue_dir,
            queue_id="q-999",
            events_dir=self.events_dir,
            spec_path=spec_path,
            exit_code="0",
        
            db=self.db,
        )

        self.assertEqual(result["outcome"], "error")

    def test_writes_event_on_success(self):
        """Successful decomposition writes decomposition_complete event."""
        spec_path = self._make_spec(VALID_DECOMPOSED_SPEC)
        entry = self._enqueue(spec_path)

        process_decomposition_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            spec_path=spec_path,
            exit_code="0",
        
            db=self.db,
        )

        events = list(Path(self.events_dir).glob("event-*.json"))
        self.assertGreater(len(events), 0)
        event_types = [json.loads(e.read_text())["type"] for e in events]
        self.assertIn("decomposition_complete", event_types)


# ── process_evaluation_completion ─────────────────────────────────────────────


class EvaluationTestCase(unittest.TestCase):
    """Base for process_evaluation_completion tests (file-based path)."""

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.boi_state = self._tmpdir.name
        self.queue_dir = os.path.join(self.boi_state, "queue")
        self.events_dir = os.path.join(self.boi_state, "events")
        self.hooks_dir = os.path.join(self.boi_state, "hooks")
        os.makedirs(self.queue_dir)
        os.makedirs(self.events_dir)
        os.makedirs(self.hooks_dir)
        # Create SQLite database
        db_path = os.path.join(self.boi_state, "boi.db")
        self.db = Database(db_path, self.queue_dir)


    def tearDown(self):
        self.db.close()
        self._tmpdir.cleanup()

    def _make_spec(self, content: str, name: str = "spec.md") -> str:
        path = os.path.join(self._tmpdir.name, name)
        Path(path).write_text(content, encoding="utf-8")
        return path

    def _enqueue_generate(self, spec_path: str, iteration: int = 1, max_iterations: int = 10) -> dict:
        entry = self.db.enqueue(spec_path)
        # Update fields via DB
        self.db.conn.execute(
            "UPDATE specs SET phase='evaluate', iteration=?, max_iterations=?, tasks_done=1, tasks_total=2 WHERE id=?",
            (iteration, max_iterations, entry["id"]),
        )
        self.db.conn.commit()
        self.db.set_running(entry["id"], "w-1", phase="evaluate")
        return self.db.get_spec(entry["id"])


class TestProcessEvaluationCompletion(EvaluationTestCase):
    """Characterization tests for process_evaluation_completion(db=self.db) file-based path."""

    def test_crash_requeues_for_retry(self):
        """Crash on first attempt → evaluate_crashed, requeued."""
        spec_path = self._make_spec(GENERATE_SPEC_ALL_MET)
        entry = self._enqueue_generate(spec_path)

        result = process_evaluation_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=spec_path,
            exit_code=None,
        
            db=self.db,
        )

        self.assertEqual(result["outcome"], "evaluate_crashed")
        updated = self.db.get_spec(entry["id"])
        # After first crash, should be requeued for retry
        self.assertIn(updated["status"], ("requeued", "failed"))

    def test_consecutive_crashes_fail_permanently(self):
        """Repeated crashes → evaluate_crashed with consecutive_failures reason."""
        spec_path = self._make_spec(GENERATE_SPEC_ALL_MET)
        entry = self._enqueue_generate(spec_path)
        # Simulate max consecutive failures
        self.db.update_spec_fields(entry["id"], consecutive_failures=4)
        self.db.set_running(entry["id"], "w-1")

        result = process_evaluation_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=spec_path,
            exit_code=None,
        
            db=self.db,
        )

        self.assertEqual(result["outcome"], "evaluate_crashed")
        self.assertEqual(result.get("reason"), "consecutive_failures")
        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "failed")

    def test_all_criteria_met_converges(self):
        """All criteria met → evaluate_converged with goal_achieved."""
        spec_path = self._make_spec(GENERATE_SPEC_ALL_MET)
        entry = self._enqueue_generate(spec_path)

        result = process_evaluation_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=spec_path,
            exit_code="0",
        
            db=self.db,
        )

        self.assertIn(result["outcome"], ("evaluate_converged",))
        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "completed")

    def test_unmet_criteria_with_pending_tasks_loops_back(self):
        """Unmet criteria + pending tasks → evaluate_loop_back, execute phase."""
        spec_path = self._make_spec(GENERATE_SPEC_UNMET)
        entry = self._enqueue_generate(spec_path, iteration=2, max_iterations=10)

        result = process_evaluation_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=spec_path,
            exit_code="0",
        
            db=self.db,
        )

        self.assertEqual(result["outcome"], "evaluate_loop_back")
        self.assertEqual(result["phase"], "execute")
        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "requeued")
        self.assertEqual(updated["phase"], "execute")

    def test_max_iterations_forces_convergence(self):
        """Max iterations reached → evaluate_converged (max_iterations reason)."""
        spec_path = self._make_spec(GENERATE_SPEC_UNMET)
        # Set iteration == max_iterations to trigger forced stop
        entry = self._enqueue_generate(spec_path, iteration=10, max_iterations=10)

        result = process_evaluation_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=spec_path,
            exit_code="0",
        
            db=self.db,
        )

        self.assertEqual(result["outcome"], "evaluate_converged")
        self.assertEqual(result.get("status"), "max_iterations")
        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "completed")

    def test_missing_entry_returns_error(self):
        """Missing queue entry → error outcome."""
        spec_path = self._make_spec(GENERATE_SPEC_ALL_MET)

        result = process_evaluation_completion(
            queue_dir=self.queue_dir,
            queue_id="q-999",
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=spec_path,
            exit_code="0",
        
            db=self.db,
        )

        self.assertEqual(result["outcome"], "error")

    def test_writes_event_on_convergence(self):
        """Convergence writes generate_completed event."""
        spec_path = self._make_spec(GENERATE_SPEC_ALL_MET)
        entry = self._enqueue_generate(spec_path)

        process_evaluation_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=spec_path,
            exit_code="0",
        
            db=self.db,
        )

        events = list(Path(self.events_dir).glob("event-*.json"))
        event_types = [json.loads(e.read_text())["type"] for e in events]
        self.assertIn("generate_completed", event_types)


# ── check_needs_review_timeouts ───────────────────────────────────────────────


class NeedsReviewTimeoutTestCase(unittest.TestCase):
    """Base for check_needs_review_timeouts tests (file-based path)."""

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.boi_state = self._tmpdir.name
        self.queue_dir = os.path.join(self.boi_state, "queue")
        self.events_dir = os.path.join(self.boi_state, "events")
        os.makedirs(self.queue_dir)
        os.makedirs(self.events_dir)
        # Create SQLite database
        db_path = os.path.join(self.boi_state, "boi.db")
        self.db = Database(db_path, self.queue_dir)


    def tearDown(self):
        self.db.close()
        self._tmpdir.cleanup()

    def _make_spec(self, content: str) -> str:
        path = os.path.join(self._tmpdir.name, "spec.md")
        Path(path).write_text(content, encoding="utf-8")
        return path

    def _make_needs_review_entry(
        self,
        queue_id: str = "q-001",
        hours_ago: float = 25.0,
    ) -> dict:
        """Create a queue entry in needs_review status with needs_review_since set."""
        spec_path = self._make_spec("# Spec\n\n## Tasks\n\n### t-1: Task\nPENDING\n\n**Spec:** x\n\n**Verify:** true\n")
        past_time = datetime.now(timezone.utc) - timedelta(hours=hours_ago)
        # Enqueue via DB and update to needs_review status
        entry = self.db.enqueue(spec_path)
        # Override the ID to match what tests expect (q-001, q-006, etc)
        self.db.conn.execute("UPDATE specs SET id=? WHERE id=?", (queue_id, entry["id"]))
        self.db.conn.commit()
        # Set to needs_review status with backdated needs_review_since
        self.db.conn.execute(
            "UPDATE specs SET status='needs_review', needs_review_since=?, submitted_at=? WHERE id=?",
            (past_time.isoformat(), past_time.isoformat(), queue_id),
        )
        self.db.conn.commit()
        return self.db.get_spec(queue_id)


class TestCheckNeedsReviewTimeouts(NeedsReviewTimeoutTestCase):
    """Characterization tests for check_needs_review_timeouts(db=self.db) file-based path."""

    def test_no_needs_review_specs_returns_empty(self):
        """With no needs_review specs, returns empty list."""
        result = check_needs_review_timeouts(
            queue_dir=self.queue_dir,
            events_dir=self.events_dir,
            state_dir=self.boi_state,
        
            db=self.db,
        )

        self.assertEqual(result, [])

    def test_non_needs_review_specs_are_ignored(self):
        """Specs with other statuses are not auto-rejected."""
        spec_path = self._make_spec("# Spec\n\n## Tasks\n")
        entry = self.db.enqueue(spec_path)
        # Entry is in 'queued' status, not needs_review

        result = check_needs_review_timeouts(
            queue_dir=self.queue_dir,
            events_dir=self.events_dir,
            state_dir=self.boi_state,
        
            db=self.db,
        )

        self.assertEqual(result, [])

    def test_timed_out_spec_is_auto_rejected(self):
        """needs_review spec past timeout → auto-rejected, requeued."""
        # Default timeout is 24 hours; set to 25 hours ago
        entry = self._make_needs_review_entry(queue_id="q-001", hours_ago=25.0)

        result = check_needs_review_timeouts(
            queue_dir=self.queue_dir,
            events_dir=self.events_dir,
            state_dir=self.boi_state,
        
            db=self.db,
        )

        self.assertIn("q-001", result)
        updated = self.db.get_spec("q-001")
        self.assertEqual(updated["status"], "requeued")
        # needs_review_since should be cleared (None in DB)
        self.assertIsNone(updated.get("needs_review_since"))

    def test_not_timed_out_spec_is_untouched(self):
        """needs_review spec within timeout → not auto-rejected."""
        # 1 hour ago — within default 24-hour timeout
        entry = self._make_needs_review_entry(queue_id="q-002", hours_ago=1.0)

        result = check_needs_review_timeouts(
            queue_dir=self.queue_dir,
            events_dir=self.events_dir,
            state_dir=self.boi_state,
        
            db=self.db,
        )

        self.assertNotIn("q-002", result)
        updated = self.db.get_spec("q-002")
        self.assertEqual(updated["status"], "needs_review")

    def test_timed_out_spec_writes_event(self):
        """Auto-rejected spec writes experiment_auto_rejected event."""
        self._make_needs_review_entry(queue_id="q-003", hours_ago=48.0)

        check_needs_review_timeouts(
            queue_dir=self.queue_dir,
            events_dir=self.events_dir,
            state_dir=self.boi_state,
        
            db=self.db,
        )

        events = list(Path(self.events_dir).glob("event-*.json"))
        event_types = [json.loads(e.read_text())["type"] for e in events]
        self.assertIn("experiment_auto_rejected", event_types)

    def test_custom_timeout_from_config(self):
        """Custom timeout_hours in config is respected."""
        # Write a config with 2-hour timeout
        config = {"experiment_timeout_hours": 2.0}
        config_path = os.path.join(self.boi_state, "config.json")
        Path(config_path).write_text(json.dumps(config), encoding="utf-8")

        # Entry that's 3 hours old — exceeds custom 2-hour timeout
        entry = self._make_needs_review_entry(queue_id="q-004", hours_ago=3.0)

        result = check_needs_review_timeouts(
            queue_dir=self.queue_dir,
            events_dir=self.events_dir,
            state_dir=self.boi_state,
        
            db=self.db,
        )

        self.assertIn("q-004", result)

    def test_missing_needs_review_since_is_skipped(self):
        """Entry with status=needs_review but no needs_review_since is skipped."""
        spec_path = self._make_spec("# Spec\n\n## Tasks\n")
        entry = {
            "id": "q-005",
            "spec_path": spec_path,
            "status": "needs_review",
            # No needs_review_since field
            "priority": 100,
            "iteration": 1,
            "max_iterations": 30,
            "blocked_by": [],
            "consecutive_failures": 0,
            "tasks_done": 0,
            "tasks_total": 1,
            "submitted_at": datetime.now(timezone.utc).isoformat(),
        }
        _write_entry(self.queue_dir, entry)

        result = check_needs_review_timeouts(
            queue_dir=self.queue_dir,
            events_dir=self.events_dir,
            state_dir=self.boi_state,
        
            db=self.db,
        )

        self.assertNotIn("q-005", result)

    def test_multiple_timed_out_specs_all_rejected(self):
        """Multiple timed-out specs are all auto-rejected in one call."""
        self._make_needs_review_entry(queue_id="q-006", hours_ago=30.0)
        self._make_needs_review_entry(queue_id="q-007", hours_ago=26.0)

        result = check_needs_review_timeouts(
            queue_dir=self.queue_dir,
            events_dir=self.events_dir,
            state_dir=self.boi_state,
        
            db=self.db,
        )

        self.assertIn("q-006", result)
        self.assertIn("q-007", result)


# ── process_critic_completion ─────────────────────────────────────────────────

CRITIC_APPROVED_SPEC = textwrap.dedent("""\
    # Test Spec

    ## Tasks

    ### t-1: Done task
    DONE

    **Spec:** Done.

    **Verify:** true

    ## Critic Approved

    Looks good.
""")

CRITIC_TASKS_SPEC = textwrap.dedent("""\
    # Test Spec

    ## Tasks

    ### t-1: Done task
    DONE

    **Spec:** Done.

    **Verify:** true

    ### t-2: Fix this [CRITIC]
    PENDING

    **Spec:** Fix it.

    **Verify:** true
""")


class CriticCompletionTestCase(unittest.TestCase):
    """Base for process_critic_completion tests."""

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.boi_state = self._tmpdir.name
        self.queue_dir = os.path.join(self.boi_state, "queue")
        self.events_dir = os.path.join(self.boi_state, "events")
        self.hooks_dir = os.path.join(self.boi_state, "hooks")
        os.makedirs(self.queue_dir)
        os.makedirs(self.events_dir)
        os.makedirs(self.hooks_dir)
        # Create SQLite database
        db_path = os.path.join(self.boi_state, "boi.db")
        self.db = Database(db_path, self.queue_dir)


        # Disable critic config so queue helpers don't fail
        critic_dir = os.path.join(self.boi_state, "critic")
        os.makedirs(critic_dir, exist_ok=True)
        os.makedirs(os.path.join(critic_dir, "custom"), exist_ok=True)
        Path(os.path.join(critic_dir, "config.json")).write_text(
            json.dumps({"enabled": False}) + "\n"
        )

    def tearDown(self):
        self.db.close()
        self._tmpdir.cleanup()

    def _make_spec(self, content: str) -> str:
        path = os.path.join(self._tmpdir.name, "spec.md")
        Path(path).write_text(content, encoding="utf-8")
        return path

    def _enqueue_entry(self, spec_path: str) -> dict:
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")
        return self.db.get_spec(entry["id"])


class TestProcessCriticCompletion(CriticCompletionTestCase):
    """Characterization tests for process_critic_completion(db=self.db) (CC=16)."""

    def test_critic_approved_returns_approved_outcome(self):
        """Spec with ## Critic Approved → outcome is 'critic_approved'."""
        from lib.daemon_ops import process_critic_completion

        spec_path = self._make_spec(CRITIC_APPROVED_SPEC)
        entry = self._enqueue_entry(spec_path)

        result = process_critic_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=entry["spec_path"],
        
            db=self.db,
        )

        self.assertEqual(result["outcome"], "critic_approved")

    def test_critic_approved_marks_spec_completed(self):
        """Critic approval marks queue entry as completed."""
        from lib.daemon_ops import process_critic_completion

        spec_path = self._make_spec(CRITIC_APPROVED_SPEC)
        entry = self._enqueue_entry(spec_path)

        process_critic_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=entry["spec_path"],
        
            db=self.db,
        )

        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "completed")

    def test_critic_tasks_added_requeues(self):
        """Spec with [CRITIC] PENDING tasks → spec is requeued for workers."""
        from lib.daemon_ops import process_critic_completion

        spec_path = self._make_spec(CRITIC_TASKS_SPEC)
        entry = self._enqueue_entry(spec_path)

        result = process_critic_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=entry["spec_path"],
        
            db=self.db,
        )

        self.assertIn(result["outcome"], ("critic_tasks_added", "requeued"))
        updated = self.db.get_spec(entry["id"])
        self.assertEqual(updated["status"], "requeued")

    def test_critic_passes_incremented(self):
        """Each call increments critic_passes on the queue entry."""
        from lib.daemon_ops import process_critic_completion

        spec_path = self._make_spec(CRITIC_APPROVED_SPEC)
        entry = self._enqueue_entry(spec_path)
        initial = entry.get("critic_passes") or 0

        process_critic_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=entry["spec_path"],
        
            db=self.db,
        )

        # critic_passes should have incremented by 1
        updated = self.db.get_spec(entry["id"])
        self.assertEqual((updated.get("critic_passes") or 0), initial + 1)

    def test_missing_entry_returns_error(self):
        """Missing queue entry → outcome 'error'."""
        from lib.daemon_ops import process_critic_completion

        spec_path = self._make_spec(CRITIC_APPROVED_SPEC)

        result = process_critic_completion(
            queue_dir=self.queue_dir,
            queue_id="q-999",
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path=spec_path,
        
            db=self.db,
        )

        self.assertEqual(result["outcome"], "error")

    def test_nonexistent_spec_file_returns_not_approved(self):
        """Missing spec file → not approved (parse_critic_result returns safe defaults)."""
        from lib.daemon_ops import process_critic_completion

        spec_path = self._make_spec(CRITIC_TASKS_SPEC)
        entry = self._enqueue_entry(spec_path)

        # Use a path that doesn't exist
        result = process_critic_completion(
            queue_dir=self.queue_dir,
            queue_id=entry["id"],
            events_dir=self.events_dir,
            hooks_dir=self.hooks_dir,
            spec_path="/nonexistent/spec.md",
        
            db=self.db,
        )

        # Should not raise; should return a valid outcome
        self.assertIn("outcome", result)
        self.assertNotEqual(result["outcome"], "error")


# ── self_heal ─────────────────────────────────────────────────────────────────


class SelfHealTestCase(unittest.TestCase):
    """Base for self_heal tests."""

    def setUp(self):
        self._tmpdir = tempfile.TemporaryDirectory()
        self.boi_state = self._tmpdir.name
        self.queue_dir = os.path.join(self.boi_state, "queue")
        os.makedirs(self.queue_dir)
        # Create SQLite database
        db_path = os.path.join(self.boi_state, "boi.db")
        self.db = Database(db_path, self.queue_dir)


    def tearDown(self):
        self.db.close()
        self._tmpdir.cleanup()

    def _make_spec(self) -> str:
        path = os.path.join(self._tmpdir.name, "spec.md")
        Path(path).write_text(
            "# Spec\n\n## Tasks\n\n### t-1: Task\nPENDING\n\n**Spec:** x\n\n**Verify:** true\n",
            encoding="utf-8",
        )
        return path


class TestSelfHeal(SelfHealTestCase):
    """Characterization tests for self_heal(db=self.db) (wraps 5 sub-healers)."""

    def test_empty_queue_returns_no_actions(self):
        """Empty queue → empty actions list."""
        result = self_heal(queue_dir=self.queue_dir, worker_specs={}, db=self.db)
        self.assertEqual(result, [])

    def test_returns_list_of_dicts(self):
        """self_heal always returns a list; each item is a dict with 'action' key."""
        result = self_heal(queue_dir=self.queue_dir, worker_specs={}, db=self.db)
        self.assertIsInstance(result, list)
        for action in result:
            self.assertIsInstance(action, dict)
            self.assertIn("action", action)

    def test_queued_spec_not_healed(self):
        """A spec in normal 'queued' state does not trigger any heal action."""
        spec_path = self._make_spec()
        self.db.enqueue(spec_path)

        result = self_heal(queue_dir=self.queue_dir, worker_specs={"w-1": ""}, db=self.db)
        self.assertEqual(result, [])

    def test_stale_running_no_pid_file_is_recovered(self):
        """A spec stuck in 'running' with dead PID is reset to requeued."""
        spec_path = self._make_spec()
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")

        # Register worker with a dead PID so recover_running_specs detects it as stale
        # Use 999999999 as a PID that won't exist
        self.db.register_worker("w-1", self.queue_dir)
        self.db.conn.execute(
            "UPDATE workers SET current_spec_id=?, current_pid=999999999 WHERE id=?",
            (entry["id"], "w-1"),
        )
        self.db.conn.commit()

        actions = self_heal(queue_dir=self.queue_dir, worker_specs={}, db=self.db)

        action_types = [a.get("action", "") for a in actions]
        self.assertTrue(
            any("stale" in t or "running" in t or "recovered" in t for t in action_types),
            f"Expected a stale-running heal action; got: {action_types}",
        )
        recovered = self.db.get_spec(entry["id"])
        self.assertIn(recovered["status"], ("requeued", "queued"))

    def test_completed_spec_not_healed(self):
        """A completed spec is not touched by self_heal."""
        spec_path = self._make_spec()
        entry = self.db.enqueue(spec_path)
        self.db.set_running(entry["id"], "w-1")
        self.db.complete(entry["id"], 0, 0)

        result = self_heal(queue_dir=self.queue_dir, worker_specs={}, db=self.db)
        self.assertEqual(result, [])


if __name__ == "__main__":
    unittest.main()
