//! SQLite-backed CRUD for `Task` and `SessionRecord`.
//!
//! Both stores borrow an existing `&rusqlite::Connection` rather than owning
//! one — this lets the caller share a single connection across multiple stores
//! and lets in-memory tests reuse the same fixture.
//!
//! Uuids are stored as TEXT (lowercase hyphenated form), paths as TEXT (UTF-8
//! lossy on the rare bad Windows path — acceptable trade-off for now), and
//! the JSON-shaped fields (command argv, env map) as TEXT containing valid
//! JSON.
//!
//! Several CRUD methods are not yet called from the main app (e.g. `delete`,
//! `mark_exited`, `list_for_task`) but are fully tested and kept as the store
//! API that future iterations will plug into. `#[allow(dead_code)]` is applied
//! intentionally to silence noise in the release build until they get wired.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, OptionalExtension, Row, params};
use uuid::Uuid;

use super::model::{
    AgentKind, SessionRecord, SessionState, StartMode, Task, TaskState, WorktreeStrategy,
};

// ─── Tasks ───────────────────────────────────────────────────────────────────

pub struct TaskStore<'a> {
    conn: &'a Connection,
}

impl<'a> TaskStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn insert(&self, task: &Task) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO tasks (
                    id, title, description, instructions, state,
                    repo_path, worktree_path, branch_name, agent_kind,
                    cli_selection, start_mode, worktree_strategy,
                    session_state, process_pid, claude_session_id,
                    project_id,
                    position, created_at, updated_at
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5,
                    ?6, ?7, ?8, ?9,
                    ?10, ?11, ?12,
                    ?13, ?14, ?15,
                    ?16,
                    ?17, ?18, ?19
                )",
                params![
                    task.id.to_string(),
                    task.title,
                    task.description,
                    task.instructions,
                    task.state.as_str(),
                    path_to_string(&task.repo_path),
                    task.worktree_path.as_deref().map(path_to_string),
                    task.branch_name,
                    task.agent_kind,
                    task.cli_selection.as_str(),
                    task.start_mode.map(|m| m.as_str()),
                    task.worktree_strategy.as_str(),
                    task.session_state.as_str(),
                    task.process_pid,
                    task.claude_session_id,
                    task.project_id.map(|id| id.to_string()),
                    task.position,
                    task.created_at,
                    task.updated_at,
                ],
            )
            .context("insert task")?;
        Ok(())
    }

    pub fn update(&self, task: &Task) -> Result<()> {
        let rows = self
            .conn
            .execute(
                "UPDATE tasks SET
                    title = ?2,
                    description = ?3,
                    instructions = ?4,
                    state = ?5,
                    repo_path = ?6,
                    worktree_path = ?7,
                    branch_name = ?8,
                    agent_kind = ?9,
                    cli_selection = ?10,
                    start_mode = ?11,
                    worktree_strategy = ?12,
                    session_state = ?13,
                    process_pid = ?14,
                    claude_session_id = ?15,
                    project_id = ?16,
                    position = ?17,
                    updated_at = ?18
                 WHERE id = ?1",
                params![
                    task.id.to_string(),
                    task.title,
                    task.description,
                    task.instructions,
                    task.state.as_str(),
                    path_to_string(&task.repo_path),
                    task.worktree_path.as_deref().map(path_to_string),
                    task.branch_name,
                    task.agent_kind,
                    task.cli_selection.as_str(),
                    task.start_mode.map(|m| m.as_str()),
                    task.worktree_strategy.as_str(),
                    task.session_state.as_str(),
                    task.process_pid,
                    task.claude_session_id,
                    task.project_id.map(|id| id.to_string()),
                    task.position,
                    task.updated_at,
                ],
            )
            .context("update task")?;
        if rows == 0 {
            return Err(anyhow!("no task with id {}", task.id));
        }
        Ok(())
    }

    pub fn delete(&self, id: Uuid) -> Result<()> {
        self.conn
            .execute("DELETE FROM tasks WHERE id = ?1", params![id.to_string()])
            .context("delete task")?;
        Ok(())
    }

    pub fn get(&self, id: Uuid) -> Result<Option<Task>> {
        self.conn
            .query_row(
                "SELECT id, title, description, instructions, state, repo_path,
                        worktree_path, branch_name, agent_kind, cli_selection,
                        start_mode, worktree_strategy, session_state, process_pid,
                        claude_session_id, project_id, position, created_at, updated_at
                 FROM tasks WHERE id = ?1",
                params![id.to_string()],
                row_to_task,
            )
            .optional()
            .context("get task")
    }

    /// Every task ordered by (state, position, created_at).
    pub fn list_all(&self) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, description, instructions, state, repo_path,
                    worktree_path, branch_name, agent_kind, cli_selection,
                    start_mode, worktree_strategy, session_state, process_pid,
                    claude_session_id, project_id, position, created_at, updated_at
             FROM tasks
             ORDER BY state, position, created_at",
        )?;
        let rows = stmt.query_map([], row_to_task)?;
        rows.map(|r| r.context("list_all row")).collect()
    }

    /// Tasks in a single column, ordered by position.
    pub fn list_by_state(&self, state: TaskState) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, description, instructions, state, repo_path,
                    worktree_path, branch_name, agent_kind, cli_selection,
                    start_mode, worktree_strategy, session_state, process_pid,
                    claude_session_id, project_id, position, created_at, updated_at
             FROM tasks WHERE state = ?1
             ORDER BY position, created_at",
        )?;
        let rows = stmt.query_map(params![state.as_str()], row_to_task)?;
        rows.map(|r| r.context("list_by_state row")).collect()
    }
}

