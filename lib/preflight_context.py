"""Pre-flight context gathering for BOI spec dispatch.

Reads project context and spec-referenced sources at dispatch time,
building a ## Preflight Context section to append to the spec before queuing.

Python stdlib only. No external dependencies.
"""

import os
import re


def _read_file(path: str) -> str:
    """Read a file, return content or empty string on error."""
    try:
        with open(path, "r") as f:
            return f.read()
    except (FileNotFoundError, PermissionError, OSError):
        return ""


def _parse_context_sources(spec_content: str) -> list:
    """Parse ## Context Sources section from spec content.
    Returns list of source paths/URLs."""
    if not spec_content:
        return []
    match = re.search(
        r"^## Context Sources\s*\n(.*?)(?=\n## |\Z)",
        spec_content,
        re.MULTILINE | re.DOTALL,
    )
    if not match:
        return []
    sources = []
    for line in match.group(1).strip().split("\n"):
        line = line.strip()
        if line.startswith("- "):
            source = line[2:].strip()
            if source:
                sources.append(source)
    return sources


def _read_local_sources(sources: list) -> list:
    """Read local file sources (skip URLs). Returns list of (path, content) tuples."""
    results = []
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
                        content = _read_file(filepath)
                        if content.strip():
                            results.append((f"{source}/{entry}", content.strip()))
            elif os.path.isfile(expanded):
                content = _read_file(expanded)
                if content.strip():
                    results.append((source, content.strip()))
        except (PermissionError, OSError):
            continue
    return results


def _summarize_section(content: str) -> str:
    """Keep headers + first 2 lines of each section for truncation."""
    lines = content.split("\n")
    result = []
    section_lines = 0
    for line in lines:
        if line.startswith("#"):
            result.append(line)
            section_lines = 0
        elif section_lines < 2:
            result.append(line)
            section_lines += 1
    return "\n".join(result)


def gather_preflight_context(
    spec_path: str,
    project_name: str,
    context_dir: str = "",
) -> str:
    """Read context sources and build a preflight context block.

    1. Read {context_dir}/projects/{project_name}/context.md (if context_dir set)
    2. Parse any ## Context Sources section from the spec for additional file paths
    3. Read each local file source
    4. Build a ## Preflight Context section with all gathered data
    5. If total context > 8000 chars, summarize by keeping headers + first 2 lines

    Args:
        spec_path: Path to the spec file.
        project_name: Name of the project for context lookup.
        context_dir: Optional external project context directory.
                     Expected structure: {context_dir}/projects/{name}/context.md

    Returns the context block to append to the spec.
    Does NOT modify the spec file (caller handles that).
    """
    parts = []

    # 1. Read external project context
    if project_name and context_dir:
        ctx_path = os.path.expanduser(
            f"{context_dir}/projects/{project_name}/context.md"
        )
        ctx_content = _read_file(ctx_path)
        if ctx_content.strip():
            parts.append(
                f"### External Project Context ({ctx_path})\n\n{ctx_content.strip()}"
            )

    # 2. Parse spec for additional context sources
    spec_content = ""
    if spec_path:
        spec_content = _read_file(spec_path)

    sources = _parse_context_sources(spec_content)

    # 3. Read local file sources
    if sources:
        local_sources = _read_local_sources(sources)
        for path, content in local_sources:
            parts.append(f"### Source: {path}\n\n{content}")

    if not parts:
        return ""

    body = "\n\n".join(parts)

    # 5. Summarize if too long
    if len(body) > 8000:
        body = _summarize_section(body)
        if len(body) > 8000:
            body = body[:7800]
            last_nl = body.rfind("\n")
            if last_nl > 6000:
                body = body[:last_nl]
            body += "\n\n[...truncated for token budget]"

    return f"## Preflight Context\n\n{body}"
