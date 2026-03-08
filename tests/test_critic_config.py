# test_critic_config.py — Tests for critic configuration system.

import json
import os
import tempfile
import unittest
from pathlib import Path

from lib.critic import run_critic
from lib.critic_config import (
    DEFAULT_CONFIG,
    ensure_critic_dirs,
    get_active_checks,
    get_critic_prompt,
    is_critic_enabled,
    load_critic_config,
    save_critic_config,
)


class TestLoadCriticConfig(unittest.TestCase):
    """Tests for load_critic_config()."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_creates_defaults_when_missing(self):
        config = load_critic_config(self.tmpdir)
        self.assertEqual(config, DEFAULT_CONFIG)
        # Config file should now exist
        config_path = os.path.join(self.tmpdir, "critic", "config.json")
        self.assertTrue(os.path.isfile(config_path))

    def test_creates_critic_directory(self):
        load_critic_config(self.tmpdir)
        self.assertTrue(os.path.isdir(os.path.join(self.tmpdir, "critic")))

    def test_creates_custom_directory(self):
        load_critic_config(self.tmpdir)
        self.assertTrue(os.path.isdir(os.path.join(self.tmpdir, "critic", "custom")))

    def test_loads_existing_config(self):
        critic_dir = os.path.join(self.tmpdir, "critic")
        os.makedirs(critic_dir, exist_ok=True)
        custom_config = {"enabled": False, "max_passes": 5}
        config_path = os.path.join(critic_dir, "config.json")
        with open(config_path, "w") as f:
            json.dump(custom_config, f)

        config = load_critic_config(self.tmpdir)
        self.assertFalse(config["enabled"])
        self.assertEqual(config["max_passes"], 5)
        # Should still have defaults for missing keys
        self.assertEqual(config["trigger"], "on_complete")
        self.assertEqual(config["timeout_seconds"], 600)

    def test_handles_corrupt_config(self):
        critic_dir = os.path.join(self.tmpdir, "critic")
        os.makedirs(critic_dir, exist_ok=True)
        config_path = os.path.join(critic_dir, "config.json")
        with open(config_path, "w") as f:
            f.write("not valid json{{{")

        config = load_critic_config(self.tmpdir)
        self.assertEqual(config, DEFAULT_CONFIG)

    def test_default_config_has_all_checks(self):
        config = load_critic_config(self.tmpdir)
        expected_checks = [
            "spec-integrity",
            "verify-commands",
            "code-quality",
            "completeness",
            "fleet-readiness",
        ]
        self.assertEqual(config["checks"], expected_checks)


class TestIsCriticEnabled(unittest.TestCase):
    """Tests for is_critic_enabled()."""

    def test_enabled_by_default(self):
        self.assertTrue(is_critic_enabled(DEFAULT_CONFIG))

    def test_enabled_true(self):
        self.assertTrue(is_critic_enabled({"enabled": True}))

    def test_disabled(self):
        self.assertFalse(is_critic_enabled({"enabled": False}))

    def test_missing_key_defaults_true(self):
        self.assertTrue(is_critic_enabled({}))


class TestSaveCriticConfig(unittest.TestCase):
    """Tests for save_critic_config()."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_save_and_reload(self):
        config = dict(DEFAULT_CONFIG)
        config["enabled"] = False
        config["max_passes"] = 10

        save_critic_config(self.tmpdir, config)
        loaded = load_critic_config(self.tmpdir)
        self.assertFalse(loaded["enabled"])
        self.assertEqual(loaded["max_passes"], 10)


