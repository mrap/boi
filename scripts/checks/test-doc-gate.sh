#!/usr/bin/env bash
# Regression harness for doc-gate.sh: a conforming fixture must pass,
# and each violation class must fail loudly.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT

mk_fixture() {
    mkdir -p "$tmp/docs/agents" "$tmp/src/cli"
    cat > "$tmp/src/cli/mod.rs" <<'RS'
pub enum Command {
    Dispatch,
    McpServe,
}
RS
    cat > "$tmp/AGENTS.md" <<'MD'
# AGENTS
## CLI
| Command | Does |
|---|---|
| `boi dispatch <spec.toml>` | start a spec |
| `boi mcp-serve` | mcp |
## Spec format
MD
    printf '# README\n' > "$tmp/README.md"
    printf '# t\n' > "$tmp/docs/agents/glossary.md"
    printf '# g\n' > "$tmp/docs/getting-started.md"
    printf '# s\n' > "$tmp/docs/security.md"
    printf '# f\n' > "$tmp/docs/faq.md"
    printf '# m\n' > "$tmp/docs/doc-maintenance.md"
}

mk_fixture
bash "$HERE/doc-gate.sh" "$tmp" >/dev/null 2>&1 \
    || { echo "test-doc-gate FAIL: conforming fixture rejected"; exit 1; }

# violation 1: broken relative link
printf '[x](docs/nope.md)\n' >> "$tmp/README.md"
out=$(bash "$HERE/doc-gate.sh" "$tmp" 2>&1 || true)
grep -q "broken link" <<<"$out" \
    || { echo "test-doc-gate FAIL: broken link not caught"; exit 1; }
mk_fixture

# violation 2: retired/dead reference
printf 'see docs/modes.md\n' >> "$tmp/docs/faq.md"
out=$(bash "$HERE/doc-gate.sh" "$tmp" 2>&1 || true)
grep -q "retired/dead reference" <<<"$out" \
    || { echo "test-doc-gate FAIL: retired reference not caught"; exit 1; }
mk_fixture

# violation 3: enum variant undocumented in CLI table
printf 'pub enum Command {\n    Dispatch,\n    McpServe,\n    Cancel,\n}\n' > "$tmp/src/cli/mod.rs"
out=$(bash "$HERE/doc-gate.sh" "$tmp" 2>&1 || true)
grep -q "missing from AGENTS.md CLI table" <<<"$out" \
    || { echo "test-doc-gate FAIL: undocumented variant not caught"; exit 1; }
mk_fixture

# violation 4: table documents a nonexistent command
# (heredoc rebuild, not sed -i: BSD and GNU sed disagree on -i '' — on Linux
# GNU sed read '' as a filename and the ghost row was never inserted)
cat > "$tmp/AGENTS.md" <<'MD'
# AGENTS
## CLI
| Command | Does |
|---|---|
| `boi dispatch <spec.toml>` | start a spec |
| `boi mcp-serve` | mcp |
| `boi doctor-x` | ghost |
## Spec format
MD
out=$(bash "$HERE/doc-gate.sh" "$tmp" 2>&1 || true)
grep -q "not a Command enum variant" <<<"$out" \
    || { echo "test-doc-gate FAIL: ghost command not caught"; exit 1; }

echo "test-doc-gate: OK"
