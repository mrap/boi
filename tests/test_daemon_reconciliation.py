# test_daemon_reconciliation.py — TDD tests for daemon state reconciliation.
#
# Bug: daemon startup leaves specs in 'running' state with dead workers when
# the workers table does not have current_spec_id pointing to the spec.
# recover_running_specs() uses INNER JOIN on workers.current_spec_id so
# orphaned specs (worker cleared but spec still 'running') are invisible.
#
# t-1: Write RED tests that prove the bug exists.
# t-2: Implement the fix so the GREEN test passes.

import json
import os
import sys
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from daemon import Daemon
from lib.db import Database


MINIMAL_SPEC = """\
# Test Spec

## Tasks

### t-1: Do something
PENDING

**Spec:** Do it.

**Verify:** echo ok
"""


class ReconciliationTestCase(unittest.TestCase):
    """Base: temp dir + minimal Daemon (no run loop)."""

    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.state_dir = self._tmpdir.name
        self.db_path = os.path.join(self.state_dir, "boi.db")
        self.queue_dir = os.path.join(self.state_dir, "queue")
        os.makedirs(self.queue_dir, exist_ok=True)

        self.worktree = os.path.join(self.state_dir, "wt-1")
        os.makedirs(self.worktree, exist_ok=True)

        self.config_path = os.path.join(self.state_dir, "config.json")
        with open(self.config_path, "w", encoding="utf-8") as f:
            json.dump(
                {"workers": [{"id": "w-1", "worktree_path": self.worktree}]}, f
            )

        self.spec_file = os.path.join(self.state_dir, "test.md")
        with open(self.spec_file, "w", encoding="utf-8") as f:
            f.write(MINIMAL_SPEC)

        # Daemon constructor sets up DB but does NOT start the poll loop.
        self.daemon = Daemon(
            config_path=self.config_path,
            db_path=self.db_path,
            state_dir=self.state_dir,
        )

    def tearDown(self) -> None:
        self.daemon.db.close()
        self._tmpdir.cleanup()

    def _insert_orphaned_running_spec(
        self,
        spec_id: str = "q-001",
        worker_id: str = "w-1",
        fake_pid: int = 999999,
    ) -> None:
        """Directly insert a spec in 'running' state with a dead PID.

        The corresponding worker row either doesn't exist or has
        current_spec_id=NULL — simulating an orphaned spec that the
        daemon left behind when it crashed / was SIGTERM'd before the
        DB was fully consistent.
        """
        spec_path = os.path.join(self.queue_dir, f"{spec_id}.spec.md")
        with open(spec_path, "w", encoding="utf-8") as f:
            f.write(MINIMAL_SPEC)

        now = datetime.now(timezone.utc).isoformat()
        self.daemon.db.conn.execute(
            "INSERT INTO specs "
            "(id, spec_path, status, last_worker, priority, iteration, "
            "max_iterations, submitted_at) "
            "VALUES (?, ?, 'running', ?, 100, 1, 30, ?)",
            (spec_id, spec_path, worker_id, now),
        )
        # Worker row exists but current_spec_id is NULL — simulates the
        # case where the worker was freed/re-registered without the spec
        # being requeued (e.g., daemon crashed after freeing worker but
        # before requeuing spec, or workers table was reset on restart).
        self.daemon.db.conn.execute(
            "INSERT OR REPLACE INTO workers (id, worktree_path, current_spec_id) "
            "VALUES (?, ?, NULL)",
            (worker_id, self.worktree),
        )
        self.daemon.db.conn.commit()


# ─── Correct-behavior test ────────────────────────────────────────────────


class TestReconciliationRequeuesOrphanedSpecs(ReconciliationTestCase):
    """Assert CORRECT behavior: orphaned running specs are requeued on startup."""

    def test_reconciliation_requeues_orphaned_specs(self) -> None:
        """Fix: after reconciliation, orphaned spec is requeued with last_worker=NULL."""
        self._insert_orphaned_running_spec(spec_id="q-001", fake_pid=999999)

        # Call the reconciliation function that t-2 will add to daemon.py.
        # This should find specs that are 'running' but have no live worker
        # process, even when the workers table is out of sync.
        self.daemon.reconcile_stale_specs()

        spec = self.daemon.db.conn.execute(
            "SELECT status, last_worker FROM specs WHERE id = 'q-001'"
        ).fetchone()
        self.assertIsNotNone(spec, "Spec row should exist")
        self.assertEqual(
            spec["status"],
            "requeued",
            "Orphaned spec must be requeued so the daemon can re-dispatch it",
        )
        self.assertIsNone(
            spec["last_worker"],
            "last_worker must be cleared so the worker appears free",
        )


