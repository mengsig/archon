//! Delivery beyond the machine (ARCHITECTURE §8.7): the push of the checked
//! commit to the run branch on `origin`, from the shadow, and its pull
//! request through `gh`. Nothing is ever forced, no repository hook runs,
//! and a pull request counts only as GitHub lists it.
//!
//! An inner `Err(String)` is a refusal no retry changes; the outer error is a
//! failure worth trying again (the network, GitHub, a timeout).

use std::path::Path;

use archon_core::error::ArchonError;
use serde::Deserialize;

use super::super::gh::{self, needs_operator};
use crate::worktree::shadow::Shadow;
use crate::worktree::stack;

/// The pull request a run branch is delivered as.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Request<'a> {
    pub branch: &'a str,
    pub base: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    /// Opened as a draft: a member of an atomic initiative, until all pass.
    pub draft: bool,
}

/// Push `commit`, the one the checks proved, to `branch` on `repo`'s `origin`, never
/// forced; what `origin` already has is a success, so a repeated delivery pushes once.
pub(super) async fn push(
    data_dir: &Path,
    repo: &Path,
    branch: &str,
    commit: &str,
) -> Result<Result<(), String>, ArchonError> {
    assert!(!branch.is_empty(), "a run has a branch");
    assert!(stack::is_commit_id(commit), "the checked commit is pushed");
    let (data_dir, repo) = (data_dir.to_owned(), repo.to_owned());
    let (branch, commit) = (branch.to_owned(), commit.to_owned());
    tokio::task::spawn_blocking(move || push_now(&data_dir, &repo, &branch, &commit))
        .await
        .map_err(|e| ArchonError::io("the push task", std::io::Error::other(e)))?
}

fn push_now(
    data_dir: &Path,
    repo: &Path,
    branch: &str,
    commit: &str,
) -> Result<Result<(), String>, ArchonError> {
    let url = match stack::origin_url(repo) {
        Ok(Some(url)) => url,
        Ok(None) => {
            return Ok(Err(
                "BLOCKED: the repository has no `origin` remote to push to; add one, or set the \
                 repository's ship mode to `local`"
                    .into(),
            ));
        }
        Err(ArchonError::Invalid(why)) => return Ok(Err(format!("BLOCKED: {why}"))),
        Err(error) => return Err(error),
    };
    let shadow = Shadow::of(data_dir, repo)?;
    let pushed = stack::push(&shadow, &url, branch, commit)?;
    if pushed.status.success() {
        return Ok(Ok(()));
    }
    let doing = format!("pushing `{branch}` to `origin`");
    let said = pushed.stderr_text();
    let stdout = String::from_utf8_lossy(&pushed.stdout);
    match push_refusal(branch, &stdout).or_else(|| needs_operator(&doing, &said)) {
        Some(refusal) => Ok(Err(refusal)),
        None => Err(ArchonError::Unavailable(format!(
            "git push of `{branch}` failed: {said}"
        ))),
    }
}

/// Why `origin` will not take the branch, read from `git push --porcelain`.
fn push_refusal(branch: &str, stdout: &str) -> Option<String> {
    let line = stdout.lines().find(|line| line.starts_with('!'))?;
    let why = line.rsplit('\t').next().unwrap_or(line).trim();
    if why.contains("[remote rejected]") {
        return Some(format!(
            "BLOCKED: `origin` refused the push of `{branch}`: {why}. Its branch rules or your \
             access decide this"
        ));
    }
    Some(format!(
        "`origin` already has a `{branch}` that holds commits this run does not: {why}. Archon \
         never force-pushes; delete or rename the remote branch, then retry the item"
    ))
}

/// The open pull requests whose head is `branch`, or a refusal for want of access.
async fn open_prs(
    binary: &Path,
    repo: &Path,
    branch: &str,
) -> Result<Result<Vec<gh::PullRequest>, String>, ArchonError> {
    match gh::of_branch(binary, repo, branch, true).await {
        Ok(prs) => Ok(Ok(prs)),
        Err(ArchonError::Invalid(blocked)) => Ok(Err(blocked)),
        Err(error) => Err(error),
    }
}

