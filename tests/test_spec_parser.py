"""test_spec_parser.py — Tests for BOI spec parser.

Tests all functions in lib/spec_parser.py:
- parse_tasks_md: parse mesh-format tasks.md
- parse_spec_json: parse spec.json dict
- parse_file: auto-detect format by extension
- parse_boi_spec: parse BOI self-evolving spec.md
- count_boi_tasks: count PENDING/DONE/SKIPPED tasks
- convert_tasks_to_spec: convert mesh format to BOI format
- Task dataclass serialization

All tests use mock data and temp directories. No live API calls.
Uses unittest (stdlib only, no pytest dependency).
"""

import json
import os
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

BOI_ROOT = str(Path(__file__).resolve().parent.parent)
sys.path.insert(0, BOI_ROOT)

from lib.spec_parser import (
    BoiTask,
    check_status_regression,
    convert_tasks_to_spec,
    count_boi_tasks,
    parse_boi_spec,
    parse_file,
    parse_spec_json,
    parse_tasks_md,
    StatusRegression,
    Task,
)


# ── parse_tasks_md Tests ──────────────────────────────────────────────────


class TestParseTasksMd(unittest.TestCase):
    """Tests for parsing mesh-format tasks.md files."""

    def test_single_task(self):
        content = textwrap.dedent("""\
            ## t-001: Build the widget
            - **Spec:** Create a new widget component.
            - **Files:** widget.py
            - **Deps:** none
            - **Verify:** python3 -m pytest tests/test_widget.py
            - **Commit prefix:** [widget][ui]
        """)
        tasks = parse_tasks_md(content)
        self.assertEqual(len(tasks), 1)
        self.assertEqual(tasks[0].id, "t-001")
        self.assertEqual(tasks[0].title, "Build the widget")
        self.assertEqual(tasks[0].spec, "Create a new widget component.")
        self.assertEqual(tasks[0].files, ["widget.py"])
        self.assertEqual(tasks[0].deps, [])
        self.assertEqual(tasks[0].verify, ["python3 -m pytest tests/test_widget.py"])
        self.assertEqual(tasks[0].commit_prefix, "[widget][ui]")

    def test_multiple_tasks(self):
        content = textwrap.dedent("""\
            ## t-001: First task
            - **Spec:** Do first thing.
            - **Files:** a.php
            - **Deps:** none
            - **Verify:** echo ok

            ## t-002: Second task
            - **Spec:** Do second thing.
            - **Files:** b.php
            - **Deps:** t-001
            - **Verify:** echo ok2
        """)
        tasks = parse_tasks_md(content)
        self.assertEqual(len(tasks), 2)
        self.assertEqual(tasks[0].id, "t-001")
        self.assertEqual(tasks[1].id, "t-002")
        self.assertEqual(tasks[1].deps, ["t-001"])

    def test_multiple_files(self):
        content = textwrap.dedent("""\
            ## t-001: Multi-file task
            - **Spec:** Touch multiple files.
            - **Files:** a.php, b.php, c.php
            - **Deps:** none
            - **Verify:** echo ok
        """)
        tasks = parse_tasks_md(content)
        self.assertEqual(tasks[0].files, ["a.php", "b.php", "c.php"])

    def test_multiple_deps(self):
        content = textwrap.dedent("""\
            ## t-003: Depends on two
            - **Spec:** Needs both.
            - **Files:** c.php
            - **Deps:** t-001, t-002
            - **Verify:** echo ok
        """)
        tasks = parse_tasks_md(content)
        self.assertEqual(tasks[0].deps, ["t-001", "t-002"])

    def test_verify_with_chained_commands(self):
        content = textwrap.dedent("""\
            ## t-001: Multi-verify
            - **Spec:** Check everything.
            - **Files:** x.php
            - **Deps:** none
            - **Verify:** python3 -m pytest tests/test_x.py && lint check && python3 -m unittest test_x
        """)
        tasks = parse_tasks_md(content)
        self.assertEqual(len(tasks[0].verify), 3)
        self.assertEqual(tasks[0].verify[0], "python3 -m pytest tests/test_x.py")
        self.assertEqual(tasks[0].verify[1], "lint check")
        self.assertEqual(tasks[0].verify[2], "python3 -m unittest test_x")

    def test_empty_content(self):
        tasks = parse_tasks_md("")
        self.assertEqual(len(tasks), 0)

    def test_no_task_headings(self):
        content = "# Some random markdown\n\nNo tasks here.\n"
        tasks = parse_tasks_md(content)
        self.assertEqual(len(tasks), 0)

    def test_missing_optional_fields(self):
        content = textwrap.dedent("""\
            ## t-001: Minimal task
            - **Spec:** Just a spec.
        """)
        tasks = parse_tasks_md(content)
        self.assertEqual(len(tasks), 1)
        self.assertEqual(tasks[0].files, [])
        self.assertEqual(tasks[0].deps, [])
        self.assertEqual(tasks[0].verify, [])
        self.assertEqual(tasks[0].commit_prefix, "")

    def test_continuation_lines(self):
        content = textwrap.dedent("""\
            ## t-001: Multiline spec
            - **Spec:** This is a long spec that
              continues on the next line
              and even further.
            - **Files:** a.php
            - **Deps:** none
            - **Verify:** echo ok
        """)
        tasks = parse_tasks_md(content)
        self.assertIn("continues on the next line", tasks[0].spec)
        self.assertIn("and even further.", tasks[0].spec)

    def test_deps_none_string(self):
        content = textwrap.dedent("""\
            ## t-001: No deps
            - **Spec:** Independent.
            - **Deps:** none
            - **Verify:** echo ok
        """)
        tasks = parse_tasks_md(content)
        self.assertEqual(tasks[0].deps, [])

    def test_preamble_before_first_task(self):
        content = textwrap.dedent("""\
            # Project Tasks

            Some preamble text that should be ignored.

            ## t-001: The real task
            - **Spec:** Do it.
            - **Verify:** echo ok
        """)
        tasks = parse_tasks_md(content)
        self.assertEqual(len(tasks), 1)
        self.assertEqual(tasks[0].id, "t-001")


