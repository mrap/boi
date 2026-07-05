#!/bin/sh
# BOI installer.
#
# Two ways to run this:
#
#   1. From a local clone (recommended if you want to poke at the source):
#        git clone https://github.com/mrap/boi && cd boi && ./install.sh
#
#   2. Piped from curl, installing straight from GitHub:
#        curl -fsSL https://raw.githubusercontent.com/mrap/boi/main/install.sh | bash
#
# Both paths end with `cargo install`ing the `boi` binary onto your PATH via
# `~/.cargo/bin`. There is no prebuilt binary (yet) — this always compiles
# from source, which takes several minutes on first run (the bundled DuckDB
# dependency compiles from scratch).
#
# POSIX `sh` throughout — no bashisms — so both invocation styles above work
# regardless of which shell curl's `| bash` (or a `| sh`) hands the script to.
#
# Flags:
#   --dry-run   Run every prerequisite check and print what would happen,
#               but skip the actual `cargo install` and phase/pipeline seed.
#               Useful for testing this script itself.
#   --help      Print this header and exit 0.

set -eu

DRY_RUN=0
for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=1 ;;
        --help|-h)
            if [ -f "$0" ] 2>/dev/null; then
                sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'
            else
                printf 'BOI installer. Flags: --dry-run, --help.\n'
                printf 'Full source + header comment: https://github.com/mrap/boi/blob/main/install.sh\n'
            fi
            exit 0
            ;;
        *)
            printf 'boi installer: unknown argument: %s\n' "$arg" >&2
            exit 1
            ;;
    esac
done

# ---------------------------------------------------------------------------
# Output helpers — every failure is loud and says exactly what to do next.
# ---------------------------------------------------------------------------

info() { printf '  %s\n' "$*"; }
step() { printf '\n==> %s\n' "$*"; }
warn() { printf '\nwarning: %s\n' "$*" >&2; }
die() {
    printf '\nboi installer: %s\n' "$*" >&2
    exit 1
}

# ---------------------------------------------------------------------------
# 1. Detect context: local clone vs. curl-piped.
#
# When piped through `curl | bash` (or `| sh`), $0 is the interpreter itself
# ("bash", "sh", or similar) — not a real file. When run as `./install.sh` or
# `sh install.sh` from inside a clone, $0 resolves to a real file whose
# directory is the repo root (it has a Cargo.toml naming the `boi` package).
# ---------------------------------------------------------------------------

MODE=remote
INSTALL_DIR=""

if [ -f "$0" ] 2>/dev/null; then
    candidate_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
    if [ -f "$candidate_dir/Cargo.toml" ] \
        && grep -q '^name = "boi"' "$candidate_dir/Cargo.toml" 2>/dev/null; then
        MODE=local
        INSTALL_DIR="$candidate_dir"
    fi
fi

step "Install mode: $MODE"
if [ "$MODE" = "local" ]; then
    info "installing from local clone: $INSTALL_DIR"
else
    info "installing from GitHub: https://github.com/mrap/boi"
fi

# ---------------------------------------------------------------------------
# 2. Prerequisites.
#
# git / a Rust toolchain / a C compiler are BUILD prerequisites — cargo
# install cannot succeed without them, so they're hard blockers. goose is a
# RUNTIME prerequisite (BOI's preflight gate checks it before every
# `dispatch`, never during install) — its absence is a loud warning, not a
# blocker, so `cargo install` can still finish.
# ---------------------------------------------------------------------------

step "Checking prerequisites"

missing=0

if command -v git >/dev/null 2>&1; then
    info "git: $(git --version)"
else
    warn "git not found on PATH — required (worker phases run in git worktrees)."
    info "Install it: https://git-scm.com/downloads (macOS: 'xcode-select --install')"
    missing=1
fi

if command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1 || command -v clang >/dev/null 2>&1; then
    cc_bin=$(command -v cc 2>/dev/null || command -v clang 2>/dev/null || command -v gcc 2>/dev/null)
    info "C compiler: $cc_bin"
else
    warn "no C compiler found on PATH — required (the bundled DuckDB dependency compiles from source)."
    info "macOS: xcode-select --install"
    info "Debian/Ubuntu: sudo apt install build-essential"
    info "Fedora/RHEL: sudo dnf groupinstall 'Development Tools'"
    missing=1
