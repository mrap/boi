# Workspace Backends

Pluggable workspace isolation for BOI. The current git worktree backend becomes one of several options.

## What BOI Needs from a Workspace

A workspace is a **directory** where a worker can read and write files in isolation. BOI's contract with a workspace backend is exactly four operations:

| Operation | Input | Output | Required |
|-----------|-------|--------|----------|
| **create** | spec_id, source (repo/path) | absolute path to isolated directory | yes |
| **exec** | workspace path, command | exit code + stdout/stderr | yes |
| **merge** | workspace path, target | success/failure | no (opt-in) |
| **cleanup** | spec_id | (none) | yes |

**Invariants every backend must satisfy:**

1. **Isolation.** Two concurrent specs with different spec_ids get non-overlapping directories. A write in one workspace never appears in another.
2. **Idempotent create.** Calling create twice with the same spec_id returns the same path without error.
3. **Best-effort cleanup.** Cleanup must not fail fatally. If the directory is already gone, that's fine.
4. **Exec runs in-directory.** The command's working directory is the workspace root.

## Interface: Command-Based Provider

A workspace backend is a set of shell commands. BOI calls them via `sh -c`. The backend does not need to be compiled into BOI.

### Config Schema

In `~/.boi/config.yaml`:

```yaml
workspace_backend:
  type: git                          # built-in type name
```

Or for custom backends:

```yaml
workspace_backend:
  type: custom
  create: "/usr/local/bin/my-backend create {spec_id} {source}"
  cleanup: "/usr/local/bin/my-backend cleanup {spec_id}"
  exec: "/usr/local/bin/my-backend exec {spec_id} {cmd}"
  merge: "/usr/local/bin/my-backend merge {spec_id} {target}"
```

### Command Contract

**create** receives:
- `{spec_id}` — unique identifier (e.g., `q-42`)
- `{source}` — the `workspace:` field from the spec (typically a repo path)

Must print exactly one line to stdout: the absolute path to the workspace directory. Non-zero exit = failure.

**cleanup** receives:
- `{spec_id}`

Silent success on non-zero exit (best-effort). BOI logs stderr but doesn't fail the spec.

**exec** receives:
- `{spec_id}`
- `{cmd}` — the shell command to execute inside the workspace

Runs `{cmd}` with cwd set to the workspace. Returns the command's exit code. If not specified, BOI defaults to `cd {workspace_path} && {cmd}`.

**merge** receives:
- `{spec_id}`
- `{target}` — where to merge changes back (branch name, path, etc.)

