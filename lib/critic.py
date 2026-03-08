# critic.py — Critic execution for BOI.
#
# The critic validates spec work quality before marking specs complete.
# It synthesizes three review perspectives (adversarial, scale/gaps,
# code actionability) into a single validation pass.
#
# Quality scoring integration (t-10):
#   - Before running checks, compute a quality score using the quality-scoring
#     prompt template.
#   - Score >= 0.85: fast-approve (skip detailed checks, still write score).
#   - Score 0.50-0.84: standard review (run all 5 checks).
#   - Score < 0.50: auto-reject (add a [CRITIC] PENDING task).
#   - Mode awareness: validate worker followed mode rules.
#
# Key functions:
#   - generate_critic_prompt(spec_path, queue_id, iteration, config, boi_dir, state_dir, queue_entry)
#     Builds the full critic prompt by injecting spec content and checks.
#   - parse_critic_result(spec_path)
#     Reads the spec file after a critic worker runs and detects approval or new tasks.
#   - should_run_critic(queue_entry, config)
#     Determines if the critic should run for a given queue entry.
#   - compute_quality_gate(quality_score)
#     Determines critic behavior based on quality score threshold.
#   - validate_mode_compliance(mode, pre_tasks, post_tasks)
#     Checks if the worker followed mode rules.

import json
import os
import re
import time
from pathlib import Path
from typing import Any, Optional

from lib.critic_config import (
    get_active_checks,
    get_critic_prompt,
    get_generate_checks,
    is_critic_enabled,
    load_critic_config,
)

# Quality score thresholds for critic gating
QUALITY_FAST_APPROVE_THRESHOLD = 0.85
QUALITY_AUTO_REJECT_THRESHOLD = 0.50


def should_run_critic(
    queue_entry: dict[str, Any],
    config: dict[str, Any],
) -> bool:
    """Determine if the critic should run for a given queue entry.

    The critic runs when:
    1. The critic is enabled in config
    2. The spec was not dispatched with --no-critic
    3. The critic_passes count is below max_passes
    4. The trigger condition is met (currently only "on_complete")

    For Generate-mode specs, uses generate_max_passes (default 3) instead of
    the standard max_passes (default 2).

    Args:
        queue_entry: The queue entry dict for the spec.
        config: The critic configuration dict.

    Returns:
        True if the critic should run.
    """
    if not is_critic_enabled(config):
        return False

    # Per-spec opt-out via --no-critic on dispatch
    if queue_entry.get("no_critic", False):
        return False

    # Use generate_max_passes for Generate-mode specs
    mode = queue_entry.get("mode", "execute") or "execute"
    if mode == "generate":
        max_passes = config.get("generate_max_passes", 3)
    else:
        max_passes = config.get("max_passes", 2)

    critic_passes = queue_entry.get("critic_passes", 0)
    if critic_passes >= max_passes:
        return False

    trigger = config.get("trigger", "on_complete")
    if trigger != "on_complete":
        return False

    return True


def compute_quality_gate(quality_score: Optional[float]) -> str:
    """Determine critic behavior based on quality score.

    Args:
        quality_score: The computed quality score (0.0-1.0), or None if
            quality scoring was not run.

    Returns:
        One of: "fast_approve", "standard", "auto_reject", "unknown".
        "unknown" is returned when quality_score is None.
    """
    if quality_score is None:
        return "unknown"
    if quality_score >= QUALITY_FAST_APPROVE_THRESHOLD:
        return "fast_approve"
    if quality_score < QUALITY_AUTO_REJECT_THRESHOLD:
        return "auto_reject"
    return "standard"


