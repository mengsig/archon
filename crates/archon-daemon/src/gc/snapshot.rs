//! A `repo-readonly` run's snapshot that outlived its run: archond removes
//! it when the run ends, unless that failed or archond stopped first.
//! Nothing in a snapshot is kept, so it goes whole, dirty or locked, with
//! or without `--force`.

use std::io::ErrorKind;
use std::path::Path;

use archon_core::catalog::is_snapshot_of;
use archon_core::id::QueueItemId;

use super::plan::Candidate;
use super::sweep::{Verdict, git_worktree_remove, is_inside, is_listed_worktree};
use super::{Roots, cache};

/// Remove the candidate, whose canonical path is `path`, when it is a real
/// directory (not a symlink), still `item`'s own snapshot directory once
/// resolved, inside the worktree root, and a worktree git lists for its
/// repository; then its item directory, if that is left empty.
pub(super) fn remove(
    candidate: &Candidate,
    path: &Path,
    roots: &Roots,
    item: &QueueItemId,
) -> Verdict {
    assert!(path.is_absolute(), "canonical paths are absolute");
    let workspace = &candidate.workspace;
    assert!(is_snapshot_of(workspace, item), "only snapshots come here");
    let real_dir = std::fs::symlink_metadata(workspace).is_ok_and(|m| m.file_type().is_dir());
    if !real_dir || !is_snapshot_of(path, item) || !is_inside(path, &roots.worktrees) {
        return Verdict::Skipped(format!(
            "not the snapshot directory of {item} inside {}",
            roots.worktrees.display()
        ));
    }
    let Some(repo) = &candidate.repo else {
        return Verdict::Skipped(
            "its repository is no longer registered and its `.git` names none".into(),
        );
    };
    match is_listed_worktree(repo, path) {
        Ok(true) => {}
        Ok(false) => return Verdict::Skipped(format!("not a worktree of {}", repo.display())),
        Err(e) => return Verdict::Failed(e.to_string()),
    }
    let bytes = match cache::disk_usage(path) {
        Ok(bytes) => bytes,
        Err(e) => return Verdict::Failed(e.to_string()),
    };
    if let Err(e) = crate::reaper::clear_for_removal(&roots.worktrees, path) {
        return Verdict::Failed(e.to_string());
    }
    // Twice: a lock its agent took keeps nothing worth keeping.
    if let Err(e) = git_worktree_remove(repo, path, 2) {
        return Verdict::Failed(e.to_string());
    }
    remove_emptied_item_dir(path, bytes)
}

/// The snapshot at `path` is gone; its item directory goes too when it held
/// nothing else.
fn remove_emptied_item_dir(path: &Path, bytes: u64) -> Verdict {
    assert!(!path.exists(), "the snapshot was removed first");
    let item_dir = path.parent().expect("snapshots sit in an item directory");
    match std::fs::remove_dir(item_dir) {
        Ok(()) => Verdict::Removed { bytes },
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty) => {
            Verdict::Removed { bytes }
        }
        Err(e) => Verdict::Failed(format!(
            "removed, but not its item directory {}: {e}",
            item_dir.display()
        )),
    }
}
