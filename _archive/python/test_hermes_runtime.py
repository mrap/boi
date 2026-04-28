"""Tests for HermesRuntime."""

import os
import sys

# Ensure lib/ is importable
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from lib.runtime import HermesRuntime, get_runtime


class TestHermesRuntime:
    """Unit tests for HermesRuntime."""

    def setup_method(self):
        self.rt = HermesRuntime()

    # ── Model mapping ─────────────────────────────────────────────────

    def test_model_id_opus(self):
        assert self.rt.model_id("opus") == "anthropic/claude-opus-4-6"

    def test_model_id_sonnet(self):
        assert self.rt.model_id("sonnet") == "anthropic/claude-sonnet-4-6"

    def test_model_id_haiku(self):
        assert self.rt.model_id("haiku") == "anthropic/claude-haiku-4-5-20251001"

    def test_model_id_passthrough(self):
        """Full model IDs pass through unchanged."""
        assert self.rt.model_id("anthropic/claude-sonnet-4-6") == "anthropic/claude-sonnet-4-6"

    def test_model_id_case_insensitive(self):
        assert self.rt.model_id("OPUS") == "anthropic/claude-opus-4-6"
        assert self.rt.model_id("Sonnet") == "anthropic/claude-sonnet-4-6"

    # ── Cost table ────────────────────────────────────────────────────

    def test_cost_per_token_opus(self):
        assert self.rt.cost_per_token("opus") == (15.0, 75.0)

    def test_cost_per_token_sonnet(self):
        assert self.rt.cost_per_token("sonnet") == (3.0, 15.0)

    def test_cost_per_token_unknown_fallback(self):
        """Unknown models fall back to sonnet-tier pricing."""
        assert self.rt.cost_per_token("some-future-model") == (3.0, 15.0)

    # ── Command building ──────────────────────────────────────────────

    def test_build_exec_cmd_basic(self):
        cmd = self.rt.build_exec_cmd("prompt.txt", "sonnet", "medium")
        assert "hermes chat -q" in cmd
        assert "anthropic/claude-sonnet-4-6" in cmd
        assert "--quiet" in cmd
        assert "--yolo" in cmd
        assert "--max-turns 50" in cmd

    def test_build_exec_cmd_bash_variable(self):
        """Prompt file as bash variable should use double quotes."""
        cmd = self.rt.build_exec_cmd("${_PROMPT_FILE}", "opus", "high")
        assert '"$(cat "${_PROMPT_FILE}")"' in cmd
        assert "anthropic/claude-opus-4-6" in cmd

    def test_build_exec_cmd_ignores_context_dirs(self):
        """Hermes has built-in memory — context_dirs are accepted but not used."""
        cmd_without = self.rt.build_exec_cmd("p.txt", "sonnet", "medium")
        cmd_with = self.rt.build_exec_cmd("p.txt", "sonnet", "medium",
                                           context_dirs=["/some/path"])
        # Both should produce the same command (context_dirs not in cmd)
        assert cmd_without == cmd_with

    def test_build_exec_cmd_full_model_id(self):
        """Full provider/model ID should be used as-is."""
        cmd = self.rt.build_exec_cmd("p.txt", "anthropic/claude-opus-4-6", "high")
        assert "anthropic/claude-opus-4-6" in cmd

    # ── Registry ──────────────────────────────────────────────────────

    def test_registry_lookup(self):
        rt = get_runtime("hermes")
        assert isinstance(rt, HermesRuntime)

    def test_registry_case_insensitive(self):
        rt = get_runtime("Hermes")
        assert isinstance(rt, HermesRuntime)

    # ── Process detection ─────────────────────────────────────────────

    def test_detect_worker_process_match(self):
        assert self.rt.detect_worker_process("12345 hermes chat -q ...")

    def test_detect_worker_process_no_match(self):
        assert not self.rt.detect_worker_process("12345 claude -p ...")
        assert not self.rt.detect_worker_process("12345 codex exec ...")

    # ── Metadata ──────────────────────────────────────────────────────

    def test_name(self):
        assert self.rt.name == "hermes"

    def test_cli_command(self):
        assert self.rt.cli_command == "hermes"
