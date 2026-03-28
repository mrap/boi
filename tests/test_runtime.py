"""test_runtime.py — Unit tests for src/lib/runtime.py

Covers:
  1. ClaudeRuntime.build_exec_cmd output
  2. CodexRuntime.build_exec_cmd output
  3. Model alias mapping for both runtimes
  4. Cost table lookup for both runtimes
  5. get_runtime() factory
  6. Config loading: global default, spec override, fallback to claude
  7. CLI detection (mock shutil.which)
"""

import json
import os
import sys
import tempfile
import unittest
from unittest.mock import patch

# Ensure src/ is on the path so `from lib.runtime import ...` works
_SRC_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if _SRC_DIR not in sys.path:
    sys.path.insert(0, _SRC_DIR)

from lib.runtime import (
    DEFAULT_RUNTIME,
    ClaudeRuntime,
    CodexRuntime,
    get_runtime,
    load_runtime_from_config,
    resolve_runtime,
    resolve_spec_runtime,
)


class TestClaudeRuntimeBuildExecCmd(unittest.TestCase):
    def setUp(self):
        self.rt = ClaudeRuntime()

    def test_contains_claude_p(self):
        cmd = self.rt.build_exec_cmd("/tmp/test.txt", "sonnet", "medium")
        self.assertIn("claude -p", cmd)

    def test_contains_env_unset_claudecode(self):
        cmd = self.rt.build_exec_cmd("/tmp/test.txt", "sonnet", "medium")
        self.assertIn("env -u CLAUDECODE", cmd)

    def test_prompt_file_in_cat(self):
        cmd = self.rt.build_exec_cmd("/tmp/test.txt", "sonnet", "medium")
        self.assertIn('/tmp/test.txt', cmd)

    def test_resolves_alias_to_model_id(self):
        cmd = self.rt.build_exec_cmd("/tmp/p.txt", "sonnet", "medium")
        self.assertIn("claude-sonnet-4-6", cmd)

    def test_opus_alias(self):
        cmd = self.rt.build_exec_cmd("/tmp/p.txt", "opus", "high")
        self.assertIn("claude-opus-4-6", cmd)

    def test_haiku_alias(self):
        cmd = self.rt.build_exec_cmd("/tmp/p.txt", "haiku", "low")
        self.assertIn("claude-haiku-4-5-20251001", cmd)

    def test_full_model_id_passthrough(self):
        cmd = self.rt.build_exec_cmd("/tmp/p.txt", "claude-opus-4-6", "high")
        self.assertIn("claude-opus-4-6", cmd)

    def test_output_format_flag(self):
        cmd = self.rt.build_exec_cmd("/tmp/p.txt", "sonnet", "medium")
        self.assertIn("--output-format stream-json", cmd)

    def test_dangerously_skip_permissions_flag(self):
        cmd = self.rt.build_exec_cmd("/tmp/p.txt", "sonnet", "medium")
        self.assertIn("--dangerously-skip-permissions", cmd)

    def test_effort_flag_present(self):
        cmd = self.rt.build_exec_cmd("/tmp/p.txt", "sonnet", "medium")
        self.assertIn("--effort", cmd)

    def test_effort_matches_alias(self):
        cmd_sonnet = self.rt.build_exec_cmd("/tmp/p.txt", "sonnet", "ignored")
        self.assertIn("--effort medium", cmd_sonnet)

        cmd_opus = self.rt.build_exec_cmd("/tmp/p.txt", "opus", "ignored")
        self.assertIn("--effort high", cmd_opus)

        cmd_haiku = self.rt.build_exec_cmd("/tmp/p.txt", "haiku", "ignored")
        self.assertIn("--effort low", cmd_haiku)


