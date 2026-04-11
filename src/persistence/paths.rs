//! OS-aware on-disk locations for Quay's persistent state.
//!
//! Resolves to:
//!   Linux:    $XDG_DATA_HOME/quay              or ~/.local/share/quay
//!   macOS:    ~/Library/Application Support/sh.quay.quay
//!   Windows:  %APPDATA%\quay\quay\data
//!
//! Inside the data dir we lay out:
//!   quay.db            — SQLite metadata (tasks, sessions, etc.)
//!   sessions/<uuid>.bin — append-only PTY byte logs (one file per session)
//!
//! The directories are created on first call so the rest of the codebase can
//! assume the paths exist.

use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;

#[derive(Debug, Clone)]
pub struct QuayDirs {
    /// Root data directory for the app on this OS.
    pub data_dir: PathBuf,
    /// Absolute path to the SQLite metadata database.
    pub db_path: PathBuf,
    /// Directory holding per-session PTY byte logs.
    pub sessions_dir: PathBuf,
}

impl QuayDirs {
    /// Discover the canonical Quay directories for the current user. Creates
    /// them if missing.
    pub fn discover() -> Result<Self> {
        let project = ProjectDirs::from("sh", "quay", "quay")
            .context("could not determine OS-specific data directory")?;
        let data_dir = project.data_dir().to_path_buf();
        Self::with_data_dir(data_dir)
    }

    /// Build a `QuayDirs` rooted at an explicit path. Useful for tests that
    /// want to operate inside a tempdir.
    pub fn with_data_dir(data_dir: PathBuf) -> Result<Self> {
        let sessions_dir = data_dir.join("sessions");
        let db_path = data_dir.join("quay.db");
        std::fs::create_dir_all(&sessions_dir)
            .with_context(|| format!("create sessions dir at {}", sessions_dir.display()))?;
        Ok(Self {
            data_dir,
            db_path,
            sessions_dir,
        })
    }

    /// Path where a freshly-spawned session should append its PTY byte log.
    /// Superseded by `task_log_path` for the current "one log per task" model,
    /// but kept for the eventual multi-session-per-task mode.
    #[allow(dead_code)]
    pub fn session_log_path(&self, session_id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{session_id}.bin"))
    }

    /// Append-only PTY log keyed by task id. Reusing the same path across
    /// app runs means the replay on the next task open reconstructs the
    /// visible scrollback from before the restart.
    pub fn task_log_path(&self, task_id: &str) -> PathBuf {
        self.sessions_dir.join(format!("task-{task_id}.bin"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn with_data_dir_creates_sessions_subdir() {
        let tmp = tempdir().unwrap();
        let dirs = QuayDirs::with_data_dir(tmp.path().to_path_buf()).unwrap();
        assert!(dirs.sessions_dir.exists());
        assert_eq!(dirs.db_path, tmp.path().join("quay.db"));
        assert_eq!(
            dirs.session_log_path("abc"),
            tmp.path().join("sessions").join("abc.bin")
        );
    }

    /// `discover` may legitimately fail in sandboxed CI environments, so this
    /// test only checks the happy path on developer machines.
    #[test]
    #[ignore = "depends on user data directory existing — run manually"]
    fn discover_returns_paths() {
        let dirs = QuayDirs::discover().unwrap();
        assert!(dirs.db_path.is_absolute());
    }
}
