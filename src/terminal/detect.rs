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

/// Number of bottom lines to inspect for prompt patterns.
const INSPECT_LINES: usize = 8;

/// Extract the text content of the last `INSPECT_LINES` rows from the
/// terminal grid. Trailing whitespace on each line is trimmed.
fn last_lines<T: EventListener>(term: &Term<T>, count: usize) -> Vec<String> {
    let grid = term.grid();
    let total_rows = grid.screen_lines();
    let cols = grid.columns();
    let start = total_rows.saturating_sub(count);
    let mut lines = Vec::with_capacity(count);
    for row in start..total_rows {
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
    // detection loop via `ends_with("❯")` because `last_lines` calls
    // `trim_end()` on each row, stripping the trailing space from "❯ ".
    // Using `contains("❯ ")` would only match lines where the user has
    // already typed text (e.g. "❯ implement...") — not the idle prompt.
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
    let lines = last_lines(term, INSPECT_LINES);
    if lines.is_empty() {
        return None;
    }

    // Look at the last few non-empty lines for patterns.
    let non_empty: Vec<&str> = lines.iter().map(|s| s.as_str()).filter(|s| !s.is_empty()).collect();
    if non_empty.is_empty() {
        return None;
    }

    let patterns = match agent {
        AgentKind::Claude => CLAUDE_AWAITING_PATTERNS,
        AgentKind::Opencode => OPENCODE_AWAITING_PATTERNS,
        AgentKind::Bare => return None, // No pattern detection for bare shell
    };

    // Check the last 3 non-empty lines for awaiting patterns.
    let check_count = non_empty.len().min(3);
    for line in &non_empty[non_empty.len() - check_count..] {
        for pattern in patterns {
            if line.contains(pattern) {
                return Some(SessionState::Awaiting);
            }
        }
        // Claude Code idle prompt: after trim_end(), "❯ " becomes "❯".
        // Match only at end-of-line to avoid false positives from lines
        // where the user already typed text ("❯ implement...").
        if matches!(agent, AgentKind::Claude) && line.ends_with("❯") {
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
