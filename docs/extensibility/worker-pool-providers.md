# Worker Pool Providers

Pluggable worker pool for BOI. The current local-thread pool becomes one of several options.

## What BOI Needs from a Worker Pool

A worker pool **accepts specs and runs them to completion**. BOI's contract with a pool provider is five operations:

| Operation | Input | Output | Required |
|-----------|-------|--------|----------|
| **spawn** | spec_id, spec_path, workspace_path, config | job_id | yes |
| **status** | job_id | running / completed / failed / timeout | yes |
| **collect** | job_id | exit_code, stdout, stderr | yes |
| **cancel** | job_id | success/failure | yes |
| **cleanup** | job_id | (none) | no |

**Invariants every provider must satisfy:**

1. **One spec per job.** A spawn call starts exactly one worker for one spec. The provider may parallelize tasks internally (that's the runtime's job), but from BOI's perspective, one spawn = one job.
2. **Idempotent cancel.** Cancelling an already-finished or already-cancelled job is not an error.
3. **Status convergence.** A job in `running` state must eventually reach `completed`, `failed`, or `timeout`. No zombie jobs.
4. **Collect after terminal.** Collect must be callable after the job reaches a terminal state. The provider retains stdout/stderr until cleanup is called (or a TTL expires).
5. **Isolation.** Two concurrent jobs must not interfere. Resource contention (CPU, memory) is acceptable; state corruption is not.

## Current Implementation: Local Thread Pool

Today's `run_daemon()` in `worker.rs` is a local thread pool:

```rust
// Simplified from actual code
let active: Vec<JoinHandle<()>> = vec![];
loop {
    active.retain(|h| !h.is_finished());
    if active.len() < max_workers {
        let rec = queue.dequeue()?;
        let handle = thread::spawn(move || {
            run_worker(&rec.id, &rec.spec_path, ...);
        });
        active.push(handle);
    }
    thread::sleep(Duration::from_secs(5));
}
```

This is the implicit `local` provider. The daemon polls the SQLite queue, spawns `std::thread` workers, and polls `JoinHandle::is_finished()` to track completion. Results flow through SQLite — the daemon never reads stdout/stderr from the thread directly.

**Key observation:** The daemon already uses an indirect result channel (SQLite), not direct thread communication. This means the pool abstraction doesn't need to change the result-collection pattern — remote providers just need a different way to get status into the database.

## Interface: Command-Based Provider

A worker pool provider is a set of shell commands. BOI calls them via `sh -c`. The provider does not need to be compiled into BOI.

### Config Schema

In `~/.boi/config.yaml`:

```yaml
worker_pool:
  type: local                          # built-in type name
  max_workers: 5
```

Or for custom providers:

```yaml
worker_pool:
  type: custom
  max_workers: 5
  spawn: "/path/to/provider spawn {spec_id} {spec_path} {workspace}"
  status: "/path/to/provider status {job_id}"
  collect: "/path/to/provider collect {job_id}"
  cancel: "/path/to/provider cancel {job_id}"
  cleanup: "/path/to/provider cleanup {job_id}"
```

### Command Contract

**spawn** receives:
- `{spec_id}` — unique identifier (e.g., `q-42`)
- `{spec_path}` — absolute path to the spec YAML file
- `{workspace}` — absolute path to the isolated workspace directory (created by the workspace backend)

Must print exactly one line to stdout: a **job_id** (opaque string, provider-defined). Non-zero exit = spawn failed. The provider is responsible for starting the actual work — it may fork a process, submit to a queue, SSH into a remote machine, etc.

**status** receives:
- `{job_id}` — the string returned by spawn

Must print exactly one line to stdout: one of `running`, `completed`, `failed`, `timeout`. Non-zero exit = status check failed (BOI retries on next poll).

**collect** receives:
- `{job_id}`

Must print JSON to stdout:

```json
{
  "exit_code": 0,
  "stdout": "...",
  "stderr": "..."
}
```

Non-zero exit = collect failed. BOI will retry. If the provider cannot retain output (e.g., ephemeral containers), stdout/stderr may be empty strings — BOI logs a warning but doesn't fail.

**cancel** receives:
- `{job_id}`

