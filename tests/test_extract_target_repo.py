# test_extract_target_repo.py — Tests for Daemon._extract_target_repo
#
# RED test: proves _extract_target_repo returns "" for "**Target repo:**" format.
# These tests FAIL before the fix (t-2) and PASS after.

import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from daemon import Daemon


class TestExtractTargetRepo(unittest.TestCase):
    """Tests for _extract_target_repo static method."""

    def _write_spec(self, content: str) -> str:
        """Write content to a temp spec file, return path."""
        f = tempfile.NamedTemporaryFile(
            mode="w", suffix=".spec.md", delete=False, encoding="utf-8"
        )
        f.write(content)
        f.close()
        self.addCleanup(os.unlink, f.name)
        return f.name

    def test_target_repo_format(self):
        """**Target repo:** format (141/216 specs) should return the path."""
        spec_path = self._write_spec(
            "# Test Spec\n\n**Target repo:** /Users/mrap/mrap-hex\n"
        )
        result = Daemon._extract_target_repo(spec_path)
        self.assertEqual(result, "/Users/mrap/mrap-hex")

    def test_target_repo_tilde_expansion(self):
        """**Target repo:** ~/mrap-hex should return expanded path."""
        spec_path = self._write_spec(
            "# Test Spec\n\n**Target repo:** ~/mrap-hex\n"
        )
        result = Daemon._extract_target_repo(spec_path)
        expected = str(Path("~/mrap-hex").expanduser())
        self.assertEqual(result, expected)

    def test_target_repo_backtick_wrapped(self):
        """**Target repo:** `/Users/mrap/mrap-hex` (backtick-wrapped) should strip backticks."""
        spec_path = self._write_spec(
            "# Test Spec\n\n**Target repo:** `/Users/mrap/mrap-hex`\n"
        )
        result = Daemon._extract_target_repo(spec_path)
        self.assertEqual(result, "/Users/mrap/mrap-hex")

    def test_legacy_target_format_still_works(self):
        """**Target:** /Users/mrap/mrap-hex (legacy format) should still work."""
        spec_path = self._write_spec(
            "# Test Spec\n\n**Target:** /Users/mrap/mrap-hex\n"
        )
        result = Daemon._extract_target_repo(spec_path)
        self.assertEqual(result, "/Users/mrap/mrap-hex")

    def test_no_target_returns_empty(self):
        """Spec with no Target field should return empty string."""
        spec_path = self._write_spec(
            "# Test Spec\n\n## Tasks\n\n### t-1: Do something\nPENDING\n"
        )
        result = Daemon._extract_target_repo(spec_path)
        self.assertEqual(result, "")

    def test_missing_file_returns_empty(self):
        """Non-existent spec file should return empty string."""
        result = Daemon._extract_target_repo("/nonexistent/path/spec.md")
        self.assertEqual(result, "")


if __name__ == "__main__":
    unittest.main()
