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
        self.assertEqual(result, ("claude-opus-4-6", "high"))

    def test_returns_none_when_no_model(self) -> None:
        result = parse_task_model(TASK_WITHOUT_MODEL)
        self.assertIsNone(result)

    def test_parses_haiku(self) -> None:
        result = parse_task_model(TASK_WITH_HAIKU)
        self.assertEqual(result, ("claude-haiku-4-5-20251001", "low"))

    def test_parses_sonnet(self) -> None:
        task = TASK_WITH_MODEL.replace("opus", "sonnet")
        result = parse_task_model(task)
        self.assertEqual(result, ("claude-sonnet-4-6", "medium"))


class TestWorkerModelOverride(unittest.TestCase):
    """Test that Worker._claude_cmd respects per-task model."""

    def test_override_changes_model(self) -> None:
        from worker import Worker
        w = Worker.__new__(Worker)  # skip __init__
        w.phase = "execute"
        w._model_routing = {"execute": ("claude-sonnet-4-6", "medium")}
        cmd = w._claude_cmd(model_override=("claude-opus-4-6", "high"))
        self.assertIn("claude-opus-4-6", cmd)
        self.assertIn("--effort high", cmd)

    def test_no_override_uses_phase_default(self) -> None:
        from worker import Worker
        w = Worker.__new__(Worker)
        w.phase = "execute"
        w._model_routing = {"execute": ("claude-sonnet-4-6", "medium")}
        cmd = w._claude_cmd()
        self.assertIn("claude-sonnet-4-6", cmd)
        self.assertIn("--effort medium", cmd)


if __name__ == "__main__":
    unittest.main()
