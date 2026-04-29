# Brain Directory

## What It Is and Why

A **brain directory** is a local folder containing a `CLAUDE.md` file that BOI reads before dispatching each worker. Its contents are prepended to the worker prompt as a `## System Context` section, giving workers project-aware context — rules, conventions, forbidden actions — without embedding that context in every spec file.

This keeps specs focused on *what to do* while letting the brain directory carry *how to do it* for your project.

## Configuration

### Global (applies to all specs)

In `~/.boi/config.yaml`:

```yaml
brain: ~/mrap-hex
```

### Per-Spec (overrides global)

In any spec YAML file, add a top-level `brain` field:

```yaml
title: "My feature"
brain: ~/projects/myrepo
mode: execute
tasks:
  - id: t-1
    title: "Do the thing"
    status: PENDING
    spec: |
      ...
```

**Precedence:** spec-level `brain` overrides the global config value. If neither is set, no brain context is injected.

### Validation

BOI validates the brain path at dispatch time. It will fail fast (not silently skip) if:
- The directory does not exist
- The directory exists but contains no `CLAUDE.md`

## Token Budget Guidance

BOI truncates brain content to **32,000 characters** (~8K tokens) before injecting it. Content beyond that limit is dropped silently from the tail.

Guidelines:
- **Keep CLAUDE.md under 16K chars** for comfortable headroom. This leaves room for the worker prompt itself within a 32K context injection.
- **Put the most critical rules at the top.** Truncation cuts from the bottom, so lead with must-know constraints.
- **Avoid long examples.** Reference file paths instead of inlining large code blocks. Workers can read files.
- **Prune regularly.** A smaller, current brain is more useful than a large, stale one.

## When to Use

- Your project has non-obvious conventions (naming, file structure, forbidden patterns) that workers otherwise get wrong.
- Multiple specs share the same repo context and you don't want to repeat it in each.
- You want to prevent specific classes of mistakes (e.g., "never drop the `events` table", "always write atomic files").

## When Not to Use

- **Simple or one-off specs** where the task spec itself is self-contained. Brain injection adds latency (a file read) and prompt overhead for no gain.
- **Sensitive information.** Brain content is sent verbatim to the LLM. Do not put credentials, tokens, or PII in `CLAUDE.md`.
- **Very large CLAUDE.md files.** Content over 32K chars is truncated. If your brain file is that large, split it and link to sub-documents from `CLAUDE.md` instead.
- **Cross-project workers.** If a spec spans multiple repos with conflicting conventions, per-spec brain is safer than a global one.
