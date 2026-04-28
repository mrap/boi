#!/usr/bin/env python3
"""BOI Critic Evaluation Harness.

Runs the critic's checks against adversarial test specs and scores
how well the critic catches known issues vs avoids false positives.

This harness tests the prompt generation and check logic. It does NOT
invoke Claude. Instead, it implements a lightweight pattern-matching
evaluator that approximates what each check definition should catch.

Usage:
    cd ~/boi && python3 tests/eval_critic.py
"""

import json
import os
import re
import sys
import tempfile
from pathlib import Path

# Add project root to path
BOI_DIR = str(Path(__file__).resolve().parent.parent)
sys.path.insert(0, BOI_DIR)

from lib.critic import generate_critic_prompt, run_critic
from lib.critic_config import DEFAULT_CONFIG, get_active_checks, load_critic_config

FIXTURES_DIR = os.path.join(BOI_DIR, "tests", "fixtures", "critic-eval")
CHECKS_DIR = os.path.join(BOI_DIR, "templates", "checks")


# ─── Expected issues per test spec ──────────────────────────────────────────

EXPECTED_ISSUES = {
    "malformed-tasks": {
        "check": "spec-integrity",
        "min_issues": 3,
        "descriptions": [
            "t-2 heading missing colon separator (### t-2 instead of ### t-2:)",
            "t-4 uses wrong heading level (## instead of ###)",
            "t-3 is DONE but was described as reverted/should be PENDING (regression)",
        ],
    },
    "fake-verify": {
        "check": "verify-commands",
        "min_issues": 2,
        "descriptions": [
            "Verify commands use trivially passing commands (true, echo ok)",
            "Multiple DONE tasks lack meaningful verification",
        ],
    },
    "unbounded-growth": {
        "check": "fleet-readiness",
        "min_issues": 2,
        "descriptions": [
            "In-memory buffer grows unbounded (self.buffer.append with no limit)",
            "Daily JSONL files accumulate with no retention/rotation policy",
            "Background aggregator process spawned without cleanup on shutdown",
            "Dashboard loads ALL historical metrics into memory without windowing",
        ],
    },
    "silent-failures": {
        "check": "code-quality",
        "min_issues": 2,
        "descriptions": [
            "Bare except: pass blocks swallow all errors silently",
            "validate_config always returns True, defeating its purpose",
            "Rollback failure leaves user with broken config and no error message",
        ],
    },
    "incomplete-spec": {
        "check": "completeness",
        "min_issues": 2,
        "descriptions": [
            "t-3 has heading but no status line (silently dropped)",
            "t-5 is SKIPPED without a reason/explanation",
        ],
    },
    "perfect-spec": {
        "check": None,
        "min_issues": 0,
        "descriptions": [],
    },
}


# ─── Lightweight check evaluators ───────────────────────────────────────────
#
# Each evaluator reads the spec content and returns a list of found issues.
# These approximate what the Claude critic should catch by pattern-matching
# against the check definition criteria.


