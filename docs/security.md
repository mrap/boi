# Security

BOI orchestrates autonomous LLM workers — headless [Goose](https://github.com/block/goose) sessions — that execute code on your machine. This page covers the security model, risks, and recommendations for running BOI safely.

## Trust Model

BOI's trust model has two key assumptions:

1. **Spec authors are trusted.** Spec content (`[contract].scope`, each task's `behavior`) is delivered to the worker as its instructions, and every `verifications` `command` is executed verbatim through `sh -c`. A malicious spec can instruct a worker to run arbitrary commands, exfiltrate data, or modify files. Only run specs you wrote or reviewed.
2. **Workers run as you, with no per-action approval.** Each worker phase is a non-interactive `goose run --recipe <file>.yaml --output-format stream-json` child. The generated recipe enables Goose's `developer` builtin — the shell + file-editor toolset — and headless mode has no human in the loop: nothing prompts for confirmation before a command runs.

If either assumption is violated (untrusted spec, shared machine), use the isolation techniques described below.

## What a Worker Can Do

A worker is a subprocess of the daemon, running as your user. It can:

- run any shell command (including `rm`, `curl`, `wget`, etc.)
- read and write any file your user can access
- make network requests to any endpoint
- see the daemon's environment — the `goose` child inherits it, including provider auth tokens

**This is equivalent to handing a script your user account.**

### Worktrees are not a security boundary

Each task runs in its own git worktree (default root `~/.boi/v2/worktrees/`, layout `<root>/<spec-id>/<task-id>` plus `<root>/<spec-id>/integration`), and the worker's working directory is set there. That is source-control isolation — parallel tasks cannot clobber each other's edits — not confinement: nothing prevents a worker from reading or writing outside its worktree. Worktrees are ephemeral and destroyed on cleanup; never treat them as a sandbox, and never edit files in them.

## Secrets

- Provider tokens live in `*.env` files under `~/.boi/v2/secrets/` (e.g. `claude.env` containing `CLAUDE_CODE_OAUTH_TOKEN=…`). They are never written into the LaunchAgent plist.
- The secrets bootstrap loads them into the process environment at startup and **refuses to start** if the directory or any `.env` file is group- or world-accessible: the directory must be `0700`, each file `0600` (symlinked targets are checked too).
- Loaded key *names* are logged for operator diagnostics; values never are.
- Generated Goose recipes carry no secrets — a recipe's `env` map holds only `BOI_REVISION_ARTIFACT` (for `plan_revision` phases). Tokens reach the `goose` child solely via process-environment inheritance.

## Recommendations

### For Personal Development Machines

If you are the only user on the machine and you trust your specs, the default configuration is acceptable. Review specs before dispatching — including every `verifications` command, which runs with your full permissions.

### For Shared Machines

- Ensure `~/.boi/` is not readable by other users: `chmod 700 ~/.boi`. (BOI enforces this for `~/.boi/v2/secrets/` itself; the rest is on you.)
- Do not run specs authored by others without reviewing them first.
- Consider container isolation (below).

### For CI/CD or Untrusted Specs

The repository ships no root `Dockerfile` or `docker-compose.yml` — the only Docker assets are the e2e test harness (`tests/e2e/Dockerfile`, run via `just e2e`). Container isolation is a pattern you assemble yourself:

- mount only the project directory, never your home directory (`~/.boi/v2/secrets/` would otherwise ride along)
- disable or restrict network access where the task allows it
- set resource limits (CPU, memory, time) and run as a non-root user

## Input Validation

What v2 validates — and what it deliberately does not:

- **Spec TOML is strict-parsed.** Unknown fields are rejected at parse time (`deny_unknown_fields`), so typos and stale fields fail loudly instead of being silently ignored.
- **`boi dispatch` validates before persisting.** Parse + validation run first; the spec's rows are then written in one transaction before the daemon is told to start.
- **Verify commands are linted pre-dispatch** against a catalogue of nine known authoring antipatterns (wrong redirect order, inverted `grep` flags, missing `PATH` for non-coreutils binaries, …). This is a reliability lint, not a security filter.
- **IDs are generated, not user-supplied.** Spec/task ids are Crockford base32 — an uppercase type prefix (`S`/`T`/`P`/`D`) plus an 8-char random body — and are format-validated on parse.
- **`unsafe_code = "deny"` crate-wide**, with two audited exemptions: the pre-runtime secrets `set_var` (`src/runtime/secrets.rs`) and the daemon-stop `kill(2)` call (`src/cli/boot.rs`).
- **Not validated:** `verifications` commands execute through `sh -c` exactly as written, and scope/behavior prose reaches the worker verbatim. Validation catches mistakes; it does not contain malice.

## Spec Safety Checklist

Before running a spec you did not write:

- [ ] Read `[contract].scope` and every task's `behavior` — they become worker instructions verbatim. Do they contain `curl`, `wget`, or network commands you don't expect?
- [ ] Read every `verifications` `command` — each runs through `sh -c` with your full permissions
- [ ] Check for `rm -rf`, file deletion, or commands that modify files outside the workspace
- [ ] Look for references to `~/.ssh`, `~/.aws`, `~/.config`, or other sensitive directories
- [ ] Confirm `workspace` and `base_branch` point where you expect — the `merge` delivery fast-forwards the integration branch into that base branch

## Reporting Security Issues

If you find a security vulnerability in BOI, please report it by opening a GitHub issue with the `security` label, or email the maintainers directly. Do not include exploit details in public issues.

---

See also: [AGENTS.md](../AGENTS.md) (canonical CLI surface + spec format) · `tests/fixtures/specs/` (runnable spec examples) · [getting-started.md](getting-started.md) · [agents/guardrails.md](agents/guardrails.md) · [agents/invariants.md](agents/invariants.md) · `src/lib.rs` (crate architecture)
