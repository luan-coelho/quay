//! In-app text editor — **stub for Phase 7**.
//!
//! This module is intentionally empty in Phase 6. It exists so that
//! Phase 7's editor implementation has a predictable place to land
//! without having to refactor the tree.
//!
//! Phase 7 will introduce:
//!
//! - `EditorBuffer` — a `ropey::Rope`-backed buffer with open/save/edit
//!   operations, dirty tracking, and undo/redo.
//! - `HighlightTheme` — a small typed colour palette for token classes,
//!   sourced from `syntect`'s built-in themes.
//! - `OpenFile` — the (path, buffer, cursor, scroll) state of one tab.
//! - A Slint-side custom widget that re-uses the terminal's `GlyphAtlas`
//!   to rasterize syntax-highlighted tokens into a `SharedPixelBuffer`,
//!   so the editor and the terminal share the same rendering pipeline.
//!
//! Until then, clicking a file in the file browser shells out to
//! `$EDITOR` (see [`crate::file_tree::open_in_editor`]).

#![allow(dead_code)]
