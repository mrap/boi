"""Guardrails config loader for BOI modular phase system."""

from __future__ import annotations

import re
import tomllib
from dataclasses import dataclass, field


@dataclass
class SpecOverride:
    pipeline: list[str] | None = None
    strictness: str | None = None
    add_gates: list[str] = field(default_factory=list)
    remove_gates: list[str] = field(default_factory=list)


@dataclass
class GuardrailConfig:
    strictness: str = "advisory"
    pipeline: list[str] = field(default_factory=lambda: ["execute", "critic"])
    hooks: dict[str, list[str]] = field(default_factory=dict)
    gate_configs: dict[str, dict] = field(default_factory=dict)


def _parse_toml_file(path: str) -> dict:
    try:
        with open(path, "rb") as f:
            return tomllib.load(f)
    except (FileNotFoundError, OSError):
        return {}


def _config_from_data(data: dict) -> GuardrailConfig:
    pipeline_section = data.get("pipeline", {})
    global_section = data.get("global", {})
    hooks_section = data.get("hooks", {})
    gates_section = data.get("gates", {})

    pipeline = pipeline_section.get("default", ["execute", "critic"])
    strictness = global_section.get("strictness", "advisory")

    hooks: dict[str, list[str]] = {}
    for key, val in hooks_section.items():
        if isinstance(val, list):
            hooks[key] = val

    gate_configs: dict[str, dict] = {}
    for gate_name, gate_cfg in gates_section.items():
        if isinstance(gate_cfg, dict):
            gate_configs[gate_name] = gate_cfg

    return GuardrailConfig(
        strictness=strictness,
        pipeline=pipeline,
        hooks=hooks,
        gate_configs=gate_configs,
    )


def _merge_dicts(base: dict, override: dict) -> dict:
    result = dict(base)
    for k, v in override.items():
        if k in result and isinstance(result[k], dict) and isinstance(v, dict):
            result[k] = _merge_dicts(result[k], v)
        else:
            result[k] = v
    return result


def load_guardrails(global_path: str, repo_path: str | None = None) -> GuardrailConfig:
    """Load guardrails config, merging global and repo-level configs."""
    global_data = _parse_toml_file(global_path)
    if repo_path:
        repo_data = _parse_toml_file(repo_path)
        data = _merge_dicts(global_data, repo_data)
    else:
        data = global_data
    return _config_from_data(data)


def parse_spec_overrides(spec_content: str) -> SpecOverride:
    """Parse **Pipeline:** and **Gates:** from spec header."""
    override = SpecOverride()

    # Parse pipeline: **Pipeline:** execute → review → critic
    pipeline_match = re.search(r"\*\*Pipeline:\*\*\s*(.+)", spec_content)
    if pipeline_match:
        raw = pipeline_match.group(1).strip()
        # Split on → (unicode arrow) or -> (ASCII arrow) or commas
        # Use alternation, not a character class, to avoid splitting on bare '-'
        # which would break hyphenated phase names like 'security-scan'.
        parts = re.split(r"\s*(?:→|->|,)\s*", raw)
        override.pipeline = [p.strip() for p in parts if p.strip()]

    # Parse gates: **Gates:** strict, +lint-pass, -no-secrets
    gates_match = re.search(r"\*\*Gates:\*\*\s*(.+)", spec_content)
    if gates_match:
        raw = gates_match.group(1).strip()
        tokens = [t.strip() for t in raw.split(",") if t.strip()]
        for token in tokens:
            if token in ("strict", "advisory", "permissive"):
                override.strictness = token
            elif token.startswith("+"):
                override.add_gates.append(token[1:])
            elif token.startswith("-"):
                override.remove_gates.append(token[1:])

    return override


def merge_config(global_config: GuardrailConfig, spec_override: SpecOverride) -> GuardrailConfig:
    """Apply per-spec overrides to global config."""
    pipeline = spec_override.pipeline if spec_override.pipeline is not None else list(global_config.pipeline)
    strictness = spec_override.strictness if spec_override.strictness is not None else global_config.strictness

    hooks = {k: list(v) for k, v in global_config.hooks.items()}

    # Apply add/remove gates to all hook points
    for hook_point in hooks:
        for gate in spec_override.add_gates:
            if gate not in hooks[hook_point]:
                hooks[hook_point].append(gate)
        hooks[hook_point] = [g for g in hooks[hook_point] if g not in spec_override.remove_gates]

    return GuardrailConfig(
        strictness=strictness,
        pipeline=pipeline,
        hooks=hooks,
        gate_configs=dict(global_config.gate_configs),
    )
