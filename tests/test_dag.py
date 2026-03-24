# test_dag.py — Tests for lib/dag.py DAG utilities.
#
# TDD: These tests are written BEFORE the implementation.
# Run with: python3 -m pytest tests/test_dag.py -v

import pytest
from lib.dag import (
    build_adjacency_list,
    critical_path,
    downstream_count,
    find_assignable_tasks,
    topological_sort,
)
from lib.spec_parser import BoiTask


def _task(tid: str, status: str = "PENDING", blocked_by: list[str] | None = None):
    """Helper to create a BoiTask with minimal fields."""
    return BoiTask(
        id=tid,
        title=f"Task {tid}",
        status=status,
        blocked_by=blocked_by or [],
    )


# ── topological_sort ────────────────────────────────────────────────


class TestTopologicalSort:
    def test_empty(self):
        assert topological_sort([]) == []

    def test_single_task(self):
        tasks = [_task("t-1")]
        result = topological_sort(tasks)
        assert result == ["t-1"]

    def test_linear_chain(self):
        """t-1 -> t-2 -> t-3"""
        tasks = [
            _task("t-1"),
            _task("t-2", blocked_by=["t-1"]),
            _task("t-3", blocked_by=["t-2"]),
        ]
        result = topological_sort(tasks)
        assert result.index("t-1") < result.index("t-2")
        assert result.index("t-2") < result.index("t-3")

    def test_diamond(self):
        """
        t-1 -> t-3
        t-2 -> t-3
        """
        tasks = [
            _task("t-1"),
            _task("t-2"),
            _task("t-3", blocked_by=["t-1", "t-2"]),
        ]
        result = topological_sort(tasks)
        assert result.index("t-1") < result.index("t-3")
        assert result.index("t-2") < result.index("t-3")

    def test_wide_fanout(self):
        """t-1 unblocks t-2, t-3, t-4, t-5"""
        tasks = [
            _task("t-1"),
            _task("t-2", blocked_by=["t-1"]),
            _task("t-3", blocked_by=["t-1"]),
            _task("t-4", blocked_by=["t-1"]),
            _task("t-5", blocked_by=["t-1"]),
        ]
        result = topological_sort(tasks)
        assert result[0] == "t-1"
        assert set(result[1:]) == {"t-2", "t-3", "t-4", "t-5"}

    def test_wide_fanin(self):
        """t-1, t-2, t-3, t-4 all block t-5"""
        tasks = [
            _task("t-1"),
            _task("t-2"),
            _task("t-3"),
            _task("t-4"),
            _task("t-5", blocked_by=["t-1", "t-2", "t-3", "t-4"]),
        ]
        result = topological_sort(tasks)
        assert result[-1] == "t-5"

    def test_cycle_raises(self):
        """t-1 -> t-2 -> t-1 (cycle)"""
        tasks = [
            _task("t-1", blocked_by=["t-2"]),
            _task("t-2", blocked_by=["t-1"]),
        ]
        with pytest.raises(ValueError, match="[Cc]ycle"):
            topological_sort(tasks)

    def test_three_node_cycle(self):
        """t-1 -> t-2 -> t-3 -> t-1"""
        tasks = [
            _task("t-1", blocked_by=["t-3"]),
            _task("t-2", blocked_by=["t-1"]),
            _task("t-3", blocked_by=["t-2"]),
        ]
        with pytest.raises(ValueError, match="[Cc]ycle"):
            topological_sort(tasks)

    def test_independent_tasks(self):
        """All tasks are independent, any order is valid."""
        tasks = [_task("t-1"), _task("t-2"), _task("t-3")]
        result = topological_sort(tasks)
        assert set(result) == {"t-1", "t-2", "t-3"}

    def test_complex_dag(self):
        """
        t-1 --> t-3 --> t-5
        t-2 --> t-3
        t-2 --> t-4 --> t-5
        """
        tasks = [
            _task("t-1"),
            _task("t-2"),
            _task("t-3", blocked_by=["t-1", "t-2"]),
            _task("t-4", blocked_by=["t-2"]),
            _task("t-5", blocked_by=["t-3", "t-4"]),
        ]
        result = topological_sort(tasks)
        assert result.index("t-1") < result.index("t-3")
        assert result.index("t-2") < result.index("t-3")
        assert result.index("t-2") < result.index("t-4")
        assert result.index("t-3") < result.index("t-5")
        assert result.index("t-4") < result.index("t-5")


