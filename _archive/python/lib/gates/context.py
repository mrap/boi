"""Shared context and result types for gates."""

from __future__ import annotations

from dataclasses import dataclass, field


@dataclass
class HookContext:
    spec_id: str = ""
    spec_content: str = ""
    spec_path: str = ""
    repo_dir: str = ""
    phase: str = ""
    extra: dict = field(default_factory=dict)


@dataclass
class HookResult:
    passed: bool
    message: str = ""
    details: str = ""
