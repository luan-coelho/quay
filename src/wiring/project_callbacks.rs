//! Polish 8 + 9 — project sidebar callbacks: toggle the active
//! project filter by clicking a row, and submit a new project from
//! the New Project modal.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use validator::Validate;

use crate::MainWindow;
use crate::wiring::context::WiringContext;
use crate::wiring::validation::{NewProjectForm, first_errors};

pub fn wire(window: &MainWindow, ctx: &WiringContext) {
    wire_project_clicked(window, ctx);
    wire_close_project_chip(window, ctx);
    wire_create_project(window, ctx);
    wire_browse_new_project_repo(window);
    wire_new_project_repo_path_changed(window);
}

/// Polish 9: click a project in the sidebar to filter the kanban by
/// project. Clicking the same project again clears the filter (toggle
/// behavior).
fn wire_project_clicked(window: &MainWindow, ctx: &WiringContext) {
    let weak = window.as_weak();
    let refresh = ctx.refresh_kanban.clone();
    let state = ctx.state.clone();
    window.on_project_clicked(move |project_id| {
        let Some(w) = weak.upgrade() else { return };
        let current = w.get_active_project_id().to_string();
        let new_value = if current == project_id.as_str() {
            String::new()
        } else {
            project_id.to_string()
        };
        w.set_active_project_id(SharedString::from(new_value.as_str()));
        // Persist the active project filter so it survives restarts.
        let settings = crate::settings::Settings::new(&state.db.conn);
        if let Err(err) = settings.set(crate::settings::KEY_ACTIVE_PROJECT, &new_value) {
            tracing::warn!(%err, "persist active_project failed");
        }
        refresh();
    });
}

/// Hide a project chip from the filter bar without deleting the
/// project. The project stays in the sidebar — closing the chip
/// just removes it from the kanban filter strip.
fn wire_close_project_chip(window: &MainWindow, ctx: &WiringContext) {
    let weak = window.as_weak();
    let refresh_kanban = ctx.refresh_kanban.clone();
    let state = ctx.state.clone();
    window.on_close_project_chip(move |id| {
        let Some(w) = weak.upgrade() else { return };
        let model = w.get_projects();
        // Find and remove the entry from the VecModel by id.
        for i in 0..model.row_count() {
            if let Some(row) = model.row_data(i)
                && row.id == id
            {
                if let Some(vec_model) = model
                    .as_any()
                    .downcast_ref::<slint::VecModel<crate::ProjectData>>()
                {
                    vec_model.remove(i);
                }
                break;
            }
        }
        // If the closed chip was the active filter, clear and persist.
        if w.get_active_project_id() == id {
            w.set_active_project_id(SharedString::from(""));
            let settings = crate::settings::Settings::new(&state.db.conn);
            let _ = settings.set(crate::settings::KEY_ACTIVE_PROJECT, "");
        }
        refresh_kanban();
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
            toast("error", t!("projects.create_failed", err = err.to_string()).to_string());
            return;
        }

        // Reset form fields + close modal.
        w.set_new_project_name(SharedString::from(""));
        w.set_new_project_repo_path(SharedString::from(""));
        w.set_new_project_base_branch(SharedString::from("main"));
        w.set_new_project_branches(ModelRc::from(Rc::new(VecModel::<SharedString>::default())));
        w.set_new_project_open(false);

        toast("success", t!("projects.created", name = project_name).to_string());
        refresh_projects();
    });
}

