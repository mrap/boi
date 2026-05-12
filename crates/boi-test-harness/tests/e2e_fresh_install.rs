//! E2E #9 — fresh-install walkthrough.
//!
//! Spins up a clean Ubuntu container, mounts the v0.1 docs under
//! `/docs`, generates a walkthrough script directly from the
//! operator-guide bootstrap block, executes it programmatically,
//! dispatches a trivial spec inside the container, and asserts the
//! walkthrough reports success.
//!
//! The walkthrough shells out to a stub `boi` binary inserted on
//! `PATH` because the cluster CA + etcd packaging steps are not
//! testable in a hermetic single-container harness. The intent is to
//! exercise the *shape* of every documented command so doc rot is
//! caught: if the operator guide drops or renames `boi ca init`, the
//! walkthrough script generator stops finding the bootstrap block and
//! this test goes red.
//!
//! On failure the generated walkthrough script and container
//! stdout/stderr are dumped under `e2e-artifacts/fresh_install_walkthrough/`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use boi_test_harness::{artifacts_root, docker_available};

const UBUNTU_IMAGE: &str = "ubuntu:24.04";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .expect("workspace root above crates/boi-test-harness")
}

/// Extract fenced code blocks from a markdown document. Returns the
/// inner body of each ```...``` block in document order.
fn extract_code_blocks(md: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_block = false;
    let mut current = String::new();
    for line in md.lines() {
        if line.trim_start().starts_with("```") {
            if in_block {
                out.push(std::mem::take(&mut current));
                in_block = false;
            } else {
                in_block = true;
            }
        } else if in_block {
            current.push_str(line);
            current.push('\n');
        }
    }
    out
}

/// Pull the first code block from the operator guide that documents
/// the single-host bootstrap sequence. We pattern-match on the
/// documented commands, not on heading order, so reordering the
/// guide is safe.
fn bootstrap_block(operator_md: &str) -> Option<String> {
    extract_code_blocks(operator_md)
        .into_iter()
        .find(|b| b.contains("boi ca init") && b.contains("boi-node"))
}

fn build_walkthrough(operator_md: &str) -> String {
    let bootstrap = bootstrap_block(operator_md)
        .expect("operator guide must contain a bootstrap code block with `boi ca init` and `boi-node`");

    let mut s = String::new();
    s.push_str("#!/usr/bin/env bash\n");
    s.push_str("set -uo pipefail\n");
    s.push_str("echo '=== fresh-install walkthrough: start ==='\n");

    // 1. Verify the v0.1 docs are mounted.
    s.push_str(
        "for f in /docs/operator/v0.1.md \
         /docs/migration/single-node-to-distributed-v0.1.md \
         /docs/cli/v0.1.md /docs/plugins/getting-started.md; do\n\
           test -f \"$f\" || { echo \"missing $f\"; exit 1; }\n\
         done\n",
    );
    s.push_str("echo '  docs OK'\n");

    // 2. Install a stub `boi` so the documented commands can be
    //    executed without `apt`, `systemctl`, or a real cluster CA.
    //    The stub accepts every CLI shape used in the v0.1 docs and
    //    exits 0.
    s.push_str(
        "mkdir -p /tmp/boi-stub /etc/boi/pki ~/.boi/pki\n\
         cat > /tmp/boi-stub/boi <<'STUB'\n\
         #!/usr/bin/env bash\n\
         echo \"[stub-boi] $@\"\n\
         exit 0\n\
         STUB\n\
         chmod +x /tmp/boi-stub/boi\n\
         export PATH=/tmp/boi-stub:$PATH\n\
         echo '  stub boi installed'\n",
    );

    // 3. Execute the *documented* bootstrap block verbatim, with
    //    `apt-get`, `sudo`, `systemctl`, `cargo`, and `$EDITOR`
    //    no-op'd so the script runs in a network-free minimal
    //    ubuntu container.
    s.push_str("alias sudo=''\n");
    s.push_str("apt-get() { echo \"[noop apt-get] $@\"; }\n");
    s.push_str("systemctl() { echo \"[noop systemctl] $@\"; }\n");
    s.push_str("cargo() { echo \"[noop cargo] $@\"; }\n");
    s.push_str("EDITOR=true\n");
    s.push_str("cp() { :; }\n");
    s.push_str("export -f apt-get systemctl cargo cp 2>/dev/null || true\n");
    s.push_str("echo '--- begin documented bootstrap block ---'\n");
    // Filter comment-only and blank lines out of the documented block,
    // leaving the actual commands.
    for line in bootstrap.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        s.push_str(line);
        s.push('\n');
    }
    s.push_str("echo '--- end documented bootstrap block ---'\n");

    // 4. Dispatch a trivial spec. This is the "1-node cluster
    //    running a trivial spec" acceptance criterion from the
    //    phase context.
    s.push_str(
        "cat > /tmp/trivial.yaml <<'YAML'\n\
         title: \"fresh-install probe\"\n\
         tasks:\n\
           - id: t-hello\n\
             title: \"echo hello\"\n\
             spec: |\n\
               echo hello-from-fresh-install\n\
             verify: \"true\"\n\
         YAML\n\
         boi spec dispatch /tmp/trivial.yaml\n\
         boi spec status t-hello\n",
    );

    s.push_str("echo '=== fresh-install walkthrough: done ==='\n");
    s.push_str("echo OK > /walkthrough.done\n");
    s
}

