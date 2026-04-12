//! Polish 16 + 35 + 41 — open-task tab strip, task quick switcher
//! (Cmd+P), and bulk tab management (close this / others / right-of
//! / all). All of these share the same "switch to a pinned task and
//! rebuild the right pane" pattern, so they live together.

use std::collections::HashMap;
use std::rc::Rc;
use std::str::FromStr;

use slint::{ComponentHandle, Image, Model, SharedString, VecModel};
use uuid::Uuid;

use crate::kanban::TaskKind;
use crate::wiring::context::WiringContext;
use crate::wiring::helpers::{kind_to_str, task_to_card};
use crate::{MainWindow, TaskCardData};

pub fn wire(
    window: &MainWindow,
    ctx: &WiringContext,
    task_search_results: Rc<VecModel<TaskCardData>>,
) {
    wire_open_task_tab(window, ctx);
    wire_close_task_tab(window, ctx);
    wire_close_other_tabs(window, ctx);
    wire_close_all_tabs(window, ctx);
    wire_close_tabs_right_of(window, ctx);
    wire_task_search(window, ctx, task_search_results);
}

/// Polish 16 — focus a pinned tab by switching the active task to it.
/// Cheap because the underlying state lives in AppState already; we
/// just rebuild the right-pane surfaces the same way on_select_task
/// does.
fn wire_open_task_tab(window: &MainWindow, ctx: &WiringContext) {
    let state = ctx.state.clone();
    let weak = window.as_weak();
    let refresh = ctx.refresh_kanban.clone();
    let refresh_panels = ctx.refresh_active_panels.clone();
    let refresh_files = ctx.refresh_files.clone();
    window.on_open_task_tab(move |id| {
        let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
        match state.select_task(uuid) {
            Ok(changed) if !changed => {}
            Ok(_) => {
                if let Some(window) = weak.upgrade() {
                    let store = state.task_store();
                    let task_opt = store.get(uuid);
                    if let Ok(Some(ref t)) = task_opt {
                        if let Some(wt) = t.worktree_path.as_deref() {
                            window.set_file_current_dir(
                                wt.to_string_lossy().into_owned().into(),
                            );
                        } else {
                            window.set_file_current_dir(SharedString::from(""));
                        }
                    }
                    refresh_files();
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
                    window.set_active_task_tokens_text(SharedString::from(""));
                    window.set_active_task_cost_text(SharedString::from(""));
                    window.set_active_task_runtime_text(SharedString::from(""));
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

/// Polish 16 — close an open tab. Removes it from AppState's pinned
/// list; if it was active, falls back to a neighbouring tab (or the
/// empty-state view if no tabs remain). The PTY session is kept alive
/// — closing a tab only hides it from the strip, it does not kill the
/// underlying process. That matches how Lanes handles it.
fn wire_close_task_tab(window: &MainWindow, ctx: &WiringContext) {
    let state = ctx.state.clone();
    let weak = window.as_weak();
    let refresh = ctx.refresh_kanban.clone();
    let refresh_panels = ctx.refresh_active_panels.clone();
    window.on_close_task_tab(move |id| {
        let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
        let fallback = state.close_open_tab(uuid);
        if let Some(next) = fallback {
            // Fall through to the same select-and-rebuild path the
            // open callback uses so the right pane doesn't show a
            // stale terminal frame.
            match state.select_task(next) {
                Ok(_) => {
                    if let Some(window) = weak.upgrade() {
                        let store = state.task_store();
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
                            window.set_active_task_agent(
                                task.cli_selection.as_str().into(),
                            );
                            window.set_active_task_tokens_text(SharedString::from(""));
                            window.set_active_task_cost_text(SharedString::from(""));
                            window.set_active_task_runtime_text(SharedString::from(""));
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
                window.set_active_task_id(SharedString::from(""));
                window.set_active_task_display(SharedString::from(""));
                window.set_active_task_title(SharedString::from(""));
                window.set_active_task_description(SharedString::from(""));
                window.set_active_task_instructions(SharedString::from(""));
                window.set_active_task_session_state(SharedString::from("idle"));
                window.set_active_task_agent(SharedString::from("claude"));
                window.set_active_task_tokens_text(SharedString::from(""));
                window.set_active_task_cost_text(SharedString::from(""));
                window.set_active_task_runtime_text(SharedString::from(""));
                window.set_active_task_message_count(0);
            }
        }
        refresh();
        refresh_panels();
    });
}

/// Polish 41 — close all OTHER tabs, keeping the target.
fn wire_close_other_tabs(window: &MainWindow, ctx: &WiringContext) {
    let state = ctx.state.clone();
    let weak = window.as_weak();
    let refresh = ctx.refresh_kanban.clone();
    let toast = ctx.show_toast.clone();
    window.on_close_other_task_tabs(move |id| {
        let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
        let switch = state.close_other_open_tabs(uuid);
        if let Some(window) = weak.upgrade()
            && let Some(next) = switch
        {
            window.invoke_open_task_tab(next.to_string().into());
        }
        toast("info", "Closed other tabs".to_string());
        refresh();
    });
}

/// Polish 41 — close every tab.
fn wire_close_all_tabs(window: &MainWindow, ctx: &WiringContext) {
    let state = ctx.state.clone();
    let weak = window.as_weak();
    let refresh = ctx.refresh_kanban.clone();
    let refresh_panels = ctx.refresh_active_panels.clone();
    let toast = ctx.show_toast.clone();
    window.on_close_all_task_tabs(move || {
        state.close_all_open_tabs();
        // Mirror the empty-state reset that on_close_task_tab does
        // when there's no fallback target.
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
            window.set_active_task_runtime_text(SharedString::from(""));
            window.set_active_task_message_count(0);
        }
        toast("info", "Closed all tabs".to_string());
        refresh();
        refresh_panels();
    });
}

/// Polish 41 — close every tab strictly to the right of the anchor.
fn wire_close_tabs_right_of(window: &MainWindow, ctx: &WiringContext) {
    let state = ctx.state.clone();
    let weak = window.as_weak();
    let refresh = ctx.refresh_kanban.clone();
    let toast = ctx.show_toast.clone();
    window.on_close_task_tabs_right_of(move |id| {
        let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
        let switch = state.close_tabs_right_of(uuid);
        if let Some(window) = weak.upgrade()
            && let Some(next) = switch
        {
            window.invoke_open_task_tab(next.to_string().into());
        }
        toast("info", "Closed tabs to the right".to_string());
        refresh();
    });
}

/// Polish 35 — task quick switcher: rebuild the filtered results
/// model on every keystroke + open-task-tab on click.
fn wire_task_search(
    window: &MainWindow,
    ctx: &WiringContext,
    task_search_results: Rc<VecModel<TaskCardData>>,
) {
    {
        // Substring case-insensitive match against `#NN` display id
        // and `title`. Empty query shows all tasks (most recently
        // updated first), capped at 50 for sanity.
        let state = ctx.state.clone();
        let model = task_search_results.clone();
        window.on_task_search_changed(move |query| {
            let q = query.to_string().to_lowercase();
            let mut tasks = state.list_tasks().unwrap_or_default();
            // Build a stable display-id map (same as refresh_kanban).
            tasks.sort_by_key(|t| t.created_at);
            let display_ids: HashMap<Uuid, i32> = tasks
                .iter()
                .enumerate()
                .map(|(i, t)| (t.id, (i + 1) as i32))
                .collect();
            // Re-sort for the search ranking: most recently updated
            // first.
            tasks.sort_by_key(|t| std::cmp::Reverse(t.updated_at));

            let filtered: Vec<TaskCardData> = tasks
                .iter()
                .filter(|t| {
                    if q.is_empty() {
                        return true;
                    }
                    let display = display_ids
                        .get(&t.id)
                        .map(|i| format!("#{i}"))
                        .unwrap_or_default();
                    t.title.to_lowercase().contains(&q) || display.contains(&q)
                })
                .take(50)
                .map(|t| {
                    let display_id = display_ids.get(&t.id).copied().unwrap_or(0);
                    task_to_card(t, display_id, false, false, Vec::new(), 0)
                })
                .collect();

            while model.row_count() > 0 {
                model.remove(model.row_count() - 1);
            }
            for card in filtered {
                model.push(card);
            }
        });
    }
    {
        // Clicking a result fires open-task-tab via the existing
        // pinned-tabs callback so the selected task pops into the
        // right pane the same way clicking a kanban card would.
        let weak = window.as_weak();
        window.on_task_search_select(move |id| {
            if let Some(w) = weak.upgrade() {
                w.invoke_open_task_tab(id);
            }
        });
    }
}
