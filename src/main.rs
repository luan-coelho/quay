//! Quay — cross-platform native workspace for orchestrating AI coding agent sessions.

mod agents;
mod app;
mod editor;
mod file_tree;
mod git;
mod hotkeys;
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
use slint::{ComponentHandle, Image, ModelRc, SharedString, Timer, TimerMode, VecModel};

use std::cell::RefCell;

use crate::app::AppState;
use crate::editor::EditorBuffer;
use crate::persistence::QuayDirs;
use crate::terminal::GlyphAtlas;
use crate::wiring::helpers::{
    auto_tag_seed_tasks, default_shell, format_cost, format_relative_time, format_runtime,
    format_tokens, home_directory, label_to_pill, replace_model,
};

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
        let label_store = state.label_store();
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

    // Sidebar: menu items. Icons are picked by `id` inside `MenuRow`
    // via the `lucide-slint` `IconSet`. The `glyph` field is kept on
    // the struct as an accessibility hint but is no longer rendered;
    // submenu rows pass the literal `"submenu"` in `shortcut` so the
    // slint side renders a Lucide chevron in that slot.
    let menu_model = Rc::new(VecModel::<MenuItemData>::default());
    for item in [
        ("new-task",      "",  "New CLI Session", "⌘N"),
        ("new-terminal",  "",  "New Terminal",    "⌘T"),
        ("quick-action",  "",  "Quick Action",    "submenu"),
        ("configure",     "",  "Configure",       ""),
        ("sessions",      "",  "Sessions",        ""),
        ("worktrees",     "",  "Worktrees",       ""),
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
        let project_store = state.project_store();
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

    // Polish 35: Cmd+P task quick switcher results model.
    let task_search_model = Rc::new(VecModel::<TaskCardData>::default());
    window.set_task_search_results(ModelRc::from(task_search_model.clone()));

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
        let label_store = state.label_store();
        let labels = label_store.list_all().unwrap_or_default();
        for l in labels {
            all_labels_model.push(label_to_pill(&l));
        }
    }

    // Polish 25: notification toast helper. Wrapped in `Rc<dyn Fn>`
    // so it can be cloned into the many callback closures that need
    // to surface errors. A monotonic generation counter cancels
    // older auto-dismiss timers when a newer toast lands.
    let toast_generation: Rc<std::cell::Cell<u32>> = Rc::new(std::cell::Cell::new(0));
    let show_toast: Rc<dyn Fn(&str, String)> = {
        let weak = window.as_weak();
        let gen_cell = toast_generation.clone();
        Rc::new(move |kind: &str, msg: String| {
            let Some(window) = weak.upgrade() else { return };
            let my_gen = gen_cell.get().wrapping_add(1);
            gen_cell.set(my_gen);
            window.set_toast_kind(SharedString::from(kind));
            window.set_toast_message(msg.into());
            window.set_toast_visible(true);
            // Schedule auto-dismiss after 3.2 s. We leak the Timer so
            // it survives this closure call — Slint's runtime cleans
            // up dropped timers itself, but the safest pattern here
            // is to keep one strong reference alive via mem::forget.
            let dismiss_weak = weak.clone();
            let dismiss_gen = gen_cell.clone();
            let timer = Box::new(Timer::default());
            timer.start(
                TimerMode::SingleShot,
                Duration::from_millis(3200),
                move || {
                    if dismiss_gen.get() == my_gen {
                        if let Some(w) = dismiss_weak.upgrade() {
                            w.set_toast_visible(false);
                        }
                    }
                },
            );
            // Outlive the closure call.
            Box::leak(timer);
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
    });
    let refresh_kanban: Rc<dyn Fn()> = {
        let state = state.clone();
        let weak = window.as_weak();
        let models = kanban_models.clone();
        Rc::new(move || {
            if let Some(window) = weak.upgrade() {
                crate::wiring::kanban_refresh::rebuild(&state, &window, &models);
            }
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
    // Phase 6 / 7 — file tree click + inline editor save/close.
    crate::wiring::editor_callbacks::wire(
        &window,
        &ctx,
        editor_buffer.clone(),
        editor_lines_model.clone(),
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
            },
        );
    }

    window.run()?;
    drop(poll_timer);
    drop(stats_refresh_timer);
    Ok(())
}
