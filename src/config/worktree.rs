//! The design-§5 `[worktree]` config block — operator-tunable worktree
//! retention (audit C1).
//!
//! Design §5 specifies a `[worktree]` config table:
//!
//! ```toml
//! [worktree]
//! auto_clean_canceled_after = "7 days"
//! ```
//!
//! v1.0 implements exactly one key — `auto_clean_canceled_after`, the
//! retention window after which a *terminal* failed/canceled spec's worktrees
//! are reclaimed from disk (the sweeper's auto-clean pass; audit C1 widened
//! the window to FAILED specs by operator decision — see that pass's doc).
//! The design's other `[worktree]` keys (`root`,
//! `delete_task_branches_after_merge`, `delete_integration_after_merge`) are
//! NOT implemented; `deny_unknown_fields` makes setting one a **loud parse
//! error** rather than a silently-ignored operator intent (SO S6).
//!
//! The file lives at `~/.boi/v2/config.toml` (`cli::paths::config_file`) —
//! the same root as the phase/pipeline declarations. An ABSENT file is the
//! documented default state (every key defaulted); a PRESENT-but-malformed
//! file is a loud [`WorktreeConfigError`] that fails daemon boot — never a
//! silent fallback to defaults.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

/// The design-§5 default retention window — `"7 days"`.
pub const DEFAULT_AUTO_CLEAN_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Parsed `[worktree]` config (design §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeConfig {
    /// How long a failed/canceled spec's worktrees are retained on disk
    /// before the sweeper's auto-clean pass reclaims them. Design key:
    /// `auto_clean_canceled_after` (applied to failed specs too — audit C1
    /// operator decision).
    pub auto_clean_canceled_after: Duration,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        WorktreeConfig {
            auto_clean_canceled_after: DEFAULT_AUTO_CLEAN_AFTER,
        }
    }
}

/// Loading `~/.boi/v2/config.toml` failed — boot must stop, loudly.
#[derive(Debug, thiserror::Error)]
pub enum WorktreeConfigError {
    /// The file exists but could not be read.
    #[error("cannot read {path}: {detail}")]
    Io {
        /// The config file path.
        path: String,
        /// The I/O error.
        detail: String,
    },
    /// The file is not valid TOML, or `[worktree]` carries an unknown key.
    #[error("malformed {path}: {detail}")]
    Parse {
        /// The config file path.
        path: String,
        /// The TOML parser's message.
        detail: String,
    },
    /// `auto_clean_canceled_after` is not a parseable duration.
    #[error("invalid [worktree].auto_clean_canceled_after `{got}` in {path}: {detail}")]
    BadDuration {
        /// The config file path.
        path: String,
        /// The unparseable value.
        got: String,
        /// The duration parser's message.
        detail: String,
    },
}

/// The raw config-file shape. Unknown TOP-LEVEL tables are tolerated (future
/// config sections must not break old binaries); unknown keys INSIDE
/// `[worktree]` are denied (a typo'd retention key silently ignored is a
/// silent failure — SO S6).
#[derive(Debug, Deserialize)]
struct RawConfigFile {
    /// The `[worktree]` table, if present.
    worktree: Option<RawWorktreeTable>,
}

/// The raw `[worktree]` table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorktreeTable {
    /// `auto_clean_canceled_after = "7 days"` — a `humantime` duration string.
    auto_clean_canceled_after: Option<String>,
}

