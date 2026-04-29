# BOI Rust Test Coverage Audit — 2026-04-29

**Status:** 127 tests pass across 11 modules.

---

## Coverage Table

### queue.rs (14 tests)

| Function | Tested? | Gap Description |
|----------|---------|-----------------|
| `gen_id()` | Partial | Uniqueness tested (`test_unique_ids`), but **collision retry loop is NOT tested** — the `loop` that re-generates on EXISTS hit is never exercised because in-memory DB is nearly empty |
| `Queue::open()` | Yes | Schema creation verified via all tests using `open_mem()` |
| `Queue::ensure_column()` | No | **Column migration path untested** — no test opens a DB with a missing column and verifies ALTER TABLE fires |
| `Queue::enqueue()` | Yes | 4 tests cover ID format, tasks, spec_path, dep mapping |
| `Queue::dequeue()` | Yes | 4 tests: empty, returns queued, skips running, priority order |
| `Queue::update_task()` | Yes | DONE path tested; RUNNING and generic paths untested |
| `Queue::update_spec()` | Yes | `completed` status tested; `running`, `failed`, generic paths tested indirectly |
| `Queue::status()` | Yes | Both found and not-found paths tested |
| `Queue::status_all()` | Yes | Ordering tested |
| `Queue::cancel()` | Yes | |
| `Queue::set_spec_fields()` | No | **None of the 4 optional fields (mode, max_iterations, project, worker_timeout_seconds) are directly tested** |
| `Queue::set_priority()` | No | Used indirectly via raw SQL in `test_dequeue_priority_order`, but `set_priority()` itself never called |
| `Queue::set_depends_on()` | No | **Untested** |
| `Queue::get_iterations()` | No | **No test inserts iteration records and queries them** |
| `Queue::insert_event()` | No | **No test verifies event insertion or querying** |
| `Queue::get_workers()` | No | **No test inserts worker records and queries them** |
| `Queue::insert_phase_run()` | Indirect | Used by worker tests but no direct unit test |
| `Queue::phase_cost_summary()` | No | **Untested** |
| `Queue::phase_cost_total()` | No | **Untested** |
| `Queue::add_task()` | Indirect | Used by apply_spec_review tests |
| `Queue::skip_task()` | No | **Untested** |
| `Queue::update_task_spec_content()` | Indirect | Via apply_spec_review |
| `Queue::update_task_verify_content()` | Indirect | Via apply_spec_review |
| `Queue::block_task()` | Indirect | Via apply_spec_review |
| `Queue::get_tasks()` | Indirect | Via worker tests |
| `Queue::get_tasks_full()` | Indirect | Via worker tests |
| `Queue::recover_stuck_specs()` | No | **CRITICAL: crash recovery is completely untested** |
| `Queue::prune_events()` | No | **Untested** |
| `Queue::prune_phase_runs()` | No | **Untested** |
| `Queue::lifetime_counts()` | No | **Untested** |
| `Queue::outcome_count()` | No | **Untested** — reads spec file from disk, complex parsing logic |
| `Queue::last_spec_update()` | No | **Untested** |

### spec.rs (14 tests)

| Function | Tested? | Gap Description |
|----------|---------|-----------------|
| `parse()` | Yes | Minimal, full, phase overrides |
| `parse_unchecked()` | Indirect | Used in circular dependency test |
| `validate()` | Yes | Missing title, no tasks, duplicate IDs, unknown dep |
| `topological_sort()` | Yes | Linear chain and circular dependency |
| `parallel_groups()` | Yes | Diamond dependency |
| `ready_tasks()` | Yes | Single ready, blocked tasks |
| `TaskStatus::Display` | No | **fmt implementation untested** |

### config.rs (5 tests)

