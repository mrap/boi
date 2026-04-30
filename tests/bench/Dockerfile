# Build context: BOI repo root (for COPY src/ and COPY tests/bench/)
# Usage: docker build -t boi-bench -f tests/bench/Dockerfile .

# ── Stage 1: compile BOI binary ───────────────────────────────────────────────
FROM rust:1.86-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/
COPY hooks/ ./hooks/
RUN cargo build --release 2>&1

# ── Stage 2: runtime image ────────────────────────────────────────────────────
FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive

# System deps: git, sqlite, jq, curl, ca-certs, Node.js 20 (for claude CLI)
RUN apt-get update && apt-get install -y --no-install-recommends \
    bash git sqlite3 jq curl ca-certificates gnupg \
    && curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*

# Pin claude CLI to match host dev version
RUN npm install -g @anthropic-ai/claude-code@2.1.123

# Install boi binary from builder stage
COPY --from=builder /build/target/release/boi /usr/local/bin/boi
RUN chmod +x /usr/local/bin/boi

# Create non-root bench user + required dirs
RUN useradd -m -s /bin/bash bench \
    && mkdir -p /home/bench/.boi/worktrees \
                /home/bench/.boi/logs \
                /home/bench/.boi/telemetry \
                /home/bench/.boi/phases \
                /home/bench/.claude \
                /opt/boi \
                /out \
    && chown -R bench:bench /home/bench /out

# Copy entrypoint (build context = repo root)
COPY tests/bench/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# Bake phases and templates so containers work without /opt/boi volume mount
COPY --chown=bench:bench phases/ /home/bench/.boi/phases/
COPY --chown=bench:bench templates/ /home/bench/.boi/templates/
COPY --chown=bench:bench phases/pipelines.toml /home/bench/.boi/pipelines.toml

USER bench

ENV HOME=/home/bench
ENV BOI_HOME=/home/bench/.boi
# Prefer user-baked phases over /opt/boi volume (still works when mounted)
ENV BOI_PHASES_DIR=/home/bench/.boi/phases
# claude CLI reads API keys from environment
ENV ANTHROPIC_API_KEY=""
ENV OPENROUTER_API_KEY=""

# /opt/boi: BOI source tree, mounted RO at runtime (phases, pipelines, bench_specs)
# /out:     results + logs, mounted RW at runtime
VOLUME ["/opt/boi", "/out"]

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
# Default: run bench over the full spec battery; override pipeline args at runtime
CMD ["--battery", "/opt/boi/tests/bench_specs"]
