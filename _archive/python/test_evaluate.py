# test_evaluate.py — Tests for the evaluate phase (lib/evaluate.py).

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

# Add parent dir to path for imports
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from lib.evaluate import (
    build_completion_summary,
    check_convergence,
    CompletionSummary,
    ConvergenceResult,
    count_criteria_met,
    evaluate_criteria,
    EvaluationResult,
    get_criteria_history,
    is_generate_spec,
    parse_success_criteria,
    write_completion_summary_to_spec,
)


class TestParseSuccessCriteria(unittest.TestCase):
    def test_basic_criteria(self):
        content = """# [Generate] Test Spec

## Goal
Build something.

## Success Criteria
- [ ] Feature A works
- [x] Feature B works
- [ ] Feature C works
"""
        criteria = parse_success_criteria(content)
        self.assertEqual(len(criteria), 3)
        self.assertFalse(criteria[0]["checked"])
        self.assertTrue(criteria[1]["checked"])
        self.assertFalse(criteria[2]["checked"])
        self.assertEqual(criteria[0]["text"], "Feature A works")

    def test_no_criteria_section(self):
        content = "# Some Spec\n\n## Goal\nDo stuff.\n"
        criteria = parse_success_criteria(content)
        self.assertEqual(len(criteria), 0)

    def test_empty_criteria_section(self):
        content = """## Success Criteria

## Next Section
"""
        criteria = parse_success_criteria(content)
        self.assertEqual(len(criteria), 0)

    def test_all_checked(self):
        content = """## Success Criteria
- [x] A
- [X] B
- [x] C
"""
        criteria = parse_success_criteria(content)
        self.assertEqual(len(criteria), 3)
        self.assertTrue(all(c["checked"] for c in criteria))

    def test_mixed_content(self):
        content = """## Success Criteria
Some intro text.

- [ ] First criterion
- [x] Second criterion

Some more text.
- [ ] Third criterion

## Other Section
"""
        criteria = parse_success_criteria(content)
        self.assertEqual(len(criteria), 3)


class TestCountCriteriaMet(unittest.TestCase):
    def test_basic(self):
        content = """## Success Criteria
- [x] Done
- [ ] Not done
- [x] Also done
"""
        met, total = count_criteria_met(content)
        self.assertEqual(met, 2)
        self.assertEqual(total, 3)