# ── parse_spec_json Tests ──────────────────────────────────────────────────


class TestParseSpecJson(unittest.TestCase):
    """Tests for parsing spec.json dicts."""

    def test_single_task(self):
        data = {
            "tasks": [
                {
                    "id": "t-001",
                    "title": "Build widget",
                    "spec": "Create widget.",
                    "files": ["widget.php"],
                    "deps": [],
                    "verify": ["echo ok"],
                }
            ]
        }
        tasks = parse_spec_json(data)
        self.assertEqual(len(tasks), 1)
        self.assertEqual(tasks[0].id, "t-001")
        self.assertEqual(tasks[0].title, "Build widget")
        self.assertEqual(tasks[0].files, ["widget.php"])

    def test_multiple_tasks(self):
        data = {
            "tasks": [
                {"id": "t-001", "title": "First"},
                {"id": "t-002", "title": "Second", "deps": ["t-001"]},
            ]
        }
        tasks = parse_spec_json(data)
        self.assertEqual(len(tasks), 2)
        self.assertEqual(tasks[1].deps, ["t-001"])

    def test_missing_tasks_key(self):
        with self.assertRaises(ValueError) as ctx:
            parse_spec_json({"not_tasks": []})
        self.assertIn("tasks", str(ctx.exception))

    def test_missing_id(self):
        with self.assertRaises(ValueError) as ctx:
            parse_spec_json({"tasks": [{"title": "No ID"}]})
        self.assertIn("id", str(ctx.exception))

    def test_missing_title(self):
        with self.assertRaises(ValueError) as ctx:
            parse_spec_json({"tasks": [{"id": "t-001"}]})
        self.assertIn("title", str(ctx.exception))

    def test_optional_fields_default(self):
        data = {"tasks": [{"id": "t-001", "title": "Minimal"}]}
        tasks = parse_spec_json(data)
        self.assertEqual(tasks[0].spec, "")
        self.assertEqual(tasks[0].files, [])
        self.assertEqual(tasks[0].deps, [])
        self.assertEqual(tasks[0].verify, [])
        self.assertEqual(tasks[0].commit_prefix, "")


# ── parse_file Tests ───────────────────────────────────────────────────────


