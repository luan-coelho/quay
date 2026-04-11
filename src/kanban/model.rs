//! Domain types for the kanban board: `TaskState`, `Task`, `SessionRecord`.
//!
//! These are the structs that pass between the SQLite `Store`s in `store.rs`
//! and the Slint UI in Task 7. They are deliberately Plain Old Data — no
//! interior mutability, no I/O, easy to serialize.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The four kanban columns. Stored as lowercase strings in SQLite to keep the
/// schema human-readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Backlog,
    Planning,
    Implementation,
    Done,
}

impl TaskState {
    /// All variants in display order — useful for iterating columns.
    #[allow(dead_code)]
    pub const ALL: [Self; 4] = [
        Self::Backlog,
        Self::Planning,
        Self::Implementation,
        Self::Done,
    ];

    /// SQL/string form. Round-trips with `parse`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::Planning => "planning",
            Self::Implementation => "implementation",
            Self::Done => "done",
        }
    }

    /// Inverse of `as_str`. Returns `None` for any unknown string so callers
    /// can decide whether to fail or coerce to a default.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "backlog" => Some(Self::Backlog),
            "planning" => Some(Self::Planning),
            "implementation" => Some(Self::Implementation),
            "done" => Some(Self::Done),
            _ => None,
        }
    }

    /// Human-friendly column heading.
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            Self::Backlog => "Backlog",
            Self::Planning => "Planning",
            Self::Implementation => "Implementation",
            Self::Done => "Done",
        }
    }
}

/// Visual classification of a task — drives the diamond/chip colours in the
/// kanban list. For now this is **derived from the title** at display time
/// (no `kind` column in the schema yet); a future migration will persist it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskKind {
    Enhancement,
    Feature,
    Bug,
}

impl TaskKind {
    /// Heuristic classifier — looks at the lowercased title for common verbs.
    /// Defaults to `Feature` when nothing matches.
    pub fn from_title(title: &str) -> Self {
        let lower = title.to_ascii_lowercase();
        let bug_words = ["fix", "bug", "crash", "broken", "regression", "hotfix"];
        let enhancement_words = ["add", "improve", "refactor", "polish", "tweak"];

        if bug_words.iter().any(|w| lower.contains(w)) {
            Self::Bug
        } else if enhancement_words.iter().any(|w| lower.contains(w)) {
            Self::Enhancement
        } else {
            Self::Feature
        }
    }

    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            Self::Enhancement => "Enhancement",
            Self::Feature => "Feature",
            Self::Bug => "Bug",
        }
    }
}

// ── Phase 1 agent/session enums ─────────────────────────────────────────────

/// Which CLI tool Quay should spawn inside the PTY for a task.
///
/// `Claude` and `Opencode` are the two AI providers shipped in Phase 1;
/// `Bare` means "no agent, just run the user's `$SHELL`" and bypasses the
/// [`crate::agents::AgentProvider`] Strategy trait entirely (see
/// [`crate::app::AppState::start_session`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
    Opencode,
    Bare,
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Opencode => "opencode",
            Self::Bare => "bare",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Self::Claude),
            "opencode" => Some(Self::Opencode),
            "bare" => Some(Self::Bare),
            _ => None,
        }
    }
}

impl Default for AgentKind {
    fn default() -> Self {
        Self::Claude
    }
}

/// Which "start gesture" the user chose when launching an agent session.
///
/// Plan asks the agent to outline an approach before touching files and moves
/// the task into the Planning column. Implement grants write permissions
/// up-front and drops the task into Implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StartMode {
    Plan,
    Implement,
}

impl StartMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Implement => "implement",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "plan" => Some(Self::Plan),
            "implement" => Some(Self::Implement),
            _ => None,
        }
    }
}

/// How a task relates to a git worktree.
///
/// `Create` (default) auto-creates a fresh worktree at
/// `<repo>/.worktrees/<slug>/` on first Plan/Implement click.
/// `None` skips worktree creation and runs in the repo root.
/// `Select` (Phase 2+) lets the user reuse an existing worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorktreeStrategy {
    Create,
    None,
    Select,
}

impl WorktreeStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::None => "none",
            Self::Select => "select",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "create" => Some(Self::Create),
            "none" => Some(Self::None),
            "select" => Some(Self::Select),
            _ => None,
        }
    }
}

impl Default for WorktreeStrategy {
    fn default() -> Self {
        Self::Create
    }
}

