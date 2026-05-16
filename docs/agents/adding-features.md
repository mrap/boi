# Adding Features

Step-by-step recipes for common changes. Each recipe assumes you've read [ARCHITECTURE.md](../../ARCHITECTURE.md) for structural context.

## New CLI subcommand

1. Add variant to `Commands` enum in `src/main.rs:54`
2. Create handler in `src/cli/mycommand.rs`
3. Add `pub mod mycommand;` in `src/cli/mod.rs`
4. Wire the match arm in `main()` in `src/main.rs:296`

## New crate

1. Create `crates/my-crate/` with `Cargo.toml` and `src/lib.rs` (or `src/main.rs` for binary)
2. Add to workspace `members` in root `Cargo.toml:2`
3. Add path dependency from consumers: `my-crate = { path = "../my-crate" }`

## New hook event

1. Add `pub const ON_MY_EVENT: &str = "on_my_event";` in `src/hooks.rs:20-33`
2. Call `hooks::fire(ON_MY_EVENT, &payload)` from the lifecycle point (usually `src/worker.rs`)
3. Add entry in `hooks/default.yaml` if it should have a default handler
4. Document in `docs/boi-hooks-spec.md`

## New migration

1. Bump `SCHEMA_VERSION` in `src/queue.rs:174`
2. Add `fn migrate_vN(conn: &Connection) -> Result<()>` below existing migrations (`src/queue.rs:337-390`)
3. Add call in the version match in `run_migrations()`
4. **Never modify an existing `migrate_vN` function** — see [invariants.md](invariants.md) #1
5. Test with `cargo test` — queue tests run against temp SQLite

## New workspace backend

1. Create `src/workspace/mybackend.rs` implementing `WorkspaceBackend` trait (`src/workspace/mod.rs:21`)
2. Satisfy the four invariants: isolation, idempotent create, best-effort cleanup, exec in-directory
3. Add `pub mod mybackend;` in `src/workspace/mod.rs`
4. Wire backend selection in config/daemon startup

## New worker pool backend

1. Create `src/pool/mypool.rs` (or `src/remote/mypool.rs`) implementing `WorkerPool` trait (`src/pool/mod.rs:52`)
2. Implement: `spawn`, `status`, `collect`, `cancel`, optionally `cleanup`, `max_workers`
3. Wire into `worker_pool.type` config option in `src/config.rs:244`

## New phase

1. Create `phases/my-phase.phase.toml` with required fields: `level`, `can_add_tasks`, `can_fail_spec` — see [invariants.md](invariants.md) #7
2. Add the phase name to a pipeline in `phases/pipelines.toml`
3. If the phase needs a built-in implementation, add it in `src/builtins.rs`

See also: [conventions.md](conventions.md) for coding patterns, [spec-format.md](spec-format.md) for spec YAML schema.
