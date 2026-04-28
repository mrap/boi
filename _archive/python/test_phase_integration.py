"""Integration test: full pipeline plan-critique -> execute -> task-verify -> code-review.

Verifies that pipeline routing is correct for all 4 phases:
1. plan-critique approves -> advances to execute
2. execute completes -> advances to task-verify
3. task-verify completes -> advances to code-review
4. code-review triggers (>50 lines changed) and approves -> pipeline complete

LLM responses are simulated by writing approve signals directly into the spec file.
The pipeline routing logic (Daemon._advance_pipeline and
Daemon._handle_custom_phase_completion) runs unmodified.
"""

from __future__ import annotations

import os
import re
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock, call, patch

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.phases import PhaseConfig, discover_phases, should_trigger

REPO_ROOT = Path(__file__).resolve().parent.parent
PHASES_DIR = REPO_ROOT / "phases"

# ---------------------------------------------------------------------------
# Synthetic spec fixture: 2 tasks (create Python file + write test)
# ---------------------------------------------------------------------------

SYNTHETIC_SPEC = """\
# Synthetic Integration Spec

**Mode:** execute

## Context

Small spec to validate the full pipeline. Creates a Python utility function
and a test for it.

### t-1: Create utility function
PENDING

**Spec:** Create `utils/math_utils.py` with a function `add(a, b)` that
returns the sum of two numbers. Include type hints.

**Verify:** `python -c "from utils.math_utils import add; assert add(2, 3) == 5"`

### t-2: Write test for utility function
PENDING

**Blocked by:** t-1

**Spec:** Create `tests/test_math_utils.py` with a test class `TestAdd` that
tests: (a) positive integers, (b) negative integers, (c) zero inputs.

**Verify:** `python -m pytest tests/test_math_utils.py -v 2>&1 | grep -q "passed"`
"""

# Spec as it appears after plan-critique approves it
SPEC_AFTER_PLAN_CRITIQUE = SYNTHETIC_SPEC + "\n## Plan Approved\n\nSpec evaluated: all checks passed.\n"

# Spec after tasks are completed by execute phase
SYNTHETIC_SPEC_DONE = SYNTHETIC_SPEC.replace(
    "### t-1: Create utility function\nPENDING",
    "### t-1: Create utility function\nDONE",
).replace(
    "### t-2: Write test for utility function\nPENDING",
    "### t-2: Write test for utility function\nDONE",
)

# Spec after code-review approves (final state)
SPEC_AFTER_CODE_REVIEW = SYNTHETIC_SPEC_DONE + "\n## Code Review Approved\n\nNo critical issues found.\n"


# ---------------------------------------------------------------------------
# Helper: build a minimal Daemon with mocked DB and real phase configs
# ---------------------------------------------------------------------------

def _make_daemon(state_dir: str, db: MagicMock) -> object:
    """Return a Daemon instance with mocked DB and real phase configs loaded."""
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
    from daemon import Daemon

    with patch("daemon.DaemonLock"), patch("daemon.Database", return_value=db):
        d = Daemon.__new__(Daemon)
        d.state_dir = state_dir
        d.db = db
        d.phase_configs = discover_phases(str(PHASES_DIR))
    return d


# ---------------------------------------------------------------------------
# 1. Validate the synthetic spec fixture
# ---------------------------------------------------------------------------

class TestSyntheticSpec(unittest.TestCase):
    """Verify the synthetic spec fixture is well-formed."""

    def test_has_two_tasks(self):
        tasks = re.findall(r"### t-\d+:", SYNTHETIC_SPEC)
        self.assertEqual(len(tasks), 2, "Synthetic spec must have exactly 2 tasks")

    def test_has_verify_commands(self):
        self.assertIn("**Verify:**", SYNTHETIC_SPEC)

    def test_has_blocked_by(self):
        """t-2 declares its dependency on t-1."""
        self.assertIn("**Blocked by:**", SYNTHETIC_SPEC)

    def test_no_em_dashes(self):
        self.assertNotIn("—", SYNTHETIC_SPEC, "No em dashes allowed in artifacts")

    def test_plan_approve_signal_in_post_critique_spec(self):
        self.assertIn("## Plan Approved", SPEC_AFTER_PLAN_CRITIQUE)

    def test_code_review_approve_signal_in_final_spec(self):
        self.assertIn("## Code Review Approved", SPEC_AFTER_CODE_REVIEW)


