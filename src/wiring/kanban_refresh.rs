//! Rebuild every kanban-column model + the right-pane open-tabs strip.
//!
//! Used to be a ~217 line closure in `main.rs` (`refresh_kanban`). The
//! body is mechanical but expensive — for each task we hit:
//!   - the label store (one query per task)
//!   - the dependency store (one query per task)
//!   - `git::status::read_status` if the task has a worktree
//!
//! and then we rewrite the 6 column models + the open-tabs model + the
//! 7 stats counter properties on `MainWindow`. Splitting it out keeps
//! `main()` focused on wiring rather than data shuffling.
//!
//! The function is intentionally not generic and not async — it runs
//! on the Slint UI thread, called from every callback that mutates
//! task state.

use std::collections::HashMap;
use std::rc::Rc;
use std::str::FromStr;

use slint::{Model, SharedString, VecModel};
use uuid::Uuid;

use crate::app::AppState;
use crate::kanban::{Task, TaskKind, TaskState};
use crate::wiring::helpers::{kind_to_str, label_to_pill, replace_model, task_to_card};
use crate::{LabelPillData, MainWindow, OpenTaskTabData, TaskCardData};

/// The seven `VecModel`s that the kanban view binds to. Bundled into a
/// single struct so the rebuild function only takes one positional
/// reference instead of 7. Each model is owned by Slint via `ModelRc`,
/// so cloning the `Rc` here is the cheap reference-count bump.
pub struct KanbanModels {
    pub backlog: Rc<VecModel<TaskCardData>>,
    pub planning: Rc<VecModel<TaskCardData>>,
    pub implementation: Rc<VecModel<TaskCardData>>,
    pub review: Rc<VecModel<TaskCardData>>,
    pub done: Rc<VecModel<TaskCardData>>,
    pub misc: Rc<VecModel<TaskCardData>>,
    pub open_tabs: Rc<VecModel<OpenTaskTabData>>,
}

