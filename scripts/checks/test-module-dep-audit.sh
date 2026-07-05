#!/usr/bin/env bash
set -uo pipefail
here="$(dirname "$0")"; tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/src/service"

printf 'use crate::{types, runtime};\n' > "$tmp/src/service/x.rs"          # bad: service → runtime
bash "$here/module-dep-audit.sh" "$tmp" >/dev/null 2>&1 \
    && { echo "FAIL: brace-import service→runtime not caught"; exit 1; }

printf 'use crate::types::SpecId;\nuse crate::service::bus::EventBus;\n' > "$tmp/src/service/x.rs"
bash "$here/module-dep-audit.sh" "$tmp" >/dev/null 2>&1 \
    || { echo "FAIL: legal forward + intra-layer import rejected"; exit 1; }

echo "OK: module-dep-audit.sh catches brace-import violations, allows intra-layer"
