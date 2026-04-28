"""Gate: check that git diff is non-empty (work was actually done)."""

from __future__ import annotations

import subprocess

from .context import HookContext, HookResult


def run(ctx: HookContext, config: dict) -> HookResult:
    """Return passed=True if the working tree has changes."""
    cwd = ctx.repo_dir or "."
    try:
        result = subprocess.run(
            ["git", "diff", "--stat", "HEAD"],
            capture_output=True,
            text=True,
            timeout=30,
            cwd=cwd,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError) as e:
        return HookResult(passed=False, message=f"git diff failed: {e}")

    diff_output = result.stdout.strip()
    if diff_output:
        return HookResult(passed=True, message="Diff is non-empty", details=diff_output)
    # Also check staged changes
    try:
        staged = subprocess.run(
            ["git", "diff", "--cached", "--stat"],
            capture_output=True,
            text=True,
            timeout=30,
            cwd=cwd,
        )
        if staged.stdout.strip():
            return HookResult(passed=True, message="Staged diff is non-empty", details=staged.stdout.strip())
    except (subprocess.TimeoutExpired, FileNotFoundError):
        pass

    return HookResult(passed=False, message="No changes detected in git diff")
