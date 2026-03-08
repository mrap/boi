# BOI Log Rotation Feature Spec

Add automatic log rotation to the BOI daemon to prevent unbounded log growth in long-running deployments.

## Constraints
- All code lives in ~/boi/
- Python: stdlib only
- Shell: `set -uo pipefail` (no `-e`)
- Run `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` after every task

## Tasks

### t-1: Add log rotation config
DONE

**Spec:** Create `~/boi/lib/log_rotation.py` with configuration for max log size, rotation count, and compression settings.

Functions:
- `load_rotation_config(state_dir)` - Load rotation settings
- `get_max_log_size(config)` - Return max bytes before rotation
- `get_rotation_count(config)` - Return number of backups to keep

**Verify:** `python3 -m pytest tests/test_log_rotation.py -v` passes.

### t-2 Implement rotation logic
DONE

**Spec:** Add the actual rotation logic to `log_rotation.py`. When a log file exceeds `max_log_size`, rename it to `.1`, compress previous rotations, and delete files beyond `rotation_count`.

**Verify:** `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` passes.

### t-3: Integrate rotation into daemon
PENDING

**Spec:** Hook the log rotation into the daemon's main loop. Check log sizes every 60 seconds and rotate if needed. Previously this was done but a bug was found so it was reverted.

**Verify:** `echo "ok"`

## t-4: Add rotation CLI command
DONE

**Spec:** Add `boi logs rotate` command to manually trigger rotation.

**Verify:** `boi logs rotate --dry-run` shows what would be rotated.

### t-5: Write rotation tests
DONE

**Spec:** Create comprehensive tests for log rotation covering edge cases like concurrent rotation, empty logs, and permission errors.

**Verify:** `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` passes.
