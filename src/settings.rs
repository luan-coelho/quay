//! User-facing settings — a thin `Settings` wrapper over the `settings`
//! KV table.
//!
//! Everything is stored as TEXT; complex values serialise to JSON at the
//! boundary. Keys are string constants (see the associated fns below) so
//! typos become compile errors instead of silent misses.

#![allow(dead_code)]

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

pub struct Settings<'a> {
    conn: &'a Connection,
}

// Known keys — use these constants everywhere, never raw strings.
pub const KEY_DEFAULT_BASE_BRANCH: &str = "default_base_branch";
pub const KEY_DEFAULT_AGENT: &str = "default_agent";
pub const KEY_DEFAULT_SHELL: &str = "default_shell";
pub const KEY_THEME: &str = "theme";

impl<'a> Settings<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn get(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("read setting")
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .context("write setting")?;
        Ok(())
    }

    pub fn delete(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM settings WHERE key = ?1", params![key])
            .context("delete setting")?;
        Ok(())
    }

    /// Convenience: read a setting or fall back to a default. Never errors.
    pub fn get_or(&self, key: &str, default: &str) -> String {
        self.get(key)
            .ok()
            .flatten()
            .unwrap_or_else(|| default.to_string())
    }

    /// Seed sensible defaults on first run. Idempotent — existing values
    /// are not overwritten.
    pub fn seed_defaults_if_empty(&self) -> Result<()> {
        let pairs: &[(&str, &str)] = &[
            (KEY_DEFAULT_BASE_BRANCH, "main"),
            (KEY_DEFAULT_AGENT, "claude"),
            (KEY_THEME, "dark"),
        ];
        for (k, v) in pairs {
            if self.get(k)?.is_none() {
                self.set(k, v)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::Database;

    #[test]
    fn round_trip_set_get() {
        let db = Database::in_memory().unwrap();
        let s = Settings::new(&db.conn);
        s.set("foo", "bar").unwrap();
        assert_eq!(s.get("foo").unwrap(), Some("bar".to_string()));
    }

    #[test]
    fn set_overwrites_existing() {
        let db = Database::in_memory().unwrap();
        let s = Settings::new(&db.conn);
        s.set("k", "v1").unwrap();
        s.set("k", "v2").unwrap();
        assert_eq!(s.get("k").unwrap(), Some("v2".to_string()));
    }

    #[test]
    fn get_or_falls_back() {
        let db = Database::in_memory().unwrap();
        let s = Settings::new(&db.conn);
        assert_eq!(s.get_or("missing", "default"), "default");
        s.set("present", "yes").unwrap();
        assert_eq!(s.get_or("present", "default"), "yes");
    }

    #[test]
    fn seed_defaults_is_idempotent() {
        let db = Database::in_memory().unwrap();
        let s = Settings::new(&db.conn);
        s.seed_defaults_if_empty().unwrap();
        s.seed_defaults_if_empty().unwrap();
        assert_eq!(s.get(KEY_DEFAULT_BASE_BRANCH).unwrap().as_deref(), Some("main"));
    }

    #[test]
    fn seed_does_not_overwrite_existing() {
        let db = Database::in_memory().unwrap();
        let s = Settings::new(&db.conn);
        s.set(KEY_DEFAULT_BASE_BRANCH, "develop").unwrap();
        s.seed_defaults_if_empty().unwrap();
        assert_eq!(s.get(KEY_DEFAULT_BASE_BRANCH).unwrap().as_deref(), Some("develop"));
    }
}