/// Lifecycle state of the PTY session attached to a task.
///
/// The UI maps these to the status dot / chip on each card:
///   Idle    → outline circle (no session yet or cleanly closed)
///   Busy    → green dot (agent actively processing)
///   Awaiting→ amber dot (agent paused for user approval)
///   Stopped → gray dot (user stopped it manually)
///   Exited  → gray dot (child process finished on its own)
///   Error   → red dot (spawn or I/O failure)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Idle,
    Busy,
    Awaiting,
    Stopped,
    Exited,
    Error,
}

impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Busy => "busy",
            Self::Awaiting => "awaiting",
            Self::Stopped => "stopped",
            Self::Exited => "exited",
            Self::Error => "error",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "idle" => Some(Self::Idle),
            "busy" => Some(Self::Busy),
            "awaiting" => Some(Self::Awaiting),
            "stopped" => Some(Self::Stopped),
            "exited" => Some(Self::Exited),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

impl Default for SessionState {
    fn default() -> Self {
        Self::Idle
    }
}

/// One kanban card. Maps 1:1 to a row in the `tasks` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: Uuid,
    pub title: String,
    pub description: Option<String>,
    /// Initial prompt sent to the agent CLI when a session is launched.
    pub instructions: Option<String>,
    pub state: TaskState,
    /// Absolute path to the parent git repository the worktree comes from.
    pub repo_path: PathBuf,
    /// Absolute path to the linked git worktree, once it has been created.
    pub worktree_path: Option<PathBuf>,
    /// Branch name git is checked out to inside the worktree.
    pub branch_name: Option<String>,
    /// Legacy free-form string kept for v1 rows (equivalent to `cli_selection`
    /// for newly created tasks). Kept alongside `cli_selection` to avoid
    /// breaking existing callers; new code should prefer `cli_selection`.
    pub agent_kind: String,
    /// Which agent provider Quay spawns for this task (Strategy dispatch).
    pub cli_selection: AgentKind,
    /// Last start gesture chosen by the user, if any. `None` until the user
    /// clicks Plan or Implement for the first time.
    pub start_mode: Option<StartMode>,
    /// How Quay should manage the git worktree for this task.
    pub worktree_strategy: WorktreeStrategy,
    /// Lifecycle state of the current PTY session (if any).
    pub session_state: SessionState,
    /// PID of the child process backing the current session, when running.
    pub process_pid: Option<i32>,
    /// Position within the column. Lower = higher up.
    pub position: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Task {
    /// Convenience constructor for a brand-new card. Sets timestamps to "now"
    /// and assigns a fresh UUID v4.
    pub fn new(
        title: impl Into<String>,
        repo_path: PathBuf,
        agent_kind: impl Into<String>,
    ) -> Self {
        let now = unix_millis_now();
        let agent_kind_str: String = agent_kind.into();
        let cli_selection =
            AgentKind::parse(&agent_kind_str).unwrap_or_default();
        Self {
            id: Uuid::new_v4(),
            title: title.into(),
            description: None,
            instructions: None,
            state: TaskState::Backlog,
            repo_path,
            worktree_path: None,
            branch_name: None,
            agent_kind: agent_kind_str,
            cli_selection,
            start_mode: None,
            worktree_strategy: WorktreeStrategy::default(),
            session_state: SessionState::default(),
            process_pid: None,
            position: 0,
            created_at: now,
            updated_at: now,
        }
    }
}

/// One PTY session attached to a task. Multiple sessions per task are allowed
/// (e.g. user wants two concurrent agent runs on the same worktree).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: Uuid,
    pub task_id: Uuid,
    /// Path to the append-only byte log on disk.
    pub pty_log_path: PathBuf,
    pub cols: u32,
    pub rows: u32,
    pub cwd: PathBuf,
    /// argv as JSON-encoded `Vec<String>` for round-trip fidelity.
    pub command: Vec<String>,
    /// Environment overrides as JSON-encoded `BTreeMap<String, String>`.
    pub env: std::collections::BTreeMap<String, String>,
    /// `None` while the child is still running.
    pub exit_status: Option<i32>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

impl SessionRecord {
    pub fn new(
        task_id: Uuid,
        pty_log_path: PathBuf,
        cols: u32,
        rows: u32,
        cwd: PathBuf,
        command: Vec<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            task_id,
            pty_log_path,
            cols,
            rows,
            cwd,
            command,
            env: std::collections::BTreeMap::new(),
            exit_status: None,
            started_at: unix_millis_now(),
            ended_at: None,
        }
    }
}

/// Shared timestamp helper. Returns the wall-clock time as Unix epoch
/// milliseconds. Defaults to 0 if the system clock is somehow before 1970.
pub fn unix_millis_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
