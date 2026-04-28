# test_modular_phases.py — Integration tests for the modular phase system.
#
# Tests the phase/guardrails/gates system end-to-end without a running daemon:
#   1. Custom phase: load a custom .phase.toml, verify config is correct.
#   2. Gate test: verify-commands-pass blocks in strict mode, appends GATE-FAIL task.
#   3. Pipeline override: spec header overrides pipeline, review phase is skipped.
#   4. Strictness: advisory mode allows gate failures without blocking.

import os
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

_PROJECT_ROOT = str(Path(__file__).resolve().parent.parent.parent)
sys.path.insert(0, _PROJECT_ROOT)

from lib.phases import PhaseConfig, discover_phases, load_phase, validate_phase
from lib.guardrails import (
    GuardrailConfig,
    load_guardrails,
    merge_config,
    parse_spec_overrides,
)
from lib.guardrail_runner import run_hooks, _append_gate_fail_task


# ---------------------------------------------------------------------------
# 1. Custom phase test
# ---------------------------------------------------------------------------


class TestCustomPhase(unittest.TestCase):
    """A custom .phase.toml can be loaded and its config is correct."""

    def setUp(self) -> None:
        self.tmp_dir = tempfile.mkdtemp()

    def tearDown(self) -> None:
        import shutil
        shutil.rmtree(self.tmp_dir, ignore_errors=True)

    def _write_phase(self, filename: str, content: str) -> str:
        path = os.path.join(self.tmp_dir, filename)
        Path(path).write_text(content, encoding="utf-8")
        return path

    def test_custom_phase_loaded_from_file(self) -> None:
        """A custom phase file is parsed into a PhaseConfig with correct fields."""
        phase_content = textwrap.dedent("""\
            name = "security-scan"
            description = "Scan code changes for security vulnerabilities"

            [worker]
            prompt_template = "~/.boi/templates/security-scan-prompt.md"
            model = "claude-sonnet-4-6"
            effort = "medium"
            timeout = 300

            [completion]
            approve_signal = "## Security Approved"
            reject_signal = "[SECURITY]"
            on_approve = "next"
            on_reject = "requeue:execute"
            on_crash = "retry"

            [hooks]
            pre = []
            post = ["no-secrets"]
        """)
        path = self._write_phase("security-scan.phase.toml", phase_content)

        config = load_phase(path)

        self.assertEqual(config.name, "security-scan")
        self.assertEqual(config.description, "Scan code changes for security vulnerabilities")
        self.assertEqual(config.approve_signal, "## Security Approved")
        self.assertEqual(config.reject_signal, "[SECURITY]")
        self.assertEqual(config.on_approve, "next")
        self.assertEqual(config.on_reject, "requeue:execute")
        self.assertEqual(config.on_crash, "retry")
        self.assertEqual(config.post_hooks, ["no-secrets"])
        self.assertEqual(config.pre_hooks, [])
        self.assertEqual(config.model, "claude-sonnet-4-6")
        self.assertEqual(config.effort, "medium")
        self.assertEqual(config.timeout, 300)

    def test_custom_phase_discovered_in_directory(self) -> None:
        """discover_phases() finds a custom phase file and registers it by name."""
        self._write_phase("security-scan.phase.toml", textwrap.dedent("""\
            name = "security-scan"
            [worker]
            prompt_template = "~/.boi/templates/security-scan-prompt.md"
            [completion]
            approve_signal = "## Security Approved"
        """))
        self._write_phase("lint-check.phase.toml", textwrap.dedent("""\
            name = "lint-check"
            [worker]
            prompt_template = "~/.boi/templates/lint-prompt.md"
            [completion]
            approve_signal = "## Lint Approved"
        """))

        phases = discover_phases(self.tmp_dir)

        self.assertIn("security-scan", phases)
        self.assertIn("lint-check", phases)
        self.assertEqual(phases["security-scan"].approve_signal, "## Security Approved")
        self.assertEqual(phases["lint-check"].approve_signal, "## Lint Approved")

    def test_custom_phase_in_pipeline(self) -> None:
        """A custom phase can appear in a pipeline parsed from a spec header."""
        spec_content = "**Pipeline:** execute → security-scan → review → critic\n"
        override = parse_spec_overrides(spec_content)
        self.assertEqual(override.pipeline, ["execute", "security-scan", "review", "critic"])

    def test_phase_name_derived_from_filename_when_not_in_toml(self) -> None:
        """Phase name is derived from filename when not set in TOML."""
        content = textwrap.dedent("""\
            [worker]
            prompt_template = "tmpl.md"
            [completion]
            approve_signal = "## Done"
        """)
        path = self._write_phase("my-custom-phase.phase.toml", content)
        config = load_phase(path)
        self.assertEqual(config.name, "my-custom-phase")

    def test_validate_phase_catches_missing_fields(self) -> None:
        """validate_phase() returns errors for a config missing required fields."""
        bad_config = PhaseConfig(
            name="",
            prompt_template="",
            approve_signal="",
        )
        errors = validate_phase(bad_config)
        self.assertTrue(len(errors) > 0)
        self.assertTrue(any("name" in e for e in errors))

    def test_validate_phase_passes_for_builtin_completion_handler(self) -> None:
        """Phase with completion_handler doesn't need approve_signal."""
        config = PhaseConfig(
            name="execute",
            prompt_template="templates/worker-prompt.md",
            approve_signal="",
            completion_handler="builtin:execute",
        )
        errors = validate_phase(config)
        # Should not require approve_signal when completion_handler is set
        self.assertFalse(any("approve_signal" in e for e in errors))