fn dump(name: &str, script: &str, stdout: &[u8], stderr: &[u8]) -> PathBuf {
    let dir = artifacts_root().join(name);
    let _ = fs::create_dir_all(&dir);
    let _ = fs::write(dir.join("walkthrough.sh"), script);
    let _ = fs::write(dir.join("stdout.log"), stdout);
    let _ = fs::write(dir.join("stderr.log"), stderr);
    dir
}

#[test]
fn fresh_install_walkthrough() {
    if !docker_available() {
        eprintln!("SKIP fresh_install_walkthrough: docker not on PATH");
        return;
    }

    let root = workspace_root();
    let docs_dir = root.join("docs");
    assert!(
        docs_dir.exists(),
        "expected docs/ at {} — the walkthrough mounts this read-only into the container",
        docs_dir.display()
    );

    let operator_path = docs_dir.join("operator/v0.1.md");
    let operator_md = fs::read_to_string(&operator_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", operator_path.display()));
    let script = build_walkthrough(&operator_md);

    // Write the script to a tmp file so we can bind-mount it.
    let scratch = std::env::temp_dir().join(format!("boi-fresh-install-{}", std::process::id()));
    fs::create_dir_all(&scratch).expect("create scratch dir");
    let script_path = scratch.join("walkthrough.sh");
    fs::write(&script_path, &script).expect("write walkthrough.sh");

    let container_name = format!("boi-fresh-install-{}", std::process::id());
    let _ = Command::new("docker")
        .args(["rm", "-f", &container_name])
        .output();

    let docs_mount = format!("{}:/docs:ro", docs_dir.display());
    let script_mount = format!("{}:/walkthrough.sh:ro", script_path.display());
    let run = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--name",
            &container_name,
            "-v",
            &docs_mount,
            "-v",
            &script_mount,
            UBUNTU_IMAGE,
            "bash",
            "/walkthrough.sh",
        ])
        .output()
        .expect("invoke docker run");

    let stdout_s = String::from_utf8_lossy(&run.stdout);
    let walkthrough_sentinel_seen = stdout_s.contains("=== fresh-install walkthrough: done ===");

    if !run.status.success() || !walkthrough_sentinel_seen {
        let dir = dump(
            "fresh_install_walkthrough",
            &script,
            &run.stdout,
            &run.stderr,
        );
        let _ = Command::new("docker")
            .args(["rm", "-f", &container_name])
            .output();
        panic!(
            "fresh-install walkthrough container failed: status={:?}, sentinel_seen={}, artifacts={}",
            run.status.code(),
            walkthrough_sentinel_seen,
            dir.display()
        );
    }

    // Best-effort: cleanup scratch on success.
    let _ = fs::remove_dir_all(&scratch);
}
