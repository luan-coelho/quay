//! Task-centric callback wiring — create / select / move / edit / delete /
//! start_plan / start_implement / filter_changed / title-description-
//! instructions live persistence.
//!
//! Each `window.on_*` registration used to live inline in `main.rs`;
//! extracting them here keeps main() focused on setup and lets each
//! group share a single `&WiringContext` reference for the common
//! resources (`state`, refreshes, toast). Window-specific state
//! (getters/setters, tab navigation) is still touched via a cloned
//! weak handle so we don't create reference cycles.

use std::str::FromStr;

use slint::{ComponentHandle, Image, SharedString};
use uuid::Uuid;
use validator::Validate;

use crate::MainWindow;
use crate::kanban::{SessionState, StartMode, TaskKind};
use crate::wiring::context::WiringContext;
use crate::wiring::helpers::kind_to_str;
use crate::wiring::validation::{NewTaskForm, TaskTitleForm, first_errors};

pub fn wire(window: &MainWindow, ctx: &WiringContext) {
    wire_select(window, ctx);
    wire_create_submit(window, ctx);
    wire_move(window, ctx);
    wire_edit_fields(window, ctx);
    wire_start_session(window, ctx);
    wire_stop_session(window, ctx);
    wire_agent_changed(window, ctx);
    wire_filter_changed(window, ctx);
    wire_delete_task(window, ctx);
}

// NOTE: the detailed delete_task impl below mirrors the one inlined
// in main.rs before the extraction so behaviour (stats reset + info
// toast + refresh_active_panels) stays bit-for-bit identical.

/// Polish 16: clicking a card always pins the task into the open-tabs
/// strip and refreshes every panel the Description tab depends on.
fn wire_select(window: &MainWindow, ctx: &WiringContext) {
    let state = ctx.state.clone();
    let refresh = ctx.refresh_kanban.clone();
    let refresh_panels = ctx.refresh_active_panels.clone();
    let refresh_files = ctx.refresh_files.clone();
    let weak = window.as_weak();
    window.on_select_task(move |id| {
        let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
        // Polish 16: clicking a card always pins the task into the
        // open-tabs strip. Harmless if already pinned.
        state.pin_open_tab(uuid);
        match state.select_task(uuid) {
            // Only refresh the UI for the description/title when the
            // active task actually changed — otherwise a second click
            // on the same card would clobber in-progress Description
            // edits.
            Ok(changed) if !changed => {}
            Ok(_) => {
                if let Some(window) = weak.upgrade() {
                    // Phase 6 / Polish 17: if the task has a worktree,
                    // point the Files tab at its root and rebuild the
                    // flattened tree immediately so the user doesn't
                    // need a second click.
                    let task_opt = state.task_store().get(uuid);
                    if let Ok(Some(ref t)) = task_opt {
                        if let Some(wt) = t.worktree_path.as_deref() {
                            window.set_file_current_dir(
                                wt.to_string_lossy().into_owned().into(),
                            );
                        } else {
                            window.set_file_current_dir("".into());
                        }
                    }
                    refresh_files();

                    let card_data = if let Ok(Some(task)) = task_opt {
                        // Compute display-id by scanning all tasks
                        // ordered by creation date — same logic as
                        // refresh.
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
                            task.cli_selection.as_str().to_string(),
                        ))
                    } else {
                        None
                    };

                    let (display, title, description, instructions, sess_state, agent) =
                        card_data.unwrap_or_default();
                    window.set_active_task_id(id.clone());
                    window.set_active_task_display(display.into());
                    window.set_active_task_title(title.into());
                    window.set_active_task_description(description.into());
                    window.set_active_task_instructions(instructions.into());
                    window.set_active_task_session_state(sess_state.into());
                    window.set_active_task_agent(if agent.is_empty() { "claude".into() } else { agent.into() });
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
                // Polish 3: also rebuild the Description tab panels so
                // the label/dep sections reflect the newly active task
                // without waiting for a second event.
                refresh_panels();
            }
            Err(err) => tracing::error!(%err, "failed to select task"),
        }
    });
}

