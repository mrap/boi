# prune-orphans First Run — 2026-05-04

## Run Summary

```
$ ~/github.com/mrap/boi/target/release/boi prune-orphans --dry-run --force
BOI prune-orphans [dry-run]  max-idle=600s

No orphan candidates found.
```

**Candidates found**: 0
**Mode**: dry-run (--force required because DB has 0 active workers and 0 active processes)
**Binary**: `~/github.com/mrap/boi/target/release/boi` (release build May 4 00:18)
**DB**: `~/.boi/boi-rust.db` (324 specs, 0 worker records, 0 process records)

---

## Manual Audit — Known Orphan Candidates

Using `ps -A -ww -o pid=,ppid=,pcpu=,rss=,etime=,tty=,args=` with correct whitespace-aware parsing, the following processes were found that should have been flagged:

| PID   | PPID  | ALIVE       | CPU% | Description | Expected Heuristic |
|-------|-------|-------------|------|-------------|-------------------|
| 64717 | 1     | 11d 3h      | 0.0  | Dead zsh shell-snapshot wrapper (status polling loop, long dead spec) | H4: LongIdle |
| 29167 | 1     | 4d 3h       | 0.0  | Dead zsh shell-snapshot wrapper | H4: LongIdle |
| 34400 | 1     | 4d          | 0.0  | Dead zsh shell-snapshot wrapper | H4: LongIdle |
| 98108 | 1     | 6d 6h       | 0.0  | Dead zsh shell (abandoned setup-token run, 6 days ago) | H4: LongIdle |
| 7007  | 1     | 11h 43m     | 0.0  | zsh polling `until boi status` for a long-finished spec | H4: LongIdle |
| 9122  | 58516 | 4h 1m       | 0.0  | zsh polling `until sqlite3` for spec S8C03 | H4: LongIdle |
| 9397  | 58516 | 4h 1m       | 11%  | Tight busy-wait `until sqlite3` — actively burning CPU | H4: LongIdle (alive_secs), or H5 if parent dies |

PID 9397 is notable: 11% CPU sustained for 4+ hours in a `until sqlite3 do true; done` loop — a hot busy-wait polling for a spec that has likely completed.

---

## Comparison to Manual Audit

The dry-run found **0 of 7** genuine orphan candidates. **All 7 are false negatives.**

---

## Root Cause Analysis

### Bug 1: `splitn(7, ' ')` cannot parse multi-space-separated `ps` output

`collect_system_processes` (src/cli/prune.rs:314) uses:

```rust
let mut parts = line.splitn(7, ' ').map(str::trim);
```

`ps` right-justifies numeric fields in fixed-width columns. On macOS:
- PIDs < 10000 (4 digits) get a **leading space** in the output
- PIDs ≥ 10000 (5 digits) have **multiple spaces** between fields

**Effect on 4-digit PIDs (leading space):** `splitn` produces an empty first token. `"".parse::<u32>()` fails → `None` → `continue` → **line silently skipped**. 277 of 1275 ps lines have leading spaces and are completely invisible to the scanner.

**Effect on 5-digit PIDs (multiple spaces between fields):** `splitn(7, ' ')` consumes the first 6 splits on the empty strings between spaces, leaving all remaining content (ppid, cpu, rss, etime, tty, args) concatenated as the 7th "cmdline" token:

```
Input:  "42069     1   0.0   2064 05-00:43:00 ??       /usr/bin/claude ..."
Parsed: pid=42069  ppid=0  cpu=0.0  rss=0  etime=""  tty=""
        cmdline="1   0.0   2064 05-00:43:00 ??       /usr/bin/claude ..."
```

All numeric fields default to 0. Crucially, **`alive_secs` is always 0** (parse_etime("") returns 0), which means:
- **Heuristic 4** (`cpu < 0.01 && alive_secs >= 600`) never fires — alive_secs is always 0
- **Heuristic 5** (`parent_dead && alive_secs >= 600`) never fires — same

Only heuristics 2 and 3 could work, but:
- **Heuristic 2** (DB ended_at set) requires `processes` table rows — the table is empty
- **Heuristic 3** (inactive worktree CWD) requires the process CWD to be a BOI worktree path, which none of the above orphans have

**Net effect**: zero candidates detected regardless of the actual process state.

### Bug 2: `is_boi_worktree_path` misses the actual macOS `$TMPDIR` path

`is_boi_worktree_path` (src/cli/prune.rs:122) checks for `/tmp/` or `/private/tmp/` but on macOS, `$TMPDIR` resolves to `/private/var/folders/z7/<hash>/T/`. BOI worktrees are created there:

```
/private/var/folders/z7/dh5d06ps11n8cr2xj1b46kbh0000gp/T/boi-SFB36-boi-rust
```

This path does not contain `/tmp/`, so heuristic 3 (inactive worktree CWD) never fires for any native-macOS BOI worker process.

Confirmed via `lsof -d cwd`: several processes with GONE BOI worktree CWDs were invisible to the scanner:

| PID   | Command | PPID | Age  | Worktree | Dir status |
|-------|---------|------|------|----------|------------|
| 1748  | `head -5` | 1  | ~44m | boi-SFB36-boi-rust | GONE |
| 34400 | `/bin/zsh` (polling loop) | 1 | 4d | boi-S72F8-boi-rust | GONE |
| 42812 | `fly agent run ...` | 1 | 3d21h | boi-S0723-boi-rust | GONE |

PIDs 1748 and 34400 are genuine orphans; PID 42812 is an external Fly.io daemon (false positive risk if the path gap is fixed — see False Positives section).

**Recommended fix:**

