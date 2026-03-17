# worker.py — Execute one iteration of a BOI spec.
#
# Replaces worker.sh. Reads the spec, generates a prompt, writes
# a run script, launches Claude in a tmux session, waits for
# completion, and writes iteration metadata.
#
# Usage:
#   python3 worker.py <queue-id> <worktree> <spec-path> <iteration>
#       [--phase execute|critic|evaluate|decompose]
#       [--timeout SECONDS]
#       [--mode execute|challenge|discover|generate]
#       [--project PROJECT_NAME]
#
# The worker:
#   1. Reads the spec
#   2. Counts PENDING tasks (exits 0 if none, for execute phase)
#   3. Generates a prompt from template + spec + mode rules
#   4. Writes a run script for the tmux session
#   5. Launches the run script in a tmux session
#   6. Waits for tmux to finish
#   7. Post-processes: count tasks, write iteration metadata

import argparse
import json
import logging
import os
import re
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

sys.path.insert(0, str(Path(__file__).resolve().parent))

from lib.spec_parser import count_boi_tasks


class WorkerHooks:
    """Extension point for injecting context into worker prompts.

    Subclass this and pass an instance to Worker(hooks=...) to inject
    additional context (e.g. hive context, project metadata) into the
    prompt before each iteration.

    Default implementation returns empty strings for all hooks.
    """

    def pre_iteration(self, spec_path: str, worktree: str) -> str:
        """Called before each iteration to inject context into the prompt.

        Args:
            spec_path: Path to the spec file (queue copy).
            worktree: Path to the worker's checkout/worktree.

        Returns:
            Additional context string to prepend to the prompt.
            Return empty string to inject nothing.
        """
        return ""


# Constants
BOI_STATE_DIR = os.path.expanduser("~/.boi")
SCRIPT_DIR = str(Path(__file__).resolve().parent)
TEMPLATE_PATH = os.path.join(SCRIPT_DIR, "templates", "worker-prompt.md")
MODES_DIR = os.path.join(SCRIPT_DIR, "templates", "modes")
CRITIC_TEMPLATE_PATH = os.path.join(
    SCRIPT_DIR, "templates", "critic-worker-prompt.md"
)
DECOMPOSE_TEMPLATE_PATH = os.path.join(
    SCRIPT_DIR, "templates", "generate-decompose-prompt.md"
)
EVALUATE_TEMPLATE_PATH = os.path.join(
    SCRIPT_DIR, "templates", "evaluate-prompt.md"
)

VALID_MODES = {"execute", "challenge", "discover", "generate"}
VALID_PHASES = {"execute", "critic", "evaluate", "decompose"}
TMUX_POLL_INTERVAL = 5  # seconds between tmux has-session polls
TMUX_SOCKET = "boi"     # tmux socket name (-L flag)

logger = logging.getLogger("boi.worker")


# Model name aliases → (full model ID, effort level)
_MODEL_MAP = {
    "opus":   ("claude-opus-4-6", "high"),
    "sonnet": ("claude-sonnet-4-6", "medium"),
    "haiku":  ("claude-haiku-4-5-20251001", "low"),
}


def parse_task_model(task_block: str) -> Optional[tuple[str, str]]:
    """Parse **Model:** field from a task block.

    Returns (model_id, effort) or None if no Model field found.
    Supports: opus, sonnet, haiku, or full model IDs.
    """
    match = re.search(r'\*\*Model:\*\*\s*(\S+)', task_block)
    if not match:
        return None
    name = match.group(1).lower().strip()
    if name in _MODEL_MAP:
        return _MODEL_MAP[name]
    # Allow full model IDs (e.g., claude-opus-4-6)
    return (name, "medium")  # default effort for custom models


