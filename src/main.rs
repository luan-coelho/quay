//! Quay — cross-platform native workspace for orchestrating AI coding agent sessions.

#[macro_use]
extern crate rust_i18n;
rust_i18n::i18n!("locales", fallback = "en");

mod agents;
mod app;
mod editor;
mod file_tree;
mod git;
mod hotkeys;
mod i18n;
mod kanban;
mod persistence;
mod process;
mod quick_actions;
mod settings;
mod terminal;
mod util;
mod wiring;

slint::include_modules!();

use std::rc::Rc;
use std::time::Duration;

use anyhow::Result;
use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

use std::cell::RefCell;

use crate::app::AppState;
use crate::editor::EditorBuffer;
use crate::persistence::QuayDirs;
use crate::terminal::GlyphAtlas;
use crate::wiring::helpers::{
    default_shell, format_cost, format_relative_time, format_runtime,
    format_tokens, home_directory, label_to_pill, replace_model,
};

const DEFAULT_COLS: usize = 96;
const DEFAULT_ROWS: usize = 28;
const FONT_SIZE: f32 = 14.0;

/// Build the Slint model for the active task's JSON session chat items.
fn build_chat_items_model(state: &AppState) -> ModelRc<ChatItemData> {
    use crate::agents::stream_json::ChatItem;

    let active = *state.active_task.borrow();
    let sessions = state.json_sessions.borrow();
    let items = active
        .and_then(|id| sessions.get(&id))
        .map(|sess| &sess.items[..])
        .unwrap_or(&[]);

    let slint_items: Vec<ChatItemData> = items
        .iter()
        .map(|item| match item {
            ChatItem::UserPrompt(text) => ChatItemData {
                kind: "user".into(),
                text: text.as_str().into(),
                tool_name: SharedString::default(),
                is_error: false,
            },
            ChatItem::AssistantText(text) => ChatItemData {
                kind: "text".into(),
                text: text.as_str().into(),
                tool_name: SharedString::default(),
                is_error: false,
            },
            ChatItem::ToolUse { name, input } => ChatItemData {
                kind: "tool-use".into(),
                text: input.as_str().into(),
                tool_name: name.as_str().into(),
                is_error: false,
            },
            ChatItem::ToolResult { output, is_error } => ChatItemData {
                kind: "tool-result".into(),
                text: output.as_str().into(),
                tool_name: "Result".into(),
                is_error: *is_error,
            },
            ChatItem::Status(text) => ChatItemData {
                kind: "status".into(),
                text: text.as_str().into(),
                tool_name: SharedString::default(),
                is_error: false,
            },
        })
        .collect();

    ModelRc::from(Rc::new(VecModel::from(slint_items)))
}

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
    // Phase 4: seed the Lanes preset label palette on first run.
    {
        let label_store = state.label_store();
        label_store.seed_presets_if_empty()?;
    }
    // Phase 5: seed the default Quick Actions + Settings on first run.
    {
        let qa_store = crate::quick_actions::QuickActionStore::new(&state.db.conn);
        qa_store.seed_defaults_if_empty()?;
        let settings_store = crate::settings::Settings::new(&state.db.conn);
        settings_store.seed_defaults_if_empty()?;
    }
    // i18n: initialise locale from user preference or system locale.
    {
        let settings_store = crate::settings::Settings::new(&state.db.conn);
        i18n::init_locale(&settings_store);
    }

    // Reset stale session states on startup. When the app closed,
    // any running PTY processes died, so tasks stuck in "busy" or
    // "awaiting" should be marked "exited" and have their PID cleared.
    {
        let reset_count = state.reset_stale_sessions();
        if reset_count > 0 {
            tracing::info!(count = reset_count, "reset stale session states to exited");
        }
    }

    // Prune stale worktree metadata on startup. Best-effort: if `git`
    // is not installed or a repo_path is invalid, we log and continue.
    {
        let projects = state.project_store().list_all().unwrap_or_default();
        if let Ok(mgr) = crate::git::worktree::WorktreeManager::detect() {
            for project in &projects {
                if crate::app::is_git_repo(&project.repo_path)
                    && let Err(err) = mgr.prune(&project.repo_path)
                {
                    tracing::warn!(
                        repo = %project.repo_path.display(),
                        %err,
                        "worktree prune failed"
                    );
                }
            }
        }
    }

    // Restore the active project filter from persisted settings so the
    // user's project context survives restarts.
    {
        let settings = crate::settings::Settings::new(&state.db.conn);
        let persisted_project = settings
            .get(crate::settings::KEY_ACTIVE_PROJECT)
            .ok()
            .flatten()
            .unwrap_or_default();
        if !persisted_project.is_empty() {
            // Validate the project still exists before restoring.
            let project_exists = uuid::Uuid::parse_str(&persisted_project)
                .ok()
                .and_then(|id| state.project_store().get(id).ok().flatten())
                .is_some();
            if project_exists {
                window.set_active_project_id(SharedString::from(persisted_project.as_str()));
                tracing::info!(project_id = %persisted_project, "restored active project filter");
            } else {
                // Stale project id — clear it.
                let _ = settings.set(crate::settings::KEY_ACTIVE_PROJECT, "");
                tracing::info!("cleared stale active project filter");
            }
        }
    }

    // Initial blank framebuffer.
    window.set_frame(Image::from_rgba8_premultiplied(
        state.framebuffer.borrow().buffer.clone(),
    ));
    window.set_active_task_id("".into());
    window.set_active_task_display("".into());
    window.set_active_task_title("".into());
    window.set_active_task_description("".into());
    window.set_active_task_session_state("idle".into());
    window.set_active_task_tokens_text("".into());
    window.set_active_task_cost_text("".into());
    window.set_active_task_runtime_text("".into());
    window.set_active_task_message_count(0);
    window.set_active_right_tab("terminal".into());

    // Sidebar: menu items. Icons are picked by `id` inside `MenuRow`
    // via the `lucide-slint` `IconSet`. The `glyph` field is kept on
    // the struct as an accessibility hint but is no longer rendered;
    // submenu rows pass the literal `"submenu"` in `shortcut` so the
    // slint side renders a Lucide chevron in that slot.
    let menu_model = Rc::new(VecModel::<MenuItemData>::default());
    for (id, glyph, label, shortcut) in [
        ("new-task",      "",  t!("menu.new_cli_session").to_string(), "".to_string()),
        ("configure",     "",  t!("menu.configure").to_string(),       "".to_string()),
    ] {
        menu_model.push(MenuItemData {
            id: id.into(),
            glyph: glyph.into(),
            label: SharedString::from(label),
            shortcut: SharedString::from(shortcut),
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
        let project_store = state.project_store();
        if project_store.list_all().map(|v| v.is_empty()).unwrap_or(false) {
            let _ = project_store.insert(&crate::kanban::Project::new(
                t!("projects.default_home").to_string(),
                state.default_cwd.clone(),
                "main",
            ));
        }
    }
    let projects_model = Rc::new(VecModel::<ProjectData>::default());
    window.set_projects(ModelRc::from(projects_model.clone()));

    // Helper closure for rebuilding the sidebar project list. The
    // actual rebuild lives in `wiring::refreshes::rebuild_projects`;
    // this closure exists only so callbacks can hold a clone-friendly
    // `Rc<dyn Fn()>` to invoke later.
    let refresh_projects: Rc<dyn Fn()> = {
        let state = state.clone();
        let model = projects_model.clone();
        Rc::new(move || crate::wiring::refreshes::rebuild_projects(&state, &model))
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

    // Phase D: per-session status dots for the status bar.
    let session_dots_model = Rc::new(VecModel::<SessionDotData>::default());
    window.set_session_dots(ModelRc::from(session_dots_model.clone()));

    // Phase D: session history model for the History tab.
    let session_history_model = Rc::new(VecModel::<SessionEntryData>::default());
    window.set_session_history(ModelRc::from(session_history_model.clone()));

    // Phase D: worktree entries for the sidebar.
    let worktree_entries_model = Rc::new(VecModel::<WorktreeEntryData>::default());
    window.set_worktree_entries(ModelRc::from(worktree_entries_model.clone()));

    // Polish 35: Cmd+P task quick switcher results model.
    let task_search_model = Rc::new(VecModel::<TaskCardData>::default());
    window.set_task_search_results(ModelRc::from(task_search_model.clone()));

    // Files tab state. Populated on select_task if the task has a
    // worktree; empty otherwise.
    let file_entries_model = Rc::new(VecModel::<FileEntryData>::default());
    window.set_file_entries(ModelRc::from(file_entries_model.clone()));
    window.set_file_current_dir("".into());

    // Read-only file viewer state. A single shared `EditorBuffer` is
    // opened on demand when the user clicks a text file in the Files tab.
    // The buffer is used only for syntax detection + highlighting, not
    // for editing.
    let viewer_buffer: Rc<RefCell<Option<EditorBuffer>>> = Rc::new(RefCell::new(None));
    window.set_viewer_open(false);
    window.set_viewer_file_path("".into());
    window.set_viewer_syntax_name("Plain Text".into());
    let viewer_lines_model = Rc::new(VecModel::<HighlightedLineData>::default());
    window.set_viewer_highlighted_lines(ModelRc::from(viewer_lines_model.clone()));
    window.set_viewer_line_count(0);

    // Settings modal state — initial values loaded from SQLite.
    let settings_qa_model = Rc::new(VecModel::<QuickActionRowData>::default());
    let settings_proc_model = Rc::new(VecModel::<ProcessRowData>::default());
    window.set_settings_quick_actions(ModelRc::from(settings_qa_model.clone()));
    window.set_settings_processes(ModelRc::from(settings_proc_model.clone()));
    // Settings is a dedicated page (active-page == "settings"), not a right-pane tab.
    {
        let settings = crate::settings::Settings::new(&state.db.conn);
        let base_branch = settings.get_or(crate::settings::KEY_DEFAULT_BASE_BRANCH, "main");
        window.set_settings_default_base_branch(base_branch.into());
        let perm_mode = settings.get_or(crate::settings::KEY_PERMISSION_MODE, "acceptEdits");
        window.set_current_permission_mode(perm_mode.into());
    }
    {
        let label_store = state.label_store();
        let labels = label_store.list_all().unwrap_or_default();
        for l in labels {
            all_labels_model.push(label_to_pill(&l));
        }
    }

    // Polish 25: stacking toast notifications (Sonner-style). Each
    // toast is pushed into a VecModel<ToastData>, rendered by a `for`
    // loop in Slint. Individual timers handle auto-dismiss: first
    // setting `alive = false` (fade-out animation), then removing the
    // entry from the model after the animation completes. Max 5
    // simultaneous toasts — oldest evicted when the cap is reached.
    let toast_model: Rc<VecModel<ToastData>> = Rc::new(VecModel::default());
    window.set_toast_items(ModelRc::from(toast_model.clone()));
    let toast_counter: Rc<std::cell::Cell<i32>> = Rc::new(std::cell::Cell::new(0));
    let show_toast: Rc<crate::wiring::context::ToastFn> = {
        let model = toast_model.clone();
        let counter = toast_counter.clone();
        Rc::new(move |kind: &str, msg: String| {
            let id = counter.get().wrapping_add(1);
            counter.set(id);
            // Cap at 5 visible toasts — evict oldest (index 0).
            const MAX_TOASTS: usize = 5;
            while model.row_count() >= MAX_TOASTS {
                model.remove(0);
            }
            model.push(ToastData {
                id,
                message: msg.into(),
                kind: SharedString::from(kind),
                alive: true,
            });
            // Auto-dismiss: after 3.2 s set alive=false (fade-out),
            // then remove from model after 250 ms (animation duration).
            let fade_model = model.clone();
            let fade_timer = Box::new(Timer::default());
            fade_timer.start(
                TimerMode::SingleShot,
                Duration::from_millis(5000),
                move || {
                    // Find this toast by id and set alive = false.
                    for i in 0..fade_model.row_count() {
                        if let Some(mut item) = fade_model.row_data(i)
                            && item.id == id
                        {
                            item.alive = false;
                            fade_model.set_row_data(i, item);
                            // Schedule actual removal after fade-out.
                            let remove_model = fade_model.clone();
                            let remove_timer = Box::new(Timer::default());
                            remove_timer.start(
                                TimerMode::SingleShot,
                                Duration::from_millis(250),
                                move || {
                                    for j in 0..remove_model.row_count() {
                                        if let Some(entry) = remove_model.row_data(j)
                                            && entry.id == id
                                        {
                                            remove_model.remove(j);
                                            break;
                                        }
                                    }
                                },
                            );
                            Box::leak(remove_timer);
                            break;
                        }
                    }
                },
            );
            Box::leak(fade_timer);
        })
    };

    // Refresh closure: re-query DB and rebuild every column model.
    //
    // Reads the current `filter-label-id` from the window — if set, only
    // tasks carrying that label survive the filter.
    // Bundle the seven kanban-related models into a single struct so the
    // rebuild function in `wiring::kanban_refresh` only needs one
    // reference. The closure below upgrades the weak window handle on
    // every fire and delegates the actual rebuild logic.
    let kanban_models = Rc::new(crate::wiring::kanban_refresh::KanbanModels {
        backlog: backlog_model.clone(),
        planning: planning_model.clone(),
        implementation: implementation_model.clone(),
        review: review_model.clone(),
        done: done_model.clone(),
        misc: misc_model.clone(),
        open_tabs: open_tabs_model.clone(),
        session_dots: session_dots_model.clone(),
    });
    let refresh_kanban: Rc<dyn Fn()> = {
        let state = state.clone();
        let weak = window.as_weak();
        let models = kanban_models.clone();
        let wt_model = worktree_entries_model.clone();
        Rc::new(move || {
            if let Some(window) = weak.upgrade() {
                crate::wiring::kanban_refresh::rebuild(&state, &window, &models);
            }
            crate::wiring::refreshes::rebuild_worktrees(&state, &wt_model);
        })
    };
    refresh_kanban();

    // Polish 3: helper that rebuilds the Description tab's per-task
    // labels / available-labels / dependencies panels. Called from
    // select_task and from each attach/detach/remove-dep callback so
    // the UI stays in sync without a full kanban refresh.
    let active_panel_models = Rc::new(crate::wiring::refreshes::ActivePanelModels {
        labels: active_task_labels_model.clone(),
        available_labels: active_task_available_labels_model.clone(),
        deps: active_task_deps_model.clone(),
        available_deps: active_task_available_deps_model.clone(),
        session_history: session_history_model.clone(),
    });
    let refresh_active_panels: Rc<dyn Fn()> = {
        let state = state.clone();
        let models = active_panel_models.clone();
        Rc::new(move || crate::wiring::refreshes::rebuild_active_panels(&state, &models))
    };
    refresh_active_panels();

    // Polish 17 — hoisted ahead of select_task so that switching tasks
    // can rebuild the flattened tree against the new worktree root
    // without needing a second user click. The actual rebuild lives in
    // `wiring::refreshes::rebuild_files`; the closure here just upgrades
    // the weak window handle and delegates.
    let refresh_files: Rc<dyn Fn()> = {
        let state = state.clone();
        let model = file_entries_model.clone();
        let weak = window.as_weak();
        Rc::new(move || {
            if let Some(window) = weak.upgrade() {
                crate::wiring::refreshes::rebuild_files(&state, &window, &model);
            }
        })
    };

    // Helper: rebuild the Settings quick action list from the DB.
    // Hoisted ahead of callback wiring so the WiringContext below can
    // carry it without reordering.
    let refresh_settings_qa: Rc<dyn Fn()> = {
        let state = state.clone();
        let model = settings_qa_model.clone();
        Rc::new(move || crate::wiring::refreshes::rebuild_settings_qa(&state, &model))
    };

    // Helper: rebuild the Settings process list. Polish 1: the tracked
    // set comes from `AppState::tracked_pids()`, so our spawned agent
    // children are classified as "Tracked" instead of "Orphans".
    let refresh_settings_processes: Rc<dyn Fn()> = {
        let state = state.clone();
        let model = settings_proc_model.clone();
        Rc::new(move || crate::wiring::refreshes::rebuild_settings_processes(&state, &model))
    };

    // ── Wiring context ───────────────────────────────────────────────────
    // Bundles every `Rc`-shared resource so the `wire_*` helpers in
    // `src/wiring/*_callbacks.rs` can accept a single reference.
    let ctx = crate::wiring::context::WiringContext {
        state: state.clone(),
        refresh_kanban: refresh_kanban.clone(),
        refresh_projects: refresh_projects.clone(),
        refresh_active_panels: refresh_active_panels.clone(),
        refresh_files: refresh_files.clone(),
        refresh_settings_qa: refresh_settings_qa.clone(),
        refresh_settings_processes: refresh_settings_processes.clone(),
        show_toast: show_toast.clone(),
    };

    // ── Callbacks ────────────────────────────────────────────────────────────
    // Task-centric callbacks: select / create / move / edit / start_session /
    // filter / delete — extracted to `wiring::task_callbacks`.
    crate::wiring::task_callbacks::wire(&window, &ctx);
    // Label / dependency attach+detach — Polish 3 + 7.
    crate::wiring::label_dep_callbacks::wire(&window, &ctx);
    // Sidebar project filter + New Project modal — Polish 8 + 9.
    crate::wiring::project_callbacks::wire(&window, &ctx);
    // Open-task tabs, bulk tab management, Cmd+P task search — Polish
    // 16 + 35 + 41.
    crate::wiring::tab_callbacks::wire(&window, &ctx, task_search_model.clone());
    // Phase 5 Settings modal: toggle, Quick Actions CRUD, base branch,
    // Process Manager.
    crate::wiring::settings_callbacks::wire(&window, &ctx);
    // File tree click + read-only viewer open/close + keyboard nav.
    crate::wiring::editor_callbacks::wire(
        &window,
        &ctx,
        viewer_buffer.clone(),
        viewer_lines_model.clone(),
        file_entries_model.clone(),
    );
    // Polish 6 + 18 key dispatcher — delegates to `classify_hotkey`.
    crate::wiring::hotkey_callbacks::wire(&window, &ctx);

    // ── Polish 3 label/dep attach+detach wiring (Polish 36 toasts) ──────
    // Polish 16: open-task tab strip — focus a pinned tab by switching
    // the active task to it. Cheap because the underlying state lives
    // in AppState already; we just rebuild the right-pane surfaces the
    // same way on_select_task does.
    // Polish 16: close an open tab. Removes it from AppState's pinned
    // list; if it was active, falls back to a neighbouring tab (or the
    // empty-state view if no tabs remain). The PTY session is kept
    // alive — closing a tab only hides it from the strip, it does not
    // kill the underlying process. That matches how Lanes handles it.
    // Polish 41: bulk tab management. Three callbacks — close
    // others, close all, close right-of. Each delegates to the
    // matching AppState method (which handles persistence + active
    // task fallback) then triggers the standard select-and-rebuild
    // path via `invoke_open_task_tab` so the right pane reflects the
    // new active tab without ad-hoc duplication of the rebuild
    // logic. The toast helper surfaces what just happened.
    // Polish 35: task quick switcher — rebuild the filtered results
    // model on every keystroke. Substring case-insensitive match
    // against `#NN` display id and `title`. Empty query shows all
    // tasks (most recently updated first), capped at 50 for sanity.
    // Polish 35: clicking a result fires open-task-tab via the
    // existing pinned-tabs callback so the selected task pops into
    // the right pane the same way clicking a kanban card would.
    // Phase D — clicking a session dot in the status bar switches to that task.
    {
        let weak = window.as_weak();
        window.on_session_dot_clicked(move |task_id| {
            if let Some(w) = weak.upgrade() {
                w.invoke_open_task_tab(task_id);
            }
        });
    }
    // Phase D — clicking a worktree entry in the sidebar switches to
    // the associated task (same pattern as session dot clicks).
    {
        let weak = window.as_weak();
        window.on_worktree_clicked(move |task_id| {
            if let Some(w) = weak.upgrade() {
                w.invoke_open_task_tab(task_id);
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
        let refresh = refresh_kanban.clone();
        let toast = show_toast.clone();
        let spinner_idx = std::cell::Cell::new(0_usize);
        let spinner_tick = std::cell::Cell::new(0_u8);
        // Check for exited sessions and resize once per second (every
        // ~60 ticks), not on every 16ms frame, to avoid unnecessary
        // DB writes and resize churn.
        let exit_check_tick = std::cell::Cell::new(0_u8);
        let prev_cols = std::cell::Cell::new(DEFAULT_COLS as i32);
        let prev_rows = std::cell::Cell::new(DEFAULT_ROWS as i32);
        poll_timer.start(
            TimerMode::Repeated,
            Duration::from_millis(16),
            move || {
                let t = spinner_tick.get().wrapping_add(1);
                spinner_tick.set(t);
                if t.is_multiple_of(6) {
                    let i = (spinner_idx.get() + 1) % SPINNER_GLYPHS.len();
                    spinner_idx.set(i);
                    if let Some(window) = weak.upgrade() {
                        window.set_spinner_glyph(SPINNER_GLYPHS[i].into());
                    }
                }
                // Sync active_tab_is_bare from the Slint UI state.
                // Detect tab changes to force a re-blit so the correct
                // session (agent vs. bare) renders without waiting for
                // new PTY bytes.
                let mut tab_changed = false;
                if let Some(w) = weak.upgrade() {
                    let is_bare = w.get_active_right_tab() == "bare-terminal";
                    let was_bare = state.active_tab_is_bare.get();
                    if is_bare != was_bare {
                        state.active_tab_is_bare.set(is_bare);
                        tab_changed = true;
                    }
                }
                let polled = state.poll_all_sessions();
                if (polled || tab_changed) && state.blit_active()
                    && let Some(window) = weak.upgrade()
                {
                    window.set_frame(Image::from_rgba8_premultiplied(
                        state.framebuffer.borrow().buffer.clone(),
                    ));
                }

                // Poll JSON streaming sessions (Claude Code non-PTY).
                let json_changed = state.poll_all_json_sessions();
                if json_changed {
                    if let Some(window) = weak.upgrade() {
                        // Rebuild the chat items model for the active task.
                        let is_chat = state.active_has_json_session();
                        window.set_is_chat_session(is_chat);
                        if is_chat {
                            let items = build_chat_items_model(&state);
                            window.set_chat_items(items);
                            // Sync session state.
                            if let Some(active_id) = *state.active_task.borrow() {
                                let sessions = state.json_sessions.borrow();
                                if let Some(sess) = sessions.get(&active_id) {
                                    window.set_active_task_session_state(
                                        sess.state.as_str().into(),
                                    );
                                }
                            }
                        }
                    }
                    refresh();
                }

                // Session exit detection + resize — check once per second.
                let et = exit_check_tick.get().wrapping_add(1);
                exit_check_tick.set(et);
                if et.is_multiple_of(60) {
                    let exited = state.check_exited_sessions();
                    if !exited.is_empty() {
                        for (_, title) in &exited {
                            toast("info", t!("sessions.finished", title = title.as_str()).to_string());
                        }
                        // Update the active task's session state on the
                        // window if it was one of the exited ones.
                        if let Some(window) = weak.upgrade() {
                            let active_id_str = window.get_active_task_id().to_string();
                            for (id, _) in &exited {
                                if id.to_string() == active_id_str {
                                    window.set_active_task_session_state("exited".into());
                                }
                            }
                        }
                        refresh();
                    }

                    // Session state detection — inspect terminal output
                    // for patterns indicating the agent is awaiting input.
                    let state_changes = state.detect_session_states();
                    if !state_changes.is_empty() {
                        if let Some(window) = weak.upgrade() {
                            let active_id_str = window.get_active_task_id().to_string();
                            for (id, new_state) in &state_changes {
                                if id.to_string() == active_id_str {
                                    window.set_active_task_session_state(
                                        new_state.as_str().into(),
                                    );
                                }
                            }
                        }
                        refresh();
                    }

                    // PTY resize detection — Slint computes cols/rows
                    // from available right-pane space. When they change
                    // (window resized), resize all PTY sessions + rebuild
                    // the framebuffer.
                    if let Some(window) = weak.upgrade() {
                        let c = window.get_cols();
                        let r = window.get_rows();
                        if c != prev_cols.get() || r != prev_rows.get() {
                            prev_cols.set(c);
                            prev_rows.set(r);
                            state.resize_all_sessions(c as usize, r as usize);
                            // Re-blit with new dimensions.
                            if state.blit_active() {
                                window.set_frame(Image::from_rgba8_premultiplied(
                                    state.framebuffer.borrow().buffer.clone(),
                                ));
                            }
                        }
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
                let store = state.task_store();
                let Ok(Some(task)) = store.get(active_id) else { return };
                let Some(worktree_path) = task.worktree_path.as_deref() else {
                    // Task has no worktree — clear the models.
                    replace_model(&diff_model, Vec::new());
                    replace_model(&log_model, Vec::new());
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

                replace_model(&diff_model, diff_files);
                replace_model(&log_model, commits);
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
        let refresh_kanban = refresh_kanban.clone();
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
                let store = state.task_store();
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

                // Auto-rename: if the task still has the auto-generated
                // title, read the first user prompt from the JSONL and
                // use it as the new title.
                let is_auto_title = task.title.starts_with("New task ")
                    || task.title.starts_with("Nova tarefa ")
                    || task.title.starts_with("Terminal ");
                if is_auto_title
                    && let Some(prompt) = crate::agents::claude_stats::read_first_prompt(&jsonl_path, 80)
                {
                    let store = state.task_store();
                    if let Ok(Some(mut t)) = store.get(active_id) {
                        t.title = prompt.clone();
                        t.updated_at = crate::kanban::unix_millis_now();
                        let _ = store.update(&t);
                        window.set_active_task_title(prompt.into());
                        refresh_kanban();
                    }
                }
            },
        );
    }

    window.run()?;
    drop(poll_timer);
    drop(stats_refresh_timer);
    Ok(())
}
