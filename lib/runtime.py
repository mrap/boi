"""runtime.py — Runtime abstraction for BOI worker execution.

Provides a pluggable interface so BOI can dispatch work to different
CLI agent backends (Claude Code, Codex, etc.) without forking logic.
"""

import json
import os
import re
import shlex
import shutil
from abc import ABC, abstractmethod
from typing import Optional

DEFAULT_RUNTIME = "claude"


def _shell_quote_path(path: str) -> str:
    """Return shell-quoted path, preserving bare bash variable references.

    Bash variable references (starting with '$') are wrapped in double quotes
    to allow expansion.  All other paths are quoted with shlex.quote (single
    quotes) to prevent shell injection.
    """
    if path.startswith("$"):
        # Only allow simple variable references like ${_VAR}, not subshell $(...)
        if "$(" in path or "`" in path:
            raise ValueError(f"Unsafe shell construct in path: {path!r}")
        return f'"{path}"'
    return shlex.quote(path)


class Runtime(ABC):
    """Abstract base for a BOI worker execution runtime."""

    name: str        # "claude" or "codex"
    cli_command: str  # "claude" or "codex"

    @abstractmethod
    def build_exec_cmd(
        self, prompt_file: str, model: str, cost_tier: str,
        context_dirs: Optional[list] = None,
    ) -> str:
        """Build the non-interactive execution command string.

        Args:
            prompt_file: Path to the prompt file (bash variable reference or
                         literal path). Use '${_PROMPT_FILE}' when called from
                         inside a bash heredoc template.
            model: Full model ID (e.g. 'claude-sonnet-4-6') or alias
                   (opus/sonnet/haiku). The runtime resolves aliases.
            cost_tier: Effort tier hint ('high', 'medium', 'low').
            context_dirs: Optional list of directory paths to pass as
                          ``--add-dir`` flags (Claude runtime only).

        Returns:
            Shell command string suitable for embedding in a bash script.
        """

    @abstractmethod
    def model_id(self, alias: str) -> str:
        """Map alias (opus/sonnet/haiku) to runtime-specific model ID.

        If alias is already a full model ID (not in alias map), return as-is.
        """

    @abstractmethod
    def cost_per_token(self, model: str) -> tuple:
        """Return (input_cost, output_cost) per 1M tokens for the model.

        Falls back to a sensible default if model is unknown.
        """

    @abstractmethod
    def check_installed(self) -> tuple:
        """Check if the runtime CLI is installed.

        Returns:
            (ok: bool, message: str)
        """

    @abstractmethod
    def detect_worker_process(self, process_line: str) -> bool:
        """Return True if a tmux process line belongs to this runtime's worker."""


class ClaudeRuntime(Runtime):
    """Runtime backed by the Claude Code CLI (`claude -p`)."""

    name = "claude"
    cli_command = "claude"

    _MODEL_MAP = {
        "opus":   ("claude-opus-4-6", "high"),
        "sonnet": ("claude-sonnet-4-6", "medium"),
        "haiku":  ("claude-haiku-4-5-20251001", "low"),
    }

    _COST_TABLE = {
        "claude-opus-4-6":          (15.0, 75.0),
        "claude-sonnet-4-6":        (3.0, 15.0),
        "claude-haiku-4-5-20251001": (1.0, 5.0),
        "claude-haiku-4-5":         (1.0, 5.0),
    }

    def build_exec_cmd(
        self, prompt_file: str, model: str, cost_tier: str,
        context_dirs: Optional[list] = None,
    ) -> str:
        resolved_model = self.model_id(model)
        # Determine effort from alias mapping or cost_tier fallback
        effort = cost_tier
        for alias, (mid, eff) in self._MODEL_MAP.items():
            if mid == resolved_model:
                effort = eff
                break

        # Build --add-dir flags (inserted before --output-format to avoid flag ordering issues)
        add_dir_flags = ""
        for d in (context_dirs or []):
            add_dir_flags += f'--add-dir {_shell_quote_path(d)} '

        return (
            f'env -u CLAUDECODE claude -p "$(cat {_shell_quote_path(prompt_file)})" '
            f'--model {resolved_model} --effort {effort} '
            f'--dangerously-skip-permissions '
            f'{add_dir_flags}'
            f'--output-format stream-json --verbose'
        )

    def model_id(self, alias: str) -> str:
        alias_lower = alias.lower()
        if alias_lower in self._MODEL_MAP:
            return self._MODEL_MAP[alias_lower][0]
        return alias  # already a full model ID

    def cost_per_token(self, model: str) -> tuple:
        resolved = self.model_id(model)
        return self._COST_TABLE.get(resolved, (3.0, 15.0))

    def check_installed(self) -> tuple:
        path = shutil.which("claude")
        if path:
            return (True, f"claude found at {path}")
        return (False, "claude CLI not found in PATH. Install Claude Code.")

    def detect_worker_process(self, process_line: str) -> bool:
        return bool(re.search(r"\bclaude\s+-p", process_line))


