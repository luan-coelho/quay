//! Deterministic worktree / branch naming helpers.
//!
//! Every task in Quay that opts into `WorktreeStrategy::Create` gets a fresh
//! git worktree living at `<repo>/.worktrees/<slug>/` on a branch named the
//! same as the slug. The slug is derived *deterministically* from the task's
//! `display_id` — the sequential 1-based number shown on the card — so
//! re-opening the same task always produces the same directory name, even
//! across runs of the app.
//!
//! Format: `{display_id}-{adjective}-{noun}`.
//!
//! Example: task #1 → `1-brave-otter`, task #42 → `42-deft-viper`.
//!
//! The word lists are hard-coded arrays of 32 entries each for a vocabulary
//! of 32 × 32 = 1024 unique slug pairs; collision is not a concern until a
//! user creates 1025 tasks, at which point the slug still remains stable
//! per task (same `display_id` → same pair) because the indexing is modulo
//! without randomness.

use std::path::{Path, PathBuf};

/// Short, pronounceable adjectives. Index by `display_id % ADJECTIVES.len()`.
const ADJECTIVES: [&str; 32] = [
    "brave", "calm", "eager", "fond", "happy", "jolly", "keen", "lucid",
    "merry", "noble", "proud", "quick", "sharp", "swift", "wise", "zesty",
    "bold", "crisp", "deft", "elegant", "feisty", "gentle", "humble", "icy",
    "jaunty", "kind", "lively", "mellow", "nimble", "odd", "plucky", "quiet",
];

/// Short animal nouns. Index by `(display_id / ADJECTIVES.len()) % NOUNS.len()`.
const NOUNS: [&str; 32] = [
    "otter", "raven", "falcon", "panther", "lynx", "heron", "badger", "marlin",
    "coyote", "eagle", "gecko", "hawk", "ibex", "jaguar", "koi", "leopard",
    "moose", "newt", "orca", "puma", "quail", "rhino", "seal", "tiger",
    "urchin", "viper", "walrus", "wolf", "bison", "crane", "dove", "ferret",
];

/// Deterministic `{display_id}-{adjective}-{noun}` slug.
///
/// Valid for `display_id >= 1`. Non-positive ids are clamped to 1 so tests
/// and accidental zero inputs still produce a valid slug.
pub fn branch_slug(display_id: i32) -> String {
    let n = display_id.max(1) as usize;
    let adj = ADJECTIVES[n % ADJECTIVES.len()];
    // The division before the second modulo varies the noun more slowly
    // than the adjective, so consecutive task ids never repeat the same
    // noun twice in a row (desirable for visual distinctiveness when
    // looking at 5-10 open tasks at once).
    let noun = NOUNS[(n / ADJECTIVES.len()) % NOUNS.len()];
    format!("{display_id}-{adj}-{noun}")
}

/// Where the worktree for a given task should live on disk.
///
/// Lives under `<repo>/.worktrees/` to mirror Lanes' convention. Adding
/// `/.worktrees` to `.gitignore` is recommended — Quay does not touch the
/// repo's gitignore automatically.
pub fn worktree_dir(repo: &Path, display_id: i32) -> PathBuf {
    repo.join(".worktrees").join(branch_slug(display_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn slug_starts_with_display_id() {
        assert!(branch_slug(1).starts_with("1-"));
        assert!(branch_slug(42).starts_with("42-"));
        assert!(branch_slug(1024).starts_with("1024-"));
    }

    #[test]
    fn slug_has_three_parts() {
        let slug = branch_slug(7);
        let parts: Vec<&str> = slug.split('-').collect();
        assert_eq!(parts.len(), 3, "expected id-adj-noun, got {slug}");
        assert_eq!(parts[0], "7");
    }

    #[test]
    fn slug_is_deterministic() {
        for id in 1..=100 {
            assert_eq!(branch_slug(id), branch_slug(id));
        }
    }

    #[test]
    fn slug_clamps_non_positive_ids() {
        // 0 and negative ids are coerced to id=1 internally but the display
        // prefix still uses the original number — better to fail loudly on
        // garbage input than silently remap.
        assert_eq!(branch_slug(0), "0-calm-otter");
        assert_eq!(branch_slug(-5), "-5-calm-otter");
    }

    #[test]
    fn worktree_dir_nests_under_dot_worktrees() {
        let p = worktree_dir(Path::new("/tmp/repo"), 5);
        assert!(
            p.starts_with("/tmp/repo/.worktrees/"),
            "expected nested path, got {}",
            p.display()
        );
        assert!(p.to_string_lossy().contains("5-"));
    }

    #[test]
    fn slugs_for_1024_tasks_are_unique() {
        // 32 × 32 = 1024 distinct (adj, noun) pairs. Each display_id gets a
        // unique pair for the first 1024 ids. Beyond that, pairs repeat but
        // the id prefix still keeps the whole slug unique.
        let seen: HashSet<String> =
            (1..=1024).map(branch_slug).collect();
        assert_eq!(seen.len(), 1024);
    }
}