class TestCodexRuntimeBuildExecCmd(unittest.TestCase):
    def setUp(self):
        self.rt = CodexRuntime()

    def test_starts_with_codex_exec(self):
        cmd = self.rt.build_exec_cmd("/tmp/test.txt", "sonnet", "medium")
        self.assertIn("codex exec", cmd)

    def test_no_claude_in_command(self):
        cmd = self.rt.build_exec_cmd("/tmp/test.txt", "sonnet", "medium")
        self.assertNotIn("claude", cmd)

    def test_model_flag_present(self):
        cmd = self.rt.build_exec_cmd("/tmp/test.txt", "sonnet", "medium")
        self.assertIn("--model", cmd)

    def test_resolves_sonnet_to_o4_mini(self):
        cmd = self.rt.build_exec_cmd("/tmp/p.txt", "sonnet", "medium")
        self.assertIn("o4-mini", cmd)

    def test_resolves_opus_to_o3(self):
        cmd = self.rt.build_exec_cmd("/tmp/p.txt", "opus", "high")
        self.assertIn("o3", cmd)

    def test_resolves_haiku_to_o4_mini(self):
        cmd = self.rt.build_exec_cmd("/tmp/p.txt", "haiku", "low")
        self.assertIn("o4-mini", cmd)

    def test_prompt_file_used_as_stdin(self):
        cmd = self.rt.build_exec_cmd("/tmp/p.txt", "sonnet", "medium")
        self.assertIn("/tmp/p.txt", cmd)
        self.assertIn("<", cmd)

    def test_full_model_id_passthrough(self):
        cmd = self.rt.build_exec_cmd("/tmp/p.txt", "o3", "high")
        self.assertIn("o3", cmd)


class TestClaudeRuntimeModelId(unittest.TestCase):
    def setUp(self):
        self.rt = ClaudeRuntime()

    def test_opus_alias(self):
        self.assertEqual(self.rt.model_id("opus"), "claude-opus-4-6")

    def test_sonnet_alias(self):
        self.assertEqual(self.rt.model_id("sonnet"), "claude-sonnet-4-6")

    def test_haiku_alias(self):
        self.assertEqual(self.rt.model_id("haiku"), "claude-haiku-4-5-20251001")

    def test_full_id_passthrough(self):
        self.assertEqual(self.rt.model_id("claude-sonnet-4-6"), "claude-sonnet-4-6")

    def test_case_insensitive(self):
        self.assertEqual(self.rt.model_id("OPUS"), "claude-opus-4-6")
        self.assertEqual(self.rt.model_id("Sonnet"), "claude-sonnet-4-6")

    def test_unknown_returns_as_is(self):
        self.assertEqual(self.rt.model_id("gpt-4o"), "gpt-4o")


class TestCodexRuntimeModelId(unittest.TestCase):
    def setUp(self):
        self.rt = CodexRuntime()

    def test_opus_alias(self):
        self.assertEqual(self.rt.model_id("opus"), "o3")

    def test_sonnet_alias(self):
        self.assertEqual(self.rt.model_id("sonnet"), "o4-mini")

    def test_haiku_alias(self):
        self.assertEqual(self.rt.model_id("haiku"), "o4-mini")

    def test_full_id_passthrough(self):
        self.assertEqual(self.rt.model_id("o3"), "o3")

    def test_case_insensitive(self):
        self.assertEqual(self.rt.model_id("OPUS"), "o3")

    def test_unknown_returns_as_is(self):
        self.assertEqual(self.rt.model_id("gpt-4o"), "gpt-4o")


class TestClaudeRuntimeCostPerToken(unittest.TestCase):
    def setUp(self):
        self.rt = ClaudeRuntime()

    def test_opus_cost(self):
        inp, out = self.rt.cost_per_token("claude-opus-4-6")
        self.assertEqual(inp, 15.0)
        self.assertEqual(out, 75.0)

    def test_sonnet_cost(self):
        inp, out = self.rt.cost_per_token("claude-sonnet-4-6")
        self.assertEqual(inp, 3.0)
        self.assertEqual(out, 15.0)

    def test_haiku_cost(self):
        inp, out = self.rt.cost_per_token("claude-haiku-4-5-20251001")
        self.assertEqual(inp, 1.0)
        self.assertEqual(out, 5.0)

    def test_alias_resolved(self):
        inp, out = self.rt.cost_per_token("opus")
        self.assertEqual(inp, 15.0)

    def test_unknown_model_returns_default(self):
        inp, out = self.rt.cost_per_token("unknown-model-xyz")
        self.assertIsInstance(inp, float)
        self.assertIsInstance(out, float)


