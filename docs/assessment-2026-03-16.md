# BOI Codebase Assessment — 2026-03-16

## Summary

Total Python source: **13,592 lines** across 19 files
Total tests: **1,536 test cases** collected
Average cyclomatic complexity: **B (5.59)**

---

## 1. Top 10 Most Complex Functions

| Rank | Function | File | Complexity | Grade |
|------|----------|------|-----------|-------|
| 1 | `format_dashboard` | `lib/status.py:1091` | 46 | F |
| 2 | `format_queue_table` | `lib/status.py:563` | 40 | E |
| 3 | `process_worker_completion` | `lib/daemon_ops.py:142` | 39 | E |
| 4 | `format_telemetry_table` | `lib/status.py:956` | 28 | D |
| 5 | `purge` | `lib/queue.py:484` | 21 | D |
| 6 | `purge` | `lib/db.py:378` | 20 | C |
| 7 | `_sort_by_dag` | `lib/status.py:438` | 20 | C |
| 8 | `validate_mode_compliance` | `lib/critic.py:114` | 19 | C |
| 9 | `migrate_from_json` | `lib/db.py:1463` | 18 | C |
| 10 | `parse_boi_spec` | `lib/spec_parser.py:229` | 18 | C |

**Notable:** `format_dashboard` (CC=46) and `process_worker_completion` (CC=39) are both far above the "high risk" threshold of 20. These are the two primary targets.

**Other high-complexity functions (CC > 10):**
- `process_evaluation_completion` (daemon_ops.py:1390) — C (17)
- `process_critic_completion` (daemon_ops.py:816) — C (16)
- `enqueue` (db.py:189) — C (16)
- `_migrate_iteration_files` (db.py:1592) — C (16)
- `process_decomposition_completion` (daemon_ops.py:1006) — C (14)
- `check_needs_review_timeouts` (daemon_ops.py:1243) — C (14)
- `_heal_max_running_duration` (daemon_ops.py:1708) — C (14)
- `_heal_stale_running_specs` (daemon_ops.py:1636) — C (11)
- `pick_next_spec` (db.py:522) — C (11)
- `dequeue` (lib/queue.py:225) — C (12)
- `_fallback_completion` (daemon.py:683) — C (13)

---

## 2. Dead Code Candidates

From `vulture` analysis (60%+ confidence):

### High confidence (90-100%):
- `daemon.py:118` — unused variable `frame` in signal handler (100%)
- `lib/daemon_ops.py:33` — unused import `evaluate_criteria` (90%)

### Medium confidence (60%):
**daemon.py:**
- `lock_file` attribute (line 95) — assigned but never read
- `converter` attribute (line 1007)

**lib/daemon_ops.py:**
- `get_active_count` function (line 794)
- `DEFAULT_DECOMPOSITION_TIMEOUT_SECONDS` variable (line 1003)
- `DEFAULT_EVALUATION_TIMEOUT_SECONDS` variable (line 1387)

**lib/db.py:**
- `row_factory` attribute (line 72)
- `enqueue` method (line 189) — large method, 0 callers found
- `purge` method (line 378)
- `has_reached_max_iterations` method (line 677)
- `get_worker_current_spec` method (line 1169)
- `get_active_processes` method (line 1230)
- `make_started_at` method (line 1323)
- `get_events` method (line 1339)
- `insert_iteration` method (line 1381)
- `get_iterations` method (line 1445)
- `migrate_from_json` method (line 1463) — 218 lines, complex (C18), possibly dead

**lib/critic.py:**
- `validate_mode_compliance` (line 114)
- `generate_auto_reject_task` (line 483)
- `get_next_task_id` (line 509)
- `run_critic` (line 524)

**lib/queue.py:**
- `enqueue` (line 139)
- `purge` (line 484)
- `update_task_counts` (line 603)
- `get_experiment_budget` (line 673)
- `set_experiment_budget` (line 678)

**lib/spec_parser.py:**
- `to_dict` method (line 32)
- `convert_tasks_to_spec` (line 425)
- `parse_error_log` (line 490)
- `extract_error_log_section` (line 553)

**lib/status.py:**
- `CYAN` constant (line 30)
- `build_queue_status` (line 172)
- `filter_entries` (line 505)
- `format_queue_table` (line 563) — CC=40, possibly dead!
- `format_queue_json` (line 807)
- `build_telemetry` (line 815)
- `format_telemetry_table` (line 956) — CC=28, possibly dead!
- `format_telemetry_json` (line 1071)
- `format_dashboard` (line 1091) — CC=46, possibly dead!
- `get_visible_queue_ids` (line 1326)

