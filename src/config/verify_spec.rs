//! verify_spec auto-detection — when a spec's `[contract].verifications` is
//! empty or absent, BOI infers the test / static-analysis / syntax commands
//! from the workspace toolchain (§3.4).
//!
//! Detection is **first-match-wins** over a fixed priority order: a workspace
//! that happens to carry both a `Cargo.toml` and a `package.json` (e.g. a Rust
//! project with a JS frontend) detects as Rust. Any author-supplied
//! verification opts the spec out of auto-detection entirely — that decision
//! is made by the caller (Phase 6's `validate` step), not here.

use std::path::Path;

/// The three commands inferred for a workspace's toolchain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedToolchain {
    /// The test command (e.g. `cargo test`).
    pub tests: String,
    /// The static-analysis / lint command (e.g. `cargo clippy -- -D warnings`).
    pub static_: String,
    /// The syntax / typecheck command (e.g. `cargo check`).
    pub syntax: String,
}

/// One marker-file → toolchain mapping, in priority order. The first marker
/// found in the workspace root wins.
struct Marker {
    /// The file whose presence identifies the toolchain.
    file: &'static str,
    /// Test command.
    tests: &'static str,
    /// Static-analysis command.
    static_: &'static str,
    /// Syntax command.
    syntax: &'static str,
}

/// The §3.4 marker table, in detection priority order. `Cargo.toml` precedes
/// `package.json`, which precedes `pyproject.toml`, etc.
const MARKERS: &[Marker] = &[
    Marker {
        file: "Cargo.toml",
        tests: "cargo test",
        static_: "cargo clippy -- -D warnings",
        syntax: "cargo check",
    },
    Marker {
        file: "package.json",
        tests: "npm test",
        static_: "eslint . --max-warnings 0",
        syntax: "tsc --noEmit",
    },
    Marker {
        file: "pyproject.toml",
        tests: "pytest",
        static_: "ruff check",
        syntax: "mypy .",
    },
    Marker {
        file: "go.mod",
        tests: "go test ./...",
        static_: "go vet ./...",
        syntax: "go build ./...",
    },
    Marker {
        file: "Gemfile",
        tests: "bundle exec rspec",
        static_: "rubocop",
        syntax: "ruby -c",
    },
    Marker {
        file: "mix.exs",
        tests: "mix test",
        static_: "mix credo",
        syntax: "mix compile --warnings-as-errors",
    },
];

/// Detect a workspace's toolchain from its marker files (§3.4).
///
/// First-match-wins over the `MARKERS` table. Returns `None` if no marker is
/// present — the caller must then fail loudly (a spec with no verifications
/// and no detectable toolchain cannot define success).
pub fn detect_toolchain(workspace: &Path) -> Option<DetectedToolchain> {
    for marker in MARKERS {
        if workspace.join(marker.file).is_file() {
            return Some(DetectedToolchain {
                tests: marker.tests.to_owned(),
                static_: marker.static_.to_owned(),
                syntax: marker.syntax.to_owned(),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A throwaway directory under the system temp dir, removed on drop. Avoids
    /// pulling in the `tempfile` crate for what `std` already provides.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            // Uniqueness: pid + a monotonically-bumped counter. Tests in one
            // binary share a process, so the counter disambiguates.
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("boi-verify-spec-{}-{tag}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }

        /// Drop a marker file into the temp dir.
        fn touch(&self, file: &str) {
            std::fs::write(self.path.join(file), b"# marker fixture\n").expect("write marker file");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            // Best-effort cleanup — a failed temp-dir removal must not panic a
            // test's unwind. `drop()` is the explicit must_use consumer that
            // satisfies the workspace `let_underscore_must_use` deny (a bare
            // `let _ =` would trip it).
            drop(std::fs::remove_dir_all(&self.path));
        }
    }

    #[test]
    fn no_marker_returns_none() {
        let dir = TempDir::new("empty");
        assert_eq!(detect_toolchain(&dir.path), None);
    }

    #[test]
    fn cargo_toml_detects_rust() {
        let dir = TempDir::new("rust");
        dir.touch("Cargo.toml");
        let tc = detect_toolchain(&dir.path).expect("rust toolchain");
        assert_eq!(tc.tests, "cargo test");
        assert_eq!(tc.static_, "cargo clippy -- -D warnings");
        assert_eq!(tc.syntax, "cargo check");
    }

    #[test]
    fn package_json_detects_node() {
        let dir = TempDir::new("node");
        dir.touch("package.json");
        let tc = detect_toolchain(&dir.path).expect("node toolchain");
        assert_eq!(tc.tests, "npm test");
        assert_eq!(tc.static_, "eslint . --max-warnings 0");
        assert_eq!(tc.syntax, "tsc --noEmit");
    }

    #[test]
    fn pyproject_toml_detects_python() {
        let dir = TempDir::new("python");
        dir.touch("pyproject.toml");
        let tc = detect_toolchain(&dir.path).expect("python toolchain");
        assert_eq!(tc.tests, "pytest");
        assert_eq!(tc.static_, "ruff check");
        assert_eq!(tc.syntax, "mypy .");
    }

    #[test]
    fn go_mod_detects_go() {
        let dir = TempDir::new("go");
        dir.touch("go.mod");
        let tc = detect_toolchain(&dir.path).expect("go toolchain");
        assert_eq!(tc.tests, "go test ./...");
        assert_eq!(tc.static_, "go vet ./...");
        assert_eq!(tc.syntax, "go build ./...");
    }

    #[test]
    fn gemfile_detects_ruby() {
        let dir = TempDir::new("ruby");
        dir.touch("Gemfile");
        let tc = detect_toolchain(&dir.path).expect("ruby toolchain");
        assert_eq!(tc.tests, "bundle exec rspec");
        assert_eq!(tc.static_, "rubocop");
        assert_eq!(tc.syntax, "ruby -c");
    }

    #[test]
    fn mix_exs_detects_elixir() {
        let dir = TempDir::new("elixir");
        dir.touch("mix.exs");
        let tc = detect_toolchain(&dir.path).expect("elixir toolchain");
        assert_eq!(tc.tests, "mix test");
        assert_eq!(tc.static_, "mix credo");
        assert_eq!(tc.syntax, "mix compile --warnings-as-errors");
    }

    #[test]
    fn mixed_workspace_returns_highest_priority_match() {
        // Cargo.toml + package.json present — Rust wins (first in MARKERS).
        let dir = TempDir::new("mixed");
        dir.touch("package.json");
        dir.touch("Cargo.toml");
        let tc = detect_toolchain(&dir.path).expect("toolchain");
        assert_eq!(tc.tests, "cargo test");
    }

    #[test]
    fn detection_walks_priority_order_when_higher_markers_absent() {
        // No Cargo.toml / package.json — pyproject.toml is the first present.
        let dir = TempDir::new("py-over-go");
        dir.touch("go.mod");
        dir.touch("pyproject.toml");
        let tc = detect_toolchain(&dir.path).expect("toolchain");
        assert_eq!(tc.tests, "pytest");
    }

    #[test]
    fn marker_must_be_a_file_not_a_directory() {
        // A *directory* named Cargo.toml must not count as a marker.
        let dir = TempDir::new("dir-marker");
        std::fs::create_dir(dir.path.join("Cargo.toml")).expect("mkdir");
        assert_eq!(detect_toolchain(&dir.path), None);
    }
}
