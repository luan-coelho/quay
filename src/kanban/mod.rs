//! Kanban board: domain types + SQLite-backed stores.

pub mod model;
pub mod store;

pub use model::{SessionRecord, Task, TaskKind, TaskState, unix_millis_now};
pub use store::{SessionStore, TaskStore};
