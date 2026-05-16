# Guardrails

Things NOT to do. Violating these causes data loss, silent failures, or broken consumers.

- **Don't edit files inside `~/.boi/worktrees/`** — they are ephemeral and destroyed on cleanup
- **Don't run two `boi daemon` instances against the same DB** — causes corruption or silent failures
- **Don't modify existing `migrate_vN` functions** — migrations are append-only; add a new `migrate_vN+1` (see [invariants.md](invariants.md) #1)
- **Don't break hook payload JSON schema** without updating hex consumers in mrap-hex
- **Don't bypass the migration system** to alter SQLite schema directly (no raw `ALTER TABLE` outside migrations)
- **Don't add a dependency without justification** in the commit message
- **Don't commit secrets** — `.env` is gitignored; verify `.gitleaksignore` covers any false positives
- **Don't remove or rename phase TOML required fields** (`level`, `can_add_tasks`, `can_fail_spec`) — daemon exits 2
- **Don't assume CONTRIBUTING.md is accurate** — it describes the Python era; use `cargo` commands
- **Don't dispatch specs without the daemon running** — they queue but never execute; use `boi doctor` to check

See also: [invariants.md](invariants.md) for structural invariants with enforcement locations, [conventions.md](conventions.md) for what TO do.