# ---------------------------------------------------------------------------
# 2. Gate test
# ---------------------------------------------------------------------------


class TestGateBlocking(unittest.TestCase):
    """verify-commands-pass gate blocks in strict mode and appends GATE-FAIL task."""

    def setUp(self) -> None:
        self.tmp_dir = tempfile.mkdtemp()

    def tearDown(self) -> None:
        import shutil
        shutil.rmtree(self.tmp_dir, ignore_errors=True)

    def _make_spec(self, content: str) -> str:
        spec_path = os.path.join(self.tmp_dir, "test.spec.md")
        Path(spec_path).write_text(content, encoding="utf-8")
        return spec_path

    def _make_guardrails_toml(self, hooks: dict, strictness: str = "strict") -> str:
        lines = [
            "[global]",
            f'strictness = "{strictness}"',
            "",
            "[hooks]",
        ]
        for point, gates in hooks.items():
            gate_list = ", ".join(f'"{g}"' for g in gates)
            lines.append(f"{point} = [{gate_list}]")
        toml_path = os.path.join(self.tmp_dir, "guardrails.toml")
        Path(toml_path).write_text("\n".join(lines), encoding="utf-8")
        return self.tmp_dir  # state_dir

    def test_failing_verify_gate_blocks_in_strict_mode(self) -> None:
        """A failing verify-commands gate in strict mode blocks and appends GATE-FAIL task."""
        spec_content = textwrap.dedent("""\
            # My Spec

            ### t-1: Do something
            DONE

            **Verify:**
            ```bash
            exit 1
            ```
        """)
        spec_path = self._make_spec(spec_content)
        state_dir = self._make_guardrails_toml({"post-execute": ["verify-commands-pass"]}, strictness="strict")

        result = run_hooks(
            hook_point="post-execute",
            spec_id="q-test-001",
            spec_path=spec_path,
            state_dir=state_dir,
        )

        self.assertFalse(result["passed"])
        self.assertEqual(result["outcome"], "blocked")
        self.assertTrue(len(result["failed_gates"]) > 0)
        self.assertEqual(result["failed_gates"][0]["gate"], "verify-commands-pass")

        # Verify GATE-FAIL task was appended to the spec
        updated_spec = Path(spec_path).read_text(encoding="utf-8")
        self.assertIn("[GATE-FAIL]", updated_spec)
        self.assertIn("PENDING", updated_spec)

    def test_passing_verify_gate_does_not_block(self) -> None:
        """A passing verify-commands gate does not block."""
        spec_content = textwrap.dedent("""\
            # My Spec

            **Verify:**
            ```bash
            exit 0
            ```
        """)
        spec_path = self._make_spec(spec_content)
        state_dir = self._make_guardrails_toml({"post-execute": ["verify-commands-pass"]}, strictness="strict")

        result = run_hooks(
            hook_point="post-execute",
            spec_id="q-test-002",
            spec_path=spec_path,
            state_dir=state_dir,
        )

        self.assertTrue(result["passed"])
        self.assertIn(result["outcome"], ("passed", "no_gates"))
        self.assertEqual(result["failed_gates"], [])

    def test_gate_fail_task_appended_with_details(self) -> None:
        """_append_gate_fail_task adds a properly-formatted GATE-FAIL task."""
        spec_content = textwrap.dedent("""\
            # My Spec

            ### t-1: Initial task
            DONE
        """)
        spec_path = self._make_spec(spec_content)

        _append_gate_fail_task(spec_path, "verify-commands-pass", "exit=1\nstdout=\nstderr=some error")

        updated = Path(spec_path).read_text(encoding="utf-8")
        self.assertIn("[GATE-FAIL]", updated)
        self.assertIn("verify-commands-pass", updated)
        self.assertIn("PENDING", updated)
        self.assertIn("t-2:", updated)  # next task ID after t-1