def eval_spec_integrity(content: str) -> list[dict]:
    """Check spec-integrity issues: malformed headings, missing status, regressions."""
    issues = []

    # Check: All tasks use ### t-N: format
    # Find task-like headings that are malformed
    lines = content.split("\n")
    for i, line in enumerate(lines):
        # Heading that looks like a task but wrong format
        if re.match(r"^#{2,4}\s+t-\d+", line):
            # Check for correct format: ### t-N: Title
            if not re.match(r"^### t-\d+:", line):
                if re.match(r"^## t-\d+", line):
                    issues.append(
                        {
                            "check": "spec-integrity",
                            "severity": "HIGH",
                            "description": (
                                "Task heading uses wrong level (## instead of ###): "
                                f"'{line.strip()}' on line {i + 1}"
                            ),
                        }
                    )
                elif re.match(r"^### t-\d+[^:]", line):
                    issues.append(
                        {
                            "check": "spec-integrity",
                            "severity": "HIGH",
                            "description": (
                                "Task heading missing colon separator: "
                                f"'{line.strip()}' on line {i + 1}"
                            ),
                        }
                    )

    # Check: Status line immediately after heading
    task_headings = []
    for i, line in enumerate(lines):
        if re.match(r"^#{2,4}\s+t-\d+", line):
            task_headings.append((i, line.strip()))

    for idx, (line_num, heading) in enumerate(task_headings):
        # Look at lines after heading for status
        found_status = False
        for j in range(line_num + 1, min(line_num + 3, len(lines))):
            stripped = lines[j].strip()
            if stripped in ("DONE", "PENDING", "SKIPPED"):
                found_status = True
                break
            if stripped and not stripped.startswith("#"):
                break  # Non-empty non-heading line before status
        if not found_status:
            issues.append(
                {
                    "check": "spec-integrity",
                    "severity": "HIGH",
                    "description": (
                        f"Task '{heading}' has no status line (DONE/PENDING/SKIPPED) "
                        f"after heading on line {line_num + 1}"
                    ),
                }
            )

    # Check: Task status contradicts description (regression detection)
    # Catches both DONE tasks described as reverted AND PENDING tasks described
    # as "previously done" or "was done but reverted"
    for i, line in enumerate(lines):
        if re.match(r"^#{2,4}\s+t-\d+", line):
            # Find status
            task_status = None
            status_line = 0
            for j in range(i + 1, min(i + 3, len(lines))):
                stripped = lines[j].strip()
                if stripped in ("DONE", "PENDING", "SKIPPED"):
                    task_status = stripped
                    status_line = j
                    break
                if stripped and not stripped.startswith("#"):
                    break

            if task_status is None:
                continue

            # Collect task body text
            task_text = ""
            for k in range(status_line + 1, len(lines)):
                if re.match(r"^#{2,4}\s+t-\d+", lines[k]):
                    break
                task_text += lines[k] + "\n"

            regression_pattern = re.compile(
                r"(previously.*done|was done|revert|regress|bug was found|"
                r"rolled back|backed out|was completed)",
                re.IGNORECASE,
            )

            if task_status == "DONE" and regression_pattern.search(task_text):
                issues.append(
                    {
                        "check": "spec-integrity",
                        "severity": "HIGH",
                        "description": (
                            f"Task '{line.strip()}' is marked DONE but its description "
                            "mentions reverting/regression, suggesting it should be PENDING"
                        ),
                    }
                )
            elif task_status == "PENDING" and regression_pattern.search(task_text):
                issues.append(
                    {
                        "check": "spec-integrity",
                        "severity": "HIGH",
                        "description": (
                            f"Task '{line.strip()}' is PENDING and its description mentions "
                            "prior completion that was reverted. This is a regression that "
                            "the critic should flag."
                        ),
                    }
                )

    return issues


def eval_verify_commands(content: str) -> list[dict]:
    """Check verify-commands issues: trivial verify commands."""
    issues = []

    # Parse tasks and their verify sections
    tasks = _parse_tasks(content)

    trivial_patterns = [
        r"^\s*`?true`?\s*$",
        r'^\s*`?echo\s+"?ok"?`?\s*$',
        r'^\s*`?echo\s+"?[^"]*"?\s*`?\s*$',
        r"^\s*`?exit\s+0`?\s*$",
    ]

    trivial_compound = re.compile(
        r'echo\s+"[^"]*"\s*&&\s*true|echo\s+"[^"]*"\s*\|\|\s*true'
    )

    for task in tasks:
        if task["status"] != "DONE":
            continue
        verify = task.get("verify", "")
        if not verify.strip():
            issues.append(
                {
                    "check": "verify-commands",
                    "severity": "HIGH",
                    "description": (
                        f"Task '{task['heading']}' is DONE but has no verify section"
                    ),
                }
            )
            continue

        # Check each line/command in verify
        verify_lines = verify.strip().split("\n")
        # Also check inline backtick commands
        inline_cmds = re.findall(r"`([^`]+)`", verify)
        all_cmds = verify_lines + inline_cmds

        is_trivial = True
        for cmd in all_cmds:
            cmd_clean = cmd.strip().strip("`")
            if not cmd_clean:
                continue
            trivial = False
            for pat in trivial_patterns:
                if re.match(pat, cmd_clean):
                    trivial = True
                    break
            if trivial_compound.search(cmd_clean):
                trivial = True
            if not trivial and cmd_clean not in ("", "true"):
                is_trivial = False
                break

        if is_trivial:
            issues.append(
                {
                    "check": "verify-commands",
                    "severity": "HIGH",
                    "description": (
                        f"Task '{task['heading']}' has only trivially passing verify "
                        "commands (e.g., 'true', 'echo ok'). Verification proves nothing."
                    ),
                }
            )

    return issues


