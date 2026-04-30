use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use thiserror::Error;

/// Per-second cost estimate for shared-cpu-1x (1 vCPU, 256 MB).
/// Source: Fly.io pricing — ~$0.0000026/sec at this size.
const PER_SECOND_RATE_USD: f64 = 0.0000026;
/// Average assumed runtime per container run when no measurement is available.
const ASSUMED_RUNTIME_SECS: f64 = 60.0;

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
        Ok(Self { api_token, app_name, base_url, client })
    }
}

// ── URL helpers ───────────────────────────────────────────────────────────────

impl FlyDispatcher {
    fn auth(&self) -> String {
        format!("Bearer {}", self.api_token)
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
                    cpu_kind: "shared".to_string(),
                    cpus: 1,
                    memory_mb: 256,
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

        let cost_usd = Some((duration_ms as f64 / 1000.0) * PER_SECOND_RATE_USD);

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
    /// `max_cost_usd`. Uses the per-second rate and an assumed 60-second
    /// average runtime. Callers should pass in a tighter estimate when known.
    pub fn check_cost_guard(
        &self,
        estimated_runs: u32,
        max_cost_usd: f64,
    ) -> Result<(), RemoteError> {
        let estimated =
            estimated_runs as f64 * ASSUMED_RUNTIME_SECS * PER_SECOND_RATE_USD;

        if estimated > max_cost_usd {
            return Err(RemoteError::CostExceeded {
                estimated,
                max: max_cost_usd,
                runs: estimated_runs,
            });
        }
        Ok(())
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
