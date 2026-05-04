# BOI Daemon Consistency — Diagnostic Report
**Date:** 2026-05-04  
**DB surveyed:** `~/.boi/boi-rust.db` at 03:32:52 UTC  
**Prior survey:** `docs/diagnostics/T4614-db-survey-findings.md`

---

## Symptoms Observed Today

Three target symptoms were queried against the live DB. Two have current counts.

### 1. Ghost Worker

**Definition:** A spec stuck in `running` for >1 hour with no recent iteration
activity, indicating the worker exited without finalising the spec.

**Query:**
```sql
SELECT id, title, status, started_at,
  (SELECT MAX(started_at) FROM iterations WHERE spec_id = specs.id) AS last_iter
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
```

**Count today:** 0 (all running specs started < 30 minutes ago)

Note: The `workers` table exists in the schema but contains 0 rows — no code
path ever inserts into it. Ghost workers therefore cannot be detected via the
workers table; the iterations-based query above is the correct proxy.

---

### 2. Duplicate Assignment / Running with No worker_id

**Definition — variant A:** Same `spec_id` assigned to two workers table rows.  
**Count today:** 0 (workers table always empty — dead schema)

**Definition — variant B:** A spec in `status='running'` with `worker_id` unset.  
**Query:**
```sql
SELECT COUNT(*) AS running_no_worker_id
FROM specs
WHERE status = 'running'
  AND (worker_id IS NULL OR worker_id = '');
```

**Count today:** 14 (all 14 currently active specs)

This count is **structural, not transient**: `update_spec("running")` in
`queue.rs:503` never writes `worker_id`. Every spec that has ever been
dispatched has `worker_id = NULL` while running. The column is only touched by
`recover_stuck_specs()` at line 912, which NULLs it on reset.

---

### 3. Stale Status

**Definition:** A spec in `running` or `assigning` for >1 hour with no recent
iteration activity.

**Query:**
```sql
SELECT id, title, status, started_at,
  (SELECT MAX(started_at) FROM iterations WHERE spec_id = specs.id) AS last_iter
FROM specs
WHERE status IN ('running', 'assigning')
  AND started_at < datetime('now', '-1 hour')
  AND (
    NOT EXISTS (SELECT 1 FROM iterations WHERE spec_id = specs.id)
    OR (SELECT MAX(started_at) FROM iterations WHERE spec_id = specs.id)
         < datetime('now', '-1 hour')
  )
ORDER BY started_at ASC;
```

**Count today:** 0 (no specs older than 1 hour in running/assigning)

Stale specs appear after daemon crashes, not during normal operation.
`recover_stuck_specs()` resets them on the next daemon startup, but there is
no in-loop periodic recovery.

---

## Hypotheses

### H1 — running specs never get worker_id (structural)

**Source:** `queue.rs:500–523` — `update_spec()`

The `"running"` branch only writes `status` and `started_at`:
```rust
// queue.rs:503
"running" => {
    self.conn.execute(
        "UPDATE specs SET status = ?1, started_at = ?2 WHERE id = ?3",
        params![status, now, spec_id],
    )?;
}
```

There is no `worker_id = ?` in any UPDATE path for the running transition.
The column exists on `SpecRecord` (`queue.rs:59`) and in the SELECT at
`queue.rs:456`, but the write path simply omits it.

**Triggered at:** `worker.rs:352`
```rust
queue.update_spec(spec_id, "running")?;
```

This is the first call after `run_worker_with_phases` starts. It has no worker
identity to pass even if `update_spec` accepted one, because the daemon never
generates or passes a worker ID to the thread (`daemon.rs:301–315`).

---

### H2 — stale status after crash (no periodic recovery)

**Source:** `cli/daemon.rs:214–220` — `recover_stuck_specs()` call site

```rust
// daemon.rs:214
if let Ok(q) = queue::Queue::open(db_str) {
    match q.recover_stuck_specs() { ... }
}
```

`recover_stuck_specs()` runs **once** at daemon startup. The daemon loop at
`daemon.rs:251–333` polls every 5 seconds for new work but never re-runs
recovery. Consequence: if a worker thread panics after marking its spec
`running`, that spec stays `running` until the daemon is restarted.

**Panic paths in run_worker_with_phases that skip cleanup:**
- Early `?` propagation before `WorkerState::Failed` is reached: if
  `TemplateVar::validate()` at `worker.rs:631` returns `Err`, the function
  returns immediately without marking the spec failed. The spec stays
  `running`.
- `queue.update_spec(spec_id, "running")?` itself at line 352: if this errors,
  the function exits before any state machine entry. The spec is left in
  `assigning` forever. `recover_stuck_specs()` will reset it on the next
  daemon restart, but not before.

---

### H3 — duplicate assignment on daemon restart (core race)

**Source:** `cli/daemon.rs:214–220` (recovery) + `queue.rs:421–467` (dequeue)

When the daemon crashes while `N` specs are running:
1. Any Claude Code subprocesses spawned with `setsid` (see `cleanup_pid_files`
   in `daemon.rs:212`) survive as orphans and continue writing to the DB.
2. On daemon restart, `recover_stuck_specs()` at `queue.rs:911–916` resets ALL
   specs in `running/assigning` back to `queued` unconditionally — including
   those whose orphan subprocess is still active.
3. `dequeue()` at `queue.rs:421` picks them up again and starts new workers.
4. Result: two agents writing to the same spec simultaneously.