Kills or stops the job. Exit code 0 = cancelled successfully. Non-zero = cancel failed (BOI logs but continues).

**cleanup** receives:
- `{job_id}`

Optional. Frees provider-side resources (removes container, deletes remote temp files, etc.). Called after collect. Non-fatal on failure.

### Template Variables

All commands support these substitutions:

| Variable | Value |
|----------|-------|
| `{spec_id}` | Queue ID (e.g., `q-42`) |
| `{spec_path}` | Absolute path to spec YAML |
| `{workspace}` | Workspace path (from workspace backend) |
| `{job_id}` | Job identifier (from spawn stdout) |
| `{task_timeout}` | Timeout in seconds (from config) |
| `{retry_count}` | Max retries (from config) |

### Environment Variables

BOI sets these environment variables for all provider commands:

| Variable | Value |
|----------|-------|
| `BOI_SPEC_ID` | Same as `{spec_id}` |
| `BOI_JOB_ID` | Same as `{job_id}` (not set for spawn) |
| `BOI_WORKSPACE` | Same as `{workspace}` |
| `BOI_QUEUE_PATH` | Path to SQLite database |
| `BOI_TASK_TIMEOUT` | Timeout in seconds |

This allows providers to be written as scripts that read env vars instead of parsing positional arguments.

## How the Daemon Changes

The current daemon loop is a tight coupling of pool management and spec orchestration. With pluggable providers, the loop splits into two responsibilities:

### Current (monolithic)

```
loop:
  1. Reap finished threads
  2. If capacity: dequeue spec
  3. Spawn thread → run_worker()
  4. Sleep 5s
```

### Proposed (provider-agnostic)

```
loop:
  1. Poll all active job_ids via provider.status()
  2. For completed/failed: provider.collect(), update SQLite, provider.cleanup()
  3. If capacity: dequeue spec
  4. workspace = backend.create(spec_id, source)
  5. job_id = provider.spawn(spec_id, spec_path, workspace)
  6. Record job_id → spec_id mapping
  7. Sleep 5s
```

The key change: **the daemon no longer runs the worker state machine directly**. For the `local` provider, the state machine still runs in a thread — but the daemon interacts with it through the same spawn/status/collect interface as remote providers.

### Job Tracking Table

Add a `jobs` table to SQLite:

```sql
CREATE TABLE jobs (
    job_id     TEXT PRIMARY KEY,
    spec_id    TEXT NOT NULL REFERENCES specs(id),
    provider   TEXT NOT NULL,
    status     TEXT NOT NULL DEFAULT 'running',
    spawned_at TEXT NOT NULL,
    updated_at TEXT,
    exit_code  INTEGER,
    error      TEXT
);
```

The daemon uses this table to track active jobs across restarts. On daemon startup, it queries `jobs WHERE status = 'running'` and re-polls their status via the provider.

## Built-in Providers

### `local` (default)

Current behavior. Spawns `std::thread` workers on the local machine.

```yaml
worker_pool:
  type: local
  max_workers: 5
```

The `local` provider is special: it doesn't use shell commands. It's implemented directly in Rust for zero-overhead. The spawn/status/collect interface is internal — the daemon calls Rust functions, not external processes.

Under the hood:
- **spawn:** `std::thread::spawn(|| run_worker(...))`, returns thread ID as job_id
- **status:** `JoinHandle::is_finished()`
- **collect:** Thread writes results to SQLite (same as today)
- **cancel:** Not cleanly supported for threads. BOI tracks the child `claude` PID in the `processes` table and sends SIGTERM. The thread detects the killed child and exits.

### `docker`

Runs each worker in a container.

```yaml
worker_pool:
  type: docker
  max_workers: 5
  image: "boi-worker:latest"
  network: "host"             # or "none" for isolation
  volumes:                    # additional mounts
    - "/var/run/docker.sock:/var/run/docker.sock"  # for docker-in-docker
  env:                        # additional env vars passed to container
    ANTHROPIC_API_KEY: "${ANTHROPIC_API_KEY}"
```

Equivalent to:

