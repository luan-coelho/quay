//! Diff + commit history queries for the Git Changes tab.
//!
//! Uses `git2` in-process (no subprocess) so the Slint timer can re-query
//! ~1×/s without latency bumps. Returns structured data — the Slint layer
//! is responsible for rendering, formatting, and colouring.

use std::path::Path;

use anyhow::{Context, Result};
use git2::{DiffFormat, DiffOptions, Repository};

/// One line of a diff hunk with its origin marker.
///
/// `origin` is:
/// - `'+'` for added lines (render in green)
/// - `'-'` for deleted lines (render in red)
/// - `' '` for context lines (render in muted text)
/// - `'H'` or `'F'` for hunk/file headers (render in dimmed text)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub origin: char,
    pub text: String,
}

/// One file in the diff view with summary counts and the full patch
/// broken into structured lines so the Slint renderer can colour each
/// line independently.
#[derive(Debug, Clone, Default)]
pub struct DiffFile {
    /// Path relative to the repo root.
    pub path: String,
    /// M / A / D / R / ? (untracked)
    pub status: char,
    pub additions: usize,
    pub deletions: usize,
    /// One entry per physical line in the patch, in the order git2
    /// emits them. Each carries its origin char and the line text
    /// without the origin prefix or trailing newline.
    pub lines: Vec<DiffLine>,
}

/// Single commit entry for the History view.
#[derive(Debug, Clone, Default)]
pub struct CommitEntry {
    pub sha_short: String,
    /// First line of the commit message.
    pub summary: String,
    pub author_name: String,
    /// Unix seconds.
    pub timestamp: i64,
}

/// Read the combined worktree-vs-base diff: uncommitted changes in the
/// worktree on top of the committed diff between the worktree branch and
/// the base branch.
///
/// `base_branch` is the short ref name (e.g. "main"). Missing base
/// branches are treated as empty and the entire worktree index is shown
/// as new.
pub fn read_diff(worktree_path: &Path, base_branch: &str) -> Result<Vec<DiffFile>> {
    let repo = Repository::open(worktree_path)
        .with_context(|| format!("open repo at {}", worktree_path.display()))?;

    let base_tree = match repo.find_branch(base_branch, git2::BranchType::Local) {
        Ok(branch) => Some(
            branch
                .get()
                .peel_to_tree()
                .context("peel base branch to tree")?,
        ),
        Err(_) => None,
    };

    let mut opts = DiffOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true);

    let diff = match base_tree {
        Some(ref tree) => repo
            .diff_tree_to_workdir_with_index(Some(tree), Some(&mut opts))
            .context("diff base → workdir+index")?,
        None => repo
            .diff_tree_to_workdir_with_index(None, Some(&mut opts))
            .context("diff <empty> → workdir+index")?,
    };

    let mut files: Vec<DiffFile> = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_file = DiffFile::default();

    diff.print(DiffFormat::Patch, |delta, _hunk, line| {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        if Some(&path) != current_path.as_ref() {
            if current_path.is_some() {
                files.push(std::mem::take(&mut current_file));
            }
            let status_char = match delta.status() {
                git2::Delta::Added => 'A',
                git2::Delta::Deleted => 'D',
                git2::Delta::Modified => 'M',
                git2::Delta::Renamed => 'R',
                git2::Delta::Copied => 'C',
                git2::Delta::Untracked => '?',
                git2::Delta::Typechange => 'T',
                _ => '?',
            };
            current_file = DiffFile {
                path: path.clone(),
                status: status_char,
                ..DiffFile::default()
            };
            current_path = Some(path);
        }

        let origin = line.origin();
        match origin {
            '+' => current_file.additions += 1,
            '-' => current_file.deletions += 1,
            _ => {}
        }

        // Decode one physical line and strip the trailing newline so
        // the Slint renderer can wrap it in its own Text element.
        let raw = String::from_utf8_lossy(line.content());
        let text = raw.trim_end_matches('\n').to_string();
        current_file.lines.push(DiffLine { origin, text });
        true
    })
    .context("walk diff")?;

    if current_path.is_some() {
        files.push(std::mem::take(&mut current_file));
    }

    Ok(files)
}

