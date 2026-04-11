//! Quay — cross-platform native workspace for orchestrating AI coding agent sessions.

mod agents;
mod app;
mod editor;
mod file_tree;
mod git;
mod kanban;
mod persistence;
mod process;
mod quick_actions;
mod settings;
mod terminal;
mod util;

slint::include_modules!();

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::str::FromStr;
use std::time::Duration;

use anyhow::Result;
use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use uuid::Uuid;

use std::cell::RefCell;

use crate::app::AppState;
use crate::editor::EditorBuffer;
use crate::kanban::{
    DependencyStore, Label, LabelStore, SessionState, StartMode, Task, TaskKind, TaskState,
};
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
    // Phase 4: seed the Lanes preset label palette on first run and
    // auto-tag any seed demo tasks by their heuristic TaskKind so the
    // kanban has colour from day one.
    {
        let label_store = LabelStore::new(&state.db.conn);
        label_store.seed_presets_if_empty()?;
        auto_tag_seed_tasks(&state)?;
    }
    // Phase 5: seed the default Quick Actions + Settings on first run.
    {
        let qa_store = crate::quick_actions::QuickActionStore::new(&state.db.conn);
        qa_store.seed_defaults_if_empty()?;
        let settings_store = crate::settings::Settings::new(&state.db.conn);
        settings_store.seed_defaults_if_empty()?;
    }

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
    // Sidebar: projects. Polish 2 — seed a default "Home" project on
    // first run pointing at the user's home directory, then load the
    // real list from `ProjectStore`. The list is refreshed whenever
    // the user creates a new project (via the `+` button in the
    // Projects section) or deletes one.
    {
        let project_store = crate::kanban::ProjectStore::new(&state.db.conn);
        if project_store.list_all().map(|v| v.is_empty()).unwrap_or(false) {
            let _ = project_store.insert(&crate::kanban::Project::new(
                "Home",
                state.default_cwd.clone(),
                "main",
            ));
        }
    }
    let projects_model = Rc::new(VecModel::<ProjectData>::default());
    window.set_projects(ModelRc::from(projects_model.clone()));

    // Helper closure for rebuilding the sidebar project list.
    let refresh_projects = {
        let state = state.clone();
        let model = projects_model.clone();
        move || {
            let list = crate::kanban::ProjectStore::new(&state.db.conn)
                .list_all()
                .unwrap_or_default();
            while model.row_count() > 0 {
                model.remove(model.row_count() - 1);
            }
            for p in list {
                model.push(ProjectData {
                    id: SharedString::from(p.id.to_string()),
                    name: SharedString::from(p.name),
                });
            }
        }
    };
    refresh_projects();

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

    // All labels — rendered as filter chips above the task list.
    let all_labels_model = Rc::new(VecModel::<LabelPillData>::default());
    window.set_all_labels(ModelRc::from(all_labels_model.clone()));
    window.set_filter_label_id("".into());

    // Polish 3: per-task label/dependency management UI state.
    let active_task_labels_model = Rc::new(VecModel::<LabelPillData>::default());
    let active_task_available_labels_model = Rc::new(VecModel::<LabelPillData>::default());
    let active_task_deps_model = Rc::new(VecModel::<TaskCardData>::default());
    window.set_active_task_labels(ModelRc::from(active_task_labels_model.clone()));
    window
        .set_active_task_available_labels(ModelRc::from(active_task_available_labels_model.clone()));
    window.set_active_task_dependencies(ModelRc::from(active_task_deps_model.clone()));
    window.set_label_picker_open(false);

    // Files tab state. Populated on select_task if the task has a
    // worktree; empty otherwise.
    let file_entries_model = Rc::new(VecModel::<FileEntryData>::default());
    window.set_file_entries(ModelRc::from(file_entries_model.clone()));
    window.set_file_current_dir("".into());

    // Phase 7 editor state. A single shared `EditorBuffer` is opened on
    // demand when the user clicks a text file in the Files tab.
    let editor_buffer: Rc<RefCell<Option<EditorBuffer>>> = Rc::new(RefCell::new(None));
    window.set_editor_open(false);
    window.set_editor_file_path("".into());
    window.set_editor_file_content("".into());
    window.set_editor_file_dirty(false);
    window.set_editor_syntax_name("Plain Text".into());
    // Polish 5: coloured preview model for the syntect view.
    let editor_lines_model = Rc::new(VecModel::<HighlightedLineData>::default());
    window.set_editor_highlighted_lines(ModelRc::from(editor_lines_model.clone()));
    window.set_editor_line_count(0);

    // Settings modal state — initial values loaded from SQLite.
    let settings_qa_model = Rc::new(VecModel::<QuickActionRowData>::default());
    let settings_proc_model = Rc::new(VecModel::<ProcessRowData>::default());
    window.set_settings_quick_actions(ModelRc::from(settings_qa_model.clone()));
    window.set_settings_processes(ModelRc::from(settings_proc_model.clone()));
    window.set_settings_open(false);
    {
        let settings = crate::settings::Settings::new(&state.db.conn);
        let base_branch = settings.get_or(crate::settings::KEY_DEFAULT_BASE_BRANCH, "main");
        window.set_settings_default_base_branch(base_branch.into());
    }
    {
        let label_store = LabelStore::new(&state.db.conn);
        let labels = label_store.list_all().unwrap_or_default();
        for l in labels {
            all_labels_model.push(label_to_pill(&l));
        }
    }

    // Refresh closure: re-query DB and rebuild every column model.
    //
    // Reads the current `filter-label-id` from the window — if set, only
    // tasks carrying that label survive the filter.
    let refresh_kanban = {
        let state = state.clone();
        let weak = window.as_weak();
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

            // Resolve the active label filter (empty string = no filter).
            let filter_label_id: Option<Uuid> = weak
                .upgrade()
                .and_then(|w| {
                    let s = w.get_filter_label_id().to_string();
                    if s.is_empty() {
                        None
                    } else {
                        Uuid::from_str(&s).ok()
                    }
                });

            let active_uuid = *state.active_task.borrow();
            let label_store = LabelStore::new(&state.db.conn);
            let dep_store = DependencyStore::new(&state.db.conn);

            let mut backlog_v = Vec::new();
            let mut planning_v = Vec::new();
            let mut implementation_v = Vec::new();
            let mut review_v = Vec::new();
            let mut done_v = Vec::new();
            let mut misc_v = Vec::new();

            for task in &tasks {
                let display_id = display_ids.get(&task.id).copied().unwrap_or(0);
                let running = active_uuid == Some(task.id);

                // Load the task's labels. Used both for the pills on the
                // card and for the filter check below.
                let task_labels = label_store.labels_for_task(task.id).unwrap_or_default();

                // Label filter: skip this task if we have an active
                // filter and this task doesn't carry the label.
                if let Some(filter_id) = filter_label_id
                    && !task_labels.iter().any(|l| l.id == filter_id)
                {
                    continue;
                }

                // Blocked count — how many dependencies are still not Done.
                let blocked_count: i32 = dep_store
                    .dependencies_of(task.id)
                    .unwrap_or_default()
                    .iter()
                    .filter(|dep_id| {
                        tasks
                            .iter()
                            .find(|t| t.id == **dep_id)
                            .map(|t| t.state != TaskState::Done)
                            .unwrap_or(false)
                    })
                    .count() as i32;

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

                let label_pills: Vec<LabelPillData> =
                    task_labels.iter().map(label_to_pill).collect();

                let card = task_to_card(
                    task,
                    display_id,
                    running,
                    dirty,
                    label_pills,
                    blocked_count,
                );
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

    // Polish 3: helper that rebuilds the Description tab's per-task
    // labels / available-labels / dependencies panels. Called from
    // select_task and from each attach/detach/remove-dep callback so
    // the UI stays in sync without a full kanban refresh.
    let refresh_active_panels = {
        let state = state.clone();
        let labels_model = active_task_labels_model.clone();
        let available_model = active_task_available_labels_model.clone();
        let deps_model = active_task_deps_model.clone();
        move || {
            // Clear everything first.
            while labels_model.row_count() > 0 {
                labels_model.remove(labels_model.row_count() - 1);
            }
            while available_model.row_count() > 0 {
                available_model.remove(available_model.row_count() - 1);
            }
            while deps_model.row_count() > 0 {
                deps_model.remove(deps_model.row_count() - 1);
            }

            let Some(active_id) = *state.active_task.borrow() else {
                return;
            };
            let label_store = LabelStore::new(&state.db.conn);
            let dep_store = DependencyStore::new(&state.db.conn);

            // Attached labels.
            let attached = label_store.labels_for_task(active_id).unwrap_or_default();
            let attached_ids: std::collections::HashSet<Uuid> =
                attached.iter().map(|l| l.id).collect();
            for l in &attached {
                labels_model.push(label_to_pill(l));
            }

            // Available labels = all labels not already attached.
            let all = label_store.list_all().unwrap_or_default();
            for l in all {
                if !attached_ids.contains(&l.id) {
                    available_model.push(label_to_pill(&l));
                }
            }

            // Direct dependencies as TaskCardData (reused for ID + title).
            let dep_ids = dep_store.dependencies_of(active_id).unwrap_or_default();
            let all_tasks = state.list_tasks().unwrap_or_default();
            // Stable display-ids: same logic as refresh_kanban.
            let mut sorted = all_tasks.clone();
            sorted.sort_by_key(|t| t.created_at);
            let display_ids: HashMap<Uuid, i32> = sorted
                .iter()
                .enumerate()
                .map(|(i, t)| (t.id, (i + 1) as i32))
                .collect();
            for dep_id in dep_ids {
                if let Some(task) = all_tasks.iter().find(|t| t.id == dep_id) {
                    let display_id = display_ids.get(&task.id).copied().unwrap_or(0);
                    deps_model.push(task_to_card(task, display_id, false, false, Vec::new(), 0));
                }
            }
        }
    };
    refresh_active_panels();

    // ── Callbacks ────────────────────────────────────────────────────────────
    {
        let state = state.clone();
        let weak = window.as_weak();
        let refresh = refresh_kanban.clone();
        let refresh_panels = refresh_active_panels.clone();
        window.on_select_task(move |id| {
            let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
            match state.select_task(uuid) {
                // Only refresh the UI for the description/title when the
                // active task actually changed — otherwise a second click on
                // the same card would clobber in-progress Description edits.
                Ok(changed) if !changed => {}
                Ok(_) => {
                    if let Some(window) = weak.upgrade() {
                        // Phase 6: if the task has a worktree, populate the
                        // file browser with its root. Otherwise clear it.
                        let task_opt =
                            crate::kanban::TaskStore::new(&state.db.conn).get(uuid);
                        if let Ok(Some(ref t)) = task_opt {
                            if let Some(wt) = t.worktree_path.as_deref() {
                                let entries = crate::file_tree::list_dir(wt)
                                    .unwrap_or_default();
                                // We don't have easy access to the file
                                // model here (it lives in a different
                                // closure). Instead, set the current dir
                                // property and let a subsequent click
                                // or tab switch repopulate via the same
                                // handler. For an immediate refresh, the
                                // Files tab re-selects on next click.
                                let _ = entries;
                                window.set_file_current_dir(
                                    wt.to_string_lossy().into_owned().into(),
                                );
                            } else {
                                window.set_file_current_dir("".into());
                            }
                        }

                        let card_data = if let Ok(Some(task)) = task_opt {
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
                    // Polish 3: also rebuild the Description tab panels
                    // so the label/dep sections reflect the newly active
                    // task without waiting for a second event.
                    refresh_panels();
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
        // Phase 4 filter chip — user clicked a label filter (or "All").
        // refresh_kanban re-reads the current `filter-label-id` from the
        // window via its captured weak handle, so we don't need to pass
        // the new id explicitly.
        let refresh = refresh_kanban.clone();
        window.on_filter_changed(move |_new_id| {
            refresh();
        });
    }

    // ── Polish 3 label/dep attach+detach wiring ─────────────────────────
    {
        let state = state.clone();
        let refresh_panels = refresh_active_panels.clone();
        let refresh = refresh_kanban.clone();
        window.on_attach_label(move |label_id| {
            let Some(active_id) = *state.active_task.borrow() else { return };
            let Ok(label_uuid) = Uuid::from_str(label_id.as_str()) else { return };
            let store = LabelStore::new(&state.db.conn);
            if let Err(err) = store.attach(active_id, label_uuid) {
                tracing::warn!(%err, "attach_label failed");
                return;
            }
            refresh_panels();
            refresh();
        });
    }
    {
        let state = state.clone();
        let refresh_panels = refresh_active_panels.clone();
        let refresh = refresh_kanban.clone();
        window.on_detach_label(move |label_id| {
            let Some(active_id) = *state.active_task.borrow() else { return };
            let Ok(label_uuid) = Uuid::from_str(label_id.as_str()) else { return };
            let store = LabelStore::new(&state.db.conn);
            if let Err(err) = store.detach(active_id, label_uuid) {
                tracing::warn!(%err, "detach_label failed");
                return;
            }
            refresh_panels();
            refresh();
        });
    }
    {
        let state = state.clone();
        let refresh_panels = refresh_active_panels.clone();
        let refresh = refresh_kanban.clone();
        window.on_remove_dependency(move |dep_id| {
            let Some(active_id) = *state.active_task.borrow() else { return };
            let Ok(dep_uuid) = Uuid::from_str(dep_id.as_str()) else { return };
            let store = DependencyStore::new(&state.db.conn);
            if let Err(err) = store.remove(active_id, dep_uuid) {
                tracing::warn!(%err, "remove_dependency failed");
                return;
            }
            refresh_panels();
            refresh();
        });
    }
    {
        // Polish 4: delete the currently-active task. Cascades handle
        // labels, dependencies, and sessions via the ON DELETE CASCADE
        // FKs already in the schema. The running PTY session (if any)
        // is dropped explicitly so the child process becomes orphan
        // and will show up in the Process Manager for the user to
        // clean up.
        let state = state.clone();
        let weak = window.as_weak();
        let refresh = refresh_kanban.clone();
        let refresh_panels = refresh_active_panels.clone();
        window.on_delete_task(move |id| {
            let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
            // Drop the live session if one exists — PtySession::drop
            // flushes the log writer; the child becomes orphan.
            state.sessions.borrow_mut().remove(&uuid);
            if let Err(err) = crate::kanban::TaskStore::new(&state.db.conn).delete(uuid) {
                tracing::warn!(%err, %uuid, "delete task failed");
                return;
            }
            // Clear active task if we just deleted it.
            let mut active = state.active_task.borrow_mut();
            if *active == Some(uuid) {
                *active = None;
            }
            drop(active);
            // Reset UI panels to empty-state.
            if let Some(window) = weak.upgrade() {
                window.set_active_task_id("".into());
                window.set_active_task_display("".into());
                window.set_active_task_title("".into());
                window.set_active_task_description("".into());
                window.set_active_task_instructions("".into());
                window.set_active_task_session_state("idle".into());
            }
            refresh_panels();
            refresh();
        });
    }

    // ── Phase 5 Settings modal wiring ───────────────────────────────────

    // Helper: rebuild the Settings quick action list from the DB.
    let refresh_settings_qa = {
        let state = state.clone();
        let model = settings_qa_model.clone();
        move || {
            let actions = crate::quick_actions::QuickActionStore::new(&state.db.conn)
                .list_all()
                .unwrap_or_default();
            while model.row_count() > 0 {
                model.remove(model.row_count() - 1);
            }
            for (i, a) in actions.iter().enumerate() {
                let shortcut = if i < 9 {
                    format!("⌘⌥{}", i + 1)
                } else {
                    String::new()
                };
                model.push(QuickActionRowData {
                    id: SharedString::from(a.id.to_string()),
                    name: SharedString::from(a.name.as_str()),
                    kind: SharedString::from(a.kind.as_str()),
                    body: SharedString::from(a.body.as_str()),
                    shortcut: SharedString::from(shortcut),
                });
            }
        }
    };

    // Helper: rebuild the Settings process list by running the process
    // manager scan against the current in-memory session PID set. Polish 1:
    // the tracked set now comes from `AppState::tracked_pids()`, so our
    // spawned agent children are classified as "Tracked" instead of
    // showing up as "Orphans" (which they technically are by parent-pid
    // relationship, but the classifier rule prefers the explicit registry).
    let refresh_settings_processes = {
        let state = state.clone();
        let model = settings_proc_model.clone();
        move || {
            let tracked: HashSet<u32> = state.tracked_pids();
            let entries = crate::process::enumerate(&tracked);
            while model.row_count() > 0 {
                model.remove(model.row_count() - 1);
            }
            for e in entries {
                model.push(ProcessRowData {
                    pid: e.pid as i32,
                    name: SharedString::from(e.name),
                    cmdline: SharedString::from(e.cmdline),
                    class: SharedString::from(e.class.as_str()),
                });
            }
        }
    };

    {
        // Toggle modal: also refresh the quick actions + process list
        // whenever we open (so the UI reflects any changes made via
        // sqlite CLI or other out-of-band edits).
        let weak = window.as_weak();
        let refresh_qa = refresh_settings_qa.clone();
        let refresh_procs = refresh_settings_processes.clone();
        window.on_toggle_settings(move || {
            if let Some(w) = weak.upgrade() {
                let opening = !w.get_settings_open();
                w.set_settings_open(opening);
                if opening {
                    refresh_qa();
                    refresh_procs();
                }
            }
        });
    }
    {
        let state = state.clone();
        window.on_settings_base_branch_changed(move |new_value| {
            let s = crate::settings::Settings::new(&state.db.conn);
            if let Err(err) =
                s.set(crate::settings::KEY_DEFAULT_BASE_BRANCH, new_value.as_str())
            {
                tracing::warn!(%err, "failed to persist base branch setting");
            }
        });
    }
    {
        let refresh_procs = refresh_settings_processes.clone();
        window.on_refresh_processes(move || {
            refresh_procs();
        });
    }
    {
        let refresh_procs = refresh_settings_processes.clone();
        window.on_process_kill(move |pid| {
            if let Err(err) = crate::process::terminate(pid as u32) {
                tracing::warn!(%err, pid, "terminate() failed");
            }
            refresh_procs();
        });
    }
    {
        let refresh_procs = refresh_settings_processes;
        window.on_process_force_kill(move |pid| {
            if let Err(err) = crate::process::force_kill(pid as u32) {
                tracing::warn!(%err, pid, "force_kill() failed");
            }
            refresh_procs();
        });
    }

    // ── Phase 6 file browser wiring ─────────────────────────────────────

    // Helper to rebuild the file entries model for a given directory.
    let refresh_files = {
        let model = file_entries_model.clone();
        let weak = window.as_weak();
        move |dir: PathBuf| {
            let entries = match crate::file_tree::list_dir(&dir) {
                Ok(v) => v,
                Err(err) => {
                    tracing::debug!(%err, "list_dir failed");
                    Vec::new()
                }
            };
            while model.row_count() > 0 {
                model.remove(model.row_count() - 1);
            }
            for e in entries {
                let kind = match e.kind {
                    crate::file_tree::EntryKind::Directory => "directory",
                    crate::file_tree::EntryKind::File => "file",
                    crate::file_tree::EntryKind::Parent => "parent",
                };
                model.push(FileEntryData {
                    name: SharedString::from(e.name),
                    path: SharedString::from(e.path.to_string_lossy().into_owned()),
                    kind: SharedString::from(kind),
                });
            }
            if let Some(w) = weak.upgrade() {
                w.set_file_current_dir(dir.to_string_lossy().into_owned().into());
            }
        }
    };

    {
        // Click handler for file tree entries.
        // - Parent / Directory → refresh the listing against the new dir.
        // - File → try to open inline in the editor. If the file is
        //   binary or too large (>1 MB), fall back to $EDITOR.
        let refresh = refresh_files.clone();
        let weak = window.as_weak();
        let buffer = editor_buffer.clone();
        let editor_lines_clone = editor_lines_model.clone();
        window.on_file_entry_clicked(move |path_str, kind_str| {
            let path = PathBuf::from(path_str.as_str());
            match kind_str.as_str() {
                "directory" | "parent" => refresh(path),
                "file" => {
                    // Open inline if readable as UTF-8 and under 1 MB.
                    let open_inline = match std::fs::metadata(&path) {
                        Ok(m) => m.len() < 1_000_000 && !is_likely_binary(&path),
                        Err(_) => false,
                    };
                    if open_inline {
                        match EditorBuffer::open(&path) {
                            Ok(buf) => {
                                if let Some(w) = weak.upgrade() {
                                    w.set_editor_file_path(
                                        path.to_string_lossy().into_owned().into(),
                                    );
                                    w.set_editor_file_content(buf.rope.to_string().into());
                                    w.set_editor_syntax_name(
                                        buf.syntax_name.clone().into(),
                                    );
                                    w.set_editor_file_dirty(false);
                                    w.set_editor_line_count(buf.line_count() as i32);
                                    w.set_editor_open(true);
                                }
                                // Polish 5: populate the coloured preview.
                                rebuild_editor_highlight(&buf, &editor_lines_clone);
                                *buffer.borrow_mut() = Some(buf);
                                return;
                            }
                            Err(err) => {
                                tracing::warn!(%err, "EditorBuffer::open failed; falling back to $EDITOR");
                            }
                        }
                    }
                    if let Err(err) = crate::file_tree::open_in_editor(&path) {
                        tracing::warn!(%err, path = %path.display(), "open_in_editor failed");
                    }
                }
                _ => {}
            }
        });
    }
    {
        let buffer = editor_buffer.clone();
        let weak = window.as_weak();
        window.on_editor_content_changed(move |new_text| {
            if let Some(buf) = buffer.borrow_mut().as_mut() {
                buf.replace_all(new_text.as_str());
                if let Some(w) = weak.upgrade() {
                    w.set_editor_file_dirty(buf.dirty);
                }
            }
        });
    }
    {
        let buffer = editor_buffer.clone();
        let weak = window.as_weak();
        let lines_model = editor_lines_model.clone();
        window.on_editor_save(move || {
            let mut borrow = buffer.borrow_mut();
            let Some(buf) = borrow.as_mut() else { return };
            match buf.save() {
                Ok(()) => {
                    if let Some(w) = weak.upgrade() {
                        w.set_editor_file_dirty(false);
                        w.set_editor_line_count(buf.line_count() as i32);
                    }
                    // Polish 5: refresh the coloured preview to reflect
                    // the on-disk state.
                    rebuild_editor_highlight(buf, &lines_model);
                    tracing::info!(
                        path = %buf.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
                        "editor save ok"
                    );
                }
                Err(err) => tracing::warn!(%err, "editor save failed"),
            }
        });
    }
    {
        let buffer = editor_buffer.clone();
        let weak = window.as_weak();
        window.on_editor_close(move || {
            *buffer.borrow_mut() = None;
            if let Some(w) = weak.upgrade() {
                w.set_editor_open(false);
                w.set_editor_file_path("".into());
                w.set_editor_file_content("".into());
                w.set_editor_file_dirty(false);
            }
        });
    }
    {
        // Key dispatcher — checked in priority order:
        //
        // 1. Cmd/Ctrl + Alt + digit (1..9) → execute quick action at
        //    (digit - 1) index. Short-circuits so the digit never reaches
        //    the PTY.
        // 2. Otherwise, translate to xterm bytes and forward to the
        //    active session.
        //
        // Cmd (meta on macOS) vs Ctrl on Linux/Windows is handled by
        // accepting either — in practice Slint reports Ctrl on Linux
        // and the user presses the right key for their OS.
        let state = state.clone();
        window.on_key_pressed(move |text, ctrl, alt, shift| {
            // Cmd/Ctrl + Alt + digit — quick action shortcut.
            if ctrl && alt && text.len() == 1 {
                let c = text.chars().next().unwrap_or(' ');
                if let Some(digit) = c.to_digit(10)
                    && (1..=9).contains(&digit)
                {
                    let idx = (digit - 1) as usize;
                    match state.execute_quick_action(idx) {
                        Ok(Some(name)) => tracing::info!(%name, idx, "quick action fired"),
                        Ok(None) => tracing::debug!(idx, "no quick action at index"),
                        Err(err) => tracing::warn!(%err, idx, "quick action failed"),
                    }
                    return;
                }
            }

            // Fall-through: normal PTY input.
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

fn task_to_card(
    task: &Task,
    display_id: i32,
    running: bool,
    dirty: bool,
    labels: Vec<LabelPillData>,
    blocked_count: i32,
) -> TaskCardData {
    let kind = TaskKind::from_title(&task.title);
    TaskCardData {
        id: SharedString::from(task.id.to_string()),
        display_id,
        title: SharedString::from(task.title.as_str()),
        kind: SharedString::from(kind_to_str(kind)),
        running,
        dirty,
        labels: ModelRc::from(Rc::new(VecModel::from(labels))),
        blocked_count,
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

/// Convert a `Label` DB record into the Slint-side `LabelPillData`,
/// parsing the `#rrggbb` hex into separate 8-bit components because
/// Slint has no built-in string-to-brush conversion.
fn label_to_pill(label: &Label) -> LabelPillData {
    let (r, g, b) = parse_hex_rgb(&label.color).unwrap_or((167, 175, 184));
    LabelPillData {
        id: SharedString::from(label.id.to_string()),
        name: SharedString::from(label.name.as_str()),
        color_r: r as i32,
        color_g: g as i32,
        color_b: b as i32,
    }
}

/// Parse `#rrggbb` → `(r, g, b)` 8-bit components. Returns None on any
/// parse error (malformed length, bad hex digits, etc.) so the caller
/// can fall back to a default colour.
fn parse_hex_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let s = hex.strip_prefix('#').unwrap_or(hex);
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r, g, b))
}

/// Polish 5: rebuild the `editor-highlighted-lines` VecModel from the
/// current buffer. Caps at `MAX_PREVIEW_LINES` so huge files don't
/// stall the UI. Called on open and on save.
const MAX_PREVIEW_LINES: usize = 500;

fn rebuild_editor_highlight(
    buffer: &EditorBuffer,
    model: &Rc<VecModel<HighlightedLineData>>,
) {
    while model.row_count() > 0 {
        model.remove(model.row_count() - 1);
    }
    let total = buffer.line_count();
    let render_count = total.min(MAX_PREVIEW_LINES);
    for i in 0..render_count {
        let spans = buffer.highlight_line(i);
        let spans_model = Rc::new(VecModel::<HlSpanData>::default());
        for s in spans {
            spans_model.push(HlSpanData {
                text: SharedString::from(s.text),
                color_r: s.r as i32,
                color_g: s.g as i32,
                color_b: s.b as i32,
                bold: s.bold,
                italic: s.italic,
            });
        }
        model.push(HighlightedLineData {
            line_no: (i + 1) as i32,
            spans: ModelRc::from(spans_model),
        });
    }
}

/// Heuristic: is this path likely a binary file the inline editor
/// should not try to load? Checks the extension against a short list;
/// returns true for common binary formats. False positives (e.g. a
/// user's `.dat` text file) are acceptable — they just fall back to
/// $EDITOR.
fn is_likely_binary(path: &std::path::Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "bmp"
            | "ico"
            | "pdf"
            | "zip"
            | "gz"
            | "bz2"
            | "xz"
            | "7z"
            | "tar"
            | "exe"
            | "dll"
            | "so"
            | "dylib"
            | "bin"
            | "ttf"
            | "otf"
            | "woff"
            | "woff2"
            | "wasm"
            | "mp3"
            | "mp4"
            | "mkv"
            | "mov"
            | "avi"
            | "flac"
            | "ogg"
            | "webm"
    )
}

/// Phase 4 seed tagging: auto-attach a colour label to each seed task
/// based on its TaskKind heuristic. Called once at startup, no-op if
/// any task already has at least one label (so the user's manual
/// tagging isn't overwritten on restart).
fn auto_tag_seed_tasks(state: &AppState) -> anyhow::Result<()> {
    let label_store = LabelStore::new(&state.db.conn);
    let labels = label_store.list_all()?;
    let name_to_label: HashMap<String, &Label> =
        labels.iter().map(|l| (l.name.clone(), l)).collect();

    let kind_to_label = |kind: TaskKind| -> Option<&Label> {
        let name = match kind {
            TaskKind::Bug => "Bug",
            TaskKind::Feature => "Feature",
            TaskKind::Enhancement => "Enhancement",
        };
        name_to_label.get(name).copied()
    };

    for task in state.list_tasks()? {
        // Skip if this task already has any labels.
        if !label_store.labels_for_task(task.id)?.is_empty() {
            continue;
        }
        let kind = TaskKind::from_title(&task.title);
        if let Some(label) = kind_to_label(kind) {
            label_store.attach(task.id, label.id)?;
        }
    }
    Ok(())
}

/// Human-friendly relative time for commit timestamps.
/// "just now" · "3m ago" · "2h ago" · "4d ago".
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