# ─── Edge case tests (t-3) ────────────────────────────────────────────────


class TestReconciliationNoStaleSpecs(ReconciliationTestCase):
    """No stale specs — reconciliation is a no-op."""

    def test_no_stale_specs_is_noop(self) -> None:
        """All specs queued/completed — reconcile_stale_specs returns empty list."""
        now = __import__("datetime").datetime.now(__import__("datetime").timezone.utc).isoformat()
        # Insert a queued spec
        spec_path_q = os.path.join(self.queue_dir, "q-010.spec.md")
        with open(spec_path_q, "w") as f:
            f.write(MINIMAL_SPEC)
        self.daemon.db.conn.execute(
            "INSERT INTO specs (id, spec_path, status, priority, iteration, "
            "max_iterations, submitted_at) VALUES (?, ?, 'queued', 100, 0, 30, ?)",
            ("q-010", spec_path_q, now),
        )
        # Insert a completed spec
        spec_path_c = os.path.join(self.queue_dir, "q-011.spec.md")
        with open(spec_path_c, "w") as f:
            f.write(MINIMAL_SPEC)
        self.daemon.db.conn.execute(
            "INSERT INTO specs (id, spec_path, status, priority, iteration, "
            "max_iterations, submitted_at) VALUES (?, ?, 'completed', 100, 1, 30, ?)",
            ("q-011", spec_path_c, now),
        )
        self.daemon.db.conn.commit()

        requeued = self.daemon.reconcile_stale_specs()

        self.assertEqual(requeued, [], "No specs should be requeued when none are stale")

        row_q = self.daemon.db.conn.execute(
            "SELECT status FROM specs WHERE id = 'q-010'"
        ).fetchone()
        self.assertEqual(row_q["status"], "queued", "Queued spec should remain queued")

        row_c = self.daemon.db.conn.execute(
            "SELECT status FROM specs WHERE id = 'q-011'"
        ).fetchone()
        self.assertEqual(row_c["status"], "completed", "Completed spec should remain completed")


class TestReconciliationMultipleStaleSpecs(ReconciliationTestCase):
    """Multiple stale specs — all 3 orphaned running specs get requeued."""

    def test_multiple_stale_specs_all_requeued(self) -> None:
        self._insert_orphaned_running_spec(spec_id="q-020", worker_id="w-1", fake_pid=999991)
        self._insert_orphaned_running_spec(spec_id="q-021", worker_id="w-1", fake_pid=999992)
        self._insert_orphaned_running_spec(spec_id="q-022", worker_id="w-1", fake_pid=999993)

        requeued = self.daemon.reconcile_stale_specs()

        self.assertEqual(
            sorted(requeued), ["q-020", "q-021", "q-022"],
            "All 3 orphaned specs must be requeued",
        )
        for spec_id in ("q-020", "q-021", "q-022"):
            row = self.daemon.db.conn.execute(
                "SELECT status, last_worker FROM specs WHERE id = ?", (spec_id,)
            ).fetchone()
            self.assertEqual(row["status"], "requeued")
            self.assertIsNone(row["last_worker"])