class TestParseFile(unittest.TestCase):
    """Tests for auto-detect format parsing."""

    def setUp(self):
        self.tmp_dir = tempfile.mkdtemp(prefix="boi-test-")

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmp_dir, ignore_errors=True)

    def test_parse_md_file(self):
        md_path = os.path.join(self.tmp_dir, "tasks.md")
        content = textwrap.dedent("""\
            ## t-001: Widget
            - **Spec:** Build it.
            - **Verify:** echo ok
        """)
        Path(md_path).write_text(content, encoding="utf-8")
        tasks = parse_file(md_path)
        self.assertEqual(len(tasks), 1)
        self.assertEqual(tasks[0].id, "t-001")

    def test_parse_json_file(self):
        json_path = os.path.join(self.tmp_dir, "spec.json")
        data = {"tasks": [{"id": "t-001", "title": "Widget"}]}
        Path(json_path).write_text(json.dumps(data), encoding="utf-8")
        tasks = parse_file(json_path)
        self.assertEqual(len(tasks), 1)
        self.assertEqual(tasks[0].id, "t-001")

    def test_unsupported_extension(self):
        txt_path = os.path.join(self.tmp_dir, "tasks.txt")
        Path(txt_path).write_text("hello", encoding="utf-8")
        with self.assertRaises(ValueError) as ctx:
            parse_file(txt_path)
        self.assertIn(".txt", str(ctx.exception))

    def test_nonexistent_file(self):
        with self.assertRaises(FileNotFoundError):
            parse_file(os.path.join(self.tmp_dir, "nope.md"))


# ── parse_boi_spec Tests ─────────────────────────────────────────────────


class TestParseBoiSpec(unittest.TestCase):
    """Tests for parsing BOI self-evolving spec.md format."""

    def test_all_pending(self):
        content = textwrap.dedent("""\
            ### t-1: First
            PENDING

            **Spec:** Do it.

            ### t-2: Second
            PENDING

            **Spec:** Do it too.
        """)
        tasks = parse_boi_spec(content)
        self.assertEqual(len(tasks), 2)
        self.assertEqual(tasks[0].status, "PENDING")
        self.assertEqual(tasks[1].status, "PENDING")

    def test_mixed_statuses(self):
        content = textwrap.dedent("""\
            ### t-1: Done task
            DONE

            **Spec:** Already done.

            ### t-2: Pending task
            PENDING

            **Spec:** Still pending.

            ### t-3: Skipped task
            SKIPPED

            **Spec:** Skipped.
        """)
        tasks = parse_boi_spec(content)
        self.assertEqual(len(tasks), 3)
        self.assertEqual(tasks[0].status, "DONE")
        self.assertEqual(tasks[1].status, "PENDING")
        self.assertEqual(tasks[2].status, "SKIPPED")

    def test_task_ids(self):
        content = textwrap.dedent("""\
            ### t-1: Alpha
            PENDING

            ### t-2: Beta
            DONE

            ### t-10: Gamma
            PENDING
        """)
        tasks = parse_boi_spec(content)
        ids = [t.id for t in tasks]
        self.assertEqual(ids, ["t-1", "t-2", "t-10"])

    def test_task_titles(self):
        content = textwrap.dedent("""\
            ### t-1: Build the authentication system
            PENDING

            **Spec:** JWT auth.
        """)
        tasks = parse_boi_spec(content)
        self.assertEqual(tasks[0].title, "Build the authentication system")

    def test_task_body_extraction(self):
        content = textwrap.dedent("""\
            ### t-1: Task with body
            PENDING

            **Spec:** Do the thing.

            **Verify:** echo ok

            **Self-evolution:** Add tasks if needed.
        """)
        tasks = parse_boi_spec(content)
        self.assertIn("**Spec:** Do the thing.", tasks[0].body)
        self.assertIn("**Verify:** echo ok", tasks[0].body)

    def test_done_with_trailing_notes(self):
        content = textwrap.dedent("""\
            ### t-1: Completed task
            DONE — completed with 5 tests passing

            **Spec:** Was done.
        """)
        tasks = parse_boi_spec(content)
        self.assertEqual(len(tasks), 1)
        self.assertEqual(tasks[0].status, "DONE")

    def test_blank_lines_before_status(self):
        content = textwrap.dedent("""\
            ### t-1: Task with blank line

            PENDING

            **Spec:** Has blank line before status.
        """)
        tasks = parse_boi_spec(content)
        self.assertEqual(len(tasks), 1)
        self.assertEqual(tasks[0].status, "PENDING")

    def test_empty_content(self):
        tasks = parse_boi_spec("")
        self.assertEqual(len(tasks), 0)

    def test_no_tasks(self):
        content = "# Just a heading\n\nNo tasks here.\n"
        tasks = parse_boi_spec(content)
        self.assertEqual(len(tasks), 0)

    def test_task_without_status_defaults_to_pending(self):
        """Tasks without a valid status line should default to PENDING."""
        content = textwrap.dedent("""\
            ### t-1: No status line

            **Spec:** This has no PENDING/DONE/SKIPPED.

            ### t-2: Has status
            PENDING

            **Spec:** This one is fine.
        """)
        tasks = parse_boi_spec(content)
        # t-1 has no explicit status → defaults to PENDING
        self.assertEqual(len(tasks), 2)
        self.assertEqual(tasks[0].id, "t-1")
        self.assertEqual(tasks[0].status, "PENDING")
        self.assertIn("**Spec:**", tasks[0].body)
        self.assertEqual(tasks[1].id, "t-2")
        self.assertEqual(tasks[1].status, "PENDING")

    def test_content_before_status_preserved(self):
        """Content between heading and status line should be kept in body."""
        content = textwrap.dedent("""\
            ### t-1: Task with pre-status content

            **Spec:** Placed before status by mistake.
            **Verify:** echo ok

            PENDING
        """)
        tasks = parse_boi_spec(content)
        self.assertEqual(len(tasks), 1)
        self.assertEqual(tasks[0].status, "PENDING")
        self.assertIn("**Spec:**", tasks[0].body)
        self.assertIn("**Verify:**", tasks[0].body)

    def test_multiple_blank_lines_before_status(self):
        """Multiple blank lines between heading and status should work."""
        content = textwrap.dedent("""\
            ### t-1: Task with many blank lines



            PENDING

            **Spec:** Do it.
        """)
        tasks = parse_boi_spec(content)
        self.assertEqual(len(tasks), 1)
        self.assertEqual(tasks[0].status, "PENDING")

    def test_preamble_ignored(self):
        content = textwrap.dedent("""\
            # My Spec

            ## Vision
            Build something great.

            ## Tasks

            ### t-1: The task
            PENDING

            **Spec:** Do it.
        """)
        tasks = parse_boi_spec(content)
        self.assertEqual(len(tasks), 1)
        self.assertEqual(tasks[0].id, "t-1")


