//! Read-only repository inspection via `git2` (libgit2 bindings).
//!
//! libgit2 is fast and avoids the per-call subprocess cost of shelling out to
//! the git CLI, which matters because the kanban polls these queries
//! frequently to keep card badges (dirty flag, branch name) up to date.
//!
//! Wired into `refresh_kanban` as of Phase 2 — every card with a
//! `worktree_path` runs one `read_status` to populate its dirty dot.

use std::path::Path;

use anyhow::{Context, Result};
use git2::{BranchType, Repository, Status, StatusOptions};

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

/// Enumerate the local branches of the repository that contains
/// `repo_path`. Used by the New Project modal to populate the "base
/// branch" Select with the real branches of whatever folder the user
/// just picked, instead of a free-text field.
///
/// Returns a stable ordering: `main` / `master` first (if present),
/// then the rest alphabetically. Non-UTF-8 branch names are silently
/// skipped — realistic repos do not ship those.
pub fn list_branches(repo_path: &Path) -> Result<Vec<String>> {
    let repo = Repository::discover(repo_path)
        .with_context(|| format!("failed to discover repo at {}", repo_path.display()))?;

    let mut names: Vec<String> = Vec::new();
    for branch in repo.branches(Some(BranchType::Local))? {
        let (branch, _) = branch?;
        if let Some(name) = branch.name()?.map(String::from) {
            names.push(name);
        }
    }

    // Sort: main/master float to the top, everything else alphabetical.
    names.sort_by(|a, b| {
        let rank = |s: &str| match s {
            "main" => 0,
            "master" => 1,
            _ => 2,
        };
        rank(a).cmp(&rank(b)).then_with(|| a.cmp(b))
    });

    Ok(names)
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
    fn list_branches_returns_local_branches_with_main_first() {
        let (_tmp, repo) = init_repo();
        // Create two extra local branches.
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
        run(&["branch", "feature/xyz"]);
        run(&["branch", "bugfix/abc"]);

        let branches = list_branches(&repo).expect("list branches");
        assert_eq!(branches.first().map(String::as_str), Some("main"));
        assert!(branches.contains(&"feature/xyz".to_string()));
        assert!(branches.contains(&"bugfix/abc".to_string()));
        // `bugfix/abc` sorts before `feature/xyz` alphabetically.
        let f = branches.iter().position(|b| b == "feature/xyz").unwrap();
        let b = branches.iter().position(|b| b == "bugfix/abc").unwrap();
        assert!(b < f, "alpha order among non-main branches: {branches:?}");
    }

    #[test]
    fn list_branches_on_missing_repo_errors() {
        let tmp = tempdir().unwrap();
        let err = list_branches(tmp.path()).expect_err("should fail");
        assert!(
            err.to_string().contains("failed to discover repo"),
            "error should mention discovery, got: {err}"
        );
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
