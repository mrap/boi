# Bench: --bare Flag Startup Reduction — critic Phase

**Date:** 2026-04-29
**Phase under test:** `critic`
**Runs per condition:** 3
**Metric:** `startup_ms` (spawn → first stdout byte)

---

## Results

| Condition | Run 1 (ms) | Run 2 (ms) | Run 3 (ms) | Avg (ms) |
|-----------|-----------|-----------|-----------|---------|
| `bare = true`  | 183 | 187 | 183 | **184.3** |
| `bare = false` | 5,209 | 5,348 | 5,214 | **5,257.0** |

**Reduction:** 5,257ms → 184ms — **−96.5%** (28× speedup)

---

## Assertion

> avg startup_ms (bare) < 50% of avg startup_ms (full)

184.3ms < 2,628.5ms — **PASS** (3.5% of full, well under 50% threshold)

---

## Raw data source

`docs/.bench_raw.json` keys `sonnet_bare` and `sonnet_short`.

## Test fixture

`cargo test --lib bare_flag`

Location: `src/spawn.rs` → `mod bench_bare_flag`

---

## Interpretation

The `--bare` flag eliminates CLI session loading, MCP discovery, and skill enumeration. All three bare runs land in a 4ms band (183–187ms), a hard floor set by:

- Process spawn: ~5–10ms
- TLS handshake + DNS: ~30–50ms
- API TTFT for a minimal reply: ~130–150ms

Full-mode runs are tightly clustered around 5,200–5,350ms. The overhead is entirely in CLI initialization, not inference. Switching this phase to bare is safe: `critic` does not use file/repo tools.

## Safe phases for bare=true

| Phase | bare | Rationale |
|-------|------|-----------|
| `critic` | ✅ true | Text-only review, no file tools needed |
| `plan-critique` | ✅ true | Text-only review |
| `spec-critique` | ✅ true | Text-only review |
| `execute` | ❌ false | Needs file/repo tools |
| `task-verify` | ❌ false | Needs file/repo tools |
| `doc-update` | ❌ false | Needs file/repo tools |
| `code-review` | ❌ false | Needs file/repo tools |

---

## Critic Approved

**Reviewed:** 2026-04-29 · Task T2166 · Spec S7EE1

All checks passed: spec integrity, verify commands, code quality, completeness, fleet-readiness, blast-radius.