| Function | Tested? | Gap Description |
|----------|---------|-----------------|
| `Config::load_from()` | Yes | Missing file and valid YAML |
| `Config::max_workers()` | Yes | Default tested |
| `Config::task_timeout_secs()` | Yes | Default and custom tested |
| `Config::retry_count()` | Yes | Default and custom tested |
| `Config::cleanup_on_failure()` | No | **Default value untested** |
| `Config::claude_bin()` | No | **Env var fallback logic untested** — CLAUDE_BIN env → config field → default "claude" |
| `Config::db_path()` | Yes | Default and custom paths |
| `Config::worktrees_dir()` | Yes | Default path |
| `Config::logs_dir()` | Yes | Default path |
| `load()` | No | **Global load() function untested** (calls `default_config_path` then `load_from`) |
| `default_config_path()` | No | **Untested** |

### hooks.rs (9 tests)

| Function | Tested? | Gap Description |
|----------|---------|-----------------|
| `load_user_or_default()` | Partial | Tests parsing but doesn't actually test HOME override path |
| `fire()` — no hooks | Yes | |
| `fire()` — event not configured | Yes | |
| `fire()` — non-blocking | Yes | |
| `fire()` — blocking | Yes | |
| `fire()` — stdin delivery | Yes | |
| `fire()` — bad command | Yes | |
| `fire()` — **timeout** | No | **CRITICAL: blocking hook timeout + child kill is untested** |
| `fire()` — **zombie prevention** | No | **Background reap thread for non-blocking hooks is untested** |
| `wait_with_timeout()` | No | **Timeout path with kill + reap never exercised** |

### worker.rs (24 tests)

| Function | Tested? | Gap Description |
|----------|---------|-----------------|
| `run_verify()` | Yes | success, failure, missing command |
| `spawn_claude()` | Yes | exit 0, exit 1, stderr capture |
| `run_worker()` | Yes | completes, fails, skips done tasks |
| `run_worker_with_phases()` | Yes | all-approved, task-fail, override, timeout, challenge mode, multi-task |
| `apply_spec_review_output()` | Yes | 7 tests cover rewrite_spec, rewrite_verify, add_dep, split, wrapped JSON, malformed, code fence |
| `extract_json_from_output()` | Indirect | Via apply_spec_review tests |
| `record_phase_run()` | Indirect | Via worker tests |
| `emit_phase_verdict()` | Indirect | Via worker tests |
| `WorkerState::Paused` | No | **Pause state transition untested** — no mock test returns `Verdict::Pause` |
| `WorkerState::TaskRequeue` | No | **Requeue limit enforcement untested** — no test exercises repeated Redo verdicts until requeue cap hit |
| `WorkerState::TaskPhaseRetry` | No | **Retry loop untested** — no test returns multiple failures to exercise retry → max → fail path |
| Cleanup with workspace | No | **Worktree commit/merge/cleanup in success path untested via mock runner** |
| Cleanup on failure with cleanup_on_failure=true | No | **Untested** |
| Worktree disappearance mid-run | No | **The worktree existence check at each state transition untested** |
| Deadlock detection | No | **TaskSelect deadlock (pending but none ready) untested** |

### state_machine.rs (0 tests)

| Function | Tested? | Gap Description |
|----------|---------|-----------------|
| `run_worker_with_phases()` | Indirect | Tests exist in worker.rs but this is a **separate copy** of the state machine — state_machine.rs appears to be the YAML-based worker (reads spec file), while worker.rs has the DB-based worker. No dedicated tests for this module. |
| All WorkerState transitions | No | Same gaps as worker.rs: Paused, TaskRequeue limits, TaskPhaseRetry exhaustion, deadlock, worktree disappearance |

### runner.rs (5 tests)

| Function | Tested? | Gap Description |
|----------|---------|-----------------|
| `ClaudePhaseRunner::run_phase()` — verify success | Yes | |
| `ClaudePhaseRunner::run_phase()` — verify failure | Yes | |
| `ClaudePhaseRunner::run_phase()` — no verify cmd | Yes | |
| `ClaudePhaseRunner::run_phase()` — spec level no claude | Yes | |
| `MockPhaseRunner` | Yes | |
| `ClaudePhaseRunner::run_phase_inner()` — **claude spawn + output parsing** | No | **Requires live claude binary; never tested via mock** |
| `ClaudePhaseRunner::run_verify_phase()` — **verify_prompt path** | No | **The verify_prompt → spawn_claude path is untested** |
| `ClaudePhaseRunner::run_phase_full()` | No | **Never tested directly; only run_phase is tested** |
| `on_crash = "retry"` logic | No | **Untested crash handling** |

