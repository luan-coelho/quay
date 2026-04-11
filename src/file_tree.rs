//! File browser for the Files tab in the right pane.
//!
//! Very simple model: the user navigates one directory at a time
//! (flat listing with parent `..` entry), and clicking a file shells
//! out to `$EDITOR` with the file path. Phase 7 will replace the
//! $EDITOR shellout with an in-app editor built on top of
//! [`crate::editor`].
//!
//! Default-ignored patterns match the common `node_modules`, `.git`,
//! `target`, `dist`, `build` conventions so dev directories don't
//! drown the user in noise.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    File,
    Parent,
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub kind: EntryKind,
}

/// Names ignored by the default list view. Matches Lanes' default-
/// collapsed directories.
const IGNORED_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    "dist",
    "build",
    ".next",
    ".turbo",
    ".idea",
    ".vscode",
    "__pycache__",
];

/// List the immediate children of `dir`, sorted (directories first,
/// then files, both alphabetically). Includes a synthetic `..` entry
/// pointing to the parent directory when applicable. Ignored
/// directories are filtered out.
pub fn list_dir(dir: &Path) -> Result<Vec<FileEntry>> {
    let mut entries: Vec<FileEntry> = Vec::new();

    // Parent entry first so it always shows at the top of the list.
    if let Some(parent) = dir.parent() {
        entries.push(FileEntry {
            name: "..".into(),
            path: parent.to_path_buf(),
            kind: EntryKind::Parent,
        });
    }

    let iter = fs::read_dir(dir)
        .with_context(|| format!("read_dir {}", dir.display()))?;

    let mut files: Vec<FileEntry> = Vec::new();
    let mut dirs: Vec<FileEntry> = Vec::new();

    for entry in iter {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().into_owned();

        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        if file_type.is_dir() {
            if IGNORED_DIRS.iter().any(|ign| ign == &name) {
                continue;
            }
            dirs.push(FileEntry {
                name,
                path: entry.path(),
                kind: EntryKind::Directory,
            });
        } else if file_type.is_file() {
            files.push(FileEntry {
                name,
                path: entry.path(),
                kind: EntryKind::File,
            });
        }
    }

    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    files.sort_by(|a, b| a.name.cmp(&b.name));

    entries.extend(dirs);
    entries.extend(files);
    Ok(entries)
}

/// Open a file in the user's preferred external editor.
///
/// Resolution order:
/// 1. `$EDITOR` environment variable
/// 2. `$VISUAL` environment variable
/// 3. Platform fallback: `xdg-open` (Linux) / `open` (macOS) /
///    `cmd /c start` (Windows).
///
/// Spawns the command detached (we don't wait or capture output) so
/// Quay's UI thread stays responsive. Errors are returned but non-fatal
/// for the caller.
pub fn open_in_editor(file: &Path) -> Result<()> {
    if let Ok(editor) = std::env::var("EDITOR").or_else(|_| std::env::var("VISUAL"))
        && !editor.is_empty()
    {
        // `$EDITOR` may contain arguments, e.g. `code --wait`. Split on
        // whitespace so the binary and its args land in the right slots.
        let mut parts = editor.split_whitespace();
        let binary = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("empty $EDITOR value"))?;
        let mut cmd = Command::new(binary);
        for arg in parts {
            cmd.arg(arg);
        }
        cmd.arg(file);
        cmd.spawn()
            .with_context(|| format!("spawn {binary} {}", file.display()))?;
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    let opener = "xdg-open";
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(target_os = "windows")]
    let opener = "cmd";

    #[cfg(not(target_os = "windows"))]
    {
        Command::new(opener)
            .arg(file)
            .spawn()
            .with_context(|| format!("spawn {opener} {}", file.display()))?;
    }
    #[cfg(target_os = "windows")]
    {
        Command::new(opener)
            .args(["/c", "start", ""])
            .arg(file)
            .spawn()
            .with_context(|| format!("spawn cmd /c start {}", file.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    #[test]
    fn list_dir_skips_ignored() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("src")).unwrap();
        fs::create_dir(root.join("node_modules")).unwrap();
        fs::create_dir(root.join("target")).unwrap();
        File::create(root.join("README.md")).unwrap();

        let entries = list_dir(root).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        // The parent ".." entry is always first (our tempdir has a parent).
        assert_eq!(names[0], "..");
        assert!(names.contains(&"src"));
        assert!(names.contains(&"README.md"));
        assert!(!names.contains(&"node_modules"));
        assert!(!names.contains(&"target"));
    }

    #[test]
    fn list_dir_sorts_dirs_before_files() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("b_dir")).unwrap();
        File::create(root.join("a_file.txt")).unwrap();

        let entries = list_dir(root).unwrap();
        // Skip the parent entry, then dirs before files.
        let after_parent: Vec<&FileEntry> = entries.iter().skip(1).collect();
        assert_eq!(after_parent[0].name, "b_dir");
        assert_eq!(after_parent[0].kind, EntryKind::Directory);
        assert_eq!(after_parent[1].name, "a_file.txt");
        assert_eq!(after_parent[1].kind, EntryKind::File);
    }

    #[test]
    fn list_dir_returns_parent_when_possible() {
        let tmp = tempdir().unwrap();
        let entries = list_dir(tmp.path()).unwrap();
        assert!(entries.iter().any(|e| e.kind == EntryKind::Parent));
    }
}
