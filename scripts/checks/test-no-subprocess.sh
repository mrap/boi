#!/usr/bin/env bash
set -uo pipefail
here="$(dirname "$0")"; tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT

mkdir -p "$tmp/src/service" "$tmp/src/runtime"
printf 'use std::process::Command;\n' > "$tmp/src/service/bad.rs"
bash "$here/no-subprocess-outside-runtime.sh" "$tmp" >/dev/null 2>&1 \
    && { echo "FAIL: service subprocess not caught"; exit 1; }

printf 'use tokio::process::Command;\n' > "$tmp/src/runtime/ok.rs"
rm "$tmp/src/service/bad.rs"
bash "$here/no-subprocess-outside-runtime.sh" "$tmp" >/dev/null 2>&1 \
    || { echo "FAIL: runtime subprocess rejected"; exit 1; }

echo "OK: no-subprocess-outside-runtime.sh regression passed"
