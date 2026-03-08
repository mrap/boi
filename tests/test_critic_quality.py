# test_critic_quality.py — Tests for quality scoring integration in the critic.

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

# Ensure boi lib is importable
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.critic import (
    _build_mode_awareness_section,
    _build_quality_scoring_section,
    _extract_quality_json,
    compute_quality_gate,
    generate_auto_reject_task,
    get_next_task_id,
    parse_critic_result,
    QUALITY_AUTO_REJECT_THRESHOLD,
    QUALITY_FAST_APPROVE_THRESHOLD,
    validate_mode_compliance,
    write_quality_to_telemetry,
)
from lib.spec_parser import BoiTask


# ---------------------------------------------------------------------------
# compute_quality_gate
# ---------------------------------------------------------------------------


class TestComputeQualityGate(unittest.TestCase):
    def test_none_returns_unknown(self):
        assert compute_quality_gate(None) == "unknown"

    def test_high_score_fast_approve(self):
        assert compute_quality_gate(0.90) == "fast_approve"
        assert compute_quality_gate(0.85) == "fast_approve"
        assert compute_quality_gate(1.0) == "fast_approve"

    def test_mid_score_standard(self):
        assert compute_quality_gate(0.84) == "standard"
        assert compute_quality_gate(0.70) == "standard"
        assert compute_quality_gate(0.50) == "standard"

    def test_low_score_auto_reject(self):
        assert compute_quality_gate(0.49) == "auto_reject"
        assert compute_quality_gate(0.0) == "auto_reject"
        assert compute_quality_gate(0.30) == "auto_reject"

    def test_boundary_values(self):
        assert compute_quality_gate(QUALITY_FAST_APPROVE_THRESHOLD) == "fast_approve"
        assert compute_quality_gate(QUALITY_FAST_APPROVE_THRESHOLD - 0.01) == "standard"
        assert compute_quality_gate(QUALITY_AUTO_REJECT_THRESHOLD) == "standard"
        assert (
            compute_quality_gate(QUALITY_AUTO_REJECT_THRESHOLD - 0.01) == "auto_reject"
        )


# ---------------------------------------------------------------------------
# validate_mode_compliance
# ---------------------------------------------------------------------------


class TestValidateModeCompliance(unittest.TestCase):
    def test_execute_no_violations(self):
        violations = validate_mode_compliance(
            "execute", {"t-1", "t-2"}, {"t-1", "t-2"}, []
        )
        assert violations == []

    def test_execute_tasks_added(self):
        violations = validate_mode_compliance(
            "execute", {"t-1"}, {"t-1", "t-2", "t-3"}, []
        )
        assert len(violations) == 1
        assert violations[0]["type"] == "mode_violation"
        assert "Execute mode" in violations[0]["message"]

    def test_challenge_no_violations(self):
        violations = validate_mode_compliance(
            "challenge", {"t-1", "t-2"}, {"t-1", "t-2"}, []
        )
        assert violations == []

    def test_challenge_tasks_added(self):
        violations = validate_mode_compliance("challenge", {"t-1"}, {"t-1", "t-2"}, [])
        assert len(violations) == 1
        assert "Challenge mode" in violations[0]["message"]

    def test_discover_proper_tasks(self):
        task = BoiTask(
            id="t-2",
            title="New task",
            status="PENDING",
            body="**Spec:** Do something\n\n**Verify:** Check it",
        )
        violations = validate_mode_compliance(
            "discover", {"t-1"}, {"t-1", "t-2"}, [task]
        )
        assert violations == []

    def test_discover_missing_spec(self):
        task = BoiTask(
            id="t-2",
            title="New task",
            status="PENDING",
            body="**Verify:** Check it",
        )
        violations = validate_mode_compliance(
            "discover", {"t-1"}, {"t-1", "t-2"}, [task]
        )
        assert len(violations) == 1
        assert "Spec" in violations[0]["message"]

    def test_discover_missing_verify(self):
        task = BoiTask(
            id="t-2",
            title="New task",
            status="PENDING",
            body="**Spec:** Do something",
        )
        violations = validate_mode_compliance(
            "discover", {"t-1"}, {"t-1", "t-2"}, [task]
        )
        assert len(violations) == 1
        assert "Verify" in violations[0]["message"]

    def test_generate_superseded_with_ref(self):
        task = BoiTask(
            id="t-1",
            title="Old task",
            status="SUPERSEDED",
            superseded_by="t-5",
        )
        violations = validate_mode_compliance("generate", {"t-1"}, {"t-1"}, [task])
        assert violations == []

    def test_generate_superseded_without_ref(self):
        task = BoiTask(
            id="t-1",
            title="Old task",
            status="SUPERSEDED",
            superseded_by="",
        )
        violations = validate_mode_compliance("generate", {"t-1"}, {"t-1"}, [task])
        assert len(violations) == 1
        assert "SUPERSEDED" in violations[0]["message"]

    def test_generate_too_many_tasks(self):
        post_ids = {"t-1", "t-2", "t-3", "t-4", "t-5", "t-6", "t-7"}
        violations = validate_mode_compliance("generate", {"t-1"}, post_ids, [])
        assert len(violations) == 1
        assert "max 5" in violations[0]["message"]

    def test_generate_within_limit(self):
        post_ids = {"t-1", "t-2", "t-3", "t-4", "t-5", "t-6"}
        violations = validate_mode_compliance("generate", {"t-1"}, post_ids, [])
        assert violations == []


