//! Carrying out an item's plan: each candidate is removed or loses its
//! caches, decided on its real path, since git lists worktrees by it and a
//! symlink inside a root could otherwise point anywhere.

use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use archon_core::error::ArchonError;
use archon_core::protocol::{GcReport, Reclaimed};

use super::plan::{Action, Candidate, Found, Kind};
use super::{Policy, Roots, cache, snapshot, team};
use crate::worktree::shadow::Shadow;
use crate::worktree::{clean, git};

/// What became of one candidate.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    Removed {
        bytes: u64,
    },
    /// Its caches were deleted; `bytes` freed.
    Reclaimed {
        bytes: u64,
    },
    /// Nothing to do.
    Untouched,
    Skipped(String),
    /// Removal was skipped for `why`, but its caches went.
    Kept {
        reclaimed: Option<u64>,
        why: String,
    },
    Failed(String),
}

fn record(report: &mut GcReport, path: &Path, verdict: Verdict) {
    let path = path.to_path_buf();
    let before = report.bytes_freed;
    let mut reclaimed = |path: std::path::PathBuf, bytes: u64| {
        report.bytes_freed = report.bytes_freed.saturating_add(bytes);
        report.reclaimed.push(Reclaimed { path, bytes });
    };
    match verdict {
        Verdict::Removed { bytes } => {
            report.bytes_freed = report.bytes_freed.saturating_add(bytes);
            report.removed.push(path);
        }
        Verdict::Reclaimed { bytes } => reclaimed(path, bytes),
        Verdict::Kept {
            reclaimed: bytes,
            why,
        } => {
            if let Some(bytes) = bytes {
                reclaimed(path.clone(), bytes);
            }
            report.skipped.push((path, why));
        }
        Verdict::Untouched => {}
        Verdict::Skipped(why) => report.skipped.push((path, why)),
        Verdict::Failed(why) => report.failed.push((path, why)),
    }
    debug_assert!(report.bytes_freed >= before, "freed space only grows");
}

/// Collect what was found, then each team root it emptied. A place gc could
/// not read is a failure against its path; the rest is still collected.
pub(super) fn sweep(found: Found, roots: &Roots, policy: &Policy) -> GcReport {
    let unreadable = found.unreadable.len();
    let mut report = GcReport::default();
    for (path, why) in found.unreadable {
        record(&mut report, &path, Verdict::Failed(why));
    }
    for candidate in &found.candidates {
        let (path, verdict) = collect(candidate, roots, policy);
        record(&mut report, &path, verdict);
    }
    for root in &found.team_roots {
        if let Some((path, verdict)) = team::remove_if_emptied(root, &roots.worktrees) {
            record(&mut report, &path, verdict);
        }
    }
    assert!(report.failed.len() >= unreadable, "no failure is dropped");
    debug_assert!(
        report.removed.len() + report.skipped.len() + report.failed.len()
            <= unreadable + found.candidates.len() + found.team_roots.len()
    );
    report
}

/// Strictly below `root`, with no `..` that could climb out of it.
pub(super) fn is_inside(path: &Path, root: &Path) -> bool {
    let plain = path
        .components()
        .all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
    plain && path != root && path.starts_with(root)
}

/// The candidate's real path and what became of it.
pub(super) fn collect(
    candidate: &Candidate,
    roots: &Roots,
    policy: &Policy,
) -> (std::path::PathBuf, Verdict) {
    let workspace = &candidate.workspace;
    assert!(workspace.is_absolute(), "run workspaces are absolute paths");
    let path = match workspace.canonicalize() {
        Ok(path) => path,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let verdict = match candidate.action {
                Action::Remove => Verdict::Skipped("it no longer exists".into()),
                Action::Reclaim => Verdict::Untouched,
            };
            return (workspace.clone(), verdict);
        }
        Err(e) => {
            return (
                workspace.clone(),
                Verdict::Failed(format!("resolving it: {e}")),
            );
        }
    };
    let verdict = match (&candidate.kind, candidate.action) {
        (Kind::KeptWorktree { within } | Kind::KeptGitDir { within }, Action::Remove) => {
            remove_kept(&path, within)
        }
        (Kind::KeptWorktree { within }, Action::Reclaim) => reclaim_kept(&path, within, policy),
        (Kind::KeptGitDir { .. }, Action::Reclaim) => Verdict::Untouched,
        (Kind::Worktree, action) => collect_workspace(candidate, &path, roots, policy, action),
        (Kind::Snapshot { item }, action) => {
            assert_eq!(action, Action::Remove, "snapshots are only ever removed");
            snapshot::remove(candidate, &path, roots, item)
        }
    };
    (path, verdict)
}

