# BOI v2 — the Docker E2E harness (Phase 10.5).
#
# This is the ONLY real-`goose` test in the suite. The hermetic L3
# orchestrator-integration tier (`tests/integration/`) drives the real
# orchestrator/bus/repo against a `MockExecutor` — by design it bypasses every
# `runtime/` provider module (`recipe` / `stream` / `goose` / `preflight` /
# `mcp_server`). This container is where those modules get exercised: a real
# `goose` binary spawned by the real `GooseRuntime`, draining real
# `stream-json`, against a real local LLM (Ollama) — no network-dependent
# provider, no API key, no credit spend.
#
# ## What the container holds
#
# - The `boi` release binary (built in stage 1).
# - A pinned `goose` CLI (>= 1.34 — the verified `stream-json` + recipe-schema
#   floor, `runtime/preflight.rs` `GOOSE_VERSION_REQ`).
# - `ollama` + a tiny local model — `goose`'s provider, so a worker phase runs
#   a real LLM with zero external dependency.
#
# ## What `entrypoint.sh` runs (the 3 scenarios)
#
#   1. `01-typo-fix`         — a real `boi dispatch` of the trivial fixture,
#                              driven to a terminal spec state.
#   2. a scripted cancel     — `boi dispatch` then `boi cancel` mid-run; the
#                              real cancellation-of-a-live-child path.
#   3. a retry-recovery run  — a worker whose first attempt fails, exercising
#                              `GooseRuntime`'s 2-retry loop.
#
# `02`–`05`'s real-provider behaviour is **uncovered at v1.0** (a stated
# risk). `04`-cross-provider real execution would need a second provider in
# the container — a documented v1.0 gap.
#
# Build + run via `just e2e` (out-of-band from `just ci`).

# --- Stage 1: build the boi release binary -------------------------------
FROM rust:1.85-bookworm AS builder

WORKDIR /build
# Copy ONLY the crate-build inputs — not `tests/e2e/` — so editing the E2E
# harness (`entrypoint.sh`) does not bust this layer and re-trigger the ~7 min
# `boi` build. SQLX_OFFLINE makes the `sqlx::query!` macros read the committed
# `.sqlx/` cache, so no build-time DB is needed.
COPY Cargo.toml Cargo.lock rust-toolchain.toml clippy.toml ./
COPY src ./src
COPY migrations ./migrations
COPY .sqlx ./.sqlx
COPY tests/lint_fixtures ./tests/lint_fixtures
COPY tests/fixtures ./tests/fixtures
COPY tests/integration.rs ./tests/integration.rs
COPY tests/integration ./tests/integration
ENV SQLX_OFFLINE=true
RUN cargo build --release --locked --bin boi

# --- Stage 2: the runtime image ------------------------------------------
FROM debian:bookworm-slim

# Runtime deps: `git` (BOI worktrees), `curl`/`ca-certificates` (the goose +
# ollama installers), `bzip2` (the goose tarball), `zstd` (the ollama
# installer extracts a zstd archive), `libgomp1` (the `goose` binary links
# `libgomp.so.1` — the GNU OpenMP runtime — and `bookworm-slim` omits it).
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        git curl ca-certificates bzip2 zstd procps libgomp1 sqlite3 \
    && rm -rf /var/lib/apt/lists/*

# `goose` — the CLI BOI's `GooseRuntime` spawns. Pinned to the verified floor
# (>= 1.34; `runtime/preflight.rs`). `CONFIGURE=false` skips the interactive
# provider setup — the recipe BOI writes carries provider/model itself.
ENV GOOSE_VERSION=v1.34.1
RUN curl -fsSL https://github.com/block/goose/releases/download/${GOOSE_VERSION}/download_cli.sh \
        -o /tmp/install-goose.sh \
    && CONFIGURE=false bash /tmp/install-goose.sh \
    && rm /tmp/install-goose.sh \
    && ls -la /root/.local/bin/goose
ENV PATH="/root/.local/bin:${PATH}"

# `ollama` — the local LLM server. `goose` talks to it over its `ollama`
# provider; no API key, no external network at run time (only at model-pull).
RUN curl -fsSL https://ollama.com/install.sh | sh

# A small instruct model — the worker LLM. `qwen2.5:0.5b` is ~400 MB; it is
# pulled at FIRST RUN by `entrypoint.sh` (a build-time pull would bloat the
# image and needs the ollama server running). `OLLAMA_MODEL` is the single
# knob `entrypoint.sh` + the generated phase TOMLs read.
ENV OLLAMA_MODEL=qwen2.5:0.5b

# The boi binary + the test assets.
COPY --from=builder /build/target/release/boi /usr/local/bin/boi
COPY tests/fixtures/specs/01-typo-fix.toml /e2e/specs/01-typo-fix.toml
COPY tests/fixtures/phases /e2e/phases-src
COPY tests/fixtures/pipelines /e2e/pipelines-src
COPY tests/e2e/entrypoint.sh /e2e/entrypoint.sh
RUN chmod +x /e2e/entrypoint.sh

# The harness drives everything; a non-zero exit fails `just e2e`.
ENTRYPOINT ["/e2e/entrypoint.sh"]
