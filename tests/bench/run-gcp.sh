#!/usr/bin/env bash
# run-gcp.sh — fan-out BOI bench across preemptible GCE VMs (one per condition).
#
# Usage: ./run-gcp.sh <experiment-slug> [output-dir]
#
# Required env:
#   GCP_PROJECT          — GCP project ID (script exits quietly if unset)
#   ANTHROPIC_API_KEY    — forwarded to container via VM metadata
#   OPENROUTER_API_KEY   — forwarded to container via VM metadata
#
# Optional env:
#   GCP_REGION    — defaults to us-central1
#   GCP_ZONE      — defaults to ${GCP_REGION}-a
#   MACHINE_TYPE  — defaults to e2-standard-2
#   MAX_COST_USD  — refuse if estimated cost exceeds this (default: 10)
#
# Cost model: spot price(MACHINE_TYPE) × estimated max runtime × N conditions.
#   e2-standard-2 spot ≈ $0.021/hr; assumed max runtime = 2 hr → guard per-exp.
#
# Results: gs://${GCP_PROJECT}-boi-bench/run-{ts}/{condition}/results.json
#   Local aggregate written to <output-dir>/<slug>.json

set -uo pipefail

# ── Soft exit if GCP_PROJECT is unset (task is optional) ─────────────────────
if [[ -z "${GCP_PROJECT:-}" ]]; then
    echo "GCP_PROJECT is unset — skipping GCP fan-out (optional task)." >&2
    exit 0
fi

SLUG="${1:?Usage: $0 <experiment-slug> [output-dir]}"
OUT_DIR="${2:-/tmp/boi-bench-gcp-out/${SLUG}}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BOI_SRC="$(cd "$SCRIPT_DIR/../.." && pwd)"

GCP_REGION="${GCP_REGION:-us-central1}"
GCP_ZONE="${GCP_ZONE:-${GCP_REGION}-a}"
MACHINE_TYPE="${MACHINE_TYPE:-e2-standard-2}"
MAX_COST_USD="${MAX_COST_USD:-10}"
MAX_RUNTIME_HR=2
POLL_INTERVAL_SECS=30

TS="$(date -u +%Y%m%dT%H%M%SZ)"
BUCKET="${GCP_PROJECT}-boi-bench"
GCS_RUN="gs://${BUCKET}/run-${TS}"

SHORT_SHA="$(git -C "$BOI_SRC" rev-parse --short HEAD 2>/dev/null || echo "dev")"
GIT_SHA="$(git -C "$BOI_SRC" rev-parse HEAD 2>/dev/null || echo "unknown")"
REGISTRY="${GCP_REGION}-docker.pkg.dev/${GCP_PROJECT}/boi-bench"
REMOTE_IMAGE="${REGISTRY}/boi-bench:${SHORT_SHA}"

echo "==> GCP project:  ${GCP_PROJECT}"
echo "==> Zone:         ${GCP_ZONE}"
echo "==> Machine type: ${MACHINE_TYPE}"
echo "==> GCS run dir:  ${GCS_RUN}"
echo "==> Remote image: ${REMOTE_IMAGE}"

# ── Discover pipeline conditions ──────────────────────────────────────────────
CONDITIONS=()
for toml in "$BOI_SRC/pipelines/experiment-${SLUG}-"*.toml; do
    [[ -f "$toml" ]] || continue
    CONDITIONS+=("$(basename "$toml" .toml)")
done

