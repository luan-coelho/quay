//! Smaller `refresh_*` rebuild functions extracted from `main.rs`.
//!
//! Each function takes the model(s) it owns + whatever AppState / window
//! handle it needs, and is wrapped by a tiny closure in `main()` so the
//! callbacks that already say `refresh_X.clone()` continue to work
//! unchanged.
//!
//! These used to live as inline `let refresh_X = { ... move || { ... } };`
//! blocks in `main()`. Pulling them out keeps the binary's `main()`
//! focused on wiring rather than data shuffling.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use slint::{Model, SharedString, VecModel};
use uuid::Uuid;

use crate::app::AppState;
use crate::wiring::helpers::{label_to_pill, task_to_card};
use crate::{
    FileEntryData, MainWindow, ProcessRowData, ProjectData, QuickActionRowData, TaskCardData,
};

/// Rebuild the sidebar's project list from the DB. Cheap — one
/// `SELECT *` against `projects`.
pub fn rebuild_projects(state: &AppState, model: &Rc<VecModel<ProjectData>>) {
    let list = state.project_store().list_all().unwrap_or_default();
    while model.row_count() > 0 {
        model.remove(model.row_count() - 1);
    }
    for p in list {
        model.push(ProjectData {
            id: SharedString::from(p.id.to_string()),
            name: SharedString::from(p.name),
        });
    }
}

/// The four `VecModel`s the Description-tab labels/dependencies panels
/// bind to. Bundled into one struct so the rebuild function only needs
/// a single positional argument.
pub struct ActivePanelModels {
    pub labels: Rc<VecModel<crate::LabelPillData>>,
    pub available_labels: Rc<VecModel<crate::LabelPillData>>,
    pub deps: Rc<VecModel<TaskCardData>>,
    pub available_deps: Rc<VecModel<TaskCardData>>,
}

/// Rebuild the per-task labels / available-labels / dependencies /
/// available-dependencies panels for the currently active task. Called
/// from `select_task` and from each attach/detach/remove-dep callback
/// so the UI stays in sync without a full kanban refresh.
pub fn rebuild_active_panels(state: &AppState, models: &ActivePanelModels) {
    // Clear everything first.
    while models.labels.row_count() > 0 {
        models.labels.remove(models.labels.row_count() - 1);
    }
    while models.available_labels.row_count() > 0 {
        models
            .available_labels
            .remove(models.available_labels.row_count() - 1);
    }
    while models.deps.row_count() > 0 {
        models.deps.remove(models.deps.row_count() - 1);
    }
    while models.available_deps.row_count() > 0 {
        models
            .available_deps
            .remove(models.available_deps.row_count() - 1);
    }

    let Some(active_id) = *state.active_task.borrow() else {
        return;
    };
    let label_store = state.label_store();
    let dep_store = state.dependency_store();

    // Attached labels.
    let attached = label_store.labels_for_task(active_id).unwrap_or_default();
    let attached_ids: HashSet<Uuid> = attached.iter().map(|l| l.id).collect();
    for l in &attached {
        models.labels.push(label_to_pill(l));
    }

    // Available labels = all labels not already attached.
    let all = label_store.list_all().unwrap_or_default();
    for l in all {
        if !attached_ids.contains(&l.id) {
            models.available_labels.push(label_to_pill(&l));
        }
    }

    // Direct dependencies as TaskCardData (reused for ID + title).
    let dep_ids = dep_store.dependencies_of(active_id).unwrap_or_default();
    let dep_id_set: HashSet<Uuid> = dep_ids.iter().copied().collect();
    let all_tasks = state.list_tasks().unwrap_or_default();
    // Stable display-ids: same logic as kanban_refresh::rebuild.
    let mut sorted = all_tasks.clone();
    sorted.sort_by_key(|t| t.created_at);
    let display_ids: HashMap<Uuid, i32> = sorted
        .iter()
        .enumerate()
        .map(|(i, t)| (t.id, (i + 1) as i32))
        .collect();
    for dep_id in &dep_ids {
        if let Some(task) = all_tasks.iter().find(|t| t.id == *dep_id) {
            let display_id = display_ids.get(&task.id).copied().unwrap_or(0);
            models
                .deps
                .push(task_to_card(task, display_id, false, false, Vec::new(), 0));
        }
    }

    // Polish 7: available dep candidates = every other task that
    // isn't already a direct prereq. Cycle detection happens at insert
    // time in DependencyStore::add, so the picker may show candidates
    // the store will later reject; that's fine — we report the error
    // and the user tries another.
    for task in &all_tasks {
        if task.id == active_id || dep_id_set.contains(&task.id) {
            continue;
        }
        let display_id = display_ids.get(&task.id).copied().unwrap_or(0);
        models
            .available_deps
            .push(task_to_card(task, display_id, false, false, Vec::new(), 0));
    }
}

