"""Gate: run configurable lint command and verify it passes."""

from __future__ import annotations

import subprocess

from .context import HookContext, HookResult


def run(ctx: HookContext, config: dict) -> HookResult:
    """Run the lint command specified in config."""
    command = config.get("command", "python3 -m flake8 .")
    timeout = config.get("timeout", 60)
    cwd = ctx.repo_dir or "."

    try:
        result = subprocess.run(
            ["bash", "-c", command],
            capture_output=True,
            text=True,
            timeout=timeout,
            cwd=cwd,
        )
    except subprocess.TimeoutExpired:
        return HookResult(passed=False, message="Lint timed out", details=f"timeout={timeout}s")

    if result.returncode == 0:
        return HookResult(passed=True, message="Lint passed")
    else:
        return HookResult(
            passed=False,
            message="Lint failed",
            details=f"exit={result.returncode}\n{result.stdout}\n{result.stderr}",
        )
