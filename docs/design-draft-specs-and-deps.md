---
Title: Draft Specs and Dependency Chains
Author: BOI Worker
Date: 2026-03-11
Status: Draft
---

# Draft Specs and Dependency Chains

## 1. Executive Summary

This document designs two interrelated features for BOI: **draft specs** and **spec-level dependency chains**. Currently, every dispatched spec is immediately eligible for execution, which prevents users from planning ahead, iterating on specs before they're ready, or expressing sequential relationships between specs (e.g., "run the implementation spec only after the research spec completes"). The design introduces `draft` as a new status in the existing `specs` table state machine — a draft is a fully tracked spec (has a queue ID, visible in `boi queue`, editable via `boi spec edit`) that the daemon's `pick_next_spec()` will never select until explicitly promoted by the user via `boi promote`. For dependencies, the design surfaces the existing but CLI-unexposed `spec_dependencies` table through new flags (`boi dispatch --after q-003`) and commands (`boi dep add/remove`, `boi deps`), with DFS-based circular dependency detection at insertion time. Key design decisions: (1) drafts live in the same `specs` table as regular specs (not in a separate table or filesystem), keeping one ID namespace and one query surface; (2) dependencies are hard constraints while priority only orders eligible specs; (3) failed/canceled dependencies block dependents until manually resolved, with warnings surfaced in `boi status`; (4) the SQLite CHECK constraint on `specs.status` is dropped in favor of application-level validation for easier future extensibility. Three alternative approaches were evaluated — filesystem drafts, tag-based states, and a separate drafts table — and rejected in favor of the in-database status approach for its simplicity and integration. The implementation spans ~350 lines of Python and ~90 lines of bash across three independently deployable phases, with full backward compatibility and simple rollback paths.

## 2. Current State Analysis

### 2.1 Current Spec State Machine

BOI manages spec lifecycle through an 8-value status field in the `specs` table. The CHECK constraint at `~/.boi/src/lib/schema.sql` line 39 defines:

```sql
CHECK (status IN ('queued','assigning','running','completed','failed','canceled','needs_review','requeued'))
```

The state transitions form this diagram:

```
                         ┌──────────────────┐
                         │                  │
                         ▼                  │
 ┌─────────┐   ┌────────────┐   ┌─────────┐│  ┌───────────┐
 │ queued   │──▶│ assigning  │──▶│ running ││  │ canceled  │
 └─────────┘   └────────────┘   └─────────┘│  └───────────┘
      ▲                              │      │       ▲
      │                              │      │       │
      │                    ┌─────────┼──────┘   (user action)
      │                    │         │
      │                    ▼         ▼
 ┌──────────┐   ┌──────────────┐  ┌────────┐
 │ requeued │   │ needs_review │  │ failed │
 └──────────┘   └──────────────┘  └────────┘
      │                │
      │                ▼
      │         ┌────────────┐
      └────────▶│ completed  │
                └────────────┘
```

Key transitions:
- **queued → assigning**: `pick_next_spec()` selects the spec (`db.py` line 437)
- **assigning → running**: Worker confirms assignment (`db.py`)
- **running → completed/failed/needs_review**: Worker finishes iteration
- **running → requeued**: More iterations needed
- **requeued → assigning**: `pick_next_spec()` re-selects (treats `requeued` same as `queued`)
- **any → canceled**: User runs `boi cancel`

### 2.2 Existing Dependency Infrastructure

BOI already has a dependency system at the database level — it's just not exposed through the CLI.

**`spec_dependencies` table** (`schema.sql` lines 43-50):
```sql
CREATE TABLE IF NOT EXISTS spec_dependencies (
    spec_id TEXT NOT NULL,
    blocks_on TEXT NOT NULL,
    PRIMARY KEY (spec_id, blocks_on),
    FOREIGN KEY (spec_id) REFERENCES specs(id) ON DELETE CASCADE,
    FOREIGN KEY (blocks_on) REFERENCES specs(id) ON DELETE CASCADE
);
```

**`pick_next_spec()` dependency check** (`db.py` lines 473-490):
The method already queries `spec_dependencies` for each candidate spec. It checks that every `blocks_on` target has `status = 'completed'`. If any dependency is not completed, the spec is skipped:

```python
deps = self.conn.execute(
    "SELECT blocks_on FROM spec_dependencies WHERE spec_id = ?", (sid,)
).fetchall()
if deps:
    all_done = True
    for dep in deps:
        dep_row = self.conn.execute(
            "SELECT status FROM specs WHERE id = ?", (dep["blocks_on"],)
        ).fetchone()
        if dep_row is None or dep_row["status"] != "completed":
            all_done = False
            break
    if not all_done:
        continue
```

**`enqueue()` accepts `blocked_by`** (`db.py` lines 153-240):
The `enqueue()` method signature includes `blocked_by: Optional[list[str]] = None`. When provided, it inserts rows into `spec_dependencies` (lines 234-240):

```python
if blocked_by:
    for dep_id in blocked_by:
        self.conn.execute(
            "INSERT INTO spec_dependencies (spec_id, blocks_on) VALUES (?, ?)",
            (queue_id, dep_id),
        )
```

**Task-level `Blocked by:` parsing** (`boi.sh` lines 2630-2635):
BOI already parses `**Blocked by:**` headers in *task* bodies (within specs) to show blocked tasks in `boi spec list`. This establishes a precedent for the `**Blocked-By:**` header format at the spec level.

### 2.3 Gap Analysis

Despite the existing database infrastructure, the following gaps prevent practical use of drafts and dependencies:

| Gap | Description | Impact |
|-----|-------------|--------|
| **No `draft` status** | The CHECK constraint only allows 8 statuses; `draft` is not among them. Specs must be immediately runnable on dispatch. | Users can't park specs for iteration |
| **No `--blocked-by` CLI flag** | `cmd_dispatch()` in `boi.sh` (line 246) does not expose the `blocked_by` parameter. The `cli_ops.dispatch()` function (line 30) doesn't accept it either. | Users can't declare deps at dispatch time |
| **No `--draft` CLI flag** | `cmd_dispatch()` has no draft mode — every dispatch immediately sets `status = 'queued'` | No way to dispatch without immediate execution eligibility |
| **No post-dispatch dep management** | No `boi dep add` / `boi dep remove` commands exist. Once dispatched, deps can't be modified. | Rigid, non-iterative workflow |
| **No dep visualization** | No `boi deps <id>` command. `boi queue` and `boi status` don't show dependency info for specs. | Users can't see what blocks what |
| **No promote/demote commands** | No way to transition between draft and queued states | Missing lifecycle management |
| **No cycle detection** | `enqueue()` inserts dep edges without checking for circular dependencies | Risk of deadlocked specs |
| **No dep status in queue display** | `format_queue_table()` in `status.py` (line 509) doesn't show dependency info per spec | Users can't see blocked/blocking relationships |

### 2.4 What Exists vs. What's Needed

| Layer | Exists | Needs Building |
|-------|--------|----------------|
| **Schema** | `spec_dependencies` table with FK cascades | Add `'draft'` to CHECK constraint; add index on `spec_dependencies` |
| **DB logic** | `pick_next_spec()` checks deps; `enqueue()` accepts `blocked_by` | `promote()`, `demote()`, `add_dependency()`, `remove_dependency()`, `detect_cycle()`, `get_dependency_chain()` |
| **CLI ops** | `dispatch()` in `cli_ops.py` | Extend with `as_draft`, add dep management functions |
| **Shell** | `cmd_dispatch()` with flag parsing | Add `--draft`, `--after` flags; add `promote`, `demote`, `dep`, `deps` subcommands |
| **Display** | `format_queue_table()`, status dashboard | Show draft label, dependency info, blocked-by warnings |

## 3. Draft Lifecycle Design

### 3.1 Extended State Machine

The new state machine adds `draft` as an entry point before `queued`. The `draft` state is a parking lot: the spec is tracked in the database (has a queue ID, appears in `boi queue`) but the daemon will never select it for execution.

```
                                      ┌──────────────────────┐
                                      │                      │
  ┌─────────┐   promote   ┌─────────┐│  ┌────────────┐   ┌─────────┐
  │  draft   │────────────▶│ queued  ││─▶│ assigning  │──▶│ running │
  └─────────┘              └─────────┘│  └────────────┘   └─────────┘
       ▲          demote        ▲     │                      │  │  │
       └────────────────────────┘     │               ┌──────┘  │  │
                                      │               │         │  │
                                 ┌──────────┐         │         │  │
                                 │ requeued │◀────────┘         │  │
                                 └──────────┘                   │  │
                                                     ┌──────────┘  │
                                                     │             │
                                                     ▼             ▼
                                              ┌────────────┐  ┌────────┐
                                              │needs_review│  │ failed │
                                              └────────────┘  └────────┘
                                                     │
                                                     ▼
                                              ┌────────────┐
                                              │ completed  │
                                              └────────────┘

  (any state) ──── boi cancel ────▶ [canceled]
```

All transitions:

| From | To | Trigger | Notes |
|------|----|---------|-------|
| *(new)* | **draft** | `boi dispatch spec.md --draft` | Spec is registered but not eligible for execution |
| **draft** | **queued** | `boi promote <queue-id>` | Explicit user action; deps are validated (targets must exist) |
| **queued** | **draft** | `boi demote <queue-id>` | Only if status is `queued` (not `assigning` or later) |
| **queued** | **assigning** | `pick_next_spec()` | Unchanged — daemon selects spec for a worker |
| **assigning** | **running** | Worker confirms assignment | Unchanged |
| **running** | **completed** | Worker finishes successfully | Unchanged |
| **running** | **failed** | Worker reports failure | Unchanged |
| **running** | **needs_review** | Critic flags for review | Unchanged |
| **running** | **requeued** | More iterations needed | Unchanged |
| **requeued** | **assigning** | `pick_next_spec()` re-selects | Unchanged |
| **needs_review** | **completed** | Review passes | Unchanged |
| *any* | **canceled** | `boi cancel <queue-id>` | Unchanged |

### 3.2 Draft Semantics

A draft spec has the following properties:

1. **Tracked in the database.** It has a queue ID (e.g., `q-047`), a row in the `specs` table, and appears in `boi queue` output with a `[draft]` label.
2. **Invisible to the daemon.** `pick_next_spec()` queries for specs with `status IN ('queued', 'requeued')`. Since drafts have `status = 'draft'`, they are automatically excluded — no code change needed in `pick_next_spec()`.
3. **Fully editable.** Users can iterate on the spec file, edit tasks (`boi spec edit`), change priority (`boi reprioritize`), add/remove dependencies (`boi dep add/remove`), and modify the spec header — all without risk of the daemon picking it up.
4. **Mode-agnostic.** Drafts work with all execution modes: `execute`, `challenge`, `discover`, `generate`. The mode is stored on dispatch and carries over on promotion.
5. **Priority preserved.** A draft's priority is set at dispatch time (via `--priority` flag or default 100). When promoted, the priority carries over unchanged.
6. **Project-aware.** If dispatched with `--project`, the project association is stored immediately and persists through promotion.

### 3.3 Promotion: draft → queued

Promotion is the explicit act of marking a draft as ready for execution.

**Command:** `boi promote <queue-id>`

**Behavior:**
1. Validate the spec exists and has `status = 'draft'`. Error if not a draft.
2. Validate all declared dependencies exist in the `specs` table (the dep targets must be real queue IDs). Error with a list of missing dep IDs if any are invalid.
3. Update `status` from `'draft'` to `'queued'`.
4. The spec is now eligible for `pick_next_spec()` — subject to priority ordering and dependency satisfaction (all `blocks_on` targets must be `completed`).