/// Polish 17 — rebuild the flattened Files-tab tree against
/// `window.file-current-dir`, walking it via `file_tree::build_tree`
/// using `AppState::expanded_dirs` to decide which directories show
/// their children.
pub fn rebuild_files(state: &AppState, window: &MainWindow, model: &Rc<VecModel<FileEntryData>>) {
    let root_str = window.get_file_current_dir().to_string();
    while model.row_count() > 0 {
        model.remove(model.row_count() - 1);
    }
    if root_str.is_empty() {
        return;
    }
    let root = PathBuf::from(&root_str);
    let expanded = state.expanded_dirs.borrow();
    let entries = match crate::file_tree::build_tree(&root, &expanded) {
        Ok(v) => v,
        Err(err) => {
            tracing::debug!(%err, "build_tree failed");
            Vec::new()
        }
    };
    for e in entries {
        let kind = match e.kind {
            crate::file_tree::EntryKind::Directory => "directory",
            crate::file_tree::EntryKind::File => "file",
        };
        // Polish 40: pick icon glyph + RGB color from the file
        // extension via the file_tree helper.
        let (icon, (r, g, b)) = crate::file_tree::pick_file_icon(&e.name, &e.kind);
        model.push(FileEntryData {
            name: SharedString::from(e.name),
            path: SharedString::from(e.path.to_string_lossy().into_owned()),
            kind: SharedString::from(kind),
            depth: e.depth as i32,
            expanded: e.expanded,
            icon: SharedString::from(icon),
            icon_r: r as i32,
            icon_g: g as i32,
            icon_b: b as i32,
        });
    }
}

/// Rebuild the Settings → Quick Actions list from the DB. The first 9
/// entries get a Cmd+Alt+digit shortcut hint; entries beyond slot 9
/// have an empty shortcut chip.
pub fn rebuild_settings_qa(state: &AppState, model: &Rc<VecModel<QuickActionRowData>>) {
    let actions = crate::quick_actions::QuickActionStore::new(&state.db.conn)
        .list_all()
        .unwrap_or_default();
    while model.row_count() > 0 {
        model.remove(model.row_count() - 1);
    }
    for (i, a) in actions.iter().enumerate() {
        let shortcut = if i < 9 {
            format!("⌘⌥{}", i + 1)
        } else {
            String::new()
        };
        model.push(QuickActionRowData {
            id: SharedString::from(a.id.to_string()),
            name: SharedString::from(a.name.as_str()),
            kind: SharedString::from(a.kind.as_str()),
            body: SharedString::from(a.body.as_str()),
            shortcut: SharedString::from(shortcut),
        });
    }
}

/// Polish 1 — rebuild the Settings → Process Manager list. Tracked
/// PIDs come from `AppState::tracked_pids()` so our spawned agent
/// children are classified as "Tracked" instead of showing up as
/// "Orphans" (which they technically are by parent-pid relationship,
/// but the classifier rule prefers the explicit registry).
pub fn rebuild_settings_processes(state: &AppState, model: &Rc<VecModel<ProcessRowData>>) {
    let tracked: HashSet<u32> = state.tracked_pids();
    let entries = crate::process::enumerate(&tracked);
    while model.row_count() > 0 {
        model.remove(model.row_count() - 1);
    }
    for e in entries {
        model.push(ProcessRowData {
            pid: e.pid as i32,
            name: SharedString::from(e.name),
            cmdline: SharedString::from(e.cmdline),
            class: SharedString::from(e.class.as_str()),
        });
    }
}
