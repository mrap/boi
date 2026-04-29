# BOI Reliability Review — 2026-04-29

Systematic trace of every failure path in the BOI daemon. Each finding includes the scenario, current behavior, impact, and fix recommendation.

---

## 1. Daemon Crash Recovery

### F-01: Daemon crash leaves specs in "running" — HANDLED

**Scenario:** Daemon process receives SIGKILL or OOM-killed mid-spec.

**Current behavior:** `recover_stuck_specs()` runs on every daemon startup (`daemon.rs:183`). It resets `running` and `assigning` specs back to `queued`, and resets their `RUNNING` tasks back to `PENDING`.

**Impact:** None — specs are recoverable. The flock on `daemon.lock` is automatically released by the OS on process death.

**Verdict:** Correctly handled.

### F-02: Daemon crash leaves orphaned worktrees

**Scenario:** Daemon dies while a worker is executing a spec with a workspace. The worker thread dies with it. Worktree at `~/.boi/worktrees/{spec_id}` is never cleaned up.

**Current behavior:** On restart, `recover_stuck_specs` resets the spec to `queued`. When the daemon picks it up again, `worktree::create()` checks `if dest.exists() { return Ok(dest) }` (worktree.rs:23) — so it reuses the existing worktree. The stale branch `boi/{spec_id}` is NOT deleted because the branch-delete logic (`git branch -D`) at worktree.rs:34 only fires during `create()`, which short-circuits before reaching it if the directory already exists.

**Impact:** The worktree is reused, but it contains **partially committed state from the prior failed run**. The worker operates on dirty state without knowing. If the prior run had done `git add -A` but not committed, the re-run inherits those staged changes.

**Fix recommendation:** Add a `cleanup_stale()` call during daemon startup, after `recover_stuck_specs()`. Or: in `worktree::create()`, when the directory already exists, run `git status --porcelain` and `git reset --hard HEAD` to ensure a clean state before returning.

### F-03: Daemon crash leaves stale PID files

**Scenario:** Worker's claude subprocess is running. Daemon crashes. PID file at `~/.boi/pids/{spec_id}.pid` survives.