**What promotion does NOT do:**
- It does not change priority.
- It does not modify dependencies.
- It does not validate that dependencies are completable (a dep could be `failed` — that's handled by the blocked-spec warning system, not promotion).

### 3.4 Demotion: queued → draft

Demotion pulls a spec back to draft status, useful when you realize a spec isn't ready after promotion.

**Command:** `boi demote <queue-id>`

**Behavior:**
1. Validate the spec exists and has `status = 'queued'`. Error if the spec is in any other state.
2. Specifically reject demotion for `assigning` (worker is being assigned — too late) and `running` (actively executing).
3. Update `status` from `'queued'` to `'draft'`.
4. The spec immediately becomes invisible to `pick_next_spec()`.

**Why only from `queued`?**
- `assigning` → The daemon has already started the assignment handshake with a worker. Pulling it back would leave the worker in an inconsistent state.
- `running` → The spec is actively being worked on. Cancellation (`boi cancel`) is the right tool here.
- `requeued` → The spec has already been partially executed. Demoting to draft doesn't make sense — the user should cancel and re-dispatch if they want to start over.
- `completed`/`failed`/`needs_review`/`canceled` → Terminal or near-terminal states. Demoting would be confusing.

### 3.5 Draft + Dependencies

Drafts interact with the dependency system in two ways:

**A draft can declare dependencies.** When you dispatch with `--draft --after q-003`, the spec is created as a draft with a dep edge to q-003. The dep is stored in `spec_dependencies` immediately. When the draft is later promoted, it enters `queued` but `pick_next_spec()` won't select it until q-003 is `completed`.

**A spec can depend on a draft.** If q-007 depends on q-005, and q-005 is a draft, then q-007 is effectively double-blocked: q-005 must be both *promoted* (so it can run) and *completed* (so the dep is satisfied). `pick_next_spec()` already handles this correctly — it checks `status = 'completed'`, and a draft is never `completed`.

**Dependency validation at promotion time:** When promoting a draft, we validate that all `blocks_on` targets exist in the `specs` table. This catches stale references (e.g., a dep on q-999 that was purged). However, we do NOT require deps to be in a "healthy" state — a dep on a `failed` spec is allowed (the user can retry it or remove the dep).

### 3.6 Schema Change

The only schema change is adding `'draft'` to the CHECK constraint on `specs.status`:

```sql
-- Before:
CHECK (status IN ('queued','assigning','running','completed','failed','canceled','needs_review','requeued'))

-- After:
CHECK (status IN ('draft','queued','assigning','running','completed','failed','canceled','needs_review','requeued'))
```

No new tables or columns are needed. The existing `spec_dependencies` table handles all dependency relationships, and the `specs` table already has all necessary columns (priority, project, mode, etc.).

## 4. Dependency Declaration

Dependencies express "spec B should not start until spec A completes." BOI already has the database infrastructure for this (`spec_dependencies` table, `pick_next_spec()` checks, `enqueue(blocked_by=...)`) — what's missing is a way for users to declare and manage dependencies. This section designs three complementary declaration mechanisms: CLI flags at dispatch time, spec header metadata, and post-dispatch mutation commands.

### 4.1 CLI Flag Approach

**Flag:** `--after <queue-id>[,<queue-id>,...]`

The `--after` flag declares dependencies at dispatch time, mapping directly to the existing `blocked_by` parameter in `db.enqueue()`.

**Syntax:**
```bash
# Single dependency
boi dispatch spec.md --after q-003

# Multiple dependencies (comma-separated, no spaces)
boi dispatch spec.md --after q-003,q-005

# Combined with draft mode
boi dispatch spec.md --draft --after q-003

# Combined with priority
boi dispatch spec.md --after q-003 --priority 50
```

**Why `--after` instead of `--blocked-by`?**

Both names are valid, but `--after` is recommended as the primary flag because:
- It reads naturally: "dispatch this spec, run it *after* q-003"
- It's shorter and easier to type
- It matches the mental model of sequencing ("do this after that")
- `--blocked-by` is technically accurate but sounds more mechanical

We support `--blocked-by` as an undocumented alias for users who think in dependency terms, but `--after` is the documented interface.

**Implementation in `boi.sh`:**
```bash
# In cmd_dispatch(), add flag parsing:
local after_ids=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --after|--blocked-by)
            after_ids="$2"
            shift 2
            ;;
        # ... existing flags ...
    esac
done

# Pass to Python CLI ops:
if [[ -n "$after_ids" ]]; then
    python3 -c "
from lib.cli_ops import dispatch
dispatch('$spec_path', blocked_by='$after_ids'.split(','))
"
fi
```

**Implementation in `cli_ops.py`:**
```python
def dispatch(
    spec_path: str,
    priority: int = 100,
    blocked_by: Optional[list[str]] = None,
    as_draft: bool = False,
    # ... existing params ...
) -> dict:
    # Validate dep targets exist
    if blocked_by:
        for dep_id in blocked_by:
            if not db.spec_exists(dep_id):
                raise ValueError(f"Dependency target '{dep_id}' does not exist")
    # Pass through to db.enqueue()
    return db.enqueue(spec_path, blocked_by=blocked_by, ...)
```

### 4.2 Spec Header Approach

**Header line:** `**Blocked-By:** q-003, q-005`

Dependencies can be declared in the spec file's metadata header, alongside existing header fields like `**Mode:**` and `**Target repo:**`. The dispatch flow parses this header and passes it to `enqueue()`.

**Format:**
```markdown
# [Execute] My Feature Spec

**Mode:** execute
**Blocked-By:** q-003, q-005

## Goal
...
```

**Parsing rules:**
- The header line must match the pattern `**Blocked-By:**` (case-insensitive for the value)
- Queue IDs are comma-separated with optional whitespace: `q-003, q-005` or `q-003,q-005`
- Queue IDs must match the pattern `q-\d+`
- The header line is optional — absence means no dependencies
- Multiple `**Blocked-By:**` lines are not supported; use a single comma-separated line

**Why this format?**

BOI already establishes a precedent for `**Keyword:** value` header metadata:
- `**Mode:** execute` (spec execution mode)
- `**Target repo:** ~/hex` (target repository)
- `**Blocked by:** t-X` (task-level blocking within a spec, in `boi.sh` line 2630)

Using `**Blocked-By:**` at the spec level is consistent with this pattern. The hyphen distinguishes spec-level blocking (`Blocked-By`) from task-level blocking (`Blocked by`), though in practice they appear in different contexts (spec header vs. task body) so confusion is unlikely.

**Implementation — spec header parsing:**
```python
import re

def parse_spec_dependencies(spec_content: str) -> list[str]:
    """Extract Blocked-By queue IDs from spec header metadata."""
    for line in spec_content.splitlines():
        match = re.match(
            r'\*\*Blocked-By:\*\*\s*(.+)',
            line, re.IGNORECASE
        )
        if match:
            raw = match.group(1).strip()
            ids = [qid.strip() for qid in raw.split(',')]
            # Validate format
            for qid in ids:
                if not re.match(r'^q-\d+$', qid):
                    raise ValueError(
                        f"Invalid queue ID in Blocked-By header: '{qid}'"
                    )
            return ids
    return []
```

### 4.3 CLI + Header Merging Strategy

When both a CLI `--after` flag and a spec header `**Blocked-By:**` are present, the dependencies are **merged** (union), not overridden.

**Example:**
```bash
# Spec header contains: **Blocked-By:** q-003
# CLI flag adds: --after q-005
boi dispatch spec.md --after q-005
# Result: spec depends on BOTH q-003 AND q-005
```

**Rationale for merging vs. overriding:**
- Merging is additive and safe — you never accidentally lose a dependency
- The spec header represents the author's intent; the CLI flag represents the dispatcher's additional context
- If a user truly wants to ignore the spec header, they can edit the spec file to remove the `**Blocked-By:**` line before dispatching

**Implementation:**
```python
def resolve_dependencies(
    spec_content: str,
    cli_blocked_by: Optional[list[str]] = None
) -> list[str]:
    """Merge dependencies from spec header and CLI flag."""
    header_deps = parse_spec_dependencies(spec_content)
    cli_deps = cli_blocked_by or []
    # Union, preserving order (header first, then CLI additions)
    seen = set()
    merged = []
    for dep_id in header_deps + cli_deps:
        if dep_id not in seen:
            seen.add(dep_id)
            merged.append(dep_id)
    return merged
```

### 4.4 Post-Dispatch Mutation

After a spec is dispatched (whether as draft or queued), users can add or remove dependencies using the `boi dep` subcommand. This is essential for iterative workflows — you don't always know all dependencies at dispatch time.

**Add a dependency:**
```bash
boi dep add <spec-id> --on <dep-id>

# Examples:
boi dep add q-007 --on q-003          # q-007 now waits for q-003
boi dep add q-007 --on q-003,q-005    # Add multiple deps at once
```

**Remove a dependency:**
```bash
boi dep remove <spec-id> --on <dep-id>

# Examples:
boi dep remove q-007 --on q-003       # q-007 no longer waits for q-003
boi dep remove q-007 --on q-003,q-005 # Remove multiple deps at once
```

**Implementation — `db.py` methods:**
```python
def add_dependency(self, spec_id: str, blocks_on: str) -> None:
    """Add a dependency edge: spec_id is blocked by blocks_on.

    Validates:
    1. Both spec_id and blocks_on exist in the specs table
    2. spec_id is not in a terminal/running state
    3. Adding this edge would not create a circular dependency
    """
    with self.lock:
        # Validate both specs exist
        for sid in (spec_id, blocks_on):
            row = self.conn.execute(
                "SELECT id, status FROM specs WHERE id = ?", (sid,)
            ).fetchone()
            if row is None:
                raise ValueError(f"Spec '{sid}' does not exist")

        # Cannot add deps to running/completed specs
        spec_row = self.conn.execute(
            "SELECT status FROM specs WHERE id = ?", (spec_id,)
        ).fetchone()
        immutable = ('running', 'assigning', 'completed', 'failed',
                     'canceled', 'needs_review')
        if spec_row["status"] in immutable:
            raise ValueError(
                f"Cannot add dependency to spec '{spec_id}' "
                f"in '{spec_row['status']}' state"
            )

        # Circular dependency detection
        if self.detect_cycle(spec_id, blocks_on):
            cycle_path = self._find_cycle_path(spec_id, blocks_on)
            raise ValueError(
                f"Circular dependency detected: {cycle_path}"
            )

        # Insert edge (ignore if already exists)
        self.conn.execute(
            "INSERT OR IGNORE INTO spec_dependencies "
            "(spec_id, blocks_on) VALUES (?, ?)",
            (spec_id, blocks_on),
        )
        self.conn.commit()

def remove_dependency(self, spec_id: str, blocks_on: str) -> None:
    """Remove a dependency edge."""
    with self.lock:
        cursor = self.conn.execute(
            "DELETE FROM spec_dependencies "
            "WHERE spec_id = ? AND blocks_on = ?",
            (spec_id, blocks_on),
        )
        if cursor.rowcount == 0:
            raise ValueError(
                f"No dependency from '{spec_id}' on '{blocks_on}'"
            )
        self.conn.commit()
```

**Which states allow dependency mutation?**

| Spec Status | Add Dep | Remove Dep | Rationale |
|-------------|---------|------------|-----------|
| `draft` | ✅ | ✅ | Drafts are fully editable |
| `queued` | ✅ | ✅ | Not yet picked up by daemon |
| `requeued` | ✅ | ✅ | Between iterations, safe to modify |
| `assigning` | ❌ | ❌ | Daemon is actively assigning — race condition risk |
| `running` | ❌ | ❌ | Actively executing — deps are already resolved |
| `completed` | ❌ | ❌ | Terminal state — deps are moot |
| `failed` | ❌ | ❌ | Terminal state |
| `canceled` | ❌ | ❌ | Terminal state |
| `needs_review` | ❌ | ❌ | Awaiting review — not user-editable |

### 4.5 Validation Rules

All dependency operations (dispatch `--after`, spec header parsing, `boi dep add`) enforce these validation rules:

**Rule 1: Dependency target must exist.**
```
$ boi dispatch spec.md --after q-999
Error: Dependency target 'q-999' does not exist.
```

**Rule 2: No circular dependencies.**

Circular dependency detection uses DFS traversal of the `spec_dependencies` graph. Before inserting an edge `A → B` (A is blocked by B), we check whether B can reach A through existing edges. If it can, inserting `A → B` would create a cycle.

```python
def detect_cycle(self, spec_id: str, blocks_on: str) -> bool:
    """Return True if adding spec_id->blocks_on would create a cycle.

    Uses DFS: starting from blocks_on, follow the 'blocks_on' edges.
    If we can reach spec_id, there's a cycle.
    """
    visited = set()
    stack = [blocks_on]
    while stack:
        current = stack.pop()
        if current == spec_id:
            return True  # Cycle detected
        if current in visited:
            continue
        visited.add(current)
        # Follow edges: what does 'current' depend on?
        deps = self.conn.execute(
            "SELECT blocks_on FROM spec_dependencies "
            "WHERE spec_id = ?", (current,)
        ).fetchall()
        for dep in deps:
            stack.append(dep["blocks_on"])
    return False
```

**Cycle path reconstruction** (for error messages):
```python
def _find_cycle_path(self, spec_id: str, blocks_on: str) -> str:
    """Return a human-readable cycle path string.

    E.g., 'q-003 → q-005 → q-003'
    """
    # BFS to find the path from blocks_on back to spec_id
    from collections import deque
    queue = deque([(blocks_on, [blocks_on])])
    visited = set()
    while queue:
        current, path = queue.popleft()
        if current == spec_id:
            full_path = [spec_id] + path
            return " → ".join(full_path)
        if current in visited:
            continue
        visited.add(current)
        deps = self.conn.execute(
            "SELECT blocks_on FROM spec_dependencies "
            "WHERE spec_id = ?", (current,)
        ).fetchall()
        for dep in deps:
            queue.append((dep["blocks_on"], path + [dep["blocks_on"]]))
    return f"{spec_id} → {blocks_on} → ... → {spec_id}"
```

**Rule 3: Cannot add dependencies to immutable specs.**

Specs in `running`, `assigning`, `completed`, `failed`, `canceled`, or `needs_review` states cannot have dependencies added or removed. This prevents race conditions with the daemon and avoids modifying specs that have already completed their lifecycle.

```
$ boi dep add q-003 --on q-005    # q-003 is running
Error: Cannot add dependency to spec 'q-003' in 'running' state.
```

**Rule 4: Self-dependency is rejected as circular.**

`boi dep add q-003 --on q-003` is a trivial cycle and is rejected by the same circular dependency detection logic (DFS from q-003 immediately finds q-003).

```
$ boi dep add q-003 --on q-003
Error: Circular dependency detected: q-003 → q-003
```

### 4.6 Recommendation Summary

| Mechanism | When to Use | Pros | Cons |
|-----------|-------------|------|------|
| **CLI `--after`** | At dispatch time when you know deps | Fast, explicit, scriptable | Only available at dispatch |
| **Spec header `**Blocked-By:**`** | When authoring specs | Self-documenting, travels with spec | Requires spec file editing |
| **`boi dep add/remove`** | After dispatch, iterative workflows | Flexible, works on drafts and queued | Extra command to learn |

**Primary recommendation:** Support all three. They are complementary, not competing:
- Spec header is the *declarative* source — it documents intent in the spec itself
- CLI `--after` is the *imperative* shortcut — quick and scriptable
- `boi dep add/remove` is the *mutation* interface — essential for iterative workflows

CLI and header deps are merged (union) at dispatch time. Post-dispatch mutations directly modify the `spec_dependencies` table. All three paths enforce the same validation rules (existence, cycle detection, state constraints).

## 5. CLI UX Proposals

This section provides concrete CLI command designs for every draft and dependency workflow. Each command follows BOI's existing patterns: flag parsing via `while/case`, output via `info`/`warn`/`die`/`progress_step`, Python bridge via inline heredoc with JSON on stdout, and `--help` on every command.

### 5.1 Creating a Draft: `boi dispatch --draft`

**Syntax:**
```
boi dispatch <spec.md> --draft [--priority N] [--after <queue-id>[,...]] [--project <name>] [--mode <mode>]
```

The `--draft` flag is a boolean flag. When present, the spec is enqueued with `status = 'draft'` instead of `'queued'`. All other dispatch flags work unchanged — priority, mode, project, after, max-iter, etc.

**Example usage:**
```bash
# Basic draft dispatch
boi dispatch feature-spec.md --draft

# Draft with priority and dependencies
boi dispatch feature-spec.md --draft --priority 50 --after q-003

# Draft with project association
boi dispatch feature-spec.md --draft --project myapp
```

**Expected output (success):**
```
boi: Dispatched as draft
  Spec: feature-spec.md
  Queue ID: q-048
  Mode: execute
  Priority: 100
  Tasks: 5 pending

  This spec will NOT run until promoted. Use 'boi promote q-048' when ready.
```

**Expected output (success with deps):**
```
boi: Dispatched as draft
  Spec: feature-spec.md
  Queue ID: q-048
  Mode: execute
  Priority: 50
  Tasks: 5 pending
  Blocked by: q-003

  This spec will NOT run until promoted. Use 'boi promote q-048' when ready.
```

**Error cases:**
```
# File not found
Error: Spec file 'missing.md' does not exist.

# Duplicate spec
Error: Spec 'feature-spec.md' is already active as q-045 (running).

# Invalid dep target
Error: Dependency target 'q-999' does not exist.
```

**Bash implementation sketch:**
```bash
# In cmd_dispatch(), add to flag parsing:
local as_draft=false
local after_ids=""

# In the while loop:
    --draft)
        as_draft=true; shift ;;
    --after|--blocked-by)
        [[ -z "${2:-}" ]] && die_usage "--after requires queue ID(s)"
        after_ids="$2"; shift 2 ;;
```

**Python bridge addition:**
```bash
# Pass draft flag and after_ids to Python:
result=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${input_file}" "${QUEUE_DIR}" \
    "${priority}" "${max_iter}" "${as_draft}" "${after_ids}" ... <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.cli_ops import dispatch

as_draft = sys.argv[5] == "true"
after_ids = [x.strip() for x in sys.argv[6].split(",") if x.strip()] if sys.argv[6] else []

result = dispatch(
    spec_path=sys.argv[1],
    queue_dir=sys.argv[2],
    priority=int(sys.argv[3]),
    as_draft=as_draft,
    blocked_by=after_ids or None,
    ...
)
print(json.dumps(result))
PYEOF
)
```

**Post-dispatch output (draft-specific):**
```bash
# After parsing the JSON result:
if [[ "${as_draft}" == "true" ]]; then
    info "Dispatched as draft"
    echo "  Spec: ${spec_file}"
    echo "  Queue ID: ${queue_id}"
    echo "  Mode: ${mode}"
    echo "  Priority: ${priority}"
    echo "  Tasks: ${pending_count} pending"
    if [[ -n "${after_ids}" ]]; then
        echo "  Blocked by: ${after_ids}"
    fi
    echo ""
    echo -e "  This spec will NOT run until promoted. Use '${BOLD}boi promote ${queue_id}${NC}' when ready."
else
    # ... existing non-draft output ...
fi
```

### 5.2 Listing Drafts: `boi queue` Enhancements

Drafts appear inline in `boi queue` output with a `[draft]` label. No separate `--drafts` filter is needed for v1 — drafts are visible in the normal queue listing, distinguished by their status label and dim coloring.

**Enhanced `boi queue` output:**
```
SPEC                           MODE      WORKER  ITER    TASKS        STATUS
───────────────────────────────────────────────────────────────────────────────
q-003  eval-system              execute   w-1     5/30    3/8 done     running
q-005  api-refactor             execute   w-2     2/30    1/5 done     running
q-007  feature-pipeline         execute   —       —       0/6 done     queued ← q-003
q-008  ux-polish                generate  —       —       0/4 done     [draft]
q-009  perf-optimization        execute   —       —       0/3 done     [draft] ← q-003, q-005

Workers: 2/3 busy  |  2 running, 1 queued, 2 drafts
```

**Key changes to the queue table:**

1. **Status column for drafts**: Shows `[draft]` in DIM cyan instead of the normal status. The brackets visually separate drafts from active statuses.

2. **Dependency indicator**: When a spec has unmet dependencies, a `← q-XXX` suffix appears after the status, showing what it's blocked by. Multiple deps are comma-separated. The dep's current status can optionally appear in parentheses for the first dep: `← q-003 (running)`.

3. **Summary line**: Updated to include draft count: `2 running, 1 queued, 2 drafts`.

4. **Color rules update in `status.py`:**
```python
STATUS_COLORS: dict[str, str] = {
    "completed":    GREEN,
    "running":      YELLOW,
    "queued":       DIM,
    "requeued":     YELLOW,
    "failed":       RED,
    "canceled":     DIM,
    "needs_review": MAGENTA,
    "draft":        CYAN,   # New — drafts in cyan to visually separate
}
```

5. **Draft status display:** Drafts show `—` for WORKER and ITER columns (no worker assigned, no iterations started). TASKS shows the count from the spec file.

**Optional filter flag (v2):**
```
boi queue --filter drafts      # Show only drafts
boi queue --filter active      # Exclude drafts and terminal states
```

This extends the existing `--filter` pattern from `boi status`. Not needed for v1.

### 5.3 Promoting a Draft: `boi promote`

**Syntax:**
```
boi promote <queue-id> [<queue-id> ...]
```

Promotes one or more drafts from `draft` to `queued` status. Supports multiple IDs for batch promotion.

**Example usage:**
```bash
# Single promotion
boi promote q-008

# Batch promotion
boi promote q-008 q-009
```

**Expected output (success):**
```
boi: Promoted q-008 to queued
  Spec: ux-polish
  Priority: 100
  Dependencies: none
  Ready for execution.
```

**Expected output (success with deps):**
```
boi: Promoted q-009 to queued
  Spec: perf-optimization
  Priority: 100
  Dependencies: q-003 (running), q-005 (running)
  Blocked until dependencies complete.
```

**Expected output (batch):**
```
boi: Promoted q-008 to queued — ready for execution
boi: Promoted q-009 to queued — blocked by q-003, q-005
```

**Error cases:**
```
# Not a draft
Error: Spec 'q-003' is not a draft (status: running). Only drafts can be promoted.

# Doesn't exist
Error: Spec 'q-999' does not exist.

# Dependency target was purged
Warning: Dependency target 'q-002' no longer exists. Use 'boi dep remove q-008 --on q-002' to clean up.
boi: Promoted q-008 to queued
```

**Help text:**
```
Usage: boi promote <queue-id> [<queue-id> ...]

Promote draft spec(s) to queued status, making them eligible for execution.
Only specs with status 'draft' can be promoted.

Examples:
  boi promote q-008              Promote a single draft
  boi promote q-008 q-009        Promote multiple drafts
```

**Bash implementation sketch:**
```bash
cmd_promote() {
    if [[ $# -eq 0 ]] || [[ "$1" == "-h" ]] || [[ "$1" == "--help" ]]; then
        echo "Usage: boi promote <queue-id> [<queue-id> ...]"
        echo ""
        echo "Promote draft spec(s) to queued status, making them eligible for execution."
        echo "Only specs with status 'draft' can be promoted."
        echo ""
        echo "Examples:"
        echo "  boi promote q-008              Promote a single draft"
        echo "  boi promote q-008 q-009        Promote multiple drafts"
        exit 0
    fi

    require_config

    for queue_id in "$@"; do
        local result
        result=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${queue_id}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.cli_ops import promote

try:
    result = promote(sys.argv[1])
    print(json.dumps(result))
except ValueError as e:
    print(json.dumps({"error": str(e)}))
    sys.exit(2)
PYEOF
        )
        local exit_code=$?

        if [[ ${exit_code} -ne 0 ]]; then
            local err_msg
            err_msg=$(echo "${result}" | python3 -c "import json,sys; print(json.load(sys.stdin)['error'])")
            die "${err_msg}"
        fi

        local spec_name deps_info
        spec_name=$(echo "${result}" | python3 -c "import json,sys; print(json.load(sys.stdin)['name'])")
        deps_info=$(echo "${result}" | python3 -c "import json,sys; print(json.load(sys.stdin).get('deps_info',''))")

        if [[ -n "${deps_info}" ]]; then
            info "Promoted ${queue_id} to queued — blocked by ${deps_info}"
        else
            info "Promoted ${queue_id} to queued — ready for execution"
        fi
    done
}
```

### 5.4 Demoting to Draft: `boi demote`

**Syntax:**
```
boi demote <queue-id>
```

Transitions a spec from `queued` back to `draft`. Only works on specs with status `queued`.

**Example usage:**
```bash
boi demote q-008
```

**Expected output (success):**
```
boi: Demoted q-008 to draft
  Spec: ux-polish
  This spec will NOT run until promoted again.
```

**Error cases:**
```
# Not queued
Error: Spec 'q-003' cannot be demoted (status: running). Only queued specs can be demoted.

# Doesn't exist
Error: Spec 'q-999' does not exist.

# Already a draft
Error: Spec 'q-008' is already a draft.
```

**Help text:**
```
Usage: boi demote <queue-id>

Demote a queued spec back to draft status, preventing it from being picked up
by the daemon. Only specs with status 'queued' can be demoted.

Examples:
  boi demote q-008               Pull a spec back to draft
```

**Bash implementation sketch:**
```bash
cmd_demote() {
    if [[ $# -eq 0 ]] || [[ "$1" == "-h" ]] || [[ "$1" == "--help" ]]; then
        echo "Usage: boi demote <queue-id>"
        echo ""
        echo "Demote a queued spec back to draft status."
        echo "Only specs with status 'queued' can be demoted."
        exit 0
    fi

    require_config

    local queue_id="$1"
    local result
    result=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${queue_id}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.cli_ops import demote

try:
    result = demote(sys.argv[1])
    print(json.dumps(result))
except ValueError as e:
    print(json.dumps({"error": str(e)}))
    sys.exit(2)
PYEOF
    )
    local exit_code=$?

    if [[ ${exit_code} -ne 0 ]]; then
        local err_msg
        err_msg=$(echo "${result}" | python3 -c "import json,sys; print(json.load(sys.stdin)['error'])")
        die "${err_msg}"
    fi

    info "Demoted ${queue_id} to draft"
    echo "  This spec will NOT run until promoted again."
}
```

### 5.5 Declaring Dependencies: `--after` and `boi dep`

#### 5.5.1 At Dispatch Time: `--after`

Covered in section 5.1 above. The `--after` flag accepts comma-separated queue IDs:

```bash
boi dispatch spec.md --after q-003
boi dispatch spec.md --after q-003,q-005
boi dispatch spec.md --draft --after q-003
```

#### 5.5.2 Post-Dispatch: `boi dep add` / `boi dep remove`

**Syntax:**
```
boi dep add <spec-id> --on <dep-id>[,<dep-id>,...]
boi dep remove <spec-id> --on <dep-id>[,<dep-id>,...]
```

The `boi dep` command is a subcommand group (like `boi project` or `boi spec`), routing to `_dep_add()` and `_dep_remove()` helpers.

**Example usage:**
```bash
# Add a dependency
boi dep add q-007 --on q-003
boi dep add q-007 --on q-003,q-005

# Remove a dependency
boi dep remove q-007 --on q-003
```

**Expected output (add, success):**
```
boi: Added dependency: q-007 blocked by q-003
```

**Expected output (add multiple, success):**
```
boi: Added dependency: q-007 blocked by q-003
boi: Added dependency: q-007 blocked by q-005
```

**Error cases (add):**
```
# Circular dependency
Error: Circular dependency detected: q-003 → q-007 → q-003

# Target doesn't exist
Error: Dependency target 'q-999' does not exist.

# Spec is running
Error: Cannot add dependency to spec 'q-003' in 'running' state.

# Already exists (silent success — idempotent)
boi: Dependency already exists: q-007 blocked by q-003
```

**Expected output (remove, success):**
```
boi: Removed dependency: q-007 no longer blocked by q-003
```

**Error cases (remove):**
```
# Doesn't exist
Error: No dependency from 'q-007' on 'q-003'.
```

**Help text:**
```
Usage: boi dep <subcommand>

Manage spec-level dependencies.

Subcommands:
  add      Add a dependency (spec waits for another spec)
  remove   Remove a dependency

Examples:
  boi dep add q-007 --on q-003           q-007 waits for q-003
  boi dep add q-007 --on q-003,q-005     q-007 waits for both
  boi dep remove q-007 --on q-003        Remove the dependency
```

**Bash implementation sketch:**
```bash
cmd_dep() {
    if [[ $# -eq 0 ]] || [[ "$1" == "-h" ]] || [[ "$1" == "--help" ]] || [[ "$1" == "help" ]]; then
        echo "Usage: boi dep <subcommand>"
        echo ""
        echo "Manage spec-level dependencies."
        echo ""
        echo "Subcommands:"
        echo "  add      Add a dependency (spec waits for another spec)"
        echo "  remove   Remove a dependency"
        echo ""
        echo "Examples:"
        echo "  boi dep add q-007 --on q-003           q-007 waits for q-003"
        echo "  boi dep add q-007 --on q-003,q-005     q-007 waits for both"
        echo "  boi dep remove q-007 --on q-003        Remove the dependency"
        exit 0
    fi

    local subcommand="$1"
    shift

    case "${subcommand}" in
        add)    _dep_add "$@" ;;
        remove) _dep_remove "$@" ;;
        -h|--help|help) cmd_dep --help ;;
        *)
            die_usage "Unknown dep subcommand: ${subcommand}. Use 'boi dep --help' for usage."
            ;;
    esac
}

_dep_add() {
    local spec_id=""
    local on_ids=""

    # First positional arg is the spec ID
    [[ $# -eq 0 ]] && die_usage "Usage: boi dep add <spec-id> --on <dep-id>"
    spec_id="$1"; shift

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --on)
                [[ -z "${2:-}" ]] && die_usage "--on requires queue ID(s)"
                on_ids="$2"; shift 2 ;;
            -h|--help) echo "Usage: boi dep add <spec-id> --on <dep-id>[,...]"; exit 0 ;;
            *) die_usage "Unknown option: $1" ;;
        esac
    done

    [[ -z "${on_ids}" ]] && die_usage "Missing --on flag. Usage: boi dep add <spec-id> --on <dep-id>"

    require_config

    # Split comma-separated IDs and add each
    IFS=',' read -ra dep_ids <<< "${on_ids}"
    for dep_id in "${dep_ids[@]}"; do
        dep_id=$(echo "${dep_id}" | xargs)  # trim whitespace
        local result
        result=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${spec_id}" "${dep_id}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.cli_ops import add_dep

try:
    result = add_dep(sys.argv[1], sys.argv[2])
    print(json.dumps(result))
except ValueError as e:
    print(json.dumps({"error": str(e)}))
    sys.exit(2)
PYEOF
        )
        local exit_code=$?

        if [[ ${exit_code} -ne 0 ]]; then
            local err_msg
            err_msg=$(echo "${result}" | python3 -c "import json,sys; print(json.load(sys.stdin)['error'])")
            die "${err_msg}"
        fi

        info "Added dependency: ${spec_id} blocked by ${dep_id}"
    done
}

_dep_remove() {
    local spec_id=""
    local on_ids=""

    [[ $# -eq 0 ]] && die_usage "Usage: boi dep remove <spec-id> --on <dep-id>"
    spec_id="$1"; shift

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --on)
                [[ -z "${2:-}" ]] && die_usage "--on requires queue ID(s)"
                on_ids="$2"; shift 2 ;;
            -h|--help) echo "Usage: boi dep remove <spec-id> --on <dep-id>[,...]"; exit 0 ;;
            *) die_usage "Unknown option: $1" ;;
        esac
    done

    [[ -z "${on_ids}" ]] && die_usage "Missing --on flag. Usage: boi dep remove <spec-id> --on <dep-id>"

    require_config

    IFS=',' read -ra dep_ids <<< "${on_ids}"
    for dep_id in "${dep_ids[@]}"; do
        dep_id=$(echo "${dep_id}" | xargs)
        local result
        result=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${spec_id}" "${dep_id}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.cli_ops import remove_dep

try:
    result = remove_dep(sys.argv[1], sys.argv[2])
    print(json.dumps(result))
except ValueError as e:
    print(json.dumps({"error": str(e)}))
    sys.exit(2)
PYEOF
        )
        local exit_code=$?

        if [[ ${exit_code} -ne 0 ]]; then
            local err_msg
            err_msg=$(echo "${result}" | python3 -c "import json,sys; print(json.load(sys.stdin)['error'])")
            die "${err_msg}"
        fi

        info "Removed dependency: ${spec_id} no longer blocked by ${dep_id}"
    done
}
```

### 5.6 Viewing Dependency Chains: `boi deps`

**Syntax:**
```
boi deps <queue-id>
```

Shows the upstream dependencies (what this spec waits for) and downstream dependents (what waits for this spec) in an ASCII tree format.

**Example usage:**
```bash
boi deps q-007
```

**Expected output (spec with deps):**
```
q-007  feature-pipeline  [queued]

  Upstream (blocked by):
    └── q-003  eval-system          [running]  ← must complete first
    └── q-005  api-refactor         [running]  ← must complete first

  Downstream (blocking):
    └── q-010  integration-tests    [queued]   ← waits for q-007
    └── q-011  deploy-staging       [draft]    ← waits for q-007
```

**Expected output (spec with no deps):**
```
q-007  feature-pipeline  [queued]

  No dependencies.
```

**Expected output (deep chain):**
```
q-010  integration-tests  [queued]

  Upstream (blocked by):
    └── q-007  feature-pipeline     [queued]
        └── q-003  eval-system      [running]  ← root dependency
        └── q-005  api-refactor     [running]  ← root dependency

  Downstream (blocking):
    └── q-012  deploy-prod          [draft]
```

The upstream tree is recursive — it walks the full dependency chain so users can see the root blockers. The tree uses a maximum depth of 10 to avoid infinite recursion in case of data corruption.

**Error cases:**
```
# Doesn't exist
Error: Spec 'q-999' does not exist.
```

**Help text:**
```
Usage: boi deps <queue-id>

Show the dependency chain for a spec: what it waits for (upstream)
and what waits for it (downstream).

Examples:
  boi deps q-007                 Show full dependency chain
```

**Bash implementation sketch:**
```bash
cmd_deps() {
    if [[ $# -eq 0 ]] || [[ "$1" == "-h" ]] || [[ "$1" == "--help" ]]; then
        echo "Usage: boi deps <queue-id>"
        echo ""
        echo "Show the dependency chain for a spec."
        exit 0
    fi

    require_config

    local queue_id="$1"
    local result
    result=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${queue_id}" <<'PYEOF'
import sys, os
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.cli_ops import get_deps

try:
    output = get_deps(sys.argv[1])
    print(output)
except ValueError as e:
    import json
    print(json.dumps({"error": str(e)}))
    sys.exit(2)
PYEOF
    )
    local exit_code=$?

    if [[ ${exit_code} -ne 0 ]]; then
        local err_msg
        err_msg=$(echo "${result}" | python3 -c "import json,sys; print(json.load(sys.stdin)['error'])")
        die "${err_msg}"
    fi

    echo "${result}"
}
```

**Python implementation in `cli_ops.py`:**
```python
def get_deps(spec_id: str) -> str:
    """Return a formatted dependency chain string for display."""
    db = _get_db()
    chain = db.get_dependency_chain(spec_id)
    # chain = {"spec": {...}, "upstream": [...], "downstream": [...]}

    spec = chain["spec"]
    lines = [f"{spec['id']}  {spec['name']}  [{spec['status']}]", ""]

    if not chain["upstream"] and not chain["downstream"]:
        lines.append("  No dependencies.")
        return "\n".join(lines)

    if chain["upstream"]:
        lines.append("  Upstream (blocked by):")
        for dep in chain["upstream"]:
            status_note = "← must complete first" if dep["status"] != "completed" else "✓ done"
            lines.append(f"    └── {dep['id']}  {dep['name']:<24s}[{dep['status']}]  {status_note}")
        lines.append("")

    if chain["downstream"]:
        lines.append("  Downstream (blocking):")
        for dep in chain["downstream"]:
            lines.append(f"    └── {dep['id']}  {dep['name']:<24s}[{dep['status']}]  ← waits for {spec_id}")

    return "\n".join(lines)
```

### 5.7 Enhanced `boi queue` Display

The queue table gains two visual enhancements — dependency indicators and draft presentation:

**Current format (unchanged columns):**
```
SPEC                           MODE      WORKER  ITER    TASKS        STATUS
```

**Enhanced STATUS column behavior:**

| Status | Display | Color | Notes |
|--------|---------|-------|-------|
| `draft` | `[draft]` | CYAN | Brackets distinguish from active statuses |
| `queued` (no deps) | `queued` | DIM | Unchanged |
| `queued` (blocked) | `queued ← q-003` | DIM | Shows first blocker |
| `running` | `running` | YELLOW | Unchanged |
| `draft` (with deps) | `[draft] ← q-003` | CYAN | Shows deps even for drafts |

**Dependency suffix rules:**
- Only shown for `queued`, `draft`, and `requeued` statuses (states where deps matter)
- Shows at most 2 dep IDs, then `+N more` if there are additional deps
- Example with many deps: `queued ← q-003, q-005 +2 more`
- Dep IDs link to their current status via color: if the blocker is `failed`, the dep ID is shown in red

**Full enhanced example:**
```
SPEC                           MODE      WORKER  ITER    TASKS        STATUS
───────────────────────────────────────────────────────────────────────────────
q-003  eval-system              execute   w-1     5/30    3/8 done     running
q-005  api-refactor             execute   w-2     2/30    1/5 done     running
q-007  feature-pipeline         execute   —       —       0/6 done     queued ← q-003
q-008  ux-polish                generate  —       —       0/4 done     [draft]
q-009  perf-optimization        execute   —       —       0/3 done     [draft] ← q-003, q-005

Workers: 2/3 busy  |  2 running, 1 queued, 2 drafts
Run 'boi log q-003' to see worker output
```

**Implementation in `format_queue_table()` (`status.py`):**
```python
def _format_status_with_deps(spec: dict, deps: list[dict]) -> str:
    """Format the status cell with optional dependency suffix."""
    status = spec["status"]
    if status == "draft":
        label = "[draft]"
    else:
        label = status

    # Only show dep suffix for blockable states
    if status in ("queued", "draft", "requeued") and deps:
        unmet = [d for d in deps if d["status"] != "completed"]
        if unmet:
            dep_strs = [d["id"] for d in unmet[:2]]
            suffix = ", ".join(dep_strs)
            if len(unmet) > 2:
                suffix += f" +{len(unmet) - 2} more"
            label += f" ← {suffix}"

    return label
```

### 5.8 Enhanced `boi status` Dashboard

The `boi status --watch` live dashboard integrates drafts and dependency info into the existing layout.

**Current dashboard sections:**
1. Header (BOI version, uptime, workers)
2. Active specs table (running/queued/requeued)
3. Recent completions
4. Worker assignment

**Enhanced sections:**
1. Header — add draft count to the summary line
2. Active specs table — same changes as `boi queue` (draft labels, dep suffixes)
3. **Drafts section (new)** — shown below active specs when drafts exist
4. Blocked spec warnings — shown when a high-priority spec is blocked by a failed dep

**Draft section in dashboard:**
```
─── Drafts ──────────────────────────────────────────────────
q-008  ux-polish             generate  pri:100  4 tasks
q-009  perf-optimization     execute   pri:100  3 tasks  ← q-003, q-005

Use 'boi promote <id>' to queue a draft for execution.
```

The drafts section uses DIM text to visually de-emphasize it relative to active specs. It appears only when there are drafts in the database.

**Blocked spec warning (in dashboard header):**
```
⚠ q-007 (priority:10) blocked by q-003 (failed) — consider retrying or removing dependency
```

This warning surfaces when a spec with priority ≤ 50 is blocked by a dependency in `failed` or `canceled` state. It draws attention to situations where high-priority work is stuck.

**Summary line enhancement:**
```
# Current:
Workers: 2/3 busy  |  2 running, 1 queued, 7 completed

# Enhanced:
Workers: 2/3 busy  |  2 running, 1 queued, 2 drafts, 7 completed
```

### 5.9 Command Routing Summary

New entries in `main()` case block:

```bash
case "${command}" in
    # ... existing commands ...
    promote)    cmd_promote "$@" ;;
    demote)     cmd_demote "$@" ;;
    dep)        cmd_dep "$@" ;;
    deps)       cmd_deps "$@" ;;
    # ... rest of existing commands ...
esac
```

New entries in the usage/help text:

```
Draft & Dependency Commands:
  promote     Promote draft spec(s) to queued
  demote      Demote a queued spec back to draft
  dep         Manage dependencies (add/remove)
  deps        View dependency chain for a spec

Dispatch Flags (new):
  --draft     Dispatch as draft (won't run until promoted)
  --after     Declare dependencies (comma-separated queue IDs)
```

### 5.10 CLI Quick Reference

| Workflow | Command |
|----------|---------|
| Create a draft | `boi dispatch spec.md --draft` |
| Create a draft with deps | `boi dispatch spec.md --draft --after q-003` |
| Dispatch with deps (immediate) | `boi dispatch spec.md --after q-003` |
| List everything (incl. drafts) | `boi queue` |
| Promote a draft | `boi promote q-008` |
| Promote multiple drafts | `boi promote q-008 q-009` |
| Demote back to draft | `boi demote q-008` |
| Add dependency after dispatch | `boi dep add q-007 --on q-003` |
| Remove a dependency | `boi dep remove q-007 --on q-003` |
| View dependency chain | `boi deps q-007` |
| Check blocked spec status | `boi status` (shows warnings) |

## 6. Priority System Interactions

### 6.1 Priority vs. Dependencies: Fundamental Rule

**Dependencies are hard constraints; priority is a soft ordering.**

A high-priority spec (priority=10) that depends on a low-priority spec (priority=200) **must still wait**. The dependency relationship is a prerequisite — it cannot be overridden by priority. Priority only determines the order in which *eligible* specs are dispatched. A spec is eligible when:

1. Its status is `queued` or `requeued` (not `draft`, `running`, `assigning`, etc.)
2. It is not in the blocked worker set
3. Its cooldown period has expired (if any)
4. **All of its dependencies have status `completed`**

Among eligible specs, `pick_next_spec()` selects the one with the lowest priority number (highest priority), breaking ties by `submitted_at` (FIFO).

```
┌─────────────────────────────────────────────────────────────┐
│                   Dispatch Decision Flow                     │
│                                                             │
│  All specs with status IN ('queued', 'requeued')            │
│         │                                                   │
│         ▼                                                   │
│  Filter: not in blocked_ids                                 │
│         │                                                   │
│         ▼                                                   │
│  Filter: cooldown_until is NULL or in the past              │
│         │                                                   │
│         ▼                                                   │
│  Filter: ALL spec_dependencies have status = 'completed'    │
│         │                                                   │
│         ▼                                                   │
│  Order by: priority ASC, submitted_at ASC                   │
│         │                                                   │
│         ▼                                                   │
│  Pick first → set to 'assigning'                            │
└─────────────────────────────────────────────────────────────┘
```

### 6.2 Priority Inheritance: Recommendation Against (v1)

**Problem:** When a high-priority spec (priority=10) depends on a low-priority spec (priority=200), the low-priority spec may sit in the queue behind many other specs, delaying the high-priority work.

**Priority inheritance** is a technique (borrowed from real-time OS scheduling) where the dependency's priority is temporarily boosted to match or exceed the dependent's priority. In this example, the low-priority spec would be promoted to priority=10 so it runs sooner.

**Why we recommend against it for v1:**

1. **Complexity:** Inheritance requires tracking the "inherited" vs "natural" priority, restoring priority after completion, and handling transitive chains (A depends on B depends on C — does C inherit A's priority?).
2. **Surprising behavior:** Users set priorities deliberately. Having the system silently change them creates confusion, especially in `boi queue` output where a spec shows a different priority than what was dispatched.
3. **Cascading effects:** In a diamond dependency (A→B, A→C, B→D, C→D), priority inheritance must propagate through all paths and resolve conflicts (what if B and C have different dependents with different priorities?).
4. **Manual workaround exists:** Users can manually reprioritize with `boi reprioritize <queue-id> <new-priority>`. This is explicit, predictable, and sufficient for v1.

**v1 recommendation:** No automatic priority inheritance. Users manage priorities manually. Revisit in v2 if usage patterns show frequent frustration.

### 6.3 Draft Priority

Drafts are assigned a priority at dispatch time (default 100, or via `--priority` flag):

```bash
boi dispatch spec.md --draft                  # priority=100 (default)
boi dispatch spec.md --draft --priority 10    # priority=10
```

**While a spec is in `draft` status, its priority is stored but has no effect.** Drafts are never selected by `pick_next_spec()` because they don't match `status IN ('queued', 'requeued')`.

When a draft is promoted (`boi promote q-007`):
- The stored priority carries over to the `queued` status
- Users can change priority before or after promotion using `boi reprioritize`

This means users can set up a batch of drafts with different priorities, iterate on them, and then promote them — the priority ordering will take effect as each draft enters the active queue.

### 6.4 Dispatch Ordering Example: 5 Specs with Mixed Priorities and Dependencies

Consider this scenario:

| Spec   | Priority | Dependencies | Status  |
|--------|:--------:|:------------:|---------|
| q-001  |    50    |    none      | queued  |
| q-002  |    10    |   q-001      | queued  |
| q-003  |   200    |    none      | queued  |
| q-004  |    30    |   q-003      | queued  |
| q-005  |   100    |    none      | draft   |

**Round 1** — `pick_next_spec()` evaluates (ordered by priority ASC):

- q-002 (priority=10): has dep on q-001, which is `queued` (not `completed`) → **skip**
- q-004 (priority=30): has dep on q-003, which is `queued` (not `completed`) → **skip**
- q-001 (priority=50): no deps → **SELECTED** → status → `assigning` → `running`
- q-003 (priority=200): not reached (q-001 was picked first)
- q-005 (priority=100): not even in query results (status is `draft`)

**Round 2** — Assuming q-001 is still running, one worker is available:

- q-002 (priority=10): dep q-001 is `running` → **skip**
- q-004 (priority=30): dep q-003 is `queued` → **skip**
- q-003 (priority=200): no deps → **SELECTED** → `assigning` → `running`

**Round 3** — q-001 completes, q-003 still running:

- q-002 (priority=10): dep q-001 is now `completed` → **SELECTED** → runs
- q-004 (priority=30): dep q-003 is `running` → still blocked

**Round 4** — q-003 completes:

- q-004 (priority=30): dep q-003 is now `completed` → **SELECTED** → runs

**Summary of execution order:** q-001 → q-003 → q-002 → q-004. Note that q-005 never runs (it's a draft).

**Key observation:** Despite q-002 having the highest priority (10), it ran third because it was blocked by q-001. This is correct behavior — dependencies are hard constraints. The priority system ensured q-002 was the *first* thing dispatched once its dependency was met, beating q-003 which was still available.

### 6.5 Starvation Risk and Mitigation

**Scenario:** A high-priority spec is blocked by a dependency that keeps failing.

```
q-010 (priority=200, no deps)        → fails, requeued
q-010 (requeued, attempt 2)          → fails again, requeued
q-010 (requeued, attempt 3)          → fails again, requeued
...
q-020 (priority=10, depends on q-010) → starved, cannot run
```

In this scenario, q-020 has high priority (10) but is stuck because q-010 (priority=200) keeps failing and requeuing. The system will keep retrying q-010 (up to its `max_iterations` limit), but q-020 cannot make progress until q-010 succeeds.

**Mitigations:**

1. **`boi status` warning:** When a high-priority spec is blocked by a dependency that has `consecutive_failures > 0`, surface a warning in the dashboard:

   ```
   ⚠ WARNING: q-020 (priority=10) is blocked by q-010 which has failed 3 times
              Consider: boi dep remove q-020 --on q-010  (unblock manually)
                        boi cancel q-010                  (give up on dependency)
   ```

2. **Max failure limit on deps:** When a dependency exhausts its `max_iterations` and transitions to `failed`, `boi status` should prominently flag all specs that depend on it:

   ```
   ✗ BLOCKED: q-020 (priority=10) depends on q-010 [FAILED]
             This spec will never run unless q-010 is retried or the dependency is removed.
   ```

3. **Manual unblock:** Users can always break out of starvation by removing the dependency:
   ```bash
   boi dep remove q-020 --on q-010    # Unblock q-020, let it run independently
   ```

4. **No automatic cascading failure (v1):** We do NOT auto-cancel or auto-fail dependents when a dependency fails. The user must decide how to proceed. This is a deliberate choice — the spec may have other value even if its dependency failed, or the dependency may be retryable after a fix.

**Starvation detection query** (for `boi status`):

```sql
SELECT s.id, s.priority, d.blocks_on, dep.status, dep.consecutive_failures
FROM specs s
JOIN spec_dependencies d ON s.id = d.spec_id
JOIN specs dep ON d.blocks_on = dep.id
WHERE s.status IN ('queued', 'requeued')
  AND dep.status IN ('failed', 'canceled')
ORDER BY s.priority ASC;
```

This query finds all eligible specs that are waiting on a failed or canceled dependency — these are the starvation candidates that need user attention.

## 7. Edge Cases

This section analyzes each edge case that arises from introducing draft specs and dependency chains. For each case: the scenario, expected behavior, and implementation notes.

### 7.1 Circular Dependencies

**Scenario:** Spec A depends on Spec B, and Spec B depends on Spec A (directly or through a longer chain: A→B→C→A).

**Expected behavior:** Reject at insertion time with a clear error showing the cycle path.

```
$ boi dep add q-003 --on q-005
Error: Circular dependency detected: q-003 → q-005 → q-003

$ boi dispatch spec.md --after q-007
Error: Circular dependency detected: q-012 → q-007 → q-009 → q-012
```

**Implementation:** DFS cycle detection runs before every dependency insertion — both at dispatch time (`--after` flag, spec header parsing) and post-dispatch (`boi dep add`). The algorithm is defined in `detect_cycle()` (see Section 4.5): starting from the proposed `blocks_on` target, follow existing edges; if we reach `spec_id`, a cycle exists. Cycle path reconstruction via BFS is provided by `_find_cycle_path()` for human-readable error messages.

**Pseudocode:**
```
function would_create_cycle(spec_id, blocks_on):
    // Would adding "spec_id is blocked by blocks_on" create a cycle?
    // Equivalent to: can we reach spec_id starting from blocks_on?
    visited = {}
    stack = [blocks_on]
    while stack is not empty:
        current = stack.pop()
        if current == spec_id:
            return true   // Cycle!
        if current in visited:
            continue
        visited.add(current)
        for each dep in get_dependencies_of(current):
            stack.push(dep)
    return false
```

**Complexity:** O(V + E) where V is the number of specs and E is the number of dependency edges. In practice, the dependency graph is small (tens of specs, not thousands), so this is negligible.

**Transitive cycles:** The DFS naturally detects cycles of any length. A chain A→B→C→D→A is caught when adding the D→A edge because the DFS from A reaches D through the existing path A→B→C→D.

### 7.2 Failed Dependency

**Scenario:** Spec A fails (status = `failed`). Spec B depends on A.

**Options considered:**

| Option | Description | Pros | Cons |
|--------|-------------|------|------|
| **(a) B stays blocked** | B remains in `queued` status, waiting for A to be retried and succeed | Simple, no new states, user stays in control | B could be forgotten if user doesn't notice |
| **(b) Auto-cancel B** | B is automatically moved to `canceled` with a message | Clean resolution | Destructive — user may not want B canceled; hard to undo |
| **(c) New `dep_failed` status** | B gets a special status indicating its dep failed | Explicit signal | Adds schema complexity, new status to handle everywhere |

**Recommendation: Option (a) — B stays blocked.**

Rationale:
- Simplicity: no new states, no automatic cascading actions
- User control: the user decides whether to retry A, remove the dep, or cancel B
- Visibility: `boi status` and `boi queue` surface a warning when a spec is blocked by a failed/canceled dependency (see Section 6 — starvation warnings)

**User workflow when a dep fails:**
```
$ boi status
⚠️  q-007 "Implement API" is blocked by q-003 "Design API" [failed]
    → Retry q-003 with: boi retry q-003
    → Or unblock q-007: boi dep remove q-007 --on q-003

$ boi retry q-003          # Requeue the failed dep
# ... or ...
$ boi dep remove q-007 --on q-003   # Unblock manually
```

**Implementation notes:**
- No change needed in `pick_next_spec()` — it already checks `status = 'completed'` for deps, so a `failed` dep naturally blocks the dependent.
- The warning is generated in `status.py` by joining `spec_dependencies` with `specs` and checking for `status IN ('failed', 'canceled')` among dep targets.
- Chain propagation: if A fails, B is blocked. If C depends on B, C is also effectively blocked (B will never run, so B will never complete). The warning system should surface this transitively: "q-009 is indirectly blocked: q-007 [queued, blocked] → q-003 [failed]".

### 7.3 Canceled Dependency

**Scenario:** Spec A is canceled (status = `canceled`). Spec B depends on A.

**Expected behavior:** Same as failed dependency — B stays blocked. The reasoning is identical: cancellation is a user action, and the user should decide what happens to dependents.

```
$ boi status
⚠️  q-007 "Implement API" is blocked by q-003 "Design API" [canceled]
    → Unblock q-007: boi dep remove q-007 --on q-003
    → Or re-dispatch q-003
```

**Difference from failed dep:** A canceled spec cannot be retried with `boi retry` (it was intentionally stopped). The user must either remove the dep or re-dispatch the canceled spec's work under a new queue ID.

**Implementation:** Identical to 7.2 — `pick_next_spec()` already handles this. The warning in `boi status` checks for both `failed` and `canceled` in the same query.

### 7.4 Dependency on Non-Existent Spec

**Scenario:** User dispatches with `--after q-999` but q-999 doesn't exist in the `specs` table.

**Expected behavior:** Reject immediately at dispatch time with a clear error.

```
$ boi dispatch spec.md --after q-999
Error: Dependency target 'q-999' does not exist.

$ boi dep add q-007 --on q-999
Error: Spec 'q-999' does not exist.
```

**Implementation:**
- In `cli_ops.dispatch()` and `db.add_dependency()`, validate that the target exists before inserting:
  ```python
  row = self.conn.execute(
      "SELECT id FROM specs WHERE id = ?", (blocks_on,)
  ).fetchone()
  if row is None:
      raise ValueError(f"Dependency target '{blocks_on}' does not exist")
  ```
- This check happens before cycle detection (no point checking cycles if the target doesn't exist).
- The FK constraint on `spec_dependencies.blocks_on` provides a database-level safety net, but the application-level check gives better error messages.

### 7.5 Self-Dependency

**Scenario:** User tries `boi dep add q-003 --on q-003`.

**Expected behavior:** Reject as a circular dependency (trivial cycle of length 1).

```
$ boi dep add q-003 --on q-003
Error: Circular dependency detected: q-003 → q-003
```

**Implementation:** The existing `detect_cycle()` DFS handles this automatically — the starting node `blocks_on = q-003` is immediately equal to `spec_id = q-003`, so it returns `True`. No special-case code needed.

However, as a fast-path optimization, `add_dependency()` can check `spec_id == blocks_on` before running the full DFS:

```python
def add_dependency(self, spec_id: str, blocks_on: str) -> None:
    if spec_id == blocks_on:
        raise ValueError(
            f"Circular dependency detected: {spec_id} → {spec_id}"
        )
    # ... rest of validation and DFS ...
```

### 7.6 Dependency Chain Across Projects

**Scenario:** Spec q-007 belongs to project "api-service". Spec q-003 belongs to project "auth-system". User declares `boi dep add q-007 --on q-003`.

**Expected behavior:** This should work. Dependencies are by queue ID, not project. Projects are organizational metadata, not execution boundaries.

```
$ boi dep add q-007 --on q-003
✓ q-007 "Implement API" now depends on q-003 "Build auth system"
  (Note: q-003 is in project 'auth-system', q-007 is in project 'api-service')
```

**Implementation:**
- No special handling needed. The `spec_dependencies` table references `specs.id` directly — there's no project constraint.
- The `boi deps` command and `boi queue` display should show the project name alongside cross-project deps for clarity.
- `pick_next_spec()` already evaluates deps globally, not per-project.

**Edge within edge case:** If `boi queue --project api-service` filters the queue display to one project, cross-project dependencies should still be visible. The dep target q-003 should appear (perhaps dimmed or with a project label) even though it's in a different project:

```
$ boi queue --project api-service
  q-007  Implement API    [queued]  blocked by q-003 [auth-system, running]
```

### 7.7 Promoting a Draft with Unmet Dependencies

**Scenario:** q-007 is a draft that depends on q-003. q-003 is still `running` (not yet completed). User promotes q-007.

**Expected behavior:** Allowed. Promotion moves q-007 from `draft` to `queued`. But `pick_next_spec()` won't select it until q-003 completes.

```
$ boi promote q-007
✓ q-007 "Implement API" promoted to queued.
  Note: Blocked by 1 dependency: q-003 [running]
```

**Rationale:** Promotion means "this spec is ready to run when its turn comes." It does not mean "run immediately." The dependency system handles scheduling — promotion just removes the "not ready for execution" hold.

**What promotion validates:**
1. The spec exists and has `status = 'draft'` ✓
2. All `blocks_on` targets exist in the `specs` table ✓ (catches stale references to purged specs)

**What promotion does NOT validate:**
- Whether deps are in a "healthy" state (completed, running, etc.) — a dep on a `failed` spec is allowed; the user can retry it later.
- Whether deps are themselves drafts — a draft depending on another draft is fine; the first draft must be promoted AND completed before the second becomes eligible.

### 7.8 Deleting/Purging a Spec That Others Depend On

**Scenario:** q-003 is purged (row deleted from `specs` table). q-007 depends on q-003.

**Database behavior:** The FK on `spec_dependencies` has `ON DELETE CASCADE`. When q-003 is deleted from `specs`, the row `(q-007, q-003)` in `spec_dependencies` is automatically deleted.

**Consequence:** After the cascade, q-007 has no remaining dep on q-003 — the dependency simply vanishes. If q-003 was q-007's only dependency, q-007 becomes immediately eligible for `pick_next_spec()`.

**Is this correct?** It depends on the user's intent:
- If the purged spec's work was completed elsewhere or is no longer needed → the cascade is correct; the dep should be removed.
- If the purged spec was accidentally deleted → the dep silently disappearing could cause q-007 to run prematurely.

**Recommendation: Warn before purging specs that have dependents.**

```
$ boi purge q-003
⚠️  Warning: q-003 has 2 dependent specs that will be unblocked:
    q-007 "Implement API" (currently queued, blocked by q-003)
    q-012 "Write tests" (currently draft, blocked by q-003)

Purge anyway? [y/N]
```

**Implementation:**
- Before purging, query for dependents:
  ```python
  dependents = self.conn.execute(
      "SELECT sd.spec_id, s.title, s.status "
      "FROM spec_dependencies sd "
      "JOIN specs s ON s.id = sd.spec_id "
      "WHERE sd.blocks_on = ?", (spec_id,)
  ).fetchall()
  ```
- If dependents exist, surface a warning in the CLI and require confirmation.
- The actual purge relies on `ON DELETE CASCADE` — no manual cleanup of `spec_dependencies` needed.

**Alternative considered:** Instead of relying on CASCADE, we could mark the dep as "orphaned" and block the dependent. But this adds complexity (orphan detection in `pick_next_spec()`, new query logic) for a rare edge case. The warning-before-purge approach is simpler and handles the real risk (accidental premature unblocking).

### 7.9 Summary Table

| Edge Case | Detection Point | Behavior | User Recovery |
|-----------|----------------|----------|---------------|
| Circular dependency | `add_dependency()`, dispatch `--after` | Reject with cycle path | Fix the dep chain |
| Failed dependency | `pick_next_spec()` (passive) | Dependent stays blocked | `boi retry` or `boi dep remove` |
| Canceled dependency | `pick_next_spec()` (passive) | Dependent stays blocked | `boi dep remove` or re-dispatch |
| Non-existent target | `add_dependency()`, dispatch `--after` | Reject with error | Use correct queue ID |
| Self-dependency | `add_dependency()` | Reject as circular | N/A |
| Cross-project deps | No detection needed | Works normally | N/A |
| Promote with unmet deps | `promote()` | Allowed; queued but blocked | Wait or remove deps |
| Purge dep target | `boi purge` (pre-action) | Warn; cascade deletes dep edge | Confirm or cancel purge |

## 8. Implementation Proposal

This section provides a file-by-file change list with concrete function signatures, SQL statements, and bash snippets. The goal is to be specific enough that a developer can implement from this document without ambiguity.

### 8.1 `~/.boi/src/lib/schema.sql`

**Change 1: Update CHECK constraint on `specs.status`** (line 39)

```sql
-- Before:
CHECK (status IN ('queued','assigning','running','completed','failed','canceled','needs_review','requeued'))

-- After:
CHECK (status IN ('draft','queued','assigning','running','completed','failed','canceled','needs_review','requeued'))
```

Note: This only affects fresh installs (new databases). Existing databases require migration — see Section 8.6.

**Change 2: Add index on `spec_dependencies` for reverse lookups**

The existing `PRIMARY KEY (spec_id, blocks_on)` provides fast lookup of "what does this spec depend on?" Adding a reverse index enables fast "what depends on this spec?" queries needed by `boi deps`, `boi purge` warnings, and the starvation detection query.

```sql
-- Add after the spec_dependencies table definition (line 50):
CREATE INDEX IF NOT EXISTS idx_spec_deps_blocks_on ON spec_dependencies(blocks_on);
```

### 8.2 `~/.boi/src/lib/db.py`

Six new methods and one modification to `enqueue()`.

**Change 1: Extend `enqueue()` with `as_draft` parameter**

Rather than creating a separate `enqueue_draft()` method (which would duplicate most of `enqueue()`), add a boolean parameter:

```python
def enqueue(
    self,
    spec_path: str,
    priority: int = DEFAULT_PRIORITY,
    max_iterations: int = DEFAULT_MAX_ITERATIONS,
    blocked_by: Optional[list[str]] = None,
    checkout: Optional[str] = None,
    queue_id: Optional[str] = None,
    sync_back: bool = True,
    project: Optional[str] = None,
    as_draft: bool = False,                     # NEW PARAMETER
) -> dict[str, Any]:
```

Changes within the method body:

```python
# Line ~177: Update duplicate detection to include 'draft' in active statuses
active_statuses = ("queued", "running", "requeued", "draft")  # Added "draft"
cursor = self.conn.execute(
    "SELECT id, status FROM specs "
    "WHERE original_spec_path = ? AND status IN (?, ?, ?, ?)",  # Added one placeholder
    (abs_spec_path, *active_statuses),
)

# Line ~222: Use 'draft' or 'queued' status based on as_draft flag
initial_status = "draft" if as_draft else "queued"

self.conn.execute(
    "INSERT INTO specs ("
    "  id, spec_path, original_spec_path, worktree, priority,"
    "  status, phase, submitted_at, iteration, max_iterations,"
    "  sync_back, project, initial_task_ids"
    ") VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    (
        queue_id,
        abs_copy_path,
        abs_spec_path,
        checkout,
        priority,
        initial_status,   # Was hardcoded "queued"
        "execute",
        now,
        0,
        max_iterations,
        1 if sync_back else 0,
        project,
        json.dumps(initial_task_ids),
    ),
)

# Line ~234: Validate dep targets exist when blocked_by is provided
if blocked_by:
    for dep_id in blocked_by:
        dep_row = self.conn.execute(
            "SELECT id FROM specs WHERE id = ?", (dep_id,)
        ).fetchone()
        if dep_row is None:
            raise ValueError(f"Dependency target '{dep_id}' does not exist")
    # Cycle detection for new spec (spec_id is the new queue_id)
    # Not needed here — new spec has no dependents yet, so no cycle is possible
    for dep_id in blocked_by:
        self.conn.execute(
            "INSERT INTO spec_dependencies (spec_id, blocks_on) "
            "VALUES (?, ?)",
            (queue_id, dep_id),
        )

# Line ~242: Update event type for drafts
event_type = "drafted" if as_draft else "queued"
self._log_event(
    event_type,
    f"Spec {'drafted' if as_draft else 'queued'}: {self._spec_name_from_path(abs_spec_path)}",
    spec_id=queue_id,
    data={
        "priority": priority,
        "max_iterations": max_iterations,
        "as_draft": as_draft,
    },
)
```

**Change 2: Add `promote()` method**

```python
def promote(self, spec_id: str) -> dict[str, Any]:
    """Transition a spec from 'draft' to 'queued'.

    Validates:
    1. Spec exists
    2. Spec has status 'draft'
    3. All dependency targets still exist (not purged)

    Returns the updated spec dict.
    Raises ValueError if validation fails.
    """
    with self.lock:
        row = self.conn.execute(
            "SELECT * FROM specs WHERE id = ?", (spec_id,)
        ).fetchone()
        if row is None:
            raise ValueError(f"Spec '{spec_id}' does not exist")
        if row["status"] != "draft":
            raise ValueError(
                f"Spec '{spec_id}' is not a draft "
                f"(status: {row['status']}). "
                f"Only drafts can be promoted."
            )

        # Validate dep targets still exist
        deps = self.conn.execute(
            "SELECT blocks_on FROM spec_dependencies WHERE spec_id = ?",
            (spec_id,),
        ).fetchall()
        missing = []
        for dep in deps:
            target = self.conn.execute(
                "SELECT id FROM specs WHERE id = ?",
                (dep["blocks_on"],),
            ).fetchone()
            if target is None:
                missing.append(dep["blocks_on"])
        # Warn but don't block — user can clean up stale deps
        # (The warning is surfaced by the CLI layer)

        self.conn.execute(
            "UPDATE specs SET status = 'queued' WHERE id = ?",
            (spec_id,),
        )
        self._log_event(
            "promoted",
            f"Draft promoted to queued",
            spec_id=spec_id,
        )
        self.conn.commit()

        updated = self.conn.execute(
            "SELECT * FROM specs WHERE id = ?", (spec_id,)
        ).fetchone()
        result = self._row_to_dict(updated)
        result["missing_deps"] = missing
        return result
```

**Change 3: Add `demote()` method**

```python
def demote(self, spec_id: str) -> dict[str, Any]:
    """Transition a spec from 'queued' to 'draft'.

    Only works on specs with status 'queued'.
    Raises ValueError for any other status.

    Returns the updated spec dict.
    """
    with self.lock:
        row = self.conn.execute(
            "SELECT * FROM specs WHERE id = ?", (spec_id,)
        ).fetchone()
        if row is None:
            raise ValueError(f"Spec '{spec_id}' does not exist")
        if row["status"] == "draft":
            raise ValueError(
                f"Spec '{spec_id}' is already a draft."
            )
        if row["status"] != "queued":
            raise ValueError(
                f"Spec '{spec_id}' cannot be demoted "
                f"(status: {row['status']}). "
                f"Only queued specs can be demoted."
            )

        self.conn.execute(
            "UPDATE specs SET status = 'draft' WHERE id = ?",
            (spec_id,),
        )
        self._log_event(
            "demoted",
            f"Spec demoted to draft",
            spec_id=spec_id,
        )
        self.conn.commit()

        updated = self.conn.execute(
            "SELECT * FROM specs WHERE id = ?", (spec_id,)
        ).fetchone()
        return self._row_to_dict(updated)
```

**Change 4: Add `add_dependency()` method**

```python
def add_dependency(self, spec_id: str, blocks_on: str) -> None:
    """Add a dependency edge: spec_id is blocked by blocks_on.

    Validates:
    1. spec_id != blocks_on (self-dep)
    2. Both spec_id and blocks_on exist in the specs table
    3. spec_id is in a mutable state (draft, queued, requeued)
    4. Adding this edge would not create a circular dependency

    Raises ValueError on validation failure.
    Uses INSERT OR IGNORE for idempotency.
    """
    if spec_id == blocks_on:
        raise ValueError(
            f"Circular dependency detected: {spec_id} → {spec_id}"
        )

    with self.lock:
        # Validate both specs exist
        for sid, label in [(spec_id, "spec"), (blocks_on, "dependency target")]:
            row = self.conn.execute(
                "SELECT id, status FROM specs WHERE id = ?", (sid,)
            ).fetchone()
            if row is None:
                raise ValueError(f"Spec '{sid}' does not exist")

        # Check spec is in a mutable state
        spec_row = self.conn.execute(
            "SELECT status FROM specs WHERE id = ?", (spec_id,)
        ).fetchone()
        mutable_states = ("draft", "queued", "requeued")
        if spec_row["status"] not in mutable_states:
            raise ValueError(
                f"Cannot add dependency to spec '{spec_id}' "
                f"in '{spec_row['status']}' state"
            )

        # Circular dependency detection
        if self.detect_cycle(spec_id, blocks_on):
            cycle_path = self._find_cycle_path(spec_id, blocks_on)
            raise ValueError(
                f"Circular dependency detected: {cycle_path}"
            )

        # Insert edge (idempotent)
        self.conn.execute(
            "INSERT OR IGNORE INTO spec_dependencies "
            "(spec_id, blocks_on) VALUES (?, ?)",
            (spec_id, blocks_on),
        )
        self._log_event(
            "dep_added",
            f"Dependency added: {spec_id} blocked by {blocks_on}",
            spec_id=spec_id,
            data={"blocks_on": blocks_on},
        )
        self.conn.commit()
```

**Change 5: Add `remove_dependency()` method**

```python
def remove_dependency(self, spec_id: str, blocks_on: str) -> None:
    """Remove a dependency edge.

    Raises ValueError if the edge doesn't exist.
    """
    with self.lock:
        cursor = self.conn.execute(
            "DELETE FROM spec_dependencies "
            "WHERE spec_id = ? AND blocks_on = ?",
            (spec_id, blocks_on),
        )
        if cursor.rowcount == 0:
            raise ValueError(
                f"No dependency from '{spec_id}' on '{blocks_on}'"
            )
        self._log_event(
            "dep_removed",
            f"Dependency removed: {spec_id} no longer blocked by {blocks_on}",
            spec_id=spec_id,
            data={"blocks_on": blocks_on},
        )
        self.conn.commit()
```

**Change 6: Add `detect_cycle()` method**

```python
def detect_cycle(self, spec_id: str, blocks_on: str) -> bool:
    """Return True if adding spec_id→blocks_on would create a cycle.

    Uses iterative DFS: starting from blocks_on, follow the
    'blocks_on' edges in spec_dependencies. If we reach spec_id,
    there's a cycle.

    Must be called while holding self.lock.
    """
    visited: set[str] = set()
    stack = [blocks_on]
    while stack:
        current = stack.pop()
        if current == spec_id:
            return True
        if current in visited:
            continue
        visited.add(current)
        deps = self.conn.execute(
            "SELECT blocks_on FROM spec_dependencies "
            "WHERE spec_id = ?",
            (current,),
        ).fetchall()
        for dep in deps:
            stack.append(dep["blocks_on"])
    return False
```

**Change 7: Add `_find_cycle_path()` method**

```python
def _find_cycle_path(self, spec_id: str, blocks_on: str) -> str:
    """Return a human-readable cycle path string.

    E.g., 'q-003 → q-005 → q-003'
    Uses BFS to find the shortest path from blocks_on back to spec_id.
    Must be called while holding self.lock.
    """
    from collections import deque
    queue: deque[tuple[str, list[str]]] = deque(
        [(blocks_on, [blocks_on])]
    )
    visited: set[str] = set()
    while queue:
        current, path = queue.popleft()
        if current == spec_id:
            full_path = [spec_id] + path
            return " → ".join(full_path)
        if current in visited:
            continue
        visited.add(current)
        deps = self.conn.execute(
            "SELECT blocks_on FROM spec_dependencies "
            "WHERE spec_id = ?",
            (current,),
        ).fetchall()
        for dep in deps:
            queue.append((dep["blocks_on"], path + [dep["blocks_on"]]))
    # Fallback (shouldn't reach here if detect_cycle returned True)
    return f"{spec_id} → {blocks_on} → ... → {spec_id}"
```

**Change 8: Add `get_dependency_chain()` method**

```python
def get_dependency_chain(self, spec_id: str) -> dict[str, Any]:
    """Return upstream and downstream dependencies with statuses.

    Returns:
        {
            "spec": {"id": str, "name": str, "status": str},
            "upstream": [{"id": str, "name": str, "status": str}, ...],
            "downstream": [{"id": str, "name": str, "status": str}, ...],
        }

    Upstream = what this spec depends on (blocks_on targets).
    Downstream = what depends on this spec (spec_ids that block on it).

    Upstream is recursive — walks the full chain up to depth 10.
    Downstream is direct (1 level) to keep output manageable.

    Raises ValueError if spec_id doesn't exist.
    """
    row = self.conn.execute(
        "SELECT * FROM specs WHERE id = ?", (spec_id,)
    ).fetchone()
    if row is None:
        raise ValueError(f"Spec '{spec_id}' does not exist")

    spec_info = {
        "id": row["id"],
        "name": self._spec_name_from_path(
            row["original_spec_path"] or row["spec_path"]
        ),
        "status": row["status"],
    }

    # Upstream (recursive): what does this spec wait for?
    upstream: list[dict[str, Any]] = []
    self._walk_upstream(spec_id, upstream, depth=0, max_depth=10)

    # Downstream (direct): what waits for this spec?
    downstream_rows = self.conn.execute(
        "SELECT sd.spec_id, s.status, s.original_spec_path, s.spec_path "
        "FROM spec_dependencies sd "
        "JOIN specs s ON s.id = sd.spec_id "
        "WHERE sd.blocks_on = ?",
        (spec_id,),
    ).fetchall()
    downstream = [
        {
            "id": r["spec_id"],
            "name": self._spec_name_from_path(
                r["original_spec_path"] or r["spec_path"]
            ),
            "status": r["status"],
        }
        for r in downstream_rows
    ]

    return {
        "spec": spec_info,
        "upstream": upstream,
        "downstream": downstream,
    }

def _walk_upstream(
    self,
    spec_id: str,
    result: list[dict[str, Any]],
    depth: int,
    max_depth: int,
    visited: Optional[set[str]] = None,
) -> None:
    """Recursively walk upstream dependencies."""
    if depth >= max_depth:
        return
    if visited is None:
        visited = set()
    if spec_id in visited:
        return
    visited.add(spec_id)

    deps = self.conn.execute(
        "SELECT sd.blocks_on, s.status, s.original_spec_path, s.spec_path "
        "FROM spec_dependencies sd "
        "JOIN specs s ON s.id = sd.blocks_on "
        "WHERE sd.spec_id = ?",
        (spec_id,),
    ).fetchall()
    for dep in deps:
        entry = {
            "id": dep["blocks_on"],
            "name": self._spec_name_from_path(
                dep["original_spec_path"] or dep["spec_path"]
            ),
            "status": dep["status"],
            "depth": depth,
        }
        result.append(entry)
        self._walk_upstream(
            dep["blocks_on"], result, depth + 1, max_depth, visited
        )
```

**Change 9: `pick_next_spec()` — NO CHANGES NEEDED**

The existing `pick_next_spec()` queries `WHERE status IN ('queued', 'requeued')`. Since drafts have `status = 'draft'`, they are automatically excluded. The dependency check also works correctly — a draft dependency (status='draft') will never be 'completed', so specs depending on it stay blocked.

### 8.3 `~/.boi/src/lib/cli_ops.py`

Extend `dispatch()` and add five new functions.

**Change 1: Extend `dispatch()` with `as_draft` and `blocked_by` parameters**

```python
def dispatch(
    queue_dir: str,
    spec_path: str,
    priority: int = 100,
    max_iterations: int = 30,
    checkout: Optional[str] = None,
    timeout: Optional[int] = None,
    mode: str = "execute",
    project: Optional[str] = None,
    experiment_budget: Optional[int] = None,
    as_draft: bool = False,                     # NEW
    blocked_by: Optional[list[str]] = None,     # NEW
) -> dict[str, Any]:
    """Enqueue a spec into the SQLite database.

    Handles the full dispatch flow: enqueue, set phase based on
    spec type, update task counts, set experiment budget and timeout.
    Supports draft mode and dependency declaration.

    Returns a dict with: id, tasks, pending, mode, phase, status.
    Raises DuplicateSpecError if the same spec is already active.
    """
    from lib.queue import get_experiment_budget
    from lib.spec_parser import count_boi_tasks
    from lib.spec_validator import is_generate_spec

    db = _get_db(queue_dir)
    try:
        counts = count_boi_tasks(spec_path)

        # Parse spec header for Blocked-By dependencies
        spec_content = Path(spec_path).read_text(encoding="utf-8")
        header_deps = parse_spec_dependencies(spec_content)

        # Merge CLI and header dependencies (union)
        all_deps = _merge_dependencies(header_deps, blocked_by)

        entry = db.enqueue(
            spec_path=spec_path,
            priority=priority,
            max_iterations=max_iterations,
            checkout=checkout,
            project=project,
            blocked_by=all_deps or None,
            as_draft=as_draft,
        )

        spec_id = entry["id"]

        # Determine phase from spec type
        phase = "decompose" if is_generate_spec(spec_content) else "execute"

        # Build update fields for post-enqueue configuration
        updates: dict[str, Any] = {
            "phase": phase,
            "tasks_done": counts["done"],
            "tasks_total": counts["total"],
        }

        if timeout is not None:
            updates["worker_timeout_seconds"] = timeout

        if experiment_budget is not None:
            updates["max_experiment_invocations"] = experiment_budget
        else:
            updates["max_experiment_invocations"] = get_experiment_budget(mode)
        updates["experiment_invocations_used"] = 0

        db.update_spec_fields(spec_id, **updates)

        return {
            "id": spec_id,
            "tasks": counts["total"],
            "pending": counts["pending"],
            "mode": mode,
            "phase": phase,
            "status": "draft" if as_draft else "queued",
            "blocked_by": all_deps,
        }
    finally:
        db.close()
```

**Change 2: Add `parse_spec_dependencies()` helper**

```python
import re

def parse_spec_dependencies(spec_content: str) -> list[str]:
    """Extract Blocked-By queue IDs from spec header metadata.

    Looks for a line matching: **Blocked-By:** q-001, q-002, ...
    Returns a list of queue IDs, or empty list if no header found.
    """
    for line in spec_content.splitlines():
        match = re.match(
            r'\*\*Blocked-By:\*\*\s*(.+)',
            line, re.IGNORECASE,
        )
        if match:
            raw = match.group(1).strip()
            ids = [qid.strip() for qid in raw.split(',')]
            for qid in ids:
                if not re.match(r'^q-\d+$', qid):
                    raise ValueError(
                        f"Invalid queue ID in Blocked-By header: '{qid}'"
                    )
            return ids
    return []


def _merge_dependencies(
    header_deps: list[str],
    cli_deps: Optional[list[str]],
) -> list[str]:
    """Merge dependencies from spec header and CLI flag (union, order-preserving)."""
    seen: set[str] = set()
    merged: list[str] = []
    for dep_id in header_deps + (cli_deps or []):
        if dep_id not in seen:
            seen.add(dep_id)
            merged.append(dep_id)
    return merged
```

**Change 3: Add `promote()` function**

```python
def promote(queue_dir: str, queue_id: str) -> dict[str, Any]:
    """Promote a draft spec to queued status.

    Returns dict with: id, name, status, deps_info, missing_deps.
    Raises ValueError if spec is not a draft or doesn't exist.
    """
    db = _get_db(queue_dir)
    try:
        result = db.promote(queue_id)
        spec_name = os.path.splitext(
            os.path.basename(result.get("original_spec_path", "") or "")
        )[0]

        # Get current dep info for display
        deps = db.conn.execute(
            "SELECT sd.blocks_on, s.status "
            "FROM spec_dependencies sd "
            "JOIN specs s ON s.id = sd.blocks_on "
            "WHERE sd.spec_id = ?",
            (queue_id,),
        ).fetchall()
        unmet = [
            f"{d['blocks_on']} ({d['status']})"
            for d in deps
            if d["status"] != "completed"
        ]

        return {
            "id": queue_id,
            "name": spec_name,
            "status": "queued",
            "deps_info": ", ".join(unmet) if unmet else "",
            "missing_deps": result.get("missing_deps", []),
        }
    finally:
        db.close()
```

**Change 4: Add `demote()` function**

```python
def demote(queue_dir: str, queue_id: str) -> dict[str, Any]:
    """Demote a queued spec back to draft status.

    Returns dict with: id, name, status.
    Raises ValueError if spec is not queued or doesn't exist.
    """
    db = _get_db(queue_dir)
    try:
        result = db.demote(queue_id)
        spec_name = os.path.splitext(
            os.path.basename(result.get("original_spec_path", "") or "")
        )[0]
        return {
            "id": queue_id,
            "name": spec_name,
            "status": "draft",
        }
    finally:
        db.close()
```

**Change 5: Add `add_dep()` function**

```python
def add_dep(queue_dir: str, spec_id: str, dep_id: str) -> dict[str, str]:
    """Add a dependency: spec_id is blocked by dep_id.

    Returns dict with: spec_id, blocks_on, status.
    Raises ValueError on validation failure.
    """
    db = _get_db(queue_dir)
    try:
        db.add_dependency(spec_id, dep_id)
        return {
            "spec_id": spec_id,
            "blocks_on": dep_id,
            "status": "added",
        }
    finally:
        db.close()
```

**Change 6: Add `remove_dep()` function**

```python
def remove_dep(queue_dir: str, spec_id: str, dep_id: str) -> dict[str, str]:
    """Remove a dependency: spec_id no longer blocked by dep_id.

    Returns dict with: spec_id, blocks_on, status.
    Raises ValueError if the dep doesn't exist.
    """
    db = _get_db(queue_dir)
    try:
        db.remove_dependency(spec_id, dep_id)
        return {
            "spec_id": spec_id,
            "blocks_on": dep_id,
            "status": "removed",
        }
    finally:
        db.close()
```

**Change 7: Add `get_deps()` function**

```python
def get_deps(queue_dir: str, spec_id: str) -> str:
    """Return a formatted dependency chain string for display.

    Returns a multi-line string showing upstream and downstream deps.
    Raises ValueError if spec doesn't exist.
    """
    db = _get_db(queue_dir)
    try:
        chain = db.get_dependency_chain(spec_id)
        spec = chain["spec"]
        lines = [f"{spec['id']}  {spec['name']}  [{spec['status']}]", ""]

        if not chain["upstream"] and not chain["downstream"]:
            lines.append("  No dependencies.")
            return "\n".join(lines)

        if chain["upstream"]:
            lines.append("  Upstream (blocked by):")
            for dep in chain["upstream"]:
                indent = "    " + "    " * dep.get("depth", 0)
                status_note = (
                    "← must complete first"
                    if dep["status"] != "completed"
                    else "✓ done"
                )
                lines.append(
                    f"{indent}└── {dep['id']}  {dep['name']:<24s}"
                    f"[{dep['status']}]  {status_note}"
                )
            lines.append("")

        if chain["downstream"]:
            lines.append("  Downstream (blocking):")
            for dep in chain["downstream"]:
                lines.append(
                    f"    └── {dep['id']}  {dep['name']:<24s}"
                    f"[{dep['status']}]  ← waits for {spec_id}"
                )

        return "\n".join(lines)
    finally:
        db.close()
```

### 8.4 `~/.boi/src/boi.sh`

**Change 1: Add `--draft` and `--after` flags to `cmd_dispatch()` (line 246)**

Add to variable declarations (after line 257):
```bash
local as_draft=false
local after_ids=""
```

Add to `while/case` flag parsing (after the `--dry-run` case, before `*)`):
```bash
    --draft)
        as_draft=true
        shift
        ;;
    --after|--blocked-by)
        [[ -z "${2:-}" ]] && die_usage "--after requires queue ID(s)"
        after_ids="$2"
        shift 2
        ;;
```

Update help text (line 315) to include new flags:
```bash
echo "  --draft           Dispatch as draft (won't run until promoted)"
echo "  --after IDs       Declare dependencies (comma-separated queue IDs)"
```

Update the Python bridge heredoc (line 474) to pass new parameters:
```bash
result=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${input_file}" "${QUEUE_DIR}" "${priority}" "${max_iter}" "${worktree_arg}" "${timeout}" "${mode}" "${project}" "${experiment_budget}" "${as_draft}" "${after_ids}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.cli_ops import dispatch
from lib.db import DuplicateSpecError

spec_path = sys.argv[1]
queue_dir = sys.argv[2]
priority = int(sys.argv[3])
max_iter = int(sys.argv[4])
checkout = sys.argv[5] if len(sys.argv) > 5 and sys.argv[5] else None
timeout_str = sys.argv[6] if len(sys.argv) > 6 and sys.argv[6] else None
mode = sys.argv[7] if len(sys.argv) > 7 and sys.argv[7] else "execute"
project_name = sys.argv[8] if len(sys.argv) > 8 and sys.argv[8] else None
experiment_budget_str = sys.argv[9] if len(sys.argv) > 9 and sys.argv[9] else None
as_draft = sys.argv[10] == "true" if len(sys.argv) > 10 else False
after_ids_str = sys.argv[11] if len(sys.argv) > 11 and sys.argv[11] else ""
after_ids = [x.strip() for x in after_ids_str.split(",") if x.strip()] if after_ids_str else None

try:
    result = dispatch(
        queue_dir=queue_dir,
        spec_path=spec_path,
        priority=priority,
        max_iterations=max_iter,
        checkout=checkout,
        timeout=int(timeout_str) if timeout_str else None,
        mode=mode,
        project=project_name,
        experiment_budget=int(experiment_budget_str) if experiment_budget_str else None,
        as_draft=as_draft,
        blocked_by=after_ids,
    )
    print(json.dumps(result))
except DuplicateSpecError as e:
    print(json.dumps({"error": "duplicate", "message": str(e)}))
    sys.exit(2)
PYEOF
)
```

Update post-dispatch output (after line 533) to handle draft mode:
```bash
if [[ "${as_draft}" == "true" ]]; then
    progress_done "${queue_id} (draft), ${pending_count}/${task_count} tasks, priority ${priority}"
    echo ""
    echo -e "  This spec will NOT run until promoted. Use '${BOLD}boi promote ${queue_id}${NC}' when ready."
    exit 0
fi
```

**Change 2: Add `cmd_promote()` function**

```bash
cmd_promote() {
    if [[ $# -eq 0 ]] || [[ "$1" == "-h" ]] || [[ "$1" == "--help" ]]; then
        echo "Usage: boi promote <queue-id> [<queue-id> ...]"
        echo ""
        echo "Promote draft spec(s) to queued status, making them eligible for execution."
        echo "Only specs with status 'draft' can be promoted."
        echo ""
        echo "Examples:"
        echo "  boi promote q-008              Promote a single draft"
        echo "  boi promote q-008 q-009        Promote multiple drafts"
        exit 0
    fi

    require_config

    for queue_id in "$@"; do
        local result
        result=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${queue_id}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.cli_ops import promote

try:
    result = promote(sys.argv[1], sys.argv[2])
    print(json.dumps(result))
except ValueError as e:
    print(json.dumps({"error": str(e)}))
    sys.exit(2)
PYEOF
        )
        local exit_code=$?

        if [[ ${exit_code} -ne 0 ]]; then
            local err_msg
            err_msg=$(echo "${result}" | python3 -c "import json,sys; print(json.load(sys.stdin)['error'])")
            die "${err_msg}"
        fi

        local deps_info
        deps_info=$(echo "${result}" | python3 -c "import json,sys; print(json.load(sys.stdin).get('deps_info',''))")

        if [[ -n "${deps_info}" ]]; then
            info "Promoted ${queue_id} to queued — blocked by ${deps_info}"
        else
            info "Promoted ${queue_id} to queued — ready for execution"
        fi
    done
}
```

**Change 3: Add `cmd_demote()` function**

```bash
cmd_demote() {
    if [[ $# -eq 0 ]] || [[ "$1" == "-h" ]] || [[ "$1" == "--help" ]]; then
        echo "Usage: boi demote <queue-id>"
        echo ""
        echo "Demote a queued spec back to draft status."
        echo "Only specs with status 'queued' can be demoted."
        exit 0
    fi

    require_config

    local queue_id="$1"
    local result
    result=$(BOI_SCRIPT_DIR="${SCRIPT_DIR}" python3 - "${QUEUE_DIR}" "${queue_id}" <<'PYEOF'
import sys, os, json
sys.path.insert(0, os.environ["BOI_SCRIPT_DIR"])
from lib.cli_ops import demote

try:
    result = demote(sys.argv[1], sys.argv[2])
    print(json.dumps(result))
except ValueError as e:
    print(json.dumps({"error": str(e)}))
    sys.exit(2)
PYEOF
    )
    local exit_code=$?

    if [[ ${exit_code} -ne 0 ]]; then
        local err_msg
        err_msg=$(echo "${result}" | python3 -c "import json,sys; print(json.load(sys.stdin)['error'])")
        die "${err_msg}"
    fi

    info "Demoted ${queue_id} to draft"
    echo "  This spec will NOT run until promoted again."
}
```

**Change 4: Add `cmd_dep()` function with `_dep_add` and `_dep_remove` helpers**

Full implementation provided in Section 5.5.2 (lines 1079-1210 of this document). The bash routing is:

```bash
cmd_dep() {
    if [[ $# -eq 0 ]] || [[ "$1" == "-h" ]] || [[ "$1" == "--help" ]]; then
        echo "Usage: boi dep <subcommand>"
        echo ""
        echo "Manage spec-level dependencies."
        echo ""
        echo "Subcommands:"
        echo "  add      Add a dependency (spec waits for another spec)"
        echo "  remove   Remove a dependency"
        echo ""
        echo "Examples:"
        echo "  boi dep add q-007 --on q-003           q-007 waits for q-003"
        echo "  boi dep remove q-007 --on q-003        Remove the dependency"
        exit 0
    fi

    local subcommand="$1"
    shift

    case "${subcommand}" in
        add)    _dep_add "$@" ;;
        remove) _dep_remove "$@" ;;
        *)      die_usage "Unknown dep subcommand: ${subcommand}" ;;
    esac
}
```

The `_dep_add` and `_dep_remove` helpers pass `QUEUE_DIR` as the first argument to the Python bridge, matching the `cli_ops.add_dep(queue_dir, spec_id, dep_id)` signature.

**Change 5: Add `cmd_deps()` function**

Full implementation provided in Section 5.6 (lines 1281-1317). The Python bridge calls `cli_ops.get_deps(queue_dir, spec_id)`.

**Change 6: Update `main()` routing table** (line 3520)

Add after existing commands:
```bash
    promote)    cmd_promote "$@" ;;
    demote)     cmd_demote "$@" ;;
    dep)        cmd_dep "$@" ;;
    deps)       cmd_deps "$@" ;;
```

### 8.5 `~/.boi/src/lib/status.py`

**Change 1: Add `draft` to `STATUS_COLORS` dict**

```python
STATUS_COLORS: dict[str, str] = {
    # ... existing entries ...
    "draft": CYAN,      # NEW — visually separates drafts from active statuses
}
```

**Change 2: Update `format_queue_table()` to show draft label and dep info** (line 509)

In the per-entry loop (around line 593), modify the status display:

```python
status = entry.get("status", "queued")

# Format status with draft brackets and dependency suffix
if status == "draft":
    status_str = "[draft]"
else:
    status_str = status

# Add dependency suffix for blockable states
if status in ("queued", "draft", "requeued"):
    deps_info = entry.get("unmet_deps", [])
    if deps_info:
        dep_ids = [d["id"] for d in deps_info[:2]]
        suffix = ", ".join(dep_ids)
        if len(deps_info) > 2:
            suffix += f" +{len(deps_info) - 2} more"
        status_str += f" ← {suffix}"
```

**Change 3: Update summary line to include draft count**

In the summary section at the bottom of `format_queue_table()`:

```python
# Count drafts separately
draft_count = sum(1 for e in entries if e.get("status") == "draft")

# Build summary parts
parts = []
if running_count: parts.append(f"{running_count} running")
if queued_count: parts.append(f"{queued_count} queued")
if draft_count: parts.append(f"{draft_count} drafts")      # NEW
if completed_count: parts.append(f"{completed_count} completed")
```

**Change 4: Populate `unmet_deps` in status data collection**

In the function that builds `status_data` (likely `get_status_data()` or the caller of `format_queue_table()`), add dependency info per entry:

```python
def _enrich_with_deps(db: Database, entries: list[dict]) -> None:
    """Add unmet_deps info to each entry for display."""
    for entry in entries:
        if entry["status"] not in ("queued", "draft", "requeued"):
            continue
        deps = db.conn.execute(
            "SELECT sd.blocks_on, s.status "
            "FROM spec_dependencies sd "
            "JOIN specs s ON s.id = sd.blocks_on "
            "WHERE sd.spec_id = ?",
            (entry["id"],),
        ).fetchall()
        unmet = [
            {"id": d["blocks_on"], "status": d["status"]}
            for d in deps
            if d["status"] != "completed"
        ]
        entry["unmet_deps"] = unmet
```

**Change 5: Add starvation warning to dashboard**

In `boi status --watch` or the dashboard view, add a warning section:

```python
def _format_starvation_warnings(db: Database) -> list[str]:
    """Generate warning lines for specs blocked by failed/canceled deps."""
    rows = db.conn.execute(
        "SELECT s.id, s.priority, d.blocks_on, dep.status, dep.consecutive_failures "
        "FROM specs s "
        "JOIN spec_dependencies d ON s.id = d.spec_id "
        "JOIN specs dep ON d.blocks_on = dep.id "
        "WHERE s.status IN ('queued', 'requeued') "
        "  AND dep.status IN ('failed', 'canceled') "
        "ORDER BY s.priority ASC",
    ).fetchall()

    warnings = []
    for r in rows:
        warnings.append(
            f"⚠ {r['id']} (priority:{r['priority']}) blocked by "
            f"{r['blocks_on']} [{r['status']}]"
        )
    return warnings
```

### 8.6 `~/.boi/src/lib/db_migrate.py`

**The SQLite CHECK constraint challenge:**

SQLite does not support `ALTER TABLE ... ALTER CONSTRAINT` or `ALTER TABLE ... DROP CONSTRAINT`. To change a CHECK constraint, you must either:

(a) **Drop the CHECK entirely** and rely on application-level validation.
(b) **Recreate the table** with the new CHECK constraint (copy data, drop old table, rename new table).

**Recommendation: Option (a) — Drop the CHECK constraint.**

Rationale:
- `db.py` already validates status values in every method that sets status (the hardcoded string values in `enqueue()`, `promote()`, `demote()`, `cancel()`, `pick_next_spec()`, etc.)
- The CHECK constraint is a safety net, not the primary validation mechanism
- Table recreation (option b) requires careful handling of FKs, indexes, triggers, and is fragile with concurrent access from the daemon
- SQLite's `PRAGMA integrity_check` doesn't validate CHECK constraints anyway

**Migration function:**

```python
def migrate_add_draft_status(db: Database) -> bool:
    """Migration: enable 'draft' status for specs.

    SQLite doesn't support ALTER CONSTRAINT, so we take the pragmatic
    approach of dropping the CHECK constraint entirely. Application-level
    validation in db.py already enforces valid status values.

    This migration:
    1. Recreates the specs table without the status CHECK constraint
    2. Copies all existing data
    3. Adds the index on spec_dependencies.blocks_on

    Returns True if migration was applied, False if already done.
    """
    # Check if migration is needed by trying to insert a draft
    try:
        db.conn.execute(
            "INSERT INTO specs (id, spec_path, priority, status, submitted_at, iteration, max_iterations) "
            "VALUES ('__migration_test__', '/dev/null', 0, 'draft', '', 0, 0)"
        )
        # If we get here, the CHECK already allows 'draft' (or is absent)
        db.conn.execute("DELETE FROM specs WHERE id = '__migration_test__'")
        db.conn.commit()
        return False  # Migration not needed
    except sqlite3.IntegrityError:
        db.conn.rollback()
        # CHECK constraint rejects 'draft' — migration needed
        pass

    with db.lock:
        # Step 1: Create new table without status CHECK
        db.conn.executescript("""
            CREATE TABLE specs_new (
                id TEXT PRIMARY KEY,
                spec_path TEXT NOT NULL,
                original_spec_path TEXT,
                worktree TEXT,
                priority INTEGER NOT NULL DEFAULT 100,
                status TEXT NOT NULL,
                phase TEXT DEFAULT 'execute',
                submitted_at TEXT NOT NULL,
                first_running_at TEXT,
                last_iteration_at TEXT,
                last_worker TEXT,
                iteration INTEGER NOT NULL DEFAULT 0,
                max_iterations INTEGER NOT NULL DEFAULT 30,
                consecutive_failures INTEGER DEFAULT 0,
                cooldown_until TEXT,
                tasks_done INTEGER DEFAULT 0,
                tasks_total INTEGER DEFAULT 0,
                sync_back INTEGER DEFAULT 1,
                project TEXT,
                initial_task_ids TEXT,
                worker_timeout_seconds INTEGER,
                failure_reason TEXT,
                needs_review_since TEXT,
                assigning_at TEXT,
                critic_passes INTEGER DEFAULT 0,
                pre_iteration_tasks TEXT,
                experiment_tasks TEXT,
                max_experiment_invocations INTEGER DEFAULT 0,
                experiment_invocations_used INTEGER DEFAULT 0,
                decomposition_retries INTEGER DEFAULT 0,
                CHECK (phase IN ('execute','critic','evaluate','decompose'))
            );

            INSERT INTO specs_new SELECT * FROM specs;

            DROP TABLE specs;

            ALTER TABLE specs_new RENAME TO specs;
        """)

        # Step 2: Recreate indexes and FKs that reference specs
        db.conn.executescript("""
            CREATE INDEX IF NOT EXISTS idx_specs_last_worker
                ON specs(last_worker);
            CREATE INDEX IF NOT EXISTS idx_spec_deps_blocks_on
                ON spec_dependencies(blocks_on);
        """)

        db.conn.commit()

    return True
```

**Where to call the migration:**

Add to `Database.__init__()` after `self.init_schema()`:

```python
def __init__(self, db_path: str, queue_dir: str) -> None:
    # ... existing setup ...
    self.init_schema()
    self._run_migrations()   # NEW

def _run_migrations(self) -> None:
    """Run any pending schema migrations."""
    from lib.db_migrate import migrate_add_draft_status
    try:
        migrate_add_draft_status(self)
    except Exception:
        pass  # Migration already applied or not needed
```

### 8.7 Change Summary

| File | Lines Added (est.) | Lines Modified (est.) | Description |
|------|:------------------:|:---------------------:|-------------|
| `schema.sql` | 2 | 1 | Add `'draft'` to CHECK, add index |
| `db.py` | ~180 | ~15 | 8 new methods, extend `enqueue()` |
| `cli_ops.py` | ~130 | ~20 | 7 new functions, extend `dispatch()` |
| `boi.sh` | ~120 | ~25 | 4 new commands, extend `cmd_dispatch()` |
| `status.py` | ~40 | ~15 | Draft display, dep info, starvation warnings |
| `db_migrate.py` | ~60 | 0 | New migration function |
| **Total** | **~530** | **~76** | |

### 8.8 Dependency Order for Implementation

The implementation should proceed in this order, as each step builds on the previous:

```
Phase 1: Schema + Core DB (can be done in one PR)
  1. schema.sql — add 'draft' to CHECK, add index
  2. db_migrate.py — migration function
  3. db.py — enqueue(as_draft=), promote(), demote()
  4. db.py — add_dependency(), remove_dependency(), detect_cycle()
  5. db.py — get_dependency_chain()

Phase 2: CLI Surface (depends on Phase 1)
  6. cli_ops.py — extend dispatch(), add promote/demote/dep functions
  7. boi.sh — add --draft, --after flags to cmd_dispatch()
  8. boi.sh — add cmd_promote, cmd_demote, cmd_dep, cmd_deps

Phase 3: Display (depends on Phase 1, independent of Phase 2)
  9. status.py — draft label, dep info, starvation warnings
```

## 9. Alternative Approaches

The recommended design uses an in-database `draft` status on the existing `specs` table. Below are three alternative approaches that were considered, with detailed trade-off analysis and verdicts.

### Alternative A: File-System Drafts (No Database)

**Concept:** Store draft specs as files in a dedicated `~/.boi/drafts/` directory, separate from the queue. Drafts live entirely on the filesystem and are not tracked in SQLite until promoted. Promotion means moving the file to the queue directory and calling `enqueue()`.

**How it would work:**

```
# Create a draft (just copies file to drafts dir)
boi draft spec.md
# → Saved to ~/.boi/drafts/spec.md

# List drafts (reads filesystem)
boi drafts
# → spec.md   (modified: 2026-03-11 14:00)
# → other.md  (modified: 2026-03-10 09:30)

# Promote to queue (moves file + enqueues)
boi promote spec.md
# → Dispatched as q-047 (status: queued)
```

**Trade-offs:**

| Aspect | Pro | Con |
|--------|-----|-----|
| Simplicity | No schema changes needed; just file operations | Requires separate listing/management code for a parallel system |
| Tracking | Files are easy to browse in a file manager | No queue ID until promotion — can't reference drafts in dependencies |
| Dependencies | N/A | Cannot declare `--after draft-spec.md` because drafts have no ID in the DB |
| Integration | Familiar file-based workflow | Completely invisible to `boi queue` and `boi status` — two separate views |
| Iteration | Edit files directly with any editor | No audit trail of changes, no status tracking |
| Consistency | Filesystem is simple and reliable | Race conditions possible if daemon scans drafts dir; no transactional guarantees |

**Verdict:** This approach is appealing for its simplicity but fundamentally breaks the "single pane of glass" UX goal. Mike's framing — "keep our drafts there and iterate on them" — implies drafts should live alongside queued specs, not in a separate location. The inability to assign queue IDs to drafts means dependencies cannot reference them, which eliminates half the feature. File-system drafts would work as a lightweight "spec scratchpad" but fall short of the integrated draft lifecycle we need.

---

### Alternative B: Tag-Based Approach Instead of Lifecycle States

**Concept:** Instead of adding a `draft` status to the state machine, add a generic tagging system to specs. A "draft" is simply a spec tagged `#draft` that `pick_next_spec()` is taught to skip. Tags are stored as a JSON array in a new `tags TEXT` column on the `specs` table.

**How it would work:**

```sql
-- Schema addition
ALTER TABLE specs ADD COLUMN tags TEXT DEFAULT '[]';

-- A draft is a queued spec with a #draft tag
INSERT INTO specs (queue_id, status, tags) VALUES ('q-047', 'queued', '["#draft"]');
```

```
# Create a draft via tag
boi dispatch spec.md --tag draft
# → q-047 dispatched (queued, tags: #draft)

# List tagged specs
boi queue --tag draft
# → Shows only specs tagged #draft

# Promote = remove the tag
boi tag remove q-047 draft
# → q-047 is now a normal queued spec

# Users could define custom tags
boi dispatch spec.md --tag wip --tag needs-review
```

**pick_next_spec() change:**

```python
def pick_next_spec(self, ...) -> Optional[dict]:
    # Added: exclude specs with #draft tag
    query = """
        SELECT * FROM specs
        WHERE status IN ('queued', 'requeued')
        AND (tags IS NULL OR tags NOT LIKE '%"#draft"%')
        -- ... existing dep checks ...
        ORDER BY priority ASC, created_at ASC
    """
```

**Trade-offs:**

| Aspect | Pro | Con |
|--------|-----|-----|
| Flexibility | Arbitrary tags for any workflow (`#wip`, `#blocked`, `#review`) | Over-engineered for the immediate need — only `#draft` is required now |
| Extensibility | Future workflows can use tags without schema changes | Tag proliferation risk — unclear which tags have special meaning vs. are informational |
| Query complexity | Tags can be combined in filters | JSON-in-SQL queries are awkward in SQLite (`LIKE` or `json_each()`) and slower than status checks |
| Semantics | Consistent with modern issue tracker UX (GitHub labels) | Blurs the line between spec state (lifecycle) and metadata (labels) — a draft is a lifecycle state, not a label |
| Daemon awareness | Tags are just data; daemon logic stays simple | Daemon must parse JSON to understand tags — fragile; new "magic" tags require code changes anyway |
| Migration | Additive column, no CHECK constraint changes | Existing queries must all handle the new column; tests must account for tag filtering |

**Verdict:** Tags are a powerful abstraction, but they solve a different problem. The draft lifecycle is fundamentally a **state machine transition** — a spec progresses from "not ready" to "ready" to "running" to "done." Encoding this as a tag makes the state machine implicit rather than explicit, which is harder to reason about and debug. A spec with `status=queued` and `tag=#draft` has contradictory semantics: is it queued or isn't it? The recommended approach keeps the state machine clean and explicit. If BOI needs tags in the future (and it likely will), they can be added as an orthogonal feature that coexists with the draft status.

---

### Alternative C: Separate Draft Table

**Concept:** Create a dedicated `drafts` table with its own schema, separate from `specs`. Promotion means INSERTing into `specs` and DELETEing from `drafts`. The two tables share a similar schema but are managed independently.

**How it would work:**

```sql
CREATE TABLE drafts (
    draft_id TEXT PRIMARY KEY,        -- e.g., 'd-001'
    spec_path TEXT NOT NULL,
    mode TEXT DEFAULT 'execute',
    priority INTEGER DEFAULT 100,
    blocked_by TEXT DEFAULT '[]',     -- JSON array of queue IDs
    project TEXT,
    created_at TEXT DEFAULT (datetime('now')),
    updated_at TEXT DEFAULT (datetime('now')),
    notes TEXT                        -- iteration notes
);
```

```
# Create a draft
boi draft spec.md
# → d-001 created (draft)

# Promote to queue
boi promote d-001
# → d-001 promoted → q-047 (queued)
# (Internally: INSERT INTO specs ... ; DELETE FROM drafts WHERE draft_id='d-001')

# List drafts
boi drafts
# → d-001  spec.md  (priority: 100, blocked-by: q-045)
```

**Trade-offs:**

| Aspect | Pro | Con |
|--------|-----|-----|
| Separation of concerns | Clear boundary: drafts are not specs | Duplicated schema — changes to `specs` must be mirrored in `drafts` |
| ID namespace | Drafts get their own ID series (`d-NNN`) — unambiguous | ID changes on promotion (`d-001` → `q-047`) — breaks any references to the old ID |
| Dependencies | Drafts can declare deps on specs | Cross-table dep references are complex — can a spec depend on a draft? Requires JOIN across tables |
| Querying | "Show all drafts" is a simple `SELECT * FROM drafts` | "Show all work items" requires UNION across both tables — every listing command gets complex |
| Promotion | Clean cut: INSERT + DELETE is atomic in a transaction | Loss of history — the draft's creation time, edit history, etc., must be explicitly copied |
| Demotion | Would require reverse: INSERT into drafts + DELETE from specs | Complex and error-prone; running/completed specs can't be demoted |

**Verdict:** Separate tables provide clean separation but introduce significant accidental complexity. The ID-change-on-promotion problem is particularly painful — if spec B says `blocked_by: d-001` and `d-001` gets promoted to `q-047`, the reference breaks. Solving this requires an ID mapping table or using stable IDs across both tables, which negates the benefit of separation. The UNION queries for "show everything" make every display command more complex. The recommended approach — a single `specs` table with a `draft` status — avoids all of these problems because a draft IS a spec, just one that isn't ready to run yet. One table, one ID namespace, one query surface.

---

### Comparison Matrix

| Criterion | Recommended (DB Status) | Alt A (Filesystem) | Alt B (Tags) | Alt C (Separate Table) |
|-----------|:----------------------:|:------------------:|:------------:|:---------------------:|
| Schema complexity | Low (add one value to CHECK) | None | Medium (new column + JSON) | High (new table) |
| Query simplicity | High (WHERE status) | N/A (no DB) | Medium (JSON parsing) | Low (UNION queries) |
| Dependency support | Full (same ID namespace) | None | Full | Partial (cross-table refs) |
| Draft-to-queue transition | UPDATE one column | File move + INSERT | UPDATE JSON column | INSERT + DELETE |
| `boi queue` integration | Native | Requires parallel listing | Native (with filtering) | Requires UNION |
| State machine clarity | Explicit new state | N/A | Implicit (tag = state) | Split across tables |
| Backward compatibility | High (additive) | High (separate system) | Medium (new column on all queries) | Medium (new table, new commands) |
| Implementation effort | ~30 min | ~1 hour | ~2 hours | ~3 hours |

### Overall Recommendation

The in-database `draft` status is the clear winner because it:

1. **Extends rather than reinvents** — leverages the existing `specs` table, `spec_dependencies` table, and `pick_next_spec()` logic with minimal changes.
2. **Maintains a single source of truth** — one table, one ID namespace, one set of queries.
3. **Keeps the state machine explicit** — `draft` is a real lifecycle state, not a tag or a separate entity.
4. **Minimizes migration risk** — adding a value to a CHECK constraint (or dropping it) is the simplest possible schema change.
5. **Aligns with Mike's vision** — "keep our drafts there" implies drafts live alongside queued specs, not in a parallel system.

## 10. Migration & Rollout

### 10.1 SQLite CHECK Constraint Migration

SQLite does not support `ALTER TABLE ... ALTER CONSTRAINT` or `ALTER TABLE ... DROP CONSTRAINT`. When a CHECK constraint needs a new value, there are two options:

**Option A: Drop the CHECK constraint (Recommended)**

Remove the CHECK constraint entirely and rely on application-level validation in `db.py`. This is the pragmatic choice for several reasons:

1. **BOI is a single-user tool** — there's no risk of external actors inserting invalid status values. All writes go through `db.py` methods, which already validate transitions.
2. **Avoids table recreation** — dropping a CHECK is far simpler than recreating the table with a new CHECK, copying data, rebuilding indexes, and fixing foreign keys.
3. **Future-proof** — adding more statuses later won't require another migration.
4. **SQLite's CHECK is a weak guarantee** — it doesn't prevent direct SQL from bypassing it, and BOI already validates at the application layer.

Implementation: A one-time migration that recreates the `specs` table without the CHECK constraint.

```python
def migrate_drop_status_check(db: Database) -> None:
    """Remove the CHECK constraint on specs.status.

    SQLite can't ALTER constraints, so this recreates the table.
    Performed in a single transaction for atomicity.
    """
    with db.lock:
        db.conn.execute("BEGIN IMMEDIATE")
        try:
            # 1. Create new table without CHECK
            db.conn.execute("""
                CREATE TABLE specs_new (
                    id TEXT PRIMARY KEY,
                    spec_path TEXT NOT NULL,
                    original_spec_path TEXT,
                    worktree TEXT,
                    priority INTEGER DEFAULT 100,
                    status TEXT NOT NULL DEFAULT 'queued',
                    phase TEXT DEFAULT 'execute',
                    submitted_at TEXT,
                    started_at TEXT,
                    completed_at TEXT,
                    iteration INTEGER DEFAULT 0,
                    max_iterations INTEGER DEFAULT 10,
                    worker TEXT,
                    sync_back INTEGER DEFAULT 1,
                    project TEXT,
                    initial_task_ids TEXT DEFAULT '[]',
                    error TEXT
                )
            """)
            # 2. Copy all data
            db.conn.execute("""
                INSERT INTO specs_new
                SELECT * FROM specs
            """)
            # 3. Drop old table and rename
            db.conn.execute("DROP TABLE specs")
            db.conn.execute("ALTER TABLE specs_new RENAME TO specs")
            # 4. Rebuild indexes
            db.conn.execute("""
                CREATE INDEX IF NOT EXISTS idx_specs_status
                ON specs(status)
            """)
            db.conn.execute("""
                CREATE INDEX IF NOT EXISTS idx_specs_priority
                ON specs(priority)
            """)
            db.conn.commit()
        except Exception:
            db.conn.rollback()
            raise
```

**Option B: Recreate table with updated CHECK (Not recommended)**

```sql
-- Same process as Option A, but the new table includes:
CHECK (status IN ('draft','queued','assigning','running','completed',
                  'failed','canceled','needs_review','requeued'))
```

This is safer in theory (the DB enforces valid values) but adds ongoing maintenance cost — every new status requires another migration. For a single-user CLI tool with application-level validation, this is unnecessary.

**Migration trigger:** The migration runs automatically when `Database.__init__()` detects that the `specs` table still has a CHECK constraint. Detection can be done via:

```python
def _needs_status_check_migration(self) -> bool:
    """Check if the specs table still has a status CHECK constraint."""
    table_info = self.conn.execute(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='specs'"
    ).fetchone()
    if table_info is None:
        return False
    return "CHECK" in table_info["sql"] and "'draft'" not in table_info["sql"]
```

### 10.2 Backward Compatibility

This feature is fully backward-compatible with existing BOI installations:

| Aspect | Impact | Details |
|--------|--------|---------|
| **Existing specs** | None | All existing specs have status `queued`/`running`/etc. — no change |
| **`boi dispatch`** | None | Default behavior unchanged. `--draft` and `--after` are new opt-in flags |
| **`boi queue`** | Additive | Drafts appear with a `[draft]` label. Users who don't use drafts see no difference |
| **`boi status`** | Additive | Draft count appears in summary. No visual change if count is zero |
| **`boi spec` commands** | None | `spec edit`, `spec add`, `spec skip` work on drafts identically to queued specs |
| **Daemon behavior** | None | `pick_next_spec()` already excludes non-`queued`/`requeued` statuses. Drafts are invisible to the daemon |
| **`spec_dependencies` table** | None | Already exists. New CLI surface exposes existing functionality |
| **Spec file format** | Additive | `**Blocked-By:**` header is optional and ignored if not present |
| **SQLite database** | One-time migration | CHECK constraint dropped; no data changes |

**Key guarantee:** A user who never uses `--draft`, `--after`, `boi promote`, `boi demote`, `boi dep`, or `boi deps` will see zero behavioral changes.

### 10.3 Rollout Phases

The implementation is split into three phases, each independently deployable and valuable:

**Phase 1: Draft Lifecycle (Core)**
- Schema migration: drop CHECK constraint on `specs.status`
- `db.py`: `as_draft` param on `enqueue()`, `promote()`, `demote()` methods
- `boi.sh`: `--draft` flag on `dispatch`, `promote` and `demote` subcommands
- `cli_ops.py`: Wire up draft dispatch, promotion, demotion
- `status.py`: Show `[draft]` label in queue listings

Estimated scope: ~150 lines Python, ~40 lines bash.

**Phase 2: Dependency CLI Surface**
- `db.py`: `add_dependency()`, `remove_dependency()`, `get_dependency_chain()`, `detect_cycle()`
- `boi.sh`: `--after` flag on `dispatch`, `dep` and `deps` subcommands
- `cli_ops.py`: Wire up dependency operations
- `status.py`: Show "blocked by q-NNN [status]" in queue listings

Estimated scope: ~120 lines Python, ~50 lines bash.

**Phase 3: Status Display Enhancements**
- `status.py`: Dependency chain in `boi status` dashboard
- `status.py`: Starvation warnings (high-priority spec blocked by failing dep)
- `status.py`: Draft section or dimmed presentation in live dashboard

Estimated scope: ~80 lines Python.

**Total estimated scope:** ~350 lines Python, ~90 lines bash.

### 10.4 Rollback Plan

If the feature causes issues, rollback is straightforward at each phase:

**Phase 1 rollback — Mass-promote all drafts:**

```sql
-- Promote all drafts back to queued (run via sqlite3 CLI)
UPDATE specs SET status = 'queued' WHERE status = 'draft';
```

This is a single SQL statement. All drafts become normal queued specs. No data loss.

**Phase 2 rollback — Clear all dependencies:**

```sql
-- Remove all dependency relationships
DELETE FROM spec_dependencies;
```

Specs that were blocked become immediately eligible for `pick_next_spec()`. No data loss — the specs themselves are untouched.

**Full rollback — Revert code:**

```bash
# Git revert the feature commits (one per phase)
git revert <phase-3-commit>
git revert <phase-2-commit>
git revert <phase-1-commit>

# Clean up database state
sqlite3 ~/.boi/state/boi.db "UPDATE specs SET status='queued' WHERE status='draft';"
sqlite3 ~/.boi/state/boi.db "DELETE FROM spec_dependencies;"
```

**Recovery from bad migration:** If the CHECK constraint migration fails mid-way (power loss, disk full), SQLite's WAL mode and the `BEGIN IMMEDIATE` transaction ensure atomicity — either the full migration completes or none of it does. The original `specs` table remains intact.

### 10.5 Testing the Migration

Before deploying, the migration should be tested against a copy of the production database:

```bash
# 1. Back up
cp ~/.boi/state/boi.db ~/.boi/state/boi.db.backup

# 2. Run migration on copy
cp ~/.boi/state/boi.db /tmp/boi-test.db
python3 -c "
from lib.db import Database
db = Database('/tmp/boi-test-state')
# Run migration...
# Verify all specs are intact
specs = db.conn.execute('SELECT count(*) FROM specs').fetchone()[0]
print(f'{specs} specs migrated successfully')
"

# 3. Verify existing functionality still works
boi queue   # Should show same specs as before
boi status  # Dashboard should render normally
```

## Appendix: Test Plan

This appendix provides a comprehensive test matrix for the draft specs and dependency chains feature. Tests follow the existing project conventions: `unittest`-based classes extending `DbTestCase`/`CrudTestCase`, using `_make_spec_file()` and `_make_spec_file_named()` helpers for spec creation, and asserting against database state via direct SQL queries or `db.get_spec()`.

All tests use mock data only — no live API calls or daemon processes.

### A.1 Unit Tests (db.py methods)

These tests belong in `~/.boi/src/tests/test_db.py`, grouped by feature area.

---

#### `test_enqueue_as_draft`

**Purpose:** Verify that `enqueue(as_draft=True)` creates a spec with `'draft'` status.

```python
class TestDraftEnqueue(CrudTestCase):

    def test_enqueue_as_draft(self) -> None:
        """enqueue(as_draft=True) sets status to 'draft'."""
        spec = self._make_spec_file()
        result = self.db.enqueue(spec, as_draft=True)
        self.assertEqual(result["status"], "draft")
        self.assertEqual(result["id"], "q-001")

        # Verify in DB
        row = self.db.conn.execute(
            "SELECT status FROM specs WHERE id = 'q-001'"
        ).fetchone()
        self.assertEqual(row["status"], "draft")

    def test_enqueue_as_draft_logs_drafted_event(self) -> None:
        """Drafting a spec logs a 'drafted' event, not 'queued'."""
        spec = self._make_spec_file()
        self.db.enqueue(spec, as_draft=True)
        cursor = self.db.conn.execute(
            "SELECT event_type FROM events WHERE spec_id = 'q-001'"
        )
        event_types = [row["event_type"] for row in cursor]
        self.assertIn("drafted", event_types)
        self.assertNotIn("queued", event_types)

    def test_enqueue_default_is_not_draft(self) -> None:
        """Default enqueue still creates 'queued' status."""
        spec = self._make_spec_file()
        result = self.db.enqueue(spec)
        self.assertEqual(result["status"], "queued")
```

---

#### `test_promote_draft`

**Purpose:** Verify that `promote()` transitions a draft to queued.

```python
class TestPromote(CrudTestCase):

    def test_promote_draft(self) -> None:
        """promote() changes draft to queued."""
        spec = self._make_spec_file()
        self.db.enqueue(spec, as_draft=True)
        result = self.db.promote("q-001")
        self.assertEqual(result["status"], "queued")

        # Verify in DB
        row = self.db.conn.execute(
            "SELECT status FROM specs WHERE id = 'q-001'"
        ).fetchone()
        self.assertEqual(row["status"], "queued")

    def test_promote_logs_event(self) -> None:
        """promote() logs a 'promoted' event."""
        spec = self._make_spec_file()
        self.db.enqueue(spec, as_draft=True)
        self.db.promote("q-001")
        cursor = self.db.conn.execute(
            "SELECT event_type FROM events WHERE spec_id = 'q-001'"
        )
        event_types = [row["event_type"] for row in cursor]
        self.assertIn("promoted", event_types)
```

---

#### `test_promote_non_draft_fails`

**Purpose:** Promoting a non-draft spec raises `ValueError`.

```python
    def test_promote_non_draft_fails(self) -> None:
        """promote() on a queued spec raises ValueError."""
        spec = self._make_spec_file()
        self.db.enqueue(spec)  # status = 'queued'
        with self.assertRaises(ValueError) as ctx:
            self.db.promote("q-001")
        self.assertIn("not a draft", str(ctx.exception))

    def test_promote_nonexistent_fails(self) -> None:
        """promote() on a non-existent spec raises ValueError."""
        with self.assertRaises(ValueError) as ctx:
            self.db.promote("q-999")
        self.assertIn("does not exist", str(ctx.exception))

    def test_promote_completed_fails(self) -> None:
        """promote() on a completed spec raises ValueError."""
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.pick_next_spec()
        self.db.set_running("q-001", "w-1")
        self.db.complete("q-001")
        with self.assertRaises(ValueError):
            self.db.promote("q-001")
```

---

#### `test_demote_queued`

**Purpose:** Verify that `demote()` transitions queued to draft.

```python
class TestDemote(CrudTestCase):

    def test_demote_queued(self) -> None:
        """demote() changes queued to draft."""
        spec = self._make_spec_file()
        self.db.enqueue(spec)  # status = 'queued'
        result = self.db.demote("q-001")
        self.assertEqual(result["status"], "draft")

    def test_demote_logs_event(self) -> None:
        """demote() logs a 'demoted' event."""
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.demote("q-001")
        cursor = self.db.conn.execute(
            "SELECT event_type FROM events WHERE spec_id = 'q-001'"
        )
        event_types = [row["event_type"] for row in cursor]
        self.assertIn("demoted", event_types)
```

---

#### `test_demote_running_fails`

**Purpose:** Demoting a running spec raises `ValueError`.

```python
    def test_demote_running_fails(self) -> None:
        """demote() on a running spec raises ValueError."""
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        self.db.pick_next_spec()
        self.db.set_running("q-001", "w-1")
        with self.assertRaises(ValueError) as ctx:
            self.db.demote("q-001")
        self.assertIn("cannot be demoted", str(ctx.exception).lower())

    def test_demote_already_draft_fails(self) -> None:
        """demote() on a draft spec raises ValueError."""
        spec = self._make_spec_file()
        self.db.enqueue(spec, as_draft=True)
        with self.assertRaises(ValueError) as ctx:
            self.db.demote("q-001")
        self.assertIn("already a draft", str(ctx.exception))

    def test_demote_nonexistent_fails(self) -> None:
        """demote() on a non-existent spec raises ValueError."""
        with self.assertRaises(ValueError):
            self.db.demote("q-999")
```

---

#### `test_add_dependency`

**Purpose:** Verify `add_dependency()` inserts into `spec_dependencies`.

```python
class TestAddDependency(CrudTestCase):

    def test_add_dependency(self) -> None:
        """add_dependency() inserts a row into spec_dependencies."""
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1)
        self.db.enqueue(s2)
        self.db.add_dependency("q-002", "q-001")

        row = self.db.conn.execute(
            "SELECT * FROM spec_dependencies "
            "WHERE spec_id = 'q-002' AND blocks_on = 'q-001'"
        ).fetchone()
        self.assertIsNotNone(row)

    def test_add_dependency_idempotent(self) -> None:
        """Adding the same dependency twice doesn't raise."""
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1)
        self.db.enqueue(s2)
        self.db.add_dependency("q-002", "q-001")
        self.db.add_dependency("q-002", "q-001")  # No error

        count = self.db.conn.execute(
            "SELECT COUNT(*) FROM spec_dependencies "
            "WHERE spec_id = 'q-002' AND blocks_on = 'q-001'"
        ).fetchone()[0]
        self.assertEqual(count, 1)

    def test_add_dependency_nonexistent_target_fails(self) -> None:
        """add_dependency() raises ValueError if target doesn't exist."""
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        with self.assertRaises(ValueError) as ctx:
            self.db.add_dependency("q-001", "q-999")
        self.assertIn("does not exist", str(ctx.exception))

    def test_add_dependency_to_running_spec_fails(self) -> None:
        """Cannot add dependency to a running spec."""
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1)
        self.db.enqueue(s2)
        self.db.pick_next_spec()
        self.db.set_running("q-001", "w-1")
        with self.assertRaises(ValueError) as ctx:
            self.db.add_dependency("q-001", "q-002")
        self.assertIn("cannot add dependency", str(ctx.exception).lower())

    def test_add_dependency_logs_event(self) -> None:
        """add_dependency() logs a 'dep_added' event."""
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1)
        self.db.enqueue(s2)
        self.db.add_dependency("q-002", "q-001")
        cursor = self.db.conn.execute(
            "SELECT event_type FROM events WHERE spec_id = 'q-002'"
        )
        event_types = [row["event_type"] for row in cursor]
        self.assertIn("dep_added", event_types)
```

---

#### `test_remove_dependency`

**Purpose:** Verify `remove_dependency()` deletes from `spec_dependencies`.

```python
class TestRemoveDependency(CrudTestCase):

    def test_remove_dependency(self) -> None:
        """remove_dependency() deletes the row from spec_dependencies."""
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1)
        self.db.enqueue(s2, blocked_by=["q-001"])
        self.db.remove_dependency("q-002", "q-001")

        row = self.db.conn.execute(
            "SELECT * FROM spec_dependencies "
            "WHERE spec_id = 'q-002' AND blocks_on = 'q-001'"
        ).fetchone()
        self.assertIsNone(row)

    def test_remove_nonexistent_dependency_fails(self) -> None:
        """remove_dependency() raises ValueError if edge doesn't exist."""
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        with self.assertRaises(ValueError) as ctx:
            self.db.remove_dependency("q-001", "q-999")
        self.assertIn("No dependency", str(ctx.exception))

    def test_remove_dependency_logs_event(self) -> None:
        """remove_dependency() logs a 'dep_removed' event."""
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1)
        self.db.enqueue(s2, blocked_by=["q-001"])
        self.db.remove_dependency("q-002", "q-001")
        cursor = self.db.conn.execute(
            "SELECT event_type FROM events WHERE spec_id = 'q-002'"
        )
        event_types = [row["event_type"] for row in cursor]
        self.assertIn("dep_removed", event_types)
```

---

#### `test_circular_dependency_detected`

**Purpose:** Verify `detect_cycle()` and `add_dependency()` reject cycles.

```python
class TestCircularDependency(CrudTestCase):

    def test_circular_dependency_detected(self) -> None:
        """A→B→A cycle is rejected by add_dependency()."""
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1)
        self.db.enqueue(s2)
        self.db.add_dependency("q-002", "q-001")  # q-002 depends on q-001
        with self.assertRaises(ValueError) as ctx:
            self.db.add_dependency("q-001", "q-002")  # Would create cycle
        self.assertIn("ircular dependency", str(ctx.exception))

    def test_self_dependency_rejected(self) -> None:
        """A spec cannot depend on itself."""
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        with self.assertRaises(ValueError) as ctx:
            self.db.add_dependency("q-001", "q-001")
        self.assertIn("ircular dependency", str(ctx.exception))

    def test_transitive_cycle_detected(self) -> None:
        """A→B→C→A cycle is rejected."""
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        s3 = self._make_spec_file_named("s3.md")
        self.db.enqueue(s1)
        self.db.enqueue(s2)
        self.db.enqueue(s3)
        self.db.add_dependency("q-002", "q-001")  # B depends on A
        self.db.add_dependency("q-003", "q-002")  # C depends on B
        with self.assertRaises(ValueError) as ctx:
            self.db.add_dependency("q-001", "q-003")  # A depends on C → cycle
        self.assertIn("ircular dependency", str(ctx.exception))

    def test_detect_cycle_returns_false_for_valid_chain(self) -> None:
        """A→B, C→B is not a cycle (diamond without closure)."""
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        s3 = self._make_spec_file_named("s3.md")
        self.db.enqueue(s1)
        self.db.enqueue(s2)
        self.db.enqueue(s3)
        self.db.add_dependency("q-002", "q-001")  # B depends on A
        # C depending on A is fine (no cycle)
        self.assertFalse(self.db.detect_cycle("q-003", "q-001"))
```

---

#### `test_pick_next_spec_skips_drafts`

**Purpose:** Verify `pick_next_spec()` never selects draft specs.

```python
class TestPickNextSpecDrafts(CrudTestCase):

    def test_pick_next_spec_skips_drafts(self) -> None:
        """Drafts are never picked by pick_next_spec()."""
        spec = self._make_spec_file()
        self.db.enqueue(spec, as_draft=True)
        result = self.db.pick_next_spec()
        self.assertIsNone(result)

    def test_pick_next_spec_skips_draft_picks_queued(self) -> None:
        """With a draft and a queued spec, only the queued one is picked."""
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1, as_draft=True)
        self.db.enqueue(s2)  # queued
        result = self.db.pick_next_spec()
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], "q-002")

    def test_promoted_draft_becomes_pickable(self) -> None:
        """After promotion, a former draft can be picked."""
        spec = self._make_spec_file()
        self.db.enqueue(spec, as_draft=True)
        self.assertIsNone(self.db.pick_next_spec())
        self.db.promote("q-001")
        result = self.db.pick_next_spec()
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], "q-001")
```

---

#### `test_pick_next_spec_skips_blocked`

**Purpose:** Verify `pick_next_spec()` skips specs with unmet dependencies.

```python
class TestPickNextSpecBlocked(CrudTestCase):

    def test_pick_next_spec_skips_blocked(self) -> None:
        """A spec blocked by an incomplete dependency is not picked."""
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1)
        self.db.enqueue(s2, blocked_by=["q-001"])
        # Only q-001 should be picked (q-002 is blocked)
        result = self.db.pick_next_spec()
        self.assertEqual(result["id"], "q-001")
```

Note: This test already exists in the codebase as `test_skips_spec_with_unfinished_dependency`. Including it here for completeness of the test matrix.

---

#### `test_pick_next_spec_unblocks_after_dep_completes`

**Purpose:** Verify that completing a dependency makes the dependent eligible.

```python
    def test_pick_next_spec_unblocks_after_dep_completes(self) -> None:
        """After dep completes, the dependent becomes eligible for picking."""
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1)
        self.db.enqueue(s2, blocked_by=["q-001"])

        # Complete q-001
        self.db.pick_next_spec()
        self.db.set_running("q-001", "w-1")
        self.db.complete("q-001")

        # Now q-002 should be pickable
        result = self.db.pick_next_spec()
        self.assertIsNotNone(result)
        self.assertEqual(result["id"], "q-002")
```

Note: This test already exists as `test_picks_spec_after_dependency_completed`. Including for matrix completeness.

---

#### Additional Unit Tests

```python
class TestGetDependencyChain(CrudTestCase):

    def test_get_dependency_chain_upstream(self) -> None:
        """get_dependency_chain() returns upstream dependencies."""
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1)
        self.db.enqueue(s2, blocked_by=["q-001"])
        chain = self.db.get_dependency_chain("q-002")
        upstream_ids = [d["id"] for d in chain["upstream"]]
        self.assertIn("q-001", upstream_ids)

    def test_get_dependency_chain_downstream(self) -> None:
        """get_dependency_chain() returns downstream dependents."""
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1)
        self.db.enqueue(s2, blocked_by=["q-001"])
        chain = self.db.get_dependency_chain("q-001")
        downstream_ids = [d["id"] for d in chain["downstream"]]
        self.assertIn("q-002", downstream_ids)

    def test_get_dependency_chain_no_deps(self) -> None:
        """Spec with no deps returns empty upstream and downstream."""
        spec = self._make_spec_file()
        self.db.enqueue(spec)
        chain = self.db.get_dependency_chain("q-001")
        self.assertEqual(chain["upstream"], [])
        self.assertEqual(chain["downstream"], [])


class TestDraftWithDependencies(CrudTestCase):

    def test_draft_with_dependency_stays_blocked_after_promote(self) -> None:
        """A promoted draft with unmet deps is queued but not pickable."""
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1)
        self.db.enqueue(s2, as_draft=True, blocked_by=["q-001"])
        self.db.promote("q-002")

        # q-002 is now queued but blocked by q-001
        spec = self.db.get_spec("q-002")
        self.assertEqual(spec["status"], "queued")

        # pick_next_spec should pick q-001, not q-002
        result = self.db.pick_next_spec()
        self.assertEqual(result["id"], "q-001")

    def test_demote_then_repromote_roundtrip(self) -> None:
        """A spec can be promoted, demoted, and promoted again."""
        spec = self._make_spec_file()
        self.db.enqueue(spec, as_draft=True)
        self.db.promote("q-001")
        self.assertEqual(self.db.get_spec("q-001")["status"], "queued")
        self.db.demote("q-001")
        self.assertEqual(self.db.get_spec("q-001")["status"], "draft")
        self.db.promote("q-001")
        self.assertEqual(self.db.get_spec("q-001")["status"], "queued")

    def test_enqueue_draft_with_blocked_by(self) -> None:
        """A draft can be created with blocked_by dependencies."""
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1)
        result = self.db.enqueue(s2, as_draft=True, blocked_by=["q-001"])
        self.assertEqual(result["status"], "draft")

        # Verify dep exists
        row = self.db.conn.execute(
            "SELECT * FROM spec_dependencies "
            "WHERE spec_id = 'q-002' AND blocks_on = 'q-001'"
        ).fetchone()
        self.assertIsNotNone(row)
```

---

### A.2 Integration Tests (CLI Level)

These tests verify end-to-end behavior through the CLI commands. They belong in `~/.boi/src/tests/integration/test_draft_deps.py` and shell out to `boi.sh` or call `cli_ops` functions directly.

---

#### `test_dispatch_as_draft_cli`

**Purpose:** `boi dispatch spec.md --draft` creates a draft in the database.

```python
class TestDraftDepsCLI(CrudTestCase):

    def test_dispatch_as_draft_cli(self) -> None:
        """boi dispatch --draft creates a spec with 'draft' status."""
        spec = self._make_spec_file()
        # Simulate what the CLI does: call cli_ops.dispatch() with as_draft=True
        from lib.cli_ops import dispatch
        result = dispatch(
            spec_path=spec,
            db=self.db,
            as_draft=True,
        )
        self.assertEqual(result["status"], "draft")
        # Verify in DB
        row = self.db.conn.execute(
            "SELECT status FROM specs WHERE id = ?", (result["id"],)
        ).fetchone()
        self.assertEqual(row["status"], "draft")
```

---

#### `test_promote_cli`

**Purpose:** `boi promote q-NNN` transitions draft to queued.

```python
    def test_promote_cli(self) -> None:
        """boi promote transitions a draft to queued."""
        spec = self._make_spec_file()
        entry = self.db.enqueue(spec, as_draft=True)
        from lib.cli_ops import promote
        result = promote(spec_id=entry["id"], db=self.db)
        self.assertEqual(result["status"], "queued")
```

---

#### `test_dispatch_with_after_cli`

**Purpose:** `boi dispatch spec.md --after q-NNN` creates a dependency.

```python
    def test_dispatch_with_after_cli(self) -> None:
        """boi dispatch --after creates a dependency."""
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        r1 = self.db.enqueue(s1)
        from lib.cli_ops import dispatch
        r2 = dispatch(
            spec_path=s2,
            db=self.db,
            blocked_by=[r1["id"]],
        )
        # Verify dep in DB
        row = self.db.conn.execute(
            "SELECT * FROM spec_dependencies "
            "WHERE spec_id = ? AND blocks_on = ?",
            (r2["id"], r1["id"]),
        ).fetchone()
        self.assertIsNotNone(row)
```

---

#### `test_deps_shows_chain`

**Purpose:** `boi deps q-NNN` returns upstream and downstream info.

```python
    def test_deps_shows_chain(self) -> None:
        """boi deps returns dependency chain info."""
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        s3 = self._make_spec_file_named("s3.md")
        self.db.enqueue(s1)
        self.db.enqueue(s2, blocked_by=["q-001"])
        self.db.enqueue(s3, blocked_by=["q-002"])

        chain = self.db.get_dependency_chain("q-002")
        upstream_ids = [d["id"] for d in chain["upstream"]]
        downstream_ids = [d["id"] for d in chain["downstream"]]

        self.assertIn("q-001", upstream_ids)
        self.assertIn("q-003", downstream_ids)
```

---

#### `test_queue_shows_drafts_with_label`

**Purpose:** `boi queue` output includes drafts with a distinguishing label.

```python
    def test_queue_shows_drafts_with_label(self) -> None:
        """Queue listing includes drafts alongside queued specs."""
        s1 = self._make_spec_file_named("s1.md")
        s2 = self._make_spec_file_named("s2.md")
        self.db.enqueue(s1)
        self.db.enqueue(s2, as_draft=True)

        # Verify both specs appear in the queue with their statuses
        rows = self.db.conn.execute(
            "SELECT id, status FROM specs ORDER BY id"
        ).fetchall()
        statuses = {row["id"]: row["status"] for row in rows}
        self.assertEqual(statuses["q-001"], "queued")
        self.assertEqual(statuses["q-002"], "draft")
```

---

### A.3 Verification Matrix

| # | Test Name | Category | Setup | Action | Assertion |
|---|-----------|----------|-------|--------|-----------|
| 1 | `test_enqueue_as_draft` | Unit | Create spec file | `db.enqueue(spec, as_draft=True)` | `result["status"] == "draft"` |
| 2 | `test_promote_draft` | Unit | Enqueue as draft | `db.promote("q-001")` | `result["status"] == "queued"` |
| 3 | `test_promote_non_draft_fails` | Unit | Enqueue as queued | `db.promote("q-001")` | Raises `ValueError` |
| 4 | `test_demote_queued` | Unit | Enqueue as queued | `db.demote("q-001")` | `result["status"] == "draft"` |
| 5 | `test_demote_running_fails` | Unit | Enqueue → pick → set_running | `db.demote("q-001")` | Raises `ValueError` |
| 6 | `test_add_dependency` | Unit | Enqueue 2 specs | `db.add_dependency("q-002", "q-001")` | Row in `spec_dependencies` |
| 7 | `test_remove_dependency` | Unit | Enqueue 2 specs + add dep | `db.remove_dependency("q-002", "q-001")` | Row deleted |
| 8 | `test_circular_dependency_detected` | Unit | Enqueue 2 specs + A→B dep | `db.add_dependency("q-001", "q-002")` | Raises `ValueError` with "circular" |
| 9 | `test_pick_next_spec_skips_drafts` | Unit | Enqueue as draft only | `db.pick_next_spec()` | Returns `None` |
| 10 | `test_pick_next_spec_skips_blocked` | Unit | Enqueue 2, q-002 blocked by q-001 | `db.pick_next_spec()` | Returns q-001 |
| 11 | `test_pick_next_spec_unblocks_after_dep_completes` | Unit | Enqueue 2, complete q-001 | `db.pick_next_spec()` | Returns q-002 |
| 12 | `test_dispatch_as_draft_cli` | Integration | Create spec file | `dispatch(spec, as_draft=True)` | DB status is `"draft"` |
| 13 | `test_promote_cli` | Integration | Enqueue as draft | `promote(spec_id)` | DB status is `"queued"` |
| 14 | `test_dispatch_with_after_cli` | Integration | Enqueue spec A | `dispatch(spec_b, blocked_by=[A])` | Row in `spec_dependencies` |
| 15 | `test_deps_shows_chain` | Integration | 3-spec chain | `get_dependency_chain("q-002")` | Shows upstream + downstream |
| 16 | `test_queue_shows_drafts_with_label` | Integration | 1 queued + 1 draft | Query specs table | Both statuses present |

### A.4 Coverage Targets

| Area | Test Count | Critical Path? |
|------|-----------|----------------|
| Draft enqueue | 3 | Yes |
| Promote | 4 | Yes |
| Demote | 4 | Yes |
| Add dependency | 5 | Yes |
| Remove dependency | 3 | No |
| Cycle detection | 4 | Yes |
| pick_next_spec (drafts) | 3 | Yes |
| pick_next_spec (blocked) | 2 | Yes (existing tests cover this) |
| Dependency chain | 3 | No |
| Draft + dep combos | 3 | Yes |
| CLI integration | 5 | Yes |
| **Total** | **39** | |

Estimated implementation effort: ~300 lines of test code for unit tests, ~100 lines for integration tests. All tests use in-memory SQLite databases (via `tempfile.TemporaryDirectory`) and require no external services.
