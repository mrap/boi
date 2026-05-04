use crate::pool::{JobId, JobOutput, JobStatus, WorkerPool};
use crate::worker::WorkerConfig;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use thiserror::Error;

/// Per-second CPU rate for shared-cpu instances (Fly.io pricing).
const SHARED_CPU_RATE_PER_SEC: f64 = 0.0000026;
/// Per-second CPU rate for performance/dedicated CPU instances.
const PERFORMANCE_CPU_RATE_PER_SEC: f64 = 0.0000080;
/// Per-second memory rate per MB (Fly.io pricing).
const MEMORY_RATE_PER_MB_PER_SEC: f64 = 0.0000000032;
/// Average assumed runtime per container run when no measurement is available.
const ASSUMED_RUNTIME_SECS: f64 = 60.0;
/// Default cost cap in USD if none is configured.
const DEFAULT_MAX_COST_USD: f64 = 10.0;

/// Estimate the USD cost for one Fly.io machine run.
///
/// Uses published Fly.io per-second rates keyed by cpu_kind.
pub fn estimate_cost_usd(
    cpu_kind: &str,
    cpu_count: u32,
    memory_mb: u32,
    expected_minutes: f64,
) -> f64 {
    let cpu_rate = match cpu_kind {
        "performance" | "dedicated" => PERFORMANCE_CPU_RATE_PER_SEC,
        _ => SHARED_CPU_RATE_PER_SEC,
    };
    let total_rate = cpu_rate * cpu_count as f64
        + MEMORY_RATE_PER_MB_PER_SEC * memory_mb as f64;
    total_rate * expected_minutes * 60.0
}

#[derive(Debug, Error)]
pub enum RemoteError {
    #[error("FLY_API_TOKEN environment variable is not set")]
    MissingToken,
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Fly.io auth error (HTTP {status}): {body}")]
    Auth { status: u16, body: String },
    #[error("machine error: {0}")]
    Machine(String),
    #[error("machine did not reach stopped state within {0}s")]
    Timeout(u64),
    #[error("cost guard: estimated ${estimated:.4} exceeds limit ${max:.2} for {runs} runs")]
    CostExceeded { estimated: f64, max: f64, runs: u32 },
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
}

pub struct FlyDispatcher {
    api_token: String,
    app_name: String,
    base_url: String,
    client: Client,
    cpu_kind: String,
    cpu_count: u32,
    memory_mb: u32,
    max_cost_usd: f64,
    image: Option<String>,
}

pub struct ContainerResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub machine_id: String,
    pub cost_usd: Option<f64>,
}

// ── Fly.io Machines API request/response shapes ───────────────────────────────

#[derive(Serialize)]
struct CreateMachineRequest {
    config: MachineConfig,
}

#[derive(Serialize)]
struct MachineConfig {
    image: String,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    env: HashMap<String, String>,
    /// Fly.io uses config.init.exec to override the full command (ENTRYPOINT + CMD).
    /// The top-level config.cmd field only overrides CMD, and is often ignored.
    #[serde(skip_serializing_if = "Option::is_none")]
    init: Option<MachineInit>,
    auto_destroy: bool,
    guest: GuestConfig,
    restart: RestartPolicy,
}

#[derive(Serialize)]
struct MachineInit {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    exec: Vec<String>,
}

#[derive(Serialize)]
struct GuestConfig {
    cpu_kind: String,
    cpus: u32,
    memory_mb: u32,
}

#[derive(Serialize)]
struct RestartPolicy {
    policy: String,
}

#[derive(Deserialize, Debug)]
struct MachineResponse {
    id: String,
    state: Option<String>,
    #[serde(default)]
    events: Vec<MachineEvent>,
}

#[derive(Deserialize, Debug, Default)]
struct MachineEvent {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    request: serde_json::Value,
}

#[derive(Deserialize, Debug, Default)]
struct LogEntry {
    #[serde(default)]
    message: String,
    #[serde(default)]
    level: String,
}

