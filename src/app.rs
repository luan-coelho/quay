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
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result};
use uuid::Uuid;

use crate::agents;
use crate::git;
use crate::kanban::{
    AgentKind, DependencyStore, LabelStore, ProjectStore, SessionRecord, SessionState,
    SessionStore, StartMode, Task, TaskState, TaskStore, WorktreeStrategy, unix_millis_now,
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

    pub cols: std::cell::Cell<usize>,
    pub rows: std::cell::Cell<usize>,

    /// Per-task PTY sessions, keyed by task UUID. Lazily populated on first
    /// `select_task` for that task. Kept inside a `RefCell` so callbacks can
    /// mutate without giving up `Rc<AppState>`.
    pub sessions: RefCell<HashMap<Uuid, PtySession>>,
    pub active_task: RefCell<Option<Uuid>>,
    /// Polish 16 — ordered list of tasks the user has "pinned" into the
    /// right pane as open tabs. The tab strip in `ui/main.slint` renders
    /// one chip per entry here. Exactly one entry matches `active_task`
    /// at any time; closing the active tab falls back to the nearest
    /// remaining entry. Polish 34: persisted as a JSON array in the
    /// `settings` KV table under `KEY_OPEN_TABS`, so the user's pinned
    /// tabs survive restarts. Stale ids (tasks deleted between runs)
    /// are filtered on load against the current task list.
    pub open_tabs: RefCell<Vec<Uuid>>,
    /// Polish 17 — absolute paths of directories the user has expanded
    /// in the Files tab tree. Passed to `file_tree::build_tree` on
    /// each refresh so the flattened output contains the right
    /// children. Keyed by absolute path so the set survives switching
    /// across tasks that share overlapping subtrees.
    pub expanded_dirs: RefCell<HashSet<PathBuf>>,
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

        // Polish 34: hydrate persisted open-tabs from settings. Stale
        // ids whose task no longer exists are filtered against the
        // current task list, so a task deleted between runs doesn't
        // leave a phantom chip on next launch.
        let open_tabs = {
            let settings = crate::settings::Settings::new(&db.conn);
            let raw = settings
                .get(crate::settings::KEY_OPEN_TABS)
                .ok()
                .flatten()
                .unwrap_or_default();
            let parsed: Vec<Uuid> = serde_json::from_str::<Vec<String>>(&raw)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|s| Uuid::parse_str(&s).ok())
                .collect();
            // Filter against existing task ids to drop stale entries.
            let existing_ids: std::collections::HashSet<Uuid> = TaskStore::new(&db.conn)
                .list_all()
                .map(|tasks| tasks.into_iter().map(|t| t.id).collect())
                .unwrap_or_default();
            parsed.into_iter().filter(|id| existing_ids.contains(id)).collect()
        };

        Ok(Self {
            atlas,
            framebuffer: RefCell::new(framebuffer),
            db,
            dirs,
            default_cwd,
            default_agent,
            default_shell,
            cols: std::cell::Cell::new(cols),
            rows: std::cell::Cell::new(rows),
            sessions: RefCell::new(HashMap::new()),
            active_task: RefCell::new(None),
            open_tabs: RefCell::new(open_tabs),
            expanded_dirs: RefCell::new(HashSet::new()),
        })
    }

    // ─── Store factory methods ───────────────────────────────────────────────
    //
    // Each method returns a fresh store that borrows `&self.db.conn`. The
    // returned `Store<'_>` lifetime is elided to `&self`, so the borrow
    // checker keeps the store from outliving the AppState. These exist purely
    // to eliminate the `crate::kanban::TaskStore::new(&state.db.conn)` boilerplate
    // that used to litter every callback in `main.rs` — there is no SQL or
    // domain logic here on purpose. Stores remain the single source of SQL.

    pub fn task_store(&self) -> TaskStore<'_> {
        TaskStore::new(&self.db.conn)
    }

    pub fn label_store(&self) -> LabelStore<'_> {
        LabelStore::new(&self.db.conn)
    }

    pub fn dependency_store(&self) -> DependencyStore<'_> {
        DependencyStore::new(&self.db.conn)
    }

    pub fn project_store(&self) -> ProjectStore<'_> {
        ProjectStore::new(&self.db.conn)
    }

    pub fn session_store(&self) -> SessionStore<'_> {
        SessionStore::new(&self.db.conn)
    }

    /// Polish 34: write the current `open_tabs` to the settings KV
    /// table as a JSON array. Called whenever the list mutates
    /// (`pin_open_tab`, `close_open_tab`, and the kanban refresh
    /// purges stale entries). Failures are logged but not propagated
    /// — losing tab persistence shouldn't crash the UI.
    pub fn persist_open_tabs(&self) {
        let tabs = self.open_tabs.borrow();
        let payload: Vec<String> = tabs.iter().map(|u| u.to_string()).collect();
        drop(tabs);
        let json = match serde_json::to_string(&payload) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(%err, "open_tabs serialise failed");
                return;
            }
        };
        let settings = crate::settings::Settings::new(&self.db.conn);
        if let Err(err) = settings.set(crate::settings::KEY_OPEN_TABS, &json) {
            tracing::warn!(%err, "open_tabs persist failed");
        }
    }

    /// Polish 16 — ensure `id` is present in `open_tabs` (appended if
    /// absent) and return true if the tab list actually changed. The
    /// caller uses this to decide whether to rebuild the Slint tab
    /// model without an unnecessary refresh. Polish 34: persists on
    /// every successful pin.
    pub fn pin_open_tab(&self, id: Uuid) -> bool {
        let mut tabs = self.open_tabs.borrow_mut();
        let changed = pin_tab_in_place(&mut tabs, id);
        drop(tabs);
        if changed {
            self.persist_open_tabs();
        }
        changed
    }

    /// Polish 16 — remove `id` from `open_tabs`. Returns `Some(next_id)`
    /// if the caller should switch the active task to a different tab
    /// (only the case when the closed tab was the active one and
    /// other tabs remain), or `None` otherwise. The fallback picks the
    /// tab at the same index, or the previous one if we closed the
    /// tail — so closing a tab always leaves the focus on a neighbour
    /// rather than jumping to an unrelated tab. Polish 34: persists.
    pub fn close_open_tab(&self, id: Uuid) -> Option<Uuid> {
        let mut tabs = self.open_tabs.borrow_mut();
        let was_active = *self.active_task.borrow() == Some(id);
        let next = close_tab_in_place(&mut tabs, id, was_active);
        if was_active && next.is_none() {
            *self.active_task.borrow_mut() = None;
        }
        drop(tabs);
        self.persist_open_tabs();
        next
    }

    /// Polish 41 — keep only `keep_id` in the open-tabs strip,
    /// dropping every other entry. Returns `Some(keep_id)` if the
    /// active task should switch (which happens when the previously
    /// active task wasn't `keep_id`), or `None` if nothing needs to
    /// move. No-op when `keep_id` isn't in the list.
    pub fn close_other_open_tabs(&self, keep_id: Uuid) -> Option<Uuid> {
        let mut tabs = self.open_tabs.borrow_mut();
        let changed = close_others_in_place(&mut tabs, keep_id);
        drop(tabs);
        if !changed {
            return None;
        }
        let active = *self.active_task.borrow();
        let switch_to = if active != Some(keep_id) {
            *self.active_task.borrow_mut() = Some(keep_id);
            Some(keep_id)
        } else {
            None
        };
        self.persist_open_tabs();
        switch_to
    }

    /// Polish 41 — close every open tab. Clears `active_task` so the
    /// right pane reverts to the empty state.
    pub fn close_all_open_tabs(&self) {
        self.open_tabs.borrow_mut().clear();
        *self.active_task.borrow_mut() = None;
        self.persist_open_tabs();
    }

    /// Polish 41 — close every tab strictly after `anchor_id` in the
    /// strip order, keeping `anchor_id` and everything to its left.
    /// Returns `Some(anchor_id)` if the active task should switch
    /// (when the previously active task got closed), or `None`.
    pub fn close_tabs_right_of(&self, anchor_id: Uuid) -> Option<Uuid> {
        let mut tabs = self.open_tabs.borrow_mut();
        let changed = close_right_of_in_place(&mut tabs, anchor_id);
        if !changed {
            return None;
        }
        let kept: std::collections::HashSet<Uuid> = tabs.iter().copied().collect();
        drop(tabs);
        let active = *self.active_task.borrow();
        let switch_to = match active {
            Some(active_id) if !kept.contains(&active_id) => {
                *self.active_task.borrow_mut() = Some(anchor_id);
                Some(anchor_id)
            }
            _ => None,
        };
        self.persist_open_tabs();
        switch_to
    }

    /// Read every task from the DB, ordered for kanban display.
    pub fn list_tasks(&self) -> Result<Vec<Task>> {
        self.task_store().list_all()
    }

    /// Append a brand-new task to the Backlog. When `project_id` is set,
    /// the task inherits the project's `repo_path` so worktree creation
    /// and agent sessions point at the right repository.
    pub fn create_task(&self, title: String, project_id: Option<Uuid>) -> Result<Task> {
        let (cwd, proj_id) = if let Some(pid) = project_id {
            let project_store = self.project_store();
            if let Some(project) = project_store.get(pid)? {
                (project.repo_path, Some(pid))
            } else {
                (self.default_cwd.clone(), None)
            }
        } else {
            (self.default_cwd.clone(), None)
        };

        let mut task = Task::new(title, cwd, self.default_agent.clone());
        task.project_id = proj_id;
        // Place at the bottom of the Backlog column.
        let store = self.task_store();
        let existing = store.list_by_state(TaskState::Backlog)?;
        task.position = existing.iter().map(|t| t.position).max().unwrap_or(-1) + 1;
        store.insert(&task)?;
        Ok(task)
    }

    /// Apply a mutation to the currently active task and persist.
    ///
    /// Returns `Ok(true)` if a task was found and updated, `Ok(false)` if
    /// there's no active task selected. Errors propagate so callers can
    /// surface them via toast.
    ///
    /// `updated_at` is bumped automatically — callers only need to mutate
    /// the field they care about.
    pub fn update_active_task<F>(&self, f: F) -> Result<bool>
    where
        F: FnOnce(&mut Task),
    {
        let Some(id) = *self.active_task.borrow() else {
            return Ok(false);
        };
        let store = self.task_store();
        let Some(mut task) = store.get(id)? else {
            return Ok(false);
        };
        f(&mut task);
        task.updated_at = unix_millis_now();
        store.update(&task)?;
        Ok(true)
    }

    /// Move a task one column forward along the primary workflow
    /// (Backlog → Planning → Implementation → Review → Done).
    /// No-op when already in Done or when the task sits in Misc (which is
    /// outside the linear flow).
    ///
    /// Phase 4: refuses to advance beyond Planning if the task has any
    /// unresolved dependencies — the user must either complete the
    /// prerequisites first or manually remove the dependency edges.
    pub fn move_forward(&self, id: Uuid) -> Result<()> {
        let deps = self.dependency_store();
        if deps.is_blocked(id)? {
            let current = self.task_store().get(id)?.map(|t| t.state);
            if matches!(current, Some(TaskState::Planning) | Some(TaskState::Backlog)) {
                // Allow Backlog → Planning (user can still plan while
                // blocked), but stop there.
                if matches!(current, Some(TaskState::Planning)) {
                    anyhow::bail!("task is blocked by an unresolved dependency");
                }
            }
        }
        self.move_state(id, |s| s.next())
    }

    /// Move a task one column backward along the primary workflow.
    pub fn move_backward(&self, id: Uuid) -> Result<()> {
        self.move_state(id, |s| s.prev())
    }

    fn move_state(
        &self,
        id: Uuid,
        next: impl FnOnce(TaskState) -> Option<TaskState>,
    ) -> Result<()> {
        let store = self.task_store();
        let mut task = store
            .get(id)?
            .with_context(|| format!("task {id} not found"))?;
        let Some(new_state) = next(task.state) else {
            return Ok(());
        };

        // When moving a task into Done, consult the worktree status. A
        // clean worktree auto-removes on the transition (matches Lanes).
        // A dirty worktree is kept and logged — a confirmation dialog is
        // a Phase 2.5 UX polish. Non-git repo_paths or missing worktrees
        // are no-ops.
        if matches!(new_state, TaskState::Done)
            && let Some(worktree_path) = task.worktree_path.clone()
            && let Err(err) = self.cleanup_worktree_on_done(&task.repo_path, &worktree_path)
        {
            tracing::warn!(
                task_id = %id,
                worktree = %worktree_path.display(),
                %err,
                "worktree cleanup on Done failed; leaving worktree in place"
            );
        }

        task.state = new_state;
        // Drop to the bottom of the new column.
        let existing = store.list_by_state(new_state)?;
        task.position = existing.iter().map(|t| t.position).max().unwrap_or(-1) + 1;
        task.updated_at = unix_millis_now();
        store.update(&task)?;
        Ok(())
    }

    /// If the worktree is clean, remove it via `git worktree remove`. If
    /// dirty, log a warning and leave it in place (Phase 2.5 will add a
    /// confirmation dialog).
    fn cleanup_worktree_on_done(
        &self,
        repo: &Path,
        worktree_path: &Path,
    ) -> Result<()> {
        // If the directory no longer exists (e.g. the user manually rm'd
        // it), there is nothing to clean.
        if !worktree_path.exists() {
            return Ok(());
        }

        let status = git::status::read_status(worktree_path)
            .with_context(|| format!("read status of {}", worktree_path.display()))?;

        if !status.clean {
            tracing::warn!(
                worktree = %worktree_path.display(),
                modified = status.modified_count,
                untracked = status.untracked_count,
                staged = status.staged_count,
                "worktree dirty on Done — keeping it; user can clean it up manually"
            );
            return Ok(());
        }

        let mgr = git::worktree::WorktreeManager::detect()?;
        mgr.remove(repo, worktree_path)?;
        tracing::info!(
            worktree = %worktree_path.display(),
            "clean worktree auto-removed on Done transition"
        );
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
        let store = self.task_store();
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
            // Prefer the project's configured base_branch when available;
            // fall back to the auto-detected main/master otherwise.
            let base_ref = if let Some(pid) = task.project_id {
                self.project_store()
                    .get(pid)?
                    .map(|p| p.base_branch)
                    .unwrap_or_else(|| resolve_base_branch(&task.repo_path).unwrap_or_else(|_| "main".to_string()))
            } else {
                resolve_base_branch(&task.repo_path)?
            };

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
                // Providers that support resume get the captured session id
                // on subsequent launches so the agent's conversation memory
                // survives restarts. Providers that don't always get None.
                let resume_id: Option<&str> = if provider.supports_resume() {
                    task.claude_session_id.as_deref()
                } else {
                    None
                };
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
        let spawn_time = std::time::SystemTime::now();
        let session = PtySession::spawn(
            self.cols.get(),
            self.rows.get(),
            &argv,
            &env,
            &cwd,
            Some(log_path.clone()),
        )?;

        // Phase 3: if this is a Claude Code session, kick off a background
        // thread to capture the session id from Claude's `.claude/projects/`
        // state directory. The capture updates the task row directly via
        // a fresh short-lived DB connection when it finds the id; on the
        // next start_session we pass it as `--resume <id>` so the agent's
        // conversation memory survives restarts.
        if matches!(task.cli_selection, AgentKind::Claude) {
            let db_path = self.dirs.db_path.clone();
            let capture_cwd = cwd.clone();
            let task_id = id;
            std::thread::Builder::new()
                .name("quay-claude-resume-capture".into())
                .spawn(move || {
                    let timeout = std::time::Duration::from_secs(30);
                    match agents::claude_resume::capture_session_id(
                        &capture_cwd,
                        spawn_time,
                        timeout,
                    ) {
                        Ok(Some(session_id)) => {
                            if let Err(err) =
                                persist_claude_session_id(&db_path, task_id, &session_id)
                            {
                                tracing::warn!(
                                    task_id = %task_id,
                                    %err,
                                    "failed to persist captured claude_session_id"
                                );
                            } else {
                                tracing::info!(
                                    task_id = %task_id,
                                    session_id = %session_id,
                                    "claude session id captured"
                                );
                            }
                        }
                        Ok(None) => {
                            tracing::debug!(
                                task_id = %task_id,
                                "no claude session file appeared within timeout"
                            );
                        }
                        Err(err) => {
                            tracing::warn!(%err, "claude_resume capture failed");
                        }
                    }
                })
                .ok(); // If we can't spawn the thread, resume support is
                       // silently skipped for this session. Not fatal.
        }

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
            self.cols.get() as u32,
            self.rows.get() as u32,
            cwd,
            argv,
        );
        self.session_store().insert(&record)?;

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

    /// Resize all live PTY sessions and rebuild the framebuffer at
    /// the new dimensions. Called when the window/right-pane resizes.
    pub fn resize_all_sessions(&self, new_cols: usize, new_rows: usize) {
        if new_cols == 0 || new_rows == 0 {
            return;
        }
        if new_cols == self.cols.get() && new_rows == self.rows.get() {
            return;
        }
        self.cols.set(new_cols);
        self.rows.set(new_rows);
        *self.framebuffer.borrow_mut() = Framebuffer::new(new_cols, new_rows, &self.atlas);
        let mut sessions = self.sessions.borrow_mut();
        for sess in sessions.values_mut() {
            sess.resize(new_cols, new_rows);
        }
        tracing::debug!(cols = new_cols, rows = new_rows, "resized all sessions");
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

    /// Check every live session for exit and update the DB accordingly.
    /// Returns a vec of `(task_id, title)` pairs for sessions that just
    /// exited — the caller can use these for toast notifications and
    /// kanban refresh.
    ///
    /// Auto-transitions: when a session in the Implementation column
    /// exits cleanly (the child process terminated), the task is
    /// automatically promoted to Review. Planning tasks stay in place
    /// so the user can review the agent's plan before proceeding.
    pub fn check_exited_sessions(&self) -> Vec<(Uuid, String)> {
        let mut exited = Vec::new();
        let mut sessions = self.sessions.borrow_mut();
        let mut to_remove = Vec::new();

        for (id, sess) in sessions.iter_mut() {
            if sess.is_exited() {
                to_remove.push(*id);
            }
        }

        drop(sessions);

        for id in to_remove {
            let store = self.task_store();
            if let Ok(Some(mut task)) = store.get(id) {
                let title = task.title.clone();
                task.session_state = SessionState::Exited;
                task.updated_at = unix_millis_now();

                // Auto-transition: Implementation → Review on exit.
                if task.state == TaskState::Implementation {
                    task.state = TaskState::Review;
                    // Re-rank to bottom of the Review column.
                    let review_tasks = store.list_by_state(TaskState::Review).unwrap_or_default();
                    task.position = review_tasks.iter().map(|t| t.position).max().unwrap_or(-1) + 1;
                    tracing::info!(task_id = %id, "auto-transition: Implementation → Review");
                }

                if let Err(err) = store.update(&task) {
                    tracing::warn!(%err, task_id = %id, "failed to update session_state to exited");
                }
                exited.push((id, title));
            }
            // Don't remove the session from the HashMap — keep it alive
            // so the terminal scrollback remains visible. The session
            // will be cleaned up when the task is deleted or a new
            // session is started for the same task.
        }

        exited
    }

    /// Inspect live sessions for output patterns indicating the agent
    /// is awaiting user input. Returns list of (task_id, new_state) pairs
    /// where the state actually changed.
    pub fn detect_session_states(&self) -> Vec<(Uuid, SessionState)> {
        let mut changed = Vec::new();
        let sessions = self.sessions.borrow();
        let store = self.task_store();

        for (id, sess) in sessions.iter() {
            let task = match store.get(*id) {
                Ok(Some(t)) => t,
                _ => continue,
            };
            // Only detect for sessions that are currently "busy".
            if task.session_state != SessionState::Busy {
                continue;
            }
            if let Some(new_state) =
                crate::terminal::detect::detect_session_state(&sess.term, task.cli_selection)
                && new_state != task.session_state
            {
                changed.push((*id, new_state));
            }
        }

        drop(sessions);

        // Persist state changes.
        for (id, new_state) in &changed {
            if let Ok(Some(mut task)) = store.get(*id) {
                task.session_state = *new_state;
                task.updated_at = unix_millis_now();
                if let Err(err) = store.update(&task) {
                    tracing::warn!(%err, task_id = %id, "detect_session_states: update failed");
                }
            }
        }

        changed
    }

    /// Gracefully stop the running session for a task. Sends SIGTERM to
    /// the child process and updates `session_state` to Stopped in the DB.
    pub fn stop_session(&self, task_id: Uuid) -> Result<()> {
        let sessions = self.sessions.borrow();
        let sess = sessions.get(&task_id).context("no session for task")?;
        if let Some(pid) = sess.child_pid() {
            crate::process::terminate(pid)?;
        }
        drop(sessions);

        let store = self.task_store();
        if let Ok(Some(mut task)) = store.get(task_id) {
            task.session_state = SessionState::Stopped;
            task.updated_at = unix_millis_now();
            store.update(&task)?;
        }
        Ok(())
    }

    /// Collect OS PIDs of every live session — feeds the Process Manager
    /// classifier so it can tag our spawned children as "Tracked" instead
    /// of lumping them into "Orphan".
    pub fn tracked_pids(&self) -> std::collections::HashSet<u32> {
        self.sessions
            .borrow()
            .values()
            .filter_map(|s| s.child_pid())
            .collect()
    }

    /// Execute the quick action at `index` (0-based) against the currently
    /// active session. Both Claude-type and Shell-type quick actions write
    /// to the PTY — Claude agents treat the text as a prompt, bare shells
    /// treat it as a shell command. A trailing `\n` is always appended so
    /// the agent/shell processes the input immediately.
    ///
    /// Returns Ok(None) if there's no quick action at that index, Ok(Some(name))
    /// if one was executed (for UI feedback).
    pub fn execute_quick_action(&self, index: usize) -> Result<Option<String>> {
        let store = crate::quick_actions::QuickActionStore::new(&self.db.conn);
        let actions = store.list_all()?;
        let Some(action) = actions.get(index) else {
            return Ok(None);
        };

        // Both kinds end up as bytes into the active PTY. The difference
        // is semantic: Claude-type is a prompt, Shell-type is a command
        // line. Quay writes the exact body + newline regardless — it's
        // up to the running process to interpret.
        let mut bytes = action.body.clone().into_bytes();
        bytes.push(b'\n');
        self.write_to_active(&bytes);

        tracing::info!(
            index,
            name = %action.name,
            kind = %action.kind.as_str(),
            "executed quick action"
        );
        Ok(Some(action.name.clone()))
    }

}

