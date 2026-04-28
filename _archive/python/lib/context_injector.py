"""Context injection for BOI workers.

Combines external project context, BOI project context, and spec-referenced
context sources into a single block for worker prompt injection.

Python stdlib only. No external dependencies.
"""

import os
import re


class ContextInjector:
    def __init__(self, context_dir: str = "", boi_dir: str = "~/.boi"):
        """Initialize with paths to project context and BOI directories.

        Args:
            context_dir: Path to external project context directory.
                         Expected structure: {context_dir}/projects/{name}/context.md
                         If empty, external context is skipped.
            boi_dir: Path to BOI state directory (~/.boi by default).
        """
        self.context_dir = os.path.expanduser(context_dir) if context_dir else ""
        self.boi_dir = os.path.expanduser(boi_dir)

    def get_external_context(self, project_name: str) -> str:
        """Read {context_dir}/projects/{project_name}/context.md if it exists.
        Returns the content or empty string."""
        if not project_name or not self.context_dir:
            return ""
        context_path = os.path.join(
            self.context_dir, "projects", project_name, "context.md"
        )
        try:
            with open(context_path, "r") as f:
                return f.read()
        except (FileNotFoundError, PermissionError, OSError):
            return ""

    def get_boi_project_context(self, project_name: str) -> str:
        """Read ~/.boi/projects/{project_name}/context.md and research.md."""
        if not project_name:
            return ""
        projects_dir = os.path.join(self.boi_dir, "projects")
        parts = []
        for filename in ("context.md", "research.md"):
            filepath = os.path.join(projects_dir, project_name, filename)
            try:
                with open(filepath, "r") as f:
                    content = f.read().strip()
                    if content:
                        parts.append(content)
            except (FileNotFoundError, PermissionError, OSError):
                continue
        return "\n\n".join(parts)

    def get_context_sources_from_spec(self, spec_content: str) -> list:
        """Parse a ## Context Sources section from the spec.
        Each line starting with '- ' is a source path or URL.
        Returns list of file paths (URLs are returned as-is for the worker to fetch)."""
        if not spec_content:
            return []
        match = re.search(
            r"^## Context Sources\s*\n(.*?)(?=\n## |\Z)",
            spec_content,
            re.MULTILINE | re.DOTALL,
        )
        if not match:
            return []
        section = match.group(1)
        sources = []
        for line in section.strip().split("\n"):
            line = line.strip()
            if line.startswith("- "):
                source = line[2:].strip()
                if source:
                    sources.append(source)
        return sources

    def read_local_sources(self, sources: list) -> str:
        """Read local file paths from the sources list.
        Skip URLs (they start with http). Return concatenated content.
        Each source wrapped in a header: '### Source: {path}'"""
        parts = []
        for source in sources:
            if source.startswith("http://") or source.startswith("https://"):
                continue
            expanded = os.path.expanduser(source)
            try:
                if os.path.isdir(expanded):
                    try:
                        entries = sorted(os.listdir(expanded))
                    except OSError:
                        continue
                    for entry in entries:
                        if entry.endswith(".md"):
                            filepath = os.path.join(expanded, entry)
                            try:
                                with open(filepath, "r") as f:
                                    content = f.read().strip()
                                    if content:
                                        parts.append(
                                            f"### Source: {source}/{entry}\n\n{content}"
                                        )
                            except (FileNotFoundError, PermissionError, OSError):
                                continue
                elif os.path.isfile(expanded):
                    with open(expanded, "r") as f:
                        content = f.read().strip()
                        if content:
                            parts.append(f"### Source: {source}\n\n{content}")
            except (PermissionError, OSError):
                continue
        return "\n\n".join(parts)

    def build_context_block(self, project_name: str, spec_content: str) -> str:
        """Combine all context sources into a single block.
        Order: external context first, then BOI project context, then spec-referenced sources.
        Deduplicate if external and BOI context point to the same file.
        Wrap in '## Injected Context' header.
        If total exceeds 5000 chars, truncate with '[...truncated, full file at {path}]'."""
        sections = []

        # 1. External project context
        ext_ctx = self.get_external_context(project_name)
        ext_path = ""
        if self.context_dir:
            ext_path = os.path.join(
                self.context_dir, "projects", project_name, "context.md"
            )
        if ext_ctx:
            sections.append(f"### External Project Context ({ext_path})\n\n{ext_ctx}")

        # 2. BOI project context (skip if identical to external context)
        boi_ctx = self.get_boi_project_context(project_name)
        if boi_ctx and boi_ctx.strip() != ext_ctx.strip():
            boi_path = os.path.join(
                self.boi_dir, "projects", project_name
            )
            sections.append(f"### BOI Project Context ({boi_path})\n\n{boi_ctx}")

        # 3. Spec-referenced context sources
        sources = self.get_context_sources_from_spec(spec_content or "")
        if sources:
            filtered = sources
            if ext_path:
                filtered = [
                    s
                    for s in sources
                    if os.path.expanduser(s) != ext_path
                ]
            local_content = self.read_local_sources(filtered)
            if local_content:
                sections.append(local_content)

        if not sections:
            return ""

        body = "\n\n".join(sections)

        # Truncate if too long
        if len(body) > 5000:
            truncated = body[:4800]
            last_nl = truncated.rfind("\n")
            if last_nl > 4000:
                truncated = truncated[:last_nl]
            paths = []
            if ext_ctx and ext_path:
                paths.append(ext_path)
            boi_path = os.path.join(self.boi_dir, "projects", project_name)
            if boi_ctx:
                paths.append(boi_path)
            path_list = ", ".join(paths) if paths else "source files"
            body = f"{truncated}\n\n[...truncated, full file at {path_list}]"

        return f"## Injected Context\n\n{body}"