// ── Constructor helpers ───────────────────────────────────────────────────────

impl FlyDispatcher {
    /// Production constructor — reads FLY_API_TOKEN (required) and
    /// FLY_APP_NAME / FLY_BASE_URL from environment with sensible defaults.
    pub fn new() -> Result<Self, RemoteError> {
        let api_token =
            std::env::var("FLY_API_TOKEN").map_err(|_| RemoteError::MissingToken)?;
        let app_name = std::env::var("FLY_APP_NAME")
            .unwrap_or_else(|_| "boi-workers".to_string());
        let base_url = std::env::var("FLY_BASE_URL")
            .unwrap_or_else(|_| "https://api.machines.dev/v1".to_string());
        Self::build(api_token, app_name, base_url)
    }

    /// Test constructor — accepts explicit configuration without touching the
    /// environment.
    pub fn new_for_test(
        api_token: impl Into<String>,
        app_name: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self, RemoteError> {
        Self::build(api_token.into(), app_name.into(), base_url.into())
    }

    fn build(
        api_token: String,
        app_name: String,
        base_url: String,
    ) -> Result<Self, RemoteError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(RemoteError::Http)?;
        Ok(Self {
            api_token,
            app_name,
            base_url,
            client,
            cpu_kind: "shared".to_string(),
            cpu_count: 1,
            memory_mb: 256,
            max_cost_usd: DEFAULT_MAX_COST_USD,
            image: None,
        })
    }

    /// Override machine sizing, app, image, and cost cap from a FlyPoolConfig.
    pub fn with_fly_config(mut self, cfg: &crate::config::FlyPoolConfig) -> Self {
        if let Some(app) = &cfg.app {
            self.app_name = app.clone();
        }
        if let Some(img) = &cfg.image {
            self.image = Some(img.clone());
        }
        if let Some(k) = &cfg.cpu_kind {
            self.cpu_kind = k.clone();
        }
        if let Some(c) = cfg.cpu_count {
            self.cpu_count = c;
        }
        if let Some(m) = cfg.memory_mb {
            self.memory_mb = m;
        }
        if let Some(cap) = cfg.max_cost_usd {
            self.max_cost_usd = cap;
        }
        self
    }
}

// ── URL helpers ───────────────────────────────────────────────────────────────

impl FlyDispatcher {
    fn auth(&self) -> String {
        // Fly Machines API requires "FlyV1" scheme, not "Bearer"
        format!("FlyV1 {}", self.api_token)
    }

    fn machines_url(&self) -> String {
        format!("{}/apps/{}/machines", self.base_url, self.app_name)
    }

    fn machine_url(&self, id: &str) -> String {
        format!("{}/apps/{}/machines/{}", self.base_url, self.app_name, id)
    }
}

// ── Core API operations ───────────────────────────────────────────────────────

impl FlyDispatcher {
    fn create_machine(
        &self,
        image: &str,
        env: HashMap<String, String>,
        cmd: Vec<String>,
    ) -> Result<String, RemoteError> {
        let init = if cmd.is_empty() {
            None
        } else {
            Some(MachineInit { exec: cmd })
        };

        let body = CreateMachineRequest {
            config: MachineConfig {
                image: image.to_string(),
                env,
                init,
                auto_destroy: false,
                guest: GuestConfig {
                    cpu_kind: self.cpu_kind.clone(),
                    cpus: self.cpu_count,
                    memory_mb: self.memory_mb,
                },
                restart: RestartPolicy { policy: "no".to_string() },
            },
        };

        let resp = self
            .client
            .post(&self.machines_url())
            .header("Authorization", self.auth())
            .json(&body)
            .send()
            .map_err(RemoteError::Http)?;

        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            let body = resp.text().unwrap_or_default();
            return Err(RemoteError::Auth { status, body });
        }