### spawn.rs (0 direct tests, tested via worker.rs)

| Function | Tested? | Gap Description |
|----------|---------|-----------------|
| `spawn_claude()` | Indirect | Exit 0, exit 1, stderr tested via worker tests |
| `spawn_claude()` — **timeout** | No | **CRITICAL: timeout path (kill + pgid kill) is untested** — test would need a script that sleeps longer than timeout |
| `spawn_claude()` — **setsid/pgid** | No | **Process group isolation untested** — no test verifies grandchildren are killed |
| `spawn_claude()` — **PID file write/cleanup** | No | **PID file lifecycle untested** |
| `pid_dir()` | No | **Untested** |
| `pid_file_for()` | No | **Untested** |
| `spawn_claude()` — **stream-json parsing** | No | **The stdout JSON event parsing (assistant, result) is untested** |

### phases.rs (35 tests)

| Function | Tested? | Gap Description |
|----------|---------|-----------------|
| `PhaseRegistry::new()` | Yes | Core phases loaded |
| `PhaseRegistry::from_dir()` | Yes | Via test_registry() |
| `PhaseRegistry::get()` | Yes | |
| `PhaseRegistry::list()` | Yes | |
| `PhaseRegistry::core_names()` | Yes | |
| `PhaseRegistry::user_names()` | Yes | |
| `PhaseRegistry::is_user_override()` | Yes | |
| `PhaseRegistry::load_user_phases()` | Yes | Override and new phase |
| `default_phases()` | Yes | All modes |
| `default_pipeline()` / `fallback_pipeline()` | Yes | All modes |
| `resolve_pipeline()` | Yes | Default, spec override, task override, both |
| `resolve_task_phases()` | Yes | Default and override |
| `build_phase_prompt()` | Yes | With template, with task context, empty template |
| `parse_phase_output()` | Yes | Approved, rejected with requeue, no signals, plan-critique rejection |
| `PhaseConfig::from_toml()` | Indirect | Via load tests |
| `load_phase_file()` | Indirect | Via registry tests |
| `TemplateVar::validate()` | Indirect | Via worker tests |
| **Pipeline from pipelines.toml** | No | **`find_pipelines_file()` and `load_pipeline_from_file()` are untested** — only fallback defaults are exercised |
| **Prompt template file resolution** | Partial | One test checks resolved template content but no test covers user override path (user ~/.boi/ > repo root) |

### worktree.rs (8 tests)

| Function | Tested? | Gap Description |
|----------|---------|-----------------|
| `create()` | Yes | Create and idempotent |
| `commit_changes()` | Yes | With changes and no changes |
| `merge_back()` | Yes | Success path |
| `cleanup()` | Yes | Existing and nonexistent |
| `delete_branch()` | Yes | |
| `cleanup_stale()` | Yes | Empty base |
| `branch_name()` | Indirect | |
| `create()` — **failure when repo doesn't exist** | No | **Error path untested** |
| `merge_back()` — **merge conflict** | No | **CRITICAL: merge conflict handling untested** |
| `cleanup_stale()` — **with actual stale worktrees** | No | Only empty-base tested; no test creates a dir without .git and verifies removal |

### telemetry.rs (8 tests)

| Function | Tested? | Gap Description |
|----------|---------|-----------------|
| `Telemetry::new()` | Yes | |
| `Telemetry::emit()` | Yes | |
| `Telemetry::recent()` | Yes | |
| `Telemetry::by_spec()` | Yes | |
| `Telemetry::by_type()` | Yes | |
| `Telemetry::by_level()` | Yes | |
| `LogLevel::from_str()` | Yes | |
| `LogLevel` ordering | Yes | |
| `Telemetry::default_db_path()` | No | **Untested** |
| `Telemetry::emit()` — **DB open failure** | No | No test verifies graceful handling when DB path is invalid |
| `Telemetry::emit()` — **stderr output** | No | No test verifies messages printed to stderr based on `stderr_level` threshold |

