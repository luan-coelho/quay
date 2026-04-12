//! Shared wiring context — the bundle of `Rc`-shared resources that
//! every callback group needs.
//!
//! Each `wire_*` function in a sibling module takes `&WiringContext`
//! by reference and clones from it whatever the individual callback
//! closures need. Putting this struct in one place keeps the wiring
//! functions from needing 6-10 positional parameters each, and makes
//! it trivial to add a new shared resource (just add a field — every
//! wire fn already has access).
//!
//! What stays OUT of this context:
//!   - `MainWindow` — callbacks want a `Weak<MainWindow>`, not a
//!     strong reference, to avoid reference cycles. Each wire fn
//!     receives `&MainWindow` explicitly.
//!   - Per-area Slint models (editor_lines_model, diff_model, etc.) —
//!     only the wire fn that uses them needs them, so they're passed
//!     as extra arguments to those specific functions.
//!
//! The `show_toast` field is `Rc<dyn Fn>` because it's constructed
//! inside `main()` with a `Weak<MainWindow>` captured and a monotonic
//! dismissal-generation counter — passing the real closure lets the
//! timer cancellation logic survive intact.

use std::rc::Rc;

use crate::app::AppState;

pub type ToastFn = dyn Fn(&str, String);

#[derive(Clone)]
pub struct WiringContext {
    pub state: Rc<AppState>,
    pub refresh_kanban: Rc<dyn Fn()>,
    pub refresh_projects: Rc<dyn Fn()>,
    pub refresh_active_panels: Rc<dyn Fn()>,
    pub refresh_files: Rc<dyn Fn()>,
    pub refresh_settings_qa: Rc<dyn Fn()>,
    pub refresh_settings_processes: Rc<dyn Fn()>,
    pub show_toast: Rc<ToastFn>,
}
