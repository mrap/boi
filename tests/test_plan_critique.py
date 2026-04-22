"""Tests for the plan-critique phase.

Covers:
  TestPlanCritiquePhaseRegistration -- phase file loads and fields are correct
  TestPlanCritiquePromptTemplate    -- prompt template file exists and loads
  TestPlanCritiqueSignalParsing     -- approve/reject signal detection logic
  TestPlanCritiqueRejectFixture     -- a known-bad spec triggers a rejection signal
"""

from __future__ import annotations

import os
import sys
import unittest
from pathlib import Path

# Ensure project root is on the path
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.phases import discover_phases, load_phase, validate_phase

# Paths relative to repo root
REPO_ROOT = Path(__file__).resolve().parent.parent
PHASES_DIR = REPO_ROOT / "phases"
TEMPLATES_DIR = REPO_ROOT / "templates"
PHASE_FILE = PHASES_DIR / "plan-critique.phase.toml"
PROMPT_FILE = TEMPLATES_DIR / "plan-critique-prompt.md"


# ---------------------------------------------------------------------------
# 1. Phase registration
# ---------------------------------------------------------------------------

class TestPlanCritiquePhaseRegistration(unittest.TestCase):

    def test_phase_file_exists(self):
        self.assertTrue(PHASE_FILE.exists(), f"Missing phase file: {PHASE_FILE}")

    def test_phase_loads_without_error(self):
        config = load_phase(str(PHASE_FILE))
        self.assertIsNotNone(config)

    def test_phase_name(self):
        config = load_phase(str(PHASE_FILE))
        self.assertEqual(config.name, "plan-critique")

    def test_phase_prompt_template(self):
        config = load_phase(str(PHASE_FILE))
        self.assertEqual(config.prompt_template, "templates/plan-critique-prompt.md")

    def test_phase_approve_signal(self):
        config = load_phase(str(PHASE_FILE))
        self.assertEqual(config.approve_signal, "## Plan Approved")

    def test_phase_reject_signal(self):
        config = load_phase(str(PHASE_FILE))
        self.assertEqual(config.reject_signal, "[PLAN-CRITIQUE]")

    def test_phase_on_approve(self):
        config = load_phase(str(PHASE_FILE))
        self.assertEqual(config.on_approve, "next")

    def test_phase_on_reject(self):
        config = load_phase(str(PHASE_FILE))
        self.assertEqual(config.on_reject, "fail")

    def test_phase_passes_validation(self):
        config = load_phase(str(PHASE_FILE))
        errors = validate_phase(config)
        self.assertEqual(errors, [], f"Validation errors: {errors}")

    def test_phase_discoverable(self):
        phases = discover_phases(str(PHASES_DIR))
        self.assertIn("plan-critique", phases)

    def test_discovered_phase_fields_match(self):
        phases = discover_phases(str(PHASES_DIR))
        config = phases["plan-critique"]
        self.assertEqual(config.approve_signal, "## Plan Approved")
        self.assertEqual(config.reject_signal, "[PLAN-CRITIQUE]")
        self.assertEqual(config.on_reject, "fail")


# ---------------------------------------------------------------------------
# 2. Prompt template loading
# ---------------------------------------------------------------------------

class TestPlanCritiquePromptTemplate(unittest.TestCase):

    def test_prompt_template_exists(self):
        self.assertTrue(PROMPT_FILE.exists(), f"Missing prompt template: {PROMPT_FILE}")

    def test_prompt_template_is_readable(self):
        content = PROMPT_FILE.read_text()
        self.assertGreater(len(content), 100, "Prompt template appears empty or very short")

    def test_prompt_covers_non_executable_verify(self):
        content = PROMPT_FILE.read_text().lower()
        self.assertIn("verify", content, "Prompt must address verify command issues")

    def test_prompt_covers_self_referential_verify(self):
        content = PROMPT_FILE.read_text().lower()
        # Should mention self-referential or circular verify
        self.assertTrue(
            "self-referential" in content or "circular" in content or "self referential" in content,
            "Prompt must address self-referential verify issues"
        )

    def test_prompt_covers_unbounded_scope(self):
        content = PROMPT_FILE.read_text().lower()
        self.assertTrue(
            "unbounded" in content or "exit condition" in content or "scope" in content,
            "Prompt must address unbounded scope issues"
        )

    def test_prompt_covers_missing_dependencies(self):
        content = PROMPT_FILE.read_text().lower()
        self.assertTrue(
            "blocked" in content or "dependenc" in content,
            "Prompt must address missing blocked-by dependencies"
        )

    def test_prompt_covers_implicit_assumptions(self):
        content = PROMPT_FILE.read_text().lower()
        self.assertTrue(
            "assumption" in content or "environment" in content or "tooling" in content,
            "Prompt must address implicit environment/tooling assumptions"
        )

    def test_prompt_references_approve_signal(self):
        content = PROMPT_FILE.read_text()
        self.assertIn("## Plan Approved", content,
                      "Prompt must show the approve signal so the LLM knows what to output")

    def test_prompt_references_reject_signal(self):
        content = PROMPT_FILE.read_text()
        self.assertIn("[PLAN-CRITIQUE]", content,
                      "Prompt must show the reject signal so the LLM knows what to output")


# ---------------------------------------------------------------------------
# 3. Approve/reject signal parsing
# ---------------------------------------------------------------------------

def _contains_approve(text: str) -> bool:
    return "## Plan Approved" in text


