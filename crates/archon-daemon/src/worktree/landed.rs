//! A dependency that landed on its base without its commits: squash- or
//! rebase-merged, the base holds its content under commits of its own, which
//! conflict with a dependent branch's copy of them. That branch, holding the
//! dependency's head, is merged over the landing as if it forked there
//! (ARCHITECTURE §8.8). Everything runs through the shadow (§13.4).

use std::path::Path;

use archon_core::error::ArchonError;

use super::daemon_git::Config;
use super::shadow::Shadow;
use super::stack::{self, Identity, Merge, contains, is_commit_id};

/// Where a base branch holds a dependency head's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Landing {
    /// This commit of the base has the head's very tree: a squash or rebase
    /// merge onto a base that had not moved. Merged as `git merge -s ours`.
    SameTree(String),
    /// This commit of the base carries the head's changes, matched by patch
    /// id: a squash or rebase merge after the base moved.
    SamePatches(String),
}

impl Landing {
    /// The base's commit a dependent branch merges.
    pub(crate) fn commit(&self) -> &str {
        match self {
            Self::SameTree(commit) | Self::SamePatches(commit) => commit,
        }
    }
}

/// Where `base` holds the content of the dependency head `dep` without
/// holding `dep` itself; `None` when it holds `dep`, or its content in no
/// way a tree or patch id proves.
pub(crate) fn find(
    shadow: &Shadow,
    repo: &Path,
    dep: &str,
    base: &str,
) -> Result<Option<Landing>, ArchonError> {
    assert!(is_commit_id(dep) && is_commit_id(base), "commit ids");
    if contains(repo, dep, base)? {
        return Ok(None);
    }
    let Some(fork) = fork_point(shadow, dep, base)? else {
        return Ok(None);
    };
    assert_ne!(fork, dep, "the base does not hold the head");
    if let Some(same) = same_tree(shadow, dep, base, &fork)? {
        return Ok(Some(Landing::SameTree(same)));
    }
    // One commit of the base with the head's whole change is a squash.
    let squash = squash_of(shadow, dep, &fork)?;
    let found = match newest_equivalent(shadow, base, &squash)? {
        Some(found) => Some(found),
        None => rebased(shadow, base, dep)?,
    };
    // Patch ids only point at a landing; its content must hold the head's change.
    match found {
        Some(found) if holds_change(shadow, &fork, dep, &found)? => {
            Ok(Some(Landing::SamePatches(found)))
        }
        _ => Ok(None),
    }
}

/// The newest commit of `base` matching one of `dep`'s own whose history
/// matches all the others, as a rebase leaves.
fn rebased(shadow: &Shadow, base: &str, dep: &str) -> Result<Option<String>, ArchonError> {
    let Some(newest) = newest_equivalent(shadow, base, dep)? else {
        return Ok(None);
    };
    Ok(all_equivalent(shadow, &newest, dep)?.then_some(newest))
}

/// Whether `landing` holds every change `dep` makes to `fork`: merging `dep`
/// into it over `fork` changes nothing, so merging over `dep` drops nothing.
fn holds_change(
    shadow: &Shadow,
    fork: &str,
    dep: &str,
    landing: &str,
) -> Result<bool, ArchonError> {
    assert!(is_commit_id(landing), "a commit of the base");
    match stack::merge_tree(shadow, Some(fork), landing, dep)? {
        Ok(tree) => Ok(tree == tree_of(shadow, landing)?),
        Err(_conflicts) => Ok(false),
    }
}

