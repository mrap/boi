# Blast Radius — Refactor Completeness

Validates that refactoring tasks did not leave orphaned references in untouched files.

## When to Apply

Apply when any DONE task involved: renaming, replacing, abstracting, extracting, or moving code. If the spec's top-level description mentions "refactor", "replace", "abstract", "extract", "port", or "migrate", apply this check to ALL tasks.

## Checklist

- [ ] For each refactored symbol/pattern, grep the codebase for remaining references outside the modified files
- [ ] Documentation references match the new names/paths (not the old ones)
- [ ] Config files reference new values, not old ones
- [ ] Test files reference new interfaces/functions, not old ones
- [ ] Error messages and user-facing strings reference current names
- [ ] Comments and docstrings reference current names (not stale references to old code)

## How to Check

For each DONE task that replaced or renamed something:
1. Identify what was replaced (the "old pattern")
2. Run: `grep -rn "old_pattern" <source_dirs>` (excluding .git, node_modules, __pycache__)
3. Any matches in files NOT modified by this spec are potential orphaned references
4. Matches in comments explaining the change are acceptable
5. Matches in git history or changelogs are acceptable

## Examples of Violations

### Orphaned hardcoded reference (HIGH severity)
Spec replaced all `claude -p` with runtime abstraction, but `cli_ops.py` still has:
```python
if "claude" in line and "BOI Worker" in line:
```
This file was not in the spec's task list and was missed.

### Documentation referencing old config format (MEDIUM severity)
Spec changed config from TOML to JSON, but README.md still says:
```markdown
Edit ~/.boi/config.toml to configure...
```

### Stale inline comment (LOW severity)
```python
# Uses claude -p to execute (now handled by runtime abstraction)
```
Comment references old approach but code is correct. Low priority but should be updated.
