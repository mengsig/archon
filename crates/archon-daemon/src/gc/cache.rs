//! Deleting a finished run's build caches while keeping its source: what
//! git ignores in a worktree (`git clean -dX`), or only the configured
//! `worktrees.cache_dirs`, and measuring the space that frees.

use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;

use crate::worktree::{git, toplevel};

/// `st_blocks` counts 512-byte units whatever the filesystem's block size.
const BLOCK_BYTES: u64 = 512;

/// Delete the ignored files of the worktree `tree`, within `cache_dirs`
/// when any are configured. Untracked source, changes and commits stay.
/// `None` when there was nothing to delete; else the bytes freed.
pub(crate) fn reclaim_worktree(
    tree: &Path,
    cache_dirs: &[PathBuf],
) -> Result<Option<u64>, ArchonError> {
    assert!(tree.is_absolute(), "worktree paths are absolute");
    // Git looks upwards for a repository: never clean some enclosing one.
    let real = tree
        .canonicalize()
        .map_err(|e| ArchonError::io(format!("resolving {}", tree.display()), e))?;
    let top = toplevel(tree)?;
    if top != real {
        return Err(ArchonError::Invalid(format!(
            "{} is not a git worktree's top level ({} is)",
            tree.display(),
            top.display()
        )));
    }
    let clean = |mode| {
        let args = clean_args(mode, cache_dirs);
        git(tree, &args.iter().map(String::as_str).collect::<Vec<_>>())
    };
    if clean("-n")?.is_empty() {
        return Ok(None);
    }
    let before = disk_usage(tree)?;
    clean("-f")?;
    let after = disk_usage(tree)?;
    Ok(Some(before.saturating_sub(after)))
}

/// `git clean -dX` with `mode` (`-n` or `-f`): ignored files only, never
/// nested repositories (that takes `-ff`), limited to `cache_dirs` if set.
fn clean_args(mode: &str, cache_dirs: &[PathBuf]) -> Vec<String> {
    assert!(matches!(mode, "-n" | "-f"), "a dry run or the real one");
    let mut args: Vec<String> = ["clean", mode, "-d", "-X", "--"].map(String::from).into();
    for dir in cache_dirs {
        assert!(dir.is_relative(), "config validation keeps them relative");
        args.push(format!(":(literal){}", dir.display()));
    }
    args
}

/// Delete each of `cache_dirs` from the set-aside worktree `tree`, in which
/// no git runs, where it is a real directory reached through no symlink.
/// `None` when none was there, or none is configured.
pub(crate) fn reclaim_kept(
    tree: &Path,
    cache_dirs: &[PathBuf],
) -> Result<Option<u64>, ArchonError> {
    assert!(tree.is_absolute(), "set-aside paths are canonical");
    let mut freed = None;
    for dir in cache_dirs {
        assert!(dir.is_relative(), "config validation keeps them relative");
        let path = tree.join(dir);
        let is_dir = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_dir());
        // Its canonical form is itself only when no symlink leads there.
        if !is_dir || path.canonicalize().ok().as_ref() != Some(&path) {
            continue;
        }
        let bytes = disk_usage(&path)?;
        std::fs::remove_dir_all(&path)
            .map_err(|e| ArchonError::io(format!("removing {}", path.display()), e))?;
        freed = Some(freed.unwrap_or(0u64).saturating_add(bytes));
    }
    Ok(freed)
}

/// Delete the tree at `path` and return the space it took. Test suites leave
/// directories without rwx for the owner, which `remove_dir_all` cannot enter,
/// so each is made owner-accessible first. Symlinks are never followed.
pub(crate) fn remove_tree(path: &Path) -> Result<u64, ArchonError> {
    let mut pending = vec![path.to_path_buf()];
    while let Some(next) = pending.pop() {
        let meta = match std::fs::symlink_metadata(&next) {
            Ok(meta) => meta,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(ArchonError::io(format!("inspecting {}", next.display()), e)),
        };
        if !meta.file_type().is_dir() {
            continue;
        }
        let mode = meta.permissions().mode();
        if mode & 0o700 != 0o700 {
            let opened = std::fs::Permissions::from_mode(mode | 0o700);
            std::fs::set_permissions(&next, opened)
                .map_err(|e| ArchonError::io(format!("making {} accessible", next.display()), e))?;
        }
        let reading = |e| ArchonError::io(format!("reading {}", next.display()), e);
        for entry in std::fs::read_dir(&next).map_err(reading)? {
            pending.push(entry.map_err(reading)?.path());
        }
    }
    let bytes = disk_usage(path)?;
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(bytes),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(0),
        Err(e) => Err(ArchonError::io(format!("removing {}", path.display()), e)),
    }
}

/// The disk space the files under `path` take, following no symlink. What
/// vanishes meanwhile counts as nothing.
pub(crate) fn disk_usage(path: &Path) -> Result<u64, ArchonError> {
    let mut total = 0u64;
    let mut pending = vec![path.to_path_buf()];
    while let Some(next) = pending.pop() {
        let meta = match std::fs::symlink_metadata(&next) {
            Ok(meta) => meta,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(ArchonError::io(format!("inspecting {}", next.display()), e)),
        };
        total = total.saturating_add(meta.blocks().saturating_mul(BLOCK_BYTES));
        if !meta.file_type().is_dir() {
            continue;
        }
        let reading = |e| ArchonError::io(format!("reading {}", next.display()), e);
        let entries = match std::fs::read_dir(&next) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(reading(e)),
        };
        for entry in entries {
            pending.push(entry.map_err(reading)?.path());
        }
    }
    Ok(total)
}
