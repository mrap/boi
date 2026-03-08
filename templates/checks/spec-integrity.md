# Spec Integrity

Validates the structural correctness of the spec file itself.

## Checklist

- [ ] All tasks use `### t-N:` heading format where N is a positive integer
- [ ] Status line (DONE, PENDING, SKIPPED) appears on its own line immediately after the heading
- [ ] No duplicate task IDs exist in the spec
- [ ] No DONE tasks have regressed to PENDING (compare against telemetry if available)
- [ ] Spec file is valid markdown with no binary corruption or truncation
- [ ] Task count in the spec matches what telemetry reports for this queue ID
- [ ] Every task has both a `**Spec:**` section and a `**Verify:**` section

## Examples of Violations

### Wrong heading format (HIGH severity)
```markdown
### t-2 Implement rotation logic   <-- missing colon after t-2
## t-4: Add CLI command            <-- wrong heading level (## instead of ###)
#### t-5: Write tests              <-- wrong heading level (#### instead of ###)
```

### Task status regression (HIGH severity)
```markdown
### t-3: Integrate rotation into daemon
DONE

**Spec:** Previously this was done but a bug was found so it was reverted.
```
The description says it was reverted, but status still says DONE. This is a regression that should be PENDING.
