# Security

BOI orchestrates Claude Code sessions that execute code autonomously. This page covers the security model, risks, and recommendations for running BOI safely.

## Trust Model

BOI's trust model has two key assumptions:

1. **Spec authors are trusted.** The spec file content is injected directly into Claude's prompt. A malicious spec can instruct Claude to execute arbitrary commands, exfiltrate data, or modify files. Only run specs you wrote or reviewed.
2. **Claude operates with full permissions.** Workers run with `--dangerously-skip-permissions`, which means Claude will not prompt for confirmation before executing shell commands, writing files, or making network requests.

If either assumption is violated (untrusted spec, shared machine), use the isolation techniques described below.

## What `--dangerously-skip-permissions` Does

By default, Claude Code prompts the user before running shell commands, writing to files, or performing other potentially destructive actions. The `--dangerously-skip-permissions` flag disables all of these prompts.

BOI requires this flag because workers run non-interactively. There is no human to approve each action. This means:

- Claude can run any shell command (including `rm`, `curl`, `wget`, etc.)
- Claude can read and write any file the process user can access
- Claude can make network requests to any endpoint
- Claude can install packages, modify system configuration, etc.

**This is equivalent to giving a script `sudo` access to your user account.**

## Recommendations

### For Personal Development Machines

If you are the only user on the machine and you trust your specs:

- BOI's default configuration is acceptable
- Workers operate within git worktrees, which provides source-level isolation
- Review self-evolved tasks (tasks BOI adds during execution) before dispatching follow-up iterations

### For Shared Machines

If multiple users share the machine:

- Run BOI inside a Docker container (see below)
- Ensure `~/.boi/` is not readable by other users: `chmod 700 ~/.boi`
- Do not run specs authored by others without reviewing them first

### For CI/CD Environments

- Always run BOI inside a container with no network access to internal services
- Mount only the project directory, not the full filesystem
- Set resource limits (CPU, memory, time) on the container

## Docker Isolation

Running BOI in Docker provides filesystem and network isolation. The worker can only access files you explicitly mount.

### Dockerfile

A `Dockerfile` is included in the repository root. Build and run:

```bash
docker build -t boi .
docker run --rm -v $(pwd):/project boi dispatch --spec /project/spec.yaml
docker run --rm boi status
```

### docker-compose

A `docker-compose.yml` is also provided:

```bash
docker compose run boi dispatch --spec /project/spec.yaml
docker compose run boi status
```

### Custom Isolation

For stricter isolation, you can:

1. **Restrict network access:**
   ```bash
   docker run --network none -v $(pwd):/project boi dispatch --spec /project/spec.yaml
   ```

2. **Run as a non-root user:**
   ```bash
   docker run --user $(id -u):$(id -g) -v $(pwd):/project boi dispatch --spec /project/spec.yaml
   ```

3. **Limit resources:**
   ```bash
   docker run --memory=4g --cpus=2 -v $(pwd):/project boi dispatch --spec /project/spec.yaml
   ```

4. **Read-only root filesystem:**
   ```bash
   docker run --read-only --tmpfs /tmp -v $(pwd):/project boi dispatch --spec /project/spec.yaml
   ```

## Input Validation

BOI validates inputs at several levels:

- **Queue IDs** are auto-generated in a strict `q-NNN` format. User-provided queue IDs are validated against this pattern.
- **Spec files** are parsed and validated before enqueuing. The spec validator checks structure, task format, and required fields.
- **File paths** are resolved with `os.path.abspath()` before use. Spec files are copied into the queue directory, so workers operate on a snapshot, not the original.
- **Subprocess calls** use list arguments (not shell strings), avoiding shell injection. No `shell=True` is used anywhere in the codebase.
- **No `eval()` or `exec()`** is used on user-provided input.

## Spec Safety Checklist

Before running a spec you did not write:

- [ ] Read every task's `Spec:` section. Does it contain `curl`, `wget`, or network commands you don't expect?
- [ ] Check for `rm -rf`, file deletion, or commands that modify files outside the project directory
- [ ] Look for references to `~/.ssh`, `~/.aws`, `~/.config`, or other sensitive directories
- [ ] Verify the spec does not override BOI's worker prompt or inject instructions into the system prompt
- [ ] Check self-evolution rules. Can the spec add tasks that escalate its own permissions?

## Reporting Security Issues

If you find a security vulnerability in BOI, please report it by opening a GitHub issue with the `security` label, or email the maintainers directly. Do not include exploit details in public issues.
