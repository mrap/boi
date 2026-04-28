"""Gate: scan git diff for API keys, tokens, and passwords."""

from __future__ import annotations

import re
import subprocess

from .context import HookContext, HookResult

# Patterns for common secret formats
SECRET_PATTERNS = [
    (r"(?i)api[_-]?key\s*[:=]\s*['\"]?[A-Za-z0-9_\-]{20,}", "API key"),
    (r"(?i)secret[_-]?key\s*[:=]\s*['\"]?[A-Za-z0-9_\-]{20,}", "secret key"),
    (r"(?i)password\s*[:=]\s*['\"]?[^\s'\"]{8,}", "password"),
    (r"(?i)token\s*[:=]\s*['\"]?[A-Za-z0-9_\-\.]{20,}", "token"),
    (r"sk-[A-Za-z0-9]{32,}", "OpenAI API key"),
    (r"xoxb-[0-9]+-[A-Za-z0-9]+", "Slack bot token"),
    (r"xoxp-[0-9]+-[A-Za-z0-9]+", "Slack user token"),
    (r"ghp_[A-Za-z0-9]{36}", "GitHub personal access token"),
    (r"gho_[A-Za-z0-9]{36}", "GitHub OAuth token"),
    (r"AIza[0-9A-Za-z\-_]{35}", "Google API key"),
    (r"AKIA[0-9A-Z]{16}", "AWS access key"),
    (r"(?i)aws[_-]?secret[_-]?access[_-]?key\s*[:=]\s*[A-Za-z0-9+/]{40}", "AWS secret key"),
    (r"-----BEGIN (?:RSA |EC |DSA )?PRIVATE KEY-----", "private key"),
]


def run(ctx: HookContext, config: dict) -> HookResult:
    """Scan the git diff for secrets."""
    cwd = ctx.repo_dir or "."
    try:
        result = subprocess.run(
            ["git", "diff", "HEAD"],
            capture_output=True,
            text=True,
            timeout=30,
            cwd=cwd,
        )
        diff = result.stdout
    except (subprocess.TimeoutExpired, FileNotFoundError) as e:
        return HookResult(passed=False, message=f"Could not get git diff: {e}")

    if not diff:
        # Try staged diff
        try:
            result = subprocess.run(
                ["git", "diff", "--cached"],
                capture_output=True,
                text=True,
                timeout=30,
                cwd=cwd,
            )
            diff = result.stdout
        except (subprocess.TimeoutExpired, FileNotFoundError):
            pass

    findings: list[str] = []
    for pattern, label in SECRET_PATTERNS:
        matches = re.findall(pattern, diff)
        if matches:
            findings.append(f"{label}: {len(matches)} match(es)")

    if findings:
        return HookResult(
            passed=False,
            message="Potential secrets detected in diff",
            details="\n".join(findings),
        )
    return HookResult(passed=True, message="No secrets detected")