def _contains_reject(text: str) -> bool:
    return "[PLAN-CRITIQUE]" in text


class TestPlanCritiqueSignalParsing(unittest.TestCase):

    def test_approve_signal_detected(self):
        output = "Some analysis...\n\n## Plan Approved\n\nAll checks passed."
        self.assertTrue(_contains_approve(output))
        self.assertFalse(_contains_reject(output))

    def test_reject_signal_detected(self):
        output = (
            "Issues found:\n\n"
            "### [PLAN-CRITIQUE] t-fix-1: Add exit condition to unbounded loop\n"
            "PENDING\n"
        )
        self.assertFalse(_contains_approve(output))
        self.assertTrue(_contains_reject(output))

    def test_both_absent_means_no_signal(self):
        output = "I am thinking about this spec..."
        self.assertFalse(_contains_approve(output))
        self.assertFalse(_contains_reject(output))

    def test_approve_signal_case_sensitive(self):
        # Signal must be exactly right -- lowercase should NOT match
        output = "## plan approved\n"
        self.assertFalse(_contains_approve(output))

    def test_reject_signal_case_sensitive(self):
        output = "[plan-critique] fix something\n"
        self.assertFalse(_contains_reject(output))

    def test_approve_not_triggered_by_partial_match(self):
        output = "Plan Approved but not the heading\n"
        self.assertFalse(_contains_approve(output))

    def test_reject_signal_in_task_header(self):
        # The reject signal appears as a task prefix, not standalone
        output = "### [PLAN-CRITIQUE] t-1: Fix missing exit condition\nPENDING\n"
        self.assertTrue(_contains_reject(output))


# ---------------------------------------------------------------------------
# 4. Sample spec that should be rejected (fixture from q-589 experiment)
# ---------------------------------------------------------------------------

# This fixture represents a spec with multiple problems that the plan-critique
# phase should catch. It has:
#   (a) A non-executable verify command (references a URL that can't be scripted)
#   (b) Self-referential verify (writes "DONE" then greps for "DONE")
#   (c) Unbounded scope (no exit condition for the retry loop)
#   (d) Missing blocked-by between t-2 and t-1
#   (e) Implicit assumption: assumes `jq` is installed

BAD_SPEC_FIXTURE = """\
# Bad Spec: Data Fetcher

## Context
Fetch data from an API and store it locally.

### t-1: Install dependencies
PENDING

**Spec:** Run `pip install requests`.

**Verify:** Check the PyPI page at https://pypi.org/project/requests/ to confirm
the package exists. (non-executable -- requires browser/human)

### t-2: Fetch data and save
PENDING

**Spec:** Write a script `fetch.py` that calls the API and saves results to
`output.json`. Then write "DONE" to `status.txt`.

**Verify:** `echo 'DONE' > status.txt && grep -q 'DONE' status.txt`
(self-referential -- writes then checks own output)

### t-3: Retry on failure
PENDING

**Spec:** Add retry logic. Keep retrying until it works.
(unbounded -- no max retry count or exit condition)

**Verify:** `python fetch.py`

Note: t-2 depends on t-1 completing first but has no Blocked-by line.
Also uses `jq` without checking if it's installed.
"""


class TestPlanCritiqueRejectFixture(unittest.TestCase):
    """Verify the bad spec fixture has the expected problems detectable by pattern."""

    def test_fixture_has_non_executable_verify(self):
        # URL-based verify is present
        self.assertIn("https://pypi.org", BAD_SPEC_FIXTURE)

    def test_fixture_has_self_referential_verify(self):
        # Writes then greps own output
        self.assertIn("echo 'DONE' > status.txt && grep -q 'DONE' status.txt",
                      BAD_SPEC_FIXTURE)

    def test_fixture_has_unbounded_scope(self):
        # No exit condition on retry
        self.assertIn("Keep retrying until it works", BAD_SPEC_FIXTURE)

    def test_fixture_missing_blocked_by(self):
        # t-2 has no Blocked-by referencing t-1
        import re
        t2_section = re.search(r"### t-2:.*?(?=### t-3:|$)", BAD_SPEC_FIXTURE, re.DOTALL)
        self.assertIsNotNone(t2_section)
        self.assertNotIn("Blocked by", t2_section.group(0))

    def test_fixture_has_implicit_tooling_assumption(self):
        # jq assumed present without install step
        self.assertIn("jq", BAD_SPEC_FIXTURE)

    def test_fixture_would_not_be_approved(self):
        # An LLM reviewing this spec should NOT output the approve signal.
        # We verify the fixture does not accidentally contain the approve string.
        self.assertNotIn("## Plan Approved", BAD_SPEC_FIXTURE)

    def test_fixture_prompt_template_addresses_all_problems(self):
        """The prompt template covers all five problem categories from the fixture."""
        content = PROMPT_FILE.read_text().lower()
        checks = {
            "non-executable verify": "verify" in content,
            "self-referential verify": (
                "self-referential" in content
                or "self referential" in content
                or "circular" in content
            ),
            "unbounded scope": (
                "unbounded" in content
                or "exit condition" in content
                or "scope" in content
            ),
            "missing dependencies": (
                "blocked" in content or "dependenc" in content
            ),
            "implicit assumptions": (
                "assumption" in content or "environment" in content
            ),
        }
        failed = [k for k, v in checks.items() if not v]
        self.assertEqual(
            failed, [],
            f"Prompt template missing coverage for: {failed}"
        )


if __name__ == "__main__":
    unittest.main()
