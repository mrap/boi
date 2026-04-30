#!/usr/bin/env python3
"""autoresearch-propose.py — LLM-driven hypothesis generator for BOI pipeline variants.

Reads latest bench results + current default pipeline + variant history,
calls OpenRouter to propose ONE new variant TOML, and writes it out.

Inputs:
  /tmp/bench-latest.json       — latest bench results (override via BENCH_RESULTS)
  phases/pipelines.toml        — current default pipeline config
  pipelines/variants/          — history of past variants + verdicts

Outputs:
  pipelines/variants/v-{ts}.toml         — proposed variant
  pipelines/variants/v-{ts}.rationale.md — 3-sentence rationale

Environment:
  OPENROUTER_API_KEY    — required
  BOI_ROOT              — optional, defaults to script's grandparent dir
  BENCH_RESULTS         — optional, overrides /tmp/bench-latest.json path
  AUTORESEARCH_MODEL    — optional, overrides default model (google/gemini-flash-1.5)
"""

import json
import os
import re
import sys
import urllib.request
import urllib.error
from datetime import datetime, timezone
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
BOI_ROOT = Path(os.environ.get("BOI_ROOT", SCRIPT_DIR.parent))
VARIANTS_DIR = BOI_ROOT / "pipelines" / "variants"
ARCHIVE_DIR = VARIANTS_DIR / "archive"
LOG_FILE = VARIANTS_DIR / "log.md"
BENCH_RESULTS = Path(os.environ.get("BENCH_RESULTS", "/tmp/bench-latest.json"))
TELEMETRY_PATH = Path(os.path.expanduser("~")) / "mrap-hex" / ".hex" / "telemetry"

OPENROUTER_URL = "https://openrouter.ai/api/v1/chat/completions"
MODEL = os.environ.get("AUTORESEARCH_MODEL", "google/gemini-flash-1.5")
MAX_SAME_AXIS_FAILS = 3

KNOWN_PHASES = [
    "critic", "plan-critique", "spec-critique", "spec-improve",
    "evaluate", "decompose", "execute", "code-review", "task-verify",
    "review", "commit", "doc-update", "merge", "cleanup",
]

OPTIMIZATION_AXES = [
    "phase_ordering",
    "phase_selection",
    "model_assignment",
    "timeout_policy",
    "effort_level",
    "conditional_skipping",
    "judgment_strategy",
]


def emit_telemetry(event_type: str, payload: dict) -> None:
    try:
        sys.path.insert(0, str(TELEMETRY_PATH))
        from emit import emit
        emit(event_type, payload, source="autoresearch-propose")
    except Exception as exc:
        print(f"[telemetry] WARN: {exc}", file=sys.stderr)


def load_bench_results() -> dict:
    if not BENCH_RESULTS.exists():
        print(f"FATAL: bench results not found at {BENCH_RESULTS}", file=sys.stderr)
        sys.exit(1)
    with open(BENCH_RESULTS) as f:
        return json.load(f)


def load_current_default() -> str:
    default_path = BOI_ROOT / "phases" / "pipelines.toml"
    if not default_path.exists():
        print(f"FATAL: default pipeline config not found at {default_path}", file=sys.stderr)
        sys.exit(1)
    return default_path.read_text()


def load_variant_history() -> list[dict]:
    history = []
    for md in sorted(VARIANTS_DIR.glob("v-*.rationale.md")):
        ts = md.stem.replace(".rationale", "").replace("v-", "")
        toml_path = VARIANTS_DIR / f"v-{ts}.toml"
        archived = not toml_path.exists() and (ARCHIVE_DIR / f"v-{ts}.toml").exists()
        verdict = "FAIL" if archived else ("PASS" if toml_path.exists() else "UNKNOWN")
        rationale = md.read_text().strip()
        history.append({
            "timestamp": ts,
            "verdict": verdict,
            "rationale": rationale,
        })
    return history


def load_axis_fail_counts() -> dict[str, int]:
    counts: dict[str, int] = {}
    if not LOG_FILE.exists():
        return counts
    current_axis = None
    for line in LOG_FILE.read_text().splitlines():
        m = re.match(r"^### .+ — FAIL \(axis: (.+)\)", line)
        if m:
            current_axis = m.group(1).strip()
            counts[current_axis] = counts.get(current_axis, 0) + 1
    return counts