def validate_mode_compliance(
    mode: str,
    pre_task_ids: set[str],
    post_task_ids: set[str],
    post_tasks: list[Any],
) -> list[dict[str, str]]:
    """Check if the worker followed mode rules.

    Args:
        mode: The mode the worker was in (execute, challenge, discover, generate).
        pre_task_ids: Set of task IDs before the iteration.
        post_task_ids: Set of task IDs after the iteration.
        post_tasks: List of BoiTask objects after the iteration.

    Returns:
        List of violation dicts with 'type' and 'message' keys.
    """
    violations: list[dict[str, str]] = []
    added_ids = post_task_ids - pre_task_ids

    if mode == "execute":
        # Execute mode: worker should NOT add tasks
        if added_ids:
            violations.append(
                {
                    "type": "mode_violation",
                    "message": (
                        f"Execute mode worker added {len(added_ids)} task(s) "
                        f"({', '.join(sorted(added_ids))}). "
                        "Execute mode does not allow adding new tasks."
                    ),
                }
            )

    elif mode == "challenge":
        # Challenge mode: worker should NOT add tasks
        if added_ids:
            violations.append(
                {
                    "type": "mode_violation",
                    "message": (
                        f"Challenge mode worker added {len(added_ids)} task(s) "
                        f"({', '.join(sorted(added_ids))}). "
                        "Challenge mode does not allow adding new tasks."
                    ),
                }
            )

    elif mode == "discover":
        # Discover mode: worker CAN add tasks, but they must have Spec + Verify
        for task in post_tasks:
            if task.id in added_ids:
                body = task.body if hasattr(task, "body") else ""
                has_spec = "**Spec:**" in body
                has_verify = "**Verify:**" in body
                if not has_spec or not has_verify:
                    missing = []
                    if not has_spec:
                        missing.append("Spec")
                    if not has_verify:
                        missing.append("Verify")
                    violations.append(
                        {
                            "type": "mode_violation",
                            "message": (
                                f"Discover mode: new task {task.id} is missing "
                                f"required section(s): {', '.join(missing)}."
                            ),
                        }
                    )

    elif mode == "generate":
        # Generate mode: worker can add/modify tasks, SUPERSEDED must reference replacement
        for task in post_tasks:
            if task.status == "SUPERSEDED":
                superseded_by = (
                    task.superseded_by if hasattr(task, "superseded_by") else ""
                )
                if not superseded_by:
                    violations.append(
                        {
                            "type": "mode_violation",
                            "message": (
                                f"Generate mode: task {task.id} is SUPERSEDED "
                                "but does not reference a replacement task (missing 'by t-N')."
                            ),
                        }
                    )

        # Check max 5 new tasks per iteration
        if len(added_ids) > 5:
            violations.append(
                {
                    "type": "mode_violation",
                    "message": (
                        f"Generate mode: worker added {len(added_ids)} tasks "
                        "(max 5 per iteration)."
                    ),
                }
            )

    return violations


def _build_quality_scoring_section(boi_dir: str) -> str:
    """Load the quality scoring prompt and wrap it as a critic section.

    Args:
        boi_dir: Path to ~/boi/ installation directory.

    Returns:
        The quality scoring prompt section, or empty string if not found.
    """
    quality_prompt_path = os.path.join(
        boi_dir, "templates", "checks", "quality-scoring.md"
    )
    if not os.path.isfile(quality_prompt_path):
        return ""

    try:
        content = Path(quality_prompt_path).read_text(encoding="utf-8")
    except OSError:
        return ""

    return (
        "## Quality Scoring (Pre-Check)\n\n"
        "Before running the detailed checks below, compute a quality score "
        "using the following scoring methodology. Output the quality score JSON "
        "FIRST, then proceed with checks based on the score:\n\n"
        "- **Score >= 0.85**: Fast-approve. Skip the detailed checks below. "
        "Still output the quality score JSON and append `## Critic Approved` "
        "to the spec.\n"
        "- **Score 0.50-0.84**: Standard review. Run all checks below. "
        "Quality score informs severity assessment.\n"
        "- **Score < 0.50**: Auto-reject. Add a `[CRITIC]` PENDING task: "
        '"Quality score is below threshold (X). Review and improve error '
        'handling, test coverage, and verify commands."\n\n'
        f"{content}\n\n"
    )


