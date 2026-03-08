# Spec Format

A BOI spec is a Markdown file that defines ordered tasks for autonomous execution. Each task is self-contained: a worker reads the spec, executes the next PENDING task, marks it DONE, and exits. The spec file on disk is the single source of truth.

## Minimal Example

```markdown
# Feature Name

## Tasks

### t-1: First task
PENDING

**Spec:** What to do.

**Verify:** How to prove it worked.

### t-2: Second task
PENDING

**Spec:** What to do next.

**Verify:** How to verify.
```

## Task Structure

Every task requires four parts:

### 1. Heading

```markdown
### t-1: Descriptive task title
```

Rules:
- Three hashes (`###`)
- `t-` prefix followed by a sequential number
- Colon and space before the title
- Numbers must be sequential (t-1, t-2, t-3...)

### 2. Status line

The status must appear on its own line immediately after the heading:

```markdown
### t-1: Set up data model
PENDING
```

Valid statuses:

| Status | Meaning |
|--------|---------|
| `PENDING` | Not yet started. Workers pick this up. |
| `DONE` | Completed successfully. |
| `SKIPPED` | Intentionally bypassed (with reason). |
| `FAILED` | Attempted but could not complete. |
| `EXPERIMENT_PROPOSED` | Worker proposed an alternative approach (awaiting review). |
| `SUPERSEDED by t-N` | Replaced by a better task (Generate mode only). |

### 3. Spec section

```markdown
**Spec:** Create a `UserPreferences` model with fields: user_id (int),
theme (enum: light/dark), language (string), notifications_enabled (bool).
Follow the existing model patterns in `src/models/`.
```

This tells the worker exactly what to do. Be concrete:
- Name specific files, functions, and patterns
- Reference earlier tasks if there are dependencies
- Include enough context for a worker with zero prior knowledge

### 4. Verify section

```markdown
**Verify:** `python3 -m pytest tests/test_models.py -v` passes.
Schema migration runs without errors.
```

This proves the task is done. Use concrete commands when possible.

## Optional Sections

### Self-evolution

```markdown
**Self-evolution:** If the database driver doesn't support the enum type,
add a new task to implement a string-based fallback with validation.
```

Guides what the worker should do if it discovers unexpected work. Only relevant in Discover and Generate modes.

### Blocked by

```markdown
**Blocked by:** t-3
```

Workers skip this task until t-3 is DONE. Set via `boi spec <queue-id> block <t-id> --on <dep>`.

## Spec Header

The spec can include a header section before the tasks with metadata:

```markdown
# Config Management CLI

Build a CLI tool for managing YAML configuration files.

## Constraints
- Python 3.10+, stdlib only
- Must work on Linux and macOS

## Mode
Discover

## Tasks
...
```

The `## Mode` header sets the execution mode (overrides the `--mode` CLI flag).

## Generate Mode Specs

Generate mode uses a goal-only format. No pre-defined tasks required:

```markdown
# [Generate] Config Management CLI

## Goal
Build a CLI tool that reads, validates, and applies YAML configuration files
with schema validation, environment variable interpolation, and dry-run mode.

## Constraints
- Python 3.10+, stdlib only
- Must work on Linux and macOS

## Success Criteria
- [ ] CLI reads and parses YAML config files
- [ ] Schema validation catches malformed configs
- [ ] Environment variables are interpolated in config values
- [ ] Dry-run mode shows what would change without applying
- [ ] Help text is complete and accurate
- [ ] Unit tests cover all core functions
```

A decomposition worker breaks the goal into 5-15 concrete tasks before execution begins. See [modes.md](modes.md) for details.

## Validation

BOI validates specs before dispatch. Invalid specs are rejected with clear error messages:

```bash
$ boi dispatch --spec broken-spec.md
Error: Task t-3 missing **Spec:** section
Error: Task t-5 has no status line after heading
Spec validation failed: 2 errors
```

## Tips for Writing Good Specs

### Scope each task to one session

Each task should take 10-30 minutes for Claude to complete. If a task would require multiple sessions, split it into smaller tasks.

Bad:
```markdown
### t-1: Build the entire API
```

Good:
```markdown
### t-1: Set up the data model
### t-2: Build the list endpoint
### t-3: Build the create endpoint
### t-4: Add input validation
### t-5: Write tests
```

### Be explicit about context

Workers start with zero context. They only read the spec. Include file paths, function names, and patterns to follow.

Bad:
```markdown
**Spec:** Add error handling to the API.
```

Good:
```markdown
**Spec:** Add error handling to `src/api/handlers.py`. Wrap the `create_user`
function in a try/except. Catch `ValidationError` and return a 400 response.
Catch `DatabaseError` and return a 500 response. Follow the pattern in
`get_user` which already has error handling.
```

### Reference earlier tasks

If task t-3 depends on output from t-1, say so explicitly:

```markdown
### t-3: Wire up the React component
PENDING

**Spec:** Create a `PreferencesPanel` component that calls the API mutation
from t-2. Use the data model created in t-1 for TypeScript types.
```

### Add concrete verification

Give workers commands they can run to prove the task is done:

```markdown
**Verify:** `python3 -m pytest tests/ -v` passes. `python3 -m mypy src/` has
no errors. The new endpoint returns 200 for valid input and 400 for invalid
input (test with `curl`).
```

### Think about what could go wrong

Use the self-evolution section to guide workers when unexpected situations arise:

```markdown
**Self-evolution:** If the existing database schema uses a different ORM than
expected, add a new task to create an adapter layer before proceeding with
the API implementation.
```
