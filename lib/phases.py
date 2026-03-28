"""Phase file schema and loader for BOI modular phase system."""

from __future__ import annotations

import os
import tomllib
from dataclasses import dataclass, field
from typing import Optional


@dataclass
class PhaseConfig:
    name: str
    prompt_template: str
    approve_signal: str
    description: str = ""
    model: str = "claude-sonnet-4-6"
    effort: str = "medium"
    timeout: int = 300
    reject_signal: str = ""
    on_approve: str = "next"
    on_reject: str = "requeue:execute"
    on_crash: str = "retry"
    pre_hooks: list[str] = field(default_factory=list)
    post_hooks: list[str] = field(default_factory=list)
    completion_handler: str = ""


def load_phase(path: str) -> PhaseConfig:
    """Parse a .phase.toml file and return a PhaseConfig."""
    with open(path, "rb") as f:
        data = tomllib.load(f)

    worker = data.get("worker", {})
    completion = data.get("completion", {})
    hooks = data.get("hooks", {})

    name = data.get("name", "")
    if not name:
        # Derive name from filename: foo.phase.toml -> foo
        basename = os.path.basename(path)
        if basename.endswith(".phase.toml"):
            name = basename[: -len(".phase.toml")]
        else:
            name = basename

    return PhaseConfig(
        name=name,
        description=data.get("description", ""),
        prompt_template=worker.get("prompt_template", ""),
        model=worker.get("model", "claude-sonnet-4-6"),
        effort=worker.get("effort", "medium"),
        timeout=worker.get("timeout", 300),
        approve_signal=completion.get("approve_signal", ""),
        reject_signal=completion.get("reject_signal", ""),
        on_approve=completion.get("on_approve", "next"),
        on_reject=completion.get("on_reject", "requeue:execute"),
        on_crash=completion.get("on_crash", "retry"),
        pre_hooks=hooks.get("pre", []),
        post_hooks=hooks.get("post", []),
        completion_handler=data.get("completion_handler", ""),
    )


def discover_phases(phases_dir: str) -> dict[str, PhaseConfig]:
    """Scan directory for *.phase.toml files, return name→PhaseConfig map."""
    phases: dict[str, PhaseConfig] = {}
    if not os.path.isdir(phases_dir):
        return phases
    for entry in os.scandir(phases_dir):
        if entry.name.endswith(".phase.toml") and entry.is_file():
            try:
                config = load_phase(entry.path)
                phases[config.name] = config
            except Exception as e:
                import logging
                logging.warning(f'Failed to load phase file {entry.path}: {e}')
    return phases


def validate_phase(config: PhaseConfig) -> list[str]:
    """Return a list of validation errors. Empty list means valid."""
    errors: list[str] = []

    if not config.name:
        errors.append("name is required")

    if not config.prompt_template:
        errors.append("[worker].prompt_template is required")

    # approve_signal is required unless a builtin completion_handler is set
    if not config.approve_signal and not config.completion_handler:
        errors.append("[completion].approve_signal is required (or set completion_handler)")

    if config.effort not in ("low", "medium", "high"):
        errors.append(f"[worker].effort must be low/medium/high, got '{config.effort}'")

    if config.timeout <= 0:
        errors.append(f"[worker].timeout must be positive, got {config.timeout}")

    valid_on_approve = {"next", "complete", "commit"}
    if config.on_approve not in valid_on_approve and not config.on_approve.startswith("phase:"):
        errors.append(
            f"[completion].on_approve must be one of {valid_on_approve} or 'phase:<name>', "
            f"got '{config.on_approve}'"
        )

    if config.on_reject and not (
        config.on_reject in {"fail", "retry"}
        or config.on_reject.startswith("requeue:")
        or config.on_reject.startswith("phase:")
    ):
        errors.append(
            f"[completion].on_reject must be fail/retry/requeue:<phase>/phase:<name>, "
            f"got '{config.on_reject}'"
        )

    return errors
