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
    FileEntryData, MainWindow, ProcessRowData, ProjectData, QuickActionRowData, SessionEntryData,
    TaskCardData, WorktreeEntryData,
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
    pub session_history: Rc<VecModel<SessionEntryData>>,
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
    while models.session_history.row_count() > 0 {
        models
            .session_history
            .remove(models.session_history.row_count() - 1);
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

    // Phase D: session history for the History tab.
    let session_store = state.session_store();
    if let Ok(records) = session_store.list_for_task(active_id) {
        use crate::wiring::helpers::format_relative_time;
        use std::time::{SystemTime, UNIX_EPOCH};

        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        // Reverse so newest sessions appear first.
        for rec in records.into_iter().rev() {
            let started_at = format_relative_time(rec.started_at);
            let duration = if let Some(ended) = rec.ended_at {
                let dur = ended.saturating_sub(rec.started_at) as u64;
                format_session_duration(dur)
            } else {
                let dur = now_secs.saturating_sub(rec.started_at) as u64;
                format!("{} (running)", format_session_duration(dur))
            };
            let exit_status = rec.exit_status.unwrap_or(-1);
            // Infer agent kind from the command argv. The first element
            // is the binary name; "claude" → claude, "opencode" → opencode,
            // everything else → bare.
            let agent_kind = rec
                .command
                .first()
                .map(|cmd| {
                    if cmd.contains("claude") {
                        "claude"
                    } else if cmd.contains("opencode") {
                        "opencode"
                    } else {
                        "bare"
                    }
                })
                .unwrap_or("bare");

            models.session_history.push(SessionEntryData {
                session_id: SharedString::from(rec.id.to_string()),
                started_at: SharedString::from(started_at),
                duration: SharedString::from(duration),
                exit_status,
                agent_kind: SharedString::from(agent_kind),
            });
        }
    }
}

/// Format a duration in seconds into a compact string: "14s", "2m 14s",
/// "1h 03m".
fn format_session_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        let m = secs / 60;
        let s = secs % 60;
        format!("{m}m {s:02}s")
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{h}h {m:02}m")
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
            format!("{}", i + 1)
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

/// Phase D — rebuild the sidebar Worktrees section from tasks that have
/// an active `worktree_path`. Each entry shows the branch name and
/// associated task title. Cheap — just iterates the in-memory task list.
pub fn rebuild_worktrees(state: &AppState, model: &Rc<VecModel<WorktreeEntryData>>) {
    while model.row_count() > 0 {
        model.remove(model.row_count() - 1);
    }
    let tasks = state.list_tasks().unwrap_or_default();
    for t in &tasks {
        let Some(ref wt_path) = t.worktree_path else {
            continue;
        };
        let branch = t
            .branch_name
            .as_deref()
            .unwrap_or("")
            .to_string();
        model.push(WorktreeEntryData {
            path: SharedString::from(wt_path.to_string_lossy().into_owned()),
            branch: SharedString::from(branch),
            task_title: SharedString::from(t.title.as_str()),
            task_id: SharedString::from(t.id.to_string()),
        });
    }
}
