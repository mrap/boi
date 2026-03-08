"""tests/test_ux_overhaul.py — Integration-style tests for UX overhaul.

Covers:
  1. lib/spec_editor.py: add_task, skip_task, reorder_task, block_task
  2. lib/project.py: create, list, get, get_context, delete
  3. lib/do.py: context gathering (mocked), prompt building, response parsing,
     classify_destructive

Run:
  cd ~/boi && python3 -m pytest tests/test_ux_overhaul.py -v
  cd ~/boi && python3 tests/test_ux_overhaul.py
"""

import json
import os
import shutil
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from lib.do import build_prompt, classify_destructive, parse_response
from lib.project import (
    create_project,
    delete_project,
    get_project,
    get_project_context,
    list_projects,
)
from lib.spec_editor import add_task, block_task, reorder_task, skip_task

# ─── Sample specs ────────────────────────────────────────────────────────────

SAMPLE_SPEC = """\
# Test Spec

## Tasks

### t-1: First task
DONE

**Spec:** Do the first thing.

**Verify:** echo "done"

### t-2: Second task
PENDING

**Spec:** Do the second thing.

**Verify:** echo "check"

### t-3: Third task
PENDING

**Spec:** Do the third thing.

**Verify:** echo "verify"

### t-4: Fourth task
PENDING

**Spec:** Do the fourth thing.

**Verify:** echo "check4"
"""


# ═══════════════════════════════════════════════════════════════════════════════
# 1. spec_editor tests
# ═══════════════════════════════════════════════════════════════════════════════


class TestAddTaskEdgeCases(unittest.TestCase):
    """Edge cases for add_task beyond the basic tests in test_spec_editor.py."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.spec_path = os.path.join(self.tmpdir, "test.spec.md")
        Path(self.spec_path).write_text(SAMPLE_SPEC, encoding="utf-8")

    def tearDown(self):
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_add_task_empty_title_raises(self):
        with self.assertRaises(ValueError):
            add_task(self.spec_path, "")

    def test_add_task_whitespace_only_title_raises(self):
        with self.assertRaises(ValueError):
            add_task(self.spec_path, "   \t  ")

    def test_add_task_increments_past_highest_id(self):
        new_id = add_task(self.spec_path, "Fifth")
        self.assertEqual(new_id, "t-5")

    def test_add_task_with_spec_and_verify(self):
        add_task(self.spec_path, "Full task", "Do something", "echo ok")
        content = Path(self.spec_path).read_text(encoding="utf-8")
        self.assertIn("### t-5: Full task", content)
        self.assertIn("**Spec:** Do something", content)
        self.assertIn("**Verify:** echo ok", content)

    def test_add_task_without_spec_or_verify(self):
        add_task(self.spec_path, "Bare task")
        content = Path(self.spec_path).read_text(encoding="utf-8")
        after = content.split("### t-5: Bare task")[1]
        self.assertNotIn("**Spec:**", after)
        self.assertNotIn("**Verify:**", after)

    def test_add_task_to_empty_spec(self):
        empty = "# Empty\n\n## Tasks\n"
        Path(self.spec_path).write_text(empty, encoding="utf-8")
        new_id = add_task(self.spec_path, "First ever")
        self.assertEqual(new_id, "t-1")

    def test_add_task_no_tmp_file_left(self):
        add_task(self.spec_path, "Atomic")
        self.assertFalse(os.path.exists(self.spec_path + ".tmp"))


class TestSkipTaskEdgeCases(unittest.TestCase):
    """Edge cases for skip_task."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.spec_path = os.path.join(self.tmpdir, "test.spec.md")
        Path(self.spec_path).write_text(SAMPLE_SPEC, encoding="utf-8")

    def tearDown(self):
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_skip_done_task_raises(self):
        with self.assertRaises(ValueError) as ctx:
            skip_task(self.spec_path, "t-1")
        self.assertIn("already DONE", str(ctx.exception))

    def test_skip_nonexistent_task_raises(self):
        with self.assertRaises(ValueError) as ctx:
            skip_task(self.spec_path, "t-99")
        self.assertIn("not found", str(ctx.exception))

    def test_skip_already_skipped_raises(self):
        skip_task(self.spec_path, "t-2")
        with self.assertRaises(ValueError) as ctx:
            skip_task(self.spec_path, "t-2")
        self.assertIn("already SKIPPED", str(ctx.exception))

    def test_skip_with_reason(self):
        skip_task(self.spec_path, "t-2", reason="Not needed")
        content = Path(self.spec_path).read_text(encoding="utf-8")
        self.assertIn("SKIPPED — Not needed", content)

    def test_skip_preserves_other_tasks(self):
        skip_task(self.spec_path, "t-2")
        content = Path(self.spec_path).read_text(encoding="utf-8")
        self.assertIn("### t-1: First task", content)
        self.assertIn("### t-3: Third task", content)
        # t-3 still PENDING
        t3_section = content.split("### t-3:")[1].split("### t-4:")[0]
        self.assertIn("PENDING", t3_section)