def eval_fleet_readiness(content: str) -> list[dict]:
    """Check fleet-readiness issues: unbounded growth, no cleanup."""
    issues = []

    # Look for unbounded growth patterns
    # Only flag instance-level buffer growth (self.X.append) or specs that
    # explicitly describe unbounded accumulation. Local list building inside
    # a function (e.g., missing.append(f)) is normal and not a fleet issue.
    # IMPORTANT: Use word boundaries to avoid matching partial words like
    # "notifications" for "no" or "escape" for "cap".
    has_instance_append = re.search(r"self\.\w+\.append\(", content)
    has_unbounded_desc = re.search(
        r"(\bunbounded\b|\bgrows?\s+(forever|indefinitely)\b|\bnever\s+cleared\b"
        r"|\bno\b\s+\b(size\s+)?limit\b|\bno\b\s+\bcap\b|\bno\b\s+\bmax\b"
        r"|\bwithout\b.{0,30}\b(limit|cap|rotation|cleanup)\b)",
        content,
        re.IGNORECASE,
    )
    has_mitigation = re.search(
        r"(\bmax_size\b|\bmax_len\b|\bmaxlen\b|\btruncate?\b"
        r"|\bclear\(\)|\b\.pop\(|\bdel\s)",
        content,
    )
    # Verify mitigation is not negated (e.g., "no clear()" or "without truncation")
    if has_mitigation:
        mit_pat = re.compile(
            r"(\bmax_size\b|\bmax_len\b|\bmaxlen\b|\btruncate?\b"
            r"|\bclear\(\)|\b\.pop\(|\bdel\s)"
        )
        confirmed = False
        for m in mit_pat.finditer(content):
            start = max(0, m.start() - 30)
            prefix = content[start : m.start()].lower()
            if not re.search(r"\bno\b|\bnot\b|\bwithout\b|\bnever\b", prefix):
                confirmed = True
                break
        if not confirmed:
            has_mitigation = None
    if (has_instance_append or has_unbounded_desc) and not has_mitigation:
        issues.append(
            {
                "check": "fleet-readiness",
                "severity": "HIGH",
                "description": (
                    "In-memory data structures use .append() without any size limit, "
                    "truncation, or clearing. This leads to unbounded memory growth."
                ),
            }
        )

    # Look for file accumulation without retention
    # Only check code blocks for retention logic, not prose descriptions
    # (the spec might say "no retention policy" which is describing the problem)
    if re.search(r'open\([^)]*,\s*"a"\)', content):
        # Extract only code blocks (indented or fenced) to check for retention logic
        code_blocks = re.findall(r"```\w*\n(.*?)```", content, re.DOTALL)
        indented = re.findall(r"(?:^    .+\n)+", content, re.MULTILINE)
        code_text = "\n".join(code_blocks + indented)

        has_retention_code = re.search(
            r"(rotat|retention_policy|purg|delete_old|max_files|max_age|"
            r"os\.remove|os\.unlink|shutil\.rmtree)",
            code_text,
        )
        # Exclude negated mentions like "no cleanup" or "no retention"
        if has_retention_code:
            # Verify it's not a negation
            for m in re.finditer(
                r"(rotat|retention_policy|cleanup|purg|delete_old|max_files|max_age|"
                r"os\.remove|os\.unlink|shutil\.rmtree)",
                code_text,
            ):
                start = max(0, m.start() - 20)
                prefix = code_text[start : m.start()].lower()
                if not re.search(r"\bno\b|\bnot\b|\bwithout\b|\bnever\b", prefix):
                    has_retention_code = True
                    break
            else:
                has_retention_code = False
        if not has_retention_code:
            issues.append(
                {
                    "check": "fleet-readiness",
                    "severity": "HIGH",
                    "description": (
                        "Files are opened in append mode without any rotation or retention "
                        "policy. Log/data files will grow indefinitely."
                    ),
                }
            )

    # Look for subprocess without cleanup
    if re.search(r"subprocess\.Popen\(", content) and not re.search(
        r"(\.terminate\(\)|\.kill\(\)|\.wait\(\)|atexit|signal\.signal|cleanup|__del__|finally)",
        content,
    ):
        issues.append(
            {
                "check": "fleet-readiness",
                "severity": "HIGH",
                "description": (
                    "Subprocess spawned via Popen without termination or cleanup logic. "
                    "Orphaned processes will accumulate."
                ),
            }
        )

    # Look for loading all data into memory
    if re.search(r"(all_data|results)\s*=\s*\[\]", content) and re.search(
        r"(\.extend\(|for.*in.*os\.listdir)", content
    ):
        # Check if there's any windowing/limiting
        if not re.search(r"(limit|window|max_|recent|last_\d)", content, re.IGNORECASE):
            issues.append(
                {
                    "check": "fleet-readiness",
                    "severity": "MEDIUM",
                    "description": (
                        "Data is loaded from all files into memory without windowing or "
                        "limiting. For long-running installations, this causes OOM."
                    ),
                }
            )

    return issues


