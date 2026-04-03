"""Tests for context injection via --add-dir."""

import json
import os
import sys
import unittest.mock as mock

import pytest

# Add repo root to path for imports
sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from lib.runtime import ClaudeRuntime, load_context_root


class TestLoadContextRoot:
    def test_returns_none_when_no_config(self, tmp_path):
        """No config.json => None (backwards compatible)."""
        result = load_context_root(str(tmp_path))
        assert result is None

    def test_returns_none_when_key_missing(self, tmp_path):
        """Config exists but no context_root key => None."""
        config = {"version": "1", "worker_count": 5}
        (tmp_path / "config.json").write_text(json.dumps(config))
        result = load_context_root(str(tmp_path))
        assert result is None

    def test_returns_expanded_path(self, tmp_path):
        """context_root with ~ gets expanded."""
        config = {"version": "1", "context_root": "~/mrap-hex"}
        (tmp_path / "config.json").write_text(json.dumps(config))
        result = load_context_root(str(tmp_path))
        assert result == os.path.expanduser("~/mrap-hex")
        assert "~" not in result

    def test_returns_absolute_path_unchanged(self, tmp_path):
        """Absolute path passes through expanduser unchanged."""
        agent_dir = tmp_path / "agent"
        agent_dir.mkdir()
        config = {"version": "1", "context_root": str(agent_dir)}
        (tmp_path / "config.json").write_text(json.dumps(config))
        result = load_context_root(str(tmp_path))
        assert result == str(agent_dir)

    def test_returns_none_for_empty_string(self, tmp_path):
        """Empty string treated as unset."""
        config = {"version": "1", "context_root": ""}
        (tmp_path / "config.json").write_text(json.dumps(config))
        result = load_context_root(str(tmp_path))
        assert result is None

    def test_returns_none_for_nonexistent_path(self, tmp_path):
        """Path that doesn't exist on disk => None (prevents silent misconfiguration)."""
        config = {"version": "1", "context_root": "/nonexistent/path/agent"}
        (tmp_path / "config.json").write_text(json.dumps(config))
        result = load_context_root(str(tmp_path))
        assert result is None


class TestBuildExecCmdWithContextDirs:
    def test_no_context_dirs_unchanged(self):
        """Without context_dirs, command is identical to current behavior."""
        rt = ClaudeRuntime()
        cmd = rt.build_exec_cmd("${_PROMPT_FILE}", "sonnet", "medium")
        assert "--add-dir" not in cmd
        assert "--dangerously-skip-permissions" in cmd

    def test_single_context_dir_appended(self):
        """Single context dir adds one --add-dir flag."""
        rt = ClaudeRuntime()
        cmd = rt.build_exec_cmd(
            "${_PROMPT_FILE}", "sonnet", "medium",
            context_dirs=["/home/user/agent"],
        )
        assert "--add-dir /home/user/agent" in cmd
        assert "--dangerously-skip-permissions" in cmd

    def test_multiple_context_dirs(self):
        """Multiple dirs each get their own --add-dir flag."""
        rt = ClaudeRuntime()
        cmd = rt.build_exec_cmd(
            "${_PROMPT_FILE}", "sonnet", "medium",
            context_dirs=["/home/user/agent", "/home/user/shared"],
        )
        assert "--add-dir /home/user/agent" in cmd
        assert "--add-dir /home/user/shared" in cmd

    def test_empty_list_no_flag(self):
        """Empty list is same as no context_dirs."""
        rt = ClaudeRuntime()
        cmd = rt.build_exec_cmd(
            "${_PROMPT_FILE}", "sonnet", "medium",
            context_dirs=[],
        )
        assert "--add-dir" not in cmd

    def test_path_with_spaces_is_quoted(self):
        """Paths with spaces are shell-quoted."""
        rt = ClaudeRuntime()
        cmd = rt.build_exec_cmd(
            "${_PROMPT_FILE}", "sonnet", "medium",
            context_dirs=["/home/user/my agent"],
        )
        assert "--add-dir" in cmd
        # Should be quoted in some form
        assert "my agent" in cmd


