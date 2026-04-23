# YAML Spec Schema

BOI supports YAML as an alternative spec format. YAML specs are machine-parseable, strictly validated, and structurally equivalent to markdown specs. Both formats produce the same queue entries in boi.db.

## File Extension

Use `.yaml` or `.yml` extension. BOI detects format by extension:

- `.yaml` / `.yml` → YAML parser
- `.md` / `.spec.md` → markdown parser

## Top-Level Fields

| Field | Required | Type | Description |
|-------|----------|------|-------------|
| `title` | Yes | string | Human-readable spec title |
| `mode` | Yes | string | Execution mode: `execute`, `generate`, `challenge`, `discover` |
| `context` | No | string | Free-text background information for workers |
| `workspace` | No | string | Pin spec to a specific worktree path |
| `blocked_by` | No | list of strings | Queue IDs this spec depends on (e.g. `[q-001, q-002]`) |
| `tasks` | Yes | list of task objects | Ordered list of tasks |

## Task Object Fields

| Field | Required | Type | Description |
|-------|----------|------|-------------|
| `id` | Yes | string | Task identifier: `t-1`, `t-2`, etc. Must be unique. |
| `title` | Yes | string | Short description of what the task does |
| `status` | Yes | string | `PENDING`, `DONE`, `FAILED`, `SKIPPED` |
| `spec` | Yes | string | Full instructions for the worker (multi-line OK) |
| `verify` | Yes | string | Shell command or steps to prove the task is complete |
| `depends` | No | list of strings | Task IDs this task waits for (intra-spec DAG) |
| `self_evolution` | No | string | Guidance for what new tasks to add if unexpected work is discovered |

### Status Values

| Status | Meaning |
|--------|---------|
| `PENDING` | Not yet started. Workers pick this up. |
| `DONE` | Completed successfully. |
| `FAILED` | Attempted but could not complete. |
| `SKIPPED` | Intentionally bypassed. |

### `depends` Field

The `depends` field lists task IDs within the same spec that must be `DONE` before this task can run. This is equivalent to `**Blocked by:**` in markdown but supports multiple dependencies and enables future parallelism.

```yaml
depends: [t-1, t-2]
```

A task with `depends` is skipped until all listed tasks are `DONE`. Circular dependencies are a validation error.

## Full Schema (YAML)

```yaml
title: string           # required
mode: execute           # required: execute | generate | challenge | discover
context: |              # optional, free text
  Multi-line context
  about the spec.
workspace: /path        # optional
blocked_by:             # optional
  - q-001
tasks:                  # required, list of task objects
  - id: t-1             # required, unique
    title: string       # required
    status: PENDING     # required: PENDING | DONE | FAILED | SKIPPED
    spec: |             # required, multi-line instructions
      Worker instructions here.
    verify: |           # required, shell command or steps
      test -f output.txt
    depends: []         # optional, list of task IDs
    self_evolution: |   # optional, discovery guidance
      If X happens, add a task to handle Y.
```

---

## Example 1 — Simple Sequential Spec (3 tasks)

```yaml
title: Add user avatar upload
mode: execute
context: |
  The app uses FastAPI + SQLAlchemy. User model is in src/models/user.py.
  File uploads go to S3 via boto3. Tests use pytest with a mock S3 fixture.

tasks:
  - id: t-1
    title: Add avatar_url column to User model
    status: PENDING
    spec: |
      Add an `avatar_url` column (nullable String) to the User model in
      src/models/user.py. Create an Alembic migration. Run the migration
      against the dev database.
    verify: |
      python3 -m alembic upgrade head
      python3 -c "from src.models.user import User; print(User.avatar_url)"

  - id: t-2
    title: Build avatar upload endpoint
    status: PENDING
    spec: |
      Add POST /users/{id}/avatar to src/api/users.py. Accept multipart/form-data,
      upload to S3 bucket `app-avatars`, save the URL to User.avatar_url.
      Use the S3 upload pattern from src/storage/s3.py upload_file().
    verify: |
      curl -s -X POST http://localhost:8000/users/1/avatar \
        -F "file=@tests/fixtures/avatar.png" | grep avatar_url

  - id: t-3
    title: Write tests for avatar upload
    status: PENDING
    spec: |
      Add tests/test_avatar.py. Test the happy path (valid image), bad file type
      (expect 400), and missing file (expect 422). Mock S3 using the mock_s3
      fixture in tests/conftest.py.
    verify: |
      python3 -m pytest tests/test_avatar.py -v
```

**Equivalent markdown spec:**

