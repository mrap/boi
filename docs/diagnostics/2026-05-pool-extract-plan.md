# WorkerPool Trait Extraction Plan — Phase 1

## Source of Truth
Design doc: `docs/extensibility/worker-pool-providers.md`

---

## 1. Functions to Lift into the Trait

Five operations, per the "What BOI Needs from a Worker Pool" table:

| Op | Signature | Notes |
|----|-----------|-------|
| `spawn` | `fn spawn(&self, spec_id: &str, spec_path: &str, workspace_path: &str, config: &WorkerConfig) -> Result<JobId>` | Starts exactly one worker; returns opaque job handle |
| `status` | `fn status(&self, job_id: &JobId) -> Result<JobStatus>` | Returns Running/Completed/Failed/Timeout/Unknown |
| `collect` | `fn collect(&self, job_id: &JobId) -> Result<JobOutput>` | Returns exit_code + stdout + stderr; valid only after terminal state |
| `cancel` | `fn cancel(&self, job_id: &JobId) -> Result<()>` | Idempotent; no error if already finished/cancelled |
| `cleanup` | `fn cleanup(&self, job_id: &JobId) -> Result<()>` | Optional; default no-op; frees provider-side resources |

Helper accessor required by the daemon loop:

```
fn max_workers(&self) -> u32;
```

---

## 2. Current Call Sites to Migrate

All live in `src/cli/daemon.rs` inside `cmd_daemon()`.

### a. Job tracking — line 248
```rust
let active: Arc<Mutex<Vec<JoinHandle<()>>>> = ...;
```
**Replaces with:** `LocalThreadPool` internal state (a `Mutex<HashMap<JobId, JoinHandle<()>>>`).
Daemon holds `Box<dyn WorkerPool>` instead.

### b. Status / reap — line 263
```rust
workers.retain(|h| !h.is_finished());
```
**Replaces with:** iterate active job_ids, call `pool.status(job_id)`:
- terminal → call `pool.collect()`, write results to SQLite, call `pool.cleanup()`
- still running → keep

### c. Capacity check — line 265
```rust
if workers.len() < wc.max_workers as usize { ... }
```
**Replaces with:** `active_jobs.len() < pool.max_workers() as usize`

### d. Spawn — lines 301–316
```rust
let handle = std::thread::spawn(move || {
    worker::run_worker(&spec_id, &spec_path, &qpath, &hc, &wc, &tel)
});
workers.push(handle);
```
**Replaces with:**
```rust
let job_id = pool.spawn(&spec_id, &spec_path, &workspace, &wc)?;
active_jobs.insert(job_id, spec_id);
```

### e. Shutdown drain — lines 344–349
```rust
workers.retain(|h| !h.is_finished());
```
**Replaces with:** poll `pool.status()` in the drain loop until all active jobs reach terminal state or timeout expires.

### f. Per-job timeout enforcement
Currently implicit (worker calls `spawn_claude` with `task_timeout_secs`).
After extraction: `LocalThreadPool` preserves this internally. No daemon-level change needed.

---

## 3. Minimum Invariants the LocalThreadPool Impl Must Preserve

From design doc "Invariants every provider must satisfy":

1. **One spawn = one job.** Each `spawn()` call starts exactly one thread calling `run_worker()`. Multiple calls produce independent jobs; no sharing of JoinHandles.

2. **Idempotent cancel.** If `cancel()` is called on a job that has already finished or been cancelled, return `Ok(())`. Use `SIGTERM` to the tracked Claude child PID (via `processes` table / pid_dir), not `JoinHandle::join()` (which panics on double-join).

3. **Status convergence — no zombie jobs.** A job that is `Running` must eventually become `Completed`, `Failed`, or `Timeout`. The `LocalThreadPool` detects this via `JoinHandle::is_finished()`; `Timeout` is set when elapsed time exceeds `task_timeout_secs`. Do not hold `JoinHandle` entries for jobs whose threads have already joined.

4. **Collect after terminal.** `collect()` must return results for any job that has reached a terminal state. For `LocalThreadPool`, results flow through SQLite (same as today) — `collect()` reads from the DB. The JoinHandle must not be dropped before collect is called; the pool retains it until `cleanup()`.

5. **Isolation.** Thread-level isolation is sufficient for `LocalThreadPool` — threads do not share mutable state. Each worker gets its own `workspace_path` and writes to its own spec row in SQLite. The `Mutex<HashMap<JobId, JoinHandle<()>>>` prevents data races on the handle map.

---

## 4. New Types Required

```rust
pub struct JobId(String);   // newtype; Debug + Clone + Hash + Eq

pub enum JobStatus {
    Running,
    Completed,
    Failed,
    Timeout,
    Unknown,
}

pub struct JobOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}
```

---

## 5. Files Changed

| File | Change |
|------|--------|
| `src/pool/mod.rs` | **new** — trait + types; re-exports from `src/pool/local.rs` |
| `src/pool/local.rs` | **new** — `LocalThreadPool` impl |
| `src/cli/daemon.rs` | Replace `Vec<JoinHandle>` with `Box<dyn WorkerPool>` in `cmd_daemon()` |
| `src/lib.rs` | Add `pub mod pool;` |
| `src/worker.rs` | No changes needed (run_worker stays as the thread body) |
