#!/usr/bin/env bash
# §13.3 — the per-module test-coverage gate (Phase 10.6).
#
# For every module under `src/service/` and `src/runtime/` this enforces:
#
#   L2: a `fn test_l2_*` in the module's file(s) — OR the module is in
#       `E2E_COVERED.toml`'s `l1_only_modules` (its correct tier IS L1).
#   L3: a `fn test_l3_<module>_*` (in `src/` or `tests/integration/`) — OR the
#       module is in `e2e_covered_modules` (the real-`goose` Docker E2E is its
#       L3) — OR in `l3_tier_covered_modules` (the orchestrator-integration
#       tier drives a real spec through it on every `test_l3_fixtures_*` run)
#       — OR in `l1_only_modules` / `l2_sufficient_modules` (documented:
#       integration-grade L2, no hermetic v1.0 L3 surface).
#
# `tests/E2E_COVERED.toml` is the single, explicit, rationale-carrying record
# of how each module's §13.3 coverage is satisfied — see its header for the
# scoping note. A `service/`/`runtime/` module that is none of
# {L2-tested-or-l1-only, L3-tested-or-listed} fails this gate LOUDLY.
#
# An optional first argument overrides the repo root — used by the
# `test-test-coverage.sh` regression harness to point the check at a synthetic
# tree.
set -uo pipefail
root="${1:-$(cd "$(dirname "$0")/../.." && pwd)}"
cd "${root}"

MANIFEST="tests/E2E_COVERED.toml"
[ -f "${MANIFEST}" ] || { echo "LINT FAIL: ${MANIFEST} is missing"; exit 1; }

# Extract a TOML array's string entries: `key = [ "a", "b", … ]` (possibly
# multi-line). Prints one entry per line.
manifest_list() {
    /usr/bin/awk -v key="$1" '
        $0 ~ "^[[:space:]]*" key "[[:space:]]*=" { collecting = 1 }
        collecting {
            n = split($0, toks, "\"")
            for (i = 2; i <= n; i += 2) print toks[i]
            if ($0 ~ /\]/) collecting = 0
        }
    ' "${MANIFEST}"
}

E2E_COVERED="$(manifest_list e2e_covered_modules)"
L3_TIER="$(manifest_list l3_tier_covered_modules)"
L1_ONLY="$(manifest_list l1_only_modules)"
L2_SUFFICIENT="$(manifest_list l2_sufficient_modules)"

# Membership test against a newline-separated list.
in_list() {
    printf '%s\n' "$2" | grep -Fxq "$1"
}

# The set of modules. A module is a `.rs` file under src/service|runtime,
# EXCLUDING `mod.rs` (pure re-export, nothing to test). The orchestrator is
# split across `orchestrator.rs` + `orchestrator/{handlers,run_loop,run_phase,
# tests}.rs` — all fold into the single module `service/orchestrator`.
modules="$(
    find src/service src/runtime -type f -name '*.rs' -not -path '*/target/*' \
        | sed -e 's#^src/##' -e 's#\.rs$##' \
        | grep -v '/mod$' | grep -vx 'service/mod' | grep -vx 'runtime/mod' \
        | sed -e 's#^service/orchestrator/.*#service/orchestrator#' \
        | sort -u
)"

# Every `fn test_l<N>_<...>` name across src/ + tests/integration/, once.
all_test_fns="$(
    grep -rhoE 'fn test_l[123]_[a-z0-9_]+' src tests/integration 2>/dev/null \
        | sed 's/^fn //' | sort -u
)"

fail=0
for module in ${modules}; do
    # The module's source files (the orchestrator's are its whole dir).
    case "${module}" in
        service/orchestrator)
            files="src/service/orchestrator.rs $(find src/service/orchestrator -name '*.rs')"
            ;;
        *)
            files="src/${module}.rs"
            ;;
    esac
    leaf="${module##*/}"   # the bare module name, for the test_l3_<module>_ check

    # --- L2 ---
    has_l2=0
    for f in ${files}; do
        [ -f "${f}" ] && grep -q 'fn test_l2_' "${f}" && has_l2=1
    done
    if [ "${has_l2}" -eq 0 ] && ! in_list "${module}" "${L1_ONLY}"; then
        echo "COVERAGE FAIL: module ${module} has no L2 test and is not l1_only"
        fail=1
    fi

    # --- L3 ---
    # A `test_l3_<leaf>_…` function anywhere counts; so does an E2E-covered or
    # L3-tier-covered or l1-only listing in the manifest.
    has_l3=0
    if printf '%s\n' "${all_test_fns}" | grep -qE "^test_l3_${leaf}_"; then
        has_l3=1
    fi
    if [ "${has_l3}" -eq 0 ] \
        && ! in_list "${module}" "${E2E_COVERED}" \
        && ! in_list "${module}" "${L3_TIER}" \
        && ! in_list "${module}" "${L1_ONLY}" \
        && ! in_list "${module}" "${L2_SUFFICIENT}"; then
        echo "COVERAGE FAIL: module ${module} has no L3 test (no test_l3_${leaf}_*)"
        echo "  and is not in e2e_covered / l3_tier_covered / l1_only / l2_sufficient"
        fail=1
    fi
done

# Sanity: every module the manifest names must still EXIST — a stale manifest
# entry (a deleted/renamed module) is itself a lint failure.
for listed in ${E2E_COVERED} ${L3_TIER} ${L1_ONLY} ${L2_SUFFICIENT}; do
    if ! printf '%s\n' "${modules}" | grep -Fxq "${listed}"; then
        echo "COVERAGE FAIL: ${MANIFEST} lists '${listed}', which is not a module"
        fail=1
    fi
done

if [ "${fail}" -ne 0 ]; then
    echo "LINT FAIL: the §13.3 per-module coverage gate found gaps (see above)"
    exit 1
fi
echo "OK: every service/ + runtime/ module has §13.3 L2 + L3 coverage"