# ── find_assignable_tasks ───────────────────────────────────────────


class TestFindAssignableTasks:
    def test_all_independent_pending(self):
        tasks = [_task("t-1"), _task("t-2"), _task("t-3")]
        result = find_assignable_tasks(tasks)
        assert set(result) == {"t-1", "t-2", "t-3"}

    def test_linear_chain_first_only(self):
        """Only t-1 is assignable in a linear chain."""
        tasks = [
            _task("t-1"),
            _task("t-2", blocked_by=["t-1"]),
            _task("t-3", blocked_by=["t-2"]),
        ]
        result = find_assignable_tasks(tasks)
        assert result == ["t-1"]

    def test_after_first_done(self):
        """t-1 done, t-2 becomes assignable."""
        tasks = [
            _task("t-1", status="DONE"),
            _task("t-2", blocked_by=["t-1"]),
            _task("t-3", blocked_by=["t-2"]),
        ]
        result = find_assignable_tasks(tasks)
        assert result == ["t-2"]

    def test_diamond_two_assignable(self):
        """t-1 and t-2 are assignable, t-3 is blocked."""
        tasks = [
            _task("t-1"),
            _task("t-2"),
            _task("t-3", blocked_by=["t-1", "t-2"]),
        ]
        result = find_assignable_tasks(tasks)
        assert set(result) == {"t-1", "t-2"}

    def test_diamond_one_done_still_blocked(self):
        """t-1 done but t-3 still blocked by t-2."""
        tasks = [
            _task("t-1", status="DONE"),
            _task("t-2"),
            _task("t-3", blocked_by=["t-1", "t-2"]),
        ]
        result = find_assignable_tasks(tasks)
        assert result == ["t-2"]

    def test_diamond_both_done_unblocks(self):
        """Both deps done, t-3 is assignable."""
        tasks = [
            _task("t-1", status="DONE"),
            _task("t-2", status="DONE"),
            _task("t-3", blocked_by=["t-1", "t-2"]),
        ]
        result = find_assignable_tasks(tasks)
        assert result == ["t-3"]

    def test_skips_done_tasks(self):
        tasks = [
            _task("t-1", status="DONE"),
            _task("t-2"),
        ]
        result = find_assignable_tasks(tasks)
        assert result == ["t-2"]

    def test_skips_skipped_tasks(self):
        tasks = [
            _task("t-1", status="SKIPPED"),
            _task("t-2", blocked_by=["t-1"]),
        ]
        # SKIPPED counts as "resolved" for dependency purposes
        result = find_assignable_tasks(tasks)
        assert result == ["t-2"]

    def test_in_progress_excluded(self):
        """Tasks already being worked on by another worker are excluded."""
        tasks = [_task("t-1"), _task("t-2"), _task("t-3")]
        result = find_assignable_tasks(tasks, in_progress={"t-1", "t-3"})
        assert result == ["t-2"]

    def test_no_assignable(self):
        """All tasks are DONE."""
        tasks = [
            _task("t-1", status="DONE"),
            _task("t-2", status="DONE"),
        ]
        result = find_assignable_tasks(tasks)
        assert result == []

    def test_all_blocked(self):
        """All PENDING tasks have unmet deps."""
        tasks = [
            _task("t-1", blocked_by=["t-2"]),
            _task("t-2", blocked_by=["t-1"]),
        ]
        result = find_assignable_tasks(tasks)
        assert result == []

    def test_wide_fanout_all_assignable(self):
        """5 independent tasks after root is done."""
        tasks = [
            _task("t-1", status="DONE"),
            _task("t-2", blocked_by=["t-1"]),
            _task("t-3", blocked_by=["t-1"]),
            _task("t-4", blocked_by=["t-1"]),
            _task("t-5", blocked_by=["t-1"]),
            _task("t-6", blocked_by=["t-1"]),
        ]
        result = find_assignable_tasks(tasks)
        assert set(result) == {"t-2", "t-3", "t-4", "t-5", "t-6"}

    def test_failed_task_does_not_unblock(self):
        """A FAILED dependency does NOT unblock downstream tasks."""
        tasks = [
            _task("t-1", status="FAILED"),
            _task("t-2", blocked_by=["t-1"]),
        ]
        result = find_assignable_tasks(tasks)
        assert result == []


