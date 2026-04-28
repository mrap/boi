# test_parallel_assignment.py — Tests for parallel DAG task assignment.
#
# Tests the daemon's ability to assign multiple workers to independent
# tasks within the same spec, respecting the dependency graph.

import os
import sqlite3
import tempfile

from lib.daemon_ops import find_parallel_assignments
from lib.dag import find_assignable_tasks
from lib.db import Database
from lib.spec_parser import BoiTask, parse_boi_spec


def _make_spec_content(tasks_block: str) -> str:
    """Wrap task definitions in a minimal spec structure."""
    return f"# Test Spec\n\n## Tasks\n\n{tasks_block}\n"


def _make_diamond_spec() -> str:
    """Create a diamond DAG spec: t-1, t-2 independent, t-3 blocked by both."""
    return _make_spec_content(
        "### t-1: Research A\nPENDING\n\n**Spec:** Do A\n**Verify:** check A\n\n"
        "### t-2: Research B\nPENDING\n\n**Spec:** Do B\n**Verify:** check B\n\n"
        "### t-3: Synthesize\nPENDING\n\n**Blocked by:** t-1, t-2\n\n"
        "**Spec:** Combine A and B\n**Verify:** check C\n"
    )


def _make_linear_spec() -> str:
    """Create a linear chain: t-1 -> t-2 -> t-3."""
    return _make_spec_content(
        "### t-1: Step 1\nPENDING\n\n**Spec:** Do 1\n**Verify:** check 1\n\n"
        "### t-2: Step 2\nPENDING\n\n**Blocked by:** t-1\n\n**Spec:** Do 2\n**Verify:** check 2\n\n"
        "### t-3: Step 3\nPENDING\n\n**Blocked by:** t-2\n\n**Spec:** Do 3\n**Verify:** check 3\n"
    )


def _make_wide_fanout_spec() -> str:
    """Create a wide fanout: t-1 done, t-2..t-6 all unblocked, t-7 blocked by all."""
    tasks = "### t-1: Root\nDONE\n\n**Spec:** Root task\n**Verify:** check\n\n"
    for i in range(2, 7):
        tasks += (
            f"### t-{i}: Branch {i}\nPENDING\n\n"
            f"**Blocked by:** t-1\n\n**Spec:** Do {i}\n**Verify:** check {i}\n\n"
        )
    blocked = ", ".join(f"t-{i}" for i in range(2, 7))
    tasks += (
        "### t-7: Synthesis\nPENDING\n\n"
        f"**Blocked by:** {blocked}\n\n**Spec:** Synthesize\n**Verify:** check 7\n"
    )
    return _make_spec_content(tasks)


def _write_minimal_spec(tmp_dir: str) -> str:
    """Write a minimal spec file and return its path."""
    spec_path = os.path.join(tmp_dir, "minimal-spec.md")
    with open(spec_path, "w") as f:
        f.write(
            "# Test\n\n## Tasks\n\n"
            "### t-1: Task A\nPENDING\n\n**Spec:** Do A\n**Verify:** ok\n"
        )
    return spec_path


def _setup_db():
    """Create a temporary Database instance with workers registered."""
    tmp = tempfile.mkdtemp()
    db_path = os.path.join(tmp, "boi.db")
    queue_dir = os.path.join(tmp, "queue")
    db = Database(db_path, queue_dir)

    # Ensure current_task_id column exists (migration for existing DBs)
    try:
        db.conn.execute("SELECT current_task_id FROM workers LIMIT 1")
    except sqlite3.OperationalError:
        db.conn.execute("ALTER TABLE workers ADD COLUMN current_task_id TEXT")
        db.conn.commit()

    # Register 5 workers
    for i in range(1, 6):
        db.register_worker(f"w-{i}", f"/tmp/worktree-{i}")
    return db, tmp


