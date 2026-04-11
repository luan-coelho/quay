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
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result};
use uuid::Uuid;

use crate::agents;
use crate::git;
use crate::kanban::{
    AgentKind, SessionRecord, SessionState, SessionStore, StartMode, Task, TaskState, TaskStore,
    WorktreeStrategy, unix_millis_now,
};
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

    /// Make `id` the active task.
    ///
    /// Phase 1 Commit 5: selection no longer auto-spawns a PTY. Spawning a
    /// session is now an explicit user gesture via [`Self::start_session`]
    /// (triggered by the Plan / Implement buttons in the Description tab).
    /// Cards without a live session simply show the empty-state overlay.
    ///
    /// Returns whether the active task actually changed (false if the card
    /// was already active — the caller uses this to skip clobbering any
    /// in-flight Description edits).
    pub fn select_task(&self, id: Uuid) -> Result<bool> {
        let mut active = self.active_task.borrow_mut();
        if *active == Some(id) {
            return Ok(false);
        }
        *active = Some(id);
        Ok(true)
    }

    /// Explicitly start an agent session for a task.
    ///
    /// This is the core entry point wired by Phase 1. It:
    ///   1. Loads the task from SQLite.
    ///   2. Creates a git worktree if the strategy is `Create`, the worktree
    ///      hasn't been materialised yet, and the repo_path is actually a
    ///      git repository (seed/demo tasks with a non-git repo_path skip
    ///      this step and run in the directory as-is).
    ///   3. Resolves the agent via `agents::detect` (Strategy pattern).
    ///      Bare mode short-circuits to `$SHELL` without a provider.
    ///   4. Spawns a `PtySession` in the resolved cwd with the resolved
    ///      argv + env.
    ///   5. Persists the updated task (new state, session_state=Busy,
    ///      start_mode, worktree_path/branch_name) and a SessionRecord row.
    ///   6. Inserts the session into the in-memory registry and makes this
    ///      task active.
    ///
    /// If a session already exists for this task, it is dropped and
    /// replaced — the old child becomes an orphan (Phase 5 Process Manager
    /// will surface and kill these). The byte log file is shared, so old
    /// scrollback remains visible above the new agent's output.
    pub fn start_session(&self, id: Uuid, mode: StartMode) -> Result<()> {
        let store = TaskStore::new(&self.db.conn);
        let mut task = store
            .get(id)?
            .with_context(|| format!("task {id} not found"))?;

        // ── 1. Worktree creation ─────────────────────────────────────────
        let should_create_worktree = matches!(
            task.worktree_strategy,
            WorktreeStrategy::Create
        ) && task.worktree_path.is_none()
            && is_git_repo(&task.repo_path);

        if should_create_worktree {
            let display_id = self.display_id_of(id)?;
            let slug = git::naming::branch_slug(display_id);
            let wt_dir = git::naming::worktree_dir(&task.repo_path, display_id);
            let base_ref = resolve_base_branch(&task.repo_path)?;

            let mgr = git::worktree::WorktreeManager::detect()?;
            mgr.create(&task.repo_path, &slug, &wt_dir, &base_ref)
                .with_context(|| {
                    format!(
                        "failed to create worktree at {} from base {}",
                        wt_dir.display(),
                        base_ref
                    )
                })?;

            tracing::info!(
                task_id = %id,
                branch = %slug,
                worktree = %wt_dir.display(),
                base = %base_ref,
                "worktree created"
            );

            task.worktree_path = Some(wt_dir);
            task.branch_name = Some(slug);
        } else if matches!(task.worktree_strategy, WorktreeStrategy::Create)
            && !is_git_repo(&task.repo_path)
        {
            tracing::warn!(
                task_id = %id,
                repo_path = %task.repo_path.display(),
                "repo_path is not a git repository — running agent in-place without a worktree"
            );
        }

        // cwd priority: linked worktree > task.repo_path. Both are absolute.
        let cwd = task
            .worktree_path
            .clone()
            .unwrap_or_else(|| task.repo_path.clone());

        // ── 2. Resolve argv + env via Strategy pattern ───────────────────
        // Bare shell bypasses the trait entirely; other kinds go through the
        // factory and produce their provider-specific argv.
        let (argv, env, resolved_name) = match task.cli_selection {
            AgentKind::Bare => (
                vec![self.default_shell.clone()],
                Vec::<(String, String)>::new(),
                "bare",
            ),
            kind => {
                let provider = agents::detect(kind)?.expect(
                    "non-bare AgentKind factories always return Some provider",
                );
                // Resume hook is in place for Phase 3: Claude Code supports
                // it, OpenCode does not. Phase 1 never has a captured
                // session id yet, so we always pass None.
                let resume_id: Option<&str> = None;
                let argv = provider.argv(
                    mode,
                    task.instructions.as_deref(),
                    resume_id,
                );
                let env = provider.env();
                (argv, env, provider.name())
            }
        };

        tracing::info!(
            task_id = %id,
            provider = %resolved_name,
            mode = %mode.as_str(),
            cwd = %cwd.display(),
            argv_len = argv.len(),
            "starting agent session"
        );

        // ── 3. Spawn PTY session ─────────────────────────────────────────
        let log_path = self.dirs.task_log_path(&id.to_string());
        let session = PtySession::spawn(
            self.cols,
            self.rows,
            &argv,
            &env,
            &cwd,
            Some(log_path.clone()),
        )?;

        // ── 4. Persist task state transitions ────────────────────────────
        task.start_mode = Some(mode);
        task.session_state = SessionState::Busy;
        task.state = match mode {
            StartMode::Plan => TaskState::Planning,
            StartMode::Implement => TaskState::Implementation,
        };
        // Re-rank within the new column so the task drops to the bottom.
        let peers = store.list_by_state(task.state)?;
        task.position = peers
            .iter()
            .filter(|t| t.id != id)
            .map(|t| t.position)
            .max()
            .unwrap_or(-1)
            + 1;
        task.updated_at = unix_millis_now();
        store.update(&task)?;

        // ── 5. Persist session record ────────────────────────────────────
        let record = SessionRecord::new(
            id,
            log_path,
            self.cols as u32,
            self.rows as u32,
            cwd,
            argv,
        );
        SessionStore::new(&self.db.conn).insert(&record)?;

        // ── 6. Register the live session + mark active ──────────────────
        // An existing session for the same id gets dropped here; its log
        // writer flushes in Drop. The child process becomes an orphan
        // (Phase 5 Process Manager will surface and kill it).
        self.sessions.borrow_mut().insert(id, session);
        *self.active_task.borrow_mut() = Some(id);
        Ok(())
    }

    /// 1-based display number for a task, computed from the created_at sort
    /// order. Stable for the lifetime of a task (insertions always go to the
    /// end, so existing tasks keep their numbers).
    fn display_id_of(&self, id: Uuid) -> Result<i32> {
        let mut tasks = self.list_tasks()?;
        tasks.sort_by_key(|t| t.created_at);
        tasks
            .iter()
            .position(|t| t.id == id)
            .map(|i| (i + 1) as i32)
            .context("task not found while computing display id")
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

    /// Whether a session is currently running for this task (in memory).
    #[allow(dead_code)]
    pub fn has_session(&self, id: Uuid) -> bool {
        self.sessions.borrow().contains_key(&id)
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
            // Seed tasks default to `WorktreeStrategy::Create`. If
            // `default_cwd` is not a git repo (typical for $HOME), the
            // worktree creation in `start_session` will be skipped
            // gracefully and the agent will run in-place. Users who want
            // actual worktree isolation should create tasks pointing at a
            // real repo (multi-project support is a Phase 6 feature).
            store.insert(&task)?;
        }
        Ok(())
    }
}

// ── Free-standing helpers ───────────────────────────────────────────────────

/// Whether a path is the root of a git repository.
///
/// Uses `git2::Repository::open` so it's purely in-process (no subprocess).
/// Returns false for non-git directories, missing paths, and broken repos.
fn is_git_repo(path: &Path) -> bool {
    git2::Repository::open(path).is_ok()
}

/// Pick the base branch to branch a new worktree from.
///
/// Tries `main` first, then `master`. If neither exists, returns a clear
/// error — repos with non-standard default branches (e.g. `develop`,
/// `trunk`) will hit this until Phase 5 adds per-project base branch
/// configuration in Settings.
fn resolve_base_branch(repo: &Path) -> Result<String> {
    let repository = git2::Repository::open(repo)
        .with_context(|| format!("open repo at {} for base branch lookup", repo.display()))?;
    for candidate in ["main", "master"] {
        if repository
            .find_branch(candidate, git2::BranchType::Local)
            .is_ok()
        {
            return Ok(candidate.to_string());
        }
    }
    anyhow::bail!(
        "no main/master branch in {} — configure a base branch in Settings (Phase 5) \
         or create the branch manually",
        repo.display()
    )
}
