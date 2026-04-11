//! `EditorBuffer` — a syntect-highlighted, ropey-backed text buffer.
//!
//! Owns:
//! - the file path (None for scratch buffers)
//! - a `ropey::Rope` as the text storage
//! - the syntect `SyntaxReference` inferred from the file extension
//! - a `dirty` flag that flips on every mutation and clears on save
//!
//! Line highlighting is lazy — callers request highlighted spans for a
//! specific line via `highlight_line(idx)` and get back a `Vec<HlSpan>`
//! with precomputed colours for each token.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ropey::Rope;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

/// One highlighted span inside a single line. Colours are already
/// resolved to 8-bit RGB so the Slint renderer can drop them straight
/// into a `SharedPixelBuffer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlSpan {
    pub text: String,
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub bold: bool,
    pub italic: bool,
}

pub struct EditorBuffer {
    pub path: Option<PathBuf>,
    pub rope: Rope,
    pub syntax_name: String,
    pub dirty: bool,
    syntax_set: SyntaxSet,
    theme_set: ThemeSet,
    theme_name: String,
}

impl EditorBuffer {
    /// Empty scratch buffer — useful for tests and new-file scenarios.
    pub fn empty() -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme_set = ThemeSet::load_defaults();
        Self {
            path: None,
            rope: Rope::new(),
            syntax_name: "Plain Text".into(),
            dirty: false,
            syntax_set,
            theme_set,
            theme_name: "base16-ocean.dark".into(),
        }
    }

    /// Load a file from disk into a new buffer. Detects the syntax from
    /// the file extension; falls back to plain text.
    pub fn open(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("read {}", path.display()))?;
        let mut buf = Self::empty();
        buf.rope = Rope::from_str(&content);
        buf.path = Some(path.to_path_buf());

        // Detect syntax by file extension.
        if let Some(ext) = path.extension().and_then(|e| e.to_str())
            && let Some(syntax) = buf.syntax_set.find_syntax_by_extension(ext)
        {
            buf.syntax_name = syntax.name.clone();
        }
        buf.dirty = false;
        Ok(buf)
    }

    /// Write the buffer back to disk at the associated path. Fails if
    /// the buffer has no path (scratch / unsaved-never).
    pub fn save(&mut self) -> Result<()> {
        let path = self
            .path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("buffer has no path — use save_as"))?;
        fs::write(path, self.rope.to_string())
            .with_context(|| format!("write {}", path.display()))?;
        self.dirty = false;
        Ok(())
    }

    /// Replace the entire contents of the buffer with new text. Marks
    /// dirty. Used by the Slint TextEdit binding.
    pub fn replace_all(&mut self, new_text: &str) {
        if self.rope.to_string() == new_text {
            return;
        }
        self.rope = Rope::from_str(new_text);
        self.dirty = true;
    }

    /// Number of lines in the buffer.
    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    /// Iterate lines as `String`s. Useful for simple renderers.
    pub fn line_at(&self, idx: usize) -> String {
        self.rope.line(idx).to_string()
    }

    /// Total character count (UTF-8 code points, not bytes).
    pub fn char_count(&self) -> usize {
        self.rope.len_chars()
    }

    fn syntax(&self) -> &SyntaxReference {
        self.syntax_set
            .find_syntax_by_name(&self.syntax_name)
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text())
    }

    /// Highlight one specific line (0-indexed). Returns an empty vec
    /// for out-of-range indices.
    ///
    /// For correctness, syntect's `HighlightLines` needs the preceding
    /// lines to establish parser state — this helper walks from the
    /// start of the file to the target line on every call. For typical
    /// files that's fine (<1ms for 1000-line files); Phase 7.5 will
    /// cache the parse state per line if it turns into a bottleneck.
    pub fn highlight_line(&self, target: usize) -> Vec<HlSpan> {
        if target >= self.line_count() {
            return Vec::new();
        }
        let theme = match self.theme_set.themes.get(&self.theme_name) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let syntax = self.syntax();
        let mut highlighter = HighlightLines::new(syntax, theme);

        let full_text = self.rope.to_string();
        let mut spans: Vec<HlSpan> = Vec::new();
        for (i, line) in LinesWithEndings::from(&full_text).enumerate() {
            let ranges: Vec<(Style, &str)> = match highlighter.highlight_line(line, &self.syntax_set) {
                Ok(v) => v,
                Err(_) => return Vec::new(),
            };
            if i == target {
                spans = ranges
                    .into_iter()
                    .map(|(style, text)| HlSpan {
                        text: text.trim_end_matches('\n').to_string(),
                        r: style.foreground.r,
                        g: style.foreground.g,
                        b: style.foreground.b,
                        bold: style
                            .font_style
                            .contains(syntect::highlighting::FontStyle::BOLD),
                        italic: style
                            .font_style
                            .contains(syntect::highlighting::FontStyle::ITALIC),
                    })
                    .collect();
                break;
            }
        }
        spans
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn open_and_read_plain_text() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("note.txt");
        File::create(&path).unwrap().write_all(b"hello\nworld\n").unwrap();

        let buf = EditorBuffer::open(&path).unwrap();
        assert_eq!(buf.line_count(), 3); // "hello\n", "world\n", ""
        assert_eq!(buf.line_at(0).trim_end(), "hello");
        assert_eq!(buf.line_at(1).trim_end(), "world");
        assert!(!buf.dirty);
        assert_eq!(buf.path.as_deref(), Some(path.as_path()));
    }

    #[test]
    fn save_writes_and_clears_dirty() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("out.txt");
        let mut buf = EditorBuffer::empty();
        buf.path = Some(path.clone());
        buf.replace_all("persisted\n");
        assert!(buf.dirty);
        buf.save().unwrap();
        assert!(!buf.dirty);
        let round_trip = fs::read_to_string(&path).unwrap();
        assert_eq!(round_trip, "persisted\n");
    }

    #[test]
    fn replace_all_is_no_op_when_identical() {
        let mut buf = EditorBuffer::empty();
        buf.replace_all("x");
        buf.dirty = false; // reset
        buf.replace_all("x");
        assert!(!buf.dirty, "no-op replace should not flip dirty");
    }

    #[test]
    fn highlight_rust_keyword() {
        // Build a buffer with a Rust extension to force the Rust syntax.
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("sample.rs");
        File::create(&path)
            .unwrap()
            .write_all(b"fn main() { let x = 1; }\n")
            .unwrap();

        let buf = EditorBuffer::open(&path).unwrap();
        let spans = buf.highlight_line(0);
        assert!(!spans.is_empty(), "Rust highlighting should produce spans");
        // At least one span should contain the `fn` keyword text.
        assert!(spans.iter().any(|s| s.text.contains("fn")));
    }

    #[test]
    fn highlight_line_out_of_range_is_empty() {
        let buf = EditorBuffer::empty();
        assert!(buf.highlight_line(999).is_empty());
    }
}
