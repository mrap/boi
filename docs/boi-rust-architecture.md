# BOI Rust Architecture

> Produced by q-917 iteration 2.
> Informed by: docs/boi-current-state.md (t-1 audit).
> Date: 2026-04-27

---

## Overview

BOI is ported to a single self-contained Rust binary (`boi`). The binary subsumes the current `boi.sh` (4929 lines), `daemon.py` (3028 lines), `worker.py` (1782 lines), and 20+ Python library modules. The core architecture is preserved: fresh-context worker sessions, SQLite-backed queue, tmux-isolated subprocesses, append-only event log. What changes: the hook system is generalized and hook execution replaces all hardcoded hex-events calls.

```
boi (Rust binary)
  ├── boi dispatch / status / log / cancel / ...    ← CLI subcommands
  ├── boi daemon --foreground                       ← daemon process (polls every 5s)
  └── boi worker <spec_id> --worktree W --iter N    ← worker process (spawned by daemon)
```

The daemon and worker are subcommands of the same binary. The daemon spawns workers via tmux, passing `boi worker ...` as the session command. No Python on PATH required at runtime.

---

## 1. Module Structure

```
boi/
  Cargo.toml
  src/
    main.rs             — clap CLI dispatch; selects daemon/worker/CLI subcommand
    cli/
      mod.rs            — subcommand enum + dispatch
      dispatch.rs       — boi dispatch (enqueue spec, start daemon if needed)
      status.rs         — boi status [--watch] [--json]
      queue.rs          — boi queue [--json]
      log.rs            — boi log <id> [--full] [--failures]
      cancel.rs         — boi cancel <id>
      resume.rs         — boi resume <id>
      workers.rs        — boi workers [--json]
      telemetry.rs      — boi telemetry <id> [--json]
      outputs.rs        — boi outputs <id>
      purge.rs          — boi purge [--all] [--dry-run]
      stop.rs           — boi stop
      install.rs        — boi install [--workers N]
      doctor.rs         — boi doctor
      spec_cmd.rs       — boi spec <id> [add|skip|next|block|edit]
      project.rs        — boi project <create|list|status|context|delete>
      config_cmd.rs     — boi config [get|set]
      critic_cmd.rs     — boi critic [status|run|disable|enable|checks]
      review_cmd.rs     — boi review <id>
      cleanup.rs        — boi cleanup
    daemon/
      mod.rs            — tokio main loop (poll every 5s)
      scheduler.rs      — dequeue eligible specs; assign to free worker slots
      monitor.rs        — detect worker completion; update spec state post-iteration
      recovery.rs       — on startup: reset stuck running→requeued with cooldown
    worker/
      mod.rs            — worker entrypoint; orchestrates one iteration
      prompt.rs         — assemble prompt from spec + mode fragment + project context
      runtime.rs        — Runtime trait + ClaudeRuntime/CodexRuntime/HermesRuntime
      worktree.rs       — git worktree create/sync/cleanup
      outputs.rs        — collect modified files into ~/.boi/outputs/<id>/
      verify.rs         — run outcome verify shell commands; reset last DONE on failure
      workspace_guard.rs— before/after git status snapshot; leak detection
    spec/
      mod.rs            — BoiSpec, BoiTask, Outcome structs
      parser.rs         — auto-detect YAML vs Markdown; parse to BoiSpec
      yaml_parser.rs    — serde_yaml deserialization
      md_parser.rs      — regex-based Markdown parser (### t-N: Title / PENDING)
      dag.rs            — topological sort (Kahn's), find_assignable, critical_path
      editor.rs         — atomic in-place spec mutations (mark DONE, append tasks)
    queue/
      mod.rs            — queue operations (enqueue, dequeue, update status, list)
      db.rs             — rusqlite connection, WAL mode, schema, versioned migrations
      lock.rs           — fcntl advisory lock on ~/.boi/queue/.lock
    hooks/
      mod.rs            — HookRunner: fire hooks with JSON payload on stdin
      config.rs         — parse hooks: section from config.yaml
    telemetry/
      mod.rs            — write/read per-iteration JSON and aggregated telemetry
      events.rs         — append-only event files in ~/.boi/events/event-NNNNN.json
    config/
      mod.rs            — load ~/.boi/config.yaml (serde_yaml); defaults
    phases/
      mod.rs            — TOML phase configs; phase resolution and next-phase logic
    critic/
      mod.rs            — critic runner (spawns boi worker in task-verify phase)
    util/
      mod.rs            — path expansion, tmux helpers, atomic file write (tmp→mv)
      tmux.rs           — tmux spawn, has-session poll, kill-session
      process.rs        — run_command helper with timeout and captured output
```