```yaml
worker_pool:
  type: custom
  max_workers: 5
  spawn: |
    docker run -d \
      --name boi-worker-{spec_id} \
      -v {workspace}:/workspace \
      -w /workspace \
      -e BOI_SPEC_ID={spec_id} \
      -e BOI_SPEC_PATH=/specs/{spec_id}.yaml \
      --network host \
      boi-worker:latest \
      claude -p "$(cat {spec_path})" --dangerously-skip-permissions
    docker inspect --format '{{.Id}}' boi-worker-{spec_id}
  status: |
    STATE=$(docker inspect --format '{{.State.Status}}' {job_id} 2>/dev/null)
    case "$STATE" in
      running) echo running ;;
      exited)
        CODE=$(docker inspect --format '{{.State.ExitCode}}' {job_id})
        [ "$CODE" = "0" ] && echo completed || echo failed ;;
      *) echo failed ;;
    esac
  collect: |
    CODE=$(docker inspect --format '{{.State.ExitCode}}' {job_id})
    STDOUT=$(docker logs {job_id} 2>/dev/null)
    STDERR=$(docker logs --stderr {job_id} 2>/dev/null)
    printf '{"exit_code":%d,"stdout":"%s","stderr":"%s"}' "$CODE" "$STDOUT" "$STDERR"
  cancel: "docker stop {job_id}"
  cleanup: "docker rm -f {job_id}"
```

Use cases: reproducible environments, untrusted specs, specs requiring specific toolchains or runtimes, sandboxing.

Trade-offs: container startup overhead (~1-3s). Docker for Mac has slower file I/O. The runtime (Claude) must be installed in the image. API keys must be forwarded to the container.

**Image requirements:** The worker image must have the runtime binary installed (e.g., `claude` CLI). BOI provides a sample Dockerfile:

```dockerfile
FROM ubuntu:22.04
RUN apt-get update && apt-get install -y curl git
RUN curl -fsSL https://claude.ai/install.sh | sh
WORKDIR /workspace
```

### `ssh`

Dispatches workers to remote machines.

```yaml
worker_pool:
  type: ssh
  max_workers: 10
  hosts:
    - host: "build1.internal"
      user: "deploy"
      key: "~/.ssh/id_ed25519"
      slots: 3                # max concurrent workers on this host
    - host: "build2.internal"
      user: "deploy"
      key: "~/.ssh/id_ed25519"
      slots: 5
    - host: "gpu-box.internal"
      user: "ml"
      slots: 2
      tags: ["gpu"]           # for spec-level host selection
```

Equivalent to (simplified, single-host):

```yaml
worker_pool:
  type: custom
  max_workers: 3
  spawn: |
    JOB_ID="boi-{spec_id}-$(date +%s)"
    ssh -i ~/.ssh/id_ed25519 deploy@build1.internal \
      "nohup claude -p '$(cat {spec_path})' --dangerously-skip-permissions \
       > /tmp/$JOB_ID.stdout 2>/tmp/$JOB_ID.stderr &
       echo \$! > /tmp/$JOB_ID.pid"
    echo "$JOB_ID"
  status: |
    PID=$(ssh deploy@build1.internal "cat /tmp/{job_id}.pid 2>/dev/null")
    if ssh deploy@build1.internal "kill -0 $PID 2>/dev/null"; then
      echo running
    elif [ -f /tmp/{job_id}.exit ]; then
      CODE=$(ssh deploy@build1.internal "cat /tmp/{job_id}.exit")
      [ "$CODE" = "0" ] && echo completed || echo failed
    else
      echo failed
    fi
  collect: |
    ssh deploy@build1.internal "cat /tmp/{job_id}.stdout" > /tmp/{job_id}.local.stdout
    ssh deploy@build1.internal "cat /tmp/{job_id}.stderr" > /tmp/{job_id}.local.stderr
    CODE=$(ssh deploy@build1.internal "cat /tmp/{job_id}.exit 2>/dev/null || echo 1")
    printf '{"exit_code":%s,"stdout":"","stderr":""}' "$CODE"
  cancel: |
    PID=$(ssh deploy@build1.internal "cat /tmp/{job_id}.pid 2>/dev/null")
    ssh deploy@build1.internal "kill $PID 2>/dev/null"
  cleanup: |
    ssh deploy@build1.internal "rm -f /tmp/{job_id}.*"
```

