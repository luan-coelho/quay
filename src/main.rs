//! Quay — cross-platform native workspace for orchestrating AI coding agent sessions.

mod agents;
mod app;
mod git;
mod kanban;
mod persistence;
mod terminal;
mod util;

slint::include_modules!();

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::str::FromStr;
use std::time::Duration;

use anyhow::Result;
use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use uuid::Uuid;

use crate::app::AppState;
use crate::kanban::{SessionState, StartMode, Task, TaskKind, TaskState};
use crate::persistence::QuayDirs;
use crate::terminal::{GlyphAtlas, key_text_to_bytes};

const DEFAULT_COLS: usize = 96;
const DEFAULT_ROWS: usize = 28;
const FONT_SIZE: f32 = 14.0;

fn main() -> Result<()> {
    util::log::init();
    tracing::info!("quay starting");

    let window = MainWindow::new()?;

    // Glyph atlas + framebuffer setup.
    let atlas = Rc::new(GlyphAtlas::new(FONT_SIZE));
    tracing::info!(
        cell_w = atlas.cell_w,
        cell_h = atlas.cell_h,
        baseline = atlas.baseline,
        "glyph atlas ready"
    );
    window.set_cell_w(atlas.cell_w as i32);
    window.set_cell_h(atlas.cell_h as i32);
    window.set_cols(DEFAULT_COLS as i32);
    window.set_rows(DEFAULT_ROWS as i32);

    // Discover OS data dirs and open the database.
    let dirs = QuayDirs::discover()?;
    tracing::info!(data_dir = %dirs.data_dir.display(), "data dirs ready");

    let (shell, _shell_label) = default_shell();
    let home = home_directory();

    let state = Rc::new(AppState::new(
        atlas.clone(),
        DEFAULT_COLS,
        DEFAULT_ROWS,
        dirs,
        home,
        shell,
        "claude".to_string(),
    )?);
    state.seed_demo_if_empty()?;

    // Initial blank framebuffer.
    window.set_frame(Image::from_rgba8_premultiplied(
        state.framebuffer.borrow().buffer.clone(),
    ));
    window.set_active_task_id("".into());
    window.set_active_task_display("".into());
    window.set_active_task_title("".into());
    window.set_active_task_description("".into());
    window.set_active_task_instructions("".into());
    window.set_active_task_session_state("idle".into());
    window.set_active_right_tab("terminal".into());

    // Sidebar: menu items.
    let menu_model = Rc::new(VecModel::<MenuItemData>::default());
    for item in [
        ("new-task",      "✦",  "New CLI Session", "⌘N"),
        ("new-terminal",  "▣",  "New Terminal",    "⌘T"),
        ("quick-action",  "⚡", "Quick Action",    "▸"),
        ("configure",     "⚙",  "Configure",       ""),
        ("sessions",      "≡",  "Sessions",        ""),
        ("worktrees",     "⎇",  "Worktrees",       ""),
    ] {
        menu_model.push(MenuItemData {
            id: item.0.into(),
            glyph: item.1.into(),
            label: item.2.into(),
            shortcut: item.3.into(),
        });
    }
    window.set_menu_items(ModelRc::from(menu_model));

    // Sidebar: projects (placeholder data; real project loading is a future task).
    let projects_model = Rc::new(VecModel::<ProjectData>::default());
    for (id, name) in [("backend", "backend"), ("frontend", "frontend")] {
        projects_model.push(ProjectData {
            id: id.into(),
            name: name.into(),
        });
    }
    window.set_projects(ModelRc::from(projects_model));

    // Kanban column models — one VecModel per TaskState (6 total after
    // Phase 2's Review + Misc addition).
    let backlog_model = Rc::new(VecModel::<TaskCardData>::default());
    let planning_model = Rc::new(VecModel::<TaskCardData>::default());
    let implementation_model = Rc::new(VecModel::<TaskCardData>::default());
    let review_model = Rc::new(VecModel::<TaskCardData>::default());
    let done_model = Rc::new(VecModel::<TaskCardData>::default());
    let misc_model = Rc::new(VecModel::<TaskCardData>::default());
    window.set_tasks_backlog(ModelRc::from(backlog_model.clone()));
    window.set_tasks_planning(ModelRc::from(planning_model.clone()));
    window.set_tasks_implementation(ModelRc::from(implementation_model.clone()));
    window.set_tasks_review(ModelRc::from(review_model.clone()));
    window.set_tasks_done(ModelRc::from(done_model.clone()));
    window.set_tasks_misc(ModelRc::from(misc_model.clone()));

    // Git Changes tab models — refreshed by a ~1s timer when the tab is
    // open. Empty on startup; populated from git::diff::read_diff and
    // read_commit_log once the user selects a task with a worktree.
    let git_diff_model = Rc::new(VecModel::<DiffFileData>::default());
    let git_log_model = Rc::new(VecModel::<CommitEntryData>::default());
    window.set_git_diff_files(ModelRc::from(git_diff_model.clone()));
    window.set_git_commit_log(ModelRc::from(git_log_model.clone()));

    // Refresh closure: re-query DB and rebuild every column model.
    let refresh_kanban = {
        let state = state.clone();
        let backlog = backlog_model.clone();
        let planning = planning_model.clone();
        let implementation = implementation_model.clone();
        let review = review_model.clone();
        let done = done_model.clone();
        let misc = misc_model.clone();
        move || {
            let tasks = match state.list_tasks() {
                Ok(t) => t,
                Err(err) => {
                    tracing::error!(%err, "failed to list tasks");
                    return;
                }
            };

            // Build a stable display-id map: order tasks by created_at and
            // assign 1, 2, 3, … so each task keeps its number across refreshes.
            let mut sorted_by_creation = tasks.clone();
            sorted_by_creation.sort_by_key(|t| t.created_at);
            let mut display_ids: HashMap<Uuid, i32> = HashMap::new();
            for (i, t) in sorted_by_creation.iter().enumerate() {
                display_ids.insert(t.id, (i + 1) as i32);
            }

            let active_uuid = *state.active_task.borrow();

            let mut backlog_v = Vec::new();
            let mut planning_v = Vec::new();
            let mut implementation_v = Vec::new();
            let mut review_v = Vec::new();
            let mut done_v = Vec::new();
            let mut misc_v = Vec::new();

            for task in &tasks {
                let display_id = display_ids.get(&task.id).copied().unwrap_or(0);
                let running = active_uuid == Some(task.id);
                // Dirty flag: only meaningful when the task actually has a
                // worktree. `git::status::read_status` is cheap (libgit2
                // in-process) but we still skip the call when there's
                // nothing to inspect to keep refresh latency low on big
                // kanbans.
                let dirty = task
                    .worktree_path
                    .as_deref()
                    .and_then(|p| match crate::git::status::read_status(p) {
                        Ok(status) => Some(!status.clean),
                        Err(err) => {
                            tracing::debug!(
                                task_id = %task.id,
                                %err,
                                "read_status failed for worktree, treating as clean"
                            );
                            None
                        }
                    })
                    .unwrap_or(false);
                let card = task_to_card(task, display_id, running, dirty);
                match task.state {
                    TaskState::Backlog => backlog_v.push(card),
                    TaskState::Planning => planning_v.push(card),
                    TaskState::Implementation => implementation_v.push(card),
                    TaskState::Review => review_v.push(card),
                    TaskState::Done => done_v.push(card),
                    TaskState::Misc => misc_v.push(card),
                }
            }

            replace_model(&backlog, backlog_v);
            replace_model(&planning, planning_v);
            replace_model(&implementation, implementation_v);
            replace_model(&review, review_v);
            replace_model(&done, done_v);
            replace_model(&misc, misc_v);
        }
    };
    refresh_kanban();

    // ── Callbacks ────────────────────────────────────────────────────────────
    {
        let state = state.clone();
        let weak = window.as_weak();
        let refresh = refresh_kanban.clone();
        window.on_select_task(move |id| {
            let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
            match state.select_task(uuid) {
                // Only refresh the UI for the description/title when the
                // active task actually changed — otherwise a second click on
                // the same card would clobber in-progress Description edits.
                Ok(changed) if !changed => {}
                Ok(_) => {
                    if let Some(window) = weak.upgrade() {
                        let card_data = if let Ok(Some(task)) =
                            crate::kanban::TaskStore::new(&state.db.conn).get(uuid)
                        {
                            // Compute display-id by scanning all tasks ordered
                            // by creation date — same logic as refresh.
                            let all = state.list_tasks().unwrap_or_default();
                            let mut sorted = all.clone();
                            sorted.sort_by_key(|t| t.created_at);
                            let display_id = sorted
                                .iter()
                                .position(|t| t.id == uuid)
                                .map(|i| i + 1)
                                .unwrap_or(0);
                            let kind = TaskKind::from_title(&task.title);
                            window.set_active_task_kind(kind_to_str(kind).into());
                            Some((
                                format!("#{display_id}"),
                                task.title.clone(),
                                task.description.clone().unwrap_or_default(),
                                task.instructions.clone().unwrap_or_default(),
                                task.session_state.as_str().to_string(),
                            ))
                        } else {
                            None
                        };

                        let (display, title, description, instructions, sess_state) =
                            card_data.unwrap_or_default();
                        window.set_active_task_id(id.clone());
                        window.set_active_task_display(display.into());
                        window.set_active_task_title(title.into());
                        window.set_active_task_description(description.into());
                        window.set_active_task_instructions(instructions.into());
                        window.set_active_task_session_state(sess_state.into());
                        if state.blit_active() {
                            window.set_frame(Image::from_rgba8_premultiplied(
                                state.framebuffer.borrow().buffer.clone(),
                            ));
                        }
                    }
                    refresh();
                }
                Err(err) => tracing::error!(%err, "failed to select task"),
            }
        });
    }
    {
        let state = state.clone();
        let refresh = refresh_kanban.clone();
        window.on_create_task(move || {
            let count = state.list_tasks().map(|t| t.len()).unwrap_or(0) + 1;
            let title = format!("New task {count}");
            if let Err(err) = state.create_task(title) {
                tracing::error!(%err, "create_task failed");
            }
            refresh();
        });
    }
    {
        let state = state.clone();
        let refresh = refresh_kanban.clone();
        window.on_move_task_forward(move |id| {
            let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
            if let Err(err) = state.move_forward(uuid) {
                tracing::error!(%err, "move_forward failed");
            }
            refresh();
        });
    }
    {
        let state = state.clone();
        let refresh = refresh_kanban.clone();
        window.on_move_task_back(move |id| {
            let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
            if let Err(err) = state.move_backward(uuid) {
                tracing::error!(%err, "move_backward failed");
            }
            refresh();
        });
    }
    {
        let state = state.clone();
        let refresh = refresh_kanban.clone();
        window.on_title_changed(move |text| {
            let Some(active_id) = *state.active_task.borrow() else {
                return;
            };
            let trimmed = text.to_string();
            if trimmed.is_empty() {
                return;
            }
            let store = crate::kanban::TaskStore::new(&state.db.conn);
            match store.get(active_id) {
                Ok(Some(mut task)) => {
                    if task.title == trimmed {
                        return;
                    }
                    task.title = trimmed;
                    task.updated_at = crate::kanban::unix_millis_now();
                    if let Err(err) = store.update(&task) {
                        tracing::error!(%err, "failed to update task title");
                        return;
                    }
                    refresh();
                }
                Ok(None) => {}
                Err(err) => tracing::error!(%err, "failed to load task for title update"),
            }
        });
    }
    {
        let state = state.clone();
        window.on_description_changed(move |text| {
            // Persist the new description on the currently-active task.
            let Some(active_id) = *state.active_task.borrow() else {
                return;
            };
            let store = crate::kanban::TaskStore::new(&state.db.conn);
            match store.get(active_id) {
                Ok(Some(mut task)) => {
                    let new_value = text.to_string();
                    task.description = if new_value.is_empty() {
                        None
                    } else {
                        Some(new_value)
                    };
                    task.updated_at = crate::kanban::unix_millis_now();
                    if let Err(err) = store.update(&task) {
                        tracing::error!(%err, "failed to update task description");
                    }
                }
                Ok(None) => {}
                Err(err) => tracing::error!(%err, "failed to load task for description update"),
            }
        });
    }
    {
        // Instructions field mirrors description: persisted live on every
        // edited event. Empty strings are coerced to NULL in the DB.
        let state = state.clone();
        window.on_instructions_changed(move |text| {
            let Some(active_id) = *state.active_task.borrow() else {
                return;
            };
            let store = crate::kanban::TaskStore::new(&state.db.conn);
            match store.get(active_id) {
                Ok(Some(mut task)) => {
                    let new_value = text.to_string();
                    task.instructions = if new_value.is_empty() {
                        None
                    } else {
                        Some(new_value)
                    };
                    task.updated_at = crate::kanban::unix_millis_now();
                    if let Err(err) = store.update(&task) {
                        tracing::error!(%err, "failed to update task instructions");
                    }
                }
                Ok(None) => {}
                Err(err) => tracing::error!(%err, "failed to load task for instructions update"),
            }
        });
    }
    {
        // Plan button: start an agent session in Plan mode. The task
        // transitions into the Planning column via start_session; here we
        // also push the updated session_state chip and re-blit the
        // framebuffer immediately so the user sees "busy" without waiting
        // for the next poll tick.
        let state = state.clone();
        let weak = window.as_weak();
        let refresh = refresh_kanban.clone();
        window.on_start_plan(move |id| {
            let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
            match state.start_session(uuid, StartMode::Plan) {
                Ok(()) => {
                    if let Some(window) = weak.upgrade() {
                        window.set_active_task_session_state(
                            SessionState::Busy.as_str().into(),
                        );
                        if state.blit_active() {
                            window.set_frame(Image::from_rgba8_premultiplied(
                                state.framebuffer.borrow().buffer.clone(),
                            ));
                        }
                        // Jump to Terminal tab so the user sees the agent start.
                        window.set_active_right_tab("terminal".into());
                    }
                    refresh();
                }
                Err(err) => {
                    tracing::error!(%err, "start_session(Plan) failed");
                    if let Some(window) = weak.upgrade() {
                        window.set_active_task_session_state(
                            SessionState::Error.as_str().into(),
                        );
                    }
                }
            }
        });
    }
    {
        // Implement button: same wiring as Plan but StartMode::Implement.
        let state = state.clone();
        let weak = window.as_weak();
        let refresh = refresh_kanban.clone();
        window.on_start_implement(move |id| {
            let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
            match state.start_session(uuid, StartMode::Implement) {
                Ok(()) => {
                    if let Some(window) = weak.upgrade() {
                        window.set_active_task_session_state(
                            SessionState::Busy.as_str().into(),
                        );
                        if state.blit_active() {
                            window.set_frame(Image::from_rgba8_premultiplied(
                                state.framebuffer.borrow().buffer.clone(),
                            ));
                        }
                        window.set_active_right_tab("terminal".into());
                    }
                    refresh();
                }
                Err(err) => {
                    tracing::error!(%err, "start_session(Implement) failed");
                    if let Some(window) = weak.upgrade() {
                        window.set_active_task_session_state(
                            SessionState::Error.as_str().into(),
                        );
                    }
                }
            }
        });
    }
    {
        let state = state.clone();
        window.on_key_pressed(move |text, ctrl, alt, shift| {
            let bytes = key_text_to_bytes(text.as_str(), ctrl, alt, shift);
            if !bytes.is_empty() {
                state.write_to_active(&bytes);
            }
        });
    }

    // PTY poll timer — drains bytes from all live sessions and blits the
    // active one. Fires ~60 Hz.
    let poll_timer = Timer::default();
    {
        let weak = window.as_weak();
        let state = state.clone();
        poll_timer.start(
            TimerMode::Repeated,
            Duration::from_millis(16),
            move || {
                if state.poll_all_sessions() && state.blit_active() {
                    if let Some(window) = weak.upgrade() {
                        window.set_frame(Image::from_rgba8_premultiplied(
                            state.framebuffer.borrow().buffer.clone(),
                        ));
                    }
                }
            },
        );
    }

    // Git Changes tab refresh timer — fires once per second. Cheap when the
    // tab is not open (early-return if active_right_tab != "git"), re-queries
    // `git::diff::read_diff` and `read_commit_log` for the active task's
    // worktree otherwise.
    let git_refresh_timer = Timer::default();
    {
        let weak = window.as_weak();
        let state = state.clone();
        let diff_model = git_diff_model.clone();
        let log_model = git_log_model.clone();
        git_refresh_timer.start(
            TimerMode::Repeated,
            Duration::from_secs(1),
            move || {
                let Some(window) = weak.upgrade() else { return };
                if window.get_active_right_tab() != "git" {
                    return;
                }
                let Some(active_id) = *state.active_task.borrow() else {
                    return;
                };
                let store = crate::kanban::TaskStore::new(&state.db.conn);
                let Ok(Some(task)) = store.get(active_id) else { return };
                let Some(worktree_path) = task.worktree_path.as_deref() else {
                    // Task has no worktree — clear the models.
                    replace_diff_model(&diff_model, Vec::new());
                    replace_log_model(&log_model, Vec::new());
                    return;
                };
                // Resolve a base branch by trying `main` then `master`.
                let base = ["main", "master"]
                    .iter()
                    .find(|b| {
                        git2::Repository::open(worktree_path)
                            .and_then(|r| r.find_branch(b, git2::BranchType::Local).map(|_| ()))
                            .is_ok()
                    })
                    .copied()
                    .unwrap_or("main");

                let diff_files = match crate::git::diff::read_diff(worktree_path, base) {
                    Ok(v) => v
                        .into_iter()
                        .map(|f| DiffFileData {
                            path: SharedString::from(f.path),
                            status: SharedString::from(f.status.to_string()),
                            additions: f.additions as i32,
                            deletions: f.deletions as i32,
                            patch: SharedString::from(f.patch),
                        })
                        .collect(),
                    Err(err) => {
                        tracing::debug!(%err, "read_diff failed");
                        Vec::new()
                    }
                };
                let commits = match crate::git::diff::read_commit_log(worktree_path, base, 20) {
                    Ok(v) => v
                        .into_iter()
                        .map(|c| CommitEntryData {
                            sha_short: SharedString::from(c.sha_short),
                            summary: SharedString::from(c.summary),
                            author_name: SharedString::from(c.author_name),
                            timestamp: SharedString::from(format_relative_time(c.timestamp)),
                        })
                        .collect(),
                    Err(err) => {
                        tracing::debug!(%err, "read_commit_log failed");
                        Vec::new()
                    }
                };

                replace_diff_model(&diff_model, diff_files);
                replace_log_model(&log_model, commits);
            },
        );
    }

    window.run()?;
    drop(poll_timer);
    Ok(())
}

