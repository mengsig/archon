//! The snapshot a `repo-readonly` run works in (ARCHITECTURE §13): a
//! detached worktree of the repository's `HEAD` commit at
//! `<root>/<repo>/<item>/snapshot`, so its agent never works in the
//! operator's checkout. Nothing in it is kept: it is removed with `--force`
//! when the run ends, and a leftover is removed before the next one.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use archon_core::catalog::SNAPSHOT_DIR;
use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;

use super::team::{check_worktree, common_dir};
use super::{Head, WorktreeManager, add_checked_out, git, head, path_arg, registered_missing};

/// A snapshot added for a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub path: PathBuf,
    /// The commit checked out, detached.
    pub commit: String,
}

impl WorktreeManager {
    /// Where the snapshot of `item` in `repo_name` lives.
    pub fn snapshot_path_for(&self, repo_name: &str, item: &QueueItemId) -> PathBuf {
        self.path_for(repo_name, item).join(SNAPSHOT_DIR)
    }

    /// `git -C <repo> worktree add --detach <snapshot> <HEAD commit>`, after
    /// removing a snapshot an earlier run of the item left behind; the
    /// caller makes sure no run of the item is live. The operator's checkout
    /// is only read: its uncommitted changes are not in the snapshot.
    pub fn create_snapshot(
        &self,
        repo_path: &Path,
        repo_name: &str,
        item: &QueueItemId,
    ) -> Result<Snapshot, ArchonError> {
        assert!(repo_path.is_absolute(), "repository paths are canonical");
        let path = self.snapshot_path_for(repo_name, item);
        let item_dir = path.parent().expect("snapshots sit in an item directory");
        refuse_symlink(item_dir)?;
        if item_dir.join(".git").exists() {
            return Err(ArchonError::Invalid(format!(
                "{} holds a single-worktree run of this item; remove it before running a \
                 repo-readonly pipeline",
                item_dir.display()
            )));
        }
        if present(&path)? {
            tracing::info!(snapshot = %path.display(), "removing a snapshot an earlier run left");
            self.remove_snapshot(repo_path, &path)?;
        }
        let commit = git(repo_path, &["rev-parse", "--verify", "HEAD^{commit}"])?;
        std::fs::create_dir_all(item_dir)
            .map_err(|e| ArchonError::io(format!("creating {}", item_dir.display()), e))?;
        forget_snapshot(repo_path, &path)?;
        let args = ["--detach", path_arg(&path)?, &commit];
        add_checked_out(&self.shadow_home(), repo_path, &path, &args)?;
        debug_assert_eq!(head(&path)?, Head::Detached, "git added it detached");
        Ok(Snapshot { path, commit })
    }

    /// Remove the snapshot a `repo-readonly` run of the item left in its
    /// item directory `item_dir`, so a single-worktree run can be added
    /// there. A worktree at `item_dir` itself is left alone: its `snapshot`,
    /// if any, is the repository's own.
    pub(crate) fn remove_leftover_snapshot(
        &self,
        repo_path: &Path,
        item_dir: &Path,
    ) -> Result<(), ArchonError> {
        assert!(
            item_dir.starts_with(&self.root),
            "item directories are under the root"
        );
        let path = item_dir.join(SNAPSHOT_DIR);
        if present(&item_dir.join(".git"))? || !present(&path)? {
            return Ok(());
        }
        tracing::info!(snapshot = %path.display(), "removing a snapshot an earlier run left");
        self.remove_snapshot(repo_path, &path)?;
        debug_assert!(!path.exists(), "the leftover snapshot is gone");
        Ok(())
    }

    /// Remove the snapshot `path` with `git worktree remove --force --force`:
    /// nothing in it is kept, not even for a lock its agent took. Git runs
    /// there only once it is proven the worktree git registered at `path`,
    /// so a symbolic link, for the snapshot or its item directory, or a
    /// rewritten `.git` never leads it elsewhere. The item directory goes
    /// too once empty.
    pub fn remove_snapshot(&self, repo_path: &Path, path: &Path) -> Result<(), ArchonError> {
        assert!(
            path.starts_with(&self.root),
            "refusing to remove {} outside the worktree root",
            path.display()
        );
        assert_eq!(
            path.file_name(),
            Some(SNAPSHOT_DIR.as_ref()),
            "only snapshots are removed with --force"
        );
        let item_dir = path.parent().expect("snapshots sit in an item directory");
        refuse_symlink(item_dir)?;
        if !present(path)? {
            // With the item directory gone too, the next snapshot's
            // `forget_snapshot` deregisters it.
            if item_dir.exists() {
                forget_snapshot(repo_path, path)?;
            }
            return remove_if_empty(item_dir);
        }
        check_worktree(path, &common_dir(repo_path)?)?;
        crate::reaper::clear_for_removal(path)?;
        let args = ["worktree", "remove", "--force", "--force", path_arg(path)?];
        git(repo_path, &args)?;
        debug_assert!(!path.exists(), "git reported success but left the tree");
        remove_if_empty(item_dir)
    }
}

/// Deregister the missing snapshot `path` if git still lists it, with
/// `--force --force`: a lock its agent took before it vanished keeps nothing.
fn forget_snapshot(repo_path: &Path, path: &Path) -> Result<(), ArchonError> {
    let Some(registered) = registered_missing(repo_path, path)? else {
        return Ok(());
    };
    assert!(
        Path::new(&registered).ends_with(SNAPSHOT_DIR),
        "only a snapshot is deregistered with --force"
    );
    tracing::warn!(snapshot = %path.display(), "snapshot vanished; deregistering it");
    git(
        repo_path,
        &["worktree", "remove", "--force", "--force", &registered],
    )?;
    Ok(())
}

/// Whether anything, a dangling symbolic link included, is at `path`.
fn present(path: &Path) -> Result<bool, ArchonError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(ArchonError::io(format!("inspecting {}", path.display()), e)),
    }
}

/// Refuse the item directory `dir` when it is a symbolic link, as gc does:
/// git would add or remove a snapshot wherever it leads.
fn refuse_symlink(dir: &Path) -> Result<(), ArchonError> {
    match std::fs::symlink_metadata(dir) {
        Ok(found) if found.file_type().is_symlink() => Err(ArchonError::Invalid(format!(
            "{} is a symbolic link, so no snapshot is added or removed through it",
            dir.display()
        ))),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(ArchonError::io(format!("inspecting {}", dir.display()), e)),
    }
}

/// Remove the item directory `dir` when the snapshot was all it held; one
/// holding something else, or already gone, is left as it is.
fn remove_if_empty(dir: &Path) -> Result<(), ArchonError> {
    match std::fs::remove_dir(dir) {
        Ok(()) => Ok(()),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty) => Ok(()),
        Err(e) => Err(ArchonError::io(format!("removing {}", dir.display()), e)),
    }
}
