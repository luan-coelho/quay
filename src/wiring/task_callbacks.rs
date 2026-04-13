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
use crate::wiring::validation::{TaskTitleForm, first_errors};

pub fn wire(window: &MainWindow, ctx: &WiringContext) {
    wire_select(window, ctx);
    wire_create_submit(window, ctx);
    wire_bare_terminal(window, ctx);
    wire_edit_fields(window, ctx);
    wire_filter_changed(window, ctx);
    wire_delete_task(window, ctx);
    wire_card_context_menu(window, ctx);
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

                    let (display, title, description, _instructions, sess_state, _agent) =
                        card_data.unwrap_or_default();
                    window.set_active_task_id(id.clone());
                    window.set_active_task_display(display.into());
                    window.set_active_task_title(title.into());
                    window.set_active_task_description(description.into());
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
                // Polish 3: also rebuild the Description tab panels so
                // the label/dep sections reflect the newly active task
                // without waiting for a second event.
                refresh_panels();
            }
            Err(err) => tracing::error!(%err, "failed to select task"),
        }
    });
}

/// Terminal-first: `on_create_task` creates a task and immediately
/// starts a Claude Code session. Called from sidebar "New CLI Session"
/// and can be reused by any UI path. The old modal-submit path has
/// been removed — tasks are created directly from the terminal.
fn wire_create_submit(window: &MainWindow, ctx: &WiringContext) {
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
        let title = t!("tasks.new_task_title", count = count).to_string();
        match state.create_task(title, project_id) {
            Ok(task) => {
                if let Err(err) = state.start_session(
                    task.id,
                    StartMode::Implement,
                ) {
                    tracing::error!(err = ?err, "cli session start failed");
                    toast("error", t!("sessions.implement_failed", err = err.to_string()).to_string());
                } else {
                    state.pin_open_tab(task.id);
                    let _ = state.select_task(task.id);
                    if let Some(w) = weak.upgrade() {
                        w.set_active_task_id(task.id.to_string().into());
                        w.set_active_task_session_state(SessionState::Busy.as_str().into());
                        w.set_active_right_tab(SharedString::from("terminal"));
                        if state.blit_active() {
                            w.set_frame(Image::from_rgba8_premultiplied(
                                state.framebuffer.borrow().buffer.clone(),
                            ));
                        }
                    }
                }
            }
            Err(err) => {
                tracing::error!(%err, "create_task failed");
                toast("error", t!("tasks.create_failed", err = err.to_string()).to_string());
            }
        }
        refresh();
    });
}


/// Lazily spawn a bare terminal for the task when the user clicks
/// the "Terminal" tab. Sets `active_tab_is_bare` so the poll/blit
/// loop renders the correct PTY.
fn wire_bare_terminal(window: &MainWindow, ctx: &WiringContext) {
    let state = ctx.state.clone();
    let toast = ctx.show_toast.clone();
    let weak = window.as_weak();
    window.on_bare_terminal_requested(move |id| {
        let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
        state.active_tab_is_bare.set(true);
        if let Err(err) = state.start_bare_terminal(uuid) {
            tracing::error!(%err, "start_bare_terminal failed");
            toast("error", t!("sessions.implement_failed", err = err.to_string()).to_string());
        } else if state.blit_active()
            && let Some(w) = weak.upgrade()
        {
            w.set_frame(Image::from_rgba8_premultiplied(
                state.framebuffer.borrow().buffer.clone(),
            ));
        }
    });
}

/// Title / description — live persistence as the user types.
/// Uses `AppState::update_active_task` to cut per-callback
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
                    toast("error", t!("tasks.title_save_failed", err = err.to_string()).to_string());
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
                toast("error", t!("tasks.description_save_failed", err = err.to_string()).to_string());
            }
        });
    }
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
            toast("error", t!("tasks.delete_failed", err = err.to_string()).to_string());
            return;
        }
        toast("info", t!("tasks.deleted").to_string());
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
            window.set_active_task_session_state(SharedString::from("idle"));
            window.set_active_task_tokens_text(SharedString::from(""));
            window.set_active_task_cost_text(SharedString::from(""));
            window.set_active_task_message_count(0);
        }
        refresh_panels();
        refresh();
    });
}

/// Card context menu — move to column, stop session, delete.
fn wire_card_context_menu(window: &MainWindow, ctx: &WiringContext) {
    // Stop session from context menu.
    {
        let state = ctx.state.clone();
        let refresh = ctx.refresh_kanban.clone();
        let toast = ctx.show_toast.clone();
        window.on_card_stop_session(move |id| {
            let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
            match state.stop_session(uuid) {
                Ok(()) => toast("info", t!("sessions.stopped").to_string()),
                Err(err) => {
                    tracing::error!(%err, "card_stop_session failed");
                    toast("error", t!("sessions.stop_failed", err = err.to_string()).to_string());
                }
            }
            refresh();
        });
    }
    // Delete from context menu — delegates to the existing delete_task
    // callback by invoking it through the window.
    {
        let weak = window.as_weak();
        window.on_card_delete(move |id| {
            if let Some(w) = weak.upgrade() {
                w.invoke_delete_task(id);
            }
        });
    }
}
