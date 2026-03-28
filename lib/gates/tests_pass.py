"""Gate: run configurable test command and verify it passes."""

from __future__ import annotations

import subprocess

from .context import HookContext, HookResult


def run(ctx: HookContext, config: dict) -> HookResult:
    """Run the test command specified in config (default: pytest)."""
    command = config.get("command", "python3 -m pytest tests/ -x --tb=short -q")
    timeout = config.get("timeout", 120)
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
        return HookResult(passed=False, message="Tests timed out", details=f"timeout={timeout}s")

    if result.returncode == 0:
        return HookResult(passed=True, message="Tests passed", details=result.stdout[-2000:])
    else:
        return HookResult(
            passed=False,
            message="Tests failed",
            details=f"exit={result.returncode}\n{result.stdout[-1000:]}\n{result.stderr[-500:]}",
        )
