#!/bin/bash
# tests/e2e/run-e2e.sh — Build and run containerized E2E tests for BOI.
#
# Usage:
#   bash tests/e2e/run-e2e.sh
#
# Builds the Docker image from Dockerfile.e2e and runs all E2E tests
# (CLI operations + daemon lifecycle) in an isolated container.
# No host filesystem access, no real Claude API calls.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

echo "Building boi-e2e image..."
docker build -f "$REPO_ROOT/Dockerfile.e2e" -t boi-e2e "$REPO_ROOT"

echo ""
echo "Running E2E tests..."
docker run --rm boi-e2e
