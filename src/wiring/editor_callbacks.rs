//! File tree click handler + read-only file viewer (open / close) +
//! keyboard navigation (activate-focused). Clicking a directory toggles
//! its expansion state; clicking a file opens the syntect-coloured
//! read-only viewer. Binary or oversized files fall back to `$EDITOR`.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use slint::{ComponentHandle, Model, SharedString, VecModel};

use crate::editor::EditorBuffer;
use crate::wiring::context::WiringContext;
use crate::wiring::helpers::{is_likely_binary, rebuild_editor_highlight};
use crate::{FileEntryData, HighlightedLineData, MainWindow};

pub fn wire(
    window: &MainWindow,
    ctx: &WiringContext,
    viewer_buffer: Rc<RefCell<Option<EditorBuffer>>>,
    viewer_lines_model: Rc<VecModel<HighlightedLineData>>,
    file_entries_model: Rc<VecModel<FileEntryData>>,
) {
    wire_file_entry_clicked(window, ctx, viewer_buffer.clone(), viewer_lines_model.clone());
    wire_viewer_close(window, viewer_buffer);
    wire_activate_focused(window, file_entries_model);
}

/// Click handler for file tree entries.
/// - Directory → toggle its presence in `state.expanded_dirs` and
///   rebuild the flattened tree model.
/// - File → open read-only in the syntect-coloured viewer. If the
///   file is binary or too large (>1 MB), fall back to `$EDITOR`.
fn wire_file_entry_clicked(
    window: &MainWindow,
    ctx: &WiringContext,
    viewer_buffer: Rc<RefCell<Option<EditorBuffer>>>,
    viewer_lines_model: Rc<VecModel<HighlightedLineData>>,
) {
    let state = ctx.state.clone();
    let refresh = ctx.refresh_files.clone();
    let weak = window.as_weak();
    window.on_file_entry_clicked(move |path_str, kind_str| {
        let path = PathBuf::from(path_str.as_str());
        match kind_str.as_str() {
            "directory" => {
                let mut expanded = state.expanded_dirs.borrow_mut();
                if expanded.contains(&path) {
                    expanded.remove(&path);
                } else {
                    expanded.insert(path);
                }
                drop(expanded);
                refresh();
            }
            "file" => {
                // Open inline if readable as UTF-8 and under 1 MB.
                let open_inline = match std::fs::metadata(&path) {
                    Ok(m) => m.len() < 1_000_000 && !is_likely_binary(&path),
                    Err(_) => false,
                };
                if open_inline {
                    match EditorBuffer::open(&path) {
                        Ok(buf) => {
                            if let Some(w) = weak.upgrade() {
                                w.set_viewer_file_path(
                                    path.to_string_lossy().into_owned().into(),
                                );
                                w.set_viewer_syntax_name(buf.syntax_name.clone().into());
                                w.set_viewer_line_count(buf.line_count() as i32);
                                w.set_viewer_open(true);
                            }
                            rebuild_editor_highlight(&buf, &viewer_lines_model);
                            *viewer_buffer.borrow_mut() = Some(buf);
                            return;
                        }
                        Err(err) => {
                            tracing::warn!(
                                %err,
                                "EditorBuffer::open failed; falling back to $EDITOR"
                            );
                        }
                    }
                }
                if let Err(err) = crate::file_tree::open_in_editor(&path) {
                    tracing::warn!(%err, path = %path.display(), "open_in_editor failed");
                }
            }
            _ => {}
        }
    });
}

/// Viewer close — clears the buffer and resets all viewer Slint
/// properties so the Files tab returns to the directory listing.
fn wire_viewer_close(
    window: &MainWindow,
    viewer_buffer: Rc<RefCell<Option<EditorBuffer>>>,
) {
    let weak = window.as_weak();
    window.on_viewer_close(move || {
        *viewer_buffer.borrow_mut() = None;
        if let Some(w) = weak.upgrade() {
            w.set_viewer_open(false);
            w.set_viewer_file_path(SharedString::from(""));
        }
    });
}

/// Keyboard navigation — when the user presses Enter on a focused row
/// in the file tree, this looks up the entry at the given index in the
/// model and fires the same logic as a mouse click (toggle directory
/// or open file via `file-entry-clicked`).
fn wire_activate_focused(
    window: &MainWindow,
    file_entries_model: Rc<VecModel<FileEntryData>>,
) {
    let weak = window.as_weak();
    window.on_activate_focused(move |idx| {
        let idx = idx as usize;
        if idx >= file_entries_model.row_count() {
            return;
        }
        let entry = file_entries_model.row_data(idx);
        let Some(entry) = entry else { return };
        if let Some(w) = weak.upgrade() {
            w.invoke_file_entry_clicked(entry.path, entry.kind);
        }
    });
}
