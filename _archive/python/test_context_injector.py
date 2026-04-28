"""Unit tests for ContextInjector.

Tests all methods of the ContextInjector class using temp directories
that mimic external context and BOI state structures. Mock data only, no live API calls.
"""

import os
import shutil
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from lib.context_injector import ContextInjector


class TestContextInjector(unittest.TestCase):
    def setUp(self):
        """Create temp directories mimicking context and BOI structures."""
        self.temp_dir = tempfile.mkdtemp()
        self.context_dir = os.path.join(self.temp_dir, "context")
        self.boi_dir = os.path.join(self.temp_dir, "boi")
        # Create project dirs
        os.makedirs(os.path.join(self.context_dir, "projects", "test-project"))
        os.makedirs(os.path.join(self.boi_dir, "projects", "test-project"))
        # Create the injector with our temp dirs
        self.ci = ContextInjector(context_dir=self.context_dir, boi_dir=self.boi_dir)

    def tearDown(self):
        shutil.rmtree(self.temp_dir)

    def _write_file(self, path, content):
        """Helper to write a file, creating parent dirs if needed."""
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            f.write(content)

    # --- get_external_context ---

    def test_get_external_context_exists(self):
        """When external context.md exists, return its content."""
        context_content = "# My Project\n\nStatus: all features wired."
        self._write_file(
            os.path.join(self.context_dir, "projects", "test-project", "context.md"),
            context_content,
        )
        result = self.ci.get_external_context("test-project")
        self.assertEqual(result, context_content)

    def test_get_external_context_missing(self):
        """When external context.md doesn't exist, return empty string."""
        result = self.ci.get_external_context("nonexistent-project-xyz")
        self.assertEqual(result, "")

    def test_get_external_context_empty_name(self):
        """Empty project name returns empty string."""
        result = self.ci.get_external_context("")
        self.assertEqual(result, "")

    def test_get_external_context_no_context_dir(self):
        """When context_dir is empty, return empty string."""
        ci = ContextInjector(context_dir="", boi_dir=self.boi_dir)
        result = ci.get_external_context("test-project")
        self.assertEqual(result, "")

    # --- get_boi_project_context ---

    def test_get_boi_project_context(self):
        """Load BOI project context.md and research.md."""
        self._write_file(
            os.path.join(self.boi_dir, "projects", "test-project", "context.md"),
            "BOI context content",
        )
        self._write_file(
            os.path.join(self.boi_dir, "projects", "test-project", "research.md"),
            "Research findings",
        )
        result = self.ci.get_boi_project_context("test-project")
        self.assertIn("BOI context content", result)
        self.assertIn("Research findings", result)

    def test_get_boi_project_context_only_context(self):
        """When only context.md exists (no research.md), return just context."""
        self._write_file(
            os.path.join(self.boi_dir, "projects", "test-project", "context.md"),
            "Only context here",
        )
        result = self.ci.get_boi_project_context("test-project")
        self.assertEqual(result, "Only context here")

    # --- build_context_block ---

    def test_build_context_block_combined(self):
        """Combine external + BOI context with proper headers."""
        self._write_file(
            os.path.join(self.context_dir, "projects", "test-project", "context.md"),
            "External context data",
        )
        self._write_file(
            os.path.join(self.boi_dir, "projects", "test-project", "context.md"),
            "BOI context data",
        )
        result = self.ci.build_context_block("test-project", "")
        self.assertIn("## Injected Context", result)
        self.assertIn("External context data", result)
        self.assertIn("BOI context data", result)
        # External should appear before BOI
        ext_pos = result.index("External context data")
        boi_pos = result.index("BOI context data")
        self.assertLess(ext_pos, boi_pos)

    def test_build_context_block_deduplication(self):
        """Don't include same content twice if external and BOI overlap."""
        same_content = "Identical context in both places"
        self._write_file(
            os.path.join(self.context_dir, "projects", "test-project", "context.md"),
            same_content,
        )
        self._write_file(
            os.path.join(self.boi_dir, "projects", "test-project", "context.md"),
            same_content,
        )
        result = self.ci.build_context_block("test-project", "")
        # Content should appear exactly once (in the external section)
        count = result.count(same_content)
        self.assertEqual(count, 1, f"Expected content once, found {count} times")

    def test_build_context_block_truncation(self):
        """Truncate if total exceeds 5000 chars."""
        large_content = "A" * 6000
        self._write_file(
            os.path.join(self.context_dir, "projects", "test-project", "context.md"),
            large_content,
        )
        result = self.ci.build_context_block("test-project", "")
        self.assertIn("## Injected Context", result)
        self.assertIn("truncated", result.lower())
        self.assertLess(len(result), 5500)

    def test_build_context_block_empty(self):
        """When no context sources exist, return empty string."""
        result = self.ci.build_context_block("nonexistent-project", "")
        self.assertEqual(result, "")

    def test_build_context_block_boi_only(self):
        """When only BOI context exists (no external dir), still works."""
        ci = ContextInjector(context_dir="", boi_dir=self.boi_dir)
        self._write_file(
            os.path.join(self.boi_dir, "projects", "test-project", "context.md"),
            "BOI only context",
        )
        result = ci.build_context_block("test-project", "")
        self.assertIn("## Injected Context", result)
        self.assertIn("BOI only context", result)

    # --- get_context_sources_from_spec ---

    def test_context_sources_parsing(self):
        """Parse ## Context Sources from spec content."""
        spec = """# My Spec

## Goal
Do something.

## Context Sources
- ~/projects/foo/context.md
- https://example.com/doc
- ~/some/other/file.md

## Approach
The approach is...
"""
        sources = self.ci.get_context_sources_from_spec(spec)
        self.assertEqual(len(sources), 3)
        self.assertEqual(sources[0], "~/projects/foo/context.md")
        self.assertEqual(sources[1], "https://example.com/doc")
        self.assertEqual(sources[2], "~/some/other/file.md")

    def test_context_sources_empty_spec(self):
        """Empty spec returns empty list."""
        self.assertEqual(self.ci.get_context_sources_from_spec(""), [])

    def test_context_sources_no_section(self):
        """Spec without Context Sources section returns empty list."""
        spec = "# Spec\n\n## Goal\nDo stuff.\n"
        self.assertEqual(self.ci.get_context_sources_from_spec(spec), [])

    # --- read_local_sources ---

    def test_read_local_sources(self):
        """Read local files from sources list, skip URLs."""
        local_file = os.path.join(self.temp_dir, "extra.md")
        self._write_file(local_file, "Extra context content")

        sources = [
            local_file,
            "https://example.com/skip-me",
            "http://also-skip.com",
        ]
        result = self.ci.read_local_sources(sources)
        self.assertIn("Extra context content", result)
        self.assertIn(f"### Source: {local_file}", result)
        self.assertNotIn("skip-me", result)

    def test_read_local_sources_directory(self):
        """Reading a directory source reads all .md files in it."""
        src_dir = os.path.join(self.temp_dir, "decisions")
        os.makedirs(src_dir)
        self._write_file(os.path.join(src_dir, "d1.md"), "Decision one")
        self._write_file(os.path.join(src_dir, "d2.md"), "Decision two")
        self._write_file(os.path.join(src_dir, "notes.txt"), "Not markdown")

        result = self.ci.read_local_sources([src_dir])
        self.assertIn("Decision one", result)
        self.assertIn("Decision two", result)
        self.assertNotIn("Not markdown", result)

    def test_read_local_sources_missing_file(self):
        """Missing files are silently skipped."""
        result = self.ci.read_local_sources(["/nonexistent/path/file.md"])
        self.assertEqual(result, "")

    # --- missing context dir ---

    def test_missing_context_dir(self):
        """Gracefully handle missing context directory."""
        ci = ContextInjector(
            context_dir=os.path.join(self.temp_dir, "no-such-dir"),
            boi_dir=self.boi_dir,
        )
        result = ci.get_external_context("test-project")
        self.assertEqual(result, "")
        block = ci.build_context_block("test-project", "")
        self.assertNotIn("error", block.lower())