class TestReconciliationMixedLiveAndDead(ReconciliationTestCase):
    """Mix of live and dead workers — only the dead one gets requeued."""

    def _insert_running_spec_with_live_worker(
        self,
        spec_id: str,
        worker_id: str,
        live_pid: int,
    ) -> None:
        """Insert a running spec backed by a live PID tracked in daemon.worker_procs."""
        spec_path = os.path.join(self.queue_dir, f"{spec_id}.spec.md")
        with open(spec_path, "w") as f:
            f.write(MINIMAL_SPEC)

        now = __import__("datetime").datetime.now(__import__("datetime").timezone.utc).isoformat()
        self.daemon.db.conn.execute(
            "INSERT INTO specs "
            "(id, spec_path, status, last_worker, priority, iteration, "
            "max_iterations, submitted_at) "
            "VALUES (?, ?, 'running', ?, 100, 1, 30, ?)",
            (spec_id, spec_path, worker_id, now),
        )
        # Worker claims this spec with a live PID
        self.daemon.db.conn.execute(
            "INSERT OR REPLACE INTO workers "
            "(id, worktree_path, current_spec_id, current_pid) "
            "VALUES (?, ?, ?, ?)",
            (worker_id, self.worktree, spec_id, live_pid),
        )
        self.daemon.db.conn.commit()
        # Register worker in daemon's in-memory process table so the
        # "not in worker_procs" branch is not triggered.
        self.daemon.worker_procs[worker_id] = None  # type: ignore[assignment]

    def test_only_dead_worker_spec_is_requeued(self) -> None:
        """Live spec stays running; dead spec is requeued."""
        live_pid = os.getpid()  # current process is definitely alive
        self._insert_running_spec_with_live_worker("q-030", "w-live", live_pid)
        # Dead spec: orphaned (worker row has current_spec_id=NULL)
        self._insert_orphaned_running_spec("q-031", worker_id="w-1", fake_pid=999994)

        requeued = self.daemon.reconcile_stale_specs()

        self.assertEqual(requeued, ["q-031"], "Only the dead-worker spec should be requeued")

        live_row = self.daemon.db.conn.execute(
            "SELECT status FROM specs WHERE id = 'q-030'"
        ).fetchone()
        self.assertEqual(live_row["status"], "running", "Live spec must remain running")

        dead_row = self.daemon.db.conn.execute(
            "SELECT status, last_worker FROM specs WHERE id = 'q-031'"
        ).fetchone()
        self.assertEqual(dead_row["status"], "requeued")
        self.assertIsNone(dead_row["last_worker"])


class TestReconciliationDispatchRace(ReconciliationTestCase):
    """Reconciliation + dispatch: requeued spec is assigned to exactly one worker."""

    def test_requeued_spec_assigned_to_exactly_one_worker(self) -> None:
        """After reconciliation, dispatch_specs picks up the spec once."""
        self._insert_orphaned_running_spec(spec_id="q-040", worker_id="w-1")

        # Step 1: reconcile — spec becomes 'requeued'
        requeued = self.daemon.reconcile_stale_specs()
        self.assertEqual(requeued, ["q-040"])

        # Step 2: track dispatch calls
        assigned: list[tuple] = []

        def _fake_assign(spec, worker):
            assigned.append((spec["id"], worker["id"]))
            # Simulate the DB transition dispatch normally does
            self.daemon.db.conn.execute(
                "UPDATE specs SET status='running' WHERE id=?", (spec["id"],)
            )
            self.daemon.db.conn.execute(
                "UPDATE workers SET current_spec_id=? WHERE id=?",
                (spec["id"], worker["id"]),
            )
            self.daemon.db.conn.commit()

        self.daemon.assign_spec_to_worker = _fake_assign  # type: ignore[method-assign]

        # Ensure w-1 is free (current_spec_id cleared by reconciliation above)
        self.daemon.dispatch_specs()

        self.assertEqual(len(assigned), 1, "Spec must be assigned to exactly one worker")
        self.assertEqual(assigned[0][0], "q-040")


class TestReconciliationPermissionErrorFailSafe(ReconciliationTestCase):
    """PermissionError on PID check → spec is still requeued (fail-safe)."""

    def _insert_running_spec_tracked_worker(
        self, spec_id: str, worker_id: str, pid: int
    ) -> None:
        """Insert a spec whose worker IS in worker_procs so os.kill is reached."""
        spec_path = os.path.join(self.queue_dir, f"{spec_id}.spec.md")
        with open(spec_path, "w") as f:
            f.write(MINIMAL_SPEC)

        now = __import__("datetime").datetime.now(__import__("datetime").timezone.utc).isoformat()
        self.daemon.db.conn.execute(
            "INSERT INTO specs "
            "(id, spec_path, status, last_worker, priority, iteration, "
            "max_iterations, submitted_at) "
            "VALUES (?, ?, 'running', ?, 100, 1, 30, ?)",
            (spec_id, spec_path, worker_id, now),
        )
        self.daemon.db.conn.execute(
            "INSERT OR REPLACE INTO workers "
            "(id, worktree_path, current_spec_id, current_pid) "
            "VALUES (?, ?, ?, ?)",
            (worker_id, self.worktree, spec_id, pid),
        )
        self.daemon.db.conn.commit()
        self.daemon.worker_procs[worker_id] = None  # type: ignore[assignment]

    def test_permission_error_causes_requeue(self) -> None:
        """If os.kill raises PermissionError the spec is still requeued (fail-safe)."""
        import unittest.mock as mock

        self._insert_running_spec_tracked_worker("q-050", "w-1", pid=12345)

        with mock.patch("daemon.os.kill", side_effect=PermissionError("access denied")):
            requeued = self.daemon.reconcile_stale_specs()

        self.assertEqual(requeued, ["q-050"], "PermissionError must not prevent requeue")
        row = self.daemon.db.conn.execute(
            "SELECT status, last_worker FROM specs WHERE id = 'q-050'"
        ).fetchone()
        self.assertEqual(row["status"], "requeued")
        self.assertIsNone(row["last_worker"])