# ---------------------------------------------------------------------------
# 3. Pipeline override test
# ---------------------------------------------------------------------------


class TestPipelineOverride(unittest.TestCase):
    """Spec header pipeline overrides the global default and can skip phases."""

    def test_pipeline_override_skips_review(self) -> None:
        """Spec with execute→critic skips review phase."""
        spec_content = "**Pipeline:** execute → critic\n"
        override = parse_spec_overrides(spec_content)
        self.assertEqual(override.pipeline, ["execute", "critic"])
        self.assertNotIn("review", override.pipeline)

    def test_pipeline_override_applied_to_global_config(self) -> None:
        """merge_config applies pipeline override to a global config that includes review."""
        global_config = GuardrailConfig(
            strictness="advisory",
            pipeline=["execute", "review", "critic"],
        )
        spec_content = "**Pipeline:** execute → critic\n"
        override = parse_spec_overrides(spec_content)
        merged = merge_config(global_config, override)

        self.assertEqual(merged.pipeline, ["execute", "critic"])
        self.assertNotIn("review", merged.pipeline)

    def test_pipeline_override_with_arrow_variants(self) -> None:
        """Pipeline parsing handles both → (unicode) and -> (ASCII) arrows."""
        spec_arrow = "**Pipeline:** execute → review → critic\n"
        spec_ascii = "**Pipeline:** execute -> review -> critic\n"

        override_arrow = parse_spec_overrides(spec_arrow)
        override_ascii = parse_spec_overrides(spec_ascii)

        self.assertEqual(override_arrow.pipeline, ["execute", "review", "critic"])
        self.assertEqual(override_ascii.pipeline, ["execute", "review", "critic"])

    def test_no_pipeline_override_preserves_global(self) -> None:
        """Spec without **Pipeline:** keeps the global config pipeline."""
        global_config = GuardrailConfig(
            strictness="advisory",
            pipeline=["execute", "review", "critic"],
        )
        spec_content = "# Spec without pipeline override\n"
        override = parse_spec_overrides(spec_content)
        merged = merge_config(global_config, override)

        self.assertEqual(merged.pipeline, ["execute", "review", "critic"])

    def test_load_guardrails_from_toml_pipeline(self) -> None:
        """load_guardrails reads pipeline from guardrails.toml."""
        with tempfile.NamedTemporaryFile(
            mode="w", suffix=".toml", delete=False
        ) as f:
            f.write('[pipeline]\ndefault = ["execute", "review", "critic"]\n')
            f.write('[global]\nstrictness = "strict"\n')
            toml_path = f.name
        try:
            config = load_guardrails(toml_path)
            self.assertEqual(config.pipeline, ["execute", "review", "critic"])
            self.assertEqual(config.strictness, "strict")
        finally:
            os.unlink(toml_path)

    def test_load_guardrails_defaults_when_file_missing(self) -> None:
        """load_guardrails returns defaults when config file is missing."""
        config = load_guardrails("/nonexistent/guardrails.toml")
        self.assertEqual(config.pipeline, ["execute", "task-verify"])
        self.assertEqual(config.strictness, "advisory")


# ---------------------------------------------------------------------------
# 4. Strictness test
# ---------------------------------------------------------------------------


