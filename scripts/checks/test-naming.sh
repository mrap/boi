#!/usr/bin/env bash
# S14: tests in src/service/ and src/runtime/ must be named test_l<1|2|3>_<name>.
# Enables grep-based level-coverage CI (no proc-macro).
# Fixes consensus convergent #2: uses find + xargs so FILENAME populates in awk.
#
# The trigger regex matches BOTH `#[test]` and `#[tokio::test]` (and any other
# `…test]` attribute): the service and runtime layers are almost entirely
# async, so keying on a literal `#[test]` let every `#[tokio::test]` escape the
# naming gate — a false-green CI gate (review A-cr-3). `scripts/checks/
# test-test-naming.sh` is the regression harness proving an async test is now
# caught.
#
# An optional first argument overrides the repo root — used by the regression
# harness to point the check at a synthetic tree.
set -euo pipefail
root="${1:-$(cd "$(dirname "$0")/../.." && pwd)}"
cd "$root"

violations=$(find src/service src/runtime tests \
        -type f -name '*.rs' \
        -not -path '*/target/*' \
        -print0 2>/dev/null \
    | xargs -0 -n1 /usr/bin/awk '
        /#\[(tokio::)?test\]/ {flag=1; next}
        flag && /fn / {
            flag=0;
            if ($0 !~ /fn test_l[123]_/) {
                printf "%s:%d: %s\n", FILENAME, NR, $0
            }
        }
    ' 2>/dev/null \
    || true)

if [ -n "$violations" ]; then
    echo "LINT FAIL: tests in service/runtime/integration must be named test_l<1|2|3>_<name>:"
    echo "$violations"
    exit 1
fi

# --- §13.3 `test_l3_<module>_<name>` attribution (Phase 10.6) -------------
#
# An L3 test must be ATTRIBUTABLE to a module: it is named
# `test_l3_<module>_<name>` where `<module>` is a real `src/service/` or
# `src/runtime/` module name, OR a `tests/integration/` file stem (`fixtures`
# / `failures` / `harness`). `scripts/checks/test-coverage.sh` relies on this
# attribution to credit a module's L3 coverage — a misattributed L3 test
# (`test_l3_handle_event_*` for a test of the `orchestrator` module) would
# silently leave that module's coverage uncredited. This check makes a bad
# `<module>` segment a loud failure.
# The known module names — every `.rs` under `src/service/` + `src/runtime/`
# (any dir that exists), plus the three `tests/integration/` file stems.
# `|| true` + `2>/dev/null` keep a missing dir benign (the regression harness
# points this check at minimal synthetic trees).
modules="$(
    {
        for d in src/service src/runtime; do
            [ -d "$d" ] && find "$d" -type f -name '*.rs' -not -path '*/target/*' \
                2>/dev/null | sed -e 's#^src/[a-z]*/##' -e 's#\.rs$##' -e 's#/.*##'
        done
        echo fixtures
        echo failures
        echo harness
    } | sort -u || true
)"

# Every `test_l3_*` function name across src/ + tests/integration/.
l3_fns="$(
    grep -rhoE 'fn test_l3_[a-z0-9_]+' src tests/integration 2>/dev/null \
        | sed 's/^fn test_l3_//' | sort -u || true
)"

bad_attr=""
for fn in $l3_fns; do
    matched=0
    for m in $modules; do
        # The fn name must begin `<module>_` — a module segment then a name.
        case "$fn" in
            "${m}_"*) matched=1; break ;;
        esac
    done
    [ "$matched" -eq 0 ] && bad_attr="${bad_attr}  test_l3_${fn}\n"
done

if [ -n "$bad_attr" ]; then
    echo "LINT FAIL: these L3 tests are not named test_l3_<module>_<name> for"
    echo "a real service/runtime module (or a tests/integration/ file stem):"
    printf "%b" "$bad_attr"
    exit 1
fi

echo "OK: all tests follow test_l<N>_ naming + the test_l3_<module>_ convention"