# ---------------------------------------------------------------------------
# _extract_quality_json
# ---------------------------------------------------------------------------


class TestExtractQualityJson(unittest.TestCase):
    def test_extracts_from_code_block(self):
        content = """Some text

```json
{"overall_quality_score": 0.85, "categories": {"code_quality": {"score": 0.9}}}
```

More text
"""
        result = _extract_quality_json(content)
        assert result is not None
        assert result["overall_quality_score"] == 0.85

    def test_returns_none_for_no_json(self):
        content = "Just plain text with no JSON"
        assert _extract_quality_json(content) is None

    def test_ignores_non_quality_json(self):
        content = """```json
{"approved": true, "issues": []}
```"""
        assert _extract_quality_json(content) is None

    def test_handles_malformed_json(self):
        content = """```json
{"overall_quality_score": invalid}
```"""
        assert _extract_quality_json(content) is None


# ---------------------------------------------------------------------------
# parse_critic_result with quality data
# ---------------------------------------------------------------------------


class TestParseCriticResultQuality(unittest.TestCase):
    def test_extracts_quality_score(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
            f.write("""## Tasks

### t-1: Task one
DONE

```json
{"overall_quality_score": 0.82, "categories": {"code_quality": {"score": 0.90}, "test_quality": {"score": 0.75}}}
```

## Critic Approved
""")
            f.flush()
            result = parse_critic_result(f.name)
            os.unlink(f.name)

        assert result["approved"] is True
        assert result["quality_score"] == 0.82
        assert result["quality_gate"] == "standard"
        assert result["quality_signals"]["code_quality"] == 0.90
        assert result["quality_signals"]["test_quality"] == 0.75

    def test_no_quality_score(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
            f.write("## Critic Approved\n")
            f.flush()
            result = parse_critic_result(f.name)
            os.unlink(f.name)

        assert result["approved"] is True
        assert result["quality_score"] is None
        assert result["quality_gate"] == "unknown"
        assert result["quality_signals"] is None

    def test_low_quality_auto_reject(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
            f.write("""```json
{"overall_quality_score": 0.35, "categories": {"code_quality": {"score": 0.30}}}
```

### t-99: [CRITIC] Quality score below threshold
PENDING

**Spec:** Fix quality

**Verify:** Check
""")
            f.flush()
            result = parse_critic_result(f.name)
            os.unlink(f.name)

        assert result["approved"] is False
        assert result["quality_score"] == 0.35
        assert result["quality_gate"] == "auto_reject"
        assert result["critic_tasks_added"] == 1


# ---------------------------------------------------------------------------
# generate_auto_reject_task
# ---------------------------------------------------------------------------


class TestGenerateAutoRejectTask(unittest.TestCase):
    def test_generates_valid_task(self):
        task_text = generate_auto_reject_task(0.35, 10)
        assert "### t-10:" in task_text
        assert "[CRITIC]" in task_text
        assert "PENDING" in task_text
        assert "0.35" in task_text
        assert "**Spec:**" in task_text
        assert "**Verify:**" in task_text


# ---------------------------------------------------------------------------
# get_next_task_id
# ---------------------------------------------------------------------------


class TestGetNextTaskId(unittest.TestCase):
    def test_empty_spec(self):
        assert get_next_task_id("No tasks here") == 1

    def test_with_tasks(self):
        content = (
            "### t-1: First\nDONE\n### t-5: Fifth\nPENDING\n### t-3: Third\nDONE\n"
        )
        assert get_next_task_id(content) == 6


# ---------------------------------------------------------------------------
# _build_quality_scoring_section
# ---------------------------------------------------------------------------


class TestBuildQualityScoringSection(unittest.TestCase):
    def test_with_valid_dir(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            checks_dir = os.path.join(tmpdir, "templates", "checks")
            os.makedirs(checks_dir)
            Path(os.path.join(checks_dir, "quality-scoring.md")).write_text(
                "# Quality Scoring\nTest content\n"
            )
            result = _build_quality_scoring_section(tmpdir)
            assert "Quality Scoring" in result
            assert "Score >= 0.85" in result
            assert "Score < 0.50" in result

    def test_with_missing_file(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            result = _build_quality_scoring_section(tmpdir)
            assert result == ""


# ---------------------------------------------------------------------------
# _build_mode_awareness_section
# ---------------------------------------------------------------------------


class TestBuildModeAwarenessSection(unittest.TestCase):
    def test_execute_mode(self):
        result = _build_mode_awareness_section("execute")
        assert "execute" in result.lower()
        assert "should NOT have added" in result

    def test_challenge_mode(self):
        result = _build_mode_awareness_section("challenge")
        assert "challenge" in result.lower()
        assert "Challenges" in result

    def test_discover_mode(self):
        result = _build_mode_awareness_section("discover")
        assert "discover" in result.lower()
        assert "Spec" in result
        assert "Verify" in result

    def test_generate_mode(self):
        result = _build_mode_awareness_section("generate")
        assert "generate" in result.lower()
        assert "SUPERSEDED" in result


# ---------------------------------------------------------------------------
# write_quality_to_telemetry
# ---------------------------------------------------------------------------


class TestWriteQualityToTelemetry(unittest.TestCase):
    def test_writes_quality_fields(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            # Create an iteration file
            iter_file = Path(tmpdir) / "q-001.iteration-1.json"
            iter_file.write_text(
                json.dumps({"iteration": 1, "tasks_completed": 1}) + "\n"
            )

            write_quality_to_telemetry(
                tmpdir,
                "q-001",
                quality_score=0.82,
                quality_signals={"code_quality": 0.90, "test_quality": 0.75},
                quality_gate="standard",
            )

            data = json.loads(iter_file.read_text())
            assert data["quality_score"] == 0.82
            assert data["quality_signals"]["code_quality"] == 0.90
            assert data["quality_gate"] == "standard"
            assert data["quality_grade"] == "B"

    def test_handles_none_score(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            iter_file = Path(tmpdir) / "q-001.iteration-1.json"
            iter_file.write_text(json.dumps({"iteration": 1}) + "\n")

            write_quality_to_telemetry(tmpdir, "q-001", None, None, "unknown")

            data = json.loads(iter_file.read_text())
            assert data["quality_score"] is None
            assert data["quality_grade"] is None

    def test_handles_missing_dir(self):
        # Should not raise
        write_quality_to_telemetry("/nonexistent/path", "q-001", 0.8, None, "standard")


# ---------------------------------------------------------------------------
# Run tests
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    import unittest

    # Collect all test classes
    loader = unittest.TestLoader()
    suite = unittest.TestSuite()

    for cls in [
        TestComputeQualityGate,
        TestValidateModeCompliance,
        TestExtractQualityJson,
        TestParseCriticResultQuality,
        TestGenerateAutoRejectTask,
        TestGetNextTaskId,
        TestBuildQualityScoringSection,
        TestBuildModeAwarenessSection,
        TestWriteQualityToTelemetry,
    ]:
        suite.addTests(loader.loadTestsFromTestCase(cls))

    runner = unittest.TextTestRunner(verbosity=2)
    result = runner.run(suite)
    sys.exit(0 if result.wasSuccessful() else 1)
