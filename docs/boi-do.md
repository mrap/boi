# `boi do` - Natural Language Interface

Talk to BOI in plain English. Claude translates your request into CLI commands.

## Usage

```bash
boi do "show me what's running"
```

BOI gathers current system state (queue, workers, projects), sends your request to Claude along with the context, and gets back the appropriate CLI commands.

## Examples

```bash
boi do "show me what's running"                    # → boi status
boi do "cancel the ios spec"                       # → boi cancel q-001
boi do "add a task to q-002 for database migration"
boi do "skip t-4 in q-001, no longer needed"
boi do "what's the quality score for q-003?"       # → boi telemetry q-003
boi do "dispatch my spec with high priority"       # → boi dispatch --spec ... --priority 10
boi do "how many workers are free?"                # → boi workers
boi do "stop everything"                           # → boi stop
```

## Safety

Destructive commands require confirmation before execution:

```bash
$ boi do "cancel everything"
→ Will run: boi cancel q-001; boi cancel q-002
→ This is destructive. Proceed? [y/N]
```

Commands classified as destructive: `cancel`, `stop`, `purge`, `delete`, `skip`, `next`, `block`, `edit`, `dispatch`.

### Skip confirmation

```bash
boi do --yes "skip t-4 in q-001"      # Execute without asking
```

### Dry run

```bash
boi do --dry-run "stop everything"    # Show commands without executing
```

Shows what would run without executing anything.

## How It Works

1. `boi do` gathers current system state by running `boi status --json`, `boi queue --json`, `boi workers --json`, and optionally `boi project list --json`
2. If your request mentions a queue ID (e.g., `q-001`), it also fetches that spec's task list
3. All context plus your request is sent to Claude with a system prompt that knows the full BOI CLI
4. Claude returns a JSON response with: the commands to run, an explanation, and whether the commands are destructive
5. BOI applies a safety-net check: even if Claude says the commands are not destructive, BOI independently classifies them based on keyword matching
6. If destructive: prompt for confirmation (unless `--yes`)
7. Execute the commands sequentially

## Limitations

- Requires Claude Code CLI to be available
- Each `boi do` invocation makes one Claude API call
- Complex multi-step requests may not translate perfectly. For precision, use the CLI directly.
- `boi do` cannot create spec files. It generates CLI commands, not file content. To write a spec, use your editor or ask Claude in a regular session.