The recovery function has no liveness check — it cannot, because PIDs are not
recorded. This is a direct consequence of H1 (worker_id never set) and the
empty `processes` table (also never written).

---

## Reproduction Steps

### Reproduce H1 — running with no worker_id (deterministic, unit-testable)

1. Open an in-memory (or temp-file) Queue.
2. Enqueue a spec via `queue.enqueue(...)`.
3. Call `queue.dequeue()` → spec moves to `assigning`.
4. Call `queue.update_spec(&spec_id, "running")`.
5. Call `queue.status(&spec_id)` and inspect `spec.worker_id`.
6. **Observe:** `worker_id` is `None`. No worker identity is recorded.

This reproduces 100% of the time with zero process spawning.

---

### Reproduce H2 — stale status after thread death (integration-testable)

1. Enqueue a spec.
2. Spawn a thread that calls `queue.update_spec(&spec_id, "running")` then
   panics immediately (simulating a mid-run crash).
3. Wait for the thread to finish.
4. Advance the DB clock: directly UPDATE `specs SET started_at =
   datetime('now', '-2 hours') WHERE id = ?`.
5. Call `recover_stuck_specs()` — it returns 0 because it already ran at
   "startup" (not called again).
6. **Observe:** spec remains `running` indefinitely despite the thread being dead.

Alternatively, use `kill -9 $$` against a real daemon PID and observe the spec
status after restart via `boi status`.

---

### Reproduce H3 — duplicate dispatch on restart (manual, requires real daemon)

1. Start the daemon: `boi daemon start`.
2. Dispatch two or more specs: `boi dispatch ...`.
3. Wait until they reach `running` status.
4. Kill the daemon: `kill -9 $(cat ~/.boi/daemon.lock)`.
5. Restart the daemon: `boi daemon start`.
6. Observe in `boi status`: the specs briefly show `queued` and then `running`
   again — two Claude Code processes are now running the same spec.
7. The orphan processes from step 3 are still alive: `ps aux | grep claude`.

---

## Resolution

### ✓ H1 — FIXED (2026-05-04)

`update_spec_running(spec_id, worker_id)` was added to `queue.rs` and called
from `worker.rs` instead of `update_spec(spec_id, "running")`. The
`worker_id` is now set as `"W-{spec_id}-{thread_id:?}"` at the top of
`run_worker_with_phases`. The `on_worker_start` hook payload also now includes
`worker_id`. H3 can now be addressed in a follow-up since `worker_id` is
populated.

---

## Recommended Fix (historical — implemented)

**Fix H1 — populate worker_id on spec start.**

This is the most contained fix with the clearest failing test.

**Why H1 over H2/H3:**
- H1 is fully unit-testable with no process spawning.
- The invariant is simple: a `running` spec must have a `worker_id`.
- Fixing H1 unlocks H3 detection: once `worker_id` is set, a restart can check
  whether the recorded worker is still live before resetting.

**Proposed change:**

Add `update_spec_running(spec_id, worker_id)` in `queue.rs` (or extend the
`"running"` branch to accept an optional worker ID):

```rust
// queue.rs — new method
pub fn update_spec_running(&self, spec_id: &str, worker_id: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    self.conn.execute(
        "UPDATE specs SET status = 'running', started_at = ?1, worker_id = ?2 WHERE id = ?3",
        params![now, worker_id, spec_id],
    )?;
    Ok(())
}
```

**Call site change in `worker.rs:352`:**

```rust
// worker.rs — before state machine entry
// Generate a stable worker ID (e.g. from spec_id + thread ID):
let worker_id = format!("W-{}-{:?}", spec_id, std::thread::current().id());
queue.update_spec_running(spec_id, &worker_id)?;
```

**Daemon change in `daemon.rs`:** optionally pass a worker ID into
`run_worker()` so the ID is assigned at dispatch time (before the thread
starts) rather than derived inside the thread.

**Test module:** `daemon_consistency` in `queue.rs` (appended after the
existing `#[cfg(test)]` block, starting at line 1873). Uses
`Queue::open(":memory:")` directly — no `setup_test_db` helper needed.

---

## Completed: `boi doctor` integration (T3A87)

All three symptom queries are now wired into `boi doctor` (`src/cli/doctor.rs`).
On every invocation it calls:

- `Queue::ghost_worker_count()` — ghost worker query (H1/H2 signal)
- `Queue::stale_status_count()` — stale running/assigning query
- `Queue::running_without_worker_id_count()` — informational; non-zero exit only with ghost or stale hits

Non-zero ghost or stale counts cause `boi doctor` to exit non-zero, making it
usable in CI or hex-events alerting policies.

---

## Open Issues (for follow-up specs)

### Open: H2 — periodic stale-status recovery

`recover_stuck_specs()` runs once at startup. The daemon loop
(`daemon.rs:251–333`) should periodically call it — or check per-thread
liveness against `started_at` — so stale specs are recovered without requiring
a daemon restart. Suggested interval: every 5 minutes.

### Open: H3 — crash-safe re-dispatch (H1 prerequisite now met)

`recover_stuck_specs()` must become liveness-aware: before resetting a
`running` spec, check whether the recorded `worker_id` corresponds to a live
process. H1 is now fixed (`worker_id` is populated), so this is unblocked.
Still requires the `processes` table to be written to (or a PID recorded
alongside `worker_id`).

---

*Report generated by BOI worker S7372, iteration 1.*
