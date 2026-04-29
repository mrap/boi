# BOI Rust Codebase -- Adversarial Code Review

**Date:** 2026-04-29
**Scope:** Full codebase review of 3,445 insertions across 39 files, 20 commits
**Tests passing:** 127

---

## CRITICAL Findings

### C-1. Dead divergent state machine (`state_machine.rs`) -- silent behavioral difference

`src/state_machine.rs` (1,095 lines) is a near-complete copy of the state machine in `src/worker.rs` but is **not wired into `lib.rs`** -- it is dead code. However, the two copies have **diverged in critical logic**:

**Pre-spec phase classification is different:**
- `worker.rs` (line 452): treats `"spec-review"` AND `"plan-critique"` as pre-task spec phases
- `state_machine.rs` (line 232): treats ONLY `"plan-critique"` as a pre-task spec phase

This means `state_machine.rs` puts `spec-review` in the **post-task** list, so it would run AFTER all tasks complete -- defeating the entire purpose of spec review (improving specs before execution). If someone wires `state_machine.rs` back in (it appears to be an older extraction), the spec-review phase silently becomes useless.

**Worker.rs has `run_phase_full` for spec-review output capture; state_machine.rs does not.** The `worker.rs` SpecPhase handler calls `runner.run_phase_full()` and then calls `apply_spec_review_output()` to apply suggested changes. `state_machine.rs` calls only `runner.run_phase()` and never applies spec-review suggestions.

**Template vars diverge.** `worker.rs` populates `SpecContext`, `TaskTitle`, `TaskSpec`, `TaskVerify`, `TaskDepends` template vars. `state_machine.rs` does not populate any task-level or `SpecContext` vars, so phase prompts with `{{SPEC_CONTEXT}}`, `{{TASK_TITLE}}`, etc. would render as empty strings.

**Impact:** If `state_machine.rs` is ever restored to active use, it will silently produce wrong behavior with no compilation error. It should be deleted or explicitly archived.


### C-2. `gen_id` has only 65,536 unique values per prefix -- infinite loop risk

`src/queue.rs` line 110-127: `gen_id` generates a 4-hex-character random ID (2 bytes = 65,536 possible values). It retries in a loop until it finds one that doesn't collide with existing IDs. For a daemon running continuously:

- At ~58,000 specs (88% full), the birthday problem means each ID attempt has an 88% chance of collision, causing many retries per enqueue.
- At 65,536 specs, the loop becomes **infinite** -- every possible ID is taken and `gen_id` spins forever, hanging the daemon.

Tasks share the same 65,536 ID space. A single large spec with thousands of tasks would exhaust the task ID space.

The loop has no retry limit, no timeout, and no log output when retrying. A full ID space causes a silent hang with no error message.

**Impact:** Production daemon freeze after ~65K lifetime specs or tasks. No warning before it happens.

**Recommendation:** Either increase the ID space (3-4 bytes = 16M-4B IDs) or add a retry cap with an error return.


### C-3. `ensure_column` uses string interpolation in SQL -- SQL injection surface

`src/queue.rs` line 248-266: The `ensure_column` method builds SQL via `format!`:

```rust
let sql = format!("ALTER TABLE {} ADD COLUMN {} {}", table, column, col_type);
```

and:

```rust
.prepare(&format!("PRAGMA table_info({})", table))
```

and in `gen_id`:

```rust
&format!("SELECT EXISTS(SELECT 1 FROM {} WHERE id = ?1)", table)
```

All three use string interpolation to inject table/column names into SQL. While these are currently called only with hardcoded string literals from within the crate (not user input), this is a structural vulnerability. If any future caller passes user-controlled data, it becomes SQL injection. The `table` parameter in `gen_id` is derived from a `char` comparison (`'s'` -> `"specs"`, else `"tasks"`) which is safe, but the pattern is fragile.

**Impact:** No current exploit path, but the pattern invites bugs. Standard Rust SQLite practice is to validate identifiers or use an allow-list.


---

## HIGH Findings

### H-1. `unchecked_transaction` bypasses rusqlite safety guarantees

`src/queue.rs` lines 274 and 316: Both `enqueue` and `dequeue` use `self.conn.unchecked_transaction()` instead of `self.conn.transaction()`. The "unchecked" variant skips the borrow checker's guarantee that no other references to the connection exist during the transaction. If `Queue` is ever shared across threads or if any method holds a prepared statement while calling `enqueue`/`dequeue`, this can cause undefined behavior at the SQLite level (concurrent writes within a single connection).

