# BOI v2 dev shortcuts. Install: `brew install just`. List: `just`.

# Default target — show available recipes
default:
    @just --list

# Full check matching CI (cargo fmt + clippy + tests + docs)
check:
    cargo fmt --all -- --check
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --locked
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked

# Apply formatting and clippy auto-fixes
fix:
    cargo fmt --all
    cargo clippy --all-targets --fix --locked --allow-dirty --allow-staged

# Run the shell-script lint checks + their regression harnesses
lint-scripts:
    bash scripts/checks/no-subprocess-outside-runtime.sh
    bash scripts/checks/test-naming.sh
    bash scripts/checks/module-dep-audit.sh
    bash scripts/checks/test-coverage.sh
    bash scripts/checks/duckdb-calls-spawn-blocking.sh
    bash scripts/checks/git2-calls-spawn-blocking.sh
    bash scripts/checks/test-no-subprocess.sh
    bash scripts/checks/test-test-naming.sh
    bash scripts/checks/test-module-dep-audit.sh
    bash scripts/checks/test-test-coverage.sh
    bash scripts/checks/test-spawn-blocking-lints.sh
    bash scripts/checks/doc-gate.sh
    bash scripts/checks/test-doc-gate.sh

# Full CI suite locally (matches .github/workflows/ci.yml jobs)
ci:
    just check
    just lint-scripts

# Build the binary and smoke-test it
smoke:
    cargo build --locked
    ./target/debug/boi

# The Docker E2E — the ONLY real-`goose` test (Phase 10.5). Builds a
# container with a pinned `goose` + a local Ollama model, then runs
# `01-typo-fix` + a scripted cancellation + a retry-recovery run. Run
# OUT-OF-BAND from `just ci` — it needs Docker + network (the Ollama model
# pull). See tests/e2e/Dockerfile + tests/e2e/entrypoint.sh.
e2e:
    docker build -f tests/e2e/Dockerfile -t boi-v2-e2e .
    docker run --rm boi-v2-e2e

# Docker E2E variant — the same 3 scenarios as `e2e`, but workers run against
# OpenRouter (default model: anthropic/claude-haiku-4.5 — period not hyphen,
# per OpenRouter ID format) instead of the local Ollama.
# Scenario 1 asserts the WORK actually happened on the integration branch,
# not just that the spec settled. Needs OpenRouter creds via `--env-file`;
# defaults to $HOME/.boi/secrets/openrouter.env (override with BOI_OPENROUTER_ENV).
# Override the model with `MODEL=...` on the command line.
e2e-openrouter:
    docker build -f tests/e2e/Dockerfile -t boi-v2-e2e .
    docker run --rm \
        --env-file "${BOI_OPENROUTER_ENV:-$HOME/.boi/secrets/openrouter.env}" \
        -e "MODEL=${MODEL:-anthropic/claude-haiku-4.5}" \
        -v "$(pwd)/tests/e2e/entrypoint-openrouter.sh:/e2e/entrypoint.sh:ro" \
        boi-v2-e2e

# Explicit one-liner to run the OpenRouter E2E with claude-haiku-4.5 (the
# model verified 2026-05-23 to follow declared task contracts reliably).
e2e-openrouter-haiku:
    MODEL=anthropic/claude-haiku-4.5 just e2e-openrouter

# Host-level real-`goose` E2E — dispatches 01-typo-fix through a real model
# and asserts the WORK happened (typo actually fixed), not just that the spec
# settled. Out-of-band from `just ci` — needs OpenRouter creds + network.
e2e-host:
    bash tests/e2e/host-smoke.sh

# Regenerate the SQLx offline cache (.sqlx/) — run after changing any
# `sqlx::query!` macro. Dev prerequisite (one-time):
#   cargo install sqlx-cli --no-default-features --features sqlite,rustls
# Uses DATABASE_URL from .env (sqlite://.dev.db). CI reads the committed
# .sqlx/ cache with SQLX_OFFLINE=true and never runs this recipe.
prep-sqlx:
    cargo sqlx database create
    cargo sqlx migrate run
    cargo sqlx prepare -- --tests
