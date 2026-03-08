# tests/test_project.py — Tests for lib/project.py
#
# Run: cd ~/boi && python3 -m pytest tests/test_project.py -v
# Or:  cd ~/boi && python3 tests/test_project.py

import json
import os
import shutil
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import lib.project  # noqa: E402, F401 — must be imported before mock.patch can target it

_test_dir = None


def _make_test_dir():
    global _test_dir
    _test_dir = tempfile.mkdtemp(prefix="boi-test-project-")
    return _test_dir


class TestProject(unittest.TestCase):
    def setUp(self):
        self.test_dir = _make_test_dir()
        self.projects_dir = os.path.join(self.test_dir, "projects")
        self.queue_dir = os.path.join(self.test_dir, "queue")
        os.makedirs(self.projects_dir, exist_ok=True)
        os.makedirs(self.queue_dir, exist_ok=True)

        # Patch PROJECTS_DIR and the queue dir used by list_projects
        self.patcher_projects = mock.patch(
            "lib.project.PROJECTS_DIR", self.projects_dir
        )
        self.patcher_projects.start()

    def tearDown(self):
        self.patcher_projects.stop()
        shutil.rmtree(self.test_dir, ignore_errors=True)

    def test_create_project_basic(self):
        from lib.project import create_project

        result = create_project("my-project", description="A test project")

        self.assertEqual(result["name"], "my-project")
        self.assertEqual(result["description"], "A test project")
        self.assertEqual(result["default_priority"], 100)
        self.assertEqual(result["default_max_iter"], 30)
        self.assertEqual(result["tags"], [])
        self.assertIn("created_at", result)

        # Verify files on disk
        pdir = os.path.join(self.projects_dir, "my-project")
        self.assertTrue(os.path.isdir(pdir))
        self.assertTrue(os.path.isfile(os.path.join(pdir, "project.json")))
        self.assertTrue(os.path.isfile(os.path.join(pdir, "context.md")))

        # Verify context.md content
        with open(os.path.join(pdir, "context.md")) as f:
            content = f.read()
        self.assertEqual(content, "# my-project Context\n")

    def test_create_project_duplicate(self):
        from lib.project import create_project

        create_project("dup-test")
        with self.assertRaises(ValueError) as ctx:
            create_project("dup-test")
        self.assertIn("already exists", str(ctx.exception))

    def test_create_project_invalid_name_spaces(self):
        from lib.project import create_project

        with self.assertRaises(ValueError) as ctx:
            create_project("has spaces")
        self.assertIn("alphanumeric", str(ctx.exception))

    def test_create_project_invalid_name_special_chars(self):
        from lib.project import create_project

        with self.assertRaises(ValueError) as ctx:
            create_project("bad_name!")
        self.assertIn("alphanumeric", str(ctx.exception))

    def test_create_project_invalid_name_empty(self):
        from lib.project import create_project

        with self.assertRaises(ValueError):
            create_project("")

    def test_create_project_valid_names(self):
        from lib.project import create_project

        # These should all work
        create_project("simple")
        create_project("with-hyphens")
        create_project("CamelCase")
        create_project("mix123")

    def test_list_projects_empty(self):
        from lib.project import list_projects

        result = list_projects()
        self.assertEqual(result, [])

    def test_list_projects_multiple(self):
        from lib.project import create_project, list_projects

        create_project("alpha", description="First")
        create_project("beta", description="Second")

        result = list_projects()
        self.assertEqual(len(result), 2)
        names = [p["name"] for p in result]
        self.assertIn("alpha", names)
        self.assertIn("beta", names)

        # All should have spec_count = 0 (no queue entries)
        for p in result:
            self.assertEqual(p["spec_count"], 0)

    def test_list_projects_with_spec_counts(self):
        from lib.project import create_project, list_projects

        create_project("counted")

        # Create a mock queue entry that references this project
        with mock.patch("lib.project.os.path.expanduser") as mock_expand:
            # We need to mock expanduser for the queue_dir inside list_projects
            def expand_side_effect(path):
                if path == "~/.boi/projects":
                    return self.projects_dir
                if path == "~/.boi/queue":
                    return self.queue_dir
                return os.path.expanduser(path)

            mock_expand.side_effect = expand_side_effect

            # Write a queue entry referencing the project
            entry = {"id": "q-001", "project": "counted", "status": "queued"}
            with open(os.path.join(self.queue_dir, "q-001.json"), "w") as f:
                json.dump(entry, f)

            result = list_projects()
            counted_proj = [p for p in result if p["name"] == "counted"][0]
            self.assertEqual(counted_proj["spec_count"], 1)

    def test_get_project_exists(self):
        from lib.project import create_project, get_project

        create_project("getme", description="Hello")
        result = get_project("getme")
        self.assertIsNotNone(result)
        self.assertEqual(result["name"], "getme")
        self.assertEqual(result["description"], "Hello")

    def test_get_project_not_found(self):
        from lib.project import get_project

        result = get_project("nonexistent")
        self.assertIsNone(result)

    def test_get_project_context(self):
        from lib.project import create_project, get_project_context

        create_project("ctx-test")
        result = get_project_context("ctx-test")
        self.assertEqual(result, "# ctx-test Context\n")

    def test_get_project_context_not_found(self):
        from lib.project import get_project_context

        result = get_project_context("no-such-project")
        self.assertEqual(result, "")

    def test_delete_project(self):
        from lib.project import create_project, delete_project, get_project

        create_project("to-delete")
        self.assertIsNotNone(get_project("to-delete"))

        delete_project("to-delete")
        self.assertIsNone(get_project("to-delete"))
        self.assertFalse(os.path.exists(os.path.join(self.projects_dir, "to-delete")))

    def test_delete_project_not_found(self):
        from lib.project import delete_project

        with self.assertRaises(ValueError) as ctx:
            delete_project("ghost")
        self.assertIn("not found", str(ctx.exception))

    def test_create_project_no_description(self):
        from lib.project import create_project

        result = create_project("no-desc")
        self.assertEqual(result["description"], "")

    def test_project_json_valid(self):
        from lib.project import create_project

        create_project("json-check")
        pjson = os.path.join(self.projects_dir, "json-check", "project.json")
        with open(pjson) as f:
            data = json.load(f)
        self.assertEqual(data["name"], "json-check")
        self.assertIn("created_at", data)


if __name__ == "__main__":
    unittest.main()