/// Opens the native folder picker (via `rfd`) so the user can browse
/// for a repository path instead of typing the absolute path by hand.
/// The picker runs synchronously on the Slint event thread — `rfd`
/// uses `xdg-portal` on Linux, Cocoa on macOS, and Win32 on Windows,
/// all of which block the caller until the user dismisses the dialog.
///
/// On success, writes the picked path into `new-project-repo-path`
/// and immediately refreshes the BASE BRANCH Select with the repo's
/// local branches — mirroring what `on_new_project_repo_path_changed`
/// does for typed input.
fn wire_browse_new_project_repo(window: &MainWindow) {
    let weak = window.as_weak();
    window.on_pick_new_project_repo_path(move || {
        // Anchor the dialog's starting directory on whatever is
        // currently in the text field (if it exists), falling back to
        // `$HOME`. Picking the exact defaults shadcn's own pattern of
        // "start where the user was last looking".
        let Some(w) = weak.upgrade() else { return };
        let current = w.get_new_project_repo_path().to_string();
        let start_dir = {
            let candidate = PathBuf::from(current.trim());
            if !current.trim().is_empty() && candidate.exists() {
                Some(candidate)
            } else {
                directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf())
            }
        };

        let mut dialog = rfd::FileDialog::new().set_title(t!("projects.pick_repo_title").to_string());
        if let Some(dir) = start_dir {
            dialog = dialog.set_directory(dir);
        }

        let Some(picked) = dialog.pick_folder() else {
            // User cancelled — nothing to do.
            return;
        };

        let path_str = picked.to_string_lossy().to_string();
        w.set_new_project_repo_path(SharedString::from(path_str.clone()));
        // Typing via the picker should also clear any stale error
        // banner on the repo-path field.
        w.set_new_project_repo_path_error("".into());

        let (branches, preferred) = load_branches_with_preferred(&picked);
        w.set_new_project_branches(branches);
        if let Some(b) = preferred {
            w.set_new_project_base_branch(SharedString::from(b));
        }
    });
}

/// Whenever the repository-path text changes (either through typing
/// or through the Browse button), we re-read the repo's local
/// branches and push them into `new-project-branches`. If the path
/// doesn't resolve to a git repository we fall back to an empty
/// list, which the Select renders as "No branches available".
fn wire_new_project_repo_path_changed(window: &MainWindow) {
    let weak = window.as_weak();
    window.on_new_project_repo_path_changed(move |new_path| {
        let Some(w) = weak.upgrade() else { return };
        let trimmed = new_path.to_string();
        let trimmed = trimmed.trim();
        if trimmed.is_empty() {
            w.set_new_project_branches(ModelRc::from(Rc::new(VecModel::<SharedString>::default())));
            return;
        }
        let path = PathBuf::from(trimmed);
        let (branches, preferred) = load_branches_with_preferred(&path);
        w.set_new_project_branches(branches);

        // Only overwrite the base-branch value if the currently-
        // selected one isn't in the new list — otherwise typing a
        // character in the path would blow away a user's branch
        // choice on every keystroke.
        if let Some(pref) = preferred {
            let current = w.get_new_project_base_branch().to_string();
            let model = w.get_new_project_branches();
            let has_current = (0..model.row_count())
                .any(|i| model.row_data(i).map(|s| s.as_str() == current).unwrap_or(false));
            if !has_current {
                w.set_new_project_base_branch(SharedString::from(pref));
            }
        }
    });
}

/// Best-effort branch enumeration for a picked repository path.
/// Returns `(branches_model, preferred_default)` where `preferred_default`
/// is the branch the caller should auto-select if the current value is
/// not already in the list (`main` → `master` → first entry).
fn load_branches_with_preferred(path: &Path) -> (ModelRc<SharedString>, Option<String>) {
    let branches = match crate::git::status::list_branches(path) {
        Ok(b) => b,
        Err(err) => {
            tracing::debug!(?err, path = %path.display(), "list_branches failed");
            Vec::new()
        }
    };

    let preferred = branches
        .iter()
        .find(|b| b.as_str() == "main")
        .or_else(|| branches.iter().find(|b| b.as_str() == "master"))
        .or_else(|| branches.first())
        .cloned();

    let shared: Vec<SharedString> = branches.into_iter().map(SharedString::from).collect();
    let model = Rc::new(VecModel::from(shared));
    (ModelRc::from(model), preferred)
}
