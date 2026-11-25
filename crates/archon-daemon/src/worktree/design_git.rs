//! What archond asks of a design session's worktree: the shape of a note's
//! commit, whether the tree is tidy, and its own revert (design §8.3, §9).
//!
//! Everything runs on the ARCHITECTURE §13.4 helpers and nothing else: read
//! only commands through sealed git, commands that read file contents through
//! the shadow, the revert's merge and commit through `stack::merge_tree` and
//! `commit-tree`, and the move of branch and files through `stack::advance`.
//! No porcelain git runs in the worktree, and no hook, filter, merge driver or
//! fsmonitor the repository names is ever run.
//!
//! ## Reconciling a revert after a crash
//!
//! `stack::advance` checks the files out (`read-tree -m -u pre new`) and then
//! moves the branch with a compare-and-set. A crash between the two leaves
//! `HEAD == pre` with the index and files exactly `new`'s tree. For a revert
//! note stored as working with commit `new`, [`reconcile_revert`] settles it:
//!
//! | `HEAD` | index and files | result |
//! |---|---|---|
//! | `new`, parents `[pre]` | tidy | `Done` |
//! | `pre` | exactly `new`'s tree, `new`'s parent is `pre` | the branch is moved `pre -> new` by compare-and-set through sealed git: `Completed` |
//! | `pre` | clean against `pre` | `NotApplied`, nothing written |
//! | anything else | | `HistoryChanged`, left as is |

use std::path::Path;

use archon_core::error::ArchonError;

use super::clean;
use super::daemon_git::{Config, DaemonGit};
use super::shadow::Shadow;
use super::stack::{self, Identity, is_commit_id};

#[cfg(test)]
mod tests;

/// The repository and worktree of a design session's item.
pub(crate) struct NoteGit<'a> {
    pub shadow: &'a Shadow,
    pub repo: &'a Path,
    pub worktree: &'a Path,
    pub branch: &'a str,
}

fn validate_commit(id: &str) -> Result<(), ArchonError> {
    if !is_commit_id(id) {
        return Err(ArchonError::Invalid(
            "expected a full design commit ID".into(),
        ));
    }
    Ok(())
}

/// The commit the worktree's `HEAD` names.
pub(crate) fn head(g: &NoteGit) -> Result<String, ArchonError> {
    let sha = DaemonGit::sealed_in(g.worktree)
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .text()?;
    if !is_commit_id(&sha) {
        return Err(ArchonError::Invalid(
            "git returned an invalid commit id".into(),
        ));
    }
    Ok(sha)
}

/// The one-line message of `commit` and the files it changed, for the review
/// page. Reads only; a commit id that is not in full is refused.
pub(crate) fn commit_facts(
    g: &NoteGit,
    commit: &str,
) -> Result<(String, Vec<String>), ArchonError> {
    if !is_commit_id(commit) {
        return Err(ArchonError::Invalid(format!(
            "{commit:?} is not a commit id"
        )));
    }
    let subject = DaemonGit::sealed_in(g.repo)
        .args(["show", "-s", "--format=%s", commit])
        .text()?;
    let files = DaemonGit::sealed_in(g.repo)
        .args([
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "--root",
            commit,
        ])
        .text()?;
    let files: Vec<String> = files.lines().map(str::to_owned).collect();
    assert!(
        files.iter().all(|f| !f.is_empty()),
        "diff-tree prints no empty path"
    );
    Ok((subject, files))
}

/// Whether `of` holds the commit `commit`.
pub(crate) fn contains(g: &NoteGit, commit: &str, of: &str) -> Result<bool, ArchonError> {
    stack::contains(g.repo, commit, of)
}

/// Whether a note's commit has the shape decision A requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Shape {
    Accepted,
    /// The plain reason the operator is shown.
    HistoryChanged(String),
}

/// `commit` and its parents.
fn parents_of(g: &NoteGit, commit: &str) -> Result<Vec<String>, ArchonError> {
    let line = DaemonGit::sealed_in(g.repo)
        .args(["rev-list", "--parents", "-n", "1", commit])
        .text()?;
    let mut ids: Vec<String> = line.split(' ').map(str::to_owned).collect();
    if ids.first().map(String::as_str) != Some(commit) || !ids.iter().all(|id| is_commit_id(id)) {
        return Err(ArchonError::Protocol(format!(
            "git rev-list printed {line:?} for {commit}"
        )));
    }
    ids.remove(0);
    Ok(ids)
}

