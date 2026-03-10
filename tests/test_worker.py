"""test_worker.py — Tests for BOI worker iteration logic.

Tests the worker's core functions:
- Task counting (spec parsing integration)
- Prompt generation from template
- Run script generation with correct metadata structure
- Iteration metadata JSON format

All tests use mock data and temp directories. No live Claude calls.
Uses unittest (stdlib only, no pytest dependency).
"""

import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

# Add the BOI root to path for lib imports
BOI_ROOT = str(Path(__file__).resolve().parent.parent)
sys.path.insert(0, BOI_ROOT)

from lib.spec_parser import count_boi_tasks, parse_boi_spec


# ── Test Data ────────────────────────────────────────────────────────────

SAMPLE_SPEC = textwrap.dedent("""\
    # Test Spec

    ## Tasks

    ### t-1: First task
    DONE

    **Spec:** Do the first thing.

    **Verify:** echo "done"

    ### t-2: Second task
    PENDING

    **Spec:** Do the second thing.

    **Verify:** echo "ok"

    ### t-3: Third task
    PENDING

    **Spec:** Do the third thing.

    **Verify:** echo "ok"

    ### t-4: Skipped task
    SKIPPED

    **Spec:** This was skipped.

    **Verify:** echo "n/a"
""")

SPECIAL_CHARS_SPEC = textwrap.dedent("""\
    # Spec with Special Characters

    This spec has {{curly braces}} and $dollar signs and ${variable_refs}.
    Also `{{ITERATION}}` and `{{SPEC_CONTENT}}` which look like template vars.

    ## Tasks

    ### t-1: Fix {{template}} injection
    PENDING

    **Spec:** Replace `${bash_substitution}` with Python. Handle `{{nested}}` braces.

    **Verify:** echo "$PATH" && echo "{{ok}}"
""")

ALL_DONE_SPEC = textwrap.dedent("""\
    # All Done Spec

    ## Tasks

    ### t-1: First task
    DONE

    **Spec:** Done.

    **Verify:** echo "done"

    ### t-2: Second task
    DONE

    **Spec:** Done.

    **Verify:** echo "done"
""")


# ── Task Counting Tests ──────────────────────────────────────────────────


class TestTaskCounting(unittest.TestCase):
    """Tests for the spec parsing that the worker relies on."""

    def setUp(self):
        self.tmp_dir = tempfile.mkdtemp(prefix="boi-test-")

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmp_dir, ignore_errors=True)

    def _write_spec(self, content, name="spec.md"):
        path = os.path.join(self.tmp_dir, name)
        with open(path, "w") as f:
            f.write(content)
        return path

    def test_count_mixed_tasks(self):
        path = self._write_spec(SAMPLE_SPEC)
        counts = count_boi_tasks(path)
        self.assertEqual(counts["pending"], 2)
        self.assertEqual(counts["done"], 1)
        self.assertEqual(counts["skipped"], 1)
        self.assertEqual(counts["total"], 4)

    def test_count_all_done(self):
        path = self._write_spec(ALL_DONE_SPEC)
        counts = count_boi_tasks(path)
        self.assertEqual(counts["pending"], 0)
        self.assertEqual(counts["done"], 2)
        self.assertEqual(counts["total"], 2)

    def test_count_nonexistent_file(self):
        path = os.path.join(self.tmp_dir, "nonexistent.md")
        counts = count_boi_tasks(path)
        self.assertEqual(counts["pending"], 0)
        self.assertEqual(counts["total"], 0)

    def test_count_empty_spec(self):
        path = self._write_spec("# Empty spec\n\nNo tasks here.\n", "empty.md")
        counts = count_boi_tasks(path)
        self.assertEqual(counts["total"], 0)

    def test_parse_extracts_task_ids(self):
        path = self._write_spec(SAMPLE_SPEC)
        with open(path) as f:
            content = f.read()
        tasks = parse_boi_spec(content)
        ids = [t.id for t in tasks]
        self.assertEqual(ids, ["t-1", "t-2", "t-3", "t-4"])

    def test_parse_extracts_statuses(self):
        path = self._write_spec(SAMPLE_SPEC)
        with open(path) as f:
            content = f.read()
        tasks = parse_boi_spec(content)
        statuses = [t.status for t in tasks]
        self.assertEqual(statuses, ["DONE", "PENDING", "PENDING", "SKIPPED"])

    def test_done_with_trailing_notes(self):
        """DONE lines can have trailing notes (e.g. 'DONE -- completed')."""
        path = self._write_spec(
            textwrap.dedent("""\
            ### t-1: Task with notes
            DONE -- completed with 5 tests

            **Spec:** Do stuff.
        """),
            "notes.md",
        )
        counts = count_boi_tasks(path)
        self.assertEqual(counts["done"], 1)
        self.assertEqual(counts["total"], 1)


