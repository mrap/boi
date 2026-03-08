# Completeness

Validates that all spec tasks were addressed and the overall goal is met.

## Checklist

- [ ] Every task in the spec has a final status of DONE or SKIPPED (none left silently unaddressed)
- [ ] SKIPPED tasks include a reason explaining why they were skipped
- [ ] The overall spec goal (from the top-level description) is addressed by the completed tasks
- [ ] No orphaned TODO or FIXME comments in modified files without corresponding follow-up tasks
- [ ] All files mentioned in task specs that should have been created or modified actually exist
- [ ] No partial implementations left without a follow-up task to complete them

## Examples of Violations

### Silently dropped task (HIGH severity)
```markdown
### t-3: Implement CLI commands

### t-4: Add wizard
DONE
```
Task t-3 has a heading but no status line. It was silently dropped.

### SKIPPED without explanation (MEDIUM severity)
```markdown
### t-5: Write tests
SKIPPED

**Verify:** Tests pass.
```
No reason given for skipping. Should include why (e.g., "deferred to follow-up spec" or "covered by existing tests").
