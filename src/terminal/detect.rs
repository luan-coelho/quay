//! Pattern-based session state detection.
//!
//! Inspects the last N lines of a terminal grid to classify whether
//! an AI agent is busy (thinking/streaming), awaiting user input
//! (tool approval, yes/no prompt), or idle at a shell prompt.
//!
//! The classifier is agent-aware: Claude Code and OpenCode have
//! different prompt patterns.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Term;

use crate::kanban::{AgentKind, SessionState};

/// How many of the bottom-most non-empty lines to check for patterns.
/// Increased from 3 to 5 to account for multi-line status bars (e.g.
/// Claude Code renders 2–3 status bar lines below the `❯` prompt).
const CHECK_NON_EMPTY: usize = 5;

/// Extract the text content of every visible row from the terminal grid.
/// Trailing whitespace on each line is trimmed.
///
/// We scan the full screen (not just the last N rows) because Claude
/// Code's `❯` prompt can sit near the top of a tall terminal — only a
/// few lines below the startup banner — with the status bar pinned to
/// the very bottom.  A small fixed window (e.g. 8 rows) would miss the
/// prompt entirely.
fn visible_lines<T: EventListener>(term: &Term<T>) -> Vec<String> {
    let grid = term.grid();
    let total_rows = grid.screen_lines();
    let cols = grid.columns();
    let mut lines = Vec::with_capacity(total_rows);
    for row in 0..total_rows {
        let mut line = String::with_capacity(cols);
        for col in 0..cols {
            let cell = &grid[Line(row as i32)][Column(col)];
            line.push(cell.c);
        }
        lines.push(line.trim_end().to_string());
    }
    lines
}

/// Claude Code patterns that indicate the agent is waiting for user
/// input (tool approval, confirmation, etc.).
const CLAUDE_AWAITING_PATTERNS: &[&str] = &[
    "Do you want to",
    "Allow ",
    "Approve?",
    "approve?",
    "(y/n)",
    "(Y/n)",
    "(yes/no)",
    "Press Enter to",
    "? (y/N)",
    "? (Y/n)",
    // Claude Code tool use approval prompt
    "Allow once",
    "Allow always",
    // NOTE: the bare "❯" idle prompt is handled separately in the
    // detection loop via `contains("❯")`. This catches both the idle
    // prompt ("❯" after trim) and the prompt with user-typed text
    // ("❯ implement…").
];

/// OpenCode patterns that indicate the agent is waiting for input.
const OPENCODE_AWAITING_PATTERNS: &[&str] = &[
    "Do you want to",
    "(y/n)",
    "(Y/n)",
    "approve",
    "Approve",
    "Accept?",
    "> ",
];

/// Classify the session state by inspecting terminal output patterns.
/// Returns `None` if no strong signal was detected (caller should keep
/// the current state).
pub fn detect_session_state<T: EventListener>(
    term: &Term<T>,
    agent: AgentKind,
) -> Option<SessionState> {
    let lines = visible_lines(term);
    if lines.is_empty() {
        return None;
    }

    // Look at the bottom-most non-empty lines for patterns.
    let non_empty: Vec<&str> = lines.iter().map(|s| s.as_str()).filter(|s| !s.is_empty()).collect();
    if non_empty.is_empty() {
        return None;
    }

    let patterns = match agent {
        AgentKind::Claude => CLAUDE_AWAITING_PATTERNS,
        AgentKind::Opencode => OPENCODE_AWAITING_PATTERNS,
        AgentKind::Bare => return None, // No pattern detection for bare shell
    };

    // Check the last CHECK_NON_EMPTY non-empty lines for awaiting patterns.
    let check_count = non_empty.len().min(CHECK_NON_EMPTY);
    for line in &non_empty[non_empty.len() - check_count..] {
        for pattern in patterns {
            if line.contains(pattern) {
                return Some(SessionState::Awaiting);
            }
        }
        // Claude Code idle prompt: `❯` appears at the prompt line.
        // Use `contains` instead of `ends_with` so we also detect the
        // prompt when the user has started typing ("❯ implement…") but
        // hasn't submitted yet — the agent is still waiting for input.
        if matches!(agent, AgentKind::Claude) && line.contains("❯") {
            return Some(SessionState::Awaiting);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_awaiting_patterns_not_empty() {
        assert!(!CLAUDE_AWAITING_PATTERNS.is_empty());
    }

    #[test]
    fn opencode_awaiting_patterns_not_empty() {
        assert!(!OPENCODE_AWAITING_PATTERNS.is_empty());
    }
}
