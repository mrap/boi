# Critic

The critic is BOI's built-in quality gate. When a spec finishes all its PENDING tasks, the critic reviews the completed work before marking the spec as done. If it finds issues, it adds new `[CRITIC]` PENDING tasks to the spec and requeues it. If everything passes, the spec is marked completed.

## How It Works

1. All tasks in a spec reach DONE or SKIPPED status
2. The daemon triggers the critic (unless `--no-critic` was used)
3. The critic generates a prompt with the spec contents and all active check definitions
4. A Claude worker evaluates the spec against each check
5. The critic computes a quality score (18 signals, 4 categories)
6. **Pass**: Writes a `## Critic Approved` section, spec is marked completed
7. **Fail**: Adds `[CRITIC]` PENDING tasks for issues found, spec is requeued

The critic gets up to 2 passes (3 for Generate mode specs). After max passes, the spec is force-approved.

## Quality Scoring

Quality is measured across 18 signals in 4 categories:

| Category | Weight | Signals |
|----------|--------|---------|
| Code Quality | 35% | Error handling, input validation, resource management, naming, complexity, edge cases |
| Test Quality | 25% | Coverage, assertion quality, edge case testing, test isolation, verify command rigor |
| Documentation | 15% | Inline comments, spec clarity, error messages |
| Architecture | 25% | Separation of concerns, dependency management, extensibility, consistency |

Each signal is scored as a ratio (e.g., "6 of 8 I/O operations have error handling = 0.75"). If a category doesn't apply (e.g., no source files were modified), its weight is redistributed proportionally.

### Grading Scale

| Grade | Score Range |
|-------|------------|
| A | 0.85 - 1.00 |
| B | 0.70 - 0.84 |
| C | 0.50 - 0.69 |
| D | 0.30 - 0.49 |
| F | 0.00 - 0.29 |

### Quality Gates

- Score >= 0.85: **Fast-approve.** Skip detailed checks.
- Score 0.50-0.84: **Standard review.** Run all checks.
- Score < 0.50: **Auto-reject.** Add a `[CRITIC]` task for quality improvement.

## Default Checks

BOI ships with 5 default checks (plus quality scoring and Generate-mode goal alignment):

| Check | What It Evaluates |
|-------|-------------------|
| `spec-integrity` | Tasks have proper format, status lines, spec/verify sections |
| `verify-commands` | Verification steps are concrete and runnable |
| `code-quality` | Code follows best practices, handles errors, validates input |
| `completeness` | All tasks are addressed, no gaps in implementation |
| `fleet-readiness` | Work is self-contained and doesn't leave broken state |

## Mode Awareness

The critic adapts its behavior based on the execution mode:

- **Execute mode**: Flags if the worker added tasks (it shouldn't have)
- **Challenge mode**: Flags if the worker added tasks (it shouldn't have)
- **Discover mode**: Validates new tasks have proper format (Spec + Verify sections)
- **Generate mode**: Validates SUPERSEDED tasks reference replacements. Runs goal-alignment check against Success Criteria.

## Configuration

The critic is configured via `~/.boi/critic/config.json`:

```json
{
  "enabled": true,
  "trigger": "on_complete",
  "max_passes": 2,
  "checks": ["spec-integrity", "verify-commands", "code-quality", "completeness", "fleet-readiness"],
  "custom_checks_dir": "custom",
  "timeout_seconds": 600
}
```

| Field | Description | Default |
|-------|-------------|---------|
| `enabled` | Whether the critic runs at all | `true` |
| `trigger` | When to run (`on_complete` = after all tasks done) | `"on_complete"` |
| `max_passes` | Maximum critic review passes before force-approving | `2` |
| `checks` | Which default checks to run (remove entries to skip them) | All 5 |
| `custom_checks_dir` | Subdirectory name for custom checks | `"custom"` |
| `timeout_seconds` | Maximum time for a critic pass | `600` |

## Custom Checks

Add `.md` files to `~/.boi/critic/custom/` to define additional review criteria. Each file contains a title, description, and checklist.

### Example: Security Review

Create `~/.boi/critic/custom/security-review.md`:

```markdown
# Security Review

Validates that code changes do not introduce security vulnerabilities.

## Checklist

- [ ] No secrets, tokens, or credentials hardcoded in source files
- [ ] All user input is validated and sanitized before use
- [ ] File paths are canonicalized to prevent path traversal
- [ ] Subprocess calls use argument lists, not shell=True with string interpolation
- [ ] No use of eval(), exec(), or equivalent with untrusted input
- [ ] Sensitive data is not logged or written to world-readable files
```

Once saved, this check is automatically included in the next critic pass. Verify with `boi critic checks`.

If a custom check has the same filename as a default check (e.g., `code-quality.md`), the custom version replaces the default.

## Custom Prompt

Create `~/.boi/critic/prompt.md` to completely replace the default critic prompt template. Supported variables:

- `{{SPEC_CONTENT}}` - Full spec file contents
- `{{CHECKS}}` - All active check definitions (default + custom)
- `{{QUEUE_ID}}` - The spec's queue ID
- `{{ITERATION}}` - Current critic pass number
- `{{SPEC_PATH}}` - Absolute path to the spec file

## Disabling the Critic

Three ways to skip critic validation:

1. **Globally:** `boi critic disable`
2. **Per-spec:** `boi dispatch --spec spec.yaml --no-critic`
3. **Edit config:** Set `"enabled": false` in `~/.boi/critic/config.json`

Re-enable with `boi critic enable`.

## Running the Critic Manually

Trigger a critic pass on any spec, regardless of completion status:

```bash
boi critic run q-001
```

## CLI Reference

```bash
boi critic status     # Show config, active checks, pass history
boi critic run q-001  # Manually trigger critic on a spec
boi critic disable    # Set enabled=false
boi critic enable     # Set enabled=true
boi critic checks     # List all active checks (default + custom)
```

`boi doctor` also reports critic status.

## Directory Structure

```
~/.boi/critic/
  config.json          # Settings (enabled, trigger, max_passes)
  prompt.md            # Optional: custom critic prompt override
  custom/              # Optional: additional check definitions
    security-review.md
    performance-check.md
```
