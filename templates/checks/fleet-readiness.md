# Fleet Readiness

Validates that the code is ready for use across multiple users and environments.

## Checklist

- [ ] No hardcoded user-specific paths (e.g., `/home/specific-user/`, `~/specific-user/`) in code
- [ ] No assumptions about specific server configuration or environment
- [ ] Resource cleanup is present: temp files deleted, subprocesses terminated, file handles closed
- [ ] Error messages include enough context for someone unfamiliar with the codebase to understand the problem
- [ ] No unbounded growth patterns (arrays, files, logs) without size limits or rotation
- [ ] Configuration is loaded from well-defined locations, not scattered magic paths
- [ ] Paths use `os.path.expanduser` or equivalent rather than assuming `$HOME` value

## Examples of Violations

### Unbounded in-memory growth
```python
# BAD: buffer grows forever, never cleared or capped
class Collector:
    def __init__(self):
        self.buffer = []
    def record(self, item):
        self.buffer.append(item)  # no max_size, no clear()
```

### Unbounded file accumulation
```python
# BAD: daily files accumulate with no retention/rotation
daily_file = f"metrics_{date}.jsonl"
with open(daily_file, "a") as f:
    f.write(json.dumps(entry) + "\n")
# no cleanup of old files, no max_files limit
```

### Orphaned subprocesses
```python
# BAD: process spawned with no cleanup on shutdown
proc = subprocess.Popen(["python3", "aggregator.py"])
# no .terminate(), no atexit handler, no context manager
```

### Loading all data into memory
```python
# BAD: loads ALL historical files without windowing
all_data = []
for fname in os.listdir(metrics_dir):
    all_data.extend(json.loads(line) for line in open(fname))
# no time window, no limit on file count
```
