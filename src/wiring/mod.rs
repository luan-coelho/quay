//! Module that holds the glue between `AppState` and the Slint window.
//!
//! Splitting `main.rs` into `wiring/*` modules keeps the binary's `main()`
//! focused on setup (window creation, AppState construction, model wiring,
//! timer registration) while every callback / refresh / model rebuild
//! lives in a focused submodule alongside the data it touches.
//!
//! Submodules:
//!   - [`helpers`] — small pure conversions and formatters used across the
//!     wiring layer (`task_to_card`, `parse_hex_rgb`, `format_*`, etc.)
//!   - [`kanban_refresh`] — rebuilds every kanban column model + the
//!     right-pane open-tabs strip + the status bar counters whenever
//!     task state changes.
//!   - [`refreshes`] — the smaller per-area rebuild functions
//!     (sidebar projects, Description-tab labels/deps panels, Files
//!     tab tree, Settings QA list, Settings process list).
//!   - [`context`] — [`context::WiringContext`], the bundle of
//!     `Rc`-shared resources every callback group needs.
//!   - [`task_callbacks`] — select / create / move / edit / delete /
//!     plan / implement / filter task-related Slint callbacks.
//!   - [`label_dep_callbacks`] — attach/detach labels and add/remove
//!     dependencies on the active task.
//!   - [`project_callbacks`] — project filter toggle and new-project
//!     modal submit.

pub mod context;
pub mod editor_callbacks;
pub mod helpers;
pub mod hotkey_callbacks;
pub mod kanban_refresh;
pub mod label_dep_callbacks;
pub mod project_callbacks;
pub mod refreshes;
pub mod settings_callbacks;
pub mod tab_callbacks;
pub mod task_callbacks;
pub mod validation;
