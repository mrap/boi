//! Provider secrets bootstrap — load `~/.boi/v2/secrets/*.env` into the
//! process environment before the tokio runtime starts.
//!
//! ## Why this module exists
//!
//! The BOI daemon runs as a launchd service. LaunchAgent plists must never
//! contain secret values. Instead, operators place `KEY=value` env files in
//! `~/.boi/v2/secrets/` and this module reads them into the process env at
//! daemon startup, before any provider (e.g. claude-code) attempts auth.
//!
//! Non-hex users: drop `claude.env` (or any `*.env`) in `~/.boi/v2/secrets/`.
//! Hex users: symlink `~/hex/.hex/secrets/claude.env` → `~/.boi/v2/secrets/claude.env`.
//! No BOI code references hex paths.
//!
//! ## Threading safety
//!
//! `std::env::set_var` is unsound in a multi-threaded context. Call
//! [`bootstrap_provider_env`] in a synchronous `main()` preamble **before**
//! `tokio::Runtime::new()` (i.e., before `#[tokio::main]` or
//! `Runtime::block_on`). The call site in `main.rs` enforces this.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// A secrets directory or file has unsafe permissions.
#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    /// The secrets directory is group/world-accessible (must be `0700`).
    #[error("secrets dir {path} has unsafe permissions {mode:#o}; run: chmod 700 {path}")]
    UnsafeDirMode {
        /// The offending directory.
        path: String,
        /// The observed permission bits.
        mode: u32,
    },
    /// A secrets file is group/world-readable (must be `0600`).
    #[error("secrets file {path} has unsafe permissions {mode:#o}; run: chmod 600 {path}")]
    UnsafeFileMode {
        /// The offending file.
        path: String,
        /// The observed permission bits.
        mode: u32,
    },
}

/// Load every `*.env` file from `secrets_dir` into the process environment.
///
/// - Skips the directory silently if it does not exist (non-hex user with no
///   secrets configured — valid state).
/// - Returns [`SecretsError`] if the directory or any file is
///   group/world-readable. The daemon should exit on this error so the
///   operator sees it immediately rather than failing later with a cryptic
///   provider auth error.
/// - Follows symlinks when checking file permissions (`metadata()`, not
///   `symlink_metadata()`), so a symlinked target file must also be 0600.
/// - Logs each loaded key name (never the value) to stderr, but only when
///   `BOI_VERBOSE` is set — this runs in `main()` before `clap` gets a
///   chance to parse args (see the `main.rs` call site), so an unconditional
///   `eprintln!` here would print on *every* invocation, including
///   `--help`/`--version` (adversarial-review item: stderr noise).
/// - Malformed lines (no `=`) are logged and skipped; they do not abort.
///
/// # Safety
///
/// Must be called before the tokio runtime starts. `set_var` is not
/// async-signal-safe and is unsound in a multi-threaded context.
#[allow(unsafe_code)]
pub fn bootstrap_provider_env(secrets_dir: &Path) -> Result<(), SecretsError> {
    if !secrets_dir.exists() {
        return Ok(());
    }

    let dir_mode = secrets_dir
        .metadata()
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0o777);
    if dir_mode & 0o077 != 0 {
        return Err(SecretsError::UnsafeDirMode {
            path: secrets_dir.display().to_string(),
            mode: dir_mode,
        });
    }

    let entries = match std::fs::read_dir(secrets_dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!(
                "boi: could not read secrets dir {}: {e}",
                secrets_dir.display()
            );
            return Ok(());
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("env") {
            continue;
        }

        // metadata() follows symlinks — the target file must also be 0600.
        let file_mode = path
            .metadata()
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0o177);
        if file_mode & 0o077 != 0 {
            return Err(SecretsError::UnsafeFileMode {
                path: path.display().to_string(),
                mode: file_mode,
            });
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("boi: skipping {}: {e}", path.display());
                continue;
            }
        };

        for raw in content.lines() {
            match parse_env_line(raw) {
                EnvLine::Pair(k, v) => {
                    // SAFETY: called before tokio::Runtime::new(); single-threaded.
                    unsafe { std::env::set_var(k, v) };
                    // Opt-in only (`BOI_VERBOSE`) — see the doc comment above.
                    if std::env::var_os("BOI_VERBOSE").is_some() {
                        eprintln!("boi: loaded {k} from {}", path.display());
                    }
                }
                EnvLine::Skip => {}
                EnvLine::Malformed(line) => {
                    eprintln!("boi: malformed line in {}: {:?}", path.display(), line);
                }
            }
        }
    }

    Ok(())
}