class TestGetActiveChecks(unittest.TestCase):
    """Tests for get_active_checks()."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.checks_dir = os.path.join(self.tmpdir, "default_checks")
        os.makedirs(self.checks_dir)
        self.state_dir = os.path.join(self.tmpdir, "state")
        os.makedirs(os.path.join(self.state_dir, "critic", "custom"), exist_ok=True)

        # Create some default check files
        for name in ["spec-integrity", "code-quality", "completeness"]:
            with open(os.path.join(self.checks_dir, f"{name}.md"), "w") as f:
                f.write(f"# {name}\nDefault check content for {name}.\n")

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_loads_default_checks(self):
        config = {
            "checks": ["spec-integrity", "code-quality", "completeness"],
            "custom_checks_dir": "custom",
        }
        checks = get_active_checks(config, self.checks_dir, self.state_dir)
        self.assertEqual(len(checks), 3)
        self.assertTrue(all(c["source"] == "default" for c in checks))

    def test_skips_missing_default_checks(self):
        config = {
            "checks": ["spec-integrity", "nonexistent-check"],
            "custom_checks_dir": "custom",
        }
        checks = get_active_checks(config, self.checks_dir, self.state_dir)
        names = [c["name"] for c in checks]
        self.assertIn("spec-integrity", names)
        self.assertNotIn("nonexistent-check", names)

    def test_loads_custom_checks(self):
        custom_dir = os.path.join(self.state_dir, "critic", "custom")
        with open(os.path.join(custom_dir, "security-review.md"), "w") as f:
            f.write("# Security Review\nCustom security check.\n")

        config = {
            "checks": ["spec-integrity"],
            "custom_checks_dir": "custom",
        }
        checks = get_active_checks(config, self.checks_dir, self.state_dir)
        names = [c["name"] for c in checks]
        self.assertIn("security-review", names)
        custom_checks = [c for c in checks if c["source"] == "custom"]
        self.assertEqual(len(custom_checks), 1)

    def test_custom_overrides_default(self):
        custom_dir = os.path.join(self.state_dir, "critic", "custom")
        with open(os.path.join(custom_dir, "spec-integrity.md"), "w") as f:
            f.write("# Custom Spec Integrity\nOverridden check.\n")

        config = {
            "checks": ["spec-integrity", "code-quality"],
            "custom_checks_dir": "custom",
        }
        checks = get_active_checks(config, self.checks_dir, self.state_dir)
        si_checks = [c for c in checks if c["name"] == "spec-integrity"]
        self.assertEqual(len(si_checks), 1)
        self.assertEqual(si_checks[0]["source"], "custom")
        self.assertIn("Overridden", si_checks[0]["content"])

    def test_no_custom_dir(self):
        import shutil

        custom_dir = os.path.join(self.state_dir, "critic", "custom")
        shutil.rmtree(custom_dir, ignore_errors=True)

        config = {
            "checks": ["spec-integrity"],
            "custom_checks_dir": "custom",
        }
        checks = get_active_checks(config, self.checks_dir, self.state_dir)
        self.assertEqual(len(checks), 1)
        self.assertEqual(checks[0]["name"], "spec-integrity")

    def test_check_content_loaded(self):
        config = {
            "checks": ["spec-integrity"],
            "custom_checks_dir": "custom",
        }
        checks = get_active_checks(config, self.checks_dir, self.state_dir)
        self.assertIn("spec-integrity", checks[0]["content"])


class TestGetCriticPrompt(unittest.TestCase):
    """Tests for get_critic_prompt()."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.state_dir = os.path.join(self.tmpdir, "state")
        self.boi_dir = os.path.join(self.tmpdir, "boi")
        os.makedirs(os.path.join(self.state_dir, "critic"), exist_ok=True)
        os.makedirs(os.path.join(self.boi_dir, "templates"), exist_ok=True)

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_loads_default_prompt(self):
        default_path = os.path.join(self.boi_dir, "templates", "critic-prompt.md")
        with open(default_path, "w") as f:
            f.write("# Default Critic Prompt\n{{SPEC_CONTENT}}\n")

        prompt = get_critic_prompt(self.state_dir, self.boi_dir)
        self.assertIn("Default Critic Prompt", prompt)
        self.assertIn("{{SPEC_CONTENT}}", prompt)

    def test_user_override_takes_precedence(self):
        # Create both default and user override
        default_path = os.path.join(self.boi_dir, "templates", "critic-prompt.md")
        with open(default_path, "w") as f:
            f.write("# Default Prompt\n")

        user_path = os.path.join(self.state_dir, "critic", "prompt.md")
        with open(user_path, "w") as f:
            f.write("# Custom User Prompt\n")

        prompt = get_critic_prompt(self.state_dir, self.boi_dir)
        self.assertIn("Custom User Prompt", prompt)
        self.assertNotIn("Default Prompt", prompt)

    def test_raises_when_no_prompt(self):
        with self.assertRaises(FileNotFoundError):
            get_critic_prompt(self.state_dir, self.boi_dir)