/// Load the `[worktree]` config from `path` (`~/.boi/v2/config.toml`).
///
/// - Absent file → [`WorktreeConfig::default`] (the documented default state).
/// - Present but unreadable / malformed / bad duration → a loud
///   [`WorktreeConfigError`] — boot fails rather than running with a config
///   the operator didn't write (SO S6).
pub fn load_worktree_config(path: &Path) -> Result<WorktreeConfig, WorktreeConfigError> {
    if !path.exists() {
        return Ok(WorktreeConfig::default());
    }
    let display = path.display().to_string();
    let raw = std::fs::read_to_string(path).map_err(|e| WorktreeConfigError::Io {
        path: display.clone(),
        detail: e.to_string(),
    })?;
    let parsed: RawConfigFile = toml::from_str(&raw).map_err(|e| WorktreeConfigError::Parse {
        path: display.clone(),
        detail: e.to_string(),
    })?;
    let mut config = WorktreeConfig::default();
    if let Some(table) = parsed.worktree {
        if let Some(dur_str) = table.auto_clean_canceled_after {
            let dur = humantime::parse_duration(&dur_str).map_err(|e| {
                WorktreeConfigError::BadDuration {
                    path: display,
                    got: dur_str.clone(),
                    detail: e.to_string(),
                }
            })?;
            config.auto_clean_canceled_after = dur;
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A throwaway config file removed on drop.
    struct TempFile {
        path: PathBuf,
    }

    impl TempFile {
        fn new(tag: &str, contents: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "boi-worktree-config-{}-{tag}.toml",
                std::process::id()
            ));
            std::fs::write(&path, contents).expect("write temp config");
            TempFile { path }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            drop(std::fs::remove_file(&self.path));
        }
    }

    /// An absent config file is the documented default state — `"7 days"`.
    #[test]
    fn test_l1_worktree_config_defaults_when_file_absent() {
        let missing = std::env::temp_dir().join("boi-no-such-config-file.toml");
        let config = load_worktree_config(&missing).expect("absent file is defaults");
        assert_eq!(config.auto_clean_canceled_after, DEFAULT_AUTO_CLEAN_AFTER);
        assert_eq!(
            DEFAULT_AUTO_CLEAN_AFTER,
            Duration::from_secs(7 * 24 * 60 * 60),
            "the design-§5 default is 7 days",
        );
    }

    /// A present `[worktree]` table overrides the retention window.
    #[test]
    fn test_l1_worktree_config_reads_auto_clean_window() {
        let file = TempFile::new("ok", "[worktree]\nauto_clean_canceled_after = \"3 days\"\n");
        let config = load_worktree_config(&file.path).expect("valid config parses");
        assert_eq!(
            config.auto_clean_canceled_after,
            Duration::from_secs(3 * 24 * 60 * 60),
        );
    }

    /// A malformed duration is a LOUD error — never a silent default (SO S6).
    #[test]
    fn test_l1_worktree_config_rejects_malformed_duration_loudly() {
        let file = TempFile::new(
            "bad-dur",
            "[worktree]\nauto_clean_canceled_after = \"not-a-duration\"\n",
        );
        let err = load_worktree_config(&file.path).expect_err("garbage duration must error");
        assert!(
            matches!(err, WorktreeConfigError::BadDuration { .. }),
            "expected BadDuration, got {err:?}",
        );
    }

    /// An unknown key inside `[worktree]` (e.g. a typo'd retention key) is a
    /// LOUD parse error, never a silently-ignored operator intent (SO S6).
    #[test]
    fn test_l1_worktree_config_rejects_unknown_worktree_key_loudly() {
        let file = TempFile::new(
            "bad-key",
            "[worktree]\nauto_clean_cancelled_after = \"7 days\"\n",
        );
        let err = load_worktree_config(&file.path).expect_err("unknown key must error");
        assert!(
            matches!(err, WorktreeConfigError::Parse { .. }),
            "expected Parse, got {err:?}",
        );
    }

    /// Unknown TOP-LEVEL tables are tolerated — future config sections must
    /// not break this loader.
    #[test]
    fn test_l1_worktree_config_tolerates_unknown_top_level_tables() {
        let file = TempFile::new(
            "other-table",
            "[future_section]\nkey = 1\n\n[worktree]\nauto_clean_canceled_after = \"1 day\"\n",
        );
        let config = load_worktree_config(&file.path).expect("unknown top-level table tolerated");
        assert_eq!(
            config.auto_clean_canceled_after,
            Duration::from_secs(24 * 60 * 60),
        );
    }
}
