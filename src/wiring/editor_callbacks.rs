//! Phase 6 / Polish 17 — file tree click handler plus Phase 7 inline
//! editor (content changed / save / close). Grouped together because
//! the file tree click either opens a file inside the editor or
//! toggles a directory's expansion state, so the two are one cohesive
//! piece of UX.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use slint::{ComponentHandle, SharedString, VecModel};

use crate::editor::EditorBuffer;
use crate::wiring::context::WiringContext;
use crate::wiring::helpers::{is_likely_binary, rebuild_editor_highlight};
use crate::{HighlightedLineData, MainWindow};

pub fn wire(
    window: &MainWindow,
    ctx: &WiringContext,
    editor_buffer: Rc<RefCell<Option<EditorBuffer>>>,
    editor_lines_model: Rc<VecModel<HighlightedLineData>>,
) {
    wire_file_entry_clicked(window, ctx, editor_buffer.clone(), editor_lines_model.clone());
    wire_editor_content_changed(window, editor_buffer.clone());
    wire_editor_save(window, ctx, editor_buffer.clone(), editor_lines_model.clone());
    wire_editor_close(window, editor_buffer);
}

/// Click handler for file tree entries.
/// Polish 17:
/// - Directory → toggle its presence in `state.expanded_dirs` and
///   rebuild the flattened tree model.
/// - File → try to open inline in the editor. If the file is binary
///   or too large (>1 MB), fall back to `$EDITOR`.
fn wire_file_entry_clicked(
    window: &MainWindow,
    ctx: &WiringContext,
    editor_buffer: Rc<RefCell<Option<EditorBuffer>>>,
    editor_lines_model: Rc<VecModel<HighlightedLineData>>,
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
                                w.set_editor_file_path(
                                    path.to_string_lossy().into_owned().into(),
                                );
                                w.set_editor_file_content(buf.rope.to_string().into());
                                w.set_editor_syntax_name(buf.syntax_name.clone().into());
                                w.set_editor_file_dirty(false);
                                w.set_editor_line_count(buf.line_count() as i32);
                                w.set_editor_open(true);
                            }
                            // Polish 5: populate the coloured preview.
                            rebuild_editor_highlight(&buf, &editor_lines_model);
                            *editor_buffer.borrow_mut() = Some(buf);
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

/// Editor — content changed live, tracks dirty flag.
fn wire_editor_content_changed(
    window: &MainWindow,
    editor_buffer: Rc<RefCell<Option<EditorBuffer>>>,
) {
    let weak = window.as_weak();
    window.on_editor_content_changed(move |new_text| {
        if let Some(buf) = editor_buffer.borrow_mut().as_mut() {
            buf.replace_all(new_text.as_str());
            if let Some(w) = weak.upgrade() {
                w.set_editor_file_dirty(buf.dirty);
            }
        }
    });
}

/// Editor — save. On success, rebuilds the coloured preview so it
/// reflects the on-disk state; on failure surfaces via toast.
fn wire_editor_save(
    window: &MainWindow,
    ctx: &WiringContext,
    editor_buffer: Rc<RefCell<Option<EditorBuffer>>>,
    editor_lines_model: Rc<VecModel<HighlightedLineData>>,
) {
    let weak = window.as_weak();
    let toast = ctx.show_toast.clone();
    window.on_editor_save(move || {
        let mut borrow = editor_buffer.borrow_mut();
        let Some(buf) = borrow.as_mut() else { return };
        match buf.save() {
            Ok(()) => {
                if let Some(w) = weak.upgrade() {
                    w.set_editor_file_dirty(false);
                    w.set_editor_line_count(buf.line_count() as i32);
                }
                // Polish 5: refresh the coloured preview to reflect
                // the on-disk state.
                rebuild_editor_highlight(buf, &editor_lines_model);
                tracing::info!(
                    path = %buf.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
                    "editor save ok"
                );
                toast("success", "File saved".to_string());
            }
            Err(err) => {
                tracing::warn!(%err, "editor save failed");
                toast("error", format!("Save failed: {err}"));
            }
        }
    });
}

/// Editor — close. Clears the buffer and all editor Slint properties
/// so the Files tab returns to the directory listing view.
fn wire_editor_close(
    window: &MainWindow,
    editor_buffer: Rc<RefCell<Option<EditorBuffer>>>,
) {
    let weak = window.as_weak();
    window.on_editor_close(move || {
        *editor_buffer.borrow_mut() = None;
        if let Some(w) = weak.upgrade() {
            w.set_editor_open(false);
            w.set_editor_file_path(SharedString::from(""));
            w.set_editor_file_content(SharedString::from(""));
            w.set_editor_file_dirty(false);
        }
    });
}