**Note:** Many `lib/status.py` functions appear dead to vulture because they're called from `dashboard.sh` via subprocess, not from Python directly. Verify before deleting.

---

## 3. Coverage Gaps

### Files with 0 dedicated test files:

| File | Lines | Functions | Coverage Status |
|------|-------|-----------|----------------|
| `lib/status.py` | 1,364 | 35 | **0 unit tests** |
| `lib/queue_compat.py` | 403 | 24 | **0 unit tests** |
| `lib/review.py` | 324 | 6 | **0 unit tests** |
| `lib/cli_ops.py` | 188 | 6 | **0 unit tests** |
| `lib/db_to_json.py` | 156 | 4 | **0 unit tests** |
| `lib/do.py` | 177 | 5 | **0 unit tests** |
| `lib/locking.py` | 39 | 1 | **0 unit tests** |

### Files with thin coverage:
- `lib/daemon_ops.py` — 118 tests, but 23 functions and CC avg is B/C (many complex functions under-tested)
- `daemon.py` — covered via `test_daemon.py` and `test_daemon_new.py` but self-heal paths untested

### Well-covered files:
- `lib/db.py` — `test_db.py` (97K, comprehensive)
- `lib/spec_parser.py` — `test_spec_parser.py` (25K, comprehensive)
- `lib/queue.py` — `test_queue.py` (52K, comprehensive)
- `lib/critic.py` — `test_critic.py` (36K, comprehensive)

---

## 4. Dependency Graph

```
daemon.py
  └── lib.db

worker.py
  └── lib.spec_parser

lib/daemon_ops.py  [highest coupling]
  ├── lib.critic
  ├── lib.critic_config
  ├── lib.evaluate
  ├── lib.event_log
  ├── lib.hooks
  ├── lib.queue
  ├── lib.spec_parser
  ├── lib.spec_validator
  └── lib.telemetry

lib/db.py
  └── (standalone — no lib deps)

lib/status.py
  └── lib.telemetry

lib/queue.py
  ├── lib.event_log
  └── lib.locking

lib/critic.py
  └── lib.critic_config

lib/telemetry.py
  └── (standalone)

lib/evaluate.py
  └── (standalone)

lib/spec_parser.py
  └── (standalone)

lib/spec_validator.py
  └── lib.spec_parser
```

**daemon_ops.py imports 9 lib modules** — it is the highest-coupling module and the primary integration point.

---

## 5. Prioritized Refactoring List

### Priority 1: `lib/status.py` — Complexity + 0 coverage
- **Why:** 3 functions with CC > 20 (including F-grade at 46), 1364 lines, NO unit tests
- **Action:** Write characterization tests first, then decompose `format_dashboard` and `format_queue_table`
- **Risk:** Medium — called from `dashboard.sh`, behavior observable

### Priority 2: `lib/daemon_ops.py` — Critical path + extreme complexity
- **Why:** `process_worker_completion` CC=39 is the main dispatch function. Any bug here breaks the whole pipeline. Also highest coupling (9 imports).
- **Action:** Decompose `process_worker_completion` into named sub-handlers
- **Risk:** High — changes here can break everything. Requires characterization tests.

### Priority 3: Dead code cleanup in `lib/db.py`
- **Why:** ~10 methods flagged as unused (including `migrate_from_json` at C18). Removing them reduces maintenance surface.
- **Action:** Verify each flagged method against callers, remove confirmed dead code
- **Risk:** Low — additive removal

### Priority 4: `lib/queue_compat.py` — CONFIRMED DEAD CODE (2026-03-16)
- **Why:** 403 lines, 24 functions, 0 tests. Was a compatibility shim for SQLite migration.
- **Finding:** Zero imports from any production file. `grep -r "from lib.queue_compat\|import queue_compat" src/` → 0 matches. All production code imports directly from `lib.queue` or `lib.db`.
- **Action:** Safe to delete. Characterization tests written (24 tests, all pass) and committed in `tests/test_queue_compat.py` to lock in behavior before removal.
- **Risk:** Low — nothing depends on it. Delete cleanly.

### Priority 5: Unused import cleanup in `daemon_ops.py`
- **Why:** `evaluate_criteria` imported but not used — clutters namespace
- **Action:** Remove unused imports
- **Risk:** Trivial

---

## Self-Evolution Trigger

The following discoveries trigger new tasks per spec rules:
- `lib/status.py`: CC=46 (> 20 threshold) AND 0 test coverage → dedicated task required
- `lib/daemon_ops.py`: `process_worker_completion` CC=39 (> 20 threshold) → dedicated task required
- `lib/queue_compat.py`: 0 test coverage + 403 lines → dedicated task required