fi

if command -v rustc >/dev/null 2>&1 && command -v cargo >/dev/null 2>&1; then
    rustc_ver=$(rustc --version 2>/dev/null | awk '{print $2}')
    rustc_major=$(printf '%s' "$rustc_ver" | cut -d. -f1)
    rustc_minor=$(printf '%s' "$rustc_ver" | cut -d. -f2)
    if [ -n "$rustc_major" ] && [ -n "$rustc_minor" ] 2>/dev/null \
        && [ "$rustc_major" -eq 1 ] 2>/dev/null; then
        if [ "$rustc_minor" -ge 85 ] 2>/dev/null; then
            info "rustc $rustc_ver (>= 1.85 required — OK)"
        else
            warn "rustc $rustc_ver found, but BOI needs >= 1.85."
            info "Update: rustup update stable   (or reinstall via https://rustup.rs)"
            missing=1
        fi
    else
        warn "could not parse rustc version ('$rustc_ver') — proceeding, but BOI needs >= 1.85."
    fi
else
    warn "Rust toolchain (rustc + cargo) not found on PATH — required."
    info "Install it: https://rustup.rs"
    missing=1
fi

# goose: a RUNTIME prerequisite, not a build one — warn, never block install.
GOOSE_MIN_MAJOR=1
GOOSE_MIN_MINOR=34
GOOSE_MAX_MAJOR_EXCLUSIVE=2
if command -v goose >/dev/null 2>&1; then
    goose_raw=$(goose --version 2>/dev/null || true)
    goose_ver=$(printf '%s' "$goose_raw" | grep -oE '[0-9]+\.[0-9]+(\.[0-9]+)?' | head -n1)
    goose_major=$(printf '%s' "$goose_ver" | cut -d. -f1)
    goose_minor=$(printf '%s' "$goose_ver" | cut -d. -f2)
    if [ -n "$goose_major" ] && [ -n "$goose_minor" ]; then
        if { [ "$goose_major" -gt "$GOOSE_MIN_MAJOR" ] \
                || { [ "$goose_major" -eq "$GOOSE_MIN_MAJOR" ] && [ "$goose_minor" -ge "$GOOSE_MIN_MINOR" ]; }; } \
            && [ "$goose_major" -lt "$GOOSE_MAX_MAJOR_EXCLUSIVE" ]; then
            info "goose $goose_ver (>=1.34, <2.0 required — OK)"
        else
            warn "goose $goose_ver found, but BOI needs >=1.34, <2.0. Dispatch will fail preflight until this is fixed."
            info "Install a matching version: https://github.com/block/goose"
        fi
    else
        warn "found goose on PATH but could not parse its version ('$goose_raw')."
        info "BOI needs goose >=1.34, <2.0 — verify manually with 'goose --version'."
    fi
else
    warn "goose not found on PATH. BOI can still be installed and built, but 'boi dispatch' will fail"
    info "preflight until goose (>=1.34, <2.0) is installed and authenticated:"
    info "https://github.com/block/goose"
fi

if [ "$missing" -eq 1 ]; then
    die "one or more build prerequisites are missing (see above) — fix those and re-run."
fi

# ---------------------------------------------------------------------------
# 3. Build + install.
# ---------------------------------------------------------------------------

step "Building boi (first build compiles DuckDB from source — this can take several minutes)"

if [ "$DRY_RUN" -eq 1 ]; then
    if [ "$MODE" = "local" ]; then
        info "[dry-run] would run: cargo install --path '$INSTALL_DIR' --locked"
    else
        info "[dry-run] would run: cargo install --git https://github.com/mrap/boi --locked"
    fi
else
    if [ "$MODE" = "local" ]; then
        (cd "$INSTALL_DIR" && cargo install --path . --locked)
    else
        cargo install --git https://github.com/mrap/boi --locked
    fi
    info "installed: $(command -v boi || echo '<not on PATH yet — check ~/.cargo/bin>')"
fi