class Worker:
    """Execute one iteration of a BOI spec.

    Reads the spec, generates prompt + run script, launches Claude
    in a tmux session, waits for completion, and post-processes.

    Args:
        spec_id: Queue ID (e.g. "q-001").
        worktree: Path to the worker's checkout/worktree.
        spec_path: Path to the spec file (queue copy).
        iteration: Current iteration number.
        phase: Phase to execute (execute|critic|evaluate|decompose).
        timeout_seconds: Optional timeout in seconds.
        mode: Optional mode override (execute|challenge|discover|generate).
        project: Optional project name for context injection.
        state_dir: Path to ~/.boi state directory.
        worker_id: Optional worker slot ID (from WORKER_ID env var).
        hooks: Optional WorkerHooks for pre-iteration context injection.
    """

    def __init__(
        self,
        spec_id: str,
        worktree: str,
        spec_path: str,
        iteration: int,
        phase: str = "execute",
        timeout_seconds: Optional[int] = None,
        mode: Optional[str] = None,
        project: Optional[str] = None,
        state_dir: Optional[str] = None,
        worker_id: Optional[str] = None,
        hooks: Optional[WorkerHooks] = None,
    ) -> None:
        self.spec_id = spec_id
        self.worktree = worktree
        self.spec_path = spec_path
        self.iteration = iteration
        self.phase = phase
        self.timeout_seconds = timeout_seconds
        self.mode = mode
        self.project = project or ""
        self.hooks = hooks

        if state_dir is None:
            self.state_dir = BOI_STATE_DIR
        else:
            self.state_dir = state_dir

        self.worker_id = worker_id or os.environ.get("WORKER_ID", "")

        # Derived paths
        self.queue_dir = os.path.join(self.state_dir, "queue")
        self.log_dir = os.path.join(self.state_dir, "logs")
        self.log_file = os.path.join(
            self.log_dir, f"{spec_id}-iter-{iteration}.log"
        )
        self.prompt_file = os.path.join(
            self.queue_dir, f"{spec_id}.prompt.md"
        )
        self.run_script = os.path.join(
            self.queue_dir, f"{spec_id}.run.sh"
        )
        self.exit_file = os.path.join(
            self.queue_dir, f"{spec_id}.exit"
        )
        self.iteration_file = os.path.join(
            self.queue_dir,
            f"{spec_id}.iteration-{iteration}.json",
        )

        # Tmux session name: boi-{spec_id} or boi-{spec_id}-{worker_id}
        if self.worker_id:
            self.tmux_session = f"boi-{spec_id}-{self.worker_id}"
        else:
            self.tmux_session = f"boi-{spec_id}"

        # Pre-iteration task counts (set during run)
        self.pre_counts: dict[str, int] = {}

        # Model routing: phase → (model, effort)
        # PLAN (decompose) uses Opus at high effort for deep reasoning.
        # WORK (execute) uses Sonnet at medium effort for speed + quality balance.
        # QUALITY (critic, evaluate) uses Sonnet at medium effort.
        self._model_routing = {
            "decompose": ("claude-opus-4-6", "high"),
            "execute":   ("claude-sonnet-4-6", "medium"),  # default; per-task **Model:** overrides
            "critic":    ("claude-sonnet-4-6", "medium"),
            "evaluate":  ("claude-sonnet-4-6", "medium"),
        }

    def _claude_cmd(self, model_override: Optional[tuple[str, str]] = None) -> str:
        """Build the claude -p command with model routing.

        Priority: per-task **Model:** field > phase-based default.

        The returned string is interpolated via {self._claude_cmd()} inside
        the outer f-string, so bash variables must use ${{var}} to survive
        the outer f-string's brace resolution ({{x}} → {x} → ${x} in bash).
        """
        if model_override:
            model, effort = model_override
        else:
            model, effort = self._model_routing.get(
                self.phase, ("claude-sonnet-4-6", "medium")
            )
        # NOTE: The outer f-string in generate_run_script() will resolve
        # {{_PROMPT_FILE}} → {_PROMPT_FILE}. But since _claude_cmd()'s
        # return is already evaluated by the time the outer f-string runs,
        # we need the literal bash: ${_PROMPT_FILE}.
        prompt_ref = '${_PROMPT_FILE}'
        log_ref = '${_LOG_FILE}'
        return (
            f'env -u CLAUDECODE claude -p "$(cat "{prompt_ref}")" '
            f'--model {model} --effort {effort} --dangerously-skip-permissions'
        )

    def run(self) -> int:
        """Execute one iteration: check tasks, generate scripts, launch.

        Returns:
            Exit code (0 = success, non-zero = failure).
        """
        logger.info(
            "Starting worker for %s (iteration %d, phase %s)",
            self.spec_id,
            self.iteration,
            self.phase,
        )

        # Validate prerequisites
        if not os.path.isfile(self.spec_path):
            logger.error("Spec file not found: %s", self.spec_path)
            return 2

        if not os.path.isdir(self.worktree):
            logger.error(
                "Worktree does not exist: %s", self.worktree
            )
            return 2

        os.makedirs(self.log_dir, exist_ok=True)
        os.makedirs(self.queue_dir, exist_ok=True)

        # Count tasks before iteration
        self.pre_counts = count_boi_tasks(self.spec_path)
        pre_pending = self.pre_counts.get("pending", 0)

        # If no pending tasks and we're in execute phase, exit success
        if pre_pending == 0 and self.phase == "execute":
            logger.info(
                "No PENDING tasks in spec. Exiting with success."
            )
            _write_file(self.exit_file, "0")
            return 0

        # Generate prompt and run script
        self.generate_run_script()

        # Launch tmux session, wait, post-process
        rc = self.launch_tmux()
        if rc != 0:
            logger.error("Failed to launch tmux session.")
            return 1

        try:
            exit_code = self.wait_for_tmux()
        except TimeoutError:
            logger.warning(
                "Worker timed out after %s seconds.",
                self.timeout_seconds,
            )
            exit_code = 124
            _write_file(self.exit_file, "124")
            self._kill_tmux_session()

        self.post_process()
        return exit_code

    def generate_run_script(self) -> None:
        """Generate the prompt file and bash run script.

        Handles all four phases:
        - execute: worker-prompt.md + mode fragment + spec content
        - critic: critic-worker-prompt.md + critic prompt
        - decompose: generate-decompose-prompt.md + spec content
        - evaluate: evaluate-prompt.md + spec content

        Writes:
        - {queue_dir}/{spec_id}.prompt.md
        - {queue_dir}/{spec_id}.run.sh
        """
        self._generate_prompt()
        self._generate_bash_run_script()

    def _generate_prompt(self) -> None:
        """Generate the prompt file based on the current phase."""
        if self.phase == "critic":
            self._generate_critic_prompt()
        elif self.phase == "decompose":
            self._generate_decompose_prompt()
        elif self.phase == "evaluate":
            self._generate_evaluate_prompt()
        else:
            self._generate_execute_prompt()

    def _generate_execute_prompt(self) -> None:
        """Generate prompt for execute phase.

        Loads the worker-prompt.md template, determines mode from
        spec header or constructor arg, loads mode fragment, loads
        project context, and replaces all placeholders.
        """
        template = _read_file(TEMPLATE_PATH)
        spec_content = _read_file(self.spec_path)
        pending_count = str(self.pre_counts.get("pending", 0))

        # Determine mode: constructor override > spec header > default
        mode = self._resolve_mode(spec_content)

        # Load mode fragment
        mode_fragment = self._load_mode_fragment(mode)

        # Load project context
        project_context = self._load_project_context()

        # Load worktree context from hooks
        worktree_context = ""
        if self.hooks and hasattr(self.hooks, "pre_iteration"):
            try:
                worktree_context = self.hooks.pre_iteration(
                    self.spec_path, self.worktree
                )
            except Exception:
                logger.exception("Hook pre_iteration failed")

        # Replace template placeholders
        # Replace non-content placeholders first so that {{ }} in
        # spec content are not processed.
        result = template.replace(
            "{{ITERATION}}", str(self.iteration)
        )
        result = result.replace("{{QUEUE_ID}}", self.spec_id)
        result = result.replace("{{SPEC_PATH}}", self.spec_path)
        result = result.replace("{{PENDING_COUNT}}", pending_count)
        result = result.replace("{{MODE_RULES}}", mode_fragment)
        result = result.replace("{{PROJECT}}", self.project)
        result = result.replace(
            "{{PROJECT_CONTEXT}}", project_context
        )
        result = result.replace(
            "{{WORKTREE_CONTEXT}}", worktree_context
        )
        result = result.replace("{{SPEC_CONTENT}}", spec_content)

        _write_file_atomic(self.prompt_file, result)
        logger.info("Prompt generated: %s", self.prompt_file)

    def _generate_critic_prompt(self) -> None:
        """Generate prompt for critic phase.

        Uses the pre-generated critic prompt file and the
        critic-worker-prompt.md template.
        """
        critic_prompt_file = os.path.join(
            self.queue_dir, f"{self.spec_id}.critic-prompt.md"
        )
        if not os.path.isfile(critic_prompt_file):
            raise FileNotFoundError(
                f"Critic prompt not found: {critic_prompt_file}"
            )

        template = _read_file(CRITIC_TEMPLATE_PATH)
        critic_prompt = _read_file(critic_prompt_file)

        result = template.replace("{{CRITIC_PROMPT}}", critic_prompt)

        _write_file_atomic(self.prompt_file, result)
        logger.info("Critic prompt generated: %s", self.prompt_file)

    def _generate_decompose_prompt(self) -> None:
        """Generate prompt for decompose phase.

        Uses generate-decompose-prompt.md template with spec content.
        """
        template = _read_file(DECOMPOSE_TEMPLATE_PATH)
        spec_content = _read_file(self.spec_path)

        result = template.replace("{{SPEC_CONTENT}}", spec_content)
        result = result.replace("{{SPEC_PATH}}", self.spec_path)

        _write_file_atomic(self.prompt_file, result)
        logger.info(
            "Decompose prompt generated: %s", self.prompt_file
        )

    def _generate_evaluate_prompt(self) -> None:
        """Generate prompt for evaluate phase.

        Uses evaluate-prompt.md template with spec content.
        """
        template = _read_file(EVALUATE_TEMPLATE_PATH)
        spec_content = _read_file(self.spec_path)

        result = template.replace("{{SPEC_CONTENT}}", spec_content)
        result = result.replace("{{SPEC_PATH}}", self.spec_path)

        _write_file_atomic(self.prompt_file, result)
        logger.info(
            "Evaluate prompt generated: %s", self.prompt_file
        )

    def _resolve_mode(self, spec_content: str) -> str:
        """Determine the execution mode.

        Priority: constructor override > spec header > queue entry > default.

        Args:
            spec_content: Raw spec file content.

        Returns:
            One of: execute, challenge, discover, generate.
        """
        # 1. Constructor override
        if self.mode and self.mode in VALID_MODES:
            return self.mode

        mode = "execute"

        # 2. Try queue entry JSON (backward compat)
        queue_entry_file = os.path.join(
            self.queue_dir, f"{self.spec_id}.json"
        )
        if os.path.isfile(queue_entry_file):
            try:
                with open(
                    queue_entry_file, encoding="utf-8"
                ) as f:
                    entry = json.load(f)
                mode = entry.get("mode", "execute") or "execute"
            except (json.JSONDecodeError, OSError):
                pass

        # 3. Spec header override: **Mode:** <word>
        mode_match = re.search(
            r"^\*\*Mode:\*\*\s*(\w+)",
            spec_content,
            re.MULTILINE,
        )
        if mode_match:
            spec_mode = mode_match.group(1).strip().lower()
            if spec_mode in VALID_MODES:
                mode = spec_mode

        return mode

    def _load_mode_fragment(self, mode: str) -> str:
        """Load the mode instruction fragment from templates/modes/.

        Falls back to execute.md if the requested mode file is missing.

        Args:
            mode: Mode name (execute, challenge, discover, generate).

        Returns:
            Mode fragment text with experiment budget placeholders filled.
        """
        mode_file = os.path.join(MODES_DIR, f"{mode}.md")
        if os.path.isfile(mode_file):
            fragment = _read_file(mode_file)
        else:
            fallback = os.path.join(MODES_DIR, "execute.md")
            if os.path.isfile(fallback):
                fragment = _read_file(fallback)
            else:
                fragment = (
                    "## Mode: Execute\n\n"
                    "Execute the current task as specified.\n"
                )

        # Handle experiment budget placeholders
        budget_text = self._get_experiment_budget_text()
        fragment = fragment.replace(
            "{{EXPERIMENT_BUDGET}}", budget_text
        )
        fragment = fragment.replace(
            "{{QUEUE_ID}}", self.spec_id
        )

        return fragment

    def _get_experiment_budget_text(self) -> str:
        """Get the experiment budget text from queue entry JSON.

        Returns:
            Budget description string.
        """
        queue_entry_file = os.path.join(
            self.queue_dir, f"{self.spec_id}.json"
        )
        max_budget = 0
        used_budget = 0

        if os.path.isfile(queue_entry_file):
            try:
                with open(
                    queue_entry_file, encoding="utf-8"
                ) as f:
                    entry = json.load(f)
                max_budget = entry.get(
                    "max_experiment_invocations", 0
                )
                used_budget = entry.get(
                    "experiment_invocations_used", 0
                )
            except (json.JSONDecodeError, OSError):
                pass

        remaining = max(0, max_budget - used_budget)

        if max_budget == 0:
            return "0. Experiments are disabled in this mode."
        if remaining == 0:
            return (
                "EXHAUSTED. Do not propose alternatives. "
                "Implement per spec."
            )
        return (
            f"{remaining} remaining "
            f"({used_budget} of {max_budget} used)"
        )

    def _load_project_context(self) -> str:
        """Load project context files if a project name is set.

        Reads context.md and research.md from
        ~/.boi/projects/{project}/ if they exist.

        Returns:
            Project context string, or empty string.
        """
        if not self.project:
            return ""

        projects_dir = os.path.join(
            self.state_dir, "projects"
        )
        context_file = os.path.join(
            projects_dir, self.project, "context.md"
        )
        research_file = os.path.join(
            projects_dir, self.project, "research.md"
        )

        parts: list[str] = []
        if os.path.isfile(context_file):
            parts.append(_read_file(context_file).rstrip())
        if os.path.isfile(research_file):
            parts.append(_read_file(research_file).rstrip())

        if parts:
            return (
                "## Project Context\n\n" + "\n\n".join(parts)
            )
        return ""

    def _generate_bash_run_script(self) -> None:
        """Generate the bash run script that executes inside tmux.

        The run script:
        1. Records start time
        2. Runs Claude with the prompt
        3. Records end time
        4. Counts post-iteration tasks
        5. Calculates deltas
        6. Writes iteration metadata JSON
        7. Writes exit code file
        """
        pre_pending = self.pre_counts.get("pending", 0)
        pre_done = self.pre_counts.get("done", 0)
        pre_skipped = self.pre_counts.get("skipped", 0)
        pre_total = self.pre_counts.get("total", 0)

        # Per-task model routing: parse **Model:** from first PENDING task
        task_model = None
        if self.phase == "execute" and os.path.isfile(self.spec_path):
            spec_content = _read_file(self.spec_path)
            # Find the first PENDING task block
            pending_match = re.search(
                r'(### t-\d+:.*?\nPENDING\b.*?)(?=\n### t-|\Z)',
                spec_content,
                re.DOTALL,
            )
            if pending_match:
                task_model = parse_task_model(pending_match.group(1))

        model_for_cost, _ = (
            task_model
            if task_model
            else self._model_routing.get(
                self.phase, ("claude-sonnet-4-6", "medium")
            )
        )

        script = f"""\
#!/bin/bash
# Auto-generated BOI worker run script for iteration {self.iteration}.
# Runs inside a tmux session. Do not edit manually.
set -uo pipefail

# ── Config (baked in at generation time) ──────────────────────────────────
_BOI_SCRIPT_DIR="{SCRIPT_DIR}"
_SPEC_PATH="{self.spec_path}"
_QUEUE_ID="{self.spec_id}"
_ITERATION="{self.iteration}"
_LOG_FILE="{self.log_file}"
_EXIT_FILE="{self.exit_file}"
_ITERATION_FILE="{self.iteration_file}"
_WORKTREE_PATH="{self.worktree}"
_PROMPT_FILE="{self.prompt_file}"
_PRE_PENDING={pre_pending}
_PRE_DONE={pre_done}
_PRE_SKIPPED={pre_skipped}
_PRE_TOTAL={pre_total}

# ── Record start time ────────────────────────────────────────────────────
_START_TIME=$(date +%s)
_START_ISO=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

# ── Run Claude (model routing: phase → model + effort) ──────────────────
cd "${{_WORKTREE_PATH}}"
{self._claude_cmd(model_override=task_model)} > "${{_LOG_FILE}}" 2>&1
_CLAUDE_EXIT=$?

# ── Record end time ──────────────────────────────────────────────────────
_END_TIME=$(date +%s)
_DURATION=$((_END_TIME - _START_TIME))

# ── Count post-iteration tasks ───────────────────────────────────────────
_POST_COUNTS=$(BOI_SCRIPT_DIR="${{_BOI_SCRIPT_DIR}}" python3 - "${{_SPEC_PATH}}" <<'PYEOF'
import sys, os
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.spec_parser import count_boi_tasks
counts = count_boi_tasks(sys.argv[1])
print(f"{{counts['pending']}} {{counts['done']}} {{counts['skipped']}} {{counts['total']}}")
PYEOF
)

_POST_PENDING=$(echo "${{_POST_COUNTS}}" | awk '{{print $1}}')
_POST_DONE=$(echo "${{_POST_COUNTS}}" | awk '{{print $2}}')
_POST_SKIPPED=$(echo "${{_POST_COUNTS}}" | awk '{{print $3}}')
_POST_TOTAL=$(echo "${{_POST_COUNTS}}" | awk '{{print $4}}')

# ── Calculate deltas ─────────────────────────────────────────────────────
_TASKS_COMPLETED=$((_POST_DONE - _PRE_DONE))
_TASKS_ADDED=$((_POST_TOTAL - _PRE_TOTAL))
_TASKS_SKIPPED_DELTA=$((_POST_SKIPPED - _PRE_SKIPPED))

# Clamp to zero
if [[ ${{_TASKS_COMPLETED}} -lt 0 ]]; then _TASKS_COMPLETED=0; fi
if [[ ${{_TASKS_ADDED}} -lt 0 ]]; then _TASKS_ADDED=0; fi
if [[ ${{_TASKS_SKIPPED_DELTA}} -lt 0 ]]; then _TASKS_SKIPPED_DELTA=0; fi

# ── Estimate token usage and cost ────────────────────────────────────────
_COST_MODEL="{model_for_cost}"
_OUTPUT_CHARS=$(wc -c < "${{_LOG_FILE}}" 2>/dev/null || echo 0)
_INPUT_CHARS=$(wc -c < "${{_PROMPT_FILE}}" 2>/dev/null || echo 0)
_OUTPUT_TOKENS=$((_OUTPUT_CHARS / 4))
_INPUT_TOKENS=$((_INPUT_CHARS / 4))

# ── Write iteration metadata ─────────────────────────────────────────────
BOI_SCRIPT_DIR="${{_BOI_SCRIPT_DIR}}" python3 - \\
    "${{_ITERATION_FILE}}" \\
    "${{_QUEUE_ID}}" \\
    "${{_ITERATION}}" \\
    "${{_CLAUDE_EXIT}}" \\
    "${{_DURATION}}" \\
    "${{_START_ISO}}" \\
    "${{_PRE_PENDING}}" "${{_PRE_DONE}}" "${{_PRE_SKIPPED}}" "${{_PRE_TOTAL}}" \\
    "${{_POST_PENDING}}" "${{_POST_DONE}}" "${{_POST_SKIPPED}}" "${{_POST_TOTAL}}" \\
    "${{_TASKS_COMPLETED}}" "${{_TASKS_ADDED}}" "${{_TASKS_SKIPPED_DELTA}}" \\
    "${{_COST_MODEL}}" "${{_INPUT_TOKENS}}" "${{_OUTPUT_TOKENS}}" <<'PYEOF'
import json, sys, os

_MODEL_PRICING = {{
    "claude-opus-4-6":   (15.0, 75.0),
    "claude-sonnet-4-6": (3.0, 15.0),
    "claude-haiku-4-5":  (1.0, 5.0),
}}

target = sys.argv[1]
_cost_model = sys.argv[18]
_input_tokens = int(sys.argv[19])
_output_tokens = int(sys.argv[20])
_price_in, _price_out = _MODEL_PRICING.get(_cost_model, (3.0, 15.0))
_estimated_cost = (_input_tokens * _price_in + _output_tokens * _price_out) / 1_000_000

data = {{
    "queue_id": sys.argv[2],
    "iteration": int(sys.argv[3]),
    "exit_code": int(sys.argv[4]),
    "duration_seconds": int(sys.argv[5]),
    "started_at": sys.argv[6],
    "pre_counts": {{
        "pending": int(sys.argv[7]),
        "done": int(sys.argv[8]),
        "skipped": int(sys.argv[9]),
        "total": int(sys.argv[10]),
    }},
    "post_counts": {{
        "pending": int(sys.argv[11]),
        "done": int(sys.argv[12]),
        "skipped": int(sys.argv[13]),
        "total": int(sys.argv[14]),
    }},
    "tasks_completed": int(sys.argv[15]),
    "tasks_added": int(sys.argv[16]),
    "tasks_skipped": int(sys.argv[17]),
    "model": _cost_model,
    "estimated_input_tokens": _input_tokens,
    "estimated_output_tokens": _output_tokens,
    "estimated_cost_usd": round(_estimated_cost, 6),
}}

tmp = target + ".tmp"
with open(tmp, "w") as f:
    json.dump(data, f, indent=2)
    f.write("\\n")
os.rename(tmp, target)
PYEOF

# ── Write exit code ──────────────────────────────────────────────────────
echo "${{_CLAUDE_EXIT}}" > "${{_EXIT_FILE}}"
"""

        _write_file_atomic(self.run_script, script)
        os.chmod(self.run_script, 0o755)
        logger.info("Run script generated: %s", self.run_script)

    def launch_tmux(self) -> int:
        """Launch the run script in a detached tmux session.

        Kills any stale session with the same name, removes stale
        exit file, launches a new detached session, and retrieves
        the pane PID.

        Returns:
            0 on success, non-zero on failure.
        """
        # Kill stale session if it exists
        if self._tmux_session_exists():
            logger.warning(
                "Stale tmux session '%s' found, killing it.",
                self.tmux_session,
            )
            self._kill_tmux_session()

        # Remove stale exit file
        if os.path.exists(self.exit_file):
            os.remove(self.exit_file)

        # Launch new detached session
        try:
            subprocess.run(
                [
                    "tmux", "-L", TMUX_SOCKET,
                    "new-session", "-d",
                    "-s", self.tmux_session,
                    "bash", self.run_script,
                ],
                check=True,
                capture_output=True,
                text=True,
            )
        except subprocess.CalledProcessError as e:
            logger.error(
                "Failed to create tmux session: %s", e.stderr
            )
            return 1

        # Brief pause for tmux to initialize the pane
        time.sleep(1)

        # Get the PID of the bash process inside the tmux pane
        try:
            result = subprocess.run(
                [
                    "tmux", "-L", TMUX_SOCKET,
                    "list-panes",
                    "-t", self.tmux_session,
                    "-F", "#{pane_pid}",
                ],
                check=True,
                capture_output=True,
                text=True,
            )
            pane_pid = result.stdout.strip()
        except subprocess.CalledProcessError:
            pane_pid = ""

        if not pane_pid:
            logger.error(
                "Failed to get PID from tmux session."
            )
            return 1

        # Write PID file atomically
        pid_file = os.path.join(
            self.queue_dir, f"{self.spec_id}.pid"
        )
        _write_file_atomic(pid_file, pane_pid)

        logger.info(
            "Worker launched: tmux session '%s', PID %s",
            self.tmux_session,
            pane_pid,
        )
        logger.info("Log file: %s", self.log_file)
        return 0

    def wait_for_tmux(self) -> int:
        """Wait for the tmux session to finish.

        Polls tmux has-session at TMUX_POLL_INTERVAL. If
        timeout_seconds is set, raises TimeoutError when exceeded.

        Returns:
            Exit code read from the .exit file (default 1).

        Raises:
            TimeoutError: If timeout_seconds elapsed.
        """
        start = time.monotonic()

        while self._tmux_session_exists():
            if (
                self.timeout_seconds is not None
                and time.monotonic() - start
                > self.timeout_seconds
            ):
                raise TimeoutError(
                    f"Worker timed out after "
                    f"{self.timeout_seconds}s"
                )
            time.sleep(TMUX_POLL_INTERVAL)

        # Read exit code from .exit file written by the run script
        return self._read_exit_code()

    def post_process(self) -> None:
        """Post-process after tmux session completes.

        Counts tasks from the spec file, computes deltas against
        pre-iteration counts, and writes iteration metadata JSON.
        """
        post_counts = count_boi_tasks(self.spec_path)

        pre_pending = self.pre_counts.get("pending", 0)
        pre_done = self.pre_counts.get("done", 0)
        pre_skipped = self.pre_counts.get("skipped", 0)
        pre_total = self.pre_counts.get("total", 0)

        post_pending = post_counts.get("pending", 0)
        post_done = post_counts.get("done", 0)
        post_skipped = post_counts.get("skipped", 0)
        post_total = post_counts.get("total", 0)

        tasks_completed = max(0, post_done - pre_done)
        tasks_added = max(0, post_total - pre_total)
        tasks_skipped = max(0, post_skipped - pre_skipped)

        exit_code = self._read_exit_code()

        metadata = {
            "queue_id": self.spec_id,
            "iteration": self.iteration,
            "exit_code": exit_code,
            "duration_seconds": 0,
            "started_at": "",
            "pre_counts": {
                "pending": pre_pending,
                "done": pre_done,
                "skipped": pre_skipped,
                "total": pre_total,
            },
            "post_counts": {
                "pending": post_pending,
                "done": post_done,
                "skipped": post_skipped,
                "total": post_total,
            },
            "tasks_completed": tasks_completed,
            "tasks_added": tasks_added,
            "tasks_skipped": tasks_skipped,
        }

        _write_file_atomic(
            self.iteration_file,
            json.dumps(metadata, indent=2) + "\n",
        )
        logger.info(
            "Iteration metadata written: %s "
            "(completed=%d, added=%d, skipped=%d)",
            self.iteration_file,
            tasks_completed,
            tasks_added,
            tasks_skipped,
        )

    # ── Internal helpers ──────────────────────────────────────────

    def _tmux_session_exists(self) -> bool:
        """Check if the tmux session is still alive."""
        result = subprocess.run(
            [
                "tmux", "-L", TMUX_SOCKET,
                "has-session", "-t", self.tmux_session,
            ],
            capture_output=True,
        )
        return result.returncode == 0

    def _kill_tmux_session(self) -> None:
        """Kill the tmux session if it exists."""
        subprocess.run(
            [
                "tmux", "-L", TMUX_SOCKET,
                "kill-session", "-t", self.tmux_session,
            ],
            capture_output=True,
        )

    def _read_exit_code(self) -> int:
        """Read exit code from the .exit file.

        Returns:
            The exit code, or 1 if file is missing/unreadable.
        """
        try:
            content = _read_file(self.exit_file).strip()
            return int(content)
        except (FileNotFoundError, ValueError):
            return 1


