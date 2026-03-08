# test_critic.py — Comprehensive tests for the BOI critic system.
#
# Covers:
#   TestCriticConfig — config loading, defaults, custom check discovery, prompt override
#   TestCriticExecution — trigger on completion, skip when disabled, max passes enforcement
#   TestCriticTaskInjection — CRITIC tasks added to spec, spec requeued after critic
#   TestCriticApproval — Critic Approved section detection, spec marked completed
#   TestCriticModularity — custom checks loaded, custom checks override defaults, custom prompt
#   TestCriticIntegration — full flow end-to-end with mock data

import json
import os
import shutil
import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

from lib.critic import (
    generate_critic_prompt,
    parse_critic_result,
    run_critic,
    should_run_critic,
)
from lib.critic_config import (
    DEFAULT_CONFIG,
    ensure_critic_dirs,
    get_active_checks,
    get_critic_prompt,
    is_critic_enabled,
    load_critic_config,
    save_critic_config,
)


def _make_temp_env(tmpdir):
    """Create a standard temp environment for critic tests.

    Returns a dict with all paths needed for critic testing.
    """
    state_dir = os.path.join(tmpdir, "state")
    queue_dir = os.path.join(state_dir, "queue")
    events_dir = os.path.join(state_dir, "events")
    hooks_dir = os.path.join(state_dir, "hooks")
    log_dir = os.path.join(state_dir, "logs")
    boi_dir = os.path.join(tmpdir, "boi")
    checks_dir = os.path.join(boi_dir, "templates", "checks")
    critic_dir = os.path.join(state_dir, "critic")
    custom_dir = os.path.join(critic_dir, "custom")

    for d in [queue_dir, events_dir, hooks_dir, log_dir, checks_dir, custom_dir]:
        os.makedirs(d, exist_ok=True)

    # Write default critic prompt template
    Path(os.path.join(boi_dir, "templates", "critic-prompt.md")).write_text(
        "# Critic Pass {{ITERATION}}\n"
        "Queue: {{QUEUE_ID}}\n"
        "Spec: {{SPEC_PATH}}\n\n"
        "{{SPEC_CONTENT}}\n\n"
        "## Checks\n{{CHECKS}}\n"
    )

    # Write default check files
    for name in DEFAULT_CONFIG["checks"]:
        Path(os.path.join(checks_dir, f"{name}.md")).write_text(
            f"# {name}\n\nCheck content for {name}.\n\n"
            f"- [ ] Item 1 for {name}\n"
            f"- [ ] Item 2 for {name}\n"
            f"- [ ] Item 3 for {name}\n"
        )

    # Write default config
    Path(os.path.join(critic_dir, "config.json")).write_text(
        json.dumps(DEFAULT_CONFIG, indent=2) + "\n"
    )

    return {
        "state_dir": state_dir,
        "queue_dir": queue_dir,
        "events_dir": events_dir,
        "hooks_dir": hooks_dir,
        "log_dir": log_dir,
        "boi_dir": boi_dir,
        "checks_dir": checks_dir,
        "critic_dir": critic_dir,
        "custom_dir": custom_dir,
    }


def _write_queue_entry(queue_dir, queue_id, **overrides):
    """Write a queue entry JSON file."""
    entry = {
        "id": queue_id,
        "spec_path": "",
        "status": "running",
        "iteration": 1,
        "max_iterations": 30,
        "critic_passes": 0,
        "tasks_done": 0,
        "tasks_total": 0,
        "consecutive_failures": 0,
    }
    entry.update(overrides)
    path = os.path.join(queue_dir, f"{queue_id}.json")
    Path(path).write_text(json.dumps(entry, indent=2) + "\n")
    return entry


def _write_spec(path, content):
    """Write a spec file."""
    Path(path).write_text(content)
    return path


