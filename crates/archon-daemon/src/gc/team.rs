//! What a team run leaves beside its run worktree: each role's worktree,
//! worktrees the daemon set aside under `<item>/.set-aside`, and their git
//! directories under `<common>/archon-set-aside` (ARCHITECTURE §13.1); and
//! what a fresh retry set aside of a flat item's run worktree (§13).

use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use archon_core::error::ArchonError;
use archon_pipeline::team::RUN_TREE_DIR;

use super::plan::{Action, Candidate, Found, Kind};
use super::sweep::{Verdict, is_inside};
use crate::worktree::git;
use crate::worktree::team::{GIT_SET_ASIDE_DIR, SET_ASIDE_DIR};

/// Everything of the team run laid out under `root` for gc to consider:
/// the run worktree, unless a shipped run already removed it, and every
/// role's worktree (removed only when git lists them), and what was set
/// aside for this root, in either place. A place that cannot be read is
/// noted in `found` and the rest still collected. A symlinked root is not
/// followed: what it leads to is not this run's.
pub(super) fn candidates(root: &Path, repo: Option<&Path>, action: Action, found: &mut Found) {
    assert!(root.is_absolute(), "team roots are absolute");
    let before = found.candidates.len();
    found.team_roots.push(root.to_path_buf());
    if found.or_note(root, is_real_dir(root)) != Some(true) {
        return;
    }
    let worktree = |workspace: PathBuf| Candidate {
        workspace,
        repo: repo.map(Path::to_path_buf),
        kind: Kind::Worktree,
        action,
    };
    let run_tree = root.join(RUN_TREE_DIR);
    if run_tree.exists() {
        found.candidates.push(worktree(run_tree));
    }
    let roles = found.or_note(root, real_dirs(root)).unwrap_or_default();
    for dir in roles.into_iter().filter(|dir| is_role(dir)) {
        found.candidates.push(worktree(dir));
    }
    let aside = root.join(SET_ASIDE_DIR);
    for dir in found.or_note(&aside, real_dirs(&aside)).unwrap_or_default() {
        let within = aside.clone();
        found
            .candidates
            .push(kept(dir, Kind::KeptWorktree { within }, action));
    }
    // A repository deleted since holds no git directory to collect.
    if let Some(repo) = repo.filter(|repo| repo.is_dir()) {
        let roots = [
            root.to_path_buf(),
            root.canonicalize().unwrap_or(root.into()),
        ];
        let under_root = |tree: &Path| tree.parent().is_some_and(|p| roots.contains(&p.into()));
        git_dirs_set_aside(repo, under_root, action, found);
    }
    debug_assert!(
        found.candidates[before..]
            .iter()
            .all(|c| c.workspace.is_absolute())
    );
}

/// The worktrees a team run may have left directly under `root`: its run
/// worktree and every role's, whether or not git still lists them.
pub(super) fn worktrees(root: &Path) -> Vec<PathBuf> {
    assert!(root.is_absolute(), "team roots are absolute");
    if !is_real_dir(root).unwrap_or(false) {
        return Vec::new();
    }
    let mut dirs = vec![root.join(RUN_TREE_DIR)];
    // Unreadable here is noted when the candidates are gathered.
    let roles = real_dirs(root).unwrap_or_default();
    dirs.extend(roles.into_iter().filter(|dir| is_role(dir)));
    dirs
}

/// A role's worktree: not the run worktree, nor hidden like `.set-aside`.
fn is_role(dir: &Path) -> bool {
    let hidden = dir
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with('.'));
    !hidden && dir.file_name() != Some(RUN_TREE_DIR.as_ref())
}

/// Remove the team root once gc has emptied it, with its empty `.set-aside`,
/// so no stray item directory is left for a later run of the item to meet.
/// The root is reported by its canonical path, as removed worktrees are,
/// and skipped when it is a symlink, which is never followed. `None` when
/// it is gone, outside `worktrees`, or still holds something.
pub(super) fn remove_if_emptied(root: &Path, worktrees: &Path) -> Option<(PathBuf, Verdict)> {
    assert!(root.is_absolute(), "team roots are absolute");
    assert!(worktrees.is_absolute(), "roots are absolute");
    let as_given = |verdict| Some((root.to_path_buf(), verdict));
    match std::fs::symlink_metadata(root) {
        Ok(found) if found.file_type().is_dir() => {}
        Ok(_) => return as_given(Verdict::Skipped("not a real directory".into())),
        Err(e) if e.kind() == ErrorKind::NotFound => return None,
        Err(e) => return as_given(Verdict::Failed(format!("inspecting it: {e}"))),
    }
    let real = match root.canonicalize() {
        Ok(real) => real,
        Err(e) if e.kind() == ErrorKind::NotFound => return None,
        Err(e) => return as_given(Verdict::Failed(format!("resolving it: {e}"))),
    };
    if !is_inside(&real, worktrees) {
        return None;
    }
    remove_emptied(&real).map(|verdict| (real, verdict))
}

/// Remove the real directory `root`, and first its `.set-aside`, if each
/// is empty. A symlinked `.set-aside` is left, so the root stays too.
fn remove_emptied(root: &Path) -> Option<Verdict> {
    let aside = root.join(SET_ASIDE_DIR);
    let real_aside = match is_real_dir(&aside) {
        Ok(real) => real,
        Err(e) => return Some(Verdict::Failed(e.to_string())),
    };
    if real_aside && let Err(e) = remove_empty(&aside) {
        return Some(Verdict::Failed(format!(
            "removing {}: {e}",
            aside.display()
        )));
    }
    match remove_empty(root) {
        Ok(true) => Some(Verdict::Removed { bytes: 0 }),
        Ok(false) => None,
        Err(e) => Some(Verdict::Failed(format!("removing it: {e}"))),
    }
}

