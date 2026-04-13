//! Shell-out wrapper around `git worktree add/remove/prune`.
//!
//! Why shell out instead of using `git2`/libgit2? libgit2's worktree creation
//! API is awkward and incomplete: it does not cleanly express the common case
//! of "create a brand new branch in a brand new worktree from a base ref" that
//! Quay needs for every kanban card. The `git` CLI gets that case right
//! reliably across every platform we care about (Linux/macOS/Windows), so we
//! use it for mutations and reserve `git2` for fast read-only queries
//! (status, diff, HEAD lookups — see `status.rs`).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Cheap-to-clone handle that knows where the `git` binary lives.
#[derive(Debug, Clone)]
pub struct WorktreeManager {
    git: PathBuf,
}

impl WorktreeManager {
    /// Locate the `git` binary on `PATH` and confirm it actually runs. Returns
    /// a friendly error if git is missing or unusable so the user knows what
    /// to install.
    pub fn detect() -> Result<Self> {
        // Try the binary literally — `Command::new("git")` consults PATH on
        // every platform. If it succeeds, we trust the system PATH.
        let output = Command::new("git")
            .arg("--version")
            .output()
            .context("could not invoke `git` — install git ≥ 2.25 and ensure it is on PATH")?;

        if !output.status.success() {
            bail!(
                "`git --version` failed with status {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        tracing::info!(
            version = %String::from_utf8_lossy(&output.stdout).trim(),
            "git binary detected"
        );

        Ok(Self { git: PathBuf::from("git") })
    }

    /// Create a new worktree at `worktree_path` checked out to a branch named
    /// `branch`, branched off `base_ref` inside the main repo.
    ///
    /// Handles all edge cases from previous sessions that weren't cleaned up:
    ///
    /// 1. **Stale git metadata** — runs `git worktree prune` first to clear
    ///    records whose on-disk paths no longer exist.
    /// 2. **Leftover worktree directory** — if the path already exists (zombie
    ///    from a previous task), removes it via `git worktree remove --force`
    ///    before creating.
    /// 3. **Existing branch** — uses `-B` (force-create) so the branch is
    ///    reset to `base_ref` instead of failing.
    ///
    /// Equivalent shell commands:
    ///     git -C <repo> worktree prune
    ///     git -C <repo> worktree remove --force <worktree_path>   # if exists
    ///     git -C <repo> worktree add -B <branch> <worktree_path> <base_ref>
    pub fn create(
        &self,
        repo: &Path,
        branch: &str,
        worktree_path: &Path,
        base_ref: &str,
    ) -> Result<()> {
        // 1. Prune stale worktree records whose on-disk paths were deleted
        //    outside of git (manual rm, crash, etc.). Without this, git
        //    refuses to create a new worktree on a branch that has a stale
        //    record pointing to a non-existent directory.
        let _ = self.prune(repo);

        // 2. If the worktree directory still exists after prune (i.e. it's a
        //    real leftover, not just stale metadata), force-remove it so the
        //    slot is free for the new task.
        if worktree_path.exists() {
            tracing::warn!(
                worktree = %worktree_path.display(),
                "removing leftover worktree before re-creating"
            );
            self.remove(repo, worktree_path)
                .with_context(|| format!(
                    "failed to remove leftover worktree at {}",
                    worktree_path.display()
                ))?;
        }

        // 3. Create with -B: resets the branch to base_ref if it already
        //    exists, or creates it fresh otherwise.
        let output = Command::new(&self.git)
            .arg("-C")
            .arg(repo)
            .arg("worktree")
            .arg("add")
            .arg("-B")
            .arg(branch)
            .arg(worktree_path)
            .arg(base_ref)
            .output()
            .context("failed to invoke `git worktree add`")?;

        if !output.status.success() {
            bail!(
                "git worktree add failed (status {}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        tracing::info!(
            repo = %repo.display(),
            branch,
            base_ref,
            worktree = %worktree_path.display(),
            "worktree created"
        );

        Ok(())
    }

    /// Remove a worktree, forcing the removal so that uncommitted state inside
    /// the worktree does not block cleanup. Callers should snapshot anything
    /// worth keeping before invoking this (e.g. show a confirm dialog).
    ///
    /// Equivalent shell command:
    ///     git -C <repo> worktree remove --force <worktree_path>
    ///
    /// Called from `AppState::cleanup_worktree_on_done` when a task
    /// transitions into the Done column with a clean worktree.
    pub fn remove(&self, repo: &Path, worktree_path: &Path) -> Result<()> {
        let output = Command::new(&self.git)
            .arg("-C")
            .arg(repo)
            .arg("worktree")
            .arg("remove")
            .arg("--force")
            .arg(worktree_path)
            .output()
            .context("failed to invoke `git worktree remove`")?;

        if !output.status.success() {
            bail!(
                "git worktree remove failed (status {}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        tracing::info!(
            repo = %repo.display(),
            worktree = %worktree_path.display(),
            "worktree removed"
        );

        Ok(())
    }

    /// Prune any worktree records whose on-disk paths no longer exist. Useful
    /// after a manual `rm -rf` of a worktree directory left dangling metadata.
    ///
    /// Equivalent shell command:
    ///     git -C <repo> worktree prune
    ///
    pub fn prune(&self, repo: &Path) -> Result<()> {
        let output = Command::new(&self.git)
            .arg("-C")
            .arg(repo)
            .arg("worktree")
            .arg("prune")
            .output()
            .context("failed to invoke `git worktree prune`")?;

        if !output.status.success() {
            bail!(
                "git worktree prune failed (status {}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    /// List worktrees attached to a repo. Useful for the kanban to discover
    /// orphaned cards on startup.
    ///
    /// Returns the absolute path of every worktree (including the main one),
    /// parsed from `git worktree list --porcelain`.
    ///
    /// `#[allow(dead_code)]` until Phase 5 surfaces this in the sidebar.
    #[allow(dead_code)]
    pub fn list(&self, repo: &Path) -> Result<Vec<PathBuf>> {
        let output = Command::new(&self.git)
            .arg("-C")
            .arg(repo)
            .arg("worktree")
            .arg("list")
            .arg("--porcelain")
            .output()
            .context("failed to invoke `git worktree list`")?;

        if !output.status.success() {
            bail!(
                "git worktree list failed (status {}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let mut paths = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(rest) = line.strip_prefix("worktree ") {
                paths.push(PathBuf::from(rest));
            }
        }
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::tempdir;

    /// Initialise a fresh git repository in a tempdir with a single committed
    /// file on the `main` branch. Returns the tempdir guard (so the directory
    /// stays alive for the duration of the test) and the absolute repo path.
    fn fixture_repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempdir().expect("tempdir");
        let repo = tmp.path().to_path_buf();

        // `git init -b main` requires git ≥ 2.28; on older versions fall back
        // to plain `git init` and rely on the test config to rename the branch.
        let init = Command::new("git")
            .arg("init")
            .arg("-b")
            .arg("main")
            .arg(&repo)
            .output()
            .expect("git init");
        assert!(init.status.success(), "git init failed: {}",
            String::from_utf8_lossy(&init.stderr));

        // Local identity for commits + disable CRLF for Windows + disable
        // commit signing so the fixture works on machines with global
        // gpg.commit.gpgsign=true.
        for kv in [
            ("user.email", "test@quay.local"),
            ("user.name", "Quay Test"),
            ("core.autocrlf", "false"),
            ("commit.gpgsign", "false"),
        ] {
            let out = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["config", kv.0, kv.1])
                .output()
                .expect("git config");
            assert!(out.status.success());
        }

        // Initial commit.
        fs::write(repo.join("README.md"), "hello quay\n").expect("write readme");
        let add = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["add", "README.md"])
            .output()
            .expect("git add");
        assert!(add.status.success());
        let commit = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["commit", "-m", "initial"])
            .output()
            .expect("git commit");
        assert!(commit.status.success(), "git commit failed: {}",
            String::from_utf8_lossy(&commit.stderr));

        (tmp, repo)
    }

    #[test]
    fn detect_finds_git() {
        let mgr = WorktreeManager::detect();
        assert!(mgr.is_ok(), "git binary should be on PATH for tests");
    }

    #[test]
    fn create_and_remove_worktree() {
        let (tmp, repo) = fixture_repo();
        let mgr = WorktreeManager::detect().unwrap();

        let worktree = tmp.path().join("wt-feature");
        mgr.create(&repo, "feature/example", &worktree, "main")
            .expect("create worktree");

        assert!(worktree.exists(), "worktree directory should exist");
        assert!(
            worktree.join("README.md").exists(),
            "worktree should contain the seeded README"
        );

        // The new worktree should be visible in `worktree list`.
        // Canonicalize both sides — on Windows, tempdir() may return a
        // different prefix (\\?\C:\ vs C:\) than git outputs.
        let listed = mgr.list(&repo).expect("list worktrees");
        let wt_canon = worktree.canonicalize().unwrap_or_else(|_| worktree.clone());
        assert!(
            listed
                .iter()
                .any(|p| p.canonicalize().unwrap_or_else(|_| p.clone()) == wt_canon),
            "newly created worktree should appear in `git worktree list`, got {listed:?}"
        );

        mgr.remove(&repo, &worktree).expect("remove worktree");
        assert!(!worktree.exists(), "worktree directory should be gone");
    }

    #[test]
    fn create_replaces_existing_worktree() {
        let (tmp, repo) = fixture_repo();
        let mgr = WorktreeManager::detect().unwrap();

        let worktree = tmp.path().join("wt-reuse");

        // First create: establishes the worktree + branch.
        mgr.create(&repo, "feature/reuse", &worktree, "main")
            .expect("first create");
        assert!(worktree.exists());

        // Second create at the same path + branch: the new logic should
        // remove the existing worktree and re-create it cleanly.
        mgr.create(&repo, "feature/reuse", &worktree, "main")
            .expect("second create should replace existing worktree");
        assert!(worktree.exists(), "worktree should exist after re-create");
        assert!(
            worktree.join("README.md").exists(),
            "re-created worktree should contain the seeded README"
        );

        mgr.remove(&repo, &worktree).expect("cleanup");
    }

    #[test]
    fn create_succeeds_when_branch_already_exists() {
        let (tmp, repo) = fixture_repo();
        let mgr = WorktreeManager::detect().unwrap();

        // First: create a branch via a worktree, then remove the worktree
        // (leaving the branch behind).
        let wt1 = tmp.path().join("wt-first");
        mgr.create(&repo, "reused-branch", &wt1, "main")
            .expect("first create");
        mgr.remove(&repo, &wt1).expect("remove first worktree");

        // The branch still exists even though the worktree is gone.
        let branch_exists = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["branch", "--list", "reused-branch"])
            .output()
            .expect("git branch --list");
        assert!(
            !String::from_utf8_lossy(&branch_exists.stdout).trim().is_empty(),
            "branch should still exist after worktree removal"
        );

        // Second: creating a new worktree on the same branch name must succeed
        // (this was the bug — `-b` failed, `-B` works).
        let wt2 = tmp.path().join("wt-second");
        mgr.create(&repo, "reused-branch", &wt2, "main")
            .expect("second create with existing branch should succeed");
        assert!(wt2.exists(), "second worktree should exist");

        mgr.remove(&repo, &wt2).expect("cleanup");
    }

    #[test]
    fn prune_succeeds_on_clean_repo() {
        let (_tmp, repo) = fixture_repo();
        let mgr = WorktreeManager::detect().unwrap();
        // Should be a no-op but must not error.
        mgr.prune(&repo).expect("prune");
    }
}