class TestReorderTaskEdgeCases(unittest.TestCase):
    """Edge cases for reorder_task."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.spec_path = os.path.join(self.tmpdir, "test.spec.md")
        Path(self.spec_path).write_text(SAMPLE_SPEC, encoding="utf-8")

    def tearDown(self):
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def _task_order(self):
        import re

        content = Path(self.spec_path).read_text(encoding="utf-8")
        return re.findall(r"###\s+(t-\d+):", content)

    def test_reorder_done_task_raises(self):
        with self.assertRaises(ValueError) as ctx:
            reorder_task(self.spec_path, "t-1")
        self.assertIn("DONE", str(ctx.exception))

    def test_reorder_nonexistent_raises(self):
        with self.assertRaises(ValueError) as ctx:
            reorder_task(self.spec_path, "t-99")
        self.assertIn("not found", str(ctx.exception))

    def test_reorder_skipped_task_raises(self):
        skip_task(self.spec_path, "t-2")
        with self.assertRaises(ValueError) as ctx:
            reorder_task(self.spec_path, "t-2")
        self.assertIn("SKIPPED", str(ctx.exception))

    def test_reorder_moves_last_pending_to_front(self):
        reorder_task(self.spec_path, "t-4")
        order = self._task_order()
        # t-4 should be right after last DONE (t-1)
        self.assertEqual(order, ["t-1", "t-4", "t-2", "t-3"])

    def test_reorder_already_first_pending_is_noop(self):
        content_before = Path(self.spec_path).read_text(encoding="utf-8")
        reorder_task(self.spec_path, "t-2")
        content_after = Path(self.spec_path).read_text(encoding="utf-8")
        self.assertEqual(content_before, content_after)

    def test_reorder_preserves_content(self):
        reorder_task(self.spec_path, "t-4")
        content = Path(self.spec_path).read_text(encoding="utf-8")
        self.assertIn("Do the fourth thing.", content)
        self.assertIn('echo "check4"', content)


class TestBlockTaskEdgeCases(unittest.TestCase):
    """Edge cases for block_task."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.spec_path = os.path.join(self.tmpdir, "test.spec.md")
        Path(self.spec_path).write_text(SAMPLE_SPEC, encoding="utf-8")

    def tearDown(self):
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_block_nonexistent_target_raises(self):
        with self.assertRaises(ValueError) as ctx:
            block_task(self.spec_path, "t-99", "t-1")
        self.assertIn("not found", str(ctx.exception))

    def test_block_nonexistent_dependency_raises(self):
        with self.assertRaises(ValueError) as ctx:
            block_task(self.spec_path, "t-2", "t-99")
        self.assertIn("not found", str(ctx.exception))

    def test_block_self_raises(self):
        with self.assertRaises(ValueError) as ctx:
            block_task(self.spec_path, "t-2", "t-2")
        self.assertIn("cannot block itself", str(ctx.exception))

    def test_block_done_task_raises(self):
        with self.assertRaises(ValueError) as ctx:
            block_task(self.spec_path, "t-1", "t-2")
        self.assertIn("already DONE", str(ctx.exception))

    def test_block_creates_line(self):
        block_task(self.spec_path, "t-2", "t-1")
        content = Path(self.spec_path).read_text(encoding="utf-8")
        t2_section = content.split("### t-2:")[1].split("### t-3:")[0]
        self.assertIn("**Blocked by:** t-1", t2_section)

    def test_block_appends_to_existing(self):
        block_task(self.spec_path, "t-3", "t-1")
        block_task(self.spec_path, "t-3", "t-2")
        content = Path(self.spec_path).read_text(encoding="utf-8")
        t3_section = content.split("### t-3:")[1].split("### t-4:")[0]
        self.assertIn("**Blocked by:** t-1, t-2", t3_section)

    def test_block_skipped_task_raises(self):
        skip_task(self.spec_path, "t-2")
        with self.assertRaises(ValueError) as ctx:
            block_task(self.spec_path, "t-2", "t-1")
        self.assertIn("already SKIPPED", str(ctx.exception))


