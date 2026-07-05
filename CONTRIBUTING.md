# Contributing to boi

Thanks for considering a contribution. This is a young project — expect the
process below to tighten up as it grows.

## Getting set up

```bash
git clone https://github.com/mrap/boi
cd boi
cargo build --release --locked
```

See [docs/getting-started.md](docs/getting-started.md) for the full operator
walkthrough (secrets, daemon, first dispatch), and
[AGENTS.md](AGENTS.md) for the architecture map and CLI/spec-format
reference — it's the canonical cold-start doc for anyone (human or agent)
working in this codebase.

## Before you send a PR

```bash
just check     # fmt --check + clippy -D warnings + cargo test + cargo doc
just ci        # just check + the architecture/guardrail lint scripts
```

(No `just`? `brew install just`, or run the equivalent `cargo` commands from
the `justfile` directly.)

- Format: `cargo fmt --all`.
- Lint: `cargo clippy --all-targets --locked -- -D warnings` must be clean.
- Tests: `cargo test --locked` must pass. New behavior needs a test; bug
  fixes need a regression test that fails before the fix.
- The `repo` layer uses `sqlx::query!` macros checked at compile time against
  a committed `.sqlx/` cache. If you change a query, regenerate it with
  `just prep-sqlx` and commit the `.sqlx/` delta.
- Migrations are append-only — never edit an applied `migrations/NNNN_*.sql`;
  add a new numbered file.

## Architecture in one line

```
src/types → src/config → src/repo → src/service → src/runtime → src/cli
```

Forward-only dependencies between layers, enforced by
`scripts/checks/module-dep-audit.sh`. See
[docs/agents/conventions.md](docs/agents/conventions.md) and
[docs/agents/adding-features.md](docs/agents/adding-features.md) for more.

## Commit style

Scoped conventional commits: `type(scope): subject` or bare `type: subject`
(`feat`, `fix`, `chore`, `docs`, `refactor`, `ci`, ...). Keep PRs focused —
one logical change per PR.

## Reporting issues

Open a GitHub issue. For anything security-related, please read
[docs/security.md](docs/security.md) first and use responsible disclosure
rather than a public issue.

## License

By contributing, you agree your contributions are licensed under the same
dual MIT / Apache-2.0 terms as the rest of the project (see
[LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE)).
