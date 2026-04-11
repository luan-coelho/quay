//! Read-only repository inspection via `git2` (libgit2 bindings).
//!
//! libgit2 is fast and avoids the per-call subprocess cost of shelling out to
//! the git CLI, which matters because the kanban polls these queries
//! frequently to keep card badges (dirty flag, branch name) up to date.
//!
//! Phase 1 does not yet call `read_status` — that wiring lands in Phase 2
//! (kanban poller populates the dirty flag on each visible card, and the
//! Done transition consults it to decide whether to auto-remove the worktree
//! or prompt the user). `#![allow(dead_code)]` is kept until then.

#![allow(dead_code)]

use std::path::Path;

use anyhow::{Context, Result};
use git2::{Repository, Status, StatusOptions};

/// Snapshot of a worktree's git status, suitable for rendering on a kanban card.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeStatus {
    /// True iff there are no modified, untracked, deleted or staged entries.
    pub clean: bool,
    pub modified_count: usize,
    pub untracked_count: usize,
    pub staged_count: usize,
    /// Short branch name (e.g. "feature/foo") if HEAD is on a branch.
    pub current_branch: Option<String>,
    /// First line of the HEAD commit message, when available.
    pub head_summary: Option<String>,
}

/// Inspect a worktree directory and report its current git status.
///
/// `worktree_path` may be the main repository or any linked worktree — git2
/// resolves both transparently via the `.git` discovery walk.
pub fn read_status(worktree_path: &Path) -> Result<WorktreeStatus> {
    let repo = Repository::open(worktree_path)
        .with_context(|| format!("failed to open repo at {}", worktree_path.display()))?;

    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .include_ignored(false)
        .recurse_untracked_dirs(true);

    let statuses = repo.statuses(Some(&mut opts))?;

    let mut modified = 0usize;
    let mut untracked = 0usize;
    let mut staged = 0usize;

    for entry in statuses.iter() {
        let s = entry.status();

        if s.contains(Status::WT_NEW) {
            untracked += 1;
        }
        if s.intersects(Status::WT_MODIFIED | Status::WT_DELETED | Status::WT_TYPECHANGE | Status::WT_RENAMED) {
            modified += 1;
        }
        if s.intersects(
            Status::INDEX_NEW
                | Status::INDEX_MODIFIED
                | Status::INDEX_DELETED
                | Status::INDEX_RENAMED
                | Status::INDEX_TYPECHANGE,
        ) {
            staged += 1;
        }
    }

    let head = repo.head().ok();
    let current_branch = head.as_ref().and_then(|h| h.shorthand()).map(String::from);
    let head_summary = head
        .as_ref()
        .and_then(|h| h.peel_to_commit().ok())
        .and_then(|c| c.summary().map(String::from));

    let clean = modified == 0 && untracked == 0 && staged == 0;

    Ok(WorktreeStatus {
        clean,
        modified_count: modified,
        untracked_count: untracked,
        staged_count: staged,
        current_branch,
        head_summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::tempdir;

    fn init_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().to_path_buf();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .expect("git");
            assert!(out.status.success(), "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr));
        };
        Command::new("git")
            .arg("init")
            .arg("-b")
            .arg("main")
            .arg(&repo)
            .output()
            .unwrap();
        run(&["config", "user.email", "test@quay.local"]);
        run(&["config", "user.name", "Quay Test"]);
        fs::write(repo.join("README.md"), "hello\n").unwrap();
        run(&["add", "README.md"]);
        run(&["commit", "-m", "init"]);
        (tmp, repo)
    }

    #[test]
    fn clean_repo_reports_clean() {
        let (_tmp, repo) = init_repo();
        let status = read_status(&repo).expect("status");
        assert!(status.clean, "freshly committed repo should be clean: {status:?}");
        assert_eq!(status.modified_count, 0);
        assert_eq!(status.untracked_count, 0);
        assert_eq!(status.staged_count, 0);
        assert_eq!(status.current_branch.as_deref(), Some("main"));
        assert_eq!(status.head_summary.as_deref(), Some("init"));
    }

    #[test]
    fn modified_file_marks_dirty() {
        let (_tmp, repo) = init_repo();
        fs::write(repo.join("README.md"), "changed\n").unwrap();
        let status = read_status(&repo).expect("status");
        assert!(!status.clean);
        assert_eq!(status.modified_count, 1);
        assert_eq!(status.untracked_count, 0);
    }

    #[test]
    fn untracked_file_marks_dirty() {
        let (_tmp, repo) = init_repo();
        fs::write(repo.join("new.txt"), "new\n").unwrap();
        let status = read_status(&repo).expect("status");
        assert!(!status.clean);
        assert_eq!(status.untracked_count, 1);
        assert_eq!(status.modified_count, 0);
    }

    #[test]
    fn staged_change_counts_separately() {
        let (_tmp, repo) = init_repo();
        fs::write(repo.join("staged.txt"), "x\n").unwrap();
        let _ = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["add", "staged.txt"])
            .output()
            .unwrap();
        let status = read_status(&repo).expect("status");
        assert!(!status.clean);
        assert_eq!(status.staged_count, 1);
        assert_eq!(status.untracked_count, 0);
    }
}
