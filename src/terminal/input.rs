//! Translate Slint `KeyEvent.text` values into the byte sequences an
//! xterm-compatible terminal expects.
//!
//! Slint delivers special keys (arrows, Home, PageUp, etc.) as single chars in
//! the Unicode Private Use Area — codepoints defined in
//! `i-slint-common/key_codes.rs`. We match on those and emit the corresponding
//! CSI escape sequences, and pass anything else through as raw UTF-8.

/// Map a Slint `KeyEvent.text` string + modifier flags into the bytes that
/// should be written to the PTY master. Returns an empty vector if the event
/// should be ignored (e.g. pure modifier presses carry no printable text).
pub fn key_text_to_bytes(text: &str, ctrl: bool, alt: bool, _shift: bool) -> Vec<u8> {
    if text.is_empty() {
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

    // Inspect the first codepoint for special-key handling. Slint sends each
    // special key as a single char in the 0xF700+ range.
    let mut chars = text.chars();
    let first = chars.next().expect("non-empty text above");
    if chars.next().is_none() {
        match first {
            // ASCII control codes Slint happens to reuse directly.
            '\u{0008}' => return vec![0x7f],        // Backspace  -> DEL
            '\u{0009}' => return vec![b'\t'],       // Tab
            '\u{000a}' => return vec![b'\r'],       // Return     -> CR
            '\u{001b}' => return vec![0x1b],        // Escape
            '\u{007f}' => return b"\x1b[3~".to_vec(), // Delete key

            // Private Use Area — arrow keys.
            '\u{F700}' => return b"\x1b[A".to_vec(), // UpArrow
            '\u{F701}' => return b"\x1b[B".to_vec(), // DownArrow
            '\u{F702}' => return b"\x1b[D".to_vec(), // LeftArrow
            '\u{F703}' => return b"\x1b[C".to_vec(), // RightArrow

            // Navigation.
            '\u{F729}' => return b"\x1b[H".to_vec(),  // Home
            '\u{F72B}' => return b"\x1b[F".to_vec(),  // End
            '\u{F72C}' => return b"\x1b[5~".to_vec(), // PageUp
            '\u{F72D}' => return b"\x1b[6~".to_vec(), // PageDown

            _ => {}
        }
    }

    // Fall-through: treat as normal text input. UTF-8 bytes straight to the
    // PTY, which is what any xterm-class terminal expects for both ASCII and
    // multibyte characters.
    text.as_bytes().to_vec()
}
