//! Claude Code CLI adapter — concrete Strategy for
//! [`super::AgentProvider`].
//!
//! Translates Quay's (StartMode, instructions, resume_id, permission_mode)
//! into the argv Claude Code's `claude` binary expects:
//!
//!   acceptEdits       → `claude --permission-mode acceptEdits --allowedTools …`
//!   bypassPermissions → `claude --dangerously-skip-permissions`
//!   Resume            → `claude --resume <id> ...` (prepended to any of above)
//!
//! The `permission_mode` setting controls how aggressively the agent
//! auto-approves operations. `acceptEdits` with an explicit allowlist is
//! the safe default — it pre-approves common dev tools (Bash, Edit, Read,
//! etc.) while keeping destructive shell commands behind a prompt.
//! `bypassPermissions` is the nuclear option for fully headless operation.
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

/// Tools pre-approved via `--allowedTools` in `acceptEdits` mode.
/// These cover the common dev workflow without granting blanket shell
/// access — destructive commands still require interactive approval.
const ALLOWED_TOOLS: &[&str] = &[
    "Bash", "Edit", "Read", "Write", "Grep", "Glob",
    "WebFetch", "WebSearch", "NotebookEdit",
];

impl AgentProvider for ClaudeCodeProvider {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn argv(
        &self,
        _mode: StartMode,
        instructions: Option<&str>,
        resume_id: Option<&str>,
        permission_mode: Option<&str>,
    ) -> Vec<String> {
        let mut argv = vec![self.binary.to_string_lossy().into_owned()];

        // Resume flag comes before any mode flag so `claude --resume <id>
        // --permission-mode acceptEdits` is the final shape on restart.
        if let Some(id) = resume_id {
            argv.push("--resume".into());
            argv.push(id.into());
        }

        // Permission mode — applies to both Plan and Implement since the
        // agent needs file access in either mode.
        match permission_mode.unwrap_or("acceptEdits") {
            "bypassPermissions" => {
                argv.push("--dangerously-skip-permissions".into());
            }
            _ => {
                // Default: acceptEdits + explicit allowlist of common dev
                // tools. This is strictly safer than bypassPermissions
                // because only named tools are pre-approved.
                argv.push("--permission-mode".into());
                argv.push("acceptEdits".into());
                argv.push("--allowedTools".into());
                for tool in ALLOWED_TOOLS {
                    argv.push((*tool).into());
                }
            }
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

    /// Build the expected argv prefix for the default acceptEdits mode.
    fn accept_edits_argv() -> Vec<String> {
        let mut v = vec![
            "--permission-mode".to_string(),
            "acceptEdits".to_string(),
            "--allowedTools".to_string(),
        ];
        for tool in ALLOWED_TOOLS {
            v.push((*tool).to_string());
        }
        v
    }

    #[test]
    fn plan_without_instructions_or_resume() {
        let p = stub();
        let argv = p.argv(StartMode::Plan, None, None, None);
        let mut expected = vec!["/usr/local/bin/claude".to_string()];
        expected.extend(accept_edits_argv());
        assert_eq!(argv, expected);
    }

    #[test]
    fn plan_with_instructions() {
        let p = stub();
        let argv = p.argv(StartMode::Plan, Some("Add dark mode"), None, None);
        let mut expected = vec!["/usr/local/bin/claude".to_string()];
        expected.extend(accept_edits_argv());
        expected.push("Add dark mode".to_string());
        assert_eq!(argv, expected);
    }

    #[test]
    fn implement_with_instructions_and_resume() {
        let p = stub();
        let argv = p.argv(
            StartMode::Implement,
            Some("Fix server crash"),
            Some("sess-abc"),
            Some("acceptEdits"),
        );
        let mut expected = vec![
            "/usr/local/bin/claude".to_string(),
            "--resume".to_string(),
            "sess-abc".to_string(),
        ];
        expected.extend(accept_edits_argv());
        expected.push("Fix server crash".to_string());
        assert_eq!(argv, expected);
    }

    #[test]
    fn implement_without_instructions() {
        let p = stub();
        let argv = p.argv(StartMode::Implement, None, None, Some("acceptEdits"));
        let mut expected = vec!["/usr/local/bin/claude".to_string()];
        expected.extend(accept_edits_argv());
        assert_eq!(argv, expected);
    }

    #[test]
    fn bypass_permissions_mode() {
        let p = stub();
        let argv = p.argv(
            StartMode::Implement,
            Some("Deploy"),
            None,
            Some("bypassPermissions"),
        );
        assert_eq!(
            argv,
            vec![
                "/usr/local/bin/claude".to_string(),
                "--dangerously-skip-permissions".to_string(),
                "Deploy".to_string(),
            ]
        );
    }

    #[test]
    fn none_permission_mode_defaults_to_accept_edits() {
        let p = stub();
        let with_none = p.argv(StartMode::Plan, None, None, None);
        let with_explicit = p.argv(StartMode::Plan, None, None, Some("acceptEdits"));
        assert_eq!(with_none, with_explicit);
    }

    #[test]
    fn plan_and_implement_get_same_permission_flags() {
        let p = stub();
        let plan = p.argv(StartMode::Plan, None, None, Some("acceptEdits"));
        let implement = p.argv(StartMode::Implement, None, None, Some("acceptEdits"));
        assert_eq!(plan, implement);
    }

    #[test]
    fn empty_instructions_treated_as_none() {
        let p = stub();
        let argv = p.argv(StartMode::Plan, Some(""), None, None);
        let mut expected = vec!["/usr/local/bin/claude".to_string()];
        expected.extend(accept_edits_argv());
        assert_eq!(argv, expected);
    }

    #[test]
    fn supports_resume_is_true() {
        let p = stub();
        assert!(p.supports_resume());
    }

    #[test]
    fn name_matches_agent_kind() {
        use crate::kanban::AgentKind;
        let p = stub();
        assert_eq!(p.name(), AgentKind::Claude.as_str());
    }

    #[test]
    fn env_is_empty_by_default() {
        let p = stub();
        assert!(p.env().is_empty());
    }

    #[test]
    fn resume_with_no_instructions() {
        let p = stub();
        let argv = p.argv(StartMode::Plan, None, Some("sess-only-resume"), None);
        let mut expected = vec![
            "/usr/local/bin/claude".to_string(),
            "--resume".to_string(),
            "sess-only-resume".to_string(),
        ];
        expected.extend(accept_edits_argv());
        assert_eq!(argv, expected);
    }
}
