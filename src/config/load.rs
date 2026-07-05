//! Filesystem loaders for the phase + pipeline declarations.
//!
//! `config/phase.rs` and `config/pipeline.rs` parse a single TOML *string*;
//! they do not touch the filesystem. The daemon (`boi daemon`) and the
//! dispatch path (`boi dispatch`) both need to load the whole `standard`
//! pipeline and every `~/.boi/v2/phases/<name>.toml` from disk — this module
//! is that loader, kept in the `config` layer (it produces only
//! `config`-layer types).

use std::collections::HashMap;
use std::path::Path;

use crate::config::phase::{PhaseDef, parse_phase};
use crate::config::pipeline::{PipelineDef, parse_pipeline};
use crate::config::spec::ConfigError;

/// A phase / pipeline declaration could not be loaded from disk.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// A declaration directory or file could not be read.
    #[error("cannot read {path}: {source}")]
    Io {
        /// The path that could not be read.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// A declaration file's TOML failed to parse / validate.
    #[error("config error in {path}: {source}")]
    Config {
        /// The file that failed.
        path: String,
        /// The underlying config error.
        source: ConfigError,
    },
}

/// Load every `*.toml` phase declaration from `phases_dir`.
///
/// Returns a `phase name → PhaseDef` map (keyed on the parsed `PhaseDef.name`,
/// which by convention matches the file stem). A directory that does not exist
/// or a file that fails to parse is a loud [`LoadError`] — never a silently
/// skipped phase (a missing phase would later surface as a confusing
/// `UnknownPhase` mid-run).
pub fn load_phases(phases_dir: &Path) -> Result<HashMap<String, PhaseDef>, LoadError> {
    let entries = std::fs::read_dir(phases_dir).map_err(|source| LoadError::Io {
        path: phases_dir.display().to_string(),
        source,
    })?;
    let mut phases = HashMap::new();
    for entry in entries {
        let entry = entry.map_err(|source| LoadError::Io {
            path: phases_dir.display().to_string(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|source| LoadError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let phase = parse_phase(&text).map_err(|source| LoadError::Config {
            path: path.display().to_string(),
            source,
        })?;
        phases.insert(phase.name.clone(), phase);
    }
    Ok(phases)
}

/// Load one named pipeline from `pipelines_dir/<name>.toml`.
pub fn load_pipeline(pipelines_dir: &Path, name: &str) -> Result<PipelineDef, LoadError> {
    let path = pipelines_dir.join(format!("{name}.toml"));
    let text = std::fs::read_to_string(&path).map_err(|source| LoadError::Io {
        path: path.display().to_string(),
        source,
    })?;
    parse_pipeline(&text).map_err(|source| LoadError::Config {
        path: path.display().to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A throwaway directory (std-only — BOI does not depend on `tempfile`).
    struct TempDir {
        path: std::path::PathBuf,
    }
    impl TempDir {
        fn new() -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("boi-config-load-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.path));
        }
    }

    /// `load_phases` reads every `*.toml` and skips non-TOML files.
    #[test]
    fn test_l2_load_phases_reads_toml_only() {
        let dir = TempDir::new();
        std::fs::write(
            dir.path.join("execute.toml"),
            "name = \"execute\"\nlevel = \"task\"\nkind = \"worker\"\n\
             prompt_template = \"execute.md\"\n\
             [runtime]\nprovider = \"claude_code\"\nmodel = \"m\"\n",
        )
        .unwrap();
        // A non-TOML file is ignored, not an error.
        std::fs::write(dir.path.join("README.md"), "ignore me").unwrap();

        let phases = load_phases(&dir.path).unwrap();
        assert_eq!(phases.len(), 1, "only the .toml is loaded");
        assert!(phases.contains_key("execute"), "keyed on the phase name");
    }

    /// A missing phases directory is a loud [`LoadError::Io`].
    #[test]
    fn test_l2_load_phases_missing_dir_is_loud() {
        let err = load_phases(Path::new("/nonexistent/boi/phases")).unwrap_err();
        assert!(matches!(err, LoadError::Io { .. }), "got {err:?}");
    }

    /// A malformed phase TOML is a loud [`LoadError::Config`].
    #[test]
    fn test_l2_load_phases_malformed_toml_is_loud() {
        let dir = TempDir::new();
        std::fs::write(dir.path.join("bad.toml"), "this is not = valid toml [[[").unwrap();
        let err = load_phases(&dir.path).unwrap_err();
        assert!(matches!(err, LoadError::Config { .. }), "got {err:?}");
    }
}
