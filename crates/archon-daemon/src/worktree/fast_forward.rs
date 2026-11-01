//! Bringing a registered checkout's local base branch up to the fetched
//! `origin/<base>`: a fast-forward and nothing else. A base that is ahead or
//! diverged, being rebased or bisected, or checked out with local changes or
//! a file in the move's way is left where it is, with the reason. Nothing is
//! reset, rebased, stashed or forced.

use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;

use super::{commit_id, git, is_ancestor};

/// What [`fast_forward`] did to the local base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Advance {
    /// Nothing to do: it is at `origin`'s, or either branch is missing.
    Current,
    /// Moved forward from one commit to the other.
    Moved { from: String, to: String },
    /// Left where it is.
    Kept(Kept),
}

/// Why the local base was left where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Kept {
    /// A stable name of the reason, so it is reported once.
    pub kind: &'static str,
    /// The reason in words, with what would let it move.
    pub why: String,
}

impl Kept {
    fn advance(kind: &'static str, why: String) -> Advance {
        assert!(!kind.is_empty() && !why.is_empty(), "a reason is named");
        Advance::Kept(Self { kind, why })
    }
}

fn heads(name: &str) -> String {
    format!("refs/heads/{name}")
}

/// Fast-forward `repo`'s `refs/heads/<base>` to `refs/remotes/origin/<base>`
/// when it is a strict ancestor of it; in the work tree it is checked out in,
/// if any, only when that has no local change.
pub(crate) fn fast_forward(repo: &Path, base: &str) -> Result<Advance, ArchonError> {
    super::check_branch(base)?;
    let remote = format!("refs/remotes/origin/{base}");
    let (Some(from), Some(to)) = (commit_id(repo, &heads(base))?, commit_id(repo, &remote)?) else {
        return Ok(Advance::Current);
    };
    if from == to || is_ancestor(repo, &to, &from)? {
        return Ok(match from == to {
            true => Advance::Current,
            false => Kept::advance(
                "ahead",
                format!("the local `{base}` holds commits `origin/{base}` does not"),
            ),
        });
    }
    if !is_ancestor(repo, &from, &to)? {
        return Ok(Kept::advance(
            "diverged",
            format!("the local `{base}` and `origin/{base}` have diverged"),
        ));
    }
    if let Some(tree) = in_progress(repo, base)? {
        return Ok(Kept::advance(
            "busy",
            format!(
                "`{base}` is being rebased or bisected in {}",
                tree.display()
            ),
        ));
    }
    match checked_out(repo, base)? {
        None => move_ref(repo, base, &from, &to),
        Some(tree) => merge_in(&tree, base, &from, &to),
    }
}

/// Move the branch no work tree has checked out; the old value makes git
/// refuse when it moved meanwhile.
fn move_ref(repo: &Path, base: &str, from: &str, to: &str) -> Result<Advance, ArchonError> {
    assert!(super::stack::is_commit_id(from) && super::stack::is_commit_id(to));
    let reference = heads(base);
    let message = format!("archon: fast-forward to origin/{base}");
    let args = ["update-ref", "-m", &message, &reference, to, from];
    if let Err(error) = git(repo, &args) {
        if commit_id(repo, &reference)?.as_deref() == Some(from) {
            return Err(error);
        }
        let why = format!("the local `{base}` moved while it was being fast-forwarded");
        return Ok(Kept::advance("moved", why));
    }
    Ok(Advance::Moved {
        from: from.to_owned(),
        to: to.to_owned(),
    })
}

/// Fast-forward the branch checked out in `tree`, which must hold no change
/// to a tracked file and no untracked or ignored file where the move writes.
fn merge_in(tree: &Path, base: &str, from: &str, to: &str) -> Result<Advance, ArchonError> {
    assert!(super::stack::is_commit_id(from) && super::stack::is_commit_id(to));
    if let Some(kept) = blocker(tree, base, from, to)? {
        return Ok(kept);
    }
    // The user's `branch.<base>.mergeOptions` (e.g. --squash) must not change this.
    let options = format!("branch.{base}.mergeOptions=");
    let args = [
        "-c",
        &options,
        "merge",
        "--quiet",
        "--ff-only",
        "--no-autostash",
        "--no-overwrite-ignore",
        to,
    ];
    if let Err(error) = git(tree, &args) {
        let why = format!(
            "git did not fast-forward `{base}` checked out in {}: {error}",
            tree.display()
        );
        return Ok(Kept::advance("refused", why));
    }
    if !on_branch_at(tree, base, to)? {
        return Err(ArchonError::Invalid(format!(
            "the work tree {} left `{base}` while it was fast-forwarded to {to}; check \
             `git status` there",
            tree.display()
        )));
    }
    Ok(Advance::Moved {
        from: from.to_owned(),
        to: to.to_owned(),
    })
}