# ═══════════════════════════════════════════════════════════════════════════════
# 2. project tests
# ═══════════════════════════════════════════════════════════════════════════════


class TestProjectCRUD(unittest.TestCase):
    """Integration tests for lib/project.py functions."""

    def setUp(self):
        self.test_dir = tempfile.mkdtemp(prefix="boi-test-ux-")
        self.projects_dir = os.path.join(self.test_dir, "projects")
        self.queue_dir = os.path.join(self.test_dir, "queue")
        os.makedirs(self.projects_dir, exist_ok=True)
        os.makedirs(self.queue_dir, exist_ok=True)
        self.patcher = mock.patch("lib.project.PROJECTS_DIR", self.projects_dir)
        self.patcher.start()

    def tearDown(self):
        self.patcher.stop()
        shutil.rmtree(self.test_dir, ignore_errors=True)

    def test_create_and_get(self):
        result = create_project("test-proj", description="Hello")
        self.assertEqual(result["name"], "test-proj")
        got = get_project("test-proj")
        self.assertIsNotNone(got)
        self.assertEqual(got["name"], "test-proj")
        self.assertEqual(got["description"], "Hello")

    def test_create_duplicate_raises(self):
        create_project("dup")
        with self.assertRaises(ValueError) as ctx:
            create_project("dup")
        self.assertIn("already exists", str(ctx.exception))

    def test_create_invalid_name_spaces(self):
        with self.assertRaises(ValueError) as ctx:
            create_project("has spaces")
        self.assertIn("alphanumeric", str(ctx.exception))

    def test_create_invalid_name_special(self):
        with self.assertRaises(ValueError):
            create_project("bad_name!")

    def test_create_invalid_name_empty(self):
        with self.assertRaises(ValueError):
            create_project("")

    def test_create_valid_names(self):
        for name in ["alpha", "with-hyphens", "CamelCase", "mix123"]:
            create_project(name)
            self.assertIsNotNone(get_project(name))

    def test_list_empty(self):
        self.assertEqual(list_projects(), [])

    def test_list_multiple(self):
        create_project("aaa")
        create_project("bbb")
        result = list_projects()
        names = [p["name"] for p in result]
        self.assertIn("aaa", names)
        self.assertIn("bbb", names)

    def test_get_nonexistent(self):
        self.assertIsNone(get_project("nope"))

    def test_get_context(self):
        create_project("ctx")
        ctx = get_project_context("ctx")
        self.assertEqual(ctx, "# ctx Context\n")

    def test_get_context_nonexistent(self):
        self.assertEqual(get_project_context("nope"), "")

    def test_delete(self):
        create_project("del-me")
        delete_project("del-me")
        self.assertIsNone(get_project("del-me"))
        self.assertFalse(os.path.exists(os.path.join(self.projects_dir, "del-me")))

    def test_delete_nonexistent_raises(self):
        with self.assertRaises(ValueError) as ctx:
            delete_project("ghost")
        self.assertIn("not found", str(ctx.exception))

    def test_project_json_schema(self):
        result = create_project("schema-check", description="desc")
        self.assertIn("created_at", result)
        self.assertEqual(result["default_priority"], 100)
        self.assertEqual(result["default_max_iter"], 30)
        self.assertEqual(result["tags"], [])