/// The URL of the one open pull request of `branch`; `Err` when GitHub lists
/// none or several. What a pipeline's own ship agent claims is checked here.
pub(in crate::executor) async fn open_pr(
    binary: &Path,
    repo: &Path,
    branch: &str,
) -> Result<String, ArchonError> {
    let prs = open_prs(binary, repo, branch)
        .await?
        .map_err(ArchonError::Unavailable)?;
    let [pr] = prs.as_slice() else {
        return Err(ArchonError::Unavailable(format!(
            "expected one open PR, found {}",
            prs.len()
        )));
    };
    Ok(pr.url.clone())
}

/// The open pull request of `request.branch`, reused (moved onto
/// `request.base`) when GitHub lists one, else opened: never a second.
pub(crate) async fn pull_request(
    binary: &Path,
    repo: &Path,
    request: Request<'_>,
) -> Result<Result<String, String>, ArchonError> {
    assert!(!request.title.is_empty() && !request.body.is_empty());
    assert_ne!(
        request.branch, request.base,
        "a change is for another branch"
    );
    let found = match open_prs(binary, repo, request.branch).await? {
        Ok(found) => found,
        Err(refusal) => return Ok(Err(refusal)),
    };
    match found.as_slice() {
        [] => create(binary, repo, request).await,
        [pr] => {
            if pr.base_ref_name != request.base {
                retarget(binary, repo, &pr.url, request.base).await?;
            }
            Ok(Ok(pr.url.clone()))
        }
        several => Ok(Err(format!(
            "GitHub lists {} open pull requests for `{}`; close all but one, then retry the item",
            several.len(),
            request.branch
        ))),
    }
}

/// Open `request` and return its URL as GitHub then lists it. A refused
/// creation is no failure when the pull request is there all the same.
async fn create(
    binary: &Path,
    repo: &Path,
    request: Request<'_>,
) -> Result<Result<String, String>, ArchonError> {
    assert!(!request.branch.is_empty() && !request.base.is_empty());
    assert_ne!(
        request.branch, request.base,
        "a pull request is for another branch"
    );
    let mut args = vec![
        "pr",
        "create",
        "--base",
        request.base,
        "--head",
        request.branch,
        "--title",
        request.title,
        "--body-file",
        "-",
    ];
    if request.draft {
        args.push("--draft");
    }
    let created = gh::run(binary, repo, &args, Some(request.body)).await?;
    let listed = match open_prs(binary, repo, request.branch).await? {
        Ok(listed) => listed,
        Err(refusal) => return Ok(Err(refusal)),
    };
    if let [pr] = listed.as_slice() {
        return Ok(Ok(pr.url.clone()));
    }
    let said = created.detail();
    if let Some(blocked) = needs_operator("opening the pull request", &said) {
        return Ok(Err(blocked));
    }
    let outcome = match created.ok {
        true => "succeeded",
        false => "failed",
    };
    Err(ArchonError::Unavailable(format!(
        "`gh pr create` {outcome}, and GitHub lists {} open pull request(s) for `{}`: {said}",
        listed.len(),
        request.branch
    )))
}

/// Point the pull request at `url` to `base`, the branch its change is for.
async fn retarget(binary: &Path, repo: &Path, url: &str, base: &str) -> Result<(), ArchonError> {
    assert!(url.starts_with("https://") && !base.is_empty());
    gh::text(binary, repo, &["pr", "edit", url, "--base", base])
        .await
        .map(drop)
}

/// The merge methods a repository allows, as `gh repo view` reports them.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeMethods {
    #[serde(default)]
    squash_merge_allowed: bool,
    #[serde(default)]
    merge_commit_allowed: bool,
    #[serde(default)]
    rebase_merge_allowed: bool,
}

/// Why auto-merge is not on, or a merge method is not known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::executor) enum MergeRefusal {
    /// GitHub or the network failed; another try may pass.
    Retry(String),
    /// The repository or the operator's access rules it out.
    Never(String),
}