Currently `Queue` is not `Send`/`Sync` (it holds a `Connection` which is `!Send`), so cross-thread sharing is prevented by the type system. But the use of `unchecked_transaction` is a footgun for any future refactoring.

**Impact:** Safe today due to `Connection: !Send`, but violates the library's safety model. If the code is ever refactored to use a connection pool, this becomes a data corruption risk.


### H-2. Worker prompt template still contains spec-modification language

`templates/worker-prompt.md` lines 130-133 include instructions like:

> "If you discover additional work needed, note it in your output"

and the Self-evolution section:

> "If you discover additional work, describe it in your output. The daemon will handle adding new tasks. Do NOT edit the spec YAML file."

The `task-verify-prompt.md` (lines 96-108, 111-127) contains extensive instructions for the critic to **append tasks to the spec file** and **modify the spec file's Tasks section**:

> "For each issue in the `issues` array, append the `suggested_task` as a new task at the end of the spec's Tasks section."

This contradicts the worker prompt's "Do NOT modify the spec YAML" and the daemon's DB-as-source-of-truth architecture. The critic prompt is telling Claude to edit the spec file, but the daemon ignores spec file changes and reads tasks from the DB. These critic-injected tasks would be silently lost.

**Impact:** Critic-generated remediation tasks are written to a file nobody reads, wasting Claude compute on work that has no effect. The spec-review phase (which outputs JSON for the daemon to apply) is the correct pattern; the critic prompt should follow the same pattern.


### H-3. Deadlock detection in `TaskSelect` is a busy-wait hot loop

`src/worker.rs` lines 704-720: When no tasks are ready but some are pending (potential deadlock), the state machine sets `state = WorkerState::TaskSelect` and continues the loop. This means it will immediately re-evaluate the same set of tasks. Since nothing changes between iterations (no new tasks complete, no external state changes), this is a **CPU-burning busy loop** that runs `max_task_select_passes` times with zero delay.

The `max_task_select_passes` is set to `order.len().max(1)`, so for a 50-task spec, this burns 50 CPU-bound iterations checking the same blocked tasks before declaring deadlock.

**Impact:** CPU waste proportional to task count during deadlock scenarios. Not a correctness bug, but unnecessary resource consumption.


### H-4. `spawn_claude` sends SIGKILL to process group on EVERY normal exit

`src/spawn.rs` line 168: After every normal child exit (not just timeout), the code unconditionally sends `SIGKILL` to the entire process group:

```rust
Some(status) => {
    exit_status = Some(status);
    unsafe { libc::kill(-pgid, libc::SIGKILL); }
    break;
}
```

This kills all grandchildren immediately on normal exit rather than letting them clean up gracefully. While this prevents zombie grandchildren, it also prevents any graceful shutdown logic in child processes. A `SIGTERM` followed by a brief wait, then `SIGKILL`, would be the standard pattern.

**Impact:** Any Claude subprocess that spawns children (e.g., background processes, watchers) will have those children SIGKILL'd immediately with no chance to flush buffers or clean up temp files.


### H-5. `Telemetry::emit` opens a new SQLite connection on every call

`src/telemetry.rs` line 88: Every `emit()` call opens a fresh SQLite connection via `self.open_conn()`, which also runs `CREATE TABLE IF NOT EXISTS` and `PRAGMA journal_mode=WAL`. During a single spec execution, `emit` is called dozens of times (phase start, phase end, task start, task end, etc.).

Each connection open + WAL pragma + CREATE TABLE is ~2-5ms of overhead. For a spec with 10 tasks and 3 phases each, that is ~60+ connection open/close cycles -- roughly 200ms of pure overhead.

**Impact:** Performance drag on every spec execution. Should cache the connection or use a persistent connection.


---

## MEDIUM Findings

### M-1. Workspace path replacement uses naive string `replace`

`src/worker.rs` line 344 (and `state_machine.rs` line 145): The workspace path substitution does:

```rust
*s = s.replace(ws.as_str(), &worktree_path);
```

This is a global string replace, not a path-aware replace. If the workspace path appears as a substring of another path (e.g., workspace is `/home/user` and a verify command references `/home/user-admin/file`), the replacement will corrupt the path to `{worktree}/admin/file`.