### prompt.rs (1 test)

| Function | Tested? | Gap Description |
|----------|---------|-----------------|
| `build_prompt()` | Yes | |

### fmt.rs (0 tests)

| Function | Tested? | Gap Description |
|----------|---------|-----------------|
| `progress_bar()` | No | **Untested** |
| `time_ago()` | No | **Untested** |
| `elapsed_since()` | No | **Untested** |
| `term_width()` | No | **Untested** (depends on terminal) |
| `truncate()` | No | **Untested** |
| `display_width()` | No | **Untested** |
| `is_pid_alive()` | No | **Untested** |
| `ensure_db_dir()` | No | **Untested** |

### CLI modules (4 tests total, all in log.rs)

| Function | Tested? | Gap Description |
|----------|---------|-----------------|
| `find_latest_daemon_log()` | Yes | 4 tests |
| All other CLI functions | No | **CLI dispatch functions are completely untested** — render_single_spec, cmd_status, cmd_dispatch, cmd_cancel, cmd_doctor, cmd_config, cmd_workers, cmd_spec, cmd_daemon, cmd_start, cmd_stop, cmd_restart, daemon_lock_path, try_acquire_daemon_lock, is_daemon_locked, read_daemon_pid, cmd_telemetry |

---

## Critical Gaps

### 1. gen_id() Collision Handling

**Risk:** If the ID space fills up (unlikely with 4 hex bytes = 65536 values per prefix, but possible with long-running queues), `gen_id()` will loop forever.

**Test to add:**
```rust
#[test]
fn test_gen_id_collision_retry() {
    // Pre-fill the specs table with a known ID, then verify gen_id
    // produces a different one (proves the EXISTS check fires).
    let q = open_mem();
    // Insert a spec with a known ID
    q.conn.execute(
        "INSERT INTO specs (id, title, mode, status, queued_at)
         VALUES ('S0000', 'blocker', 'execute', 'queued', '2026-01-01T00:00:00Z')",
        [],
    ).unwrap();
    // gen_id should skip S0000 and produce a different ID
    let id = gen_id('S', &q.conn);
    assert_ne!(id, "S0000");
    assert!(is_valid_spec_id(&id));
}
```

### 2. recover_stuck_specs()

**Risk:** Daemon crash recovery is the safety net for production reliability. If it silently fails, specs stuck in 'running' or 'assigning' are permanently orphaned.

**Test to add:**
```rust
#[test]
fn test_recover_stuck_specs() {
    let q = open_mem();
    let spec = make_spec("S", vec![make_task("t-1", "T")]);

    // Create two specs: one running, one assigning
    let id1 = q.enqueue(&spec, None).unwrap();
    q.update_spec(&id1, "running").unwrap();
    let st = q.status(&id1).unwrap().unwrap();
    let t1 = st.tasks[0].id.clone();
    q.update_task(&id1, &t1, "RUNNING").unwrap();

    let id2 = q.enqueue(&spec, None).unwrap();
    q.conn.execute(
        "UPDATE specs SET status = 'assigning' WHERE id = ?1",
        params![id2],
    ).unwrap();

    // Also a completed spec (should NOT be reset)
    let id3 = q.enqueue(&spec, None).unwrap();
    q.update_spec(&id3, "completed").unwrap();

    let count = q.recover_stuck_specs().unwrap();
    assert_eq!(count, 2); // two specs reset

    let st1 = q.status(&id1).unwrap().unwrap();
    assert_eq!(st1.spec.status, "queued");
    assert_eq!(st1.tasks[0].status, "PENDING"); // task reset too

    let st2 = q.status(&id2).unwrap().unwrap();
    assert_eq!(st2.spec.status, "queued");

    let st3 = q.status(&id3).unwrap().unwrap();
    assert_eq!(st3.spec.status, "completed"); // untouched
}
```

