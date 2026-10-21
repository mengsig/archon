//! Landing a `ship = local` item's branch in its base branch: a fast-forward
//! and nothing else. Never forced; a base that moved, history that does not
//! descend from the base, or a checked-out base with local changes is
//! refused with the reason and the command that fixes it.

use std::path::{Path, PathBuf};

use archon_core::error::ArchonError;
use archon_core::id::QueueItemId;
use archon_core::merge::LocalReview;

use crate::worktree::{self, git};

/// What landing the branch does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Plan {
    /// The base already holds the reviewed commit.
    AlreadyIn,
    /// Move the base to the reviewed commit; `tree` is where the base is
    /// checked out, if anywhere, and was clean when checked.
    FastForward {
        base_tip: String,
        tree: Option<PathBuf>,
    },
}

/// The branch, base and commit a landing is for.
#[derive(Debug, Clone, Copy)]
pub(super) struct Landing<'a> {
    pub item: &'a QueueItemId,
    pub repo: &'a Path,
    pub branch: &'a str,
    pub base: &'a str,
    pub head: &'a str,
}

fn heads(name: &str) -> String {
    format!("refs/heads/{name}")
}

/// What landing would do; `Err` is the refusal, in words with the next step.
pub(super) fn check(landing: Landing<'_>) -> Result<Result<Plan, String>, ArchonError> {
    let Landing {
        item,
        repo,
        branch,
        base,
        head,
    } = landing;
    assert!(!base.is_empty() && !branch.is_empty(), "named branches");
    assert!(crate::worktree::stack::is_commit_id(head), "a full commit");
    let Some(tip) = worktree::commit_id(repo, &heads(branch))? else {
        return Ok(Err(format!(
            "branch `{branch}` no longer exists in {}; nothing to merge",
            repo.display()
        )));
    };
    if tip != head {
        return Ok(Err(format!(
            "branch `{branch}` moved to {} since it was reviewed at {}; retry the item so the \
             new commits are reviewed",
            archon_core::merge::short(&tip),
            archon_core::merge::short(head)
        )));
    }
    let Some(base_tip) = worktree::commit_id(repo, &heads(base))? else {
        return Ok(Err(format!(
            "base branch `{base}` does not exist in {}; create it or set the repository's base \
             with `archon repo set <repo> --base <branch>`",
            repo.display()
        )));
    };
    if worktree::is_ancestor(repo, head, &base_tip)? {
        return Ok(Ok(Plan::AlreadyIn));
    }
    if !worktree::is_ancestor(repo, &base_tip, head)? {
        return Ok(Err(not_descended(landing, &base_tip)?));
    }
    let tree = match checked_out(repo, base)? {
        Some(tree) => tree,
        None => {
            return Ok(Ok(Plan::FastForward {
                base_tip,
                tree: None,
            }));
        }
    };
    if let Some(why) = dirty(&tree)? {
        return Ok(Err(format!(
            "`{base}` is checked out in {} with local changes ({why}); commit or stash them, \
             then run `archon merge {item}` again. Nothing was touched",
            tree.display()
        )));
    }
    Ok(Ok(Plan::FastForward {
        base_tip,
        tree: Some(tree),
    }))
}

/// Why the branch cannot fast-forward into the base.
fn not_descended(landing: Landing<'_>, base_tip: &str) -> Result<String, ArchonError> {
    let Landing {
        item,
        repo,
        branch,
        base,
        head,
    } = landing;
    let shared = worktree::merge_base(repo, head, &[base_tip.to_owned()])?;
    Ok(match shared {
        None => format!(
            "branch `{branch}` shares no history with `{base}`, so it cannot be fast-forwarded; \
             it was not merged"
        ),
        Some(_) => format!(
            "`{base}` moved on since `{branch}` was made, so it is not a fast-forward; run \
             `archon queue refresh {item}` to bring the branch up to date, then merge again"
        ),
    })
}