/// Legacy `on_create_task` path — Cmd/Ctrl+N shortcut that creates a
/// task with an auto-generated title — plus the modal-submit path.
fn wire_create_submit(window: &MainWindow, ctx: &WiringContext) {
    {
        let state = ctx.state.clone();
        let refresh = ctx.refresh_kanban.clone();
        let toast = ctx.show_toast.clone();
        let weak = window.as_weak();
        window.on_create_task(move || {
            let project_id = weak.upgrade().and_then(|w| {
                let id_str = w.get_active_project_id().to_string();
                Uuid::from_str(&id_str).ok()
            });
            let count = state.list_tasks().map(|t| t.len()).unwrap_or(0) + 1;
            let title = format!("New task {count}");
            match state.create_task(title.clone(), project_id) {
                Ok(_) => toast("success", format!("Created '{title}'")),
                Err(err) => {
                    tracing::error!(%err, "create_task failed");
                    toast("error", format!("Create failed: {err}"));
                }
            }
            refresh();
        });
    }
    {
        // Polish 10: user submitted the New Task modal. Insert using
        // the title + instructions they typed, then reset the form
        // fields, close the modal, and refresh the kanban.
        let state = ctx.state.clone();
        let weak = window.as_weak();
        let refresh = ctx.refresh_kanban.clone();
        let toast = ctx.show_toast.clone();
        window.on_submit_new_task(move || {
            let Some(w) = weak.upgrade() else { return };
            let title = w.get_new_task_title().to_string().trim().to_string();
            let instructions = w.get_new_task_instructions().to_string().trim().to_string();

            // Clear any prior inline error (the user will see the new
            // one below if validation fails again).
            w.set_new_task_title_error("".into());

            // Validate — inline errors instead of a toast.
            let form = NewTaskForm { title: title.clone() };
            if let Err(errs) = form.validate() {
                let map = first_errors(&errs);
                if let Some(msg) = map.get("title") {
                    w.set_new_task_title_error(msg.clone().into());
                }
                tracing::warn!("submit_new_task: validation failed");
                return;
            }

            let project_id = {
                let id_str = w.get_active_project_id().to_string();
                Uuid::from_str(&id_str).ok()
            };

            match state.create_task(title, project_id) {
                Ok(task) => {
                    if !instructions.is_empty() {
                        let store = state.task_store();
                        if let Ok(Some(mut t)) = store.get(task.id) {
                            t.instructions = Some(instructions);
                            t.updated_at = crate::kanban::unix_millis_now();
                            if let Err(err) = store.update(&t) {
                                tracing::warn!(%err, "set instructions on new task failed");
                                toast(
                                    "error",
                                    format!("Saved task, but instructions failed: {err}"),
                                );
                            }
                        }
                    }
                }
                Err(err) => {
                    tracing::error!(%err, "create_task via modal failed");
                    toast("error", format!("Create task failed: {err}"));
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
}

/// Move a task forward/back along the kanban workflow.
fn wire_move(window: &MainWindow, ctx: &WiringContext) {
    {
        let state = ctx.state.clone();
        let refresh = ctx.refresh_kanban.clone();
        let toast = ctx.show_toast.clone();
        window.on_move_task_forward(move |id| {
            let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
            if let Err(err) = state.move_forward(uuid) {
                tracing::error!(%err, "move_forward failed");
                // move_forward refuses to advance past Planning when a
                // dependency is unresolved — surfacing the error tells
                // the user *why* the card didn't move.
                toast("error", format!("Cannot move forward: {err}"));
            }
            refresh();
        });
    }
    {
        let state = ctx.state.clone();
        let refresh = ctx.refresh_kanban.clone();
        let toast = ctx.show_toast.clone();
        window.on_move_task_back(move |id| {
            let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
            if let Err(err) = state.move_backward(uuid) {
                tracing::error!(%err, "move_backward failed");
                toast("error", format!("Cannot move back: {err}"));
            }
            refresh();
        });
    }
}

/// Title / description / instructions — live persistence as the user
/// types. Uses `AppState::update_active_task` to cut per-callback
/// boilerplate and surface errors via toast.
fn wire_edit_fields(window: &MainWindow, ctx: &WiringContext) {
    {
        let state = ctx.state.clone();
        let refresh = ctx.refresh_kanban.clone();
        let toast = ctx.show_toast.clone();
        let weak = window.as_weak();
        window.on_title_changed(move |text| {
            let Some(w) = weak.upgrade() else { return };
            let trimmed = text.to_string();
            // Live validate via TaskTitleForm. On empty: surface an
            // inline error on the Description panel and do NOT persist.
            // On non-empty: clear the error and persist as before.
            let form = TaskTitleForm { title: trimmed.clone() };
            if let Err(errs) = form.validate() {
                let map = first_errors(&errs);
                if let Some(msg) = map.get("title") {
                    w.set_active_task_title_error(msg.clone().into());
                }
                return;
            }
            w.set_active_task_title_error("".into());
            // Title shows on the kanban card, so a successful update
            // has to refresh the column models.
            let result = state.update_active_task(|task| {
                if task.title != trimmed {
                    task.title = trimmed;
                }
            });
            match result {
                Ok(true) => refresh(),
                Ok(false) => {}
                Err(err) => {
                    tracing::error!(%err, "failed to update task title");
                    toast("error", format!("Failed to save title: {err}"));
                }
            }
        });
    }
    {
        let state = ctx.state.clone();
        let toast = ctx.show_toast.clone();
        window.on_description_changed(move |text| {
            // Persist the new description on the currently-active
            // task. Description doesn't show on the kanban card, so no
            // refresh needed.
            let new_value = if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            };
            if let Err(err) =
                state.update_active_task(|task| task.description = new_value)
            {
                tracing::error!(%err, "failed to update task description");
                toast("error", format!("Failed to save description: {err}"));
            }
        });
    }
    {
        // Instructions field mirrors description: persisted live on
        // every edited event. Empty strings are coerced to NULL in the
        // DB.
        let state = ctx.state.clone();
        let toast = ctx.show_toast.clone();
        window.on_instructions_changed(move |text| {
            let new_value = if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            };
            if let Err(err) =
                state.update_active_task(|task| task.instructions = new_value)
            {
                tracing::error!(%err, "failed to update task instructions");
                toast("error", format!("Failed to save instructions: {err}"));
            }
        });
    }
}

/// Plan / Implement buttons — spawn an agent session via the Strategy
/// pattern in `agents::`. The session state chip updates immediately
/// so the user sees "busy" without waiting for the next poll tick.
fn wire_start_session(window: &MainWindow, ctx: &WiringContext) {
    {
        let state = ctx.state.clone();
        let weak = window.as_weak();
        let refresh = ctx.refresh_kanban.clone();
        let toast = ctx.show_toast.clone();
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
                        window.set_active_right_tab(SharedString::from("terminal"));
                    }
                    toast("success", "Plan session started".to_string());
                    refresh();
                }
                Err(err) => {
                    tracing::error!(%err, "start_session(Plan) failed");
                    if let Some(window) = weak.upgrade() {
                        window.set_active_task_session_state(
                            SessionState::Error.as_str().into(),
                        );
                    }
                    toast("error", format!("Plan failed: {err}"));
                }
            }
        });
    }
    {
        // Implement button: same wiring as Plan but StartMode::Implement.
        let state = ctx.state.clone();
        let weak = window.as_weak();
        let refresh = ctx.refresh_kanban.clone();
        let toast = ctx.show_toast.clone();
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
                        window.set_active_right_tab(SharedString::from("terminal"));
                    }
                    toast("success", "Implement session started".to_string());
                    refresh();
                }
                Err(err) => {
                    tracing::error!(%err, "start_session(Implement) failed");
                    if let Some(window) = weak.upgrade() {
                        window.set_active_task_session_state(
                            SessionState::Error.as_str().into(),
                        );
                    }
                    toast("error", format!("Implement failed: {err}"));
                }
            }
        });
    }
}