/// Read the commits on the worktree's HEAD branch that are not on
/// `base_branch`. Newest first. Limited to `limit` entries.
pub fn read_commit_log(
    worktree_path: &Path,
    base_branch: &str,
    limit: usize,
) -> Result<Vec<CommitEntry>> {
    let repo = Repository::open(worktree_path)
        .with_context(|| format!("open repo at {}", worktree_path.display()))?;

    let head = repo.head().context("resolve HEAD")?;
    let head_oid = head.target().context("HEAD has no target")?;

    let base_oid = repo
        .find_branch(base_branch, git2::BranchType::Local)
        .ok()
        .and_then(|b| b.get().target());

    let mut revwalk = repo.revwalk().context("revwalk")?;
    revwalk.push(head_oid)?;
    if let Some(oid) = base_oid {
        revwalk.hide(oid)?;
    }
    revwalk.set_sorting(git2::Sort::TIME)?;

    let mut out: Vec<CommitEntry> = Vec::new();
    for oid in revwalk.take(limit) {
        let oid = oid?;
        let commit = repo.find_commit(oid)?;
        let sha_short = oid.to_string().chars().take(7).collect::<String>();
        let summary = commit.summary().unwrap_or("").to_string();
        let author = commit.author();
        let author_name = author.name().unwrap_or("unknown").to_string();
        let timestamp = commit.time().seconds();
        out.push(CommitEntry {
            sha_short,
            summary,
            author_name,
            timestamp,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use tempfile::tempdir;

    /// Build a fresh repo with one commit on `main` containing a README.
    fn fixture_repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().to_path_buf();

        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };

        let init = Command::new("git")
            .arg("init")
            .arg("-b")
            .arg("main")
            .arg(&repo)
            .output()
            .expect("git init");
        assert!(
            init.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );

        run(&["config", "user.email", "test@quay.local"]);
        run(&["config", "user.name", "Quay Test"]);
        // Prevent Windows CRLF conversion from corrupting diffs.
        run(&["config", "core.autocrlf", "false"]);
        // Disable commit signing so tests work on machines with global gpgsign.
        run(&["config", "commit.gpgsign", "false"]);

        fs::write(repo.join("README.md"), "hello\n").unwrap();
        run(&["add", "README.md"]);
        run(&["commit", "-m", "initial"]);

        (tmp, repo)
    }

    #[test]
    fn read_diff_returns_empty_for_clean_repo() {
        let (_tmp, repo) = fixture_repo();
        let files = read_diff(&repo, "main").unwrap();
        assert!(files.is_empty(), "clean repo should have no diff, got {files:?}");
    }

    #[test]
    fn read_diff_reports_modified_file() {
        let (_tmp, repo) = fixture_repo();
        fs::write(repo.join("README.md"), "hello\nworld\n").unwrap();
        let files = read_diff(&repo, "main").unwrap();

        assert_eq!(files.len(), 1, "one modified file expected");
        let f = &files[0];
        assert_eq!(f.path, "README.md");
        assert_eq!(f.status, 'M');
        assert!(f.additions > 0);
        // At least one line with origin '+' containing "world".
        assert!(
            f.lines.iter().any(|l| l.origin == '+' && l.text.contains("world")),
            "expected a '+' line mentioning 'world' in {:?}",
            f.lines
        );
    }

    #[test]
    fn read_diff_reports_untracked_file() {
        let (_tmp, repo) = fixture_repo();
        fs::write(repo.join("new.txt"), "fresh\n").unwrap();
        let files = read_diff(&repo, "main").unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "new.txt");
        assert_eq!(files[0].status, '?');
    }

    #[test]
    fn read_commit_log_returns_head_when_base_missing() {
        let (_tmp, repo) = fixture_repo();
        let log = read_commit_log(&repo, "nonexistent", 10).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].summary, "initial");
        assert!(!log[0].sha_short.is_empty());
    }

    #[test]
    fn read_commit_log_excludes_base_commits() {
        let (_tmp, repo) = fixture_repo();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["checkout", "-b", "feature"]);
        fs::write(repo.join("feat.txt"), "x\n").unwrap();
        run(&["add", "feat.txt"]);
        run(&["commit", "-m", "feat commit"]);

        let log = read_commit_log(&repo, "main", 10).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].summary, "feat commit");
    }
}