### 3. Hook Timeout + Zombie Prevention

**Risk:** A blocking hook that hangs forever will stall the entire worker. The timeout + kill path has never been exercised.

**Test to add:**
```rust
#[test]
fn test_fire_blocking_timeout_kills_child() {
    let mut hooks = HashMap::new();
    hooks.insert(
        ON_TASK_START.to_string(),
        HookEntry {
            command: "sleep 60".to_string(),
            blocking: Some(true),
            timeout: Some(1), // 1 second timeout
        },
    );
    let config = HookConfig { hooks: Some(hooks) };
    let payload = json!({"spec_id": "s0001"});
    let start = std::time::Instant::now();
    let result = fire(&config, ON_TASK_START, &payload);
    let elapsed = start.elapsed();
    assert!(result.is_ok()); // must not panic
    assert!(elapsed.as_secs() < 5, "should timeout in ~1s, took {:?}", elapsed);
}
```

### 4. spawn_claude Timeout + Process Group Kill

**Risk:** If a claude subprocess spawns grandchildren that hold pipes open, the parent will hang on `reader_handle.join()`. The setsid + pgid kill path is the fix, but it's never tested.

**Test to add:**
```rust
#[test]
fn test_spawn_claude_timeout() {
    // Create a script that sleeps longer than timeout
    let script = test_utils::mock_claude_script_with_output(0, "", "", "timeout_test");
    // Overwrite with a sleep script
    std::fs::write(&script, "#!/bin/sh\nsleep 60\n").unwrap();
    let bin = script.to_str().unwrap();
    let cr = spawn_claude("prompt", "/tmp", 2, None, None, bin).unwrap();
    assert!(!cr.success);
    assert_eq!(cr.output, "timeout");
    assert!(cr.total_ms >= 2000);
    assert!(cr.total_ms < 10000); // should not hang
}
```

### 5. Worker State Machine — Pause Verdict

**Risk:** The `Verdict::Pause` path sets spec to "paused" and exits the loop. Never tested via mock runner.

**Test to add:**
```rust
#[test]
fn test_phase_pipeline_pause_verdict() {
    let yaml = "title: \"Pause Test\"\nmode: execute\ntasks:\n  - id: t-1\n    title: \"Task\"\n    status: PENDING\n";
    let (queue, spec_id, db_path, spec_path, repo) = setup_phase_test("pipeline_pause", yaml);
    let config = WorkerConfig { retry_count: 0, ..Default::default() };
    let registry = PhaseRegistry::new();
    let mock = MockPhaseRunner::new(vec![
        Verdict::Proceed, // spec-review
        Verdict::Pause { prompt: "Need human input".into() },
    ]);
    let tel = test_telemetry();
    with_test_env("true", repo.to_str().unwrap(), || {
        run_worker_with_phases(&spec_id, &spec_path, &db_path, &HookConfig::default(), &config, &registry, &mock, &tel).unwrap();
    });
    let st = queue.status(&spec_id).unwrap().unwrap();
    assert_eq!(st.spec.status, "paused");
}
```

### 6. Worker State Machine — Requeue Limit Exceeded

**Risk:** If a task gets requeued more than `retry_count` times, it should fail. This path is never tested.