class TestWorkerContextInjection:
    def test_worker_passes_context_root_to_runtime(self, tmp_path):
        """Worker reads context_root from config and passes to build_exec_cmd."""
        state_dir = str(tmp_path / "boi")
        os.makedirs(os.path.join(state_dir, "queue"), exist_ok=True)
        os.makedirs(os.path.join(state_dir, "logs"), exist_ok=True)
        agent_dir = tmp_path / "agent"
        agent_dir.mkdir()
        config = {"version": "1", "context_root": str(agent_dir)}
        (tmp_path / "boi" / "config.json").write_text(json.dumps(config))

        spec_path = os.path.join(state_dir, "queue", "q-999.spec.md")
        with open(spec_path, "w") as f:
            f.write("# Test Spec\n\n**Mode:** execute\n\n### t-1: Do thing\nPENDING\n")

        worktree = str(tmp_path / "worktree")
        os.makedirs(worktree, exist_ok=True)

        from worker import Worker
        w = Worker(
            spec_id="q-999",
            worktree=worktree,
            spec_path=spec_path,
            iteration=1,
            phase="execute",
            state_dir=state_dir,
        )

        mock_rt = mock.MagicMock()
        mock_rt.build_exec_cmd.return_value = "echo test"
        w.runtime = mock_rt

        w._build_exec_cmd()
        mock_rt.build_exec_cmd.assert_called_once()
        call_kwargs = mock_rt.build_exec_cmd.call_args
        assert call_kwargs[1].get("context_dirs") == [str(agent_dir)]

    def test_worker_no_context_root_passes_none(self, tmp_path):
        """Without context_root in config, context_dirs is None or empty."""
        state_dir = str(tmp_path / "boi")
        os.makedirs(os.path.join(state_dir, "queue"), exist_ok=True)
        os.makedirs(os.path.join(state_dir, "logs"), exist_ok=True)
        config = {"version": "1"}
        (tmp_path / "boi" / "config.json").write_text(json.dumps(config))

        spec_path = os.path.join(state_dir, "queue", "q-999.spec.md")
        with open(spec_path, "w") as f:
            f.write("# Test Spec\n\n**Mode:** execute\n\n### t-1: Do thing\nPENDING\n")

        worktree = str(tmp_path / "worktree")
        os.makedirs(worktree, exist_ok=True)

        from worker import Worker
        w = Worker(
            spec_id="q-999",
            worktree=worktree,
            spec_path=spec_path,
            iteration=1,
            phase="execute",
            state_dir=state_dir,
        )

        mock_rt = mock.MagicMock()
        mock_rt.build_exec_cmd.return_value = "echo test"
        w.runtime = mock_rt

        w._build_exec_cmd()
        call_kwargs = mock_rt.build_exec_cmd.call_args
        ctx = call_kwargs[1].get("context_dirs")
        assert ctx is None or ctx == []


class TestIntegrationRunScript:
    """Verify the generated run script contains --add-dir when configured."""

    def test_generated_run_script_contains_add_dir(self, tmp_path):
        """Full pipeline: config -> Worker -> run.sh contains --add-dir."""
        state_dir = str(tmp_path / "boi")
        queue_dir = os.path.join(state_dir, "queue")
        os.makedirs(queue_dir, exist_ok=True)
        os.makedirs(os.path.join(state_dir, "logs"), exist_ok=True)

        agent_dir = str(tmp_path / "agent")
        os.makedirs(agent_dir, exist_ok=True)

        config = {"version": "1", "context_root": agent_dir}
        (tmp_path / "boi" / "config.json").write_text(json.dumps(config))

        spec_path = os.path.join(queue_dir, "q-999.spec.md")
        with open(spec_path, "w") as f:
            f.write("# Test\n\n**Mode:** execute\n\n### t-1: Task\nPENDING\n\n**Spec:** Do work.\n\n**Verify:** Check.\n")

        worktree = str(tmp_path / "worktree")
        os.makedirs(worktree, exist_ok=True)

        from worker import Worker
        w = Worker(
            spec_id="q-999",
            worktree=worktree,
            spec_path=spec_path,
            iteration=1,
            phase="execute",
            state_dir=state_dir,
        )

        spec_content = open(spec_path).read()
        w.runtime = ClaudeRuntime()
        w.pre_counts = {"pending": 1, "done": 0, "skipped": 0, "total": 1}
        w.generate_run_script(spec_content)

        run_script = open(w.run_script).read()
        assert f"--add-dir {agent_dir}" in run_script or f"--add-dir '{agent_dir}'" in run_script

    def test_generated_run_script_no_add_dir_without_config(self, tmp_path):
        """Without context_root, run.sh has no --add-dir."""
        state_dir = str(tmp_path / "boi")
        queue_dir = os.path.join(state_dir, "queue")
        os.makedirs(queue_dir, exist_ok=True)
        os.makedirs(os.path.join(state_dir, "logs"), exist_ok=True)

        config = {"version": "1"}
        (tmp_path / "boi" / "config.json").write_text(json.dumps(config))

        spec_path = os.path.join(queue_dir, "q-999.spec.md")
        with open(spec_path, "w") as f:
            f.write("# Test\n\n**Mode:** execute\n\n### t-1: Task\nPENDING\n\n**Spec:** Do work.\n\n**Verify:** Check.\n")

        worktree = str(tmp_path / "worktree")
        os.makedirs(worktree, exist_ok=True)

        from worker import Worker
        w = Worker(
            spec_id="q-999",
            worktree=worktree,
            spec_path=spec_path,
            iteration=1,
            phase="execute",
            state_dir=state_dir,
        )

        spec_content = open(spec_path).read()
        w.runtime = ClaudeRuntime()
        w.pre_counts = {"pending": 1, "done": 0, "skipped": 0, "total": 1}
        w.generate_run_script(spec_content)

        run_script = open(w.run_script).read()
        assert "--add-dir" not in run_script