class TestEnsureCriticDirs(unittest.TestCase):
    """Tests for ensure_critic_dirs()."""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_creates_all_directories(self):
        ensure_critic_dirs(self.tmpdir)
        self.assertTrue(os.path.isdir(os.path.join(self.tmpdir, "critic")))
        self.assertTrue(os.path.isdir(os.path.join(self.tmpdir, "critic", "custom")))

    def test_creates_default_config(self):
        ensure_critic_dirs(self.tmpdir)
        config_path = os.path.join(self.tmpdir, "critic", "config.json")
        self.assertTrue(os.path.isfile(config_path))
        with open(config_path, "r") as f:
            config = json.load(f)
        self.assertTrue(config["enabled"])

    def test_does_not_overwrite_existing_config(self):
        critic_dir = os.path.join(self.tmpdir, "critic")
        os.makedirs(critic_dir, exist_ok=True)
        config_path = os.path.join(critic_dir, "config.json")
        with open(config_path, "w") as f:
            json.dump({"enabled": False}, f)

        ensure_critic_dirs(self.tmpdir)
        with open(config_path, "r") as f:
            config = json.load(f)
        self.assertFalse(config["enabled"])


class TestRunCritic(unittest.TestCase):
    """Tests for run_critic()."""

    def test_generates_prompt_file(self):
        """run_critic generates a critic prompt file in the queue dir."""
        with tempfile.TemporaryDirectory() as tmpdir:
            # Set up dirs
            state_dir = os.path.join(tmpdir, "state")
            queue_dir = os.path.join(state_dir, "queue")
            boi_dir = os.path.join(tmpdir, "boi")
            checks_dir = os.path.join(boi_dir, "templates", "checks")
            critic_dir = os.path.join(state_dir, "critic")

            os.makedirs(queue_dir)
            os.makedirs(checks_dir)
            os.makedirs(os.path.join(critic_dir, "custom"))

            # Write critic prompt template
            Path(os.path.join(boi_dir, "templates", "critic-prompt.md")).write_text(
                "{{SPEC_CONTENT}}\n{{CHECKS}}\n{{QUEUE_ID}}\n{{ITERATION}}\n"
            )

            # Write config
            Path(os.path.join(critic_dir, "config.json")).write_text(
                json.dumps(
                    {"enabled": True, "checks": [], "custom_checks_dir": "custom"}
                )
            )

            # Write spec
            spec_path = os.path.join(tmpdir, "spec.md")
            Path(spec_path).write_text("# Test\n## Tasks\n### t-1: Task\nDONE\n")

            # Write queue entry
            entry = {"id": "q-001", "critic_passes": 0}
            entry_path = os.path.join(queue_dir, "q-001.json")
            Path(entry_path).write_text(json.dumps(entry))

            os.environ["BOI_SCRIPT_DIR"] = boi_dir
            try:
                result = run_critic(spec_path, queue_dir, "q-001", DEFAULT_CONFIG)
                self.assertIn("prompt_path", result)
                self.assertTrue(os.path.isfile(result["prompt_path"]))
            finally:
                os.environ.pop("BOI_SCRIPT_DIR", None)

    def test_return_type(self):
        """run_critic returns a dict with expected keys."""
        with tempfile.TemporaryDirectory() as tmpdir:
            state_dir = os.path.join(tmpdir, "state")
            queue_dir = os.path.join(state_dir, "queue")
            boi_dir = os.path.join(tmpdir, "boi")
            checks_dir = os.path.join(boi_dir, "templates", "checks")
            critic_dir = os.path.join(state_dir, "critic")

            os.makedirs(queue_dir)
            os.makedirs(checks_dir)
            os.makedirs(os.path.join(critic_dir, "custom"))

            Path(os.path.join(boi_dir, "templates", "critic-prompt.md")).write_text(
                "{{SPEC_CONTENT}}\n{{CHECKS}}\n{{QUEUE_ID}}\n{{ITERATION}}\n"
            )
            Path(os.path.join(critic_dir, "config.json")).write_text(
                json.dumps(
                    {"enabled": True, "checks": [], "custom_checks_dir": "custom"}
                )
            )

            spec_path = os.path.join(tmpdir, "spec.md")
            Path(spec_path).write_text("# Test\n## Tasks\n### t-1: Task\nDONE\n")

            entry = {"id": "q-001", "critic_passes": 0}
            Path(os.path.join(queue_dir, "q-001.json")).write_text(json.dumps(entry))

            os.environ["BOI_SCRIPT_DIR"] = boi_dir
            try:
                result = run_critic(spec_path, queue_dir, "q-001", {})
                self.assertIsInstance(result, dict)
                self.assertIn("approved", result)
                self.assertIn("issues", result)
                self.assertIn("prompt_path", result)
            finally:
                os.environ.pop("BOI_SCRIPT_DIR", None)


if __name__ == "__main__":
    unittest.main()