**Current behavior:** On the next run, the spec is re-queued. A new claude process is spawned. `spawn_claude` writes a new PID to the same file (spawn.rs:95), overwriting the stale one. The old claude process (from the crashed daemon's worker) is now orphaned — no one kills it.

**Impact:** **Orphaned claude processes.** The setsid/process-group mechanism (spawn.rs:73) means the old process and its children are in their own process group, but nobody sends them a signal after the daemon dies. They run until they hit their own timeout or indefinitely.

**Fix recommendation:** On daemon startup, iterate `~/.boi/pids/*.pid`, check if each PID is alive via `kill(pid, 0)`, and if alive, kill the process group (`kill(-pid, SIGKILL)`) before deleting the PID file. Add this before `recover_stuck_specs`.

### F-04: Daemon shutdown does not kill running workers' claude subprocesses

**Scenario:** `boi stop` sends SIGTERM to the daemon. The daemon sets `running = false` and waits up to 30 seconds for worker threads to finish (daemon.rs:294).

**Current behavior:** Worker threads are spawned via `std::thread::spawn` and run `run_worker`. Inside `run_worker`, each phase spawns claude via `spawn_claude` which polls `child.try_wait()` in a 2-second loop (spawn.rs:162). The worker thread has no mechanism to detect that the daemon is shutting down. It continues polling until the claude process finishes or times out (up to `task_timeout_secs`, default 1800s = 30 min).

The daemon's 30-second shutdown wait (daemon.rs:300) will expire. The daemon process exits. Worker threads are killed by the OS. But the claude subprocesses survive because `setsid` put them in their own process group.

**Impact:** **Orphaned claude processes on daemon stop.** Every `boi stop` during active work leaves claude processes running indefinitely (or until their own timeout). This burns API credits.

**Fix recommendation:** Share the `running` AtomicBool with worker threads. In `spawn_claude`, check the flag in the polling loop alongside the timeout check. When it goes false, kill the child and return a timeout/cancelled result. Alternatively, on daemon shutdown, iterate `~/.boi/pids/*.pid` and kill each process group before waiting for threads.

---

## 2. Worker / Claude Process Lifecycle

### F-05: Timeout polling granularity is 2 seconds — ACCEPTABLE

**Scenario:** Claude process should be killed after `timeout_secs`.

**Current behavior:** The poll loop sleeps 2 seconds between `try_wait` calls (spawn.rs:179). Timeout enforcement is accurate to +2 seconds.

**Impact:** Minor — 2 seconds of overrun is acceptable for 30-minute timeouts.

**Verdict:** Acceptable.

### F-06: Process group kill fires SIGKILL on normal exit

**Scenario:** Claude subprocess exits normally (success).

**Current behavior:** After `child.try_wait()` returns `Some(status)` (normal exit), the code immediately fires `kill(-pgid, SIGKILL)` (spawn.rs:168). This kills any grandchild processes in the same group.

**Impact:** None — the grandchildren should already be dead if claude exited cleanly. SIGKILL on non-existent processes returns ESRCH which is ignored. **However**, if the PID has been recycled by the OS into a new process group, `kill(-pgid, SIGKILL)` could kill an unrelated process group. This is unlikely in practice because the window between exit detection and the kill call is microseconds, and PID recycling of the *group leader* PID requires the original process to be fully reaped first.

**Impact:** Theoretical risk only — no practical fix needed.

### F-07: `boi cancel` kills the claude PID, not the process group

**Scenario:** User runs `boi cancel <spec_id>` while claude is running.

**Current behavior:** `cmd_cancel` (cancel.rs:31) reads the PID file, sends SIGTERM to the PID, waits 2s, then sends SIGKILL to the PID. It signals the claude process only, not `-pid` (the process group).

**Impact:** Claude's grandchildren (e.g., subprocesses spawned by tool use) survive the cancel. The `setsid` call means claude is the group leader, so killing `-pid` would clean up all grandchildren. But `boi cancel` only kills `pid`, leaving orphans.

**Fix recommendation:** Change cancel.rs:36 from `libc::kill(pid, libc::SIGTERM)` to `libc::kill(-pid, libc::SIGTERM)` and similarly for SIGKILL. This kills the entire process group.

---

## 3. Database Corruption Prevention

### F-08: `update_task` for DONE/SKIPPED is not atomic — LOW RISK

**Scenario:** `update_task(spec_id, task_id, "DONE")` runs two separate SQL statements (queue.rs:365-377): (1) UPDATE tasks SET status/completed_at, (2) UPDATE specs SET completed_tasks + 1.

**Current behavior:** No transaction wraps these two statements. If the process crashes between them, the task is marked DONE but `completed_tasks` is not incremented.

**Impact:** Low — `completed_tasks` is a display counter used in status output and progress bars. It does not affect execution logic. The actual task status (`DONE`) is authoritative.

**Fix recommendation:** Wrap in a transaction for correctness. Low priority since the counter is cosmetic.

### F-09: `block_task` is a read-modify-write without transaction — RACE CONDITION

**Scenario:** Two concurrent callers add a dependency to the same task simultaneously.

**Current behavior:** `block_task` (queue.rs:697-714) does: (1) SELECT depends, (2) deserialize JSON array, (3) append new dep, (4) UPDATE depends. No transaction or locking. If two callers read the same initial depends, one write clobbers the other.

**Impact:** Lost dependency. A task could run before its dependency is complete. In practice, this only happens if `apply_spec_review_output` is called concurrently on the same spec, which the single-threaded worker architecture prevents. **Risk is currently mitigated by architecture, not code.**

**Fix recommendation:** Wrap in an `unchecked_transaction()` for defense in depth.

### F-10: `add_task` is not atomic — similar to F-08

**Scenario:** `add_task` (queue.rs:645-675) runs three statements: INSERT task, UPDATE total_tasks, INSERT event. No transaction.

**Impact:** If crash between INSERT and UPDATE, `total_tasks` count is stale. Cosmetic only.

### F-11: `set_spec_fields` runs up to 4 independent UPDATEs without transaction

**Scenario:** `set_spec_fields` (queue.rs:471-504) runs up to 4 separate UPDATE statements for mode, max_iterations, project, and worker_timeout_seconds.

**Impact:** If crash between them, some fields are set and others aren't. Low risk since this runs during dispatch (before work starts) and the CLI process is short-lived.

### F-12: No `busy_timeout` PRAGMA set — DB CONTENTION

**Scenario:** The daemon opens a fresh `Queue::open` connection on every poll cycle (daemon.rs:224). Worker threads each open their own connection. Telemetry opens yet another connection on every `emit()` call (telemetry.rs:88). All connections target the same DB file.

**Current behavior:** WAL mode is enabled, which allows concurrent readers + one writer. But no `PRAGMA busy_timeout` is set. If two writers collide (e.g., a worker updating task status while the daemon is dequeuing), the loser gets `SQLITE_BUSY` immediately, which surfaces as a rusqlite Error.

**Impact:** `SQLITE_BUSY` errors can cause: (a) a spec failing to dequeue (daemon retries next cycle — self-healing), (b) a task status update failing — propagated as `Err` from `run_worker_with_phases`, which logs `[boi daemon] worker error` and the spec is left in `running` state (recovered on next daemon restart but NOT on this daemon instance).

**Fix recommendation:** Add `PRAGMA busy_timeout = 5000;` (5 seconds) in `Queue::open()` after WAL mode. This makes rusqlite retry automatically on contention instead of failing immediately. Critical fix.

### F-13: Telemetry opens a new connection per event — PERFORMANCE

**Scenario:** Every telemetry event (dozens per spec) calls `self.open_conn()` (telemetry.rs:88) which does `Connection::open`, `PRAGMA journal_mode=WAL`, and a `CREATE TABLE IF NOT EXISTS`.

**Impact:** Excessive file descriptor churn and WAL checkpoint overhead. Each open/close cycle may trigger a WAL checkpoint. Under high load, this amplifies F-12 contention.

**Fix recommendation:** Hold a persistent connection in the `Telemetry` struct, or use connection pooling. Lower priority than F-12 but contributes to the same problem.

---

## 4. Git Worktree Edge Cases

### F-14: Source repo with uncommitted changes — UNHANDLED

**Scenario:** User dispatches a spec with `workspace: /path/to/repo` while the repo has uncommitted changes (modified tracked files).

**Current behavior:** `worktree::create` runs `git worktree add -b boi/{spec_id}`. Git creates the worktree from HEAD, inheriting the index but NOT the working directory changes. The worktree starts clean. **However**, after the spec completes, `merge_back` (worktree.rs:97) merges the branch into the source repo's current branch using `git merge --no-edit`. If the source repo has uncommitted changes in the same files, `git merge` will refuse with "Your local changes to the following files would be overwritten by merge."

**Impact:** Merge fails. The error is caught (worker.rs:1263/state_machine.rs:1010), logged, the worktree is preserved, but the completed work is stranded on the `boi/{spec_id}` branch. The spec shows as "completed" in the DB even though the merge failed. **The user's work is not lost but requires manual recovery.**

**Fix recommendation:** Before `merge_back`, check if the source repo has uncommitted changes (`git status --porcelain`). If it does, stash them first, merge, then unstash. Or: skip the merge and leave the branch for the user to merge manually, updating the spec status to "completed-unmerged".

### F-15: Branch name collision

**Scenario:** Two specs dispatch to the same workspace repo simultaneously. Both try to create `boi/{spec_id}` branches with different spec IDs, so no collision. BUT: if a user manually creates a branch named `boi/SXXXX` matching a future spec ID, `git worktree add -b boi/SXXXX` will fail because the branch exists.

**Current behavior:** `worktree::create` first tries to delete the stale branch (`git branch -D boi/{spec_id}`, worktree.rs:34). This handles the case.

**Impact:** None — correctly handled by the stale branch deletion.

### F-16: Disk full during commit — HANDLED

**Scenario:** Disk fills up during `commit_changes`.

**Current behavior:** `git commit` fails with a non-zero exit code. `commit_changes` returns `Err(...)`. The worker's Cleanup state catches this (worker.rs:1277), logs it, and breaks without cleaning up the worktree. The spec is already marked "completed" at this point.

**Impact:** Spec shows "completed" but changes are not committed or merged back. Similar to F-14 — work is stranded.

**Fix recommendation:** If commit or merge fails, update the spec status to something like "completed-merge-failed" rather than leaving it as "completed". This is a reporting accuracy issue.

### F-17: `git worktree remove --force` can corrupt git index

**Scenario:** `worktree::cleanup` (worktree.rs:117) runs `git worktree remove --force`. If the worktree has an active git process (e.g., a backgrounded `git status`), force-removing it can leave the main repo's `.git/worktrees/` in an inconsistent state.

**Impact:** Low — BOI's setsid/process-group kill should have already killed all claude grandchildren before cleanup runs.

---

## 5. Process Lifecycle and PIDs

### F-18: PID reuse race in `boi cancel`

**Scenario:** (1) Claude process exits. (2) OS reuses the PID for an unrelated process. (3) User runs `boi cancel <spec_id>`. (4) Cancel reads the stale PID file and sends SIGTERM/SIGKILL to the unrelated process.

**Current behavior:** `spawn_claude` deletes the PID file after the child exits (spawn.rs:189). But there's a race: if `boi cancel` reads the PID file between child exit and PID file deletion, it will try to kill a dead-or-recycled PID.

**Impact:** Sending SIGTERM to an unrelated process. The PID recycling window is very small (microseconds between exit and file deletion), and PID reuse on macOS/Linux requires wrapping the PID counter (65536+ processes). **Extremely low probability** but not zero.

**Fix recommendation:** In `cmd_cancel`, after reading the PID, verify it belongs to a claude process (e.g., check `/proc/{pid}/cmdline` on Linux, or verify the process was started by this user). Or: use the process group instead of a PID file — kill `-pgid` is safer since process groups are not recycled as quickly.

### F-19: Zombie processes are properly prevented — HANDLED

**Scenario:** Claude subprocess exits but parent hasn't called `wait()`.

**Current behavior:** The poll loop in `spawn_claude` calls `child.try_wait()` which reaps the zombie. On timeout, `child.kill()` + `child.wait()` (spawn.rs:173-174) ensures reaping. For hooks, non-blocking hooks spawn a background thread that calls `child.wait()` (hooks.rs:133).

**Verdict:** Correctly handled.

---

## 6. Concurrent Access

### F-20: Two daemon instances — PREVENTED by flock

**Scenario:** Two `boi daemon foreground` processes start simultaneously.

**Current behavior:** `try_acquire_daemon_lock()` uses `flock(LOCK_EX | LOCK_NB)` on `~/.boi/daemon.lock`. This is atomic and OS-enforced. The second process fails immediately and exits.

**Verdict:** Correctly handled. The `_lock_file` is held for the daemon's entire lifetime. Dropping it releases the lock.

### F-21: `is_daemon_locked` has a TOCTOU race

**Scenario:** `is_daemon_locked()` (daemon.rs:48) calls `try_acquire_daemon_lock()`. If it succeeds, the File is dropped immediately, releasing the lock. Another process could acquire the lock between the check and the caller's action.

**Current behavior:** `is_daemon_locked` is used in `cmd_start` (daemon.rs:59) and `cmd_restart` (daemon.rs:117) as a pre-check. In `cmd_start`, if the check says "not locked," it proceeds to spawn a new daemon which will do its own `try_acquire_daemon_lock` — so the race is benign (the real daemon does its own check). In `cmd_stop`, the PID is read from the file, so no race there.

**Impact:** Benign — the TOCTOU is a UX convenience check, not a safety guard. The actual lock acquisition in `cmd_daemon` is the real guard.

### F-22: Multiple workers writing to the same DB — SEE F-12

**Scenario:** Two worker threads run simultaneously (max_workers > 1). Both open independent `Queue::open` connections. Both write task updates.

**Current behavior:** WAL mode allows concurrent writes but they serialize at the page level. Without `busy_timeout`, one may get `SQLITE_BUSY`. See F-12 for full analysis.

---

## 7. Error Propagation — Suppressed Errors

### F-23: `let _ = queue.add_task(...)` suppresses task injection failures

**Location:** worker.rs:596, 848; state_machine.rs:361, 604, 899

**Scenario:** During a Verdict::Redo, new tasks are injected via `queue.add_task(...)`. The result is discarded with `let _ =`.

**Current behavior:** If the INSERT fails (e.g., SQLITE_BUSY from F-12), the task is silently not added. The worker continues as if it was added. Later, when `TaskSelect` looks for this task, it won't exist in `task_map` (since `task_map` is built from the spec's task list, not the DB). The injected task is simply lost.