# ── Worker Script Argument Validation ────────────────────────────────────


@unittest.skip("worker.sh archived; see test_worker_new.py for Python worker tests")
class TestWorkerArgValidation(unittest.TestCase):
    """Test that archived worker.sh validates arguments correctly."""

    def test_missing_args_exits_with_2(self):
        result = subprocess.run(
            ["bash", os.path.join(BOI_ROOT, "archive", "worker.sh")],
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("Missing required arguments", result.stderr)

    def test_nonexistent_spec_exits_with_2(self):
        with tempfile.TemporaryDirectory(prefix="boi-test-") as tmp_dir:
            result = subprocess.run(
                [
                    "bash",
                    os.path.join(BOI_ROOT, "archive", "worker.sh"),
                    "q-test",
                    tmp_dir,
                    os.path.join(tmp_dir, "nonexistent.md"),
                    "1",
                ],
                capture_output=True,
                text=True,
                timeout=10,
            )
            self.assertEqual(result.returncode, 2)
            self.assertIn("Spec file not found", result.stderr)

    def test_nonexistent_worktree_exits_with_2(self):
        with tempfile.TemporaryDirectory(prefix="boi-test-") as tmp_dir:
            spec_path = os.path.join(tmp_dir, "spec.md")
            with open(spec_path, "w") as f:
                f.write(SAMPLE_SPEC)
            result = subprocess.run(
                [
                    "bash",
                    os.path.join(BOI_ROOT, "archive", "worker.sh"),
                    "q-test",
                    "/nonexistent/worktree",
                    spec_path,
                    "1",
                ],
                capture_output=True,
                text=True,
                timeout=10,
            )
            self.assertEqual(result.returncode, 2)
            self.assertIn("Worktree path does not exist", result.stderr)


# ── Worker Exits on No Pending Tasks ─────────────────────────────────────


@unittest.skip("worker.sh archived; see test_worker_new.py for Python worker tests")
class TestWorkerNoPending(unittest.TestCase):
    """Test that worker exits cleanly when no PENDING tasks remain."""

    def test_all_done_exits_0(self):
        """Worker should exit with 0 when all tasks are DONE."""
        with tempfile.TemporaryDirectory(prefix="boi-test-") as tmp_dir:
            spec_path = os.path.join(tmp_dir, "spec.md")
            with open(spec_path, "w") as f:
                f.write(ALL_DONE_SPEC)

            os.makedirs(os.path.join(tmp_dir, ".boi", "queue"), exist_ok=True)
            os.makedirs(os.path.join(tmp_dir, ".boi", "logs"), exist_ok=True)

            result = subprocess.run(
                [
                    "bash",
                    os.path.join(BOI_ROOT, "archive", "worker.sh"),
                    "q-test",
                    tmp_dir,
                    spec_path,
                    "1",
                ],
                capture_output=True,
                text=True,
                timeout=10,
                env={**os.environ, "HOME": tmp_dir},
            )
            self.assertEqual(result.returncode, 0)
            self.assertIn("No PENDING tasks", result.stdout)

    def test_all_done_writes_exit_file(self):
        """Worker should write exit file with code 0 when no PENDING tasks."""
        with tempfile.TemporaryDirectory(prefix="boi-test-") as tmp_dir:
            spec_path = os.path.join(tmp_dir, "spec.md")
            with open(spec_path, "w") as f:
                f.write(ALL_DONE_SPEC)

            queue_dir = os.path.join(tmp_dir, ".boi", "queue")
            os.makedirs(queue_dir, exist_ok=True)
            os.makedirs(os.path.join(tmp_dir, ".boi", "logs"), exist_ok=True)

            result = subprocess.run(
                [
                    "bash",
                    os.path.join(BOI_ROOT, "archive", "worker.sh"),
                    "q-test",
                    tmp_dir,
                    spec_path,
                    "1",
                ],
                capture_output=True,
                text=True,
                timeout=10,
                env={**os.environ, "HOME": tmp_dir},
            )
            self.assertEqual(result.returncode, 0)

            # Verify exit file was written
            exit_file = os.path.join(queue_dir, "q-test.exit")
            self.assertTrue(
                os.path.isfile(exit_file),
                "Exit file should be written when 0 PENDING tasks",
            )
            with open(exit_file) as f:
                exit_code = f.read().strip()
            self.assertEqual(exit_code, "0")

    def test_all_done_does_not_write_pid_file(self):
        """Worker should NOT write a PID file when no PENDING tasks (no tmux)."""
        with tempfile.TemporaryDirectory(prefix="boi-test-") as tmp_dir:
            spec_path = os.path.join(tmp_dir, "spec.md")
            with open(spec_path, "w") as f:
                f.write(ALL_DONE_SPEC)

            queue_dir = os.path.join(tmp_dir, ".boi", "queue")
            os.makedirs(queue_dir, exist_ok=True)
            os.makedirs(os.path.join(tmp_dir, ".boi", "logs"), exist_ok=True)

            result = subprocess.run(
                [
                    "bash",
                    os.path.join(BOI_ROOT, "archive", "worker.sh"),
                    "q-test",
                    tmp_dir,
                    spec_path,
                    "1",
                ],
                capture_output=True,
                text=True,
                timeout=10,
                env={**os.environ, "HOME": tmp_dir},
            )
            self.assertEqual(result.returncode, 0)

            # No PID file should exist
            pid_file = os.path.join(queue_dir, "q-test.pid")
            self.assertFalse(
                os.path.isfile(pid_file),
                "PID file should NOT be written when 0 PENDING tasks",
            )


# ── Prompt Template Tests ────────────────────────────────────────────────


class TestPromptTemplate(unittest.TestCase):
    """Test that the worker prompt template has required content."""

    def setUp(self):
        self.template_path = os.path.join(BOI_ROOT, "templates", "worker-prompt.md")
        self.assertTrue(
            os.path.isfile(self.template_path),
            f"Template not found: {self.template_path}",
        )
        with open(self.template_path) as f:
            self.content = f.read()

    def test_has_required_placeholders(self):
        for placeholder in [
            "{{SPEC_CONTENT}}",
            "{{ITERATION}}",
            "{{QUEUE_ID}}",
            "{{SPEC_PATH}}",
            "{{PENDING_COUNT}}",
        ]:
            self.assertIn(
                placeholder, self.content, f"Missing placeholder: {placeholder}"
            )

    def test_has_self_evolution_instructions(self):
        lower = self.content.lower()
        self.assertTrue(
            "self-evolving" in lower or "{{MODE_RULES}}" in self.content,
            "Template should reference self-evolution or mode rules",
        )
        self.assertTrue(
            "add new pending tasks" in lower,
            "Template should instruct adding new PENDING tasks",
        )

    def test_has_fresh_context_note(self):
        lower = self.content.lower()
        self.assertTrue(
            "no memory" in lower or "no prior context" in lower,
            "Template should note this is a fresh session",
        )

    def test_has_no_checkpoint_protocol(self):
        """The BOI template should NOT have the old mesh checkpoint protocol."""
        self.assertNotIn("checkpoint.json", self.content)
        self.assertNotIn(".mesh/", self.content)

    def test_has_one_task_per_iteration(self):
        self.assertTrue(
            "one task" in self.content.lower() or "One task" in self.content,
            "Template should instruct one task per iteration",
        )


# ── Run Script / Prompt Generation (end-to-end) ─────────────────────────


@unittest.skip("worker.sh archived; see test_worker_new.py for Python worker tests")
class TestWorkerEndToEnd(unittest.TestCase):
    """Test archived worker.sh generates the right artifacts before launching Claude."""

    def _run_worker(self, spec_content):
        """Run archived worker.sh with a spec. Returns (result, tmp_dir)."""
        tmp_dir = tempfile.mkdtemp(prefix="boi-test-")
        spec_path = os.path.join(tmp_dir, "spec.md")
        with open(spec_path, "w") as f:
            f.write(spec_content)

        os.makedirs(os.path.join(tmp_dir, ".boi", "queue"), exist_ok=True)
        os.makedirs(os.path.join(tmp_dir, ".boi", "logs"), exist_ok=True)

        result = subprocess.run(
            [
                "bash",
                os.path.join(BOI_ROOT, "archive", "worker.sh"),
                "q-test",
                tmp_dir,
                spec_path,
                "1",
            ],
            capture_output=True,
            text=True,
            timeout=15,
            env={**os.environ, "HOME": tmp_dir},
        )
        return result, tmp_dir

    def test_prints_pending_count(self):
        """Worker should log the number of PENDING tasks."""
        result, tmp_dir = self._run_worker(SAMPLE_SPEC)
        import shutil

        try:
            self.assertIn("2 PENDING task(s) found", result.stdout)
        finally:
            shutil.rmtree(tmp_dir, ignore_errors=True)

    def test_generates_prompt_file(self):
        """Worker should generate a prompt file containing spec content."""
        result, tmp_dir = self._run_worker(SAMPLE_SPEC)
        import shutil

        try:
            prompt_file = os.path.join(tmp_dir, ".boi", "queue", "q-test.prompt.md")
            self.assertTrue(os.path.isfile(prompt_file), "Prompt file not generated")
            with open(prompt_file) as f:
                content = f.read()
            self.assertIn("Second task", content)
            self.assertIn("Third task", content)
            self.assertIn("Iteration", content)
        finally:
            shutil.rmtree(tmp_dir, ignore_errors=True)

    def test_generates_run_script(self):
        """Worker should generate an executable run script."""
        result, tmp_dir = self._run_worker(SAMPLE_SPEC)
        import shutil

        try:
            run_script = os.path.join(tmp_dir, ".boi", "queue", "q-test.run.sh")
            self.assertTrue(os.path.isfile(run_script), "Run script not generated")
            self.assertTrue(os.access(run_script, os.X_OK), "Run script not executable")
            with open(run_script) as f:
                content = f.read()
            self.assertIn("--dangerously-skip-permissions", content)
            self.assertIn("_PRE_PENDING=2", content)
            self.assertIn("_PRE_DONE=1", content)
            self.assertIn("iteration-1.json", content)
        finally:
            shutil.rmtree(tmp_dir, ignore_errors=True)

    def test_run_script_has_post_count_logic(self):
        """Run script should count tasks after Claude exits."""
        result, tmp_dir = self._run_worker(SAMPLE_SPEC)
        import shutil

        try:
            run_script = os.path.join(tmp_dir, ".boi", "queue", "q-test.run.sh")
            with open(run_script) as f:
                content = f.read()
            self.assertIn("count_boi_tasks", content)
            self.assertIn("_POST_PENDING", content)
            self.assertIn("_TASKS_COMPLETED", content)
            self.assertIn("_TASKS_ADDED", content)
        finally:
            shutil.rmtree(tmp_dir, ignore_errors=True)

    def test_prompt_handles_special_characters(self):
        """Spec with {{curly braces}}, $dollar signs, and ${var_refs} should be injected verbatim."""
        result, tmp_dir = self._run_worker(SPECIAL_CHARS_SPEC)
        import shutil

        try:
            prompt_file = os.path.join(tmp_dir, ".boi", "queue", "q-test.prompt.md")
            self.assertTrue(os.path.isfile(prompt_file), "Prompt file not generated")
            with open(prompt_file) as f:
                content = f.read()
            # Verify special characters survived template injection intact
            self.assertIn("{{curly braces}}", content)
            self.assertIn("$dollar signs", content)
            self.assertIn("${variable_refs}", content)
            self.assertIn("${bash_substitution}", content)
            self.assertIn("{{nested}}", content)
            self.assertIn('echo "$PATH"', content)
            self.assertIn('"{{ok}}"', content)
            # Verify the template's own placeholders were replaced (not left raw)
            # {{SPEC_CONTENT}} is injected last, so it doesn't appear as a raw placeholder
            # in the template portion. But the spec *content* itself contains the string
            # "{{SPEC_CONTENT}}" as literal text, so it WILL appear in the output.
            # What matters: {{PENDING_COUNT}} (not in spec content) must be replaced.
            # Count occurrences: the template placeholder is gone, only spec content remains.
            self.assertEqual(
                content.count("{{PENDING_COUNT}}"),
                0,
                "Template placeholder {{PENDING_COUNT}} should be replaced",
            )
            # {{ITERATION}} in the spec content should survive as literal text
            # (because ITERATION replacement happens before SPEC_CONTENT injection)
            self.assertIn(
                "`{{ITERATION}}`",
                content,
                "Spec's literal {{ITERATION}} text should survive injection",
            )
        finally:
            shutil.rmtree(tmp_dir, ignore_errors=True)


# ── Iteration Metadata Format Tests ──────────────────────────────────────


class TestIterationMetadata(unittest.TestCase):
    """Test the iteration-N.json file format."""

    def test_metadata_json_structure(self):
        """Iteration metadata should have all required fields."""
        metadata = {
            "queue_id": "q-001",
            "iteration": 1,
            "exit_code": 0,
            "duration_seconds": 120,
            "started_at": "2025-03-06T10:00:00Z",
            "pre_counts": {"pending": 3, "done": 1, "skipped": 0, "total": 4},
            "post_counts": {"pending": 2, "done": 2, "skipped": 0, "total": 4},
            "tasks_completed": 1,
            "tasks_added": 0,
            "tasks_skipped": 0,
        }

        with tempfile.NamedTemporaryFile(mode="w", suffix=".json", delete=False) as f:
            json.dump(metadata, f, indent=2)
            f.write("\n")
            path = f.name

        try:
            with open(path) as f:
                loaded = json.load(f)
            self.assertEqual(loaded["queue_id"], "q-001")
            self.assertEqual(loaded["iteration"], 1)
            self.assertEqual(loaded["exit_code"], 0)
            self.assertEqual(loaded["duration_seconds"], 120)
            self.assertEqual(loaded["tasks_completed"], 1)
            self.assertEqual(loaded["tasks_added"], 0)
            self.assertEqual(loaded["pre_counts"]["pending"], 3)
            self.assertEqual(loaded["post_counts"]["pending"], 2)
        finally:
            os.unlink(path)

    def test_self_evolution_detected_in_metadata(self):
        """If total increases between pre/post, tasks_added should be positive."""
        pre_total = 5
        post_total = 6
        tasks_added = max(0, post_total - pre_total)
        self.assertEqual(tasks_added, 1)

    def test_delta_calculation_clamps_negative(self):
        """Deltas should never be negative (clamped to 0)."""
        pre_done = 3
        post_done = 2  # shouldn't happen, but guard against it
        tasks_completed = max(0, post_done - pre_done)
        self.assertEqual(tasks_completed, 0)


if __name__ == "__main__":
    unittest.main()