def eval_code_quality(content: str) -> list[dict]:
    """Check code-quality issues: bare excepts, silent failures."""
    issues = []

    # Look for bare except: pass
    bare_except_count = len(
        re.findall(
            r"except[^:]*:\s*\n\s*pass",
            content,
        )
    )
    if bare_except_count > 0:
        issues.append(
            {
                "check": "code-quality",
                "severity": "HIGH",
                "description": (
                    f"Found {bare_except_count} bare 'except: pass' blocks that silently "
                    "swallow all errors including disk full, permission denied, and "
                    "corruption. Errors should be logged or re-raised."
                ),
            }
        )

    # Look for functions that always return True (useless validation)
    # Pattern: def validate/check that has return True and except: pass
    if re.search(r"def (validate|check)\w*\(", content):
        # Check if the function always returns True
        func_blocks = re.finditer(
            r"def (validate|check)\w*\([^)]*\):[^\n]*\n((?:(?!^def ).+\n)*)",
            content,
            re.MULTILINE,
        )
        for match in func_blocks:
            func_body = match.group(2)
            returns = re.findall(r"return\s+(\S+)", func_body)
            if returns and all(r == "True" for r in returns):
                if re.search(r"except.*:\s*\n\s*pass", func_body):
                    issues.append(
                        {
                            "check": "code-quality",
                            "severity": "HIGH",
                            "description": (
                                f"Function '{match.group(0).split('(')[0].replace('def ', '')}' "
                                "always returns True and catches all exceptions silently. "
                                "The function promises validation but delivers none."
                            ),
                        }
                    )

    # Look for except Exception: pass (slightly less bare but still bad)
    except_pass_count = len(
        re.findall(
            r"except\s+Exception[^:]*:\s*\n\s*pass",
            content,
        )
    )
    if except_pass_count > 0 and bare_except_count == 0:
        issues.append(
            {
                "check": "code-quality",
                "severity": "HIGH",
                "description": (
                    f"Found {except_pass_count} 'except Exception: pass' blocks. "
                    "Errors are caught and silently discarded."
                ),
            }
        )

    # Look for error handling that does nothing (return without message)
    silent_returns = re.findall(
        r"except[^:]*:\s*\n\s*return\b(?!\s+\{)",
        content,
    )
    if silent_returns:
        issues.append(
            {
                "check": "code-quality",
                "severity": "MEDIUM",
                "description": (
                    f"Found {len(silent_returns)} exception handlers that return silently "
                    "without error messages. Users won't know what went wrong."
                ),
            }
        )

    return issues


