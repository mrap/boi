"""Tests for three-phase pipeline advancement (execute → review → critic).

Covers:
  1. _advance_pipeline with ["execute", "review", "critic"] advances correctly.
  2. Review rejection ([REVIEW] signal) requeues back to execute phase.
  3. Review approval (## Review Approved) advances to critic.
  4. Backward compat: missing guardrails.toml defaults to ["execute", "critic"].
"""

from __future__ import annotations

import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock, call, patch

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.phases import PhaseConfig


# ---------------------------------------------------------------------------
# Minimal Daemon stub — avoids importing the full daemon (which imports
# DaemonLock, Database, etc. that need real files).
# ---------------------------------------------------------------------------

def _make_daemon(state_dir: str) -> object:
    """Return a minimal Daemon-like object with only the methods under test."""
    # Import lazily so we only pull in daemon.py if it can be imported cleanly.
    # If import fails we still want guardrails tests to pass.
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
    from daemon import Daemon  # type: ignore

    db = MagicMock()
    db.get_spec.return_value = {"tasks_done": 2, "tasks_total": 5}

    # Daemon.__init__ tries to open config_path and db_path; patch heavily.
    with patch("daemon.DaemonLock"), patch("daemon.Database", return_value=db):
        d = Daemon.__new__(Daemon)
        d.state_dir = state_dir
        d.db = db
    return d


# ---------------------------------------------------------------------------
# 1. _advance_pipeline: execute → review → critic → complete
# ---------------------------------------------------------------------------

class TestAdvancePipelineThreePhase(unittest.TestCase):

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.state_dir = self._tmp.name
        # Write a guardrails.toml with 3-phase pipeline
        guardrails = os.path.join(self.state_dir, "guardrails.toml")
        with open(guardrails, "w") as f:
            f.write('[pipeline]\ndefault = ["execute", "review", "critic"]\n')

    def tearDown(self):
        self._tmp.cleanup()

    def _make(self):
        return _make_daemon(self.state_dir)

    def test_execute_advances_to_review(self):
        d = self._make()
        d._advance_pipeline("q-1", "execute")
        d.db.requeue.assert_called_once_with("q-1", 2, 5)
        d.db.update_spec_fields.assert_called_once_with("q-1", phase="review")
        d.db.complete.assert_not_called()

    def test_review_advances_to_critic(self):
        d = self._make()
        d._advance_pipeline("q-1", "review")
        d.db.requeue.assert_called_once_with("q-1", 2, 5)
        d.db.update_spec_fields.assert_called_once_with("q-1", phase="critic")
        d.db.complete.assert_not_called()

    def test_critic_completes_spec(self):
        d = self._make()
        d._advance_pipeline("q-1", "critic")
        d.db.complete.assert_called_once_with("q-1", 2, 5)
        d.db.requeue.assert_not_called()


# ---------------------------------------------------------------------------
# 2 & 3. Review reject/approve via _handle_custom_phase_completion
# ---------------------------------------------------------------------------

class TestReviewPhaseCompletion(unittest.TestCase):

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.state_dir = self._tmp.name
        guardrails = os.path.join(self.state_dir, "guardrails.toml")
        with open(guardrails, "w") as f:
            f.write('[pipeline]\ndefault = ["execute", "review", "critic"]\n')

        # Spec file used by _handle_custom_phase_completion
        self.spec_path = os.path.join(self.state_dir, "spec.md")

        # Minimal review PhaseConfig (mirrors review.phase.toml)
        self.review_phase = PhaseConfig(
            name="review",
            prompt_template="templates/review-worker-prompt.md",
            approve_signal="## Review Approved",
            reject_signal="[REVIEW]",
            on_approve="next",
            on_reject="requeue:execute",
            on_crash="retry",
        )

    def tearDown(self):
        self._tmp.cleanup()

    def _write_spec(self, content: str):
        with open(self.spec_path, "w") as f:
            f.write(content)

    def _make(self):
        return _make_daemon(self.state_dir)

    def test_review_approve_advances_to_critic(self):
        """Spec containing '## Review Approved' advances review → critic."""
        self._write_spec("# My Spec\n\n## Review Approved\n\nAll good.\n")
        d = self._make()
        # Patch run_hooks to avoid actual hook execution
        with patch("lib.guardrail_runner.run_hooks", return_value=MagicMock(approved=True, blocked=False)):
            d._handle_custom_phase_completion("q-1", "review", self.review_phase, 0, self.spec_path)
        # review approved → _advance_pipeline("q-1", "review") → requeue + phase=critic
        d.db.requeue.assert_called_once_with("q-1", 2, 5)
        d.db.update_spec_fields.assert_called_once_with("q-1", phase="critic")

    def test_review_reject_requeues_to_execute(self):
        """Spec containing '[REVIEW]' sends spec back to execute phase."""
        self._write_spec("# My Spec\n\n### [REVIEW] t-7: Fix missing touchpoint\nPENDING\n")
        d = self._make()
        with patch("lib.guardrail_runner.run_hooks", return_value=MagicMock(approved=True, blocked=False)):
            d._handle_custom_phase_completion("q-1", "review", self.review_phase, 0, self.spec_path)
        # on_reject = "requeue:execute" → requeue + phase=execute
        d.db.requeue.assert_called_once_with("q-1", 2, 5)
        d.db.update_spec_fields.assert_called_once_with("q-1", phase="execute")
        d.db.complete.assert_not_called()


# ---------------------------------------------------------------------------
# 4. Backward compat: missing guardrails.toml → default ["execute", "critic"]
# ---------------------------------------------------------------------------

class TestAdvancePipelineDefaultFallback(unittest.TestCase):

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.state_dir = self._tmp.name
        # Deliberately do NOT write guardrails.toml

    def tearDown(self):
        self._tmp.cleanup()

    def _make(self):
        return _make_daemon(self.state_dir)

    def test_no_guardrails_execute_advances_to_critic(self):
        """Without guardrails.toml, execute phase advances directly to critic."""
        d = self._make()
        d._advance_pipeline("q-1", "execute")
        d.db.requeue.assert_called_once_with("q-1", 2, 5)
        d.db.update_spec_fields.assert_called_once_with("q-1", phase="critic")
        d.db.complete.assert_not_called()

    def test_no_guardrails_critic_completes(self):
        """Without guardrails.toml, critic is the final phase → spec completes."""
        d = self._make()
        d._advance_pipeline("q-1", "critic")
        d.db.complete.assert_called_once_with("q-1", 2, 5)
        d.db.requeue.assert_not_called()

    def test_no_guardrails_review_not_in_pipeline(self):
        """Without guardrails.toml, 'review' is not in the default pipeline → completes."""
        d = self._make()
        d._advance_pipeline("q-1", "review")
        # review not in default pipeline — treated as unknown → complete
        d.db.complete.assert_called_once_with("q-1", 2, 5)


if __name__ == "__main__":
    unittest.main()