def _build_mode_awareness_section(
    mode: str,
    queue_entry: Optional[dict[str, Any]] = None,
) -> str:
    """Build mode-awareness instructions for the critic prompt.

    Args:
        mode: The mode the worker was in.
        queue_entry: The queue entry dict (for pre_iteration_tasks).

    Returns:
        Mode awareness prompt section.
    """
    sections = ["## Mode Awareness\n"]
    sections.append(f"The worker was operating in **{mode}** mode.\n")
    sections.append("Check the following mode-specific rules:\n")

    if mode == "execute":
        sections.append(
            "- **Execute mode**: The worker should NOT have added any new tasks. "
            "If you see new `### t-N:` headings that were not present before "
            "this iteration, flag it as a HIGH severity issue.\n"
        )
    elif mode == "challenge":
        sections.append(
            "- **Challenge mode**: The worker should NOT have added new tasks. "
            "It may write `## Challenges` sections. SKIPPED tasks must have "
            "a detailed reason. If tasks were added, flag it as a HIGH severity issue.\n"
        )
    elif mode == "discover":
        sections.append(
            "- **Discover mode**: The worker CAN add new tasks. Verify that "
            "every new task has both a `**Spec:**` section and a `**Verify:**` section. "
            "New tasks without these sections are HIGH severity issues.\n"
        )
    elif mode == "generate":
        sections.append(
            "- **Generate mode**: The worker has full creative authority. Verify that:\n"
            "  - SUPERSEDED tasks reference their replacement (e.g., `SUPERSEDED by t-N`).\n"
            "  - No more than 5 new tasks were added in this iteration.\n"
            "  - DONE tasks were not modified.\n"
            "  - Tasks were not deleted (only SKIPPED or SUPERSEDED).\n"
        )

    return "\n".join(sections) + "\n"


def generate_critic_prompt(
    spec_path: str,
    queue_id: str,
    iteration: int,
    config: dict[str, Any],
    boi_dir: str,
    state_dir: str,
    queue_entry: Optional[dict[str, Any]] = None,
) -> str:
    """Generate the full critic prompt with spec content, checks, and quality scoring.

    Args:
        spec_path: Path to the spec file to validate.
        queue_id: The spec queue ID (e.g., "q-001").
        iteration: The current critic pass number.
        config: The critic configuration dict.
        boi_dir: Path to ~/boi/ installation directory.
        state_dir: Path to ~/.boi/ state directory.
        queue_entry: Optional queue entry dict for mode awareness.

    Returns:
        The fully rendered critic prompt string.

    Raises:
        FileNotFoundError: If the prompt template or spec file is missing.
    """
    # Load the critic prompt template
    template = get_critic_prompt(state_dir, boi_dir)

    # Read spec content
    spec_content = Path(spec_path).read_text(encoding="utf-8")

    # Load active checks
    checks_dir = os.path.join(boi_dir, "templates", "checks")
    checks = get_active_checks(config, checks_dir, state_dir)

    # Build quality scoring section (injected before checks)
    quality_section = _build_quality_scoring_section(boi_dir)

    # Build mode awareness section
    mode = "execute"
    if queue_entry:
        mode = queue_entry.get("mode", "execute") or "execute"
    # Check spec header for mode override
    mode_match = re.search(r"^\*\*Mode:\*\*\s*(\w+)", spec_content, re.MULTILINE)
    if mode_match:
        spec_mode = mode_match.group(1).strip().lower()
        if spec_mode in {"execute", "challenge", "discover", "generate"}:
            mode = spec_mode

    mode_section = _build_mode_awareness_section(mode, queue_entry)

    # Format checks for injection (quality scoring goes first)
    checks_text = ""
    if quality_section:
        checks_text += quality_section
    checks_text += mode_section
    for check in checks:
        checks_text += f"### Check: {check['name']} ({check['source']})\n\n"
        checks_text += check["content"] + "\n\n"

    # Add Generate-mode-specific checks (e.g., goal-alignment)
    if mode == "generate":
        generate_checks = get_generate_checks(config, checks_dir, state_dir)
        for check in generate_checks:
            checks_text += f"### Check: {check['name']} ({check['source']}) [Generate-mode only]\n\n"
            checks_text += check["content"] + "\n\n"

    # Replace template variables
    result = template.replace("{{SPEC_CONTENT}}", spec_content)
    result = result.replace("{{CHECKS}}", checks_text)
    result = result.replace("{{QUEUE_ID}}", queue_id)
    result = result.replace("{{ITERATION}}", str(iteration))
    result = result.replace("{{SPEC_PATH}}", spec_path)

    return result


