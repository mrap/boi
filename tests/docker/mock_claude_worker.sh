#!/bin/bash
# mock_claude_worker.sh — Mock "claude" binary for Docker bulletproof install test.
#
# Called by the BOI worker run script as:
#   env -u CLAUDECODE claude -p "PROMPT_CONTENT" --model ... --effort ... \
#       --dangerously-skip-permissions --output-format stream-json --verbose
#
# This mock:
#   1. Parses the prompt content from the -p argument
#   2. Extracts the spec path (injected as `{{SPEC_PATH}}` → `/path/to/q-N.spec.md`)
#   3. Marks the first PENDING task DONE in the spec file (atomic write)
#   4. Exits 0
#
# Install at ~/.local/bin/claude before running the bulletproof install test.
# The worker run script prepends $HOME/.local/bin to PATH, so this mock is
# found automatically without modifying the installer.
set -uo pipefail

PROMPT_CONTENT=""

# Parse arguments: grab content after -p, ignore all other flags
while [[ $# -gt 0 ]]; do
    case "$1" in
        -p)
            PROMPT_CONTENT="${2:-}"
            shift 2
            ;;
        *)
            shift
            ;;
    esac
done

if [[ -z "${PROMPT_CONTENT}" ]]; then
    echo "mock_claude_worker: no -p argument received" >&2
    exit 1
fi

# Extract spec path: the worker-prompt.md template emits the spec path on its
# own line wrapped in backticks: `{{SPEC_PATH}}` → `/path/to/q-N.spec.md`
# Match any absolute path ending in .spec.md
SPEC_PATH=$(printf '%s\n' "${PROMPT_CONTENT}" | grep -oE '/[^ \`\n]+\.spec\.md' | head -1)

if [[ -z "${SPEC_PATH}" ]]; then
    echo "mock_claude_worker: could not extract spec path from prompt" >&2
    echo "mock_claude_worker: prompt snippet: $(printf '%s' "${PROMPT_CONTENT}" | head -5)" >&2
    exit 1
fi

if [[ ! -f "${SPEC_PATH}" ]]; then
    echo "mock_claude_worker: spec file not found: ${SPEC_PATH}" >&2
    exit 1
fi

# Mark first PENDING task as DONE (atomic write via .tmp + os.replace)
python3 - "${SPEC_PATH}" <<'PYEOF'
import os
import re
import sys

spec_path = sys.argv[1]

with open(spec_path) as f:
    content = f.read()

lines = content.split('\n')
result = []
marked = False
in_task = False

for line in lines:
    # Match standard BOI task headings: ### t-N: title
    if re.match(r'^### t-\d+:', line):
        in_task = True
    # If we're inside a task heading block, mark the first PENDING line DONE
    if in_task and not marked and line.strip() == 'PENDING':
        result.append('DONE')
        marked = True
        in_task = False
        continue
    result.append(line)

if not marked:
    print(f"mock_claude_worker: no PENDING task found in {spec_path}", file=sys.stderr)
    sys.exit(1)

tmp = spec_path + '.mock.tmp'
with open(tmp, 'w') as f:
    f.write('\n'.join(result))
os.replace(tmp, spec_path)
print(f"mock_claude_worker: marked t-1 DONE in {spec_path}")
sys.exit(0)
PYEOF
