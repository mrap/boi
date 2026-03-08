# BOI Health Check Spec

Add a `boi doctor` command that validates the BOI installation, checks for common issues, and reports system health.

## Constraints
- All code lives in ~/boi/
- Python: stdlib only
- Shell: `set -uo pipefail` (no `-e`)
- Run `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` after every task

## Tasks

### t-1: Create health check module
DONE

**Spec:** Create `~/boi/lib/health.py` with individual health check functions.

```python
import os
import json
import subprocess
import shutil
from pathlib import Path

def check_installation(boi_dir):
    """Verify all required BOI files exist."""
    required = ["boi.sh", "worker.sh", "daemon.sh", "lib/queue.py", "lib/spec_parser.py"]
    missing = []
    for f in required:
        path = os.path.join(boi_dir, f)
        if not os.path.exists(path):
            missing.append(f)
    if missing:
        return {"status": "FAIL", "message": f"Missing files: {', '.join(missing)}"}
    return {"status": "OK", "message": "All required files present"}

def check_permissions(boi_dir):
    """Verify scripts are executable."""
    scripts = ["boi.sh", "worker.sh", "daemon.sh"]
    issues = []
    for s in scripts:
        path = os.path.join(boi_dir, s)
        if os.path.exists(path) and not os.access(path, os.X_OK):
            issues.append(f"{s} is not executable")
    if issues:
        return {"status": "WARN", "message": "; ".join(issues)}
    return {"status": "OK", "message": "All scripts executable"}

def check_state_dir(state_dir):
    """Verify state directory exists and is writable."""
    if not os.path.exists(state_dir):
        return {"status": "FAIL", "message": f"State directory {state_dir} does not exist"}
    if not os.access(state_dir, os.W_OK):
        return {"status": "FAIL", "message": f"State directory {state_dir} is not writable"}
    return {"status": "OK", "message": "State directory OK"}

def check_claude_available():
    """Check if claude CLI is available."""
    path = shutil.which("claude")
    if path is None:
        return {"status": "FAIL", "message": "claude CLI not found in PATH"}
    return {"status": "OK", "message": f"claude found at {path}"}
```

Each check returns a dict with `status` (OK, WARN, FAIL) and `message`. Functions handle missing paths gracefully without raising exceptions.

**Verify:** `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` passes.

### t-2: Create doctor command
DONE

**Spec:** Add `boi doctor` command to `boi.sh` that runs all health checks and prints a formatted report.

```bash
cmd_doctor() {
    echo "BOI Health Check"
    echo ""
    python3 -c "
import sys
sys.path.insert(0, '$BOI_DIR/lib')
from health import (
    check_installation,
    check_permissions,
    check_state_dir,
    check_claude_available
)
checks = [
    ('Installation', check_installation('$BOI_DIR')),
    ('Permissions', check_permissions('$BOI_DIR')),
    ('State Directory', check_state_dir('$STATE_DIR')),
    ('Claude CLI', check_claude_available()),
]
has_issues = False
for name, result in checks:
    icon = '✓' if result['status'] == 'OK' else '⚠' if result['status'] == 'WARN' else '✗'
    print(f'  {icon} {name}: {result[\"message\"]}')
    if result['status'] != 'OK':
        has_issues = True
if has_issues:
    print()
    print('Some checks failed. Run with --fix to auto-repair.')
    sys.exit(1)
else:
    print()
    print('All checks passed.')
"
}
```

The command prints a clear report with icons, runs all checks, and exits non-zero if any checks fail.

**Verify:** `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` passes.

### t-3: Add auto-fix capability
DONE

**Spec:** Add `boi doctor --fix` that attempts to auto-fix issues found by the health checks.

```python
def fix_permissions(boi_dir):
    """Make scripts executable."""
    scripts = ["boi.sh", "worker.sh", "daemon.sh"]
    fixed = []
    for s in scripts:
        path = os.path.join(boi_dir, s)
        if os.path.exists(path) and not os.access(path, os.X_OK):
            os.chmod(path, 0o755)
            fixed.append(s)
    if fixed:
        return {"fixed": True, "message": f"Fixed permissions for: {', '.join(fixed)}"}
    return {"fixed": False, "message": "No permission issues to fix"}

def fix_state_dir(state_dir):
    """Create state directory if missing."""
    if not os.path.exists(state_dir):
        os.makedirs(state_dir, mode=0o755, exist_ok=True)
        return {"fixed": True, "message": f"Created state directory: {state_dir}"}
    return {"fixed": False, "message": "State directory already exists"}
```

Each fix function returns whether it made changes and what it did. The `--fix` flag runs checks first, then attempts fixes only for failed checks, then re-runs checks to verify.

**Verify:** `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` passes.

### t-4: Write health check tests
DONE

**Spec:** Create `~/boi/tests/test_health.py` with tests for all health checks and auto-fix.

Tests:
- `test_check_installation_all_present` - All files exist returns OK
- `test_check_installation_missing_file` - Missing file returns FAIL
- `test_check_permissions_executable` - Executable scripts return OK
- `test_check_permissions_not_executable` - Non-executable returns WARN
- `test_check_state_dir_exists` - Existing writable dir returns OK
- `test_check_state_dir_missing` - Missing dir returns FAIL
- `test_fix_permissions` - Fix makes scripts executable
- `test_fix_state_dir` - Fix creates missing directory

All tests use `tempfile.mkdtemp()` for isolation and clean up after themselves.

**Verify:** `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` passes.