// ─── Sessions ────────────────────────────────────────────────────────────────

pub struct SessionStore<'a> {
    conn: &'a Connection,
}

impl<'a> SessionStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn insert(&self, sess: &SessionRecord) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO sessions (
                    id, task_id, pty_log_path, cols, rows, cwd, command,
                    env_json, exit_status, started_at, ended_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    sess.id.to_string(),
                    sess.task_id.to_string(),
                    path_to_string(&sess.pty_log_path),
                    sess.cols as i64,
                    sess.rows as i64,
                    path_to_string(&sess.cwd),
                    serde_json::to_string(&sess.command).context("serialize command")?,
                    serde_json::to_string(&sess.env).context("serialize env")?,
                    sess.exit_status,
                    sess.started_at,
                    sess.ended_at,
                ],
            )
            .context("insert session")?;
        Ok(())
    }

    /// Mark a session as exited. The byte log file is left untouched on disk.
    pub fn mark_exited(&self, id: Uuid, status: i32, ended_at: i64) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE sessions SET exit_status = ?2, ended_at = ?3 WHERE id = ?1",
                params![id.to_string(), status, ended_at],
            )
            .context("mark session exited")?;
        if n == 0 {
            return Err(anyhow!("no session with id {id}"));
        }
        Ok(())
    }

    pub fn get(&self, id: Uuid) -> Result<Option<SessionRecord>> {
        self.conn
            .query_row(
                "SELECT id, task_id, pty_log_path, cols, rows, cwd, command,
                        env_json, exit_status, started_at, ended_at
                 FROM sessions WHERE id = ?1",
                params![id.to_string()],
                row_to_session,
            )
            .optional()
            .context("get session")
    }

    pub fn list_for_task(&self, task_id: Uuid) -> Result<Vec<SessionRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, pty_log_path, cols, rows, cwd, command,
                    env_json, exit_status, started_at, ended_at
             FROM sessions WHERE task_id = ?1
             ORDER BY started_at",
        )?;
        let rows = stmt.query_map(params![task_id.to_string()], row_to_session)?;
        rows.map(|r| r.context("list_for_task row")).collect()
    }
}

// ─── Row mappers ────────────────────────────────────────────────────────────

