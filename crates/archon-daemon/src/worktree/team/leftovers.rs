//! What a role can leave behind in its worktree, and how the daemon saves
//! it without losing work: operations in progress ended keeping files and
//! index, uncommitted work committed, a detached `HEAD` given a name.

use std::path::Path;

use archon_core::error::ArchonError;

use super::{SavedWork, daemon_commit_options, git, merge_in_progress, nul_entries, nul_paths};

/// What [`commit_leftovers`] did.
#[derive(Debug, Default)]
pub(super) struct Leftovers {
    /// What it committed, if anything.
    pub saved: Option<SavedWork>,
    /// What git cannot commit there, which stays as it is: repositories
    /// nested in the worktree, and changes to tracked files of submodules.
    pub kept: Vec<String>,
}

/// Git operations a role can leave in progress: the marker under the git
/// directory, and the command that ends it keeping files and index.
const OPERATIONS: [(&str, &str, &[&str]); 7] = [
    ("rebase-apply/applying", "am", &["am", "--quit"]),
    ("rebase-apply", "rebase", &["rebase", "--quit"]),
    ("rebase-merge", "rebase", &["rebase", "--quit"]),
    (
        "CHERRY_PICK_HEAD",
        "cherry-pick",
        &["cherry-pick", "--quit"],
    ),
    ("REVERT_HEAD", "revert", &["revert", "--quit"]),
    (
        "sequencer",
        "cherry-pick or revert",
        &["cherry-pick", "--quit"],
    ),
    ("BISECT_LOG", "bisect", &["bisect", "reset", "HEAD"]),
];

/// The first of [`OPERATIONS`] left in progress in the worktree `path`,
/// by its name and the command that ends it; one `rev-parse` finds them.
fn pending_operation(
    path: &Path,
) -> Result<Option<(&'static str, &'static [&'static str])>, ArchonError> {
    let mut args = vec!["rev-parse", "--path-format=absolute"];
    for (marker, _, _) in OPERATIONS {
        args.extend(["--git-path", marker]);
    }
    let listed = git(path, &args)?;
    let markers: Vec<&Path> = listed.lines().map(Path::new).collect();
    assert_eq!(markers.len(), OPERATIONS.len(), "git names every marker");
    assert!(
        markers.iter().all(|marker| marker.is_absolute()),
        "git was asked for absolute paths"
    );
    let found = markers
        .iter()
        .zip(OPERATIONS)
        .find(|(marker, _)| marker.exists())
        .map(|(_, (_, name, quit))| (name, quit));
    Ok(found)
}

/// The operation left in progress in the worktree `path`, if any.
pub(super) fn operation_in_progress(path: &Path) -> Result<Option<&'static str>, ArchonError> {
    if let Some((name, _)) = pending_operation(path)? {
        return Ok(Some(name));
    }
    if merge_in_progress(path)? {
        return Ok(Some("merge"));
    }
    Ok(None)
}

/// End every operation left in progress in the worktree `path`: its files
/// and index stay as they are, and `HEAD` stays where it is.
pub(super) fn quit_operations(path: &Path) -> Result<(), ArchonError> {
    assert!(path.is_absolute(), "worker worktrees are absolute");
    // Each command ends one kind; one git cannot end is caught below.
    for _ in OPERATIONS {
        let Some((name, quit)) = pending_operation(path)? else {
            break;
        };
        tracing::warn!(worktree = %path.display(), operation = name, "ending an operation a role left in progress");
        git(path, quit)?;
    }
    if merge_in_progress(path)? {
        tracing::warn!(worktree = %path.display(), "ending a merge a role left in progress");
        git(path, &["merge", "--quit"])?;
    }
    if let Some(left) = operation_in_progress(path)? {
        return Err(ArchonError::Invalid(format!(
            "git did not end the {left} left in progress in {}",
            path.display()
        )));
    }
    Ok(())
}

