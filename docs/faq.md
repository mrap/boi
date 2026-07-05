# FAQ

Philosophy and identity only. For commands, spec format, and operational detail,
the root [AGENTS.md](../AGENTS.md) is canonical (alongside `boi --help`); the
runnable spec examples are [`tests/fixtures/specs/*.toml`](../tests/fixtures/specs/).

## General

### What is BOI?

BOI is a single-binary Rust harness that orchestrates LLM-powered
software-engineering tasks. You hand it a spec — a TOML file of tasks — and it
runs each task through a phase pipeline in an isolated git worktree, verifies
the result, and merges.

### Why "Beginning of Infinity"?

Named after David Deutsch's book *The Beginning of Infinity*. The core idea:
knowledge grows through conjecture and criticism. A spec is a conjecture; the
verification phases are the criticism. And the plan is not frozen: when a task
hits work the spec didn't foresee, it can file a blocking report that triggers
a plan-revision phase, which can add, remove, or retarget tasks. Refinement is
part of the design, not an exception.

### How is BOI different from one long agent session?

Long sessions degrade. Context fills up, instructions get lost, and the agent
drifts. BOI prevents this by design: every LLM phase runs as a fresh agent
process — the harness builds a fresh recipe per worker-phase run and spawns a
new subprocess from it. Zero accumulated context. Durable state lives outside
the context window: the spec snapshot and phase-run history in the SQLite
store (`~/.boi/v2/boi.db`), the work itself in the task's git worktree.

### Why a worktree per task?

Isolation. Each task gets its own git worktree, so tasks can't interfere with
each other's work and your checkout is never edited in place — results arrive
via merge. Worktrees are ephemeral: created for the run, removed at teardown,
never somewhere to keep anything.

## See also

- [AGENTS.md](../AGENTS.md) — canonical CLI commands and spec format
- [getting-started.md](getting-started.md) — first dispatch
- [agents/glossary.md](agents/glossary.md) — domain terms
- [security.md](security.md) — trust model
- [`src/lib.rs`](../src/lib.rs) — crate-level architecture doc