fn row_to_task(row: &Row<'_>) -> rusqlite::Result<Task> {
    // Column order must match the SELECT statements above. Index numbers
    // are mirrored in comments so it's easy to cross-check.
    let id_str: String = row.get(0)?;                    // 0  id
    let id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(e),
        )
    })?;
    let title: String = row.get(1)?;                     // 1  title
    let description: Option<String> = row.get(2)?;       // 2  description
    let instructions: Option<String> = row.get(3)?;      // 3  instructions

    let state_str: String = row.get(4)?;                 // 4  state
    let state = TaskState::parse(&state_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown task state {state_str:?}"),
            )),
        )
    })?;

    let repo_path: String = row.get(5)?;                 // 5  repo_path
    let worktree_path: Option<String> = row.get(6)?;     // 6  worktree_path
    let branch_name: Option<String> = row.get(7)?;       // 7  branch_name
    let agent_kind: String = row.get(8)?;                // 8  agent_kind

    let cli_str: String = row.get(9)?;                   // 9  cli_selection
    let cli_selection = AgentKind::parse(&cli_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            9,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown cli_selection {cli_str:?}"),
            )),
        )
    })?;

    let start_mode_str: Option<String> = row.get(10)?;   // 10 start_mode
    let start_mode = match start_mode_str {
        Some(s) => Some(StartMode::parse(&s).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                10,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown start_mode {s:?}"),
                )),
            )
        })?),
        None => None,
    };

    let strat_str: String = row.get(11)?;                // 11 worktree_strategy
    let worktree_strategy = WorktreeStrategy::parse(&strat_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            11,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown worktree_strategy {strat_str:?}"),
            )),
        )
    })?;

    let sess_str: String = row.get(12)?;                 // 12 session_state
    let session_state = SessionState::parse(&sess_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            12,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown session_state {sess_str:?}"),
            )),
        )
    })?;

    let process_pid: Option<i32> = row.get(13)?;         // 13 process_pid
    let claude_session_id: Option<String> = row.get(14)?; // 14 claude_session_id

    // 15 project_id — nullable uuid.
    let project_id_str: Option<String> = row.get(15)?;
    let project_id = match project_id_str {
        Some(s) => Some(Uuid::parse_str(&s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                15,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?),
        None => None,
    };

    let position: i64 = row.get(16)?;                    // 16 position
    let created_at: i64 = row.get(17)?;                  // 17 created_at
    let updated_at: i64 = row.get(18)?;                  // 18 updated_at

    Ok(Task {
        id,
        title,
        description,
        instructions,
        state,
        repo_path: PathBuf::from(repo_path),
        worktree_path: worktree_path.map(PathBuf::from),
        branch_name,
        agent_kind,
        cli_selection,
        start_mode,
        worktree_strategy,
        session_state,
        process_pid,
        claude_session_id,
        project_id,
        position,
        created_at,
        updated_at,
    })
}

fn row_to_session(row: &Row<'_>) -> rusqlite::Result<SessionRecord> {
    let id_str: String = row.get(0)?;
    let id = Uuid::parse_str(&id_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let task_id_str: String = row.get(1)?;
    let task_id = Uuid::parse_str(&task_id_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e)))?;

    let cols: i64 = row.get(3)?;
    let rows_n: i64 = row.get(4)?;
    let cwd: String = row.get(5)?;
    let command_json: String = row.get(6)?;
    let env_json: String = row.get(7)?;

    let command: Vec<String> = serde_json::from_str(&command_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let env: BTreeMap<String, String> = serde_json::from_str(&env_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
    })?;

    let pty_log_path: String = row.get(2)?;

    Ok(SessionRecord {
        id,
        task_id,
        pty_log_path: PathBuf::from(pty_log_path),
        cols: cols as u32,
        rows: rows_n as u32,
        cwd: PathBuf::from(cwd),
        command,
        env,
        exit_status: row.get(8)?,
        started_at: row.get(9)?,
        ended_at: row.get(10)?,
    })
}

