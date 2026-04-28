"""Run guardrail hooks at phase transitions.

This module provides the hook execution engine for the BOI guardrails
system. It loads guardrails config, resolves gate callables, runs them
sequentially, applies strictness, and handles gate failures.
"""

from __future__ import annotations

import logging
import os
import re
import subprocess
from pathlib import Path
from typing import Any, Optional

_logger = logging.getLogger("boi.guardrail_runner")


def _load_guardrails(state_dir: str) -> Any:
    """Load guardrails config from state_dir/guardrails.toml."""
    try:
        from lib.guardrails import load_guardrails
        global_path = os.path.join(state_dir, "guardrails.toml")
        return load_guardrails(global_path)
    except Exception:
        try:
            from lib.guardrails import GuardrailConfig
            return GuardrailConfig()
        except Exception:
            return None


def _make_shell_gate(script_path: str):
    """Return a gate callable that runs a shell script."""
    from lib.gates.context import HookContext, HookResult

    def run(ctx: HookContext, config: dict) -> HookResult:
        try:
            env = dict(os.environ)
            env["SPEC_PATH"] = ctx.spec_path
            env["SPEC_ID"] = ctx.spec_id
            result = subprocess.run(
                ["/bin/sh", script_path],
                capture_output=True,
                text=True,
                timeout=60,
                env=env,
            )
            passed = result.returncode == 0
            msg = result.stdout.strip() or result.stderr.strip()
            return HookResult(passed=passed, message=msg)
        except Exception as exc:
            return HookResult(passed=False, message=str(exc))

    return run


def _resolve_gate(gate_name: str, state_dir: str, gate_configs: dict) -> Any:
    """Resolve a gate name to a callable.

    Checks built-in registry first, then falls back to a shell script
    at state_dir/gates/<name>.sh.
    """
    try:
        from lib.gates import BUILTIN_GATES
        if gate_name in BUILTIN_GATES:
            return BUILTIN_GATES[gate_name]
    except Exception:
        pass

    script_path = os.path.join(state_dir, "gates", f"{gate_name}.sh")
    if os.path.isfile(script_path):
        return _make_shell_gate(script_path)

    return None


def _append_gate_fail_task(spec_path: str, gate_name: str, details: str) -> None:
    """Append a [GATE-FAIL] PENDING task to the spec file."""
    try:
        content = Path(spec_path).read_text(encoding="utf-8")
        task_ids = re.findall(r"### t-(\d+):", content)
        next_id = max((int(i) for i in task_ids), default=0) + 1

        gate_task = (
            f"\n### t-{next_id}: [GATE-FAIL] Fix gate failure: {gate_name}\n"
            f"PENDING\n\n"
            f"**Spec:** The gate `{gate_name}` failed at a phase transition. "
            f"Fix the issue and re-run the gate to confirm it passes.\n\n"
            f"Details:\n{details or 'No details provided.'}\n\n"
            f"**Verify:**\n"
            f"Run the gate check manually to confirm it passes.\n"
        )
        tmp = spec_path + ".gate-fail.tmp"
        Path(tmp).write_text(content + gate_task, encoding="utf-8")
        os.replace(tmp, spec_path)
        _logger.info("Appended GATE-FAIL task for gate '%s'", gate_name)
    except Exception as exc:
        _logger.warning("Failed to append gate-fail task for '%s': %s", gate_name, exc)


def run_hooks(
    hook_point: str,
    spec_id: str,
    spec_path: str,
    state_dir: str,
    phase_config: Any = None,
    extra_gate_names: Optional[list[str]] = None,
) -> dict[str, Any]:
    """Run guardrail hooks for a given hook point.

    Args:
        hook_point: The hook point name (e.g. 'post-execute', 'pre-commit').
        spec_id: Queue ID of the spec.
        spec_path: Path to the spec file.
        state_dir: BOI state directory (~/.boi or equivalent).
        phase_config: Optional PhaseConfig for phase-specific hooks.
        extra_gate_names: Additional gate names to run beyond config.

    Returns:
        dict with keys:
          passed (bool): True if all gates passed or strictness allows.
          failed_gates (list): List of dicts with gate/message/details.
          outcome (str): 'no_gates' | 'passed' | 'warned' | 'blocked'.
    """
    try:
        from lib.gates.context import HookContext
    except Exception:
        return {"passed": True, "failed_gates": [], "outcome": "no_gates"}

    config = _load_guardrails(state_dir)
    if config is None:
        return {"passed": True, "failed_gates": [], "outcome": "no_gates"}

    strictness = getattr(config, "strictness", "advisory")
    gate_configs = getattr(config, "gate_configs", {})
    hooks_map = getattr(config, "hooks", {})

    # Collect gate names for this hook point
    gate_names: list[str] = list(hooks_map.get(hook_point, []))
    if extra_gate_names:
        gate_names.extend(extra_gate_names)

    # Add phase-specific pre/post hooks
    if phase_config is not None:
        if "pre" in hook_point:
            gate_names.extend(getattr(phase_config, "pre_hooks", []))
        elif "post" in hook_point:
            gate_names.extend(getattr(phase_config, "post_hooks", []))

    if not gate_names:
        return {"passed": True, "failed_gates": [], "outcome": "no_gates"}

    # Read spec content for gates that need it
    spec_content = ""
    if spec_path and os.path.isfile(spec_path):
        try:
            spec_content = Path(spec_path).read_text(encoding="utf-8")
        except Exception:
            pass

    ctx = HookContext(
        spec_id=spec_id,
        spec_content=spec_content,
        spec_path=spec_path,
        phase=hook_point,
    )

    failed_gates: list[dict] = []

    for gate_name in gate_names:
        gate_fn = _resolve_gate(gate_name, state_dir, gate_configs)
        if gate_fn is None:
            _logger.warning("Gate '%s' not found at '%s', skipping", gate_name, hook_point)
            continue

        gate_cfg = gate_configs.get(gate_name, {})
        try:
            result = gate_fn(ctx, gate_cfg)
        except Exception as exc:
            _logger.warning("Gate '%s' raised exception: %s", gate_name, exc)
            from lib.gates.context import HookResult
            result = HookResult(passed=False, message=str(exc))

        if not result.passed:
            failed_gates.append({
                "gate": gate_name,
                "message": getattr(result, "message", ""),
                "details": getattr(result, "details", ""),
            })
            _logger.warning(
                "Gate '%s' FAILED for %s at '%s': %s",
                gate_name, spec_id, hook_point, getattr(result, "message", ""),
            )

    if not failed_gates:
        return {"passed": True, "failed_gates": [], "outcome": "passed"}

    # Apply strictness
    if strictness == "permissive":
        return {"passed": True, "failed_gates": failed_gates, "outcome": "warned"}

    if strictness == "advisory":
        _logger.warning(
            "Advisory gate failures for %s at '%s': %s",
            spec_id, hook_point, [g["gate"] for g in failed_gates],
        )
        return {"passed": True, "failed_gates": failed_gates, "outcome": "warned"}

    # strict: block on first failure, append task, return blocked
    first_fail = failed_gates[0]
    details = first_fail.get("details") or first_fail.get("message", "")
    _append_gate_fail_task(spec_path, first_fail["gate"], details)

    return {"passed": False, "failed_gates": failed_gates, "outcome": "blocked"}
