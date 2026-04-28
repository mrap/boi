#!/bin/bash
set -uo pipefail

# Auth: ANTHROPIC_API_KEY must be passed as env var
if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
    echo "ERROR: ANTHROPIC_API_KEY env var required" >&2
    exit 1
fi

# Run the worker's run script (mounted at /workspace/run.sh)
if [ -f "/workspace/run.sh" ]; then
    exec bash /workspace/run.sh
else
    echo "ERROR: No run script found at /workspace/run.sh" >&2
    exit 1
fi
