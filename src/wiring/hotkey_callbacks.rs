//! Global keyboard dispatcher.
//!
//! [`wire`] registers `window.on_key_pressed` and delegates the
//! classification to [`crate::hotkeys::classify_hotkey`] (pure, unit
//! tested). The dispatch layer here needs access to AppState + window
//! getters (to decide which modal Escape should close) so it can't
//! live alongside `classify_hotkey` — that's the only reason for this
//! file's existence.

use std::str::FromStr;

use slint::ComponentHandle;

use crate::hotkeys::{self, HotkeyAction};
use crate::terminal::key_text_to_bytes;
use crate::wiring::context::WiringContext;
use crate::MainWindow;

pub fn wire(window: &MainWindow, ctx: &WiringContext) {
    // ── Terminal mouse-wheel scroll ─────────────────────────────────
    {
        let state = ctx.state.clone();
        let weak = window.as_weak();
        window.on_terminal_scroll(move |delta_px| {
            // Convert pixel delta to terminal lines. Cell height is
            // available via the framebuffer; fall back to 18 px if the
            // framebuffer is not initialised yet.
            let cell_h = state.framebuffer.borrow().cell_h.max(1) as f32;
            let mut lines = (delta_px / cell_h).round() as i32;
            // Guarantee at least 1 line per scroll tick so tiny deltas
            // from high-resolution touchpads still produce visible
            // movement.
            if lines == 0 && delta_px != 0.0 {
                lines = if delta_px > 0.0 { 1 } else { -1 };
            }
            state.scroll_active(lines);

            // Re-blit immediately so the user sees the result without
            // waiting for the next PTY poll tick.
            if state.blit_active()
                && let Some(w) = weak.upgrade()
            {
                w.set_frame(slint::Image::from_rgba8_premultiplied(
                    state.framebuffer.borrow().buffer.clone(),
                ));
            }
        });
    }

    // ── Keyboard dispatch ───────────────────────────────────────────
    let state = ctx.state.clone();
    let weak = window.as_weak();
    let refresh = ctx.refresh_kanban.clone();
    let toast = ctx.show_toast.clone();
    window.on_key_pressed(move |text, ctrl, alt, shift, meta| {
        // Pure classification — no Slint, no AppState, no I/O. Tested
        // unit-style in src/hotkeys.rs.
        let action = hotkeys::classify_hotkey(text.as_str(), ctrl, alt, shift, meta);

        match action {
            HotkeyAction::QuickAction(idx) => {
                match state.execute_quick_action(idx) {
                    Ok(Some(name)) => tracing::info!(%name, idx, "quick action fired"),
                    Ok(None) => tracing::debug!(idx, "no quick action at index"),
                    Err(err) => {
                        tracing::warn!(%err, idx, "quick action failed");
                        toast("error", t!("hotkeys.quick_action_failed", err = err.to_string()).to_string());
                    }
                }
            }
            HotkeyAction::CloseTopModal => {
                // Decide which modal to dismiss in priority order.
                // The decision needs MainWindow getters, so it stays
                // here rather than in classify_hotkey. If no modal is
                // open, we fall through to the PTY so Escape still
                // reaches vim/less/etc.
                let mut consumed = false;
                if let Some(w) = weak.upgrade() {
                    // Tab context menu has highest priority — it's
                    // the smallest popup so dismissing it first
                    // matches user expectation.
                    if w.get_card_menu_open() {
                        w.set_card_menu_open(false);
                        consumed = true;
                    } else if w.get_tab_menu_open() {
                        w.set_tab_menu_open(false);
                        consumed = true;
                    } else if w.get_task_search_open() {
                        w.set_task_search_open(false);
                        consumed = true;
                    } else if w.get_shortcuts_open() {
                        w.set_shortcuts_open(false);
                        consumed = true;
                    } else if w.get_new_project_open() {
                        w.set_new_project_open(false);
                        consumed = true;
                    } else if w.get_active_page() == "settings" {
                        w.set_active_page(slint::SharedString::from("home"));
                        consumed = true;
                    }
                }
                if !consumed {
                    // No modal open — forward Escape to the PTY.
                    let bytes = key_text_to_bytes(text.as_str(), ctrl, alt, shift);
                    if !bytes.is_empty() && state.write_to_active(&bytes) {
                        // Session was auto-resumed — refresh UI.
                        if let Some(w) = weak.upgrade() {
                            w.set_active_task_session_state("busy".into());
                        }
                        refresh();
                    }
                }
            }
            HotkeyAction::CreateTask => {
                // Terminal-first: create task + start Claude Code session immediately.
                let project_id = weak.upgrade().and_then(|w| {
                    let id_str = w.get_active_project_id().to_string();
                    uuid::Uuid::from_str(&id_str).ok()
                });
                let count = state.list_tasks().map(|t| t.len()).unwrap_or(0) + 1;
                let title = t!("tasks.new_task_title", count = count).to_string();
                match state.create_task(title, project_id) {
                    Ok(task) => {
                        if let Err(err) = state.start_session(
                            task.id,
                            crate::kanban::StartMode::Implement,
                        ) {
                            tracing::error!(err = ?err, "cli session start failed");
                            toast("error", t!("sessions.implement_failed", err = err.to_string()).to_string());
                        } else {
                            state.pin_open_tab(task.id);
                            let _ = state.select_task(task.id);
                            if let Some(w) = weak.upgrade() {
                                w.set_active_task_id(task.id.to_string().into());
                                w.set_active_task_session_state(crate::kanban::SessionState::Busy.as_str().into());
                                w.set_active_right_tab(slint::SharedString::from("terminal"));
                                if state.blit_active() {
                                    w.set_frame(slint::Image::from_rgba8_premultiplied(
                                        state.framebuffer.borrow().buffer.clone(),
                                    ));
                                }
                            }
                        }
                    }
                    Err(err) => {
                        tracing::error!(%err, "create_task via shortcut failed");
                        toast("error", t!("tasks.create_failed", err = err.to_string()).to_string());
                    }
                }
                refresh();
            }
            HotkeyAction::ToggleSettings => {
                if let Some(w) = weak.upgrade() {
                    if w.get_active_page() == "settings" {
                        w.set_active_page(slint::SharedString::from("home"));
                    } else {
                        w.set_active_page(slint::SharedString::from("settings"));
                    }
                }
            }
            HotkeyAction::CloseActiveTab => {
                // Polish 18: re-uses the same close/fall-back logic
                // as clicking × on the chip by invoking the
                // Slint-side close-task-tab callback directly.
                if let Some(active_id) = *state.active_task.borrow()
                    && let Some(w) = weak.upgrade()
                {
                    w.invoke_close_task_tab(active_id.to_string().into());
                }
            }
            HotkeyAction::OpenTaskSearch => {
                // Polish 35: pre-populate the result list with every
                // task by firing a search with an empty query, then
                // opens the modal.
                if let Some(w) = weak.upgrade() {
                    w.set_task_search_query("".into());
                    w.invoke_task_search_changed("".into());
                    w.set_task_search_open(true);
                }
            }
            HotkeyAction::ToggleShortcuts => {
                if let Some(w) = weak.upgrade() {
                    let open = !w.get_shortcuts_open();
                    w.set_shortcuts_open(open);
                }
            }
            HotkeyAction::CloseOtherTabs => {
                if let Some(w) = weak.upgrade() {
                    let active_id = w.get_active_task_id().to_string();
                    if !active_id.is_empty() {
                        w.invoke_close_other_task_tabs(active_id.into());
                    }
                }
            }
            HotkeyAction::CloseAllTabs => {
                if let Some(w) = weak.upgrade() {
                    w.invoke_close_all_task_tabs();
                }
            }
            HotkeyAction::CycleTabsForward | HotkeyAction::CycleTabsBackward => {
                let forward = matches!(action, HotkeyAction::CycleTabsForward);
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
            }
            HotkeyAction::Paste => {
                match state.paste_to_active() {
                    Ok(()) => tracing::debug!("clipboard paste"),
                    Err(err) => {
                        tracing::warn!(%err, "clipboard paste failed");
                        toast("error", t!("hotkeys.paste_failed", err = err).to_string());
                    }
                }
            }
            HotkeyAction::Fallthrough => {
                let bytes = key_text_to_bytes(text.as_str(), ctrl, alt, shift);
                if !bytes.is_empty() && state.write_to_active(&bytes) {
                    // Session was auto-resumed — refresh UI.
                    if let Some(w) = weak.upgrade() {
                        w.set_active_task_session_state("busy".into());
                    }
                    refresh();
                }
            }
        }
    });
}