/// A run workspace: a scratch directory, or a worktree inside the root.
fn collect_workspace(
    candidate: &Candidate,
    path: &Path,
    roots: &Roots,
    policy: &Policy,
    action: Action,
) -> Verdict {
    assert!(path.is_absolute(), "canonical paths are absolute");
    if is_inside(path, &roots.scratch) {
        // No git runs in a scratch directory to tell its caches apart.
        return match action {
            Action::Remove => remove_dir(path),
            Action::Reclaim => Verdict::Untouched,
        };
    }
    if !is_inside(path, &roots.worktrees) {
        return match action {
            Action::Remove => Verdict::Skipped(format!(
                "outside the worktree root {} and the scratch directory",
                roots.worktrees.display()
            )),
            Action::Reclaim => Verdict::Untouched,
        };
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
    match action {
        Action::Remove => remove_worktree(repo, path, roots, policy),
        Action::Reclaim => reclaimed(cache::reclaim_worktree(path, &policy.cache_dirs)),
    }
}

fn reclaimed(result: Result<Option<u64>, ArchonError>) -> Verdict {
    match result {
        Ok(Some(bytes)) => Verdict::Reclaimed { bytes },
        Ok(None) => Verdict::Untouched,
        Err(e) => Verdict::Failed(format!("deleting its caches: {e}")),
    }
}

/// `path` is canonical; it goes only when it is strictly inside `within`,
/// resolved, and `within` is a real directory, not a symlink leading away.
fn remove_kept(path: &Path, within: &Path) -> Verdict {
    match check_kept(path, within) {
        Some(skipped) => skipped,
        None => remove_dir(path),
    }
}

/// A set-aside worktree runs no git, so only `cache_dirs` go from it.
fn reclaim_kept(path: &Path, within: &Path, policy: &Policy) -> Verdict {
    match check_kept(path, within) {
        Some(skipped) => skipped,
        None => reclaimed(cache::reclaim_kept(path, &policy.cache_dirs)),
    }
}

/// `None` when the canonical `path` is strictly inside the real directory
/// `within`; else the skip that says why not.
pub(super) fn check_kept(path: &Path, within: &Path) -> Option<Verdict> {
    assert!(path.is_absolute(), "canonical paths are absolute");
    let real_dir = std::fs::symlink_metadata(within).is_ok_and(|m| m.file_type().is_dir());
    let inside = within
        .canonicalize()
        .is_ok_and(|within| is_inside(path, &within));
    let ok = real_dir && inside;
    (!ok).then(|| {
        Verdict::Skipped(format!(
            "not inside the real directory {}",
            within.display()
        ))
    })
}

fn remove_dir(path: &Path) -> Verdict {
    assert!(path.is_absolute(), "canonical paths are absolute");
    let bytes = match cache::disk_usage(path) {
        Ok(bytes) => bytes,
        Err(e) => return Verdict::Failed(e.to_string()),
    };
    match std::fs::remove_dir_all(path) {
        Ok(()) => Verdict::Removed { bytes },
        Err(e) => Verdict::Failed(e.to_string()),
    }
}

/// `path` is canonical, inside the worktree root, and a worktree of
/// `repo`. One with uncommitted changes stays, without its caches.
fn remove_worktree(repo: &Path, path: &Path, roots: &Roots, policy: &Policy) -> Verdict {
    // Reaped first: a process ending on SIGTERM may still write work.
    if let Err(e) = crate::reaper::clear_for_removal(&roots.worktrees, path) {
        return Verdict::Failed(e.to_string());
    }
    if !policy.force {
        match unremovable(&roots.data_dir, repo, path, &policy.cache_dirs) {
            Ok(None) => {}
            Ok(Some(why)) => {
                let reclaimed = match if policy.reclaim_before == archon_core::Timestamp::MIN_UTC {
                    Ok(None)
                } else {
                    cache::reclaim_worktree(path, &policy.cache_dirs)
                } {
                    Ok(bytes) => bytes,
                    Err(e) => return Verdict::Failed(format!("deleting its caches: {e}")),
                };
                let why = format!("{why}; `archon gc --force` removes it anyway");
                return Verdict::Kept { reclaimed, why };
            }
            Err(e) => return Verdict::Failed(e.to_string()),
        }
    }
    let bytes = match cache::disk_usage(path) {
        Ok(bytes) => bytes,
        Err(e) => return Verdict::Failed(e.to_string()),
    };
    // Git's own check, which runs the repository's filters, was asked above
    // instead; a locked worktree is still refused.
    match git_worktree_remove(repo, path, 1) {
        Ok(()) => Verdict::Removed { bytes },
        Err(e) => Verdict::Failed(e.to_string()),
    }
}

/// Whether git lists the canonical `path` as a worktree of `repo` other than
/// its main one, comparing canonical forms on both sides. `repo` may be a
/// repository or its common git directory.
pub(super) fn is_listed_worktree(repo: &Path, path: &Path) -> Result<bool, ArchonError> {
    let repo = repo
        .canonicalize()
        .map_err(|e| ArchonError::io(format!("resolving {}", repo.display()), e))?;
    let out = git(&repo, &["worktree", "list", "--porcelain"])?;
    // A listed path that cannot be resolved (a pruned worktree) is compared
    // as git wrote it; it cannot equal an existing directory's real path.
    let listed = out
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(|listed| {
            Path::new(listed)
                .canonicalize()
                .unwrap_or_else(|_| listed.into())
        })
        .any(|listed| listed == path);
    Ok(listed && path != repo)
}

/// Whether git has `path` locked, which `git worktree remove` refuses even with one `--force`.
pub(super) fn is_locked_worktree(repo: &Path, path: &Path) -> Result<bool, ArchonError> {
    let out = git(repo, &["worktree", "list", "--porcelain"])?;
    assert!(
        out.contains("worktree "),
        "the main worktree is always listed"
    );
    let locked = out.split("\n\n").any(|block| {
        let mut lines = block.lines();
        let listed = lines
            .next()
            .and_then(|l| l.strip_prefix("worktree "))
            .map(|l| Path::new(l).canonicalize().unwrap_or_else(|_| l.into()));
        listed.as_deref() == Some(path)
            && block
                .lines()
                .any(|l| l == "locked" || l.starts_with("locked "))
    });
    Ok(locked)
}

/// Why `git worktree remove` would refuse `worktree` without `--force`, untracked
/// caches aside, asked through `repo`'s shadow under `data_dir` so no filter runs.
pub(super) fn unremovable(
    data_dir: &Path,
    repo: &Path,
    worktree: &Path,
    cache_dirs: &[PathBuf],
) -> Result<Option<String>, ArchonError> {
    let shadow = Shadow::of(data_dir, repo)?;
    clean::unremovable(&shadow, worktree, cache_dirs)
}

/// `git worktree remove` with `forces` times `--force`: once removes a
/// dirty worktree, twice a locked one too.
pub(super) fn git_worktree_remove(
    repo: &Path,
    path: &Path,
    forces: usize,
) -> Result<(), ArchonError> {
    assert!(forces <= 2, "git knows no stronger force");
    let path_text = path
        .to_str()
        .ok_or_else(|| ArchonError::Invalid(format!("{} is not UTF-8", path.display())))?;
    let mut args = vec!["worktree", "remove"];
    args.extend(std::iter::repeat_n("--force", forces));
    args.push(path_text);
    git(repo, &args)?;
    debug_assert!(!path.exists(), "git reported success but left the tree");
    Ok(())
}