# ---------------------------------------------------------------------------
# 2. Phase 1: plan-critique approves -> routes to execute
# ---------------------------------------------------------------------------

class TestPlanCritiqueRouting(unittest.TestCase):
    """plan-critique with approve signal advances the pipeline to execute."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.state_dir = self._tmp.name
        guardrails = os.path.join(self.state_dir, "guardrails.toml")
        with open(guardrails, "w") as f:
            f.write(
                '[pipeline]\n'
                'default = ["plan-critique", "execute", "task-verify", "code-review"]\n'
            )
        self.spec_path = os.path.join(self.state_dir, "spec.md")
        with open(self.spec_path, "w") as f:
            f.write(SPEC_AFTER_PLAN_CRITIQUE)

    def tearDown(self):
        self._tmp.cleanup()

    def _make(self) -> object:
        db = MagicMock()
        db.get_spec.return_value = {"tasks_done": 0, "tasks_total": 2}
        return _make_daemon(self.state_dir, db)

    def test_plan_critique_config_loaded(self):
        """plan-critique phase config must be discoverable from phases/."""
        d = self._make()
        self.assertIn("plan-critique", d.phase_configs)

    def test_approve_signal_routes_to_execute(self):
        """plan-critique approval advances pipeline to execute."""
        d = self._make()
        phase_config = d.phase_configs["plan-critique"]

        with patch("lib.guardrail_runner.run_hooks", return_value=MagicMock()):
            d._handle_custom_phase_completion(
                "q-test", "plan-critique", phase_config, 0, self.spec_path
            )

        # _advance_pipeline should have been called, setting phase=execute
        d.db.requeue.assert_called_once_with("q-test", 0, 2)
        d.db.update_spec_fields.assert_any_call("q-test", phase="execute")
        d.db.complete.assert_not_called()
        d.db.fail.assert_not_called()

    def test_reject_signal_fails_spec(self):
        """plan-critique rejection (on_reject=fail) marks spec failed."""
        with open(self.spec_path, "w") as f:
            f.write(
                SYNTHETIC_SPEC
                + "\n### [PLAN-CRITIQUE] t-fix-1: Add exit condition\nPENDING\n"
            )
        d = self._make()
        phase_config = d.phase_configs["plan-critique"]

        with patch("lib.guardrail_runner.run_hooks", return_value=MagicMock()):
            d._handle_custom_phase_completion(
                "q-test", "plan-critique", phase_config, 0, self.spec_path
            )

        d.db.fail.assert_called_once()
        d.db.complete.assert_not_called()


# ---------------------------------------------------------------------------
# 3. Phase 2: execute completes -> routes to task-verify
# ---------------------------------------------------------------------------

class TestExecuteAdvancesToTaskVerify(unittest.TestCase):
    """execute completion advances pipeline to task-verify."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.state_dir = self._tmp.name
        guardrails = os.path.join(self.state_dir, "guardrails.toml")
        with open(guardrails, "w") as f:
            f.write(
                '[pipeline]\n'
                'default = ["plan-critique", "execute", "task-verify", "code-review"]\n'
            )

    def tearDown(self):
        self._tmp.cleanup()

    def _make(self) -> object:
        db = MagicMock()
        db.get_spec.return_value = {"tasks_done": 2, "tasks_total": 2}
        return _make_daemon(self.state_dir, db)

    def test_advance_from_execute_sets_task_verify(self):
        """Calling _advance_pipeline('execute') advances to task-verify."""
        d = self._make()
        d._advance_pipeline("q-test", "execute")
        d.db.requeue.assert_called_once_with("q-test", 2, 2)
        d.db.update_spec_fields.assert_any_call("q-test", phase="task-verify")
        d.db.complete.assert_not_called()


