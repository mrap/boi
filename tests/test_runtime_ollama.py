"""Tests for OllamaRuntime."""

import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from lib.runtime import OllamaRuntime, get_runtime


class TestOllamaRuntime:
    def setup_method(self):
        self.rt = OllamaRuntime()

    # ── Registry ──────────────────────────────────────────────────────

    def test_registry_lookup(self):
        rt = get_runtime("ollama")
        assert isinstance(rt, OllamaRuntime)

    def test_registry_case_insensitive(self):
        rt = get_runtime("Ollama")
        assert isinstance(rt, OllamaRuntime)

    def test_name(self):
        assert self.rt.name == "ollama"

    # ── Model mapping ─────────────────────────────────────────────────

    def test_model_id_gemma_small(self):
        assert self.rt.model_id("gemma-small") == "gemma4:e4b"

    def test_model_id_gemma(self):
        assert self.rt.model_id("gemma") == "gemma4:26b"

    def test_model_id_gemma_large(self):
        assert self.rt.model_id("gemma-large") == "gemma4:31b"

    def test_model_id_passthrough(self):
        assert self.rt.model_id("gemma4:26b") == "gemma4:26b"
        assert self.rt.model_id("llama3:8b") == "llama3:8b"

    def test_model_id_case_insensitive_aliases(self):
        assert self.rt.model_id("GEMMA") == "gemma4:26b"
        assert self.rt.model_id("Gemma-Large") == "gemma4:31b"

    # ── Cost table ────────────────────────────────────────────────────

    def test_cost_per_token_always_zero(self):
        assert self.rt.cost_per_token("gemma") == (0.0, 0.0)
        assert self.rt.cost_per_token("gemma4:26b") == (0.0, 0.0)
        assert self.rt.cost_per_token("any-future-model") == (0.0, 0.0)

    # ── Command building ──────────────────────────────────────────────

    def test_build_exec_cmd_contains_model(self):
        cmd = self.rt.build_exec_cmd("prompt.txt", "gemma", "medium")
        assert "gemma4:26b" in cmd
        assert "ollama_react_worker.py" in cmd

    def test_build_exec_cmd_prompt_file_quoted(self):
        cmd = self.rt.build_exec_cmd("/some/path/prompt.txt", "gemma", "medium")
        assert "/some/path/prompt.txt" in cmd

    def test_build_exec_cmd_bash_variable(self):
        cmd = self.rt.build_exec_cmd("${_PROMPT_FILE}", "gemma-large", "high")
        assert "gemma4:31b" in cmd
        assert "${_PROMPT_FILE}" in cmd

    def test_build_exec_cmd_ignores_context_dirs(self):
        cmd_without = self.rt.build_exec_cmd("p.txt", "gemma", "medium")
        cmd_with = self.rt.build_exec_cmd("p.txt", "gemma", "medium",
                                           context_dirs=["/some/dir"])
        assert cmd_without == cmd_with

    # ── Process detection ─────────────────────────────────────────────

    def test_detect_worker_process_match(self):
        assert self.rt.detect_worker_process("python3 /boi/lib/ollama_react_worker.py --model gemma4:26b")

    def test_detect_worker_process_no_match(self):
        assert not self.rt.detect_worker_process("claude -p prompt.txt")
        assert not self.rt.detect_worker_process("hermes chat -q prompt")
