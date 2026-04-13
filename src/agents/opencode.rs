//! OpenCode CLI adapter — concrete Strategy for [`super::AgentProvider`].
//!
//! OpenCode (https://opencode.ai) is an open-source TUI coding agent. Unlike
//! Claude Code it does not expose a distinct "permission-mode" flag at the
//! CLI level — the TUI manages permissions interactively — so Plan and
//! Implement produce the same argv here. The instructions prompt, when
//! present, is passed as a positional argument.
//!
//! Session resume is disabled in Phase 1; OpenCode's persistence model is
//! not yet integrated. `supports_resume()` returns `false`, so Quay will
//! never pass a `resume_id` for OpenCode tasks.
//!
//! If the OpenCode CLI changes in a way that requires a different invocation
//! shape, the Strategy pattern isolates the fix to this file — no call site
//! needs to care.

use std::path::PathBuf;

use anyhow::{Context, Result};

use super::AgentProvider;
use crate::kanban::StartMode;

pub struct OpencodeProvider {
    pub binary: PathBuf,
}

impl OpencodeProvider {
    pub fn detect() -> Result<Self> {
        let binary = which::which("opencode")
            .context("`opencode` not found on PATH — install from opencode.ai")?;
        Ok(Self { binary })
    }
}

impl AgentProvider for OpencodeProvider {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn argv(
        &self,
        _mode: StartMode,
        instructions: Option<&str>,
        _resume_id: Option<&str>,
        _permission_mode: Option<&str>,
    ) -> Vec<String> {
        let mut argv = vec![self.binary.to_string_lossy().into_owned()];
        if let Some(prompt) = instructions
            && !prompt.is_empty()
        {
            argv.push(prompt.to_string());
        }
        argv
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stub() -> OpencodeProvider {
        OpencodeProvider {
            binary: PathBuf::from("/usr/local/bin/opencode"),
        }
    }

    #[test]
    fn plan_without_instructions() {
        let p = stub();
        let argv = p.argv(StartMode::Plan, None, None, None);
        assert_eq!(argv, vec!["/usr/local/bin/opencode".to_string()]);
    }

    #[test]
    fn implement_with_instructions_ignores_resume() {
        // OpenCode does not support resume, so even a non-None resume_id
        // should not leak into the argv.
        let p = stub();
        let argv = p.argv(
            StartMode::Implement,
            Some("Refactor the auth module"),
            Some("sess-xyz"),
            None,
        );
        assert_eq!(
            argv,
            vec![
                "/usr/local/bin/opencode".to_string(),
                "Refactor the auth module".to_string(),
            ]
        );
    }

    #[test]
    fn plan_and_implement_produce_identical_argv() {
        // The Strategy collapses modes — OpenCode has no Plan/Implement
        // distinction at the CLI level.
        let p = stub();
        let plan = p.argv(StartMode::Plan, Some("Test"), None, None);
        let implement = p.argv(StartMode::Implement, Some("Test"), None, None);
        assert_eq!(plan, implement);
    }

    #[test]
    fn permission_mode_is_ignored() {
        // OpenCode has no CLI-level permission controls. The permission_mode
        // parameter must not leak into argv regardless of its value.
        let p = stub();
        let argv = p.argv(
            StartMode::Implement,
            Some("Test"),
            None,
            Some("bypassPermissions"),
        );
        assert_eq!(
            argv,
            vec![
                "/usr/local/bin/opencode".to_string(),
                "Test".to_string(),
            ]
        );
    }

    #[test]
    fn supports_resume_is_false() {
        let p = stub();
        assert!(!p.supports_resume());
    }

    #[test]
    fn name_matches_agent_kind() {
        // Use the kanban-side enum as the source of truth so a rename
        // there breaks this test loudly instead of silently desyncing
        // the strategy lookup.
        use crate::kanban::AgentKind;
        let p = stub();
        assert_eq!(p.name(), AgentKind::Opencode.as_str());
    }

    #[test]
    fn env_is_empty_by_default() {
        let p = stub();
        assert!(p.env().is_empty());
    }

    #[test]
    fn empty_instructions_treated_as_none() {
        // Mirrors the claude_code edge case: empty string should not
        // append a stray empty positional argument to argv.
        let p = stub();
        let argv = p.argv(StartMode::Plan, Some(""), None, None);
        assert_eq!(argv, vec!["/usr/local/bin/opencode".to_string()]);
    }
}
