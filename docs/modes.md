# Execution Modes

BOI has four execution modes that control what workers can do during each iteration. Modes are a graduated capability system, from strict task execution to full creative authority.

## Mode Comparison

| Mode | Add Tasks | Skip Tasks | Write Challenges | Modify PENDING | Supersede | Experiments |
|------|-----------|------------|------------------|----------------|-----------|-------------|
| **Execute** | No | No | No | No | No | No |
| **Challenge** | No | Yes (with reason) | Yes | No | No | Yes (budget: 2) |
| **Discover** | Yes | Yes (with reason) | No | No | No | Yes (budget: 3) |
| **Generate** | Yes | Yes | Yes | Yes | Yes | Yes (budget: 5) |

## Setting the Mode

Three ways, in order of precedence:

1. **Spec header** (highest priority):
   ```yaml
   mode: discover
   ```

2. **CLI flag**:
   ```bash
   boi dispatch --spec spec.yaml --mode discover
   boi dispatch --spec spec.yaml -m d
   ```

3. **Default**: `execute` if nothing else is specified.

Mode aliases: `execute`/`e`, `challenge`/`c`, `discover`/`d`, `generate`/`g`.

## Execute Mode

The strictest mode. Workers execute the current task exactly as specified. No task additions, no skipping, no modifications.

**Use when:** Tasks are well-defined and straightforward. You know exactly what needs to be done and don't want the agent improvising.

```bash
boi dispatch --spec spec.yaml -m e
```

**Worker rules:**
- Execute the current PENDING task
- Run the verification steps
- Mark the task DONE or FAILED
- Do not add, skip, or modify any tasks

## Challenge Mode

Execute the task, but also flag concerns. Workers can write observations and skip tasks with reasoning, but cannot add new tasks or modify the spec structure.

**Use when:** You want a second pair of eyes on the approach. Good for code reviews, security audits, or when you're less confident about the plan.

```bash
boi dispatch --spec spec.yaml -m c
```

**Worker rules:**
- Execute the current task
- Write observations to a `## Challenges` section
- Skip tasks with detailed reasoning
- Cannot add new tasks or modify PENDING task specs

**Challenge format:**
```markdown
## Challenges

### c-1: [task t-3] Missing error handling
**Observed:** The API endpoint has no retry logic for transient failures.
**Risk:** HIGH
**Suggestion:** Add exponential backoff with 3 retries.
```

## Discover Mode

Execute the task AND handle what you find. Workers can add new PENDING tasks when they discover work that wasn't foreseeable at planning time. This is what makes BOI fundamentally different from a task runner.

**Use when:** Most real-world work. You have a plan but expect surprises. The agent adapts to what it finds in the codebase.

```bash
boi dispatch --spec spec.yaml -m d
```

**Worker rules:**
- Execute the current task
- Add new PENDING tasks when discovering necessary work
- Skip tasks with reasoning
- Cannot modify existing PENDING tasks

**Discovery documentation:**
```markdown
## Discovery

### Iteration 5
- **Found:** The database schema needs a new index for the user lookup query.
- **Added:** t-8 (add database index).
- **Rationale:** Without the index, getUserByEmail does a full table scan.
```

**Example:** A worker implementing an API endpoint discovers the database schema needs a migration. It adds:

```yaml
- id: t-7
  title: Add database migration for email index
  status: PENDING
  spec: |
    The getUserByEmail query in t-3 does a full table scan. Add an index
    on the email column in the User table.
  verify: |
    Tests pass. Query plan shows index usage.
```

The daemon detects the new PENDING task and keeps iterating.

## Generate Mode

Full creative authority. Workers can add, modify, supersede tasks, and restructure the entire plan. Uses a goal-only spec format and a three-phase lifecycle.

**Use when:** The path to the goal is unclear. You know what you want but not how to get there. Good for exploratory work, prototypes, and greenfield features.

```bash
boi dispatch --spec goal-spec.yaml -m g
```

### Goal-Only Spec Format

Generate mode specs define a goal and success criteria instead of tasks:

```yaml
title: Config Management CLI
mode: generate
context: |
  Build a CLI tool that reads, validates, and applies YAML configuration files
  with schema validation, environment variable interpolation, and dry-run mode.
  Python 3.10+, stdlib only. Must work on Linux and macOS.

success_criteria:
  - CLI reads and parses YAML config files
  - Schema validation catches malformed configs
  - Environment variables are interpolated in config values
  - Dry-run mode shows what would change without applying
  - Help text is complete and accurate
  - Unit tests cover all core functions
```

### Three-Phase Lifecycle

1. **Decompose**: A decomposition worker breaks the goal into 5-15 concrete tasks. These become the spec's task list.

2. **Execute**: Workers execute tasks iteratively, same as other modes. Workers can add, modify, or supersede tasks.

3. **Evaluate**: An evaluation worker checks each Success Criterion against the implementation. Unmet criteria generate new tasks. The loop continues until all criteria are met or convergence is reached.

### Convergence

Generate mode stops when:
- All Success Criteria are met and the critic approves (ideal outcome)
- Max iterations reached (default 50 for Generate)
- No progress for 5 consecutive iterations (stalled)
- Diminishing returns: last 3 iterations improved criteria by less than 1 each, and more than 80% are met (good enough)

### Superseding Tasks

In Generate mode, workers can replace a PENDING task with a better alternative:

```yaml
- id: t-3
  title: Parse config with regex
  status: SUPERSEDED by t-8

- id: t-8
  title: Parse config with YAML stdlib module
  status: PENDING
  spec: |
    Replace the regex-based parser from t-3 with Python's yaml module...
```

## Experiments

In Challenge, Discover, and Generate modes, workers can propose alternative approaches. Each mode has an experiment budget (default: 2, 3, and 5 respectively).

### How experiments work

1. A worker finds evidence for a better approach during task execution
2. It creates a git branch: `experiment-{queue_id}-{task_id}`
3. It implements the alternative on that branch
4. It writes an `#### Experiment:` section with thesis, evidence, and results
5. It marks the task `EXPERIMENT_PROPOSED`
6. The daemon pauses the spec (`needs_review`) and notifies you

### Reviewing experiments

```bash
boi review q-001
```

For each experiment, choose:
- `[a]` **Adopt**: Merge the experiment branch, mark the task DONE
- `[r]` **Reject**: Delete the branch, reset the task to PENDING
- `[d]` **Defer**: Keep the spec paused
- `[v]` **View**: See full experiment details

Experiments auto-reject after 24 hours if not reviewed (configurable via `experiment_timeout_hours` in `~/.boi/config.json`).

### Overriding the experiment budget

```bash
boi dispatch --spec spec.yaml --mode discover --experiment-budget 10
```

## Error Log

Workers (in all modes except Execute) can append to an `## Error Log` section when an approach fails. Future workers read the Error Log and avoid retrying documented failures:

```markdown
## Error Log

### [iter-5] Attempted regex-based parsing
Tried to parse the config file with regex. Failed because nested YAML
structures can't be reliably matched. Future workers should use the
yaml module from stdlib instead.
```

## Choosing a Mode

| Situation | Recommended Mode |
|-----------|-----------------|
| Well-defined tasks, no surprises expected | Execute |
| Want quality feedback alongside execution | Challenge |
| Real-world features, expect some surprises | Discover |
| Exploratory work, unclear path to goal | Generate |
| Bug fixes with known root cause | Execute |
| Refactoring with potential side effects | Discover |
| Security audit or code review | Challenge |
| Building a new system from scratch | Generate |
