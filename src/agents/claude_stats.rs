//! Parse Claude Code session JSONL transcripts for token / cost / runtime
//! metadata so Quay's Description tab can show live usage stats.
//!
//! The session file layout (undocumented but stable across recent
//! Claude Code releases) is at
//! `~/.claude/projects/<cwd-encoded>/<session-uuid>.jsonl`, one JSON
//! message per line. Assistant responses carry a `message.usage`
//! object with Anthropic's standard token fields:
//!
//! ```json
//! {
//!   "type": "assistant",
//!   "message": {
//!     "usage": {
//!       "input_tokens": 123,
//!       "cache_creation_input_tokens": 0,
//!       "cache_read_input_tokens": 0,
//!       "output_tokens": 456
//!     }
//!   },
//!   "timestamp": "2026-04-11T14:12:33.456Z"
//! }
//! ```
//!
//! We don't depend on the exact shape — the parser walks the whole
//! JSON tree looking for any `input_tokens` / `output_tokens` /
//! `cache_*_tokens` keys and sums them. Unknown lines are skipped
//! silently so format drift never crashes the reader.

#![allow(dead_code)]

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct SessionStats {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// Cost in USD cents, computed from Sonnet 4.x pricing.
    pub cost_cents: u64,
    /// Elapsed wall-clock seconds from the file's creation to its
    /// last modification. `None` if either timestamp is unavailable.
    pub runtime_secs: Option<u64>,
    /// Number of non-empty JSONL lines successfully parsed.
    pub message_count: u64,
}

impl SessionStats {
    /// Total tokens = input + cache read + cache creation + output.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_read_tokens
            + self.cache_creation_tokens
    }
}

/// Read a Claude Code session JSONL file and compute its aggregate
/// stats. Missing / empty / unreadable files return a default
/// (all zeros) so callers can treat absence as "no stats yet"
/// without special-casing.
pub fn read_session_stats(path: &Path) -> std::io::Result<SessionStats> {
    let content = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SessionStats::default());
        }
        Err(err) => return Err(err),
    };

    let mut stats = SessionStats::default();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        stats.message_count += 1;
        collect_tokens(&value, &mut stats);
    }

    stats.cost_cents = compute_cost_cents(&stats);

    stats.runtime_secs = fs::metadata(path).ok().and_then(|m| {
        let modified = m.modified().ok()?;
        let created = m.created().ok().unwrap_or_else(|| {
            modified
                .checked_sub(Duration::from_secs(0))
                .unwrap_or(SystemTime::UNIX_EPOCH)
        });
        modified.duration_since(created).ok().map(|d| d.as_secs())
    });

    Ok(stats)
}

/// Recursively walk a JSON value looking for known token keys and add
/// their values to `stats`. Ignores non-integer values.
fn collect_tokens(value: &Value, stats: &mut SessionStats) {
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter() {
                match (key.as_str(), v) {
                    ("input_tokens", Value::Number(n)) => {
                        if let Some(n) = n.as_u64() {
                            stats.input_tokens += n;
                        }
                    }
                    ("output_tokens", Value::Number(n)) => {
                        if let Some(n) = n.as_u64() {
                            stats.output_tokens += n;
                        }
                    }
                    ("cache_read_input_tokens", Value::Number(n)) => {
                        if let Some(n) = n.as_u64() {
                            stats.cache_read_tokens += n;
                        }
                    }
                    ("cache_creation_input_tokens", Value::Number(n)) => {
                        if let Some(n) = n.as_u64() {
                            stats.cache_creation_tokens += n;
                        }
                    }
                    _ => collect_tokens(v, stats),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_tokens(item, stats);
            }
        }
        _ => {}
    }
}

/// Compute session cost in USD cents from Sonnet 4.x pricing:
///   input:          $3.00  / 1M  →  300 cents / M
///   output:         $15.00 / 1M  →  1500 cents / M
///   cache read:     $0.30  / 1M  →  30 cents / M
///   cache creation: $3.75  / 1M  →  375 cents / M
fn compute_cost_cents(stats: &SessionStats) -> u64 {
    let input_cents = stats.input_tokens * 300 / 1_000_000;
    let output_cents = stats.output_tokens * 1500 / 1_000_000;
    let cache_read_cents = stats.cache_read_tokens * 30 / 1_000_000;
    let cache_creation_cents = stats.cache_creation_tokens * 375 / 1_000_000;
    input_cents + output_cents + cache_read_cents + cache_creation_cents
}

/// Resolve the current session JSONL path for a task from its cwd +
/// session id, using the same `.claude/projects/<encoded-cwd>/<id>.jsonl`
/// layout the `claude_resume` capture already uses.
pub fn resolve_session_path(cwd: &Path, session_id: &str) -> Option<std::path::PathBuf> {
    let home = directories::UserDirs::new()?
        .home_dir()
        .join(".claude/projects");
    let encoded = cwd
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "-");
    Some(home.join(encoded).join(format!("{session_id}.jsonl")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn empty_or_missing_returns_zeros() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.jsonl");
        let stats = read_session_stats(&path).unwrap();
        assert_eq!(stats.input_tokens, 0);
        assert_eq!(stats.output_tokens, 0);
        assert_eq!(stats.message_count, 0);
    }

    #[test]
    fn sums_tokens_across_lines() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("sess.jsonl");
        let mut f = File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"usage":{{"input_tokens":100,"output_tokens":50}}}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"usage":{{"input_tokens":200,"output_tokens":80,"cache_read_input_tokens":300}}}}}}"#
        )
        .unwrap();

        let stats = read_session_stats(&path).unwrap();
        assert_eq!(stats.input_tokens, 300);
        assert_eq!(stats.output_tokens, 130);
        assert_eq!(stats.cache_read_tokens, 300);
        assert_eq!(stats.message_count, 2);
    }

    #[test]
    fn cost_for_small_session() {
        let stats = SessionStats {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            ..SessionStats::default()
        };
        // 1M input × 300/M = 300 cents
        // 500k output × 1500/M = 750 cents
        assert_eq!(compute_cost_cents(&stats), 1050);
    }

    #[test]
    fn garbage_lines_are_ignored() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("sess.jsonl");
        let mut f = File::create(&path).unwrap();
        writeln!(f, "this is not json").unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"usage":{{"input_tokens":10,"output_tokens":5}}}}}}"#
        )
        .unwrap();
        writeln!(f, "{{ unterminated json").unwrap();

        let stats = read_session_stats(&path).unwrap();
        assert_eq!(stats.input_tokens, 10);
        assert_eq!(stats.output_tokens, 5);
        assert_eq!(stats.message_count, 1);
    }

    #[test]
    fn total_tokens_sums_all_categories() {
        let stats = SessionStats {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 200,
            cache_creation_tokens: 300,
            ..SessionStats::default()
        };
        assert_eq!(stats.total_tokens(), 650);
    }
}