class CodexRuntime(Runtime):
    """Runtime backed by the Codex CLI (`codex exec`)."""

    name = "codex"
    cli_command = "codex"

    _MODEL_MAP = {
        "opus":   ("o3", "high"),
        "sonnet": ("o4-mini", "medium"),
        "haiku":  ("o4-mini", "low"),
    }

    # Estimated pricing as of 2026-03 -- verify at openai.com/pricing
    _COST_TABLE = {
        "o3":      (10.0, 40.0),
        "o4-mini": (1.1, 4.4),
    }

    def build_exec_cmd(
        self, prompt_file: str, model: str, cost_tier: str,
        context_dirs: Optional[list] = None,
    ) -> str:
        resolved_model = self.model_id(model)
        # CodexRuntime ignores context_dirs (not supported by Codex CLI)
        # --dangerously-bypass-approvals-and-sandbox is the Codex equivalent of
        # Claude's --dangerously-skip-permissions: skips all confirmation prompts
        # and runs without sandboxing. Required for unattended BOI worker execution.
        return (
            f'codex exec --model {resolved_model} '
            f'--dangerously-bypass-approvals-and-sandbox '
            f'< {_shell_quote_path(prompt_file)}'
        )

    def model_id(self, alias: str) -> str:
        alias_lower = alias.lower()
        if alias_lower in self._MODEL_MAP:
            return self._MODEL_MAP[alias_lower][0]
        return alias  # already a full model ID

    def cost_per_token(self, model: str) -> tuple:
        resolved = self.model_id(model)
        return self._COST_TABLE.get(resolved, (1.1, 4.4))

    def check_installed(self) -> tuple:
        path = shutil.which("codex")
        if path:
            return (True, f"codex found at {path}")
        return (False, "codex CLI not found in PATH. Install Codex CLI.")

    def detect_worker_process(self, process_line: str) -> bool:
        return bool(re.search(r"\bcodex\s+exec\b", process_line))


_REGISTRY = {
    "claude": ClaudeRuntime,
    "codex":  CodexRuntime,
}


def get_all_runtimes() -> list:
    """Return one instance of every registered runtime."""
    return [cls() for cls in _REGISTRY.values()]


def get_runtime(name: str) -> Runtime:
    """Return a Runtime instance for the given name.

    Args:
        name: Runtime name ("claude" or "codex"). Case-insensitive.

    Returns:
        Runtime instance.

    Raises:
        ValueError: If the runtime name is not recognized.
    """
    key = name.lower().strip()
    if key not in _REGISTRY:
        raise ValueError(
            f"Unknown runtime: {name!r}. Valid options: {list(_REGISTRY)}"
        )
    return _REGISTRY[key]()


# ── Config loading ─────────────────────────────────────────────────────────

def load_runtime_from_config(state_dir: str) -> str:
    """Read the default runtime from {state_dir}/config.json.

    Expects an optional ``"runtime": {"default": "claude"}`` key in the
    config.  Returns DEFAULT_RUNTIME when the file is absent, unreadable,
    or the key is missing.
    """
    config_path = os.path.join(state_dir, "config.json")
    try:
        with open(config_path, encoding="utf-8") as f:
            data = json.load(f)
        return data.get("runtime", {}).get("default", DEFAULT_RUNTIME)
    except (OSError, json.JSONDecodeError):
        return DEFAULT_RUNTIME


def load_context_root(state_dir: str) -> Optional[str]:
    """Read the optional context_root from {state_dir}/config.json.

    When set, workers pass ``--add-dir <context_root>`` to the Claude CLI,
    giving them read access to the agent directory (CLAUDE.md, memory, skills)
    while running from the target repo worktree.

    Returns:
        Expanded absolute path, or None if unset/empty/nonexistent.
    """
    config_path = os.path.join(state_dir, "config.json")
    try:
        with open(config_path, encoding="utf-8") as f:
            data = json.load(f)
        raw = data.get("context_root", "")
        if not raw:
            return None
        expanded = os.path.expanduser(raw)
        if not os.path.isdir(expanded):
            import logging
            logging.getLogger(__name__).warning(
                "context_root '%s' does not exist on disk, ignoring", expanded
            )
            return None
        return expanded
    except (OSError, json.JSONDecodeError):
        return None


def resolve_spec_runtime(spec_content: str) -> Optional[str]:
    """Parse ``**Runtime:**`` from a spec header.

    Scans only lines before the first task heading (``### t-N:``).  Returns
    the runtime name if found and valid, else ``None``.
    """
    for line in spec_content.splitlines():
        if re.match(r"^###\s+t-\d+:", line):
            break
        m = re.match(r"^\*\*Runtime:\*\*\s*(\w+)", line.strip())
        if m:
            name = m.group(1).strip().lower()
            if name in _REGISTRY:
                return name
    return None


def resolve_runtime(state_dir: Optional[str] = None, spec_content: str = "") -> str:
    """Resolve runtime with priority: spec header > global config > default.

    Args:
        state_dir: Path to the BOI state directory (e.g. ``~/.boi``).
                   Pass ``None`` to skip the global-config lookup.
        spec_content: Raw spec file text.  If provided, a ``**Runtime:**``
                      header line overrides the global config.

    Returns:
        A valid runtime name (``"claude"`` or ``"codex"``).
    """
    spec_rt = resolve_spec_runtime(spec_content)
    if spec_rt:
        return spec_rt

    if state_dir:
        return load_runtime_from_config(state_dir)

    return DEFAULT_RUNTIME
