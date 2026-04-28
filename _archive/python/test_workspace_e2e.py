"""End-to-end test: workspace isolation via worktree and Docker staging.

Creates a temp git repo, a spec with **Workspace:** worktree,
validates it, sets up a worktree, makes changes, merges back.
Also tests in-place passthrough and Docker staging lifecycle.
"""

import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from lib.spec_validator import validate_spec

try:
    from worker import (
        _parse_workspace,
        _parse_target_repo,
        _setup_worktree,
        _merge_worktree,
        _cleanup_worktree,
    )
    _WORKER_HELPERS_AVAILABLE = True
except ImportError:
    _WORKER_HELPERS_AVAILABLE = False


def _init_temp_git_repo(path):
    subprocess.run(["git", "init", path], check=True, capture_output=True)
    readme = os.path.join(path, "README.md")
    with open(readme, "w") as f:
        f.write("# Test\n")
    subprocess.run(["git", "-C", path, "add", "."], check=True, capture_output=True)
    subprocess.run(
        ["git", "-C", path, "commit", "-m", "init"],
        check=True,
        capture_output=True,
        env={
            **os.environ,
            "GIT_AUTHOR_NAME": "test",
            "GIT_AUTHOR_EMAIL": "t@t",
            "GIT_COMMITTER_NAME": "test",
            "GIT_COMMITTER_EMAIL": "t@t",
        },
    )


@unittest.skipUnless(_WORKER_HELPERS_AVAILABLE, "worker workspace helpers not yet implemented")
class TestWorkspaceE2E(unittest.TestCase):

    def test_full_worktree_lifecycle(self):
        """Spec validation -> worktree create -> modify -> merge -> cleanup."""
        with tempfile.TemporaryDirectory() as tmpdir:
            repo_path = os.path.join(tmpdir, "my-project")
            os.makedirs(repo_path)
            _init_temp_git_repo(repo_path)

            spec = textwrap.dedent(f"""\
                # Refactor My Project

                **Mode:** execute
                **Workspace:** worktree
                **Target repo:** {repo_path}

                ### t-1: Add feature
                PENDING

                **Spec:** Create feature.py with a hello function.
                **Verify:** python3 -c "from feature import hello"
            """)

            # 1. Validate
            result = validate_spec(spec)
            self.assertTrue(result.valid, result.errors)
            self.assertEqual(result.workspace, "worktree")

            # 2. Parse
            self.assertEqual(_parse_workspace(spec), "worktree")
            self.assertEqual(_parse_target_repo(spec), repo_path)

            # 3. Create worktree
            wt_path = _setup_worktree(repo_path, "q-e2e", 1)
            self.assertTrue(os.path.isdir(wt_path))

            try:
                # 4. Make a change (simulating what Claude would do)
                feature_file = os.path.join(wt_path, "feature.py")
                with open(feature_file, "w") as f:
                    f.write("def hello():\n    return 'hello'\n")
                subprocess.run(
                    ["git", "-C", wt_path, "add", "."],
                    check=True,
                    capture_output=True,
                )
                subprocess.run(
                    ["git", "-C", wt_path, "commit", "-m", "feat: add feature"],
                    check=True,
                    capture_output=True,
                    env={
                        **os.environ,
                        "GIT_AUTHOR_NAME": "boi",
                        "GIT_AUTHOR_EMAIL": "boi@test",
                        "GIT_COMMITTER_NAME": "boi",
                        "GIT_COMMITTER_EMAIL": "boi@test",
                    },
                )

                # 5. Verify main repo is NOT modified yet
                self.assertFalse(
                    os.path.isfile(os.path.join(repo_path, "feature.py"))
                )

                # 6. Merge back
                merged = _merge_worktree(repo_path, wt_path)
                self.assertTrue(merged)

                # 7. Verify main repo IS modified now
                self.assertTrue(
                    os.path.isfile(os.path.join(repo_path, "feature.py"))
                )
            finally:
                _cleanup_worktree(repo_path, wt_path)

            # 8. Verify worktree is cleaned up
            self.assertFalse(os.path.isdir(wt_path))

    def test_in_place_spec_passes_validation(self):
        """In-place specs are valid and don't trigger worktree setup."""
        spec = textwrap.dedent("""\
            # Analyze Code

            **Mode:** execute
            **Workspace:** in-place

            ### t-1: Run analysis
            PENDING

            **Spec:** Count lines of code.
            **Verify:** echo ok
        """)
        result = validate_spec(spec)
        self.assertTrue(result.valid, result.errors)
        self.assertEqual(result.workspace, "in-place")
        self.assertEqual(_parse_workspace(spec), "in-place")
        self.assertIsNone(_parse_target_repo(spec))


def _docker_available():
    """Check if Docker is available."""
    try:
        return (
            subprocess.run(
                ["docker", "info"], capture_output=True
            ).returncode
            == 0
        )
    except FileNotFoundError:
        return False


@unittest.skipUnless(
    _WORKER_HELPERS_AVAILABLE and _docker_available(),
    "worker helpers or Docker not available",
)
class TestDockerWorkspaceE2E(unittest.TestCase):

    @classmethod
    def setUpClass(cls):
        """Build the boi-worker image if not present."""
        result = subprocess.run(
            ["docker", "image", "inspect", "boi-worker:latest"],
            capture_output=True,
        )
        if result.returncode != 0:
            docker_dir = os.path.join(
                os.path.dirname(os.path.dirname(__file__)), "docker"
            )
            subprocess.run(
                [
                    "docker",
                    "build",
                    "-t",
                    "boi-worker:latest",
                    "-f",
                    os.path.join(docker_dir, "Dockerfile"),
                    docker_dir,
                ],
                check=True,
            )

    def test_docker_staging_lifecycle(self):
        """Create staging -> modify -> extract -> cleanup."""
        from worker import (
            _setup_docker_staging,
            _extract_docker_changes,
            _cleanup_docker_staging,
        )

        with tempfile.TemporaryDirectory() as tmpdir:
            repo_path = os.path.join(tmpdir, "my-project")
            os.makedirs(repo_path)
            _init_temp_git_repo(repo_path)

            staging = _setup_docker_staging(repo_path, "q-docker", 1)
            try:
                # Simulate Claude making changes in staging
                feature = os.path.join(staging, "feature.py")
                with open(feature, "w") as f:
                    f.write("def hello(): return 'hi'\n")
                subprocess.run(
                    ["git", "-C", staging, "add", "."],
                    check=True,
                    capture_output=True,
                )
                subprocess.run(
                    ["git", "-C", staging, "commit", "-m", "add feature"],
                    check=True,
                    capture_output=True,
                    env={
                        **os.environ,
                        "GIT_AUTHOR_NAME": "boi",
                        "GIT_AUTHOR_EMAIL": "boi@test",
                        "GIT_COMMITTER_NAME": "boi",
                        "GIT_COMMITTER_EMAIL": "boi@test",
                    },
                )

                # Verify original is untouched
                self.assertFalse(
                    os.path.isfile(os.path.join(repo_path, "feature.py"))
                )

                # Extract changes
                applied = _extract_docker_changes(staging, repo_path)
                self.assertTrue(applied)

                # Verify original now has the change
                self.assertTrue(
                    os.path.isfile(os.path.join(repo_path, "feature.py"))
                )
            finally:
                _cleanup_docker_staging(staging)


if __name__ == "__main__":
    unittest.main()