**Impact:** Silent loss of dynamically injected tasks. The spec may complete "successfully" while missing work that the critic/review phase requested. **This is a correctness bug hidden by `let _ =`.**

**Fix recommendation:** At minimum, log an ERROR if `add_task` fails. Better: propagate the error and transition to `Failed` state.

### F-24: `serde_json::from_str(&t.depends).unwrap_or_default()` silently drops malformed deps

**Location:** worker.rs:367, 435; state_machine.rs:215

**Scenario:** The `depends` column in the DB contains malformed JSON (e.g., from a failed partial write during F-09).

**Current behavior:** `serde_json::from_str(&dt.depends).unwrap_or_default()` returns an empty Vec. The task appears to have no dependencies and runs immediately, potentially before its actual dependencies complete.

**Impact:** Task ordering violation. A task could run before its prerequisites. **This is the most dangerous silent failure** — it could cause incorrect code generation or failed builds because dependencies were skipped.

**Fix recommendation:** Log a warning when JSON parsing fails for depends. Consider treating parse failure as a task failure.

### F-25: Telemetry `emit()` silently swallows connection errors

**Location:** telemetry.rs:89-91

**Scenario:** Telemetry DB file is on a full disk or the SQLite file is corrupted.

**Current behavior:** `open_conn()` failure causes `emit()` to silently return without logging.

