# Context Injection for BOI Workers

## Problem

BOI workers run in isolated Claude sessions with no memory of previous iterations. The spec prompt is their only context. When a worker needs to assess project state, it can only discover facts through active search (file reads, web fetches). It has no access to accumulated project knowledge that may exist outside the spec.

This can cause factual errors when workers start from a stale baseline, miss recent decisions, or waste iterations correcting wrong assumptions.

**Root cause:** Workers load project context from `~/.boi/projects/{project}/` but may not know about context maintained elsewhere (e.g., a separate project management directory).

---

## Solution: Three-Layer Context Injection

### Layer 1: External Context Auto-Inject (automatic)

**What it does:** The worker reads from a configurable external project context directory and injects it into the worker prompt alongside the existing BOI project context.

**How it works:** The `ContextInjector` class in `lib/context_injector.py` combines context from both directories. It takes a `context_dir` parameter pointing to your external project workspace.

**When it fires:** Automatically, whenever the queue entry has a `project` field set and `context_dir` is configured.

**Action needed from spec authors:** None. Just ensure the `project` field is set when dispatching.

### Layer 2: Spec Context Sources (opt-in)

**What it does:** Spec authors can add a `## Context Sources` section listing specific files or URLs to inject into the worker prompt.

**How to use:** Add this section to any spec:

```markdown
## Context Sources
- ~/projects/my-project/context.md
- ~/projects/my-project/decisions/architecture.md
- https://docs.google.com/spreadsheets/d/example
```

Local file paths are read and injected. URLs are listed for the worker to fetch at runtime.

**When to use:** When you need context beyond the project's `context.md`. For example, specific decision docs, meeting notes, or external references.

### Layer 3: Pre-flight Gathering (automatic)

**What it does:** At dispatch time, a pre-flight script (`lib/preflight_context.py`) runs and:
1. Reads `{context_dir}/projects/{project_name}/context.md`
2. Parses any `## Context Sources` section from the spec
3. Reads each local file source
4. Appends a `## Preflight Context` section to the spec file

**When it fires:** Automatically during `boi dispatch`, before the spec is queued.

**Failure behavior:** Non-fatal. If pre-flight fails, dispatch continues without injected context. Workers still get Layer 1 at prompt generation time.

---

## How to Use

### For most specs: do nothing

If you set the `project` field when dispatching, context is injected automatically via Layers 1 and 3.

```bash
boi dispatch --project my-project my-spec.md
```

The worker will receive:
- External project context (if `context_dir` is configured)
- BOI project context (`~/.boi/projects/my-project/context.md`)
- Any pre-flight gathered context appended to the spec

### For specs needing specific context: add Context Sources

Add a `## Context Sources` section to list additional files:

```markdown
## Context Sources
- ~/projects/my-project/decisions/api-design.md
- ~/meetings/2026-03-07-sprint-review.md
```

---

## Limitations

1. **Staleness at dispatch time.** Pre-flight context (Layer 3) is gathered once when the spec is dispatched. If the spec sits in the queue for days, the injected context may become stale.

2. **URLs are not fetched automatically.** URLs listed in `## Context Sources` are passed through to the worker prompt as-is. The worker can fetch them at runtime, but this is not guaranteed. For critical context, copy the relevant content into the spec directly.

3. **Token cost.** The full context is injected into every worker prompt. For large context files (>5000 chars), the `ContextInjector` truncates with a pointer to the full file. Monitor prompt sizes if context files grow significantly.

4. **No semantic filtering.** The current implementation injects the entire context, not just relevant sections.

---

## Architecture Diagram

```
+------------------------------------------------------------------+
|                        boi dispatch                               |
|                                                                   |
|  1. Write spec to ~/.boi/queue/q-NNN.spec.md                    |
|  2. Pre-flight context gather (Layer 3)                          |
|     +-- Read {context_dir}/projects/{project}/context.md         |
|     +-- Parse ## Context Sources from spec                       |
|     +-- Read local file sources                                  |
|     +-- Append ## Preflight Context to spec                      |
|  3. Create queue entry with project field                        |
+-------------------------------+----------------------------------+
                                |
                                v
+------------------------------------------------------------------+
|                     daemon picks up spec                          |
|                                                                   |
|  worker prompt generation                                        |
|     +-- ContextInjector.build_context_block()                    |
|     |   +-- Read external project context (Layer 1)              |
|     |   +-- Read ~/.boi/projects/{project}/context.md            |
|     |   +-- Read spec-referenced sources (Layer 2)               |
|     |   +-- Deduplicate + truncate if > 5000 chars               |
|     +-- Inject into worker prompt                                |
+-------------------------------+----------------------------------+
                                |
                                v
+------------------------------------------------------------------+
|                    Claude worker session                          |
|                                                                   |
|  Prompt includes:                                                |
|     - Spec content (with ## Preflight Context from Layer 3)      |
|     - Injected context block (external + BOI context, Layer 1)   |
|     - Instructions to reference Injected Context (Layer 2)       |
|                                                                   |
|  Worker has full context to make informed assessments             |
+------------------------------------------------------------------+
```

---

## Key Files

| File | Purpose |
|------|---------|
| `lib/context_injector.py` | Core class that combines context from all sources |
| `lib/preflight_context.py` | Pre-flight gathering script called at dispatch time |
| `tests/test_context_injector.py` | Tests for context injection and preflight gathering |