**Test to add:**
```rust
#[test]
fn test_phase_pipeline_requeue_limit() {
    let yaml = "title: \"Requeue Test\"\nmode: execute\ntasks:\n  - id: t-1\n    title: \"Task\"\n    status: PENDING\n";
    let (queue, spec_id, db_path, spec_path, repo) = setup_phase_test("pipeline_requeue", yaml);
    let config = WorkerConfig { retry_count: 1, ..Default::default() };
    let registry = PhaseRegistry::new();
    // spec-review proceeds, then execute passes, but task-verify keeps requesting redo
    let mock = MockPhaseRunner::new(vec![
        Verdict::Proceed, // spec-review
        Verdict::Proceed, // execute
        Verdict::Redo { tasks: vec![] }, // task-verify -> requeue to execute (attempt 1)
        Verdict::Proceed, // execute (retry)
        Verdict::Redo { tasks: vec![] }, // task-verify -> requeue to execute (attempt 2, exceeds limit)
    ]);
    let tel = test_telemetry();
    with_test_env("true", repo.to_str().unwrap(), || {
        run_worker_with_phases(&spec_id, &spec_path, &db_path, &HookConfig::default(), &config, &registry, &mock, &tel).unwrap();
    });
    let st = queue.status(&spec_id).unwrap().unwrap();
    assert_eq!(st.spec.status, "failed");
}
```

### 7. Deadlock Detection in TaskSelect

**Risk:** If all remaining tasks are blocked by unsatisfied deps (e.g., circular deps introduced at runtime via `block_task()`), the worker should fail with a deadlock error instead of looping forever.

**Test to add:**
```rust
#[test]
fn test_phase_pipeline_deadlock_detection() {
    let yaml = "title: \"Deadlock Test\"\nmode: execute\ntasks:\n  - id: t-1\n    title: \"A\"\n    status: PENDING\n    depends: [t-2]\n  - id: t-2\n    title: \"B\"\n    status: PENDING\n    depends: [t-1]\n";
    // Use parse_unchecked to bypass validation (circular deps)
    // Then manually set up DB with circular deps
    // Worker should detect deadlock and fail the spec
}
```

### 8. ensure_column Migration

**Risk:** Schema migration silently fails if ALTER TABLE errors. This is the only path for upgrading existing DBs.

**Test to add:**
```rust
#[test]
fn test_ensure_column_adds_missing_column() {
    let q = open_mem();
    // Create a table without the 'workspace' column
    q.conn.execute("DROP TABLE IF EXISTS test_tbl", []).unwrap();
    q.conn.execute("CREATE TABLE test_tbl (id TEXT PRIMARY KEY)", []).unwrap();
    Queue::ensure_column(&q.conn, "test_tbl", "workspace", "TEXT");
    // Verify column exists
    let has_col: bool = q.conn
        .prepare("PRAGMA table_info(test_tbl)")
        .and_then(|mut stmt| {
            let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
            Ok(rows.filter_map(|r| r.ok()).any(|n| n == "workspace"))
        })
        .unwrap_or(false);
    assert!(has_col, "workspace column should have been added");
}
```

### 9. Merge Conflict Handling

**Risk:** If the worktree branch has conflicting changes with the source branch, `merge_back()` fails. The error path is tested only indirectly (Cleanup state breaks on merge error), but no test verifies conflict detection.

**Test to add:**
```rust
#[test]
fn test_merge_back_conflict() {
    // Create repo, create worktree, modify same file in both,
    // verify merge_back returns Err
    let repo_dir = test_utils::test_git_repo("wt-conflict-repo");
    let wt_base = test_utils::test_dir("wt-conflict-home");
    std::env::set_var("HOME", wt_base.to_str().unwrap());
    let spec_id = "test-conflict-001";
    let repo = repo_dir.to_str().unwrap();
    let dest = create(spec_id, repo).unwrap();
    // Modify file in worktree
    std::fs::write(dest.join("README.md"), "worktree version").unwrap();
    commit_changes(spec_id, "worktree change").unwrap();
    // Modify same file in source repo
    std::fs::write(repo_dir.join("README.md"), "source version").unwrap();
    std::process::Command::new("git").args(["add", "."]).current_dir(&repo_dir).output().unwrap();
    std::process::Command::new("git").args(["commit", "-m", "source change"]).current_dir(&repo_dir).output().unwrap();
    // merge_back should fail
    let result = merge_back(spec_id, repo);
    assert!(result.is_err(), "merge should fail on conflict");
    cleanup(spec_id).unwrap();
}
```

### 10. Daemon Lock and Lifecycle

