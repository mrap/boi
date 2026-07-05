//! Pure unit/plist renderers. No OS calls → compiled + unit-tested on every
//! platform (so the macOS plist is tested on Linux CI and vice-versa).

use crate::ServiceSpec;

/// XML-escape a string for inclusion in a plist `<string>` value.
fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render the macOS launchd LaunchAgent plist.
///
/// Critical invariants (see daemon-abstraction design):
/// - **NO `SessionCreate`** — it detaches the job from the Aqua login session and
///   BLOCKS the login keychain (verified rc=36). A plain gui/<uid> agent inherits it.
/// - The program is `ProgramArguments[0]` **directly** (absolute path), never a
///   `bash -c` wrapper, to keep the keychain-ACL identity clean.
pub fn launchd_plist(spec: &ServiceSpec) -> String {
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    s.push_str("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n");
    s.push_str("<plist version=\"1.0\">\n<dict>\n");
    s.push_str(&format!(
        "    <key>Label</key>\n    <string>{}</string>\n",
        xml(&spec.label)
    ));

    s.push_str("    <key>ProgramArguments</key>\n    <array>\n");
    s.push_str(&format!(
        "        <string>{}</string>\n",
        xml(&spec.program.to_string_lossy())
    ));
    for a in &spec.args {
        s.push_str(&format!("        <string>{}</string>\n", xml(a)));
    }
    s.push_str("    </array>\n");

    if let Some(wd) = &spec.working_dir {
        s.push_str(&format!(
            "    <key>WorkingDirectory</key>\n    <string>{}</string>\n",
            xml(&wd.to_string_lossy())
        ));
    }

    if !spec.env.is_empty() {
        s.push_str("    <key>EnvironmentVariables</key>\n    <dict>\n");
        for (k, v) in &spec.env {
            s.push_str(&format!(
                "        <key>{}</key>\n        <string>{}</string>\n",
                xml(k),
                xml(v)
            ));
        }
        s.push_str("    </dict>\n");
    }

    if spec.run_at_load {
        s.push_str("    <key>RunAtLoad</key>\n    <true/>\n");
    }
    if spec.keep_alive {
        // Restart on crash, not on a clean (manual) stop.
        s.push_str("    <key>KeepAlive</key>\n    <dict>\n");
        s.push_str("        <key>SuccessfulExit</key>\n        <false/>\n");
        s.push_str("    </dict>\n");
    }

    if let Some(lp) = &spec.log_path {
        let p = xml(&lp.to_string_lossy());
        s.push_str(&format!(
            "    <key>StandardOutPath</key>\n    <string>{p}</string>\n"
        ));
        s.push_str(&format!(
            "    <key>StandardErrorPath</key>\n    <string>{p}</string>\n"
        ));
    }
    s.push_str("    <key>ProcessType</key>\n    <string>Background</string>\n");
    // NOTE: intentionally NO <key>SessionCreate</key>.
    s.push_str("</dict>\n</plist>\n");
    s
}

/// Render the Linux `systemd --user` `.service` unit.
pub fn systemd_unit(spec: &ServiceSpec) -> String {
    let mut exec = spec.program.to_string_lossy().to_string();
    for a in &spec.args {
        exec.push(' ');
        if a.contains(char::is_whitespace) {
            exec.push('"');
            exec.push_str(a);
            exec.push('"');
        } else {
            exec.push_str(a);
        }
    }
    let mut s = String::new();
    s.push_str("[Unit]\n");
    s.push_str(&format!("Description={}\n", spec.label));
    s.push_str("After=network-online.target\nWants=network-online.target\n");
    s.push_str("StartLimitIntervalSec=60\nStartLimitBurst=5\n\n");

    s.push_str("[Service]\nType=simple\n");
    s.push_str(&format!("ExecStart={exec}\n"));
    if let Some(wd) = &spec.working_dir {
        s.push_str(&format!("WorkingDirectory={}\n", wd.to_string_lossy()));
    }
    for (k, v) in &spec.env {
        s.push_str(&format!("Environment={k}={v}\n"));
    }
    if spec.keep_alive {
        s.push_str("Restart=always\nRestartSec=2\n");
    }
    if let Some(lp) = &spec.log_path {
        let p = lp.to_string_lossy();
        s.push_str(&format!(
            "StandardOutput=append:{p}\nStandardError=append:{p}\n"
        ));
    }
    s.push('\n');

    s.push_str("[Install]\n");
    if spec.run_at_load {
        s.push_str("WantedBy=default.target\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServiceSpec;

    fn spec() -> ServiceSpec {
        ServiceSpec::new("com.example.daemon", "/usr/local/bin/exampled")
            .arg("serve")
            .env("EX_DIR", "/home/x")
            .log_path("/tmp/ex.log")
    }

    #[test]
    fn launchd_has_no_sessioncreate_and_direct_program() {
        let p = launchd_plist(&spec());
        assert!(
            !p.contains("SessionCreate"),
            "plist MUST NOT contain SessionCreate; got:\n{p}"
        );
        assert!(
            p.contains("<string>/usr/local/bin/exampled</string>"),
            "direct program; got:\n{p}"
        );
        assert!(!p.contains("bash"), "no bash wrapper; got:\n{p}");
        assert!(p.contains("<key>Label</key>"));
        assert!(p.contains("<string>com.example.daemon</string>"));
        assert!(p.contains("EX_DIR"));
        assert!(p.contains("RunAtLoad"));
        assert!(p.contains("KeepAlive"));
        assert!(p.contains("StandardOutPath"));
    }

    #[test]
    fn launchd_escapes_xml_in_values() {
        let s = ServiceSpec::new("com.x", "/bin/x").env("Q", "a&b<c>");
        let p = launchd_plist(&s);
        assert!(
            p.contains("a&amp;b&lt;c&gt;"),
            "values must be XML-escaped; got:\n{p}"
        );
    }

    #[test]
    fn systemd_has_execstart_restart_and_wantedby() {
        let u = systemd_unit(&spec());
        assert!(
            u.contains("ExecStart=/usr/local/bin/exampled serve"),
            "got:\n{u}"
        );
        assert!(u.contains("Restart=always"), "got:\n{u}");
        assert!(u.contains("WantedBy=default.target"), "got:\n{u}");
        assert!(u.contains("Environment=EX_DIR=/home/x"), "got:\n{u}");
        assert!(u.contains("StartLimitBurst=5"), "got:\n{u}");
    }

    #[test]
    fn keep_alive_false_omits_restart_directives() {
        let s = spec().keep_alive(false);
        assert!(!systemd_unit(&s).contains("Restart=always"));
        let p = launchd_plist(&s);
        assert!(!p.contains("KeepAlive"));
    }
}