# ---------------------------------------------------------------------------
# 4. Phase 3: task-verify completes -> routes to code-review
# ---------------------------------------------------------------------------

class TestTaskVerifyAdvancesToCodeReview(unittest.TestCase):
    """task-verify completion advances pipeline to code-review."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.state_dir = self._tmp.name
        guardrails = os.path.join(self.state_dir, "guardrails.toml")
        with open(guardrails, "w") as f:
            f.write(
                '[pipeline]\n'
                'default = ["plan-critique", "execute", "task-verify", "code-review"]\n'
            )

    def tearDown(self):
        self._tmp.cleanup()

    def _make(self) -> object:
        db = MagicMock()
        db.get_spec.return_value = {"tasks_done": 2, "tasks_total": 2}
        return _make_daemon(self.state_dir, db)

    def test_advance_from_task_verify_sets_code_review(self):
        """Calling _advance_pipeline('task-verify') advances to code-review."""
        d = self._make()
        d._advance_pipeline("q-test", "task-verify")
        d.db.requeue.assert_called_once_with("q-test", 2, 2)
        d.db.update_spec_fields.assert_any_call("q-test", phase="code-review")
        d.db.complete.assert_not_called()


# ---------------------------------------------------------------------------
# 5. Phase 4: code-review triggers on >50 lines changed and approves
# ---------------------------------------------------------------------------

class TestCodeReviewRouting(unittest.TestCase):
    """code-review triggers when lines changed > 50 and approves to complete pipeline."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.state_dir = self._tmp.name
        guardrails = os.path.join(self.state_dir, "guardrails.toml")
        with open(guardrails, "w") as f:
            f.write(
                '[pipeline]\n'
                'default = ["plan-critique", "execute", "task-verify", "code-review"]\n'
            )
        self.spec_path = os.path.join(self.state_dir, "spec.md")
        with open(self.spec_path, "w") as f:
            f.write(SPEC_AFTER_CODE_REVIEW)

    def tearDown(self):
        self._tmp.cleanup()

    def _make(self) -> object:
        db = MagicMock()
        db.get_spec.return_value = {"tasks_done": 2, "tasks_total": 2}
        return _make_daemon(self.state_dir, db)

    def test_code_review_config_loaded(self):
        d = self._make()
        self.assertIn("code-review", d.phase_configs)

    def test_code_review_triggers_when_lines_exceed_50(self):
        """code-review only runs when lines changed strictly exceeds 50."""
        d = self._make()
        cfg = d.phase_configs["code-review"]
        # The spec produces >50 lines of code changes -- must trigger
        self.assertTrue(should_trigger(cfg, lines_changed=51))
        self.assertTrue(should_trigger(cfg, lines_changed=100))

    def test_code_review_does_not_trigger_at_or_below_threshold(self):
        """code-review skips when lines changed <= 50."""
        d = self._make()
        cfg = d.phase_configs["code-review"]
        self.assertFalse(should_trigger(cfg, lines_changed=50))
        self.assertFalse(should_trigger(cfg, lines_changed=10))
        self.assertFalse(should_trigger(cfg, lines_changed=0))

    def test_approve_signal_completes_pipeline(self):
        """code-review approval completes the pipeline (last phase)."""
        d = self._make()
        phase_config = d.phase_configs["code-review"]

        with patch("lib.guardrail_runner.run_hooks", return_value=MagicMock()):
            d._handle_custom_phase_completion(
                "q-test", "code-review", phase_config, 0, self.spec_path
            )

        d.db.complete.assert_called_once()
        d.db.fail.assert_not_called()

    def test_reject_signal_requeues_to_execute(self):
        """code-review rejection (on_reject=requeue:execute) sends spec back to execute."""
        with open(self.spec_path, "w") as f:
            f.write(
                SYNTHETIC_SPEC_DONE
                + "\n### [CODE-REVIEW] Critical: shell injection in run.py:15\n"
            )
        d = self._make()
        phase_config = d.phase_configs["code-review"]

        with patch("lib.guardrail_runner.run_hooks", return_value=MagicMock()):
            d._handle_custom_phase_completion(
                "q-test", "code-review", phase_config, 0, self.spec_path
            )

        d.db.requeue.assert_called_once()
        d.db.update_spec_fields.assert_any_call("q-test", phase="execute")
        d.db.complete.assert_not_called()
        d.db.fail.assert_not_called()