**Impact:** All telemetry is silently lost. No audit trail of what the daemon did. For debugging, this is a significant loss.

**Fix recommendation:** Log to stderr on connection failure, at least on the first occurrence.

### F-26: `hooks::fire` stderr/stdout are suppressed

**Location:** hooks.rs:103-104

**Scenario:** A hook script fails with an important error message.

**Current behavior:** Hook stdout and stderr are piped to `/dev/null` (Stdio::null()). Only the exit code is checked (for blocking hooks).

**Impact:** Hook failures are invisible. A notification hook that fails to send a Slack message produces no diagnostic output.

**Fix recommendation:** At minimum, capture stderr for blocking hooks and log it on failure. For non-blocking hooks, consider a small stderr buffer.

---

## 8. State Machine Specific Issues

### F-27: `WorkerState::Paused` leaves worktree allocated indefinitely

**Scenario:** A spec pauses (e.g., plan-critique returns `Verdict::Pause`). The worker thread exits. The spec status is set to "paused" in the DB.

**Current behavior:** There is no `boi decide` or `boi resume` command implemented. The spec stays "paused" forever. The worktree at `~/.boi/worktrees/{spec_id}` is never cleaned up. The branch `boi/{spec_id}` persists.

**Impact:** Worktree and branch leak. Over time, these accumulate. `cleanup_stale()` (worktree.rs:156) only removes directories without a `.git` file — paused worktrees have valid `.git` files and are NOT cleaned up.

