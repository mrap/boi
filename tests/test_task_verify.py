# test_task_verify.py -- Tests for task-verify phase (renamed from critic).
#
# This file merges the stale-reference guard with the existing critic test suite.
# Imports from lib.critic and lib.critic_config remain valid; the file rename
# is purely cosmetic at this stage. Fixture paths use "task-verify" to match
# the updated state-directory layout.

import json
import os
import shutil
import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

from lib.critic import (
    generate_auto_reject_task,
    generate_critic_prompt,
    parse_critic_result,
    run_critic,
    should_run_critic,
)
from lib.daemon_ops import CompletionContext
from lib.db import Database
from lib.critic_config import (
    DEFAULT_CONFIG,
    ensure_critic_dirs,
    get_active_checks,
    get_critic_prompt,
    is_critic_enabled,
    load_critic_config,
    save_critic_config,
)


# ---------------------------------------------------------------------------
# Stale-reference guard
# ---------------------------------------------------------------------------

_BOI_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

_SCAN_DIRS = [
    ("phases", [".toml"]),
    ("templates", [".md"]),
    ("lib", [".py"]),
]
_SKIP_BASENAMES = {"CHANGELOG.md", "changelog.md"}


def _find_critic_string_refs() -> list[str]:
    """Return list of 'file:line' entries where the literal string "critic"
    (with surrounding double-quotes) appears outside of excluded files."""
    findings: list[str] = []

    for subdir, exts in _SCAN_DIRS:
        scan_dir = os.path.join(_BOI_DIR, subdir)
        if not os.path.isdir(scan_dir):
            continue
        for root, dirs, files in os.walk(scan_dir):
            dirs[:] = [d for d in dirs if d != ".git"]
            for fname in files:
                if not any(fname.endswith(ext) for ext in exts):
                    continue
                if fname in _SKIP_BASENAMES:
                    continue
                fpath = os.path.join(root, fname)
                try:
                    with open(fpath, encoding="utf-8", errors="ignore") as fh:
                        for lineno, line in enumerate(fh, 1):
                            if '"critic"' in line:
                                findings.append(f"{fpath}:{lineno}")
                except OSError:
                    continue

    # Also check worker.py directly
    worker_path = os.path.join(_BOI_DIR, "worker.py")
    if os.path.isfile(worker_path):
        with open(worker_path, encoding="utf-8", errors="ignore") as fh:
            for lineno, line in enumerate(fh, 1):
                if '"critic"' in line:
                    findings.append(f"{worker_path}:{lineno}")

    return findings


class TestNoStaleCriticRefs(unittest.TestCase):
    """Guard: after the rename, no "critic" string literals should remain
    in phases/, templates/, lib/, or worker.py."""

    def test_no_critic_string_literals(self):
        findings = _find_critic_string_refs()
        self.assertEqual(
            findings,
            [],
            msg=(
                'Stale "critic" string literal(s) found after rename to task-verify:\n'
                + "\n".join(findings)
            ),
        )


# ---------------------------------------------------------------------------
# Helpers (ported from test_critic.py with task-verify paths)
# ---------------------------------------------------------------------------