**Host selection logic:**

The `ssh` provider includes a built-in scheduler:
1. For each spawn, pick the host with the most available slots (slots - active_jobs)
2. If the spec has `tags: ["gpu"]`, filter to hosts with matching tags
3. If all hosts are full, the spawn returns non-zero and BOI retries on next poll

**Workspace transfer:**

The workspace backend creates a local directory. The ssh provider must transfer it:
1. **rsync on spawn:** `rsync -az {workspace}/ {user}@{host}:{remote_workspace}/` before starting the worker
2. **rsync on collect:** `rsync -az {user}@{host}:{remote_workspace}/ {workspace}/` to pull back changes
3. **Incremental sync:** For large repos, use `rsync --checksum` or `git push/pull`

This means the `ssh` provider handles workspace transport as part of spawn/collect. The workspace backend creates the local copy; the pool provider moves it to/from the remote host.

Trade-offs: network latency on status polls. Large workspace transfers. SSH key management. Connection drops leave orphaned remote processes. Need a way to detect and clean up stale jobs.

### `queue`

Submits jobs to an external job queue (AWS Batch, Kubernetes Job, Meta's internal TW, etc.).

```yaml
worker_pool:
  type: queue
  max_workers: 20
  submit: "aws batch submit-job --job-name boi-{spec_id} --job-queue boi-workers --job-definition boi-worker --parameters specPath={spec_path}"
  status: "aws batch describe-jobs --jobs {job_id} | jq -r '.jobs[0].status' | tr A-Z a-z"
  collect: "aws batch describe-jobs --jobs {job_id} | jq '{exit_code: .jobs[0].container.exitCode, stdout: \"\", stderr: .jobs[0].statusReason}'"
  cancel: "aws batch cancel-job --job-id {job_id} --reason 'cancelled by boi'"
```

The `queue` type is really just `custom` with a semantic name. It exists to signal intent: "this provider submits to an external queue system."

Use cases: enterprise environments with existing job infrastructure, scaling beyond a single machine, GPU-heavy workloads, compliance-controlled environments.

Trade-offs: queue latency (jobs may wait). External infrastructure dependency. Log retrieval varies by platform. Status mapping from queue-specific states to BOI's four states.

## How BOI Gets Results from Remote Workers

This is the central design question. Three approaches:

### Approach 1: Poll-based (recommended)

The daemon polls `provider.status(job_id)` on every tick (5s). When a job completes, it calls `provider.collect(job_id)` to get results.

```
daemon tick:
  for each active job_id:
    status = provider.status(job_id)
    if status == "completed" or "failed":
      result = provider.collect(job_id)
      update_sqlite(spec_id, result)
      provider.cleanup(job_id)
```

Pros: Simple. Stateless. Works with any provider. No callback infrastructure needed.

Cons: 5s latency between completion and detection. N status commands per tick (one per active job).

This is the recommended approach because it mirrors the current implementation (the daemon already polls `JoinHandle::is_finished()` on each tick) and requires no additional infrastructure.

### Approach 2: Callback-based

The provider calls a BOI endpoint when a job completes. BOI exposes a webhook or writes to a known path.

```yaml
worker_pool:
  type: custom
  callback_mode: true
  callback_command: "boi callback {job_id} {status}"
```

Pros: Instant notification. Zero polling overhead.

Cons: Requires BOI to expose an endpoint (HTTP server or filesystem watcher). Adds complexity. Provider must be able to reach back to BOI (network, filesystem). Failure to deliver callback = silent job loss.

Not recommended for v1. Could be added later as an optimization for high-worker-count deployments.

### Approach 3: Shared database

Remote workers write directly to BOI's SQLite database (via network mount or a shared Postgres).

Pros: Real-time status. Workers can update task-level progress, not just spec-level.

Cons: SQLite doesn't support concurrent writers well. Network-mounted SQLite is fragile. Requires a shared database, which defeats the simplicity goal.

Not recommended. The provider interface is the right abstraction boundary.

## How BOI Monitors Remote Worker Health

### Heartbeat Protocol

For providers that support it, BOI can check liveness:

```yaml
worker_pool:
  type: ssh
  heartbeat_command: "ssh {user}@{host} 'test -f /tmp/{job_id}.heartbeat && [ $(( $(date +%s) - $(stat -f %m /tmp/{job_id}.heartbeat) )) -lt 120 ]'"
  heartbeat_interval_secs: 60
```

If the heartbeat check fails for `stale_timeout_secs` (default: 300), BOI marks the job as failed and fires `on_stall`.

For providers without heartbeat support, BOI relies on `task_timeout_secs`. If a job has been `running` for longer than the timeout, BOI calls `provider.cancel(job_id)` and marks it failed.

### Recovery on Daemon Restart

On startup, the daemon:
1. Queries `jobs WHERE status = 'running'`
2. For each, calls `provider.status(job_id)`
3. If the provider returns `completed` or `failed`, collects results
4. If the provider returns `running`, continues monitoring
5. If the provider returns an error (job unknown), marks as failed and requeues the spec

This handles the case where the daemon crashed while jobs were in-flight.

## How BOI Handles Remote Worker Failures

| Failure Mode | Detection | Recovery |
|-------------|-----------|----------|
| Worker process crash | `provider.status()` returns `failed` | Collect partial output, requeue spec |
| Network disconnect | `provider.status()` returns error | Retry status check. After N failures, mark failed |
| Remote machine down | SSH connection refused | Mark all jobs on that host as failed, redistribute |
| Timeout | Running time > `task_timeout_secs` | `provider.cancel()`, mark failed, requeue if retries remain |
| Daemon crash | Jobs table has `running` entries on startup | Re-poll status via provider, resume or recover |
| Provider script error | Non-zero exit from spawn/status | Log error, retry on next tick |

### Retry Policy

Provider failures (non-zero exit from status/collect) are transient by default. BOI retries up to `provider_retry_count` (default: 3) before marking the job as failed. This handles flaky networks without masking real failures.

```yaml
worker_pool:
  type: ssh
  provider_retry_count: 5    # more retries for flaky networks
  provider_retry_delay_secs: 10
```

## Per-Spec Provider Selection

Specs can override the global pool:

```yaml
title: "Heavy ML training"
worker_pool: gpu-cluster       # named pool from config
tasks:
  - id: t-1
    title: "Train model"
    spec: "Run training pipeline"
    verify: "test -f model.pt"
```

Or with inline config:

```yaml
title: "Run on build server"
worker_pool:
  type: ssh
  hosts:
    - host: "build-server"
      user: "deploy"
      slots: 1
tasks:
  - id: t-1
    title: "Heavy build"
    spec: "Compile with optimizations"
    verify: "test -f target/release/binary"
```

Resolution order:
1. Spec-level `worker_pool` (inline object or named profile)
2. Named profile (if spec has `profile: meta-internal`)
3. Global `worker_pool` in `config.yaml`
4. Default: `local` with `max_workers: 5`

### Named Pools

Config can define named pools:

```yaml
pools:
  local:
    type: local
    max_workers: 5

  gpu-cluster:
    type: ssh
    max_workers: 6
    hosts:
      - host: "gpu1"
        slots: 2
        tags: ["gpu", "a100"]
      - host: "gpu2"
        slots: 2
        tags: ["gpu", "a100"]
      - host: "gpu3"
        slots: 2
        tags: ["gpu", "h100"]

  ci:
    type: docker
    max_workers: 10
    image: "ci-runner:latest"
```

Named pools allow specs to select a pool by name without repeating the full config.

## Changes to the Spec Format

Current:

```yaml
# No worker_pool field exists
```

Proposed additions:

```yaml
worker_pool: gpu-cluster           # short form: named pool
worker_pool:                       # long form: inline config
  type: ssh
  hosts: [...]
```

The `max_workers` field at the spec level is intentionally absent. The pool's `max_workers` is a pool-wide limit, not a per-spec limit. A spec runs on one worker; the pool limits how many specs run concurrently.

## Interaction with Workspace Backends

The worker pool provider and workspace backend are independent but interact at two points:

### 1. Workspace Path Handoff

The daemon creates the workspace first, then passes the path to the pool provider:

```
1. workspace_path = workspace_backend.create(spec_id, source)
2. job_id = worker_pool.spawn(spec_id, spec_path, workspace_path)
```

For local pools, the workspace path is directly usable. For remote pools, the provider must transfer the workspace to the remote machine (rsync, docker volume mount, etc.).

### 2. Workspace Cleanup Ordering

After a job completes:

```
1. worker_pool.collect(job_id)      # get results
2. worker_pool.cleanup(job_id)      # free provider resources
3. workspace_backend.cleanup(spec_id)  # remove workspace
```

For remote providers, collect should also pull back any workspace changes (if the spec needs merge-back).

### Compatibility Matrix

Not every workspace + pool combination makes sense:

| Pool \ Workspace | git | directory | docker | ssh | none |
|-----------------|-----|-----------|--------|-----|------|
| **local** | native | native | via exec | via ssh exec | native |
| **docker** | mount | mount | nested (complex) | n/a | mount |
| **ssh** | rsync + remote git | rsync | remote docker | remote ssh (hop) | rsync |
| **queue** | depends on queue | depends on queue | native (k8s) | n/a | depends on queue |

The recommended pairings:
- `local` + `git` — today's default
- `docker` + `git` or `directory` — sandboxed execution
- `ssh` + `git` — remote machines with git repos
- `queue` + `docker` — cloud-scale via container orchestration

## Implementation Plan

### Phase 1: Extract the Trait

Refactor the daemon loop to separate pool management from spec orchestration:

```rust
pub trait WorkerPoolProvider: Send + Sync {
    fn spawn(
        &self,
        spec_id: &str,
        spec_path: &str,
        workspace: &str,
        config: &WorkerConfig,
    ) -> Result<String>;  // returns job_id

    fn status(&self, job_id: &str) -> Result<JobStatus>;

    fn collect(&self, job_id: &str) -> Result<JobResult>;

    fn cancel(&self, job_id: &str) -> Result<()>;

    fn cleanup(&self, job_id: &str) -> Result<()> {
        let _ = job_id;
        Ok(())  // default no-op
    }

    fn max_workers(&self) -> u32;
}

pub enum JobStatus {
    Running,
    Completed,
    Failed,
    Timeout,
    Unknown,
}

pub struct JobResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}
```

Current `run_daemon()` becomes `LocalPoolProvider`:

```rust
pub struct LocalPoolProvider {
    max_workers: u32,
    active: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl WorkerPoolProvider for LocalPoolProvider {
    fn spawn(&self, spec_id: &str, ...) -> Result<String> {
        let handle = thread::spawn(move || run_worker(...));
        let job_id = format!("local-{}-{}", spec_id, thread_id);
        self.active.lock().unwrap().insert(job_id.clone(), handle);
        Ok(job_id)
    }

    fn status(&self, job_id: &str) -> Result<JobStatus> {
        match self.active.lock().unwrap().get(job_id) {
            Some(h) if h.is_finished() => Ok(JobStatus::Completed),
            Some(_) => Ok(JobStatus::Running),
            None => Ok(JobStatus::Unknown),
        }
    }
}
```

### Phase 2: Add `CustomPoolProvider`

Implements the trait by shelling out to configured commands:

```rust
pub struct CustomPoolProvider {
    spawn_cmd: String,
    status_cmd: String,
    collect_cmd: String,
    cancel_cmd: String,
    cleanup_cmd: Option<String>,
    max_workers: u32,
}

impl WorkerPoolProvider for CustomPoolProvider {
    fn spawn(&self, spec_id: &str, spec_path: &str, workspace: &str, _config: &WorkerConfig) -> Result<String> {
        let cmd = self.spawn_cmd
            .replace("{spec_id}", spec_id)
            .replace("{spec_path}", spec_path)
            .replace("{workspace}", workspace);
        let output = Command::new("sh").args(["-c", &cmd]).output()?;
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    fn status(&self, job_id: &str) -> Result<JobStatus> {
        let cmd = self.status_cmd.replace("{job_id}", job_id);
        let output = Command::new("sh").args(["-c", &cmd]).output()?;
        let status_str = String::from_utf8(output.stdout)?.trim().to_lowercase();
        match status_str.as_str() {
            "running" => Ok(JobStatus::Running),
            "completed" => Ok(JobStatus::Completed),
            "failed" => Ok(JobStatus::Failed),
            "timeout" => Ok(JobStatus::Timeout),
            _ => Ok(JobStatus::Unknown),
        }
    }
}
```

### Phase 3: Refactor Daemon Loop

Replace the monolithic `run_daemon()` with a provider-agnostic loop:

```rust
pub fn run_daemon(queue_path: &str, provider: &dyn WorkerPoolProvider, ...) {
    let mut active_jobs: HashMap<String, String> = HashMap::new(); // job_id → spec_id

    loop {
        // 1. Poll active jobs
        let completed: Vec<String> = active_jobs.iter()
            .filter(|(job_id, _)| matches!(provider.status(job_id), Ok(JobStatus::Completed | JobStatus::Failed)))
            .map(|(job_id, _)| job_id.clone())
            .collect();

        for job_id in completed {
            let spec_id = active_jobs.remove(&job_id).unwrap();
            let result = provider.collect(&job_id)?;
            // update SQLite
            provider.cleanup(&job_id)?;
        }

        // 2. Spawn new jobs
        while active_jobs.len() < provider.max_workers() as usize {
            match queue.dequeue()? {
                Some(rec) => {
                    let workspace = backend.create(&rec.id, &rec.workspace)?;
                    let job_id = provider.spawn(&rec.id, &rec.spec_path, &workspace, &config)?;
                    active_jobs.insert(job_id, rec.id);
                }
                None => break,
            }
        }

        thread::sleep(Duration::from_secs(5));
    }
}
```

### Phase 4: Config Parsing + Provider Resolution

Add `worker_pool` to `Config` and `BoiSpec`. Resolution logic:
1. Parse spec-level override (inline or named pool reference)
2. Look up named pool in `config.yaml`
3. Fall back to config-level default
4. Fall back to `LocalPoolProvider`

### Phase 5: Built-in Providers

Add `DockerPoolProvider` and `SshPoolProvider` as built-in types. Each wraps `CustomPoolProvider` with provider-specific defaults and validation.

## Open Questions

1. **Worker state machine location.** Today the worker state machine (`WorkerState` enum) runs inside `run_worker()` on the local machine. For remote providers, does the entire state machine run remotely? Or does BOI orchestrate task-by-task, calling spawn once per task? Recommendation: the entire state machine runs remotely. BOI spawns one job per spec, and the remote worker handles all tasks. This keeps the provider interface simple (spawn/status/collect) and avoids N round-trips for N tasks.

2. **Task-level progress from remote workers.** The local provider updates SQLite after each task. Remote providers can only report spec-level status (running/completed/failed). Should the provider interface support task-level updates? Recommendation: not in v1. The `collect` output can include task-level detail, but the provider interface stays spec-level. Task progress is a runtime concern, not a pool concern.

3. **Credential forwarding.** Remote workers need API keys (e.g., `ANTHROPIC_API_KEY`). How are credentials distributed? Options: (a) env vars in the provider config, (b) a secrets manager the provider reads from, (c) SSH agent forwarding. Recommendation: env vars in config for v1, with a note that production deployments should use a secrets manager.

4. **Multi-pool daemon.** Can the daemon manage multiple pools simultaneously (e.g., local + SSH)? The per-spec pool selection implies yes. The daemon would need one active-jobs tracker per pool, and the dequeue logic would need to consider which pool to use for each spec. This adds complexity but is essential for the profile system.

5. **Provider liveness.** How does BOI verify the provider itself is healthy (not just individual jobs)? A `docker` provider might fail if Docker isn't running. An `ssh` provider might fail if hosts are unreachable. Recommendation: add an optional `health` command to the provider interface. `boi doctor` calls it. Not required for job dispatch.

6. **Log streaming.** Today BOI captures the full stdout/stderr after completion. For long-running remote jobs, streaming logs would be valuable. This could be an optional `logs` command: `provider logs {job_id} [--follow]`. Not required for v1 but worth designing the interface for.
