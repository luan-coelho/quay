//! Label CRUD + task-label junction queries.
//!
//! Labels are Lanes-style color-coded tags (`Bug` red, `Feature` blue,
//! `Enhancement` green, etc.). Each task can have any number of labels
//! and each label can decorate any number of tasks — classic M2M via the
//! `task_labels` junction table.

#![allow(dead_code)]

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A label definition. `color` is a 7-char `#rrggbb` hex string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    pub id: Uuid,
    pub name: String,
    pub color: String,
}

impl Label {
    pub fn new(name: impl Into<String>, color: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            color: color.into(),
        }
    }
}

/// Lanes ships 13 colour presets — we bundle the same palette so a
/// fresh Quay install has something to pick from without the user
/// having to define colours first.
pub const DEFAULT_PRESETS: &[(&str, &str)] = &[
    ("Bug", "#f87171"),
    ("Feature", "#60a5fa"),
    ("Enhancement", "#10b981"),
    ("Refactor", "#a78bfa"),
    ("Tech Debt", "#f5b54e"),
    ("Docs", "#34d399"),
    ("Tests", "#14b8a6"),
    ("Performance", "#22d3ee"),
    ("Security", "#ef4444"),
    ("UI/UX", "#ec4899"),
    ("Infra", "#6b7280"),
    ("Research", "#fbbf24"),
    ("Question", "#818cf8"),
];

pub struct LabelStore<'a> {
    conn: &'a Connection,
}

impl<'a> LabelStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn insert(&self, label: &Label) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO labels (id, name, color) VALUES (?1, ?2, ?3)",
                params![label.id.to_string(), label.name, label.color],
            )
            .context("insert label")?;
        Ok(())
    }

    pub fn update(&self, label: &Label) -> Result<()> {
        self.conn
            .execute(
                "UPDATE labels SET name = ?2, color = ?3 WHERE id = ?1",
                params![label.id.to_string(), label.name, label.color],
            )
            .context("update label")?;
        Ok(())
    }

    pub fn delete(&self, id: Uuid) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM labels WHERE id = ?1",
                params![id.to_string()],
            )
            .context("delete label")?;
        Ok(())
    }

    pub fn list_all(&self) -> Result<Vec<Label>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, color FROM labels ORDER BY name")?;
        let rows = stmt.query_map([], label_from_row)?;
        rows.map(|r| r.context("label row")).collect()
    }

    /// Seed the preset palette if the `labels` table is currently empty.
    /// No-op otherwise, so running this on every startup is safe.
    pub fn seed_presets_if_empty(&self) -> Result<()> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM labels", [], |row| row.get(0))?;
        if count > 0 {
            return Ok(());
        }
        for (name, color) in DEFAULT_PRESETS {
            self.insert(&Label::new(*name, *color))?;
        }
        Ok(())
    }

    // ── Task-label junction ─────────────────────────────────────────────

    pub fn attach(&self, task_id: Uuid, label_id: Uuid) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO task_labels (task_id, label_id)
                 VALUES (?1, ?2)",
                params![task_id.to_string(), label_id.to_string()],
            )
            .context("attach label to task")?;
        Ok(())
    }

    pub fn detach(&self, task_id: Uuid, label_id: Uuid) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM task_labels WHERE task_id = ?1 AND label_id = ?2",
                params![task_id.to_string(), label_id.to_string()],
            )
            .context("detach label from task")?;
        Ok(())
    }

    pub fn labels_for_task(&self, task_id: Uuid) -> Result<Vec<Label>> {
        let mut stmt = self.conn.prepare(
            "SELECT l.id, l.name, l.color
             FROM labels l
             JOIN task_labels tl ON tl.label_id = l.id
             WHERE tl.task_id = ?1
             ORDER BY l.name",
        )?;
        let rows = stmt.query_map(params![task_id.to_string()], label_from_row)?;
        rows.map(|r| r.context("label row")).collect()
    }

    pub fn tasks_with_label(&self, label_id: Uuid) -> Result<Vec<Uuid>> {
        let mut stmt = self
            .conn
            .prepare("SELECT task_id FROM task_labels WHERE label_id = ?1")?;
        let rows = stmt.query_map(params![label_id.to_string()], |row| {
            let id_str: String = row.get(0)?;
            Ok(id_str)
        })?;
        rows.filter_map(|r| r.ok())
            .map(|s| {
                Uuid::parse_str(&s).map_err(|e| anyhow::anyhow!("bad uuid: {e}"))
            })
            .collect()
    }
}

fn label_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Label> {
    let id_str: String = row.get(0)?;
    let id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(e),
        )
    })?;
    Ok(Label {
        id,
        name: row.get(1)?,
        color: row.get(2)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kanban::{Task, TaskStore};
    use crate::persistence::Database;
    use std::path::PathBuf;

    #[test]
    fn round_trip_label() {
        let db = Database::in_memory().unwrap();
        let store = LabelStore::new(&db.conn);
        let label = Label::new("Bug", "#f87171");
        store.insert(&label).unwrap();

        let all = store.list_all().unwrap();
        assert_eq!(all, vec![label]);
    }

    #[test]
    fn attach_detach_labels_to_task() {
        let db = Database::in_memory().unwrap();
        let task_store = TaskStore::new(&db.conn);
        let label_store = LabelStore::new(&db.conn);

        let task = Task::new("Fix crash", PathBuf::from("/tmp"), "claude");
        task_store.insert(&task).unwrap();

        let bug = Label::new("Bug", "#f87171");
        let high = Label::new("High", "#fbbf24");
        label_store.insert(&bug).unwrap();
        label_store.insert(&high).unwrap();

        label_store.attach(task.id, bug.id).unwrap();
        label_store.attach(task.id, high.id).unwrap();

        let labels = label_store.labels_for_task(task.id).unwrap();
        assert_eq!(labels.len(), 2);
        let names: Vec<&str> = labels.iter().map(|l| l.name.as_str()).collect();
        assert!(names.contains(&"Bug"));
        assert!(names.contains(&"High"));

        label_store.detach(task.id, bug.id).unwrap();
        let labels = label_store.labels_for_task(task.id).unwrap();
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].name, "High");
    }

    #[test]
    fn cascade_on_task_delete_removes_junction() {
        let db = Database::in_memory().unwrap();
        let task_store = TaskStore::new(&db.conn);
        let label_store = LabelStore::new(&db.conn);

        let task = Task::new("Throwaway", PathBuf::from("/tmp"), "claude");
        task_store.insert(&task).unwrap();
        let l = Label::new("Bug", "#f87171");
        label_store.insert(&l).unwrap();
        label_store.attach(task.id, l.id).unwrap();

        task_store.delete(task.id).unwrap();

        // Label itself still exists — only the junction row was cascaded.
        let all_labels = label_store.list_all().unwrap();
        assert_eq!(all_labels.len(), 1);
        let junction = label_store.tasks_with_label(l.id).unwrap();
        assert!(junction.is_empty(), "junction should have cascaded");
    }

    #[test]
    fn seed_presets_is_idempotent() {
        let db = Database::in_memory().unwrap();
        let store = LabelStore::new(&db.conn);
        store.seed_presets_if_empty().unwrap();
        store.seed_presets_if_empty().unwrap();
        store.seed_presets_if_empty().unwrap();
        let all = store.list_all().unwrap();
        assert_eq!(all.len(), DEFAULT_PRESETS.len());
    }
}
