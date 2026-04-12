//! Claude Code CLI adapter — concrete Strategy for
//! [`super::AgentProvider`].
//!
//! Translates Quay's (StartMode, instructions, resume_id) triple into the
//! argv Claude Code's `claude` binary expects:
//!
//!   Plan      → `claude [<instructions>]`
//!   Implement → `claude --permission-mode acceptEdits [<instructions>]`
//!   Resume    → `claude --resume <id> ...` (prepended to any of the above)
//!
//! Claude Code supports `--resume <session-id>` to rehydrate the agent's
//! internal conversation memory from its state directory at
//! `~/.claude/projects/<cwd>/<session-id>.jsonl`. Quay does not yet capture
//! that session id (that work lands in Phase 3); for now the resume hook is
//! in place but only gets used once the id is persisted.

use std::path::PathBuf;

use anyhow::{Context, Result};

use super::AgentProvider;
use crate::kanban::StartMode;

pub struct ClaudeCodeProvider {
    pub binary: PathBuf,
}

impl ClaudeCodeProvider {
    /// Locate `claude` on PATH. Fails with a friendly message if the user
    /// has not installed Claude Code yet.
    pub fn detect() -> Result<Self> {
        let binary = which::which("claude").context(
            "`claude` not found on PATH — install Claude Code from anthropic.com/claude-code",
        )?;
        Ok(Self { binary })
    }
}

impl AgentProvider for ClaudeCodeProvider {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn argv(
        &self,
        mode: StartMode,
        instructions: Option<&str>,
        resume_id: Option<&str>,
    ) -> Vec<String> {
        let mut argv = vec![self.binary.to_string_lossy().into_owned()];

        // Resume flag comes before any mode flag so `claude --resume <id>
        // --permission-mode acceptEdits` is the final shape on restart.
        if let Some(id) = resume_id {
            argv.push("--resume".into());
            argv.push(id.into());
        }

        // Implement mode auto-accepts file edits; Plan mode leaves the
        // default permission model in place (agent proposes, user confirms).
        if matches!(mode, StartMode::Implement) {
            argv.push("--permission-mode".into());
            argv.push("acceptEdits".into());
        }

        // Prompt passed as positional argument triggers interactive mode
        // with the given starting message. (Not `-p`, which would run in
        // non-interactive print mode and exit after one turn.)
        if let Some(prompt) = instructions
            && !prompt.is_empty()
        {
            argv.push(prompt.to_string());
        }

        argv
    }

    fn supports_resume(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construct a provider without hitting the filesystem — useful for argv
    /// shape tests that don't care whether `claude` is actually installed.
    fn stub() -> ClaudeCodeProvider {
        ClaudeCodeProvider {
            binary: PathBuf::from("/usr/local/bin/claude"),
        }
    }

    #[test]
    fn plan_without_instructions_or_resume() {
        let p = stub();
        let argv = p.argv(StartMode::Plan, None, None);
        assert_eq!(argv, vec!["/usr/local/bin/claude".to_string()]);
    }

    #[test]
    fn plan_with_instructions() {
        let p = stub();
        let argv = p.argv(StartMode::Plan, Some("Add dark mode"), None);
        assert_eq!(
            argv,
            vec![
                "/usr/local/bin/claude".to_string(),
                "Add dark mode".to_string(),
            ]
        );
    }

    #[test]
    fn implement_with_instructions_and_resume() {
        let p = stub();
        let argv = p.argv(
            StartMode::Implement,
            Some("Fix server crash"),
            Some("sess-abc"),
        );
        assert_eq!(
            argv,
            vec![
                "/usr/local/bin/claude".to_string(),
                "--resume".to_string(),
                "sess-abc".to_string(),
                "--permission-mode".to_string(),
                "acceptEdits".to_string(),
                "Fix server crash".to_string(),
            ]
        );
    }

    #[test]
    fn implement_without_instructions() {
        let p = stub();
        let argv = p.argv(StartMode::Implement, None, None);
        assert_eq!(
            argv,
            vec![
                "/usr/local/bin/claude".to_string(),
                "--permission-mode".to_string(),
                "acceptEdits".to_string(),
            ]
        );
    }

    #[test]
    fn empty_instructions_treated_as_none() {
        let p = stub();
        let argv = p.argv(StartMode::Plan, Some(""), None);
        assert_eq!(argv, vec!["/usr/local/bin/claude".to_string()]);
    }

    #[test]
    fn supports_resume_is_true() {
        let p = stub();
        assert!(p.supports_resume());
    }

    #[test]
    fn name_matches_agent_kind() {
        // Use the kanban-side enum as the source of truth so a rename
        // there breaks this test loudly instead of silently desyncing
        // the strategy lookup.
        use crate::kanban::AgentKind;
        let p = stub();
        assert_eq!(p.name(), AgentKind::Claude.as_str());
    }

    #[test]
    fn env_is_empty_by_default() {
        // Claude Code uses no extra environment variables — the default
        // empty `env()` keeps Quay from accidentally injecting something
        // into the child process.
        let p = stub();
        assert!(p.env().is_empty());
    }

    #[test]
    fn resume_with_no_instructions() {
        // Resume + Plan with no prompt — argv is `claude --resume <id>`.
        let p = stub();
        let argv = p.argv(StartMode::Plan, None, Some("sess-only-resume"));
        assert_eq!(
            argv,
            vec![
                "/usr/local/bin/claude".to_string(),
                "--resume".to_string(),
                "sess-only-resume".to_string(),
            ]
        );
    }
}
