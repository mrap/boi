# Guardrails

Things NOT to do. Violating these causes data loss, silent failures, or broken consumers.

1. **Don't edit files inside `~/.boi/v2/worktrees/`** — task and integration
   worktrees are ephemeral; the `teardown` step removes them when a spec
   settles.

2. **Don't run two `boi daemon` instances against the same DB** — the
   orchestrator and event bus live in ONE long-running daemon; a second
   instance takes over the control socket (`~/.boi/v2/daemon.sock`) out from
   under the first.

3. **Don't expect write-side commands to work without the daemon** —
   `dispatch`, `cancel`, `unblock`, `resolve-conflict`, and `fail` are
   control-socket clients; with no daemon running they fail loud with a
   non-zero exit (never a silent DB-only flip). Check with `boi daemon status`.
   Read-only commands (`dashboard`, `log`, `traces`, `failures`, `spec show`)
   read SQLite / DuckDB directly and need no daemon.

4. **Don't edit an applied migration file** — migrations are append-only
   numbered SQL (`migrations/NNNN_*.sql`); add a new numbered file instead.
   `repo::connect` runs `sqlx::migrate!("./migrations")` at every startup.

5. **Don't bypass the migration system** to alter the SQLite schema directly
   (no raw `ALTER TABLE` outside `migrations/`).

6. **Don't put unknown fields in phase TOMLs** (`~/.boi/v2/phases/<name>.toml`)
   — they parse with `deny_unknown_fields`, so typos and removed fields fail
   loud at load. Required fields: `name`, `level`, `kind`, `runtime`; a
   `deterministic` phase must NOT declare `prompt_template`.

7. **Don't import a higher layer** — module dependencies flow forward only:
   `types → config → repo → service → runtime → cli`.
   `scripts/checks/module-dep-audit.sh` (run via `just lint-scripts`) fails on
   violations.

8. **Don't spawn subprocesses outside `src/runtime/`** —
   `scripts/checks/no-subprocess-outside-runtime.sh` enforces it.

9. **Don't call the blocking `duckdb` or `git2` APIs bare in async code** —
   wrap every call site in `tokio::task::spawn_blocking`;
   `scripts/checks/duckdb-calls-spawn-blocking.sh` and
   `scripts/checks/git2-calls-spawn-blocking.sh` lint for it.

10. **Don't add a dependency without justification** in the commit message.

11. **Don't commit secrets** — `.env` is gitignored; verify `.gitleaksignore`
    covers any false positives. Provider keys belong in
    `~/.boi/v2/secrets/*.env` (dir `0700`, files `0600` — permissions are
    enforced at daemon startup).

See also: [invariants.md](invariants.md) for structural invariants with enforcement locations, [conventions.md](conventions.md) for what TO do, and the root [AGENTS.md](../../AGENTS.md) for the CLI surface and spec format.