# ---------------------------------------------------------------------------
# 6. Full pipeline sequence: all 4 phases in order
# ---------------------------------------------------------------------------

class TestFullPipelineSequence(unittest.TestCase):
    """End-to-end pipeline test: all 4 phases run and route correctly in sequence."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.state_dir = self._tmp.name
        guardrails = os.path.join(self.state_dir, "guardrails.toml")
        with open(guardrails, "w") as f:
            f.write(
                '[pipeline]\n'
                'default = ["plan-critique", "execute", "task-verify", "code-review"]\n'
            )
        self.spec_path = os.path.join(self.state_dir, "spec.md")

    def tearDown(self):
        self._tmp.cleanup()

    def test_full_pipeline_phase_transitions(self):
        """All 4 phases run in order and route to the correct next phase."""
        phases_observed: list[str] = []

        db = MagicMock()
        db.get_spec.return_value = {"tasks_done": 2, "tasks_total": 2}

        # Track every phase= kwarg passed to update_spec_fields
        def track_update(spec_id: str, **kwargs: object) -> None:
            if "phase" in kwargs:
                phases_observed.append(str(kwargs["phase"]))

        db.update_spec_fields.side_effect = track_update

        d = _make_daemon(self.state_dir, db)

        # ---- Phase 1: plan-critique approves --------------------------------
        with open(self.spec_path, "w") as f:
            f.write(SPEC_AFTER_PLAN_CRITIQUE)

        plan_critique_cfg = d.phase_configs.get("plan-critique")
        self.assertIsNotNone(plan_critique_cfg, "plan-critique phase config missing")

        with patch("lib.guardrail_runner.run_hooks", return_value=MagicMock()):
            d._handle_custom_phase_completion(
                "q-seq", "plan-critique", plan_critique_cfg, 0, self.spec_path
            )

        # pipeline must have advanced to execute
        self.assertIn("execute", phases_observed, "plan-critique must route to execute")

        # ---- Phase 2: execute completes (advance via _advance_pipeline) -----
        db.requeue.reset_mock()
        db.update_spec_fields.side_effect = track_update  # keep tracking

        d._advance_pipeline("q-seq", "execute")

        self.assertIn("task-verify", phases_observed, "execute must route to task-verify")

        # ---- Phase 3: task-verify completes ---------------------------------
        db.requeue.reset_mock()

        d._advance_pipeline("q-seq", "task-verify")

        self.assertIn("code-review", phases_observed, "task-verify must route to code-review")

        # ---- Phase 4: code-review approves -> pipeline complete -------------
        with open(self.spec_path, "w") as f:
            f.write(SPEC_AFTER_CODE_REVIEW)

        code_review_cfg = d.phase_configs.get("code-review")
        self.assertIsNotNone(code_review_cfg, "code-review phase config missing")

        # Verify code-review triggers for >50 lines (the synthetic spec would
        # produce >50 lines of code changes if executed)
        self.assertTrue(
            should_trigger(code_review_cfg, lines_changed=51),
            "code-review must trigger when lines_changed > 50",
        )

        with patch("lib.guardrail_runner.run_hooks", return_value=MagicMock()):
            d._handle_custom_phase_completion(
                "q-seq", "code-review", code_review_cfg, 0, self.spec_path
            )

        db.complete.assert_called_once()
        db.fail.assert_not_called()

        # Verify overall phase transition order
        self.assertEqual(
            phases_observed[:3],
            ["execute", "task-verify", "code-review"],
            f"Pipeline must advance execute -> task-verify -> code-review, got: {phases_observed}",
        )


if __name__ == "__main__":
    unittest.main()
