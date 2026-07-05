//! [`preflight`] — the pre-dispatch gate (amendment §9, corrected by the Goose
//! research spike `docs/research/goose-spike-2026-05-20.md`).
//!
//! Before a spec's first `phase_runs` insert, [`preflight`] runs a fast, loud,
//! pre-spend check: `goose` is installed at a usable version, and every
//! distinct provider the spec's phases name has reachable credentials. A
//! failure → `SpecFailed{ FailureReason::PreflightFailed }`, no phase runs.
//!
//! ## Goose-spike corrections folded in here
//!
//! - **The version floor is `>=1.34`, not `>=1.0`.** Goose's `stream-json`
//!   output (PR #6228) and the current recipe schema both postdate 1.0; a
//!   `goose` of `1.0.x` would pass a `>=1.0` gate and then fail at runtime
//!   when `--output-format stream-json` is rejected. v1.34 is the verified
//!   floor (spike §Q1). `<2.0` is a moving bound — revisit at a Goose 2.0 GA.
//! - **There is NO extension-registration check.** Amendment §9 item 3 ("every
//!   extension the recipes need is registered in the local Goose install") is
//!   a fictional requirement: a recipe-declared `stdio` extension needs no
//!   pre-registration — Goose spawns it from the recipe at runtime (spike §Q2).
//!   The plan's former preflight check #3 and a `PreflightError::ExtensionMissing`
//!   variant are deliberately absent.
//!
//! ## Provider-credential check — v1.0 scope
//!
//! v1.0 checks that each distinct provider's credential **environment
//! variable** is set and non-empty. A real provider *ping* (a live auth
//! round-trip) needs the network and a configured provider — that is an L3 /
//! Docker check (Phase 10), out of Phase 7's mocked scope. The enumeration
//! (every distinct provider is visited) is the load-bearing v1.0 behavior; the
//! depth of the per-provider check deepens in Phase 10.
//!
//! **`claude_code` is exempt from the env-var check** (review C-cr-4). It is
//! BOI's default provider and authenticates through its own CLI session
//! (`~/.claude`), not an `ANTHROPIC_API_KEY` env var — gating it on that var
//! was a false-positive that would block every dispatch on a working install
//! with no env var set. `ANTHROPIC_API_KEY` is checked only for the raw
//! `anthropic` provider.
//!
//! ## Provider liveness probe — 429 hardening (incident 2026-06-06)
//!
//! A *set* credential can still be **throttled** (HTTP 429 — a Claude Max
//! rolling window) or **revoked** (401/403); goose swallows both into empty
//! completions, so without a probe the spec burns its full phase-iteration
//! budget discovering what one cheap request would have said up front. Check 3
//! therefore runs a [`ProviderProbe`] — a 1-token authenticated no-op against
//! each distinct provider — once per dispatch (the daemon seam; never
//! per-phase). A 429 refuses the dispatch with **"RATE LIMITED — paused"**, a
//! 401/403 with **"AUTH FAILED"** — both pre-spend, both loud. A probe that
//! cannot run at all (no credential, no curl, no network) does NOT refuse —
//! it is loudly logged and the dispatch proceeds: the probe is a 429/auth
//! gate, not a network gate (the per-attempt timeout in `goose.rs` D1 is the
//! runtime backstop). The probe sits behind the [`ProviderProbe`] trait so
//! tests stub it; production wires [`CurlProviderProbe`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::{PhaseDef, SkillRef};
use crate::runtime::branch_policy::{self, PolicyVerdict};

/// The Goose version requirement BOI's recipes depend on.
///
/// `>=1.34` — Goose's `stream-json` (PR #6228) and the current recipe schema
/// both postdate 1.0, so 1.0 is NOT a safe floor (spike §Q1 — the plan's
/// original `>=1.0` is wrong). `<2.0` is a moving upper bound: a Goose 2.0 GA
/// needs a deliberate re-spike of the recipe schema + stream-json shape.
pub const GOOSE_VERSION_REQ: &str = ">=1.34, <2.0";

/// The inclusive lower bound `(major, minor)` derived from [`GOOSE_VERSION_REQ`].
const MIN_VERSION: (u32, u32) = (1, 34);

/// The exclusive upper bound major version derived from [`GOOSE_VERSION_REQ`].
const MAX_MAJOR_EXCLUSIVE: u32 = 2;

/// A preflight check failed — the spec must not be dispatched.
///
/// Each variant is loud and typed (no free-text-only failure). A `preflight`
/// failure maps to `SpecFailed{ FailureReason::PreflightFailed{ details } }`.
///
/// There is deliberately NO `ExtensionMissing` variant — recipe-declared
/// `stdio` extensions need no pre-registration (spike §Q2).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PreflightError {
    /// `goose` is not on PATH / not executable.
    #[error("goose binary not found or not executable: {0}")]
    GooseMissing(String),
    /// `goose --version` reported a version outside [`GOOSE_VERSION_REQ`].
    #[error("goose version {found} does not satisfy the requirement {required}")]
    GooseVersion {
        /// The version `goose --version` reported.
        found: String,
        /// The required version range.
        required: String,
    },
    /// A provider's credentials are not reachable.
    #[error("provider credentials unavailable: {0}")]
    ProviderCreds(String),
    /// The workspace branch policy rejected the spec's `base_branch`
    /// (GitFlow program R-B7 — the daemon-side backstop for dispatches that
    /// bypassed the CLI gate). Carries the fully-rendered policy detail,
    /// `Fix:` line included.
    #[error("workspace branch policy: {0}")]
    BranchPolicy(String),
    /// The provider is rate-limiting its credential — the preflight probe got
    /// HTTP 429 (429 hardening). The dispatch is refused pre-spend: re-running
    /// the same throttled token through 3 retries × N phases produces nothing
    /// but empty completions (incident 2026-06-06).
    #[error(
        "RATE LIMITED — paused: provider `{provider}` is throttling its credential; \
         refusing the dispatch before any phase spends iterations on it: {detail}"
    )]
    ProviderRateLimited {
        /// The provider whose probe hit HTTP 429.
        provider: String,
        /// What the probe saw.
        detail: String,
    },
    /// The provider rejected its credential — the preflight probe got HTTP
    /// 401/403 (429 hardening). Refused pre-spend, loudly.
    #[error(
        "AUTH FAILED: provider `{provider}` rejected its credential on the \
         preflight probe: {detail}"
    )]
    ProviderAuthFailed {
        /// The provider whose probe was rejected.
        provider: String,
        /// What the probe saw.
        detail: String,
    },
}

