# Code Quality

Validates that code changes meet quality standards and follow project conventions.

## Checklist

- [ ] Modified files use consistent style with surrounding code (indentation, naming, spacing)
- [ ] No `any` or `mixed` types introduced in Hack or Python code
- [ ] No hardcoded paths that should be configurable (e.g., absolute user-specific paths in library code)
- [ ] Error handling is present for all I/O operations (file reads, writes, subprocess calls)
- [ ] No bare `except:` (Python) or `catch (\Exception)` (Hack) without logging the error
- [ ] No commented-out code left behind without a clear TODO explaining why
- [ ] Functions have clear single responsibilities and are not excessively long

## Examples of Violations

### Silent error swallowing (HIGH severity)
```python
# BAD: bare except: pass silently discards all errors
try:
    with open(config_path) as f:
        config = json.load(f)
except:
    pass  # disk full? permission denied? corrupted file? nobody will ever know
```

### Validation function that always passes (HIGH severity)
```python
# BAD: catches all errors, always returns True
def validate_config(path):
    try:
        config = json.load(open(path))
    except Exception:
        return True  # "valid" even when unreadable
    for key in required_keys:
        try:
            _ = config[key]
        except KeyError:
            pass  # missing required keys silently ignored
    return True  # always True, validation is theater
```

### Silent return on error (MEDIUM severity)
```python
# BAD: user gets no feedback on failure
except Exception:
    return  # rollback failed, config is broken, user has no idea
```