def find_blocked_axes(fail_counts: dict[str, int]) -> list[str]:
    return [axis for axis, count in fail_counts.items() if count >= MAX_SAME_AXIS_FAILS]


def build_system_prompt(blocked_axes: list[str]) -> str:
    blocked_clause = ""
    if blocked_axes:
        blocked_clause = (
            "\n\nBLOCKED AXES (3+ consecutive failures — do NOT propose changes on these):\n"
            + "\n".join(f"  - {a}" for a in blocked_axes)
            + "\n\nInstead, pick from these axes: "
            + ", ".join(a for a in OPTIMIZATION_AXES if a not in blocked_axes)
        )

    return f"""You are tuning a BOI pipeline. Given the current default config and recent
bench results, propose ONE variant that might improve wall_time by ≥10% while
maintaining completion_rate and keeping cost within 5% of baseline.

Output EXACTLY two sections separated by "---":

SECTION 1: A valid TOML pipeline config. It MUST contain a [pipeline] table with:
  name = "v-{{timestamp}}"   (use the timestamp provided)
  spec_phases = [...]
  task_phases = [...]
  post_phases = []

SECTION 2: A 3-sentence rationale explaining:
  1. What optimization axis you're targeting
  2. What specific change you made
  3. Why you expect it to improve wall_time

Available phases: {', '.join(KNOWN_PHASES)}

HARD CONSTRAINTS:
  - Never change v1 (it's the legacy control)
  - Never modify the execute or task-verify base prompts (high blast radius)
  - task_phases MUST always include "execute" and "task-verify"
  - Only change phase ordering, phase selection, or config knobs
  - The variant must be a standalone TOML file{blocked_clause}"""


def build_user_prompt(
    bench_results: dict,
    default_config: str,
    variant_history: list[dict],
    timestamp: str,
) -> str:
    history_text = "None yet."
    if variant_history:
        entries = []
        for v in variant_history[-10:]:
            entries.append(f"  - {v['timestamp']}: {v['verdict']} — {v['rationale'][:120]}")
        history_text = "\n".join(entries)

    bench_summary = json.dumps(bench_results, indent=2)
    if len(bench_summary) > 4000:
        bench_summary = bench_summary[:4000] + "\n... (truncated)"

    return f"""TIMESTAMP FOR THIS VARIANT: {timestamp}

CURRENT DEFAULT PIPELINE CONFIG:
```toml
{default_config}
```

RECENT BENCH RESULTS:
```json
{bench_summary}
```

VARIANT HISTORY (most recent 10):
{history_text}

Propose exactly one variant. Output the TOML first, then "---", then the rationale."""


def call_openrouter(system_prompt: str, user_prompt: str) -> str:
    api_key = os.environ.get("OPENROUTER_API_KEY")
    if not api_key:
        print("FATAL: OPENROUTER_API_KEY not set", file=sys.stderr)
        sys.exit(1)

    body = json.dumps({
        "model": MODEL,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_prompt},
        ],
        "temperature": 0.7,
        "max_tokens": 2048,
    }).encode()

    req = urllib.request.Request(
        OPENROUTER_URL,
        data=body,
        headers={
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
            "HTTP-Referer": "https://github.com/mrap/boi",
            "X-Title": "BOI Autoresearch",
        },
    )

    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            data = json.loads(resp.read())
            return data["choices"][0]["message"]["content"]
    except urllib.error.HTTPError as e:
        body_text = e.read().decode() if e.fp else ""
        print(f"FATAL: OpenRouter API error {e.code}: {body_text}", file=sys.stderr)
        sys.exit(1)
    except Exception as e:
        print(f"FATAL: OpenRouter request failed: {e}", file=sys.stderr)
        sys.exit(1)


