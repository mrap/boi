# Verify Commands

Validates that verification steps for completed tasks are concrete and actionable.

## Checklist

- [ ] Every DONE task has a `**Verify:**` section with at least one concrete command or assertion
- [ ] Verify commands are not trivially passing (e.g., `true`, `echo ok`, `exit 0`)
- [ ] File paths referenced in verify commands exist in the working directory
- [ ] If a verify command includes a type checker or linter, the referenced file path is valid
- [ ] Test commands reference actual test files or test classes that exist
- [ ] Verify sections describe observable outcomes, not just "it works"

## Examples of Violations

### Trivially passing verify commands (HIGH severity)
```markdown
**Verify:** `true`
**Verify:** `echo "ok"`
**Verify:** `echo "notifications work"`
**Verify:** `echo "integration done" && true`
**Verify:** `exit 0`
```

These prove nothing. A meaningful verify command runs real assertions:
```markdown
**Verify:** `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` passes.
**Verify:** `boi logs rotate --dry-run` shows what would be rotated.
**Verify:** All 6 files exist in `tests/fixtures/`. Each has at least 3 tasks.
```
