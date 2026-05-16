# Critical Invariants

If you violate these, the system breaks. Each row cites the enforcement location.

| # | Invariant | Enforcement |
|---|-----------|-------------|
| 1 | **Migrations are append-only.** Never modify `migrate_v1` or `migrate_v2`. New schema changes require `migrate_vN` + `SCHEMA_VERSION` bump. | `src/queue.rs:174` (version), `src/queue.rs:337-390` (migrations) |
| 2 | **SQLite single-writer per DB path.** WAL allows concurrent reads; one writer only. `busy_timeout=5000ms`. | `src/queue.rs:182` — violation = corruption or 5s stall then error |
| 3 | **`assigning` prevents double-dispatch.** `dequeue()` wraps claim in a transaction that atomically sets status `assigning`, preventing concurrent daemons from claiming the same spec. | `src/queue.rs:514` (dequeue), `src/queue.rs:545` (assigning update) |
| 4 | **Worktrees are ephemeral.** Never edit files in `~/.boi/worktrees/` directly — destroyed on cleanup. | Convention (not enforced in code) |
| 5 | **Verify commands must be idempotent.** Worker may re-run verify on retry. `CREATE TABLE`-style verify breaks retries. | Convention (not enforced) |
| 6 | **Hook payloads are stable JSON contracts.** Breaking changes to payload structs break hex consumers. | Convention — no schema versioning in hook payloads |
| 7 | **Phase TOMLs must declare `level`, `can_add_tasks`, `can_fail_spec`.** | `src/phases.rs:198` — `PhaseConfig::from_toml` returns Err; daemon exits 2 |
| 8 | **Pipelines use `spec_pre_phases`/`spec_post_phases`** (not legacy `spec_phases`). | `phases/pipelines.toml` — loud WARN at load time on legacy shape |
| 9 | **Spec intake validation.** Non-PENDING task statuses rejected before DB write. | `src/spec.rs:411` — `validate_intake()` |
| 10 | **Daemon restart recovery.** Specs stuck in running/assigning reset to queued. | `src/queue.rs:1530` — `recover_stuck_specs()` |

See also: [guardrails.md](guardrails.md) for behavioral rules, [conventions.md](conventions.md) for coding patterns.
