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
    wire_quick_actions(window, ctx);
    wire_base_branch(window, ctx);
    wire_process_manager(window, ctx);
}

/// Toggle modal: also refresh the quick actions + process list
/// whenever we open (so the UI reflects any changes made via sqlite
/// CLI or other out-of-band edits).
fn wire_toggle_settings(window: &MainWindow, ctx: &WiringContext) {
    let weak = window.as_weak();
    let refresh_qa = ctx.refresh_settings_qa.clone();
    let refresh_procs = ctx.refresh_settings_processes.clone();
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
                "New action",
                crate::quick_actions::QuickActionKind::Claude,
                "",
                crate::quick_actions::QuickActionCategory::General,
                position,
            );
            if let Err(err) = store.insert(&action) {
                tracing::warn!(%err, "add_quick_action insert failed");
                toast("error", format!("Add quick action failed: {err}"));
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
                toast("error", format!("Delete quick action failed: {err}"));
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
                toast("error", format!("Update quick action failed: {err}"));
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
            toast("error", format!("Save base branch failed: {err}"));
        }
    });
}

/// Polish 1 — Process Manager: refresh + stop + kill.
fn wire_process_manager(window: &MainWindow, ctx: &WiringContext) {
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
                toast("error", format!("Terminate pid {pid} failed: {err}"));
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
                toast("error", format!("Force kill pid {pid} failed: {err}"));
            }
            refresh_procs();
        });
    }
}
