//! Task dependency graph with acyclic enforcement.
//!
//! A dependency edge is directed: `(task_id, depends_on)` means task
//! `task_id` cannot proceed until task `depends_on` is in state `Done`.
//! The set of edges must stay a DAG — cycles are rejected at insertion
//! time via a recursive CTE walk.

#![allow(dead_code)]

use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use super::model::TaskState;

pub struct DependencyStore<'a> {
    conn: &'a Connection,
}

impl<'a> DependencyStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// Add an edge `task_id` depends on `depends_on`.
    ///
    /// Rejects:
    /// - self edges (`task_id == depends_on`) — the SQL CHECK constraint
    ///   catches this too but we short-circuit with a friendlier error.
    /// - edges that would create a cycle — walked via recursive CTE from
    ///   `depends_on` back through the existing graph; if we can already
    ///   reach `task_id` from `depends_on`, the new edge closes a cycle.
    pub fn add(&self, task_id: Uuid, depends_on: Uuid) -> Result<()> {
        if task_id == depends_on {
            return Err(anyhow!("a task cannot depend on itself"));
        }
        if self.would_create_cycle(task_id, depends_on)? {
            return Err(anyhow!(
                "refusing to add dependency: {task_id} -> {depends_on} would create a cycle"
            ));
        }
        self.conn
            .execute(
                "INSERT OR IGNORE INTO task_dependencies (task_id, depends_on)
                 VALUES (?1, ?2)",
                params![task_id.to_string(), depends_on.to_string()],
            )
            .context("insert task dependency")?;
        Ok(())
    }

    pub fn remove(&self, task_id: Uuid, depends_on: Uuid) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM task_dependencies
                 WHERE task_id = ?1 AND depends_on = ?2",
                params![task_id.to_string(), depends_on.to_string()],
            )
            .context("remove task dependency")?;
        Ok(())
    }

    /// Returns true if `task_id` has at least one prerequisite still not
    /// in state `Done`. Uses a single join query so it's cheap enough for
    /// the refresh loop to call per card.
    pub fn is_blocked(&self, task_id: Uuid) -> Result<bool> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*)
                 FROM task_dependencies td
                 JOIN tasks t ON t.id = td.depends_on
                 WHERE td.task_id = ?1 AND t.state != ?2",
                params![task_id.to_string(), TaskState::Done.as_str()],
                |row| row.get(0),
            )
            .context("count blocking dependencies")?;
        Ok(count > 0)
    }

    /// List all tasks `task_id` depends on (direct edges only, no
    /// transitive closure).
    pub fn dependencies_of(&self, task_id: Uuid) -> Result<Vec<Uuid>> {
        let mut stmt = self.conn.prepare(
            "SELECT depends_on FROM task_dependencies WHERE task_id = ?1",
        )?;
        let rows = stmt.query_map(params![task_id.to_string()], |row| {
            let s: String = row.get(0)?;
            Ok(s)
        })?;
        rows.filter_map(|r| r.ok())
            .map(|s| Uuid::parse_str(&s).map_err(|e| anyhow!("bad uuid: {e}")))
            .collect()
    }

    /// List all tasks that directly depend on `task_id` (reverse edges,
    /// single hop).
    pub fn dependents_of(&self, task_id: Uuid) -> Result<Vec<Uuid>> {
        let mut stmt = self.conn.prepare(
            "SELECT task_id FROM task_dependencies WHERE depends_on = ?1",
        )?;
        let rows = stmt.query_map(params![task_id.to_string()], |row| {
            let s: String = row.get(0)?;
            Ok(s)
        })?;
        rows.filter_map(|r| r.ok())
            .map(|s| Uuid::parse_str(&s).map_err(|e| anyhow!("bad uuid: {e}")))
            .collect()
    }

    /// Would adding `task_id -> depends_on` create a cycle?
    ///
    /// Walks the existing graph transitively starting from `depends_on`,
    /// following the forward direction (depends_on → its prereqs). If
    /// `task_id` appears in that reachable set, the new edge would close
    /// a cycle.
    fn would_create_cycle(&self, task_id: Uuid, depends_on: Uuid) -> Result<bool> {
        let result: Option<i64> = self
            .conn
            .query_row(
                "WITH RECURSIVE reach(id) AS (
                    SELECT depends_on FROM task_dependencies WHERE task_id = ?1
                    UNION
                    SELECT td.depends_on
                    FROM task_dependencies td
                    JOIN reach r ON td.task_id = r.id
                 )
                 SELECT 1 FROM reach WHERE id = ?2 LIMIT 1",
                params![depends_on.to_string(), task_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(result.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kanban::{Task, TaskStore};
    use crate::persistence::Database;
    use std::path::PathBuf;

    fn seed_task(db: &Database, title: &str) -> Uuid {
        let store = TaskStore::new(&db.conn);
        let task = Task::new(title, PathBuf::from("/tmp"), "claude");
        let id = task.id;
        store.insert(&task).unwrap();
        id
    }

    #[test]
    fn add_and_remove_dependency() {
        let db = Database::in_memory().unwrap();
        let a = seed_task(&db, "A");
        let b = seed_task(&db, "B");

        let deps = DependencyStore::new(&db.conn);
        deps.add(b, a).unwrap();
        assert_eq!(deps.dependencies_of(b).unwrap(), vec![a]);
        assert_eq!(deps.dependents_of(a).unwrap(), vec![b]);

        deps.remove(b, a).unwrap();
        assert!(deps.dependencies_of(b).unwrap().is_empty());
    }

    #[test]
    fn rejects_self_dependency() {
        let db = Database::in_memory().unwrap();
        let a = seed_task(&db, "A");
        let deps = DependencyStore::new(&db.conn);
        let err = deps.add(a, a).expect_err("self-edge should fail");
        assert!(err.to_string().contains("itself"));
    }

    #[test]
    fn rejects_direct_cycle() {
        let db = Database::in_memory().unwrap();
        let a = seed_task(&db, "A");
        let b = seed_task(&db, "B");
        let deps = DependencyStore::new(&db.conn);
        deps.add(a, b).unwrap();
        // Adding b -> a would create a cycle.
        let err = deps.add(b, a).expect_err("direct cycle should fail");
        assert!(err.to_string().contains("cycle"));
    }

    #[test]
    fn rejects_indirect_cycle() {
        let db = Database::in_memory().unwrap();
        let a = seed_task(&db, "A");
        let b = seed_task(&db, "B");
        let c = seed_task(&db, "C");
        let deps = DependencyStore::new(&db.conn);
        // a -> b -> c, then c -> a should fail.
        deps.add(a, b).unwrap();
        deps.add(b, c).unwrap();
        let err = deps.add(c, a).expect_err("indirect cycle should fail");
        assert!(err.to_string().contains("cycle"));
    }

    #[test]
    fn is_blocked_requires_pending_prereq() {
        let db = Database::in_memory().unwrap();
        let store = TaskStore::new(&db.conn);

        let a = seed_task(&db, "A");
        let b = seed_task(&db, "B");
        let deps = DependencyStore::new(&db.conn);
        deps.add(b, a).unwrap();

        // A is in Backlog (not Done) → B is blocked.
        assert!(deps.is_blocked(b).unwrap());

        // Mark A as Done → B should unblock.
        let mut task_a = store.get(a).unwrap().unwrap();
        task_a.state = TaskState::Done;
        store.update(&task_a).unwrap();
        assert!(!deps.is_blocked(b).unwrap());
    }

    #[test]
    fn task_with_no_deps_is_never_blocked() {
        let db = Database::in_memory().unwrap();
        let a = seed_task(&db, "A");
        let deps = DependencyStore::new(&db.conn);
        assert!(!deps.is_blocked(a).unwrap());
    }
}
