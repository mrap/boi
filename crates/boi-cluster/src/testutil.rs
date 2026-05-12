//! Shared helpers for boi-cluster's live-etcd tests.
//!
//! Each schema module's tests spin up its own bitnami/etcd:3.5 container
//! and exercise a real etcd. When Docker is not available the caller
//! cleanly returns Ok, so `cargo test -p boi-cluster` is green on
//! machines without a container runtime (same pattern as `client.rs`).

#![cfg(test)]

use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
    GenericImage, ImageExt,
};

pub(crate) fn docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub(crate) async fn etcd_endpoint(
) -> Option<(testcontainers::ContainerAsync<GenericImage>, String)> {
    if !docker_available() {
        eprintln!("docker not available — skipping live-etcd subtest");
        return None;
    }
    let img = GenericImage::new("bitnami/etcd", "3.5")
        .with_exposed_port(2379.tcp())
        .with_wait_for(WaitFor::message_on_stderr("ready to serve client requests"))
        .with_env_var("ALLOW_NONE_AUTHENTICATION", "yes")
        .with_env_var("ETCD_ADVERTISE_CLIENT_URLS", "http://0.0.0.0:2379")
        .with_env_var("ETCD_LISTEN_CLIENT_URLS", "http://0.0.0.0:2379");
    let container = match img.start().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to start etcd container; skipping: {e}");
            return None;
        }
    };
    let port = match container.get_host_port_ipv4(2379).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("failed to read mapped port; skipping: {e}");
            return None;
        }
    };
    Some((container, format!("http://127.0.0.1:{port}")))
}
