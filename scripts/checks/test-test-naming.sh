#!/usr/bin/env bash
# Regression harness for test-naming.sh (review A-cr-3).
#
# The bug: the trigger keyed on a literal `#[test]`, so `#[tokio::test]` —
# every async test in service/ and runtime/ — escaped the naming gate. This
# harness proves a badly-named `#[tokio::test]` is now CAUGHT, and a
# correctly-named one still passes.
set -uo pipefail
here="$(dirname "$0")"; tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT

mkdir -p "$tmp/src/service" "$tmp/src/runtime" "$tmp/tests"

# 1. A badly-named `#[tokio::test]` MUST be caught (the regression).
cat > "$tmp/src/service/bad.rs" <<'RS'
#[tokio::test]
async fn badly_named_async_test() {}
RS
bash "$here/test-naming.sh" "$tmp" >/dev/null 2>&1 \
    && { echo "FAIL: badly-named #[tokio::test] not caught"; exit 1; }

# 2. A badly-named plain `#[test]` is still caught (no behavior lost).
rm "$tmp/src/service/bad.rs"
cat > "$tmp/src/runtime/bad_sync.rs" <<'RS'
#[test]
fn badly_named_sync_test() {}
RS
bash "$here/test-naming.sh" "$tmp" >/dev/null 2>&1 \
    && { echo "FAIL: badly-named #[test] not caught"; exit 1; }

# 3. Correctly-named tests of BOTH kinds pass.
rm "$tmp/src/runtime/bad_sync.rs"
cat > "$tmp/src/runtime/ok.rs" <<'RS'
#[tokio::test]
async fn test_l2_async_ok() {}

#[test]
fn test_l1_sync_ok() {}
RS
bash "$here/test-naming.sh" "$tmp" >/dev/null 2>&1 \
    || { echo "FAIL: correctly-named tests rejected"; exit 1; }

echo "OK: test-naming.sh regression passed"