/// The outcome of one provider liveness probe (429 hardening).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The authenticated no-op was neither throttled nor rejected — dispatch
    /// may proceed.
    Ok,
    /// HTTP 429 — the provider is rate-limiting this credential. The dispatch
    /// is refused LOUDLY ("RATE LIMITED — paused").
    RateLimited {
        /// What the probe saw.
        detail: String,
    },
    /// HTTP 401/403 — the provider rejected the credential. The dispatch is
    /// refused LOUDLY ("AUTH FAILED").
    AuthFailed {
        /// What the probe saw.
        detail: String,
    },
    /// The probe itself could not run (no credential to probe with, curl
    /// missing, no network, a timeout). Loudly logged; does NOT refuse — the
    /// probe is a 429/auth gate, not a network gate.
    Unavailable {
        /// Why the probe could not run.
        detail: String,
    },
    /// No probe shape exists for this provider — skipped silently.
    Unsupported,
}

/// A cheap authenticated no-op against a provider, run once per dispatch
/// BEFORE any phase spends iterations (429 hardening, incident 2026-06-06).
///
/// Behind a trait so tests stub it deterministically; production wires
/// [`CurlProviderProbe`]. `model` is the model the spec's phases will
/// actually use — probing with the real model keeps the provider's
/// rate-limit/validation gates representative.
pub trait ProviderProbe: Send + Sync {
    /// Probe `provider` (with `model`) and classify the result.
    fn probe<'a>(&'a self, provider: &'a str, model: &'a str) -> BoxFuture<'a, ProbeOutcome>;
}

/// Run the pre-dispatch preflight check (amendment §9 + 429 hardening).
///
/// Takes the spec's FULL phase set so it can enumerate **every distinct
/// provider** — specs are multi-provider, and a single-`recipe_sample` arg
/// could only check one. Checks, in order:
///
/// 1. `goose` is on PATH and `goose --version` is within [`GOOSE_VERSION_REQ`].
/// 2. Every distinct `phase.runtime.provider` across `phases` has reachable
///    credentials (v1.0: a credential env var is set — see the module doc).
/// 3. Every distinct provider passes its liveness `probe` — a 429 refuses the
///    dispatch with "RATE LIMITED — paused", a 401/403 with "AUTH FAILED"
///    (see the module doc's 429-hardening section).
///
/// There is NO extension-registration check (the spike's deletion of the
/// plan's former check #3) — `skills` is accepted for signature stability and
/// forward use but is not inspected.
///
/// On the first failure it returns a typed [`PreflightError`]; the Phase 9
/// dispatch path turns that into `SpecFailed{PreflightFailed}` and runs no
/// phase.
pub async fn preflight(
    goose_bin: &Path,
    phases: &[PhaseDef],
    skills: &[SkillRef],
    probe: &dyn ProviderProbe,
) -> Result<(), PreflightError> {
    // The real credential resolver — `std::env::var`. The provider-check is
    // factored through `preflight_with` so a test can inject a deterministic
    // resolver instead of mutating the process environment.
    preflight_with(
        goose_bin,
        phases,
        skills,
        |env_var| std::env::var(env_var).ok(),
        probe,
    )
    .await
}

/// [`preflight`] with an injectable credential-env-var resolver.
///
/// The `resolve_env` closure maps a credential env-var name to its value
/// (`None` when unset). Production passes `std::env::var`; tests pass a stub —
/// so the provider-enumeration behavior is verified without mutating the
/// process environment (env mutation is `unsafe` and racy across parallel
/// tests). This is a Phase 7 deviation from the plan's bare `preflight`
/// signature, kept private so the public surface is exactly the plan's.
async fn preflight_with<F>(
    goose_bin: &Path,
    phases: &[PhaseDef],
    skills: &[SkillRef],
    resolve_env: F,
    probe: &dyn ProviderProbe,
) -> Result<(), PreflightError>
where
    F: Fn(&str) -> Option<String>,
{
    // `skills` is intentionally not inspected — recipe-declared stdio
    // extensions need no pre-registration (spike §Q2). The parameter is kept
    // for signature stability with the plan + amendment §9.
    let _ = skills;

    // --- Check 1: goose binary + version ---
    check_goose_version(goose_bin).await?;

    // --- Check 2: every distinct provider's credentials ---
    for provider in distinct_providers(phases) {
        check_provider_creds(&provider, &resolve_env)?;
    }

    // --- Check 3: every distinct provider's liveness probe (429 hardening) ---
    for (provider, model) in distinct_provider_models(phases) {
        // Placeholder providers (deterministic phases) never reach a runtime.
        if is_placeholder_provider(&provider) {
            continue;
        }
        match probe.probe(&provider, &model).await {
            ProbeOutcome::Ok | ProbeOutcome::Unsupported => {}
            ProbeOutcome::Unavailable { detail } => {
                // Loud but non-blocking: the probe gates 429/auth, never
                // network reachability (S6 — logged, not swallowed; D1's
                // per-attempt timeout is the runtime backstop).
                tracing::warn!(
                    provider = %provider,
                    detail = %detail,
                    "provider liveness probe could not run — proceeding with the dispatch",
                );
            }
            ProbeOutcome::RateLimited { detail } => {
                tracing::error!(
                    provider = %provider,
                    detail = %detail,
                    "RATE LIMITED — refusing the dispatch pre-spend (429 hardening)",
                );
                return Err(PreflightError::ProviderRateLimited { provider, detail });
            }
            ProbeOutcome::AuthFailed { detail } => {
                tracing::error!(
                    provider = %provider,
                    detail = %detail,
                    "AUTH FAILED — refusing the dispatch pre-spend (429 hardening)",
                );
                return Err(PreflightError::ProviderAuthFailed { provider, detail });
            }
        }
    }

    Ok(())
}

