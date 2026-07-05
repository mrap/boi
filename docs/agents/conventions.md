# Code Conventions

Patterns and standards used across the BOI codebase. The rule behind most of them is the
Layered Domain Architecture: forward-only dependencies
`types → config → repo → service → runtime → cli` — see the crate `//!` doc in `src/lib.rs`.

## Coding patterns

| Concern | Pattern | Exemplary file |
|---------|---------|---------------|
| Error handling | Per-layer `thiserror` enums; `cli::run` collapses each subcommand's typed error into one `CliError`, rendered by the testable `report_error` fn — `main.rs` never formats errors itself | `src/cli/mod.rs` (`CliError`), `src/config/spec.rs` (`ConfigError`) |
| Config parsing | TOML via serde with `#[serde(deny_unknown_fields)]` so typos fail loudly at parse time; every `ConfigError` message carries a `Fix:` hint | `src/config/spec.rs`, `src/config/phase.rs` |
| CLI surface | `clap` derive with a `Command` subcommand enum; one file per command under `src/cli/` | `src/cli/mod.rs` |
| Async runtime | tokio `rt-multi-thread`, built explicitly in `main` | `src/main.rs` |
| Database access | `sqlx::query!` compile-time-checked macros live in `src/repo/` ONLY; other layers compose repo functions instead of adding queries | `src/repo/specs.rs` |
| Query cache | Committed `.sqlx/` offline cache; CI builds with `SQLX_OFFLINE=true`; regenerate via `just prep-sqlx` only when a `query!` changes, and commit the delta | `.sqlx/`, `justfile` |
| Schema changes | Numbered SQL migrations run by sqlx `migrate` | `migrations/0001_initial.sql` |
| Logging | `tracing::{info,warn,error,debug}` on the daemon path; `init_tracing` stands up the OTel pipeline once at daemon boot; subcommand user output is plain `println!` plus the single `error:`-prefixed `report_error` line | `src/cli/boot.rs`, `src/runtime/otel_export.rs` |
| Tests | Inline `#[cfg(test)]` modules at the bottom of the file under test | `src/cli/mod.rs`, `src/repo/db.rs` |
| Test naming | Tests in `src/service/`, `src/runtime/`, and `tests/` are `test_l<1|2|3>_<name>`; L3 tests add module attribution: `test_l3_<module>_<name>` (CI-enforced) | `scripts/checks/test-naming.sh` |
| SQLite test isolation | Single-connection in-memory pool per test (`sqlite::memory:` with `max_connections(1)` — an in-memory database is per-connection) | `src/repo/db.rs` (`memory_pool`) |
| Scratch dirs in tests | No `tempfile` dependency — std-only throwaway-dir helpers | `src/cli/mod.rs` (`testtmp`), `src/runtime/git_ops.rs` |
| Module docs | Every layer `mod.rs` (and most files) opens with a `//!` design doc; each `src/<layer>/` also carries a thin `AGENTS.md` router for that layer | `src/runtime/goose.rs` (the exemplar) |
| Subprocess spawning | Allowed only in `src/runtime/` — `cli/` stays thin, logic lives in `service`/`runtime` (CI-enforced) | `scripts/checks/no-subprocess-outside-runtime.sh` |
| Layering | `use crate::<other>` only points at a lower layer (CI-enforced) | `scripts/checks/module-dep-audit.sh` |
| Formatting & lints | `cargo fmt` (default rustfmt); clippy at `-D warnings`; workspace lints in `Cargo.toml [lints]` (`unsafe_code = "deny"`, `missing_docs = "deny"` — every `pub` item needs a `///`; prefer `pub(crate)` over filler docs — `unwrap_used = "warn"`, `panic = "warn"`) | `justfile`, `Cargo.toml` |

## Enforcement

The conventions above are gates, not suggestions:

```bash
just check          # cargo fmt --check + clippy -D warnings + cargo test + cargo doc -D warnings
just lint-scripts   # the 11 shell checks: layering, subprocess, test naming/coverage + their regression harnesses
just ci             # both — matches .github/workflows/ci.yml
```

All work happens on a `feature/<slug>` or `fix/<slug>` branch and lands on `develop`
via PR with the required checks green — run `just check` locally before pushing the
branch, and `just ci` before merging anything that touches layer boundaries, test
names, or subprocess code. `main` is ceremony-only: it moves only by the release
ceremony's release/hotfix merges, and a direct push to `main` is an incident (see
"Branching & releases" in the root AGENTS.md for the model and the remediation).

## Git conventions

| Item | Pattern |
|------|---------|
| Commit messages | Scoped conventional commits — `type(scope): subject` or bare `type: subject`. Types in current use: `feat`, `fix`, `chore`, `docs`, `refactor`, `ci` (see `git log`) |
| Branch model | GitFlow — `feature/<slug>`/`fix/<slug>` → `develop` via PR; `release/X.Y.Z` and `hotfix/*` are created by the release ceremony only; `main` moves only by ceremony merges. Full table: root [AGENTS.md](../../AGENTS.md) "Branching & releases" |
| Spec branches (auto-created) | `spec/<SpecId>/integration` for a spec's integration branch, `spec/<SpecId>/<TaskId>` per task — created by `src/runtime/worktree.rs`; never create these by hand |
| Salvage branches (manual) | `salvage/<SpecId>` — operator convention for rescuing stranded `spec/*` work; merges to `develop`, never `main` |

See also: the root [AGENTS.md](../../AGENTS.md) (canonical commands, CLI table, and spec
format — runnable spec examples live in `tests/fixtures/specs/*.toml`), `src/lib.rs`
(crate `//!` — the architecture map), [adding-features.md](adding-features.md) for change
recipes, [guardrails.md](guardrails.md) and [invariants.md](invariants.md) for the rules
behind these patterns.
