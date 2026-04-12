//! Capture Claude Code session ids from the agent's own state directory.
//!
//! Claude Code persists every session as a JSONL file at
//! `~/.claude/projects/<cwd-encoded>/<session-id>.jsonl`, where
//! `<cwd-encoded>` is the working directory with path separators replaced
//! by hyphens (`/`, `\` → `-`). Each JSONL line is a message in the
//! session transcript; the filename itself is the session id Claude's
//! `--resume <id>` flag expects.
//!
//! Quay never parses the JSONL contents — the *filename* is the session
//! id, and we only care about finding the newest file whose mtime is
//! after our spawn time so we know it belongs to the session we just
//! launched (not a stale one from earlier).
//!
//! Called from `AppState::start_session` after the PTY child is
//! spawned, with a short retry loop because Claude Code takes a moment
//! to create the file on startup.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;

/// Attempt to locate the session id of the Claude Code session that was
/// spawned in `cwd` at or after `spawn_time`.
///
/// Behaviour:
/// - Polls `~/.claude/projects/<cwd-encoded>/` with a short delay, giving
///   Claude Code up to `timeout` to create the file.
/// - On each poll, finds every `.jsonl` whose mtime is >= `spawn_time`
///   and returns the filename stem of the newest one.
/// - Returns `Ok(None)` if nothing appeared within the timeout — callers
///   should treat that as "resume not yet captured, try again next
///   spawn" rather than an error.
/// - Returns `Err` only for actual I/O failures reading the projects
///   directory (permission denied, etc.).
pub fn capture_session_id(
    cwd: &Path,
    spawn_time: SystemTime,
    timeout: Duration,
) -> Result<Option<String>> {
    let projects_dir = match claude_projects_dir() {
        Some(dir) => dir,
        None => return Ok(None),
    };

    let encoded_cwd_dir = projects_dir.join(encode_cwd(cwd));

    let deadline = Instant::now() + timeout;
    let poll_interval = Duration::from_millis(200);

    loop {
        if let Some(id) = find_latest_session(&encoded_cwd_dir, spawn_time)? {
            return Ok(Some(id));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(poll_interval);
    }
}

/// `~/.claude/projects/` — the root of Claude Code's per-project state.
/// Returns `None` if the home directory cannot be determined.
fn claude_projects_dir() -> Option<PathBuf> {
    directories::UserDirs::new().map(|d| d.home_dir().join(".claude/projects"))
}

/// Translate a cwd into the encoded directory name Claude Code uses.
///
/// The convention is: take the absolute path, replace every `/` (or `\` on
/// Windows) with `-`. Leading separators become leading hyphens. For
/// example, `/home/luan/repos/quay` → `-home-luan-repos-quay`.
fn encode_cwd(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    s.replace(['/', '\\'], "-")
}

/// Scan a `.claude/projects/<cwd>/` directory for `.jsonl` files whose
/// modification time is >= `spawn_time`. Returns the filename stem of
/// the newest such file, or `None` if the directory is missing, unread-
/// able, or contains no qualifying file yet.
fn find_latest_session(dir: &Path, spawn_time: SystemTime) -> Result<Option<String>> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        // Directory does not exist yet (Claude hasn't written anything)
        // or we cannot read it — treat both as "no session yet".
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => return Ok(None),
        Err(err) => return Err(err.into()),
    };

    let mut best: Option<(SystemTime, String)> = None;

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = match metadata.modified() {
            Ok(t) => t,
            Err(_) => continue,
        };

        if mtime < spawn_time {
            continue;
        }

        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };

        match best {
            Some((best_mtime, _)) if best_mtime >= mtime => {}
            _ => best = Some((mtime, stem)),
        }
    }

    Ok(best.map(|(_, id)| id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn encode_cwd_replaces_separators() {
        let p = Path::new("/home/luan/repos/quay");
        let encoded = encode_cwd(p);
        assert_eq!(encoded, "-home-luan-repos-quay");
    }

    #[test]
    fn find_latest_session_returns_none_for_missing_dir() {
        let tmp = tempdir().unwrap();
        let nope = tmp.path().join("does-not-exist");
        let result = find_latest_session(&nope, SystemTime::UNIX_EPOCH).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn find_latest_session_picks_newest_fresh_file() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path();

        // Create an old file (before the spawn time — must be ignored).
        let old = dir.join("old-session.jsonl");
        File::create(&old).unwrap().write_all(b"{}").unwrap();

        let spawn_time = SystemTime::now();
        std::thread::sleep(Duration::from_millis(20));

        // Two new files; the second should win because of mtime.
        let first = dir.join("first-session.jsonl");
        File::create(&first).unwrap().write_all(b"{}").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let second = dir.join("second-session.jsonl");
        File::create(&second).unwrap().write_all(b"{}").unwrap();

        let result = find_latest_session(dir, spawn_time).unwrap();
        assert_eq!(result.as_deref(), Some("second-session"));
    }

    #[test]
    fn find_latest_session_ignores_non_jsonl() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path();

        // Not a JSONL — must be ignored even if mtime is fresh.
        let junk = dir.join("notes.txt");
        File::create(&junk).unwrap().write_all(b"hello").unwrap();

        let spawn_time = SystemTime::UNIX_EPOCH;
        let result = find_latest_session(dir, spawn_time).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn find_latest_session_skips_files_older_than_spawn() {
        // A directory full of stale .jsonl files (all older than the
        // spawn time) must return None — we never want to "resume"
        // a session that pre-dates the spawn we're tracking.
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        let stale_a = dir.join("stale-a.jsonl");
        let stale_b = dir.join("stale-b.jsonl");
        File::create(&stale_a).unwrap().write_all(b"{}").unwrap();
        File::create(&stale_b).unwrap().write_all(b"{}").unwrap();

        std::thread::sleep(Duration::from_millis(20));
        let spawn_time = SystemTime::now();

        let result = find_latest_session(dir, spawn_time).unwrap();
        assert!(
            result.is_none(),
            "stale files must not be returned as a fresh session id"
        );
    }

    #[test]
    fn encode_cwd_handles_spaces_and_unicode() {
        // Project paths with spaces or non-ASCII chars must round-trip
        // safely — Claude Code accepts them in its directory layout.
        let p = Path::new("/home/usuário/My Projects/quay");
        let encoded = encode_cwd(p);
        assert_eq!(encoded, "-home-usuário-My Projects-quay");
    }

    #[test]
    fn encode_cwd_handles_root_only() {
        // Root path is "/" — encoding leaves a single hyphen.
        let p = Path::new("/");
        let encoded = encode_cwd(p);
        assert_eq!(encoded, "-");
    }
}