/// GitFlow Layer 2 (R-B7) — the daemon-side branch-policy preflight.
///
/// The same evaluation as the Layer-1 dispatch gate, run pre-spend for every
/// started spec: it catches dispatches that never crossed the CLI (direct
/// control-socket clients) and specs persisted by a pre-gate binary. The
/// policy is read fresh from the committed tree of
/// `refs/heads/<base_branch>` in `workspace` (D-13 — never any checkout's
/// working tree).
///
/// A failure maps to `SpecFailed{ FailureReason::PreflightFailed }` at the
/// dispatch handler, carrying the fully-rendered detail. The M8 advisory is
/// a Layer-1 (dispatch output) surface — the daemon has no operator-facing
/// warning channel, so an advisory-carrying Allow is simply an allow here.
pub async fn branch_policy_gate(
    workspace: PathBuf,
    base_branch: String,
) -> Result<(), PreflightError> {
    let ctx = branch_policy::load_policy(workspace, base_branch.clone()).await;
    match ctx.verdict(&base_branch) {
        PolicyVerdict::Allow { .. } | PolicyVerdict::Skip { .. } => Ok(()),
        PolicyVerdict::ProtectedBase { branch, fix_hint } => Err(PreflightError::BranchPolicy(
            format!("base_branch `{branch}` is protected in this workspace\n  {fix_hint}"),
        )),
        PolicyVerdict::MissingBase { branch, hint } => Err(PreflightError::BranchPolicy(format!(
            "base_branch `{branch}` does not exist in the workspace\n  {hint}"
        ))),
        PolicyVerdict::PolicyInvalid { reason } => Err(PreflightError::BranchPolicy(format!(
            "the workspace branch policy could not be read\n  {reason}"
        ))),
    }
}

/// Run `goose --version` and verify the reported version is within
/// [`GOOSE_VERSION_REQ`].
async fn check_goose_version(goose_bin: &Path) -> Result<(), PreflightError> {
    let output = Command::new(goose_bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| {
            PreflightError::GooseMissing(format!(
                "running `{} --version`: {e}",
                goose_bin.display(),
            ))
        })?;

    if !output.status.success() {
        return Err(PreflightError::GooseMissing(format!(
            "`{} --version` exited with status {}",
            goose_bin.display(),
            output.status,
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let found = parse_version_string(&stdout).ok_or_else(|| PreflightError::GooseVersion {
        found: stdout.trim().to_owned(),
        required: GOOSE_VERSION_REQ.to_owned(),
    })?;

    if !version_satisfies(found) {
        return Err(PreflightError::GooseVersion {
            found: format!("{}.{}", found.0, found.1),
            required: GOOSE_VERSION_REQ.to_owned(),
        });
    }
    Ok(())
}

/// Parse the `(major, minor)` version pair out of a `goose --version` line.
///
/// `goose --version` prints something like `goose 1.34.1` or `1.34.1`; this
/// finds the first `MAJOR.MINOR[.PATCH]`-shaped token and returns
/// `(major, minor)`. A token that does not parse (`goose`, the program name)
/// is skipped — the loop continues to the next token. `None` when no
/// version-shaped token is found at all.
fn parse_version_string(text: &str) -> Option<(u32, u32)> {
    for token in text.split_whitespace() {
        if let Some(parsed) = parse_version_token(token) {
            return Some(parsed);
        }
    }
    None
}

/// Parse one whitespace-delimited token as a `MAJOR.MINOR[.PATCH]` version.
///
/// `None` when the token is not version-shaped (so [`parse_version_string`]
/// can skip it and try the next token — never aborting the whole scan).
fn parse_version_token(token: &str) -> Option<(u32, u32)> {
    // Strip a leading non-digit prefix (e.g. a `v` in `v1.34.1`).
    let digits = token.trim_start_matches(|c: char| !c.is_ascii_digit());
    let mut parts = digits.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    // A bare `1` with no `.minor` is treated as `1.0`.
    let minor: u32 = match parts.next() {
        Some(m) => {
            // The minor part may carry a trailing pre-release tag (`34-rc1`)
            // — take the leading digits.
            let lead: String = m.chars().take_while(|c| c.is_ascii_digit()).collect();
            lead.parse().ok()?
        }
        None => 0,
    };
    Some((major, minor))
}

/// Whether a `(major, minor)` version satisfies [`GOOSE_VERSION_REQ`].
fn version_satisfies((major, minor): (u32, u32)) -> bool {
    let at_or_above_floor =
        major > MIN_VERSION.0 || (major == MIN_VERSION.0 && minor >= MIN_VERSION.1);
    let below_ceiling = major < MAX_MAJOR_EXCLUSIVE;
    at_or_above_floor && below_ceiling
}

/// The distinct providers named across a spec's phases.
///
/// Deterministic ordering — a `BTreeSet` — so the preflight error (if one
/// provider fails) is reproducible. `deterministic` phases also carry a
/// `runtime` (it is inert for them); their provider is typically a placeholder
/// like `deterministic`, which `check_provider_creds` skips.
fn distinct_providers(phases: &[PhaseDef]) -> BTreeSet<String> {
    phases.iter().map(|p| p.runtime.provider.clone()).collect()
}

/// The distinct providers with a representative model each (the first model
/// seen per provider) — what the liveness probe runs against. `BTreeMap` for
/// the same deterministic ordering as [`distinct_providers`].
fn distinct_provider_models(phases: &[PhaseDef]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for p in phases {
        map.entry(p.runtime.provider.clone())
            .or_insert_with(|| p.runtime.model.clone());
    }
    map
}

// ---------------------------------------------------------------------------
// The provider liveness probe (429 hardening) — see the module doc.
// ---------------------------------------------------------------------------

/// The Anthropic Messages endpoint both probeable providers hit.
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";

/// The hard cap on one probe round-trip.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// One provider probe's HTTP request shape.
struct ProbeRequest {
    /// The endpoint to POST.
    url: String,
    /// Request headers, one `name: value` per entry. Passed to curl via
    /// stdin (`-H @-`) — credentials never ride argv (ps-visible).
    headers: Vec<String>,
    /// The JSON body — a 1-token message, the cheapest authenticated no-op
    /// that still exercises the provider's rate-limit gate.
    body: String,
}

/// What the probe should do for one provider.
enum ProbePlan {
    /// POST this request and classify the HTTP status.
    Request(ProbeRequest),
    /// The provider is probeable but its credential is missing — the probe
    /// reports [`ProbeOutcome::Unavailable`] (loud, non-blocking).
    Unavailable(String),
    /// No probe shape exists for this provider — skipped.
    Unsupported,
}

/// Build the probe request for `provider`, resolving credentials through
/// `resolve_env` (injected so tests never mutate the process environment —
/// same pattern as `preflight_with`).
///
/// - `claude_code` — the incident provider: a Messages call authenticated
///   with the `CLAUDE_CODE_OAUTH_TOKEN` bearer + the oauth beta header
///   (exactly the probe that confirmed the 2026-06-06 root cause).
/// - `anthropic` — the same call with `x-api-key`.
/// - everything else — `Unsupported` (no probe shape known; v1 scope is the
///   incident class).
fn probe_plan_with<F>(provider: &str, model: &str, resolve_env: &F) -> ProbePlan
where
    F: Fn(&str) -> Option<String>,
{
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 1,
        "messages": [{ "role": "user", "content": "ping" }],
    })
    .to_string();
    match provider {
        "claude_code" | "claude-code" => match resolve_env("CLAUDE_CODE_OAUTH_TOKEN") {
            Some(tok) if !tok.trim().is_empty() => ProbePlan::Request(ProbeRequest {
                url: ANTHROPIC_MESSAGES_URL.to_owned(),
                headers: vec![
                    format!("authorization: Bearer {}", tok.trim()),
                    "anthropic-beta: oauth-2025-04-20".to_owned(),
                    "anthropic-version: 2023-06-01".to_owned(),
                    "content-type: application/json".to_owned(),
                ],
                body,
            }),
            _ => ProbePlan::Unavailable(
                "provider `claude_code`: CLAUDE_CODE_OAUTH_TOKEN is unset in the daemon \
                 environment — cannot probe the credential"
                    .to_owned(),
            ),
        },
        "anthropic" => match resolve_env("ANTHROPIC_API_KEY") {
            Some(key) if !key.trim().is_empty() => ProbePlan::Request(ProbeRequest {
                url: ANTHROPIC_MESSAGES_URL.to_owned(),
                headers: vec![
                    format!("x-api-key: {}", key.trim()),
                    "anthropic-version: 2023-06-01".to_owned(),
                    "content-type: application/json".to_owned(),
                ],
                body,
            }),
            _ => ProbePlan::Unavailable(
                "provider `anthropic`: ANTHROPIC_API_KEY is unset — cannot probe the credential"
                    .to_owned(),
            ),
        },
        _ => ProbePlan::Unsupported,
    }
}

/// Classify a probe's HTTP status: 429 → [`ProbeOutcome::RateLimited`],
/// 401/403 → [`ProbeOutcome::AuthFailed`], anything else → `Ok` — a 200
/// obviously, but also a 400/404 (the request was *judged*, so the auth and
/// throttle gates were already passed) and a 5xx (a provider hiccup is not
/// the probe's business; D1's per-attempt timeout is the runtime backstop).
fn classify_probe_status(provider: &str, status: u16) -> ProbeOutcome {
    match status {
        429 => ProbeOutcome::RateLimited {
            detail: format!(
                "the `{provider}` probe got HTTP 429 (rate_limit_error) — the credential \
                 is inside a throttled rolling window"
            ),
        },
        401 | 403 => ProbeOutcome::AuthFailed {
            detail: format!(
                "the `{provider}` probe got HTTP {status} — the credential was rejected \
                 (expired or revoked token?)"
            ),
        },
        _ => ProbeOutcome::Ok,
    }
}

/// The production [`ProviderProbe`] — shells out to `curl` (the only HTTP
/// client in BOI's dependency surface; subprocess use is a runtime-layer
/// concern, which this module is).
pub struct CurlProviderProbe {
    /// The per-probe round-trip cap (curl `--max-time` + an outer reap bound).
    timeout: Duration,
}

impl CurlProviderProbe {
    /// Construct with the default `PROBE_TIMEOUT` (10s round-trip cap).
    pub fn new() -> Self {
        Self {
            timeout: PROBE_TIMEOUT,
        }
    }
}

impl Default for CurlProviderProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderProbe for CurlProviderProbe {
    fn probe<'a>(&'a self, provider: &'a str, model: &'a str) -> BoxFuture<'a, ProbeOutcome> {
        Box::pin(async move {
            match probe_plan_with(provider, model, &|var| std::env::var(var).ok()) {
                ProbePlan::Unsupported => ProbeOutcome::Unsupported,
                ProbePlan::Unavailable(detail) => ProbeOutcome::Unavailable { detail },
                ProbePlan::Request(req) => run_curl_probe(provider, &req, self.timeout).await,
            }
        })
    }
}