class TestCodexRuntimeCostPerToken(unittest.TestCase):
    def setUp(self):
        self.rt = CodexRuntime()

    def test_o3_cost(self):
        inp, out = self.rt.cost_per_token("o3")
        self.assertEqual(inp, 10.0)
        self.assertEqual(out, 40.0)

    def test_o4_mini_cost(self):
        inp, out = self.rt.cost_per_token("o4-mini")
        self.assertEqual(inp, 1.1)
        self.assertEqual(out, 4.4)

    def test_alias_resolved(self):
        inp, out = self.rt.cost_per_token("opus")
        self.assertEqual(inp, 10.0)

    def test_unknown_model_returns_default(self):
        inp, out = self.rt.cost_per_token("unknown-model")
        self.assertIsInstance(inp, float)
        self.assertIsInstance(out, float)


class TestGetRuntimeFactory(unittest.TestCase):
    def test_returns_claude_runtime(self):
        rt = get_runtime("claude")
        self.assertIsInstance(rt, ClaudeRuntime)

    def test_returns_codex_runtime(self):
        rt = get_runtime("codex")
        self.assertIsInstance(rt, CodexRuntime)

    def test_case_insensitive_claude(self):
        rt = get_runtime("CLAUDE")
        self.assertIsInstance(rt, ClaudeRuntime)

    def test_case_insensitive_codex(self):
        rt = get_runtime("Codex")
        self.assertIsInstance(rt, CodexRuntime)

    def test_unknown_raises_value_error(self):
        with self.assertRaises(ValueError):
            get_runtime("openai")

    def test_default_runtime_constant(self):
        self.assertEqual(DEFAULT_RUNTIME, "claude")

    def test_default_gives_claude_runtime(self):
        rt = get_runtime(DEFAULT_RUNTIME)
        self.assertIsInstance(rt, ClaudeRuntime)


class TestConfigLoading(unittest.TestCase):
    def test_load_runtime_from_config_claude(self):
        with tempfile.TemporaryDirectory() as d:
            cfg = {"runtime": {"default": "claude"}}
            with open(os.path.join(d, "config.json"), "w") as f:
                json.dump(cfg, f)
            result = load_runtime_from_config(d)
        self.assertEqual(result, "claude")

    def test_load_runtime_from_config_codex(self):
        with tempfile.TemporaryDirectory() as d:
            cfg = {"runtime": {"default": "codex"}}
            with open(os.path.join(d, "config.json"), "w") as f:
                json.dump(cfg, f)
            result = load_runtime_from_config(d)
        self.assertEqual(result, "codex")

    def test_load_runtime_missing_config_returns_default(self):
        with tempfile.TemporaryDirectory() as d:
            result = load_runtime_from_config(d)
        self.assertEqual(result, DEFAULT_RUNTIME)

    def test_load_runtime_missing_key_returns_default(self):
        with tempfile.TemporaryDirectory() as d:
            with open(os.path.join(d, "config.json"), "w") as f:
                json.dump({}, f)
            result = load_runtime_from_config(d)
        self.assertEqual(result, DEFAULT_RUNTIME)

    def test_load_runtime_corrupt_json_returns_default(self):
        with tempfile.TemporaryDirectory() as d:
            with open(os.path.join(d, "config.json"), "w") as f:
                f.write("{ not json }")
            result = load_runtime_from_config(d)
        self.assertEqual(result, DEFAULT_RUNTIME)


