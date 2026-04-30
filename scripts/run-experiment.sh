#!/usr/bin/env bash
set -uo pipefail

# BOI Pipeline Experiment Runner
# Records analysis-plan hash, BOI config snapshot, and all conditions
# before invoking the bench harness per the pre-registered protocol.

BOI_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PIPELINES_DIR="$BOI_ROOT/pipelines"
BENCH_SPECS_DIR="$BOI_ROOT/tests/bench_specs"
ANALYSIS_PLAN="/Users/mrap/mrap-hex/projects/hex-autonomy/boi-experiments/2026-04-29-analysis-plan.md"
PROTOCOL="/Users/mrap/mrap-hex/projects/hex-autonomy/boi-experiments/2026-04-29-experiment-protocol.md"
LOG_DIR="/Users/mrap/mrap-hex/projects/hex-autonomy/boi-experiments/logs"
EXPECTED_PLAN_HASH="9f320aa0a5ddc1e0b38afda82066f3deddb0fac3bfaddb44654297766dbfb618"

usage() {
    cat <<'USAGE'
Usage: run-experiment.sh <experiment-number> [options]

Arguments:
  experiment-number   1-8 (matches protocol experiment numbers)

Options:
  --runs N            Runs per (spec, pipeline) pair (default: 1)
  --json              Output results as JSON
  --dry-run           Validate all configs without running bench
  --battery DIR       Override bench spec directory
  --phase-only PHASE  Run phase-level bench for a single phase
  --help              Show this help

Examples:
  ./scripts/run-experiment.sh 1 --runs 5 --json
  ./scripts/run-experiment.sh 5 --dry-run
  ./scripts/run-experiment.sh 1 --phase-only critic --runs 25
USAGE
    exit 0
}

log() { echo "[$(date +%Y-%m-%dT%H:%M:%S)] $*"; }
die() { log "FATAL: $*" >&2; exit 1; }

# --- Argument parsing ---
EXPERIMENT=""
RUNS=1
JSON_FLAG=""
DRY_RUN=0
BATTERY_OVERRIDE=""
PHASE_ONLY=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --runs)    RUNS="$2"; shift 2 ;;
        --json)    JSON_FLAG="--json"; shift ;;
        --dry-run) DRY_RUN=1; shift ;;
        --battery) BATTERY_OVERRIDE="$2"; shift 2 ;;
        --phase-only) PHASE_ONLY="$2"; shift 2 ;;
        --help)    usage ;;
        [1-8])     EXPERIMENT="$1"; shift ;;
        *)         die "Unknown argument: $1" ;;
    esac
done

[[ -n "$EXPERIMENT" ]] || die "Experiment number (1-8) required"

# --- Experiment-to-slug mapping ---
declare -A SLUGS=(
    [1]="coldstart"
    [2]="modelassign"
    [3]="cacheopt"
    [4]="hygiene"
    [5]="condphase"
    [6]="forkvote"
    [7]="detverify"
    [8]="timeout"
)

declare -A EXPERIMENT_NAMES=(
    [1]="Cold-Start Runtime Swap"
    [2]="Per-Phase Model Assignment"
    [3]="Prompt Cache Optimization"
    [4]="Prompt Hygiene Bundle"
    [5]="Conditional Phase Execution"
    [6]="Fork-and-Vote"
    [7]="Deterministic Pre-Verification"
    [8]="Adaptive Timeout + Early Kill"
)

SLUG="${SLUGS[$EXPERIMENT]}"
EXP_NAME="${EXPERIMENT_NAMES[$EXPERIMENT]}"

# --- Spec battery per experiment ---
declare -A BATTERIES=(
    [1]="simple.yaml multi.yaml medium_spec.yaml large_spec.yaml generate_spec.yaml"
    [2]="simple.yaml medium_spec.yaml large_spec.yaml generate_spec.yaml"
    [3]="medium_spec.yaml"
    [4]="simple.yaml medium_spec.yaml large_spec.yaml generate_spec.yaml"
    [5]="simple_generate.yaml config_only.yaml complex_refactor.yaml medium_spec.yaml no_file_change.yaml"
    [6]="simple.yaml medium_spec.yaml"
    [7]="simple.yaml multi.yaml medium_spec.yaml complex_refactor.yaml verify.yaml"
    [8]="simple.yaml medium_spec.yaml large_spec.yaml complex_refactor.yaml verify.yaml"
)

