"""Gate: run **Verify:** commands from the spec and check they pass."""

from __future__ import annotations

import re
import subprocess

from .context import HookContext, HookResult


def run(ctx: HookContext, config: dict) -> HookResult:
    """Parse **Verify:** block from spec and run commands."""
    spec = ctx.spec_content
    match = re.search(r"\*\*Verify:\*\*\s*```(?:bash)?\s*\n(.*?)\n```", spec, re.DOTALL)
    if not match:
        return HookResult(passed=True, message="No verify commands found")

    commands = match.group(1).strip()
    if not commands:
        return HookResult(passed=True, message="Verify block is empty")

    timeout = config.get("timeout", 60)
    cwd = ctx.repo_dir or None

    try:
        result = subprocess.run(
            ["bash", "-c", commands],
            capture_output=True,
            text=True,
            timeout=timeout,
            cwd=cwd,
        )
    except subprocess.TimeoutExpired:
        return HookResult(passed=False, message="Verify commands timed out", details=f"timeout={timeout}s")

    if result.returncode == 0:
        return HookResult(passed=True, message="Verify commands passed", details=result.stdout)
    else:
        return HookResult(
            passed=False,
            message="Verify commands failed",
            details=f"exit={result.returncode}\nstdout={result.stdout}\nstderr={result.stderr}",
        )
