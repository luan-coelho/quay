//! Project model + ProjectStore.
//!
//! A project is a named (repo_path, base_branch) pair. Tasks carry an
//! optional `project_id` foreign key so the kanban can filter the board
//! by project and new tasks automatically inherit the repo/base-branch
//! from their parent project.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::model::unix_millis_now;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    pub repo_path: PathBuf,
    pub base_branch: String,
    pub created_at: i64,
}

impl Project {
    pub fn new(
        name: impl Into<String>,
        repo_path: impl Into<PathBuf>,
        base_branch: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            repo_path: repo_path.into(),
            base_branch: base_branch.into(),
            created_at: unix_millis_now(),
        }
    }
}

pub struct ProjectStore<'a> {
    conn: &'a Connection,
}

impl<'a> ProjectStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn insert(&self, project: &Project) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO projects (id, name, repo_path, base_branch, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    project.id.to_string(),
                    project.name,
                    project.repo_path.to_string_lossy().into_owned(),
                    project.base_branch,
                    project.created_at,
                ],
            )
            .context("insert project")?;
        Ok(())
    }

    pub fn update(&self, project: &Project) -> Result<()> {
        self.conn
            .execute(
                "UPDATE projects SET
                    name = ?2, repo_path = ?3, base_branch = ?4
                 WHERE id = ?1",
                params![
                    project.id.to_string(),
                    project.name,
                    project.repo_path.to_string_lossy().into_owned(),
                    project.base_branch,
                ],
            )
            .context("update project")?;
        Ok(())
    }

    pub fn delete(&self, id: Uuid) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM projects WHERE id = ?1",
                params![id.to_string()],
            )
            .context("delete project")?;
        Ok(())
    }

    pub fn get(&self, id: Uuid) -> Result<Option<Project>> {
        self.conn
            .query_row(
                "SELECT id, name, repo_path, base_branch, created_at
                 FROM projects WHERE id = ?1",
                params![id.to_string()],
                project_from_row,
            )
            .optional()
            .context("get project")
    }

    pub fn list_all(&self) -> Result<Vec<Project>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, repo_path, base_branch, created_at
             FROM projects ORDER BY name",
        )?;
        let rows = stmt.query_map([], project_from_row)?;
        rows.map(|r| r.context("project row")).collect()
    }

    /// Find a project whose name matches exactly.
    pub fn find_by_name(&self, name: &str) -> Result<Option<Project>> {
        self.conn
            .query_row(
                "SELECT id, name, repo_path, base_branch, created_at
                 FROM projects WHERE name = ?1",
                params![name],
                project_from_row,
            )
            .optional()
            .context("find project by name")
    }

    /// Convenience: return the project ID whose repo_path == `repo_path`,
    /// creating one if missing. Used for automatic project detection.
    pub fn get_or_create_for_repo(
        &self,
        name: &str,
        repo_path: &Path,
        base_branch: &str,
    ) -> Result<Project> {
        if let Some(existing) = self.find_by_name(name)? {
            return Ok(existing);
        }
        let project = Project::new(name, repo_path.to_path_buf(), base_branch.to_string());
        self.insert(&project)?;
        Ok(project)
    }
}

fn project_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    let id_str: String = row.get(0)?;
    let id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(e),
        )
    })?;
    let repo_path_str: String = row.get(2)?;
    Ok(Project {
        id,
        name: row.get(1)?,
        repo_path: PathBuf::from(repo_path_str),
        base_branch: row.get(3)?,
        created_at: row.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::Database;

    #[test]
    fn round_trip_insert_get() {
        let db = Database::in_memory().unwrap();
        let store = ProjectStore::new(&db.conn);
        let p = Project::new("backend", "/home/user/backend", "main");
        store.insert(&p).unwrap();

        let fetched = store.get(p.id).unwrap().unwrap();
        assert_eq!(fetched, p);
    }

    #[test]
    fn list_all_sorts_by_name() {
        let db = Database::in_memory().unwrap();
        let store = ProjectStore::new(&db.conn);
        store
            .insert(&Project::new("frontend", "/a", "main"))
            .unwrap();
        store.insert(&Project::new("backend", "/b", "main")).unwrap();
        store.insert(&Project::new("shared", "/c", "main")).unwrap();
        let all = store.list_all().unwrap();
        assert_eq!(
            all.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            vec!["backend", "frontend", "shared"]
        );
    }

    #[test]
    fn unique_name_is_enforced() {
        let db = Database::in_memory().unwrap();
        let store = ProjectStore::new(&db.conn);
        store.insert(&Project::new("backend", "/a", "main")).unwrap();
        let duplicate = Project::new("backend", "/b", "main");
        assert!(store.insert(&duplicate).is_err());
    }

    #[test]
    fn get_or_create_is_idempotent() {
        let db = Database::in_memory().unwrap();
        let store = ProjectStore::new(&db.conn);
        let p1 = store
            .get_or_create_for_repo("backend", Path::new("/a"), "main")
            .unwrap();
        let p2 = store
            .get_or_create_for_repo("backend", Path::new("/a"), "main")
            .unwrap();
        assert_eq!(p1.id, p2.id);
        assert_eq!(store.list_all().unwrap().len(), 1);
    }

    #[test]
    fn find_by_name_returns_none_for_missing() {
        let db = Database::in_memory().unwrap();
        let store = ProjectStore::new(&db.conn);
        assert!(store.find_by_name("missing").unwrap().is_none());
    }
}
