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
import shutil
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

sys.path.insert(0, str(Path(__file__).resolve().parent))

from lib.phases import PhaseConfig
from lib.runtime import ClaudeRuntime, Runtime, get_runtime, resolve_runtime, load_context_root
from lib.spec_parser import count_boi_tasks, parse_spec
from lib.workspace_guard import WorkspaceBoundaryChecker, diff_status, snapshot_git_status


class WorkerHooks:
    """Extension point for injecting context into worker prompts.

    Subclass this and pass an instance to Worker(hooks=...) to inject
    additional context (e.g. hex context, project metadata) into the
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
REVIEW_TEMPLATE_PATH = os.path.join(
    SCRIPT_DIR, "templates", "review-worker-prompt.md"
)

VALID_MODES = {"execute", "challenge", "discover", "generate"}
VALID_PHASES = {"execute", "task-verify", "evaluate", "decompose", "review", "plan-critique", "code-review"}
TMUX_POLL_INTERVAL = 5  # seconds between tmux has-session polls
TMUX_SOCKET = "boi"     # tmux socket name (-L flag)
NO_TMUX = os.environ.get("BOI_NO_TMUX", "") == "1"  # headless mode for Docker/CI

logger = logging.getLogger("boi.worker")


def parse_task_model(task_block: str) -> Optional[str]:
    """Parse **Model:** field from a task block.

    Returns the raw alias or model ID string (e.g. 'opus', 'claude-opus-4-6'),
    or None if no Model field found. Alias resolution is handled by the runtime.
    """
    # Match **Model:** only at the start of a line (field, not prose)
    match = re.search(r'^\*\*Model:\*\*\s*(\S+)', task_block, re.MULTILINE)
    if not match:
        return None
    name = match.group(1).lower().strip()
    # Reject names that are clearly not model IDs (punctuation, backticks)
    if not re.match(r'^[a-z0-9]', name):
        return None
    return name


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
        self.context_root = load_context_root(self.state_dir)

        # Load context_root for --add-dir injection
        self.context_root = load_context_root(self.state_dir)

        if worker_id is None:
            self.worker_id = os.environ.get("WORKER_ID", "")
        else:
            self.worker_id = worker_id

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

        # Runtime: resolved from spec + config in run(). Default is None until run() sets it.
        self.runtime: Optional[Runtime] = None

        # Phase config: loaded from phases/*.phase.toml in run().
        # Single source of truth for model, effort, runtime per phase.
        self.phase_config: Optional['PhaseConfig'] = None

    def _build_exec_cmd(self, model_override: Optional[str] = None) -> str:
        """Build the worker execution command using the runtime abstraction.

        Priority: per-task **Model:** field (alias or full ID) > phase-based default.

        model_override: raw alias (e.g. 'opus') or full model ID, or None.

        The returned string is interpolated via {self._build_exec_cmd()} inside
        the outer f-string, so bash variables must use ${{var}} to survive
        the outer f-string's brace resolution ({{x}} → {x} → ${x} in bash).
        """
        rt = self.runtime if self.runtime is not None else ClaudeRuntime()
        if model_override:
            model = model_override
            effort = "medium"  # runtime derives correct effort from alias
        else:
            pc = self.phase_config or PhaseConfig(name=self.phase, prompt_template="", approve_signal="")
            model = pc.model
            effort = pc.effort
        # NOTE: The outer f-string in generate_run_script() will resolve
        # {{_PROMPT_FILE}} → {_PROMPT_FILE}. But since _build_exec_cmd()'s
        # return is already evaluated by the time the outer f-string runs,
        # we need the literal bash: ${_PROMPT_FILE}.
        prompt_ref = '${_PROMPT_FILE}'
        context_dirs = [self.context_root] if self.context_root else None
        return rt.build_exec_cmd(prompt_ref, model, effort, context_dirs=context_dirs)

    def _resolve_execute_model(self, task_block: str) -> Optional[str]:
        """Return code_model if task content matches code keywords, else None.

        Uses phase_config.code_model as the override model. If no code_model
        is configured, returns None (caller uses phase default).
        """
        if not self.phase_config or not self.phase_config.code_model:
            return None
        code_keywords = [
            "implement", "refactor", "fix", "test",
            "function", "class", "module", "API", "endpoint", "bug",
            "code", "script", "program", "algorithm", "patch",
            "debug", "error", "exception", "compile", "run",
        ]
        task_lower = task_block.lower()
        for kw in code_keywords:
            if kw in task_lower:
                return self.phase_config.code_model
        return None


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

        # If no pending tasks and we're in execute phase, run outcome verification
        # then E2E phase before marking COMPLETED.
        if pre_pending == 0 and self.phase == "execute":
            spec_content = _read_file(self.spec_path)

            # Outcome verification: runs before E2E
            outcomes_passed = self._run_outcome_verification(spec_content)
            if not outcomes_passed:
                logger.warning(
                    "Outcome verification FAILED for %s — last task reset to PENDING.",
                    self.spec_id,
                )
                _write_file(self.exit_file, "1")
                return 1

            # Re-read spec after potential outcome-driven reset
            spec_content = _read_file(self.spec_path)
            e2e_passed = self._run_e2e_phase(spec_content)
            if e2e_passed:
                logger.info(
                    "No PENDING tasks in spec and E2E phase passed. Exiting with success."
                )
                _write_file(self.exit_file, "0")
                return 0
            else:
                logger.warning(
                    "E2E phase FAILED for %s — last task reset to PENDING.", self.spec_id
                )
                _write_file(self.exit_file, "1")
                return 1

        # Read spec once; pass to all helpers that need it
        spec_content = _read_file(self.spec_path)

        # Load phase config from .phase.toml (single source of truth for model/runtime)
        if self.phase_config is None:
            from lib.phases import load_phase
            phase_file = os.path.join(SCRIPT_DIR, "phases", f"{self.phase}.phase.toml")
            if os.path.isfile(phase_file):
                self.phase_config = load_phase(phase_file)

        # Resolve runtime: phase config > spec header > global config > default (claude)
        runtime_name = self.phase_config.runtime if self.phase_config else resolve_runtime(self.state_dir, spec_content)
        self.runtime = get_runtime(runtime_name)

        # Generate prompt and run script
        self.generate_run_script(spec_content)

        # Boundary checker: snapshot main repo state before worker runs
        boundary = WorkspaceBoundaryChecker(
            worktree_path=self.worktree,
            spec_id=self.spec_id,
            worker_id=self.worker_id,
        )
        boundary.snapshot_before()

        # Parse **Target:** from spec to snapshot the TARGET REPO (not BOI worktree)
        _target_match = re.search(
            r'^\*\*Target:\*\*\s*(.+)$', spec_content, re.MULTILINE
        )
        _target_repo = (
            os.path.expanduser(_target_match.group(1).strip()) if _target_match else None
        )
        _track_target = bool(_target_repo and os.path.isdir(_target_repo))
        pre_target_status = snapshot_git_status(_target_repo) if _track_target else set()

        # Launch worker: tmux (default) or direct subprocess (BOI_NO_TMUX=1)
        if NO_TMUX:
            rc = self.launch_direct()
        else:
            rc = self.launch_tmux()
        if rc != 0:
            logger.error("Failed to launch worker session.")
            return 1

        try:
            if NO_TMUX:
                exit_code = self._direct_exit_code
            else:
                exit_code = self.wait_for_tmux()
        except TimeoutError:
            logger.warning(
                "Worker timed out after %s seconds.",
                self.timeout_seconds,
            )
            exit_code = 124
            _write_file(self.exit_file, "124")
            self._kill_tmux_session()

        # Boundary check: detect leaks before reporting results
        boundary.check_after()

        # Write changed-files manifest for scoped auto-commit
        try:
            if _track_target:
                post_target_status = snapshot_git_status(_target_repo)
                new_files = diff_status(pre_target_status, post_target_status)
                if new_files:
                    manifest_path = os.path.join(
                        self.queue_dir, f"{self.spec_id}.changed-files"
                    )
                    existing: set = set()
                    if os.path.isfile(manifest_path):
                        with open(manifest_path) as f:
                            existing = {l.strip() for l in f if l.strip()}
                    all_files = existing | set(new_files)
                    with open(manifest_path, "w") as f:
                        f.write("\n".join(sorted(all_files)) + "\n")
                    logger.info(
                        "changed-files manifest updated: %s (%d files)",
                        manifest_path,
                        len(all_files),
                    )
        except Exception:
            logger.exception(
                "Failed to write changed-files manifest for %s", self.spec_id
            )

        # Collect and preserve all outputs before any potential worktree cleanup.
        try:
            self.collect_outputs()
        except Exception:
            logger.exception(
                "collect_outputs failed for %s — worktree preserved, not deleted",
                self.spec_id,
            )

        self.post_process()
        return exit_code

    def generate_run_script(self, spec_content: str) -> None:
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
        self._generate_prompt(spec_content)
        self._generate_bash_run_script(spec_content)

    def _generate_prompt(self, spec_content: str) -> None:
        """Generate the prompt file based on the current phase."""
        if self.phase == "task-verify":
            self._generate_critic_prompt()
        elif self.phase == "decompose":
            self._generate_decompose_prompt(spec_content)
        elif self.phase == "evaluate":
            self._generate_evaluate_prompt(spec_content)
        elif self.phase == "review":
            self._generate_review_prompt(spec_content)
        else:
            self._generate_execute_prompt(spec_content)

    def _build_workspace_header(self, spec_content: str) -> str:
        """Build a compact workspace context header to reduce spec corruption.

        Reads workspace_header_enabled from config.json. Returns empty string
        if disabled or if parsing fails.
        """
        config_path = os.path.join(self.state_dir, "config.json")
        try:
            with open(config_path, "r") as f:
                cfg = json.load(f)
            if not cfg.get("workspace_header_enabled", True):
                return ""
        except (FileNotFoundError, json.JSONDecodeError, OSError):
            pass  # Default to enabled if config unreadable

        try:
            tasks = list(parse_spec(spec_content))
            total = len([t for t in tasks if t.status != "SUPERSEDED"])
            done = len([t for t in tasks if t.status in ("DONE", "SKIPPED")])
            pending_tasks = [t for t in tasks if t.status == "PENDING"]
            next_id = pending_tasks[0].id if pending_tasks else "none"
        except Exception:
            return ""

        return (
            "> **WORKSPACE GUARD** — Format: `### t-N: Title` then `PENDING`/`DONE` on its own line. "
            f"Tasks: {done}/{total} done. Next PENDING: {next_id}.\n"
            "> Do NOT alter DONE tasks. Do NOT add prose between headings and status lines. "
            "Do NOT duplicate task sections.\n\n"
        )

    def _generate_execute_prompt(self, spec_content: str) -> None:
        """Generate prompt for execute phase.

        Loads the worker-prompt.md template, determines mode from
        spec header or constructor arg, loads mode fragment, loads
        project context, and replaces all placeholders.
        """
        template = _read_file(TEMPLATE_PATH)
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
        result = result.replace(
            "{{WORKSPACE_HEADER}}", self._build_workspace_header(spec_content)
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

    def _generate_decompose_prompt(self, spec_content: str) -> None:
        """Generate prompt for decompose phase.

        Uses generate-decompose-prompt.md template with spec content.
        """
        template = _read_file(DECOMPOSE_TEMPLATE_PATH)

        result = template.replace("{{SPEC_CONTENT}}", spec_content)
        result = result.replace("{{SPEC_PATH}}", self.spec_path)

        _write_file_atomic(self.prompt_file, result)
        logger.info(
            "Decompose prompt generated: %s", self.prompt_file
        )

    def _generate_evaluate_prompt(self, spec_content: str) -> None:
        """Generate prompt for evaluate phase.

        Uses evaluate-prompt.md template with spec content.
        """
        template = _read_file(EVALUATE_TEMPLATE_PATH)

        result = template.replace("{{SPEC_CONTENT}}", spec_content)
        result = result.replace("{{SPEC_PATH}}", self.spec_path)

        _write_file_atomic(self.prompt_file, result)
        logger.info(
            "Evaluate prompt generated: %s", self.prompt_file
        )

    def _generate_review_prompt(self, spec_content: str) -> None:
        """Generate prompt for review phase.

        Uses review-worker-prompt.md template with spec content and
        the git diff from the target repo (HEAD~1..HEAD) so the
        reviewer sees what the execute phase actually changed.
        """
        template = _read_file(REVIEW_TEMPLATE_PATH)

        # Resolve target repo from spec header (**Target:** field).
        target_match = re.search(
            r'^\*\*Target:\*\*\s*(.+)$', spec_content, re.MULTILINE
        )
        target_repo = (
            os.path.expanduser(target_match.group(1).strip())
            if target_match
            else None
        )

        git_diff = ""
        if target_repo and os.path.isdir(target_repo):
            try:
                # Try committed diff first (HEAD~1..HEAD).
                diff_result = subprocess.run(
                    ["git", "-C", target_repo, "diff", "HEAD~1", "HEAD"],
                    capture_output=True,
                    text=True,
                    timeout=30,
                )
                git_diff = diff_result.stdout.strip()
            except (subprocess.TimeoutExpired, FileNotFoundError, OSError) as exc:
                logger.warning(
                    "Review phase: could not get commit diff for %s: %s",
                    self.spec_id,
                    exc,
                )

            if not git_diff:
                # Fall back to staged+unstaged changes vs HEAD.
                try:
                    diff_result = subprocess.run(
                        ["git", "-C", target_repo, "diff", "HEAD"],
                        capture_output=True,
                        text=True,
                        timeout=30,
                    )
                    git_diff = diff_result.stdout.strip()
                except (subprocess.TimeoutExpired, FileNotFoundError, OSError) as exc:
                    logger.warning(
                        "Review phase: could not get HEAD diff for %s: %s",
                        self.spec_id,
                        exc,
                    )
        else:
            logger.warning(
                "Review phase: no target repo found for %s; GIT_DIFF will be empty",
                self.spec_id,
            )

        result = template.replace("{{SPEC_CONTENT}}", spec_content)
        result = result.replace("{{GIT_DIFF}}", git_diff or "(no diff available)")

        _write_file_atomic(self.prompt_file, result)
        logger.info(
            "Review prompt generated: %s (diff length: %d chars)",
            self.prompt_file,
            len(git_diff),
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

    def _generate_bash_run_script(self, spec_content: str) -> None:
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
        rt = self.runtime if self.runtime is not None else ClaudeRuntime()
        task_model_alias = None
        first_pending_task_block = None
        if self.phase == "execute":
            # Split by task headings, find first with PENDING status on line 2
            task_blocks = re.split(r'(?=^### t-\d+:)', spec_content, flags=re.MULTILINE)
            for block in task_blocks:
                block = block.strip()
                if not block:
                    continue
                lines = block.split("\n")
                if len(lines) >= 2 and lines[1].strip() == "PENDING":
                    task_model_alias = parse_task_model(block)
                    first_pending_task_block = block
                    break

        # Heuristic: if no explicit Model field, decide based on task content
        if task_model_alias is None and self.phase == "execute" and first_pending_task_block:
            task_model_alias = self._resolve_execute_model(first_pending_task_block)

        if task_model_alias:
            model_for_cost = rt.model_id(task_model_alias)
        else:
            pc = self.phase_config or PhaseConfig(name=self.phase, prompt_template="", approve_signal="")
            model_for_cost = rt.model_id(pc.model)
        price_in, price_out = rt.cost_per_token(model_for_cost)

        script = f"""\