def parse_critic_result(spec_path: str) -> dict[str, Any]:
    """Parse the spec file after a critic worker runs.

    Looks for:
    1. `## Critic Approved` section (critic approved the spec)
    2. New `[CRITIC]` PENDING tasks (critic found issues)
    3. Quality score JSON block (from quality scoring)

    Args:
        spec_path: Path to the spec file after critic review.

    Returns:
        A dict with:
          approved: bool — True if `## Critic Approved` is found.
          critic_tasks_added: int — Count of new [CRITIC] PENDING tasks.
          quality_score: float or None — Overall quality score if found.
          quality_gate: str — "fast_approve", "standard", "auto_reject", or "unknown".
          quality_signals: dict or None — Per-category scores if found.
    """
    try:
        content = Path(spec_path).read_text(encoding="utf-8")
    except OSError:
        return {
            "approved": False,
            "critic_tasks_added": 0,
            "quality_score": None,
            "quality_gate": "unknown",
            "quality_signals": None,
        }

    # Check for Critic Approved section
    approved = bool(re.search(r"^## Critic Approved", content, re.MULTILINE))

    # Count [CRITIC] PENDING tasks
    critic_pending = len(
        re.findall(
            r"^### t-\d+:.*\[CRITIC\].*\n\s*PENDING",
            content,
            re.MULTILINE,
        )
    )

    # Extract quality score from JSON block
    quality_score = None
    quality_signals = None
    quality_json = _extract_quality_json(content)
    if quality_json is not None:
        quality_score = quality_json.get("overall_quality_score")
        categories = quality_json.get("categories", {})
        if categories:
            quality_signals = {
                cat_name: cat_data.get("score")
                for cat_name, cat_data in categories.items()
                if isinstance(cat_data, dict)
            }

    quality_gate = compute_quality_gate(quality_score)

    return {
        "approved": approved,
        "critic_tasks_added": critic_pending,
        "quality_score": quality_score,
        "quality_gate": quality_gate,
        "quality_signals": quality_signals,
    }


def _extract_quality_json(content: str) -> Optional[dict[str, Any]]:
    """Extract the quality scoring JSON from critic output.

    Looks for a JSON block containing "overall_quality_score" key.

    Args:
        content: The spec file content after critic review.

    Returns:
        Parsed quality JSON dict, or None if not found.
    """
    # Look for JSON code blocks
    json_blocks = re.findall(r"```json\s*\n(.*?)\n```", content, re.DOTALL)
    for block in json_blocks:
        try:
            data = json.loads(block)
            if isinstance(data, dict) and "overall_quality_score" in data:
                return data
        except (json.JSONDecodeError, TypeError):
            continue

    # Also try bare JSON objects containing the key
    json_pattern = re.findall(
        r'\{[^{}]*"overall_quality_score"[^{}]*\}', content, re.DOTALL
    )
    for match in json_pattern:
        try:
            data = json.loads(match)
            if isinstance(data, dict):
                return data
        except (json.JSONDecodeError, TypeError):
            continue

    return None


def generate_auto_reject_task(
    quality_score: float,
    next_task_id: int,
) -> str:
    """Generate a [CRITIC] PENDING task for auto-reject due to low quality score.

    Args:
        quality_score: The quality score that triggered auto-reject.
        next_task_id: The next available task ID number.

    Returns:
        The task definition string to append to the spec.
    """
    return (
        f"\n### t-{next_task_id}: [CRITIC] Quality score below threshold\n"
        "PENDING\n\n"
        f"**Spec:** Quality score is below threshold ({quality_score:.2f}). "
        "Review and improve error handling, test coverage, and verify commands. "
        "Focus on: (1) adding try/except blocks around I/O operations, "
        "(2) writing tests for new functions, (3) replacing trivial verify "
        "commands with substantive checks that validate actual behavior.\n\n"
        "**Verify:** Run the quality scoring prompt again and confirm the "
        "overall quality score is >= 0.50.\n"
    )


