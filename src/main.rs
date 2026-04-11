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
    window.set_active_task_tokens_text("".into());
    window.set_active_task_cost_text("".into());
    window.set_active_task_runtime_text("".into());
    window.set_active_task_message_count(0);
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
    let active_task_available_deps_model = Rc::new(VecModel::<TaskCardData>::default());
    window.set_active_task_labels(ModelRc::from(active_task_labels_model.clone()));
    window
        .set_active_task_available_labels(ModelRc::from(active_task_available_labels_model.clone()));
    window.set_active_task_dependencies(ModelRc::from(active_task_deps_model.clone()));
    window.set_active_task_available_deps(ModelRc::from(active_task_available_deps_model.clone()));
    window.set_label_picker_open(false);
    window.set_dep_picker_open(false);

    // Polish 16: open-task tabs model for the right-pane tab strip.
    // One entry per pinned task — rebuilt on every `refresh_kanban`
    // so newly-renamed or state-changed tasks stay in sync.
    let open_tabs_model = Rc::new(VecModel::<OpenTaskTabData>::default());
    window.set_open_task_tabs(ModelRc::from(open_tabs_model.clone()));

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
        let open_tabs = open_tabs_model.clone();
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

            // Polish 9: resolve the active project filter. When set,
            // only tasks whose `project_id` matches survive. Tasks with
            // no project_id are hidden when any filter is active.
            let filter_project_id: Option<Uuid> = weak
                .upgrade()
                .and_then(|w| {
                    let s = w.get_active_project_id().to_string();
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

                // Project filter: skip tasks whose project_id doesn't
                // match the active project. Tasks with no project are
                // hidden whenever a project filter is active.
                if let Some(project_filter) = filter_project_id {
                    if task.project_id != Some(project_filter) {
                        continue;
                    }
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

            // Polish 13: update the status bar stats before the models
            // are replaced. We use the raw task list (pre-filter) for
            // the totals so the bar reflects the whole board even when
            // a label or project filter is active — the user wants to
            // know what they have, not just what's visible.
            if let Some(w) = weak.upgrade() {
                let mut counts = [0i32; 6];
                for t in &tasks {
                    let idx = match t.state {
                        TaskState::Backlog => 0,
                        TaskState::Planning => 1,
                        TaskState::Implementation => 2,
                        TaskState::Review => 3,
                        TaskState::Done => 4,
                        TaskState::Misc => 5,
                    };
                    counts[idx] += 1;
                }
                w.set_stats_backlog(counts[0]);
                w.set_stats_planning(counts[1]);
                w.set_stats_implementation(counts[2]);
                w.set_stats_review(counts[3]);
                w.set_stats_done(counts[4]);
                w.set_stats_misc(counts[5]);
                w.set_stats_total(tasks.len() as i32);
                w.set_stats_sessions(state.sessions.borrow().len() as i32);
            }

            replace_model(&backlog, backlog_v);
            replace_model(&planning, planning_v);
            replace_model(&implementation, implementation_v);
            replace_model(&review, review_v);
            replace_model(&done, done_v);
            replace_model(&misc, misc_v);

            // Polish 16: rebuild the open-task tabs model from the
            // AppState pinned list. Purge any tabs whose task was
            // deleted in the meantime so the UI never shows a stale
            // orphan chip. The list preserves the user's insertion
            // order — no re-sort.
            let tasks_by_id: HashMap<Uuid, &Task> =
                tasks.iter().map(|t| (t.id, t)).collect();
            let mut pinned = state.open_tabs.borrow_mut();
            pinned.retain(|id| tasks_by_id.contains_key(id));
            let snapshot: Vec<Uuid> = pinned.clone();
            drop(pinned);
            while open_tabs.row_count() > 0 {
                open_tabs.remove(open_tabs.row_count() - 1);
            }
            for id in &snapshot {
                if let Some(task) = tasks_by_id.get(id) {
                    let display_id = display_ids.get(id).copied().unwrap_or(0);
                    let kind = TaskKind::from_title(&task.title);
                    open_tabs.push(OpenTaskTabData {
                        id: SharedString::from(id.to_string()),
                        display_id,
                        title: SharedString::from(task.title.as_str()),
                        kind: SharedString::from(kind_to_str(kind)),
                        is_active: active_uuid == Some(*id),
                        session_state: SharedString::from(task.session_state.as_str()),
                    });
                }
            }
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
        let available_deps_model = active_task_available_deps_model.clone();
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
            while available_deps_model.row_count() > 0 {
                available_deps_model.remove(available_deps_model.row_count() - 1);
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
            let dep_id_set: std::collections::HashSet<Uuid> = dep_ids.iter().copied().collect();
            let all_tasks = state.list_tasks().unwrap_or_default();
            // Stable display-ids: same logic as refresh_kanban.
            let mut sorted = all_tasks.clone();
            sorted.sort_by_key(|t| t.created_at);
            let display_ids: HashMap<Uuid, i32> = sorted
                .iter()
                .enumerate()
                .map(|(i, t)| (t.id, (i + 1) as i32))
                .collect();
            for dep_id in &dep_ids {
                if let Some(task) = all_tasks.iter().find(|t| t.id == *dep_id) {
                    let display_id = display_ids.get(&task.id).copied().unwrap_or(0);
                    deps_model.push(task_to_card(task, display_id, false, false, Vec::new(), 0));
                }
            }

            // Polish 7: available dep candidates = every other task that
            // isn't already a direct prereq. Cycle detection happens at
            // insert time in DependencyStore::add, so the picker may
            // show candidates the store will later reject; that's fine
            // — we report the error and the user tries another.
            for task in &all_tasks {
                if task.id == active_id || dep_id_set.contains(&task.id) {
                    continue;
                }
                let display_id = display_ids.get(&task.id).copied().unwrap_or(0);
                available_deps_model
                    .push(task_to_card(task, display_id, false, false, Vec::new(), 0));
            }
        }
    };
    refresh_active_panels();

    // Polish 17 — hoisted ahead of select_task so that switching tasks
    // can rebuild the flattened tree against the new worktree root
    // without needing a second user click. The closure reads the
    // current root from the Slint `file-current-dir` property (which
    // select_task updates) and walks it via `file_tree::build_tree`
    // using `AppState::expanded_dirs`.
    let refresh_files = {
        let state = state.clone();
        let model = file_entries_model.clone();
        let weak = window.as_weak();
        move || {
            let root_str = match weak.upgrade() {
                Some(w) => w.get_file_current_dir().to_string(),
                None => return,
            };
            while model.row_count() > 0 {
                model.remove(model.row_count() - 1);
            }
            if root_str.is_empty() {
                return;
            }
            let root = PathBuf::from(&root_str);
            let expanded = state.expanded_dirs.borrow();
            let entries = match crate::file_tree::build_tree(&root, &expanded) {
                Ok(v) => v,
                Err(err) => {
                    tracing::debug!(%err, "build_tree failed");
                    Vec::new()
                }
            };
            for e in entries {
                let kind = match e.kind {
                    crate::file_tree::EntryKind::Directory => "directory",
                    crate::file_tree::EntryKind::File => "file",
                };
                model.push(FileEntryData {
                    name: SharedString::from(e.name),
                    path: SharedString::from(e.path.to_string_lossy().into_owned()),
                    kind: SharedString::from(kind),
                    depth: e.depth as i32,
                    expanded: e.expanded,
                });
            }
        }
    };

    // ── Callbacks ────────────────────────────────────────────────────────────
    {
        let state = state.clone();
        let weak = window.as_weak();
        let refresh = refresh_kanban.clone();
        let refresh_panels = refresh_active_panels.clone();
        let refresh_files_click = refresh_files.clone();
        window.on_select_task(move |id| {
            let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
            // Polish 16: clicking a card always pins the task into
            // the open-tabs strip. Harmless if already pinned.
            state.pin_open_tab(uuid);
            match state.select_task(uuid) {
                // Only refresh the UI for the description/title when the
                // active task actually changed — otherwise a second click on
                // the same card would clobber in-progress Description edits.
                Ok(changed) if !changed => {}
                Ok(_) => {
                    if let Some(window) = weak.upgrade() {
                        // Phase 6 / Polish 17: if the task has a
                        // worktree, point the Files tab at its root
                        // and rebuild the flattened tree immediately
                        // so the user doesn't need a second click.
                        let task_opt =
                            crate::kanban::TaskStore::new(&state.db.conn).get(uuid);
                        if let Ok(Some(ref t)) = task_opt {
                            if let Some(wt) = t.worktree_path.as_deref() {
                                window.set_file_current_dir(
                                    wt.to_string_lossy().into_owned().into(),
                                );
                            } else {
                                window.set_file_current_dir("".into());
                            }
                        }
                        refresh_files_click();

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
                        // Polish 15: clear old stats immediately so a
                        // stale chip row from the previous task doesn't
                        // flicker until the 2s timer fires.
                        window.set_active_task_tokens_text("".into());
                        window.set_active_task_cost_text("".into());
                        window.set_active_task_runtime_text("".into());
                        window.set_active_task_message_count(0);
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
        // Legacy `on_create_task` path — still wired for the
        // Cmd/Ctrl+N shortcut which creates a task immediately
        // without opening the modal. Auto-names so the user gets
        // a row instantly.
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
        // Polish 10: user submitted the New Task modal. Insert using
        // the title + instructions they typed, then reset the form
        // fields, close the modal, and refresh the kanban.
        let state = state.clone();
        let weak = window.as_weak();
        let refresh = refresh_kanban.clone();
        window.on_submit_new_task(move || {
            let Some(w) = weak.upgrade() else { return };
            let title = w.get_new_task_title().to_string().trim().to_string();
            let instructions = w.get_new_task_instructions().to_string().trim().to_string();

            if title.is_empty() {
                tracing::warn!("submit_new_task: title is required");
                return;
            }

            match state.create_task(title) {
                Ok(task) => {
                    if !instructions.is_empty() {
                        let store = crate::kanban::TaskStore::new(&state.db.conn);
                        if let Ok(Some(mut t)) = store.get(task.id) {
                            t.instructions = Some(instructions);
                            t.updated_at = crate::kanban::unix_millis_now();
                            if let Err(err) = store.update(&t) {
                                tracing::warn!(%err, "set instructions on new task failed");
                            }
                        }
                    }
                }
                Err(err) => {
                    tracing::error!(%err, "create_task via modal failed");
                    return;
                }
            }

            // Reset form + close modal.
            w.set_new_task_title("".into());
            w.set_new_task_instructions("".into());
            w.set_new_task_open(false);
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
        // Polish 7: add_dependency — validated via DependencyStore's
        // cycle detection. On cycle reject we log a warning and leave
        // the UI unchanged so the user can pick a different prereq.
        let state = state.clone();
        let refresh_panels = refresh_active_panels.clone();
        let refresh = refresh_kanban.clone();
        window.on_add_dependency(move |dep_id| {
            let Some(active_id) = *state.active_task.borrow() else { return };
            let Ok(dep_uuid) = Uuid::from_str(dep_id.as_str()) else { return };
            let store = DependencyStore::new(&state.db.conn);
            match store.add(active_id, dep_uuid) {
                Ok(()) => {
                    refresh_panels();
                    refresh();
                }
                Err(err) => {
                    tracing::warn!(%err, "add_dependency rejected");
                }
            }
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
        // Polish 9: click a project in the sidebar to filter the kanban
        // by project. Clicking the same project again clears the filter
        // (toggle behavior).
        let weak = window.as_weak();
        let refresh = refresh_kanban.clone();
        window.on_project_clicked(move |project_id| {
            let Some(w) = weak.upgrade() else { return };
            let current = w.get_active_project_id().to_string();
            if current == project_id.as_str() {
                w.set_active_project_id("".into());
            } else {
                w.set_active_project_id(project_id);
            }
            refresh();
        });
    }
    {
        // Polish 8: create a new project from the "New Project" modal.
        // Validates name + repo_path (both required, non-empty, repo
        // path must exist as a directory). On success, persists via
        // ProjectStore, refreshes the sidebar list, closes the modal,
        // and clears the fields.
        let state = state.clone();
        let weak = window.as_weak();
        let refresh_projects = refresh_projects.clone();
        window.on_create_project(move || {
            let Some(w) = weak.upgrade() else { return };
            let name = w.get_new_project_name().to_string();
            let repo_path_str = w.get_new_project_repo_path().to_string();
            let base_branch = w.get_new_project_base_branch().to_string();

            if name.trim().is_empty() || repo_path_str.trim().is_empty() {
                tracing::warn!("create_project: name and repo_path are required");
                return;
            }
            let repo_path = PathBuf::from(repo_path_str.trim());
            if !repo_path.is_absolute() {
                tracing::warn!("create_project: repo_path must be absolute");
                return;
            }
            let base = if base_branch.trim().is_empty() {
                "main".to_string()
            } else {
                base_branch.trim().to_string()
            };

            let project = crate::kanban::Project::new(
                name.trim(),
                repo_path,
                base,
            );
            if let Err(err) = crate::kanban::ProjectStore::new(&state.db.conn).insert(&project) {
                tracing::warn!(%err, "create_project insert failed");
                return;
            }

            // Reset form fields + close modal.
            w.set_new_project_name("".into());
            w.set_new_project_repo_path("".into());
            w.set_new_project_base_branch("main".into());
            w.set_new_project_open(false);

            refresh_projects();
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
            // Polish 16: also drop it from the open-tabs strip.
            state.open_tabs.borrow_mut().retain(|t| *t != uuid);
            // Reset UI panels to empty-state.
            if let Some(window) = weak.upgrade() {
                window.set_active_task_id("".into());
                window.set_active_task_display("".into());
                window.set_active_task_title("".into());
                window.set_active_task_description("".into());
                window.set_active_task_instructions("".into());
                window.set_active_task_session_state("idle".into());
                window.set_active_task_tokens_text("".into());
                window.set_active_task_cost_text("".into());
                window.set_active_task_runtime_text("".into());
                window.set_active_task_message_count(0);
            }
            refresh_panels();
            refresh();
        });
    }

    // Polish 16: open-task tab strip — focus a pinned tab by switching
    // the active task to it. Cheap because the underlying state lives
    // in AppState already; we just rebuild the right-pane surfaces the
    // same way on_select_task does.
    {
        let state = state.clone();
        let weak = window.as_weak();
        let refresh = refresh_kanban.clone();
        let refresh_panels = refresh_active_panels.clone();
        let refresh_files_for_tab = refresh_files.clone();
        window.on_open_task_tab(move |id| {
            let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
            match state.select_task(uuid) {
                Ok(changed) if !changed => {}
                Ok(_) => {
                    if let Some(window) = weak.upgrade() {
                        let store = crate::kanban::TaskStore::new(&state.db.conn);
                        let task_opt = store.get(uuid);
                        if let Ok(Some(ref t)) = task_opt {
                            if let Some(wt) = t.worktree_path.as_deref() {
                                window.set_file_current_dir(
                                    wt.to_string_lossy().into_owned().into(),
                                );
                            } else {
                                window.set_file_current_dir("".into());
                            }
                        }
                        refresh_files_for_tab();
                        let card_data = if let Ok(Some(task)) = task_opt {
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
                        window.set_active_task_tokens_text("".into());
                        window.set_active_task_cost_text("".into());
                        window.set_active_task_runtime_text("".into());
                        window.set_active_task_message_count(0);
                        if state.blit_active() {
                            window.set_frame(Image::from_rgba8_premultiplied(
                                state.framebuffer.borrow().buffer.clone(),
                            ));
                        }
                    }
                    refresh();
                    refresh_panels();
                }
                Err(err) => tracing::error!(%err, "on_open_task_tab select failed"),
            }
        });
    }

    // Polish 16: close an open tab. Removes it from AppState's pinned
    // list; if it was active, falls back to a neighbouring tab (or the
    // empty-state view if no tabs remain). The PTY session is kept
    // alive — closing a tab only hides it from the strip, it does not
    // kill the underlying process. That matches how Lanes handles it.
    {
        let state = state.clone();
        let weak = window.as_weak();
        let refresh = refresh_kanban.clone();
        let refresh_panels = refresh_active_panels.clone();
        window.on_close_task_tab(move |id| {
            let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
            let fallback = state.close_open_tab(uuid);
            if let Some(next) = fallback {
                // Fall through to the same select-and-rebuild path the
                // open callback uses so the right pane doesn't show
                // a stale terminal frame.
                match state.select_task(next) {
                    Ok(_) => {
                        if let Some(window) = weak.upgrade() {
                            let store = crate::kanban::TaskStore::new(&state.db.conn);
                            if let Ok(Some(task)) = store.get(next) {
                                let all = state.list_tasks().unwrap_or_default();
                                let mut sorted = all.clone();
                                sorted.sort_by_key(|t| t.created_at);
                                let display_id = sorted
                                    .iter()
                                    .position(|t| t.id == next)
                                    .map(|i| i + 1)
                                    .unwrap_or(0);
                                let kind = TaskKind::from_title(&task.title);
                                window.set_active_task_kind(kind_to_str(kind).into());
                                window.set_active_task_id(next.to_string().into());
                                window.set_active_task_display(
                                    format!("#{display_id}").into(),
                                );
                                window.set_active_task_title(task.title.clone().into());
                                window.set_active_task_description(
                                    task.description.clone().unwrap_or_default().into(),
                                );
                                window.set_active_task_instructions(
                                    task.instructions.clone().unwrap_or_default().into(),
                                );
                                window.set_active_task_session_state(
                                    task.session_state.as_str().into(),
                                );
                                window.set_active_task_tokens_text("".into());
                                window.set_active_task_cost_text("".into());
                                window.set_active_task_runtime_text("".into());
                                window.set_active_task_message_count(0);
                            }
                            if state.blit_active() {
                                window.set_frame(Image::from_rgba8_premultiplied(
                                    state.framebuffer.borrow().buffer.clone(),
                                ));
                            }
                        }
                    }
                    Err(err) => tracing::error!(%err, "fallback select_task failed"),
                }
            } else if state.active_task.borrow().is_none() {
                // No active task left at all — reset the detail panel.
                if let Some(window) = weak.upgrade() {
                    window.set_active_task_id("".into());
                    window.set_active_task_display("".into());
                    window.set_active_task_title("".into());
                    window.set_active_task_description("".into());
                    window.set_active_task_instructions("".into());
                    window.set_active_task_session_state("idle".into());
                    window.set_active_task_tokens_text("".into());
                    window.set_active_task_cost_text("".into());
                    window.set_active_task_runtime_text("".into());
                    window.set_active_task_message_count(0);
                }
            }
            refresh();
            refresh_panels();
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
    // ── Polish 11 Quick Actions CRUD ───────────────────────────────────
    {
        // Add a new quick action with sensible defaults. Position is set
        // to the bottom of the list so it picks up the next free Cmd+Alt
        // slot. Refreshes the Settings list so the new row appears.
        let state = state.clone();
        let refresh_qa = refresh_settings_qa.clone();
        window.on_add_quick_action(move || {
            let store = crate::quick_actions::QuickActionStore::new(&state.db.conn);
            let existing = store.list_all().unwrap_or_default();
            let position = existing.iter().map(|a| a.position).max().unwrap_or(-1) + 1;
            let action = crate::quick_actions::QuickAction::new(
                "New action",
                crate::quick_actions::QuickActionKind::Claude,
                "",
                crate::quick_actions::QuickActionCategory::General,
                position,
            );
            if let Err(err) = store.insert(&action) {
                tracing::warn!(%err, "add_quick_action insert failed");
                return;
            }
            refresh_qa();
        });
    }
    {
        let state = state.clone();
        let refresh_qa = refresh_settings_qa.clone();
        window.on_delete_quick_action(move |id| {
            let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
            if let Err(err) = crate::quick_actions::QuickActionStore::new(&state.db.conn).delete(uuid) {
                tracing::warn!(%err, "delete_quick_action failed");
                return;
            }
            refresh_qa();
        });
    }
    {
        // Inline edit from the Settings row: name / kind / body. Loads
        // the row, mutates the requested fields, and writes back. Fires
        // on every LineEdit keystroke so the DB stays in sync.
        let state = state.clone();
        window.on_update_quick_action(move |id, name, kind, body| {
            let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
            let store = crate::quick_actions::QuickActionStore::new(&state.db.conn);
            let Ok(list) = store.list_all() else { return };
            let Some(mut action) = list.into_iter().find(|a| a.id == uuid) else { return };
            action.name = name.to_string();
            action.body = body.to_string();
            action.kind = match kind.as_str() {
                "shell" => crate::quick_actions::QuickActionKind::Shell,
                _ => crate::quick_actions::QuickActionKind::Claude,
            };
            if let Err(err) = store.update(&action) {
                tracing::warn!(%err, "update_quick_action failed");
            }
            // NB: we don't refresh_qa here because the current
            // LineEdit is already showing the updated text. Refreshing
            // would replace the model mid-edit and lose focus.
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

    // ── Phase 6 / Polish 17 file browser wiring ─────────────────────────
    // (The `refresh_files` closure is hoisted earlier so select_task can
    //  invoke it too; this block only wires the click handler.)

    {
        // Click handler for file tree entries.
        // Polish 17:
        // - Directory → toggle its presence in `state.expanded_dirs`
        //   and rebuild the flattened tree model.
        // - File → try to open inline in the editor. If the file is
        //   binary or too large (>1 MB), fall back to $EDITOR.
        let refresh = refresh_files.clone();
        let state_for_click = state.clone();
        let weak = window.as_weak();
        let buffer = editor_buffer.clone();
        let editor_lines_clone = editor_lines_model.clone();
        window.on_file_entry_clicked(move |path_str, kind_str| {
            let path = PathBuf::from(path_str.as_str());
            match kind_str.as_str() {
                "directory" => {
                    let mut expanded = state_for_click.expanded_dirs.borrow_mut();
                    if expanded.contains(&path) {
                        expanded.remove(&path);
                    } else {
                        expanded.insert(path);
                    }
                    drop(expanded);
                    refresh();
                }
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
        // Polish 6 + 18 key dispatcher — checked in priority order. The
        // "primary" modifier (`cmd` on macOS, `ctrl` elsewhere) is
        // computed from `ctrl || meta` so both feel native.
        //
        // Shortcuts (all consume the key so it never reaches the PTY):
        //
        //   Cmd+Alt+1..9   → execute quick action at (digit - 1)
        //   Cmd+N          → create new task
        //   Cmd+D          → move active task forward to Done
        //   Cmd+,          → toggle settings modal
        //   Cmd+W          → close active open-task tab  (Polish 18)
        //   Cmd+Shift+]    → cycle to next open tab      (Polish 18)
        //   Cmd+Shift+[    → cycle to prev open tab      (Polish 18)
        //
        // Everything else falls through to the xterm byte encoder.
        let state = state.clone();
        let weak = window.as_weak();
        let refresh = refresh_kanban.clone();
        let refresh_panels = refresh_active_panels.clone();
        window.on_key_pressed(move |text, ctrl, alt, shift, meta| {
            let primary = ctrl || meta;

            // 1. Quick action shortcut: primary+Alt+digit.
            if primary && alt && text.len() == 1 {
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

            // 2. Escape — close any open modal. Polish 12.
            if text == "\u{001b}" {
                if let Some(w) = weak.upgrade() {
                    if w.get_settings_open() {
                        w.set_settings_open(false);
                        return;
                    }
                    if w.get_new_project_open() {
                        w.set_new_project_open(false);
                        return;
                    }
                    if w.get_new_task_open() {
                        w.set_new_task_open(false);
                        return;
                    }
                }
                // No modal open — fall through so Esc still reaches the
                // PTY (needed for vim/less/etc).
            }

            // 3. Global primary-modifier shortcuts.
            if primary && !alt && text.len() == 1 {
                match text.chars().next().unwrap_or('\0') {
                    'n' | 'N' => {
                        // Cmd/Ctrl+N — create new task at bottom of Backlog.
                        let count = state.list_tasks().map(|t| t.len()).unwrap_or(0) + 1;
                        let title = format!("New task {count}");
                        if let Err(err) = state.create_task(title) {
                            tracing::error!(%err, "create_task via shortcut failed");
                        }
                        refresh();
                        return;
                    }
                    'd' | 'D' => {
                        // Cmd/Ctrl+D — move active task forward (towards Done).
                        if let Some(active_id) = *state.active_task.borrow() {
                            if let Err(err) = state.move_forward(active_id) {
                                tracing::warn!(%err, "move_forward via shortcut failed");
                            }
                            refresh();
                            refresh_panels();
                        }
                        return;
                    }
                    ',' => {
                        // Cmd/Ctrl+, — toggle the settings modal.
                        if let Some(w) = weak.upgrade() {
                            let open = !w.get_settings_open();
                            w.set_settings_open(open);
                        }
                        return;
                    }
                    'w' | 'W' => {
                        // Polish 18: Cmd/Ctrl+W — close the active open-task
                        // tab. Re-uses the same close/fall-back logic as
                        // clicking the × on the chip by invoking the
                        // Slint-side `close-task-tab` callback directly.
                        if let Some(active_id) = *state.active_task.borrow() {
                            if let Some(w) = weak.upgrade() {
                                w.invoke_close_task_tab(active_id.to_string().into());
                            }
                        }
                        return;
                    }
                    _ => {}
                }
            }

            // Polish 18: Cmd+Shift+] / Cmd+Shift+[ → cycle forward /
            // backward through the open task tabs. Shift turns `[`
            // into `{` and `]` into `}` on most keyboards, so we
            // match the shifted glyph. Wraps at the ends.
            if primary && shift && text.len() == 1 {
                let c = text.chars().next().unwrap_or('\0');
                if c == '}' || c == ']' || c == '{' || c == '[' {
                    let forward = c == '}' || c == ']';
                    let tabs = state.open_tabs.borrow().clone();
                    if tabs.len() >= 2 {
                        let active = *state.active_task.borrow();
                        let current_idx = active
                            .and_then(|id| tabs.iter().position(|t| *t == id))
                            .unwrap_or(0);
                        let next_idx = if forward {
                            (current_idx + 1) % tabs.len()
                        } else {
                            (current_idx + tabs.len() - 1) % tabs.len()
                        };
                        let next_id = tabs[next_idx];
                        if let Some(w) = weak.upgrade() {
                            w.invoke_open_task_tab(next_id.to_string().into());
                        }
                    }
                    return;
                }
            }

            // 4. Fall-through: normal PTY input.
            let bytes = key_text_to_bytes(text.as_str(), ctrl, alt, shift);
            if !bytes.is_empty() {
                state.write_to_active(&bytes);
            }
        });
    }

    // PTY poll timer — drains bytes from all live sessions and blits the
    // active one. Fires ~60 Hz.
    //
    // Polish 22: also drives the window-wide braille spinner phase.
    // Every 6th tick (~96 ms) advances `spinner-glyph` through the
    // 8-glyph braille cycle. All Spinner instances on screen read
    // the same property and stay in lockstep — same convention every
    // CLI spinner (cargo, npm, rustup, …) follows.
    const SPINNER_GLYPHS: [&str; 8] = ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];
    let poll_timer = Timer::default();
    {
        let weak = window.as_weak();
        let state = state.clone();
        let spinner_idx = std::cell::Cell::new(0_usize);
        let spinner_tick = std::cell::Cell::new(0_u8);
        poll_timer.start(
            TimerMode::Repeated,
            Duration::from_millis(16),
            move || {
                let t = spinner_tick.get().wrapping_add(1);
                spinner_tick.set(t);
                if t % 6 == 0 {
                    let i = (spinner_idx.get() + 1) % SPINNER_GLYPHS.len();
                    spinner_idx.set(i);
                    if let Some(window) = weak.upgrade() {
                        window.set_spinner_glyph(SPINNER_GLYPHS[i].into());
                    }
                }
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
                        .map(|f| {
                            let lines_model = Rc::new(VecModel::<DiffLineData>::default());
                            for l in f.lines {
                                lines_model.push(DiffLineData {
                                    origin: SharedString::from(l.origin.to_string()),
                                    text: SharedString::from(l.text),
                                });
                            }
                            DiffFileData {
                                path: SharedString::from(f.path),
                                status: SharedString::from(f.status.to_string()),
                                additions: f.additions as i32,
                                deletions: f.deletions as i32,
                                lines: ModelRc::from(lines_model),
                            }
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

    // Polish 15 — agent metadata refresh timer. Fires every 2s; when the
    // active task has a captured `claude_session_id`, parse its JSONL
    // transcript with `agents::claude_stats::read_session_stats` and push
    // the totals into the four UI properties the chips render from.
    // When there's no active task or no session id yet, reset to the
    // empty display (message_count==0 hides the whole row).
    let stats_refresh_timer = Timer::default();
    {
        let weak = window.as_weak();
        let state = state.clone();
        stats_refresh_timer.start(
            TimerMode::Repeated,
            Duration::from_secs(2),
            move || {
                let Some(window) = weak.upgrade() else { return };
                let Some(active_id) = *state.active_task.borrow() else {
                    window.set_active_task_tokens_text("".into());
                    window.set_active_task_cost_text("".into());
                    window.set_active_task_runtime_text("".into());
                    window.set_active_task_message_count(0);
                    return;
                };
                let store = crate::kanban::TaskStore::new(&state.db.conn);
                let Ok(Some(task)) = store.get(active_id) else { return };
                let Some(session_id) = task.claude_session_id.as_deref() else {
                    window.set_active_task_tokens_text("".into());
                    window.set_active_task_cost_text("".into());
                    window.set_active_task_runtime_text("".into());
                    window.set_active_task_message_count(0);
                    return;
                };
                // Claude Code encodes the session cwd into the project
                // directory name — we pass the worktree path if we
                // created one, otherwise the original repo path.
                let cwd = task
                    .worktree_path
                    .as_deref()
                    .unwrap_or(task.repo_path.as_path());
                let Some(jsonl_path) =
                    crate::agents::claude_stats::resolve_session_path(cwd, session_id)
                else {
                    return;
                };
                let stats = match crate::agents::claude_stats::read_session_stats(&jsonl_path) {
                    Ok(s) => s,
                    Err(err) => {
                        tracing::debug!(%err, "read_session_stats failed");
                        return;
                    }
                };
                window.set_active_task_tokens_text(
                    format_tokens(stats.total_tokens()).into(),
                );
                window.set_active_task_cost_text(format_cost(stats.cost_cents).into());
                window
                    .set_active_task_runtime_text(format_runtime(stats.runtime_secs.unwrap_or(0)).into());
                window.set_active_task_message_count(stats.message_count as i32);
            },
        );
    }

    window.run()?;
    drop(poll_timer);
    drop(stats_refresh_timer);
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
        // Polish 22: drives the spinner-vs-static-dot decision in CardRow.
        session_state: SharedString::from(task.session_state.as_str()),
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

/// Polish 15 — compact token count: 1_234 → "1.2k tok", 3_450_000 → "3.4M tok".
/// The `◇` glyph sits in the chip to echo Claude Code's own transcript marker.
fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("◇ {:.1}M tok", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("◇ {:.1}k tok", n as f64 / 1_000.0)
    } else {
        format!("◇ {n} tok")
    }
}

/// Polish 15 — cost in cents → "$0.04" / "$12.35". Sub-cent totals
/// stay at "$0.00" to keep the chip width stable.
fn format_cost(cents: u64) -> String {
    let dollars = cents / 100;
    let remainder = cents % 100;
    format!("$ {dollars}.{remainder:02}")
}

/// Polish 15 — runtime in seconds → "42s" / "7m 12s" / "2h 03m".
fn format_runtime(secs: u64) -> String {
    if secs < 60 {
        format!("⏱ {secs}s")
    } else if secs < 3600 {
        let m = secs / 60;
        let s = secs % 60;
        format!("⏱ {m}m {s:02}s")
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("⏱ {h}h {m:02}m")
    }
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