def parse_response(response: str, timestamp: str) -> tuple[str, str]:
    parts = re.split(r"\n---+\n", response, maxsplit=1)
    if len(parts) < 2:
        parts = response.split("---", 1)
    if len(parts) < 2:
        print("FATAL: LLM response missing --- separator between TOML and rationale", file=sys.stderr)
        print(f"Response:\n{response}", file=sys.stderr)
        sys.exit(1)

    toml_raw = parts[0].strip()
    rationale = parts[1].strip()

    toml_raw = re.sub(r"^```toml\s*\n?", "", toml_raw)
    toml_raw = re.sub(r"\n?```\s*$", "", toml_raw)

    if "[pipeline]" not in toml_raw:
        print("FATAL: generated TOML missing [pipeline] section", file=sys.stderr)
        print(f"TOML:\n{toml_raw}", file=sys.stderr)
        sys.exit(1)

    toml_raw = re.sub(
        r'name\s*=\s*"[^"]*"',
        f'name = "v-{timestamp}"',
        toml_raw,
        count=1,
    )

    if "task_phases" in toml_raw:
        if '"execute"' not in toml_raw or '"task-verify"' not in toml_raw:
            print("FATAL: variant must include execute and task-verify in task_phases", file=sys.stderr)
            sys.exit(1)

    rationale = re.sub(r"^```\w*\s*\n?", "", rationale)
    rationale = re.sub(r"\n?```\s*$", "", rationale)

    return toml_raw, rationale


def detect_axis(toml_text: str, rationale: str) -> str:
    combined = (toml_text + " " + rationale).lower()
    for axis in OPTIMIZATION_AXES:
        if axis.replace("_", " ") in combined or axis.replace("_", "-") in combined:
            return axis
    if any(kw in combined for kw in ["order", "reorder", "before", "after", "sequence"]):
        return "phase_ordering"
    if any(kw in combined for kw in ["add phase", "remove phase", "skip", "fewer"]):
        return "phase_selection"
    if any(kw in combined for kw in ["model", "haiku", "gemini", "flash", "opus"]):
        return "model_assignment"
    if any(kw in combined for kw in ["timeout", "kill", "deadline"]):
        return "timeout_policy"
    return "unknown"


def main() -> None:
    VARIANTS_DIR.mkdir(parents=True, exist_ok=True)
    ARCHIVE_DIR.mkdir(parents=True, exist_ok=True)

    timestamp = datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")

    bench_results = load_bench_results()
    default_config = load_current_default()
    variant_history = load_variant_history()
    fail_counts = load_axis_fail_counts()
    blocked_axes = find_blocked_axes(fail_counts)

    if blocked_axes:
        print(f"Blocked axes (≥{MAX_SAME_AXIS_FAILS} fails): {', '.join(blocked_axes)}")

    available_axes = [a for a in OPTIMIZATION_AXES if a not in blocked_axes]
    if not available_axes:
        print("FATAL: all optimization axes are blocked. Manual intervention needed.", file=sys.stderr)
        emit_telemetry("boi.autoresearch.propose.exhausted", {
            "blocked_axes": blocked_axes,
            "fail_counts": fail_counts,
        })
        sys.exit(1)

    system_prompt = build_system_prompt(blocked_axes)
    user_prompt = build_user_prompt(bench_results, default_config, variant_history, timestamp)

    print(f"Proposing variant v-{timestamp} via {MODEL}...")
    response = call_openrouter(system_prompt, user_prompt)

    toml_text, rationale = parse_response(response, timestamp)
    axis = detect_axis(toml_text, rationale)

    toml_path = VARIANTS_DIR / f"v-{timestamp}.toml"
    rationale_path = VARIANTS_DIR / f"v-{timestamp}.rationale.md"

    header = f"# Variant v-{timestamp}\n\n**Axis:** {axis}\n\n"
    toml_path.write_text(toml_text + "\n")
    rationale_path.write_text(header + rationale + "\n")

    print(f"Wrote variant:   {toml_path}")
    print(f"Wrote rationale: {rationale_path}")
    print(f"Detected axis:   {axis}")

    emit_telemetry("boi.autoresearch.propose", {
        "variant": f"v-{timestamp}",
        "axis": axis,
        "model": MODEL,
        "blocked_axes": blocked_axes,
    })


if __name__ == "__main__":
    main()
