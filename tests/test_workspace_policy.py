# test_workspace_policy.py — Tests for workspace policy validation in spec_validator.py
#
# Tests:
#   - in-place with git repo target produces warning
#   - in-place with Workspace-Justification suppresses warning
#   - worktree/docker produce no warning
#   - missing Workspace defaults to worktree (no warning)
#   - check_workspace_policy function directly
#   - validate_spec integrates workspace warnings

import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

BOI_ROOT = str(Path(__file__).resolve().parent.parent)
sys.path.insert(0, BOI_ROOT)

from lib.spec_validator import (
    check_workspace_policy,
    validate_spec,
)


def _make_git_repo(tmpdir: str) -> str:
    """Create a minimal git repo in tmpdir and return its path."""
    repo = os.path.join(tmpdir, "repo")
    os.makedirs(repo)
    subprocess.run(["git", "init", repo], capture_output=True, check=True)
    return repo


def _make_spec(workspace: str, target: str, justification: str = "") -> str:
    """Build a minimal valid BOI spec string with given header fields."""
    lines = [
        "# Test Spec",
        "",
        f"**Workspace:** {workspace}",
        f"**Target:** {target}",
    ]
    if justification:
        lines.append(f"**Workspace-Justification:** {justification}")
    lines += [
        "",
        "## Tasks",
        "",
        "### t-1: Do a thing",
        "PENDING",
        "",
        "**Spec:** Do the thing.",
        "",
        "**Verify:** echo ok",
    ]
    return "\n".join(lines)


class TestCheckWorkspacePolicy(unittest.TestCase):
    """Unit tests for check_workspace_policy() standalone function."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="boi-ws-test-")

    def tearDown(self):
        import shutil
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_inplace_git_repo_produces_warning(self):
        repo = _make_git_repo(self.tmp)
        spec = _make_spec("in-place", repo)
        warnings = check_workspace_policy(spec)
        self.assertTrue(
            any("in-place workspace targeting git repo" in w for w in warnings),
            f"Expected warning, got: {warnings}",
        )

    def test_inplace_git_repo_warning_contains_target(self):
        repo = _make_git_repo(self.tmp)
        spec = _make_spec("in-place", repo)
        warnings = check_workspace_policy(spec)
        self.assertTrue(any(repo in w for w in warnings))

    def test_inplace_with_justification_suppresses_warning(self):
        repo = _make_git_repo(self.tmp)
        spec = _make_spec("in-place", repo, justification="New standalone project")
        warnings = check_workspace_policy(spec)
        self.assertFalse(
            any("in-place workspace targeting git repo" in w for w in warnings),
            f"Expected no warning, got: {warnings}",
        )

    def test_worktree_produces_no_warning(self):
        repo = _make_git_repo(self.tmp)
        spec = _make_spec("worktree", repo)
        warnings = check_workspace_policy(spec)
        self.assertEqual(warnings, [])

    def test_docker_produces_no_warning(self):
        repo = _make_git_repo(self.tmp)
        spec = _make_spec("docker", repo)
        warnings = check_workspace_policy(spec)
        self.assertEqual(warnings, [])

    def test_missing_workspace_defaults_to_worktree_no_warning(self):
        """Spec without Workspace: field should not warn (treated as worktree)."""
        spec = textwrap.dedent("""\
            # Test Spec

            **Target:** /some/path

            ### t-1: Do it
            PENDING

            **Spec:** Work.
            **Verify:** echo ok
        """)
        warnings = check_workspace_policy(spec)
        self.assertEqual(warnings, [])

    def test_inplace_nonexistent_target_no_warning(self):
        """Non-existent target path should not warn (can't confirm it's a git repo)."""
        spec = _make_spec("in-place", "/nonexistent/path/that/does/not/exist")
        warnings = check_workspace_policy(spec)
        self.assertEqual(warnings, [])

    def test_inplace_non_git_dir_no_warning(self):
        """in-place targeting a plain directory (not git) should not warn."""
        plain_dir = os.path.join(self.tmp, "plain")
        os.makedirs(plain_dir)
        spec = _make_spec("in-place", plain_dir)
        warnings = check_workspace_policy(spec)
        self.assertEqual(warnings, [])

    def test_inplace_no_target_no_warning(self):
        """in-place with no Target field should not warn."""
        spec = textwrap.dedent("""\
            # Test Spec

            **Workspace:** in-place

            ### t-1: Do it
            PENDING

            **Spec:** Work.
            **Verify:** echo ok
        """)
        warnings = check_workspace_policy(spec)
        self.assertEqual(warnings, [])


class TestValidateSpecWorkspaceIntegration(unittest.TestCase):
    """Tests that validate_spec() integrates workspace policy warnings."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="boi-ws-test-")

    def tearDown(self):
        import shutil
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_validate_spec_inplace_git_repo_adds_warning(self):
        repo = _make_git_repo(self.tmp)
        spec = _make_spec("in-place", repo)
        result = validate_spec(spec)
        # Spec is still valid (warnings don't block dispatch)
        self.assertTrue(result.valid, result.errors)
        self.assertTrue(
            any("in-place workspace targeting git repo" in w for w in result.warnings),
            f"Expected workspace warning in result.warnings, got: {result.warnings}",
        )

    def test_validate_spec_worktree_no_workspace_warning(self):
        repo = _make_git_repo(self.tmp)
        spec = _make_spec("worktree", repo)
        result = validate_spec(spec)
        self.assertTrue(result.valid, result.errors)
        self.assertFalse(
            any("in-place workspace targeting git repo" in w for w in result.warnings)
        )

    def test_validate_spec_inplace_with_justification_no_warning(self):
        repo = _make_git_repo(self.tmp)
        spec = _make_spec("in-place", repo, justification="Read-only assessment")
        result = validate_spec(spec)
        self.assertTrue(result.valid, result.errors)
        self.assertFalse(
            any("in-place workspace targeting git repo" in w for w in result.warnings)
        )

    def test_validate_spec_inplace_git_repo_is_still_valid(self):
        """Workspace warnings must never make the spec invalid."""
        repo = _make_git_repo(self.tmp)
        spec = _make_spec("in-place", repo)
        result = validate_spec(spec)
        self.assertTrue(result.valid)
        self.assertEqual(result.errors, [])


if __name__ == "__main__":
    unittest.main()