# ── build_adjacency_list ────────────────────────────────────────────


class TestBuildAdjacencyList:
    def test_no_deps(self):
        tasks = [_task("t-1"), _task("t-2")]
        adj = build_adjacency_list(tasks)
        assert adj == {"t-1": [], "t-2": []}

    def test_linear(self):
        tasks = [
            _task("t-1"),
            _task("t-2", blocked_by=["t-1"]),
        ]
        adj = build_adjacency_list(tasks)
        assert adj == {"t-1": ["t-2"], "t-2": []}

    def test_diamond(self):
        tasks = [
            _task("t-1"),
            _task("t-2"),
            _task("t-3", blocked_by=["t-1", "t-2"]),
        ]
        adj = build_adjacency_list(tasks)
        assert "t-3" in adj["t-1"]
        assert "t-3" in adj["t-2"]
        assert adj["t-3"] == []


# ── downstream_count ────────────────────────────────────────────────


class TestDownstreamCount:
    def test_leaf_has_zero(self):
        tasks = [_task("t-1"), _task("t-2", blocked_by=["t-1"])]
        assert downstream_count(tasks, "t-2") == 0

    def test_root_counts_all(self):
        tasks = [
            _task("t-1"),
            _task("t-2", blocked_by=["t-1"]),
            _task("t-3", blocked_by=["t-1"]),
        ]
        assert downstream_count(tasks, "t-1") == 2

    def test_transitive(self):
        """t-1 -> t-2 -> t-3: t-1 has 2 downstream"""
        tasks = [
            _task("t-1"),
            _task("t-2", blocked_by=["t-1"]),
            _task("t-3", blocked_by=["t-2"]),
        ]
        assert downstream_count(tasks, "t-1") == 2
        assert downstream_count(tasks, "t-2") == 1

    def test_diamond_no_double_count(self):
        """t-1 -> t-3, t-2 -> t-3: t-1 has 1 downstream (t-3)"""
        tasks = [
            _task("t-1"),
            _task("t-2"),
            _task("t-3", blocked_by=["t-1", "t-2"]),
        ]
        assert downstream_count(tasks, "t-1") == 1


# ── critical_path ───────────────────────────────────────────────────


class TestCriticalPath:
    def test_single_task(self):
        tasks = [_task("t-1")]
        result = critical_path(tasks)
        assert result == ["t-1"]

    def test_linear_chain(self):
        tasks = [
            _task("t-1"),
            _task("t-2", blocked_by=["t-1"]),
            _task("t-3", blocked_by=["t-2"]),
        ]
        result = critical_path(tasks)
        assert result == ["t-1", "t-2", "t-3"]

    def test_diamond_longest(self):
        """
        t-1 -> t-2 -> t-4
        t-1 -> t-3 -> t-4
        Both paths are length 3, either is valid.
        """
        tasks = [
            _task("t-1"),
            _task("t-2", blocked_by=["t-1"]),
            _task("t-3", blocked_by=["t-1"]),
            _task("t-4", blocked_by=["t-2", "t-3"]),
        ]
        result = critical_path(tasks)
        assert len(result) == 3
        assert result[0] == "t-1"
        assert result[-1] == "t-4"

    def test_independent_tasks(self):
        """No deps, longest path is 1."""
        tasks = [_task("t-1"), _task("t-2"), _task("t-3")]
        result = critical_path(tasks)
        assert len(result) == 1

    def test_asymmetric(self):
        """
        t-1 -> t-2 -> t-3 -> t-4 (length 4)
        t-5 -> t-4 (length 2)
        Critical path is t-1 -> t-2 -> t-3 -> t-4
        """
        tasks = [
            _task("t-1"),
            _task("t-2", blocked_by=["t-1"]),
            _task("t-3", blocked_by=["t-2"]),
            _task("t-4", blocked_by=["t-3", "t-5"]),
            _task("t-5"),
        ]
        result = critical_path(tasks)
        assert len(result) == 4
        assert result == ["t-1", "t-2", "t-3", "t-4"]