/// One parsed line of a secrets `*.env` file.
enum EnvLine<'a> {
    /// A `KEY=value` pair, key trimmed and value unquoted.
    Pair(&'a str, &'a str),
    /// Blank line or `#` comment — silently skippable.
    Skip,
    /// Non-comment payload with no `=` — the caller logs it.
    Malformed(&'a str),
}

/// Parse one raw line: optional `export` prefix, `'`/`"`-unquoting, blank/
/// comment skipping. Pure — no environment access.
fn parse_env_line(raw: &str) -> EnvLine<'_> {
    let line = raw.trim().trim_start_matches("export").trim();
    if line.is_empty() || line.starts_with('#') {
        return EnvLine::Skip;
    }
    match line.split_once('=') {
        Some((k, v)) => EnvLine::Pair(k.trim(), v.trim().trim_matches('\'').trim_matches('"')),
        None => EnvLine::Malformed(line),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str, dir_mode: u32) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("boi-secrets-{}-{tag}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temp dir");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(dir_mode))
                .expect("chmod temp dir");
            TempDir { path }
        }

        fn write_env(&self, name: &str, mode: u32, content: &str) {
            let p = self.path.join(name);
            std::fs::write(&p, content).expect("write env file");
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode))
                .expect("chmod env file");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.path));
        }
    }

    // NOTE: every test below stays on paths that return BEFORE any
    // `set_var` — the env-mutating happy path is single-threaded-preamble
    // only and is exercised (loudly) at every CLI startup. Rationale:
    // tests/E2E_COVERED.toml `l2_sufficient_modules`.

    #[test]
    fn test_l2_secrets_missing_dir_is_valid_noop() {
        let t = TempDir::new("missing", 0o700);
        assert!(bootstrap_provider_env(&t.path.join("nope")).is_ok());
    }

    #[test]
    fn test_l2_secrets_rejects_group_accessible_dir() {
        let t = TempDir::new("dirmode", 0o750);
        match bootstrap_provider_env(&t.path) {
            Err(SecretsError::UnsafeDirMode { mode, .. }) => assert_eq!(mode, 0o750),
            other => panic!("expected UnsafeDirMode, got {other:?}"),
        }
    }

    #[test]
    fn test_l2_secrets_rejects_world_readable_env_file() {
        let t = TempDir::new("filemode", 0o700);
        t.write_env("claude.env", 0o644, "K=v\n");
        match bootstrap_provider_env(&t.path) {
            Err(SecretsError::UnsafeFileMode { mode, .. }) => assert_eq!(mode, 0o644),
            other => panic!("expected UnsafeFileMode, got {other:?}"),
        }
    }

    #[test]
    fn test_l2_secrets_ignores_non_env_files_and_loads_nothing() {
        let t = TempDir::new("nonenv", 0o700);
        t.write_env("README.txt", 0o644, "not an env file; mode unchecked\n");
        assert!(bootstrap_provider_env(&t.path).is_ok());
    }

    #[test]
    fn test_l2_secrets_parses_export_prefix_and_quotes() {
        assert!(matches!(
            parse_env_line("export FOO='bar'"),
            EnvLine::Pair("FOO", "bar")
        ));
        assert!(matches!(
            parse_env_line("  K=\"v with spaces\"  "),
            EnvLine::Pair("K", "v with spaces")
        ));
    }

    #[test]
    fn test_l2_secrets_parse_skips_blanks_and_comments() {
        assert!(matches!(parse_env_line(""), EnvLine::Skip));
        assert!(matches!(parse_env_line("   # comment"), EnvLine::Skip));
    }

    #[test]
    fn test_l2_secrets_parse_flags_missing_equals_as_malformed() {
        assert!(matches!(
            parse_env_line("NOEQUALS"),
            EnvLine::Malformed("NOEQUALS")
        ));
    }
}