**Impact:** Rare in practice (workspace paths are typically specific enough), but could cause subtle verify command failures in edge cases.


### M-2. `recover_stuck_specs` has no protection against re-recovery loops

`src/queue.rs` lines 751-765 and `src/cli/daemon.rs` line 183: On daemon startup, all specs in `running` or `assigning` state are reset to `queued`. If a spec consistently crashes the worker (e.g., due to a bad spec file), the daemon will restart it on every daemon restart, creating an infinite crash-restart loop.

There is no attempt counter or circuit breaker. A poisonous spec will burn Claude API credits indefinitely across daemon restarts.

**Impact:** Unbounded cost exposure from a single bad spec.


### M-3. `dequeue` atomicity is incomplete -- no row-level lock

`src/queue.rs` lines 315-360: The `dequeue` method uses a transaction to SELECT the next queued spec and UPDATE its status to `assigning`. However, SQLite transactions do not provide row-level locking. If two threads call `dequeue` simultaneously on the same connection (not currently possible since `Connection: !Send`, but architecturally relevant), they could both select the same spec ID.

The daemon currently runs `dequeue` on a single thread in the polling loop, so this is safe in practice. But the method's doc comment says "Atomically sets the spec status to 'assigning' to prevent double-dispatch" which overpromises for the SQLite concurrency model.

**Impact:** Safe in current single-threaded daemon loop. Would be a double-dispatch bug if the daemon were ever made multi-threaded.


### M-4. Template variable stripping removes ALL `{{...}}` patterns

`src/phases.rs` lines 764-770: After substituting known template variables, the code strips ALL remaining `{{VAR}}` patterns:

```rust
while let Some(start) = prompt.find("{{") {
    if let Some(end) = prompt[start..].find("}}") {
        prompt.replace_range(start..start + end + 2, "");
    } else {
        break;
    }
}
```

This means any `{{` in the spec content, code blocks, or JSON examples will be silently removed. For example, a Rust spec containing `HashMap<{{K, V}}>` in a code example would have `{{K, V}}` stripped, producing `HashMap<>`.

**Impact:** Corrupted prompts when specs contain code with double-brace syntax (Handlebars templates, Jinja2, Rust turbofish-like patterns). Could cause confused Claude output.


### M-5. Tasks dynamically added by `add_task` are invisible to the topological order

`src/worker.rs` lines 596-605 and similar: When a Redo verdict injects new tasks via `queue.add_task()`, the tasks are added to the DB but the `order` vector (computed once from `topological_sort` at the start) is never updated. The `task_map` is also never updated.

This means dynamically added tasks will never be selected by `TaskSelect` because they are not in the `order` list. They will not cause a deadlock either (since they are not counted as pending). They are effectively ghost tasks -- present in the DB but never executed.

**Impact:** Any phase that returns `Verdict::Redo { tasks: [...] }` with new tasks is silently broken. The tasks are added to the DB but never run.

**This is the most impactful correctness bug after the gen_id issue.**


### M-6. `PhaseConfig.retry_count` is never set from TOML files

`src/phases.rs` line 241: `retry_count` is always set to `None` in `PhaseConfig::from_toml`:

```rust
retry_count: None,
```

There is no TOML field that maps to `retry_count`. This means per-phase retry configuration is impossible -- all phases fall back to the global `config.retry_count`. The `PhaseConfig.retry_count` field exists but can never be populated from configuration.

**Impact:** Configuration dead end. Users cannot configure per-phase retry counts despite the field existing.


### M-7. `outcome_count` parses YAML with ad-hoc line scanning

`src/queue.rs` lines 799-838: The `outcome_count` method parses YAML by scanning for `"outcomes:"` and counting `"- description:"` lines. This is a fragile ad-hoc YAML parser that will break on:
- Quoted strings containing `outcomes:` or `- description:`
- Multi-line descriptions using YAML block scalars
- Different indentation styles
- Comments

Since this method is used for informational display only (not correctness), the impact is limited, but it sets a bad precedent when `serde_yml` is already a dependency.

**Impact:** Incorrect outcome counts displayed in status output for some spec formats.


---

## LOW Findings

### L-1. `2s` polling interval in `spawn_claude` timeout check

`src/spawn.rs` line 180: The timeout check loop sleeps for 2 seconds between checks:

