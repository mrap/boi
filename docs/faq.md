# FAQ

## General

### What is BOI?

BOI (Beginning of Infinity) is a self-evolving autonomous agent fleet for Claude Code. It dispatches specs (task lists in Markdown) to parallel workers, each running in a fresh Claude session with an isolated git worktree. Workers iterate until all tasks are done.

### Why "Beginning of Infinity"?

Named after David Deutsch's book *The Beginning of Infinity*. The core idea: knowledge grows through conjecture and criticism. BOI specs are conjectures. Each iteration is a round of criticism and refinement. The system discovers work it couldn't foresee and adds new tasks.

### How is BOI different from running Claude in a long session?

Long sessions degrade. Context fills up, instructions get lost, and the agent starts hallucinating or repeating itself. BOI prevents this by design: every iteration starts with a fresh Claude session. Zero accumulated context. The spec file on disk carries all state.

### What do I need to run BOI?

- Python 3.10+
- Git
- tmux
- Claude Code CLI
- Bash

No pip packages, no Docker (optional), no cloud services.

## Installation

### How do I install BOI?

```bash
curl -fsSL https://raw.githubusercontent.com/mrap/boi/main/install-public.sh | bash
```

Or manually:
```bash
git clone https://github.com/mrap/boi.git ~/boi
bash ~/boi/install.sh --workers 3
```

### How many workers should I use?

Default is 3. Each worker is an isolated git worktree running its own Claude session. More workers means more specs can run in parallel. The max is 5. Start with 3 and adjust based on your machine's resources.

### Can I use existing directories as worktrees?

Yes:
```bash
boi install --worktree-paths /path/to/worktree1,/path/to/worktree2
```

### How do I verify the install?

```bash
boi doctor
```

Checks all prerequisites and reports any issues.

## Specs

### What is a spec?

A Markdown file with ordered tasks. Each task has a heading (`### t-N: Title`), a status line (`PENDING`), a `**Spec:**` section (what to do), and a `**Verify:**` section (how to prove it's done). See [spec-format.md](spec-format.md).

### How big should each task be?

Each task should be completable in a single Claude session: roughly 10-30 minutes of work. If a task is too big, split it. A good rule of thumb: if you'd need multiple conversations with Claude to explain and complete it, it's too big.

### Can workers add new tasks?

In Discover and Generate modes, yes. Workers can add new PENDING tasks when they discover work that wasn't foreseeable at planning time. In Execute and Challenge modes, workers cannot add tasks.

### What happens if a task fails?

The worker marks it FAILED. The daemon detects remaining PENDING tasks and requeues the spec. If the same task fails repeatedly, the consecutive failure counter increments. After enough consecutive failures, the spec is marked failed.

### Can I modify a spec while it's running?

Yes, through `boi spec`:
```bash
boi spec q-001 add "New task title"    # Add a task
boi spec q-001 skip t-4                # Skip a task
boi spec q-001 next t-6                # Reorder a task to run next
boi spec q-001 edit                    # Open in $EDITOR
```

Or edit the spec file directly. Workers read the spec fresh each iteration.

## Running

### How do I check progress?

```bash
boi status              # Snapshot
boi status --watch      # Live auto-refresh
boi dashboard           # Compact view
boi telemetry q-001     # Per-iteration breakdown
boi log q-001           # Worker output
```

### How do I cancel a spec?

```bash
boi cancel q-001
```

### How do I stop everything?

```bash
boi stop
```

Stops the daemon and kills all worker sessions.

### Can I run multiple specs at once?

Yes. BOI has a priority queue. Dispatch multiple specs and they run in priority order. If you have N workers, up to N specs can run simultaneously.

```bash
boi dispatch --spec spec-a.md --priority 10
boi dispatch --spec spec-b.md --priority 50
boi dispatch --spec spec-c.md --priority 100
```

### What happens if my machine reboots?

The daemon stops. Specs in `running` state are detected as crashed on next startup. Run `boi dispatch` or start the daemon again and they'll be requeued.

## Modes

### Which mode should I use?

| Situation | Mode |
|-----------|------|
| Well-defined, no surprises | Execute |
| Want quality feedback | Challenge |
| Real-world features, expect surprises | Discover |
| Exploratory, unclear path | Generate |

See [modes.md](modes.md) for the full breakdown.

### What are experiments?

In Challenge, Discover, and Generate modes, workers can propose alternative approaches. They create a git branch, implement the alternative, and mark the task `EXPERIMENT_PROPOSED`. You review with `boi review q-001` and adopt or reject.

## Critic

### What does the critic do?

After all tasks in a spec are done, the critic reviews the work. It checks spec integrity, verification rigor, code quality, completeness, and fleet-readiness. It also computes a quality score. If issues are found, it adds new `[CRITIC]` tasks. See [critic.md](critic.md).

### How do I disable the critic?

```bash
boi critic disable                              # Globally
boi dispatch --spec spec.md --no-critic         # Per-spec
```

### Can I add custom checks?

Yes. Add `.md` files to `~/.boi/critic/custom/`. Each file defines a checklist that the critic evaluates. See [critic.md](critic.md) for details.

## Troubleshooting

### `boi doctor` says Claude CLI not found

Install Claude Code: https://docs.anthropic.com/en/docs/claude-code

### Workers are stuck / not making progress

1. Check the worker log: `boi log q-001 --full`
2. Check if the task is too vague. Workers need concrete instructions.
3. Check if the verification step is impossible. Workers try to satisfy `**Verify:**` and fail if they can't.
4. Cancel and redispatch with a rewritten spec: `boi cancel q-001`

### The daemon isn't starting

Check if another daemon is already running:
```bash
cat ~/.boi/daemon.pid
ps -p $(cat ~/.boi/daemon.pid)
```

If the PID file is stale, remove it:
```bash
rm ~/.boi/daemon.pid
```

### Worktrees are in a bad state

Reset a worktree manually:
```bash
cd ~/.boi/worktrees/boi-worker-1
git checkout main
git clean -fd
```

Or reinstall:
```bash
boi install --workers 3
```

### How do I see what tmux sessions BOI is using?

```bash
tmux -L boi list-sessions
```

BOI uses a dedicated tmux server (`-L boi`) to keep its sessions separate from yours.