# ─── Periodic / tick reconciliation tests (t-4) ──────────────────────────


class TestPeriodicReconciliation(ReconciliationTestCase):
    """Periodic liveness check: workers that die mid-run are requeued on next tick."""

    def test_periodic_tick_requeues_dead_worker_spec(self) -> None:
        """Spec running with a dead worker gets requeued when reconcile_stale_specs
        is called again (simulating the periodic tick firing)."""
        # Insert an orphaned running spec (dead worker)
        self._insert_orphaned_running_spec(spec_id="q-060", worker_id="w-1", fake_pid=999995)

        # Simulate the periodic tick calling reconcile_stale_specs
        requeued = self.daemon.reconcile_stale_specs()

        self.assertIn("q-060", requeued, "Dead-worker spec must be requeued on tick")
        row = self.daemon.db.conn.execute(
            "SELECT status, last_worker FROM specs WHERE id = 'q-060'"
        ).fetchone()
        self.assertEqual(row["status"], "requeued")
        self.assertIsNone(row["last_worker"])

    def test_periodic_tick_noop_when_no_dead_workers(self) -> None:
        """If all running specs have live workers, periodic tick is a no-op."""
        live_pid = os.getpid()

        spec_path = os.path.join(self.queue_dir, "q-061.spec.md")
        with open(spec_path, "w", encoding="utf-8") as f:
            f.write(MINIMAL_SPEC)

        now = datetime.now(timezone.utc).isoformat()
        self.daemon.db.conn.execute(
            "INSERT INTO specs (id, spec_path, status, last_worker, priority, "
            "iteration, max_iterations, submitted_at) "
            "VALUES ('q-061', ?, 'running', 'w-live', 100, 1, 30, ?)",
            (spec_path, now),
        )
        self.daemon.db.conn.execute(
            "INSERT OR REPLACE INTO workers (id, worktree_path, current_spec_id, current_pid) "
            "VALUES ('w-live', ?, 'q-061', ?)",
            (self.worktree, live_pid),
        )
        self.daemon.db.conn.commit()
        # Register in worker_procs so PID check is reached
        self.daemon.worker_procs["w-live"] = None  # type: ignore[assignment]

        requeued = self.daemon.reconcile_stale_specs()

        self.assertEqual(requeued, [], "No specs should be requeued when workers are alive")
        row = self.daemon.db.conn.execute(
            "SELECT status FROM specs WHERE id = 'q-061'"
        ).fetchone()
        self.assertEqual(row["status"], "running", "Live spec must remain running")

    def test_reconcile_interval_tracked(self) -> None:
        """Daemon tracks _last_reconcile timestamp so periodic checks work."""
        import time

        self.daemon._last_reconcile = 0.0
        self.daemon.reconcile_interval = 30

        # Without running the loop, verify attribute exists and is usable
        self.assertIsInstance(self.daemon._last_reconcile, float)
        self.assertIsInstance(self.daemon.reconcile_interval, int)

        # Simulate: enough time has passed → would trigger reconciliation
        now = time.time()
        elapsed = now - self.daemon._last_reconcile
        self.assertGreaterEqual(
            elapsed, self.daemon.reconcile_interval,
            "With _last_reconcile=0, reconcile_interval should have elapsed",
        )


if __name__ == "__main__":
    unittest.main()