/// Merge `landing` into `branch` of `repo`, which holds the dependency head
/// `dep`, as if the branch forked there: a [`Landing::SameTree`] keeps the
/// branch's tree, a [`Landing::SamePatches`] merges over [`virtual_base`]
/// with the branch's `merged` parent heads instead of the commits' merge
/// base. Nothing is written on a conflict.
pub(crate) fn merge(
    shadow: &Shadow,
    repo: &Path,
    branch: &str,
    (dep, merged, landing): (&str, &[String], &Landing),
    identity: &Identity,
) -> Result<Merge, ArchonError> {
    assert!(is_commit_id(dep), "a dependency head");
    let ours = stack::sha(repo, branch)?;
    let theirs = landing.commit();
    if contains(repo, theirs, &ours)? {
        return Ok(Merge::Contained);
    }
    if !contains(repo, dep, &ours)? {
        return Err(ArchonError::Invalid(format!(
            "`{branch}` does not hold {dep}, so it cannot be merged over that commit's landing"
        )));
    }
    let (tree, why) = match landing {
        Landing::SameTree(_) => {
            if tree_of(shadow, theirs)? != tree_of(shadow, dep)? {
                return Err(ArchonError::Protocol(format!(
                    "{theirs} no longer has the tree of {dep}"
                )));
            }
            let why = "has the tree of";
            (tree_of(shadow, &ours)?, why)
        }
        Landing::SamePatches(_) => {
            let base = match virtual_base(shadow, repo, (dep, merged), (&ours, theirs))? {
                Ok(base) => base,
                Err(files) => return Ok(Merge::Conflict(files)),
            };
            match stack::merge_tree(shadow, Some(&base), &ours, theirs)? {
                Ok(tree) => (tree, "carries the changes of"),
                Err(files) => return Ok(Merge::Conflict(files)),
            }
        }
    };
    let message = format!(
        "Merge commit '{theirs}' into {branch}\n\n{theirs} {why} {dep}, which this branch \
         holds: the dependency landed as a squash or rebase, so it is merged over {dep}."
    );
    let to = stack::commit_merge(shadow, &tree, (&ours, theirs), &message, identity)?;
    Ok(Merge::Merged { from: ours, to })
}

/// The merge base `ours` and the landing `theirs` would have without squash or
/// rebase: `dep` merged with each of their best common commits and with each
/// other `merged` parent head `ours` holds whose whole change `theirs` holds,
/// so nothing both hold counts as new on either side; or the conflicting files.
fn virtual_base(
    shadow: &Shadow,
    repo: &Path,
    (dep, merged): (&str, &[String]),
    (ours, theirs): (&str, &str),
) -> Result<Result<String, String>, ArchonError> {
    assert!(is_commit_id(ours) && is_commit_id(theirs), "commit ids");
    let mut parts = common_bases(shadow, ours, theirs)?;
    for head in merged.iter().filter(|head| head.as_str() != dep) {
        if contains(repo, head, ours)? && landed_in(shadow, repo, head, theirs)? {
            parts.push(head.clone());
        }
    }
    let mut base = dep.to_owned();
    for part in parts {
        if contains(repo, &part, &base)? {
            continue;
        }
        base = match contains(repo, &base, &part)? {
            true => part,
            false => match stack::merge_tree(shadow, None, &base, &part)? {
                Ok(tree) => scratch_commit(shadow, &tree, &[&base, &part], "merge base")?,
                Err(files) => return Ok(Err(files)),
            },
        };
    }
    assert!(
        contains(repo, dep, &base)?,
        "the base holds the dependency head"
    );
    Ok(Ok(base))
}

/// Whether `base` holds the parent head `head` or its whole change.
fn landed_in(shadow: &Shadow, repo: &Path, head: &str, base: &str) -> Result<bool, ArchonError> {
    if contains(repo, head, base)? {
        return Ok(true);
    }
    match fork_point(shadow, head, base)? {
        Some(fork) => holds_change(shadow, &fork, head, base),
        None => Ok(false),
    }
}

/// Every best common commit of `ours` and `theirs`: several after criss-cross
/// merges, none when they share no history.
fn common_bases(shadow: &Shadow, ours: &str, theirs: &str) -> Result<Vec<String>, ArchonError> {
    let ran = git(shadow)?
        .args(["merge-base", "--all", ours, theirs])
        .output()?;
    // `merge-base` exits 1 exactly when the commits share no history.
    match ran.status.code() {
        Some(0) => {
            let bases: Vec<String> = ran.stdout_text()?.lines().map(str::to_owned).collect();
            assert!(!bases.is_empty(), "merge-base printed a commit");
            assert!(bases.iter().all(|b| is_commit_id(b)), "merge-base ids");
            Ok(bases)
        }
        Some(1) => Ok(Vec::new()),
        _ => Err(ran.failure()),
    }
}

/// The newest commit both `dep` and `base` hold.
fn fork_point(shadow: &Shadow, dep: &str, base: &str) -> Result<Option<String>, ArchonError> {
    let ran = git(shadow)?.args(["merge-base", dep, base]).output()?;
    // `merge-base` exits 1 exactly when the commits share no history.
    match ran.status.code() {
        Some(0) => {
            let fork = ran.stdout_text()?;
            assert!(is_commit_id(&fork), "merge-base prints a commit id");
            Ok(Some(fork))
        }
        Some(1) => Ok(None),
        _ => Err(ran.failure()),
    }
}