class TestEvaluateCriteria(unittest.TestCase):
    def test_all_met(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
            f.write("""## Success Criteria
- [x] A
- [x] B
""")
            f.flush()
            result = evaluate_criteria(f.name)
            self.assertTrue(result.all_met)
            self.assertEqual(result.status, "goal_achieved")
            self.assertEqual(result.criteria_met, 2)
            self.assertEqual(result.criteria_total, 2)
            os.unlink(f.name)

    def test_partial(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
            f.write("""## Success Criteria
- [x] A
- [ ] B
- [ ] C
- [x] D
""")
            f.flush()
            result = evaluate_criteria(f.name)
            self.assertFalse(result.all_met)
            self.assertEqual(result.status, "needs_work")
            self.assertEqual(result.criteria_met, 2)
            self.assertEqual(result.criteria_unmet, 2)
            self.assertEqual(result.unmet_criteria, ["B", "C"])
            os.unlink(f.name)

    def test_nonexistent_file(self):
        result = evaluate_criteria("/tmp/nonexistent-spec.md")
        self.assertEqual(result.criteria_total, 0)

    def test_no_criteria(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
            f.write("# Spec\n\n## Goal\nDo stuff.\n")
            f.flush()
            result = evaluate_criteria(f.name)
            self.assertTrue(result.all_met)
            self.assertEqual(result.status, "goal_achieved")
            os.unlink(f.name)


class TestCheckConvergence(unittest.TestCase):
    def _make_spec(self, criteria_text):
        f = tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False)
        f.write(criteria_text)
        f.flush()
        f.close()
        return f.name

    def test_all_criteria_met(self):
        spec = self._make_spec("""## Success Criteria
- [x] A
- [x] B
""")
        entry = {"iteration": 5, "max_iterations": 50}
        result = check_convergence(entry, spec)
        self.assertTrue(result.should_stop)
        self.assertEqual(result.reason, "goal_achieved")
        os.unlink(spec)

    def test_max_iterations(self):
        spec = self._make_spec("""## Success Criteria
- [x] A
- [ ] B
""")
        entry = {"iteration": 50, "max_iterations": 50}
        result = check_convergence(entry, spec)
        self.assertTrue(result.should_stop)
        self.assertEqual(result.reason, "max_iterations")
        os.unlink(spec)

    def test_stalled(self):
        spec = self._make_spec("""## Success Criteria
- [x] A
- [ ] B
- [ ] C
""")
        entry = {"iteration": 10, "max_iterations": 50}
        # 5 iterations with no change in criteria met
        history = [1, 1, 1, 1, 1]
        result = check_convergence(entry, spec, history)
        self.assertTrue(result.should_stop)
        self.assertEqual(result.reason, "stalled")
        os.unlink(spec)

    def test_diminishing_returns(self):
        spec = self._make_spec("""## Success Criteria
- [x] A
- [x] B
- [x] C
- [x] D
- [ ] E
""")
        entry = {"iteration": 10, "max_iterations": 50}
        # Last 3 iterations: 4, 4, 4 (no improvement, but 80% met)
        history = [2, 3, 4, 4, 4]
        result = check_convergence(entry, spec, history)
        self.assertTrue(result.should_stop)
        self.assertEqual(result.reason, "good_enough")
        os.unlink(spec)

    def test_not_converged(self):
        spec = self._make_spec("""## Success Criteria
- [x] A
- [ ] B
- [ ] C
""")
        entry = {"iteration": 3, "max_iterations": 50}
        history = [0, 1]
        result = check_convergence(entry, spec, history)
        self.assertFalse(result.should_stop)
        os.unlink(spec)

    def test_not_stalled_with_progress(self):
        spec = self._make_spec("""## Success Criteria
- [x] A
- [ ] B
""")
        entry = {"iteration": 10, "max_iterations": 50}
        # Progress in last 5: changing values
        history = [0, 0, 0, 1, 1]
        result = check_convergence(entry, spec, history)
        # Not stalled because values aren't all the same
        self.assertFalse(result.should_stop)
        os.unlink(spec)


class TestBuildCompletionSummary(unittest.TestCase):
    def test_goal_achieved(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
            f.write("""## Success Criteria
- [x] Feature A
- [x] Feature B
""")
            f.flush()
            entry = {"iteration": 5, "max_iterations": 50}
            summary = build_completion_summary("goal_achieved", entry, f.name)
            self.assertEqual(summary.status, "goal_achieved")
            self.assertEqual(summary.criteria_met, 2)
            self.assertEqual(summary.criteria_total, 2)
            self.assertEqual(summary.iterations_used, 5)
            self.assertEqual(len(summary.unmet_criteria), 0)
            os.unlink(f.name)

    def test_with_unmet(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
            f.write("""## Success Criteria
- [x] Feature A
- [ ] Feature B
""")
            f.flush()
            entry = {"iteration": 50, "max_iterations": 50}
            summary = build_completion_summary("max_iterations", entry, f.name)
            self.assertEqual(summary.status, "max_iterations")
            self.assertEqual(summary.criteria_met, 1)
            self.assertEqual(summary.unmet_criteria, ["Feature B"])
            self.assertEqual(len(summary.follow_ups), 1)
            os.unlink(f.name)


class TestWriteCompletionSummary(unittest.TestCase):
    def test_writes_summary(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
            f.write("""# [Generate] Test

## Success Criteria
- [x] A
- [x] B

### t-1: Do A
DONE
""")
            f.flush()
            summary = CompletionSummary(
                status="goal_achieved",
                iterations_used=5,
                max_iterations=50,
                time_elapsed_seconds=120.0,
                criteria_met=2,
                criteria_total=2,
            )
            write_completion_summary_to_spec(f.name, summary)

            content = Path(f.name).read_text()
            self.assertIn("## Completion Summary", content)
            self.assertIn("goal_achieved", content)
            self.assertIn("5 / 50", content)
            self.assertIn("2 / 2", content)
            os.unlink(f.name)

    def test_replaces_existing_summary(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
            f.write("""# Spec

## Completion Summary

**Status:** needs_work
""")
            f.flush()
            summary = CompletionSummary(
                status="goal_achieved",
                iterations_used=10,
                max_iterations=50,
                time_elapsed_seconds=300.0,
                criteria_met=4,
                criteria_total=4,
            )
            write_completion_summary_to_spec(f.name, summary)

            content = Path(f.name).read_text()
            # Should have exactly one Completion Summary
            self.assertEqual(content.count("## Completion Summary"), 1)
            self.assertIn("goal_achieved", content)
            self.assertNotIn("needs_work", content)
            os.unlink(f.name)


class TestGetCriteriaHistory(unittest.TestCase):
    def test_loads_history(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            for i in range(1, 4):
                data = {"criteria_met": i, "post_counts": {"done": i}}
                path = Path(tmpdir) / f"q-001.iteration-{i}.json"
                path.write_text(json.dumps(data))

            history = get_criteria_history(tmpdir, "q-001")
            self.assertEqual(history, [1, 2, 3])

    def test_empty_history(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            history = get_criteria_history(tmpdir, "q-001")
            self.assertEqual(history, [])

    def test_fallback_to_post_counts(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            data = {"post_counts": {"done": 5}}
            path = Path(tmpdir) / "q-001.iteration-1.json"
            path.write_text(json.dumps(data))

            history = get_criteria_history(tmpdir, "q-001")
            self.assertEqual(history, [5])


class TestIsGenerateSpec(unittest.TestCase):
    def test_mode_in_entry(self):
        self.assertTrue(is_generate_spec({"mode": "generate"}))
        self.assertFalse(is_generate_spec({"mode": "execute"}))
        self.assertFalse(is_generate_spec({}))

    def test_generate_title_in_spec(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
            f.write("# [Generate] Build a CLI tool\n\n## Goal\nBuild it.\n")
            f.flush()
            self.assertTrue(is_generate_spec({"mode": "execute", "spec_path": f.name}))
            os.unlink(f.name)

    def test_mode_header_in_spec(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
            f.write("# Some Spec\n\n**Mode:** generate\n\n## Goal\nBuild it.\n")
            f.flush()
            self.assertTrue(is_generate_spec({"mode": "", "spec_path": f.name}))
            os.unlink(f.name)


if __name__ == "__main__":
    unittest.main()