/// Ask GitHub to merge the pull request at `url` once its checks and reviews
/// allow, by the first method the repository permits. With `head`, only
/// that commit: GitHub refuses when the branch moved on.
pub(in crate::executor) async fn auto_merge(
    binary: &Path,
    repo: &Path,
    url: &str,
    head: Option<&str>,
) -> Result<(), MergeRefusal> {
    assert!(url.starts_with("https://"), "a verified pull request URL");
    let method = merge_method(binary, repo).await?;
    let mut merge = vec!["pr", "merge", url, "--auto", method];
    if let Some(head) = head {
        assert!(!head.is_empty(), "a pinned commit is named");
        merge.extend(["--match-head-commit", head]);
    }
    gh::text(binary, repo, &merge)
        .await
        .map(drop)
        .map_err(refusal)
}

/// The `gh pr merge` flag of the first method the repository allows:
/// squash, then a merge commit, then rebase.
pub(in crate::executor) async fn merge_method(
    binary: &Path,
    repo: &Path,
) -> Result<&'static str, MergeRefusal> {
    let fields = "squashMergeAllowed,mergeCommitAllowed,rebaseMergeAllowed";
    let viewed = gh::text(binary, repo, &["repo", "view", "--json", fields])
        .await
        .map_err(refusal)?;
    let allowed: MergeMethods = serde_json::from_str(&viewed)
        .map_err(|e| MergeRefusal::Retry(format!("invalid `gh repo view` response: {e}")))?;
    let method = match allowed {
        MergeMethods {
            squash_merge_allowed: true,
            ..
        } => "--squash",
        MergeMethods {
            merge_commit_allowed: true,
            ..
        } => "--merge",
        MergeMethods {
            rebase_merge_allowed: true,
            ..
        } => "--rebase",
        _ => {
            return Err(MergeRefusal::Never(
                "the repository allows no merge method".into(),
            ));
        }
    };
    assert!(method.starts_with("--"), "a gh pr merge flag");
    Ok(method)
}

/// A `gh` failure as a [`MergeRefusal`]: GitHub refusing the request itself,
/// or access only the operator can grant, is final.
fn refusal(error: ArchonError) -> MergeRefusal {
    const FINAL: [&str; 5] = [
        "auto merge is not allowed",
        "auto-merge is not allowed",
        "protected branch rules not configured",
        "pull request is closed",
        "is in clean status",
    ];
    match error {
        ArchonError::Invalid(blocked) => MergeRefusal::Never(blocked),
        error => {
            let said = error.to_string();
            let lower = said.to_lowercase();
            match FINAL.iter().any(|sign| lower.contains(sign)) {
                true => MergeRefusal::Never(said),
                false => MergeRefusal::Retry(said),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rejected_push_is_never_forced_and_says_what_holds_it() {
        let moved = "To github.com:o/r.git\n!\trefs/heads/a/b:refs/heads/a/b\t[rejected] (fetch first)\nDone\n";
        let refusal = push_refusal("a/b", moved).unwrap();
        assert!(refusal.contains("[rejected] (fetch first)"), "{refusal}");
        assert!(refusal.contains("never force-pushes") && !refusal.starts_with("BLOCKED:"));
        let ruled = "!\trefs/heads/a/b:refs/heads/a/b\t[remote rejected] (protected branch hook declined)\n";
        assert!(push_refusal("a/b", ruled).unwrap().starts_with("BLOCKED:"));
        assert_eq!(push_refusal("a/b", ""), None);
    }

    #[test]
    fn only_a_refusal_of_github_or_of_access_ends_the_auto_merge_tries() {
        let denied = ArchonError::Invalid("BLOCKED: gh pr merge was refused: HTTP 403".into());
        assert!(matches!(refusal(denied), MergeRefusal::Never(_)));
        let off = ArchonError::Unavailable(
            "gh pr merge failed: Auto merge is not allowed for this repository".into(),
        );
        assert!(matches!(refusal(off), MergeRefusal::Never(_)));
        let network = ArchonError::Unavailable("gh pr merge timed out after 60s".into());
        assert!(matches!(refusal(network), MergeRefusal::Retry(_)));
    }
}
