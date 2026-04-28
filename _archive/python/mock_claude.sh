#!/bin/bash
# mock_claude.sh — Mock Claude for integration tests.
#
# Simulates Claude worker behavior by reading the spec file,
# finding the assigned task (or first PENDING task), marking it DONE,
# and writing a verification artifact.
#
# Usage:
#   mock_claude.sh <spec_path> [task_id] [delay_seconds] [exit_code]
#
# Arguments:
#   spec_path       — Path to the spec file to modify
#   task_id         — Specific task ID to complete (e.g., t-1). If omitted,
#                     completes the first PENDING task.
#   delay_seconds   — Seconds to sleep before completing (default: 0)
#   exit_code       — Exit code to return (default: 0)
#
# The script writes atomically (write to .tmp, then mv) to prevent
# corruption when multiple workers modify the same spec file.

set -uo pipefail

SPEC_PATH="${1:?Usage: mock_claude.sh <spec_path> [task_id] [delay_seconds] [exit_code]}"
TASK_ID="${2:-}"
DELAY="${3:-0}"
EXIT_CODE="${4:-0}"

# Sleep if requested (simulate work)
if [ "$DELAY" != "0" ]; then
    sleep "$DELAY"
fi

# If exit code is non-zero and no task specified, just fail
if [ "$EXIT_CODE" != "0" ] && [ -z "$TASK_ID" ]; then
    exit "$EXIT_CODE"
fi

# Read spec file
if [ ! -f "$SPEC_PATH" ]; then
    echo "ERROR: Spec file not found: $SPEC_PATH" >&2
    exit 1
fi

# Use Python for reliable spec modification (handles edge cases better than sed)
python3 - "$SPEC_PATH" "$TASK_ID" <<'PYEOF'
import os
import re
import sys

spec_path = sys.argv[1]
target_task_id = sys.argv[2] if len(sys.argv) > 2 and sys.argv[2] else None

with open(spec_path, 'r') as f:
    content = f.read()

lines = content.split('\n')
result = []
found_target = False
in_target_task = False
marked = False

for i, line in enumerate(lines):
    # Check for task heading
    heading_match = re.match(r'^### (t-\d+):', line)
    if heading_match:
        task_id = heading_match.group(1)
        if target_task_id:
            in_target_task = (task_id == target_task_id)
        else:
            in_target_task = not marked  # First task encountered
        result.append(line)
        continue

    # Mark PENDING as DONE for the target task
    if in_target_task and not marked and line.strip() == 'PENDING':
        result.append(line.replace('PENDING', 'DONE'))
        marked = True
        in_target_task = False

        # Write verify artifact
        verify_dir = os.path.dirname(spec_path)
        artifact_path = os.path.join(verify_dir, f'{target_task_id or "task"}.verify')
        with open(artifact_path, 'w') as af:
            af.write(f'DONE by mock_claude at {os.getpid()}\n')
        continue

    result.append(line)

# Atomic write
tmp_path = spec_path + '.tmp'
with open(tmp_path, 'w') as f:
    f.write('\n'.join(result))
os.rename(tmp_path, spec_path)

if not marked:
    print(f"WARNING: No PENDING task found to mark DONE", file=sys.stderr)
    sys.exit(1)
PYEOF

exit "$EXIT_CODE"
