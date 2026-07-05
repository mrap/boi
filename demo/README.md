# Demo

A <90s "money shot" of BOI: a real 3-task spec with a DAG dependency and both
verify-gate kinds, dispatched and shown live in the dashboard TUI.

## Files

- **`spec.toml`** — the demo spec. Three tasks: `add-handler` and (once that
  passes) `wire-router` run, then `document-endpoint` waits on both. Mixes a
  `command` gate (`cargo test ...`, must exit 0) and an `intent` gate (LLM-judged).
  `workspace` points at `/tmp/boi-demo-workspace` — a throwaway git repo the
  tape creates on the fly (`git init -b main` + one empty commit), so the
  spec's `base_branch` policy check has something real to open. Nothing here
  reads or writes any real project.
- **`demo.tape`** — a [VHS](https://github.com/charmbracelet/vhs) tape. Record
  it yourself with `vhs demo/demo.tape` (from the repo root) — this repo does
  **not** ship a rendered `.gif`/`.cast`; nothing has been recorded or
  published.

## What this tape shows, entirely offline and credential-free

No `boi daemon` is running during this recording, on purpose:

1. `cat demo/spec.toml` — the spec itself: the DAG, the verify gates.
2. Create the throwaway workspace repo.
3. `boi dispatch demo/spec.toml` — parses, lints, and validates the spec,
   then **persists it** (mints a spec id + 3 task ids, one SQLite
   transaction). Since no daemon is listening, dispatch can't hand it off to
   be scheduled, so it exits non-zero with a `NoDaemon` message. This is real
   engine behavior, not a recording bug — see "fail loud, no daemon → no
   silent write" in [`../docs/architecture.md`](../docs/architecture.md).
   Because no daemon ever ran preflight, this beat needs **no `goose`
   install and no provider credential** to work.
4. `boi dashboard` — read-only, reads the persisted spec straight out of
   `~/.boi/v2/boi.db`. Shows the real DAG: the spec `queued`, 3 tasks,
   `document-endpoint` `blocked` on the other two.

That's the reproducible half of the demo: anyone can run it, on any machine
with `boi` built, with zero setup beyond `git` and this repo.

## What needs Mike physically present

The other half — watching the pipeline actually *execute* — needs a live
environment this tape deliberately doesn't touch:

- `boi daemon start` running, with a real `goose` (`>=1.34, <2.0`) on `PATH`.
- A real `~/.boi/v2/secrets/claude.env` with a live `CLAUDE_CODE_OAUTH_TOKEN`
  (or another authenticated provider) — preflight probes it live before any
  phase spends a token.
- Re-dispatching `demo/spec.toml` (or a fresh copy) against that daemon, then
  watching `boi dashboard` while the real pipeline runs: `add-handler` and
  `wire-router` moving `active → passing` in parallel, `document-endpoint`
  unblocking once both clear, real commits landing in the worktrees, and the
  spec merging to `main` in the demo workspace.

Do not record or publish anything until that live pass has actually been run
and reviewed — this tape and this README are the safe, reviewable half only.
