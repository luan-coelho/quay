//! Phase 5 Settings modal wiring — toggle, Quick Actions CRUD, base
//! branch setting, and Process Manager (stop / kill / refresh).
//!
//! The Settings modal's two list models (`settings_qa_model` and
//! `settings_proc_model`) are rebuilt via closures in
//! [`crate::wiring::context::WiringContext`] so this module only
//! needs to register the callbacks and delegate.

use std::str::FromStr;

use slint::ComponentHandle;
use uuid::Uuid;

use crate::MainWindow;
use crate::wiring::context::WiringContext;

pub fn wire(window: &MainWindow, ctx: &WiringContext) {
    wire_toggle_settings(window, ctx);
    wire_locale(window, ctx);
    wire_permission_mode(window, ctx);
    wire_quick_actions(window, ctx);
    wire_base_branch(window, ctx);
    wire_process_manager(window, ctx);
}

/// i18n: switch the active locale when the user picks a different language.
/// Rebuilds the sidebar menu model with translated labels so the change
/// is immediately visible without restarting the app.
fn wire_locale(window: &MainWindow, ctx: &WiringContext) {
    let state = ctx.state.clone();
    let toast = ctx.show_toast.clone();
    let weak = window.as_weak();
    window.on_locale_changed(move |locale| {
        let locale_str = locale.to_string();
        crate::i18n::apply_locale(&locale_str);
        let s = crate::settings::Settings::new(&state.db.conn);
        if let Err(err) = s.set(crate::settings::KEY_LOCALE, &locale_str) {
            tracing::warn!(%err, "failed to persist locale setting");
            toast("error", t!("settings.base_branch_save_failed", err = err.to_string()).to_string());
        }
        // Rebuild sidebar menu with freshly-translated labels.
        if let Some(w) = weak.upgrade() {
            crate::wiring::helpers::rebuild_menu_model(&w);
        }
    });
}

/// Persist the user-chosen permission mode for AI agent sessions.
fn wire_permission_mode(window: &MainWindow, ctx: &WiringContext) {
    let state = ctx.state.clone();
    let toast = ctx.show_toast.clone();
    window.on_permission_mode_changed(move |mode| {
        let mode_str = mode.to_string();
        let s = crate::settings::Settings::new(&state.db.conn);
        if let Err(err) = s.set(crate::settings::KEY_PERMISSION_MODE, &mode_str) {
            tracing::warn!(%err, "failed to persist permission mode setting");
            toast("error", t!("settings.permission_mode_save_failed", err = err.to_string()).to_string());
        }
    });
}

/// Navigate to the Settings page — also refresh the quick actions +
/// process list (so the UI reflects any changes made via sqlite CLI
/// or other out-of-band edits).
fn wire_toggle_settings(window: &MainWindow, ctx: &WiringContext) {
    let weak = window.as_weak();
    let refresh_qa = ctx.refresh_settings_qa.clone();
    let refresh_procs = ctx.refresh_settings_processes.clone();
    window.on_toggle_settings(move || {
        if let Some(w) = weak.upgrade() {
            if w.get_active_page() == "settings" {
                // Toggle back to home when already on settings.
                w.set_active_page(slint::SharedString::from("home"));
            } else {
                w.set_active_page(slint::SharedString::from("settings"));
                refresh_qa();
                refresh_procs();
            }
        }
    });
}

/// Polish 11 — Quick Actions CRUD (add/delete/update). Each operation
/// hits the `QuickActionStore` and surfaces failures via toast.
fn wire_quick_actions(window: &MainWindow, ctx: &WiringContext) {
    {
        // Add a new quick action with sensible defaults. Position is
        // set to the bottom of the list so it picks up the next free
        // Cmd+Alt slot. Refreshes the Settings list so the new row
        // appears.
        let state = ctx.state.clone();
        let refresh_qa = ctx.refresh_settings_qa.clone();
        let toast = ctx.show_toast.clone();
        window.on_add_quick_action(move || {
            let store = crate::quick_actions::QuickActionStore::new(&state.db.conn);
            let existing = store.list_all().unwrap_or_default();
            let position = existing.iter().map(|a| a.position).max().unwrap_or(-1) + 1;
            let action = crate::quick_actions::QuickAction::new(
                t!("settings.new_action_name").to_string(),
                crate::quick_actions::QuickActionKind::Claude,
                "",
                crate::quick_actions::QuickActionCategory::General,
                position,
            );
            if let Err(err) = store.insert(&action) {
                tracing::warn!(%err, "add_quick_action insert failed");
                toast("error", t!("settings.add_qa_failed", err = err.to_string()).to_string());
                return;
            }
            refresh_qa();
        });
    }
    {
        let state = ctx.state.clone();
        let refresh_qa = ctx.refresh_settings_qa.clone();
        let toast = ctx.show_toast.clone();
        window.on_delete_quick_action(move |id| {
            let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
            if let Err(err) = crate::quick_actions::QuickActionStore::new(&state.db.conn)
                .delete(uuid)
            {
                tracing::warn!(%err, "delete_quick_action failed");
                toast("error", t!("settings.delete_qa_failed", err = err.to_string()).to_string());
                return;
            }
            refresh_qa();
        });
    }
    {
        // Inline edit from the Settings row: name / kind / body.
        // Loads the row, mutates the requested fields, and writes
        // back. Fires on every LineEdit keystroke so the DB stays in
        // sync.
        let state = ctx.state.clone();
        let toast = ctx.show_toast.clone();
        window.on_update_quick_action(move |id, name, kind, body| {
            let Ok(uuid) = Uuid::from_str(id.as_str()) else { return };
            let store = crate::quick_actions::QuickActionStore::new(&state.db.conn);
            let Ok(list) = store.list_all() else { return };
            let Some(mut action) = list.into_iter().find(|a| a.id == uuid) else {
                return;
            };
            action.name = name.to_string();
            action.body = body.to_string();
            action.kind = match kind.as_str() {
                "shell" => crate::quick_actions::QuickActionKind::Shell,
                _ => crate::quick_actions::QuickActionKind::Claude,
            };
            if let Err(err) = store.update(&action) {
                tracing::warn!(%err, "update_quick_action failed");
                toast("error", t!("settings.update_qa_failed", err = err.to_string()).to_string());
            }
            // NB: we don't refresh_qa here because the current
            // LineEdit is already showing the updated text. Refreshing
            // would replace the model mid-edit and lose focus.
        });
    }
}