// ── Free-standing helpers ───────────────────────────────────────────────────

/// Update `tasks.claude_session_id` for one task via a fresh short-lived
/// rusqlite connection. Called from the background claude-resume capture
/// thread, which cannot share the main `AppState.db.conn` (Connection is
/// not Send).
///
/// Opens in WAL mode with foreign keys on, same as `Database::configure`,
/// so concurrent reads from the main thread are cheap and consistent.
fn persist_claude_session_id(
    db_path: &Path,
    task_id: Uuid,
    session_id: &str,
) -> Result<()> {
    let conn = rusqlite::Connection::open(db_path)
        .with_context(|| format!("open sqlite at {}", db_path.display()))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute(
        "UPDATE tasks SET claude_session_id = ?1, updated_at = ?2 WHERE id = ?3",
        rusqlite::params![
            session_id,
            unix_millis_now(),
            task_id.to_string(),
        ],
    )
    .context("UPDATE tasks.claude_session_id")?;
    Ok(())
}

/// Whether a path is the root of a git repository.
///
/// Uses `git2::Repository::open` so it's purely in-process (no subprocess).
/// Returns false for non-git directories, missing paths, and broken repos.
pub fn is_git_repo(path: &Path) -> bool {
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

// ── Polish 16 pure helpers ───────────────────────────────────────────────
//
// Exposed as free functions so tests can exercise the open-tab list
// logic without having to build an `AppState` (which needs a full
// `Database`, `GlyphAtlas` and `QuayDirs`).

/// Append `id` to `tabs` if not already present. Returns true if the
/// list was changed.
fn pin_tab_in_place(tabs: &mut Vec<Uuid>, id: Uuid) -> bool {
    if tabs.contains(&id) {
        return false;
    }
    tabs.push(id);
    true
}

/// Remove `id` from `tabs`. When `was_active` is true and the list is
/// still non-empty after the removal, returns the neighbouring tab id
/// the caller should focus (same index, clamped to the new tail).
/// Otherwise returns `None`.
fn close_tab_in_place(tabs: &mut Vec<Uuid>, id: Uuid, was_active: bool) -> Option<Uuid> {
    let Some(idx) = tabs.iter().position(|t| *t == id) else {
        return None;
    };
    tabs.remove(idx);
    if !was_active {
        return None;
    }
    if tabs.is_empty() {
        return None;
    }
    let fallback_idx = idx.min(tabs.len() - 1);
    Some(tabs[fallback_idx])
}

/// Polish 41 — keep only `keep_id` in `tabs`. Returns true if the
/// list actually changed (false if it already contained only that id
/// or if `keep_id` wasn't present at all).
fn close_others_in_place(tabs: &mut Vec<Uuid>, keep_id: Uuid) -> bool {
    if !tabs.contains(&keep_id) {
        return false;
    }
    if tabs.len() == 1 {
        return false;
    }
    tabs.clear();
    tabs.push(keep_id);
    true
}

/// Polish 41 — drop every element strictly after `anchor_id`,
/// keeping the anchor and everything to its left. Returns true if
/// the list actually changed.
fn close_right_of_in_place(tabs: &mut Vec<Uuid>, anchor_id: Uuid) -> bool {
    let Some(anchor_idx) = tabs.iter().position(|t| *t == anchor_id) else {
        return false;
    };
    if anchor_idx + 1 >= tabs.len() {
        return false;
    }
    tabs.truncate(anchor_idx + 1);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // Four stable uuids so fail messages point at the right tab.
    fn u(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn pin_appends_missing_and_ignores_duplicates() {
        let mut tabs = Vec::new();
        assert!(pin_tab_in_place(&mut tabs, u(1)));
        assert!(pin_tab_in_place(&mut tabs, u(2)));
        assert!(!pin_tab_in_place(&mut tabs, u(1)), "duplicate should no-op");
        assert_eq!(tabs, vec![u(1), u(2)]);
    }

    #[test]
    fn close_non_active_tab_keeps_focus_and_returns_none() {
        let mut tabs = vec![u(1), u(2), u(3)];
        let next = close_tab_in_place(&mut tabs, u(2), false);
        assert_eq!(next, None);
        assert_eq!(tabs, vec![u(1), u(3)]);
    }

    #[test]
    fn close_active_middle_tab_falls_back_to_same_index() {
        // Closing #2 (active) from [1,2,3] → focus lands on what sits
        // at index 1, i.e. tab #3, which slid into the slot.
        let mut tabs = vec![u(1), u(2), u(3)];
        let next = close_tab_in_place(&mut tabs, u(2), true);
        assert_eq!(next, Some(u(3)));
        assert_eq!(tabs, vec![u(1), u(3)]);
    }

    #[test]
    fn close_active_tail_tab_falls_back_to_previous() {
        // Closing #3 (active) from [1,2,3] → no slot-1 anymore, so
        // we clamp back to index 1 in the reduced list and land on #2.
        let mut tabs = vec![u(1), u(2), u(3)];
        let next = close_tab_in_place(&mut tabs, u(3), true);
        assert_eq!(next, Some(u(2)));
        assert_eq!(tabs, vec![u(1), u(2)]);
    }

    #[test]
    fn close_only_tab_returns_none() {
        let mut tabs = vec![u(1)];
        let next = close_tab_in_place(&mut tabs, u(1), true);
        assert_eq!(next, None);
        assert!(tabs.is_empty());
    }

    #[test]
    fn close_missing_tab_is_noop() {
        let mut tabs = vec![u(1), u(2)];
        let next = close_tab_in_place(&mut tabs, u(99), true);
        assert_eq!(next, None);
        assert_eq!(tabs, vec![u(1), u(2)]);
    }

    // Polish 41 — close-others helper.

    #[test]
    fn close_others_keeps_only_target() {
        let mut tabs = vec![u(1), u(2), u(3), u(4)];
        let changed = close_others_in_place(&mut tabs, u(2));
        assert!(changed);
        assert_eq!(tabs, vec![u(2)]);
    }

    #[test]
    fn close_others_noop_when_already_alone() {
        let mut tabs = vec![u(1)];
        let changed = close_others_in_place(&mut tabs, u(1));
        assert!(!changed);
        assert_eq!(tabs, vec![u(1)]);
    }

    #[test]
    fn close_others_noop_when_target_missing() {
        let mut tabs = vec![u(1), u(2), u(3)];
        let changed = close_others_in_place(&mut tabs, u(99));
        assert!(!changed);
        assert_eq!(tabs, vec![u(1), u(2), u(3)]);
    }

    // Polish 41 — close-right-of helper.

    #[test]
    fn close_right_of_drops_tail() {
        let mut tabs = vec![u(1), u(2), u(3), u(4), u(5)];
        let changed = close_right_of_in_place(&mut tabs, u(2));
        assert!(changed);
        assert_eq!(tabs, vec![u(1), u(2)]);
    }

    #[test]
    fn close_right_of_anchor_at_tail_is_noop() {
        let mut tabs = vec![u(1), u(2), u(3)];
        let changed = close_right_of_in_place(&mut tabs, u(3));
        assert!(!changed);
        assert_eq!(tabs, vec![u(1), u(2), u(3)]);
    }

    #[test]
    fn close_right_of_missing_anchor_is_noop() {
        let mut tabs = vec![u(1), u(2)];
        let changed = close_right_of_in_place(&mut tabs, u(99));
        assert!(!changed);
        assert_eq!(tabs, vec![u(1), u(2)]);
    }

    #[test]
    fn close_right_of_first_tab_keeps_only_first() {
        let mut tabs = vec![u(1), u(2), u(3)];
        let changed = close_right_of_in_place(&mut tabs, u(1));
        assert!(changed);
        assert_eq!(tabs, vec![u(1)]);
    }
}