def eval_completeness(content: str) -> list[dict]:
    """Check completeness issues: dropped tasks, unexplained skips."""
    issues = []
    lines = content.split("\n")

    # Find all task headings
    task_headings = []
    for i, line in enumerate(lines):
        match = re.match(r"^#{2,4}\s+(t-\d+)[:.]?\s*(.*)", line)
        if match:
            task_headings.append(
                {
                    "line_num": i,
                    "id": match.group(1),
                    "heading": line.strip(),
                }
            )

    # Check each task for status
    for task in task_headings:
        found_status = None
        for j in range(task["line_num"] + 1, min(task["line_num"] + 3, len(lines))):
            stripped = lines[j].strip()
            if stripped in ("DONE", "PENDING", "SKIPPED"):
                found_status = stripped
                break
            if stripped and not stripped.startswith("#"):
                break

        if found_status is None:
            issues.append(
                {
                    "check": "completeness",
                    "severity": "HIGH",
                    "description": (
                        f"Task '{task['heading']}' has no status. It appears to have been "
                        "silently dropped without being addressed."
                    ),
                }
            )
        elif found_status == "SKIPPED":
            # Check if there's a reason/explanation
            task_text = ""
            for k in range(task["line_num"] + 2, len(lines)):
                if re.match(r"^#{2,4}\s+t-\d+", lines[k]):
                    break
                task_text += lines[k] + "\n"
            # A SKIPPED task should have explanation text beyond just the Verify line
            has_spec = "**Spec:**" in task_text
            has_reason = bool(
                re.search(
                    r"(reason|because|skip.*because|not needed|out of scope|deferred)",
                    task_text,
                    re.IGNORECASE,
                )
            )
            if not has_spec and not has_reason:
                issues.append(
                    {
                        "check": "completeness",
                        "severity": "MEDIUM",
                        "description": (
                            f"Task '{task['heading']}' is SKIPPED but has no explanation "
                            "for why it was skipped."
                        ),
                    }
                )

    return issues


# ─── Helpers ────────────────────────────────────────────────────────────────


def _parse_tasks(content: str) -> list[dict]:
    """Parse spec content into task dicts with heading, status, verify."""
    tasks = []
    lines = content.split("\n")

    i = 0
    while i < len(lines):
        match = re.match(r"^#{2,4}\s+(t-\d+)[:.]?\s*(.*)", lines[i])
        if match:
            heading = lines[i].strip()
            status = None

            # Find status on next non-empty line
            for j in range(i + 1, min(i + 3, len(lines))):
                stripped = lines[j].strip()
                if stripped in ("DONE", "PENDING", "SKIPPED"):
                    status = stripped
                    break
                if stripped:
                    break

            # Collect task body until next task heading
            body_lines = []
            k = i + 1
            while k < len(lines):
                if re.match(r"^#{2,4}\s+t-\d+", lines[k]):
                    break
                body_lines.append(lines[k])
                k += 1

            body = "\n".join(body_lines)

            # Extract verify section
            verify_match = re.search(
                r"\*\*Verify:\*\*\s*(.*?)(?=\*\*(?:Spec|Self-evolution):\*\*|\Z)",
                body,
                re.DOTALL,
            )
            verify = verify_match.group(1).strip() if verify_match else ""

            tasks.append(
                {
                    "id": match.group(1),
                    "heading": heading,
                    "status": status,
                    "verify": verify,
                    "body": body,
                }
            )
            i = k
        else:
            i += 1

    return tasks


# ─── Evaluation logic ───────────────────────────────────────────────────────

CHECK_EVALUATORS = {
    "spec-integrity": eval_spec_integrity,
    "verify-commands": eval_verify_commands,
    "fleet-readiness": eval_fleet_readiness,
    "code-quality": eval_code_quality,
    "completeness": eval_completeness,
}


