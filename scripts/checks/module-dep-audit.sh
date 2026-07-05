#!/usr/bin/env bash
# Enforce LDA forward-only deps:  types(0) → config(1) → repo(2) → service(3) → runtime(4) → cli(5)
# A file in src/<mod>/ may `use crate::<other>` only when <other> is a LOWER layer; intra-layer is fine.
set -uo pipefail
cd "${1:-$(dirname "$0")/../..}"

declare -A LAYER_ORDER=( [types]=0 [config]=1 [repo]=2 [service]=3 [runtime]=4 [cli]=5 )

# Emit every `crate::<mod>` layer token in a file — handles multi-line `use`
# statements, single paths (crate::types::Foo), and brace groups
# (crate::{types, runtime}, crate::service::{bus::Bus, x}). Brace groups are
# expanded so each sibling gets its own `crate::` prefix before scanning.
crate_deps() {
    /usr/bin/awk '
        { if (in_use) buf = buf " " $0
          else if ($0 ~ /(^|[^[:alnum:]_])use[[:space:]]/) { buf = $0; in_use = 1 } }
        in_use && buf ~ /;/ {
            stmt = buf
            if (match(stmt, /crate::/)) {
                seg = substr(stmt, RSTART)
                changed = 1
                while (changed) {
                    changed = 0
                    if (match(seg, /crate::\{[^{}]*\}/)) {
                        head  = substr(seg, 1, RSTART - 1)
                        group = substr(seg, RSTART + 8, RLENGTH - 9)   # inside crate::{ }
                        rest  = substr(seg, RSTART + RLENGTH)
                        n = split(group, parts, ",")
                        expanded = ""
                        for (i = 1; i <= n; i++) {
                            gsub(/^[[:space:]]+|[[:space:]]+$/, "", parts[i])
                            if (parts[i] != "")
                                expanded = expanded " crate::" parts[i]
                        }
                        seg = head expanded rest
                        changed = 1
                    }
                }
                tmp = seg
                while (match(tmp, /crate::(types|config|repo|service|runtime|cli)([^[:alnum:]_]|$)/)) {
                    inner = substr(tmp, RSTART + 7, RLENGTH - 7)
                    sub(/[^[:alnum:]_].*$/, "", inner)               # strip trailing delimiter
                    print inner
                    tmp = substr(tmp, RSTART + RLENGTH - 1)
                }
            }
            in_use = 0; buf = ""
        }
    ' "$1"
}

fail=0
for mod_dir in src/types src/config src/repo src/service src/runtime src/cli; do
    [ -d "$mod_dir" ] || continue
    mod=$(basename "$mod_dir"); my_order=${LAYER_ORDER[$mod]}
    while IFS= read -r -d '' file; do
        while IFS= read -r dep; do
            [ -z "$dep" ] && continue
            [ "$dep" = "$mod" ] && continue                        # intra-layer — always legal
            if [ "${LAYER_ORDER[$dep]}" -gt "$my_order" ]; then     # strict: dep == mod already skipped
                echo "LAYER VIOLATION: src/$mod imports crate::$dep ($file)"
                fail=1
            fi
        done < <(crate_deps "$file")
    done < <(find "$mod_dir" -type f -name '*.rs' -not -path '*/target/*' -print0 2>/dev/null)
done

[ "$fail" -eq 1 ] && exit 1
echo "OK: all module use-statements respect LDA layering"