/// General → default base branch (written to the `settings` KV table).
fn wire_base_branch(window: &MainWindow, ctx: &WiringContext) {
    let state = ctx.state.clone();
    let toast = ctx.show_toast.clone();
    window.on_settings_base_branch_changed(move |new_value| {
        let s = crate::settings::Settings::new(&state.db.conn);
        if let Err(err) = s.set(crate::settings::KEY_DEFAULT_BASE_BRANCH, new_value.as_str())
        {
            tracing::warn!(%err, "failed to persist base branch setting");
            toast("error", t!("settings.base_branch_save_failed", err = err.to_string()).to_string());
        }
    });
}

/// Polish 1 — Process Manager: refresh + stop + kill + bulk actions.
fn wire_process_manager(window: &MainWindow, ctx: &WiringContext) {
    // Bulk stop: SIGTERM all tracked sessions.
    {
        let state = ctx.state.clone();
        let refresh_procs = ctx.refresh_settings_processes.clone();
        let refresh_kanban = ctx.refresh_kanban.clone();
        let toast = ctx.show_toast.clone();
        window.on_stop_all_sessions(move || {
            let sessions = state.sessions.borrow();
            let pids: Vec<u32> = sessions
                .values()
                .filter_map(|s| s.child_pid())
                .collect();
            drop(sessions);
            let mut stopped = 0;
            for pid in &pids {
                if crate::process::terminate(*pid).is_ok() {
                    stopped += 1;
                }
            }
            tracing::info!(stopped, total = pids.len(), "bulk stop sessions");
            toast("info", t!("settings.bulk_stopped", count = stopped).to_string());
            refresh_procs();
            refresh_kanban();
        });
    }
    // Bulk kill: SIGKILL all detected agent processes.
    {
        let state = ctx.state.clone();
        let refresh_procs = ctx.refresh_settings_processes.clone();
        let refresh_kanban = ctx.refresh_kanban.clone();
        let toast = ctx.show_toast.clone();
        window.on_kill_all_processes(move || {
            let tracked_pids: std::collections::HashSet<u32> = state
                .sessions
                .borrow()
                .values()
                .filter_map(|s| s.child_pid())
                .collect();
            let entries = crate::process::enumerate(&tracked_pids);
            let mut killed = 0;
            for entry in &entries {
                if crate::process::force_kill(entry.pid).is_ok() {
                    killed += 1;
                }
            }
            tracing::info!(killed, total = entries.len(), "bulk kill all");
            toast("info", t!("settings.bulk_killed", count = killed).to_string());
            refresh_procs();
            refresh_kanban();
        });
    }
    {
        let refresh_procs = ctx.refresh_settings_processes.clone();
        window.on_refresh_processes(move || {
            refresh_procs();
        });
    }
    {
        let refresh_procs = ctx.refresh_settings_processes.clone();
        let toast = ctx.show_toast.clone();
        window.on_process_kill(move |pid| {
            if let Err(err) = crate::process::terminate(pid as u32) {
                tracing::warn!(%err, pid, "terminate() failed");
                toast("error", t!("settings.terminate_failed", pid = pid, err = err.to_string()).to_string());
            }
            refresh_procs();
        });
    }
    {
        let refresh_procs = ctx.refresh_settings_processes.clone();
        let toast = ctx.show_toast.clone();
        window.on_process_force_kill(move |pid| {
            if let Err(err) = crate::process::force_kill(pid as u32) {
                tracing::warn!(%err, pid, "force_kill() failed");
                toast("error", t!("settings.force_kill_failed", pid = pid, err = err.to_string()).to_string());
            }
            refresh_procs();
        });
    }
}
