# BOI Daemon

## Overview

The BOI daemon is a long-running process that monitors the queue and dispatches workers for pending specs. It is started with `boi daemon start` (background) or `boi daemon foreground` (attached to the terminal).

## Tick Cadence

The daemon polls every ~5 seconds (10 × 500 ms sleep increments). Each tick:

1. Writes a heartbeat timestamp to `~/.boi/daemon.heartbeat`.
2. Checks the SIGHUP reload flag and applies config changes if set.
3. Reaps finished worker threads.
4. Computes how many new workers to spawn this tick and drains the queue up to that cap.
5. Sleeps 500 ms × 10 before the next tick (interruptible by SIGTERM).

## Batched Dequeue (`spawns_per_tick`)

Rather than spawning one worker per tick, the daemon drains up to `spawns_per_tick` eligible specs per tick (default 4). The actual number spawned is:

```
to_spawn = min(max_workers - current_workers, spawns_per_tick)
```

A 50–150 ms randomized jitter is inserted between successive spawns within a single tick to smooth cold-start bursts on the Anthropic API. Configure `spawns_per_tick` in `~/.boi/config.yaml`:

```yaml
spawns_per_tick: 4   # default; raise once cold-start behavior is validated
```

## SIGHUP Config Hot-Reload

Sending SIGHUP to the daemon triggers a live config reload **without restarting** or interrupting in-flight workers.

### What reloads

| Setting | Reloaded? |
|---------|-----------|
| `max_workers` | Yes |
| `spawns_per_tick` | Yes |
| `claude_bin` | Yes |
| `task_timeout_minutes` | No — startup snapshot |
| `retry_count` | No — startup snapshot |
| `cleanup_on_failure` | No — startup snapshot |
| `paths.*` | No — startup snapshot |

### Reload semantics

- **Parse failure is a no-op.** If the config file is syntactically invalid, the daemon logs `[boi daemon] reload FAILED: ...; keeping current config` and retains the current values.
- **In-flight workers are unaffected.** Workers receive a snapshot of `WorkerConfig` at spawn time; live config mutation never reaches them.
- **No restart required.** The daemon process continues running; only the three live fields are updated.

### Triggering a reload

```bash
# Recommended: set a value then reload in one step
boi config set max_workers 10
boi daemon reload

# Or send SIGHUP directly
kill -HUP $(cat ~/.boi/daemon.lock)
```

`boi daemon reload` reads the PID from `~/.boi/daemon.lock`, verifies the process is alive, and sends SIGHUP. The reload takes effect within the next tick (≤ 5 seconds).

## Daemon Commands

| Command | Description |
|---------|-------------|
| `boi daemon start` | Start daemon in the background |
| `boi daemon stop` | Send SIGTERM; waits up to 10s, then SIGKILL |
| `boi daemon restart` | Stop + start |
| `boi daemon foreground` | Run attached to the terminal |
| `boi daemon reload` | Send SIGHUP to reload `max_workers`, `spawns_per_tick`, `claude_bin` |

## PID and Lock File

The daemon uses an exclusive `flock` on `~/.boi/daemon.lock` (which also stores the PID) as its singleton guard. This is crash-safe: the lock auto-releases when the process exits, so stale PID files can never block a restart.
