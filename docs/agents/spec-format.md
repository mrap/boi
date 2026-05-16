# Spec Format Reference

YAML schema for BOI specs. See also: [glossary.md](glossary.md) for term definitions, [docs/design-draft-specs-and-deps.md](../design-draft-specs-and-deps.md) for historical design rationale.

## YAML schema

```yaml
title: "Feature name"
mode: execute              # execute | challenge | discover | generate | v2
workspace: /path/to/repo   # optional — override workspace
tasks:
  - id: T1A2B
    title: "Task name"
    status: PENDING         # must be PENDING at intake (enforced by validate_intake)
    depends: ["T0X1Y"]     # optional — comma-separated also accepted
    spec: |
      What to implement. Be concrete.
    verify: "command that returns 0 on success"
```

**Intake rules** (`src/spec.rs:411` — `validate_intake()`):
- All tasks must have status `PENDING`
- Task list must be non-empty
- Status values must be valid enum variants

## Pipeline modes

Defined in `phases/pipelines.toml`. Each mode selects a different phase sequence.

| Mode | Pre-phases | Task phases | Post-phases |
|------|-----------|-------------|-------------|
| `execute` (default) | spec-critique, spec-improve | execute, task-verify | critic |
| `challenge` | plan-critique | execute, task-verify | critic |
| `discover` | — | execute, task-verify | critic, evaluate |
| `generate` | plan-critique | decompose, execute, code-review, task-verify | critic, evaluate |
| `v2` | spec-critique, spec-improve | execute, review, commit | doc-update, critic, merge, cleanup |

See also: [ARCHITECTURE.md](../../ARCHITECTURE.md) for lifecycle flow, [adding-features.md](adding-features.md) for creating new phases.
