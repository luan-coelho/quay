//! Application-wide state for the Quay main window.
//!
//! `AppState` owns:
//!   - the SQLite `Database` (shared across all stores)
//!   - the glyph `GlyphAtlas` and the active `Framebuffer`
//!   - one `PtySession` per task, lazily spawned the first time a task is
//!     selected, kept alive until the app exits or the task is deleted
//!   - the currently selected task id
//!
//! It does **not** touch Slint directly — the main loop reads/writes Slint
//! properties using AppState as a backing store. This keeps the Slint <-> Rust
//! boundary thin and easy to test.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::{Context, Result};
use uuid::Uuid;

use crate::kanban::{SessionStore, Task, TaskState, TaskStore, unix_millis_now};
use crate::persistence::{Database, QuayDirs};
use crate::terminal::{Framebuffer, GlyphAtlas, PtySession};

pub struct AppState {
    pub atlas: Rc<GlyphAtlas>,
    pub framebuffer: RefCell<Framebuffer>,
    pub db: Database,
    pub dirs: QuayDirs,
    pub default_cwd: PathBuf,
    pub default_agent: String,
    pub default_shell: String,

    pub cols: usize,
    pub rows: usize,

    /// Per-task PTY sessions, keyed by task UUID. Lazily populated on first
    /// `select_task` for that task. Kept inside a `RefCell` so callbacks can
    /// mutate without giving up `Rc<AppState>`.
    pub sessions: RefCell<HashMap<Uuid, PtySession>>,
    pub active_task: RefCell<Option<Uuid>>,
}

impl AppState {
    pub fn new(
        atlas: Rc<GlyphAtlas>,
        cols: usize,
        rows: usize,
        dirs: QuayDirs,
        default_cwd: PathBuf,
        default_shell: String,
        default_agent: String,
    ) -> Result<Self> {
        let db = Database::open(&dirs.db_path)?;
        let framebuffer = Framebuffer::new(cols, rows, &atlas);

        Ok(Self {
            atlas,
            framebuffer: RefCell::new(framebuffer),
            db,
            dirs,
            default_cwd,
            default_agent,
            default_shell,
            cols,
            rows,
            sessions: RefCell::new(HashMap::new()),
            active_task: RefCell::new(None),
        })
    }

    /// Read every task from the DB, ordered for kanban display.
    pub fn list_tasks(&self) -> Result<Vec<Task>> {
        TaskStore::new(&self.db.conn).list_all()
    }

    /// Append a brand-new task to the Backlog with an auto-generated title.
    pub fn create_task(&self, title: String) -> Result<Task> {
        let mut task = Task::new(title, self.default_cwd.clone(), self.default_agent.clone());
        // Place at the bottom of the Backlog column.
        let existing = TaskStore::new(&self.db.conn).list_by_state(TaskState::Backlog)?;
        task.position = existing.iter().map(|t| t.position).max().unwrap_or(-1) + 1;
        TaskStore::new(&self.db.conn).insert(&task)?;
        Ok(task)
    }

    /// Move a task one column forward (Backlog → Planning → Implementation → Done).
    /// No-op when already in the rightmost column.
    pub fn move_forward(&self, id: Uuid) -> Result<()> {
        self.move_state(id, |s| match s {
            TaskState::Backlog => Some(TaskState::Planning),
            TaskState::Planning => Some(TaskState::Implementation),
            TaskState::Implementation => Some(TaskState::Done),
            TaskState::Done => None,
        })
    }

    /// Move a task one column backward.
    pub fn move_backward(&self, id: Uuid) -> Result<()> {
        self.move_state(id, |s| match s {
            TaskState::Backlog => None,
            TaskState::Planning => Some(TaskState::Backlog),
            TaskState::Implementation => Some(TaskState::Planning),
            TaskState::Done => Some(TaskState::Implementation),
        })
    }

    fn move_state(
        &self,
        id: Uuid,
        next: impl FnOnce(TaskState) -> Option<TaskState>,
    ) -> Result<()> {
        let store = TaskStore::new(&self.db.conn);
        let mut task = store
            .get(id)?
            .with_context(|| format!("task {id} not found"))?;
        let Some(new_state) = next(task.state) else {
            return Ok(());
        };
        task.state = new_state;
        // Drop to the bottom of the new column.
        let existing = store.list_by_state(new_state)?;
        task.position = existing.iter().map(|t| t.position).max().unwrap_or(-1) + 1;
        task.updated_at = unix_millis_now();
        store.update(&task)?;
        Ok(())
    }