**Fix recommendation:** Implement a `boi resume` command that sets the spec back to `queued`. Add a staleness check in `boi doctor` that flags paused specs older than N days and offers cleanup.

### F-28: Deadlock detection has a false positive window

**Scenario:** Tasks t-1 and t-2 are both PENDING with no dependencies. The worker picks t-1, marks it RUNNING, runs it. While t-1 is running, `TaskSelect` doesn't fire. After t-1 completes, `done_ids` is updated and `TaskSelect` runs again.

**Current behavior:** The deadlock detection counter (`task_select_passes`) is reset to 0 when a task is found. But within a single `TaskSelect` pass, if no tasks are found, the counter increments. The max is `order.len()`, so for a 1-task spec, the deadlock threshold is 1. If there's a task that's PENDING but blocked on a dependency that was DONE in a previous spec-redo cycle, the counter may falsely trigger.

**Impact:** False deadlock detection causes a spec to fail prematurely. The `max_task_select_passes = order.len().max(1)` is a somewhat arbitrary heuristic.

**Fix recommendation:** Instead of a pass counter, directly check whether the dependency graph is satisfiable: if there exist pending tasks whose dependencies include a task that is neither DONE nor PENDING (e.g., FAILED), that's a real deadlock. Otherwise, it's just waiting.

### F-29: `state_machine.rs` reads spec YAML at runtime — drift risk

**Scenario:** The state_machine.rs implementation reads the spec YAML from `spec_path` at runtime (`state_machine.rs:114`). The worker.rs implementation reads from the DB.

**Current behavior:** Two independent implementations of `run_worker_with_phases` exist — one in `worker.rs` (DB-driven) and one in `state_machine.rs` (YAML-file-driven). The daemon calls `worker::run_worker` which uses the DB-driven path. `state_machine.rs` appears to be an alternative/older implementation.

**Impact:** If the spec YAML file is modified between dispatch and execution (e.g., user edits it), the state_machine.rs path would pick up the edits but the worker.rs path would not (it reads from DB). This is a consistency risk if someone switches which path is used.

**Fix recommendation:** Remove or clearly deprecate `state_machine.rs` if it's not actively used. Having two implementations of the same logic is a maintenance liability.

