"""Tests for blank-line handling in parse_boi_spec.

Regression tests for the bug where tasks without explicit status lines
(or with blank lines / non-status content between heading and status)
were silently dropped from the parsed output.
"""

import pytest

from lib.spec_parser import parse_boi_spec


class TestBlankLineBetweenHeadingAndStatus:
    """Tasks with blank lines before the status line must be parsed."""

    def test_single_blank_line(self):
        spec = "### t-1: Task\n\nPENDING\n\n**Spec:** stuff\n"
        tasks = parse_boi_spec(spec)
        assert len(tasks) == 1
        assert tasks[0].id == "t-1"
        assert tasks[0].status == "PENDING"

    def test_multiple_blank_lines(self):
        spec = "### t-1: Task\n\n\n\nPENDING\n\n**Spec:** stuff\n"
        tasks = parse_boi_spec(spec)
        assert len(tasks) == 1
        assert tasks[0].status == "PENDING"

    def test_blank_line_with_done(self):
        spec = "### t-1: Task\n\nDONE\n\n**Spec:** stuff\n"
        tasks = parse_boi_spec(spec)
        assert len(tasks) == 1
        assert tasks[0].status == "DONE"

    def test_blank_line_with_critic_prefix(self):
        spec = "### t-1: Task\n\n[CRITIC] PENDING\n\n**Spec:** stuff\n"
        tasks = parse_boi_spec(spec)
        assert len(tasks) == 1
        assert tasks[0].status == "PENDING"

    def test_blank_line_with_superseded(self):
        spec = "### t-1: Old task\n\nSUPERSEDED by t-5\n"
        tasks = parse_boi_spec(spec)
        assert len(tasks) == 1
        assert tasks[0].status == "SUPERSEDED"
        assert tasks[0].superseded_by == "t-5"

    def test_multiple_tasks_some_with_blank_lines(self):
        spec = (
            "### t-1: First task\nPENDING\n\n**Spec:** a\n\n"
            "### t-2: Second task\n\nPENDING\n\n**Spec:** b\n\n"
            "### t-3: Third task\nDONE\n\n**Spec:** c\n"
        )
        tasks = parse_boi_spec(spec)
        assert len(tasks) == 3
        assert tasks[0].status == "PENDING"
        assert tasks[1].status == "PENDING"
        assert tasks[2].status == "DONE"

    def test_body_preserved_with_blank_line(self):
        spec = "### t-1: Task\n\nPENDING\n\n**Spec:** Do the thing\n**Verify:** run test\n"
        tasks = parse_boi_spec(spec)
        assert "**Spec:** Do the thing" in tasks[0].body
        assert "**Verify:** run test" in tasks[0].body


class TestMissingStatusDefaultsToPending:
    """Tasks without any status line should default to PENDING, not be dropped."""

    def test_no_status_before_next_heading(self):
        spec = (
            "### t-1: Task without status\n\n"
            "**Spec:** do thing\n\n"
            "### t-2: Task with status\nPENDING\n\n**Spec:** other\n"
        )
        tasks = parse_boi_spec(spec)
        assert len(tasks) == 2, f"Expected 2 tasks, got {len(tasks)}: {[t.id for t in tasks]}"
        assert tasks[0].id == "t-1"
        assert tasks[0].status == "PENDING"  # defaulted
        assert tasks[1].id == "t-2"
        assert tasks[1].status == "PENDING"

    def test_no_status_at_end_of_file(self):
        spec = "### t-1: First\nDONE\n\n### t-2: Last task no status\n"
        tasks = parse_boi_spec(spec)
        assert len(tasks) == 2
        assert tasks[0].status == "DONE"
        assert tasks[1].id == "t-2"
        assert tasks[1].status == "PENDING"  # defaulted

    def test_single_task_no_status(self):
        spec = "### t-1: Only task\n\n**Spec:** do it\n"
        tasks = parse_boi_spec(spec)
        assert len(tasks) == 1
        assert tasks[0].status == "PENDING"

    def test_body_preserved_when_status_missing(self):
        spec = "### t-1: Task\n\n**Spec:** important body\n**Verify:** check\n"
        tasks = parse_boi_spec(spec)
        assert len(tasks) == 1
        assert "important body" in tasks[0].body


class TestNonStatusTextBeforeStatus:
    """Non-status text between heading and status should be captured as body."""

    def test_description_before_status(self):
        spec = "### t-1: Task\nSome description text\nPENDING\n\n**Spec:** stuff\n"
        tasks = parse_boi_spec(spec)
        assert len(tasks) == 1
        assert tasks[0].status == "PENDING"
        assert "Some description text" in tasks[0].body

    def test_non_status_text_not_silently_discarded(self):
        spec = "### t-1: Task\nExtra notes here\n\nPENDING\n"
        tasks = parse_boi_spec(spec)
        assert len(tasks) == 1
        assert "Extra notes" in tasks[0].body
