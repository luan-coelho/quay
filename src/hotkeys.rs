//! Pure hotkey classification — no Slint, no AppState, no I/O.
//!
//! [`classify_hotkey`] takes the raw key event payload that Slint hands us
//! (`text`, `ctrl`, `alt`, `shift`, `meta`) and decides which abstract
//! [`HotkeyAction`] should fire. The dispatch into actual side effects
//! (creating tasks, closing modals, etc.) lives in `main.rs` because it
//! needs access to the `MainWindow` and `AppState` — but the *decision*
//! is here, isolated and unit-testable in microseconds.
//!
//! This split was extracted from the ~180 line `on_key_pressed` closure
//! in `main.rs`. The logic is preserved verbatim from the original
//! handler so any quirks (e.g. shift-aware branches that the leading
//! `match` short-circuits) carry over unchanged. Quirks are flagged with
//! comments where they exist.

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum HotkeyAction {
    /// Cmd/Ctrl + Alt + digit (1..=9). The payload is the 0-based slot
    /// index, so `1` → `QuickAction(0)`.
    QuickAction(usize),
    /// Escape pressed. The dispatcher in `main.rs` decides which modal
    /// gets dismissed (or falls through to the PTY if none is open).
    CloseTopModal,
    /// Cmd/Ctrl + N — start a new CLI session (terminal-first).
    CreateTask,
    /// Cmd/Ctrl + , — toggle the Settings modal.
    ToggleSettings,
    /// Cmd/Ctrl + W — close the active open-task tab.
    CloseActiveTab,
    /// Cmd/Ctrl + P — open the task quick-switcher.
    OpenTaskSearch,
    /// Cmd/Ctrl + Shift + ? — toggle the keyboard shortcuts overlay.
    ToggleShortcuts,
    /// Cmd/Ctrl + Shift + W — close all *other* open task tabs.
    ///
    /// NOTE: in the current dispatch order, the bare-`W` branch wins
    /// before this one is consulted, making this variant effectively
    /// unreachable from `classify_hotkey`. Preserved for documentation
    /// and future bug-fix work.
    CloseOtherTabs,
    /// Cmd/Ctrl + Alt + W — close every open task tab.
    CloseAllTabs,
    /// Cmd/Ctrl + Shift + ] (or `}`) — cycle to the next open tab.
    CycleTabsForward,
    /// Cmd/Ctrl + Shift + [ (or `{`) — cycle to the previous open tab.
    CycleTabsBackward,
    /// Ctrl+Shift+V or Shift+Insert — paste clipboard into the PTY.
    Paste,
    /// Not a recognised hotkey — caller should encode and forward to
    /// the active PTY.
    Fallthrough,
}

