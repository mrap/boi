"""Tests for per-task model routing in BOI worker."""
import unittest

from worker import parse_task_model


TASK_WITH_MODEL = """\
### t-1: Deep research task
PENDING

**Model:** opus

**Spec:** Do deep research.

**Verify:** check output
"""

TASK_WITHOUT_MODEL = """\
### t-1: Normal task
PENDING

**Spec:** Do normal work.

**Verify:** check output
"""

TASK_WITH_HAIKU = """\
### t-1: Simple file task
PENDING

**Model:** haiku

**Spec:** Move a file.

**Verify:** check file
"""


class TestParseTaskModel(unittest.TestCase):
    def test_parses_opus(self) -> None:
        result = parse_task_model(TASK_WITH_MODEL)
        self.assertEqual(result, "opus")

    def test_returns_none_when_no_model(self) -> None:
        result = parse_task_model(TASK_WITHOUT_MODEL)
        self.assertIsNone(result)

    def test_parses_haiku(self) -> None:
        result = parse_task_model(TASK_WITH_HAIKU)
        self.assertEqual(result, "haiku")

    def test_parses_sonnet(self) -> None:
        task = TASK_WITH_MODEL.replace("opus", "sonnet")
        result = parse_task_model(task)
        self.assertEqual(result, "sonnet")


class TestWorkerModelOverride(unittest.TestCase):
    """Test that Worker._build_exec_cmd respects per-task model and phase_config."""

    def test_override_changes_model(self) -> None:
        from worker import Worker
        from lib.runtime import ClaudeRuntime
        from lib.phases import PhaseConfig
        w = Worker.__new__(Worker)  # skip __init__
        w.phase = "execute"
        w.runtime = ClaudeRuntime()
        w.context_root = None
        w.phase_config = PhaseConfig(
            name="execute", prompt_template="t.md", approve_signal="",
            model="deepseek/deepseek-v3.2", effort="medium",
        )
        cmd = w._build_exec_cmd(model_override="opus")
        self.assertIn("claude-opus-4-6", cmd)
        self.assertIn("--effort high", cmd)

    def test_no_override_uses_phase_config(self) -> None:
        from worker import Worker
        from lib.runtime import ClaudeRuntime
        from lib.phases import PhaseConfig
        w = Worker.__new__(Worker)
        w.phase = "execute"
        w.runtime = ClaudeRuntime()
        w.context_root = None
        w.phase_config = PhaseConfig(
            name="execute", prompt_template="t.md", approve_signal="",
            model="claude-sonnet-4-6", effort="medium",
        )
        cmd = w._build_exec_cmd()
        self.assertIn("claude-sonnet-4-6", cmd)
        self.assertIn("--effort medium", cmd)

    def test_no_phase_config_uses_fallback(self) -> None:
        from worker import Worker
        from lib.runtime import ClaudeRuntime
        w = Worker.__new__(Worker)
        w.phase = "execute"
        w.runtime = ClaudeRuntime()
        w.context_root = None
        w.phase_config = None
        cmd = w._build_exec_cmd()
        # Fallback is deepseek/deepseek-v3.2 which ClaudeRuntime won't know,
        # but the string should be passed through
        self.assertIn("--effort medium", cmd)


class TestResolveExecuteModel(unittest.TestCase):
    """Test that _resolve_execute_model uses phase_config.code_model."""

    def test_code_task_returns_code_model(self) -> None:
        from worker import Worker
        from lib.phases import PhaseConfig
        w = Worker.__new__(Worker)
        w.phase_config = PhaseConfig(
            name="execute", prompt_template="t.md", approve_signal="",
            model="deepseek/deepseek-v3.2", code_model="minimax/minimax-m2.5",
        )
        result = w._resolve_execute_model("implement a new function")
        self.assertEqual(result, "minimax/minimax-m2.5")

    def test_non_code_task_returns_none(self) -> None:
        from worker import Worker
        from lib.phases import PhaseConfig
        w = Worker.__new__(Worker)
        w.phase_config = PhaseConfig(
            name="execute", prompt_template="t.md", approve_signal="",
            model="deepseek/deepseek-v3.2", code_model="minimax/minimax-m2.5",
        )
        result = w._resolve_execute_model("write a blog post about cooking")
        self.assertIsNone(result)

    def test_no_code_model_returns_none(self) -> None:
        from worker import Worker
        from lib.phases import PhaseConfig
        w = Worker.__new__(Worker)
        w.phase_config = PhaseConfig(
            name="execute", prompt_template="t.md", approve_signal="",
            model="deepseek/deepseek-v3.2",
        )
        result = w._resolve_execute_model("implement a new function")
        self.assertIsNone(result)

    def test_no_phase_config_returns_none(self) -> None:
        from worker import Worker
        w = Worker.__new__(Worker)
        w.phase_config = None
        result = w._resolve_execute_model("implement a new function")
        self.assertIsNone(result)


if __name__ == "__main__":
    unittest.main()