/// POST one [`ProbeRequest`] through `curl` and classify the HTTP status.
///
/// `curl -s -o /dev/null -w %{http_code}` — the status code is the entire
/// stdout. Headers ride stdin (`-H @-`), never argv. Every infrastructure
/// failure (spawn, timeout, non-zero curl exit, unparseable status) is
/// [`ProbeOutcome::Unavailable`] — loud at the caller, never a refusal.
async fn run_curl_probe(provider: &str, req: &ProbeRequest, timeout: Duration) -> ProbeOutcome {
    let unavailable = |detail: String| ProbeOutcome::Unavailable { detail };
    let mut child = match Command::new("curl")
        .arg("-s")
        .arg("-o")
        .arg("/dev/null")
        .arg("-w")
        .arg("%{http_code}")
        .arg("--max-time")
        .arg(timeout.as_secs().to_string())
        .arg("-X")
        .arg("POST")
        .arg(&req.url)
        .arg("-H")
        .arg("@-")
        .arg("--data")
        .arg(&req.body)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return unavailable(format!(
                "could not spawn curl for the `{provider}` probe: {e}"
            ));
        }
    };

    // Write the headers to curl's stdin (`-H @-`), then drop the handle so
    // curl sees EOF.
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(req.headers.join("\n").as_bytes()).await {
            return unavailable(format!(
                "could not write the `{provider}` probe headers: {e}"
            ));
        }
    }

    // `--max-time` bounds curl itself; the outer timeout is the reap backstop
    // (a wedged curl must never wedge the dispatch path).
    let output = match tokio::time::timeout(
        timeout + Duration::from_secs(5),
        child.wait_with_output(),
    )
    .await
    {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return unavailable(format!("waiting on the `{provider}` probe curl: {e}")),
        Err(_) => {
            return unavailable(format!(
                "the `{provider}` probe curl did not finish within its reap window"
            ));
        }
    };

    if !output.status.success() {
        // curl exits non-zero on transport failures (DNS, no network, its own
        // --max-time) — NOT on HTTP error statuses.
        return unavailable(format!(
            "the `{provider}` probe curl exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
        ));
    }
    let code_text = String::from_utf8_lossy(&output.stdout);
    match code_text.trim().parse::<u16>() {
        Ok(status) => classify_probe_status(provider, status),
        Err(_) => unavailable(format!(
            "the `{provider}` probe curl printed an unparseable status `{code_text}`"
        )),
    }
}