# ── count_boi_tasks Tests ────────────────────────────────────────────────


class TestCountBoiTasks(unittest.TestCase):
    """Tests for counting task statuses in a BOI spec file."""

    def setUp(self):
        self.tmp_dir = tempfile.mkdtemp(prefix="boi-test-")

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmp_dir, ignore_errors=True)

    def _write(self, content, name="spec.md"):
        path = os.path.join(self.tmp_dir, name)
        Path(path).write_text(content, encoding="utf-8")
        return path

    def test_all_pending(self):
        path = self._write(
            textwrap.dedent("""\
            ### t-1: A
            PENDING
            ### t-2: B
            PENDING
            ### t-3: C
            PENDING
        """)
        )
        counts = count_boi_tasks(path)
        self.assertEqual(counts["pending"], 3)
        self.assertEqual(counts["done"], 0)
        self.assertEqual(counts["skipped"], 0)
        self.assertEqual(counts["total"], 3)

    def test_all_done(self):
        path = self._write(
            textwrap.dedent("""\
            ### t-1: A
            DONE
            ### t-2: B
            DONE
        """)
        )
        counts = count_boi_tasks(path)
        self.assertEqual(counts["done"], 2)
        self.assertEqual(counts["pending"], 0)
        self.assertEqual(counts["total"], 2)

    def test_mixed(self):
        path = self._write(
            textwrap.dedent("""\
            ### t-1: A
            DONE
            ### t-2: B
            PENDING
            ### t-3: C
            SKIPPED
            ### t-4: D
            PENDING
        """)
        )
        counts = count_boi_tasks(path)
        self.assertEqual(counts["done"], 1)
        self.assertEqual(counts["pending"], 2)
        self.assertEqual(counts["skipped"], 1)
        self.assertEqual(counts["total"], 4)

    def test_nonexistent_file(self):
        counts = count_boi_tasks("/nonexistent/path.md")
        self.assertEqual(counts["total"], 0)

    def test_empty_file(self):
        path = self._write("")
        counts = count_boi_tasks(path)
        self.assertEqual(counts["total"], 0)


