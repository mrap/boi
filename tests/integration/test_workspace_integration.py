# tests/integration/test_workspace_integration.py — Integration tests for
# the workspace boundary guard.
#
# Tests the full WorkspaceBoundaryChecker flow against real temp git repos.
# Does NOT require hex-events to be installed — emit calls are mocked.
#
# Pytest filter: -k 'integration and workspace'

import os
import subprocess
import sys
import tempfile
import shutil
import unittest
from pathlib import Path
from unittest.mock import call, patch, MagicMock

BOI_ROOT = str(Path(__file__).resolve().parent.parent.parent)
sys.path.insert(0, BOI_ROOT)

from lib.workspace_guard import (
    WorkspaceBoundaryChecker,
    emit_leak_event,
    get_main_repo,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _git(*args, cwd: str) -> None:
    """Run a git command, raising subprocess.CalledProcessError on failure."""
    subprocess.run(
        ["git"] + list(args),
        cwd=cwd,
        check=True,
        capture_output=True,
    )


def _init_repo(path: str) -> None:
    """Initialise a minimal git repo with one commit at *path*."""
    os.makedirs(path, exist_ok=True)
    _git("init", cwd=path)
    _git("config", "user.email", "test@boi.test", cwd=path)
    _git("config", "user.name", "BOI Test", cwd=path)
    readme = os.path.join(path, "README.md")
    with open(readme, "w") as fh:
        fh.write("integration test repo\n")
    _git("add", "README.md", cwd=path)
    _git("commit", "-m", "init", cwd=path)


def _add_worktree(main_repo: str, wt_path: str) -> None:
    """Create a linked worktree at *wt_path* from *main_repo*."""
    _git("worktree", "add", wt_path, cwd=main_repo)


def _write(path: str, filename: str, content: str = "data\n") -> str:
    """Write *content* to *filename* inside *path*; return full file path."""
    full = os.path.join(path, filename)
    with open(full, "w") as fh:
        fh.write(content)
    return full


# ---------------------------------------------------------------------------
# Fixtures via setUp/tearDown
# ---------------------------------------------------------------------------

class WorkspaceIntegrationBase(unittest.TestCase):
    """Base class that provides a main repo + linked worktree in a tmp dir."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp(prefix="boi_wsi_")
        self.main_repo = os.path.join(self.tmpdir, "main_repo")
        self.worktree = os.path.join(self.tmpdir, "linked_wt")
        _init_repo(self.main_repo)
        _add_worktree(self.main_repo, self.worktree)

    def tearDown(self):
        try:
            _git(
                "worktree", "remove", "--force", self.worktree,
                cwd=self.main_repo,
            )
        except Exception:
            pass
        shutil.rmtree(self.tmpdir, ignore_errors=True)


# ---------------------------------------------------------------------------
# Integration tests
# ---------------------------------------------------------------------------

class TestWorkspaceIntegrationLeakDetected(WorkspaceIntegrationBase):
    """Worker writes to main repo → boundary check detects leak."""

    def test_integration_workspace_leak_detected(self):
        """Full flow: worker leaks file to main repo, event emitted with correct payload."""
        checker = WorkspaceBoundaryChecker(
            worktree_path=self.worktree,
            spec_id="q-integration-001",
            worker_id="w-int-1",
        )
        checker.snapshot_before()

        # Simulate worker writing to MAIN repo (boundary violation)
        _write(self.main_repo, "leaked_output.txt", "this should not be here\n")

        captured_calls = []

        def fake_emit(spec_id, worker_id, leaked_files, worktree_path):
            captured_calls.append({
                "spec_id": spec_id,
                "worker_id": worker_id,
                "leaked_files": leaked_files,
                "worktree_path": worktree_path,
            })

        with patch("lib.workspace_guard.emit_leak_event", side_effect=fake_emit):
            leaked = checker.check_after()

        # Boundary violation must be detected
        self.assertGreater(len(leaked), 0, "Expected leak to be detected")
        self.assertTrue(
            any("leaked_output.txt" in f for f in leaked),
            f"Expected leaked_output.txt in {leaked}",
        )

        # Event was emitted with correct payload
        self.assertEqual(len(captured_calls), 1)
        evt = captured_calls[0]
        self.assertEqual(evt["spec_id"], "q-integration-001")
        self.assertEqual(evt["worker_id"], "w-int-1")
        self.assertEqual(evt["worktree_path"], self.worktree)
        self.assertTrue(
            any("leaked_output.txt" in f for f in evt["leaked_files"]),
            f"Leaked file list incorrect: {evt['leaked_files']}",
        )

    def test_integration_workspace_leak_payload_file_list(self):
        """Leak event payload contains ALL files that leaked, not just the first."""
        checker = WorkspaceBoundaryChecker(
            worktree_path=self.worktree,
            spec_id="q-integration-002",
            worker_id="w-int-2",
        )
        checker.snapshot_before()

        # Write multiple files to main repo
        _write(self.main_repo, "leak_a.py", "# leaked\n")
        _write(self.main_repo, "leak_b.txt", "leaked\n")

        captured = {}

        def capture_emit(spec_id, worker_id, leaked_files, worktree_path):
            captured["leaked_files"] = list(leaked_files)

        with patch("lib.workspace_guard.emit_leak_event", side_effect=capture_emit):
            leaked = checker.check_after()

        self.assertEqual(len(leaked), 2, f"Expected 2 leaked files, got {leaked}")
        self.assertIn("leaked_files", captured)
        self.assertEqual(len(captured["leaked_files"]), 2)
        names = [os.path.basename(f) for f in captured["leaked_files"]]
        self.assertIn("leak_a.py", names)
        self.assertIn("leak_b.txt", names)


class TestWorkspaceIntegrationCleanWorker(WorkspaceIntegrationBase):
    """Worker writes only to worktree → no leak event emitted."""

    def test_integration_workspace_clean_worker_no_leak(self):
        """Full flow: worker writes only to worktree, no event emitted."""
        checker = WorkspaceBoundaryChecker(
            worktree_path=self.worktree,
            spec_id="q-integration-003",
            worker_id="w-int-3",
        )
        checker.snapshot_before()

        # Worker writes ONLY to its worktree (legitimate)
        _write(self.worktree, "output.txt", "legitimate result\n")
        _write(self.worktree, "report.md", "# Report\n")

        with patch("lib.workspace_guard.emit_leak_event") as mock_emit:
            leaked = checker.check_after()

        self.assertEqual(leaked, [], f"Expected no leaks, got {leaked}")
        mock_emit.assert_not_called()

    def test_integration_workspace_no_hex_events_graceful(self):
        """Emit is skipped gracefully when hex-events is not installed."""
        checker = WorkspaceBoundaryChecker(
            worktree_path=self.worktree,
            spec_id="q-integration-004",
            worker_id="w-int-4",
        )
        checker.snapshot_before()

        # Simulate leak
        _write(self.main_repo, "leak.py", "oops\n")

        # With hex_emit.py absent (patched to nonexistent path), no exception
        with patch("lib.workspace_guard.HEX_EMIT_PATH", "/nonexistent/hex_emit.py"):
            try:
                leaked = checker.check_after()
            except Exception as exc:
                self.fail(f"check_after raised unexpectedly: {exc}")

        self.assertGreater(len(leaked), 0, "Leak should still be detected")

    def test_integration_workspace_get_main_repo_from_worktree(self):
        """get_main_repo() resolves correctly from a linked worktree."""
        resolved = get_main_repo(self.worktree)
        self.assertIsNotNone(resolved)
        self.assertEqual(
            os.path.realpath(resolved),
            os.path.realpath(self.main_repo),
        )


if __name__ == "__main__":
    unittest.main()