/// Verify one provider's credentials are reachable.
///
/// v1.0 (Phase 7 scope): a provider's credential environment variable is set
/// and non-empty. A real auth ping is an L3 / Docker check (Phase 10).
///
/// Two provider classes need NO env-var check:
/// - A non-LLM placeholder (`deterministic`, `n/a`) — those phases never reach
///   the runtime.
/// - `claude_code` (the **default** provider) — it authenticates through its
///   own CLI session (`~/.claude` credentials), NOT an `ANTHROPIC_API_KEY`
///   env var. Gating it on `ANTHROPIC_API_KEY` was a false-positive: a working
///   `claude_code` install with no `ANTHROPIC_API_KEY` set would fail
///   preflight and block every dispatch (review C-cr-4). `ANTHROPIC_API_KEY`
///   is the credential for the *raw* `anthropic` provider only.
///
/// `resolve_env` maps the credential env-var name to its value — injected so
/// the check is testable without mutating the process environment.
fn check_provider_creds<F>(provider: &str, resolve_env: &F) -> Result<(), PreflightError>
where
    F: Fn(&str) -> Option<String>,
{
    // Providers that authenticate without a BOI-visible credential env var —
    // placeholder (deterministic) phases and `claude_code` (its own CLI
    // session). Both are skipped (review C-cr-4).
    if skips_env_credential_check(provider) {
        return Ok(());
    }
    let env_var = credential_env_var(provider);
    match resolve_env(&env_var) {
        Some(v) if !v.trim().is_empty() => Ok(()),
        _ => Err(PreflightError::ProviderCreds(format!(
            "provider `{provider}`: credential env var `{env_var}` is unset or empty",
        ))),
    }
}

/// Whether `provider` needs NO credential-env-var check.
///
/// Three classes authenticate without a BOI-visible credential env var:
/// - a non-LLM placeholder (`deterministic`, `n/a`) — those phases never
///   reach the runtime;
/// - `claude_code` (the default provider) — its own CLI session (review
///   C-cr-4);
/// - `ollama` — a **local** model server (`http://127.0.0.1:11434`); it has
///   no API key at all. Gating it on `OLLAMA_API_KEY` was a false-positive
///   that blocked every Ollama dispatch (surfaced by the Phase 10 Docker E2E,
///   which runs `goose` against a local Ollama model).
fn skips_env_credential_check(provider: &str) -> bool {
    is_placeholder_provider(provider) || matches!(provider, "claude_code" | "ollama")
}

/// Whether `provider` is a non-LLM placeholder (a `deterministic` phase's inert
/// `runtime.provider`).
fn is_placeholder_provider(provider: &str) -> bool {
    matches!(provider, "deterministic" | "n/a" | "none" | "")
}

