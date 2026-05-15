# Multi-stage build for the boi-node binary used in distributed E2E tests.
#
# NOTE (Phase 0a, red-baseline): `cargo build -p boi-node` produces the
# stub binary from crates/boi-node/src/main.rs that exits 78 (EX_CONFIG).
# This Dockerfile builds and packages that stub unchanged; tests assert
# against that exit code to confirm "binary not yet implemented" as the
# red signal. Phase 0c replaces the stub with the real implementation
# and this Dockerfile keeps working without changes.

FROM rust:latest AS builder
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler libprotobuf-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release -p boi-node -p boi-mock-plugin

FROM debian:trixie-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/boi-node /usr/local/bin/boi-node
COPY --from=builder /src/target/release/boi-mock-plugin /usr/local/bin/boi-mock-plugin
ENTRYPOINT ["/usr/local/bin/boi-node"]