class TestFindParallelAssignments:
    """Test find_parallel_assignments() from daemon_ops.py."""

    def test_diamond_returns_two_independent(self):
        db, tmp = _setup_db()
        spec_path = os.path.join(tmp, "spec.md")
        with open(spec_path, "w") as f:
            f.write(_make_diamond_spec())

        # Enqueue a spec
        result = db.enqueue(spec_path, queue_id="q-001")
        spec_id = result["id"]

        assignments = find_parallel_assignments(db, spec_id, spec_path)
        assert set(assignments) == {"t-1", "t-2"}
        # t-3 should NOT be assignable (blocked by t-1, t-2)
        assert "t-3" not in assignments
        db.close()

    def test_linear_returns_first_only(self):
        db, tmp = _setup_db()
        spec_path = os.path.join(tmp, "spec.md")
        with open(spec_path, "w") as f:
            f.write(_make_linear_spec())

        result = db.enqueue(spec_path, queue_id="q-001")
        spec_id = result["id"]

        assignments = find_parallel_assignments(db, spec_id, spec_path)
        assert assignments == ["t-1"]
        db.close()

    def test_wide_fanout_returns_five(self):
        db, tmp = _setup_db()
        spec_path = os.path.join(tmp, "spec.md")
        with open(spec_path, "w") as f:
            f.write(_make_wide_fanout_spec())

        result = db.enqueue(spec_path, queue_id="q-001")
        spec_id = result["id"]

        assignments = find_parallel_assignments(db, spec_id, spec_path)
        assert set(assignments) == {"t-2", "t-3", "t-4", "t-5", "t-6"}
        assert "t-7" not in assignments
        db.close()

    def test_excludes_in_progress_tasks(self):
        db, tmp = _setup_db()
        spec_path = os.path.join(tmp, "spec.md")
        with open(spec_path, "w") as f:
            f.write(_make_diamond_spec())

        result = db.enqueue(spec_path, queue_id="q-001")
        spec_id = result["id"]

        # Simulate w-1 already working on t-1
        db.assign_worker("w-1", spec_id, pid=12345, task_id="t-1")

        assignments = find_parallel_assignments(db, spec_id, spec_path)
        assert assignments == ["t-2"]
        assert "t-1" not in assignments
        db.close()

    def test_sorted_by_downstream_impact(self):
        """Tasks unblocking more downstream work should come first."""
        db, tmp = _setup_db()
        # Create spec where t-1 unblocks 3 tasks, t-2 unblocks 1 task
        content = _make_spec_content(
            "### t-1: High impact\nPENDING\n\n**Spec:** Do\n**Verify:** check\n\n"
            "### t-2: Low impact\nPENDING\n\n**Spec:** Do\n**Verify:** check\n\n"
            "### t-3: Dep of t-1\nPENDING\n\n**Blocked by:** t-1\n\n**Spec:** Do\n**Verify:** check\n\n"
            "### t-4: Dep of t-1\nPENDING\n\n**Blocked by:** t-1\n\n**Spec:** Do\n**Verify:** check\n\n"
            "### t-5: Dep of t-1\nPENDING\n\n**Blocked by:** t-1\n\n**Spec:** Do\n**Verify:** check\n\n"
            "### t-6: Dep of t-2\nPENDING\n\n**Blocked by:** t-2\n\n**Spec:** Do\n**Verify:** check\n"
        )
        spec_path = os.path.join(tmp, "spec.md")
        with open(spec_path, "w") as f:
            f.write(content)

        result = db.enqueue(spec_path, queue_id="q-001")
        spec_id = result["id"]

        assignments = find_parallel_assignments(db, spec_id, spec_path)
        assert assignments[0] == "t-1"  # Unblocks 3 tasks
        assert "t-2" in assignments  # Unblocks 1 task
        db.close()


