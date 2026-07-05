//! Red test for the disabled-label EIO-5 fix.
//!
//! Root cause (2026-06-06): `launchctl bootstrap gui/<uid>/<label>` fails with
//! `Input/output error` (errno 5) whenever the label is in launchd's *disabled*
//! state. `bootout` does NOT re-enable a disabled label, so once disabled the
//! daemon can never restart. The fix is to call `launchctl enable
//! gui/<uid>/<label>` after the bootout/wait and before the bootstrap retry,
//! mirroring the systemd backend's `systemctl enable --now`.
//!
//! This test pins the source-level invariant required by the spec's
//! `enable-call-present` and `enable-before-bootstrap` verifications.

use std::fs;

#[test]
fn bootstrap_robust_invokes_launchctl_enable_after_bootout_and_before_bootstrap() {
    let src = fs::read_to_string("src/launchd.rs").expect("read src/launchd.rs");

    // Locate bootstrap_robust().
    let fn_start = src
        .find("fn bootstrap_robust(")
        .expect("bootstrap_robust() must exist in src/launchd.rs");
    let body = &src[fn_start..];

    // Find the bootout call and the first bootstrap call inside the function.
    let bootout_idx = body
        .find("\"bootout\"")
        .expect("bootstrap_robust() must call launchctl bootout");
    let bootstrap_idx = body
        .find("\"bootstrap\"")
        .expect("bootstrap_robust() must call launchctl bootstrap");
    let enable_idx = body.find("\"enable\"").unwrap_or(usize::MAX);

    assert!(
        enable_idx != usize::MAX,
        "bootstrap_robust() must invoke `launchctl enable` to self-heal a \
         disabled label (root cause of the EIO-5 bootstrap failure)."
    );
    assert!(
        enable_idx > bootout_idx,
        "`launchctl enable` must come AFTER the bootout/wait step \
         (enable is the self-heal for a disabled label)."
    );
    assert!(
        enable_idx < bootstrap_idx,
        "`launchctl enable` must come BEFORE the bootstrap retry — \
         a disabled label cannot be bootstrapped (macOS surfaces it as EIO-5)."
    );
}