Optional. Only called if the spec declares `merge_back: true`. Non-zero exit = merge failed (BOI reports but doesn't retry).

### Template Variables

All commands support these substitutions:

| Variable | Value |
|----------|-------|
| `{spec_id}` | Queue ID (e.g., `q-42`) |
| `{source}` | Spec's `workspace:` field |
| `{workspace}` | Absolute path returned by create |
| `{cmd}` | Command to execute (for exec) |
| `{target}` | Merge target (for merge) |

## Built-in Backends

### `git` (default)

Current behavior. Creates a detached git worktree.

```yaml
workspace_backend:
  type: git
```

Equivalent to:

```yaml
workspace_backend:
  type: custom
  create: "git -C {source} worktree add --detach ~/.boi/worktrees/{spec_id}"
  cleanup: "git worktree remove --force ~/.boi/worktrees/{spec_id}"
```

How create works: runs `git worktree add --detach` in the repo specified by `workspace:`. Returns `~/.boi/worktrees/{spec_id}`. Idempotent — returns existing path if already created.

How cleanup works: `git worktree remove --force`, falls back to `rm -rf` if git command fails.

Merge-back: not automatic. Workers can commit and push within the worktree. A future `merge` command could run `git -C {workspace} push origin HEAD:refs/boi/{spec_id}`.

Stale detection: `cleanup_stale()` prunes git's internal list and removes orphaned directories (those without a `.git` pointer file).

### `directory`

Copies the source directory. No VCS required.

```yaml
workspace_backend:
  type: directory
```

Equivalent to:

```yaml
workspace_backend:
  type: custom
  create: "cp -R {source} ~/.boi/worktrees/{spec_id}"
  cleanup: "rm -rf ~/.boi/worktrees/{spec_id}"
```

Use cases: non-git repos, Mercurial repos where you just want a copy, document-generation specs where the "source" is a template directory.

Trade-offs: full copy is slow for large repos. No merge-back unless the provider implements it. No deduplication.

### `docker`

Runs the worker inside a container with the workspace mounted as a volume.

```yaml
workspace_backend:
  type: docker
  image: "ubuntu:22.04"
  mount_source: true     # mount {source} as /workspace (read-only)
  copy_source: false     # or copy files into the container
```

Equivalent to:

```yaml
workspace_backend:
  type: custom
  create: |
    docker create --name boi-{spec_id} -v {source}:/source:ro -w /workspace {image} sleep infinity
    docker start boi-{spec_id}
    docker exec boi-{spec_id} cp -R /source/. /workspace/
    echo /workspace
  exec: "docker exec -w /workspace boi-{spec_id} sh -c '{cmd}'"
  cleanup: "docker rm -f boi-{spec_id}"
  merge: "docker cp boi-{spec_id}:/workspace/. {target}"
```

Use cases: untrusted spec execution, reproducible environments, specs that need specific toolchains installed.

Trade-offs: container startup overhead (~1-3s). File I/O slower on Docker for Mac. Network-isolated by default (which might be desired or not). The runtime (Claude) must also run inside or be able to reach into the container.

Open question: when workspace is `docker`, does BOI also run the *runtime* (Claude) inside the container? Or does Claude run on the host and `exec` commands go into the container? The latter is simpler and matches how BOI currently works — Claude generates commands, BOI runs them. With Docker backend, BOI runs them inside the container.

### `ssh`

Creates a workspace on a remote machine.

```yaml
workspace_backend:
  type: ssh
  host: "build-server.internal"
  user: "deploy"
  key: "~/.ssh/id_ed25519"
  remote_base: "/tmp/boi-workspaces"
```

Equivalent to:

```yaml
workspace_backend:
  type: custom
  create: |
    ssh -i {key} {user}@{host} "mkdir -p {remote_base}/{spec_id} && rsync -a {source}/ {remote_base}/{spec_id}/"
    echo "{remote_base}/{spec_id}"
  exec: "ssh -i {key} {user}@{host} 'cd {remote_base}/{spec_id} && {cmd}'"
  cleanup: "ssh -i {key} {user}@{host} 'rm -rf {remote_base}/{spec_id}'"
  merge: "rsync -a {user}@{host}:{remote_base}/{spec_id}/ {target}/"
```

Use cases: offloading builds to beefy machines, running specs on Linux from a Mac, GPU-dependent tasks.

Trade-offs: network latency on every exec. Source must be transferred to the remote. Changes must be transferred back. SSH keys must be configured. Connection drops mid-spec leave orphaned directories.

### `none`

No isolation. Runs in-place in the source directory.

```yaml
workspace_backend:
  type: none
```

Equivalent to:

```yaml
workspace_backend:
  type: custom
  create: "echo {source}"
  cleanup: "true"
```

Use cases: read-only specs (analysis, report generation), specs that operate on global state (config updates), specs where isolation would break the task (needs to see other workers' output).

Trade-offs: no isolation at all. Concurrent specs touching the same files will conflict. No cleanup — changes persist. Use sparingly.

### `sapling` / `hg`

Mercurial or Sapling (Meta's VCS) workspace.

```yaml
workspace_backend:
  type: sapling
```

Equivalent to:

```yaml
workspace_backend:
  type: custom
  create: |
    cd {source}
    sl bookmark boi-{spec_id}
    sl checkout boi-{spec_id}
    echo {source}
  cleanup: |
    cd {source}
    sl checkout main
    sl bookmark -d boi-{spec_id}
  merge: |
    cd {source}
    sl checkout main
    sl rebase -s boi-{spec_id} -d main
```

Note: Sapling doesn't use worktrees the same way git does. Isolation via bookmarks means you can't have two concurrent specs in the same repo (they'd see each other's working copy). For true isolation, combine with `directory` backend (copy the repo, then bookmark within the copy).

A more robust Sapling backend would use `sl clone --shallow` to create a lightweight clone per spec. This is a real implementation detail that a `sapling` built-in would handle.

## Per-Spec Backend Override

Specs can override the global backend:

```yaml
title: "Run ML training"
workspace: /path/to/ml-repo
workspace_backend: docker        # use docker for this spec only

tasks:
  - id: t-1
    title: "Train model"
    spec: "Run training pipeline"
    verify: "test -f model.pt"
```

Or with inline config:

```yaml
title: "Run on build server"
workspace: /path/to/repo
workspace_backend:
  type: ssh
  host: "gpu-box"
  user: "deploy"
  remote_base: "/data/boi"

tasks:
  - id: t-1
    title: "Heavy build"
    spec: "Compile with optimizations"
    verify: "test -f target/release/binary"
```

Resolution order:
1. Spec-level `workspace_backend` (inline object or type name)
2. Named profile (if spec has `profile: meta-internal`)
3. Global `workspace_backend` in `config.yaml`
4. Default: `git` if `workspace:` is a git repo, `directory` otherwise

## Changes to the Spec Format

Current:

```yaml
workspace: /path/to/repo   # optional, string
```

Proposed additions:

```yaml
workspace: /path/to/repo              # source path (unchanged)
workspace_backend: docker              # short form: built-in type name
workspace_backend:                     # long form: inline config
  type: custom
  create: "..."
  cleanup: "..."
merge_back: true                       # trigger merge operation after success
```

The `workspace` field remains a simple path — the *source* to isolate from. `workspace_backend` controls *how* that isolation happens.

## Implementation Plan

### Phase 1: Extract the Trait ✓ done

Refactor `worktree.rs` into a `WorkspaceBackend` trait (done — `src/workspace/mod.rs`):

```rust
pub type BackendResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub trait WorkspaceBackend: Send + Sync {
    fn create(&self, spec_id: &str, source: &str) -> BackendResult<PathBuf>;
    fn exec(&self, workspace_path: &Path, command: &str) -> BackendResult<ExecResult>;
    fn merge(&self, workspace_path: &Path, target: &str) -> BackendResult<()>; // default: Ok(())
    fn cleanup(&self, spec_id: &str) -> BackendResult<()>;
}
```

Current code becomes `GitWorkspace` (done — `src/workspace/git.rs`). `worker.rs` now uses
`Box<dyn WorkspaceBackend>` (done — T8BB9); direct `crate::worktree::*` calls replaced:

```rust
// before
let worktree_dir = crate::worktree::create(spec_id, ws)?;

// after
let workspace: Box<dyn WorkspaceBackend> = Box::new(GitWorkspace::new());
let worktree_dir = workspace.create(spec_id, ws)?;
```

`worktree.rs` is now a thin re-export shim for any callers not yet migrated.

### Phase 2: Add `CustomBackend`

Implements the trait by shelling out to configured commands:

```rust
pub struct CustomBackend {
    create_cmd: String,
    cleanup_cmd: String,
}

impl WorkspaceBackend for CustomBackend {
    fn create(&self, spec_id: &str, source: &str) -> Result<PathBuf> {
        let cmd = self.create_cmd
            .replace("{spec_id}", spec_id)
            .replace("{source}", source);
        let output = Command::new("sh").args(["-c", &cmd]).output()?;
        let path = String::from_utf8(output.stdout)?.trim().to_string();
        Ok(PathBuf::from(path))
    }
}
```

### Phase 3: Config Parsing + Resolution

Add `workspace_backend` to `Config` and `BoiSpec`. Resolution logic:
1. Parse spec-level override
2. Fall back to config-level default
3. Fall back to `GitWorkspaceBackend`

### Phase 4: Built-in Backends

Add `DirectoryBackend`, `DockerBackend`, `NoneBackend` as built-in types. Each is a few dozen lines implementing the trait.

## Open Questions

1. **Merge-back workflow.** Should BOI have opinions about *how* changes merge back (git push, PR, rsync), or is that entirely the backend's job? Current recommendation: backend's job. BOI just calls `merge` if `merge_back: true`.

2. **Workspace path lifetime.** Currently the workspace path is stored in the `workers` table. With remote backends, the "path" might be a remote path that's meaningless locally. Should the table store a local proxy path, or the remote path with the backend type?

3. **Docker + runtime interaction.** When workspace is Docker, should the *runtime* (Claude) also run inside the container? Or host-side Claude with commands proxied into the container? Recommendation: host-side Claude, commands proxied. This avoids needing Claude installed in every container image.

4. **Concurrent Sapling.** True parallel isolation requires clones, not bookmarks. Should the `sapling` backend always clone, or only when `max_workers > 1`?

5. **Transfer overhead.** SSH and Docker backends need the source transferred. For large repos, this dominates create time. Should BOI support a "warm cache" (pre-synced base that gets incremental updates)?