/// The credential environment variable name for a provider.
///
/// v1.0 mapping — the common providers' standard credential env vars. An
/// unknown provider maps to `<PROVIDER>_API_KEY` (upper-cased), an honest
/// best-effort default rather than a silent skip.
///
/// `claude_code` is deliberately ABSENT — it never reaches here
/// ([`skips_env_credential_check`] short-circuits it); it authenticates via
/// its own CLI session, not an env var (review C-cr-4). `ANTHROPIC_API_KEY`
/// belongs to the raw `anthropic` provider.
fn credential_env_var(provider: &str) -> String {
    match provider {
        "anthropic" => "ANTHROPIC_API_KEY".to_owned(),
        "openrouter" => "OPENROUTER_API_KEY".to_owned(),
        "openai" => "OPENAI_API_KEY".to_owned(),
        "gemini" | "google" => "GEMINI_API_KEY".to_owned(),
        other => format!("{}_API_KEY", other.to_uppercase()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A throwaway directory removed on drop.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("boi-preflight-{}-{tag}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.path));
        }
    }

    /// Write a stub `goose` that prints `version_line` for `--version`.
    fn stub_goose(dir: &Path, version_line: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("stub-goose.sh");
        // `goose --version` → print the line; any other arg → exit 0.
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '{version_line}'; fi\nexit 0\n"
        );
        std::fs::write(&bin, script).unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();
        bin
    }

    /// A worker `PhaseDef` for `provider`.
    fn worker_phase(name: &str, provider: &str) -> PhaseDef {
        // Start from the real `execute` fixture, override name + provider.
        let toml = std::fs::read_to_string(format!(
            "{}/tests/fixtures/phases/execute.toml",
            env!("CARGO_MANIFEST_DIR"),
        ))
        .unwrap();
        let mut phase = crate::config::parse_phase(&toml).unwrap();
        phase.name = name.to_owned();
        phase.runtime.provider = provider.to_owned();
        phase
    }

    /// A credential resolver that reports every env var present.
    fn all_creds_present(_env_var: &str) -> Option<String> {
        Some("present".to_owned())
    }

    /// A credential resolver that reports every env var absent.
    fn no_creds(_env_var: &str) -> Option<String> {
        None
    }

    /// A stub [`ProviderProbe`] returning a fixed outcome for every provider.
    struct StubProbe(ProbeOutcome);

    impl ProviderProbe for StubProbe {
        fn probe<'a>(
            &'a self,
            _provider: &'a str,
            _model: &'a str,
        ) -> futures::future::BoxFuture<'a, ProbeOutcome> {
            Box::pin(async move { self.0.clone() })
        }
    }

    /// The probe stub every non-probe test passes — always `Ok`.
    fn ok_probe() -> StubProbe {
        StubProbe(ProbeOutcome::Ok)
    }

    /// 429 hardening: a rate-limited provider probe REFUSES the dispatch
    /// pre-spend, with "RATE LIMITED — paused" in the rendered error — the
    /// spec must not burn its phase iterations on empty completions
    /// (incident 2026-06-06).
    #[tokio::test]
    async fn test_l2_rate_limited_probe_refuses_dispatch_loudly() {
        let dir = TempDir::new("probe-429");
        let goose = stub_goose(&dir.path, "goose 1.34.1");
        let err = preflight_with(
            &goose,
            &[worker_phase("execute", "claude_code")],
            &[],
            all_creds_present,
            &StubProbe(ProbeOutcome::RateLimited {
                detail: "HTTP 429 rate_limit_error".to_owned(),
            }),
        )
        .await
        .unwrap_err();
        let PreflightError::ProviderRateLimited { provider, .. } = &err else {
            unreachable!("a 429 probe must yield ProviderRateLimited, got {err:?}");
        };
        assert_eq!(provider, "claude_code");
        let rendered = err.to_string();
        assert!(
            rendered.contains("RATE LIMITED — paused"),
            "the rendered error must shout RATE LIMITED — paused, got: {rendered}",
        );
        assert!(
            rendered.contains("429"),
            "the rendered error must carry the probe evidence, got: {rendered}",
        );
    }

    /// 429 hardening: a 401/403 probe REFUSES the dispatch with "AUTH FAILED"
    /// in the rendered error — an expired/revoked token must not be
    /// discovered three empty completions deep into a phase.
    #[tokio::test]
    async fn test_l2_auth_failed_probe_refuses_dispatch_loudly() {
        let dir = TempDir::new("probe-auth");
        let goose = stub_goose(&dir.path, "goose 1.34.1");
        let err = preflight_with(
            &goose,
            &[worker_phase("execute", "claude_code")],
            &[],
            all_creds_present,
            &StubProbe(ProbeOutcome::AuthFailed {
                detail: "HTTP 401 authentication_error".to_owned(),
            }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, PreflightError::ProviderAuthFailed { .. }),
            "a 401 probe must yield ProviderAuthFailed, got {err:?}",
        );
        let rendered = err.to_string();
        assert!(
            rendered.contains("AUTH FAILED"),
            "the rendered error must shout AUTH FAILED, got: {rendered}",
        );
    }

    /// A probe that cannot run (no credential to probe with, no curl, no
    /// network) does NOT refuse the dispatch — the probe is a 429/auth gate,
    /// not a network gate. It is logged loudly instead.
    #[tokio::test]
    async fn test_l2_unavailable_probe_does_not_refuse_dispatch() {
        let dir = TempDir::new("probe-unavailable");
        let goose = stub_goose(&dir.path, "goose 1.34.1");
        let result = preflight_with(
            &goose,
            &[worker_phase("execute", "claude_code")],
            &[],
            all_creds_present,
            &StubProbe(ProbeOutcome::Unavailable {
                detail: "curl not found".to_owned(),
            }),
        )
        .await;
        assert!(
            result.is_ok(),
            "an unavailable probe must not block the dispatch, got {result:?}",
        );
    }

    /// A passing probe lets the dispatch through.
    #[tokio::test]
    async fn test_l2_ok_probe_passes_preflight() {
        let dir = TempDir::new("probe-ok");
        let goose = stub_goose(&dir.path, "goose 1.34.1");
        let result = preflight_with(
            &goose,
            &[worker_phase("execute", "claude_code")],
            &[],
            all_creds_present,
            &ok_probe(),
        )
        .await;
        assert!(result.is_ok(), "an Ok probe must pass, got {result:?}");
    }

    /// A placeholder (deterministic) provider is never probed — a probe stub
    /// that would refuse everything proves it was not consulted.
    #[tokio::test]
    async fn test_l2_placeholder_provider_is_never_probed() {
        let dir = TempDir::new("probe-placeholder");
        let goose = stub_goose(&dir.path, "goose 1.34.1");
        let result = preflight_with(
            &goose,
            &[worker_phase("merge", "deterministic")],
            &[],
            all_creds_present,
            &StubProbe(ProbeOutcome::RateLimited {
                detail: "would refuse if consulted".to_owned(),
            }),
        )
        .await;
        assert!(
            result.is_ok(),
            "a placeholder provider must not be probed, got {result:?}",
        );
    }

    /// `classify_probe_status` — the HTTP-status → outcome mapping: 429 →
    /// rate-limited, 401/403 → auth-failed, anything else (200, 400, 5xx) →
    /// Ok (the auth/throttle gates were passed or the failure is not the
    /// probe's business).
    #[test]
    fn test_l1_classify_probe_status_maps_http_codes() {
        assert!(matches!(
            classify_probe_status("claude_code", 429),
            ProbeOutcome::RateLimited { .. }
        ));
        assert!(matches!(
            classify_probe_status("claude_code", 401),
            ProbeOutcome::AuthFailed { .. }
        ));
        assert!(matches!(
            classify_probe_status("claude_code", 403),
            ProbeOutcome::AuthFailed { .. }
        ));
        assert!(matches!(
            classify_probe_status("claude_code", 200),
            ProbeOutcome::Ok
        ));
        // 400 = the request was judged (bad model id, …) — auth and throttle
        // gates were already passed; not the probe's business.
        assert!(matches!(
            classify_probe_status("claude_code", 400),
            ProbeOutcome::Ok
        ));
        assert!(matches!(
            classify_probe_status("claude_code", 500),
            ProbeOutcome::Ok
        ));
    }

    /// `probe_plan_with` — the per-provider probe request shapes: `claude_code`
    /// probes the Anthropic Messages API with the OAuth bearer token (+ the
    /// oauth beta header); raw `anthropic` uses `x-api-key`; a missing
    /// credential is `Unavailable` (loud, non-blocking); an unsupported
    /// provider is `Unsupported`.
    #[test]
    fn test_l1_probe_plan_per_provider_shapes() {
        let env = |var: &str| match var {
            "CLAUDE_CODE_OAUTH_TOKEN" => Some("tok-oauth".to_owned()),
            "ANTHROPIC_API_KEY" => Some("sk-key".to_owned()),
            _ => None,
        };

        let ProbePlan::Request(req) = probe_plan_with("claude_code", "claude-opus-4-8", &env)
        else {
            panic!("claude_code with a token must yield a request");
        };
        assert!(
            req.headers
                .iter()
                .any(|h| h == "authorization: Bearer tok-oauth")
        );
        assert!(
            req.headers
                .iter()
                .any(|h| h.starts_with("anthropic-beta: oauth-")),
            "the claude-code OAuth probe needs the oauth beta header, got {:?}",
            req.headers,
        );
        assert!(req.body.contains("claude-opus-4-8"));

        let ProbePlan::Request(req) = probe_plan_with("anthropic", "claude-opus-4-8", &env) else {
            panic!("anthropic with a key must yield a request");
        };
        assert!(req.headers.iter().any(|h| h == "x-api-key: sk-key"));

        let none = |_: &str| None;
        assert!(matches!(
            probe_plan_with("claude_code", "claude-opus-4-8", &none),
            ProbePlan::Unavailable(_)
        ));
        assert!(matches!(
            probe_plan_with("openrouter", "some-model", &env),
            ProbePlan::Unsupported
        ));
    }

    /// A missing `goose` binary → `GooseMissing`.
    #[tokio::test]
    async fn test_l2_missing_goose_binary_yields_goose_missing() {
        let err = preflight_with(
            Path::new("/nonexistent/path/to/goose"),
            &[worker_phase("execute", "claude_code")],
            &[],
            all_creds_present,
            &ok_probe(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, PreflightError::GooseMissing(_)),
            "a missing goose bin must yield GooseMissing, got {err:?}",
        );
    }

    /// A stub `goose` reporting an out-of-range version → `GooseVersion`.
    #[tokio::test]
    async fn test_l2_out_of_range_goose_version_yields_goose_version() {
        let dir = TempDir::new("old-version");
        // 1.0.0 is below the >=1.34 floor (spike §Q1 — 1.0 predates stream-json).
        let goose = stub_goose(&dir.path, "goose 1.0.0");
        let err = preflight_with(
            &goose,
            &[worker_phase("execute", "claude_code")],
            &[],
            all_creds_present,
            &ok_probe(),
        )
        .await
        .unwrap_err();
        let PreflightError::GooseVersion { found, required } = &err else {
            unreachable!("an old version must yield GooseVersion, got {err:?}");
        };
        assert_eq!(found, "1.0");
        assert_eq!(required, GOOSE_VERSION_REQ);
    }

    /// A 2-provider phase set → BOTH providers are checked. The injected
    /// resolver reports `claude_code`'s creds present but the second
    /// provider's absent — the absent one fails, proving the enumeration
    /// visits every distinct provider.
    #[tokio::test]
    async fn test_l2_two_provider_phase_set_checks_both_providers() {
        let dir = TempDir::new("two-providers");
        // A goose at a satisfying version so check 1 passes and the test
        // reaches the provider check.
        let goose = stub_goose(&dir.path, "goose 1.34.1");

        let phases = vec![
            worker_phase("execute", "claude_code"),
            worker_phase("review", "provider_b"),
        ];
        // The resolver: `ANTHROPIC_API_KEY` (claude_code) present, everything
        // else absent — so the check must REACH provider_b to fail.
        let resolver = |env_var: &str| {
            if env_var == "ANTHROPIC_API_KEY" {
                Some("present".to_owned())
            } else {
                None
            }
        };
        let err = preflight_with(&goose, &phases, &[], resolver, &ok_probe())
            .await
            .unwrap_err();

        // The check reached `provider_b` (the second provider) and failed on
        // its missing creds — proving both providers were enumerated.
        let PreflightError::ProviderCreds(detail) = &err else {
            unreachable!("the missing provider's creds must fail, got {err:?}");
        };
        assert!(
            detail.contains("provider_b"),
            "the failure must name the second provider — got {detail}",
        );
    }

    /// A satisfying goose + a provider with creds present → `preflight` passes.
    #[tokio::test]
    async fn test_l2_preflight_passes_with_satisfying_version_and_creds() {
        let dir = TempDir::new("pass");
        let goose = stub_goose(&dir.path, "goose 1.34.1");
        let result = preflight_with(
            &goose,
            &[worker_phase("execute", "claude_code")],
            &[],
            all_creds_present,
            &ok_probe(),
        )
        .await;
        assert!(
            result.is_ok(),
            "a satisfying goose + present creds must pass, got {result:?}",
        );
    }

    /// A satisfying goose but ABSENT creds → `ProviderCreds` (loud, typed).
    ///
    /// Uses the raw `anthropic` provider, NOT `claude_code` — `claude_code`
    /// authenticates via its own CLI session and is exempt from the env-var
    /// check (review C-cr-4), so it could never produce this error.
    #[tokio::test]
    async fn test_l2_absent_creds_yields_provider_creds_error() {
        let dir = TempDir::new("no-creds");
        let goose = stub_goose(&dir.path, "goose 1.34.1");
        let err = preflight_with(
            &goose,
            &[worker_phase("execute", "anthropic")],
            &[],
            no_creds,
            &ok_probe(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, PreflightError::ProviderCreds(_)),
            "absent creds must yield ProviderCreds, got {err:?}",
        );
    }

    /// Regression test for C-cr-4 — `claude_code` is exempt from the
    /// credential-env-var check.
    ///
    /// `claude_code` is BOI's default provider and authenticates via its own
    /// CLI session, not `ANTHROPIC_API_KEY`. The OLD `credential_env_var`
    /// mapped `claude_code → ANTHROPIC_API_KEY`, so `preflight` on a
    /// `claude_code` phase with no `ANTHROPIC_API_KEY` set failed with
    /// `ProviderCreds` — a false-positive that would block EVERY dispatch on a
    /// working install. With the fix `claude_code` skips the env check:
    /// `preflight` passes even when the resolver reports every var absent.
    #[tokio::test]
    async fn test_l2_claude_code_provider_skips_the_credential_env_check() {
        let dir = TempDir::new("claude-code-no-env");
        let goose = stub_goose(&dir.path, "goose 1.34.1");
        // `no_creds` — EVERY env var is absent. A `claude_code` phase must
        // still pass preflight (it does not need an env-var credential).
        let result = preflight_with(
            &goose,
            &[worker_phase("execute", "claude_code")],
            &[],
            no_creds,
            &ok_probe(),
        )
        .await;
        assert!(
            result.is_ok(),
            "C-cr-4 regression: a `claude_code` phase must pass preflight with \
             NO credential env var set — it authenticates via its own CLI \
             session; got {result:?}",
        );
    }

    /// A deterministic phase's placeholder provider needs no credentials —
    /// `check_provider_creds` skips it even when the resolver reports nothing.
    #[test]
    fn test_l1_placeholder_provider_needs_no_creds() {
        assert!(check_provider_creds("deterministic", &no_creds).is_ok());
        assert!(check_provider_creds("n/a", &no_creds).is_ok());
    }

    /// `version_satisfies` enforces the `>=1.34, <2.0` window (spike §Q1).
    #[test]
    fn test_l1_version_satisfies_window() {
        assert!(!version_satisfies((1, 0)), "1.0 is below the floor");
        assert!(!version_satisfies((1, 33)), "1.33 is below the floor");
        assert!(version_satisfies((1, 34)), "1.34 is the floor — satisfies");
        assert!(version_satisfies((1, 35)), "1.35 satisfies");
        assert!(version_satisfies((1, 99)), "1.99 satisfies");
        assert!(
            !version_satisfies((2, 0)),
            "2.0 is at the ceiling — excluded"
        );
        assert!(!version_satisfies((3, 0)), "3.0 is above the ceiling");
    }

    /// `parse_version_string` extracts `(major, minor)` from `goose --version`
    /// output in its common shapes.
    #[test]
    fn test_l1_parse_version_string_shapes() {
        assert_eq!(parse_version_string("goose 1.34.1"), Some((1, 34)));
        assert_eq!(parse_version_string("1.35.0"), Some((1, 35)));
        assert_eq!(parse_version_string("v1.34.2"), Some((1, 34)));
        assert_eq!(parse_version_string("goose 2.0.0-rc-04-27-0"), Some((2, 0)));
        assert_eq!(parse_version_string("no version here"), None);
    }

    /// `credential_env_var` maps the common providers to their standard env
    /// vars and falls back to `<PROVIDER>_API_KEY` for an unknown one.
    ///
    /// `ANTHROPIC_API_KEY` belongs to the raw `anthropic` provider — NOT
    /// `claude_code` (review C-cr-4): `claude_code` is exempt from the env
    /// check entirely and never reaches `credential_env_var`.
    #[test]
    fn test_l1_credential_env_var_mapping() {
        assert_eq!(credential_env_var("anthropic"), "ANTHROPIC_API_KEY");
        assert_eq!(credential_env_var("openrouter"), "OPENROUTER_API_KEY");
        assert_eq!(credential_env_var("openai"), "OPENAI_API_KEY");
        assert_eq!(
            credential_env_var("some_new_provider"),
            "SOME_NEW_PROVIDER_API_KEY"
        );
    }

    /// `skips_env_credential_check` — `claude_code` and the placeholders are
    /// exempt; a real LLM provider is not (review C-cr-4).
    #[test]
    fn test_l1_claude_code_and_placeholders_skip_the_env_check() {
        assert!(skips_env_credential_check("claude_code"));
        assert!(skips_env_credential_check("deterministic"));
        assert!(skips_env_credential_check("n/a"));
        // `ollama` — a local model server, no API key (Phase 10 E2E fix).
        assert!(
            skips_env_credential_check("ollama"),
            "`ollama` is a local server — it needs no credential env var",
        );
        assert!(
            !skips_env_credential_check("anthropic"),
            "the raw `anthropic` provider DOES need its ANTHROPIC_API_KEY",
        );
        assert!(!skips_env_credential_check("openrouter"));
    }

    // -----------------------------------------------------------------
    // GitFlow Layer 2 — `branch_policy_gate` (R-B7).
    // -----------------------------------------------------------------

    use crate::runtime::branch_policy::testkit;

    /// An unmanaged workspace (no marker) with an existing `main` passes the
    /// gate — today's behavior, unchanged (M6 — the unmanaged-workspace path).
    #[tokio::test]
    async fn test_l2_branch_policy_gate_allows_unmanaged_main() {
        let dir = TempDir::new("policy-gate-unmanaged");
        testkit::init_repo_on_main(&dir.path);
        branch_policy_gate(dir.path.clone(), "main".to_owned())
            .await
            .expect("an unmanaged workspace allows main");
    }

    /// A gitflow workspace refuses a `main`-targeted spec at preflight (M2):
    /// `PreflightError::BranchPolicy` carrying the rendered fix hint — the
    /// detail the daemon folds into `FailureReason::PreflightFailed`.
    #[tokio::test]
    async fn test_l2_branch_policy_gate_refuses_protected_base() {
        let dir = TempDir::new("policy-gate-protected");
        testkit::init_repo_on_main(&dir.path);
        testkit::commit_on_branch(
            &dir.path,
            "main",
            &[(".boi-policy.toml", testkit::GITFLOW_MARKER)],
        );

        let err = branch_policy_gate(dir.path.clone(), "main".to_owned())
            .await
            .expect_err("a protected base must fail preflight");
        let PreflightError::BranchPolicy(detail) = &err else {
            panic!("expected BranchPolicy, got {err:?}");
        };
        assert!(detail.contains("protected"), "{detail}");
        assert!(
            detail.contains("base_branch = \"develop\""),
            "the detail teaches the fix: {detail}"
        );
    }

    /// A nonexistent `base_branch` fails preflight with the existence check's
    /// hint (M7) — the failure `types/reasons.rs`'s `PreflightFailed`
    /// round-trip example always anticipated.
    #[tokio::test]
    async fn test_l2_branch_policy_gate_refuses_missing_base() {
        let dir = TempDir::new("policy-gate-missing");
        testkit::init_repo_on_main(&dir.path);

        let err = branch_policy_gate(dir.path.clone(), "no-such-branch".to_owned())
            .await
            .expect_err("a missing base must fail preflight");
        let PreflightError::BranchPolicy(detail) = &err else {
            panic!("expected BranchPolicy, got {err:?}");
        };
        assert!(detail.contains("does not exist"), "{detail}");
        assert!(detail.contains("Fix:"), "{detail}");
    }

    /// An unreadable marker is a loud `BranchPolicy` failure (M11 / R-B2) —
    /// never silently treated as unmanaged.
    #[tokio::test]
    async fn test_l2_branch_policy_gate_refuses_invalid_marker() {
        let dir = TempDir::new("policy-gate-invalid");
        testkit::init_repo_on_main(&dir.path);
        testkit::commit_on_branch(&dir.path, "main", &[(".boi-policy.toml", "model = [42]\n")]);

        let err = branch_policy_gate(dir.path.clone(), "main".to_owned())
            .await
            .expect_err("an unreadable marker must fail preflight");
        let PreflightError::BranchPolicy(detail) = &err else {
            panic!("expected BranchPolicy, got {err:?}");
        };
        assert!(
            detail.contains("could not be read"),
            "the detail is the R-B2 taxonomy, not a silent allow: {detail}"
        );
    }
}
