//! File browser for the Files tab in the right pane.
//!
//! Polish 17: the earlier implementation showed a flat single-directory
//! listing with a synthetic `..` parent entry — functional, but awkward
//! for navigating real projects. This version builds a **recursive
//! tree** flattened into a `Vec<FileEntry>`, with `depth` and
//! `expanded` fields so the Slint side can indent each row and render a
//! chevron that flips when the user clicks a directory.
//!
//! The list is intentionally flat (rather than a recursive Slint
//! component) because Slint's `for` binding expects a model and
//! composing nested `for` loops is painful. Flattening keeps one
//! `VecModel<FileEntryData>` + a `depth` int for indentation, which the
//! Slint renderer handles trivially.
//!
//! Directories the user has "opened" are tracked externally (in
//! `AppState::expanded_dirs`) and passed to `build_tree` on each
//! refresh. This means expansion state survives the `list_dir`
//! equivalent being re-called after a file system change, and clicking
//! into a directory toggles its presence in the set.
//!
//! Default-ignored patterns match the common `node_modules`, `.git`,
//! `target`, `dist`, `build` conventions so dev directories don't
//! drown the user in noise — same list Lanes ships with.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    File,
}

/// One row of the flattened tree. `depth == 0` is a top-level entry
/// directly under the tree root; every nested level adds one. `expanded`
/// is only meaningful for `Directory` entries.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub kind: EntryKind,
    pub depth: usize,
    /// True if this is a directory and the user has expanded it.
    pub expanded: bool,
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

/// Build the flattened tree rooted at `root`. Directories present in
/// `expanded` are walked into; their children are appended right after
/// the directory entry, with `depth` incremented by 1. Ignored
/// directories are skipped entirely.
///
/// Children are sorted directories-first then alphabetically within
/// each group — same rule as a typical file browser.
pub fn build_tree(root: &Path, expanded: &HashSet<PathBuf>) -> Result<Vec<FileEntry>> {
    let mut out = Vec::new();
    append_children(root, 0, expanded, &mut out)?;
    Ok(out)
}

fn append_children(
    dir: &Path,
    depth: usize,
    expanded: &HashSet<PathBuf>,
    out: &mut Vec<FileEntry>,
) -> Result<()> {
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
            let path = entry.path();
            let is_expanded = expanded.contains(&path);
            dirs.push(FileEntry {
                name,
                path,
                kind: EntryKind::Directory,
                depth,
                expanded: is_expanded,
            });
        } else if file_type.is_file() {
            files.push(FileEntry {
                name,
                path: entry.path(),
                kind: EntryKind::File,
                depth,
                expanded: false,
            });
        }
    }

    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    files.sort_by(|a, b| a.name.cmp(&b.name));

    // Emit each directory, and if it's expanded, recursively emit its
    // children right after — depth-first traversal in natural order.
    for d in dirs {
        let recurse_path = if d.expanded { Some(d.path.clone()) } else { None };
        out.push(d);
        if let Some(path) = recurse_path {
            // Silently drop errors on a single sub-directory so one
            // unreadable folder doesn't blank the whole tree.
            if let Err(err) = append_children(&path, depth + 1, expanded, out) {
                tracing::debug!(%err, path = %path.display(), "sub-dir read failed, skipping");
            }
        }
    }
    for f in files {
        out.push(f);
    }
    Ok(())
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
    fn build_tree_flat_hides_nested_children() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("src")).unwrap();
        fs::create_dir(root.join("src/inner")).unwrap();
        File::create(root.join("src/file.rs")).unwrap();
        File::create(root.join("README.md")).unwrap();

        let entries = build_tree(root, &HashSet::new()).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        // Collapsed: only top-level entries.
        assert_eq!(names, vec!["src", "README.md"]);
        // src should be a directory at depth 0, not expanded.
        let src = &entries[0];
        assert_eq!(src.kind, EntryKind::Directory);
        assert_eq!(src.depth, 0);
        assert!(!src.expanded);
    }

    #[test]
    fn build_tree_walks_expanded_dirs_in_order() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("src")).unwrap();
        fs::create_dir(root.join("src/inner")).unwrap();
        File::create(root.join("src/file.rs")).unwrap();
        File::create(root.join("README.md")).unwrap();

        let mut expanded = HashSet::new();
        expanded.insert(root.join("src"));

        let entries = build_tree(root, &expanded).unwrap();
        let pairs: Vec<(String, usize)> = entries
            .iter()
            .map(|e| (e.name.clone(), e.depth))
            .collect();

        // src expanded → its children appear between src and README.md,
        // at depth 1. Directories sort before files inside src.
        assert_eq!(
            pairs,
            vec![
                ("src".to_string(), 0),
                ("inner".to_string(), 1),
                ("file.rs".to_string(), 1),
                ("README.md".to_string(), 0),
            ]
        );
        // src is marked expanded in the output.
        assert!(entries[0].expanded);
    }

    #[test]
    fn build_tree_walks_multiple_levels_deep() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        File::create(root.join("a/b/c/deep.txt")).unwrap();

        let mut expanded = HashSet::new();
        expanded.insert(root.join("a"));
        expanded.insert(root.join("a/b"));
        expanded.insert(root.join("a/b/c"));

        let entries = build_tree(root, &expanded).unwrap();
        let rows: Vec<(String, usize)> = entries
            .iter()
            .map(|e| (e.name.clone(), e.depth))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("a".to_string(), 0),
                ("b".to_string(), 1),
                ("c".to_string(), 2),
                ("deep.txt".to_string(), 3),
            ]
        );
    }

    #[test]
    fn build_tree_skips_ignored_dirs() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("src")).unwrap();
        fs::create_dir(root.join("node_modules")).unwrap();
        fs::create_dir(root.join("target")).unwrap();
        File::create(root.join("README.md")).unwrap();

        let entries = build_tree(root, &HashSet::new()).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["src", "README.md"]);
        assert!(!names.contains(&"node_modules"));
        assert!(!names.contains(&"target"));
    }

    #[test]
    fn build_tree_dirs_sort_before_files_at_each_level() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("b_dir")).unwrap();
        File::create(root.join("a_file.txt")).unwrap();

        let entries = build_tree(root, &HashSet::new()).unwrap();
        assert_eq!(entries[0].name, "b_dir");
        assert_eq!(entries[0].kind, EntryKind::Directory);
        assert_eq!(entries[1].name, "a_file.txt");
        assert_eq!(entries[1].kind, EntryKind::File);
    }
}