/// The newest commit on `base`'s first-parent line since `fork` whose tree is `dep`'s.
fn same_tree(
    shadow: &Shadow,
    dep: &str,
    base: &str,
    fork: &str,
) -> Result<Option<String>, ArchonError> {
    let wanted = tree_of(shadow, dep)?;
    let since = format!("^{fork}");
    let args = [
        "rev-list",
        "--first-parent",
        "--no-commit-header",
        "--format=%H %T",
        base,
        &since,
    ];
    let listed = git(shadow)?.args(args).text()?;
    let found = listed.lines().find_map(|line| {
        let (commit, tree) = line.split_once(' ')?;
        (tree == wanted).then(|| commit.to_owned())
    });
    assert!(found.as_deref().is_none_or(is_commit_id), "rev-list ids");
    Ok(found)
}

/// A commit no ref names with `dep`'s tree and the one parent `fork`: the
/// change `dep` makes, as a squash merge would put it.
fn squash_of(shadow: &Shadow, dep: &str, fork: &str) -> Result<String, ArchonError> {
    scratch_commit(shadow, &tree_of(shadow, dep)?, &[fork], "squash")
}

/// A commit of `tree` over `parents` that no ref names, the same on every
/// call: a squash or merge base of Archon's own, never pushed.
fn scratch_commit(
    shadow: &Shadow,
    tree: &str,
    parents: &[&str],
    message: &str,
) -> Result<String, ArchonError> {
    assert!(is_commit_id(tree), "a written tree");
    assert!(
        !parents.is_empty() && parents.iter().all(|p| is_commit_id(p)),
        "parent commits"
    );
    let mut args = vec!["commit-tree", "--no-gpg-sign", tree];
    for parent in parents {
        args.extend(["-p", parent]);
    }
    args.extend(["-m", message]);
    let commit = git(shadow)?
        .env("GIT_AUTHOR_NAME", "Archon")
        .env("GIT_AUTHOR_EMAIL", "archon@localhost")
        .env("GIT_AUTHOR_DATE", "@0 +0000")
        .env("GIT_COMMITTER_NAME", "Archon")
        .env("GIT_COMMITTER_EMAIL", "archon@localhost")
        .env("GIT_COMMITTER_DATE", "@0 +0000")
        .args(args)
        .text()?;
    assert!(is_commit_id(&commit), "commit-tree prints the commit");
    Ok(commit)
}

/// The newest commit of `base`, not `side`'s, whose patch id one of `side`'s
/// own commits has.
fn newest_equivalent(
    shadow: &Shadow,
    base: &str,
    side: &str,
) -> Result<Option<String>, ArchonError> {
    let marked = cherry_marked(shadow, "--left-only", base, side)?;
    let found = marked
        .into_iter()
        .find_map(|(equivalent, commit)| equivalent.then_some(commit));
    Ok(found)
}

/// Whether every commit `dep` adds to its fork from `base`, merges aside,
/// has a commit `base` holds with its patch id.
fn all_equivalent(shadow: &Shadow, base: &str, dep: &str) -> Result<bool, ArchonError> {
    let marked = cherry_marked(shadow, "--right-only", base, dep)?;
    Ok(!marked.is_empty() && marked.iter().all(|(equivalent, _)| *equivalent))
}

/// `side`'s commits of `left...right`, newest first, merges aside, each with
/// whether the other side has one of the same patch id.
fn cherry_marked(
    shadow: &Shadow,
    side: &str,
    left: &str,
    right: &str,
) -> Result<Vec<(bool, String)>, ArchonError> {
    assert!(matches!(side, "--left-only" | "--right-only"), "one side");
    let range = format!("{left}...{right}");
    let args = ["rev-list", "--cherry-mark", "--no-merges", side, &range];
    let listed = git(shadow)?.args(args).text()?;
    listed
        .lines()
        .map(|line| match line.split_at_checked(1) {
            Some(("=", commit)) if is_commit_id(commit) => Ok((true, commit.to_owned())),
            Some(("+", commit)) if is_commit_id(commit) => Ok((false, commit.to_owned())),
            _ => Err(ArchonError::Protocol(format!(
                "git rev-list --cherry-mark printed {line:?}"
            ))),
        })
        .collect()
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
fn git(shadow: &Shadow) -> Result<super::daemon_git::DaemonGit, ArchonError> {
    shadow.git(None, Config::Sealed)
}

#[cfg(test)]
#[path = "landed_tests.rs"]
mod tests;
