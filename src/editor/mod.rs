//! In-app text editor — Phase 7.
//!
//! The editor stack is intentionally minimal: `ropey` for the text
//! buffer (so edits to large files stay O(log n)) and `syntect` for
//! syntax highlighting with bundled TextMate grammars. Neither has C
//! dependencies — the whole thing stays pure Rust and cross-platform.
//!
//! See [`buffer::EditorBuffer`] for the core type. The Slint-side
//! widget that renders highlighted lines lives in `src/main.rs` /
//! `ui/main.slint` alongside the Files tab; it opens files from the
//! file browser click handler.

#![allow(dead_code)]

pub mod buffer;

pub use buffer::{EditorBuffer, HlSpan};