class TestStrictness(unittest.TestCase):
    """Advisory strictness warns on gate failures but does not block."""

    def setUp(self) -> None:
        self.tmp_dir = tempfile.mkdtemp()

    def tearDown(self) -> None:
        import shutil
        shutil.rmtree(self.tmp_dir, ignore_errors=True)

    def _make_spec(self, content: str) -> str:
        spec_path = os.path.join(self.tmp_dir, "test.spec.md")
        Path(spec_path).write_text(content, encoding="utf-8")
        return spec_path

    def _make_guardrails_toml(self, strictness: str) -> str:
        content = (
            f'[global]\nstrictness = "{strictness}"\n\n'
            '[hooks]\n'
            'post-execute = ["verify-commands-pass"]\n'
        )
        toml_path = os.path.join(self.tmp_dir, "guardrails.toml")
        Path(toml_path).write_text(content, encoding="utf-8")
        return self.tmp_dir  # state_dir

    def test_advisory_mode_does_not_block_on_gate_failure(self) -> None:
        """In advisory mode, a failing gate returns passed=True with outcome='warned'."""
        spec_content = textwrap.dedent("""\
            # Spec

            **Verify:**
            ```bash
            exit 1
            ```
        """)
        spec_path = self._make_spec(spec_content)
        state_dir = self._make_guardrails_toml("advisory")

        result = run_hooks(
            hook_point="post-execute",
            spec_id="q-test-advisory",
            spec_path=spec_path,
            state_dir=state_dir,
        )

        # Advisory: passed=True (allowed to proceed) but gate recorded as failed
        self.assertTrue(result["passed"])
        self.assertEqual(result["outcome"], "warned")
        self.assertTrue(len(result["failed_gates"]) > 0)

        # Advisory: no GATE-FAIL task appended
        updated_spec = Path(spec_path).read_text(encoding="utf-8")
        self.assertNotIn("[GATE-FAIL]", updated_spec)

    def test_permissive_mode_does_not_block_or_warn(self) -> None:
        """In permissive mode, a failing gate returns passed=True with outcome='warned'."""
        spec_content = textwrap.dedent("""\
            # Spec

            **Verify:**
            ```bash
            exit 1
            ```
        """)
        spec_path = self._make_spec(spec_content)
        state_dir = self._make_guardrails_toml("permissive")

        result = run_hooks(
            hook_point="post-execute",
            spec_id="q-test-permissive",
            spec_path=spec_path,
            state_dir=state_dir,
        )

        self.assertTrue(result["passed"])
        self.assertEqual(result["outcome"], "warned")

    def test_strict_mode_blocks_on_gate_failure(self) -> None:
        """In strict mode, a failing gate returns passed=False with outcome='blocked'."""
        spec_content = textwrap.dedent("""\
            # Spec

            **Verify:**
            ```bash
            exit 1
            ```
        """)
        spec_path = self._make_spec(spec_content)
        state_dir = self._make_guardrails_toml("strict")

        result = run_hooks(
            hook_point="post-execute",
            spec_id="q-test-strict",
            spec_path=spec_path,
            state_dir=state_dir,
        )

        self.assertFalse(result["passed"])
        self.assertEqual(result["outcome"], "blocked")

    def test_gates_strictness_via_spec_override(self) -> None:
        """Spec with **Gates:** strict overrides advisory global config."""
        global_config = GuardrailConfig(strictness="advisory", pipeline=["execute", "critic"])
        spec_content = "**Gates:** strict, +lint-pass\n"
        override = parse_spec_overrides(spec_content)
        merged = merge_config(global_config, override)

        self.assertEqual(merged.strictness, "strict")

    def test_spec_gates_add_removes_work(self) -> None:
        """**Gates:** +gate and -gate add/remove gates from hook lists."""
        global_config = GuardrailConfig(
            strictness="advisory",
            pipeline=["execute", "critic"],
            hooks={"post-execute": ["diff-is-non-empty", "no-secrets"]},
        )
        # Remove no-secrets, add lint-pass
        spec_content = "**Gates:** -no-secrets, +lint-pass\n"
        override = parse_spec_overrides(spec_content)
        merged = merge_config(global_config, override)

        post_execute_gates = merged.hooks.get("post-execute", [])
        self.assertIn("lint-pass", post_execute_gates)
        self.assertNotIn("no-secrets", post_execute_gates)
        self.assertIn("diff-is-non-empty", post_execute_gates)


# ---------------------------------------------------------------------------
# 5. Built-in gate registry
# ---------------------------------------------------------------------------


class TestBuiltinGateRegistry(unittest.TestCase):
    """BUILTIN_GATES registry has all required gates registered."""

    def test_all_required_gates_registered(self) -> None:
        """BUILTIN_GATES contains all 5 required built-in gates."""
        from lib.gates import BUILTIN_GATES

        required = [
            "verify-commands-pass",
            "diff-is-non-empty",
            "tests-pass",
            "lint-pass",
            "no-secrets",
        ]
        for gate_name in required:
            self.assertIn(gate_name, BUILTIN_GATES, f"Gate '{gate_name}' not in BUILTIN_GATES")

        self.assertGreaterEqual(len(BUILTIN_GATES), 5)

    def test_gate_callables_are_callable(self) -> None:
        """All registered gate functions are callable."""
        from lib.gates import BUILTIN_GATES
        for name, fn in BUILTIN_GATES.items():
            self.assertTrue(callable(fn), f"Gate '{name}' is not callable")


if __name__ == "__main__":
    unittest.main()
