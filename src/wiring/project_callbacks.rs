//! Polish 8 + 9 — project sidebar callbacks: toggle the active
//! project filter by clicking a row, and submit a new project from
//! the New Project modal.

use std::path::PathBuf;

use slint::{ComponentHandle, SharedString};
use validator::Validate;

use crate::MainWindow;
use crate::wiring::context::WiringContext;
use crate::wiring::validation::{NewProjectForm, first_errors};

pub fn wire(window: &MainWindow, ctx: &WiringContext) {
    wire_project_clicked(window, ctx);
    wire_create_project(window, ctx);
}

/// Polish 9: click a project in the sidebar to filter the kanban by
/// project. Clicking the same project again clears the filter (toggle
/// behavior).
fn wire_project_clicked(window: &MainWindow, ctx: &WiringContext) {
    let weak = window.as_weak();
    let refresh = ctx.refresh_kanban.clone();
    window.on_project_clicked(move |project_id| {
        let Some(w) = weak.upgrade() else { return };
        let current = w.get_active_project_id().to_string();
        if current == project_id.as_str() {
            w.set_active_project_id(SharedString::from(""));
        } else {
            w.set_active_project_id(project_id);
        }
        refresh();
    });
}

/// Polish 8: create a new project from the "New Project" modal.
/// Validates name + repo_path via [`NewProjectForm`] — errors flow to
/// inline Slint properties (`new_project_name_error`,
/// `new_project_repo_path_error`) instead of transient toasts. On
/// success, persists via ProjectStore, refreshes the sidebar list,
/// closes the modal, and clears the fields.
fn wire_create_project(window: &MainWindow, ctx: &WiringContext) {
    let state = ctx.state.clone();
    let weak = window.as_weak();
    let refresh_projects = ctx.refresh_projects.clone();
    let toast = ctx.show_toast.clone();
    window.on_create_project(move || {
        let Some(w) = weak.upgrade() else { return };
        let name = w.get_new_project_name().to_string();
        let repo_path_str = w.get_new_project_repo_path().to_string();
        let base_branch = w.get_new_project_base_branch().to_string();

        // Clear any prior inline errors (a fresh submit is a fresh
        // validation attempt).
        w.set_new_project_name_error("".into());
        w.set_new_project_repo_path_error("".into());

        // Validate via the shared schema. Field errors populate the
        // inline properties; no toast is emitted on validation failure.
        let form = NewProjectForm {
            name: name.trim().to_string(),
            repo_path: repo_path_str.trim().to_string(),
        };
        if let Err(errs) = form.validate() {
            let map = first_errors(&errs);
            if let Some(msg) = map.get("name") {
                w.set_new_project_name_error(msg.clone().into());
            }
            if let Some(msg) = map.get("repo_path") {
                w.set_new_project_repo_path_error(msg.clone().into());
            }
            tracing::warn!("create_project: validation failed");
            return;
        }

        let repo_path = PathBuf::from(repo_path_str.trim());
        let base = if base_branch.trim().is_empty() {
            "main".to_string()
        } else {
            base_branch.trim().to_string()
        };

        let project_name = name.trim().to_string();
        let project = crate::kanban::Project::new(&project_name, repo_path, base);
        if let Err(err) = state.project_store().insert(&project) {
            tracing::warn!(%err, "create_project insert failed");
            // Server-side errors (DB, filesystem) still use toast
            // because they aren't input validation — they're runtime
            // failures the user can't fix by typing differently.
            toast("error", format!("Create project failed: {err}"));
            return;
        }

        // Reset form fields + close modal.
        w.set_new_project_name(SharedString::from(""));
        w.set_new_project_repo_path(SharedString::from(""));
        w.set_new_project_base_branch(SharedString::from("main"));
        w.set_new_project_open(false);

        toast("success", format!("Created project “{project_name}”"));
        refresh_projects();
    });
}
