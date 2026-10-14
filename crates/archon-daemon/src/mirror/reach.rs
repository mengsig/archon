//! Which mirrors the directories of an agent's scope let it read. A helper
//! of the orchestrator follows the model policy of exactly those
//! repositories, so a helper given one mirror answers to that repository.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::MIRRORS_DIR;

/// Most bytes read of a `.git` file: it holds one `gitdir:` line.
const MAX_GIT_FILE_BYTES: u64 = 4096;

/// The mirrors a scope reaches.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Reach {
    /// The scope holds the mirrors directory itself, or is not known.
    all: bool,
    repos: BTreeSet<String>,
}

impl Reach {
    pub fn includes(&self, repo: &str) -> bool {
        self.all || self.repos.contains(repo)
    }

    /// For a refusal: what the scope reaches, in words.
    pub fn describe(&self) -> String {
        if self.all {
            return "every repository's mirror".to_owned();
        }
        let names: Vec<String> = self.repos.iter().map(|name| format!("`{name}`")).collect();
        match names.len() {
            0 => "no repository's mirror".to_owned(),
            1 => format!("the mirror of {}", names[0]),
            _ => format!("the mirrors of {}", names.join(", ")),
        }
    }
}

/// The mirrors in `workspace` that an agent's `roots` reach: one a root is in
/// or is a worktree of; all for a root above them, or for no root at all.
pub fn reach(workspace: &Path, roots: &[PathBuf]) -> Reach {
    let mirrors = resolved(&workspace.join(MIRRORS_DIR));
    let mut found = Reach {
        all: roots.is_empty(),
        repos: BTreeSet::new(),
    };
    for root in roots {
        assert!(root.is_absolute(), "Aether scopes are absolute: {root:?}");
        let at = resolved(root);
        if mirrors.starts_with(&at) {
            found.all = true;
        } else if let Some(name) =
            mirror_holding(&at, &mirrors).or_else(|| worktree_of(&at, &mirrors))
        {
            found.repos.insert(name);
        }
    }
    found
}

/// A path that does not exist leads nowhere else: compared as written.
fn resolved(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

/// The mirror `at` lies in, when it lies in one.
fn mirror_holding(at: &Path, mirrors: &Path) -> Option<String> {
    let inside = at.strip_prefix(mirrors).ok()?;
    let name = inside.components().next()?;
    Some(name.as_os_str().to_string_lossy().into_owned())
}

/// The mirror `at` is a git worktree of (an isolated child's workspace): the
/// nearest `.git` above it is a file whose `gitdir:` leads into it.
fn worktree_of(at: &Path, mirrors: &Path) -> Option<String> {
    let dot_git = at
        .ancestors()
        .map(|dir| dir.join(".git"))
        .find(|dot_git| dot_git.symlink_metadata().is_ok())?;
    let meta = dot_git.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_GIT_FILE_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let git_dir = text.lines().find_map(|line| line.strip_prefix("gitdir:"))?;
    let git_dir = dot_git.parent()?.join(git_dir.trim());
    mirror_holding(&resolved(&git_dir), mirrors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("orchestrator");
        let alpha = workspace.join(MIRRORS_DIR).join("alpha");
        std::fs::create_dir_all(alpha.join(".git/worktrees/w1")).unwrap();
        std::fs::create_dir_all(alpha.join("src")).unwrap();
        std::fs::create_dir_all(workspace.join("scratch")).unwrap();
        (dir, workspace, alpha)
    }

    #[test]
    fn a_root_in_a_mirror_reaches_that_mirror_only() {
        let (_dir, workspace, alpha) = layout();
        for root in [alpha.clone(), alpha.join("src")] {
            let found = reach(&workspace, &[root]);
            assert!(found.includes("alpha") && !found.includes("beta"));
            assert_eq!(found.describe(), "the mirror of `alpha`");
        }
    }

    #[test]
    fn scratch_reaches_no_mirror_and_the_workspace_reaches_all() {
        let (_dir, workspace, _alpha) = layout();
        let scratch = reach(&workspace, &[workspace.join("scratch")]);
        assert!(!scratch.includes("alpha"));
        assert_eq!(scratch.describe(), "no repository's mirror");
        for root in [workspace.clone(), workspace.join(MIRRORS_DIR)] {
            let found = reach(&workspace, &[root]);
            assert!(found.includes("alpha") && found.includes("anything"));
        }
        let unknown = reach(&workspace, &[]);
        assert!(unknown.includes("alpha"), "an unknown scope reaches all");
    }

    #[test]
    fn a_worktree_of_a_mirror_reaches_it_from_outside_the_workspace() {
        let (dir, workspace, alpha) = layout();
        let child = dir.path().join("aether/child/workspace");
        std::fs::create_dir_all(child.join("src")).unwrap();
        let git_dir = alpha.join(".git/worktrees/w1");
        std::fs::write(
            child.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();
        for root in [child.clone(), child.join("src")] {
            let found = reach(&workspace, &[root]);
            assert!(found.includes("alpha") && !found.includes("beta"));
        }
        let elsewhere = dir.path().join("aether/other");
        std::fs::create_dir_all(elsewhere.join(".git")).unwrap();
        assert!(!reach(&workspace, &[elsewhere]).includes("alpha"));
    }

    #[test]
    fn a_link_is_followed_to_the_mirror_it_leads_into() {
        let (dir, workspace, alpha) = layout();
        let link = dir.path().join("shortcut");
        std::os::unix::fs::symlink(&alpha, &link).unwrap();
        let found = reach(&workspace, &[link, workspace.join("scratch")]);
        assert!(found.includes("alpha") && !found.includes("beta"));
    }
}