#!/bin/bash
# Auto-generated BOI worker run script for iteration {self.iteration}.
# Runs inside a tmux session. Do not edit manually.
set -uo pipefail

# Ensure PATH includes common tool locations (needed when launched via launchd)
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

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

# ── Run worker (model routing: phase → model + effort) ──────────────────
cd "${{_WORKTREE_PATH}}"
{self._build_exec_cmd(model_override=task_model_alias)} > "${{_LOG_FILE}}" 2>&1
_AGENT_EXIT=$?

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
    "${{_AGENT_EXIT}}" \\
    "${{_DURATION}}" \\
    "${{_START_ISO}}" \\
    "${{_PRE_PENDING}}" "${{_PRE_DONE}}" "${{_PRE_SKIPPED}}" "${{_PRE_TOTAL}}" \\
    "${{_POST_PENDING}}" "${{_POST_DONE}}" "${{_POST_SKIPPED}}" "${{_POST_TOTAL}}" \\
    "${{_TASKS_COMPLETED}}" "${{_TASKS_ADDED}}" "${{_TASKS_SKIPPED_DELTA}}" \\
    "${{_COST_MODEL}}" "${{_INPUT_TOKENS}}" "${{_OUTPUT_TOKENS}}" <<'PYEOF'
import json, sys, os