# ---------------------------------------------------------------------------
# 4. Seed ~/.boi/v2/phases and ~/.boi/v2/pipelines.
#
# The daemon loads every phase declaration + the "standard" pipeline from
# disk at boot (`~/.boi/v2/phases/*.toml`, `~/.boi/v2/pipelines/standard.toml`)
# and fails loud if either is missing — there is no baked-in default. The
# canonical declarations ship in this repo's `tests/fixtures/{phases,pipelines}`
# (already pointed at the default `claude_code` provider), so seed them here
# rather than leaving every fresh install to hit that failure on first
# `boi daemon start`. Idempotent: never overwrites files an operator already
# customized.
# ---------------------------------------------------------------------------

step "Seeding ~/.boi/v2/phases and ~/.boi/v2/pipelines (phase + pipeline declarations)"

BOI_ROOT="${HOME}/.boi/v2"
PHASES_DIR="$BOI_ROOT/phases"
PIPELINES_DIR="$BOI_ROOT/pipelines"

seed_from() {
    # $1 = directory containing fixtures/{phases,pipelines}
    fixture_root="$1"
    if [ ! -d "$fixture_root/phases" ] || [ ! -f "$fixture_root/pipelines/standard.toml" ]; then
        warn "could not find phase/pipeline fixtures under $fixture_root — skipping seed."
        info "You'll need to populate $PHASES_DIR and $PIPELINES_DIR yourself before 'boi daemon start' will boot."
        return 0
    fi

    mkdir -p "$PHASES_DIR" "$PIPELINES_DIR"

    seeded_any=0
    for src in "$fixture_root"/phases/*.toml "$fixture_root"/phases/*.md; do
        [ -e "$src" ] || continue
        name=$(basename "$src")
        if [ -e "$PHASES_DIR/$name" ]; then
            continue # never clobber an existing/customized declaration
        fi
        cp "$src" "$PHASES_DIR/$name"
        seeded_any=1
    done

    if [ ! -e "$PIPELINES_DIR/standard.toml" ]; then
        cp "$fixture_root/pipelines/standard.toml" "$PIPELINES_DIR/standard.toml"
        seeded_any=1
    fi

    if [ "$seeded_any" -eq 1 ]; then
        info "seeded default phase + pipeline declarations into $BOI_ROOT"
    else
        info "$PHASES_DIR and $PIPELINES_DIR already populated — left untouched"
    fi
}

if [ "$DRY_RUN" -eq 1 ]; then
    info "[dry-run] would seed $PHASES_DIR and $PIPELINES_DIR from the repo's tests/fixtures"
elif [ "$MODE" = "local" ]; then
    seed_from "$INSTALL_DIR/tests/fixtures"
else
    # curl|bash mode has no local source tree — shallow-clone just to pull the
    # fixture declarations, then discard the clone. git is already a
    # confirmed prerequisite at this point.
    tmp_clone=$(mktemp -d 2>/dev/null || mktemp -d -t boi-install)
    trap 'rm -rf "$tmp_clone"' EXIT
    if git clone --depth 1 --quiet https://github.com/mrap/boi "$tmp_clone" 2>/dev/null; then
        seed_from "$tmp_clone/tests/fixtures"
    else
        warn "could not shallow-clone https://github.com/mrap/boi to seed default phase/pipeline declarations."
        info "You'll need to populate $PHASES_DIR and $PIPELINES_DIR yourself before 'boi daemon start' will boot"
        info "— see https://github.com/mrap/boi/tree/main/tests/fixtures for the canonical files."
    fi
fi

# ---------------------------------------------------------------------------
# 5. Next steps.
# ---------------------------------------------------------------------------

step "Done"
cat <<'EOF'

Next steps:

  1. Add a provider credential (default provider: Claude Code):
       mkdir -p ~/.boi/v2/secrets && chmod 700 ~/.boi/v2/secrets
       printf 'CLAUDE_CODE_OAUTH_TOKEN=...\n' > ~/.boi/v2/secrets/claude.env
       chmod 600 ~/.boi/v2/secrets/claude.env

  2. Start the daemon (installs + starts the per-user background service):
       boi daemon start
       boi daemon status

  3. Dispatch your first spec — see docs/getting-started.md for the full
     walkthrough, or start straight from a runnable example:
       tests/fixtures/specs/01_minimum.toml   (github.com/mrap/boi)

  4. Watch it run:
       boi dashboard

Full quickstart: https://github.com/mrap/boi/blob/main/docs/getting-started.md
EOF
