//! Whether the review of a pull request's commit still covers its later head
//! (ARCHITECTURE §18.6): every commit since is a merge, on the branch's first
//! parents, of a commit its base holds, whose content is either git's own
//! clean merge of it or, for a squash or rebase landing (§8.8), the branch's
//! own tree because the branch already held the merged commit's content. A
//! resolved conflict, an added change or any other commit is not covered.

use std::path::Path;

use archon_core::error::ArchonError;
use archon_core::merge::short;

use super::daemon_git::{Config, DaemonGit};
use super::shadow::Shadow;
use super::stack::{self, contains, is_commit_id};

/// More merges than this after the reviewed commit are not read.
const MAX_MERGES: usize = 32;

/// Why the review of `reviewed` covers `head`, a later head of its branch,
/// whose pull request targets the commit `base`: the merges since, in words;
/// `Err` why it does not. All three commits must be in `repo`.
pub(crate) fn base_merges(
    shadow: &Shadow,
    repo: &Path,
    (reviewed, head): (&str, &str),
    base: &str,
) -> Result<Result<String, String>, ArchonError> {
    assert!(
        [reviewed, head, base].iter().all(|c| is_commit_id(c)),
        "commit ids in full"
    );
    assert_ne!(reviewed, head, "only a moved head is carried over");
    if !contains(repo, reviewed, head)? {
        return Ok(Err(format!(
            "{} does not hold {}: the branch was rewritten",
            short(head),
            short(reviewed)
        )));
    }
    let mut merges = Vec::new();
    let mut at = head.to_owned();
    while at != reviewed {
        if merges.len() == MAX_MERGES {
            return Ok(Err(format!("more than {MAX_MERGES} commits came after it")));
        }
        let Some((ours, theirs)) = parents(shadow, &at)? else {
            return Ok(Err(format!("{} is not a merge of two commits", short(&at))));
        };
        if !contains(repo, reviewed, &ours)? {
            return Ok(Err(format!(
                "{} holds {} only through the commit it merges",
                short(&at),
                short(reviewed)
            )));
        }
        match explain(shadow, repo, (&at, &ours, &theirs), base)? {
            Ok(how) => merges.push(how),
            Err(why) => return Ok(Err(why)),
        }
        at = ours;
    }
    assert!(
        !merges.is_empty(),
        "a moved head has a commit after the review"
    );
    merges.reverse();
    Ok(Ok(format!(
        "only base merges came after it: {}",
        merges.join("; ")
    )))
}

/// How the base explains the commit `merge` of `theirs` into `ours`, in
/// words; `Err` why it does not.
fn explain(
    shadow: &Shadow,
    repo: &Path,
    (merge, ours, theirs): (&str, &str, &str),
    base: &str,
) -> Result<Result<String, String>, ArchonError> {
    assert!(is_commit_id(ours) && is_commit_id(theirs), "merge parents");
    if !contains(repo, theirs, base)? {
        return Ok(Err(format!(
            "{} merges {}, which the base does not hold",
            short(merge),
            short(theirs)
        )));
    }
    let tree = tree_of(shadow, merge)?;
    // First, as it needs no merge base: a recreated base may share no history.
    if tree == tree_of(shadow, ours)? && holds_tree_of(shadow, ours, theirs)? {
        return Ok(Ok(format!(
            "{} merges {}, whose content the branch already held, and changes no file",
            short(merge),
            short(theirs)
        )));
    }
    if stack::merge_tree(shadow, None, ours, theirs)? == Ok(tree) {
        return Ok(Ok(format!(
            "{} merges {} as git does, with no conflict",
            short(merge),
            short(theirs)
        )));
    }
    Ok(Err(format!(
        "{}'s content is not git's own merge of {}: it resolves a conflict or adds a change",
        short(merge),
        short(theirs)
    )))
}

/// The two parents of `commit`; `None` for a commit with any other number.
fn parents(shadow: &Shadow, commit: &str) -> Result<Option<(String, String)>, ArchonError> {
    assert!(is_commit_id(commit), "a commit id");
    let listed = git(shadow)?
        .args(["rev-list", "--parents", "--max-count=1", commit])
        .text()?;
    let ids: Vec<&str> = listed.split_whitespace().collect();
    if ids.first() != Some(&commit) || !ids.iter().all(|id| is_commit_id(id)) {
        return Err(ArchonError::Protocol(format!(
            "git rev-list --parents printed {listed:?} for {commit}"
        )));
    }
    Ok(match ids[1..] {
        [ours, theirs] => Some((ours.to_owned(), theirs.to_owned())),
        _ => None,
    })
}

/// Whether a commit of `ours` that `theirs` does not hold has `theirs`'s tree:
/// the branch held that content before it merged `theirs`.
fn holds_tree_of(shadow: &Shadow, ours: &str, theirs: &str) -> Result<bool, ArchonError> {
    let wanted = tree_of(shadow, theirs)?;
    let not_theirs = format!("^{theirs}");
    let trees = git(shadow)?
        .args(["log", "--format=%T", ours, &not_theirs])
        .text()?;
    assert!(
        trees.lines().all(is_commit_id),
        "git log prints one tree id a commit"
    );
    Ok(trees.lines().any(|tree| tree == wanted))
}

fn tree_of(shadow: &Shadow, commit: &str) -> Result<String, ArchonError> {
    assert!(is_commit_id(commit), "a commit id");
    let spec = format!("{commit}^{{tree}}");
    let tree = git(shadow)?.args(["rev-parse", "--verify", &spec]).text()?;
    if !is_commit_id(&tree) {
        return Err(ArchonError::Protocol(format!(
            "git rev-parse printed {tree:?} for the tree of {commit}"
        )));
    }
    Ok(tree)
}

/// Git on the repository's objects through the shadow, which has no refs:
/// every revision named is a commit id.
fn git(shadow: &Shadow) -> Result<DaemonGit, ArchonError> {
    shadow.git(None, Config::Sealed)
}

#[cfg(test)]
#[path = "carried_tests.rs"]
mod tests;