class TestDBWorkerTaskTracking:
    """Test current_task_id tracking in the workers table."""

    def test_assign_with_task_id(self):
        db, tmp = _setup_db()
        result = db.enqueue(_write_minimal_spec(tmp), queue_id="q-001")
        spec_id = result["id"]

        db.assign_worker("w-1", spec_id, pid=100, task_id="t-1")
        worker = db.get_worker("w-1")
        assert worker["current_task_id"] == "t-1"
        assert worker["current_spec_id"] == spec_id
        db.close()

    def test_free_clears_task_id(self):
        db, tmp = _setup_db()
        result = db.enqueue(_write_minimal_spec(tmp), queue_id="q-001")
        spec_id = result["id"]

        db.assign_worker("w-1", spec_id, pid=100, task_id="t-1")
        db.free_worker("w-1")
        worker = db.get_worker("w-1")
        assert worker["current_task_id"] is None
        assert worker["current_spec_id"] is None
        db.close()

    def test_get_in_progress_task_ids(self):
        db, tmp = _setup_db()
        result = db.enqueue(_write_minimal_spec(tmp), queue_id="q-001")
        spec_id = result["id"]

        db.assign_worker("w-1", spec_id, pid=100, task_id="t-1")
        db.assign_worker("w-2", spec_id, pid=101, task_id="t-2")

        in_progress = db.get_in_progress_task_ids(spec_id)
        assert in_progress == {"t-1", "t-2"}
        db.close()

    def test_get_workers_on_spec(self):
        db, tmp = _setup_db()
        result = db.enqueue(_write_minimal_spec(tmp), queue_id="q-001")
        spec_id = result["id"]

        db.assign_worker("w-1", spec_id, pid=100, task_id="t-1")
        db.assign_worker("w-2", spec_id, pid=101, task_id="t-2")

        workers = db.get_workers_on_spec(spec_id)
        assert len(workers) == 2
        worker_ids = {w["id"] for w in workers}
        assert worker_ids == {"w-1", "w-2"}
        db.close()

    def test_no_workers_on_spec(self):
        db, tmp = _setup_db()
        result = db.enqueue(_write_minimal_spec(tmp), queue_id="q-001")
        spec_id = result["id"]

        workers = db.get_workers_on_spec(spec_id)
        assert workers == []
        in_progress = db.get_in_progress_task_ids(spec_id)
        assert in_progress == set()
        db.close()


class TestSpecConcurrency:
    """Test that multiple workers can work on the same spec safely."""

    def test_two_workers_different_tasks_same_spec(self):
        """Two workers assigned to different tasks on the same spec."""
        db, tmp = _setup_db()
        spec_path = os.path.join(tmp, "spec.md")
        with open(spec_path, "w") as f:
            f.write(_make_diamond_spec())

        result = db.enqueue(spec_path, queue_id="q-001")
        spec_id = result["id"]

        # Assign w-1 to t-1, w-2 to t-2
        db.assign_worker("w-1", spec_id, pid=100, task_id="t-1")
        db.assign_worker("w-2", spec_id, pid=101, task_id="t-2")

        # Verify both are tracked
        in_progress = db.get_in_progress_task_ids(spec_id)
        assert in_progress == {"t-1", "t-2"}

        # After w-1 completes, free it
        db.free_worker("w-1")
        in_progress = db.get_in_progress_task_ids(spec_id)
        assert in_progress == {"t-2"}

        # After w-2 completes, free it
        db.free_worker("w-2")
        in_progress = db.get_in_progress_task_ids(spec_id)
        assert in_progress == set()
        db.close()

    def test_pool_limit_respected(self):
        """When more assignable tasks than workers, limit is respected."""
        db, tmp = _setup_db()
        spec_path = os.path.join(tmp, "spec.md")
        with open(spec_path, "w") as f:
            f.write(_make_wide_fanout_spec())

        result = db.enqueue(spec_path, queue_id="q-001")
        spec_id = result["id"]

        assignments = find_parallel_assignments(db, spec_id, spec_path)
        # 5 assignable tasks, 5 workers available
        assert len(assignments) == 5

        # Assign all 5 workers
        for i, task_id in enumerate(assignments):
            db.assign_worker(f"w-{i + 1}", spec_id, pid=200 + i, task_id=task_id)

        # No more tasks assignable (all in progress or blocked)
        assignments = find_parallel_assignments(db, spec_id, spec_path)
        assert assignments == []
        db.close()
