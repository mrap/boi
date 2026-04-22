"""Tests for the code-review phase.

Covers:
  TestCodeReviewPhaseRegistration  -- phase file loads and fields are correct
  TestCodeReviewPersonaFiles       -- all 4 persona guide files exist and have content
  TestCodeReviewTriggerThreshold   -- trigger logic only fires when lines_changed > 50
  TestCodeReviewSignalParsing      -- approve/reject signal detection logic
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.phases import discover_phases, load_phase, should_trigger, validate_phase

REPO_ROOT = Path(__file__).resolve().parent.parent
PHASES_DIR = REPO_ROOT / "phases"
TEMPLATES_DIR = REPO_ROOT / "templates"
PHASE_FILE = PHASES_DIR / "code-review.phase.toml"
PROMPT_FILE = TEMPLATES_DIR / "code-review-prompt.md"
PERSONAS_DIR = TEMPLATES_DIR / "code-review-personas"

PERSONA_FILES = [
    "code-quality.md",
    "data-testing.md",
    "security-privacy.md",
    "architecture-migration.md",
]


# ---------------------------------------------------------------------------
# 1. Phase registration
# ---------------------------------------------------------------------------

class TestCodeReviewPhaseRegistration(unittest.TestCase):

    def test_phase_file_exists(self):
        self.assertTrue(PHASE_FILE.exists(), f"Missing phase file: {PHASE_FILE}")

    def test_phase_loads_without_error(self):
        config = load_phase(str(PHASE_FILE))
        self.assertIsNotNone(config)

    def test_phase_name(self):
        config = load_phase(str(PHASE_FILE))
        self.assertEqual(config.name, "code-review")

    def test_phase_prompt_template(self):
        config = load_phase(str(PHASE_FILE))
        self.assertEqual(config.prompt_template, "templates/code-review-prompt.md")

    def test_phase_approve_signal(self):
        config = load_phase(str(PHASE_FILE))
        self.assertEqual(config.approve_signal, "## Code Review Approved")

    def test_phase_reject_signal(self):
        config = load_phase(str(PHASE_FILE))
        self.assertEqual(config.reject_signal, "[CODE-REVIEW]")

    def test_phase_on_approve(self):
        config = load_phase(str(PHASE_FILE))
        self.assertEqual(config.on_approve, "next")

    def test_phase_on_reject(self):
        config = load_phase(str(PHASE_FILE))
        self.assertEqual(config.on_reject, "requeue:execute")

    def test_phase_passes_validation(self):
        config = load_phase(str(PHASE_FILE))
        errors = validate_phase(config)
        self.assertEqual(errors, [], f"Validation errors: {errors}")

    def test_phase_discoverable(self):
        phases = discover_phases(str(PHASES_DIR))
        self.assertIn("code-review", phases)

    def test_discovered_phase_fields_match(self):
        phases = discover_phases(str(PHASES_DIR))
        config = phases["code-review"]
        self.assertEqual(config.approve_signal, "## Code Review Approved")
        self.assertEqual(config.reject_signal, "[CODE-REVIEW]")
        self.assertEqual(config.on_reject, "requeue:execute")

    def test_phase_has_trigger_threshold(self):
        config = load_phase(str(PHASE_FILE))
        self.assertEqual(config.trigger_min_lines_changed, 50)


# ---------------------------------------------------------------------------
# 2. Persona file loading
# ---------------------------------------------------------------------------

class TestCodeReviewPersonaFiles(unittest.TestCase):

    def test_personas_directory_exists(self):
        self.assertTrue(PERSONAS_DIR.exists(), f"Missing personas dir: {PERSONAS_DIR}")
        self.assertTrue(PERSONAS_DIR.is_dir())

    def test_all_persona_files_exist(self):
        for fname in PERSONA_FILES:
            fpath = PERSONAS_DIR / fname
            self.assertTrue(fpath.exists(), f"Missing persona file: {fpath}")

    def test_persona_files_have_content(self):
        for fname in PERSONA_FILES:
            fpath = PERSONAS_DIR / fname
            content = fpath.read_text()
            self.assertGreater(len(content), 50,
                               f"Persona file appears empty: {fpath}")

    def test_code_quality_covers_naming_and_structure(self):
        content = (PERSONAS_DIR / "code-quality.md").read_text().lower()
        self.assertTrue(
            "naming" in content or "structure" in content or "duplication" in content,
            "code-quality.md must cover naming, structure, or duplication"
        )

    def test_data_testing_covers_coverage_and_assertions(self):
        content = (PERSONAS_DIR / "data-testing.md").read_text().lower()
        self.assertTrue(
            "coverage" in content or "assertion" in content or "edge case" in content,
            "data-testing.md must cover test coverage, assertions, or edge cases"
        )

    def test_security_privacy_covers_injection(self):
        content = (PERSONAS_DIR / "security-privacy.md").read_text().lower()
        self.assertTrue(
            "injection" in content or "secret" in content or "path traversal" in content,
            "security-privacy.md must cover injection, secrets, or path traversal"
        )

    def test_architecture_migration_covers_caller_updates(self):
        content = (PERSONAS_DIR / "architecture-migration.md").read_text().lower()
        self.assertTrue(
            "caller" in content or "import" in content or "config" in content,
            "architecture-migration.md must cover caller updates, imports, or config renames"
        )

    def test_prompt_template_loads_persona_references(self):
        content = PROMPT_FILE.read_text().lower()
        for fname in PERSONA_FILES:
            stem = fname.replace(".md", "")
            self.assertTrue(
                stem in content,
                f"Prompt template does not reference persona '{stem}'"
            )


# ---------------------------------------------------------------------------
# 3. Trigger threshold logic
# ---------------------------------------------------------------------------

class TestCodeReviewTriggerThreshold(unittest.TestCase):

    def setUp(self):
        self.config = load_phase(str(PHASE_FILE))

    def test_trigger_fires_above_threshold(self):
        self.assertTrue(should_trigger(self.config, lines_changed=51))

    def test_trigger_fires_at_100_lines(self):
        self.assertTrue(should_trigger(self.config, lines_changed=100))

    def test_trigger_does_not_fire_at_threshold(self):
        # Exactly at threshold (50) should NOT fire -- must EXCEED it
        self.assertFalse(should_trigger(self.config, lines_changed=50))

    def test_trigger_does_not_fire_below_threshold(self):
        self.assertFalse(should_trigger(self.config, lines_changed=10))

    def test_trigger_does_not_fire_at_zero(self):
        self.assertFalse(should_trigger(self.config, lines_changed=0))

    def test_phase_with_zero_threshold_always_fires(self):
        from lib.phases import PhaseConfig
        no_threshold = PhaseConfig(
            name="test",
            prompt_template="templates/foo.md",
            approve_signal="## OK",
            trigger_min_lines_changed=0,
        )
        self.assertTrue(should_trigger(no_threshold, lines_changed=0))
        self.assertTrue(should_trigger(no_threshold, lines_changed=1))

    def test_phase_with_negative_threshold_always_fires(self):
        from lib.phases import PhaseConfig
        negative = PhaseConfig(
            name="test",
            prompt_template="templates/foo.md",
            approve_signal="## OK",
            trigger_min_lines_changed=-1,
        )
        self.assertTrue(should_trigger(negative, lines_changed=0))


# ---------------------------------------------------------------------------
# 4. Approve/reject signal parsing
# ---------------------------------------------------------------------------

def _contains_approve(text: str) -> bool:
    return "## Code Review Approved" in text


def _contains_reject(text: str) -> bool:
    return "[CODE-REVIEW]" in text


class TestCodeReviewSignalParsing(unittest.TestCase):

    def test_approve_signal_detected(self):
        output = "Analysis complete.\n\n## Code Review Approved\n\nNo issues found."
        self.assertTrue(_contains_approve(output))
        self.assertFalse(_contains_reject(output))

    def test_reject_signal_detected(self):
        output = (
            "Issues found:\n\n"
            "### [CODE-REVIEW] Critical: SQL injection in db.py:42\n"
            "**[data-testing]** Missing assertions for null input.\n"
        )
        self.assertFalse(_contains_approve(output))
        self.assertTrue(_contains_reject(output))

    def test_both_absent_means_no_signal(self):
        output = "Reviewing code..."
        self.assertFalse(_contains_approve(output))
        self.assertFalse(_contains_reject(output))

    def test_approve_signal_case_sensitive(self):
        output = "## code review approved\n"
        self.assertFalse(_contains_approve(output))

    def test_reject_signal_case_sensitive(self):
        output = "[code-review] fix something\n"
        self.assertFalse(_contains_reject(output))

    def test_approve_not_triggered_by_partial_match(self):
        output = "Code Review Approved but not a heading\n"
        self.assertFalse(_contains_approve(output))

    def test_reject_signal_in_finding_header(self):
        output = "### [CODE-REVIEW] Critical: shell injection in run.py:15\n"
        self.assertTrue(_contains_reject(output))

    def test_prompt_references_approve_signal(self):
        content = PROMPT_FILE.read_text()
        self.assertIn("## Code Review Approved", content,
                      "Prompt must show the approve signal")

    def test_prompt_references_reject_signal(self):
        content = PROMPT_FILE.read_text()
        self.assertIn("[CODE-REVIEW]", content,
                      "Prompt must show the reject signal")


if __name__ == "__main__":
    unittest.main()
