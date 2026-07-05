#!/usr/bin/env bash
# Doc gate: the LIVE documentation set must stay truthful and routed.
# The live set and the frozen-dated-record convention are defined in
# docs/doc-maintenance.md (routing map). Frozen records and design-fiction
# dirs are exempt by design — being outdated there is not drift.
set -uo pipefail
cd "${1:-$(dirname "$0")/../..}"

fail=0
err() { echo "doc-gate FAIL: $*" >&2; fail=1; }

shopt -s nullglob
LIVE=(README.md AGENTS.md src/*/AGENTS.md docs/agents/*.md
      docs/getting-started.md docs/security.md docs/faq.md docs/doc-maintenance.md)
[ ${#LIVE[@]} -ge 4 ] || err "live-doc globs matched only ${#LIVE[@]} files — layout changed? sync docs/doc-maintenance.md"

# 1 — relative markdown links in live docs must resolve.
for f in "${LIVE[@]}"; do
    dir=$(dirname "$f")
    while IFS= read -r t; do
        case "$t" in http*|mailto:*|/*|'~'*|'') continue ;; esac
        [ -e "$dir/$t" ] || err "broken link in $f -> $t"
    done < <(grep -oE '\]\([^)#[:space:]]+' "$f" | sed 's/^](//')
done

# 2 — live docs (maintenance record exempt: it may name history) must not
#     reference retired docs, dead commands, or pre-v2 runtime paths.
BANNED='docs/agents/spec-format\.md|docs/agents/cli-reference\.md|docs/modes\.md|docs/spec-format\.md|docs/yaml-spec-schema\.md|ARCHITECTURE\.md|CONTRIBUTING\.md|boi doctor|~/\.boi/worktrees/'
for f in "${LIVE[@]}"; do
    [ "$f" = "docs/doc-maintenance.md" ] && continue
    hits=$(grep -nE "$BANNED" "$f" || true)
    [ -z "$hits" ] || err "retired/dead reference in $f:"$'\n'"$hits"
done

# 3 — AGENTS.md CLI table <-> src/cli/mod.rs Command enum, both directions.
variants=$(awk '/^pub enum Command/,/^\}/' src/cli/mod.rs \
    | grep -oE '^    [A-Z][A-Za-z]+' | tr -d ' ' \
    | sed -E 's/([a-z0-9])([A-Z])/\1-\2/g' | tr '[:upper:]' '[:lower:]' | sort -u)
documented=$(awk '/^## CLI/,/^## [^C]/' AGENTS.md | grep '^| ' \
    | grep -oE '`boi [a-z][a-z-]*' | awk '{print $2}' | sort -u)
[ -n "$variants" ]   || err "could not extract Command enum variants from src/cli/mod.rs"
[ -n "$documented" ] || err "could not extract CLI table rows from AGENTS.md"
for v in $variants;   do grep -qx "$v" <<<"$documented" || err "CLI enum variant 'boi $v' missing from AGENTS.md CLI table"; done
for d in $documented; do grep -qx "$d" <<<"$variants"   || err "AGENTS.md CLI table documents 'boi $d' which is not a Command enum variant"; done

# 4 — entry/topic budgets (lost-in-the-middle discipline).
for f in README.md AGENTS.md; do
    [ "$(wc -l < "$f")" -le 200 ] || err "$f exceeds 200-line entry budget"
done
for f in docs/agents/*.md; do
    [ "$(wc -l < "$f")" -le 150 ] || err "$f exceeds 150-line topic budget"
done

[ "$fail" -eq 0 ] && echo "doc-gate: OK (${#LIVE[@]} live docs)" || exit 1