# ═══════════════════════════════════════════════════════════════════════════════
# 3. do.py tests
# ═══════════════════════════════════════════════════════════════════════════════


class TestParseResponse(unittest.TestCase):
    """Tests for parse_response — JSON extraction and validation."""

    def test_valid_json(self):
        resp = json.dumps(
            {
                "commands": ["boi status"],
                "explanation": "Show status",
                "destructive": False,
            }
        )
        result = parse_response(resp)
        self.assertEqual(result["commands"], ["boi status"])
        self.assertFalse(result["destructive"])

    def test_json_in_code_block(self):
        resp = '```json\n{"commands": ["boi queue"], "explanation": "List queue", "destructive": false}\n```'
        result = parse_response(resp)
        self.assertEqual(result["commands"], ["boi queue"])

    def test_json_in_generic_code_block(self):
        resp = '```\n{"commands": [], "explanation": "Ambiguous", "destructive": false}\n```'
        result = parse_response(resp)
        self.assertEqual(result["commands"], [])

    def test_json_with_surrounding_text(self):
        resp = 'Here is the result:\n{"commands": ["boi status"], "explanation": "ok", "destructive": false}\nDone.'
        result = parse_response(resp)
        self.assertEqual(result["commands"], ["boi status"])

    def test_missing_commands_raises(self):
        resp = json.dumps({"explanation": "hi", "destructive": False})
        with self.assertRaises(ValueError) as ctx:
            parse_response(resp)
        self.assertIn("commands", str(ctx.exception))

    def test_missing_explanation_raises(self):
        resp = json.dumps({"commands": [], "destructive": False})
        with self.assertRaises(ValueError) as ctx:
            parse_response(resp)
        self.assertIn("explanation", str(ctx.exception))

    def test_missing_destructive_raises(self):
        resp = json.dumps({"commands": [], "explanation": "hi"})
        with self.assertRaises(ValueError) as ctx:
            parse_response(resp)
        self.assertIn("destructive", str(ctx.exception))

    def test_commands_not_list_raises(self):
        resp = json.dumps(
            {"commands": "boi status", "explanation": "hi", "destructive": False}
        )
        with self.assertRaises(ValueError) as ctx:
            parse_response(resp)
        self.assertIn("list", str(ctx.exception))

    def test_commands_item_not_string_raises(self):
        resp = json.dumps(
            {"commands": [123], "explanation": "hi", "destructive": False}
        )
        with self.assertRaises(ValueError) as ctx:
            parse_response(resp)
        self.assertIn("string", str(ctx.exception))

    def test_invalid_json_raises(self):
        with self.assertRaises(ValueError) as ctx:
            parse_response("not json at all")
        self.assertIn("parse", str(ctx.exception).lower())

    def test_non_dict_json_raises(self):
        with self.assertRaises(ValueError) as ctx:
            parse_response("[1, 2, 3]")
        self.assertIn("object", str(ctx.exception))


class TestClassifyDestructive(unittest.TestCase):
    """Tests for classify_destructive — keyword-based safety net."""

    def test_safe_commands(self):
        self.assertFalse(classify_destructive(["boi status"]))
        self.assertFalse(classify_destructive(["boi queue --json"]))
        self.assertFalse(classify_destructive(["boi workers"]))
        self.assertFalse(classify_destructive(["boi log q-001"]))

    def test_destructive_cancel(self):
        self.assertTrue(classify_destructive(["boi cancel q-001"]))

    def test_destructive_stop(self):
        self.assertTrue(classify_destructive(["boi stop"]))

    def test_destructive_purge(self):
        self.assertTrue(classify_destructive(["boi purge --all"]))

    def test_destructive_delete(self):
        self.assertTrue(classify_destructive(["boi project delete foo"]))

    def test_destructive_skip(self):
        self.assertTrue(classify_destructive(["boi spec q-001 skip t-2"]))

    def test_destructive_dispatch(self):
        self.assertTrue(classify_destructive(["boi dispatch --spec f.md"]))

    def test_destructive_block(self):
        self.assertTrue(classify_destructive(["boi spec q-001 block t-2 --on t-1"]))

    def test_destructive_edit(self):
        self.assertTrue(classify_destructive(["boi spec q-001 edit t-3"]))

    def test_destructive_next(self):
        self.assertTrue(classify_destructive(["boi spec q-001 next t-5"]))

    def test_mixed_safe_and_destructive(self):
        self.assertTrue(classify_destructive(["boi status", "boi cancel q-001"]))

    def test_empty_commands(self):
        self.assertFalse(classify_destructive([]))

    def test_case_insensitive(self):
        self.assertTrue(classify_destructive(["boi CANCEL q-001"]))
        self.assertTrue(classify_destructive(["boi Cancel q-001"]))


