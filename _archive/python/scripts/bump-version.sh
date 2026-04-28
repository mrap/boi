#!/bin/bash
# bump-version.sh — Bump the BOI version number (major/minor/patch).
#
# Usage:
#   bash scripts/bump-version.sh patch   # 0.1.0 -> 0.1.1
#   bash scripts/bump-version.sh minor   # 0.1.0 -> 0.2.0
#   bash scripts/bump-version.sh major   # 0.1.0 -> 1.0.0
#   bash scripts/bump-version.sh         # Prints current version

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BOI_SH="${SCRIPT_DIR}/../boi.sh"

if [[ ! -f "$BOI_SH" ]]; then
    echo "Error: boi.sh not found at ${BOI_SH}" >&2
    exit 1
fi

# Read current version from boi.sh (single source of truth)
CURRENT_VERSION=$(grep -m1 '^BOI_VERSION=' "$BOI_SH" | sed 's/BOI_VERSION="//' | sed 's/"//')

if [[ -z "$CURRENT_VERSION" ]]; then
    echo "Error: Could not read BOI_VERSION from boi.sh" >&2
    exit 1
fi

BUMP_TYPE="${1:-}"

if [[ -z "$BUMP_TYPE" ]]; then
    echo "$CURRENT_VERSION"
    exit 0
fi

# Parse semver components
IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT_VERSION"

case "$BUMP_TYPE" in
    major)
        MAJOR=$((MAJOR + 1))
        MINOR=0
        PATCH=0
        ;;
    minor)
        MINOR=$((MINOR + 1))
        PATCH=0
        ;;
    patch)
        PATCH=$((PATCH + 1))
        ;;
    *)
        echo "Usage: bash scripts/bump-version.sh [major|minor|patch]" >&2
        echo "  No argument prints the current version." >&2
        exit 1
        ;;
esac

NEW_VERSION="${MAJOR}.${MINOR}.${PATCH}"

# Update boi.sh in place
sed -i.bak "s/^BOI_VERSION=\"${CURRENT_VERSION}\"/BOI_VERSION=\"${NEW_VERSION}\"/" "$BOI_SH"
rm -f "${BOI_SH}.bak"

echo "Bumped version: ${CURRENT_VERSION} -> ${NEW_VERSION}"