```rust
pub fn is_boi_worktree_path(path: &str) -> bool {
    let has_tmp = path.contains("/tmp/")
        || path.contains("/private/tmp/")
        || path.contains("/var/folders/");  // macOS $TMPDIR
    has_tmp && path.contains("boi-") && path.contains("-boi-rust")
}
```

### Bug 3: `processes` table is empty — heuristic 2 can never fire

The DB has 324 spec records but 0 rows in `processes`. Either the current daemon version doesn't write process rows, or the table was cleared. Until this is populated, DB-marked-ended (heuristic 2) is permanently inert.

---

## False Positives Found

If the parsing bug is fixed, the following legitimate processes would be incorrectly flagged:

| PID   | PPID  | ALIVE  | Reason it would be flagged | Why it's safe |
|-------|-------|--------|---------------------------|---------------|
| 50653 | 6062  | 29m    | H4: 0% CPU, alive > 600s  | Child of active worker 6062 (`claude -p` BOI Doc-Update Worker) |
| 63048 | 22964 | 9m     | Under 600s now, but borderline | Child of active worker 22964 |
| 72277 | 52741 | 27m    | H4: 0% CPU, alive > 600s  | Child of active worker 52741 |
| 78182 | 52539 | 25m    | H4: 0% CPU, alive > 600s  | Child of active worker 52539 |
| 71476 | 58453 | 4h 13m | H4: 0% CPU, alive > 600s  | Child of active worker 58453 |

These are `claude -p` subprocess shell-snapshots — the `/bin/zsh -c source ...` wrappers that claude spawns to run tools. They legitimately idle for long periods between tool calls.

**Additional false positive risk after Bug 2 fix (worktree path):**

| PID   | PPID | ALIVE  | Command | Why it's a false positive |
|-------|------|--------|---------|---------------------------|
| 42812 | 1    | 3d21h  | `fly agent run ...` (Fly.io CLI daemon) | External daemon; CWD happened to be inside a GONE BOI worktree; not BOI-related |

To prevent this: extend `DAEMON_SAFELIST` with known external daemon patterns (e.g., `"fly agent"`, `"ngrok"`) or add a safelist check by binary path prefix.

**Heuristic to add**: Transitively protect children (and grandchildren) of processes in `workers.current_pid`. A process whose PPID or PPID chain leads to an active worker should never be pruned.

---

## Recommended Fixes

### Priority 1 — Fix ps parsing (blocks all heuristics)

Replace:
```rust
let mut parts = line.splitn(7, ' ').map(str::trim);
```

With a whitespace-aware split that handles fixed-width ps columns correctly. One approach — split on whitespace for the first 6 fields, then take the rest of the original line as args:

```rust
// Split the first 6 whitespace-delimited fields; args = remainder of line
let mut field_iter = line.split_ascii_whitespace();
let pid: u32  = field_iter.next().and_then(|s| s.parse().ok())?;
let ppid: u32 = field_iter.next().and_then(|s| s.parse().ok()).unwrap_or(0);
let cpu: f64  = field_iter.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
let rss: u64  = field_iter.next().and_then(|s| s.parse().ok()).unwrap_or(0);
let etime_str = field_iter.next().unwrap_or("0:00");
let tty_str   = field_iter.next().unwrap_or("??");
// args = everything in the line after advancing past the 6 fields
// reconstruct by byte-offset or collect remaining tokens joined by spaces
let cmdline: String = field_iter.collect::<Vec<_>>().join(" ");
```

Note: This loses embedded spaces in the args string (the `-ww` flag causes `ps` to not wrap, but spaces within args remain). A more robust approach is to find the byte offset of the 7th token in the original line:

```rust
let cmdline = {
    let mut n = 0u32;
    let mut start = 0;
    let bytes = line.as_bytes();
    let mut in_ws = true;
    for (i, &b) in bytes.iter().enumerate() {
        let ws = b == b' ' || b == b'\t';
        if in_ws && !ws { n += 1; if n == 7 { start = i; break; } }
        in_ws = ws;
    }
    line[start..].to_string()
};
```

### Priority 1b — Fix `is_boi_worktree_path` to cover macOS `$TMPDIR`

One-line fix in `src/cli/prune.rs` (see Bug 2 section above). Add `/var/folders/` as a third recognized temp prefix. This is required for heuristic 3 (inactive worktree CWD) to ever fire on macOS.

### Priority 2 — Transitively protect worker subprocesses

In `classify_candidate`, after checking `worker_pids`, also check if the process's PPID is a worker PID (or its PPID chain leads to a worker). The `find_orphan_candidates` function can pre-build a set of "worker-descendent" PIDs before classifying:

```rust
// Build transitive child set of all worker PIDs
fn build_worker_descendent_set(all_procs: &[ProcessInfo], worker_pids: &HashSet<u32>) -> HashSet<u32> {
    let mut protected = worker_pids.clone();
    let mut changed = true;
    while changed {
        changed = false;
        for p in all_procs {
            if !protected.contains(&p.pid) && protected.contains(&p.ppid) {
                protected.insert(p.pid);
                changed = true;
            }
        }
    }
    protected
}
```

### Priority 3 — Start writing to `processes` table

Once the daemon creates rows in the `processes` table, heuristic 2 (DB-marked-ended) becomes useful. Track `INSERT INTO processes (pid, spec_id, ...)` when a worker starts and `UPDATE processes SET ended_at = ? WHERE pid = ?` on exit.

---

## Next Steps

1. Fix `collect_system_processes` parsing bug (Priority 1 above) — tracked as a new task
2. Add transitive worker-child protection (Priority 2) — prevents false positives post-fix
3. Verify processes table gets populated — without it, heuristic 2 is dead code

After fixes, re-run: `~/github.com/mrap/boi/target/release/boi prune-orphans --dry-run` (no --force needed once DB has active worker records).