        let machine: MachineResponse = resp.json().map_err(RemoteError::Http)?;
        Ok(machine.id)
    }

    /// Wait for the machine to stop. Returns the exit code from machine events (0 if unknown).
    fn wait_for_stop(&self, id: &str, timeout_secs: u64) -> Result<i32, RemoteError> {
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);

        loop {
            if Instant::now() > deadline {
                return Err(RemoteError::Timeout(timeout_secs));
            }

            let resp = self
                .client
                .get(&self.machine_url(id))
                .header("Authorization", self.auth())
                .send()
                .map_err(RemoteError::Http)?;

            let machine: MachineResponse = resp.json().map_err(RemoteError::Http)?;

            match machine.state.as_deref() {
                Some("stopped") | Some("destroyed") => {
                    let exit_code = machine
                        .events
                        .iter()
                        .find(|e| e.kind == "exit")
                        .and_then(|e| e.request.get("exit_code").and_then(|v| v.as_i64()))
                        .unwrap_or(0) as i32;
                    return Ok(exit_code);
                }
                Some("failed") => {
                    return Err(RemoteError::Machine(format!(
                        "machine {id} reached failed state"
                    )));
                }
                _ => std::thread::sleep(Duration::from_secs(2)),
            }
        }
    }

    fn fetch_logs(&self, id: &str) -> (String, String) {
        let url = format!("{}/logs", self.machine_url(id));
        let resp = match self
            .client
            .get(&url)
            .header("Authorization", self.auth())
            .send()
        {
            Ok(r) => r,
            Err(_) => return (String::new(), String::new()),
        };

        if !resp.status().is_success() {
            return (String::new(), String::new());
        }

        let text = resp.text().unwrap_or_default();
        let mut stdout_lines = Vec::new();
        let mut stderr_lines = Vec::new();

        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<LogEntry>(line) {
                if entry.level == "error" {
                    stderr_lines.push(entry.message);
                } else {
                    stdout_lines.push(entry.message);
                }
            } else {
                // Plain-text line — treat as stdout
                stdout_lines.push(line.to_string());
            }
        }

        (stdout_lines.join("\n"), stderr_lines.join("\n"))
    }

    fn delete_machine(&self, id: &str) {
        let url = format!("{}?force=true", self.machine_url(id));
        let _ = self
            .client
            .delete(&url)
            .header("Authorization", self.auth())
            .send();
    }

    fn get_machine(&self, id: &str) -> Result<MachineResponse, RemoteError> {
        let resp = self
            .client
            .get(&self.machine_url(id))
            .header("Authorization", self.auth())
            .send()
            .map_err(RemoteError::Http)?;
        resp.json().map_err(RemoteError::Http)
    }
}

// ── Public interface ──────────────────────────────────────────────────────────

impl FlyDispatcher {
    /// Dispatch a container to Fly.io, wait for it to finish, capture its
    /// output, and delete the machine. Any error after machine creation still
    /// attempts cleanup before propagating.
    pub fn run_container(
        &self,
        image: &str,
        env: HashMap<String, String>,
        cmd: Vec<String>,
        timeout_secs: u64,
    ) -> Result<ContainerResult, RemoteError> {
        let start = Instant::now();

        let machine_id = self.create_machine(image, env, cmd)?;

        let stop_result = self.wait_for_stop(&machine_id, timeout_secs);
        // Note: Fly.io Machines API /logs endpoint is not available (returns 404).
        // stdout/stderr remain empty; result is inferred from exit_code.
        let (stdout, stderr) = self.fetch_logs(&machine_id);
        let duration_ms = start.elapsed().as_millis() as u64;

        // Always attempt cleanup, even on error.
        self.delete_machine(&machine_id);

        let exit_code = stop_result?;

        let cost_usd = Some(estimate_cost_usd(&self.cpu_kind, self.cpu_count, self.memory_mb, duration_ms as f64 / 60_000.0));

        Ok(ContainerResult {
            exit_code,
            stdout,
            stderr,
            duration_ms,
            machine_id,
            cost_usd,
        })
    }

