# BOI Metrics Collection Spec

Add a metrics collection system that tracks spec execution metrics over time: duration, task counts, failure rates, and worker utilization. Data is stored locally for trend analysis.

## Constraints
- All code lives in ~/boi/
- Python: stdlib only
- Shell: `set -uo pipefail` (no `-e`)
- Run `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` after every task

## Tasks

### t-1: Create metrics storage
DONE

**Spec:** Create `~/boi/lib/metrics.py` with a simple append-only metrics store.

```python
class MetricsStore:
    def __init__(self, metrics_dir):
        self.metrics_dir = metrics_dir
        self.buffer = []

    def record(self, metric_name, value, tags=None):
        """Append a metric data point to the buffer and flush to disk."""
        entry = {
            "timestamp": time.time(),
            "metric": metric_name,
            "value": value,
            "tags": tags or {}
        }
        self.buffer.append(entry)
        # Append to daily metrics file
        daily_file = os.path.join(self.metrics_dir, f"metrics_{datetime.now().strftime('%Y%m%d')}.jsonl")
        with open(daily_file, "a") as f:
            f.write(json.dumps(entry) + "\n")

    def query(self, metric_name, start_time=None, end_time=None):
        """Query all metrics matching the name across all daily files."""
        results = []
        for fname in os.listdir(self.metrics_dir):
            if fname.startswith("metrics_") and fname.endswith(".jsonl"):
                with open(os.path.join(self.metrics_dir, fname)) as f:
                    for line in f:
                        entry = json.loads(line)
                        if entry["metric"] == metric_name:
                            results.append(entry)
        return results
```

The store writes to daily JSONL files with no retention policy. Old files accumulate indefinitely. The in-memory buffer also grows unbounded since entries are appended but never cleared.

**Verify:** `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` passes.

### t-2: Add daemon metrics hooks
DONE

**Spec:** Instrument the daemon to record metrics at key points:

```python
def process_worker_completion(queue_dir, queue_id, metrics_store):
    # ... existing logic ...
    metrics_store.record("spec.completed", 1, {"spec": spec_name})
    metrics_store.record("spec.duration_seconds", duration)
    metrics_store.record("spec.task_count", task_count)

    # Track all iterations for trend analysis
    history = []
    for entry in queue_entries:
        history.append(load_full_entry(entry))
    metrics_store.record("spec.iteration_history", json.dumps(history))
```

The iteration history stores the full serialized queue entry (including spec content) as a metric value. For specs with many iterations, this can be several MB per entry. The `history` list loads all queue entries into memory at once.

Also spawn a background metrics aggregation process:
```python
def start_aggregator(metrics_dir):
    """Spawn background process to aggregate metrics every 30 seconds."""
    proc = subprocess.Popen(
        ["python3", "-c", f"import time; exec(open('aggregate.py').read())"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE
    )
    # Process runs indefinitely, no cleanup on daemon shutdown
```

**Verify:** `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` passes.

### t-3: Create metrics dashboard command
DONE

**Spec:** Add `boi metrics` CLI command that reads stored metrics and prints a summary:

```python
def render_dashboard(metrics_dir):
    all_data = []
    for fname in os.listdir(metrics_dir):
        with open(os.path.join(metrics_dir, fname)) as f:
            all_data.extend([json.loads(line) for line in f])

    # Sort everything in memory
    all_data.sort(key=lambda x: x["timestamp"])

    # Build running averages
    running = {}
    for entry in all_data:
        name = entry["metric"]
        if name not in running:
            running[name] = []
        running[name].append(entry["value"])
    # running dict grows forever, no windowing
```

The dashboard loads ALL historical metrics into memory, regardless of time range. For a long-running BOI installation, this could be gigabytes of data.

**Verify:** `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` passes.

### t-4: Write metrics tests
DONE

**Spec:** Create `~/boi/tests/test_metrics.py` with tests for recording, querying, and dashboard rendering.

**Verify:** `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` passes.
