//! Deleting a finished run's build caches while keeping its source: the
//! ignored files in the configured `worktrees.cache_dirs`, or the known
//! build-cache directories when none are set (`git clean -dX` on literal
//! paths), never anything under `.archon/`, and measuring the space freed.

use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;

use crate::worktree::{git, toplevel};

/// `st_blocks` counts 512-byte units whatever the filesystem's block size.
const BLOCK_BYTES: u64 = 512;

/// Git looks upwards for a repository: never clean some enclosing one.
fn ensure_top_level(tree: &Path) -> Result<(), ArchonError> {
    assert!(tree.is_absolute(), "worktree paths are absolute");
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
    Ok(())
}

/// Build output at the top of a worktree that `cache_dirs` replaces when set.
const DEFAULT_CACHE_ROOTS: [&str; 6] = ["target", "dist", "build", ".venv", "venv", ".tox"];
/// Build and tool caches named at any depth, such as a nested package's.
const DEFAULT_CACHE_NAMES: [&str; 5] = [
    "node_modules",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
];
/// Archon's own directory in a worktree: reports, evidence and services a
/// retry needs again. No cache is ever looked for in it.
const ARCHON_DIR: &str = ".archon";

/// Delete the ignored files of the worktree `tree` within `cache_dirs`, or
/// the known build caches when none are configured; other ignored files such
/// as `.env` or `.archon/reports/` stay, as do untracked source, changes and
/// commits. `None` when there was nothing to delete; else the bytes freed.
pub(crate) fn reclaim_worktree(
    tree: &Path,
    cache_dirs: &[PathBuf],
) -> Result<Option<u64>, ArchonError> {
    ensure_top_level(tree)?;
    let pathspecs = cache_pathspecs(tree, cache_dirs)?;
    if pathspecs.is_empty() {
        return Ok(None);
    }
    let clean = |mode| {
        let args = clean_args(mode, &pathspecs);
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

/// `git clean -dX` with `mode` (`-n` or `-f`) on exactly `pathspecs`:
/// ignored files only, never nested repositories (that takes `-ff`).
fn clean_args(mode: &str, pathspecs: &[String]) -> Vec<String> {
    assert!(matches!(mode, "-n" | "-f"), "a dry run or the real one");
    assert!(!pathspecs.is_empty(), "no pathspec would clean everything");
    let mut args: Vec<String> = ["clean", mode, "-d", "-X", "--"].map(String::from).into();
    args.extend_from_slice(pathspecs);
    args
}

/// One literal pathspec per cache: each of `cache_dirs`, or each known build
/// cache git lists in `tree` when none are configured, but none holding an
/// `.archon` directory. Only literal paths: with a `:(glob)**/name` one, git
/// deletes every wholly ignored directory.
fn cache_pathspecs(tree: &Path, cache_dirs: &[PathBuf]) -> Result<Vec<String>, ArchonError> {
    let dirs: Vec<String> = match cache_dirs.is_empty() {
        true => default_caches(tree)?,
        false => cache_dirs.iter().map(|d| d.display().to_string()).collect(),
    };
    let mut pathspecs = Vec::with_capacity(dirs.len());
    for dir in &dirs {
        assert!(
            !dir.is_empty() && !dir.starts_with('/'),
            "relative to the worktree"
        );
        assert!(
            !is_under_archon(dir),
            "{dir}: nothing under .archon/ is a cache"
        );
        if !holds_archon_dir(&tree.join(dir))? {
            pathspecs.push(format!(":(literal){dir}"));
        }
    }
    Ok(pathspecs)
}

/// Whether a directory named `.archon` is at or below `path`, following no
/// symlink: git would delete it with the cache that holds it.
fn holds_archon_dir(path: &Path) -> Result<bool, ArchonError> {
    assert!(path.is_absolute(), "a path inside a worktree");
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
        if next.file_name().is_some_and(|name| name == ARCHON_DIR) {
            return Ok(true);
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
    Ok(false)
}

/// The ignored paths of `tree` in known build caches. Git lists a wholly
/// ignored directory once, without what is in it, so a cache inside ignored
/// data such as `bench/results/` is never listed, nor deleted.
fn default_caches(tree: &Path) -> Result<Vec<String>, ArchonError> {
    let args = [
        "ls-files",
        "-z",
        "--others",
        "--ignored",
        "--exclude-standard",
        "--directory",
    ];
    let listed = git(tree, &args)?;
    let caches: Vec<String> = listed
        .split('\0')
        .filter(|entry| is_in_default_cache(entry))
        .map(|entry| entry.trim_end_matches('/').to_owned())
        .collect();
    assert!(
        caches.iter().all(|dir| !dir.is_empty()),
        "git names each path"
    );
    Ok(caches)
}

/// Whether the ignored `entry` as git lists it (a directory ends in `/`) is
/// or lies in a known cache: a build-output root at the top, or a cache name
/// at any depth, such as `target/debug/` of a `target/` with tracked files.
fn is_in_default_cache(entry: &str) -> bool {
    let (path, is_dir) = match entry.strip_suffix('/') {
        Some(dir) => (dir, true),
        None => (entry, false),
    };
    if path.is_empty() || is_under_archon(path) {
        return false;
    }
    let parts: Vec<&str> = path.split('/').collect();
    let dirs = if is_dir {
        &parts[..]
    } else {
        &parts[..parts.len() - 1]
    };
    let at_root = dirs
        .first()
        .is_some_and(|top| DEFAULT_CACHE_ROOTS.contains(top));
    at_root || dirs.iter().any(|dir| DEFAULT_CACHE_NAMES.contains(dir))
}

fn is_under_archon(dir: &str) -> bool {
    dir.split('/').any(|part| part == ARCHON_DIR)
}

/// What [`reclaim_worktree`] would delete from `tree`, one path as git lists
/// it each, with nothing deleted.
pub(crate) fn preview_worktree(
    tree: &Path,
    cache_dirs: &[PathBuf],
) -> Result<Vec<String>, ArchonError> {
    ensure_top_level(tree)?;
    let pathspecs = cache_pathspecs(tree, cache_dirs)?;
    if pathspecs.is_empty() {
        return Ok(Vec::new());
    }
    let args = clean_args("-n", &pathspecs);
    let listed = git(tree, &args.iter().map(String::as_str).collect::<Vec<_>>())?;
    let paths: Vec<String> = listed
        .lines()
        .map(|line| {
            line.strip_prefix("Would remove ")
                .unwrap_or(line)
                .to_owned()
        })
        .collect();
    assert!(paths.iter().all(|p| !p.is_empty()), "git names each path");
    Ok(paths)
}

/// Delete each of `cache_dirs` from the set-aside worktree `tree`, in which
/// no git runs, where it is a real directory reached through no symlink and
/// holds no `.archon` directory. `None` when none was there, or configured.
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
        if is_dir && holds_archon_dir(&path)? {
            continue;
        }
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