class TestPreflightContext(unittest.TestCase):
    """Tests for preflight_context.py functions."""

    def setUp(self):
        self.temp_dir = tempfile.mkdtemp()

    def tearDown(self):
        shutil.rmtree(self.temp_dir)

    def _write_file(self, path, content):
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            f.write(content)

    def test_gather_with_context_dir(self):
        """gather_preflight_context reads from context_dir when provided."""
        from lib.preflight_context import gather_preflight_context

        ctx_dir = os.path.join(self.temp_dir, "ctx")
        self._write_file(
            os.path.join(ctx_dir, "projects", "myproj", "context.md"),
            "Project context here",
        )
        spec_path = os.path.join(self.temp_dir, "spec.md")
        self._write_file(spec_path, "# Spec\n\n## Tasks\n\n### t-1: Do it\nPENDING\n")

        result = gather_preflight_context(spec_path, "myproj", context_dir=ctx_dir)
        self.assertIn("## Preflight Context", result)
        self.assertIn("Project context here", result)

    def test_gather_without_context_dir(self):
        """gather_preflight_context works without context_dir."""
        from lib.preflight_context import gather_preflight_context

        spec_path = os.path.join(self.temp_dir, "spec.md")
        self._write_file(spec_path, "# Spec\n\n## Tasks\n\n### t-1: Do it\nPENDING\n")

        result = gather_preflight_context(spec_path, "myproj")
        self.assertEqual(result, "")

    def test_gather_with_context_sources(self):
        """gather_preflight_context reads spec-referenced sources."""
        from lib.preflight_context import gather_preflight_context

        source_file = os.path.join(self.temp_dir, "extra.md")
        self._write_file(source_file, "Extra info")

        spec_path = os.path.join(self.temp_dir, "spec.md")
        self._write_file(
            spec_path,
            f"# Spec\n\n## Context Sources\n- {source_file}\n\n## Tasks\n\n### t-1: Do it\nPENDING\n",
        )

        result = gather_preflight_context(spec_path, "myproj")
        self.assertIn("## Preflight Context", result)
        self.assertIn("Extra info", result)

    def test_gather_truncates_large_content(self):
        """gather_preflight_context truncates content over 8000 chars."""
        from lib.preflight_context import gather_preflight_context

        source_file = os.path.join(self.temp_dir, "big.md")
        self._write_file(source_file, "A" * 10000)

        spec_path = os.path.join(self.temp_dir, "spec.md")
        self._write_file(
            spec_path,
            f"# Spec\n\n## Context Sources\n- {source_file}\n\n## Tasks\n",
        )

        result = gather_preflight_context(spec_path, "")
        self.assertIn("truncated", result.lower())

    def test_gather_missing_spec(self):
        """gather_preflight_context handles missing spec file."""
        from lib.preflight_context import gather_preflight_context

        result = gather_preflight_context("/nonexistent/spec.md", "myproj")
        self.assertEqual(result, "")


if __name__ == "__main__":
    unittest.main()