/// Why `tree` must not be fast-forwarded from `from` to `to`, if it must not:
/// a local change, a file in the way, or no longer `base` at `from`. Checked
/// last just before the merge; git has no lock that spans the two.
fn blocker(tree: &Path, base: &str, from: &str, to: &str) -> Result<Option<Advance>, ArchonError> {
    assert!(tree.is_absolute(), "git lists work trees by absolute path");
    let place = tree.display();
    if let Some(why) = dirty(tree)? {
        let why = format!("`{base}` is checked out in {place} with local changes ({why})");
        return Ok(Some(Kept::advance("dirty", why)));
    }
    if let Some(path) = in_the_way(tree, from, to)? {
        let why = format!(
            "`{base}` is checked out in {place}, where the untracked or ignored `{path}` \
             would be overwritten"
        );
        return Ok(Some(Kept::advance("dirty", why)));
    }
    if !on_branch_at(tree, base, from)? {
        let why = format!("{place} switched away from `{base}` while it was checked");
        return Ok(Some(Kept::advance("moved", why)));
    }
    Ok(None)
}

/// Whether `tree` has `base` checked out at `commit`.
fn on_branch_at(tree: &Path, base: &str, commit: &str) -> Result<bool, ArchonError> {
    assert!(super::stack::is_commit_id(commit), "a full commit id");
    if !super::head(tree)?.is_branch(base) {
        return Ok(false);
    }
    Ok(commit_id(tree, "HEAD")?.as_deref() == Some(commit))
}

/// The first untracked or ignored path of `tree` that moving from `from` to
/// `to` would write over: one of the paths it changes, or a directory of one.
fn in_the_way(tree: &Path, from: &str, to: &str) -> Result<Option<String>, ArchonError> {
    assert!(super::stack::is_commit_id(from) && super::stack::is_commit_id(to));
    let changed = git(
        tree,
        &["diff", "--name-only", "--no-renames", "-z", from, to],
    )?;
    let others = git(tree, &["ls-files", "--others", "--directory", "-z"])?;
    let others: Vec<&str> = others.split('\0').filter(|p| !p.is_empty()).collect();
    for path in changed.split('\0').filter(|p| !p.is_empty()) {
        let blocked = others.iter().find(|other| {
            let dir = other.strip_suffix('/');
            **other == path
                || dir.is_some_and(|dir| path.starts_with(&format!("{dir}/")))
                || other.starts_with(&format!("{path}/"))
        });
        if let Some(other) = blocked {
            return Ok(Some((*other).to_owned()));
        }
    }
    Ok(None)
}

/// The work tree `base` is checked out in, if any.
pub(crate) fn checked_out(repo: &Path, base: &str) -> Result<Option<PathBuf>, ArchonError> {
    let listing = git(repo, &["worktree", "list", "--porcelain"])?;
    let wanted = heads(base);
    let mut tree: Option<&str> = None;
    for line in listing.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            tree = Some(path);
        } else if line.strip_prefix("branch ") == Some(wanted.as_str()) {
            return Ok(tree.map(PathBuf::from));
        }
    }
    Ok(None)
}

/// The first change a work tree holds that a merge must not run over.
pub(crate) fn dirty(tree: &Path) -> Result<Option<String>, ArchonError> {
    let status = git(tree, &["status", "--porcelain", "--untracked-files=no"])?;
    let count = status.lines().count();
    Ok((count > 0).then(|| format!("{count} changed file{}", if count == 1 { "" } else { "s" })))
}

/// The git directory of a work tree of `repo` that is rebasing or bisecting
/// `base`, where its detached `HEAD` hides the branch from `worktree list`.
fn in_progress(repo: &Path, base: &str) -> Result<Option<PathBuf>, ArchonError> {
    let common = git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = PathBuf::from(common);
    assert!(common.is_absolute(), "asked for an absolute path");
    let mut dirs = vec![common.clone()];
    let linked = common.join("worktrees");
    if linked.is_dir() {
        let entries = std::fs::read_dir(&linked)
            .map_err(|e| ArchonError::io(format!("listing {}", linked.display()), e))?;
        for entry in entries {
            let entry = entry.map_err(|e| ArchonError::io("reading a worktree entry", e))?;
            dirs.push(entry.path());
        }
    }
    let branch = heads(base);
    let marks = [
        ("rebase-merge/head-name", branch.as_str()),
        ("rebase-apply/head-name", branch.as_str()),
        ("BISECT_START", base),
    ];
    for dir in dirs {
        for (file, wanted) in marks {
            if read_mark(&dir.join(file))?.as_deref() == Some(wanted) {
                return Ok(Some(dir));
            }
        }
    }
    Ok(None)
}

/// The trimmed text of a git state file; `None` when it does not exist.
fn read_mark(path: &Path) -> Result<Option<String>, ArchonError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text.trim().to_owned())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ArchonError::io(format!("reading {}", path.display()), e)),
    }
}

#[cfg(test)]
#[path = "fast_forward_tests.rs"]
mod tests;