class TestBuildPrompt(unittest.TestCase):
    """Tests for build_prompt — template substitution."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.template_path = os.path.join(self.tmpdir, "do-prompt.md")
        template = (
            "Status: {{BOI_STATUS}}\n"
            "Queue: {{BOI_QUEUE}}\n"
            "Workers: {{BOI_WORKERS}}\n"
            "Projects: {{BOI_PROJECTS}}\n"
            "Spec: {{BOI_SPEC}}\n"
            "Input: {{USER_INPUT}}\n"
        )
        Path(self.template_path).write_text(template, encoding="utf-8")
        self.patcher = mock.patch("lib.do.TEMPLATE_PATH", Path(self.template_path))
        self.patcher.start()

    def tearDown(self):
        self.patcher.stop()
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_substitutes_all_placeholders(self):
        context = {
            "status": "running",
            "queue": "[]",
            "workers": "2 active",
            "projects": "[]",
            "spec": "tasks here",
        }
        result = build_prompt("show status", context)
        self.assertIn("Status: running", result)
        self.assertIn("Queue: []", result)
        self.assertIn("Workers: 2 active", result)
        self.assertIn("Projects: []", result)
        self.assertIn("Spec: tasks here", result)
        self.assertIn("Input: show status", result)

    def test_missing_context_keys_become_empty(self):
        result = build_prompt("test", {})
        self.assertIn("Status: \n", result)
        self.assertIn("Input: test", result)

    def test_template_not_found_raises(self):
        with mock.patch("lib.do.TEMPLATE_PATH", Path("/nonexistent/template.md")):
            with self.assertRaises(FileNotFoundError):
                build_prompt("test", {})


class TestGatherContext(unittest.TestCase):
    """Tests for gather_context with mocked subprocess calls."""

    @mock.patch("lib.do._run_boi")
    @mock.patch("lib.do.PROJECTS_DIR", Path("/nonexistent"))
    def test_basic_gathering(self, mock_run):
        mock_run.side_effect = lambda args, **kw: {
            ("status", "--json"): '{"running": true}',
            ("queue", "--json"): "[]",
            ("workers", "--json"): '{"count": 2}',
        }.get(tuple(args), "")

        from lib.do import gather_context

        ctx = gather_context("show me the status")
        self.assertIn("running", ctx["status"])
        self.assertEqual(ctx["projects"], "")  # no projects dir
        self.assertEqual(ctx["spec"], "")  # no queue ID in input

    @mock.patch("lib.do._run_boi")
    def test_extracts_queue_id(self, mock_run):
        mock_run.return_value = '{"tasks": []}'

        # Ensure PROJECTS_DIR doesn't exist for this test
        with mock.patch("lib.do.PROJECTS_DIR", Path("/nonexistent")):
            from lib.do import gather_context

            ctx = gather_context("show spec for q-007")
        # Should have called spec with q-007
        calls = [tuple(c[0][0]) for c in mock_run.call_args_list]
        self.assertIn(("spec", "q-007", "--json"), calls)


# ═══════════════════════════════════════════════════════════════════════════════
# Main
# ═══════════════════════════════════════════════════════════════════════════════

if __name__ == "__main__":
    unittest.main()
