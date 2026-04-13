//! Translate Slint `KeyEvent.text` values into the byte sequences an
//! xterm-compatible terminal expects.
//!
//! Slint delivers special keys (arrows, Home, PageUp, etc.) as single chars in
//! the Unicode Private Use Area — codepoints defined in
//! `i-slint-common/key_codes.rs`. We match on those and emit the corresponding
//! CSI escape sequences, and pass anything else through as raw UTF-8.
//!
//! Modifier-only key presses (Shift, Ctrl, Alt, Meta, etc.) live in the
//! U+0010..U+0018 range and are filtered out — forwarding them would inject
//! raw control codes into the PTY (e.g. Shift = 0x10 = DLE, which readline
//! interprets as Ctrl+P → "previous command").

/// Map a Slint `KeyEvent.text` string + modifier flags into the bytes that
/// should be written to the PTY master. Returns an empty vector if the event
/// should be ignored (e.g. pure modifier presses carry no printable text).
pub fn key_text_to_bytes(text: &str, ctrl: bool, alt: bool, shift: bool) -> Vec<u8> {
    if text.is_empty() {
        return Vec::new();
    }

    let first = text.chars().next().expect("non-empty text above");
    let single_char = text.chars().count() == 1;

    // ── Modifier-only presses — must not reach the PTY ──────────────
    // Slint encodes bare modifier keys as single chars in U+0010..U+0018:
    //   0x10=Shift  0x11=Control  0x12=Alt  0x13=AltGr  0x14=CapsLock
    //   0x15=ShiftR 0x16=ControlR 0x17=Meta 0x18=MetaR
    if single_char && ('\u{0010}'..='\u{0018}').contains(&first) {
        return Vec::new();
    }

    // Ctrl + ASCII letter/digit → control code. Covers Ctrl+C, Ctrl+D, Ctrl+Z,
    // etc. which are the minimum needed to interrupt a running agent.
    if ctrl && text.len() == 1 {
        let byte = text.as_bytes()[0];
        if byte.is_ascii_alphabetic() {
            return vec![byte.to_ascii_uppercase() - b'@'];
        }
        // A few common non-letter Ctrl combinations.
        match byte {
            b'@' | b' ' => return vec![0x00], // Ctrl+Space / Ctrl+@
            b'[' => return vec![0x1b],        // Ctrl+[ → ESC
            b'\\' => return vec![0x1c],
            b']' => return vec![0x1d],
            b'^' => return vec![0x1e],
            b'_' => return vec![0x1f],
            _ => {}
        }
    }

    // Alt + char → ESC prefix (xterm "meta" convention).
    if alt && text.len() == 1 {
        let mut out = vec![0x1b];
        out.push(text.as_bytes()[0]);
        return out;
    }

    // ── Special keys (single-char PUA or control codes) ─────────────
    if single_char {
        // xterm modifier parameter: 1 + (shift?1:0) + (alt?2:0) + (ctrl?4:0)
        let mods = 1u8 + (shift as u8) + ((alt as u8) << 1) + ((ctrl as u8) << 2);
        let has_mods = mods > 1;

        // CSI final-byte sequence, with optional modifier parameter.
        // Unmodified: ESC [ <final>. Modified: ESC [ 1 ; <mod> <final>.
        let csi = |final_byte: u8| -> Vec<u8> {
            if has_mods {
                format!("\x1b[1;{}{}", mods, final_byte as char).into_bytes()
            } else {
                vec![0x1b, b'[', final_byte]
            }
        };
        // Tilde-style: ESC [ <num> ~ or ESC [ <num> ; <mod> ~
        let tilde = |num: u8| -> Vec<u8> {
            if has_mods {
                format!("\x1b[{};{}~", num, mods).into_bytes()
            } else {
                format!("\x1b[{}~", num).into_bytes()
            }
        };

        match first {
            // ASCII control codes Slint reuses directly.
            '\u{0008}' => return vec![0x7f],          // Backspace → DEL
            '\u{0009}' => {                            // Tab
                if shift { return b"\x1b[Z".to_vec(); }
                return vec![b'\t'];
            }
            '\u{000a}' => return vec![b'\r'],          // Return → CR
            '\u{001b}' => return vec![0x1b],           // Escape
            '\u{0019}' => return b"\x1b[Z".to_vec(),   // Backtab (Shift+Tab)
            '\u{007f}' => return tilde(3),             // Delete

            // Arrow keys.
            '\u{F700}' => return csi(b'A'),  // Up
            '\u{F701}' => return csi(b'B'),  // Down
            '\u{F702}' => return csi(b'D'),  // Left
            '\u{F703}' => return csi(b'C'),  // Right

            // Navigation.
            '\u{F727}' => return tilde(2),   // Insert
            '\u{F729}' => return csi(b'H'),  // Home
            '\u{F72B}' => return csi(b'F'),  // End
            '\u{F72C}' => return tilde(5),   // PageUp
            '\u{F72D}' => return tilde(6),   // PageDown

            // F-keys: F1–F4 use SS3 (unmodified) or CSI (modified).
            '\u{F704}' => {
                if has_mods { return format!("\x1b[1;{}P", mods).into_bytes(); }
                return b"\x1bOP".to_vec();
            }
            '\u{F705}' => {
                if has_mods { return format!("\x1b[1;{}Q", mods).into_bytes(); }
                return b"\x1bOQ".to_vec();
            }
            '\u{F706}' => {
                if has_mods { return format!("\x1b[1;{}R", mods).into_bytes(); }
                return b"\x1bOR".to_vec();
            }
            '\u{F707}' => {
                if has_mods { return format!("\x1b[1;{}S", mods).into_bytes(); }
                return b"\x1bOS".to_vec();
            }
            // F5–F12 use tilde-style sequences.
            '\u{F708}' => return tilde(15), // F5
            '\u{F709}' => return tilde(17), // F6
            '\u{F70A}' => return tilde(18), // F7
            '\u{F70B}' => return tilde(19), // F8
            '\u{F70C}' => return tilde(20), // F9
            '\u{F70D}' => return tilde(21), // F10
            '\u{F70E}' => return tilde(23), // F11
            '\u{F70F}' => return tilde(24), // F12

            _ => {}
        }
    }

    // Fall-through: treat as normal text input. UTF-8 bytes straight to the
    // PTY, which is what any xterm-class terminal expects for both ASCII and
    // multibyte characters (including composed dead-key output like á, ã, ü).
    text.as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Modifier-only filtering ─────────────────────────────────────

    #[test]
    fn shift_alone_produces_no_bytes() {
        assert!(key_text_to_bytes("\u{0010}", false, false, true).is_empty());
    }

    #[test]
    fn control_alone_produces_no_bytes() {
        assert!(key_text_to_bytes("\u{0011}", false, false, false).is_empty());
    }

    #[test]
    fn alt_alone_produces_no_bytes() {
        assert!(key_text_to_bytes("\u{0012}", false, false, false).is_empty());
    }

    #[test]
    fn all_modifier_keys_filtered() {
        for cp in '\u{0010}'..='\u{0018}' {
            let s = cp.to_string();
            assert!(
                key_text_to_bytes(&s, false, false, false).is_empty(),
                "modifier U+{:04X} should be filtered",
                cp as u32
            );
        }
    }

    // ── Basic printable input ───────────────────────────────────────

    #[test]
    fn ascii_letter() {
        assert_eq!(key_text_to_bytes("a", false, false, false), b"a");
    }

    #[test]
    fn uppercase_letter_with_shift() {
        assert_eq!(key_text_to_bytes("A", false, false, true), b"A");
    }

    #[test]
    fn accented_character() {
        assert_eq!(key_text_to_bytes("á", false, false, false), "á".as_bytes());
    }

    #[test]
    fn tilde_a() {
        assert_eq!(key_text_to_bytes("ã", false, false, false), "ã".as_bytes());
    }

    // ── Ctrl combos ─────────────────────────────────────────────────

    #[test]
    fn ctrl_c() {
        assert_eq!(key_text_to_bytes("c", true, false, false), vec![0x03]);
    }

    #[test]
    fn ctrl_d() {
        assert_eq!(key_text_to_bytes("d", true, false, false), vec![0x04]);
    }

    // ── Special keys ────────────────────────────────────────────────

    #[test]
    fn backspace() {
        assert_eq!(key_text_to_bytes("\u{0008}", false, false, false), vec![0x7f]);
    }

    #[test]
    fn tab() {
        assert_eq!(key_text_to_bytes("\u{0009}", false, false, false), vec![b'\t']);
    }

    #[test]
    fn shift_tab_produces_backtab() {
        assert_eq!(
            key_text_to_bytes("\u{0009}", false, false, true),
            b"\x1b[Z"
        );
    }

    #[test]
    fn backtab_codepoint() {
        assert_eq!(key_text_to_bytes("\u{0019}", false, false, false), b"\x1b[Z");
    }

    #[test]
    fn return_key() {
        assert_eq!(key_text_to_bytes("\u{000a}", false, false, false), vec![b'\r']);
    }

    #[test]
    fn escape() {
        assert_eq!(key_text_to_bytes("\u{001b}", false, false, false), vec![0x1b]);
    }

    #[test]
    fn delete() {
        assert_eq!(key_text_to_bytes("\u{007f}", false, false, false), b"\x1b[3~");
    }

    // ── Arrow keys ──────────────────────────────────────────────────

    #[test]
    fn arrow_up() {
        assert_eq!(key_text_to_bytes("\u{F700}", false, false, false), b"\x1b[A");
    }

    #[test]
    fn arrow_down() {
        assert_eq!(key_text_to_bytes("\u{F701}", false, false, false), b"\x1b[B");
    }

    #[test]
    fn shift_arrow_up() {
        // xterm mod = 1 + 1(shift) = 2 → ESC [ 1 ; 2 A
        assert_eq!(key_text_to_bytes("\u{F700}", false, false, true), b"\x1b[1;2A");
    }

    #[test]
    fn ctrl_shift_arrow_right() {
        // mod = 1 + 1(shift) + 4(ctrl) = 6
        assert_eq!(key_text_to_bytes("\u{F703}", true, false, true), b"\x1b[1;6C");
    }

    // ── F-keys ──────────────────────────────────────────────────────

    #[test]
    fn f1_unmodified() {
        assert_eq!(key_text_to_bytes("\u{F704}", false, false, false), b"\x1bOP");
    }

    #[test]
    fn f5_unmodified() {
        assert_eq!(key_text_to_bytes("\u{F708}", false, false, false), b"\x1b[15~");
    }

    // ── Navigation ──────────────────────────────────────────────────

    #[test]
    fn home() {
        assert_eq!(key_text_to_bytes("\u{F729}", false, false, false), b"\x1b[H");
    }

    #[test]
    fn end() {
        assert_eq!(key_text_to_bytes("\u{F72B}", false, false, false), b"\x1b[F");
    }

    #[test]
    fn insert() {
        assert_eq!(key_text_to_bytes("\u{F727}", false, false, false), b"\x1b[2~");
    }

    #[test]
    fn page_up() {
        assert_eq!(key_text_to_bytes("\u{F72C}", false, false, false), b"\x1b[5~");
    }

    // ── Empty / edge cases ──────────────────────────────────────────

    #[test]
    fn empty_text_returns_empty() {
        assert!(key_text_to_bytes("", false, false, false).is_empty());
    }
}
