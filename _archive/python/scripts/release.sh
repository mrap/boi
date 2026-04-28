#!/bin/bash
# release.sh — Create a BOI release.
#
# Reads version from boi.sh, updates CHANGELOG.md, creates a git tag, and
# optionally pushes the tag to trigger the GitHub Actions release workflow.
#
# Usage:
#   bash scripts/release.sh              # Full release (tag + push)
#   bash scripts/release.sh --dry-run    # Validate without making changes
#   bash scripts/release.sh --no-push    # Tag locally but don't push

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="${SCRIPT_DIR}/.."
BOI_SH="${REPO_ROOT}/boi.sh"
CHANGELOG="${REPO_ROOT}/CHANGELOG.md"

DRY_RUN=false
NO_PUSH=false

for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=true ;;
        --no-push) NO_PUSH=true ;;
        --help|-h)
            echo "Usage: bash scripts/release.sh [--dry-run] [--no-push]"
            echo ""
            echo "Options:"
            echo "  --dry-run    Validate release without making changes"
            echo "  --no-push    Create tag locally but don't push to remote"
            exit 0
            ;;
        *)
            echo "Unknown option: $arg" >&2
            echo "Usage: bash scripts/release.sh [--dry-run] [--no-push]" >&2
            exit 1
            ;;
    esac
done

# --- Read version from boi.sh (single source of truth) ---
if [[ ! -f "$BOI_SH" ]]; then
    echo "Error: boi.sh not found at ${BOI_SH}" >&2
    exit 1
fi

VERSION=$(grep -m1 '^BOI_VERSION=' "$BOI_SH" | sed 's/BOI_VERSION="//' | sed 's/"//')
TAG="v${VERSION}"

if [[ -z "$VERSION" ]]; then
    echo "Error: Could not read BOI_VERSION from boi.sh" >&2
    exit 1
fi

echo "=== BOI Release ==="
echo "Version: ${VERSION}"
echo "Tag:     ${TAG}"
echo "Dry run: ${DRY_RUN}"
echo ""

# --- Validate preconditions ---

# Check we're in a git repo
if ! git -C "$REPO_ROOT" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    echo "Error: Not inside a git repository" >&2
    exit 1
fi

# Check for uncommitted changes
if [[ -n "$(git -C "$REPO_ROOT" status --porcelain 2>/dev/null)" ]]; then
    echo "Error: Working directory has uncommitted changes. Commit or stash them first." >&2
    if $DRY_RUN; then
        echo "(Continuing anyway for dry-run validation)"
    else
        exit 1
    fi
fi

# Check tag doesn't already exist
if git -C "$REPO_ROOT" tag -l "$TAG" | grep -q "^${TAG}$"; then
    echo "Error: Tag ${TAG} already exists. Bump the version first:" >&2
    echo "  bash scripts/bump-version.sh patch" >&2
    if $DRY_RUN; then
        echo "(Continuing anyway for dry-run validation)"
    else
        exit 1
    fi
fi

# Check CHANGELOG has an entry for this version
if [[ -f "$CHANGELOG" ]]; then
    if ! grep -q "\[${VERSION}\]" "$CHANGELOG"; then
        echo "Warning: CHANGELOG.md has no entry for version ${VERSION}."
        echo "  Add a ## [${VERSION}] section before releasing."
        if ! $DRY_RUN; then
            exit 1
        fi
    else
        echo "CHANGELOG.md has entry for ${VERSION}. Good."
    fi
else
    echo "Warning: CHANGELOG.md not found at ${CHANGELOG}"
    if ! $DRY_RUN; then
        exit 1
    fi
fi

# --- Run tests ---
echo ""
echo "Running tests..."
if ! (cd "$REPO_ROOT" && python3 -m unittest discover -s tests -p 'test_*.py' 2>&1); then
    echo "Error: Tests failed. Fix them before releasing." >&2
    if ! $DRY_RUN; then
        exit 1
    fi
fi
echo "Tests passed."

# --- Dry run stops here ---
if $DRY_RUN; then
    echo ""
    echo "=== Dry run complete ==="
    echo "Everything looks good for release ${TAG}."
    echo "Run without --dry-run to create the release."
    exit 0
fi

# --- Create tag ---
echo ""
echo "Creating tag ${TAG}..."
git -C "$REPO_ROOT" tag -a "$TAG" -m "Release ${VERSION}"
echo "Tag ${TAG} created."

# --- Push tag ---
if $NO_PUSH; then
    echo ""
    echo "Tag created locally (--no-push). Push manually with:"
    echo "  git push origin ${TAG}"
else
    echo ""
    echo "Pushing tag ${TAG} to origin..."
    git -C "$REPO_ROOT" push origin "$TAG"
    echo "Tag pushed. GitHub Actions release workflow should trigger."
fi

echo ""
echo "=== Release ${VERSION} complete ==="