fn path_to_string(p: &std::path::Path) -> String {
    p.to_string_lossy().into_owned()
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::Database;

    fn mk_task(title: &str, state: TaskState, position: i64) -> Task {
        let mut t = Task::new(
            title.to_string(),
            PathBuf::from("/tmp/repo"),
            "claude".to_string(),
        );
        t.state = state;
        t.position = position;
        t
    }

    #[test]
    fn task_round_trip() {
        let db = Database::in_memory().unwrap();
        let store = TaskStore::new(&db.conn);

        let mut task = mk_task("Implement dark mode", TaskState::Backlog, 0);
        task.description = Some("Toggle + persistence".to_string());
        task.worktree_path = Some(PathBuf::from("/tmp/wt-dark"));
        task.branch_name = Some("feature/dark-mode".to_string());

        store.insert(&task).unwrap();

        let fetched = store.get(task.id).unwrap().expect("task should exist");
        assert_eq!(fetched, task);
    }

    #[test]
    fn task_update_changes_fields() {
        let db = Database::in_memory().unwrap();
        let store = TaskStore::new(&db.conn);

        let mut task = mk_task("first title", TaskState::Backlog, 0);
        store.insert(&task).unwrap();

        task.title = "second title".to_string();
        task.state = TaskState::Implementation;
        task.position = 3;
        task.updated_at += 1000;
        store.update(&task).unwrap();

        let fetched = store.get(task.id).unwrap().unwrap();
        assert_eq!(fetched.title, "second title");
        assert_eq!(fetched.state, TaskState::Implementation);
        assert_eq!(fetched.position, 3);
    }

    #[test]
    fn list_by_state_orders_by_position() {
        let db = Database::in_memory().unwrap();
        let store = TaskStore::new(&db.conn);

        let a = mk_task("alpha", TaskState::Implementation, 2);
        let b = mk_task("beta", TaskState::Implementation, 0);
        let c = mk_task("gamma", TaskState::Implementation, 1);
        let d = mk_task("delta", TaskState::Backlog, 0);

        for t in [&a, &b, &c, &d] {
            store.insert(t).unwrap();
        }

        let impl_list = store.list_by_state(TaskState::Implementation).unwrap();
        let titles: Vec<&str> = impl_list.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, vec!["beta", "gamma", "alpha"]);
    }

    #[test]
    fn delete_removes_task() {
        let db = Database::in_memory().unwrap();
        let store = TaskStore::new(&db.conn);
        let task = mk_task("to delete", TaskState::Backlog, 0);
        store.insert(&task).unwrap();
        store.delete(task.id).unwrap();
        assert!(store.get(task.id).unwrap().is_none());
    }

    #[test]
    fn session_round_trip() {
        let db = Database::in_memory().unwrap();
        let task_store = TaskStore::new(&db.conn);
        let session_store = SessionStore::new(&db.conn);

        let task = mk_task("with session", TaskState::Backlog, 0);
        task_store.insert(&task).unwrap();

        let mut sess = SessionRecord::new(
            task.id,
            PathBuf::from("/tmp/sess.bin"),
            100,
            30,
            PathBuf::from("/tmp/wt"),
            vec!["bash".into(), "-l".into()],
        );
        sess.env.insert("FOO".into(), "bar".into());

        session_store.insert(&sess).unwrap();
        let fetched = session_store.get(sess.id).unwrap().expect("exists");
        assert_eq!(fetched, sess);
    }

    #[test]
    fn session_mark_exited_sets_status() {
        let db = Database::in_memory().unwrap();
        let task_store = TaskStore::new(&db.conn);
        let session_store = SessionStore::new(&db.conn);

        let task = mk_task("with session", TaskState::Backlog, 0);
        task_store.insert(&task).unwrap();

        let sess = SessionRecord::new(
            task.id,
            PathBuf::from("/tmp/sess.bin"),
            80,
            24,
            PathBuf::from("/tmp/wt"),
            vec!["sh".into()],
        );
        session_store.insert(&sess).unwrap();

        session_store.mark_exited(sess.id, 0, 1_700_000_000).unwrap();
        let after = session_store.get(sess.id).unwrap().unwrap();
        assert_eq!(after.exit_status, Some(0));
        assert_eq!(after.ended_at, Some(1_700_000_000));
    }

    #[test]
    fn session_cascade_on_task_delete() {
        let db = Database::in_memory().unwrap();
        let task_store = TaskStore::new(&db.conn);
        let session_store = SessionStore::new(&db.conn);

        let task = mk_task("parent", TaskState::Backlog, 0);
        task_store.insert(&task).unwrap();
        let sess = SessionRecord::new(
            task.id,
            PathBuf::from("/tmp/sess.bin"),
            80,
            24,
            PathBuf::from("/tmp/wt"),
            vec!["sh".into()],
        );
        session_store.insert(&sess).unwrap();

        task_store.delete(task.id).unwrap();
        // ON DELETE CASCADE on `sessions.task_id` should drop the session row.
        let after = session_store.list_for_task(task.id).unwrap();
        assert!(after.is_empty(), "ON DELETE CASCADE should remove sessions");
    }
}
