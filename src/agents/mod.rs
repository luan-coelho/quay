//! Agent CLI providers — Strategy pattern.
//!
//! Each AI coding agent we support (Claude Code, OpenCode, future additions
//! like Cursor or Aider) is a concrete `Strategy`: a struct implementing the
//! [`AgentProvider`] trait. The trait captures "how do you translate a
//! combination of (StartMode, instructions, resume_id) into the exact argv
//! + env a given CLI expects to run an interactive session".
//!
//! The factory [`detect`] maps a persisted [`AgentKind`] from the `tasks`
//! table to a concrete provider, detecting the binary on PATH in the process.
//! Calling code ([`crate::app::AppState::start_session`]) only knows about
//! the trait — it never matches on concrete provider types. Adding a new
//! provider is purely additive: create a new module under `src/agents/`,
//! implement `AgentProvider`, add a variant to `AgentKind`, wire it into
//! `detect`, and update the CHECK constraint in a new schema migration.
//!
//! **Bare shell is not a provider**: when `AgentKind::Bare` is selected, the
//! `start_session` path in `app.rs` skips the trait entirely and spawns the
//! user's `$SHELL` directly. Keeping Bare outside the trait keeps the
//! abstraction conceptually clean — `AgentProvider` is "AI coding agent",
//! not "anything you can run in a PTY".

pub mod claude_code;
pub mod claude_resume;
pub mod opencode;

use anyhow::Result;

use crate::kanban::{AgentKind, StartMode};

/// Strategy interface for AI agent CLI providers.
///
/// Implementors own the knowledge of:
/// - where their binary lives (detected at construction time)
/// - how to translate `(StartMode, instructions, resume_id)` into argv
/// - which env vars the CLI needs
/// - whether the CLI supports session resumption
///
/// `Send + Sync` bounds are included so a provider can be stored inside
/// `Rc<AppState>`/cloned across Slint callbacks freely.
pub trait AgentProvider: Send + Sync {
    /// Short identifier matching the value stored in `tasks.cli_selection`.
    fn name(&self) -> &'static str;

    /// Produce the argv (index 0 is the binary path) for a PTY spawn.
    ///
    /// `instructions` is the initial prompt the user typed into the
    /// Description tab. `resume_id` is only relevant when the provider
    /// reports `supports_resume() == true` — callers should pass `None`
    /// otherwise.
    fn argv(
        &self,
        mode: StartMode,
        instructions: Option<&str>,
        resume_id: Option<&str>,
    ) -> Vec<String>;

    /// Environment variables to export before spawning the child process.
    /// Default is an empty set.
    fn env(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Whether this provider supports resuming a prior session by id.
    /// Returning `true` means Quay will record the session id for future
    /// runs and pass it via `resume_id` next time.
    ///
    /// Default is `false` — most CLIs start a fresh session every launch.
    ///
    /// `#[allow(dead_code)]` until Phase 3 captures Claude Code session
    /// ids from `~/.claude/projects/...` and starts passing them through.
    #[allow(dead_code)]
    fn supports_resume(&self) -> bool {
        false
    }
}

/// Factory — maps the persisted [`AgentKind`] to a concrete provider.
///
/// Returns `Ok(None)` for [`AgentKind::Bare`], signalling the caller that
/// this task should bypass the Strategy path and spawn a plain shell. Any
/// other variant either returns `Ok(Some(provider))` or bubbles a detection
/// error (e.g. the CLI binary is not on PATH).
pub fn detect(kind: AgentKind) -> Result<Option<Box<dyn AgentProvider>>> {
    match kind {
        AgentKind::Claude => {
            Ok(Some(Box::new(claude_code::ClaudeCodeProvider::detect()?)))
        }
        AgentKind::Opencode => Ok(Some(Box::new(opencode::OpencodeProvider::detect()?))),
        AgentKind::Bare => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_bare_returns_none() {
        // Bare shell intentionally does not go through the provider Strategy
        // — start_session uses the user's $SHELL directly.
        let result = detect(AgentKind::Bare).expect("detect should not error");
        assert!(result.is_none(), "Bare must short-circuit to None");
    }
}