**Risk:** `cmd_daemon()` is the production entry point. Lock acquisition, heartbeat writing, worker spawning, spec polling, and graceful shutdown are all untested.

No concrete test proposed here because these require process-level integration testing, but the gap should be noted.

---

## Test Quality Issues

### 1. Shared Environment State (worktree.rs)

The worktree tests mutate `HOME` environment variable:
```rust
std::env::set_var("HOME", wt_base.to_str().unwrap());
```

While they use a `TEST_LOCK` mutex for serialization, this is **not sufficient** if other modules' tests read HOME concurrently (e.g., `config.rs` default paths, `hooks.rs` user file lookup). The env var mutation is also marked as an unsafe operation in recent Rust editions.

**Fix:** Use `test_utils::test_dir()` to create isolated HOME directories and pass paths explicitly instead of mutating the global env.

### 2. Shared Environment State (worker.rs)

The `with_test_env()` helper also mutates `CLAUDE_BIN` and `BOI_REPO` env vars, guarded by `ENV_LOCK`. Same risk as above — other tests may read these vars without acquiring the lock.

### 3. Hardcoded /tmp Paths in Tests

Three test functions use hardcoded `/tmp`:
- `test_run_verify_success()` — `run_verify("true", "/tmp")`
- `test_run_verify_failure()` — `run_verify("false", "/tmp")`
- `test_run_verify_missing_command()` — `run_verify("exit 1", "/tmp")`
- Various runner tests pass `"/tmp"` as worktree_path

These work on macOS/Linux but are technically not portable. The `test_utils::test_dir()` function exists and should be used instead.

### 4. Duplicate State Machine Code

`state_machine.rs` and `worker.rs` both contain a `WorkerState` enum and `run_worker_with_phases()` function. They diverge in how they load spec data (YAML file vs DB-sourced). Only `worker.rs`'s copy has tests. The `state_machine.rs` copy is tested only indirectly through `worker.rs`'s `run_worker()` function calling the DB-based version.

**Risk:** If the two implementations diverge further, bugs in `state_machine.rs` won't be caught.

### 5. No Tests for CLI Output Formatting

`cli/status.rs::render_single_spec()` produces complex ANSI-formatted output. No test verifies it doesn't panic on edge cases (empty tasks, missing timestamps, very long titles).

### 6. fmt.rs Has Zero Tests

All formatting utilities (`progress_bar`, `time_ago`, `elapsed_since`, `truncate`) are untested. These are called from CLI rendering code and could silently break output.

### 7. Test File Cleanup

Test helpers create files under `/tmp/boi-test-*` but rely on `remove_file` calls that are often wrapped in `let _ = ...`. There's no cleanup sweep. Long-running test suites accumulate temp files.

---

## Summary of Priorities

| Priority | Gap | Impact |
|----------|-----|--------|
| P0 | `recover_stuck_specs()` untested | Daemon crash recovery is the production safety net |
| P0 | `spawn_claude` timeout + pgid kill untested | Worker hang risk on runaway subprocesses |
| P0 | Hook timeout + zombie prevention untested | Hook hangs stall workers |
| P1 | Worker Pause / Requeue / Retry state transitions untested | Core state machine paths with no coverage |
| P1 | Deadlock detection untested | Worker infinite loop risk |
| P1 | Merge conflict handling untested | Worktree cleanup path on conflict unknown |
| P1 | `ensure_column` migration untested | DB upgrade path is production-critical |
| P2 | `gen_id()` collision loop untested | Low probability but infinite loop risk |
| P2 | `set_spec_fields`, `set_priority`, `set_depends_on` untested | Queue mutation functions used by CLI |
| P2 | `phase_cost_summary`, `phase_cost_total` untested | Cost tracking queries |
| P2 | fmt.rs entirely untested | Display formatting bugs |
| P3 | CLI commands untested | Hard to unit test but integration gaps |
| P3 | Daemon lock/lifecycle untested | Requires process-level testing |
| P3 | Env var mutation in tests | Test isolation risk |