/// `Some(c)` is accepted only when HEAD is `c` with exactly one parent, `base`;
/// `None` only when HEAD is `base`. Writes nothing.
pub(crate) fn check_shape(
    g: &NoteGit,
    base: &str,
    done: Option<&str>,
) -> Result<Shape, ArchonError> {
    validate_commit(base)?;
    let now = head(g)?;
    let changed = |why: &str| Ok(Shape::HistoryChanged(why.to_owned()));
    let Some(done) = done else {
        return if now == base {
            Ok(Shape::Accepted)
        } else {
            changed("the agent reported no change, but the code history moved")
        };
    };
    if now != done {
        return changed("the code history is not at the commit the agent reported");
    }
    let parents = parents_of(g, &now)?;
    match parents.as_slice() {
        [parent] if parent == base => Ok(Shape::Accepted),
        [_] => changed("the commit does not sit directly on where the note started"),
        [] => changed("the commit has no parent"),
        _ => changed("the commit is a merge"),
    }
}

/// Whether the worktree is ready for the next step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Tidy {
    Tidy,
    /// What is wrong and how the agent fixes it.
    Untidy(String),
}

/// The first tracked path whose index or file differs from `head`.
fn first_changed(g: &NoteGit, index: &str, head: &str) -> Result<Option<String>, ArchonError> {
    for args in [
        vec![
            "diff-files",
            "--name-only",
            "-z",
            "--ignore-submodules=dirty",
        ],
        vec!["diff-index", "--cached", "--name-only", "-z", head],
    ] {
        let ran = g
            .shadow
            .git(Some(g.worktree), Config::Operator)?
            .env("GIT_INDEX_FILE", index)
            .args(args)
            .output()?;
        if !ran.status.success() {
            return Err(ran.failure());
        }
        let text = ran.stdout_text_untrimmed()?;
        if let Some(path) = text.split('\0').find(|p| !p.is_empty()) {
            return Ok(Some(path.to_owned()));
        }
    }
    Ok(None)
}

/// The first untracked, not ignored file outside `.archon/`.
fn first_untracked(g: &NoteGit) -> Result<Option<String>, ArchonError> {
    let ran = DaemonGit::sealed_in(g.worktree)
        .args(["ls-files", "-z", "--others", "--exclude-standard"])
        .output()?;
    if !ran.status.success() {
        return Err(ran.failure());
    }
    let text = ran.stdout_text_untrimmed()?;
    Ok(text
        .split('\0')
        .find(|p| !p.is_empty() && !p.starts_with(".archon/"))
        .map(str::to_owned))
}

/// The tracked tree is `HEAD`'s, no untracked file lies outside `.archon/`,
/// and for `Some(id)` HEAD's message has the line `Archon-Note: <id>`.
pub(crate) fn check_tidy(g: &NoteGit, trailer: Option<&str>) -> Result<Tidy, ArchonError> {
    let now = head(g)?;
    let index = clean::index_of(g.worktree)?;
    if !clean::is_clean(g.shadow, g.worktree, &index, &now)? {
        let what = match first_changed(g, index_str(&index)?, &now)? {
            Some(path) => format!("uncommitted changes in {path}"),
            None => "uncommitted changes".to_owned(),
        };
        return Ok(Tidy::Untidy(format!(
            "{what}: commit them into the note's one commit with `git commit --amend`"
        )));
    }
    if let Some(path) = first_untracked(g)? {
        return Ok(Tidy::Untidy(format!(
            "untracked file {path}: add it to the commit with `git add` and `git commit --amend`, or delete it"
        )));
    }
    if let Some(id) = trailer {
        let message = DaemonGit::sealed_in(g.worktree)
            .args(["show", "-s", "--format=%(trailers:key=Archon-Note)", "HEAD"])
            .output()?;
        if !message.status.success() {
            return Err(message.failure());
        }
        let wanted = format!("Archon-Note: {id}");
        let has = message
            .stdout_text_untrimmed()?
            .lines()
            .any(|line| line.trim_end() == wanted);
        if !has {
            return Ok(Tidy::Untidy(format!(
                "add the trailer with `git commit --amend` (keeps the parent): a line `{wanted}` at the end of the message"
            )));
        }
    }
    Ok(Tidy::Tidy)
}

fn index_str(index: &Path) -> Result<&str, ArchonError> {
    index
        .to_str()
        .ok_or_else(|| ArchonError::Invalid("a non-UTF-8 index path".into()))
}

/// What preparing a revert came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RevertPrep {
    /// `new` reverts the original on top of `pre`; no ref names it yet.
    Ready { pre: String, new: String },
    /// Nothing was done; the plain reason.
    Refused(String),
    /// The files that conflict; nothing was written to the worktree.
    Conflict(Vec<String>),
}