if [[ ${#CONDITIONS[@]} -eq 0 ]]; then
    echo "FATAL: no pipeline TOMLs found for slug '${SLUG}'" >&2
    echo "       Looked in: $BOI_SRC/pipelines/experiment-${SLUG}-*.toml" >&2
    exit 1
fi

N=${#CONDITIONS[@]}
echo "==> Conditions (${N}): ${CONDITIONS[*]}"

# ── Cost guard ────────────────────────────────────────────────────────────────
# Spot prices (USD/hr) for common machine types.  Extend as needed.
declare -A SPOT_PRICES=(
    ["e2-standard-2"]="0.021"
    ["e2-standard-4"]="0.042"
    ["e2-highmem-2"]="0.027"
    ["n2-standard-2"]="0.035"
    ["n2-standard-4"]="0.070"
    ["n2d-standard-2"]="0.030"
)
PRICE_PER_HR="${SPOT_PRICES[$MACHINE_TYPE]:-0.050}"

ESTIMATED_COST=$(python3 -c "
n=${N}; p=float('${PRICE_PER_HR}'); t=${MAX_RUNTIME_HR}; limit=float('${MAX_COST_USD}')
cost = n * p * t
print(f'{cost:.2f}')
")

echo "==> Cost estimate: \$${ESTIMATED_COST}  (${N} VMs × \$${PRICE_PER_HR}/hr spot × ${MAX_RUNTIME_HR}hr max)"

python3 -c "
import sys
cost = float('${ESTIMATED_COST}')
limit = float('${MAX_COST_USD}')
if cost > limit:
    print(f'FATAL: estimated cost \${cost:.2f} exceeds \${limit:.2f} limit.', file=sys.stderr)
    print(f'       Options: fewer conditions, smaller machine type, or raise MAX_COST_USD.', file=sys.stderr)
    sys.exit(1)
"

echo "==> Cost guard passed (\$${ESTIMATED_COST} ≤ \$${MAX_COST_USD})"

# ── Build + push image to Artifact Registry ───────────────────────────────────
LOCAL_IMAGE="boi-bench:${SHORT_SHA}"

echo "==> Building ${LOCAL_IMAGE}..."
docker build \
    -t "$LOCAL_IMAGE" \
    -f "$SCRIPT_DIR/Dockerfile" \
    "$BOI_SRC"

IMAGE_HASH=$(docker inspect --format='{{.Id}}' "$LOCAL_IMAGE")
echo "==> Image hash: ${IMAGE_HASH:0:16}"

# Ensure AR repository exists
if ! gcloud artifacts repositories describe boi-bench \
        --location="$GCP_REGION" \
        --project="$GCP_PROJECT" \
        --quiet 2>/dev/null; then
    echo "==> Creating Artifact Registry repository 'boi-bench'..."
    gcloud artifacts repositories create boi-bench \
        --repository-format=docker \
        --location="$GCP_REGION" \
        --project="$GCP_PROJECT" \
        --quiet
fi

gcloud auth configure-docker "${GCP_REGION}-docker.pkg.dev" --quiet

echo "==> Tagging → ${REMOTE_IMAGE}"
docker tag "$LOCAL_IMAGE" "$REMOTE_IMAGE"

echo "==> Pushing ${REMOTE_IMAGE}..."
docker push "$REMOTE_IMAGE"

# ── Provision GCS bucket + upload BOI source snapshot ────────────────────────
if ! gsutil ls "gs://${BUCKET}" >/dev/null 2>&1; then
    echo "==> Creating GCS bucket gs://${BUCKET}..."
    gsutil mb -p "$GCP_PROJECT" -l "$GCP_REGION" "gs://${BUCKET}"
fi

echo "==> Uploading BOI source snapshot to ${GCS_RUN}/src/..."
gsutil -m rsync -r -x '\.git|/target/' "$BOI_SRC/" "${GCS_RUN}/src/"

# ── Build per-VM startup script (written to a temp file for gcloud) ───────────
STARTUP_TMP="$(mktemp /tmp/boi-bench-startup-XXXXXX.sh)"
trap 'rm -f "$STARTUP_TMP"' EXIT

cat > "$STARTUP_TMP" <<'STARTUP_EOF'
#!/bin/bash
set -uo pipefail

meta() {
    curl -sf "http://metadata.google.internal/computeMetadata/v1/instance/attributes/$1" \
         -H 'Metadata-Flavor: Google'
}

CONDITION="$(meta condition)"
SLUG="$(meta slug)"
REMOTE_IMAGE="$(meta remote_image)"
GCS_RUN="$(meta gcs_run)"
GCS_SRC="$(meta gcs_src)"
ANTHROPIC_API_KEY="$(meta anthropic_api_key)"
OPENROUTER_API_KEY="$(meta openrouter_api_key)"
GCP_PROJECT="$(meta gcp_project)"
GCP_REGION="$(meta gcp_region)"
INSTANCE_ZONE="$(curl -sf 'http://metadata.google.internal/computeMetadata/v1/instance/zone' \
    -H 'Metadata-Flavor: Google' | cut -d/ -f4)"
INSTANCE_NAME="$(curl -sf 'http://metadata.google.internal/computeMetadata/v1/instance/name' \
    -H 'Metadata-Flavor: Google')"

LOG_FILE="/tmp/bench-${CONDITION}.log"
GCS_LOG="${GCS_RUN}/${CONDITION}/bench.log"
GCS_RESULT="${GCS_RUN}/${CONDITION}/results.json"
GCS_DONE="${GCS_RUN}/${CONDITION}/.done"

log() { echo "[$(date -u +%H:%M:%S)] $*" | tee -a "$LOG_FILE"; }

log "VM booted: instance=${INSTANCE_NAME} condition=${CONDITION} slug=${SLUG}"

# Install docker if missing (Debian 12 base image doesn't include it)
if ! command -v docker &>/dev/null; then
    log "Installing docker..."
    apt-get update -q
    apt-get install -y -q \
        docker.io apt-transport-https ca-certificates curl gnupg 2>&1 | tee -a "$LOG_FILE"
    systemctl enable --now docker
fi

# Auth docker for Artifact Registry
gcloud auth configure-docker "${GCP_REGION}-docker.pkg.dev" --quiet 2>&1 | tee -a "$LOG_FILE"

log "Pulling image: ${REMOTE_IMAGE}"
docker pull "$REMOTE_IMAGE" 2>&1 | tee -a "$LOG_FILE"

# Download BOI source snapshot (pipelines, bench_specs, phases)
log "Downloading BOI source snapshot from ${GCS_SRC}..."
mkdir -p /opt/boi
gsutil -m rsync -r "${GCS_SRC}" /opt/boi/ 2>&1 | tee -a "$LOG_FILE"

mkdir -p /tmp/boi-out

PIPELINE_TOML="/opt/boi/pipelines/${CONDITION}.toml"

if [[ ! -f "$PIPELINE_TOML" ]]; then
    log "FATAL: pipeline TOML not found: ${PIPELINE_TOML}"
    echo "{\"error\":\"pipeline TOML not found\",\"condition\":\"${CONDITION}\"}" \
        > /tmp/boi-out/results-raw.json
else
    log "Running bench container for condition: ${CONDITION}"
    docker run --rm \
        -v /opt/boi:/opt/boi:ro \
        -v /tmp/boi-out:/out \
        -e "ANTHROPIC_API_KEY=${ANTHROPIC_API_KEY}" \
        -e "OPENROUTER_API_KEY=${OPENROUTER_API_KEY}" \
        "$REMOTE_IMAGE" \
        --battery /opt/boi/tests/bench_specs \
        --pipeline "${CONDITION}:/opt/boi/pipelines/${CONDITION}.toml" \
        --runs 1 \
        --json \
        2>&1 | tee -a "$LOG_FILE"

    # Rename output to raw (run-local.sh writes <slug>.json)
    if [[ -f "/tmp/boi-out/${SLUG}.json" ]]; then
        mv "/tmp/boi-out/${SLUG}.json" /tmp/boi-out/results-raw.json
    else
        echo '{}' > /tmp/boi-out/results-raw.json
    fi
fi

# Annotate with VM + run metadata
TIMESTAMP="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
python3 - /tmp/boi-out/results-raw.json /tmp/boi-out/results.json \
    "$CONDITION" "$SLUG" "$INSTANCE_NAME" "$TIMESTAMP" <<'PYEOF'
import sys, json, os
in_path, out_path, condition, slug, instance, timestamp = sys.argv[1:7]
try:
    with open(in_path) as f:
        data = json.load(f)
except Exception:
    data = {}
data.update({
    "condition": condition,
    "experiment_slug": slug,
    "gce_instance": instance,
    "timestamp": timestamp,
    "run_type": "gcp",
})
tmp = out_path + ".tmp"
with open(tmp, "w") as f:
    json.dump(data, f, indent=2)
    f.write("\n")
os.rename(tmp, out_path)
PYEOF

# Upload results + log to GCS
log "Uploading results to ${GCS_RESULT}..."
gsutil cp /tmp/boi-out/results.json "$GCS_RESULT" 2>&1 | tee -a "$LOG_FILE"
gsutil cp "$LOG_FILE" "$GCS_LOG" 2>&1 | tee -a "$LOG_FILE"

# Write done marker so the local poller knows this condition finished
echo "done" | gsutil cp - "$GCS_DONE"

log "Done. Self-deleting instance ${INSTANCE_NAME} in ${INSTANCE_ZONE}..."
gcloud compute instances delete "$INSTANCE_NAME" \
    --zone="$INSTANCE_ZONE" \
    --project="$GCP_PROJECT" \
    --quiet 2>&1 | tee -a "$LOG_FILE" || true
STARTUP_EOF

# ── Launch one preemptible VM per condition ───────────────────────────────────
declare -A VM_NAMES=()

for CONDITION in "${CONDITIONS[@]}"; do
    # VM names: lowercase, alphanumeric + hyphens, max 63 chars
    RAW_NAME="boi-bench-${SLUG}-${CONDITION}-${TS}"
    VM_NAME="$(echo "${RAW_NAME}" | tr '[:upper:]_.' '[:lower:]--' | tr -cd 'a-z0-9-' | cut -c1-63)"
    VM_NAMES["$CONDITION"]="$VM_NAME"

    echo "==> Launching ${VM_NAME} for condition '${CONDITION}'..."
    gcloud compute instances create "$VM_NAME" \
        --project="$GCP_PROJECT" \
        --zone="$GCP_ZONE" \
        --machine-type="$MACHINE_TYPE" \
        --provisioning-model=SPOT \
        --instance-termination-action=DELETE \
        --image-family=debian-12 \
        --image-project=debian-cloud \
        --boot-disk-size=20GB \
        --boot-disk-type=pd-standard \
        --scopes=cloud-platform \
        --metadata="condition=${CONDITION},slug=${SLUG},remote_image=${REMOTE_IMAGE},gcs_run=${GCS_RUN},gcs_src=${GCS_RUN}/src,anthropic_api_key=${ANTHROPIC_API_KEY:-},openrouter_api_key=${OPENROUTER_API_KEY:-},gcp_project=${GCP_PROJECT},gcp_region=${GCP_REGION}" \
        --metadata-from-file=startup-script="$STARTUP_TMP" \
        --quiet
done

echo ""
echo "==> ${N} VM(s) launched. Polling for completion (timeout: $((MAX_RUNTIME_HR * 60)) min)..."

# ── Poll GCS for per-condition completion markers ─────────────────────────────
TIMEOUT_SECS=$((MAX_RUNTIME_HR * 3600))
START_SECS=$SECONDS
declare -a DONE_CONDITIONS=()

while true; do
    ELAPSED=$((SECONDS - START_SECS))

    if [[ $ELAPSED -gt $TIMEOUT_SECS ]]; then
        echo "WARNING: timed out after ${MAX_RUNTIME_HR}hr — not all conditions finished." >&2
        MISSING=()
        for c in "${CONDITIONS[@]}"; do
            [[ " ${DONE_CONDITIONS[*]:-} " =~ " ${c} " ]] || MISSING+=("$c")
        done
        echo "         Missing: ${MISSING[*]}" >&2
        break
    fi

    for CONDITION in "${CONDITIONS[@]}"; do
        [[ " ${DONE_CONDITIONS[*]:-} " =~ " ${CONDITION} " ]] && continue
        if gsutil ls "${GCS_RUN}/${CONDITION}/.done" >/dev/null 2>&1; then
            echo "    [done] ${CONDITION}  (${ELAPSED}s elapsed)"
            DONE_CONDITIONS+=("$CONDITION")
        fi
    done

    if [[ ${#DONE_CONDITIONS[@]} -eq $N ]]; then
        echo "==> All ${N} condition(s) complete."
        break
    fi

    REMAINING=$((N - ${#DONE_CONDITIONS[@]}))
    echo "    Waiting... ${REMAINING}/${N} pending  (${ELAPSED}s elapsed)"
    sleep "$POLL_INTERVAL_SECS"
done

# ── Download + aggregate results locally ──────────────────────────────────────
mkdir -p "$OUT_DIR"
AGGREGATE_JSON="${OUT_DIR}/${SLUG}.json"
AGGREGATE_LOG="${OUT_DIR}/${SLUG}.log"

echo "" | tee -a "$AGGREGATE_LOG"
echo "==> Downloading results from ${GCS_RUN}/..." | tee -a "$AGGREGATE_LOG"

RESULT_FILES=()
for CONDITION in "${DONE_CONDITIONS[@]}"; do
    DEST="${OUT_DIR}/${CONDITION}-results.json"
    if gsutil cp "${GCS_RUN}/${CONDITION}/results.json" "$DEST" 2>/dev/null; then
        RESULT_FILES+=("$DEST")
        echo "    Downloaded: ${CONDITION}" | tee -a "$AGGREGATE_LOG"
        gsutil cp "${GCS_RUN}/${CONDITION}/bench.log" "${OUT_DIR}/${CONDITION}.log" 2>/dev/null || true
    else
        echo "    WARNING: no results for '${CONDITION}'" | tee -a "$AGGREGATE_LOG" >&2
    fi
done

# Aggregate all condition results into a single JSON (same shape as run-local.sh)
python3 - "$AGGREGATE_JSON" "$SLUG" "$GIT_SHA" "$IMAGE_HASH" "$TS" "${RESULT_FILES[@]:-}" <<'PYEOF'
import sys, json, os

out_path, slug, git_sha, image_hash, ts, *result_files = sys.argv[1:]

conditions = {}
for rfile in result_files:
    if not rfile:
        continue
    try:
        with open(rfile) as f:
            data = json.load(f)
        cond = data.get("condition", os.path.basename(rfile).replace("-results.json", ""))
        conditions[cond] = data
    except Exception as e:
        conditions[os.path.basename(rfile)] = {"error": str(e)}

aggregate = {
    "experiment_slug": slug,
    "git_sha": git_sha,
    "docker_image_hash": image_hash,
    "run_timestamp": ts,
    "run_type": "gcp",
    "conditions": conditions,
}
tmp = out_path + ".tmp"
with open(tmp, "w") as f:
    json.dump(aggregate, f, indent=2)
    f.write("\n")
os.rename(tmp, out_path)
print(f"==> Aggregate result: {out_path}")
PYEOF

echo ""
echo "==> GCP fan-out complete for experiment '${SLUG}'."
echo "    Conditions run:  ${#DONE_CONDITIONS[@]} / ${N}"
echo "    GCS results:     ${GCS_RUN}/"
echo "    Local aggregate: ${AGGREGATE_JSON}"
echo "    Local log:       ${AGGREGATE_LOG}"