/// Whether the empty directory `dir` was removed; one that still holds
/// something, or is gone, is not an error.
fn remove_empty(dir: &Path) -> std::io::Result<bool> {
    match std::fs::remove_dir(dir) {
        Ok(()) => Ok(true),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty) => {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

fn kept(dir: PathBuf, kind: Kind, action: Action) -> Candidate {
    assert!(!matches!(kind, Kind::Worktree), "only what was set aside");
    Candidate {
        workspace: dir,
        repo: None,
        kind,
        action,
    }
}

/// Whether `entry` is `<name>-<n>`, a fresh retry's set-aside copy of the
/// flat run worktree named `name`.
pub(crate) fn set_aside_of(name: &OsStr, entry: &OsStr) -> bool {
    let (name, entry) = (name.to_string_lossy(), entry.to_string_lossy());
    entry
        .strip_prefix(name.as_ref())
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// What fresh retries set aside of the flat run worktree `worktree` of
/// `repo`: each `<worktrees>/<repo>/.set-aside/<item>-<n>`, and each git
/// directory under `<common>/archon-set-aside` whose `gitdir` names it.
pub(super) fn flat_set_asides(worktree: &Path, repo: &Path, action: Action, found: &mut Found) {
    assert!(worktree.is_absolute(), "run workspaces are absolute");
    let (Some(parent), Some(name)) = (worktree.parent(), worktree.file_name()) else {
        return;
    };
    let before = found.candidates.len();
    let is_ours = |dir: &Path| {
        dir.file_name()
            .is_some_and(|entry| set_aside_of(name, entry))
    };
    let aside = parent.join(SET_ASIDE_DIR);
    for dir in found.or_note(&aside, real_dirs(&aside)).unwrap_or_default() {
        if is_ours(&dir) {
            let within = aside.clone();
            found
                .candidates
                .push(kept(dir, Kind::KeptWorktree { within }, action));
        }
    }
    // A fresh retry moves the worktree and its git directory together, so
    // an item that set no worktree aside costs no git call.
    if found.candidates.len() > before && repo.is_dir() {
        let names = [
            worktree.to_path_buf(),
            worktree.canonicalize().unwrap_or(worktree.into()),
        ];
        let is_it = |tree: &Path| names.iter().any(|n| n == tree);
        git_dirs_set_aside(repo, is_it, action, found);
    }
    debug_assert!(
        found.candidates[before..]
            .iter()
            .all(|c| !matches!(c.kind, Kind::Worktree))
    );
}

/// The git directories under `<common>/archon-set-aside` whose `gitdir`
/// file names a worktree `belongs` accepts: this run's, and no other.
fn git_dirs_set_aside(
    repo: &Path,
    belongs: impl Fn(&Path) -> bool,
    action: Action,
    found: &mut Found,
) {
    let args = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
    let Some(common) = found.or_note(repo, git(repo, &args)) else {
        return;
    };
    let aside = PathBuf::from(common).join(GIT_SET_ASIDE_DIR);
    assert!(aside.is_absolute(), "git was asked for an absolute path");
    for dir in found.or_note(&aside, real_dirs(&aside)).unwrap_or_default() {
        let Some(Some(worktree)) = found.or_note(&dir, registered_worktree(&dir)) else {
            continue;
        };
        if belongs(&worktree) {
            let within = aside.clone();
            found
                .candidates
                .push(kept(dir, Kind::KeptGitDir { within }, action));
        }
    }
}

/// The worktree the git directory `dir` was registered for, from the
/// `gitdir` file git wrote: absolute, or relative to the git directory,
/// which sits as deep here as it did under `worktrees/`.
fn registered_worktree(dir: &Path) -> Result<Option<PathBuf>, ArchonError> {
    let file = dir.join("gitdir");
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ArchonError::io(format!("reading {}", file.display()), e)),
    };
    let mut normal = PathBuf::new();
    for part in dir.join(text.trim_end_matches(['\n', '\r'])).components() {
        match part {
            Component::ParentDir => {
                normal.pop();
            }
            Component::CurDir => {}
            other => normal.push(other),
        }
    }
    debug_assert!(normal.is_absolute(), "dir is absolute");
    // `<worktree>/.git`: the worktree is its parent.
    Ok(normal.parent().map(Path::to_path_buf))
}

/// The real directories (not symlinks) directly in `dir`, sorted; none when
/// `dir` is missing or is not itself a real directory.
fn real_dirs(dir: &Path) -> Result<Vec<PathBuf>, ArchonError> {
    if !is_real_dir(dir)? {
        return Ok(Vec::new());
    }
    let reading = |e| ArchonError::io(format!("reading {}", dir.display()), e);
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(reading)? {
        let entry = entry.map_err(reading)?;
        if entry.file_type().map_err(reading)?.is_dir() {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// Whether `path` is a real directory, not a symlink to one; a missing
/// path is not one either.
fn is_real_dir(path: &Path) -> Result<bool, ArchonError> {
    match std::fs::symlink_metadata(path) {
        Ok(found) => Ok(found.file_type().is_dir()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(ArchonError::io(format!("inspecting {}", path.display()), e)),
    }
}
