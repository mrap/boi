# CLI Reference

Full command reference for the `boi` binary. Source: `src/main.rs:54` (`Commands` enum).

```
boi dispatch <spec.yaml> [--after SPEC_ID] [--priority N] [--mode e|c|d|g]
             [--max-iter N] [--timeout N] [--no-critic] [--project X]
             [--dry-run] [--workspace PATH]
boi status [spec-id] [--all] [--watch] [--json] [--verbose/-v]
boi log <spec-id> [--full] [--debug] [--follow/-f]
boi cancel <spec-id>
boi outputs <spec-id>
boi daemon [start|stop [--destroy-running] [--yes] | restart [--destroy-running] [--yes] | foreground]
boi config [key] [value]
boi workers
boi stop
boi telemetry <spec-id>
boi spec <queue-id> [show|add|skip|block|tail]
boi phases [<name>] [--spec SPEC_ID] [--full]
boi providers [list]
boi doctor
boi version
boi bench [--phase P] [--spec FILE|--battery DIR] [--pipeline name:path] [--runs N] [--json]
boi dashboard
boi completions <bash|zsh|fish|elvish|powershell>
boi prune-orphans [--dry-run|--apply] [--yes] [--force] [--max-idle-secs N]
                  [--exclude-pattern PAT] [--json]
boi research <brief.md> [--threads N] [--project NAME]
```

**Not implemented** (referenced in SKILL.md or Python archive but absent from `Commands` enum): `boi resume`, `boi dep`, `boi project`, `boi critic`.

See also: [debugging.md](debugging.md) for diagnostic commands, [spec-format.md](spec-format.md) for dispatch input format.
