//! Kanban board: domain types + SQLite-backed stores.

pub mod deps;
pub mod labels;
pub mod model;
pub mod store;

pub use deps::DependencyStore;
pub use labels::{Label, LabelStore};
pub use model::{
    AgentKind, SessionRecord, SessionState, StartMode, Task, TaskKind, TaskState,
    WorktreeStrategy, unix_millis_now,
};
pub use store::{SessionStore, TaskStore};