```markdown
# Add user avatar upload

The app uses FastAPI + SQLAlchemy. User model is in src/models/user.py.
File uploads go to S3 via boto3. Tests use pytest with a mock S3 fixture.

## Tasks

### t-1: Add avatar_url column to User model
PENDING

**Spec:** Add an `avatar_url` column (nullable String) to the User model in
src/models/user.py. Create an Alembic migration. Run the migration
against the dev database.

**Verify:** `python3 -m alembic upgrade head` and
`python3 -c "from src.models.user import User; print(User.avatar_url)"` succeed.

### t-2: Build avatar upload endpoint
PENDING

**Spec:** Add POST /users/{id}/avatar to src/api/users.py. Accept multipart/form-data,
upload to S3 bucket `app-avatars`, save the URL to User.avatar_url.
Use the S3 upload pattern from src/storage/s3.py upload_file().

**Verify:** `curl -s -X POST http://localhost:8000/users/1/avatar -F "file=@tests/fixtures/avatar.png" | grep avatar_url`

### t-3: Write tests for avatar upload
PENDING

**Spec:** Add tests/test_avatar.py. Test the happy path (valid image), bad file type
(expect 400), and missing file (expect 422). Mock S3 using the mock_s3
fixture in tests/conftest.py.

**Verify:** `python3 -m pytest tests/test_avatar.py -v`
```

---

## Example 2 — Fan-Out Research Spec (with `depends`)

This spec has a gather phase (t-1), parallel research tasks (t-2, t-3, t-4 all depend only on t-1), and a synthesis task (t-5) that depends on all three.

```yaml
title: Research state management options for the iOS app
mode: discover
context: |
  The iOS app currently uses a mix of @StateObject and singletons. We're
  evaluating TCA, Redux-like patterns, and vanilla SwiftUI state for a
  rewrite. Workers should NOT make implementation decisions — research only.

tasks:
  - id: t-1
    title: Audit current state usage
    status: PENDING
    spec: |
      Search the codebase for @StateObject, @ObservedObject, @EnvironmentObject,
      and singleton patterns. Count usages per module. Write a summary to
      /tmp/state-audit.md listing: which modules use which patterns, total
      counts, and the 3 most complex state flows.
    verify: |
      test -f /tmp/state-audit.md && wc -l /tmp/state-audit.md | awk '$1 > 10'

  - id: t-2
    title: Research TCA (The Composable Architecture)
    status: PENDING
    depends: [t-1]
    spec: |
      Read the TCA README and docs at https://github.com/pointfreeco/swift-composable-architecture.
      Given the audit from t-1, evaluate TCA fit: boilerplate cost, testability,
      migration path from current patterns. Write findings to /tmp/tca-eval.md.
    verify: |
      test -f /tmp/tca-eval.md && grep -c 'pros\|cons\|migration' /tmp/tca-eval.md

  - id: t-3
    title: Research Redux-style patterns in Swift
    status: PENDING
    depends: [t-1]
    spec: |
      Evaluate ReSwift and similar Redux-style libraries for Swift. Given the
      audit from t-1, assess fit: boilerplate, SwiftUI integration, community
      health. Write findings to /tmp/redux-eval.md.
    verify: |
      test -f /tmp/redux-eval.md

  - id: t-4
    title: Research vanilla SwiftUI state scaling
    status: PENDING
    depends: [t-1]
    spec: |
      Research how teams scale vanilla SwiftUI state (@Observable, @State,
      environment) beyond simple apps. Find 2-3 documented approaches from
      WWDC talks or well-regarded blog posts. Write findings to /tmp/swiftui-eval.md.
    verify: |
      test -f /tmp/swiftui-eval.md

  - id: t-5
    title: Synthesize findings into recommendation report
    status: PENDING
    depends: [t-2, t-3, t-4]
    spec: |
      Read /tmp/tca-eval.md, /tmp/redux-eval.md, and /tmp/swiftui-eval.md.
      Write a decision document to ~/mrap-hex/me/decisions/ios-state-management-2026-04-22.md.
      Include: option comparison table, recommendation with rationale, migration
      cost estimate, and top 3 risks.
    verify: |
      test -f ~/mrap-hex/me/decisions/ios-state-management-2026-04-22.md && \
      grep -c 'recommendation\|risk' ~/mrap-hex/me/decisions/ios-state-management-2026-04-22.md
```

In this spec, t-2, t-3, and t-4 can run in any order (or in parallel in future BOI versions) because they all only `depends` on t-1. Task t-5 waits for all three to finish.

---

## Comparison: YAML vs Markdown

| Feature | YAML | Markdown |
|---------|------|----------|
| Machine-parseable | Yes — standard YAML libraries | Fragile — regex/line parsing |
| Validation errors | Typed, schema-level | "Missing blank line after heading" |
| `depends` / DAG | Native list field | `**Blocked by:**` (single task only) |
| Multi-line text | YAML block scalars (`|`) | Natural but requires blank-line discipline |
| Human-readable | Good (more structured) | Best (prose-first) |
| `boi dispatch` | `.yaml` / `.yml` extension | `.md` / `.spec.md` extension |
| Migration | Gradual — both formats work | Existing format, fully supported |

Both formats produce identical queue entries in boi.db. Choose based on preference or tooling needs.
