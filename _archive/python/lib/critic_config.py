# critic_config.py — Critic configuration management for BOI.
#
# Manages the critic system configuration at ~/.boi/critic/.
# The critic validates spec work quality before marking specs complete.
#
# Configuration lives at:
#   ~/.boi/critic/config.json   — settings (enabled, trigger, max_passes, etc.)
#   ~/.boi/critic/custom/       — user's custom check definitions
#   ~/.boi/critic/prompt.md     — optional user prompt override
#
# Default checks ship with BOI at ~/boi/templates/checks/.

import json
import os
from pathlib import Path
from typing import Any


DEFAULT_CONFIG = {
    "enabled": True,
    "trigger": "on_complete",
    "max_passes": 2,
    "checks": [
        "spec-integrity",
        "verify-commands",
        "code-quality",
        "completeness",
        "fleet-readiness",
    ],
    "generate_checks": [
        "goal-alignment",
    ],
    "generate_max_passes": 3,
    "custom_checks_dir": "custom",
    "timeout_seconds": 600,
}

DEFAULT_CHECKS = [
    "spec-integrity",
    "verify-commands",
    "code-quality",
    "completeness",
    "fleet-readiness",
    "conjecture-criticism",
    "goal-alignment",
    "quality-scoring",
]


def load_critic_config(state_dir: str) -> dict[str, Any]:
    """Load critic config from state_dir/critic/config.json.

    Creates default config and directory structure if missing.

    Args:
        state_dir: Path to ~/.boi/ state directory.

    Returns:
        The critic configuration dict.
    """
    critic_dir = os.path.join(state_dir, "task-verify")
    config_path = os.path.join(critic_dir, "config.json")
    custom_dir = os.path.join(critic_dir, DEFAULT_CONFIG["custom_checks_dir"])

    # Ensure directory structure exists
    os.makedirs(critic_dir, exist_ok=True)
    os.makedirs(custom_dir, exist_ok=True)

    if os.path.isfile(config_path):
        try:
            with open(config_path, "r") as f:
                config = json.load(f)
            # Merge with defaults for any missing keys
            merged = dict(DEFAULT_CONFIG)
            merged.update(config)
            return merged
        except (json.JSONDecodeError, OSError):
            return dict(DEFAULT_CONFIG)
    else:
        # Write default config
        _write_config(config_path, DEFAULT_CONFIG)
        return dict(DEFAULT_CONFIG)


def _write_config(config_path: str, config: dict[str, Any]) -> None:
    """Atomically write config to disk."""
    tmp_path = config_path + ".tmp"
    with open(tmp_path, "w") as f:
        json.dump(config, f, indent=2)
        f.write("\n")
    os.replace(tmp_path, config_path)


def save_critic_config(state_dir: str, config: dict[str, Any]) -> None:
    """Save critic config to state_dir/critic/config.json.

    Args:
        state_dir: Path to ~/.boi/ state directory.
        config: The configuration dict to save.
    """
    config_path = os.path.join(state_dir, "task-verify", "config.json")
    os.makedirs(os.path.dirname(config_path), exist_ok=True)
    _write_config(config_path, config)


def is_critic_enabled(config: dict[str, Any]) -> bool:
    """Check if the critic should run.

    Args:
        config: The critic configuration dict.

    Returns:
        True if the critic is enabled.
    """
    return bool(config.get("enabled", True))