/// Stop a running session. Sends SIGTERM to the child process and
/// updates the card state to "stopped".
fn wire_stop_session(window: &MainWindow, ctx: &WiringContext) {
    let state = ctx.state.clone();
    let refresh = ctx.refresh_kanban.clone();
    let toast = ctx.show_toast.clone();
    let weak = window.as_weak();
    window.on_stop_session(move |id| {
        let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
        match state.stop_session(uuid) {
            Ok(()) => {
                if let Some(window) = weak.upgrade() {
                    window.set_active_task_session_state("stopped".into());
                }
                toast("info", "Session stopped".to_string());
            }
            Err(err) => {
                tracing::error!(%err, "stop_session failed");
                toast("error", format!("Stop failed: {err}"));
            }
        }
        refresh();
    });
}

/// Agent picker — user changed the agent selection (Claude/OpenCode/Bare)
/// on the description panel. Persist to `task.cli_selection`.
fn wire_agent_changed(window: &MainWindow, ctx: &WiringContext) {
    let state = ctx.state.clone();
    window.on_agent_changed(move |agent_str| {
        let kind = crate::kanban::AgentKind::parse(agent_str.as_str())
            .unwrap_or(crate::kanban::AgentKind::Claude);
        if let Err(err) = state.update_active_task(|task| {
            task.cli_selection = kind;
        }) {
            tracing::warn!(%err, "agent_changed: update_active_task failed");
        }
    });
}