def get_next_task_id(spec_content: str) -> int:
    """Find the next available task ID number from spec content.

    Args:
        spec_content: The spec file content.

    Returns:
        The next task ID number (highest existing + 1).
    """
    task_ids = re.findall(r"^### t-(\d+):", spec_content, re.MULTILINE)
    if not task_ids:
        return 1
    return max(int(tid) for tid in task_ids) + 1


def run_critic(
    spec_path: str,
    queue_dir: str,
    queue_id: str,
    config: dict[str, Any],
) -> dict[str, Any]:
    """Run the critic on a completed spec.

    This function is called by daemon_ops to prepare for a critic review.
    It does NOT launch the Claude process itself (the daemon handles that).
    Instead, it generates the critic prompt and writes it to disk.

    Args:
        spec_path: Path to the spec file to validate.
        queue_dir: Path to ~/.boi/queue/.
        queue_id: The spec queue ID (e.g., "q-001").
        config: The critic configuration dict.

    Returns:
        A dict with:
          approved: bool — whether the spec passed validation.
          issues: list — any issues found (empty if approved).
          prompt_path: str — path to the generated critic prompt file.
    """
    start_time = time.monotonic()

    # Derive paths
    state_dir = str(Path(queue_dir).parent)
    boi_dir = os.environ.get(
        "BOI_SCRIPT_DIR", str(Path(__file__).resolve().parent.parent)
    )

    # Get current critic pass count and queue entry from queue
    from lib.queue import get_entry

    entry = get_entry(queue_dir, queue_id)
    critic_passes = entry.get("critic_passes", 0) if entry else 0

    # Generate the critic prompt
    prompt = generate_critic_prompt(
        spec_path=spec_path,
        queue_id=queue_id,
        iteration=critic_passes + 1,
        config=config,
        boi_dir=boi_dir,
        state_dir=state_dir,
        queue_entry=entry,
    )

    # Write prompt to queue dir
    prompt_path = os.path.join(queue_dir, f"{queue_id}.critic-prompt.md")
    tmp_path = prompt_path + ".tmp"
    Path(tmp_path).write_text(prompt, encoding="utf-8")
    os.replace(tmp_path, prompt_path)

    elapsed = time.monotonic() - start_time

    return {
        "approved": False,
        "issues": [],
        "prompt_path": prompt_path,
        "elapsed_seconds": round(elapsed, 3),
    }


def write_quality_to_telemetry(
    queue_dir: str,
    queue_id: str,
    quality_score: Optional[float],
    quality_signals: Optional[dict[str, Optional[float]]],
    quality_gate: str,
) -> None:
    """Write quality score data to the current iteration's telemetry file.

    Finds the latest iteration-N.json and adds quality fields.

    Args:
        queue_dir: Path to ~/.boi/queue/.
        queue_id: The spec queue ID.
        quality_score: Overall quality score (0.0-1.0) or None.
        quality_signals: Per-category quality scores or None.
        quality_gate: The quality gate decision.
    """
    queue_path = Path(queue_dir)
    if not queue_path.is_dir():
        return

    # Find the latest iteration file
    prefix = f"{queue_id}.iteration-"
    iter_files = sorted(
        [
            f
            for f in queue_path.iterdir()
            if f.name.startswith(prefix) and f.name.endswith(".json")
        ],
        key=lambda f: f.name,
    )
    if not iter_files:
        return

    latest = iter_files[-1]
    try:
        data = json.loads(latest.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError):
        return

    # Add quality fields
    data["quality_score"] = quality_score
    data["quality_signals"] = quality_signals
    data["quality_gate"] = quality_gate

    # Compute grade if we have a score
    if quality_score is not None:
        from lib.quality import grade

        data["quality_grade"] = grade(quality_score)
    else:
        data["quality_grade"] = None

    # Write back atomically
    tmp = latest.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
    os.rename(str(tmp), str(latest))