### Python → Rust Module Mapping

| Python | Rust |
|--------|------|
| `boi.sh` | `src/main.rs` + `src/cli/` |
| `daemon.py` | `src/daemon/` |
| `worker.py` | `src/worker/` |
| `lib/spec_parser.py` | `src/spec/parser.rs` + `yaml_parser.rs` + `md_parser.rs` |
| `lib/dag.py` | `src/spec/dag.rs` |
| `lib/db.py` | `src/queue/db.rs` |
| `lib/queue.py` | `src/queue/mod.rs` |
| `lib/cli_ops.py` | `src/cli/*.rs` |
| `lib/daemon_ops.py` | `src/daemon/scheduler.rs` + `monitor.rs` |
| `lib/hooks.py` | `src/hooks/mod.rs` |
| `lib/locking.py` | `src/queue/lock.rs` |
| `lib/runtime.py` | `src/worker/runtime.rs` |
| `lib/task_worktree.py` | `src/worker/worktree.rs` |
| `lib/workspace_guard.py` | `src/worker/workspace_guard.rs` |
| `lib/event_log.py` | `src/telemetry/events.rs` |
| `lib/telemetry.py` | `src/telemetry/mod.rs` |
| `lib/phases/*.toml` | `src/phases/mod.rs` |
| `lib/config.py` | `src/config/mod.rs` |
| `lib/spec_editor.py` | `src/spec/editor.rs` |

---

## 2. CLI

Built with `clap` (derive feature). Subcommands are identical to the Python CLI for full drop-in compatibility.

```rust
#[derive(Parser)]
#[command(name = "boi", version)]
enum Cli {
    Dispatch(DispatchArgs),
    Status(StatusArgs),
    Queue(QueueArgs),
    Log(LogArgs),
    Cancel(CancelArgs),
    Resume(ResumeArgs),
    Workers(WorkersArgs),
    Telemetry(TelemetryArgs),
    Outputs(OutputsArgs),
    Dashboard,
    Purge(PurgeArgs),
    Stop,
    Install(InstallArgs),
    Doctor,
    Spec(SpecArgs),
    Project(ProjectArgs),
    Config(ConfigArgs),
    Critic(CriticArgs),
    Review(ReviewArgs),
    Cleanup,
    // Internal — invoked by daemon via tmux
    Daemon(DaemonArgs),
    Worker(WorkerArgs),
    // New
    Migrate(MigrateArgs),
}
```

### New Commands (not in Python)

| Command | Purpose |
|---------|---------|
| `boi migrate` | Upgrade boi.db + spec IDs from q-NNN/t-N to SNNNNNN/TNNNNNN |
| `boi daemon` | Start daemon (replaces daemon.py); supports `--foreground` |
| `boi worker` | Internal: execute one iteration (replaces worker.py) |

### `boi dispatch` Flags (full parity)

```
--spec FILE           Path to spec file (required)
--priority N          Queue priority (default: 100)
--max-iter N          Max iterations (default: 30)
--mode MODE           execute|challenge|discover|generate (aliases e/c/d/g)
--worktree PATH       Pin to a specific worktree
--no-critic           Skip critic on completion
--timeout N           Worker timeout seconds (default: 600)
--project NAME        Associate with project
--experiment-budget N Override experiment budget
--push                Push git changes after completion
--commit-scope SCOPE  Git commit scope string
```

---

## 3. Spec Parser