# ── Helpers ──────────────────────────────────────────────────────────────


def _read_file(path: str) -> str:
    """Read a file and return its contents as a string."""
    with open(path, encoding="utf-8") as f:
        return f.read()


def _write_file(path: str, content: str) -> None:
    """Write content to a file (non-atomic)."""
    with open(path, "w", encoding="utf-8") as f:
        f.write(content)


def _write_file_atomic(path: str, content: str) -> None:
    """Write content to a file atomically via tmp + rename."""
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        f.write(content)
    os.rename(tmp, path)


# ── CLI entry point ──────────────────────────────────────────────────────


def _load_hooks_from_env() -> Optional[WorkerHooks]:
    """Load WorkerHooks from the BOI_HOOKS_MODULE env var.

    If BOI_HOOKS_MODULE is set (e.g. "overlay.hooks"), dynamically
    import that module and call its create_hooks() factory function.

    Returns:
        A WorkerHooks instance, or None if not configured or on error.
    """
    module_name = os.environ.get("BOI_HOOKS_MODULE", "")
    if not module_name:
        return None

    import importlib

    try:
        mod = importlib.import_module(module_name)
        factory = getattr(mod, "create_hooks", None)
        if factory is None:
            logger.warning(
                "BOI_HOOKS_MODULE '%s' has no create_hooks()",
                module_name,
            )
            return None
        hooks = factory()
        logger.info("Loaded hooks from %s", module_name)
        return hooks
    except Exception:
        logger.exception(
            "Failed to load hooks from %s", module_name
        )
        return None


