# T4614 DB Survey Findings
# Surveyed: 2026-05-04T03:32:52Z against ~/.boi/boi-rust.db
# NOTE: H1 (worker_id never set) was fixed 2026-05-04. See 2026-05-daemon-consistency.md §Resolution.
# Points 3 and the Key code locations below describe the pre-fix state.

## DB structure discoveries (prerequisites for accurate queries)

Before writing the three symptom queries, several schema gaps were found that
reshape how each symptom can be detected:

1. **`workers` table is never written to.** No INSERT INTO workers exists in
   any source file. The table is dead schema — it has the right columns
   (id, worktree_path, current_spec_id, current_pid) but zero rows ever.

2. **`processes` table is never written to.** Same situation — schema exists,
   zero code paths produce rows, zero rows today.

3. **`worker_id` on `specs` is never SET, only NULLed.** `update_spec()` in
   `queue.rs:500` transitions status but never touches `worker_id`. The only
   write is `recover_stuck_specs()` (line 912) which NULLs it on reset.
   Therefore ALL running specs have `worker_id = NULL` — this is structural,
   not a symptom of a specific failure.

4. **`iterations` table is also currently empty** (0 rows). Iterations are
   only written by `run_worker_with_phases()` in `worker.rs` — specs running
   as remote Claude Code agents do not go through that path.

These gaps mean the symptom queries must be written against `specs` and
`iterations` rather than `workers`/`processes`.

---

## Counts as of 2026-05-04T03:32:52Z

| Symptom | Query variant | Count |
|---------|--------------|-------|
| Ghost worker (running > 1h, no recent iteration) | SQL below | **0** |
| Duplicate: two workers holding same spec_id | workers table | **0** (table empty) |
| Duplicate: running spec with no worker_id | specs table | **14** |
| Stale status > 1 hour | SQL below | **0** |
| Stale status > 30 min | SQL below | **0** |

The 14 `running_no_worker_id` specs are the current live dispatch batch
(started 03:23–03:29 UTC, all < 30 min ago). This count is expected given
that `worker_id` is structurally never populated — it's a persistent gap,
not a transient fault.

---

## Health-check SQL queries

These three queries are the ongoing health checks. Run them with:
`sqlite3 ~/.boi/boi-rust.db < query.sql`

### Query 1 — Ghost worker
```sql
-- Ghost worker: spec is 'running' for > 1 hour with no iteration
-- activity in the same window. Indicates a Claude Code agent that
-- exited without marking the spec completed/failed.
SELECT
  id,
  title,
  status,
  started_at,
  (SELECT MAX(started_at) FROM iterations WHERE spec_id = specs.id) AS last_iter_started
FROM specs
WHERE status = 'running'
  AND started_at < datetime('now', '-1 hour')
  AND (
    NOT EXISTS (
      SELECT 1 FROM iterations i
      WHERE i.spec_id = specs.id
        AND i.started_at > datetime('now', '-1 hour')
    )
  );
-- COUNT variant for health-check pass/fail:
-- SELECT COUNT(*) FROM specs WHERE status='running'
--   AND started_at < datetime('now','-1 hour')
--   AND NOT EXISTS (SELECT 1 FROM iterations i WHERE i.spec_id=specs.id
--                   AND i.started_at > datetime('now','-1 hour'));
```

### Query 2 — Duplicate assignment
```sql
-- Part A: same spec_id in two workers rows (kept for when workers table is used)
SELECT current_spec_id, COUNT(*) AS worker_count
FROM workers
WHERE current_spec_id IS NOT NULL
GROUP BY current_spec_id
HAVING COUNT(*) > 1;

-- Part B: running spec with no worker_id
-- H1 fixed 2026-05-04: update_spec_running() now sets worker_id.
-- This query should return 0 for specs dispatched after the fix.
SELECT COUNT(*) AS running_no_worker_id
FROM specs
WHERE status = 'running'
  AND (worker_id IS NULL OR worker_id = '');
```

### Query 3 — Stale status
```sql
-- Stale status: spec in running/assigning with no iteration activity
-- for > 1 hour AND itself started > 1 hour ago.
SELECT
  id,
  title,
  status,
  started_at,
  (SELECT MAX(started_at) FROM iterations WHERE spec_id = specs.id) AS last_iter
FROM specs
WHERE status IN ('running', 'assigning')
  AND started_at < datetime('now', '-1 hour')
  AND (
    NOT EXISTS (SELECT 1 FROM iterations WHERE spec_id = specs.id)
    OR
    (SELECT MAX(started_at) FROM iterations WHERE spec_id = specs.id)
      < datetime('now', '-1 hour')
  )
ORDER BY started_at ASC;
-- COUNT variant:
-- SELECT COUNT(*) FROM specs WHERE status IN ('running','assigning')
--   AND started_at < datetime('now','-1 hour')
--   AND (NOT EXISTS (SELECT 1 FROM iterations WHERE spec_id=specs.id)
--        OR (SELECT MAX(started_at) FROM iterations WHERE spec_id=specs.id)
--              < datetime('now','-1 hour'));
```

---

## Key code locations for TA152

*(Pre-fix state — H1 fixed 2026-05-04)*
- `queue.rs:503`  — `update_spec_running()`: **new** — sets status='running' AND worker_id atomically
- `queue.rs:512`  — `update_spec()`: sets status but still does NOT set worker_id (use `update_spec_running` for the running transition)
- `queue.rs:924`  — `recover_stuck_specs()`: resets ALL running/assigning to queued on daemon start
- `worker.rs`     — now calls `queue.update_spec_running(spec_id, &worker_id)` at worker entry
- `cli/daemon.rs:214` — calls `recover_stuck_specs()` on every daemon startup

## The core race (for TA152 hypotheses section)

`recover_stuck_specs()` is called unconditionally on daemon startup
(`daemon.rs:215`). If the daemon crashes and restarts while Claude Code
agents are still processing specs, ALL in-progress specs are reset to
`queued` and immediately re-dispatched — producing both stale-status
(brief window) and duplicate-assignment (two agents running the same spec).
