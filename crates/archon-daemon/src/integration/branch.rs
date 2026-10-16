//! The integration branch as git sees it: its head, the merges on it, and
//! the daemon-owned checkout a full check runs in. Every function blocks.
//!
//! The checkout is `<data>/integration/<repo>`, its git directory
//! `<data>/integration-git/<repo>`, made and sealed as the orchestrator's
//! mirrors are, so nothing the checked code writes there runs as the daemon.

use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;

use crate::mirror::git::{Base, Mirror, write_push_guard};
use crate::worktree::shadow::Shadow;
use crate::worktree::stack::{self, is_commit_id};

/// Under the data directory: the work trees full checks run in.
const WORK_DIR: &str = "integration";
/// Under the data directory: their git directories.
const GIT_DIR: &str = "integration-git";

/// Where a branch's commits are read: `origin`'s, fetched into the
/// registered repository, or the repository's own branch.
#[derive(Debug, Clone)]
pub(crate) struct Source {
    pub repo: PathBuf,
    pub data_dir: PathBuf,
    /// `pr` mode: `origin` holds the branches, when there is one.
    pub remote: bool,
}

/// A branch's head commit and the ref of the registered repository that names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Head {
    pub commit: String,
    pub source_ref: String,
}

impl Source {
    /// The head of `branch` now: `origin`'s, fetched, in `pr` mode with an
    /// `origin`, else the repository's own; `None` when it has no such branch.
    pub fn head(&self, branch: &str) -> Result<Option<Head>, ArchonError> {
        assert!(self.repo.is_absolute(), "registered paths are absolute");
        if self.remote {
            let shadow = Shadow::of(&self.data_dir, &self.repo)?;
            if let Some(commit) = stack::fetch_head(&shadow, &self.repo, branch)? {
                let source_ref = format!("refs/remotes/origin/{branch}");
                return Ok(Some(Head { commit, source_ref }));
            }
        }
        let source_ref = format!("refs/heads/{branch}");
        let head = crate::worktree::commit_id(&self.repo, &source_ref)?;
        assert!(head.as_deref().is_none_or(is_commit_id), "a full commit id");
        Ok(head.map(|commit| Head { commit, source_ref }))
    }

    /// The first-parent commits after `after` up to `head`, oldest first:
    /// one per merge; empty when `after` is no ancestor of `head`.
    pub fn merges(&self, after: &str, head: &str) -> Result<Vec<String>, ArchonError> {
        assert!(is_commit_id(after) && is_commit_id(head), "full commit ids");
        if !crate::worktree::is_ancestor(&self.repo, after, head)? {
            return Ok(Vec::new());
        }
        let range = format!("{after}..{head}");
        let args = ["rev-list", "--first-parent", "--reverse", range.as_str()];
        let listed = crate::worktree::git(&self.repo, &args)?;
        let merges: Vec<String> = listed.lines().map(str::to_owned).collect();
        assert!(
            merges.iter().all(|c| is_commit_id(c)),
            "rev-list lists commit ids"
        );
        assert!(
            merges.last().is_none_or(|last| last == head),
            "the head is the last merge"
        );
        Ok(merges)
    }

    /// Whether `commit` is `head` or one of its ancestors.
    pub fn contains(&self, head: &str, commit: &str) -> Result<bool, ArchonError> {
        crate::worktree::is_ancestor(&self.repo, commit, head)
    }

    /// The commit on `release`'s first-parent line where `head` entered it:
    /// the promotion's merge commit, or `head` itself after a fast-forward.
    /// `None` when `release` does not hold `head`.
    pub fn entered_at(&self, release: &str, head: &str) -> Result<Option<String>, ArchonError> {
        assert!(
            is_commit_id(release) && is_commit_id(head),
            "full commit ids"
        );
        if !self.contains(release, head)? {
            return Ok(None);
        }
        let range = format!("{head}..{release}");
        let args = ["rev-list", "--first-parent", "--ancestry-path", "--reverse"];
        let listed = crate::worktree::git(&self.repo, &[&args[..], &[range.as_str()]].concat())?;
        let Some(oldest) = listed.lines().next() else {
            assert_eq!(release, head, "only the head itself lists nothing");
            return Ok(Some(head.to_owned()));
        };
        assert!(is_commit_id(oldest), "rev-list lists commit ids");
        let parent = crate::worktree::git(&self.repo, &["rev-parse", &format!("{oldest}^1")])?;
        if parent == head {
            return Ok(Some(head.to_owned()));
        }
        Ok(Some(oldest.to_owned()))
    }

    /// Where `head` left the release branch: the commit assumed green when
    /// none was recorded. `None` when the release branch is missing.
    pub fn fork_point(&self, release: &str, head: &str) -> Result<Option<String>, ArchonError> {
        let Some(release) = self.head(release)? else {
            return Ok(None);
        };
        let args = ["merge-base", release.commit.as_str(), head];
        let base = crate::worktree::git(&self.repo, &args)?;
        assert!(is_commit_id(&base), "merge-base names a commit");
        Ok(Some(base))
    }

    /// The subject line of `commit`.
    pub fn subject(&self, commit: &str) -> Result<String, ArchonError> {
        assert!(is_commit_id(commit), "a full commit id");
        crate::worktree::git(&self.repo, &["log", "-1", "--format=%s", commit])
    }
}

/// The daemon-owned checkout of one repository's integration branch.
#[derive(Debug)]
pub(crate) struct Checkout {
    mirror: Mirror,
    base: Base,
    pub log: PathBuf,
}

impl Checkout {
    /// Make the checkout of repository `name` if it is missing and fetch
    /// `head`'s source into it.
    pub fn prepare(source: &Source, name: &str, head: &Head) -> Result<Self, ArchonError> {
        assert!(!name.is_empty() && !name.contains('/'), "a registered name");
        let work_root = source.data_dir.join(WORK_DIR);
        let hooks = crate::mirror::hooks_dir(&source.data_dir);
        for dir in [&work_root, &hooks] {
            std::fs::create_dir_all(dir)
                .map_err(|e| ArchonError::io(format!("creating {}", dir.display()), e))?;
        }
        write_push_guard(&hooks)?;
        let mirror = Mirror {
            work: work_root.join(name),
            git_dir: source.data_dir.join(GIT_DIR).join(name),
            hooks,
        };
        let base = Base::resolve(&source.repo, Some(&head.source_ref))?;
        mirror.prepare(&base)?;
        mirror.fetch(&source.repo, &base)?;
        let log = work_root.join(format!("{name}.log"));
        Ok(Self { mirror, base, log })
    }

    /// Check the work tree out at `commit`, untracked files removed and
    /// ignored build output kept; returns the work tree.
    pub fn at(&self, commit: &str) -> Result<&Path, ArchonError> {
        let base = self.base.at(commit);
        self.mirror.checkout(&base)?;
        self.mirror.clean(&base)?;
        assert_eq!(
            self.mirror.head(&base)?.as_deref(),
            Some(commit),
            "the checkout is at the commit asked for"
        );
        Ok(&self.mirror.work)
    }
}