    /// Estimate whether dispatching `estimated_runs` containers would exceed
    /// `max_cost_usd`. Uses a fixed assumed 60-second average runtime.
    /// Prefer `check_cost_guard_for_run` when cpu/mem/time are known.
    pub fn check_cost_guard(
        &self,
        estimated_runs: u32,
        max_cost_usd: f64,
    ) -> Result<(), RemoteError> {
        let estimated = estimated_runs as f64
            * estimate_cost_usd(&self.cpu_kind, self.cpu_count, self.memory_mb, ASSUMED_RUNTIME_SECS / 60.0);

        if estimated > max_cost_usd {
            return Err(RemoteError::CostExceeded {
                estimated,
                max: max_cost_usd,
                runs: estimated_runs,
            });
        }
        Ok(())
    }

    /// Guard a single planned run against the configured cost cap.
    ///
    /// Computes the estimate from actual cpu/mem sizing and expected runtime,
    /// then returns CostExceeded if `estimated > self.max_cost_usd`.
    pub fn check_cost_guard_for_run(
        &self,
        cpu_kind: &str,
        cpu_count: u32,
        memory_mb: u32,
        expected_minutes: f64,
    ) -> Result<(), RemoteError> {
        let estimated = estimate_cost_usd(cpu_kind, cpu_count, memory_mb, expected_minutes);
        if estimated > self.max_cost_usd {
            return Err(RemoteError::CostExceeded {
                estimated,
                max: self.max_cost_usd,
                runs: 1,
            });
        }
        Ok(())
    }
}

// ── WorkerPool adapter ────────────────────────────────────────────────────────

impl WorkerPool for FlyDispatcher {
    /// POST /apps/{app}/machines — create and start a machine, return its ID.
    fn spawn(
        &self,
        spec_id: &str,
        spec_path: &str,
        workspace_path: &str,
        config: &WorkerConfig,
    ) -> anyhow::Result<JobId> {
        let image = self.image.clone()
            .or_else(|| std::env::var("FLY_IMAGE").ok())
            .unwrap_or_else(|| "registry.fly.io/boi-workers:latest".to_string());

        let mut env = HashMap::new();
        env.insert("BOI_SPEC_ID".to_string(), spec_id.to_string());
        env.insert("BOI_SPEC_PATH".to_string(), spec_path.to_string());
        env.insert("BOI_WORKSPACE".to_string(), workspace_path.to_string());
        env.insert("BOI_TIMEOUT".to_string(), config.task_timeout_secs.to_string());

        let expected_minutes = config.task_timeout_secs as f64 / 60.0;
        self.check_cost_guard_for_run(
            &self.cpu_kind.clone(),
            self.cpu_count,
            self.memory_mb,
            expected_minutes,
        )
        .map_err(|e| anyhow::anyhow!("fly spawn cost guard: {e}"))?;

        let machine_id = self
            .create_machine(&image, env, vec![])
            .map_err(|e| anyhow::anyhow!("fly spawn: {e}"))?;
        Ok(JobId::new(machine_id))
    }

    /// GET /apps/{app}/machines/{id} — map machine state to JobStatus.
    fn status(&self, job_id: &JobId) -> anyhow::Result<JobStatus> {
        let machine = self
            .get_machine(job_id.as_str())
            .map_err(|e| anyhow::anyhow!("fly status: {e}"))?;

        let exit_code = || {
            machine
                .events
                .iter()
                .find(|e| e.kind == "exit")
                .and_then(|e| e.request.get("exit_code").and_then(|v| v.as_i64()))
                .unwrap_or(0)
        };

        let status = match machine.state.as_deref() {
            Some("stopped") | Some("destroyed") => {
                if exit_code() == 0 { JobStatus::Completed } else { JobStatus::Failed }
            }
            Some("failed") => JobStatus::Failed,
            None => JobStatus::Unknown,
            _ => JobStatus::Running,
        };
        Ok(status)
    }

