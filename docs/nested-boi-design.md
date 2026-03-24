# Nested BOI Design Decision: DON'T BUILD

**Decision:** Do not implement nested/recursive BOI spawning.

## Summary

After analyzing 5 concrete use cases for nested BOI (worker-spawned child specs), the recommendation is **DON'T BUILD**. Existing BOI features (self-evolution via Discover mode, DAG-based blocking, Generate mode decomposition) already cover the scenarios where nesting would help, without the coordination complexity.

## Why Nesting Isn't Needed

### Existing features cover the use cases

| Use Case | Nested BOI Would... | Existing Feature That Solves It |
|----------|---------------------|-------------------------------|
| Task too large for one spec | Spawn child specs for sub-work | **Discover mode**: add PENDING tasks to current spec |
| N independent research paths | Parallelize via child specs | **Manual dispatch + DAG blocking**: user dispatches N specs |
| Meta-improvement testing | Spawn test spec for validation | **Verification tasks**: add test tasks to current spec |
| Dependency chains (A then B) | Auto-dispatch B when A finishes | **DAG blocking**: `--blocked-by` flag on dispatch |
| Fan-out (same transform, N files) | Spawn N parallel child specs | **Self-evolution**: add N tasks serially (overhead is lower) |

### The coordination complexity is not justified

Nested BOI would require building all of the following:

1. **Depth limit system** to prevent infinite recursion (child spawns grandchild spawns...)
2. **Resource budget inheritance** so children don't consume all workers
3. **Priority inheritance** to prevent parent blocked on low-priority child
4. **Orphan detection + cleanup** when parent spec fails or is cancelled
5. **Completion tracking** so parent knows when children finish
6. **Worktree conflict resolution** when parent and child edit overlapping files

Each of these is a non-trivial engineering effort. Together, they represent a significant increase in BOI's complexity for marginal benefit.

### Nesting undermines BOI's core principles

BOI's strengths are:

- **Fresh context per iteration.** No accumulated state across iterations.
- **Spec file as single source of truth.** All state is in one file.
- **User controls the queue.** The user sees and manages every spec.

Nested spawning introduces hidden state (child specs the user didn't dispatch), implicit coordination (parent waiting for children), and emergent behavior (resource contention between parent and child work). These directly conflict with the simplicity that makes BOI reliable.

## Risk Assessment

| Risk | Severity | Why It Matters |
|------|----------|---------------|
| Infinite recursion | CRITICAL | A bug in spawn logic could create unbounded specs |
| Resource exhaustion | HIGH | Children steal workers from other queued specs |
| Orphaned specs | HIGH | Parent failure leaves children running with no consumer |
| Worktree conflicts | HIGH | Parent + child editing same repo creates merge hell |
| Hidden work graphs | MEDIUM | User loses visibility into what's actually running |
| Complexity budget | HIGH | BOI becomes harder to understand and debug |

## Alternatives

### Current alternatives (already implemented)

1. **Self-evolution (Discover mode):** Worker adds new PENDING tasks to the current spec. Handles decomposition, fan-out, and incremental discovery without any coordination overhead.

2. **DAG blocking:** User dispatches multiple specs with explicit dependencies (`--blocked-by`). Handles parallel independent work streams with full user visibility.

3. **Generate mode:** Decomposes high-level goals into 5-15 concrete tasks automatically. Handles the "task is too big" scenario at dispatch time.

### Future lightweight alternative (recommended if demand emerges)

**`boi suggest` command:** A worker writes follow-on spec suggestions to a `## Suggested Specs` section in the current spec. The daemon surfaces these as recommendations to the user, who can dispatch manually. This preserves user control while enabling worker-driven discovery of follow-on work.

This is lightweight, requires no coordination infrastructure, and keeps the user in control of the queue.

## Decision Record

- **Evaluated:** 5 scenarios (decomposition, discovery, meta-improvement, dependency chains, fan-out)
- **Recommendation:** DON'T BUILD
- **Rationale:** YAGNI. No user has been blocked by inability to nest. All scenarios have adequate workarounds.
- **Revisit if:** A concrete, recurring use case emerges where self-evolution + DAG blocking are genuinely insufficient. The most likely trigger would be large-scale parallel fan-out (50+ files) where serial execution is too slow.