class TestCriticConfig(unittest.TestCase):
    """Config loading, defaults, custom check discovery, prompt override."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.env = _make_temp_env(self.tmpdir)

    def tearDown(self):
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_load_config_creates_defaults(self):
        """Loading config from empty dir creates defaults."""
        fresh_dir = os.path.join(self.tmpdir, "fresh")
        os.makedirs(fresh_dir)
        config = load_critic_config(fresh_dir)
        self.assertTrue(config["enabled"])
        self.assertEqual(config["max_passes"], 2)
        self.assertEqual(config["trigger"], "on_complete")
        self.assertEqual(len(config["checks"]), 5)

    def test_load_config_merges_partial(self):
        """Partial config file gets missing keys from defaults."""
        critic_dir = os.path.join(self.tmpdir, "partial", "critic")
        os.makedirs(critic_dir, exist_ok=True)
        Path(os.path.join(critic_dir, "config.json")).write_text(
            json.dumps({"enabled": False, "max_passes": 5})
        )
        config = load_critic_config(os.path.join(self.tmpdir, "partial"))
        self.assertFalse(config["enabled"])
        self.assertEqual(config["max_passes"], 5)
        # Merged from defaults
        self.assertEqual(config["trigger"], "on_complete")
        self.assertEqual(config["timeout_seconds"], 600)

    def test_load_config_handles_corrupt_json(self):
        """Corrupt config falls back to defaults."""
        critic_dir = os.path.join(self.tmpdir, "corrupt", "critic")
        os.makedirs(critic_dir, exist_ok=True)
        Path(os.path.join(critic_dir, "config.json")).write_text("{invalid json!!")
        config = load_critic_config(os.path.join(self.tmpdir, "corrupt"))
        self.assertEqual(config, DEFAULT_CONFIG)

    def test_is_critic_enabled_default(self):
        self.assertTrue(is_critic_enabled(DEFAULT_CONFIG))

    def test_is_critic_enabled_false(self):
        self.assertFalse(is_critic_enabled({"enabled": False}))

    def test_is_critic_enabled_missing_key(self):
        self.assertTrue(is_critic_enabled({}))

    def test_save_and_reload(self):
        config = dict(DEFAULT_CONFIG)
        config["max_passes"] = 10
        config["enabled"] = False
        save_critic_config(self.env["state_dir"], config)
        loaded = load_critic_config(self.env["state_dir"])
        self.assertFalse(loaded["enabled"])
        self.assertEqual(loaded["max_passes"], 10)

    def test_ensure_critic_dirs(self):
        fresh_dir = os.path.join(self.tmpdir, "ensure_test")
        os.makedirs(fresh_dir)
        ensure_critic_dirs(fresh_dir)
        self.assertTrue(os.path.isdir(os.path.join(fresh_dir, "critic")))
        self.assertTrue(os.path.isdir(os.path.join(fresh_dir, "critic", "custom")))
        self.assertTrue(
            os.path.isfile(os.path.join(fresh_dir, "critic", "config.json"))
        )

    def test_ensure_critic_dirs_no_overwrite(self):
        critic_dir = os.path.join(self.tmpdir, "no_overwrite", "critic")
        os.makedirs(critic_dir, exist_ok=True)
        Path(os.path.join(critic_dir, "config.json")).write_text(
            json.dumps({"enabled": False})
        )
        ensure_critic_dirs(os.path.join(self.tmpdir, "no_overwrite"))
        with open(os.path.join(critic_dir, "config.json")) as f:
            config = json.load(f)
        self.assertFalse(config["enabled"])

    def test_custom_check_discovery(self):
        """Custom checks in custom/ dir are discovered."""
        Path(os.path.join(self.env["custom_dir"], "security.md")).write_text(
            "# Security\nCustom security check.\n- [ ] Check 1\n"
        )
        config = load_critic_config(self.env["state_dir"])
        checks = get_active_checks(
            config, self.env["checks_dir"], self.env["state_dir"]
        )
        names = [c["name"] for c in checks]
        self.assertIn("security", names)
        security = [c for c in checks if c["name"] == "security"]
        self.assertEqual(security[0]["source"], "custom")

    def test_prompt_override(self):
        """User prompt.md overrides default."""
        user_prompt = os.path.join(self.env["critic_dir"], "prompt.md")
        Path(user_prompt).write_text("# My Custom Critic\nDo things differently.\n")
        prompt = get_critic_prompt(self.env["state_dir"], self.env["boi_dir"])
        self.assertIn("My Custom Critic", prompt)
        self.assertNotIn("{{SPEC_CONTENT}}", prompt)

    def test_prompt_default_fallback(self):
        """Without user override, default prompt is loaded."""
        prompt = get_critic_prompt(self.env["state_dir"], self.env["boi_dir"])
        self.assertIn("{{SPEC_CONTENT}}", prompt)

    def test_prompt_missing_raises(self):
        """No prompt anywhere raises FileNotFoundError."""
        empty_state = os.path.join(self.tmpdir, "empty_state")
        empty_boi = os.path.join(self.tmpdir, "empty_boi")
        os.makedirs(os.path.join(empty_state, "critic"), exist_ok=True)
        os.makedirs(os.path.join(empty_boi, "templates"), exist_ok=True)
        with self.assertRaises(FileNotFoundError):
            get_critic_prompt(empty_state, empty_boi)


class TestCriticExecution(unittest.TestCase):
    """Trigger on completion, skip when disabled, max passes enforcement."""

    def test_should_run_enabled_under_max(self):
        """Critic runs when enabled and under max passes."""
        config = dict(DEFAULT_CONFIG)
        entry = {"critic_passes": 0}
        self.assertTrue(should_run_critic(entry, config))

    def test_should_run_enabled_at_one_pass(self):
        """Critic runs on second pass when max is 2."""
        config = dict(DEFAULT_CONFIG)
        entry = {"critic_passes": 1}
        self.assertTrue(should_run_critic(entry, config))

    def test_should_not_run_disabled(self):
        """Critic does not run when disabled."""
        config = dict(DEFAULT_CONFIG)
        config["enabled"] = False
        entry = {"critic_passes": 0}
        self.assertFalse(should_run_critic(entry, config))

    def test_should_not_run_max_passes_reached(self):
        """Critic does not run when max passes reached."""
        config = dict(DEFAULT_CONFIG)
        config["max_passes"] = 2
        entry = {"critic_passes": 2}
        self.assertFalse(should_run_critic(entry, config))

    def test_should_not_run_max_passes_exceeded(self):
        """Critic does not run when passes exceed max."""
        config = dict(DEFAULT_CONFIG)
        config["max_passes"] = 1
        entry = {"critic_passes": 3}
        self.assertFalse(should_run_critic(entry, config))

    def test_should_not_run_wrong_trigger(self):
        """Critic does not run with unsupported trigger."""
        config = dict(DEFAULT_CONFIG)
        config["trigger"] = "manual"
        entry = {"critic_passes": 0}
        self.assertFalse(should_run_critic(entry, config))

    def test_should_run_missing_critic_passes(self):
        """Missing critic_passes defaults to 0, so critic runs."""
        config = dict(DEFAULT_CONFIG)
        entry = {}
        self.assertTrue(should_run_critic(entry, config))

    def test_generate_critic_prompt_injects_variables(self):
        """generate_critic_prompt replaces template variables."""
        with tempfile.TemporaryDirectory() as tmpdir:
            env = _make_temp_env(tmpdir)
            spec_path = os.path.join(tmpdir, "test.md")
            _write_spec(spec_path, "# My Spec\n### t-1: Do thing\nDONE\n")

            prompt = generate_critic_prompt(
                spec_path=spec_path,
                queue_id="q-042",
                iteration=2,
                config=DEFAULT_CONFIG,
                boi_dir=env["boi_dir"],
                state_dir=env["state_dir"],
            )

            self.assertIn("q-042", prompt)
            self.assertIn("2", prompt)
            self.assertIn("My Spec", prompt)
            self.assertIn("spec-integrity", prompt)
            self.assertNotIn("{{SPEC_CONTENT}}", prompt)
            self.assertNotIn("{{CHECKS}}", prompt)
            self.assertNotIn("{{QUEUE_ID}}", prompt)
            self.assertNotIn("{{ITERATION}}", prompt)

    def test_generate_critic_prompt_includes_all_checks(self):
        """All configured checks appear in the generated prompt."""
        with tempfile.TemporaryDirectory() as tmpdir:
            env = _make_temp_env(tmpdir)
            spec_path = os.path.join(tmpdir, "test.md")
            _write_spec(spec_path, "# Spec\n")

            prompt = generate_critic_prompt(
                spec_path=spec_path,
                queue_id="q-001",
                iteration=1,
                config=DEFAULT_CONFIG,
                boi_dir=env["boi_dir"],
                state_dir=env["state_dir"],
            )

            for check_name in DEFAULT_CONFIG["checks"]:
                self.assertIn(check_name, prompt)

    def test_generate_critic_prompt_missing_spec_raises(self):
        """Missing spec file raises FileNotFoundError."""
        with tempfile.TemporaryDirectory() as tmpdir:
            env = _make_temp_env(tmpdir)
            with self.assertRaises(FileNotFoundError):
                generate_critic_prompt(
                    spec_path="/nonexistent/spec.md",
                    queue_id="q-001",
                    iteration=1,
                    config=DEFAULT_CONFIG,
                    boi_dir=env["boi_dir"],
                    state_dir=env["state_dir"],
                )

    def test_run_critic_writes_prompt_file(self):
        """run_critic writes a prompt file to the queue dir."""
        with tempfile.TemporaryDirectory() as tmpdir:
            env = _make_temp_env(tmpdir)
            spec_path = os.path.join(tmpdir, "spec.md")
            _write_spec(spec_path, "# Test\n### t-1: Task\nDONE\n")
            _write_queue_entry(env["queue_dir"], "q-001", spec_path=spec_path)

            os.environ["BOI_SCRIPT_DIR"] = env["boi_dir"]
            try:
                result = run_critic(
                    spec_path, env["queue_dir"], "q-001", DEFAULT_CONFIG
                )
                self.assertIn("prompt_path", result)
                self.assertTrue(os.path.isfile(result["prompt_path"]))
                content = Path(result["prompt_path"]).read_text()
                self.assertIn("Test", content)
            finally:
                os.environ.pop("BOI_SCRIPT_DIR", None)


class TestCriticTaskInjection(unittest.TestCase):
    """CRITIC tasks added to spec, spec requeued after critic."""

    def test_parse_finds_critic_pending_tasks(self):
        """parse_critic_result counts [CRITIC] PENDING tasks."""
        with tempfile.TemporaryDirectory() as tmpdir:
            spec_path = os.path.join(tmpdir, "spec.md")
            _write_spec(
                spec_path,
                "# Spec\n\n"
                "### t-1: Original task\nDONE\n\n"
                "### t-2: [CRITIC] Fix missing error handling\nPENDING\n\n"
                "**Spec:** Fix it.\n\n"
                "**Verify:** Run tests.\n\n"
                "### t-3: [CRITIC] Add edge case test\nPENDING\n\n"
                "**Spec:** Add test.\n\n"
                "**Verify:** Tests pass.\n",
            )
            result = parse_critic_result(spec_path)
            self.assertFalse(result["approved"])
            self.assertEqual(result["critic_tasks_added"], 2)

    def test_parse_critic_task_done_not_counted(self):
        """DONE [CRITIC] tasks are not counted as pending."""
        with tempfile.TemporaryDirectory() as tmpdir:
            spec_path = os.path.join(tmpdir, "spec.md")
            _write_spec(
                spec_path,
                "# Spec\n\n"
                "### t-1: [CRITIC] Fixed issue\nDONE\n\n"
                "### t-2: [CRITIC] Still pending\nPENDING\n\n",
            )
            result = parse_critic_result(spec_path)
            self.assertEqual(result["critic_tasks_added"], 1)

    def test_parse_no_critic_tasks(self):
        """Spec with no [CRITIC] tasks and no approval."""
        with tempfile.TemporaryDirectory() as tmpdir:
            spec_path = os.path.join(tmpdir, "spec.md")
            _write_spec(
                spec_path,
                "# Spec\n\n### t-1: Regular task\nDONE\n",
            )
            result = parse_critic_result(spec_path)
            self.assertFalse(result["approved"])
            self.assertEqual(result["critic_tasks_added"], 0)

    def test_parse_missing_spec_file(self):
        """Missing spec file returns safe defaults."""
        result = parse_critic_result("/nonexistent/spec.md")
        self.assertFalse(result["approved"])
        self.assertEqual(result["critic_tasks_added"], 0)

    def test_process_critic_completion_tasks_added(self):
        """process_critic_completion requeues when critic adds tasks."""
        with tempfile.TemporaryDirectory() as tmpdir:
            env = _make_temp_env(tmpdir)
            spec_path = os.path.join(tmpdir, "spec.md")
            _write_spec(
                spec_path,
                "# Spec\n\n"
                "### t-1: Task\nDONE\n\n"
                "### t-2: [CRITIC] Fix bug\nPENDING\n\n"
                "**Spec:** Fix.\n\n**Verify:** Test.\n",
            )
            _write_queue_entry(
                env["queue_dir"],
                "q-001",
                spec_path=spec_path,
                critic_passes=0,
                status="running",
            )

            from lib.daemon_ops import process_critic_completion

            with patch("lib.daemon_ops.run_completion_hooks"):
                result = process_critic_completion(
                    env["queue_dir"],
                    "q-001",
                    env["events_dir"],
                    env["hooks_dir"],
                    spec_path,
                )

            self.assertEqual(result["outcome"], "critic_tasks_added")
            self.assertEqual(result["critic_tasks_added"], 1)

            # Verify critic_passes incremented
            entry_path = os.path.join(env["queue_dir"], "q-001.json")
            entry = json.loads(Path(entry_path).read_text())
            self.assertEqual(entry["critic_passes"], 1)

            # Verify requeued
            self.assertEqual(entry["status"], "requeued")


class TestCriticApproval(unittest.TestCase):
    """Critic Approved section detection, spec marked completed."""

    def test_parse_approved(self):
        """parse_critic_result detects ## Critic Approved."""
        with tempfile.TemporaryDirectory() as tmpdir:
            spec_path = os.path.join(tmpdir, "spec.md")
            _write_spec(
                spec_path,
                "# Spec\n\n### t-1: Task\nDONE\n\n## Critic Approved\n\n2026-03-06\n",
            )
            result = parse_critic_result(spec_path)
            self.assertTrue(result["approved"])
            self.assertEqual(result["critic_tasks_added"], 0)

    def test_parse_approved_case_sensitive(self):
        """## Critic Approved must be exact heading format."""
        with tempfile.TemporaryDirectory() as tmpdir:
            spec_path = os.path.join(tmpdir, "spec.md")
            _write_spec(
                spec_path,
                "# Spec\n\n### t-1: Task\nDONE\n\nCritic Approved\n",
            )
            result = parse_critic_result(spec_path)
            # Without the ## heading prefix, it should not be detected
            self.assertFalse(result["approved"])

    def test_parse_approved_at_start_of_line(self):
        """## Critic Approved must start at beginning of line."""
        with tempfile.TemporaryDirectory() as tmpdir:
            spec_path = os.path.join(tmpdir, "spec.md")
            _write_spec(
                spec_path,
                "# Spec\n\n### t-1: Task\nDONE\n\n  ## Critic Approved\n",
            )
            result = parse_critic_result(spec_path)
            # Indented heading should not match
            self.assertFalse(result["approved"])

    def test_process_critic_completion_approved(self):
        """process_critic_completion marks spec completed on approval."""
        with tempfile.TemporaryDirectory() as tmpdir:
            env = _make_temp_env(tmpdir)
            spec_path = os.path.join(tmpdir, "spec.md")
            _write_spec(
                spec_path,
                "# Spec\n\n### t-1: Task\nDONE\n\n## Critic Approved\n\n2026-03-06\n",
            )
            _write_queue_entry(
                env["queue_dir"],
                "q-001",
                spec_path=spec_path,
                critic_passes=0,
                status="running",
            )

            from lib.daemon_ops import process_critic_completion

            with patch("lib.daemon_ops.run_completion_hooks"):
                result = process_critic_completion(
                    env["queue_dir"],
                    "q-001",
                    env["events_dir"],
                    env["hooks_dir"],
                    spec_path,
                )

            self.assertEqual(result["outcome"], "critic_approved")

            # Verify status is completed
            entry_path = os.path.join(env["queue_dir"], "q-001.json")
            entry = json.loads(Path(entry_path).read_text())
            self.assertEqual(entry["status"], "completed")

    def test_process_critic_completion_no_output(self):
        """No approval and no tasks = treat as approved (avoid infinite loop)."""
        with tempfile.TemporaryDirectory() as tmpdir:
            env = _make_temp_env(tmpdir)
            spec_path = os.path.join(tmpdir, "spec.md")
            _write_spec(
                spec_path,
                "# Spec\n\n### t-1: Task\nDONE\n",
            )
            _write_queue_entry(
                env["queue_dir"],
                "q-001",
                spec_path=spec_path,
                critic_passes=0,
                status="running",
            )

            from lib.daemon_ops import process_critic_completion

            with patch("lib.daemon_ops.run_completion_hooks"):
                result = process_critic_completion(
                    env["queue_dir"],
                    "q-001",
                    env["events_dir"],
                    env["hooks_dir"],
                    spec_path,
                )

            # Treated as approved to prevent infinite loops
            self.assertEqual(result["outcome"], "critic_approved")

    def test_process_critic_completion_missing_entry(self):
        """Missing queue entry returns error."""
        with tempfile.TemporaryDirectory() as tmpdir:
            env = _make_temp_env(tmpdir)

            from lib.daemon_ops import process_critic_completion

            result = process_critic_completion(
                env["queue_dir"],
                "q-nonexistent",
                env["events_dir"],
                env["hooks_dir"],
                "/fake/spec.md",
            )
            self.assertEqual(result["outcome"], "error")


