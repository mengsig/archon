//! Whether the review of a pull request's commit still covers its later head
//! (ARCHITECTURE §18.6): every commit since is a merge, on the branch's first
//! parents, of a commit its base holds, whose content is either git's own
//! clean merge of it or, for a stacked parent's squash landing (§8.8), the
//! branch's own tree over a commit of exactly the parent head's content the
//! review read on top of. A resolved conflict, an added change, a tree that
//! merely matches or any other commit is not covered.

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
/// `Err` why it does not. `stacked_on` are the recorded heads of the item's
/// stack parents. All commits must be in `repo`.
pub(crate) fn base_merges(
    shadow: &Shadow,
    repo: &Path,
    (reviewed, head): (&str, &str),
    base: &str,
    stacked_on: &[String],
) -> Result<Result<String, String>, ArchonError> {
    assert!(
        [reviewed, head, base].iter().all(|c| is_commit_id(c)),
        "commit ids in full"
    );
    assert!(
        stacked_on.iter().all(|c| is_commit_id(c)),
        "stack parent heads in full"
    );
    let review = Review {
        reviewed,
        base,
        stacked_on,
    };
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
        match explain(shadow, repo, (&at, &ours, &theirs), &review)? {
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

/// The review a later head is checked against.
struct Review<'a> {
    reviewed: &'a str,
    /// The commit the pull request targets.
    base: &'a str,
    /// The recorded heads of the item's stack parents.
    stacked_on: &'a [String],
}

/// How the base explains the commit `merge` of `theirs` into `ours`, in
/// words; `Err` why it does not.
fn explain(
    shadow: &Shadow,
    repo: &Path,
    (merge, ours, theirs): (&str, &str, &str),
    review: &Review,
) -> Result<Result<String, String>, ArchonError> {
    assert!(is_commit_id(ours) && is_commit_id(theirs), "merge parents");
    if !contains(repo, theirs, review.base)? {
        return Ok(Err(format!(
            "{} merges {}, which the base does not hold",
            short(merge),
            short(theirs)
        )));
    }
    let tree = tree_of(shadow, merge)?;
    if stack::merge_tree(shadow, None, ours, theirs)? == Ok(tree.clone()) {
        return Ok(Ok(format!(
            "{} merges {} as git does, with no conflict",
            short(merge),
            short(theirs)
        )));
    }
    // Landing it adds what the review read on top of that parent head, alone.
    if tree == tree_of(shadow, ours)? && lands_parent(shadow, repo, theirs, review)? {
        return Ok(Ok(format!(
            "{} merges {}, the landing of a stack parent the review read on top of, \
             and changes no file",
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

/// Whether `theirs` has the exact content of a stack parent head the
/// reviewed commit holds: a squash or rebase landing of that parent.
fn lands_parent(
    shadow: &Shadow,
    repo: &Path,
    theirs: &str,
    review: &Review,
) -> Result<bool, ArchonError> {
    let wanted = tree_of(shadow, theirs)?;
    for parent in review.stacked_on {
        assert!(is_commit_id(parent), "a stack parent head");
        // The reviewed commit holds only commits that are here.
        if parent == review.reviewed || !has_commit(repo, parent)? {
            continue;
        }
        if tree_of(shadow, parent)? == wanted && contains(repo, parent, review.reviewed)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn has_commit(repo: &Path, commit: &str) -> Result<bool, ArchonError> {
    assert!(is_commit_id(commit), "a commit id, no option");
    let spec = format!("{commit}^{{commit}}");
    let ran = DaemonGit::sealed_in(repo)
        .args(["cat-file", "-e", &spec])
        .output()?;
    Ok(ran.status.success())
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
