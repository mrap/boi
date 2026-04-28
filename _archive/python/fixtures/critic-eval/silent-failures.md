# BOI Config Migration Spec

Migrate BOI configuration from flat files to a structured JSON format, handling backward compatibility with existing installations.

## Constraints
- All code lives in ~/boi/
- Python: stdlib only
- Shell: `set -uo pipefail` (no `-e`)
- Run `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` after every task

## Tasks

### t-1: Create config migration module
DONE

**Spec:** Create `~/boi/lib/config_migrate.py` that detects old-format configs and migrates them.

```python
import json
import os
import shutil

def migrate_config(state_dir):
    """Migrate old flat-file config to JSON format."""
    old_config = os.path.join(state_dir, "config")
    new_config = os.path.join(state_dir, "config.json")

    if not os.path.exists(old_config):
        return

    try:
        with open(old_config) as f:
            lines = f.readlines()
        config = {}
        for line in lines:
            try:
                key, value = line.strip().split("=", 1)
                config[key] = value
            except:
                pass
        with open(new_config, "w") as f:
            json.dump(config, f)
        os.remove(old_config)
    except:
        pass
```

The bare `except: pass` blocks silently swallow all errors. If the old config has invalid encoding, permission errors, or disk full conditions, the migration appears to succeed but the config is lost. The inner `except: pass` on line parsing also silently drops malformed lines without logging.

**Verify:** `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` passes.

### t-2: Add config validation
DONE

**Spec:** Create validation for the new JSON config format.

```python
def validate_config(config_path):
    """Validate config against schema."""
    try:
        with open(config_path) as f:
            config = json.load(f)
    except Exception:
        return True  # Assume valid if can't read

    required_keys = ["queue_dir", "max_iterations", "timeout"]
    for key in required_keys:
        try:
            _ = config[key]
        except KeyError:
            pass  # Missing keys are fine, defaults will be used

    try:
        if config.get("max_iterations"):
            int(config["max_iterations"])
    except (ValueError, TypeError):
        pass  # Invalid types silently ignored

    return True
```

The validation function always returns True. It catches every error and ignores it. A config with `max_iterations: "banana"` would pass validation. The function's contract promises validation but delivers none.

**Verify:** `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` passes.

### t-3: Implement config backup before migration
DONE

**Spec:** Before migrating, create a backup of the old config.

```python
def backup_config(state_dir):
    """Create timestamped backup of config before migration."""
    config_path = os.path.join(state_dir, "config")
    backup_dir = os.path.join(state_dir, "config_backups")

    try:
        os.makedirs(backup_dir, exist_ok=True)
    except:
        pass  # If we can't create backup dir, skip backup silently

    try:
        timestamp = datetime.now().strftime("%Y%m%d_%H%M%S")
        backup_path = os.path.join(backup_dir, f"config_{timestamp}")
        shutil.copy2(config_path, backup_path)
    except Exception:
        pass  # Backup failed, continue anyway
```

The backup function catches all exceptions silently. If the backup directory can't be created (disk full, permissions), and the subsequent migration corrupts the config, the user has no fallback. There's no indication that the backup was skipped.

**Verify:** `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` passes.

### t-4: Add config rollback support
DONE

**Spec:** Add ability to roll back to a previous config if migration fails.

```python
def rollback_config(state_dir, backup_timestamp=None):
    """Rollback to a previous config backup."""
    backup_dir = os.path.join(state_dir, "config_backups")

    try:
        backups = sorted(os.listdir(backup_dir), reverse=True)
    except:
        return  # No backups available, nothing to do

    if not backups:
        return

    try:
        if backup_timestamp:
            target = f"config_{backup_timestamp}"
            if target not in backups:
                return  # Requested backup not found, fail silently
        else:
            target = backups[0]

        source = os.path.join(backup_dir, target)
        dest = os.path.join(state_dir, "config.json")
        shutil.copy2(source, dest)
    except Exception:
        pass  # Rollback failed, user is stuck with broken config
```

The rollback function also silently fails. If the user's config is corrupted and they try to rollback, a failure here means data loss with no error message.

**Verify:** `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` passes.
