//! Smoke test for the harness itself.
//!
//! Brings up only the `etcd` service from the compose file and asserts
//! the readiness probe succeeds. No `boi-node` is started, so this test
//! does not depend on Phase 1+ implementation and PASSES in the red
//! baseline. Its job is to prove the harness scaffolding is intact.
//!
//! Skipped (not failed) when `docker` is not available on PATH, so
//! `cargo test -p boi-test-harness` works on dev machines without
//! docker installed.

use std::process::Command;
use std::time::Duration;

use boi_test_harness::{docker_available, docker_dir, wait_for_etcd_key};

#[test]
fn harness_smoke_etcd_only() {
    if !docker_available() {
        eprintln!("SKIP harness_smoke_etcd_only: docker not on PATH");
        return;
    }

    let compose = docker_dir().join("docker-compose.yaml");
    assert!(
        compose.exists(),
        "compose file should exist at {}",
        compose.display()
    );

    let up = Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(&compose)
        .arg("up")
        .arg("-d")
        .arg("etcd")
        .status()
        .expect("invoke docker compose up");
    assert!(up.success(), "docker compose up etcd failed");

    // Readiness: wait until etcd serves an empty /boi/ prefix without error.
    let waited = wait_for_etcd_key("/boi/", |_kvs| true, Duration::from_secs(15));

    let _ = Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(&compose)
        .arg("down")
        .arg("-v")
        .status();

    waited.expect("etcd should be reachable within 15s");
}
