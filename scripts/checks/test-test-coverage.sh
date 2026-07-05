#!/usr/bin/env bash
# Regression harness for test-coverage.sh (Phase 10.6).
#
# Proves the §13.3 per-module coverage gate CATCHES an uncovered module and
# PASSES a covered one — a false-green coverage gate is worse than none.
set -uo pipefail
here="$(dirname "$0")"
tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT

# Build a synthetic repo skeleton: a manifest + one `service/` module.
seed() {
    rm -rf "${tmp}/src" "${tmp}/tests"
    mkdir -p "${tmp}/src/service" "${tmp}/tests"
    cp "${here}/test-coverage.sh" "${tmp}/test-coverage.sh" 2>/dev/null || true
}

# 1. A module with NO test of any kind, NOT on any manifest list → CAUGHT.
seed
cat > "${tmp}/tests/E2E_COVERED.toml" <<'TOML'
e2e_covered_modules = []
l3_tier_covered_modules = []
l1_only_modules = []
l2_sufficient_modules = []
TOML
cat > "${tmp}/src/service/widget.rs" <<'RS'
pub fn widget() {}
#[cfg(test)]
mod tests {
    #[test]
    fn test_l1_widget_smoke() {}
}
RS
bash "${here}/test-coverage.sh" "${tmp}" >/dev/null 2>&1 \
    && { echo "FAIL: an uncovered module (L1-only, unlisted) was not caught"; exit 1; }

# 2. A module with L2 but no L3 and not listed → still CAUGHT (the L3 gap).
seed
cat > "${tmp}/tests/E2E_COVERED.toml" <<'TOML'
e2e_covered_modules = []
l3_tier_covered_modules = []
l1_only_modules = []
l2_sufficient_modules = []
TOML
cat > "${tmp}/src/service/widget.rs" <<'RS'
pub fn widget() {}
#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn test_l2_widget_integration() {}
}
RS
bash "${here}/test-coverage.sh" "${tmp}" >/dev/null 2>&1 \
    && { echo "FAIL: a module with L2 but no L3 (unlisted) was not caught"; exit 1; }

# 3. A stale manifest entry — a listed module that does not exist → CAUGHT.
seed
cat > "${tmp}/tests/E2E_COVERED.toml" <<'TOML'
e2e_covered_modules = []
l3_tier_covered_modules = ["service/ghost"]
l1_only_modules = []
l2_sufficient_modules = []
TOML
cat > "${tmp}/src/service/widget.rs" <<'RS'
pub fn widget() {}
#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn test_l2_widget_integration() {}
    #[tokio::test]
    async fn test_l3_widget_end_to_end() {}
}
RS
bash "${here}/test-coverage.sh" "${tmp}" >/dev/null 2>&1 \
    && { echo "FAIL: a stale manifest entry (nonexistent module) was not caught"; exit 1; }

# 4. A fully-covered module — L2 + a `test_l3_<module>_*` → PASSES.
seed
cat > "${tmp}/tests/E2E_COVERED.toml" <<'TOML'
e2e_covered_modules = []
l3_tier_covered_modules = []
l1_only_modules = []
l2_sufficient_modules = []
TOML
cat > "${tmp}/src/service/widget.rs" <<'RS'
pub fn widget() {}
#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn test_l2_widget_integration() {}
    #[tokio::test]
    async fn test_l3_widget_end_to_end() {}
}
RS
bash "${here}/test-coverage.sh" "${tmp}" >/dev/null 2>&1 \
    || { echo "FAIL: a fully L2+L3-covered module was rejected"; exit 1; }

# 5. An L3 gap closed by a manifest `l2_sufficient_modules` listing → PASSES.
seed
cat > "${tmp}/tests/E2E_COVERED.toml" <<'TOML'
e2e_covered_modules = []
l3_tier_covered_modules = []
l1_only_modules = []
l2_sufficient_modules = ["service/widget"]
TOML
cat > "${tmp}/src/service/widget.rs" <<'RS'
pub fn widget() {}
#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn test_l2_widget_integration() {}
}
RS
bash "${here}/test-coverage.sh" "${tmp}" >/dev/null 2>&1 \
    || { echo "FAIL: an L2-sufficient-listed module was rejected"; exit 1; }

echo "OK: test-coverage.sh regression passed"