def evaluate_spec(spec_name: str, spec_path: str) -> dict:
    """Run all check evaluators against a spec and score the results.

    Returns a dict with:
        spec_name: str
        issues_found: list of issue dicts
        expected: dict from EXPECTED_ISSUES
        recall: float (0-1)
        precision: float (0-1)
        passed: bool
    """
    content = Path(spec_path).read_text(encoding="utf-8")
    expected = EXPECTED_ISSUES[spec_name]

    # Run all evaluators
    all_issues = []
    for check_name, evaluator in CHECK_EVALUATORS.items():
        found = evaluator(content)
        all_issues.extend(found)

    # Filter to primary check for this spec
    primary_check = expected["check"]
    primary_issues = (
        [i for i in all_issues if i["check"] == primary_check] if primary_check else []
    )

    # For the perfect spec, ALL issues are false positives
    if expected["min_issues"] == 0:
        false_positives = len(all_issues)
        recall = 1.0  # Nothing to catch
        precision = 1.0 if false_positives == 0 else 0.0
        passed = false_positives == 0
        return {
            "spec_name": spec_name,
            "issues_found": all_issues,
            "primary_issues": primary_issues,
            "expected": expected,
            "recall": recall,
            "precision": precision,
            "false_positives": false_positives,
            "passed": passed,
        }

    # Score: recall = caught / expected
    caught = len(primary_issues)
    expected_count = expected["min_issues"]
    recall = min(caught / expected_count, 1.0) if expected_count > 0 else 1.0

    # Score: precision = true positives / total issues
    # For the target check, all issues are "true" since we designed the spec to have them.
    # Issues from OTHER checks against this spec are false positives (unless also expected).
    other_issues = [i for i in all_issues if i["check"] != primary_check]
    false_positives = len(other_issues)
    total_found = len(all_issues)
    precision = (
        (total_found - false_positives) / total_found if total_found > 0 else 1.0
    )

    passed = recall >= 1.0 and false_positives <= 1  # Allow 1 FP tolerance

    return {
        "spec_name": spec_name,
        "issues_found": all_issues,
        "primary_issues": primary_issues,
        "expected": expected,
        "recall": recall,
        "precision": precision,
        "false_positives": false_positives,
        "passed": passed,
    }


def evaluate_prompt_generation(spec_name: str, spec_path: str) -> dict:
    """Verify that generate_critic_prompt works for each test spec.

    Creates a temp environment and generates the critic prompt.
    Returns a dict with success status and any errors.
    """
    with tempfile.TemporaryDirectory() as tmpdir:
        queue_dir = os.path.join(tmpdir, "queue")
        state_dir = tmpdir
        os.makedirs(queue_dir, exist_ok=True)

        # Create critic config dirs
        critic_dir = os.path.join(state_dir, "critic")
        custom_dir = os.path.join(critic_dir, "custom")
        os.makedirs(custom_dir, exist_ok=True)

        try:
            prompt = generate_critic_prompt(
                spec_path=spec_path,
                queue_id="q-test",
                iteration=1,
                config=DEFAULT_CONFIG,
                boi_dir=BOI_DIR,
                state_dir=state_dir,
            )
            # Verify prompt contains the spec content and checks
            content = Path(spec_path).read_text(encoding="utf-8")
            has_spec = content[:100] in prompt
            has_checks = (
                "spec-integrity" in prompt.lower() or "Spec Integrity" in prompt
            )

            return {
                "success": True,
                "has_spec_content": has_spec,
                "has_checks": has_checks,
                "prompt_length": len(prompt),
            }
        except Exception as e:
            return {
                "success": False,
                "error": str(e),
            }


def evaluate_run_critic(spec_name: str, spec_path: str) -> dict:
    """Verify that run_critic() generates a prompt file for each test spec.

    Creates a temp environment, runs run_critic(), and checks the output.
    """
    with tempfile.TemporaryDirectory() as tmpdir:
        queue_dir = os.path.join(tmpdir, "queue")
        state_dir = tmpdir
        os.makedirs(queue_dir, exist_ok=True)

        # Create critic dirs
        critic_dir = os.path.join(state_dir, "critic")
        custom_dir = os.path.join(critic_dir, "custom")
        os.makedirs(custom_dir, exist_ok=True)

        # Create a mock queue entry
        entry = {
            "id": "q-test",
            "spec_path": spec_path,
            "status": "completed",
            "critic_passes": 0,
        }
        entry_path = os.path.join(queue_dir, "q-test.json")
        with open(entry_path, "w") as f:
            json.dump(entry, f)

        # Set BOI_SCRIPT_DIR so run_critic finds templates
        old_env = os.environ.get("BOI_SCRIPT_DIR")
        os.environ["BOI_SCRIPT_DIR"] = BOI_DIR

        try:
            result = run_critic(
                spec_path=spec_path,
                queue_dir=queue_dir,
                queue_id="q-test",
                config=DEFAULT_CONFIG,
            )
            prompt_exists = os.path.isfile(result.get("prompt_path", ""))
            return {
                "success": True,
                "prompt_path_exists": prompt_exists,
                "result": result,
            }
        except Exception as e:
            return {
                "success": False,
                "error": str(e),
            }
        finally:
            if old_env is not None:
                os.environ["BOI_SCRIPT_DIR"] = old_env
            elif "BOI_SCRIPT_DIR" in os.environ:
                del os.environ["BOI_SCRIPT_DIR"]


