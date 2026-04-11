//! Replay a persisted PTY byte log into a fresh `alacritty_terminal::Term`.
//!
//! Used by `PtySession` to reconstruct the visible scrollback from a previous
//! run of the same task, before the new PTY child process takes over.

use std::fs;
use std::path::Path;

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::vte::ansi::Processor;

/// Read the entire byte log from `path` and feed it into `term` via `processor`.
/// Returns the number of bytes replayed. Missing files are treated as empty.
pub fn replay_log(
    path: &Path,
    processor: &mut Processor,
    term: &mut Term<VoidListener>,
) -> std::io::Result<usize> {
    match fs::read(path) {
        Ok(bytes) => {
            processor.advance(term, &bytes);
            Ok(bytes.len())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(err) => Err(err),
    }
}