# ── convert_tasks_to_spec Tests ──────────────────────────────────────────


class TestConvertTasksToSpec(unittest.TestCase):
    """Tests for converting mesh tasks.md to BOI spec.md."""

    def setUp(self):
        self.tmp_dir = tempfile.mkdtemp(prefix="boi-test-")

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmp_dir, ignore_errors=True)

    def test_basic_conversion(self):
        tasks_path = os.path.join(self.tmp_dir, "tasks.md")
        Path(tasks_path).write_text(
            textwrap.dedent("""\
                ## t-001: Build widget
                - **Spec:** Create a widget.
                - **Files:** widget.php
                - **Deps:** none
                - **Verify:** echo ok

                ## t-002: Test widget
                - **Spec:** Add tests.
                - **Files:** test_widget.php
                - **Deps:** t-001
                - **Verify:** python3 -m pytest test_widget.py
            """),
            encoding="utf-8",
        )

        output_path = os.path.join(self.tmp_dir, "spec.md")
        count = convert_tasks_to_spec(tasks_path, output_path)

        self.assertEqual(count, 2)
        self.assertTrue(os.path.isfile(output_path))

        content = Path(output_path).read_text(encoding="utf-8")
        self.assertIn("### t-1:", content)
        self.assertIn("### t-2:", content)
        self.assertIn("PENDING", content)

    def test_converted_spec_is_parseable(self):
        tasks_path = os.path.join(self.tmp_dir, "tasks.md")
        Path(tasks_path).write_text(
            textwrap.dedent("""\
                ## t-001: Task A
                - **Spec:** Do A.
                - **Verify:** echo a
            """),
            encoding="utf-8",
        )

        output_path = os.path.join(self.tmp_dir, "spec.md")
        convert_tasks_to_spec(tasks_path, output_path)

        # The output should be parseable by count_boi_tasks
        counts = count_boi_tasks(output_path)
        self.assertEqual(counts["pending"], 1)
        self.assertEqual(counts["total"], 1)

    def test_empty_tasks_raises(self):
        tasks_path = os.path.join(self.tmp_dir, "empty.md")
        Path(tasks_path).write_text("# No tasks\n", encoding="utf-8")

        output_path = os.path.join(self.tmp_dir, "spec.md")
        with self.assertRaises(ValueError):
            convert_tasks_to_spec(tasks_path, output_path)

    def test_preserves_spec_content(self):
        tasks_path = os.path.join(self.tmp_dir, "tasks.md")
        Path(tasks_path).write_text(
            textwrap.dedent("""\
                ## t-001: Important feature
                - **Spec:** Build the authentication system with JWT tokens.
                - **Verify:** python3 -m pytest tests/test_auth.py && lint check
            """),
            encoding="utf-8",
        )

        output_path = os.path.join(self.tmp_dir, "spec.md")
        convert_tasks_to_spec(tasks_path, output_path)

        content = Path(output_path).read_text(encoding="utf-8")
        self.assertIn("authentication system with JWT tokens", content)

    def test_preserves_verify_commands(self):
        tasks_path = os.path.join(self.tmp_dir, "tasks.md")
        Path(tasks_path).write_text(
            textwrap.dedent("""\
                ## t-001: Build it
                - **Spec:** Build.
                - **Verify:** python3 -m pytest tests/test_x.py && lint check
            """),
            encoding="utf-8",
        )

        output_path = os.path.join(self.tmp_dir, "spec.md")
        convert_tasks_to_spec(tasks_path, output_path)

        content = Path(output_path).read_text(encoding="utf-8")
        self.assertIn("python3 -m pytest tests/test_x.py", content)
        self.assertIn("lint check", content)


# ── Task dataclass Tests ─────────────────────────────────────────────────