# ─── Main ───────────────────────────────────────────────────────────────────


def main():
    """Run the full critic evaluation suite and print results."""
    spec_names = [
        "malformed-tasks",
        "fake-verify",
        "unbounded-growth",
        "silent-failures",
        "incomplete-spec",
        "perfect-spec",
    ]

    print("BOI Critic Evaluation")
    print("=" * 60)
    print()

    results = []
    total_recall = 0.0
    total_precision = 0.0
    passed_count = 0
    partial_count = 0

    for spec_name in spec_names:
        spec_path = os.path.join(FIXTURES_DIR, f"{spec_name}.md")
        if not os.path.isfile(spec_path):
            print(f"  {spec_name}: MISSING (fixture file not found)")
            continue

        # Run check evaluators
        result = evaluate_spec(spec_name, spec_path)
        results.append(result)

        # Run prompt generation test
        prompt_result = evaluate_prompt_generation(spec_name, spec_path)
        result["prompt_generation"] = prompt_result

        # Run run_critic() test
        critic_result = evaluate_run_critic(spec_name, spec_path)
        result["run_critic"] = critic_result

        # Format output
        expected = result["expected"]
        if expected["min_issues"] == 0:
            # Perfect spec
            if result["passed"]:
                status = "PASS"
                detail = f"approved, {result['false_positives']} false positives"
                passed_count += 1
            else:
                status = "FAIL"
                detail = f"{result['false_positives']} false positives"
        else:
            caught = len(result["primary_issues"])
            expected_min = expected["min_issues"]
            fp = result["false_positives"]

            if result["passed"]:
                status = "PASS"
                passed_count += 1
            elif result["recall"] > 0:
                status = "PARTIAL"
                partial_count += 1
            else:
                status = "FAIL"
            detail = f"{caught}/{expected_min} issues caught, {fp} false positives"

        # Prompt gen status
        pg_ok = "ok" if prompt_result.get("success") else "FAIL"
        rc_ok = "ok" if critic_result.get("success") else "FAIL"

        print(f"  {spec_name + ':':24s}{status:8s}({detail})")
        print(f"  {'':24s}prompt_gen={pg_ok}, run_critic={rc_ok}")

        total_recall += result["recall"]
        total_precision += result["precision"]

    print()
    count = len(results)
    failed_count = count - passed_count - partial_count

    print(f"  Overall: {passed_count}/{count} passed", end="")
    if partial_count > 0:
        print(f", {partial_count} partial", end="")
    if failed_count > 0:
        print(f", {failed_count} failed", end="")
    print()

    avg_precision = total_precision / count if count > 0 else 0
    avg_recall = total_recall / count if count > 0 else 0
    print(f"  Precision: {avg_precision:.0%} | Recall: {avg_recall:.0%}")
    print()

    # Print detailed issues for debugging
    print("─" * 60)
    print("Detailed Findings")
    print("─" * 60)
    for result in results:
        name = result["spec_name"]
        print(f"\n  {name}:")
        if not result["issues_found"]:
            print("    (no issues found)")
        for issue in result["issues_found"]:
            sev = issue["severity"]
            check = issue["check"]
            desc = issue["description"][:100]
            print(f"    [{sev}] {check}: {desc}")

    # Return exit code
    all_passed = all(r["passed"] for r in results)
    all_prompts_ok = all(
        r.get("prompt_generation", {}).get("success", False) for r in results
    )
    all_critics_ok = all(r.get("run_critic", {}).get("success", False) for r in results)

    if all_passed and all_prompts_ok and all_critics_ok:
        print("\nAll checks passed.")
        return 0
    else:
        print("\nSome checks need attention.")
        return 1


if __name__ == "__main__":
    sys.exit(main())