---

## 9. Concurrency Architecture Issues

### F-30: Daemon opens fresh DB connection on every 5-second poll cycle

**Scenario:** The daemon's main loop runs every 5 seconds (daemon.rs:283: 10 * 500ms). Each iteration opens `queue::Queue::open(db_str)` (daemon.rs:224).

**Current behavior:** Every 5 seconds: open SQLite connection, parse all PRAGMA, run `CREATE TABLE IF NOT EXISTS` for 7 tables, check for queued specs, close connection.

**Impact:** Unnecessary I/O and contention contributor. Combined with F-13 (telemetry opens per-event), the system may have 10+ connection open/close cycles per second during active work.

**Fix recommendation:** Hold a persistent connection in the daemon's main loop. Re-open only on error.

### F-31: `dequeue` uses `unchecked_transaction` — name is misleading but safe

**Scenario:** `dequeue()` uses `self.conn.unchecked_transaction()` which creates a transaction without checking if one is already active.

**Current behavior:** Since each connection is opened fresh (F-30) and the Queue struct is not shared between threads, there's never an existing transaction. `unchecked_transaction` is actually safe here despite its scary name — it's the rusqlite way to start a transaction on a `&self` borrow (vs `transaction()` which needs `&mut self`).

**Impact:** None — correctly used.

---

## 10. Summary — Prioritized Fix List

### Critical (fix before next release)

| ID | Issue | Impact |
|----|-------|--------|
| **F-12** | No `busy_timeout` PRAGMA | SQLITE_BUSY errors under concurrent workers; spec stuck in "running" until daemon restart |
| **F-04** | Daemon stop doesn't kill claude subprocesses | Orphaned claude processes burn API credits indefinitely |
| **F-03** | No orphan PID cleanup on startup | Orphaned claude processes from prior crashes |

### High (fix this week)

| ID | Issue | Impact |
|----|-------|--------|
| **F-23** | `let _ = queue.add_task(...)` | Silent loss of dynamically injected tasks |
| **F-24** | Malformed deps JSON → `unwrap_or_default()` | Tasks can run before their prerequisites |
| **F-07** | `boi cancel` kills PID not process group | Grandchild processes survive cancel |
| **F-02** | Reused worktree has dirty state | Re-queued spec operates on partially-committed state |
| **F-14** | Merge-back fails with dirty source repo | Completed work stranded on branch; spec shows "completed" |

### Medium (fix this sprint)

| ID | Issue | Impact |
|----|-------|--------|
| **F-27** | Paused specs leak worktrees forever | Disk accumulation over time |
| **F-16** | Commit/merge failure doesn't update spec status | "completed" spec that didn't actually merge |
| **F-25** | Telemetry silently swallows connection errors | Lost audit trail |
| **F-08** | `update_task(DONE)` not atomic | Cosmetic counter drift |
| **F-13** | Telemetry opens connection per event | Performance degradation under load |
| **F-30** | Daemon opens fresh connection per poll | Unnecessary I/O |

### Low (track, fix opportunistically)

| ID | Issue | Impact |
|----|-------|--------|
| **F-09** | `block_task` read-modify-write race | Mitigated by single-worker architecture |
| **F-10** | `add_task` not atomic | Cosmetic counter drift |
| **F-11** | `set_spec_fields` not atomic | Pre-execution only |
| **F-26** | Hook stderr suppressed | Hook debugging difficulty |
| **F-28** | Deadlock detection heuristic | Rare false positive |
| **F-29** | Two implementations of worker state machine | Maintenance risk |
| **F-18** | PID reuse race in cancel | Extremely low probability |

---

## Appendix: `let _ =` Audit

All `let _ =` in non-test production code were reviewed. Those marked `// intentional: ...` are legitimate best-effort operations (file cleanup, hook notifications, zombie reaping). The problematic ones are:

- **`let _ = queue.add_task(...)` (F-23)** — should propagate or log errors
- **`let _ = registry` (worker.rs:255)** — harmless; unused parameter suppression

All other `let _ =` are correctly annotated with `// intentional` comments explaining why the result is discarded.

## Appendix: `unwrap_or_default` Audit

Most `unwrap_or_default()` calls are safe (returning empty vecs for missing optional data). The dangerous one is:

- **`serde_json::from_str(&dt.depends).unwrap_or_default()` (F-24)** — silently drops corrupted dependency data, leading to task ordering violations
