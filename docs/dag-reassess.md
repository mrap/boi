# DAG Reassessment: `boi plan` + `boi dispatch-many`

## Why this exists

Before this feature, dependency ordering between BOI specs was managed entirely by hand: whoever dispatched a batch of specs had to remember which in-flight spec IDs each new spec depended on, then manually thread `--after` flags at the right positions. Three failure modes:

1. **Wrong ordering** — only surfaces when the dependent spec fails mid-execution (tokens and time already spent)
2. **Implicit artifact deps** — spec B reads a file that spec A writes; neither spec declares this; B starts in parallel with A and fails
3. **Scope drift** — spec A's scope expands after B was queued, breaking B's assumed contract

This feature makes dependency analysis mechanical, not verbal.

## The model

Every spec is a **node** in a DAG. Edges come from two sources:

- **Declared deps** — the `depends_on` field in a spec explicitly names upstream specs
- **Artifact deps** — one spec writes a file path that another spec reads; inferred by scanning `verify` and `spec` task fields for path patterns

BOI builds this graph from the full set of in-flight + queued + new specs, topologically sorts it, and optionally asks an LLM to critique the result.

### Critique severity levels

| Severity | Meaning | Effect on `dispatch-many` |
|----------|---------|--------------------------|
| `block` | Hard ordering violation; dispatch will likely fail | Refuses dispatch entirely |
| `warn` | Probable dep not declared; may cause issues | Prompts for confirmation (or auto-approves with `--yes`) |
| `info` | Observation; no action required | Shown but does not gate dispatch |

The LLM critique is cached by hash of (DAG topology + spec titles). Re-running `plan` on unchanged state costs zero tokens.

## Commands

### `boi plan`

Builds the DAG and runs the LLM critique. **Does not dispatch anything.**

```bash
boi plan                              # analyze current in-flight + queued state
boi plan spec-a.yaml spec-b.yaml      # include new specs in the analysis
boi plan --force-refresh              # bypass cache, re-run LLM critique
```

Use `boi plan` when:
- You're about to dispatch a batch and want to verify ordering first
- You want to understand what the current in-flight queue looks like as a graph
- You suspect implicit deps between specs you're about to queue

### `boi dispatch-many`

Runs `plan`, then dispatches all specs in topological order with correct `--after` chains.

```bash
boi dispatch-many spec-a.yaml spec-b.yaml spec-c.yaml
boi dispatch-many specs/*.yaml --yes   # auto-approve warns
boi dispatch-many specs/*.yaml --force # override warns (not blocks)
```

Use `dispatch-many` when:
- You're dispatching 2+ specs that may depend on each other
- You want the ordering to be verified automatically, not by memory
- You want `--after` chains emitted without manual bookkeeping

### `boi dispatch` (single-spec, lightweight check)

When dispatching a single spec into an existing in-flight queue, `boi dispatch` runs a lightweight deterministic check (no LLM):
- If the new spec's artifacts overlap with an in-flight spec AND no `--after` flag was provided: **WARN** (not block), showing the implicit dep + suggested `--after` flag
- Use `--skip-plan` to bypass this check when you know the ordering is correct

The full LLM critique is only invoked by `plan` and `dispatch-many`.

## When to use which command

| Situation | Command |
|-----------|---------|
| Dispatching a single spec; no in-flight queue | `boi dispatch` |
| Dispatching a single spec; in-flight queue exists | `boi dispatch` (lightweight check runs automatically) |
| Dispatching 2+ specs in a related batch | `boi dispatch-many` |
| Uncertain about ordering; want to review before committing | `boi plan` first, then `dispatch-many` |
| Emergency dispatch; know the ordering is correct | `boi dispatch-many --force` or `boi dispatch --skip-plan` |

## Example: 3-spec chain

Three specs for a feature track:

```
build-schema.yaml    — creates src/schema/*.rs
build-api.yaml       — reads src/schema/*.rs, creates src/api/*.rs
build-frontend.yaml  — reads src/api/*.rs contract
```

### Before (manual `--after`, misordered)

```bash
boi dispatch build-schema.yaml
boi dispatch build-api.yaml                         # BUG: forgot --after
boi dispatch build-frontend.yaml --after <api-id>
```

`build-api` starts in parallel with `build-schema`. Fails 4 tasks in because schema files don't exist yet. ~12k tokens wasted. Re-dispatch required.

### After (`boi dispatch-many`)

```bash
boi dispatch-many build-schema.yaml build-api.yaml build-frontend.yaml
```

```
Analyzing DAG...

DAG (3 nodes):
  SA000 (build-schema)    ← no deps
  SA001 (build-api)       ← SA000 [implicit: src/schema/*.rs]
  SA002 (build-frontend)  ← SA001 [declared]

Critique:
  [WARN] build-api has implicit dep on build-schema via src/schema/*.rs
         but no --after declared. Adding automatically.

Proposed dispatch order: SA000 → SA001 → SA002

Dispatch? [y/N] y

Dispatched: SA000 (build-schema)
Dispatched: SA001 (build-api) --after SA000
Dispatched: SA002 (build-frontend) --after SA001
```

No misordering. No re-dispatch. The implicit dep was caught before a single token was spent on the wrong order.
