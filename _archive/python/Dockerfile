# BOI — Beginning of Infinity
# Multi-stage build: compile Rust binary then run E2E tests in a slim runtime.
#
# Build:  docker build -t boi-e2e .
# Run:    docker run --rm boi-e2e

# ── Stage 1: Build ────────────────────────────────────────────────────────────
# Pin to bookworm so GLIBC matches the runtime stage exactly.
FROM rust:1-slim-bookworm AS builder
WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/

RUN cargo build --release

# ── Stage 2: Runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        bash \
        git \
        python3 \
    && rm -rf /var/lib/apt/lists/*

# Non-root user matches common CI conventions
RUN useradd -m -s /bin/bash boi

# Install the boi binary system-wide
COPY --from=builder /build/target/release/boi /usr/local/bin/boi

# Copy the shell wrapper and E2E script
COPY --chown=boi:boi boi.sh       /home/boi/boi.sh
COPY --chown=boi:boi e2e-tests.sh  /home/boi/e2e-tests.sh
COPY --chown=boi:boi test-spec.yaml /home/boi/test-spec.yaml

USER boi
WORKDIR /home/boi

RUN git config --global user.email "boi@localhost" && \
    git config --global user.name "BOI" && \
    git config --global init.defaultBranch main

# Wire ~/.boi/bin/boi so the bash wrapper delegates to the Rust binary
RUN mkdir -p /home/boi/.boi/bin /home/boi/.boi/rust && \
    cp /usr/local/bin/boi /home/boi/.boi/bin/boi && \
    chmod +x /home/boi/boi.sh /home/boi/e2e-tests.sh

ENV PATH="/home/boi/.local/bin:/usr/local/bin:${PATH}"

CMD ["/bin/bash", "/home/boi/e2e-tests.sh"]