/// Map a raw key event to a [`HotkeyAction`].
///
/// The branch order matters: it mirrors the original `on_key_pressed`
/// flow so refactoring stays behaviour-compatible. In particular, the
/// "primary + letter" branch (3) intentionally does *not* check `shift`,
/// because the original handler did not — preserving that quirk keeps
/// the refactor mechanical.
pub fn classify_hotkey(
    text: &str,
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
) -> HotkeyAction {
    let primary = ctrl || meta;

    // 1. Quick action shortcut: primary + Alt + digit (1..=9).
    if primary && alt && text.len() == 1 {
        let c = text.chars().next().unwrap_or(' ');
        if let Some(digit) = c.to_digit(10)
            && (1..=9).contains(&digit)
        {
            return HotkeyAction::QuickAction((digit - 1) as usize);
        }
    }

    // 2. Escape — close any open modal (or fall through to PTY).
    if text == "\u{001b}" {
        return HotkeyAction::CloseTopModal;
    }

    // 2.5. Clipboard paste: Ctrl+Shift+V or Shift+Insert.
    if primary && shift && (text == "v" || text == "V") {
        return HotkeyAction::Paste;
    }
    if shift && text == "\u{F727}" {
        return HotkeyAction::Paste;
    }

    // 3. Global primary-modifier shortcuts (no Alt).
    //
    // NOTE: this branch does NOT check `shift`. Cmd+Shift+W therefore
    // matches `'W'` here and is reported as `CloseActiveTab` rather than
    // reaching branch 5 (`CloseOtherTabs`). Preserved verbatim from the
    // original handler — fixing this is a separate behavioural change.
    if primary && !alt && text.len() == 1 {
        match text.chars().next().unwrap_or('\0') {
            'n' | 'N' => return HotkeyAction::CreateTask,
            ',' => return HotkeyAction::ToggleSettings,
            'w' | 'W' => return HotkeyAction::CloseActiveTab,
            'p' | 'P' => return HotkeyAction::OpenTaskSearch,
            _ => {}
        }
    }

    // 4. Cmd+Shift+? — toggle the shortcuts overlay. Listed before the
    // bracket cycle so a future bracket-related shortcut can't shadow it.
    if primary && shift && text == "?" {
        return HotkeyAction::ToggleShortcuts;
    }

    // 5. Cmd+Shift+W — close OTHER open tabs.
    // (Currently unreachable — see note on `CloseOtherTabs`.)
    if primary && shift && (text == "W" || text == "w") {
        return HotkeyAction::CloseOtherTabs;
    }

    // 6. Cmd+Alt+W — close ALL open tabs.
    if primary && alt && (text == "w" || text == "W") {
        return HotkeyAction::CloseAllTabs;
    }

    // 7. Cmd+Shift+] / Cmd+Shift+[ — cycle through open tabs. Shift
    // turns `[`→`{` and `]`→`}` on most layouts, so we accept both
    // forms.
    if primary && shift && text.len() == 1 {
        let c = text.chars().next().unwrap_or('\0');
        if c == '}' || c == ']' {
            return HotkeyAction::CycleTabsForward;
        }
        if c == '{' || c == '[' {
            return HotkeyAction::CycleTabsBackward;
        }
    }

    HotkeyAction::Fallthrough
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper makes the call sites read like the keypress they describe.
    fn key(text: &str, primary: bool, alt: bool, shift: bool) -> HotkeyAction {
        // `primary` collapses ctrl+meta — it doesn't matter which one
        // the platform reports as long as classify_hotkey treats them
        // the same.
        classify_hotkey(text, primary, alt, shift, false)
    }

    #[test]
    fn quick_action_digit_1_through_9() {
        for d in 1..=9u32 {
            let text = d.to_string();
            assert_eq!(
                key(&text, true, true, false),
                HotkeyAction::QuickAction((d - 1) as usize),
                "Cmd+Alt+{d} should map to QuickAction({})",
                d - 1
            );
        }
    }

    #[test]
    fn quick_action_zero_is_not_a_slot() {
        assert_eq!(key("0", true, true, false), HotkeyAction::Fallthrough);
    }

    #[test]
    fn quick_action_requires_primary_and_alt() {
        // Primary alone — no quick action.
        assert_eq!(key("3", true, false, false), HotkeyAction::Fallthrough);
        // Alt alone — no quick action.
        assert_eq!(key("3", false, true, false), HotkeyAction::Fallthrough);
        // Neither — no quick action.
        assert_eq!(key("3", false, false, false), HotkeyAction::Fallthrough);
    }

    #[test]
    fn escape_closes_top_modal() {
        assert_eq!(
            key("\u{001b}", false, false, false),
            HotkeyAction::CloseTopModal
        );
    }

    #[test]
    fn escape_with_modifiers_still_closes_modal() {
        // Original handler ignored modifiers on Escape — preserved.
        assert_eq!(
            key("\u{001b}", true, false, false),
            HotkeyAction::CloseTopModal
        );
    }

    #[test]
    fn cmd_n_creates_task() {
        assert_eq!(key("n", true, false, false), HotkeyAction::CreateTask);
        assert_eq!(key("N", true, false, false), HotkeyAction::CreateTask);
    }

    #[test]
    fn cmd_comma_toggles_settings() {
        assert_eq!(key(",", true, false, false), HotkeyAction::ToggleSettings);
    }

    #[test]
    fn cmd_w_closes_active_tab() {
        assert_eq!(key("w", true, false, false), HotkeyAction::CloseActiveTab);
        assert_eq!(key("W", true, false, false), HotkeyAction::CloseActiveTab);
    }

    #[test]
    fn cmd_p_opens_task_search() {
        assert_eq!(key("p", true, false, false), HotkeyAction::OpenTaskSearch);
        assert_eq!(key("P", true, false, false), HotkeyAction::OpenTaskSearch);
    }

    #[test]
    fn cmd_shift_question_toggles_shortcuts() {
        assert_eq!(
            key("?", true, false, true),
            HotkeyAction::ToggleShortcuts
        );
    }

    #[test]
    fn cmd_alt_w_closes_all_tabs() {
        assert_eq!(key("w", true, true, false), HotkeyAction::CloseAllTabs);
        assert_eq!(key("W", true, true, false), HotkeyAction::CloseAllTabs);
    }

    #[test]
    fn cmd_shift_close_bracket_cycles_forward() {
        assert_eq!(
            key("}", true, false, true),
            HotkeyAction::CycleTabsForward
        );
        assert_eq!(
            key("]", true, false, true),
            HotkeyAction::CycleTabsForward
        );
    }

    #[test]
    fn cmd_shift_open_bracket_cycles_backward() {
        assert_eq!(
            key("{", true, false, true),
            HotkeyAction::CycleTabsBackward
        );
        assert_eq!(
            key("[", true, false, true),
            HotkeyAction::CycleTabsBackward
        );
    }

    #[test]
    fn ctrl_shift_v_pastes() {
        assert_eq!(key("V", true, false, true), HotkeyAction::Paste);
        assert_eq!(key("v", true, false, true), HotkeyAction::Paste);
    }

    #[test]
    fn shift_insert_pastes() {
        assert_eq!(key("\u{F727}", false, false, true), HotkeyAction::Paste);
    }

    #[test]
    fn ctrl_v_without_shift_falls_through() {
        // Ctrl+V (no shift) should send raw 0x16 to PTY, not paste.
        assert_eq!(key("v", true, false, false), HotkeyAction::Fallthrough);
    }

    #[test]
    fn plain_letter_falls_through() {
        assert_eq!(key("a", false, false, false), HotkeyAction::Fallthrough);
        assert_eq!(key("z", false, false, false), HotkeyAction::Fallthrough);
    }

    #[test]
    fn unknown_primary_letter_falls_through() {
        // Cmd+G is not a hotkey we know about.
        assert_eq!(key("g", true, false, false), HotkeyAction::Fallthrough);
    }

    #[test]
    fn empty_text_falls_through() {
        assert_eq!(key("", false, false, false), HotkeyAction::Fallthrough);
        assert_eq!(key("", true, true, true), HotkeyAction::Fallthrough);
    }

    #[test]
    fn ctrl_and_meta_are_equivalent_primary() {
        // Ctrl+N (Linux/Windows) and Cmd+N (macOS) should both reach
        // the CreateTask handler.
        assert_eq!(
            classify_hotkey("n", true, false, false, false),
            HotkeyAction::CreateTask
        );
        assert_eq!(
            classify_hotkey("n", false, false, false, true),
            HotkeyAction::CreateTask
        );
    }
}
