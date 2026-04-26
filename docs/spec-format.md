# Spec Format

A BOI spec is a YAML file that defines ordered tasks for autonomous execution. Each task is self-contained: a worker reads the spec, executes the next PENDING task, marks it DONE, and exits. The spec file on disk is the single source of truth.

## Minimal Example

```yaml
title: Feature Name
mode: execute

tasks:
  - id: t-1
    title: First task
    status: PENDING
    spec: What to do.
    verify: How to prove it worked.

  - id: t-2
    title: Second task
    status: PENDING
    spec: What to do next.
    verify: How to verify.
```

## Task Structure

Every task requires five fields:

### 1. `id`

```yaml
id: t-1
```

Rules:
- `t-` prefix followed by a sequential number
- Must be unique within the spec
- Numbers must be sequential (t-1, t-2, t-3...)

### 2. `title`

```yaml
title: Descriptive task title
```

A short human-readable description of what this task does.

### 3. `status`

```yaml
status: PENDING
```

Valid statuses:

| Status | Meaning |
|--------|---------|
| `PENDING` | Not yet started. Workers pick this up. |
| `DONE` | Completed successfully. |
| `SKIPPED` | Intentionally bypassed (with reason). |
| `FAILED` | Attempted but could not complete. |

### 4. `spec`

```yaml
spec: |
  Create a `UserPreferences` model with fields: user_id (int),
  theme (enum: light/dark), language (string), notifications_enabled (bool).
  Follow the existing model patterns in `src/models/`.
```

This tells the worker exactly what to do. Be concrete:
- Name specific files, functions, and patterns
- Reference earlier tasks if there are dependencies
- Include enough context for a worker with zero prior knowledge

### 5. `verify`

```yaml
verify: |
  python3 -m pytest tests/test_models.py -v
  Schema migration runs without errors.
```

This proves the task is done. Use concrete commands when possible.

## Optional Fields

### `self_evolution`

```yaml
self_evolution: |
  If the database driver doesn't support the enum type,
  add a new task to implement a string-based fallback with validation.
```

Guides what the worker should do if it discovers unexpected work. Only relevant in Discover and Generate modes.

### `depends`

```yaml
depends: [t-2, t-3]
```

Workers skip this task until all listed tasks are DONE. Supports multi-task dependencies (unlike the legacy `**Blocked by:**` field). Set via `boi spec <queue-id> block <t-id> --on <dep>`.

## Top-Level Fields

The spec file has several top-level fields before the tasks:

```yaml
title: Config Management CLI
mode: discover
context: |
  Build a CLI tool for managing YAML configuration files.
  Python 3.10+, stdlib only. Must work on Linux and macOS.
workspace: /path/to/worktree    # optional: pin to a specific worktree
blocked_by: [q-001]             # optional: wait for another spec

outcomes:
  - description: "CLI reads config files"
    verify: "python3 cli.py --config sample.yaml"

tasks:
  ...
```

The `mode` field sets the execution mode (overrides the `--mode` CLI flag).

## Generate Mode Specs

Generate mode uses a goal-only format. No pre-defined tasks required:

```yaml
title: Config Management CLI
mode: generate
context: |
  Build a CLI tool that reads, validates, and applies YAML configuration files
  with schema validation, environment variable interpolation, and dry-run mode.

constraints:
  - Python 3.10+, stdlib only
  - Must work on Linux and macOS

success_criteria:
  - CLI reads and parses YAML config files
  - Schema validation catches malformed configs
  - Environment variables are interpolated in config values
  - Dry-run mode shows what would change without applying
  - Help text is complete and accurate
  - Unit tests cover all core functions
```

A decomposition worker breaks the goal into 5-15 concrete tasks before execution begins. See [modes.md](modes.md) for details.

## Validation

BOI validates specs before dispatch. Invalid specs are rejected with clear error messages:

```bash
$ boi dispatch --spec broken-spec.yaml
Error: Task t-3 missing 'spec' field
Error: Task t-5 missing 'status' field
Spec validation failed: 2 errors
```

## Tips for Writing Good Specs

### Scope each task to one session

Each task should take 10-30 minutes for Claude to complete. If a task would require multiple sessions, split it into smaller tasks.

Bad:
```yaml
- id: t-1
  title: Build the entire API
```

Good:
```yaml
- id: t-1
  title: Set up the data model
- id: t-2
  title: Build the list endpoint
- id: t-3
  title: Build the create endpoint
- id: t-4
  title: Add input validation
- id: t-5
  title: Write tests
```

### Be explicit about context

Workers start with zero context. They only read the spec. Include file paths, function names, and patterns to follow.

Bad:
```yaml
spec: Add error handling to the API.
```

Good:
```yaml
spec: |
  Add error handling to `src/api/handlers.py`. Wrap the `create_user`
  function in a try/except. Catch `ValidationError` and return a 400 response.
  Catch `DatabaseError` and return a 500 response. Follow the pattern in
  `get_user` which already has error handling.
```

### Reference earlier tasks

If task t-3 depends on output from t-1, say so explicitly:

```yaml
- id: t-3
  title: Wire up the React component
  status: PENDING
  depends: [t-1, t-2]
  spec: |
    Create a `PreferencesPanel` component that calls the API mutation
    from t-2. Use the data model created in t-1 for TypeScript types.
```

### Add concrete verification

Give workers commands they can run to prove the task is done:

```yaml
verify: |
  python3 -m pytest tests/ -v
  python3 -m mypy src/
  # Endpoint returns 200 for valid input:
  curl -s -X POST http://localhost:8000/api/create -d '{"name":"test"}' | grep '"id"'
```

### Think about what could go wrong

Use the self_evolution field to guide workers when unexpected situations arise:

```yaml
self_evolution: |
  If the existing database schema uses a different ORM than expected,
  add a new task to create an adapter layer before proceeding with
  the API implementation.
```
