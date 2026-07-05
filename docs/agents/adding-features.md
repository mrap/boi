# Adding Features

Step-by-step recipes for common changes. Each recipe assumes you've read the crate doc in `src/lib.rs` (the Layered Domain Architecture: `types → config → repo → service → runtime → cli`) and the root [AGENTS.md](../../AGENTS.md). Every recipe ends with the same gate CI runs: `just check && just lint-scripts`.

## New CLI subcommand

1. Add a variant to the `Command` enum in `src/cli/mod.rs` (the `clap` derive tree)
2. Create the handler in `src/cli/mycommand.rs`; add `pub mod mycommand;` in `src/cli/mod.rs`
3. Wrap the handler's error type as a new `CliError` variant in `src/cli/mod.rs`
4. Wire the match arm in `cli::run` (`src/cli/mod.rs`)
5. Add the subcommand name to `test_l2_help_lists_every_subcommand` in `src/cli/mod.rs`'s tests
6. Pick the right side of the process split: write-side commands are control-socket clients (`src/cli/control.rs`) and fail loud when no daemon is running; read-only commands open SQLite/DuckDB directly. `cli/` spawns no subprocess — `scripts/checks/no-subprocess-outside-runtime.sh` enforces it
7. Gate: `just check && just lint-scripts`

## New DB column or table

1. Add a new numbered `migrations/NNNN_description.sql`. **Never edit an applied migration** — forward-only, no down migrations; `sqlx::migrate!("./migrations")` in `src/repo/db.rs::connect` tracks applied versions and picks the new file up automatically
2. Update the affected `sqlx::query!` calls in `src/repo/`
3. Regenerate the offline query cache: `just prep-sqlx` (needs `sqlx-cli` and a `.env` with `DATABASE_URL=sqlite://.dev.db`); commit the `.sqlx/` delta together with the query change — CI builds with `SQLX_OFFLINE=true` and never regenerates it
4. Gate: `just check && just lint-scripts`

## New deterministic phase step

1. Write the step body as a plain `fn` item returning `BoxFuture<'static, Result<StepRun, StepError>>` — an `async fn` does NOT coerce to the `DetStep` fn-pointer type. Existing bodies live in `src/runtime/worktree.rs` and `src/runtime/validate.rs`
2. Add the phase name to `DETERMINISTIC_PHASES` and a match arm in `resolve` in `src/runtime/deterministic.rs`; update the table-size test there (it pins the entry count)
3. Declare the phase in `~/.boi/v2/phases/<name>.toml` with `kind = "deterministic"` — no `prompt_template` (that combination is a loud parse rejection), though `[runtime]` is structurally required (inert for deterministic phases). Parser: `src/config/phase.rs`
4. Add the phase name to the pipeline in `~/.boi/v2/pipelines/standard.toml`
5. Gate: `just check && just lint-scripts`

## New worker (LLM) phase

1. Declare `~/.boi/v2/phases/<name>.toml`: `name`, `level` (`spec` runs once per spec, `task` runs per task in parallel), `kind = "worker"`, `prompt_template = "<name>.md"`, `[runtime]` with `provider` + `model`, and `[on.<verdict>]` routing (`passing` / `redo` / `blocked` / `fail`; omit `next` to make a route terminal). Parser: `src/config/phase.rs`
2. Put the prompt template file in `~/.boi/v2/phases/` too — the daemon resolves `prompt_template` filenames against the phases dir (`src/cli/daemon.rs`)
3. Add the phase name to `~/.boi/v2/pipelines/standard.toml`
4. No new runtime code: every worker phase routes through `runtime::goose::GooseRuntime`, which generates a fresh Goose recipe per phase run (`src/runtime/recipe.rs`)
5. The daemon loads all phase + pipeline TOML at boot (`src/cli/boot.rs`); a malformed declaration or routing graph is a loud startup rejection, never a mid-run stall
6. Gate: `just check && just lint-scripts`

## New spec / phase / pipeline TOML field

1. Add the field to the serde struct in `src/config/` — `spec.rs` (`RawSpec` / `RawContract` / `RawTask`), `phase.rs` (`PhaseDef`), or `pipeline.rs`. All are `#[serde(deny_unknown_fields)]`; use `#[serde(default)]` if existing TOML files must keep parsing
2. Add cross-field checks in `src/config/validate.rs` (spec) or `PhaseDef::from_toml` (phase)
3. For spec fields, carry the value through normalization to `Spec` in `src/config/spec.rs`
4. Add or extend a fixture in `tests/fixtures/specs/` — those are the canonical runnable examples, and the `*_rejects_*` fixtures pin the error cases
5. Gate: `just check && just lint-scripts`

## New bus event

1. Add a variant to `BoiEvent` in `src/types/event.rs` — one variant per *legal* transition; illegal transitions get no variant
2. Handle it in the `persist` match in `src/service/bus.rs`. The match has **no `_` arm**, so the build breaks until you do: a state-mutating variant runs the transition guard then the repo write; an observed-only variant joins the explicit `=> Ok(())` arm list (never a wildcard)
3. If it's a lifecycle transition, add its legality rule in `src/service/transitions.rs`
4. Observers (`EmitObserver` implementations, e.g. the OTel adapter) receive every emitted variant automatically — observation is best-effort and never aborts the emit
5. Gate: `just check && just lint-scripts`

See also: [conventions.md](conventions.md) for coding patterns, [invariants.md](invariants.md) for the rules these recipes must not break, [debugging.md](debugging.md) when a change misbehaves, the root [AGENTS.md](../../AGENTS.md) for commands + spec format, and `tests/fixtures/specs/*.toml` for runnable spec examples.
