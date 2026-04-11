//! Kanban board: domain types + SQLite-backed stores.

pub mod model;
pub mod store;

pub use model::{
    AgentKind, SessionRecord, SessionState, StartMode, Task, TaskKind, TaskState,
    WorktreeStrategy, unix_millis_now,
};
pub use store::{SessionStore, TaskStore};