def get_active_checks(
    config: dict[str, Any], checks_dir: str, state_dir: str
) -> list[dict[str, str]]:
    """Return list of active check definitions (default + custom).

    Each check is a dict with 'name', 'source' ('default' or 'custom'),
    and 'content' (the markdown content of the check file).

    Custom checks with the same filename as a default check replace the default.

    Args:
        config: The critic configuration dict.
        checks_dir: Path to default checks (~/boi/templates/checks/).
        state_dir: Path to ~/.boi/ state directory.

    Returns:
        List of check definition dicts.
    """
    enabled_checks = config.get("checks", DEFAULT_CHECKS)
    custom_dir_name = config.get("custom_checks_dir", "custom")
    custom_dir = os.path.join(state_dir, "task-verify", custom_dir_name)

    # Collect custom check names for override detection
    custom_check_names: set[str] = set()
    if os.path.isdir(custom_dir):
        for fname in os.listdir(custom_dir):
            if fname.endswith(".md"):
                custom_check_names.add(fname[:-3])  # strip .md

    checks: list[dict[str, str]] = []

    # Load default checks (skip if overridden by custom)
    for check_name in enabled_checks:
        if check_name in custom_check_names:
            # Custom override exists, skip default
            continue
        check_path = os.path.join(checks_dir, f"{check_name}.md")
        if os.path.isfile(check_path):
            try:
                with open(check_path, "r") as f:
                    content = f.read()
                checks.append(
                    {
                        "name": check_name,
                        "source": "default",
                        "content": content,
                    }
                )
            except OSError:
                continue

    # Load custom checks (both overrides and new ones)
    if os.path.isdir(custom_dir):
        for fname in sorted(os.listdir(custom_dir)):
            if not fname.endswith(".md"):
                continue
            check_name = fname[:-3]
            check_path = os.path.join(custom_dir, fname)
            try:
                with open(check_path, "r") as f:
                    content = f.read()
                checks.append(
                    {
                        "name": check_name,
                        "source": "custom",
                        "content": content,
                    }
                )
            except OSError:
                continue

    return checks


def get_generate_checks(
    config: dict[str, Any], checks_dir: str, state_dir: str
) -> list[dict[str, str]]:
    """Return list of Generate-mode-specific check definitions.

    These are additional checks that run only for Generate-mode specs,
    in addition to the standard checks.

    Args:
        config: The critic configuration dict.
        checks_dir: Path to default checks (~/boi/templates/checks/).
        state_dir: Path to ~/.boi/ state directory.

    Returns:
        List of check definition dicts with 'name', 'source', 'content'.
    """
    generate_check_names = config.get("generate_checks", ["goal-alignment"])
    custom_dir_name = config.get("custom_checks_dir", "custom")
    custom_dir = os.path.join(state_dir, "task-verify", custom_dir_name)

    # Collect custom check names for override detection
    custom_check_names: set[str] = set()
    if os.path.isdir(custom_dir):
        for fname in os.listdir(custom_dir):
            if fname.endswith(".md"):
                custom_check_names.add(fname[:-3])

    checks: list[dict[str, str]] = []

    for check_name in generate_check_names:
        # Check custom dir first
        if check_name in custom_check_names:
            check_path = os.path.join(custom_dir, f"{check_name}.md")
        else:
            check_path = os.path.join(checks_dir, f"{check_name}.md")

        if os.path.isfile(check_path):
            try:
                with open(check_path, "r") as f:
                    content = f.read()
                source = "custom" if check_name in custom_check_names else "default"
                checks.append(
                    {
                        "name": check_name,
                        "source": source,
                        "content": content,
                    }
                )
            except OSError:
                continue

    return checks


def get_critic_prompt(state_dir: str, boi_dir: str) -> str:
    """Load the critic prompt template.

    Checks for user override at ~/.boi/critic/prompt.md first,
    then falls back to the default at ~/boi/templates/critic-prompt.md.

    Args:
        state_dir: Path to ~/.boi/ state directory.
        boi_dir: Path to ~/boi/ installation directory.

    Returns:
        The prompt template content as a string.

    Raises:
        FileNotFoundError: If no prompt template is found.
    """
    # Check for user override
    user_prompt = os.path.join(state_dir, "task-verify", "prompt.md")
    if os.path.isfile(user_prompt):
        with open(user_prompt, "r") as f:
            return f.read()

    # Fall back to default
    default_prompt = os.path.join(boi_dir, "templates", "task-verify-prompt.md")
    if os.path.isfile(default_prompt):
        with open(default_prompt, "r") as f:
            return f.read()

    raise FileNotFoundError(
        f"No critic prompt template found. Checked: {user_prompt}, {default_prompt}"
    )


def ensure_critic_dirs(state_dir: str) -> None:
    """Create the default critic directory structure.

    Called during `boi install` to set up:
      ~/.boi/critic/
      ~/.boi/critic/custom/
      ~/.boi/critic/config.json (with defaults)

    Args:
        state_dir: Path to ~/.boi/ state directory.
    """
    critic_dir = os.path.join(state_dir, "task-verify")
    custom_dir = os.path.join(critic_dir, "custom")

    os.makedirs(critic_dir, exist_ok=True)
    os.makedirs(custom_dir, exist_ok=True)

    config_path = os.path.join(critic_dir, "config.json")
    if not os.path.isfile(config_path):
        _write_config(config_path, DEFAULT_CONFIG)