### Data Structures

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoiSpec {
    pub title: Option<String>,
    pub initiative: Option<String>,
    pub mode: ExecutionMode,
    pub runtime: RuntimeKind,
    pub workspace: Option<PathBuf>,
    pub max_iterations: u32,
    pub timeout_seconds: u32,
    pub push: bool,
    pub commit_scope: Option<String>,
    pub no_critic: bool,
    pub outcomes: Vec<Outcome>,
    pub tasks: Vec<BoiTask>,
    pub context: Option<String>,
    pub error_log: Vec<ErrorEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoiTask {
    pub id: String,              // "t-1" or future "T0000001"
    pub title: String,
    pub status: TaskStatus,
    pub spec: String,            // task body / instructions
    pub verify: Option<String>,  // shell command
    pub depends: Vec<String>,    // dependency task IDs
    pub files: Vec<String>,      // hint: files to read
    pub body: String,            // full raw body (preserved for md round-trip)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TaskStatus {
    Pending,
    Done,
    Failed,
    Skipped,
    ExperimentProposed,
    Superseded(String),  // "t-N"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub description: String,
    pub verify: String,
    pub status: OutcomeStatus,
}
```

### Parser Auto-Detection

```rust
pub fn parse_spec(path: &Path) -> anyhow::Result<BoiSpec> {
    let content = fs::read_to_string(path)?;
    if path.extension().map(|e| e == "yaml" || e == "yml").unwrap_or(false)
        || (!content.trim_start().starts_with('#') && content.contains("tasks:"))
    {
        yaml_parser::parse(&content)
    } else {
        md_parser::parse(&content)
    }
}
```

**YAML format**: direct `serde_yaml::from_str::<BoiSpec>(&content)`.

**Markdown format**: regex-based line scanner:
- Section `### t-N: Title` → task boundary
- Next non-blank line → status (`PENDING`, `DONE`, etc.)
- `**Spec:**`, `**Verify:**`, `**Files:**`, `**Blocked by:**` → field extraction
- `## Outcomes` → outcome list
- `## Error Log` → error entries

### Atomic Spec Editor

```rust
pub fn mark_task_done(path: &Path, task_id: &str) -> anyhow::Result<()> {
    let content = fs::read_to_string(path)?;
    let updated = replace_task_status(&content, task_id, "PENDING", "DONE")?;
    atomic_write(path, &updated)  // write to .tmp, then rename
}

pub fn append_task(path: &Path, task: &BoiTask) -> anyhow::Result<()> {
    // Append new ### t-N: ... PENDING block at end of file
}
```

---

## 4. Queue Management

### SQLite Schema

Identical to Python schema (enables zero-downtime migration). Managed via versioned migrations in `db.rs`.

```rust
const DB_PATH: &str = "~/.boi/boi.db";

pub fn open_db() -> anyhow::Result<Connection> {
    let conn = Connection::open(expand_tilde(DB_PATH))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    run_migrations(&conn)?;
    Ok(conn)
}
```

Migrations are numbered (`001_initial.sql`, `002_add_phase.sql`, ...) and applied in order at startup if the `schema_version` user_version is behind.

### Dequeue Logic

```rust
pub fn dequeue_next(conn: &Connection, available_workers: &[&str]) -> anyhow::Result<Option<QueueEntry>> {
    conn.query_row(
        "SELECT * FROM specs
         WHERE status IN ('queued', 'requeued')
           AND (cooldown_until IS NULL OR cooldown_until < datetime('now'))
           AND (worktree IS NULL OR worktree IN (?))
         ORDER BY priority ASC, submitted_at ASC
         LIMIT 1",
        params![available_worker_list],
        map_row_to_entry,
    ).optional()
}
```

### File Lock

All queue mutations (outside SQLite transactions) acquire an advisory `fcntl` lock on `~/.boi/queue/.lock`. In Rust, use `fs2` crate or manual `fcntl(F_SETLK)` via `nix`:

```rust
pub struct QueueLock(File);

impl QueueLock {
    pub fn acquire(timeout: Duration) -> anyhow::Result<Self> {
        let file = OpenOptions::new().create(true).write(true).open(LOCK_PATH)?;
        let deadline = Instant::now() + timeout;
        loop {
            match flock(&file, FlockArg::LockExclusiveNonblock) {
                Ok(()) => return Ok(QueueLock(file)),
                Err(_) if Instant::now() < deadline => sleep(Duration::from_secs(1)),
                Err(e) => bail!("Could not acquire queue lock: {e}"),
            }
        }
    }
}
```

---

## 5. Worker Management

### Daemon Loop

```rust
// src/daemon/mod.rs
#[tokio::main]
pub async fn run_daemon(config: &Config) -> anyhow::Result<()> {
    recovery::reset_stuck_specs(&db)?;
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        interval.tick().await;
        scheduler::try_assign(&db, &config).await?;
        monitor::check_completions(&db, &config).await?;
    }
}
```

### Worker Spawn

Daemon spawns workers via tmux so the worker persists past daemon restarts and is observable:

```rust
pub async fn spawn_worker(spec_id: &str, worktree: &str, iter: u32, timeout: u32) -> anyhow::Result<()> {
    let session = format!("boi-{spec_id}");
    let cmd = format!(
        "boi worker {spec_id} --worktree {worktree} --iter {iter} --timeout {timeout}"
    );
    tokio::process::Command::new("tmux")
        .args(["-L", "boi", "new-session", "-d", "-s", &session, "bash", "-c", &cmd])
        .status()
        .await?;
    Ok(())
}
```

When `BOI_NO_TMUX=1` is set, the daemon spawns the worker directly via `tokio::process::Command::spawn()` (useful for testing and CI).

### Worker Monitoring

```rust
pub async fn wait_for_worker(spec_id: &str, timeout: Duration) -> anyhow::Result<ExitOutcome> {
    let session = format!("boi-{spec_id}");
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() > deadline {
            kill_tmux_session(&session).await?;
            return Ok(ExitOutcome::Timeout);
        }
        let alive = tmux::has_session(&session).await?;
        if !alive {
            let code = read_exit_file(spec_id)?;
            return Ok(ExitOutcome::Done(code));
        }
        sleep(Duration::from_secs(5)).await;
    }
}
```

### Worker Entrypoint (`boi worker`)

```rust
pub async fn run_worker(args: WorkerArgs) -> anyhow::Result<()> {
    let spec = parse_spec(&args.spec_path)?;
    if !has_pending_tasks(&spec) { return Ok(()); }

    workspace_guard::snapshot_before(&args.worktree)?;
    let prompt = prompt::build(&spec, &args)?;
    let run_sh = runtime::generate_run_script(&prompt, &args)?;

    // Write prompt + run script
    atomic_write(&prompt_path, &prompt)?;
    atomic_write(&run_sh_path, &run_sh)?;

    // Execute (tmux or direct)
    let exit_code = runtime::execute(&run_sh_path, args.timeout).await?;

    // Post-iteration
    workspace_guard::snapshot_after(&args.worktree)?;
    outputs::collect(&args.spec_id, &args.worktree)?;
    verify::run_outcomes(&spec, &args.worktree)?;
    telemetry::write_iteration(&args, exit_code, &pre_counts, &post_counts)?;

    // Fire hooks
    hooks::fire(HookPoint::TaskComplete, &payload).await?;

    fs::write(&exit_path, exit_code.to_string())?;
    Ok(())
}
```

---

## 6. Git Worktree Management

All git operations use `std::process::Command` (no libgit2). This keeps the dependency minimal and matches BOI's existing approach.

```rust
pub fn create_worktree(base_path: &Path, slot: &str) -> anyhow::Result<PathBuf> {
    let worktree_path = boi_dir().join("worktrees").join(slot);
    let branch = format!("boi-worker-{slot}");
    Command::new("git")
        .args(["worktree", "add", "-B", &branch, worktree_path.to_str().unwrap(), "HEAD"])
        .current_dir(&base_path)
        .status()?;
    Ok(worktree_path)
}

pub fn sync_back(spec_id: &str, worktree: &Path, workspace: &Path) -> anyhow::Result<()> {
    // rsync or cp modified files from worktree back to target workspace
    let changed = read_changed_files_manifest(spec_id)?;
    for file in changed {
        fs::copy(worktree.join(&file), workspace.join(&file))?;
    }
    Ok(())
}

pub fn cleanup_worktree(worktree: &Path) -> anyhow::Result<()> {
    Command::new("git").args(["worktree", "remove", "--force", worktree.to_str().unwrap()]).status()?;
    fs::remove_dir_all(worktree).ok();
    Ok(())
}
```

Worktree health check at daemon startup:
```rust
pub fn check_worktree(path: &Path) -> WorktreeHealth {
    if !path.exists() { return WorktreeHealth::Missing; }
    let ok = Command::new("git").args(["-C", path.to_str().unwrap(), "status"]).status().is_ok();
    if ok { WorktreeHealth::Ok } else { WorktreeHealth::Unhealthy }
}
```

---

## 7. Hook System

The hook system is the primary integration point between BOI and external systems (hex-events, notifications, custom tooling). It replaces both the legacy `~/.boi/hooks/` shell scripts and the hardcoded `hex_emit.py` calls.

### Config (`~/.boi/config.yaml`)

```yaml
workers: 3
runtime:
  default: claude

hooks:
  on_dispatch:
    command: "python3 ~/.hex-events/hex_emit.py boi.spec.dispatched"
    blocking: false
    timeout: 10
  on_worker_start:
    command: "python3 ~/.hex-events/hex_emit.py boi.worker.start"
    blocking: false
    timeout: 10
  on_task_start:
    command: "python3 ~/.hex-events/hex_emit.py boi.task.start"
    blocking: false
    timeout: 10
  on_task_complete:
    command: "python3 ~/.hex-events/hex_emit.py boi.task.completed"
    blocking: false
    timeout: 10
  on_task_fail:
    command: "python3 ~/.hex-events/hex_emit.py boi.task.failed"
    blocking: false
    timeout: 10
  on_complete:
    command: "python3 ~/.hex-events/hex_emit.py boi.spec.completed"
    blocking: false
    timeout: 10
  on_fail:
    command: "python3 ~/.hex-events/hex_emit.py boi.spec.failed"
    blocking: false
    timeout: 10
  on_cancel:
    command: "python3 ~/.hex-events/hex_emit.py boi.spec.cancelled"
    blocking: false
    timeout: 10
  on_stall:
    command: "python3 ~/.hex-events/hex_emit.py boi.spec.stalled"
    blocking: false
    timeout: 10
```

### Rust Config Structs

```rust
#[derive(Debug, Deserialize)]
pub struct HookConfig {
    pub command: String,
    #[serde(default)]
    pub blocking: bool,
    #[serde(default = "default_hook_timeout")]
    pub timeout: u64,
}

#[derive(Debug, Deserialize, Default)]
pub struct HooksConfig {
    pub on_dispatch: Option<HookConfig>,
    pub on_worker_start: Option<HookConfig>,
    pub on_task_start: Option<HookConfig>,
    pub on_task_complete: Option<HookConfig>,
    pub on_task_fail: Option<HookConfig>,
    pub on_complete: Option<HookConfig>,
    pub on_fail: Option<HookConfig>,
    pub on_cancel: Option<HookConfig>,
    pub on_stall: Option<HookConfig>,
}
```

### Hook Runner

```rust
pub async fn fire(point: HookPoint, payload: &serde_json::Value) -> anyhow::Result<()> {
    let config = load_config()?;
    let hook = match config.hooks.get(point) { Some(h) => h, None => return Ok(()) };
    let json = serde_json::to_string(payload)?;

    let mut child = tokio::process::Command::new("sh")
        .args(["-c", &hook.command])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    if let Some(stdin) = child.stdin.take() {
        let mut stdin = tokio::io::BufWriter::new(stdin);
        stdin.write_all(json.as_bytes()).await.ok();
    }

    if hook.blocking {
        tokio::time::timeout(
            Duration::from_secs(hook.timeout),
            child.wait(),
        ).await.ok();
    }
    // non-blocking: fire and forget, child runs independently
    Ok(())
}
```

### Hook Payload Schema

All hooks receive a JSON object on stdin. Common fields plus hook-specific ones:

```json
{
  "hook": "on_task_complete",
  "spec_id": "q-001",
  "spec_path": "/Users/mrap/.boi/queue/q-001.spec.md",
  "iteration": 3,
  "timestamp": "2026-04-27T08:00:00Z",
  "task_id": "t-2",
  "task_title": "Design the Rust architecture",
  "tasks_done": 2,
  "tasks_total": 4,
  "duration_seconds": 87
}
```

---

## 8. Telemetry

### Per-Iteration JSON

Written by worker after each iteration to `~/.boi/queue/{spec_id}.iteration-{N}.json`:

```json
{
  "spec_id": "q-001",
  "iteration": 3,
  "exit_code": 0,
  "duration_seconds": 87,
  "started_at": "2026-04-27T08:00:00Z",
  "pre_counts": {"pending": 3, "done": 1, "skipped": 0, "total": 4},
  "post_counts": {"pending": 2, "done": 2, "skipped": 0, "total": 4},
  "tasks_completed": 1,
  "tasks_added": 0,
  "tasks_skipped": 0,
  "model": "claude-sonnet-4-6",
  "estimated_cost_usd": 0.0473
}
```

### Event Log

Append-only files in `~/.boi/events/event-NNNNN.json` (same format as Python). Sequence number managed by reading the current highest file in the directory.

```rust
pub fn append_event(event: &BoiEvent) -> anyhow::Result<()> {
    let next_seq = next_event_seq()?;
    let filename = format!("event-{:05}.json", next_seq);
    let path = events_dir().join(filename);
    atomic_write(&path, &serde_json::to_string_pretty(event)?)
}
```

### Aggregated Telemetry

```rust
pub fn update_telemetry(spec_id: &str, iteration: &IterationMeta) -> anyhow::Result<()> {
    let path = queue_dir().join(format!("{spec_id}.telemetry.json"));
    let mut telem: Telemetry = if path.exists() {
        serde_json::from_str(&fs::read_to_string(&path)?)?
    } else { Telemetry::new(spec_id) };
    telem.push_iteration(iteration);
    atomic_write(&path, &serde_json::to_string_pretty(&telem)?)
}
```

---

## 9. Concurrency Model

### Runtime Choice

The daemon uses `tokio` (multi-threaded). Workers are separate OS processes (not tokio tasks); this preserves process isolation and tmux observability.

```
Daemon process (tokio multi-thread)
  ├── Poll loop task          — wakes every 5s, assigns work
  ├── Monitor loop task       — wakes every 5s, checks completions
  └── Signal handler task     — SIGTERM/SIGINT graceful shutdown

Worker process (spawned by daemon via tmux)
  └── Synchronous execution (no tokio needed; worker is single-threaded)
      └── claude subprocess   — tmux session running claude -p
```

### Concurrency Configuration

```yaml
workers: 3        # max parallel worker slots (default 3, max 10)
poll_interval: 5  # daemon poll interval in seconds
```

Each worker slot maps to a worktree (`w-1`, `w-2`, `w-3`). The scheduler holds a `Arc<Mutex<Connection>>` for SQLite access. The lock is short-lived: only held during dequeue SELECT + UPDATE.

### Connection Pool

For concurrent CLI commands (e.g. `boi status` while daemon runs), use SQLite WAL mode (same as Python). No separate connection pool needed: each CLI invocation opens its own connection, reads with WAL, exits. The daemon's connection is the only persistent writer.

---

## 10. Error Handling

### Domain Error Enum

```rust
#[derive(Debug, thiserror::Error)]
pub enum BoiError {
    #[error("spec not found: {0}")]
    SpecNotFound(String),
    #[error("spec parse error in {path}: {msg}")]
    ParseError { path: String, msg: String },
    #[error("worker timeout after {0}s")]
    WorkerTimeout(u64),
    #[error("db error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("hook failed: {0}")]
    HookFailed(String),
    #[error("queue locked: {0}")]
    QueueLocked(String),
}
```

All internal functions return `anyhow::Result<T>`. At the boundary (CLI and daemon loop), errors are caught, logged, and converted to exit codes or state transitions.

### Retry / Failure Escalation

| Condition | Action |
|-----------|--------|
| Worker exit code != 0 | `consecutive_failures += 1`; cooldown 60s; requeue |
| Worker timeout | Exit code 124; same as above |
| `consecutive_failures >= 5` | Status → `failed`; fire `on_fail` hook |
| `iteration >= max_iterations` | Status → `failed`; fire `on_fail` hook |
| Outcome verify fails | Reset last DONE task → PENDING; requeue |
| Hook subprocess fails | Log warning; never block spec progression |
| Daemon crash | On restart: `recovery::reset_stuck_specs()` finds `status=running`, resets to `requeued` + cooldown |

### Graceful Shutdown

Daemon catches SIGTERM:
1. Stop polling loop
2. Wait up to 30s for any in-flight monitors to finish
3. Leave running worker tmux sessions alive (they self-terminate via `{spec_id}.exit`)
4. Exit 0

---

## 11. Data Migration

### Phase 1 — Coexistence (no migration required)

The Rust binary reads and writes the existing `~/.boi/boi.db` using the same SQLite schema. Python and Rust can operate on the same database simultaneously. The spec file format (Markdown + YAML) is identical. Users can install the Rust binary without touching their existing queue.

Migration path:
1. Build `boi` Rust binary
2. Replace `~/.boi/boi` wrapper to call Rust binary (or add to PATH)
3. Python daemon can be stopped; Rust daemon started (`boi daemon --foreground &`)
4. Existing queued specs continue running unmodified

### Phase 2 — Identifier Upgrade (`boi migrate`)

After full Rust cutover, run `boi migrate` to upgrade IDs:

```
q-001 → S0000001
q-002 → S0000002
t-1   → T0000001
t-2   → T0000002
```

Migration steps:
1. Lock queue (acquire `~/.boi/queue/.lock`)
2. Require no running specs (`status='running'` count = 0)
3. For each spec in SQLite: generate new `SNNNNNN` ID; update `boi.db`
4. Rename spec files: `q-001.spec.md` → `S0000001.spec.md`
5. Update task IDs within each spec file (regex replace `### t-N:` → `### TNNNNNN:`)
6. Write migration record to `~/.boi/events/` (event type: `migration_complete`)
7. Release lock

Rollback: migration writes a backup `boi.db.pre-migrate` and a `queue-backup/` directory before making any changes.

### Spec File Backward Compatibility

The Rust parser accepts both old-style (`q-NNN`, `t-N`) and new-style (`SNNNNNN`, `TNNNNNN`) identifiers. The `depends:` field and all cross-references are resolved by string equality regardless of format. No spec file changes are required for Phase 1.

---

## Crate Dependencies

```toml
[dependencies]
clap = { version = "4", features = ["derive"] }
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde_yaml = "0.9"
rusqlite = { version = "0.31", features = ["bundled"] }
anyhow = "1"
thiserror = "1"
regex = "1"
nix = { version = "0.28", features = ["fs"] }  # fcntl file locking
chrono = { version = "0.4", features = ["serde"] }
```

Optional (for dashboard/status color output):
```toml
crossterm = "0.27"
```

The `rusqlite` `bundled` feature compiles SQLite from source, eliminating the system SQLite version assumption. No Python on PATH required at runtime.

---

## Key Design Decisions

1. **Daemon + Worker as subcommands of the same binary** — eliminates coordination complexity; daemon spawns `boi worker ...` which it already knows how to find.

2. **tmux is preserved** — process isolation and observability matter more than architectural purity. Replacing tmux with pure tokio tasks would require rebuilding all the session observability tooling.

3. **SQLite schema is unchanged in Phase 1** — enables zero-downtime migration; Python and Rust coexist on the same DB during transition.

4. **No libgit2** — `std::process::Command` for all git ops matches the Python approach and avoids a large native dependency with complex linking on macOS.

5. **`anyhow` throughout, `thiserror` at domain boundaries** — consistent with hex harness patterns; easy propagation without losing context.

6. **Hooks unify the two existing mechanisms** — the old `~/.boi/hooks/*.sh` scripts and the hardcoded hex-events calls both become entries in `config.yaml hooks:`. This is the primary architectural improvement over Python.