/// Phase 4 filter chip — user clicked a label filter (or "All").
/// `refresh_kanban` re-reads the current `filter-label-id` from the
/// window via its captured weak handle, so we don't need to pass the
/// new id explicitly.
fn wire_filter_changed(window: &MainWindow, ctx: &WiringContext) {
    let refresh = ctx.refresh_kanban.clone();
    window.on_filter_changed(move |_new_id| {
        refresh();
    });
}

/// Polish 16 — delete task. Drops the live session, deletes the DB
/// row (FKs cascade labels/deps), strips any open tab chip, resets
/// every right-pane surface back to empty-state, and fires both a
/// full kanban refresh and an active-panels rebuild.
fn wire_delete_task(window: &MainWindow, ctx: &WiringContext) {
    let state = ctx.state.clone();
    let weak = window.as_weak();
    let refresh = ctx.refresh_kanban.clone();
    let refresh_panels = ctx.refresh_active_panels.clone();
    let toast = ctx.show_toast.clone();
    window.on_delete_task(move |id| {
        let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
        // Drop the live session if one exists — PtySession::drop
        // flushes the log writer; the child becomes orphan.
        state.sessions.borrow_mut().remove(&uuid);
        if let Err(err) = state.task_store().delete(uuid) {
            tracing::warn!(%err, %uuid, "delete task failed");
            toast("error", format!("Delete failed: {err}"));
            return;
        }
        toast("info", "Task deleted".to_string());
        // Clear active task if we just deleted it.
        let mut active = state.active_task.borrow_mut();
        if *active == Some(uuid) {
            *active = None;
        }
        drop(active);
        // Polish 16 + 34: drop the deleted task from the open-tabs
        // strip and persist the new list to settings.
        {
            let mut tabs = state.open_tabs.borrow_mut();
            let before = tabs.len();
            tabs.retain(|t| *t != uuid);
            let changed = tabs.len() != before;
            drop(tabs);
            if changed {
                state.persist_open_tabs();
            }
        }
        // Reset UI panels to empty-state.
        if let Some(window) = weak.upgrade() {
            window.set_active_task_id(SharedString::from(""));
            window.set_active_task_display(SharedString::from(""));
            window.set_active_task_title(SharedString::from(""));
            window.set_active_task_description(SharedString::from(""));
            window.set_active_task_instructions(SharedString::from(""));
            window.set_active_task_session_state(SharedString::from("idle"));
            window.set_active_task_agent(SharedString::from("claude"));
            window.set_active_task_tokens_text(SharedString::from(""));
            window.set_active_task_cost_text(SharedString::from(""));
            window.set_active_task_message_count(0);
        }
        refresh_panels();
        refresh();
    });
}
