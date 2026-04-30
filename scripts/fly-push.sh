#!/usr/bin/env bash
# Build the BOI bench image and push to Fly.io registry.
# Must be run from the repo root (build context = repo root).
#
# Usage:
#   scripts/fly-push.sh [--app <app-name>] [--no-latest]
#
# Env:
#   FLY_APP   — override app name (default: boi-workers)

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DOCKERFILE="$REPO_ROOT/tests/bench/Dockerfile"

# ── Parse args ───────────────────────────────────────────────────────────────
FLY_APP="${FLY_APP:-boi-workers}"
PUSH_LATEST=true

while [[ $# -gt 0 ]]; do
    case "$1" in
        --app)     FLY_APP="$2"; shift 2 ;;
        --no-latest) PUSH_LATEST=false; shift ;;
        *) echo "Unknown arg: $1" >&2; exit 1 ;;
    esac
done

# ── Resolve BOI version from Cargo.toml ──────────────────────────────────────
VERSION="$(grep '^version' "$REPO_ROOT/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')"
if [[ -z "$VERSION" ]]; then
    echo "ERROR: could not read version from Cargo.toml" >&2
    exit 1
fi
VERSION_TAG="v$VERSION"

REGISTRY="registry.fly.io/$FLY_APP"

echo "==> BOI version: $VERSION_TAG"
echo "==> App:         $FLY_APP"
echo "==> Registry:    $REGISTRY"
echo ""

# ── Authenticate Docker with Fly.io registry ──────────────────────────────────
echo "==> Authenticating Docker with Fly.io registry..."
fly auth docker
echo ""

# ── Build image ───────────────────────────────────────────────────────────────
echo "==> Building image from $DOCKERFILE (context: $REPO_ROOT)..."
docker build \
    -t "$REGISTRY:$VERSION_TAG" \
    -t "$REGISTRY:latest" \
    -f "$DOCKERFILE" \
    "$REPO_ROOT"
echo ""

# ── Push version tag ─────────────────────────────────────────────────────────
echo "==> Pushing $REGISTRY:$VERSION_TAG ..."
docker push "$REGISTRY:$VERSION_TAG"

# ── Push latest tag (optional) ───────────────────────────────────────────────
if [[ "$PUSH_LATEST" == "true" ]]; then
    echo "==> Pushing $REGISTRY:latest ..."
    docker push "$REGISTRY:latest"
fi

echo ""
echo "Done. Image available at:"
echo "  $REGISTRY:$VERSION_TAG"
if [[ "$PUSH_LATEST" == "true" ]]; then
    echo "  $REGISTRY:latest"
fi
