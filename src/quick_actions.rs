//! Quick Actions — user-defined shortcut commands.
//!
//! Two flavours:
//! - **Claude-type**: injects a prompt into the active agent session (by
//!   writing the text to the PTY, followed by a newline). Useful for
//!   "ask the agent to commit the changes" or "summarise the diff".
//! - **Shell-type**: runs a shell command in the task's worktree cwd.
//!   Useful for "cargo test" or "git status".
//!
//! Users bind them to Cmd+Alt+1..9 by position (see `main.rs` keyboard
//! dispatcher).

#![allow(dead_code)]

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::kanban::unix_millis_now;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuickActionKind {
    Claude,
    Shell,
}

impl QuickActionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Shell => "shell",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Self::Claude),
            "shell" => Some(Self::Shell),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuickActionCategory {
    General,
    Worktree,
}

impl QuickActionCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Worktree => "worktree",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "general" => Some(Self::General),
            "worktree" => Some(Self::Worktree),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickAction {
    pub id: Uuid,
    pub name: String,
    pub kind: QuickActionKind,
    /// The prompt (for Claude kind) or shell command (for Shell kind).
    pub body: String,
    pub category: QuickActionCategory,
    pub position: i64,
    pub created_at: i64,
}

impl QuickAction {
    pub fn new(
        name: impl Into<String>,
        kind: QuickActionKind,
        body: impl Into<String>,
        category: QuickActionCategory,
        position: i64,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            kind,
            body: body.into(),
            category,
            position,
            created_at: unix_millis_now(),
        }
    }
}

/// Lanes' default quick action set. Ships on first run so the user has
/// something to bind to Cmd+Alt+N without having to configure anything.
pub const DEFAULTS: &[(&str, QuickActionKind, &str, QuickActionCategory)] = &[
    ("Commit", QuickActionKind::Claude, "Commit the current changes with a descriptive message.", QuickActionCategory::Worktree),
    ("Review Changes", QuickActionKind::Claude, "Review the current diff for correctness, style, and obvious bugs.", QuickActionCategory::Worktree),
    ("Add Tests", QuickActionKind::Claude, "Add tests for the changes I just made.", QuickActionCategory::Worktree),
    ("Fix Lint", QuickActionKind::Claude, "Fix any lint errors or warnings in the current changes.", QuickActionCategory::Worktree),
    ("Refactor", QuickActionKind::Claude, "Refactor the code you just wrote for clarity and simplicity without changing behaviour.", QuickActionCategory::General),
    ("Run Tests", QuickActionKind::Shell, "cargo test", QuickActionCategory::Worktree),
    ("Git Status", QuickActionKind::Shell, "git status", QuickActionCategory::General),
];

pub struct QuickActionStore<'a> {
    conn: &'a Connection,
}

impl<'a> QuickActionStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn insert(&self, action: &QuickAction) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO quick_actions
                 (id, name, kind, body, category, position, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    action.id.to_string(),
                    action.name,
                    action.kind.as_str(),
                    action.body,
                    action.category.as_str(),
                    action.position,
                    action.created_at,
                ],
            )
            .context("insert quick action")?;
        Ok(())
    }

    pub fn update(&self, action: &QuickAction) -> Result<()> {
        self.conn
            .execute(
                "UPDATE quick_actions SET
                    name = ?2, kind = ?3, body = ?4, category = ?5, position = ?6
                 WHERE id = ?1",
                params![
                    action.id.to_string(),
                    action.name,
                    action.kind.as_str(),
                    action.body,
                    action.category.as_str(),
                    action.position,
                ],
            )
            .context("update quick action")?;
        Ok(())
    }

    pub fn delete(&self, id: Uuid) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM quick_actions WHERE id = ?1",
                params![id.to_string()],
            )
            .context("delete quick action")?;
        Ok(())
    }

    pub fn list_all(&self) -> Result<Vec<QuickAction>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, kind, body, category, position, created_at
             FROM quick_actions
             ORDER BY position, created_at",
        )?;
        let rows = stmt.query_map([], row_to_action)?;
        rows.map(|r| r.context("quick action row")).collect()
    }

    /// Seed the DEFAULTS set if the table is empty. Idempotent.
    pub fn seed_defaults_if_empty(&self) -> Result<()> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM quick_actions", [], |row| row.get(0))?;
        if count > 0 {
            return Ok(());
        }
        for (i, (name, kind, body, category)) in DEFAULTS.iter().enumerate() {
            let action = QuickAction::new(*name, *kind, *body, *category, i as i64);
            self.insert(&action)?;
        }
        Ok(())
    }
}

fn row_to_action(row: &rusqlite::Row<'_>) -> rusqlite::Result<QuickAction> {
    let id_str: String = row.get(0)?;
    let id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(e),
        )
    })?;
    let kind_str: String = row.get(2)?;
    let kind = QuickActionKind::parse(&kind_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown quick action kind {kind_str:?}"),
            )),
        )
    })?;
    let category_str: String = row.get(4)?;
    let category = QuickActionCategory::parse(&category_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown quick action category {category_str:?}"),
            )),
        )
    })?;
    Ok(QuickAction {
        id,
        name: row.get(1)?,
        kind,
        body: row.get(3)?,
        category,
        position: row.get(5)?,
        created_at: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::Database;

    #[test]
    fn seed_defaults_is_idempotent() {
        let db = Database::in_memory().unwrap();
        let store = QuickActionStore::new(&db.conn);
        store.seed_defaults_if_empty().unwrap();
        store.seed_defaults_if_empty().unwrap();
        let all = store.list_all().unwrap();
        assert_eq!(all.len(), DEFAULTS.len());
    }

    #[test]
    fn list_order_follows_position() {
        let db = Database::in_memory().unwrap();
        let store = QuickActionStore::new(&db.conn);
        store.seed_defaults_if_empty().unwrap();
        let list = store.list_all().unwrap();
        for (i, action) in list.iter().enumerate() {
            assert_eq!(action.position, i as i64);
        }
    }

    #[test]
    fn round_trip_insert_update_delete() {
        let db = Database::in_memory().unwrap();
        let store = QuickActionStore::new(&db.conn);
        let mut a = QuickAction::new(
            "Test",
            QuickActionKind::Shell,
            "echo hello",
            QuickActionCategory::General,
            0,
        );
        store.insert(&a).unwrap();
        a.name = "Renamed".into();
        a.body = "echo world".into();
        store.update(&a).unwrap();

        let list = store.list_all().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Renamed");
        assert_eq!(list[0].body, "echo world");

        store.delete(a.id).unwrap();
        assert!(store.list_all().unwrap().is_empty());
    }
}