/// A detached `HEAD` about to be switched away from may hold commits no
/// branch has: name it so that git keeps them. Returns the name.
pub(super) fn keep_detached_head(path: &Path) -> Result<String, ArchonError> {
    let commit = git(path, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let reference = keep_commit(path, &commit)?;
    tracing::warn!(worktree = %path.display(), %reference, "kept the detached HEAD a role left");
    Ok(reference)
}

/// Name the full object name `commit`, which must be a commit of the
/// repository `tree` belongs to, `refs/archon/saved/<commit>`, so that
/// git keeps it. Returns the name.
pub(super) fn keep_commit(tree: &Path, commit: &str) -> Result<String, ArchonError> {
    assert!(
        commit.len() >= 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "a full object name"
    );
    let reference = format!("refs/archon/saved/{commit}");
    // `update-ref` would name any object; only a commit is kept.
    let verified = git(
        tree,
        &["rev-parse", "--verify", &format!("{commit}^{{commit}}")],
    )?;
    if verified != commit {
        return Err(ArchonError::Invalid(format!(
            "{commit} is not the full name of commit {verified}"
        )));
    }
    git(tree, &["update-ref", &reference, commit])?;
    Ok(reference)
}

/// Commit whatever a role left uncommitted in its worktree `path`,
/// untracked files included, onto what is checked out there (`on` names
/// it): before its branch is merged, or before the worktree moves on.
pub(super) fn commit_leftovers(
    path: &Path,
    on: &str,
    message: &str,
) -> Result<Leftovers, ArchonError> {
    assert!(path.is_absolute(), "worker worktrees are absolute");
    assert!(
        !message.is_empty(),
        "the daemon's commits say what they are"
    );
    if changed_paths(path)?.is_empty() {
        return Ok(Leftovers::default());
    }
    let Some(nested) = nested_repositories(path)? else {
        tracing::warn!(worktree = %path.display(), "a nested repository's name is not UTF-8; committing nothing");
        let kept = changed_paths(path)?;
        return Ok(Leftovers { saved: None, kept });
    };
    let mut add = vec![
        "add",
        "--all",
        "--",
        ".",
        ":(exclude,glob)**/__pycache__/**",
    ];
    add.extend([":(exclude,glob)**/*.pyc", ":(exclude,glob)**/*.pyo"]);
    add.extend(nested.iter().map(String::as_str));
    git(path, &add)?;
    unstage_caches(path)?;
    let staged = nul_paths(path, &["diff", "--cached", "--name-only"])?;
    let saved = match staged.is_empty() {
        true => None,
        false => Some(commit_staged(path, on, message, staged)?),
    };
    let kept = changed_paths(path)?;
    debug_assert!(
        nul_paths(path, &["diff", "--cached", "--name-only"])?.is_empty(),
        "nothing staged is left behind"
    );
    Ok(Leftovers { saved, kept })
}

/// Pathspecs leaving out every repository nested untracked in the worktree
/// `path`, which `add --all` would commit as a gitlink that no
/// `.gitmodules` names, and ship. `None` when a name is not UTF-8, as a
/// pathspec could not name it exactly.
fn nested_repositories(path: &Path) -> Result<Option<Vec<String>>, ArchonError> {
    let untracked = nul_entries(path, &["ls-files", "--others", "--exclude-standard"])?;
    let mut excluded = Vec::new();
    for dir in untracked.iter().filter(|entry| entry.ends_with(b"/")) {
        let Ok(dir) = std::str::from_utf8(dir) else {
            return Ok(None);
        };
        let dir = dir.trim_end_matches('/');
        assert!(!dir.is_empty(), "git names nested repositories by path");
        excluded.push(format!(":(exclude,literal){dir}"));
    }
    Ok(Some(excluded))
}

/// Commit the `files` staged in `path` as the daemon.
fn commit_staged(
    path: &Path,
    on: &str,
    message: &str,
    files: Vec<String>,
) -> Result<SavedWork, ArchonError> {
    assert!(!files.is_empty(), "only staged files are committed");
    let before = git(path, &["rev-parse", "HEAD"])?;
    let mut args = daemon_commit_options(path)?;
    args.extend(["commit", "--quiet", "-m", message]);
    git(path, &args)?;
    let commit = git(path, &["rev-parse", "HEAD"])?;
    assert_ne!(before, commit, "the commit moved HEAD");
    tracing::warn!(worktree = %path.display(), %commit, message, files = ?files, "committed the work a role left uncommitted");
    Ok(SavedWork {
        worktree: path.to_owned(),
        on: on.to_owned(),
        commit,
        files,
    })
}

/// Take caches the role staged itself out of the index; its files stay.
fn unstage_caches(path: &Path) -> Result<(), ArchonError> {
    let staged = nul_entries(path, &["diff", "--cached", "--name-only"])?;
    let mut caches = Vec::new();
    for name in staged {
        let name = match String::from_utf8(name) {
            Ok(name) => name,
            Err(raw) if is_python_cache(&String::from_utf8_lossy(raw.as_bytes())) => {
                return Err(ArchonError::Invalid(format!(
                    "a cache staged in {} has a name that is not UTF-8, so it cannot be \
                     left out of the commit: {}",
                    path.display(),
                    String::from_utf8_lossy(raw.as_bytes())
                )));
            }
            Err(_) => continue,
        };
        if is_python_cache(&name) {
            caches.push(format!(":(literal){name}"));
        }
    }
    if caches.is_empty() {
        return Ok(());
    }
    let mut reset = vec!["reset", "--quiet", "--"];
    reset.extend(caches.iter().map(String::as_str));
    git(path, &reset)?;
    debug_assert!(
        nul_paths(path, &["diff", "--cached", "--name-only"])?
            .iter()
            .all(|name| !is_python_cache(name)),
        "no cache stays staged"
    );
    Ok(())
}

/// Bytecode a worker's test run leaves: never committed on its behalf, and
/// not a reason to refuse its branch.
fn is_python_cache(path: &str) -> bool {
    path.split('/').any(|part| part == "__pycache__")
        || path.ends_with(".pyc")
        || path.ends_with(".pyo")
}

/// The paths `git status` reports changed in `tree`, untracked included,
/// but not a submodule holding only untracked files, such as its build
/// output. Before `add` only whether it is empty matters; after the commit
/// nothing is staged, so no entry is a rename's second name.
fn changed_paths(tree: &Path) -> Result<Vec<String>, ArchonError> {
    let args = [
        "status",
        "--porcelain",
        "--untracked-files=all",
        "--ignore-submodules=untracked",
    ];
    let entries = nul_paths(tree, &args)?;
    let changed: Vec<String> = entries
        .iter()
        .map(|entry| entry.get(3..).unwrap_or(entry).to_owned())
        .filter(|path| !is_python_cache(path))
        .collect();
    debug_assert!(changed.iter().all(|path| !path.is_empty()));
    Ok(changed)
}