# ============================================================
# GATE 1: Verify analysis plan hash (pre-registration integrity)
# ============================================================
log "=== Experiment $EXPERIMENT: $EXP_NAME ==="
log "Verifying analysis plan hash..."

if [[ ! -f "$ANALYSIS_PLAN" ]]; then
    die "Analysis plan not found at $ANALYSIS_PLAN"
fi

ACTUAL_HASH=$(shasum -a 256 "$ANALYSIS_PLAN" | cut -d' ' -f1)
if [[ "$ACTUAL_HASH" != "$EXPECTED_PLAN_HASH" ]]; then
    die "Analysis plan hash mismatch!
  Expected: $EXPECTED_PLAN_HASH
  Actual:   $ACTUAL_HASH
  The analysis plan has been modified after pre-registration.
  If this is intentional, file a dated addendum and update EXPECTED_PLAN_HASH."
fi
log "Analysis plan hash verified: $ACTUAL_HASH"

# ============================================================
# GATE 2: BOI config snapshot (config freeze protocol §4.3)
# ============================================================
log "Recording BOI config snapshot..."

mkdir -p "$LOG_DIR"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
LOGFILE="$LOG_DIR/experiment-${EXPERIMENT}-${SLUG}-${TIMESTAMP}.log"

{
    echo "=== BOI Experiment Log ==="
    echo "experiment: $EXPERIMENT"
    echo "name: $EXP_NAME"
    echo "slug: $SLUG"
    echo "timestamp: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "analysis_plan_hash: $ACTUAL_HASH"
    echo ""
    echo "=== BOI Version ==="
    echo "commit: $(cd "$BOI_ROOT" && git rev-parse HEAD 2>/dev/null || echo 'not-a-git-repo')"
    echo "binary: $(shasum -a 256 "$(which boi 2>/dev/null || echo /dev/null)" 2>/dev/null | cut -d' ' -f1 || echo 'boi-not-installed')"
    echo ""
    echo "=== Phase Config Hashes ==="
    for f in "$BOI_ROOT"/phases/*.phase.toml; do
        echo "$(basename "$f"): $(shasum -a 256 "$f" | cut -d' ' -f1)"
    done
    echo ""
    echo "pipeline_config: $(shasum -a 256 "$BOI_ROOT/phases/pipelines.toml" | cut -d' ' -f1)"
    echo ""
} > "$LOGFILE"

log "Config snapshot written to $LOGFILE"

# ============================================================
# GATE 3: Discover pipeline TOMLs for this experiment
# ============================================================
log "Discovering pipeline TOMLs for experiment $SLUG..."

PIPELINE_TOMLS=()
for toml in "$PIPELINES_DIR"/experiment-"$SLUG"-*.toml; do
    if [[ -f "$toml" ]]; then
        PIPELINE_TOMLS+=("$toml")
    fi
done

if [[ ${#PIPELINE_TOMLS[@]} -eq 0 ]]; then
    die "No pipeline TOMLs found matching: $PIPELINES_DIR/experiment-$SLUG-*.toml"
fi

log "Found ${#PIPELINE_TOMLS[@]} pipeline conditions:"
{
    echo "=== Pipeline Conditions ==="
    for toml in "${PIPELINE_TOMLS[@]}"; do
        name=$(grep '^name' "$toml" | head -1 | cut -d'"' -f2)
        echo "  - $name ($(basename "$toml"))"
    done
    echo ""
} | tee -a "$LOGFILE"

# ============================================================
# GATE 4: Validate all pipeline TOMLs parse
# ============================================================
log "Validating pipeline TOML parsing..."

for toml in "${PIPELINE_TOMLS[@]}"; do
    # Basic TOML validation: check required fields exist
    if ! grep -q '^\[pipeline\]' "$toml"; then
        die "Pipeline TOML missing [pipeline] section: $toml"
    fi
    if ! grep -q '^name' "$toml"; then
        die "Pipeline TOML missing name field: $toml"
    fi
    if ! grep -q 'spec_phases' "$toml"; then
        die "Pipeline TOML missing spec_phases field: $toml"
    fi
    if ! grep -q 'task_phases' "$toml"; then
        die "Pipeline TOML missing task_phases field: $toml"
    fi
done
log "All pipeline TOMLs validated"

# ============================================================
# GATE 5: Validate spec battery with dry-run
# ============================================================
log "Validating spec battery..."

BATTERY_DIR="$BENCH_SPECS_DIR"
if [[ -n "$BATTERY_OVERRIDE" ]]; then
    BATTERY_DIR="$BATTERY_OVERRIDE"
fi

SPEC_FILES=()
for spec_name in ${BATTERIES[$EXPERIMENT]}; do
    spec_path="$BATTERY_DIR/$spec_name"
    if [[ ! -f "$spec_path" ]]; then
        die "Spec fixture not found: $spec_path"
    fi
    SPEC_FILES+=("$spec_path")
done

{
    echo "=== Spec Battery ==="
    for spec in "${SPEC_FILES[@]}"; do
        echo "  - $(basename "$spec")"
    done
    echo ""
} | tee -a "$LOGFILE"

# Dry-run validation for each spec
for spec in "${SPEC_FILES[@]}"; do
    result=$(boi dispatch --dry-run "$spec" 2>&1) || true
    if echo "$result" | grep -q "spec valid"; then
        log "  VALID: $(basename "$spec") — $result"
    else
        die "Spec dry-run failed: $(basename "$spec"): $result"
    fi
done
log "All spec fixtures validated"

# ============================================================
# Record experiment parameters
# ============================================================
{
    echo "=== Experiment Parameters ==="
    echo "runs_per_condition: $RUNS"
    echo "total_conditions: ${#PIPELINE_TOMLS[@]}"
    echo "total_specs: ${#SPEC_FILES[@]}"
    echo "total_bench_runs: $(( ${#PIPELINE_TOMLS[@]} * ${#SPEC_FILES[@]} * RUNS ))"
    echo "phase_only: ${PHASE_ONLY:-none}"
    echo ""
} >> "$LOGFILE"

if [[ $DRY_RUN -eq 1 ]]; then
    log "=== DRY RUN COMPLETE ==="
    log "All gates passed. ${#PIPELINE_TOMLS[@]} pipelines × ${#SPEC_FILES[@]} specs × $RUNS runs = $(( ${#PIPELINE_TOMLS[@]} * ${#SPEC_FILES[@]} * RUNS )) bench runs would execute."
    log "Log: $LOGFILE"
    exit 0
fi

# ============================================================
# Execute bench harness
# ============================================================
log "=== Starting bench runs ==="

# Build pipeline args: --pipeline name:path for each condition
PIPELINE_ARGS=()
for toml in "${PIPELINE_TOMLS[@]}"; do
    name=$(grep '^name' "$toml" | head -1 | cut -d'"' -f2)
    PIPELINE_ARGS+=(--pipeline "$name:$toml")
done

if [[ -n "$PHASE_ONLY" ]]; then
    # Phase-level bench: run each spec individually with --phase
    for spec in "${SPEC_FILES[@]}"; do
        log "Phase bench: $(basename "$spec") / phase=$PHASE_ONLY"
        for toml in "${PIPELINE_TOMLS[@]}"; do
            name=$(grep '^name' "$toml" | head -1 | cut -d'"' -f2)
            boi bench \
                --spec "$spec" \
                --pipeline "$name:$toml" \
                --phase "$PHASE_ONLY" \
                --runs "$RUNS" \
                $JSON_FLAG \
                2>&1 | tee -a "$LOGFILE"
        done
    done
else
    # Full-spec bench: use battery mode with block randomization
    # Each block = one run of each pipeline in random order
    for run_idx in $(seq 1 "$RUNS"); do
        log "Block $run_idx / $RUNS"
        # Shuffle pipeline order for this block (block randomization per §3 Exp 1)
        SHUFFLED=($(printf '%s\n' "${PIPELINE_TOMLS[@]}" | sort -R))
        for toml in "${SHUFFLED[@]}"; do
            name=$(grep '^name' "$toml" | head -1 | cut -d'"' -f2)
            log "  Running pipeline: $name"
            boi bench \
                --battery "$BATTERY_DIR" \
                --pipeline "$name:$toml" \
                --runs 1 \
                $JSON_FLAG \
                2>&1 | tee -a "$LOGFILE"
        done
    done
fi

log "=== Experiment $EXPERIMENT complete ==="
log "Log: $LOGFILE"
