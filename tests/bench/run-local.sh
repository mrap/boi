#!/usr/bin/env bash
# run-local.sh — build + run all conditions for one experiment in a container.
#
# Usage: ./run-local.sh <experiment-slug> [output-dir]
#   e.g. ./run-local.sh coldstart /tmp/bench-out/coldstart
#
# Writes:
#   <out-dir>/<slug>.log   — full streamed container output
#   <out-dir>/<slug>.json  — annotated bench summary (pipeline metrics + metadata)
#
# Hard requirement: pgrep claude on the HOST must return nothing during the run.
# Any host-side claude process is a potential state leak and will abort the script.

set -uo pipefail

SLUG="${1:?Usage: $0 <experiment-slug> [output-dir]}"
OUT_DIR="${2:-/tmp/boi-bench-out/${SLUG}}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BOI_SRC="$(cd "$SCRIPT_DIR/../.." && pwd)"

# ── Pre-flight: no host claude processes ─────────────────────────────────────
if pgrep -x "claude" >/dev/null 2>&1; then
    echo "FATAL: host claude process detected before bench run." >&2
    echo "       Kill all local claude sessions before running the bench." >&2
    exit 1
fi
echo "==> Pre-flight: host clean (no claude processes)"

# ── Build docker image (context = BOI repo root) ─────────────────────────────
SHORT_SHA=$(git -C "$BOI_SRC" rev-parse --short HEAD 2>/dev/null || echo "dev")
GIT_SHA=$(git -C "$BOI_SRC" rev-parse HEAD 2>/dev/null || echo "unknown")
IMAGE="boi-bench:${SHORT_SHA}"

echo "==> Building $IMAGE  (context: $BOI_SRC)"
docker build \
    -t "$IMAGE" \
    -f "$SCRIPT_DIR/Dockerfile" \
    "$BOI_SRC"

IMAGE_HASH=$(docker inspect --format='{{.Id}}' "$IMAGE")
echo "==> Image: $IMAGE  hash: ${IMAGE_HASH:0:16}"

# ── Collect pipeline conditions for this experiment ──────────────────────────
PIPELINE_ARGS=()
CONDITION_NAMES=()
for toml in "$BOI_SRC/pipelines/experiment-${SLUG}-"*.toml; do
    [[ -f "$toml" ]] || continue
    arm=$(basename "$toml" .toml)
    container_path="/opt/boi/pipelines/$(basename "$toml")"
    PIPELINE_ARGS+=(--pipeline "${arm}:${container_path}")
    CONDITION_NAMES+=("$arm")
done

if [[ ${#PIPELINE_ARGS[@]} -eq 0 ]]; then
    echo "FATAL: no pipeline TOMLs found for slug '$SLUG'" >&2
    echo "       Looked in: $BOI_SRC/pipelines/experiment-${SLUG}-*.toml" >&2
    exit 1
fi

echo "==> Conditions (${#CONDITION_NAMES[@]}): ${CONDITION_NAMES[*]}"

# ── Prepare output dir ────────────────────────────────────────────────────────
mkdir -p "$OUT_DIR"
LOG_FILE="$OUT_DIR/${SLUG}.log"
RESULT_JSON="$OUT_DIR/${SLUG}.json"
echo "==> Output dir: $OUT_DIR"
echo "==> Log:        $LOG_FILE"

# ── Run bench container ───────────────────────────────────────────────────────
echo "==> Starting bench container..."
docker run --rm \
    -v "${BOI_SRC}:/opt/boi:ro" \
    -v "${OUT_DIR}:/out" \
    -e "ANTHROPIC_API_KEY=${ANTHROPIC_API_KEY:-}" \
    -e "OPENROUTER_API_KEY=${OPENROUTER_API_KEY:-}" \
    "$IMAGE" \
    --battery /opt/boi/tests/bench_specs \
    "${PIPELINE_ARGS[@]}" \
    --runs 1 \
    --json \
    2>&1 | tee "$LOG_FILE"

CONTAINER_EXIT=${PIPESTATUS[0]}

# ── Post-run: verify no host claude leak ─────────────────────────────────────
if pgrep -x "claude" >/dev/null 2>&1; then
    echo "FATAL: host claude process detected AFTER bench run." >&2
    echo "       Possible host state leak — results may be contaminated." >&2
    exit 1
fi
echo "==> Post-run:  host clean (no claude processes)"

# ── Extract JSON summary from log, annotate with run metadata ────────────────
TIMESTAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ)

python3 - "$LOG_FILE" "$RESULT_JSON" "$SLUG" "$GIT_SHA" "$IMAGE_HASH" "$TIMESTAMP" <<'PYEOF'
import sys, json, re, os

log_path, out_path, slug, git_sha, image_hash, timestamp = sys.argv[1:7]

with open(log_path) as f:
    text = f.read()

# Strip carriage-return overwrites from progress lines
text = re.sub(r'[^\n]*\r', '', text)

# Find the last JSON block that looks like a bench summary ({ ... "pipelines": ... })
data = {}
for m in reversed(list(re.finditer(r'^(\{)', text, re.MULTILINE))):
    candidate = text[m.start():]
    for end_m in re.finditer(r'^(\})', candidate, re.MULTILINE):
        chunk = candidate[:end_m.end()]
        try:
            obj = json.loads(chunk)
            if 'pipelines' in obj:
                data = obj
                break
        except Exception:
            continue
    if data:
        break

data.update({
    "experiment_slug": slug,
    "git_sha": git_sha,
    "docker_image_hash": image_hash,
    "timestamp": timestamp,
})

tmp = out_path + ".tmp"
with open(tmp, "w") as f:
    json.dump(data, f, indent=2)
    f.write("\n")
os.rename(tmp, out_path)
print(f"==> Result written: {out_path}")
PYEOF

echo ""
echo "==> Experiment '${SLUG}' done.  Exit: ${CONTAINER_EXIT}"
echo "    Log:    ${LOG_FILE}"
echo "    Result: ${RESULT_JSON}"

exit "$CONTAINER_EXIT"