    /// GET exit code from machine events + fetch logs.
    fn collect(&self, job_id: &JobId) -> anyhow::Result<JobOutput> {
        let machine = self
            .get_machine(job_id.as_str())
            .map_err(|e| anyhow::anyhow!("fly collect: {e}"))?;

        let exit_code = machine
            .events
            .iter()
            .find(|e| e.kind == "exit")
            .and_then(|e| e.request.get("exit_code").and_then(|v| v.as_i64()))
            .unwrap_or(0) as i32;

        let (stdout, stderr) = self.fetch_logs(job_id.as_str());
        Ok(JobOutput { exit_code, stdout, stderr })
    }

    /// POST /apps/{app}/machines/{id}/stop — signal the machine to stop.
    fn cancel(&self, job_id: &JobId) -> anyhow::Result<()> {
        let stop_url = format!("{}/stop", self.machine_url(job_id.as_str()));
        let _ = self
            .client
            .post(&stop_url)
            .header("Authorization", self.auth())
            .send();
        Ok(())
    }

    /// DELETE /apps/{app}/machines/{id}?force=true — destroy the machine.
    fn cleanup(&self, job_id: &JobId) -> anyhow::Result<()> {
        self.delete_machine(job_id.as_str());
        Ok(())
    }

    fn max_workers(&self) -> u32 {
        10
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod fly_dispatch {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    // A minimal test HTTP server that handles one request per pre-programmed
    // response. `Connection: close` forces reqwest to reconnect each time so
    // the server can serve responses sequentially from a single thread.
    struct MockServer {
        port: u16,
        _handle: std::thread::JoinHandle<()>,
    }

    impl MockServer {
        fn new(responses: Vec<(u16, String)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();

            let handle = std::thread::spawn(move || {
                for (status, body) in responses {
                    let Ok((mut stream, _)) = listener.accept() else {
                        break;
                    };
                    // Drain the incoming request (avoid broken-pipe on client)
                    let mut buf = [0u8; 8192];
                    let _ = stream.read(&mut buf);

                    let resp = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes());
                }
            });

            // Give the server a moment to bind before tests fire requests.
            std::thread::sleep(Duration::from_millis(10));

            Self { port, _handle: handle }
        }

        fn url(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }
    }

    fn dispatcher(server: &MockServer) -> FlyDispatcher {
        FlyDispatcher::new_for_test("test-token", "boi-workers", server.url()).unwrap()
    }

    // ── Cost guard unit tests (no HTTP) ──────────────────────────────────────

    #[test]
    fn test_cost_guard_passes_within_limit() {
        let server = MockServer::new(vec![]);
        let d = dispatcher(&server);
        // 1 run × 60s × 0.0000026/s ≈ $0.000156 — well under $10
        assert!(d.check_cost_guard(1, 10.0).is_ok());
    }

    #[test]
    fn test_cost_guard_passes_many_runs_large_budget() {
        let server = MockServer::new(vec![]);
        let d = dispatcher(&server);
        // 900 runs × 60s × 0.0000026/s ≈ $0.14 — under $10
        assert!(d.check_cost_guard(900, 10.0).is_ok());
    }

    #[test]
    fn test_cost_guard_fails_over_limit() {
        let server = MockServer::new(vec![]);
        let d = dispatcher(&server);
        // 1_000_000 runs would cost ~$156 — must be rejected
        let err = d.check_cost_guard(1_000_000, 10.0).unwrap_err();
        assert!(
            matches!(err, RemoteError::CostExceeded { .. }),
            "expected CostExceeded, got {err}"
        );
    }

    #[test]
    fn test_cost_guard_zero_runs_always_passes() {
        let server = MockServer::new(vec![]);
        let d = dispatcher(&server);
        assert!(d.check_cost_guard(0, 0.0).is_ok());
    }

    #[test]
    fn test_cost_guard_rejects_at_tiny_budget() {
        let server = MockServer::new(vec![]);
        let d = dispatcher(&server);
        // Budget of $0.0001 is below the per-run estimate → rejected
        let result = d.check_cost_guard(1, 0.0001);
        assert!(
            result.is_err(),
            "expected error for tiny budget but got Ok"
        );
    }

    // ── HTTP mock tests ───────────────────────────────────────────────────────

    #[test]
    fn test_create_machine_returns_id() {
        let server = MockServer::new(vec![(
            200,
            r#"{"id":"m-abc123","state":"created"}"#.to_string(),
        )]);
        let d = dispatcher(&server);
        let id = d
            .create_machine("registry.fly.io/boi-workers:latest", HashMap::new(), vec![])
            .unwrap();
        assert_eq!(id, "m-abc123");
    }

    #[test]
    fn test_create_machine_auth_error() {
        let server = MockServer::new(vec![(401, r#"{"error":"unauthorized"}"#.to_string())]);
        let d = dispatcher(&server);
        let err = d
            .create_machine("registry.fly.io/boi-workers:latest", HashMap::new(), vec![])
            .unwrap_err();
        assert!(
            matches!(err, RemoteError::Auth { status: 401, .. }),
            "expected Auth error, got {err}"
        );
    }

    #[test]
    fn test_wait_for_stop_succeeds_immediately() {
        let server = MockServer::new(vec![(
            200,
            r#"{"id":"m-abc123","state":"stopped","events":[]}"#.to_string(),
        )]);
        let d = dispatcher(&server);
        let exit_code = d.wait_for_stop("m-abc123", 10).unwrap();
        assert_eq!(exit_code, 0);
    }

    #[test]
    fn test_wait_for_stop_returns_exit_code_from_events() {
        let server = MockServer::new(vec![(
            200,
            r#"{"id":"m-abc123","state":"stopped","events":[{"type":"exit","request":{"exit_code":1}}]}"#.to_string(),
        )]);
        let d = dispatcher(&server);
        let exit_code = d.wait_for_stop("m-abc123", 10).unwrap();
        assert_eq!(exit_code, 1);
    }

    #[test]
    fn test_wait_for_stop_failed_state_returns_machine_error() {
        let server = MockServer::new(vec![(
            200,
            r#"{"id":"m-abc123","state":"failed","events":[]}"#.to_string(),
        )]);
        let d = dispatcher(&server);
        let err = d.wait_for_stop("m-abc123", 10).unwrap_err();
        assert!(
            matches!(err, RemoteError::Machine(_)),
            "expected Machine error, got {err}"
        );
    }

    #[test]
    fn test_fetch_logs_parses_ndjson() {
        let ndjson = concat!(
            r#"{"message":"hello stdout","level":"info"}"#,
            "\n",
            r#"{"message":"oops","level":"error"}"#,
            "\n"
        );
        let server = MockServer::new(vec![(200, ndjson.to_string())]);
        let d = dispatcher(&server);
        let (stdout, stderr) = d.fetch_logs("m-abc123");
        assert_eq!(stdout, "hello stdout");
        assert_eq!(stderr, "oops");
    }

    #[test]
    fn test_missing_token_error() {
        // Unset env var — new() must return MissingToken (not panic)
        std::env::remove_var("FLY_API_TOKEN");
        let result = FlyDispatcher::new();
        assert!(
            matches!(result, Err(RemoteError::MissingToken)),
            "expected MissingToken error"
        );
    }
}

// ── fly_cost_guard: per-run estimate + guard tests ────────────────────────────

#[cfg(test)]
mod fly_cost_guard {
    use super::*;

    fn dispatcher() -> FlyDispatcher {
        FlyDispatcher::new_for_test("tok", "app", "http://127.0.0.1:1").unwrap()
    }

    // ── estimate_cost_usd ────────────────────────────────────────────────────

    #[test]
    fn fly_cost_guard_estimate_shared_cpu_1x_256mb_1min() {
        let cost = estimate_cost_usd("shared", 1, 256, 1.0);
        // 0.0000026 * 1 + 0.0000000032 * 256 = 0.0000026 + ~0.000000819 = ~0.000003419
        // × 60s = ~0.000205
        assert!(cost > 0.0001 && cost < 0.01, "unexpected cost: {cost}");
    }

    #[test]
    fn fly_cost_guard_estimate_performance_cpu_costs_more() {
        let shared = estimate_cost_usd("shared", 2, 512, 5.0);
        let perf = estimate_cost_usd("performance", 2, 512, 5.0);
        assert!(perf > shared, "performance CPUs must cost more than shared");
    }

    #[test]
    fn fly_cost_guard_estimate_longer_runtime_costs_more() {
        let short = estimate_cost_usd("shared", 1, 256, 1.0);
        let long = estimate_cost_usd("shared", 1, 256, 60.0);
        assert!(long > short, "longer runtime must cost more");
        // Should be approximately 60× more
        assert!((long / short - 60.0).abs() < 1.0, "cost should scale linearly with time");
    }

    #[test]
    fn fly_cost_guard_estimate_zero_minutes_is_zero() {
        let cost = estimate_cost_usd("shared", 4, 1024, 0.0);
        assert_eq!(cost, 0.0);
    }

    // ── check_cost_guard_for_run ─────────────────────────────────────────────

    #[test]
    fn fly_cost_guard_passes_when_cost_under_cap() {
        let d = dispatcher();
        // shared-1x 256MB 1 minute ≈ $0.0002 — well under $10 default cap
        assert!(d.check_cost_guard_for_run("shared", 1, 256, 1.0).is_ok());
    }

    #[test]
    fn fly_cost_guard_fails_when_cost_over_cap() {
        // Use a $1.00 cap. performance-8x 8GB for 10 hours ≈ $3.25 → exceeds cap.
        let cfg = crate::config::FlyPoolConfig {
            max_cost_usd: Some(1.0),
            cpu_kind: Some("performance".to_string()),
            cpu_count: Some(8),
            memory_mb: Some(8192),
            ..Default::default()
        };
        let d = FlyDispatcher::new_for_test("tok", "app", "http://127.0.0.1:1")
            .unwrap()
            .with_fly_config(&cfg);
        let err = d.check_cost_guard_for_run("performance", 8, 8192, 600.0).unwrap_err();
        assert!(
            matches!(err, RemoteError::CostExceeded { .. }),
            "expected CostExceeded, got {err}"
        );
    }

    #[test]
    fn fly_cost_guard_custom_cap_from_config() {
        let cfg = crate::config::FlyPoolConfig {
            max_cost_usd: Some(0.001),
            cpu_kind: Some("shared".to_string()),
            cpu_count: Some(1),
            memory_mb: Some(256),
            ..Default::default()
        };
        let d = FlyDispatcher::new_for_test("tok", "app", "http://127.0.0.1:1")
            .unwrap()
            .with_fly_config(&cfg);
        // $0.001 cap — even a 5-minute run should hit the cap
        let result = d.check_cost_guard_for_run("shared", 1, 256, 5.0);
        assert!(result.is_err(), "should fail with low custom cap");
    }

    #[test]
    fn fly_cost_guard_default_cap_is_ten_dollars() {
        let d = dispatcher();
        assert_eq!(d.max_cost_usd, DEFAULT_MAX_COST_USD);
    }

    #[test]
    fn fly_cost_guard_spawn_blocked_by_guard() {
        use crate::worker::WorkerConfig;
        let cfg = crate::config::FlyPoolConfig {
            max_cost_usd: Some(0.0), // $0 cap — always reject
            cpu_kind: Some("shared".to_string()),
            cpu_count: Some(1),
            memory_mb: Some(256),
            ..Default::default()
        };
        let d = FlyDispatcher::new_for_test("tok", "app", "http://127.0.0.1:1")
            .unwrap()
            .with_fly_config(&cfg);
        let worker_cfg = WorkerConfig { task_timeout_secs: 60, ..WorkerConfig::default() };
        let err = d.spawn("spec-1", "/tmp/spec.yaml", "/tmp/ws", &worker_cfg).unwrap_err();
        assert!(
            err.to_string().contains("cost guard"),
            "spawn must fail with cost guard error, got: {err}"
        );
    }
}