/// The work tree `base` is checked out in, if any.
fn checked_out(repo: &Path, base: &str) -> Result<Option<PathBuf>, ArchonError> {
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
fn dirty(tree: &Path) -> Result<Option<String>, ArchonError> {
    let status = git(tree, &["status", "--porcelain", "--untracked-files=no"])?;
    let count = status.lines().count();
    Ok((count > 0).then(|| format!("{count} changed file{}", if count == 1 { "" } else { "s" })))
}

/// Do what `plan` says, after checking again that the checkout and the base
/// are as `check` found them. `Err` is a refusal found at the last moment.
pub(super) fn land(landing: Landing<'_>, plan: &Plan) -> Result<Result<(), String>, ArchonError> {
    let Plan::FastForward { base_tip, tree } = plan else {
        return Ok(Ok(()));
    };
    let Landing {
        repo, base, head, ..
    } = landing;
    assert!(
        crate::worktree::stack::is_commit_id(base_tip),
        "a full commit"
    );
    if let Some(why) = changed_since_check(landing, base_tip, tree.as_deref())? {
        return Ok(Err(why));
    }
    let landed = match tree {
        // The old value makes git refuse when the base moved meanwhile.
        None => {
            let reference = heads(base);
            let args = [
                "update-ref",
                "-m",
                "archon merge",
                &reference,
                head,
                base_tip,
            ];
            git(repo, &args).map(drop)
        }
        Some(tree) => git(tree, &["merge", "--ff-only", "--no-overwrite-ignore", head]).map(drop),
    };
    if let Err(error) = landed {
        return Ok(Err(moved_meanwhile(base, &error)));
    }
    let now = worktree::commit_id(repo, &heads(base))?;
    if now.as_deref() != Some(head) {
        return Ok(Err(format!(
            "`{base}` does not hold the reviewed commit after the merge; check `git status` in {}",
            repo.display()
        )));
    }
    Ok(Ok(()))
}

/// Why the plan no longer holds: the base moved, or its checkout appeared,
/// went away, moved to another work tree or gained local changes.
fn changed_since_check(
    landing: Landing<'_>,
    base_tip: &str,
    planned: Option<&Path>,
) -> Result<Option<String>, ArchonError> {
    let Landing {
        item, repo, base, ..
    } = landing;
    let tip = worktree::commit_id(repo, &heads(base))?;
    if tip.as_deref() != Some(base_tip) {
        return Ok(Some(format!(
            "`{base}` moved while the merge was starting; nothing was changed, run \
             `archon merge {item}` again"
        )));
    }
    let now = checked_out(repo, base)?;
    if now.as_deref() != planned {
        return Ok(Some(format!(
            "`{base}` was checked out or switched away from while the merge was starting; \
             nothing was changed, run `archon merge {item}` again"
        )));
    }
    let Some(tree) = now else { return Ok(None) };
    Ok(dirty(&tree)?
        .map(|why| format!("`{base}` gained local changes ({why}); nothing was changed")))
}

fn moved_meanwhile(base: &str, error: &ArchonError) -> String {
    format!("git did not fast-forward `{base}` and changed nothing: {error}")
}

/// The branch's changes against its base, for `archon diff` and the Done notice.
pub(super) fn review(landing: Landing<'_>, full: bool) -> Result<LocalReview, ArchonError> {
    let Landing {
        item,
        repo,
        branch,
        base,
        head,
    } = landing;
    let merge_base = worktree::merge_base(repo, head, &[heads(base)])?;
    let from = merge_base.as_deref().unwrap_or(base);
    let range = [from, head];
    let stat = match full {
        true => git(repo, &["diff", "--stat=100", range[0], range[1]])?,
        false => git(repo, &["diff", "--shortstat", range[0], range[1]])?,
    };
    let blocker = match check(landing)? {
        Ok(Plan::AlreadyIn) => Some(format!("`{base}` already holds `{branch}`")),
        Ok(Plan::FastForward { .. }) => None,
        Err(why) => Some(why),
    };
    Ok(LocalReview {
        item: item.clone(),
        repo_path: repo.to_path_buf(),
        branch: branch.to_owned(),
        base: base.to_owned(),
        head: head.to_owned(),
        diffstat: stat,
        blocker,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_in(dir: &Path, args: &[&str]) -> String {
        git(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "-c",
                "commit.gpgsign=false",
            ][..]
                .iter()
                .chain(args)
                .copied()
                .collect::<Vec<_>>()
                .as_slice(),
        )
        .expect("git")
    }

    #[test]
    fn a_checkout_switched_after_the_check_refuses_and_moves_no_other_branch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        git_in(repo, &["init", "--quiet", "--initial-branch=main"]);
        git_in(repo, &["commit", "--quiet", "--allow-empty", "-m", "one"]);
        git_in(repo, &["checkout", "--quiet", "-b", "item"]);
        git_in(repo, &["commit", "--quiet", "--allow-empty", "-m", "two"]);
        let head = git_in(repo, &["rev-parse", "item"]);
        git_in(repo, &["checkout", "--quiet", "main"]);
        let item = QueueItemId::new();
        let landing = Landing {
            item: &item,
            repo,
            branch: "item",
            base: "main",
            head: &head,
        };

        let plan = check(landing).expect("git").expect("mergeable");
        assert!(matches!(plan, Plan::FastForward { tree: Some(_), .. }));
        git_in(repo, &["checkout", "--quiet", "-b", "other"]);
        let refused = land(landing, &plan).expect("git").expect_err("refused");

        assert!(refused.contains("switched away"), "{refused}");
        let main = git_in(repo, &["rev-parse", "main"]);
        assert_eq!(
            git_in(repo, &["rev-parse", "other"]),
            main,
            "other untouched"
        );
        assert_ne!(main, head, "main untouched");
    }
}