/// The revert of `original` on HEAD, as `git revert --no-edit` would make it,
/// made by `identity` and trailed `Archon-Note: <revert_note>`. Writes only
/// objects. A refusal names the commit, as no note id is known here.
pub(crate) fn prepare_revert(
    g: &NoteGit,
    original: &str,
    revert_note: &str,
    identity: &Identity,
) -> Result<RevertPrep, ArchonError> {
    validate_commit(original)?;
    assert!(!identity.email.is_empty(), "a revert has an author");
    let pre = head(g)?;
    if check_tidy(g, None)? != Tidy::Tidy {
        return Ok(RevertPrep::Refused(
            "the worktree has uncommitted changes".into(),
        ));
    }
    if !contains(g, original, &pre)? {
        return Ok(RevertPrep::Refused(format!(
            "{original}'s change is no longer in the code history"
        )));
    }
    let parent = match parents_of(g, original)?.as_slice() {
        [parent] => parent.clone(),
        _ => {
            return Ok(RevertPrep::Refused(format!(
                "{original} is not a single-parent commit"
            )));
        }
    };
    let tree = match stack::merge_tree_paths(g.shadow, Some(original), &pre, &parent)? {
        Ok(tree) => tree,
        Err(files) => return Ok(RevertPrep::Conflict(files)),
    };
    let subject = DaemonGit::sealed_in(g.repo)
        .args(["show", "-s", "--format=%s", original])
        .text()?;
    let message = format!(
        "Revert \"{subject}\"\n\nThis reverts commit {original}.\n\nArchon-Note: {revert_note}"
    );
    let new = g
        .shadow
        .git(None, Config::Sealed)?
        .env("GIT_AUTHOR_NAME", &identity.name)
        .env("GIT_AUTHOR_EMAIL", &identity.email)
        .env("GIT_COMMITTER_NAME", &identity.name)
        .env("GIT_COMMITTER_EMAIL", &identity.email)
        .args([
            "commit-tree",
            "--no-gpg-sign",
            &tree,
            "-p",
            &pre,
            "-m",
            &message,
        ])
        .text()?;
    validate_commit(&new)?;
    Ok(RevertPrep::Ready { pre, new })
}

/// What applying a revert came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RevertApplied {
    Applied,
    HistoryChanged(String),
}

/// Move the branch and the worktree from `pre` to `new`, then check the result
/// has the shape of a revert note's commit on `pre` and is tidy.
pub(crate) fn apply_revert(
    g: &NoteGit,
    pre: &str,
    new: &str,
) -> Result<RevertApplied, ArchonError> {
    validate_commit(pre)?;
    validate_commit(new)?;
    let moved = || RevertApplied::HistoryChanged("the code history moved during the revert".into());
    if stack::sha(g.repo, g.branch)? != pre {
        return Ok(moved());
    }
    if let Err(error) = stack::advance(g.shadow, g.repo, g.worktree, g.branch, pre, new) {
        // `advance` put the files back when the branch moved meanwhile.
        return if stack::sha(g.repo, g.branch)? == pre {
            Err(error)
        } else {
            Ok(moved())
        };
    }
    after(g, pre, new)
}

/// The result `HEAD == new`, parents `[pre]`, tidy.
fn after(g: &NoteGit, pre: &str, new: &str) -> Result<RevertApplied, ArchonError> {
    if let Shape::HistoryChanged(why) = check_shape(g, pre, Some(new))? {
        return Ok(RevertApplied::HistoryChanged(why));
    }
    match check_tidy(g, None)? {
        Tidy::Tidy => Ok(RevertApplied::Applied),
        Tidy::Untidy(why) => Ok(RevertApplied::HistoryChanged(why)),
    }
}

/// How a revert stored as working settled at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reconciled {
    /// It had been applied whole.
    Done,
    /// Only the branch move was missing; it was made.
    Completed,
    /// Nothing of it was applied; nothing was written.
    NotApplied,
    HistoryChanged(String),
}

/// Settle a revert note stored as working with commit `new` on `pre`; see the
/// module documentation for the rule.
pub(crate) fn reconcile_revert(
    g: &NoteGit,
    pre: &str,
    new: &str,
) -> Result<Reconciled, ArchonError> {
    validate_commit(pre)?;
    validate_commit(new)?;
    let now = head(g)?;
    let changed = |why: &str| Ok(Reconciled::HistoryChanged(why.to_owned()));
    if now == new {
        return Ok(match after(g, pre, new)? {
            RevertApplied::Applied => Reconciled::Done,
            RevertApplied::HistoryChanged(why) => Reconciled::HistoryChanged(why),
        });
    }
    if now != pre {
        return changed("the code history is neither before nor after the revert");
    }
    let index = clean::index_of(g.worktree)?;
    if clean::is_clean(g.shadow, g.worktree, &index, new)? {
        if parents_of(g, new)? != [pre.to_owned()] {
            return changed("the revert commit does not sit on the code history");
        }
        let reference = format!("refs/heads/{}", g.branch);
        let ran = DaemonGit::sealed_in(g.repo)
            .args(["update-ref", "-m", "archon: revert", &reference, new, pre])
            .output()?;
        if !ran.status.success() {
            return if stack::sha(g.repo, g.branch)? == pre {
                Err(ran.failure())
            } else {
                changed("the code history moved during the revert")
            };
        }
        return Ok(Reconciled::Completed);
    }
    if clean::is_clean(g.shadow, g.worktree, &index, pre)? {
        return Ok(Reconciled::NotApplied);
    }
    changed("the worktree is neither before nor after the revert")
}