/// Re-query the DB and rebuild every kanban column + open-tabs model.
///
/// Reads the active label / project filter from `window` so the result
/// reflects whatever the user has clicked, then walks every task and
/// places it into the model matching its `TaskState`. Also pushes the
/// raw counts to the status bar (the totals are pre-filter so the bar
/// reflects the whole board even when a label/project filter hides
/// some cards).
pub fn rebuild(state: &AppState, window: &MainWindow, models: &KanbanModels) {
    let tasks = match state.list_tasks() {
        Ok(t) => t,
        Err(err) => {
            tracing::error!(%err, "failed to list tasks");
            return;
        }
    };

    // Build a stable display-id map: order tasks by created_at and
    // assign 1, 2, 3, … so each task keeps its number across refreshes.
    let mut sorted_by_creation = tasks.clone();
    sorted_by_creation.sort_by_key(|t| t.created_at);
    let mut display_ids: HashMap<Uuid, i32> = HashMap::new();
    for (i, t) in sorted_by_creation.iter().enumerate() {
        display_ids.insert(t.id, (i + 1) as i32);
    }

    // Resolve the active label filter (empty string = no filter).
    let filter_label_id: Option<Uuid> = {
        let s = window.get_filter_label_id().to_string();
        if s.is_empty() {
            None
        } else {
            Uuid::from_str(&s).ok()
        }
    };

    // Polish 9: resolve the active project filter. When set, only tasks
    // whose `project_id` matches survive. Tasks with no project_id are
    // hidden when any filter is active.
    let filter_project_id: Option<Uuid> = {
        let s = window.get_active_project_id().to_string();
        if s.is_empty() {
            None
        } else {
            Uuid::from_str(&s).ok()
        }
    };

    let active_uuid = *state.active_task.borrow();
    let label_store = state.label_store();
    let dep_store = state.dependency_store();

    let mut backlog_v = Vec::new();
    let mut planning_v = Vec::new();
    let mut implementation_v = Vec::new();
    let mut review_v = Vec::new();
    let mut done_v = Vec::new();
    let mut misc_v = Vec::new();

    for task in &tasks {
        let display_id = display_ids.get(&task.id).copied().unwrap_or(0);
        let running = active_uuid == Some(task.id);

        // Load the task's labels. Used both for the pills on the
        // card and for the filter check below.
        let task_labels = label_store.labels_for_task(task.id).unwrap_or_default();

        // Label filter: skip this task if we have an active filter and
        // this task doesn't carry the label.
        if let Some(filter_id) = filter_label_id
            && !task_labels.iter().any(|l| l.id == filter_id)
        {
            continue;
        }

        // Project filter: skip tasks whose project_id doesn't match
        // the active project. Tasks with no project are hidden whenever
        // a project filter is active.
        if let Some(project_filter) = filter_project_id {
            if task.project_id != Some(project_filter) {
                continue;
            }
        }

        // Blocked count — how many dependencies are still not Done.
        let blocked_count: i32 = dep_store
            .dependencies_of(task.id)
            .unwrap_or_default()
            .iter()
            .filter(|dep_id| {
                tasks
                    .iter()
                    .find(|t| t.id == **dep_id)
                    .map(|t| t.state != TaskState::Done)
                    .unwrap_or(false)
            })
            .count() as i32;

        // Dirty flag: only meaningful when the task actually has a
        // worktree. `git::status::read_status` is cheap (libgit2
        // in-process) but we still skip the call when there's nothing
        // to inspect to keep refresh latency low on big kanbans.
        let dirty = task
            .worktree_path
            .as_deref()
            .and_then(|p| match crate::git::status::read_status(p) {
                Ok(status) => Some(!status.clean),
                Err(err) => {
                    tracing::debug!(
                        task_id = %task.id,
                        %err,
                        "read_status failed for worktree, treating as clean"
                    );
                    None
                }
            })
            .unwrap_or(false);

        let label_pills: Vec<LabelPillData> = task_labels.iter().map(label_to_pill).collect();

        let card = task_to_card(task, display_id, running, dirty, label_pills, blocked_count);
        match task.state {
            TaskState::Backlog => backlog_v.push(card),
            TaskState::Planning => planning_v.push(card),
            TaskState::Implementation => implementation_v.push(card),
            TaskState::Review => review_v.push(card),
            TaskState::Done => done_v.push(card),
            TaskState::Misc => misc_v.push(card),
        }
    }

    // Polish 13: update the status bar stats before the models are
    // replaced. We use the raw task list (pre-filter) for the totals
    // so the bar reflects the whole board even when a label or project
    // filter is active — the user wants to know what they have, not
    // just what's visible.
    let mut counts = [0i32; 6];
    for t in &tasks {
        let idx = match t.state {
            TaskState::Backlog => 0,
            TaskState::Planning => 1,
            TaskState::Implementation => 2,
            TaskState::Review => 3,
            TaskState::Done => 4,
            TaskState::Misc => 5,
        };
        counts[idx] += 1;
    }
    window.set_stats_backlog(counts[0]);
    window.set_stats_planning(counts[1]);
    window.set_stats_implementation(counts[2]);
    window.set_stats_review(counts[3]);
    window.set_stats_done(counts[4]);
    window.set_stats_misc(counts[5]);
    window.set_stats_total(tasks.len() as i32);
    window.set_stats_sessions(state.sessions.borrow().len() as i32);

    replace_model(&models.backlog, backlog_v);
    replace_model(&models.planning, planning_v);
    replace_model(&models.implementation, implementation_v);
    replace_model(&models.review, review_v);
    replace_model(&models.done, done_v);
    replace_model(&models.misc, misc_v);

    // Polish 16: rebuild the open-task tabs model from the AppState
    // pinned list. Purge any tabs whose task was deleted in the
    // meantime so the UI never shows a stale orphan chip. The list
    // preserves the user's insertion order — no re-sort.
    let tasks_by_id: HashMap<Uuid, &Task> = tasks.iter().map(|t| (t.id, t)).collect();
    let mut pinned = state.open_tabs.borrow_mut();
    let before = pinned.len();
    pinned.retain(|id| tasks_by_id.contains_key(id));
    let purged = pinned.len() != before;
    let snapshot: Vec<Uuid> = pinned.clone();
    drop(pinned);
    // Polish 34: if we purged stale ids, persist the trimmed list so
    // the next launch doesn't re-hydrate them.
    if purged {
        state.persist_open_tabs();
    }
    while models.open_tabs.row_count() > 0 {
        models.open_tabs.remove(models.open_tabs.row_count() - 1);
    }
    for id in &snapshot {
        if let Some(task) = tasks_by_id.get(id) {
            let display_id = display_ids.get(id).copied().unwrap_or(0);
            let kind = TaskKind::from_title(&task.title);
            models.open_tabs.push(OpenTaskTabData {
                id: SharedString::from(id.to_string()),
                display_id,
                title: SharedString::from(task.title.as_str()),
                kind: SharedString::from(kind_to_str(kind)),
                is_active: active_uuid == Some(*id),
                session_state: SharedString::from(task.session_state.as_str()),
            });
        }
    }

}