def main() -> int:
    """CLI entry point for the BOI worker.

    Parses arguments, creates a Worker, and runs one iteration.

    Returns:
        Exit code (0 = success, non-zero = failure).
    """
    parser = argparse.ArgumentParser(
        description="BOI worker: execute one iteration of a spec."
    )
    parser.add_argument(
        "spec_id", help="Queue ID (e.g. q-001)"
    )
    parser.add_argument(
        "worktree", help="Path to the worker's worktree"
    )
    parser.add_argument(
        "spec_path", help="Path to the spec file (queue copy)"
    )
    parser.add_argument(
        "iteration", type=int, help="Iteration number"
    )
    parser.add_argument(
        "--phase",
        choices=sorted(VALID_PHASES),
        default="execute",
        help="Phase to execute (default: execute)",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=None,
        help="Timeout in seconds (worker self-terminates)",
    )
    parser.add_argument(
        "--mode",
        choices=sorted(VALID_MODES),
        default=None,
        help="Mode override",
    )
    parser.add_argument(
        "--project",
        default=None,
        help="Project name for context injection",
    )
    parser.add_argument(
        "--state-dir",
        default=None,
        help="State directory (default: ~/.boi)",
    )

    args = parser.parse_args()

    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(name)s %(levelname)s %(message)s",
    )

    # Load hooks from overlay if BOI_HOOKS_MODULE is set.
    # The env var names a Python module with a create_hooks()
    # factory function (e.g. "overlay.hooks").
    hooks = _load_hooks_from_env()

    worker = Worker(
        spec_id=args.spec_id,
        worktree=args.worktree,
        spec_path=args.spec_path,
        iteration=args.iteration,
        phase=args.phase,
        timeout_seconds=args.timeout,
        mode=args.mode,
        project=args.project,
        state_dir=args.state_dir,
        hooks=hooks,
    )
    return worker.run()


if __name__ == "__main__":
    sys.exit(main())