class TestCriticModularity(unittest.TestCase):
    """Custom checks loaded, custom checks override defaults, custom prompt used."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.env = _make_temp_env(self.tmpdir)

    def tearDown(self):
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_custom_checks_appended(self):
        """Custom checks appear alongside default checks."""
        Path(os.path.join(self.env["custom_dir"], "perf-check.md")).write_text(
            "# Performance Check\n- [ ] Check latency\n- [ ] Check memory\n"
        )
        config = load_critic_config(self.env["state_dir"])
        checks = get_active_checks(
            config, self.env["checks_dir"], self.env["state_dir"]
        )
        names = [c["name"] for c in checks]
        # All 5 defaults + 1 custom
        self.assertIn("spec-integrity", names)
        self.assertIn("perf-check", names)
        default_count = sum(1 for c in checks if c["source"] == "default")
        custom_count = sum(1 for c in checks if c["source"] == "custom")
        self.assertEqual(default_count, 5)
        self.assertEqual(custom_count, 1)

    def test_custom_check_overrides_default(self):
        """Custom check with same name replaces default."""
        Path(os.path.join(self.env["custom_dir"], "spec-integrity.md")).write_text(
            "# Custom Spec Integrity\nMy own spec integrity rules.\n"
        )
        config = load_critic_config(self.env["state_dir"])
        checks = get_active_checks(
            config, self.env["checks_dir"], self.env["state_dir"]
        )
        si = [c for c in checks if c["name"] == "spec-integrity"]
        self.assertEqual(len(si), 1)
        self.assertEqual(si[0]["source"], "custom")
        self.assertIn("My own spec integrity rules", si[0]["content"])

    def test_custom_prompt_used_in_generation(self):
        """Custom prompt.md is used when generating critic prompt."""
        Path(os.path.join(self.env["critic_dir"], "prompt.md")).write_text(
            "CUSTOM CRITIC: {{SPEC_CONTENT}} // {{CHECKS}} // {{QUEUE_ID}}\n"
        )
        spec_path = os.path.join(self.tmpdir, "spec.md")
        _write_spec(spec_path, "# Hello\n")

        prompt = generate_critic_prompt(
            spec_path=spec_path,
            queue_id="q-099",
            iteration=1,
            config=DEFAULT_CONFIG,
            boi_dir=self.env["boi_dir"],
            state_dir=self.env["state_dir"],
        )
        self.assertIn("CUSTOM CRITIC:", prompt)
        self.assertIn("Hello", prompt)
        self.assertIn("q-099", prompt)

    def test_custom_checks_sorted_alphabetically(self):
        """Custom checks are sorted by filename."""
        for name in ["zebra", "alpha", "middle"]:
            Path(os.path.join(self.env["custom_dir"], f"{name}.md")).write_text(
                f"# {name}\n- [ ] Check\n"
            )
        config = load_critic_config(self.env["state_dir"])
        checks = get_active_checks(
            config, self.env["checks_dir"], self.env["state_dir"]
        )
        custom_names = [c["name"] for c in checks if c["source"] == "custom"]
        self.assertEqual(custom_names, sorted(custom_names))

    def test_non_md_files_ignored_in_custom(self):
        """Non-.md files in custom/ are ignored."""
        Path(os.path.join(self.env["custom_dir"], "notes.txt")).write_text("ignore me")
        Path(os.path.join(self.env["custom_dir"], "script.py")).write_text("pass")
        Path(os.path.join(self.env["custom_dir"], "real-check.md")).write_text(
            "# Real\n- [ ] Check\n"
        )
        config = load_critic_config(self.env["state_dir"])
        checks = get_active_checks(
            config, self.env["checks_dir"], self.env["state_dir"]
        )
        custom_names = [c["name"] for c in checks if c["source"] == "custom"]
        self.assertEqual(custom_names, ["real-check"])

    def test_selective_checks_config(self):
        """Config with subset of checks only loads those checks."""
        config = dict(DEFAULT_CONFIG)
        config["checks"] = ["spec-integrity", "code-quality"]
        checks = get_active_checks(
            config, self.env["checks_dir"], self.env["state_dir"]
        )
        default_names = [c["name"] for c in checks if c["source"] == "default"]
        self.assertEqual(set(default_names), {"spec-integrity", "code-quality"})


class TestCriticIntegration(unittest.TestCase):
    """Full flow: spec completes -> critic runs -> tasks added -> critic again -> approved."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.env = _make_temp_env(self.tmpdir)

    def tearDown(self):
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_full_flow_critic_triggers_on_completion(self):
        """When all tasks are DONE, critic is triggered (not completed)."""
        spec_path = os.path.join(self.tmpdir, "spec.md")
        _write_spec(
            spec_path,
            "# Test Spec\n\n## Tasks\n\n"
            "### t-1: Do thing\nDONE\n\n"
            "**Spec:** Did it.\n**Verify:** true\n",
        )
        _write_queue_entry(
            self.env["queue_dir"],
            "q-001",
            spec_path=spec_path,
            iteration=1,
            critic_passes=0,
            tasks_done=1,
            tasks_total=1,
        )

        from lib.daemon_ops import process_worker_completion

        with (
            patch("lib.daemon_ops.run_completion_hooks"),
            patch("lib.daemon_ops.update_telemetry"),
            patch("lib.daemon_ops.get_tasks_added_from_telemetry", return_value=0),
        ):
            result = process_worker_completion(
                queue_dir=self.env["queue_dir"],
                queue_id="q-001",
                events_dir=self.env["events_dir"],
                log_dir=self.env["log_dir"],
                hooks_dir=self.env["hooks_dir"],
                script_dir=self.env["boi_dir"],
                exit_code="0",
            )

        self.assertEqual(result["outcome"], "critic_review")
        self.assertIn("critic_prompt_path", result)
        self.assertEqual(result["critic_pass"], 1)

    def test_full_flow_critic_disabled_completes_immediately(self):
        """With critic disabled, spec completes without critic review."""
        spec_path = os.path.join(self.tmpdir, "spec.md")
        _write_spec(
            spec_path,
            "# Test Spec\n\n## Tasks\n\n"
            "### t-1: Do thing\nDONE\n\n"
            "**Spec:** Did it.\n**Verify:** true\n",
        )
        _write_queue_entry(
            self.env["queue_dir"],
            "q-001",
            spec_path=spec_path,
            iteration=1,
            critic_passes=0,
        )

        # Disable critic
        config = dict(DEFAULT_CONFIG)
        config["enabled"] = False
        save_critic_config(self.env["state_dir"], config)

        from lib.daemon_ops import process_worker_completion

        with (
            patch("lib.daemon_ops.run_completion_hooks"),
            patch("lib.daemon_ops.update_telemetry"),
            patch("lib.daemon_ops.get_tasks_added_from_telemetry", return_value=0),
        ):
            result = process_worker_completion(
                queue_dir=self.env["queue_dir"],
                queue_id="q-001",
                events_dir=self.env["events_dir"],
                log_dir=self.env["log_dir"],
                hooks_dir=self.env["hooks_dir"],
                script_dir=self.env["boi_dir"],
                exit_code="0",
            )

        self.assertEqual(result["outcome"], "completed")

    def test_full_flow_max_passes_completes(self):
        """After max passes, spec completes without more critic runs."""
        spec_path = os.path.join(self.tmpdir, "spec.md")
        _write_spec(
            spec_path,
            "# Test Spec\n\n## Tasks\n\n"
            "### t-1: Do thing\nDONE\n\n"
            "**Spec:** Did it.\n**Verify:** true\n",
        )
        _write_queue_entry(
            self.env["queue_dir"],
            "q-001",
            spec_path=spec_path,
            iteration=3,
            critic_passes=2,  # At max (DEFAULT_CONFIG max_passes=2)
        )

        from lib.daemon_ops import process_worker_completion

        with (
            patch("lib.daemon_ops.run_completion_hooks"),
            patch("lib.daemon_ops.update_telemetry"),
            patch("lib.daemon_ops.get_tasks_added_from_telemetry", return_value=0),
        ):
            result = process_worker_completion(
                queue_dir=self.env["queue_dir"],
                queue_id="q-001",
                events_dir=self.env["events_dir"],
                log_dir=self.env["log_dir"],
                hooks_dir=self.env["hooks_dir"],
                script_dir=self.env["boi_dir"],
                exit_code="0",
            )

        self.assertEqual(result["outcome"], "completed")

    def test_full_flow_critic_approval_then_completion(self):
        """Critic approves -> process_critic_completion -> completed."""
        spec_path = os.path.join(self.tmpdir, "spec.md")
        _write_spec(
            spec_path,
            "# Test Spec\n\n"
            "## Tasks\n\n"
            "### t-1: Do thing\nDONE\n\n"
            "## Critic Approved\n\n2026-03-06\n",
        )
        _write_queue_entry(
            self.env["queue_dir"],
            "q-001",
            spec_path=spec_path,
            critic_passes=0,
            status="running",
        )

        from lib.daemon_ops import process_critic_completion

        with patch("lib.daemon_ops.run_completion_hooks"):
            result = process_critic_completion(
                self.env["queue_dir"],
                "q-001",
                self.env["events_dir"],
                self.env["hooks_dir"],
                spec_path,
            )

        self.assertEqual(result["outcome"], "critic_approved")

        entry = json.loads(
            Path(os.path.join(self.env["queue_dir"], "q-001.json")).read_text()
        )
        self.assertEqual(entry["status"], "completed")
        self.assertEqual(entry["critic_passes"], 1)

    def test_full_flow_critic_adds_tasks_then_requeued(self):
        """Critic adds tasks -> requeued -> workers handle -> critic again."""
        spec_path = os.path.join(self.tmpdir, "spec.md")

        # Step 1: Critic adds tasks
        _write_spec(
            spec_path,
            "# Test Spec\n\n"
            "## Tasks\n\n"
            "### t-1: Original\nDONE\n\n"
            "**Spec:** Did it.\n\n**Verify:** true\n\n"
            "### t-2: [CRITIC] Fix error handling\nPENDING\n\n"
            "**Spec:** Fix it.\n\n**Verify:** Test.\n",
        )
        _write_queue_entry(
            self.env["queue_dir"],
            "q-001",
            spec_path=spec_path,
            critic_passes=0,
            status="running",
        )

        from lib.daemon_ops import process_critic_completion

        with patch("lib.daemon_ops.run_completion_hooks"):
            result = process_critic_completion(
                self.env["queue_dir"],
                "q-001",
                self.env["events_dir"],
                self.env["hooks_dir"],
                spec_path,
            )

        self.assertEqual(result["outcome"], "critic_tasks_added")
        entry = json.loads(
            Path(os.path.join(self.env["queue_dir"], "q-001.json")).read_text()
        )
        self.assertEqual(entry["status"], "requeued")
        self.assertEqual(entry["critic_passes"], 1)

        # Step 2: Worker fixes the task (simulate by marking DONE)
        _write_spec(
            spec_path,
            "# Test Spec\n\n"
            "## Tasks\n\n"
            "### t-1: Original\nDONE\n\n"
            "**Spec:** Did it.\n\n**Verify:** true\n\n"
            "### t-2: [CRITIC] Fix error handling\nDONE\n\n"
            "**Spec:** Fix it.\n\n**Verify:** Test.\n",
        )

        # Update entry for next worker completion
        entry["iteration"] = 2
        entry["status"] = "running"
        Path(os.path.join(self.env["queue_dir"], "q-001.json")).write_text(
            json.dumps(entry, indent=2) + "\n"
        )

        # Step 3: Worker completes, critic triggers again (pass 2)
        from lib.daemon_ops import process_worker_completion

        with (
            patch("lib.daemon_ops.run_completion_hooks"),
            patch("lib.daemon_ops.update_telemetry"),
            patch("lib.daemon_ops.get_tasks_added_from_telemetry", return_value=0),
        ):
            result2 = process_worker_completion(
                queue_dir=self.env["queue_dir"],
                queue_id="q-001",
                events_dir=self.env["events_dir"],
                log_dir=self.env["log_dir"],
                hooks_dir=self.env["hooks_dir"],
                script_dir=self.env["boi_dir"],
                exit_code="0",
            )

        # Should trigger critic again (pass 2, under max of 2)
        self.assertEqual(result2["outcome"], "critic_review")
        self.assertEqual(result2["critic_pass"], 2)

    def test_events_written_for_critic_flow(self):
        """Critic flow writes events to events_dir."""
        spec_path = os.path.join(self.tmpdir, "spec.md")
        _write_spec(
            spec_path,
            "# Test\n\n### t-1: Task\nDONE\n\n## Critic Approved\n\n",
        )
        _write_queue_entry(
            self.env["queue_dir"],
            "q-001",
            spec_path=spec_path,
            critic_passes=0,
            status="running",
        )

        from lib.daemon_ops import process_critic_completion

        with patch("lib.daemon_ops.run_completion_hooks"):
            process_critic_completion(
                self.env["queue_dir"],
                "q-001",
                self.env["events_dir"],
                self.env["hooks_dir"],
                spec_path,
            )

        # Check that at least one event file was written
        event_files = [
            f for f in os.listdir(self.env["events_dir"]) if f.endswith(".json")
        ]
        self.assertGreater(len(event_files), 0)

        # Verify event content
        found_critic_event = False
        for ef in event_files:
            data = json.loads(
                Path(os.path.join(self.env["events_dir"], ef)).read_text()
            )
            if data.get("type") == "critic_approved":
                found_critic_event = True
                self.assertEqual(data["queue_id"], "q-001")
        self.assertTrue(found_critic_event)


if __name__ == "__main__":
    unittest.main()