class TestSpecRuntimeOverride(unittest.TestCase):
    def test_spec_runtime_claude(self):
        spec = "# My Spec\n**Runtime:** claude\n\n### t-1: task\nPENDING\n"
        self.assertEqual(resolve_spec_runtime(spec), "claude")

    def test_spec_runtime_codex(self):
        spec = "# My Spec\n**Runtime:** codex\n\n### t-1: task\nPENDING\n"
        self.assertEqual(resolve_spec_runtime(spec), "codex")

    def test_spec_runtime_absent_returns_none(self):
        spec = "# My Spec\n\n### t-1: task\nPENDING\n"
        self.assertIsNone(resolve_spec_runtime(spec))

    def test_spec_runtime_after_task_heading_ignored(self):
        # Runtime line after first task heading should not be parsed
        spec = "# My Spec\n\n### t-1: task\n**Runtime:** codex\nPENDING\n"
        self.assertIsNone(resolve_spec_runtime(spec))

    def test_spec_runtime_unknown_returns_none(self):
        spec = "# My Spec\n**Runtime:** openai\n\n### t-1: task\nPENDING\n"
        self.assertIsNone(resolve_spec_runtime(spec))

    def test_resolve_runtime_spec_overrides_config(self):
        spec = "**Runtime:** claude\n### t-1:\n"
        with tempfile.TemporaryDirectory() as d:
            cfg = {"runtime": {"default": "codex"}}
            with open(os.path.join(d, "config.json"), "w") as f:
                json.dump(cfg, f)
            result = resolve_runtime(state_dir=d, spec_content=spec)
        self.assertEqual(result, "claude")

    def test_resolve_runtime_falls_back_to_config(self):
        spec = "# No runtime header\n### t-1:\n"
        with tempfile.TemporaryDirectory() as d:
            cfg = {"runtime": {"default": "codex"}}
            with open(os.path.join(d, "config.json"), "w") as f:
                json.dump(cfg, f)
            result = resolve_runtime(state_dir=d, spec_content=spec)
        self.assertEqual(result, "codex")

    def test_resolve_runtime_falls_back_to_default(self):
        result = resolve_runtime(state_dir=None, spec_content="")
        self.assertEqual(result, DEFAULT_RUNTIME)


class TestCliDetection(unittest.TestCase):
    def test_claude_found(self):
        rt = ClaudeRuntime()
        with patch("shutil.which", return_value="/usr/local/bin/claude"):
            ok, msg = rt.check_installed()
        self.assertTrue(ok)
        self.assertIn("claude", msg)
        self.assertIn("/usr/local/bin/claude", msg)

    def test_claude_not_found(self):
        rt = ClaudeRuntime()
        with patch("shutil.which", return_value=None):
            ok, msg = rt.check_installed()
        self.assertFalse(ok)
        self.assertIn("claude", msg.lower())

    def test_codex_found(self):
        rt = CodexRuntime()
        with patch("shutil.which", return_value="/usr/local/bin/codex"):
            ok, msg = rt.check_installed()
        self.assertTrue(ok)
        self.assertIn("codex", msg)
        self.assertIn("/usr/local/bin/codex", msg)

    def test_codex_not_found(self):
        rt = CodexRuntime()
        with patch("shutil.which", return_value=None):
            ok, msg = rt.check_installed()
        self.assertFalse(ok)
        self.assertIn("codex", msg.lower())

    def test_claude_detect_worker_process_true(self):
        rt = ClaudeRuntime()
        self.assertTrue(rt.detect_worker_process("claude -p /tmp/prompt.txt"))

    def test_claude_detect_worker_process_false(self):
        rt = ClaudeRuntime()
        self.assertFalse(rt.detect_worker_process("python3 worker.py"))

    def test_codex_detect_worker_process_true(self):
        rt = CodexRuntime()
        self.assertTrue(rt.detect_worker_process("codex exec --model o3"))

    def test_codex_detect_worker_process_false(self):
        rt = CodexRuntime()
        self.assertFalse(rt.detect_worker_process("claude -p /tmp/prompt.txt"))


class TestRuntimeAttributes(unittest.TestCase):
    def test_claude_name(self):
        self.assertEqual(ClaudeRuntime.name, "claude")

    def test_claude_cli_command(self):
        self.assertEqual(ClaudeRuntime.cli_command, "claude")

    def test_codex_name(self):
        self.assertEqual(CodexRuntime.name, "codex")

    def test_codex_cli_command(self):
        self.assertEqual(CodexRuntime.cli_command, "codex")


if __name__ == "__main__":
    unittest.main()