fn task_to_card(task: &Task, display_id: i32, running: bool, dirty: bool) -> TaskCardData {
    let kind = TaskKind::from_title(&task.title);
    TaskCardData {
        id: SharedString::from(task.id.to_string()),
        display_id,
        title: SharedString::from(task.title.as_str()),
        kind: SharedString::from(kind_to_str(kind)),
        running,
        dirty,
    }
}

fn kind_to_str(kind: TaskKind) -> &'static str {
    match kind {
        TaskKind::Enhancement => "enhancement",
        TaskKind::Feature => "feature",
        TaskKind::Bug => "bug",
    }
}

fn replace_model(model: &Rc<VecModel<TaskCardData>>, items: Vec<TaskCardData>) {
    while model.row_count() > 0 {
        model.remove(model.row_count() - 1);
    }
    for item in items {
        model.push(item);
    }
}

fn replace_diff_model(model: &Rc<VecModel<DiffFileData>>, items: Vec<DiffFileData>) {
    while model.row_count() > 0 {
        model.remove(model.row_count() - 1);
    }
    for item in items {
        model.push(item);
    }
}

fn replace_log_model(model: &Rc<VecModel<CommitEntryData>>, items: Vec<CommitEntryData>) {
    while model.row_count() > 0 {
        model.remove(model.row_count() - 1);
    }
    for item in items {
        model.push(item);
    }
}

/// Human-friendly relative time for commit timestamps.
/// "just now" · "3m ago" · "2h ago" · "4d ago" · "2026-04-10".
fn format_relative_time(commit_time_secs: i64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let delta = now_secs.saturating_sub(commit_time_secs);
    if delta < 30 {
        "just now".into()
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86400 {
        format!("{}h ago", delta / 3600)
    } else if delta < 7 * 86400 {
        format!("{}d ago", delta / 86400)
    } else {
        // Fall back to a YYYY-MM-DD-ish rendering using the absolute time.
        // We avoid pulling in chrono for this — a crude div/mod is fine.
        let days_since_epoch = commit_time_secs / 86400;
        format!("~{days_since_epoch} days since epoch")
    }
}

#[cfg(unix)]
fn default_shell() -> (String, String) {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let label = std::path::Path::new(&shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("sh")
        .to_string();
    (shell, label)
}

#[cfg(windows)]
fn default_shell() -> (String, String) {
    ("powershell.exe".to_string(), "powershell".to_string())
}

fn home_directory() -> PathBuf {
    if let Some(dirs) = directories::UserDirs::new() {
        return dirs.home_dir().to_path_buf();
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}