target = sys.argv[1]
_cost_model = sys.argv[18]
_input_tokens = int(sys.argv[19])
_output_tokens = int(sys.argv[20])
_price_in, _price_out = ({price_in}, {price_out})
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
echo "${{_AGENT_EXIT}}" > "${{_EXIT_FILE}}"
"""

        _write_file_atomic(self.run_script, script)
        os.chmod(self.run_script, 0o755)
        logger.info("Run script generated: %s", self.run_script)

    def launch_direct(self) -> int:
        """Launch the run script directly via subprocess (headless/Docker mode).

        Used when BOI_NO_TMUX=1. Runs the bash script synchronously and
        captures the exit code. No tmux involved.

        Returns:
            0 on success, non-zero on failure.
        """
        self._direct_exit_code = 1
        if os.path.exists(self.exit_file):
            os.remove(self.exit_file)

        logger.info("Launching worker directly (no tmux): %s", self.run_script)
        try:
            result = subprocess.run(
                ["bash", self.run_script],
                capture_output=True,
                text=True,
                timeout=self.timeout_seconds or 600,
                cwd=self.worktree,
            )
            logger.info("Direct worker exited with code %d", result.returncode)
            if result.stderr:
                logger.debug("Worker stderr: %s", result.stderr[:500])
        except subprocess.TimeoutExpired:
            logger.warning("Direct worker timed out after %ds", self.timeout_seconds or 600)
            self._direct_exit_code = 1
            return 0  # still "launched" successfully, timeout handled by caller
        except Exception as e:
            logger.error("Failed to run worker directly: %s", e)
            return 1

        # Read exit code from the exit file (written by the run script)
        if os.path.exists(self.exit_file):
            try:
                self._direct_exit_code = int(
                    open(self.exit_file).read().strip()
                )
            except (ValueError, OSError):
                self._direct_exit_code = result.returncode
        else:
            self._direct_exit_code = result.returncode
        return 0

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

    # ── E2E Phase ──────────────────────────────────────────────────────────────

    def _detect_web_artifacts(self, spec_content: str) -> list[str]:
        """Scan spec content and modified files for web signals."""
        signals: list[str] = []
        web_patterns = [
            "HTTPServer", "ThreadingHTTPServer", "uvicorn", "flask",
            "FastAPI", "http.server", "EventSource", "SSE",
        ]
        web_file_suffixes = ["server.py", "index.html", "app.py", "index.htm"]
        web_keywords = ["dashboard", "localhost:", "http://", "https://", "serve", " web ",
                        "index.html", "index.htm", ".html", ".htm"]

        # Check spec content for code patterns and web keywords
        for pattern in web_patterns:
            if pattern in spec_content:
                signals.append(f"pattern:{pattern}")

        for kw in web_keywords:
            if kw.lower() in spec_content.lower():
                signals.append(f"keyword:{kw}")

        # Check worktree for modified files with web-related names (fast: git diff only)
        try:
            result = subprocess.run(
                ["git", "-C", self.worktree, "diff", "--name-only", "HEAD"],
                capture_output=True, text=True, timeout=10,
            )
            for f in result.stdout.splitlines():
                f = f.strip()
                if any(f.endswith(suf) for suf in web_file_suffixes):
                    signals.append(f"file:{f}")
        except Exception:
            pass

        return signals

    def _detect_service_url(self, spec_content: str, web_signals: list[str]) -> str:
        """Auto-detect the URL to test from hex-router ROUTES, spec content, or modified files.

        Priority: hex-router routes > explicit URLs in spec > localhost port from files.
        """
        router_path = os.path.join(
            os.environ.get("AGENT_DIR", os.path.expanduser("~/hex")), ".hex", "scripts", "hex-router", "router.py"
        )
        base_host = "https://mac-mini.tailbd5748.ts.net"

        # 1. Parse hex-router ROUTES and check if any route matches spec content
        if os.path.isfile(router_path):
            try:
                router_src = Path(router_path).read_text(encoding="utf-8")
                route_re = re.compile(
                    r'^\s*\("(/\w+)"[^,]*,\s*"[^"]*",\s*(\d+)',
                    re.MULTILINE,
                )
                for m in route_re.finditer(router_src):
                    prefix = m.group(1)
                    port = m.group(2)
                    # Check if the spec mentions this route prefix or port
                    if prefix.strip("/") in spec_content.lower() or port in spec_content:
                        return f"{base_host}{prefix}/"
            except Exception:
                pass

        # 2. Scan spec content for explicit URLs
        url_re = re.compile(r'https?://[^\s\'"<>]+')
        for m in url_re.finditer(spec_content):
            url = m.group(0).rstrip(".,;)")
            if url:
                return url

        # 3. localhost:PORT from spec content
        localhost_re = re.compile(r'localhost:(\d{4,5})')
        m = localhost_re.search(spec_content)
        if m:
            return f"http://localhost:{m.group(1)}/"

        # 4. PORT = NNNN from modified server.py files
        try:
            result = subprocess.run(
                ["git", "-C", self.worktree, "diff", "--name-only", "HEAD"],
                capture_output=True, text=True, timeout=10,
            )
            for f in result.stdout.splitlines():
                f = f.strip()
                if f.endswith("server.py") or f.endswith("app.py"):
                    full_path = os.path.join(self.worktree, f)
                    if os.path.isfile(full_path):
                        src = Path(full_path).read_text(encoding="utf-8", errors="replace")
                        port_m = re.search(r'PORT\s*=\s*(\d{4,5})', src)
                        if port_m:
                            return f"http://localhost:{port_m.group(1)}/"
        except Exception:
            pass

        return ""

    def _run_e2e_phase(self, spec_content: str) -> bool:
        """E2E verification phase — runs after all tasks pass, before COMPLETED.

        Returns True to proceed to COMPLETED, False to block and reset last task.
        """
        logger.info("E2E phase: starting detection for %s", self.spec_id)

        # 1. Detect web artifacts
        web_signals = self._detect_web_artifacts(spec_content)
        if not web_signals:
            logger.info("E2E phase: not applicable (no web artifacts) for %s", self.spec_id)
            return True

        logger.info("E2E phase: web signals found: %s", web_signals)

        # 2. Detect URL
        url = self._detect_service_url(spec_content, web_signals)
        if not url:
            logger.warning(
                "E2E phase: web artifacts found but could not detect URL for %s — skipping",
                self.spec_id,
            )
            return True

        # 3. Find the verify.py script path
        e2e_guard_candidates = [
            os.path.join(
                os.environ.get("AGENT_DIR", os.path.expanduser("~/hex")), ".hex", "scripts", "e2e-guard", "verify.py"
            ),
        ]
        e2e_guard_path = next((p for p in e2e_guard_candidates if os.path.isfile(p)), None)
        if not e2e_guard_path:
            logger.warning(
                "E2E phase: verify.py not found — skipping for %s", self.spec_id
            )
            return True

        # 4. Check Playwright availability (graceful skip if missing)
        try:
            import importlib.util
            if importlib.util.find_spec("playwright") is None:
                logger.warning(
                    "E2E phase: skipped (Playwright not available) for %s", self.spec_id
                )
                return True
        except Exception:
            pass

        # 5. Run verify.py — write marker file so `boi status` shows "E2E verifying..."
        e2e_marker = os.path.join(self.queue_dir, f"{self.spec_id}.e2e-phase")
        logger.info("E2E phase: running verify.py for %s against %s", self.spec_id, url)
        try:
            _write_file(e2e_marker, url)
            result = subprocess.run(
                ["python3", e2e_guard_path, "--url", url, "--timeout", "30"],
                capture_output=True, text=True, timeout=60,
            )
        except subprocess.TimeoutExpired:
            logger.error("E2E phase: verify.py timed out for %s", self.spec_id)
            return False
        except Exception as exc:
            logger.error("E2E phase: error running verify.py for %s: %s", self.spec_id, exc)
            return True  # Don't block on runner errors
        finally:
            try:
                os.remove(e2e_marker)
            except OSError:
                pass

        if result.returncode == 0:
            logger.info("E2E phase: PASS for %s (%s)", self.spec_id, url)
            self._append_e2e_result_to_log(f"E2E phase: PASS ({url})")
            return True

        logger.warning(
            "E2E phase: FAIL for %s (%s)\n%s", self.spec_id, url, result.stdout[:2000]
        )
        self._append_e2e_result_to_log(f"E2E phase: FAIL ({url})\n{result.stdout[:2000]}")
        self._reset_last_done_task_to_pending(spec_content)
        return False

    def _append_e2e_result_to_log(self, message: str) -> None:
        """Append E2E phase result to the iteration log file."""
        try:
            os.makedirs(self.log_dir, exist_ok=True)
            with open(self.log_file, "a", encoding="utf-8") as f:
                f.write(f"\n[E2E] {message}\n")
        except Exception:
            pass

    def _reset_last_done_task_to_pending(self, spec_content: str, note: str = "") -> None:
        """Reset the last DONE task back to PENDING so the worker must re-fix it."""
        from lib.spec_parser import parse_boi_spec
        try:
            tasks = parse_boi_spec(spec_content)
            # Find the highest-numbered DONE task
            done_tasks = [t for t in tasks if t.status == "DONE"]
            if not done_tasks:
                return
            last_done = done_tasks[-1]
            # Build replacement: PENDING, optionally with a note line
            replacement = r'\1PENDING'
            if note:
                safe_note = note.replace("\\", "\\\\")
                replacement += f"\n\n{safe_note}"
            # Rewrite the spec file: change "DONE" → "PENDING" for that task's heading block
            new_content = re.sub(
                r'(###\s+' + re.escape(last_done.id) + r':.*\n)DONE(\b)',
                replacement,
                spec_content,
                count=1,
            )
            if new_content != spec_content:
                _write_file_atomic(self.spec_path, new_content)
                logger.info(
                    "Reset task %s from DONE to PENDING in %s%s",
                    last_done.id, self.spec_path,
                    f" (note: {note})" if note else "",
                )
        except Exception as exc:
            logger.error("Failed to reset task to PENDING: %s", exc)

    # ── Outcome Verification Phase ─────────────────────────────────────────────

    def _run_outcome_verification(self, spec_content: str) -> bool:
        """Run spec-level outcome verification after all tasks are DONE.

        Runs each outcome's verify command; marks outcomes PASS or FAIL.
        Returns True if all outcomes pass (or none are declared).
        Returns False and resets the last DONE task if any outcome fails.
        """
        from lib.spec_parser import parse_spec

        spec_data = parse_spec(spec_content)
        outcomes = spec_data.outcomes

        if not outcomes:
            logger.info(
                "Outcome verification: no outcomes declared for %s", self.spec_id
            )
            return True

        logger.info(
            "Outcome verification: running %d outcome(s) for %s",
            len(outcomes),
            self.spec_id,
        )

        all_pass = True
        result_lines: list[str] = []

        for outcome in outcomes:
            try:
                proc = subprocess.run(
                    outcome.verify,
                    shell=True,
                    capture_output=True,
                    text=True,
                    timeout=60,
                )
                if proc.returncode == 0:
                    outcome.status = "PASS"
                    result_lines.append(f"  ✓ {outcome.description}")
                    logger.info("Outcome PASS: %s", outcome.description)
                else:
                    outcome.status = "FAIL"
                    all_pass = False
                    detail = (proc.stdout + proc.stderr).strip()[:200]
                    result_lines.append(
                        f"  ✗ {outcome.description}"
                        + (f" — {detail}" if detail else "")
                    )
                    logger.warning(
                        "Outcome FAIL: %s — %s", outcome.description, detail
                    )
            except subprocess.TimeoutExpired:
                outcome.status = "FAIL"
                all_pass = False
                result_lines.append(
                    f"  ✗ {outcome.description} — timed out after 60s"
                )
                logger.warning("Outcome FAIL (timeout): %s", outcome.description)
            except Exception as exc:
                outcome.status = "FAIL"
                all_pass = False
                result_lines.append(
                    f"  ✗ {outcome.description} — error: {exc}"
                )
                logger.error("Outcome error: %s: %s", outcome.description, exc)

        pass_count = sum(1 for o in outcomes if o.status == "PASS")
        total = len(outcomes)
        log_msg = (
            f"Outcomes: {pass_count}/{total} passed\n" + "\n".join(result_lines)
        )
        self._append_outcome_results_to_log(log_msg)

        if not all_pass:
            failed = [o for o in outcomes if o.status == "FAIL"]
            note = (
                f"Outcome verification failed: {failed[0].description}. "
                "Fix and re-verify."
            )
            logger.warning(
                "Outcome verification: %d/%d failed for %s — resetting last task",
                total - pass_count,
                total,
                self.spec_id,
            )
            self._reset_last_done_task_to_pending(spec_content, note=note)

        return all_pass

    def _append_outcome_results_to_log(self, message: str) -> None:
        """Append outcome verification results to the iteration log file."""
        try:
            os.makedirs(self.log_dir, exist_ok=True)
            with open(self.log_file, "a", encoding="utf-8") as f:
                f.write(f"\n[OUTCOMES] {message}\n")
        except Exception:
            pass

    def collect_outputs(self) -> None:
        """Preserve all spec outputs to ~/.boi/outputs/{spec_id}/ before cleanup.

        Detects files created or modified in the worktree (git diff + untracked),
        copies them to a permanent outputs directory along with the spec file and
        a manifest.json.  Called after the agent completes and before any worktree
        cleanup so outputs are never destroyed.

        If collection fails the error is logged but the caller should NOT delete
        the worktree — better to leave a stale worktree than lose outputs.
        """
        outputs_dir = os.path.join(self.state_dir, "outputs", self.spec_id)
        files_dir = os.path.join(outputs_dir, "files")
        os.makedirs(files_dir, exist_ok=True)

        # 1. Copy the final spec file so we preserve the all-DONE state.
        if os.path.isfile(self.spec_path):
            shutil.copy2(self.spec_path, os.path.join(outputs_dir, "spec.md"))

        # 2. Detect changed/new files in worktree via git.
        changed: list[dict] = []
        try:
            r = subprocess.run(
                ["git", "-C", self.worktree, "diff", "--name-only", "HEAD"],
                capture_output=True, text=True, timeout=30,
            )
            if r.returncode == 0:
                for line in r.stdout.splitlines():
                    f = line.strip()
                    if f:
                        changed.append({"path": f, "action": "modified"})

            r = subprocess.run(
                ["git", "-C", self.worktree, "ls-files", "--others", "--exclude-standard"],
                capture_output=True, text=True, timeout=30,
            )
            if r.returncode == 0:
                for line in r.stdout.splitlines():
                    f = line.strip()
                    if f:
                        changed.append({"path": f, "action": "created"})
        except Exception:
            logger.warning("collect_outputs: git enumerate failed for %s", self.spec_id)

        # 3. Copy each changed file preserving relative directory structure.
        file_entries: list[dict] = []
        for entry in changed:
            rel = entry["path"]
            src = os.path.join(self.worktree, rel)
            if not os.path.isfile(src):
                continue
            dst = os.path.join(files_dir, rel)
            os.makedirs(os.path.dirname(dst) or ".", exist_ok=True)
            shutil.copy2(src, dst)
            file_entries.append({"path": rel, "action": entry["action"], "size": os.path.getsize(src)})

        # 4. Write manifest.json describing all preserved outputs.
        manifest = {
            "queue_id": self.spec_id,
            "completed_at": datetime.now(timezone.utc).isoformat(),
            "files": file_entries,
        }
        _write_file_atomic(
            os.path.join(outputs_dir, "manifest.json"),
            json.dumps(manifest, indent=2) + "\n",
        )

        # 5. Append iteration log tail to verify-outputs.log.
        verify_log = os.path.join(outputs_dir, "verify-outputs.log")
        if os.path.isfile(self.log_file):
            try:
                with open(self.log_file, encoding="utf-8", errors="replace") as lf:
                    with open(verify_log, "a", encoding="utf-8") as vf:
                        vf.write(f"\n=== {self.spec_id} iter {self.iteration} ===\n")
                        vf.write(lf.read())
            except Exception:
                logger.warning("collect_outputs: could not append log for %s", self.spec_id)

        logger.info(
            "collect_outputs: preserved %d files for %s → %s",
            len(file_entries), self.spec_id, outputs_dir,
        )

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
    parser.add_argument(
        "--task-id",
        default=None,
        help="Specific task ID for parallel task dispatch (sets BOI_TASK_ID env)",
    )

    args = parser.parse_args()

    if args.task_id:
        os.environ["BOI_TASK_ID"] = args.task_id

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
