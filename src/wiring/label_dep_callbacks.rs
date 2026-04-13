//! Polish 3 + Polish 7 — attach/detach labels and add/remove
//! dependencies. Each callback mutates the active task's attached
//! set and refreshes both the kanban (so blocked-count on the card
//! updates) and the Description tab panels (so the user sees their
//! click reflected immediately).

use std::str::FromStr;

use uuid::Uuid;

use crate::MainWindow;
use crate::wiring::context::WiringContext;

pub fn wire(window: &MainWindow, ctx: &WiringContext) {
    wire_labels(window, ctx);
    wire_dependencies(window, ctx);
}

fn wire_labels(window: &MainWindow, ctx: &WiringContext) {
    {
        let state = ctx.state.clone();
        let refresh_panels = ctx.refresh_active_panels.clone();
        let refresh = ctx.refresh_kanban.clone();
        let toast = ctx.show_toast.clone();
        window.on_attach_label(move |label_id| {
            let Some(active_id) = *state.active_task.borrow() else { return };
            let Ok(label_uuid) = Uuid::from_str(label_id.as_str()) else { return };
            let store = state.label_store();
            if let Err(err) = store.attach(active_id, label_uuid) {
                tracing::warn!(%err, "attach_label failed");
                toast("error", t!("labels.attach_failed", err = err.to_string()).to_string());
                return;
            }
            refresh_panels();
            refresh();
        });
    }
    {
        let state = ctx.state.clone();
        let refresh_panels = ctx.refresh_active_panels.clone();
        let refresh = ctx.refresh_kanban.clone();
        let toast = ctx.show_toast.clone();
        window.on_detach_label(move |label_id| {
            let Some(active_id) = *state.active_task.borrow() else { return };
            let Ok(label_uuid) = Uuid::from_str(label_id.as_str()) else { return };
            let store = state.label_store();
            if let Err(err) = store.detach(active_id, label_uuid) {
                tracing::warn!(%err, "detach_label failed");
                toast("error", t!("labels.detach_failed", err = err.to_string()).to_string());
                return;
            }
            refresh_panels();
            refresh();
        });
    }
}

fn wire_dependencies(window: &MainWindow, ctx: &WiringContext) {
    {
        // Polish 7: add_dependency — validated via DependencyStore's
        // cycle detection. Polish 36: cycle rejection surfaces as an
        // error toast instead of silently failing.
        let state = ctx.state.clone();
        let refresh_panels = ctx.refresh_active_panels.clone();
        let refresh = ctx.refresh_kanban.clone();
        let toast = ctx.show_toast.clone();
        window.on_add_dependency(move |dep_id| {
            let Some(active_id) = *state.active_task.borrow() else { return };
            let Ok(dep_uuid) = Uuid::from_str(dep_id.as_str()) else { return };
            let store = state.dependency_store();
            match store.add(active_id, dep_uuid) {
                Ok(()) => {
                    toast("success", t!("dependencies.added").to_string());
                    refresh_panels();
                    refresh();
                }
                Err(err) => {
                    tracing::warn!(%err, "add_dependency rejected");
                    toast("error", t!("dependencies.cannot_add", err = err.to_string()).to_string());
                }
            }
        });
    }
    {
        let state = ctx.state.clone();
        let refresh_panels = ctx.refresh_active_panels.clone();
        let refresh = ctx.refresh_kanban.clone();
        let toast = ctx.show_toast.clone();
        window.on_remove_dependency(move |dep_id| {
            let Some(active_id) = *state.active_task.borrow() else { return };
            let Ok(dep_uuid) = Uuid::from_str(dep_id.as_str()) else { return };
            let store = state.dependency_store();
            if let Err(err) = store.remove(active_id, dep_uuid) {
                tracing::warn!(%err, "remove_dependency failed");
                toast("error", t!("dependencies.remove_failed", err = err.to_string()).to_string());
                return;
            }
            refresh_panels();
            refresh();
        });
    }
}
