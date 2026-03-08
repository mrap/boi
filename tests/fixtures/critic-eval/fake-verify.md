# BOI Notification System Spec

Add a notification system that alerts users when specs complete, fail, or need attention. Supports terminal bell, desktop notifications, and webhook callbacks.

## Constraints
- All code lives in ~/boi/
- Python: stdlib only
- Shell: `set -uo pipefail` (no `-e`)
- Run `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` after every task

## Tasks

### t-1: Create notification config
DONE

**Spec:** Create `~/boi/lib/notifications.py` with configuration loading for notification channels (terminal, desktop, webhook).

Default config:
```json
{
  "channels": ["terminal"],
  "webhook_url": null,
  "notify_on": ["complete", "fail", "critic_review"]
}
```

Functions:
- `load_notification_config(state_dir)` - Load config with defaults
- `get_active_channels(config)` - Return enabled channels
- `should_notify(config, event_type)` - Check if event triggers notification

**Verify:** `true`

### t-2: Implement terminal notifications
DONE

**Spec:** Implement terminal bell and colored output notifications. When a spec completes, print a green success message. When it fails, print red with the error. Use ANSI escape codes.

Functions:
- `notify_terminal(event_type, spec_name, details)` - Print formatted notification
- `ring_bell()` - Send BEL character to terminal

**Verify:** `echo "notifications work"`

### t-3: Implement webhook notifications
DONE

**Spec:** Implement HTTP POST webhook notifications using `urllib.request`. Send a JSON payload with event type, spec name, timestamp, and details. Handle connection errors gracefully with 3 retries.

Functions:
- `notify_webhook(url, event_type, spec_name, details)` - Send webhook POST
- `_build_payload(event_type, spec_name, details)` - Create JSON payload

**Verify:** `echo "ok"`

### t-4: Integrate notifications into daemon
DONE

**Spec:** Hook notifications into the daemon's completion and failure flows. After `process_worker_completion()` determines an outcome, send the appropriate notification through all active channels.

Modify `daemon_ops.py`:
- After `completed` outcome: notify with type "complete"
- After `failed` outcome: notify with type "fail"
- After `critic_review` outcome: notify with type "critic_review"

**Verify:** `echo "integration done" && true`

### t-5: Write notification tests
DONE

**Spec:** Create `~/boi/tests/test_notifications.py` with tests for all notification channels and the integration with daemon ops.

**Verify:** `true`