    /// Make `id` the active task. Spawns a fresh PTY session for it the first
    /// time it is selected. Returns whether the active task actually changed
    /// (false if it was already active).
    pub fn select_task(&self, id: Uuid) -> Result<bool> {
        let mut active = self.active_task.borrow_mut();
        if *active == Some(id) {
            return Ok(false);
        }

        // Spawn a session if we have not seen this task yet.
        if !self.sessions.borrow().contains_key(&id) {
            let task = TaskStore::new(&self.db.conn)
                .get(id)?
                .with_context(|| format!("task {id} not found"))?;
            let cwd = task
                .worktree_path
                .clone()
                .unwrap_or_else(|| self.default_cwd.clone());

            // The byte log is keyed by task id and persists across app
            // restarts. Re-opening a task replays the log into the new
            // session's Term so the scrollback survives.
            let log_path = self.dirs.task_log_path(&id.to_string());

            tracing::info!(
                task_id = %id,
                title = %task.title,
                cwd = %cwd.display(),
                log_path = %log_path.display(),
                "spawning PTY session for task"
            );
            let session = PtySession::spawn(
                self.cols,
                self.rows,
                &self.default_shell,
                &cwd,
                Some(log_path.clone()),
            )?;

            // Keep a historical record in SQLite so multiple PTY spawns on
            // the same task can be counted later. The pty_log_path points to
            // the same shared file for now.
            let record = crate::kanban::SessionRecord::new(
                id,
                log_path,
                self.cols as u32,
                self.rows as u32,
                cwd.clone(),
                vec![self.default_shell.clone()],
            );
            SessionStore::new(&self.db.conn).insert(&record)?;

            self.sessions.borrow_mut().insert(id, session);
        }

        *active = Some(id);
        Ok(true)
    }

    /// Drain pending bytes from every live session into its `Term`. Returns
    /// `true` if the **active** session received any new bytes (so the caller
    /// knows whether to re-blit the framebuffer).
    pub fn poll_all_sessions(&self) -> bool {
        let active = *self.active_task.borrow();
        let mut sessions = self.sessions.borrow_mut();
        let mut active_dirty = false;
        for (id, sess) in sessions.iter_mut() {
            let processed = sess.poll();
            if processed && Some(*id) == active {
                active_dirty = true;
            }
        }
        active_dirty
    }

    /// Re-render the active session's terminal into the shared framebuffer.
    /// No-op if there is no active task.
    pub fn blit_active(&self) -> bool {
        let active = match *self.active_task.borrow() {
            Some(id) => id,
            None => return false,
        };
        let mut sessions = self.sessions.borrow_mut();
        let Some(sess) = sessions.get_mut(&active) else {
            return false;
        };
        let mut fb = self.framebuffer.borrow_mut();
        fb.blit_from_term(&sess.term, &self.atlas);
        true
    }

    /// Forward bytes to the active session. No-op if no task is active.
    pub fn write_to_active(&self, bytes: &[u8]) {
        let active = match *self.active_task.borrow() {
            Some(id) => id,
            None => return,
        };
        let mut sessions = self.sessions.borrow_mut();
        if let Some(sess) = sessions.get_mut(&active) {
            sess.write(bytes);
        }
    }

    /// Helper for seed data: if the DB is empty, insert a few demo tasks so
    /// the kanban is not blank on first run.
    pub fn seed_demo_if_empty(&self) -> Result<()> {
        if !self.list_tasks()?.is_empty() {
            return Ok(());
        }
        let store = TaskStore::new(&self.db.conn);
        let titles_and_states = [
            ("Add dark mode", TaskState::Backlog, 0),
            ("Fix server crash on corrupted db.json", TaskState::Backlog, 1),
            ("Implement user authentication", TaskState::Planning, 0),
            ("Set up CI pipeline", TaskState::Done, 0),
            ("Set up git and repo", TaskState::Done, 1),
        ];
        for (title, state, position) in titles_and_states {
            let mut task = Task::new(
                title.to_string(),
                self.default_cwd.clone(),
                self.default_agent.clone(),
            );
            task.state = state;
            task.position = position;
            store.insert(&task)?;
        }
        Ok(())
    }
}
