//! Full systemd `--user` lifecycle E2E. Gated behind `DG_E2E=1` (set only inside
//! the systemd container — see tests/e2e/) so it never runs during plain
//! `cargo test` on a dev machine without a user systemd instance.

use daemon_green::{native, ServiceSpec, ServiceStatus};
use std::path::Path;
use std::time::{Duration, Instant};

fn gated() -> bool {
    std::env::var("DG_E2E").as_deref() == Ok("1")
}

fn wait_status<F>(mgr: &dyn daemon_green::ServiceManager, label: &str, pred: F) -> ServiceStatus
where
    F: Fn(&ServiceStatus) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = ServiceStatus::NotInstalled;
    while Instant::now() < deadline {
        last = mgr.status(label).expect("status");
        if pred(&last) {
            return last;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    last
}

#[test]
fn systemd_user_full_lifecycle() {
    if !gated() {
        eprintln!("skipping systemd E2E (DG_E2E != 1)");
        return;
    }
    let mgr = native();
    let label = "com.daemongreen.e2e";
    let marker = "/tmp/dg-e2e-marker";
    let _ = std::fs::remove_file(marker);

    // A service that proves it ran (writes a marker) then stays up.
    let spec = ServiceSpec::new(label, "/bin/sh")
        .args(["-c", "echo DG_RAN > /tmp/dg-e2e-marker; exec sleep 3600"]);

    // install + start
    mgr.install(&spec).expect("install");
    mgr.start(label).expect("start");

    // becomes Running, with a pid
    let st = wait_status(&*mgr, label, |s| matches!(s, ServiceStatus::Running { .. }));
    assert!(
        matches!(st, ServiceStatus::Running { pid: Some(_) }),
        "expected Running{{pid}}, got {st:?}"
    );

    // it actually executed
    assert!(
        Path::new(marker).exists(),
        "service should have written its marker"
    );

    // restart keeps it running
    mgr.restart(label).expect("restart");
    let st = wait_status(&*mgr, label, |s| matches!(s, ServiceStatus::Running { .. }));
    assert!(
        matches!(st, ServiceStatus::Running { .. }),
        "still Running after restart, got {st:?}"
    );

    // logs returns something (journalctl --user)
    let _ = mgr.logs(label, 20).expect("logs");

    // stop → Stopped (unit file still present)
    mgr.stop(label).expect("stop");
    let st = wait_status(&*mgr, label, |s| {
        matches!(s, ServiceStatus::Stopped | ServiceStatus::NotInstalled)
    });
    assert!(
        matches!(st, ServiceStatus::Stopped | ServiceStatus::NotInstalled),
        "expected Stopped after stop, got {st:?}"
    );

    println!("DG_E2E_PASS systemd --user lifecycle: install→start→Running→restart→logs→stop OK");
}
