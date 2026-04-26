"""Characterization test — ensures verify instruction stays in worker prompt template.

This test guards against accidental removal of the MUST run/Verify instruction
that was added in t-3 of the completion-fraud fix spec.
"""

import os
import sys
import tempfile
import textwrap
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from worker import TEMPLATE_PATH, Worker


# ---------------------------------------------------------------------------
# Helper
# ---------------------------------------------------------------------------

MINIMAL_SPEC = textwrap.dedent("""\
    # Test Spec

    ## Tasks

    ### t-1: Only task
    PENDING

    **Spec:** Do something.

    **Verify:** echo ok
""")


def _make_worker(spec_path: str, state_dir: str) -> Worker:
    return Worker(
        spec_path=spec_path,
        spec_id="q-test",
        worktree="/tmp",
        project="test",
        phase="execute",
        iteration=1,
        state_dir=state_dir,
    )


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


class TestVerifyInstructionInTemplate:
    """Ensure the worker prompt template contains the verify instruction."""

    def test_template_file_exists(self):
        assert os.path.isfile(TEMPLATE_PATH), (
            f"Worker prompt template not found at {TEMPLATE_PATH}"
        )

    def test_template_contains_must_run(self):
        with open(TEMPLATE_PATH) as f:
            content = f.read()
        assert "MUST run" in content, (
            "Worker prompt template missing 'MUST run' — verify instruction was removed"
        )

    def test_template_contains_verify_keyword(self):
        with open(TEMPLATE_PATH) as f:
            content = f.read()
        assert "Verify" in content, (
            "Worker prompt template missing 'Verify' keyword"
        )

    def test_template_contains_before_marking_done(self):
        with open(TEMPLATE_PATH) as f:
            content = f.read()
        lower = content.lower()
        assert "before marking" in lower or "before mark" in lower, (
            "Worker prompt template missing 'before marking' phrase"
        )

    def test_template_contains_done_reference(self):
        with open(TEMPLATE_PATH) as f:
            content = f.read()
        assert "DONE" in content, (
            "Worker prompt template missing 'DONE' reference in verify instruction"
        )


class TestVerifyInstructionInGeneratedPrompt:
    """Ensure the generated prompt (from Worker) carries the verify instruction."""

    def test_generated_prompt_contains_verify_instruction(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            spec_path = os.path.join(tmpdir, "q-test.spec.md")
            with open(spec_path, "w") as f:
                f.write(MINIMAL_SPEC)

            os.makedirs(os.path.join(tmpdir, "queue"), exist_ok=True)
            worker = _make_worker(spec_path, tmpdir)

            # generate_run_script writes {state_dir}/queue/{spec_id}.prompt.md
            # Stub out _generate_bash_run_script so we don't need tmux/claude
            with patch.object(worker, "_generate_bash_run_script"):
                worker.generate_run_script(MINIMAL_SPEC)

            prompt_path = os.path.join(tmpdir, "queue", "q-test.prompt.md")
            assert os.path.isfile(prompt_path), (
                "generate_run_script() did not write a prompt file"
            )

            with open(prompt_path) as f:
                prompt = f.read()

            assert "MUST run" in prompt, (
                "Generated worker prompt missing 'MUST run' verify instruction"
            )
            assert "Verify" in prompt, (
                "Generated worker prompt missing 'Verify' keyword"
            )