```rust
std::thread::sleep(Duration::from_secs(2));
```

This means a process that finishes could wait up to 2 seconds before being reaped, and the timeout precision is +/- 2 seconds. For long-running Claude calls (10-30 minutes), this is negligible. For short timeout tests, it adds unnecessary delay.


### L-2. `derive_level` logic is duplicated with `derive_can_add_tasks` and `derive_can_fail_spec`

`src/phases.rs` lines 261-282: Three separate functions derive phase properties from the phase name using independent match statements. If a new phase is added, all three must be updated. A single struct or table mapping phase names to their properties would be more maintainable.


### L-3. `registry_load_user` is a no-op

`src/worker.rs` lines 252-256: This function does nothing:

```rust
fn registry_load_user(registry: &PhaseRegistry) {
    let _ = registry; // intentional: consume parameter to suppress unused warning
}
```

The comment says "PhaseRegistry::new() already loads core phases." This is dead code that should be removed.


### L-4. Worktree tests use `set_var` without ENV_LOCK in all cases

`src/worktree.rs` tests use `std::env::set_var("HOME", ...)` which is marked `unsafe` in recent Rust editions due to thread safety. While the tests hold `TEST_LOCK`, other test threads in the same process may be reading `HOME`. The `worker.rs` tests use `unsafe { std::env::set_var(...) }` blocks with proper SAFETY comments, but the worktree tests do not.

**Impact:** Potential test flakiness. Not a production issue.


### L-5. `hook_config` stderr is swallowed for non-blocking hooks

`src/hooks.rs` line 103-104: Non-blocking hooks have their stdout and stderr piped to `/dev/null`:

```rust
.stdout(Stdio::null())
.stderr(Stdio::null())
```

If a hook fails, the error is silently discarded. The background thread only reaps the zombie but never checks the exit status or stderr. Combined with standing order S12 ("No quiet failures"), this is a rule violation.


---

## Architectural Observations

### A-1. Three copies of the state machine

The state machine logic exists in three places:
1. `src/worker.rs` -- the active, canonical version (1,965 lines including tests)
2. `src/state_machine.rs` -- dead code, diverged copy (1,095 lines)
3. The `worker.rs` and `state_machine.rs` share the same `WorkerState` enum definition, duplicated identically

This triples the maintenance surface and creates a drift risk (already realized, see C-1). `state_machine.rs` should be deleted.

### A-2. Worker.rs reads from DB; state_machine.rs reads from YAML file

`worker.rs` line 340 reads task data from the DB (`queue.get_tasks_full`), while `state_machine.rs` line 114 reads from the YAML spec file (`std::fs::read_to_string(spec_path)`). This is a fundamental architectural divergence -- `worker.rs` treats the DB as source of truth (correct), while `state_machine.rs` treats the YAML file as source of truth (incorrect for the current architecture).

### A-3. Template prompt structure is solid

The `TemplateVar` enum with `validate()` is a good pattern that prevents typos in template variable names. The phase TOML configuration with priority resolution (user > core > fallback) is well-designed.

### A-4. Hook system is clean and well-bounded

The hook system's design -- YAML config, timeout enforcement, zombie prevention, non-blocking background reap -- is production-grade. The `wait_with_timeout` implementation using mpsc channels is correct.

### A-5. Test coverage is good but gaps exist

127 tests cover queue operations, spec parsing, worker lifecycle, phase pipeline, worktree operations, and hook firing. Notable gaps: no tests for `gen_id` exhaustion, no tests for dynamically added tasks being executed, no tests for the deadlock detection path completing correctly.

---

## Summary

| Severity | Count | Key Themes |
|----------|-------|------------|
| CRITICAL | 3 | Dead divergent code, ID space exhaustion, SQL interpolation |
| HIGH | 5 | Spec modification in prompts, busy-wait, SIGKILL on normal exit, telemetry perf |
| MEDIUM | 7 | Ghost tasks from Redo, path replace bugs, crash loop, template corruption |
| LOW | 5 | Test safety, dead code, silent hook failures |

The most impactful finding is **M-5 (dynamically added tasks never execute)** -- this silently breaks the self-evolution loop that is core to BOI's value proposition. Combined with **C-2 (gen_id exhaustion)** which is a ticking time bomb, these two issues should be addressed before any other work.