def _make_temp_env(tmpdir):
    """Create a standard temp environment for task-verify tests."""
    state_dir = os.path.join(tmpdir, "state")
    queue_dir = os.path.join(state_dir, "queue")
    events_dir = os.path.join(state_dir, "events")
    hooks_dir = os.path.join(state_dir, "hooks")
    log_dir = os.path.join(state_dir, "logs")
    boi_dir = os.path.join(tmpdir, "boi")
    checks_dir = os.path.join(boi_dir, "templates", "checks")
    task_verify_dir = os.path.join(state_dir, "task-verify")
    custom_dir = os.path.join(task_verify_dir, "custom")

    for d in [queue_dir, events_dir, hooks_dir, log_dir, checks_dir, custom_dir]:
        os.makedirs(d, exist_ok=True)

    # Write default task-verify prompt template
    Path(os.path.join(boi_dir, "templates", "task-verify-prompt.md")).write_text(
        "# Task Verification Pass {{ITERATION}}\n"
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
    Path(os.path.join(task_verify_dir, "config.json")).write_text(
        json.dumps(DEFAULT_CONFIG, indent=2) + "\n"
    )

    # Create SQLite database
    db_path = os.path.join(state_dir, "boi.db")
    db = Database(db_path, queue_dir)

    return {
        "state_dir": state_dir,
        "queue_dir": queue_dir,
        "events_dir": events_dir,
        "hooks_dir": hooks_dir,
        "log_dir": log_dir,
        "boi_dir": boi_dir,
        "checks_dir": checks_dir,
        "task_verify_dir": task_verify_dir,
        "custom_dir": custom_dir,
        "db": db,
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


def _make_db_entry(db, queue_id, spec_path, **overrides):
    """Create a spec entry in the Database (DB-backed tests)."""
    entry = db.enqueue(spec_path)
    new_id = queue_id
    db.conn.execute("UPDATE specs SET id=? WHERE id=?", (new_id, entry["id"]))
    db.conn.commit()

    fields_to_update = {
        "status": overrides.get("status", "running"),
        "iteration": overrides.get("iteration", 1),
        "max_iterations": overrides.get("max_iterations", 30),
        "critic_passes": overrides.get("critic_passes", 0),
        "tasks_done": overrides.get("tasks_done", 0),
        "tasks_total": overrides.get("tasks_total", 0),
        "consecutive_failures": overrides.get("consecutive_failures", 0),
        "phase": overrides.get("phase", "task-verify"),
    }

    set_clauses = ", ".join(f"{k}=?" for k in fields_to_update)
    db.conn.execute(
        f"UPDATE specs SET {set_clauses} WHERE id=?",
        list(fields_to_update.values()) + [new_id],
    )
    db.conn.commit()
    return db.get_spec(new_id)


def _write_spec(queue_dir, queue_id, content):
    """Write a spec file."""
    path = os.path.join(queue_dir, f"{queue_id}.spec.md")
    Path(path).write_text(content)
    return path


# ---------------------------------------------------------------------------
# Tests (ported from test_critic.py)
# ---------------------------------------------------------------------------


class TestTaskVerifyConfig(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.env = _make_temp_env(self.tmpdir)

    def tearDown(self):
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_load_config_returns_defaults_when_no_file(self):
        fresh_state = os.path.join(self.tmpdir, "fresh_state")
        config = load_critic_config(fresh_state)
        self.assertTrue(config["enabled"])
        self.assertEqual(config["max_passes"], 2)

    def test_load_config_merges_with_defaults(self):
        state_dir = self.env["state_dir"]
        custom_config = {"enabled": False, "max_passes": 5}
        save_critic_config(state_dir, custom_config)
        config = load_critic_config(state_dir)
        self.assertFalse(config["enabled"])
        self.assertEqual(config["max_passes"], 5)
        # Defaults filled in for missing keys
        self.assertIn("checks", config)

    def test_is_critic_enabled_true_by_default(self):
        config = dict(DEFAULT_CONFIG)
        self.assertTrue(is_critic_enabled(config))

    def test_is_critic_enabled_false_when_disabled(self):
        config = dict(DEFAULT_CONFIG)
        config["enabled"] = False
        self.assertFalse(is_critic_enabled(config))

    def test_get_active_checks_returns_defaults(self):
        env = self.env
        checks = get_active_checks(
            DEFAULT_CONFIG, env["checks_dir"], env["state_dir"]
        )
        self.assertGreater(len(checks), 0)
        names = [c["name"] for c in checks]
        self.assertIn("spec-integrity", names)

    def test_ensure_critic_dirs_creates_structure(self):
        fresh_state = os.path.join(self.tmpdir, "ensure_test")
        ensure_critic_dirs(fresh_state)
        task_verify_dir = os.path.join(fresh_state, "task-verify")
        self.assertTrue(os.path.isdir(task_verify_dir))
        self.assertTrue(os.path.isdir(os.path.join(task_verify_dir, "custom")))
        self.assertTrue(
            os.path.isfile(os.path.join(task_verify_dir, "config.json"))
        )


class TestShouldRunCritic(unittest.TestCase):
    def setUp(self):
        self.config = dict(DEFAULT_CONFIG)

    def test_runs_when_enabled(self):
        entry = {"critic_passes": 0, "no_critic": False, "mode": "execute"}
        self.assertTrue(should_run_critic(entry, self.config))

    def test_skips_when_disabled(self):
        self.config["enabled"] = False
        entry = {"critic_passes": 0, "no_critic": False, "mode": "execute"}
        self.assertFalse(should_run_critic(entry, self.config))

    def test_skips_when_no_critic_flag(self):
        entry = {"critic_passes": 0, "no_critic": True, "mode": "execute"}
        self.assertFalse(should_run_critic(entry, self.config))

    def test_skips_when_max_passes_reached(self):
        self.config["max_passes"] = 2
        entry = {"critic_passes": 2, "no_critic": False, "mode": "execute"}
        self.assertFalse(should_run_critic(entry, self.config))

    def test_runs_when_passes_below_max(self):
        self.config["max_passes"] = 2
        entry = {"critic_passes": 1, "no_critic": False, "mode": "execute"}
        self.assertTrue(should_run_critic(entry, self.config))


class TestParseCriticResult(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()

    def tearDown(self):
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def _write_spec(self, content):
        path = os.path.join(self.tmpdir, "q-001.spec.md")
        Path(path).write_text(content)
        return path

    def test_detects_approval_signal(self):
        spec_path = self._write_spec(
            "# Spec\n\n### t-1: Task\nDONE\n\n## Task Verification Approved\n\nLooks good.\n"
        )
        result = parse_critic_result(spec_path)
        self.assertTrue(result["approved"])
        self.assertEqual(result["new_tasks"], [])

    def test_detects_reject_signal_new_task(self):
        spec_path = self._write_spec(
            "# Spec\n\n### t-1: Task\nDONE\n\n### t-2: [TASK-VERIFY] Fix something\nPENDING\n\n"
            "**Spec:** Fix the issue.\n\n**Verify:** echo done\n"
        )
        result = parse_critic_result(spec_path)
        self.assertFalse(result["approved"])
        self.assertGreater(len(result["new_tasks"]), 0)

    def test_no_signal_returns_not_approved(self):
        spec_path = self._write_spec("# Spec\n\n### t-1: Task\nDONE\n")
        result = parse_critic_result(spec_path)
        self.assertFalse(result["approved"])


class TestGenerateCriticPrompt(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()
        self.env = _make_temp_env(self.tmpdir)

    def tearDown(self):
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_generates_prompt_with_checks(self):
        env = self.env
        spec_path = os.path.join(env["queue_dir"], "q-001.spec.md")
        Path(spec_path).write_text("# Test Spec\n\n### t-1: Task\nDONE\n")

        entry = {"critic_passes": 0, "no_critic": False, "mode": "execute"}

        prompt = generate_critic_prompt(
            spec_path=spec_path,
            queue_id="q-001",
            iteration=1,
            config=DEFAULT_CONFIG,
            boi_dir=env["boi_dir"],
            state_dir=env["state_dir"],
            queue_entry=entry,
        )
        self.assertIsInstance(prompt, str)
        self.assertIn("q-001", prompt)

    def test_prompt_includes_spec_content(self):
        env = self.env
        spec_path = os.path.join(env["queue_dir"], "q-002.spec.md")
        Path(spec_path).write_text("# My Unique Spec 12345\n\n### t-1: Task\nDONE\n")

        entry = {"critic_passes": 0, "no_critic": False, "mode": "execute"}

        prompt = generate_critic_prompt(
            spec_path=spec_path,
            queue_id="q-002",
            iteration=1,
            config=DEFAULT_CONFIG,
            boi_dir=env["boi_dir"],
            state_dir=env["state_dir"],
            queue_entry=entry,
        )
        self.assertIn("My Unique Spec 12345", prompt)


class TestGenerateAutoRejectTask(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.mkdtemp()

    def tearDown(self):
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def test_appends_reject_task_to_spec(self):
        spec_path = os.path.join(self.tmpdir, "q-001.spec.md")
        Path(spec_path).write_text("# Spec\n\n### t-1: Task\nDONE\n")
        generate_auto_reject_task(spec_path, "Low quality score: 0.40")
        content = Path(spec_path).read_text()
        self.assertIn("[TASK-VERIFY]", content)
        self.assertIn("PENDING", content)

    def test_reject_task_mentions_quality(self):
        spec_path = os.path.join(self.tmpdir, "q-002.spec.md")
        Path(spec_path).write_text("# Spec\n\n### t-1: Task\nDONE\n")
        generate_auto_reject_task(spec_path, "score=0.30")
        content = Path(spec_path).read_text()
        self.assertIn("score=0.30", content)


if __name__ == "__main__":
    unittest.main()