class TestTaskDataclass(unittest.TestCase):
    """Tests for the Task dataclass."""

    def test_to_dict(self):
        task = Task(
            id="t-001",
            title="Build widget",
            spec="Create widget.",
            files=["widget.php"],
            deps=["t-000"],
            verify=["echo ok"],
            commit_prefix="[w]",
        )
        d = task.to_dict()
        self.assertEqual(d["id"], "t-001")
        self.assertEqual(d["title"], "Build widget")
        self.assertEqual(d["files"], ["widget.php"])
        self.assertEqual(d["deps"], ["t-000"])
        self.assertEqual(d["verify"], ["echo ok"])

    def test_default_fields(self):
        task = Task(id="t-001", title="Minimal")
        self.assertEqual(task.spec, "")
        self.assertEqual(task.files, [])
        self.assertEqual(task.deps, [])
        self.assertEqual(task.verify, [])
        self.assertEqual(task.commit_prefix, "")

    def test_to_dict_serializable(self):
        """to_dict output should be JSON-serializable."""
        task = Task(id="t-001", title="Test", files=["a.php"])
        d = task.to_dict()
        serialized = json.dumps(d)
        self.assertIsInstance(serialized, str)


class TestBoiTaskDataclass(unittest.TestCase):
    """Tests for the BoiTask dataclass."""

    def test_basic_fields(self):
        task = BoiTask(
            id="t-1", title="Build it", status="PENDING", body="**Spec:** Do it."
        )
        self.assertEqual(task.id, "t-1")
        self.assertEqual(task.title, "Build it")
        self.assertEqual(task.status, "PENDING")
        self.assertIn("Spec", task.body)

    def test_default_body(self):
        task = BoiTask(id="t-1", title="Minimal", status="DONE")
        self.assertEqual(task.body, "")


class TestCheckStatusRegression(unittest.TestCase):
    """Tests for check_status_regression()."""

    def test_no_regression_when_no_changes(self):
        """No regression detected when statuses are unchanged."""
        prev = [BoiTask(id="t-1", title="A", status="DONE")]
        curr = [BoiTask(id="t-1", title="A", status="DONE")]
        result = check_status_regression(prev, curr)
        self.assertEqual(result, [])

    def test_done_to_pending_detected(self):
        """Detects regression from DONE back to PENDING."""
        prev = [
            BoiTask(id="t-1", title="A", status="DONE"),
            BoiTask(id="t-2", title="B", status="PENDING"),
        ]
        curr = [
            BoiTask(id="t-1", title="A", status="PENDING"),
            BoiTask(id="t-2", title="B", status="DONE"),
        ]
        result = check_status_regression(prev, curr)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0].task_id, "t-1")
        self.assertEqual(result[0].previous_status, "DONE")
        self.assertEqual(result[0].current_status, "PENDING")

    def test_pending_to_done_not_regression(self):
        """Forward progress (PENDING -> DONE) is not a regression."""
        prev = [BoiTask(id="t-1", title="A", status="PENDING")]
        curr = [BoiTask(id="t-1", title="A", status="DONE")]
        result = check_status_regression(prev, curr)
        self.assertEqual(result, [])

    def test_new_tasks_not_regression(self):
        """New tasks in current that didn't exist before are not regressions."""
        prev = [BoiTask(id="t-1", title="A", status="DONE")]
        curr = [
            BoiTask(id="t-1", title="A", status="DONE"),
            BoiTask(id="t-2", title="B", status="PENDING"),
        ]
        result = check_status_regression(prev, curr)
        self.assertEqual(result, [])

    def test_multiple_regressions(self):
        """Detects multiple regressions in one pass."""
        prev = [
            BoiTask(id="t-1", title="A", status="DONE"),
            BoiTask(id="t-2", title="B", status="DONE"),
            BoiTask(id="t-3", title="C", status="PENDING"),
        ]
        curr = [
            BoiTask(id="t-1", title="A", status="PENDING"),
            BoiTask(id="t-2", title="B", status="SKIPPED"),
            BoiTask(id="t-3", title="C", status="DONE"),
        ]
        result = check_status_regression(prev, curr)
        self.assertEqual(len(result), 2)
        ids = {r.task_id for r in result}
        self.assertEqual(ids, {"t-1", "t-2"})

    def test_empty_lists(self):
        """Empty task lists produce no regressions."""
        result = check_status_regression([], [])
        self.assertEqual(result, [])

    def test_status_regression_dataclass(self):
        """StatusRegression dataclass holds expected fields."""
        r = StatusRegression(
            task_id="t-1", previous_status="DONE", current_status="PENDING"
        )
        self.assertEqual(r.task_id, "t-1")
        self.assertEqual(r.previous_status, "DONE")
        self.assertEqual(r.current_status, "PENDING")


if __name__ == "__main__":
    unittest.main()
